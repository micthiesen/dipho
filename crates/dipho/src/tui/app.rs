//! App state and the update half of the Elm loop: pure-ish event handling,
//! no rendering and no I/O beyond posting search requests, player commands,
//! and the edit autosave.

use std::path::PathBuf;

use crossterm::event::{Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use dipho_core::corpus::SearchHit;
use dipho_core::edl::{Clip, PreviewPlan, ProvenanceRef};
use dipho_core::span::{Channel, Span};
use ratatui::widgets::TableState;
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler;

use super::db::DbHandle;
use super::edit::{Edge, EditSession};
use super::event::Event;
use super::player::{Audition, PlayerHandle, PlayerUpdate, Preview, PreviewSeek};

/// Context padding around the matched span for the play-with-context key.
const CONTEXT_PAD: f64 = 0.5;

/// Output-time padding around a trimmed clip for the neighborhood replay.
const REPLAY_PAD: f64 = 0.5;

/// Trim/nudge step sizes (ROADMAP M5).
const NUDGE_FINE: f64 = 0.005;
const NUDGE_COARSE: f64 = 0.025;

/// The three audition playback actions, one key each.
#[derive(Clone, Copy)]
enum AuditionKind {
    /// ab-loop the exact matched word span.
    LoopExact,
    /// The matched span ±500 ms, played once.
    Context,
    /// The whole utterance, played once.
    FullUtterance,
}

/// Which pane owns the keyboard.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Search,
    Edit,
}

pub struct App {
    pub corpus_db: PathBuf,
    pub input: Input,
    pub hits: Vec<SearchHit>,
    pub table: TableState,
    /// Monotonic query revision; results tagged with an older generation
    /// are stale and dropped.
    pub generation: u64,
    pub searching: bool,
    /// A problem or confirmation to surface in the status line.
    pub note: Option<String>,
    /// Latest audition player status (None until mpv reports in).
    pub player_state: Option<PlayerUpdate>,
    pub quit: bool,
    pub focus: Pane,
    pub edit: EditSession,
    pub edl_table: TableState,
    /// The compiled preview geometry, recomputed after every mutation.
    pub plan: Option<PreviewPlan>,
    preview_uri: Option<String>,
    /// The slave currently has the preview timeline loaded (vs a master
    /// for audition), so Space toggles pause instead of reloading.
    preview_active: bool,
    db: Option<DbHandle>,
    player: PlayerHandle,
}

impl App {
    pub fn new(
        corpus_db: PathBuf,
        db: Result<DbHandle, String>,
        player: PlayerHandle,
        edit: EditSession,
        mut startup_notes: Vec<String>,
    ) -> Self {
        let (db, db_note) = match db {
            Ok(handle) => (Some(handle), None),
            Err(message) => (None, Some(message)),
        };
        if edit.recovered {
            startup_notes.push(format!(
                "recovered unsaved work from {}.autosave — press s in the edit pane to commit",
                edit.path.display()
            ));
        }
        startup_notes.extend(db_note);
        let mut app = Self {
            corpus_db,
            input: Input::default(),
            hits: Vec::new(),
            table: TableState::default(),
            generation: 0,
            searching: false,
            note: (!startup_notes.is_empty()).then(|| startup_notes.join("; ")),
            player_state: None,
            quit: false,
            focus: Pane::Search,
            edit,
            edl_table: TableState::default(),
            plan: None,
            preview_uri: None,
            preview_active: false,
            db,
            player,
        };
        app.recompile();
        app.select_clip(0);
        app
    }

