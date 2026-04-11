//! Session list panel. Phase 2: status glyphs + color-coded per state.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;

use crate::app::AppState;
use crate::tmux::detector::Status;
use crate::tmux::session::SessionView;
use crate::ui::Theme;

pub fn render(frame: &mut Frame<'_>, area: Rect, state: &AppState, theme: &Theme) {
    let block = Block::default().style(Style::default().bg(theme.bg));

    let lines: Vec<Line<'_>> = if state.sessions.is_empty() {
        vec![
            Line::from(""),
            Line::from(Span::styled(
                "  no tmux sessions",
                Style::default().fg(theme.text_muted),
            )),
            Line::from(Span::styled(
                "  (press r to refresh)",
                Style::default().fg(theme.text_muted),
            )),
        ]
    } else {
        state
            .sessions
            .iter()
            .enumerate()
            .map(|(i, v)| render_row(v, i == state.selected, theme))
            .collect()
    };

    let p = Paragraph::new(lines).block(block);
    frame.render_widget(p, area);
}

fn status_color(status: Status, theme: &Theme) -> Color {
    match status {
        Status::Running => theme.status_running,
        Status::Waiting => theme.status_waiting,
        Status::Idle | Status::Unknown => theme.status_idle,
        Status::Error => theme.status_error,
    }
}

fn render_row(view: &SessionView, selected: bool, theme: &Theme) -> Line<'static> {
    let marker = if selected { "▌" } else { " " };
    let row_bg = if selected {
        theme.selection_bg
    } else {
        theme.panel
    };

    let marker_style = if selected {
        Style::default().fg(theme.accent).bg(row_bg)
    } else {
        Style::default().fg(row_bg).bg(row_bg)
    };

    let name_style = if selected {
        Style::default()
            .fg(theme.text)
            .bg(row_bg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.text).bg(row_bg)
    };

    let status_style = Style::default()
        .fg(status_color(view.status, theme))
        .bg(row_bg);
    let glyph = view.status.glyph().to_string();
    let name = view.display().to_string();
    let windows = view.session.windows;

    let mut spans = vec![
        Span::styled(format!(" {} ", marker), marker_style),
        Span::styled(glyph, status_style),
        Span::styled("  ", Style::default().bg(row_bg)),
        Span::styled(name, name_style),
        Span::styled(
            format!("  {}w", windows),
            Style::default().fg(theme.text_muted).bg(row_bg),
        ),
    ];

    if view.session.attached {
        spans.push(Span::styled(
            "  •attached",
            Style::default().fg(theme.status_running).bg(row_bg),
        ));
    }

    Line::from(spans)
}
