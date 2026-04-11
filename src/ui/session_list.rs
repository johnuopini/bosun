//! Session list panel. Phase 1: name + attached marker, hardcoded colors.
//! Theme integration comes in Phase 4.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;

use crate::app::AppState;

const BG: Color = Color::Rgb(11, 13, 18);
const PANEL: Color = Color::Rgb(17, 20, 27);
const SELECTION_BG: Color = Color::Rgb(30, 36, 51);
const ACCENT: Color = Color::Rgb(124, 92, 255);
const TEXT: Color = Color::Rgb(230, 233, 239);
const MUTED: Color = Color::Rgb(124, 132, 149);
const GREEN: Color = Color::Rgb(98, 217, 140);

pub fn render(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let block = Block::default().style(Style::default().bg(BG));

    let lines: Vec<Line<'_>> = if state.sessions.is_empty() {
        vec![
            Line::from(""),
            Line::from(Span::styled(
                "  no tmux sessions",
                Style::default().fg(MUTED),
            )),
            Line::from(Span::styled(
                "  (press r to refresh)",
                Style::default().fg(MUTED),
            )),
        ]
    } else {
        state
            .sessions
            .iter()
            .enumerate()
            .map(|(i, s)| render_row(i, s, i == state.selected))
            .collect()
    };

    let p = Paragraph::new(lines).block(block);
    frame.render_widget(p, area);
}

fn render_row(_idx: usize, session: &crate::tmux::TmuxSession, selected: bool) -> Line<'static> {
    let marker = if selected { "▌" } else { " " };
    let name = session.name.clone();
    let attached = session.attached;

    let row_bg = if selected { SELECTION_BG } else { PANEL };

    let marker_style = if selected {
        Style::default().fg(ACCENT).bg(row_bg)
    } else {
        Style::default().fg(row_bg).bg(row_bg)
    };

    let name_style = if selected {
        Style::default()
            .fg(TEXT)
            .bg(row_bg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(TEXT).bg(row_bg)
    };

    let attached_style = Style::default().fg(GREEN).bg(row_bg);

    let mut spans = vec![
        Span::styled(format!(" {} ", marker), marker_style),
        Span::styled(name, name_style),
    ];

    if attached {
        spans.push(Span::styled("  •", attached_style));
        spans.push(Span::styled(
            " attached",
            Style::default().fg(MUTED).bg(row_bg),
        ));
    }

    Line::from(spans)
}
