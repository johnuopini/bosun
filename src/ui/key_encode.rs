//! Encode crossterm `KeyEvent`s into the byte sequences a terminal-
//! based child process expects on stdin.
//!
//! Coverage is the MVP set for Step 4 focus mode: printable chars,
//! Ctrl modifiers, Enter / Tab / Backspace / Esc, arrow keys, Home /
//! End / PageUp / PageDown, F1-F12, and Insert / Delete. We emit
//! standard xterm-compatible sequences — the same encoding crossterm
//! itself decodes when *receiving* keystrokes on the bosun side.
//! That round-trip symmetry is what makes the encoded bytes look
//! like a "real" terminal to the child.
//!
//! ## Not covered (yet — see PLAN_2_0.md Step 4 follow-ups)
//!
//! - **Cursor key application mode** (DECCKM): the child can switch
//!   between `CSI A` / `SS3 A` for the same arrow. We always emit
//!   the CSI form, which is the default and what `xterm-256color`
//!   uses when CKM is off. Most modern apps handle both forms.
//! - **Application keypad mode** (DECPAM): numeric vs. SS3-encoded
//!   keypad. We always emit numeric.
//! - **modifyOtherKeys / kitty keyboard protocol**: enhanced shifted/
//!   meta combinations. We emit only the basics — Shift+arrow as
//!   `CSI 1;2 A`, Alt+char as `ESC c`. Anything beyond is dropped.
//! - **Bracketed paste**: large pastes don't get bracketed; the child
//!   sees the bytes as if typed. Fine for short input, not great for
//!   pasting code.
//! - **SGR mouse mode (1006)**: focus mode doesn't forward mouse yet.
//!   Bosun's own mouse handling (divider drag) is unaffected.
//!
//! These gaps are why focus mode is "MVP" — it's enough for typing
//! into Claude / Codex / a shell, not enough for `vim` / `fzf` /
//! `htop` to feel right. Post-MVP arc fills them in.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// Encoding context — knobs the inner app has communicated
/// through its DECSET stream that change the right byte sequence
/// for a given key press. Currently a single field; held as a
/// struct so future additions (modifyOtherKeys mode tracking,
/// kitty keyboard flags, application keypad) slot in without
/// churning every call site.
#[derive(Debug, Clone, Copy, Default)]
pub struct EncodeContext {
    /// DECCKM (cursor-key application mode). When true, arrow
    /// keys and Home/End use the SS3 form (`\eOA` etc.) instead
    /// of the default CSI form (`\e[A` etc.).
    pub application_cursor: bool,
}

