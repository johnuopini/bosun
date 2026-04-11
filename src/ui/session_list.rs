//! Session list panel. Phase 2: status glyphs + color-coded per state.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;

use crate::app::AppState;
use crate::tmux::detector::Status;
use crate::tmux::session::SessionView;

const BG: Color = Color::Rgb(11, 13, 18);
const PANEL: Color = Color::Rgb(17, 20, 27);
const SELECTION_BG: Color = Color::Rgb(30, 36, 51);
const ACCENT: Color = Color::Rgb(124, 92, 255);
const TEXT: Color = Color::Rgb(230, 233, 239);
const MUTED: Color = Color::Rgb(124, 132, 149);

const STATUS_RUNNING: Color = Color::Rgb(98, 217, 140);
const STATUS_WAITING: Color = Color::Rgb(244, 193, 105);
const STATUS_IDLE: Color = Color::Rgb(124, 132, 149);
const STATUS_ERROR: Color = Color::Rgb(255, 93, 107);

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
            .map(|(i, v)| render_row(v, i == state.selected))
            .collect()
    };

    let p = Paragraph::new(lines).block(block);
    frame.render_widget(p, area);
}

fn status_color(status: Status) -> Color {
    match status {
        Status::Running => STATUS_RUNNING,
        Status::Waiting => STATUS_WAITING,
        Status::Idle | Status::Unknown => STATUS_IDLE,
        Status::Error => STATUS_ERROR,
    }
}

fn render_row(view: &SessionView, selected: bool) -> Line<'static> {
    let marker = if selected { "▌" } else { " " };
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

    let status_style = Style::default().fg(status_color(view.status)).bg(row_bg);
    let glyph = view.status.glyph().to_string();
    let name = view.name().to_string();
    let windows = view.session.windows;

    let mut spans = vec![
        Span::styled(format!(" {} ", marker), marker_style),
        Span::styled(glyph, status_style),
        Span::styled("  ", Style::default().bg(row_bg)),
        Span::styled(name, name_style),
        Span::styled(
            format!("  {}w", windows),
            Style::default().fg(MUTED).bg(row_bg),
        ),
    ];

    if view.session.attached {
        spans.push(Span::styled(
            "  •attached",
            Style::default().fg(STATUS_RUNNING).bg(row_bg),
        ));
    }

    Line::from(spans)
}
