//! Bottom status bar. Shows key hints + any warning string from the app.

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::AppState;

const BG: Color = Color::Rgb(19, 23, 34);
const TEXT: Color = Color::Rgb(230, 233, 239);
const MUTED: Color = Color::Rgb(124, 132, 149);
const ACCENT: Color = Color::Rgb(124, 92, 255);
const WARN: Color = Color::Rgb(244, 193, 105);

pub fn render(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let left = Line::from(vec![
        Span::styled(" bosun ", Style::default().fg(TEXT).bg(ACCENT)),
        Span::styled(" ", Style::default().bg(BG)),
        if let Some(w) = &state.warning {
            Span::styled(w.clone(), Style::default().fg(WARN).bg(BG))
        } else {
            Span::styled(
                format!("{} sessions", state.sessions.len()),
                Style::default().fg(MUTED).bg(BG),
            )
        },
    ]);

    let right = "↑/↓ nav · ↵ attach · q quit ";
    let hint_style = Style::default().fg(MUTED).bg(BG);

    let width = area.width as usize;
    let left_w = line_width(&left);
    let right_w = right.chars().count();
    let pad = width.saturating_sub(left_w + right_w);

    let mut full_spans = left.spans;
    full_spans.push(Span::styled(" ".repeat(pad), Style::default().bg(BG)));
    full_spans.push(Span::styled(right.to_string(), hint_style));

    let p = Paragraph::new(Line::from(full_spans)).style(Style::default().bg(BG));
    frame.render_widget(p, area);
}

fn line_width(line: &Line<'_>) -> usize {
    line.spans.iter().map(|s| s.content.chars().count()).sum()
}
