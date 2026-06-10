//! App state and the update half of the Elm loop: pure-ish event handling,
//! no rendering and no I/O beyond posting search requests.

use std::path::PathBuf;

use crossterm::event::{Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use dipho_core::corpus::SearchHit;
use ratatui::widgets::TableState;
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler;

use super::db::DbHandle;
use super::event::Event;
use super::player::{Audition, PlayerHandle, PlayerUpdate};

/// Context padding around the matched span for the play-with-context key.
const CONTEXT_PAD: f64 = 0.5;

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

pub struct App {
    pub corpus_db: PathBuf,
    pub input: Input,
    pub hits: Vec<SearchHit>,
    pub table: TableState,
    /// Monotonic query revision; results tagged with an older generation
    /// are stale and dropped.
    pub generation: u64,
    pub searching: bool,
    /// A problem to surface in the status line (no corpus, search error).
    pub note: Option<String>,
    /// Latest audition player status (None until mpv reports in).
    pub player_state: Option<PlayerUpdate>,
    pub quit: bool,
    db: Option<DbHandle>,
    player: PlayerHandle,
}

impl App {
    pub fn new(corpus_db: PathBuf, db: Result<DbHandle, String>, player: PlayerHandle) -> Self {
        let (db, note) = match db {
            Ok(handle) => (Some(handle), None),
            Err(message) => (None, Some(message)),
        };
        Self {
            corpus_db,
            input: Input::default(),
            hits: Vec::new(),
            table: TableState::default(),
            generation: 0,
            searching: false,
            note,
            player_state: None,
            quit: false,
            db,
            player,
        }
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
            KeyCode::Esc => self.quit = true,
            KeyCode::Char('c') if ctrl => self.quit = true,
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Down => self.move_selection(1),
            KeyCode::PageUp => self.move_selection(-10),
            KeyCode::PageDown => self.move_selection(10),
            KeyCode::Enter => self.audition(AuditionKind::LoopExact),
            KeyCode::Char('t') if ctrl => self.audition(AuditionKind::Context),
            KeyCode::Char('u') if ctrl => self.audition(AuditionKind::FullUtterance),
            KeyCode::Char('p') if ctrl => self.player.stop(),
            _ => {
                if let Some(change) = self.input.handle_event(&TermEvent::Key(key))
                    && change.value
                {
                    self.issue_search();
                }
            }
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
            master_path: hit.master_path.clone(),
            start,
            end,
            looped,
            label,
        });
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
}

#[cfg(test)]
mod tests {
    use super::super::player::PlayerCmd;
    use super::*;
    use dipho_core::corpus::WordSpan;
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

    fn hit(text: &str) -> SearchHit {
        SearchHit {
            utterance_id: 1,
            source_id: SourceId(1),
            origin: "test".to_string(),
            master_path: "/masters/test.mkv".to_string(),
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
    }

    fn app_with_db() -> (App, Channels) {
        let (db_tx, db_rx) = std::sync::mpsc::channel();
        let (player_tx, player_rx) = tokio::sync::mpsc::unbounded_channel();
        let app = App::new(
            PathBuf::from("test.db"),
            Ok(DbHandle { tx: db_tx }),
            PlayerHandle { tx: player_tx },
        );
        (
            app,
            Channels {
                db: db_rx,
                player: player_rx,
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
                assert_eq!(a.master_path, "/masters/test.mkv");
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
}