/// Encode a `KeyEvent` into the byte sequence to write to the
/// child's PTY. Returns `None` for events we explicitly don't
/// forward — currently key-release events on terminals that report
/// them, and Null / unmapped function keys.
pub fn encode(key: KeyEvent, ctx: EncodeContext) -> Option<Vec<u8>> {
    // Only forward presses. Some terminals (kitty, foot, recent
    // alacritty with kitty keyboard) also emit Release events;
    // forwarding those would double every keystroke from the
    // child's perspective.
    if key.kind == KeyEventKind::Release {
        return None;
    }

    let m = key.modifiers;
    let ctrl = m.contains(KeyModifiers::CONTROL);
    let alt = m.contains(KeyModifiers::ALT);
    let shift = m.contains(KeyModifiers::SHIFT);

    match key.code {
        KeyCode::Char(c) => Some(encode_char(c, ctrl, alt)),
        // Enter / Tab / Backspace with a non-trivial modifier emit
        // the xterm `modifyOtherKeys=2` form so apps that opt in to
        // that protocol (Claude Code, kitty, recent xterm, etc.)
        // can distinguish e.g. Shift+Enter from plain Enter. Plain
        // (no modifier) keeps the bare byte for everything else.
        // Apps that don't understand modifyOtherKeys will see the
        // escape sequence as an unknown CSI and typically drop it
        // silently — strictly better than the prior behavior of
        // sending Shift+Enter as a literal `\r` (i.e. submit) and
        // confusing the user.
        KeyCode::Enter => Some(modified_or(13, shift, ctrl, alt, b"\r")),
        KeyCode::Tab => Some(modified_or(9, shift, ctrl, alt, b"\t")),
        KeyCode::BackTab => Some(b"\x1b[Z".to_vec()),
        // Alt+Backspace (macOS Option+Delete) is the universal
        // "delete previous word" chord. Real terminals send it as
        // Meta+DEL = ESC + 0x7f, which readline / zsh / bash / Claude
        // Code all map to backward-kill-word. The modifyOtherKeys
        // form used for the Ctrl/Shift combos below is *not*
        // recognized as a word-delete by those apps — they fall back
        // to deleting a single char — so Alt-only gets the
        // ESC-prefixed bare byte instead.
        KeyCode::Backspace if alt && !ctrl && !shift => Some(b"\x1b\x7f".to_vec()),
        KeyCode::Backspace => Some(modified_or(127, shift, ctrl, alt, b"\x7f")),
        KeyCode::Esc => Some(b"\x1b".to_vec()),
        // Alt+Left / Alt+Right (macOS Option+Arrow) = move by word.
        // Emit the readline Meta-b / Meta-f sequences (ESC b / ESC f),
        // the universal backward-word / forward-word bindings that
        // zsh / bash / readline / Claude Code all honor. The xterm
        // Alt+arrow CSI form (`\e[1;3D`) the modified `arrow_seq`
        // would otherwise produce is not recognized as word-motion by
        // those apps, so the cursor wouldn't move at all. Mirrors the
        // Alt+Backspace → `\x1b\x7f` (backward-kill-word) choice above.
        KeyCode::Left if alt && !ctrl && !shift => Some(b"\x1bb".to_vec()),
        KeyCode::Right if alt && !ctrl && !shift => Some(b"\x1bf".to_vec()),
        KeyCode::Left => Some(arrow_seq(b'D', shift, ctrl, alt, ctx.application_cursor)),
        KeyCode::Right => Some(arrow_seq(b'C', shift, ctrl, alt, ctx.application_cursor)),
        KeyCode::Up => Some(arrow_seq(b'A', shift, ctrl, alt, ctx.application_cursor)),
        KeyCode::Down => Some(arrow_seq(b'B', shift, ctrl, alt, ctx.application_cursor)),
        KeyCode::Home => Some(arrow_seq(b'H', shift, ctrl, alt, ctx.application_cursor)),
        KeyCode::End => Some(arrow_seq(b'F', shift, ctrl, alt, ctx.application_cursor)),
        KeyCode::PageUp => Some(tilde_seq(b"5", shift, ctrl, alt)),
        KeyCode::PageDown => Some(tilde_seq(b"6", shift, ctrl, alt)),
        KeyCode::Insert => Some(tilde_seq(b"2", shift, ctrl, alt)),
        KeyCode::Delete => Some(tilde_seq(b"3", shift, ctrl, alt)),
        KeyCode::F(n) => function_key(n, shift, ctrl, alt),
        // Skip everything we don't explicitly handle. Includes
        // Null, CapsLock, ScrollLock, NumLock, PrintScreen, Pause,
        // Menu, KeypadBegin, Media keys, Modifier-only events,
        // Mouse-as-key (kitty). Dropping silently is the right
        // default for MVP — none of these are needed to drive
        // typical agent / shell input.
        _ => None,
    }
}

fn encode_char(c: char, ctrl: bool, alt: bool) -> Vec<u8> {
    let bytes: Vec<u8> = if ctrl {
        ctrl_char_bytes(c)
    } else {
        let mut s = [0u8; 4];
        c.encode_utf8(&mut s).as_bytes().to_vec()
    };
    if alt {
        let mut out = Vec::with_capacity(bytes.len() + 1);
        out.push(0x1b);
        out.extend_from_slice(&bytes);
        out
    } else {
        bytes
    }
}

/// Map a character + Ctrl modifier to its control-code byte. The
/// classic 0x1f mask gives us `Ctrl-A=0x01 ... Ctrl-_=0x1f`. A few
/// special cases that diverge from the mask are handled by name.
fn ctrl_char_bytes(c: char) -> Vec<u8> {
    let lc = c.to_ascii_lowercase();
    let byte: u8 = match lc {
        // Most letters / standard punctuation: `c & 0x1f`. Note
        // that lowercase `c` is the canonical form because the
        // user typed without Shift; uppercase variants only show
        // up when Shift is also held, which we treat as a separate
        // modifier (and don't encode at the byte level — Ctrl-Shift-A
        // and Ctrl-A produce the same byte without kitty keyboard).
        'a'..='z' => (lc as u8) & 0x1f,
        '@' => 0x00,
        '[' => 0x1b,
        '\\' => 0x1c,
        ']' => 0x1d,
        '^' => 0x1e,
        '_' => 0x1f,
        '?' => 0x7f, // Ctrl-? often produces DEL on real terminals.
        ' ' => 0x00, // Ctrl-Space → NUL.
        // Fallback: numeric keys + everything else just pass
        // through. Ctrl-1, Ctrl-2 etc. on most terminals send the
        // raw digit; modifyOtherKeys would send a CSI sequence
        // instead, but we don't implement that in the MVP.
        other => other as u8,
    };
    vec![byte]
}

