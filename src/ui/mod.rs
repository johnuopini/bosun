pub mod banner;
pub mod embed_terminal;
pub mod layout;
pub mod modal;
pub mod preview;
pub mod section_preview;
pub mod session_list;
pub mod statusbar;
pub mod theme;

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::Frame;

use crate::app::AppState;
use crate::ui::embed_terminal::EmbedTerminal;
pub use theme::Theme;

pub fn draw(frame: &mut Frame<'_>, state: &AppState, theme: &Theme, embed: Option<&EmbedTerminal>) {
    let area = frame.area();
    let l = layout::compute(area, state.divider_x);
    session_list::render(frame, l.list, state, theme);
    // Preview is hidden on narrow terminals (mobile / mosh).
    if let Some(preview_area) = l.preview {
        preview::render(frame, preview_area, state, theme, embed);
    }
    // Divider glyph sits between list and preview in wide mode.
    // Accent color while the user is dragging, muted otherwise so
    // it reads as a passive separator until you reach for it.
    if let Some(divider_area) = l.divider {
        render_divider(frame, divider_area, state, theme);
    }
    statusbar::render(frame, l.statusbar, state, theme);
    // Modals paint last so they float above everything else. The
    // stack handles dimming the background and rendering one or more
    // modals top-down.
    state.modals.render(frame, area, theme);
}

fn render_divider(frame: &mut Frame<'_>, area: Rect, state: &AppState, theme: &Theme) {
    let fg = if state.dragging_divider {
        theme.accent
    } else {
        theme.text_muted
    };
    let style = Style::default().fg(fg).bg(theme.bg);
    let buf = frame.buffer_mut();
    for y in area.top()..area.bottom() {
        let cell = &mut buf[(area.left(), y)];
        cell.set_char('│');
        cell.set_style(style);
    }
}
