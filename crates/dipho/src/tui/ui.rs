//! The render half: draw the search input, the results table (speaker and
//! confidence columns, hit highlighted inside its utterance), and a status
//! line. Pure function of the App.

use dipho_core::corpus::SearchHit;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Paragraph, Row, Table};

use super::app::App;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let [input_area, results_area, status_area] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    let inner_width = input_area.width.saturating_sub(2) as usize;
    let scroll = app.input.visual_scroll(inner_width);
    let input = Paragraph::new(app.input.value())
        .scroll((0, scroll as u16))
        .block(Block::bordered().title("Search"));
    frame.render_widget(input, input_area);
    let cursor_x = (app.input.visual_cursor().saturating_sub(scroll)) as u16;
    frame.set_cursor_position((
        input_area.x + 1 + cursor_x.min(input_area.width.saturating_sub(2)),
        input_area.y + 1,
    ));

    let occurrences: usize = app.hits.iter().map(|h| h.matches.len()).sum();
    let title = if app.input.value().trim().is_empty() {
        "Results".to_string()
    } else {
        format!(
            "Results — {occurrences} match{} in {} utterance{}",
            if occurrences == 1 { "" } else { "es" },
            app.hits.len(),
            if app.hits.len() == 1 { "" } else { "s" },
        )
    };
    let rows = app.hits.iter().map(|hit| {
        Row::new(vec![
            Cell::from(format!("{}", hit.source_id.0)),
            Cell::from(format!("{:8.2}s", hit.t_start)),
            Cell::from(hit.speaker.clone().unwrap_or_else(|| "—".to_string())),
            Cell::from(
                hit.confidence
                    .map(|c| format!("{c:.2}"))
                    .unwrap_or_else(|| "—".to_string()),
            ),
            Cell::from(highlighted_line(hit)),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(3),
            Constraint::Length(9),
            Constraint::Length(14),
            Constraint::Length(4),
            Constraint::Min(10),
        ],
    )
    .header(Row::new(["src", "time", "speaker", "conf", "utterance"]).style(Style::new().bold()))
    .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
    .block(Block::bordered().title(title));
    frame.render_stateful_widget(table, results_area, &mut app.table);

    let status = match (&app.note, app.searching) {
        (Some(note), _) => Line::from(note.clone().red()),
        (None, true) => Line::from("searching…".yellow()),
        (None, false) => Line::from(vec![
            Span::raw(format!("corpus: {}", app.corpus_db.display())),
            Span::raw("   ↑/↓ select   Esc quit").dark_gray(),
        ]),
    };
    frame.render_widget(Paragraph::new(status), status_area);
}

/// The utterance's normalized token stream as one line, every matched
/// token highlighted.
fn highlighted_line(hit: &SearchHit) -> Line<'static> {
    let highlight = Style::new().fg(Color::Yellow).bold();
    let mut spans = Vec::with_capacity(hit.words.len() * 2);
    for (i, word) in hit.words.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" "));
        }
        if hit.matches.iter().any(|m| m.contains(&i)) {
            spans.push(Span::styled(word.text.clone(), highlight));
        } else {
            spans.push(Span::raw(word.text.clone()));
        }
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::App;
    use dipho_core::corpus::WordSpan;
    use dipho_core::span::SourceId;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;

    #[test]
    fn renders_hits_with_highlighted_matches() {
        let mut app = App::new(PathBuf::from("test.db"), Err("no corpus here".to_string()));
        app.hits = vec![SearchHit {
            utterance_id: 1,
            source_id: SourceId(1),
            origin: "test".to_string(),
            t_start: 2.0,
            t_end: 4.0,
            text: "25 plus 25".to_string(),
            speaker: Some("Liquid".to_string()),
            multi_speaker: false,
            confidence: Some(0.8),
            words: ["twenty", "five", "plus", "twenty", "five"]
                .iter()
                .enumerate()
                .map(|(i, w)| WordSpan {
                    text: w.to_string(),
                    t_start: i as f64,
                    t_end: i as f64 + 1.0,
                    confidence: None,
                })
                .collect(),
            matches: vec![0..2, 3..5],
        }];
        app.table.select(Some(0));

        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let buffer = terminal.backend().buffer();
        let rendered: Vec<String> = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect()
            })
            .collect();
        let all = rendered.join("\n");
        assert!(all.contains("twenty five plus twenty five"));
        assert!(all.contains("Liquid"));
        assert!(all.contains("0.80"));
        assert!(all.contains("no corpus here"));

        // The matched tokens are highlighted; the connector "plus" is not.
        let row = rendered
            .iter()
            .position(|line| line.contains("twenty five plus"))
            .unwrap() as u16;
        let line = &rendered[row as usize];
        let style_at = |s: &str| {
            let x = line.find(s).unwrap() as u16;
            buffer[(x, row)].style().fg
        };
        assert_eq!(style_at("twenty"), Some(Color::Yellow));
        assert_eq!(style_at("plus"), Some(Color::Reset));
    }
}
