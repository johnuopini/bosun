//! Bosun-branded tmux status bar + prefix-1..9 quick-jump bindings.
//!
//! Paints a purple "⚓ bosun" brand on the left, a numbered list of
//! sessions in the right half with the attached one highlighted, and
//! `^Q detach · <prefix> 1-9 jump` as a hint on the far right. Binds
//! digits 1..9 in the tmux **prefix** key table to `switch-client -t
//! <nth session>` so users press their normal prefix (e.g. Ctrl+A)
//! then a digit to hop between sessions. This matches agent-deck's
//! idiom and respects whatever prefix the user has configured.
//!
//! Side effect: we override the default `prefix 1..9` bindings that
//! tmux ships with (select-window). Most bosun users aren't using
//! prefix-digit for window-jumping anyway, but Phase 5 will save and
//! restore them around the install cycle. For now we just overwrite
//! and unbind on exit — your `select-window` bindings will come back
//! when you next start a fresh tmux server.
//!
//! User config preservation: we save the user's existing `status*`
//! options into `@bosun_saved_*` user options at install time and
//! restore them on uninstall. A small refcount in `@bosun_sb_refcount`
//! coordinates multi-instance — the first bosun to start saves the
//! user's config, the last to exit restores it.
//!
//! This module is synchronous. Its callers are the `tmux_actor` task
//! (which is fine to briefly block) and the panic hook (which must be
//! sync). Total install cost is ~150ms on macOS (9 subprocess spawns).

use crate::error::{BosunError, Result};
use crate::tmux::client::sync_tmux;

// --- Visual constants -------------------------------------------------

const BRAND: &str = "#[bg=#7c5cff,fg=#0b0d12,bold] ⚓ bosun #[default] ";
const BRAND_LEN: &str = "14";
const STATUS_RIGHT_LEN: &str = "400";
const STATUS_STYLE: &str = "bg=default,fg=#e6e9ef";

const OPTIONS_TO_SAVE: &[&str] = &[
    "status",
    "status-style",
    "status-left",
    "status-right",
    "status-left-length",
    "status-right-length",
    "status-justify",
];

const REFCOUNT_OPT: &str = "@bosun_sb_refcount";

// --- Guard -----------------------------------------------------------

/// RAII guard: on drop, decrements the refcount and — if it hits zero —
/// restores the user's saved status options and removes the Ctrl+1..9
/// bindings. Callers hold this for as long as they want the bar active.
#[must_use = "dropping the guard immediately restores the user's config"]
pub struct StatusBarGuard {
    socket: Option<String>,
    done: bool,
}

impl StatusBarGuard {
    /// Explicitly release the guard, surfacing any restore errors.
    #[allow(dead_code)]
    pub fn release(mut self) -> Result<()> {
        self.done = true;
        uninstall(self.socket.as_deref())
    }
}

impl Drop for StatusBarGuard {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let _ = uninstall(self.socket.as_deref());
    }
}

// --- Public API ------------------------------------------------------

/// Install bosun's status bar. Safe to call from multiple instances —
/// the first one saves the user's config; later ones just bump the
/// refcount.
pub fn install(socket: Option<&str>) -> Result<StatusBarGuard> {
    let refcount = read_refcount(socket);
    if refcount == 0 {
        save_user_options(socket)?;
        apply_bosun_options(socket)?;
    }
    set_refcount(socket, refcount + 1)?;
    Ok(StatusBarGuard {
        socket: socket.map(|s| s.to_string()),
        done: false,
    })
}

/// Update the status-right session list and rebind prefix-1..9. Called
/// from the tmux actor when the session list changes. Sessions beyond
/// the 9th are visible in the bar but won't be bound to a jump key.
pub fn sync_sessions(socket: Option<&str>, sessions: &[(String, bool)]) -> Result<()> {
    let hint = build_hint(socket);
    let status_right = build_status_right(sessions, &hint);
    run(socket, &["set-option", "-g", "status-right", &status_right])?;
    run(
        socket,
        &["set-option", "-g", "status-right-length", STATUS_RIGHT_LEN],
    )?;
    bind_jump_keys(socket, sessions)?;
    Ok(())
}

