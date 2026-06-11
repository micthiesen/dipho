//! The render half: the search input, the results table (speaker and
//! confidence columns, hit highlighted inside its utterance), the edit
//! pane (clip list with output times), and a status line. Pure function of
//! the App.

use dipho_core::corpus::SearchHit;
use dipho_core::edl::Transform;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Paragraph, Row, Table};

use super::app::{App, Pane};
use super::player::PlayerUpdate;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let [input_area, results_area, edl_area, status_area] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Fill(3),
        Constraint::Fill(2),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    let pane_block = |title: String, focused: bool| {
        let block = Block::bordered().title(title);
        if focused {
            block.border_style(Style::new().fg(Color::Cyan))
        } else {
            block
        }
    };
    let search_focus = matches!(app.focus, Pane::Search);

    let inner_width = input_area.width.saturating_sub(2) as usize;
    let scroll = app.input.visual_scroll(inner_width);
    let input = Paragraph::new(app.input.value())
        .scroll((0, scroll as u16))
        .block(pane_block("Search".to_string(), search_focus));
    frame.render_widget(input, input_area);
    if search_focus {
        let cursor_x = (app.input.visual_cursor().saturating_sub(scroll)) as u16;
        frame.set_cursor_position((
            input_area.x + 1 + cursor_x.min(input_area.width.saturating_sub(2)),
            input_area.y + 1,
        ));
    }

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
            Cell::from(format!("{}", hit.source.id.0)),
            Cell::from(format!("{:8.2}s", hit.t_start)),
            Cell::from(match (&hit.speaker, hit.multi_speaker) {
                // "+": a second speaker overlaps >20% of the utterance.
                (Some(speaker), true) => format!("{speaker}+"),
                (Some(speaker), false) => speaker.clone(),
                (None, _) => "—".to_string(),
            }),
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
    .block(pane_block(title, search_focus));
    frame.render_stateful_widget(table, results_area, &mut app.table);

    draw_edl(frame, app, edl_area);

    let mut status = match (&app.note, app.searching) {
        (Some(note), _) => vec![note.clone().yellow(), Span::raw("   ")],
        (None, true) => vec!["searching…".yellow(), Span::raw("   ")],
        (None, false) => vec![Span::raw(format!("corpus: {}   ", app.corpus_db.display()))],
    };
    status.push(player_span(&app.player_state));
    let hints = match app.focus {
        Pane::Search => {
            "   Enter loop  ^T context  ^U utterance  ^A add clip  ^P stop  Tab edit  Esc quit"
        }
        Pane::Edit => {
            "   Space play  Enter from clip  ,.<> start  []{} end  J/K move  d del  u/U undo  s save  Tab search"
        }
    };
    status.push(Span::raw(hints).dark_gray());
    frame.render_widget(Paragraph::new(Line::from(status)), status_area);
}

