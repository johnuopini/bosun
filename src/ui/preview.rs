//! Live pane preview rendered from `tmux capture-pane -e` output via
//! `ansi-to-tui`. The preview buffer for the selected session lives in
//! `AppState`; this module is pure rendering.

use ansi_to_tui::IntoText;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::AppState;

const BG: Color = Color::Rgb(17, 20, 27);
const MUTED: Color = Color::Rgb(124, 132, 149);

pub fn render(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let block = Block::default().style(Style::default().bg(BG));

    let text: Text<'_> = match state.selected_preview() {
        Some(bytes) if !bytes.is_empty() => bytes
            .into_text()
            .unwrap_or_else(|_| placeholder("preview: (ansi parse failed)")),
        _ => placeholder("preview: capturing…"),
    };

    let p = Paragraph::new(text)
        .block(block)
        .wrap(Wrap { trim: false })
        .style(Style::default().bg(BG));
    frame.render_widget(p, area);
}

fn placeholder(msg: &str) -> Text<'static> {
    Text::from(vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("  {}", msg),
            Style::default().fg(MUTED).bg(BG),
        )),
    ])
}