/// Best-effort uninstall for panic-hook use. Does not surface errors.
pub fn emergency_uninstall(socket: Option<&str>) {
    let _ = uninstall(socket);
}

// --- Internal: install / uninstall -----------------------------------

fn uninstall(socket: Option<&str>) -> Result<()> {
    let refcount = read_refcount(socket);
    if refcount > 1 {
        set_refcount(socket, refcount - 1)?;
        return Ok(());
    }
    restore_user_options(socket)?;
    unbind_jump_keys(socket);
    clear_refcount(socket);
    Ok(())
}

fn apply_bosun_options(socket: Option<&str>) -> Result<()> {
    run(socket, &["set-option", "-g", "status", "on"])?;
    run(socket, &["set-option", "-g", "status-style", STATUS_STYLE])?;
    run(socket, &["set-option", "-g", "status-left", BRAND])?;
    run(
        socket,
        &["set-option", "-g", "status-left-length", BRAND_LEN],
    )?;
    run(socket, &["set-option", "-g", "status-justify", "left"])?;
    let hint = build_hint(socket);
    run(socket, &["set-option", "-g", "status-right", &hint])?;
    run(
        socket,
        &["set-option", "-g", "status-right-length", STATUS_RIGHT_LEN],
    )?;
    Ok(())
}

fn save_user_options(socket: Option<&str>) -> Result<()> {
    for opt in OPTIONS_TO_SAVE {
        let value = show_option(socket, opt);
        let saved = saved_name(opt);
        run(socket, &["set-option", "-g", &saved, &value])?;
    }
    Ok(())
}

fn restore_user_options(socket: Option<&str>) -> Result<()> {
    for opt in OPTIONS_TO_SAVE {
        let saved = saved_name(opt);
        let value = show_option(socket, &saved);
        if value.is_empty() {
            // Nothing to restore — reset to tmux's default.
            let _ = run(socket, &["set-option", "-gu", opt]);
        } else {
            let _ = run(socket, &["set-option", "-g", opt, &value]);
        }
        let _ = run(socket, &["set-option", "-gu", &saved]);
    }
    Ok(())
}

// --- Internal: bindings ---------------------------------------------

fn bind_jump_keys(socket: Option<&str>, sessions: &[(String, bool)]) -> Result<()> {
    for i in 0..9 {
        let key = digit_key(i + 1);
        match sessions.get(i) {
            Some((name, _)) => {
                let target = tmux_quote(name);
                let cmd = format!("switch-client -t {}", target);
                run(socket, &["bind-key", "-T", "prefix", &key, &cmd])?;
            }
            None => {
                let _ = run(socket, &["unbind-key", "-T", "prefix", &key]);
            }
        }
    }
    Ok(())
}

fn unbind_jump_keys(socket: Option<&str>) {
    for i in 1..=9 {
        let key = digit_key(i);
        let _ = run(socket, &["unbind-key", "-T", "prefix", &key]);
    }
}

fn digit_key(n: usize) -> String {
    n.to_string()
}

// --- Internal: string building --------------------------------------

fn build_status_right(sessions: &[(String, bool)], hint: &str) -> String {
    let list: String = sessions
        .iter()
        .enumerate()
        .take(9)
        .map(|(i, (name, attached))| format_chip(i + 1, name, *attached))
        .collect::<Vec<_>>()
        .join(" ");
    if list.is_empty() {
        hint.to_string()
    } else {
        format!("{}  {}", list, hint)
    }
}

/// Build the right-hand hint string with the user's actual prefix key.
/// Defaults to `C-b` (tmux's shipped default) if the option is unset.
fn build_hint(socket: Option<&str>) -> String {
    let prefix = show_option(socket, "prefix");
    let prefix = if prefix.is_empty() || prefix == "None" {
        "C-b"
    } else {
        prefix.as_str()
    };
    format!("#[fg=#7c8495]^Q detach · {} 1-9 jump ", prefix)
}