#[allow(dead_code)]
fn prepend_alt(alt: bool, bytes: Vec<u8>) -> Option<Vec<u8>> {
    if alt {
        let mut out = Vec::with_capacity(bytes.len() + 1);
        out.push(0x1b);
        out.extend_from_slice(&bytes);
        Some(out)
    } else {
        Some(bytes)
    }
}

/// xterm `modifyOtherKeys=2` encoding: `\e[27;<mod>;<keycode>~`.
/// When no modifier is active (`mod == 1`), returns the plain
/// `bare` bytes so backwards-compat is preserved for apps that
/// don't understand the protocol. Callers pass the key's
/// codepoint as `keycode` (e.g. 13 for Enter, 9 for Tab).
fn modified_or(keycode: u16, shift: bool, ctrl: bool, alt: bool, bare: &[u8]) -> Vec<u8> {
    let code = modifier_code(shift, ctrl, alt);
    if code == 1 {
        return bare.to_vec();
    }
    let mut out = Vec::with_capacity(12);
    out.extend_from_slice(b"\x1b[27;");
    out.extend_from_slice(code.to_string().as_bytes());
    out.push(b';');
    out.extend_from_slice(keycode.to_string().as_bytes());
    out.push(b'~');
    out
}

/// CSI-letter sequences (`ESC [ <mods> <letter>`) used by arrow
/// keys, Home, End. With no modifiers, the bare form depends on
/// DECCKM (cursor-key application mode): SS3 `ESC O <letter>` when
/// the inner app has enabled it (vim command mode, readline,
/// etc.), CSI `ESC [ <letter>` otherwise. With modifiers it's
/// always `CSI 1; <code> <letter>` regardless of DECCKM — xterm's
/// modified-arrow encoding is the same in both modes.
fn arrow_seq(letter: u8, shift: bool, ctrl: bool, alt: bool, app_cursor: bool) -> Vec<u8> {
    let code = modifier_code(shift, ctrl, alt);
    if code == 1 {
        if app_cursor {
            vec![0x1b, b'O', letter]
        } else {
            vec![0x1b, b'[', letter]
        }
    } else {
        let mut out = Vec::with_capacity(8);
        out.extend_from_slice(b"\x1b[1;");
        out.extend_from_slice(code.to_string().as_bytes());
        out.push(letter);
        out
    }
}

/// CSI-tilde sequences (`ESC [ <num> ~`) used by PgUp/PgDn, Ins,
/// Del, F-keys. With modifiers: `ESC [ <num> ; <code> ~`.
fn tilde_seq(num: &[u8], shift: bool, ctrl: bool, alt: bool) -> Vec<u8> {
    let code = modifier_code(shift, ctrl, alt);
    let mut out = Vec::with_capacity(8);
    out.extend_from_slice(b"\x1b[");
    out.extend_from_slice(num);
    if code != 1 {
        out.push(b';');
        out.extend_from_slice(code.to_string().as_bytes());
    }
    out.push(b'~');
    out
}

/// xterm modifier code: 1 = none, 2 = Shift, 3 = Alt, 4 = Shift+Alt,
/// 5 = Ctrl, 6 = Ctrl+Shift, 7 = Ctrl+Alt, 8 = Ctrl+Shift+Alt.
fn modifier_code(shift: bool, ctrl: bool, alt: bool) -> u8 {
    let mut code = 1u8;
    if shift {
        code += 1;
    }
    if alt {
        code += 2;
    }
    if ctrl {
        code += 4;
    }
    code
}