    pub fn update(&mut self, event: Event) {
        match event {
            Event::Term(TermEvent::Key(key)) if key.kind != KeyEventKind::Release => {
                self.on_key(key);
            }
            Event::Term(_) => {}
            Event::SearchDone { generation, result } if generation == self.generation => {
                self.searching = false;
                match result {
                    Ok(hits) => {
                        self.hits = hits;
                        self.select(0);
                    }
                    Err(message) => self.note = Some(message),
                }
            }
            // Superseded by a newer query revision.
            Event::SearchDone { .. } => {}
            Event::Player(update) => self.player_state = Some(update),
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => return self.quit = true,
            KeyCode::Char('c') if ctrl => return self.quit = true,
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Pane::Search => Pane::Edit,
                    Pane::Edit => Pane::Search,
                };
                return;
            }
            KeyCode::Char('a') if ctrl => return self.append_hit(),
            KeyCode::Char('p') if ctrl => return self.player.stop(),
            _ => {}
        }
        match self.focus {
            Pane::Search => self.on_search_key(key),
            Pane::Edit => self.on_edit_key(key),
        }
    }

    fn on_search_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Down => self.move_selection(1),
            KeyCode::PageUp => self.move_selection(-10),
            KeyCode::PageDown => self.move_selection(10),
            KeyCode::Enter => self.audition(AuditionKind::LoopExact),
            KeyCode::Char('t') if ctrl => self.audition(AuditionKind::Context),
            KeyCode::Char('u') if ctrl => self.audition(AuditionKind::FullUtterance),
            _ => {
                if let Some(change) = self.input.handle_event(&TermEvent::Key(key))
                    && change.value
                {
                    self.issue_search();
                }
            }
        }
    }

    fn on_edit_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Up if shift => self.move_clip(-1),
            KeyCode::Down if shift => self.move_clip(1),
            KeyCode::Char('K') => self.move_clip(-1),
            KeyCode::Char('J') => self.move_clip(1),
            KeyCode::Up => self.move_clip_selection(-1),
            KeyCode::Down => self.move_clip_selection(1),
            KeyCode::PageUp => self.move_clip_selection(-10),
            KeyCode::PageDown => self.move_clip_selection(10),
            KeyCode::Char('d') | KeyCode::Delete | KeyCode::Backspace => self.remove_clip(),
            KeyCode::Char('u') => self.undo(),
            KeyCode::Char('U') => self.redo(),
            KeyCode::Char('r') if ctrl => self.redo(),
            KeyCode::Char(',') => self.trim_clip(Edge::Start, -NUDGE_FINE),
            KeyCode::Char('.') => self.trim_clip(Edge::Start, NUDGE_FINE),
            KeyCode::Char('<') => self.trim_clip(Edge::Start, -NUDGE_COARSE),
            KeyCode::Char('>') => self.trim_clip(Edge::Start, NUDGE_COARSE),
            KeyCode::Char('[') => self.trim_clip(Edge::End, -NUDGE_FINE),
            KeyCode::Char(']') => self.trim_clip(Edge::End, NUDGE_FINE),
            KeyCode::Char('{') => self.trim_clip(Edge::End, -NUDGE_COARSE),
            KeyCode::Char('}') => self.trim_clip(Edge::End, NUDGE_COARSE),
            KeyCode::Char(' ') => self.toggle_preview(),
            KeyCode::Enter => self.preview_from_selected(),
            KeyCode::Char('s') => self.save(),
            _ => {}
        }
    }

    fn issue_search(&mut self) {
        self.generation += 1;
        let query = self.input.value().trim().to_string();
        if query.is_empty() {
            self.hits.clear();
            self.searching = false;
            self.select(0);
            return;
        }
        if let Some(db) = &self.db {
            db.request(self.generation, query);
            self.searching = true;
        }
    }

    /// Send the selected hit to the player. Loop and context audition the
    /// hit's first occurrence; further occurrences of the same utterance
    /// are reachable through the full-utterance key for now.
    fn audition(&mut self, kind: AuditionKind) {
        let Some(hit) = self.table.selected().and_then(|i| self.hits.get(i)) else {
            return;
        };
        let Some(m) = hit.matches.first() else {
            return;
        };
        let (start, end) = hit.match_span(m);
        let phrase = || {
            hit.words[m.clone()]
                .iter()
                .map(|w| w.text.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        };
        let (start, end, looped, label) = match kind {
            AuditionKind::LoopExact => (start, end, true, phrase()),
            AuditionKind::Context => (
                (start - CONTEXT_PAD).max(0.0),
                // May exceed the source's duration; the player's
                // eof-reached backstop (mpv keep-open) ends playback there.
                end + CONTEXT_PAD,
                false,
                phrase(),
            ),
            AuditionKind::FullUtterance => (hit.t_start, hit.t_end, false, hit.text.clone()),
        };
        self.player.audition(Audition {
            master_path: hit.source.master_path.clone(),
            start,
            end,
            looped,
            label,
        });
        // The slave now holds the master, not the preview timeline.
        self.preview_active = false;
    }

    /// Append the selected hit's matched span to the edit as a clip.
    fn append_hit(&mut self) {
        let Some(hit) = self.table.selected().and_then(|i| self.hits.get(i)) else {
            return;
        };
        let Some(m) = hit.matches.first() else {
            return;
        };
        let (t_start, t_end) = hit.match_span(m);
        let label = hit.words[m.clone()]
            .iter()
            .map(|w| w.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        let clip = Clip {
            span: Span {
                source: hit.source.id,
                t_start,
                t_end,
                channel: Channel::Both,
            },
            transforms: vec![],
            provenance: Some(ProvenanceRef::Utterance(hit.utterance_id)),
            label: Some(label),
        };
        let source = hit.source.clone();
        self.edit.append(clip, &source);
        self.edl_table.select(Some(self.edit.clips().len() - 1));
        self.after_mutation(None);
    }

    fn remove_clip(&mut self) {
        let Some(i) = self.edl_table.selected() else {
            return;
        };
        if self.edit.remove(i) {
            self.select_clip(i);
            self.after_mutation(None);
        }
    }

    fn move_clip(&mut self, delta: isize) {
        let Some(i) = self.edl_table.selected() else {
            return;
        };
        if let Some(to) = self.edit.shift(i, delta) {
            self.edl_table.select(Some(to));
            self.after_mutation(None);
        }
    }

    fn trim_clip(&mut self, edge: Edge, delta: f64) {
        let Some(i) = self.edl_table.selected() else {
            return;
        };
        if self.edit.trim(i, edge, delta) {
            self.after_mutation(Some(i));
        }
    }

    fn undo(&mut self) {
        if self.edit.undo() {
            self.clamp_clip_selection();
            self.after_mutation(None);
        }
    }

    fn redo(&mut self) {
        if self.edit.redo() {
            self.clamp_clip_selection();
            self.after_mutation(None);
        }
    }

    fn save(&mut self) {
        match self.edit.save() {
            Ok(()) => self.note = Some(format!("saved {}", self.edit.path.display())),
            Err(e) => self.note = Some(e.to_string()),
        }
    }

    /// Space: toggle preview pause, loading the timeline on first play.
    fn toggle_preview(&mut self) {
        if self.preview_active {
            self.player.toggle_pause();
            return;
        }
        let Some(uri) = self.preview_uri.clone() else {
            return;
        };
        if self.edit.clips().is_empty() {
            return;
        }
        self.player.preview(Preview {
            uri,
            seek: PreviewSeek::From(0.0),
        });
        self.preview_active = true;
    }

    /// Enter: play the preview from the selected clip's output position.
    fn preview_from_selected(&mut self) {
        let Some(i) = self.edl_table.selected() else {
            return;
        };
        let (Some(plan), Some(uri)) = (&self.plan, self.preview_uri.clone()) else {
            return;
        };
        let Some(&(start, _)) = plan.clip_output.get(i) else {
            return;
        };
        self.player.preview(Preview {
            uri,
            seek: PreviewSeek::From(start),
        });
        self.preview_active = true;
    }

    /// Every EDL mutation funnels here: autosave, instant recompile, and
    /// the right preview reload — a neighborhood replay around a trimmed
    /// clip, else a position-preserving resume if the preview is loaded.
    fn after_mutation(&mut self, replay_clip: Option<usize>) {
        if let Err(e) = self.edit.autosave() {
            self.note = Some(e.to_string());
        }
        self.recompile();
        let (Some(plan), Some(uri)) = (&self.plan, self.preview_uri.clone()) else {
            return;
        };
        if self.edit.clips().is_empty() {
            if self.preview_active {
                self.player.stop();
                self.preview_active = false;
            }
            return;
        }
        if let Some(i) = replay_clip {
            let Some(&(out_start, out_end)) = plan.clip_output.get(i) else {
                return;
            };
            self.player.preview(Preview {
                uri,
                seek: PreviewSeek::Window {
                    start: (out_start - REPLAY_PAD).max(0.0),
                    end: (out_end + REPLAY_PAD).min(plan.total_duration),
                },
            });
            self.preview_active = true;
        } else if self.preview_active {
            self.player.preview(Preview {
                uri,
                seek: PreviewSeek::Resume {
                    max: plan.total_duration,
                },
            });
        }
    }

    fn recompile(&mut self) {
        match self.edit.compile() {
            Ok((plan, mpv)) => {
                self.preview_uri = Some(mpv.uri());
                self.plan = Some(plan);
            }
            Err(e) => {
                // Unreachable through the M5 UI (mutations validate), but
                // loaded edits can carry anything.
                self.note = Some(e.to_string());
                self.plan = None;
                self.preview_uri = None;
            }
        }
    }

    fn move_selection(&mut self, delta: isize) {
        if self.hits.is_empty() {
            return;
        }
        let current = self.table.selected().unwrap_or(0) as isize;
        let last = self.hits.len() as isize - 1;
        self.select((current + delta).clamp(0, last) as usize);
    }

    fn select(&mut self, index: usize) {
        if self.hits.is_empty() {
            self.table.select(None);
        } else {
            self.table.select(Some(index.min(self.hits.len() - 1)));
        }
    }

    fn move_clip_selection(&mut self, delta: isize) {
        if self.edit.clips().is_empty() {
            return;
        }
        let current = self.edl_table.selected().unwrap_or(0) as isize;
        let last = self.edit.clips().len() as isize - 1;
        self.select_clip((current + delta).clamp(0, last) as usize);
    }

    fn select_clip(&mut self, index: usize) {
        if self.edit.clips().is_empty() {
            self.edl_table.select(None);
        } else {
            self.edl_table
                .select(Some(index.min(self.edit.clips().len() - 1)));
        }
    }

    fn clamp_clip_selection(&mut self) {
        self.select_clip(self.edl_table.selected().unwrap_or(0));
    }
}

#[cfg(test)]
mod tests {
    use super::super::player::PlayerCmd;
    use super::*;
    use dipho_core::corpus::WordSpan;
    use dipho_core::edl::CorpusSource;
    use dipho_core::span::SourceId;

    fn key(code: KeyCode) -> Event {
        Event::Term(TermEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    fn ctrl(c: char) -> Event {
        Event::Term(TermEvent::Key(KeyEvent::new(
            KeyCode::Char(c),
            KeyModifiers::CONTROL,
        )))
    }

    pub(crate) fn test_source(id: i64) -> CorpusSource {
        CorpusSource {
            id: SourceId(id),
            origin: "test".to_string(),
            origin_id: format!("yt:{id}"),
            master_path: format!("/masters/test{id}.mkv"),
            master_hash: format!("hash-{id}"),
            duration: 100.0,
            fps: Some(30.0),
        }
    }

    pub(crate) fn hit(text: &str) -> SearchHit {
        SearchHit {
            utterance_id: 1,
            source: test_source(1),
            t_start: 0.0,
            t_end: 1.0,
            text: text.to_string(),
            speaker: None,
            multi_speaker: false,
            confidence: None,
            words: Vec::new(),
            matches: Vec::new(),
        }
    }

    /// A hit whose first match is "twenty five" at 0.2–0.9 s, inside an
    /// utterance spanning 0.0–2.0 s.
    fn matched_hit() -> SearchHit {
        let mut h = hit("I said 25 ok");
        h.t_end = 2.0;
        h.words = [
            ("i", 0.0),
            ("said", 0.1),
            ("twenty", 0.2),
            ("five", 0.6),
            ("ok", 0.9),
        ]
        .iter()
        .map(|&(w, t)| WordSpan {
            text: w.to_string(),
            t_start: t,
            t_end: t + 0.3,
            confidence: None,
        })
        .collect();
        h.matches.push(2..4);
        h
    }

    struct Channels {
        db: std::sync::mpsc::Receiver<super::super::db::SearchRequest>,
        player: tokio::sync::mpsc::UnboundedReceiver<PlayerCmd>,
        _dir: tempfile::TempDir,
    }

    fn app_with_db() -> (App, Channels) {
        let dir = tempfile::tempdir().unwrap();
        let (db_tx, db_rx) = std::sync::mpsc::channel();
        let (player_tx, player_rx) = tokio::sync::mpsc::unbounded_channel();
        let app = App::new(
            PathBuf::from("test.db"),
            Ok(DbHandle { tx: db_tx }),
            PlayerHandle { tx: player_tx },
            EditSession::empty(dir.path().join("edit.json")),
            Vec::new(),
        );
        (
            app,
            Channels {
                db: db_rx,
                player: player_rx,
                _dir: dir,
            },
        )
    }

    #[test]
    fn typing_issues_a_search_per_revision() {
        let (mut app, ch) = app_with_db();
        app.update(key(KeyCode::Char('h')));
        app.update(key(KeyCode::Char('i')));
        assert_eq!(app.generation, 2);
        assert!(app.searching);
        let reqs: Vec<_> = ch.db.try_iter().collect();
        assert_eq!(reqs.len(), 2);
        assert_eq!(reqs[1].generation, 2);
        assert_eq!(reqs[1].query, "hi");
    }

    #[test]
    fn stale_results_are_dropped_and_current_ones_applied() {
        let (mut app, _ch) = app_with_db();
        app.update(key(KeyCode::Char('a')));
        app.update(key(KeyCode::Char('b')));
        app.update(Event::SearchDone {
            generation: 1,
            result: Ok(vec![hit("stale")]),
        });
        assert!(app.hits.is_empty());
        assert!(app.searching);
        app.update(Event::SearchDone {
            generation: 2,
            result: Ok(vec![hit("fresh"), hit("fresh2")]),
        });
        assert!(!app.searching);
        assert_eq!(app.hits.len(), 2);
        assert_eq!(app.table.selected(), Some(0));
    }

    #[test]
    fn clearing_the_query_clears_hits_without_a_request() {
        let (mut app, ch) = app_with_db();
        app.update(key(KeyCode::Char('a')));
        app.update(Event::SearchDone {
            generation: 1,
            result: Ok(vec![hit("one")]),
        });
        app.update(key(KeyCode::Backspace));
        assert!(app.hits.is_empty());
        assert!(!app.searching);
        assert_eq!(app.table.selected(), None);
        // Only the original "a" request went to the db.
        assert_eq!(ch.db.try_iter().count(), 1);
        // A late result for the cleared query is stale by generation.
        app.update(Event::SearchDone {
            generation: 1,
            result: Ok(vec![hit("late")]),
        });
        assert!(app.hits.is_empty());
    }

    #[test]
    fn selection_moves_within_bounds_and_q_is_just_text() {
        let (mut app, _ch) = app_with_db();
        app.update(key(KeyCode::Char('q')));
        assert!(!app.quit);
        assert_eq!(app.input.value(), "q");
        app.update(Event::SearchDone {
            generation: 1,
            result: Ok(vec![hit("a"), hit("b"), hit("c")]),
        });
        app.update(key(KeyCode::Down));
        app.update(key(KeyCode::Down));
        app.update(key(KeyCode::Down));
        assert_eq!(app.table.selected(), Some(2));
        app.update(key(KeyCode::PageUp));
        assert_eq!(app.table.selected(), Some(0));
        app.update(key(KeyCode::Esc));
        assert!(app.quit);
    }

    #[test]
    fn search_errors_surface_in_the_note() {
        let (mut app, _ch) = app_with_db();
        app.update(key(KeyCode::Char('a')));
        app.update(Event::SearchDone {
            generation: 1,
            result: Err("database disappeared".to_string()),
        });
        assert_eq!(app.note.as_deref(), Some("database disappeared"));
    }

    fn select_matched_hit(app: &mut App) {
        app.update(key(KeyCode::Char('x')));
        app.update(Event::SearchDone {
            generation: 1,
            result: Ok(vec![matched_hit()]),
        });
    }

    #[test]
    fn enter_loops_the_exact_match_span() {
        let (mut app, mut ch) = app_with_db();
        select_matched_hit(&mut app);
        app.update(key(KeyCode::Enter));
        match ch.player.try_recv().unwrap() {
            PlayerCmd::Audition(a) => {
                assert_eq!(a.master_path, "/masters/test1.mkv");
                assert!((a.start - 0.2).abs() < 1e-9 && (a.end - 0.9).abs() < 1e-9);
                assert!(a.looped);
                assert_eq!(a.label, "twenty five");
            }
            other => panic!("expected audition, got {other:?}"),
        }
    }

    #[test]
    fn ctrl_t_plays_with_context_clamped_at_zero() {
        let (mut app, mut ch) = app_with_db();
        select_matched_hit(&mut app);
        app.update(ctrl('t'));
        match ch.player.try_recv().unwrap() {
            PlayerCmd::Audition(a) => {
                // 0.2 - 0.5 clamps to 0; 0.9 + 0.5 = 1.4.
                assert!(a.start.abs() < 1e-9 && (a.end - 1.4).abs() < 1e-9);
                assert!(!a.looped);
            }
            other => panic!("expected audition, got {other:?}"),
        }
    }

    #[test]
    fn ctrl_u_plays_the_full_utterance_and_ctrl_p_stops() {
        let (mut app, mut ch) = app_with_db();
        select_matched_hit(&mut app);
        app.update(ctrl('u'));
        match ch.player.try_recv().unwrap() {
            PlayerCmd::Audition(a) => {
                assert!(a.start.abs() < 1e-9 && (a.end - 2.0).abs() < 1e-9);
                assert!(!a.looped);
                assert_eq!(a.label, "I said 25 ok");
            }
            other => panic!("expected audition, got {other:?}"),
        }
        app.update(ctrl('p'));
        assert!(matches!(ch.player.try_recv().unwrap(), PlayerCmd::Stop));
    }

    #[test]
    fn audition_without_hits_sends_nothing_and_updates_land_in_state() {
        let (mut app, mut ch) = app_with_db();
        app.update(key(KeyCode::Enter));
        app.update(ctrl('t'));
        assert!(ch.player.try_recv().is_err());
        app.update(Event::Player(PlayerUpdate::Ready {
            version: "mpv v0.41.0".to_string(),
        }));
        assert!(matches!(app.player_state, Some(PlayerUpdate::Ready { .. })));
    }

    #[test]
    fn ctrl_a_appends_the_matched_span_as_a_clip() {
        let (mut app, _ch) = app_with_db();
        select_matched_hit(&mut app);
        app.update(ctrl('a'));
        let clips = app.edit.clips();
        assert_eq!(clips.len(), 1);
        let clip = &clips[0];
        assert_eq!(clip.span.source, SourceId(1));
        assert!((clip.span.t_start - 0.2).abs() < 1e-9);
        assert!((clip.span.t_end - 0.9).abs() < 1e-9);
        assert_eq!(clip.label.as_deref(), Some("twenty five"));
        assert_eq!(clip.provenance, Some(ProvenanceRef::Utterance(1)));
        assert_eq!(app.edl_table.selected(), Some(0));
        // Manifest + source map entries came from the hit.
        assert_eq!(app.edit.edl.sources[&SourceId(1)].origin_id, "yt:1");
        assert!(app.edit.source_map.contains_key(&SourceId(1)));
        // The plan is recomputed and the autosave written.
        assert_eq!(app.plan.as_ref().unwrap().segments.len(), 1);
        assert!(app.edit.path.with_file_name("edit.json.autosave").exists());
    }

    fn app_with_three_clips() -> (App, Channels) {
        let (mut app, mut ch) = app_with_db();
        select_matched_hit(&mut app);
        app.update(ctrl('a'));
        app.update(ctrl('a'));
        app.update(ctrl('a'));
        // Drain anything the appends produced.
        while ch.player.try_recv().is_ok() {}
        app.update(key(KeyCode::Tab));
        assert!(matches!(app.focus, Pane::Edit));
        (app, ch)
    }

    #[test]
    fn edit_pane_keys_reorder_remove_and_trim() {
        let (mut app, mut ch) = app_with_three_clips();
        // Three identical spans never elide: three segments.
        assert_eq!(app.plan.as_ref().unwrap().segments.len(), 3);

        // Trim the selected (last) clip's end edge out by 25 ms.
        app.update(key(KeyCode::Char('}')));
        assert!((app.edit.clips()[2].span.t_end - 0.925).abs() < 1e-9);
        // A trim triggers a neighborhood replay window around the clip.
        match ch.player.try_recv().unwrap() {
            PlayerCmd::Preview(p) => match p.seek {
                PreviewSeek::Window { start, end } => {
                    // Clip 2 spans output [1.4, 2.125]; ±0.5 clamped.
                    assert!((start - 0.9).abs() < 1e-6, "{start}");
                    assert!((end - 2.125).abs() < 1e-6, "{end}");
                }
                other => panic!("expected a window, got {other:?}"),
            },
            other => panic!("expected preview, got {other:?}"),
        }

        // Move it up; selection follows.
        app.update(key(KeyCode::Char('K')));
        assert_eq!(app.edl_table.selected(), Some(1));
        assert!((app.edit.clips()[1].span.t_end - 0.925).abs() < 1e-9);

        // Remove it.
        app.update(key(KeyCode::Char('d')));
        assert_eq!(app.edit.clips().len(), 2);
        assert_eq!(app.edl_table.selected(), Some(1));

        // Undo restores it, redo removes it again.
        app.update(key(KeyCode::Char('u')));
        assert_eq!(app.edit.clips().len(), 3);
        app.update(key(KeyCode::Char('U')));
        assert_eq!(app.edit.clips().len(), 2);

        // Rejected trims (start below zero) do nothing.
        let before = app.edit.clips()[0].span.t_start;
        for _ in 0..50 {
            app.update(key(KeyCode::Char('<')));
        }
        assert!(app.edit.clips()[0].span.t_start >= 0.0);
        let after = app.edit.clips()[0].span.t_start;
        assert!(after <= before);
    }

    #[test]
    fn space_starts_preview_then_toggles_and_enter_seeks_to_clip() {
        let (mut app, mut ch) = app_with_three_clips();
        app.update(key(KeyCode::Char(' ')));
        match ch.player.try_recv().unwrap() {
            PlayerCmd::Preview(p) => {
                assert!(p.uri.starts_with("edl://"));
                assert!(matches!(p.seek, PreviewSeek::From(t) if t.abs() < 1e-9));
            }
            other => panic!("expected preview, got {other:?}"),
        }
        // Preview is loaded now: Space toggles pause.
        app.update(key(KeyCode::Char(' ')));
        assert!(matches!(
            ch.player.try_recv().unwrap(),
            PlayerCmd::TogglePause
        ));

        // Enter plays from the selected clip's output position (the last
        // append left clip 2 selected; Up moves to clip 1).
        app.update(key(KeyCode::Up));
        app.update(key(KeyCode::Enter));
        match ch.player.try_recv().unwrap() {
            PlayerCmd::Preview(p) => {
                // Three 0.7 s clips: clip 1 starts at output 0.7.
                assert!(matches!(p.seek, PreviewSeek::From(t) if (t - 0.7).abs() < 1e-6));
            }
            other => panic!("expected preview, got {other:?}"),
        }

        // A mutation while the preview is active resumes in place.
        app.update(key(KeyCode::Char('d')));
        match ch.player.try_recv().unwrap() {
            PlayerCmd::Preview(p) => {
                assert!(matches!(p.seek, PreviewSeek::Resume { max } if (max - 1.4).abs() < 1e-6));
            }
            other => panic!("expected preview, got {other:?}"),
        }
    }

    #[test]
    fn save_key_writes_the_edit_file() {
        let (mut app, _ch) = app_with_three_clips();
        app.update(key(KeyCode::Char('s')));
        assert!(app.edit.path.exists());
        assert!(!app.edit.dirty());
        assert!(app.note.as_deref().unwrap().contains("saved"));
        // The autosave is gone after an explicit save.
        assert!(!app.edit.path.with_file_name("edit.json.autosave").exists());
    }

    #[test]
    fn removing_the_last_clip_stops_an_active_preview() {
        let (mut app, mut ch) = app_with_db();
        select_matched_hit(&mut app);
        app.update(ctrl('a'));
        app.update(key(KeyCode::Tab));
        app.update(key(KeyCode::Char(' ')));
        while ch.player.try_recv().is_ok() {}
        app.update(key(KeyCode::Char('d')));
        assert!(app.edit.clips().is_empty());
        assert!(matches!(ch.player.try_recv().unwrap(), PlayerCmd::Stop));
        // With nothing to play, Space does nothing.
        app.update(key(KeyCode::Char(' ')));
        assert!(ch.player.try_recv().is_err());
    }
}
