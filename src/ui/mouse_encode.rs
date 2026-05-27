//! Encode crossterm `MouseEvent`s into the SGR 1006 byte sequence
//! a terminal-based child process expects on stdin.
//!
//! SGR 1006 format (xterm `?1006` enabled): `\e[<button;col;row(M|m)`.
//! `M` for press/drag, `m` for release. Coords are 1-based. Button
//! codes:
//!
//! - 0 = left, 1 = middle, 2 = right press/drag
//! - 32 + button = motion-with-button-held
//! - 64 = scroll up, 65 = scroll down
//! - 66 = scroll left, 67 = scroll right
//! - Modifiers add: +4 (shift), +8 (alt/meta), +16 (ctrl)
//!
//! We always emit SGR 1006 because that's what every modern app
//! that opts into mouse asks for (DECSET 1006). The pre-1006 normal
//! mode is 7-bit and limited to col<223 — not worth supporting.
//!
//! ## When the bytes flow
//!
//! Apps signal "send me mouse events" by enabling one of the mouse-
//! tracking modes (`1000` press-only, `1002` button-motion, `1003`
//! any-motion). The vt100 parser tracks which is active via
//! `screen().mouse_protocol_mode()`. The caller (App::run) checks
//! that and only forwards events into focus mode when the inner
//! app explicitly wants them. Otherwise we'd be pumping escape
//! sequences into agents that interpret them as garbage input.

use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

/// Encode a mouse event in SGR 1006 form. `local_col` / `local_row`
/// are the event's column / row relative to the embed area's
/// top-left, 0-based — this function converts to the 1-based form
/// SGR 1006 expects. Returns `None` for events that don't have a
/// meaningful encoding (e.g. `Moved` without a button — apps only
/// want those if they've enabled any-motion mode, and the caller
/// decides that separately).
pub fn encode(event: MouseEvent, local_col: u16, local_row: u16) -> Option<Vec<u8>> {
    let (button_base, suffix) = match event.kind {
        MouseEventKind::Down(b) => (button_code(b)?, b'M'),
        MouseEventKind::Up(b) => (button_code(b)?, b'm'),
        MouseEventKind::Drag(b) => (button_code(b)? + 32, b'M'),
        MouseEventKind::Moved => (35, b'M'),
        MouseEventKind::ScrollUp => (64, b'M'),
        MouseEventKind::ScrollDown => (65, b'M'),
        MouseEventKind::ScrollLeft => (66, b'M'),
        MouseEventKind::ScrollRight => (67, b'M'),
    };
    let button = button_base + modifier_offset(event.modifiers);

    let col = local_col.saturating_add(1);
    let row = local_row.saturating_add(1);

    let mut out = Vec::with_capacity(16);
    out.extend_from_slice(b"\x1b[<");
    out.extend_from_slice(button.to_string().as_bytes());
    out.push(b';');
    out.extend_from_slice(col.to_string().as_bytes());
    out.push(b';');
    out.extend_from_slice(row.to_string().as_bytes());
    out.push(suffix);
    Some(out)
}

fn button_code(b: MouseButton) -> Option<u32> {
    match b {
        MouseButton::Left => Some(0),
        MouseButton::Middle => Some(1),
        MouseButton::Right => Some(2),
    }
}

fn modifier_offset(m: KeyModifiers) -> u32 {
    let mut off = 0u32;
    if m.contains(KeyModifiers::SHIFT) {
        off += 4;
    }
    if m.contains(KeyModifiers::ALT) {
        off += 8;
    }
    if m.contains(KeyModifiers::CONTROL) {
        off += 16;
    }
    off
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: MouseEventKind, col: u16, row: u16, mods: KeyModifiers) -> MouseEvent {
        MouseEvent {
            kind,
            column: col,
            row,
            modifiers: mods,
        }
    }

    #[test]
    fn left_press_at_origin() {
        let e = ev(
            MouseEventKind::Down(MouseButton::Left),
            0,
            0,
            KeyModifiers::NONE,
        );
        assert_eq!(encode(e, 0, 0).unwrap(), b"\x1b[<0;1;1M".to_vec());
    }

    #[test]
    fn left_release_uses_lowercase_m() {
        let e = ev(
            MouseEventKind::Up(MouseButton::Left),
            5,
            5,
            KeyModifiers::NONE,
        );
        assert_eq!(encode(e, 5, 5).unwrap(), b"\x1b[<0;6;6m".to_vec());
    }

    #[test]
    fn right_press_is_button_2() {
        let e = ev(
            MouseEventKind::Down(MouseButton::Right),
            10,
            3,
            KeyModifiers::NONE,
        );
        assert_eq!(encode(e, 10, 3).unwrap(), b"\x1b[<2;11;4M".to_vec());
    }

    #[test]
    fn scroll_up_is_64() {
        let e = ev(MouseEventKind::ScrollUp, 0, 0, KeyModifiers::NONE);
        assert_eq!(encode(e, 0, 0).unwrap(), b"\x1b[<64;1;1M".to_vec());
    }

    #[test]
    fn scroll_down_is_65() {
        let e = ev(MouseEventKind::ScrollDown, 0, 0, KeyModifiers::NONE);
        assert_eq!(encode(e, 0, 0).unwrap(), b"\x1b[<65;1;1M".to_vec());
    }

    #[test]
    fn left_drag_adds_motion_bit() {
        // Drag(Left) = 0 + 32 = 32.
        let e = ev(
            MouseEventKind::Drag(MouseButton::Left),
            2,
            2,
            KeyModifiers::NONE,
        );
        assert_eq!(encode(e, 2, 2).unwrap(), b"\x1b[<32;3;3M".to_vec());
    }

    #[test]
    fn ctrl_left_press_adds_16() {
        let e = ev(
            MouseEventKind::Down(MouseButton::Left),
            0,
            0,
            KeyModifiers::CONTROL,
        );
        assert_eq!(encode(e, 0, 0).unwrap(), b"\x1b[<16;1;1M".to_vec());
    }

    #[test]
    fn shift_scroll_up_adds_4() {
        let e = ev(MouseEventKind::ScrollUp, 0, 0, KeyModifiers::SHIFT);
        assert_eq!(encode(e, 0, 0).unwrap(), b"\x1b[<68;1;1M".to_vec());
    }
}