/// F1-F12 encoding. F1-F4 use the SS3 form (`ESC O P`...) without
/// modifiers and the CSI 1;<code>P form with modifiers. F5-F12 use
/// the CSI ~ form throughout. F13+ exists in xterm but is not
/// covered here (keyboards almost never have them and the encoding
/// changes again).
fn function_key(n: u8, shift: bool, ctrl: bool, alt: bool) -> Option<Vec<u8>> {
    let code = modifier_code(shift, ctrl, alt);
    match n {
        1..=4 => {
            let letter = b"PQRS"[(n - 1) as usize];
            if code == 1 {
                Some(vec![0x1b, b'O', letter])
            } else {
                let mut out = Vec::with_capacity(8);
                out.extend_from_slice(b"\x1b[1;");
                out.extend_from_slice(code.to_string().as_bytes());
                out.push(letter);
                Some(out)
            }
        }
        5 => Some(tilde_seq(b"15", shift, ctrl, alt)),
        6 => Some(tilde_seq(b"17", shift, ctrl, alt)),
        7 => Some(tilde_seq(b"18", shift, ctrl, alt)),
        8 => Some(tilde_seq(b"19", shift, ctrl, alt)),
        9 => Some(tilde_seq(b"20", shift, ctrl, alt)),
        10 => Some(tilde_seq(b"21", shift, ctrl, alt)),
        11 => Some(tilde_seq(b"23", shift, ctrl, alt)),
        12 => Some(tilde_seq(b"24", shift, ctrl, alt)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn k(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    /// Encode shim that fills in a default (cursor-mode off)
    /// context so the bulk of the existing tests don't need to
    /// pass it explicitly. DECCKM-specific tests construct the
    /// context themselves.
    fn encode(key: KeyEvent) -> Option<Vec<u8>> {
        super::encode(key, EncodeContext::default())
    }

    #[test]
    fn plain_letter() {
        assert_eq!(
            encode(k(KeyCode::Char('a'), KeyModifiers::NONE)),
            Some(b"a".to_vec())
        );
    }

    #[test]
    fn ctrl_letter() {
        assert_eq!(
            encode(k(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            Some(vec![0x01])
        );
        assert_eq!(
            encode(k(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(vec![0x03])
        );
    }

    #[test]
    fn alt_letter_prepends_esc() {
        assert_eq!(
            encode(k(KeyCode::Char('x'), KeyModifiers::ALT)),
            Some(vec![0x1b, b'x'])
        );
    }

    #[test]
    fn enter_is_cr() {
        assert_eq!(
            encode(k(KeyCode::Enter, KeyModifiers::NONE)),
            Some(b"\r".to_vec())
        );
    }

    #[test]
    fn shift_enter_uses_modify_other_keys() {
        // Mod code 2 = shift. Keycode 13 = Enter (\r). Claude Code
        // listens for this exact sequence to insert a newline
        // without submitting.
        assert_eq!(
            encode(k(KeyCode::Enter, KeyModifiers::SHIFT)),
            Some(b"\x1b[27;2;13~".to_vec())
        );
    }

    #[test]
    fn ctrl_enter_uses_modify_other_keys() {
        // Mod code 5 = ctrl.
        assert_eq!(
            encode(k(KeyCode::Enter, KeyModifiers::CONTROL)),
            Some(b"\x1b[27;5;13~".to_vec())
        );
    }

    #[test]
    fn ctrl_tab_uses_modify_other_keys() {
        assert_eq!(
            encode(k(KeyCode::Tab, KeyModifiers::CONTROL)),
            Some(b"\x1b[27;5;9~".to_vec())
        );
    }

    #[test]
    fn backspace_is_del() {
        assert_eq!(
            encode(k(KeyCode::Backspace, KeyModifiers::NONE)),
            Some(b"\x7f".to_vec())
        );
    }

    #[test]
    fn alt_backspace_deletes_word() {
        // macOS Option+Delete. Meta+DEL = ESC + 0x7f, the readline
        // backward-kill-word chord — not the modifyOtherKeys form,
        // which apps don't treat as a word-delete.
        assert_eq!(
            encode(k(KeyCode::Backspace, KeyModifiers::ALT)),
            Some(b"\x1b\x7f".to_vec())
        );
    }

    #[test]
    fn ctrl_backspace_uses_modify_other_keys() {
        assert_eq!(
            encode(k(KeyCode::Backspace, KeyModifiers::CONTROL)),
            Some(b"\x1b[27;5;127~".to_vec())
        );
    }

    #[test]
    fn alt_left_right_move_by_word() {
        // macOS Option+Arrow. ESC b / ESC f are the readline
        // backward-word / forward-word bindings claude & shells honor;
        // the xterm `\e[1;3D` Alt-arrow form is ignored by them.
        assert_eq!(
            encode(k(KeyCode::Left, KeyModifiers::ALT)),
            Some(b"\x1bb".to_vec())
        );
        assert_eq!(
            encode(k(KeyCode::Right, KeyModifiers::ALT)),
            Some(b"\x1bf".to_vec())
        );
    }

    #[test]
    fn ctrl_alt_left_falls_through_to_csi() {
        // Only plain Alt+arrow is word-motion; combos keep the
        // modified CSI form (Ctrl+Alt code 7).
        assert_eq!(
            encode(k(KeyCode::Left, KeyModifiers::ALT | KeyModifiers::CONTROL)),
            Some(b"\x1b[1;7D".to_vec())
        );
    }

    #[test]
    fn esc_is_esc() {
        assert_eq!(
            encode(k(KeyCode::Esc, KeyModifiers::NONE)),
            Some(b"\x1b".to_vec())
        );
    }

    #[test]
    fn arrow_bare() {
        assert_eq!(
            encode(k(KeyCode::Up, KeyModifiers::NONE)),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            encode(k(KeyCode::Left, KeyModifiers::NONE)),
            Some(b"\x1b[D".to_vec())
        );
    }

    #[test]
    fn arrow_bare_application_cursor_mode() {
        // DECCKM on: SS3 form instead of CSI.
        let ctx = EncodeContext {
            application_cursor: true,
        };
        assert_eq!(
            super::encode(k(KeyCode::Up, KeyModifiers::NONE), ctx),
            Some(b"\x1bOA".to_vec())
        );
        assert_eq!(
            super::encode(k(KeyCode::Down, KeyModifiers::NONE), ctx),
            Some(b"\x1bOB".to_vec())
        );
        assert_eq!(
            super::encode(k(KeyCode::Right, KeyModifiers::NONE), ctx),
            Some(b"\x1bOC".to_vec())
        );
        assert_eq!(
            super::encode(k(KeyCode::Left, KeyModifiers::NONE), ctx),
            Some(b"\x1bOD".to_vec())
        );
    }

    #[test]
    fn arrow_with_modifier_ignores_application_cursor() {
        // Modified arrows use the same `\e[1;<code>X` form in
        // both modes — DECCKM only affects the bare form.
        let ctx = EncodeContext {
            application_cursor: true,
        };
        assert_eq!(
            super::encode(k(KeyCode::Up, KeyModifiers::SHIFT), ctx),
            Some(b"\x1b[1;2A".to_vec())
        );
    }

    #[test]
    fn home_end_obey_application_cursor() {
        let ctx = EncodeContext {
            application_cursor: true,
        };
        assert_eq!(
            super::encode(k(KeyCode::Home, KeyModifiers::NONE), ctx),
            Some(b"\x1bOH".to_vec())
        );
        assert_eq!(
            super::encode(k(KeyCode::End, KeyModifiers::NONE), ctx),
            Some(b"\x1bOF".to_vec())
        );
    }

    #[test]
    fn arrow_with_shift() {
        // Shift modifier code = 2.
        assert_eq!(
            encode(k(KeyCode::Up, KeyModifiers::SHIFT)),
            Some(b"\x1b[1;2A".to_vec())
        );
    }

    #[test]
    fn arrow_with_ctrl() {
        // Ctrl modifier code = 5.
        assert_eq!(
            encode(k(KeyCode::Right, KeyModifiers::CONTROL)),
            Some(b"\x1b[1;5C".to_vec())
        );
    }

    #[test]
    fn pgup_bare_and_with_ctrl() {
        assert_eq!(
            encode(k(KeyCode::PageUp, KeyModifiers::NONE)),
            Some(b"\x1b[5~".to_vec())
        );
        assert_eq!(
            encode(k(KeyCode::PageUp, KeyModifiers::CONTROL)),
            Some(b"\x1b[5;5~".to_vec())
        );
    }

    #[test]
    fn f1_uses_ss3() {
        assert_eq!(
            encode(k(KeyCode::F(1), KeyModifiers::NONE)),
            Some(b"\x1bOP".to_vec())
        );
    }

    #[test]
    fn f5_uses_tilde() {
        assert_eq!(
            encode(k(KeyCode::F(5), KeyModifiers::NONE)),
            Some(b"\x1b[15~".to_vec())
        );
    }

    #[test]
    fn release_is_dropped() {
        let mut ev = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        ev.kind = KeyEventKind::Release;
        assert_eq!(encode(ev), None);
    }
}
