//! ratatui app shell: Elm-flavored single event loop on tokio. One App
//! state struct, one merged Event enum, all producers into one mpsc, a
//! single consumer that coalesces event bursts and renders once per batch.

mod app;
mod db;
mod event;
mod ui;

use std::path::PathBuf;

use anyhow::Result;
use crossterm::event::EventStream;
use futures_util::StreamExt;
use tokio::sync::mpsc;

use app::App;
use event::Event;

pub fn run(corpus_db: PathBuf) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run_app(corpus_db))
}

async fn run_app(corpus_db: PathBuf) -> Result<()> {
    // Raw mode before any producer starts, so no input is ever read
    // line-buffered.
    let mut terminal = ratatui::init();

    let (tx, mut rx) = mpsc::unbounded_channel::<Event>();
    let term_tx = tx.clone();
    tokio::spawn(async move {
        let mut stream = EventStream::new();
        while let Some(Ok(term_event)) = stream.next().await {
            if term_tx.send(Event::Term(term_event)).is_err() {
                break;
            }
        }
    });
    let mut app = App::new(corpus_db.clone(), db::spawn(&corpus_db, tx.clone()));

    let result = event_loop(&mut terminal, &mut app, &mut rx).await;
    ratatui::restore();
    result
}

async fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    rx: &mut mpsc::UnboundedReceiver<Event>,
) -> Result<()> {
    terminal.draw(|frame| ui::draw(frame, app))?;
    while let Some(event) = rx.recv().await {
        app.update(event);
        // Coalesce whatever is already queued (keystroke bursts, a search
        // result racing a keystroke) into one render.
        while let Ok(event) = rx.try_recv() {
            app.update(event);
        }
        if app.quit {
            return Ok(());
        }
        terminal.draw(|frame| ui::draw(frame, app))?;
    }
    Ok(())
}
