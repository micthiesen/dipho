//! Binding an edit file's sources manifest to a corpus. Precedence per
//! DESIGN.md: (1) master_hash match → bind; (2) origin_id match with
//! duration sanity (±0.5 s) → bind with a surfaced warning, the manifest
//! updated only on explicit save; (3) neither → typed UnresolvedSource.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use crate::span::SourceId;

use super::{Edl, SourceInfo, SourceMap, SourceRef};

/// Duration sanity window for an origin_id-only rebind.
pub const REBIND_DURATION_TOLERANCE: f64 = 0.5;

/// One row of the corpus `sources` table, as the rebind input and the
/// builder for manifest entries and the compile-time source map — resolved
/// by the caller, keeping this module corpus-I/O-free.
#[derive(Debug, Clone, PartialEq)]
pub struct CorpusSource {
    pub id: SourceId,
    pub origin: String,
    pub origin_id: String,
    pub master_path: String,
    pub master_hash: String,
    pub duration: f64,
    /// Post-normalization CFR rate; None for audio-only sources.
    pub fps: Option<f64>,
}

impl CorpusSource {
    pub fn source_ref(&self) -> SourceRef {
        SourceRef {
            origin: self.origin.clone(),
            origin_id: self.origin_id.clone(),
            master_filename: Path::new(&self.master_path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| self.master_path.clone()),
            duration: self.duration,
            master_hash: self.master_hash.clone(),
        }
    }

