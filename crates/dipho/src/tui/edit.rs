//! The edit session: the in-memory EDL plus everything that makes its
//! mutations safe — bounded undo/redo (EDL scope only), autosave after
//! every mutation with recovery on open, and save/load with the mandatory
//! sources manifest (rebind precedence handled by dipho-core).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use dipho_core::edl::{
    Clip, CorpusSource, Edl, EdlCompileError, MpvEdl, PreviewPlan, SourceMap, plan_preview, rebind,
};

/// Undo depth. Old states fall off the bottom; nudging is one step per
/// keypress, so this absorbs a lot of fiddling.
const UNDO_LIMIT: usize = 200;

/// The smallest clip a trim may leave behind: no zero or negative-length
/// spans (compile-time validation would reject them). Equal to the
/// join-elision epsilon, which is harmless — identical and non-forward
/// spans never merge regardless of length.
const MIN_CLIP_LEN: f64 = 0.001;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Start,
    End,
}

pub struct EditSession {
    pub path: PathBuf,
    pub edl: Edl,
    /// Compile-time source map for every manifest source, from the corpus
    /// (rebind) or the search hit (append).
    pub source_map: SourceMap,
    /// The session began from `<path>.autosave` — unsaved work from an
    /// earlier session; an explicit save commits it.
    pub recovered: bool,
    dirty: bool,
    undo: Vec<Vec<Clip>>,
    redo: Vec<Vec<Clip>>,
}

impl EditSession {
    pub fn empty(path: PathBuf) -> Self {
        Self {
            path,
            edl: Edl::default(),
            source_map: SourceMap::new(),
            recovered: false,
            dirty: false,
            undo: Vec::new(),
            redo: Vec::new(),
        }
    }

    /// Open `path`, preferring its autosave (recovery), and rebind the
    /// manifest against the corpus. Returns the session plus any rebind
    /// warnings. A missing file is an empty session, not an error.
    pub fn open(path: PathBuf, corpus: &[CorpusSource]) -> Result<(Self, Vec<String>)> {
        let autosave = autosave_path(&path);
        let (read_from, recovered) = if autosave.exists() {
            (autosave, true)
        } else if path.exists() {
            (path.clone(), false)
        } else {
            return Ok((Self::empty(path), Vec::new()));
        };
        let json = std::fs::read_to_string(&read_from)
            .with_context(|| format!("reading edit {}", read_from.display()))?;
        let edl = Edl::from_json(&json)
            .with_context(|| format!("loading edit {}", read_from.display()))?;
        let bound = rebind(&edl, corpus)
            .with_context(|| format!("binding edit {} to the corpus", read_from.display()))?;
        Ok((
            Self {
                path,
                edl: bound.edl,
                source_map: bound.source_map,
                recovered,
                // Recovered work isn't committed until an explicit save.
                dirty: recovered,
                undo: Vec::new(),
                redo: Vec::new(),
            },
            bound.warnings,
        ))
    }

    pub fn dirty(&self) -> bool {
        self.dirty
    }

    pub fn clips(&self) -> &[Clip] {
        &self.edl.clips
    }

    pub fn compile(&self) -> Result<(PreviewPlan, MpvEdl), EdlCompileError> {
        let plan = plan_preview(&self.edl, &self.source_map)?;
        let mpv = MpvEdl::from_plan(&plan);
        Ok((plan, mpv))
    }

    /// Every mutation goes through this first: push the current clips for
    /// undo, invalidate redo.
    fn snapshot(&mut self) {
        if self.undo.len() == UNDO_LIMIT {
            self.undo.remove(0);
        }
        self.undo.push(self.edl.clips.clone());
        self.redo.clear();
        self.dirty = true;
    }

    pub fn append(&mut self, clip: Clip, source: &CorpusSource) {
        self.snapshot();
        self.edl
            .sources
            .entry(source.id)
            .or_insert_with(|| source.source_ref());
        self.source_map
            .entry(source.id)
            .or_insert_with(|| source.source_info());
        self.edl.clips.push(clip);
    }

    pub fn remove(&mut self, index: usize) -> bool {
        if index >= self.edl.clips.len() {
            return false;
        }
        self.snapshot();
        self.edl.clips.remove(index);
        true
    }

    /// Move the clip at `index` one slot up (-1) or down (+1); returns its
    /// new index when it moved.
    pub fn shift(&mut self, index: usize, delta: isize) -> Option<usize> {
        let to = index.checked_add_signed(delta)?;
        if index >= self.edl.clips.len() || to >= self.edl.clips.len() {
            return None;
        }
        self.snapshot();
        self.edl.clips.swap(index, to);
        Some(to)
    }

