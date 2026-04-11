pub mod layout;
pub mod session_list;
pub mod statusbar;

use ratatui::Frame;

use crate::app::AppState;

pub fn draw(frame: &mut Frame<'_>, state: &AppState) {
    let l = layout::compute(frame.area());
    session_list::render(frame, l.list, state);
    render_preview_placeholder(frame, l.preview);
    statusbar::render(frame, l.statusbar, state);
}

fn render_preview_placeholder(frame: &mut Frame<'_>, area: ratatui::layout::Rect) {
    use ratatui::style::{Color, Style};
    use ratatui::widgets::{Block, Paragraph};
    let block = Block::default().style(Style::default().bg(Color::Rgb(17, 20, 27)));
    let text = Paragraph::new("preview coming in Phase 2")
        .style(
            Style::default()
                .fg(Color::Rgb(124, 132, 149))
                .bg(Color::Rgb(17, 20, 27)),
        )
        .block(block);
    frame.render_widget(text, area);
}
