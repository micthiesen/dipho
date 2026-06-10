//! The corpus reader: a dedicated thread owning a read-only connection
//! (rusqlite is blocking; the TUI loop must never be). Requests supersede —
//! only the newest pending query runs — and results return through the one
//! event mpsc, tagged with their generation.

use std::path::Path;
use std::sync::mpsc as std_mpsc;

use tokio::sync::mpsc::UnboundedSender;

use super::event::Event;

pub struct SearchRequest {
    pub generation: u64,
    pub query: String,
}

pub struct DbHandle {
    pub(super) tx: std_mpsc::Sender<SearchRequest>,
}

impl DbHandle {
    pub fn request(&self, generation: u64, query: String) {
        // A send error means the db thread died; the app keeps running and
        // simply stops getting results.
        let _ = self.tx.send(SearchRequest { generation, query });
    }
}

/// Open the corpus and spawn the reader thread. On failure the TUI still
/// runs, surfacing the problem in its status line instead of results.
pub fn spawn(corpus_db: &Path, events: UnboundedSender<Event>) -> Result<DbHandle, String> {
    let corpus = crate::search::open_corpus(corpus_db).map_err(|e| e.to_string())?;
    let (tx, rx) = std_mpsc::channel::<SearchRequest>();
    std::thread::spawn(move || {
        while let Ok(mut req) = rx.recv() {
            // Drain the backlog: keystrokes can outpace queries, and only
            // the newest revision matters.
            while let Ok(newer) = rx.try_recv() {
                req = newer;
            }
            let result = corpus.search(&req.query).map_err(|e| e.to_string());
            let done = Event::SearchDone {
                generation: req.generation,
                result,
            };
            if events.send(done).is_err() {
                break;
            }
        }
    });
    Ok(DbHandle { tx })
}
