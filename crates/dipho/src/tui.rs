//! ratatui app shell. Placeholder screen until the search UI lands
//! (milestone: word search).

use crossterm::event::{self, Event, KeyCode};
use ratatui::Frame;
use ratatui::widgets::{Block, Paragraph};

pub fn run() -> anyhow::Result<()> {
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal);
    ratatui::restore();
    result
}

fn event_loop(terminal: &mut ratatui::DefaultTerminal) -> anyhow::Result<()> {
    loop {
        terminal.draw(draw)?;
        if let Event::Key(key) = event::read()?
            && matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
        {
            return Ok(());
        }
    }
}

fn draw(frame: &mut Frame) {
    let placeholder = Paragraph::new("dipho — nothing here yet. Press q to quit.")
        .block(Block::bordered().title("dipho"));
    frame.render_widget(placeholder, frame.area());
}