    /// Nudge one edge of a clip by `delta` seconds. Rejected (no mutation)
    /// when the result would leave [0, source duration] or collapse the
    /// clip below MIN_CLIP_LEN.
    pub fn trim(&mut self, index: usize, edge: Edge, delta: f64) -> bool {
        let Some(clip) = self.edl.clips.get(index) else {
            return false;
        };
        let Some(info) = self.source_map.get(&clip.span.source) else {
            return false;
        };
        let (t_start, t_end) = match edge {
            Edge::Start => (clip.span.t_start + delta, clip.span.t_end),
            Edge::End => (clip.span.t_start, clip.span.t_end + delta),
        };
        if t_start < 0.0 || t_end > info.duration + 1e-9 || t_end - t_start < MIN_CLIP_LEN {
            return false;
        }
        self.snapshot();
        let span = &mut self.edl.clips[index].span;
        span.t_start = t_start;
        span.t_end = t_end;
        true
    }

    pub fn undo(&mut self) -> bool {
        let Some(clips) = self.undo.pop() else {
            return false;
        };
        self.redo
            .push(std::mem::replace(&mut self.edl.clips, clips));
        self.dirty = true;
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(clips) = self.redo.pop() else {
            return false;
        };
        self.undo
            .push(std::mem::replace(&mut self.edl.clips, clips));
        self.dirty = true;
        true
    }

    /// Write the recovery file. Called after every mutation; cheap (the
    /// EDL is small) and synchronous by design — losing a nudge to a crash
    /// would defeat its purpose. tmp + rename, so a crash mid-write can
    /// never leave a corrupt autosave for the next open to choke on.
    pub fn autosave(&self) -> Result<()> {
        let json = self.edl.to_json()?;
        write_atomically(&autosave_path(&self.path), &json)
            .with_context(|| format!("autosaving {}", self.path.display()))?;
        Ok(())
    }

    /// Explicit save: prune unreferenced manifest entries, write the edit
    /// file (tmp + rename), drop the autosave.
    pub fn save(&mut self) -> Result<()> {
        let referenced: BTreeSet<_> = self.edl.clips.iter().map(|c| c.span.source).collect();
        self.edl.sources.retain(|id, _| referenced.contains(id));
        let json = self.edl.to_json()?;
        write_atomically(&self.path, &json)
            .with_context(|| format!("saving {}", self.path.display()))?;
        let _ = std::fs::remove_file(autosave_path(&self.path));
        self.dirty = false;
        self.recovered = false;
        Ok(())
    }
}

fn write_atomically(path: &Path, contents: &str) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)
}

