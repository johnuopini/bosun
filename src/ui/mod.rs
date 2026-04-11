pub mod layout;
pub mod modal;
pub mod preview;
pub mod session_list;
pub mod statusbar;
pub mod theme;

use ratatui::Frame;

use crate::app::AppState;
pub use theme::Theme;

pub fn draw(frame: &mut Frame<'_>, state: &AppState, theme: &Theme) {
    let area = frame.area();
    let l = layout::compute(area);
    session_list::render(frame, l.list, state, theme);
    preview::render(frame, l.preview, state, theme);
    statusbar::render(frame, l.statusbar, state, theme);
    // Modals paint last so they float above everything else. The
    // stack handles dimming the background and rendering one or more
    // modals top-down.
    state.modals.render(frame, area, theme);
}
