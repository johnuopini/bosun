//! Bottom status bar. Shows key hints + any warning string from the app.

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::AppState;
use crate::ui::Theme;

pub fn render(frame: &mut Frame<'_>, area: Rect, state: &AppState, theme: &Theme) {
    let bg = theme.panel_alt;
    let left = Line::from(vec![
        Span::styled(" bosun ", Style::default().fg(theme.text).bg(theme.accent)),
        Span::styled(
            format!(" v{} ", env!("CARGO_PKG_VERSION")),
            Style::default().fg(theme.text_muted).bg(bg),
        ),
        if let Some(w) = &state.warning {
            Span::styled(w.clone(), Style::default().fg(theme.status_waiting).bg(bg))
        } else {
            Span::styled(
                format!("{} sessions", state.sessions.len()),
                Style::default().fg(theme.text_muted).bg(bg),
            )
        },
    ]);

    let right = "↵ attach · n new · g group · 1-9 move · r ren · d kill · t theme · ? help · q quit ";
    let hint_style = Style::default().fg(theme.text_muted).bg(bg);

    let width = area.width as usize;
    let left_w = line_width(&left);
    let right_w = right.chars().count();
    let pad = width.saturating_sub(left_w + right_w);

    let mut full_spans = left.spans;
    full_spans.push(Span::styled(" ".repeat(pad), Style::default().bg(bg)));
    full_spans.push(Span::styled(right.to_string(), hint_style));

    let p = Paragraph::new(Line::from(full_spans)).style(Style::default().bg(bg));
    frame.render_widget(p, area);
}

fn line_width(line: &Line<'_>) -> usize {
    line.spans.iter().map(|s| s.content.chars().count()).sum()
}