fn autosave_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".autosave");
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dipho_core::span::{Channel, SourceId, Span};

    fn source(id: i64) -> CorpusSource {
        CorpusSource {
            id: SourceId(id),
            origin: format!("https://example.com/{id}"),
            origin_id: format!("yt:{id}"),
            master_path: format!("/masters/{id}/master.mkv"),
            master_hash: format!("hash-{id}"),
            duration: 100.0,
            fps: Some(30.0),
        }
    }

    fn clip(source: i64, t_start: f64, t_end: f64) -> Clip {
        Clip {
            span: Span {
                source: SourceId(source),
                t_start,
                t_end,
                channel: Channel::Both,
            },
            transforms: vec![],
            provenance: None,
            label: Some("word".to_string()),
        }
    }

    fn session_with_two_clips(dir: &Path) -> EditSession {
        let mut session = EditSession::empty(dir.join("edit.json"));
        session.append(clip(1, 1.0, 2.0), &source(1));
        session.append(clip(1, 5.0, 6.0), &source(1));
        session
    }

    #[test]
    fn append_records_manifest_and_source_map_once() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_with_two_clips(dir.path());
        assert_eq!(session.edl.clips.len(), 2);
        assert_eq!(session.edl.sources.len(), 1);
        assert_eq!(
            session.edl.sources[&SourceId(1)].master_filename,
            "master.mkv"
        );
        assert_eq!(session.source_map.len(), 1);
        assert!(session.dirty());
    }

    #[test]
    fn trim_validates_against_source_bounds_and_min_length() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = session_with_two_clips(dir.path());
        assert!(session.trim(0, Edge::Start, -0.025));
        assert!((session.clips()[0].span.t_start - 0.975).abs() < 1e-9);
        assert!(session.trim(0, Edge::End, 0.005));
        assert!((session.clips()[0].span.t_end - 2.005).abs() < 1e-9);
        // Start can't cross below zero, the end can't pass the source's
        // duration, and the clip can't collapse.
        assert!(!session.trim(0, Edge::Start, -1.0));
        assert!(!session.trim(1, Edge::End, 95.0));
        assert!(!session.trim(0, Edge::Start, 1.04));
        // Rejected trims mutate nothing.
        assert!((session.clips()[0].span.t_start - 0.975).abs() < 1e-9);
    }

    #[test]
    fn undo_redo_round_trips_every_mutation_kind() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = session_with_two_clips(dir.path());
        session.trim(0, Edge::End, 0.025);
        session.shift(0, 1);
        session.remove(0);
        assert_eq!(session.clips().len(), 1);

        assert!(session.undo()); // un-remove
        assert_eq!(session.clips().len(), 2);
        assert!(session.undo()); // un-shift
        assert!((session.clips()[0].span.t_end - 2.025).abs() < 1e-9);
        assert!(session.undo()); // un-trim
        assert!((session.clips()[0].span.t_end - 2.0).abs() < 1e-9);

        assert!(session.redo());
        assert!((session.clips()[0].span.t_end - 2.025).abs() < 1e-9);
        // A new mutation invalidates the redo branch.
        session.trim(0, Edge::End, 0.005);
        assert!(!session.redo());
        // Two appends remain undoable beneath everything.
        while session.undo() {}
        assert!(session.clips().is_empty());
    }

    #[test]
    fn save_then_open_round_trips_through_rebind() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = session_with_two_clips(dir.path());
        session.autosave().unwrap();
        session.save().unwrap();
        assert!(!session.dirty());
        assert!(!autosave_path(&session.path).exists());

        let (reopened, warnings) = EditSession::open(session.path.clone(), &[source(1)]).unwrap();
        assert!(warnings.is_empty());
        assert!(!reopened.recovered);
        assert_eq!(reopened.edl, session.edl);
        assert_eq!(reopened.source_map.len(), 1);
    }

    #[test]
    fn autosave_is_recovered_and_committed_only_on_explicit_save() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = session_with_two_clips(dir.path());
        session.save().unwrap();
        // More work, autosaved but never saved (a crash).
        session.trim(0, Edge::End, 0.025);
        session.autosave().unwrap();
        drop(session);

        let path = dir.path().join("edit.json");
        let (mut recovered, _) = EditSession::open(path.clone(), &[source(1)]).unwrap();
        assert!(recovered.recovered);
        assert!(recovered.dirty());
        assert!((recovered.clips()[0].span.t_end - 2.025).abs() < 1e-9);
        recovered.save().unwrap();
        assert!(!recovered.recovered);

        let (clean, _) = EditSession::open(path, &[source(1)]).unwrap();
        assert!(!clean.recovered);
        assert!((clean.clips()[0].span.t_end - 2.025).abs() < 1e-9);
    }

    #[test]
    fn open_of_a_missing_file_is_an_empty_session() {
        let dir = tempfile::tempdir().unwrap();
        let (session, warnings) =
            EditSession::open(dir.path().join("edit.json"), &[source(1)]).unwrap();
        assert!(warnings.is_empty());
        assert!(session.clips().is_empty());
        assert!(!session.dirty());
    }

    #[test]
    fn open_with_an_unresolvable_source_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = session_with_two_clips(dir.path());
        session.save().unwrap();
        let err = match EditSession::open(session.path.clone(), &[]) {
            Err(e) => e,
            Ok(_) => panic!("open with an empty corpus must fail"),
        };
        assert!(err.to_string().contains("binding edit"), "{err}");
    }

    #[test]
    fn save_prunes_unreferenced_manifest_entries() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = session_with_two_clips(dir.path());
        session.append(clip(2, 0.0, 1.0), &source(2));
        session.remove(2);
        assert_eq!(session.edl.sources.len(), 2);
        session.save().unwrap();
        assert_eq!(session.edl.sources.len(), 1);
    }

    #[test]
    fn compile_produces_plan_and_uri() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_with_two_clips(dir.path());
        let (plan, mpv) = session.compile().unwrap();
        assert_eq!(plan.segments.len(), 2);
        assert!((plan.total_duration - 2.0).abs() < 1e-9);
        assert!(mpv.uri().starts_with("edl://"));
    }
}