fn format_chip(num: usize, name: &str, attached: bool) -> String {
    let safe = escape_tmux_format(name);
    if attached {
        format!("#[bg=#1e2433,fg=#7c5cff,bold] {}:{} #[default]", num, safe)
    } else {
        format!("#[fg=#e6e9ef] {}:{} #[default]", num, safe)
    }
}

/// Tmux format strings interpret `#` as the start of a directive; double
/// it to `##` to render a literal `#`.
fn escape_tmux_format(s: &str) -> String {
    s.replace('#', "##")
}

/// Wrap `s` in double quotes for passing through tmux's own argv parser.
/// tmux parses args POSIX-style, so we backslash-escape internal quotes.
fn tmux_quote(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{}\"", escaped)
}

fn saved_name(opt: &str) -> String {
    format!("@bosun_saved_{}", opt.replace('-', "_"))
}

// --- Internal: tmux shell helpers -----------------------------------

fn read_refcount(socket: Option<&str>) -> u32 {
    show_option(socket, REFCOUNT_OPT).parse().unwrap_or(0)
}

fn set_refcount(socket: Option<&str>, value: u32) -> Result<()> {
    run(
        socket,
        &["set-option", "-g", REFCOUNT_OPT, &value.to_string()],
    )
}

fn clear_refcount(socket: Option<&str>) {
    let _ = run(socket, &["set-option", "-gu", REFCOUNT_OPT]);
}

fn show_option(socket: Option<&str>, opt: &str) -> String {
    let out = sync_tmux(socket, ["show-options", "-gqv", opt]).output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => String::new(),
    }
}

fn run(socket: Option<&str>, args: &[&str]) -> Result<()> {
    let status = sync_tmux(socket, args).status().map_err(BosunError::Io)?;
    if !status.success() {
        return Err(BosunError::Tmux(format!(
            "tmux {:?} failed: {}",
            args, status
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_chip_highlights_attached() {
        let attached = format_chip(2, "main", true);
        assert!(attached.contains("#[bg=#1e2433,fg=#7c5cff,bold]"));
        assert!(attached.contains(" 2:main "));

        let idle = format_chip(3, "work", false);
        assert!(idle.contains("#[fg=#e6e9ef]"));
        assert!(idle.contains(" 3:work "));
    }

    #[test]
    fn escape_tmux_format_doubles_hash() {
        assert_eq!(escape_tmux_format("a#b"), "a##b");
        assert_eq!(escape_tmux_format("no hash"), "no hash");
        assert_eq!(escape_tmux_format("#start"), "##start");
    }

    #[test]
    fn tmux_quote_wraps_and_escapes() {
        assert_eq!(tmux_quote("plain"), "\"plain\"");
        assert_eq!(tmux_quote("has space"), "\"has space\"");
        assert_eq!(tmux_quote("has\"quote"), "\"has\\\"quote\"");
    }

    const TEST_HINT: &str = "#[fg=#7c8495]^Q detach · C-a 1-9 jump ";

    #[test]
    fn build_status_right_caps_at_nine() {
        let sessions: Vec<(String, bool)> = (1..=12).map(|i| (format!("s{}", i), i == 3)).collect();
        let out = build_status_right(&sessions, TEST_HINT);
        assert!(out.contains("9:s9"));
        assert!(!out.contains("10:s10"));
        assert!(out.contains(TEST_HINT));
    }

    #[test]
    fn build_status_right_empty_keeps_hint() {
        let out = build_status_right(&[], TEST_HINT);
        assert_eq!(out, TEST_HINT);
    }

    #[test]
    fn build_status_right_escapes_hash_in_names() {
        let sessions = vec![("pre#post".to_string(), false)];
        let out = build_status_right(&sessions, TEST_HINT);
        assert!(out.contains("1:pre##post"));
    }

    #[test]
    fn digit_key_is_bare_digit() {
        assert_eq!(digit_key(1), "1");
        assert_eq!(digit_key(9), "9");
    }

    #[test]
    fn saved_name_uses_underscores() {
        assert_eq!(saved_name("status-left"), "@bosun_saved_status_left");
        assert_eq!(saved_name("status"), "@bosun_saved_status");
    }
}
