pub mod layout;
pub mod preview;
pub mod session_list;
pub mod statusbar;

use ratatui::Frame;

use crate::app::AppState;

pub fn draw(frame: &mut Frame<'_>, state: &AppState) {
    let l = layout::compute(frame.area());
    session_list::render(frame, l.list, state);
    preview::render(frame, l.preview, state);
    statusbar::render(frame, l.statusbar, state);
}
