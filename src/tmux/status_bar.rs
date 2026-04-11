//! Per-session bosun-branded tmux status bar + prefix-1..9 quick-jump
//! bindings.
//!
//! Why per-session: tmux's `status-*` options have a global default and
//! per-session overrides. If we set them globally (as agent-deck does
//! and as the previous version of this module did) we overwrite the
//! user's bar for **every** session on the server — including sessions
//! managed by other tools. By writing per-session options instead, we
//! only touch sessions bosun is managing and leave everything else
//! alone. Agent-deck's sessions keep agent-deck's footer; bosun's
//! sessions get bosun's footer.
//!
//! Key bindings live on the server (not a session) so the prefix-1..9
//! jump keys have to be global. We install them once when bosun sees
//! its first managed session and unbind them on exit. We don't save
//! the user's original bindings for those keys — Phase 5 TODO.
//!
//! Cleanup responsibilities:
//!   * Per-session options die when the session dies — no cleanup
//!     needed on session kill.
//!   * Global bindings are removed by `ActorStatusBar::drop` when the
//!     actor task ends, and by the panic hook via `emergency_uninstall`.
//!   * Per-session options on still-living sessions are left in place
//!     after bosun exits (the session still reads "⚓ bosun" in the
//!     bar until it's killed). Harmless, and the next bosun run will
//!     reuse them.
//!
//! This module is synchronous. Its callers are the `tmux_actor` task
//! (which is fine to briefly block) and the panic hook (which must be
//! sync).

use crate::error::{BosunError, Result};
use crate::tmux::client::sync_tmux;

// --- Visual constants -------------------------------------------------

const BRAND: &str = "#[bg=#7c5cff,fg=#0b0d12,bold] ⚓ bosun #[default] ";
const BRAND_LEN: &str = "14";
const STATUS_RIGHT_LEN: &str = "400";
const STATUS_STYLE: &str = "bg=default,fg=#e6e9ef";

// --- Public API ------------------------------------------------------

/// Install the global parts of the status bar: prefix-1..9 jump
/// bindings. The per-session status-* options are written by
/// `configure_session`. Idempotent — safe to call every tick.
pub fn install_globals(socket: Option<&str>, sessions: &[(String, bool)]) -> Result<()> {
    bind_jump_keys(socket, sessions)
}

/// Remove the global prefix-1..9 jump bindings. Called on graceful
/// shutdown by `ActorStatusBar::drop` and from the panic hook.
pub fn uninstall_globals(socket: Option<&str>) {
    unbind_jump_keys(socket);
}

/// Write bosun's status bar options onto a single session. Only touches
/// that session; other sessions on the same tmux server are unaffected.
/// The status-right list shows all `sessions` passed in (which should
/// be bosun's filtered view), with the matching entry highlighted.
pub fn configure_session(
    socket: Option<&str>,
    session: &str,
    sessions: &[(String, bool)],
) -> Result<()> {
    let hint = build_hint(socket);
    let status_right = build_status_right(sessions, &hint);
    let target = &["-t", session];

    run_targeted(socket, target, &["set-option", "status", "on"])?;
    run_targeted(
        socket,
        target,
        &["set-option", "status-style", STATUS_STYLE],
    )?;
    run_targeted(socket, target, &["set-option", "status-left", BRAND])?;
    run_targeted(
        socket,
        target,
        &["set-option", "status-left-length", BRAND_LEN],
    )?;
    run_targeted(socket, target, &["set-option", "status-justify", "left"])?;
    run_targeted(
        socket,
        target,
        &["set-option", "status-right", &status_right],
    )?;
    run_targeted(
        socket,
        target,
        &["set-option", "status-right-length", STATUS_RIGHT_LEN],
    )?;
    Ok(())
}

/// Best-effort cleanup for panic-hook use. Removes only the global
/// bindings; per-session options are left in place (they'll die with
/// their sessions and can't cause a key-table conflict on their own).
pub fn emergency_uninstall(socket: Option<&str>) {
    unbind_jump_keys(socket);
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
fn tmux_quote(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{}\"", escaped)
}

// --- Internal: tmux shell helpers -----------------------------------

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

/// Run a targeted set-option. `target` is `["-t", session]`, inserted
/// between the subcommand and its args. We don't pass `-g` because the
/// whole point of this function is to write session-local overrides.
fn run_targeted(socket: Option<&str>, target: &[&str], args: &[&str]) -> Result<()> {
    // args[0] is the subcommand (e.g. "set-option"); inject target after.
    let mut full: Vec<&str> = Vec::with_capacity(args.len() + target.len());
    full.push(args[0]);
    full.extend_from_slice(target);
    full.extend_from_slice(&args[1..]);
    run(socket, &full)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_HINT: &str = "#[fg=#7c8495]^Q detach · C-a 1-9 jump ";

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
}