    pub fn source_info(&self) -> SourceInfo {
        SourceInfo {
            master_path: self.master_path.clone().into(),
            duration: self.duration,
            fps: self.fps,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RebindError {
    #[error("source {origin_id:?} ({origin}) is not in this corpus — ingest it, then reopen")]
    UnresolvedSource { origin_id: String, origin: String },
    #[error("clip {clip_index} references source {source_id:?} missing from the sources manifest")]
    MissingManifestEntry {
        clip_index: usize,
        source_id: SourceId,
    },
}

/// A successfully bound edit: clip source ids rewritten to corpus ids, the
/// manifest rebuilt from corpus values (persisted only on explicit save),
/// and the source map ready for compilation. Unreferenced manifest entries
/// are pruned.
#[derive(Debug, Clone)]
pub struct Rebind {
    pub edl: Edl,
    pub source_map: SourceMap,
    /// One entry per origin_id-only bind (the master hash didn't match).
    pub warnings: Vec<String>,
}

pub fn rebind(edl: &Edl, corpus: &[CorpusSource]) -> Result<Rebind, RebindError> {
    let referenced: BTreeSet<SourceId> = edl.clips.iter().map(|c| c.span.source).collect();
    let mut mapping: HashMap<SourceId, SourceId> = HashMap::new();
    let mut sources = BTreeMap::new();
    let mut source_map = SourceMap::new();
    let mut warnings = Vec::new();

    for old_id in referenced {
        let entry = edl
            .sources
            .get(&old_id)
            .ok_or_else(|| RebindError::MissingManifestEntry {
                // The first clip referencing the missing source; one must
                // exist, since `referenced` was collected from the clips.
                clip_index: edl
                    .clips
                    .iter()
                    .position(|c| c.span.source == old_id)
                    .expect("referenced source ids come from clips"),
                source_id: old_id,
            })?;
        let by_hash = corpus.iter().find(|c| c.master_hash == entry.master_hash);
        let bound = match by_hash {
            Some(source) => source,
            None => {
                let by_origin = corpus.iter().find(|c| {
                    c.origin_id == entry.origin_id
                        && (c.duration - entry.duration).abs() <= REBIND_DURATION_TOLERANCE
                });
                let source = by_origin.ok_or_else(|| RebindError::UnresolvedSource {
                    origin_id: entry.origin_id.clone(),
                    origin: entry.origin.clone(),
                })?;
                warnings.push(format!(
                    "source {} rebound by origin_id (master hash differs); manifest updates on save",
                    entry.origin
                ));
                source
            }
        };
        mapping.insert(old_id, bound.id);
        sources.insert(bound.id, bound.source_ref());
        source_map.insert(bound.id, bound.source_info());
    }

    let clips = edl
        .clips
        .iter()
        .map(|clip| {
            let mut clip = clip.clone();
            clip.span.source = mapping[&clip.span.source];
            clip
        })
        .collect();
    Ok(Rebind {
        edl: Edl { clips, sources },
        source_map,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::super::Clip;
    use super::*;
    use crate::span::{Channel, Span};

    fn corpus_source(id: i64, origin_id: &str, hash: &str, duration: f64) -> CorpusSource {
        CorpusSource {
            id: SourceId(id),
            origin: format!("https://example.com/{origin_id}"),
            origin_id: origin_id.to_string(),
            master_path: format!("/masters/{origin_id}/master.mkv"),
            master_hash: hash.to_string(),
            duration,
            fps: Some(30.0),
        }
    }

    fn edit_with(entries: Vec<(i64, SourceRef)>, clip_sources: Vec<i64>) -> Edl {
        Edl {
            clips: clip_sources
                .into_iter()
                .map(|s| Clip {
                    span: Span {
                        source: SourceId(s),
                        t_start: 1.0,
                        t_end: 2.0,
                        channel: Channel::Both,
                    },
                    transforms: vec![],
                    provenance: None,
                    label: None,
                })
                .collect(),
            sources: entries
                .into_iter()
                .map(|(id, r)| (SourceId(id), r))
                .collect(),
        }
    }

    fn source_ref(origin_id: &str, hash: &str, duration: f64) -> SourceRef {
        SourceRef {
            origin: format!("https://example.com/{origin_id}"),
            origin_id: origin_id.to_string(),
            master_filename: "master.mkv".to_string(),
            duration,
            master_hash: hash.to_string(),
        }
    }

    #[test]
    fn master_hash_match_binds_silently() {
        // The edit knew the source as id 9; this corpus has it as id 3.
        let edl = edit_with(vec![(9, source_ref("yt:a", "hash-a", 50.0))], vec![9]);
        let corpus = [corpus_source(3, "yt:a", "hash-a", 50.0)];
        let bound = rebind(&edl, &corpus).unwrap();
        assert!(bound.warnings.is_empty());
        assert_eq!(bound.edl.clips[0].span.source, SourceId(3));
        assert!(bound.edl.sources.contains_key(&SourceId(3)));
        assert_eq!(
            bound.source_map[&SourceId(3)].master_path,
            std::path::PathBuf::from("/masters/yt:a/master.mkv")
        );
        assert_eq!(
            bound.edl.sources[&SourceId(3)].master_filename,
            "master.mkv"
        );
    }

    #[test]
    fn origin_id_match_with_duration_sanity_binds_with_warning() {
        // Hash differs (re-encoded master on another machine), duration
        // within ±0.5 s.
        let edl = edit_with(vec![(1, source_ref("yt:a", "old-hash", 50.0))], vec![1]);
        let corpus = [corpus_source(7, "yt:a", "new-hash", 50.3)];
        let bound = rebind(&edl, &corpus).unwrap();
        assert_eq!(bound.warnings.len(), 1);
        assert!(bound.warnings[0].contains("rebound by origin_id"));
        assert_eq!(bound.edl.clips[0].span.source, SourceId(7));
        // The in-memory manifest now carries corpus values (persisted only
        // when the user explicitly saves).
        assert_eq!(bound.edl.sources[&SourceId(7)].master_hash, "new-hash");
    }

    #[test]
    fn duration_outside_tolerance_is_unresolved() {
        let edl = edit_with(vec![(1, source_ref("yt:a", "old-hash", 50.0))], vec![1]);
        let corpus = [corpus_source(7, "yt:a", "new-hash", 51.0)];
        match rebind(&edl, &corpus) {
            Err(RebindError::UnresolvedSource { origin_id, .. }) => {
                assert_eq!(origin_id, "yt:a");
            }
            other => panic!("expected UnresolvedSource, got {other:?}"),
        }
    }

    #[test]
    fn unknown_source_is_unresolved_and_unreferenced_entries_are_pruned() {
        let edl = edit_with(
            vec![
                (1, source_ref("yt:a", "hash-a", 50.0)),
                (2, source_ref("yt:gone", "hash-gone", 10.0)),
            ],
            vec![1],
        );
        let corpus = [corpus_source(1, "yt:a", "hash-a", 50.0)];
        // Entry 2 is unreferenced by any clip: pruned, not an error.
        let bound = rebind(&edl, &corpus).unwrap();
        assert_eq!(bound.edl.sources.len(), 1);

        // But a referenced unknown source is a typed error.
        let edl = edit_with(vec![(2, source_ref("yt:gone", "hash-gone", 10.0))], vec![2]);
        assert!(matches!(
            rebind(&edl, &corpus),
            Err(RebindError::UnresolvedSource { .. })
        ));
    }

    #[test]
    fn hand_built_edl_without_manifest_entry_is_typed() {
        let edl = edit_with(vec![], vec![5]);
        assert!(matches!(
            rebind(&edl, &[]),
            Err(RebindError::MissingManifestEntry {
                clip_index: 0,
                source_id: SourceId(5)
            })
        ));
    }
}
