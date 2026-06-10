//! The one merged event enum. All producers (terminal, db) feed a single
//! mpsc; the app loop is the single consumer.

use dipho_core::corpus::SearchHit;

pub enum Event {
    /// A terminal event from crossterm's event stream.
    Term(crossterm::event::Event),
    /// A finished corpus search. `generation` ties the result to the query
    /// revision that requested it; stale results are dropped by the app.
    SearchDone {
        generation: u64,
        result: Result<Vec<SearchHit>, String>,
    },
}
