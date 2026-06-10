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
    pub quit: bool,
    db: Option<DbHandle>,
}

impl App {
    pub fn new(corpus_db: PathBuf, db: Result<DbHandle, String>) -> Self {
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
            quit: false,
            db,
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
    use super::*;
    use dipho_core::span::SourceId;

    fn key(code: KeyCode) -> Event {
        Event::Term(TermEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    fn hit(text: &str) -> SearchHit {
        SearchHit {
            utterance_id: 1,
            source_id: SourceId(1),
            origin: "test".to_string(),
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

    fn app_with_db() -> (
        App,
        std::sync::mpsc::Receiver<super::super::db::SearchRequest>,
    ) {
        let (tx, rx) = std::sync::mpsc::channel();
        let app = App::new(PathBuf::from("test.db"), Ok(DbHandle { tx }));
        (app, rx)
    }

    #[test]
    fn typing_issues_a_search_per_revision() {
        let (mut app, rx) = app_with_db();
        app.update(key(KeyCode::Char('h')));
        app.update(key(KeyCode::Char('i')));
        assert_eq!(app.generation, 2);
        assert!(app.searching);
        let reqs: Vec<_> = rx.try_iter().collect();
        assert_eq!(reqs.len(), 2);
        assert_eq!(reqs[1].generation, 2);
        assert_eq!(reqs[1].query, "hi");
    }

    #[test]
    fn stale_results_are_dropped_and_current_ones_applied() {
        let (mut app, _rx) = app_with_db();
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
        let (mut app, rx) = app_with_db();
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
        assert_eq!(rx.try_iter().count(), 1);
        // A late result for the cleared query is stale by generation.
        app.update(Event::SearchDone {
            generation: 1,
            result: Ok(vec![hit("late")]),
        });
        assert!(app.hits.is_empty());
    }

    #[test]
    fn selection_moves_within_bounds_and_q_is_just_text() {
        let (mut app, _rx) = app_with_db();
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
        let (mut app, _rx) = app_with_db();
        app.update(key(KeyCode::Char('a')));
        app.update(Event::SearchDone {
            generation: 1,
            result: Err("database disappeared".to_string()),
        });
        assert_eq!(app.note.as_deref(), Some("database disappeared"));
    }
}
