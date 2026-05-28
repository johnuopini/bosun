pub mod banner;
pub mod embed_terminal;
pub mod key_encode;
pub mod layout;
pub mod modal;
pub mod mouse_encode;
pub mod preview;
pub mod section_preview;
pub mod session_list;
pub mod statusbar;
pub mod tab_strip;
pub mod theme;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::Frame;

use crate::app::AppState;
use crate::ui::embed_terminal::EmbedTerminal;
pub use theme::Theme;

pub fn draw(
    frame: &mut Frame<'_>,
    state: &AppState,
    theme: &Theme,
    embed: Option<&EmbedTerminal>,
    embed_focused: bool,
) {
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
    // Single-window mode: outline whichever pane currently has
    // keyboard focus so the user can tell whether keystrokes go to
    // bosun (list nav) or to the embedded tmux session. The unfocused
    // pane gets no border so the indicator stays unambiguous. Drawn
    // after content + before modals so it overlays the perimeter
    // cells but stays underneath any open modal.
    if state.single_window_mode {
        let active = if embed_focused {
            l.preview
        } else {
            Some(l.list)
        };
        if let Some(rect) = active {
            draw_focus_border(frame.buffer_mut(), rect, theme.accent, theme.bg);
        }
    }
    // Modals paint last so they float above everything else. The
    // stack handles dimming the background and rendering one or more
    // modals top-down.
    state.modals.render(frame, area, theme);
}

fn draw_focus_border(buf: &mut Buffer, area: Rect, fg: Color, bg: Color) {
    if area.width < 2 || area.height < 2 {
        return;
    }
    let style = Style::default().fg(fg).bg(bg);
    let left = area.left();
    let right = area.right() - 1;
    let top = area.top();
    let bottom = area.bottom() - 1;

    for x in left..=right {
        let cell = &mut buf[(x, top)];
        cell.set_char('─');
        cell.set_style(style);
        let cell = &mut buf[(x, bottom)];
        cell.set_char('─');
        cell.set_style(style);
    }
    for y in top..=bottom {
        let cell = &mut buf[(left, y)];
        cell.set_char('│');
        cell.set_style(style);
        let cell = &mut buf[(right, y)];
        cell.set_char('│');
        cell.set_style(style);
    }
    let cell = &mut buf[(left, top)];
    cell.set_char('╭');
    cell.set_style(style);
    let cell = &mut buf[(right, top)];
    cell.set_char('╮');
    cell.set_style(style);
    let cell = &mut buf[(left, bottom)];
    cell.set_char('╰');
    cell.set_style(style);
    let cell = &mut buf[(right, bottom)];
    cell.set_char('╯');
    cell.set_style(style);
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
