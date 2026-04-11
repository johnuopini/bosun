pub mod layout;
pub mod modal;
pub mod preview;
pub mod session_list;
pub mod statusbar;

use ratatui::Frame;

use crate::app::AppState;

pub fn draw(frame: &mut Frame<'_>, state: &AppState) {
    let area = frame.area();
    let l = layout::compute(area);
    session_list::render(frame, l.list, state);
    preview::render(frame, l.preview, state);
    statusbar::render(frame, l.statusbar, state);
    // Modals paint last so they float above everything else. The
    // stack handles dimming the background and rendering one or more
    // modals top-down.
    state.modals.render(frame, area);
}