fn draw_edl(frame: &mut Frame, app: &mut App, area: ratatui::layout::Rect) {
    let dirty = if app.edit.dirty() { " *" } else { "" };
    let total = app
        .plan
        .as_ref()
        .map(|p| format!("  {:.2}s", p.total_duration))
        .unwrap_or_default();
    let title = format!(
        "Edit — {}{dirty} — {} clip{}{total}",
        app.edit.path.display(),
        app.edit.clips().len(),
        if app.edit.clips().len() == 1 { "" } else { "s" },
    );
    let rows = app.edit.clips().iter().enumerate().map(|(i, clip)| {
        let out = app
            .plan
            .as_ref()
            .and_then(|p| p.clip_output.get(i))
            .map(|(start, _)| format!("{start:8.2}s"))
            .unwrap_or_else(|| "       —".to_string());
        Row::new(vec![
            Cell::from(format!("{i:3}")),
            Cell::from(out),
            Cell::from(format!("{}", clip.span.source.0)),
            Cell::from(format!("{:.3}–{:.3}", clip.span.t_start, clip.span.t_end)),
            Cell::from(format!("{:6.3}s", clip.span.duration())),
            Cell::from(badges(&clip.transforms)),
            Cell::from(clip.label.clone().unwrap_or_default()),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(3),
            Constraint::Length(9),
            Constraint::Length(3),
            Constraint::Length(17),
            Constraint::Length(8),
            Constraint::Length(6),
            Constraint::Min(10),
        ],
    )
    .header(Row::new(["#", "out", "src", "span", "dur", "fx", "clip"]).style(Style::new().bold()))
    .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
    .block({
        let block = Block::bordered().title(title);
        if matches!(app.focus, Pane::Edit) {
            block.border_style(Style::new().fg(Color::Cyan))
        } else {
            block
        }
    });
    frame.render_stateful_widget(table, area, &mut app.edl_table);
}

/// Compact transform badges. Loop/Stutter preview natively; the rest are
/// render-only, so a badge is the cue that preview plays them plain.
fn badges(transforms: &[Transform]) -> String {
    transforms
        .iter()
        .map(|t| match t {
            Transform::Loop { count } => format!("⟳{count}"),
            Transform::Stutter { .. } => "st".to_string(),
            Transform::Pitch { .. } => "♯".to_string(),
            Transform::Speed { .. } => "spd".to_string(),
            Transform::Reverse => "rev".to_string(),
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// The audition player's corner of the status line.
fn player_span(state: &Option<PlayerUpdate>) -> Span<'static> {
    match state {
        None => Span::raw("mpv: starting…").dark_gray(),
        Some(PlayerUpdate::Ready { version }) => Span::raw(version.clone()).dark_gray(),
        Some(PlayerUpdate::Playing {
            label,
            looped,
            seek_ms,
        }) => {
            let mut text = format!("▶ {label}");
            if *looped {
                text.push_str(" ⟳");
            }
            if let Some(ms) = seek_ms {
                text.push_str(&format!("  seek {ms} ms"));
            }
            Span::raw(text).green()
        }
        Some(PlayerUpdate::Done) => Span::raw("■ done"),
        Some(PlayerUpdate::Stopped) => Span::raw("■ stopped"),
        Some(PlayerUpdate::Failed(e)) => Span::raw(format!("mpv: {e}")).red(),
    }
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
    use crate::tui::edit::EditSession;
    use dipho_core::corpus::WordSpan;
    use dipho_core::edl::{Clip, CorpusSource};
    use dipho_core::span::{Channel, SourceId, Span as TimeSpan};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;

    fn render(app: &mut App) -> Vec<String> {
        // Wide enough that the edit pane title (which carries a tempdir
        // path) isn't truncated.
        let mut terminal = Terminal::new(TestBackend::new(140, 20)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect()
            })
            .collect()
    }

    fn test_app(dir: &std::path::Path) -> App {
        let (player_tx, _player_rx) = tokio::sync::mpsc::unbounded_channel();
        App::new(
            PathBuf::from("test.db"),
            Err("no corpus here".to_string()),
            crate::tui::player::PlayerHandle { tx: player_tx },
            EditSession::empty(dir.join("edit.json")),
            Vec::new(),
        )
    }

    #[test]
    fn renders_hits_with_highlighted_matches() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = test_app(dir.path());
        app.hits = vec![SearchHit {
            utterance_id: 1,
            source: CorpusSource {
                id: SourceId(1),
                origin: "test".to_string(),
                origin_id: "yt:1".to_string(),
                master_path: "master.mkv".to_string(),
                master_hash: "hash".to_string(),
                duration: 10.0,
                fps: Some(30.0),
            },
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

        let rendered = render(&mut app);
        let all = rendered.join("\n");
        assert!(all.contains("twenty five plus twenty five"));
        assert!(all.contains("Liquid"));
        assert!(all.contains("0.80"));
        assert!(all.contains("no corpus here"));

        // The matched tokens are highlighted; the connector "plus" is not.
        // (Same size as render() so positions line up.)
        let mut terminal = Terminal::new(TestBackend::new(140, 20)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
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

    #[test]
    fn renders_the_edit_pane_with_clips_and_output_times() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = test_app(dir.path());
        let source = CorpusSource {
            id: SourceId(1),
            origin: "test".to_string(),
            origin_id: "yt:1".to_string(),
            master_path: "/masters/m.mkv".to_string(),
            master_hash: "hash".to_string(),
            duration: 100.0,
            fps: Some(30.0),
        };
        for (t_start, t_end, label) in [(1.0, 2.0, "twenty"), (5.0, 5.5, "five")] {
            app.edit.append(
                Clip {
                    span: TimeSpan {
                        source: SourceId(1),
                        t_start,
                        t_end,
                        channel: Channel::Both,
                    },
                    transforms: vec![dipho_core::edl::Transform::Loop { count: 2 }],
                    provenance: None,
                    label: Some(label.to_string()),
                },
                &source,
            );
        }
        // Recompute the plan the way a mutation would.
        app.edit.autosave().unwrap();
        let (plan, _) = app.edit.compile().unwrap();
        app.plan = Some(plan);

        let all = render(&mut app).join("\n");
        assert!(all.contains("2 clips"), "{all}");
        assert!(all.contains("3.00s"), "{all}"); // 2×1.0 + 2×0.5 looped
        assert!(all.contains("1.000–2.000"), "{all}");
        assert!(all.contains("⟳2"), "{all}");
        assert!(all.contains("twenty"), "{all}");
        assert!(all.contains("*"), "dirty marker: {all}");
    }
}
