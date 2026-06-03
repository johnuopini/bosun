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
//!     after bosun exits (the session keeps bosun's status-right hint
//!     until it's killed). Harmless, and the next bosun run will
//!     reuse them.
//!
//! This module is synchronous. Its callers are the `tmux_actor` task
//! (which is fine to briefly block) and the panic hook (which must be
//! sync).

use crate::error::{BosunError, Result};
use crate::tmux::client::sync_tmux;

/// One row in the status bar's session list. Carries both the
/// internal tmux session name (for `switch-client -t`) and the
/// pretty display name (for the chip label). Passing just a
/// display name confuses tmux because bosun sessions are stored
/// as `bosun-<display>-<hex>` internally.
#[derive(Debug, Clone)]
pub struct BarSession {
    /// The actual tmux session name — what `tmux list-sessions` gives you.
    pub internal: String,
    /// The pretty name the user sees in the bosun UI and in the chip.
    pub display: String,
    /// True if any client is currently attached to this session.
    pub attached: bool,
}

// --- Visual constants -------------------------------------------------

/// Status-left: brand + current session's display name. The name is
/// read from the per-session `@bosun_display` user option (set when
/// the session is created); we fall back to `#S` (the internal tmux
/// name) for any session that somehow lacks it. The same format
/// string works for every session because tmux expands
/// `#{@bosun_display}` in the context of whichever session the bar is
/// being drawn for.
///
/// Empty by design. The active session's display name already shows
/// in bosun's tab strip at the top of the pane, and bosun's own TUI
/// footer carries the brand — so a tmux status-left segment here is
/// pure redundancy fighting the agent UI running inside the pane.
/// The status-right hint (detach / cycle / jump keys) still renders,
/// and the prefix+1..9 jump bindings still install regardless (see
/// `bind_jump_keys`).
const STATUS_LEFT: &str = "";
/// Unused now that `STATUS_LEFT` is empty, but kept so the
/// `set-option` call site stays uniform with `status-right`.
const STATUS_LEFT_LEN: &str = "0";
const STATUS_RIGHT_LEN: &str = "120";
const STATUS_STYLE: &str = "bg=default,fg=#e6e9ef";

// --- Public API ------------------------------------------------------

/// Install the global parts of the status bar: prefix-1..9 jump
/// bindings. The per-session status-* options are written by
/// `configure_session`. Idempotent — safe to call every tick.
pub fn install_globals(socket: Option<&str>, sessions: &[BarSession]) -> Result<()> {
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
    _sessions: &[BarSession],
) -> Result<()> {
    let hint = build_hint(socket);
    let target = &["-t", session];

    run_targeted(socket, target, &["set-option", "status", "on"])?;
    run_targeted(
        socket,
        target,
        &["set-option", "status-style", STATUS_STYLE],
    )?;
    run_targeted(socket, target, &["set-option", "status-left", STATUS_LEFT])?;
    run_targeted(
        socket,
        target,
        &["set-option", "status-left-length", STATUS_LEFT_LEN],
    )?;
    run_targeted(socket, target, &["set-option", "status-justify", "left"])?;
    run_targeted(socket, target, &["set-option", "status-right", &hint])?;
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

fn bind_jump_keys(socket: Option<&str>, sessions: &[BarSession]) -> Result<()> {
    for i in 0..9 {
        let key = digit_key(i + 1);
        match sessions.get(i) {
            Some(entry) => {
                // Target must be the INTERNAL tmux name, not the
                // display name — tmux wouldn't find `ytunnel` when
                // the actual session is `bosun-ytunnel-3b0529e8`.
                let target = tmux_quote(&entry.internal);
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

/// Build the right-hand hint string with the user's actual prefix key.
/// Defaults to `C-b` (tmux's shipped default) if the option is unset.
fn build_hint(socket: Option<&str>) -> String {
    let prefix = show_option(socket, "prefix");
    let prefix = if prefix.is_empty() || prefix == "None" {
        "C-b"
    } else {
        prefix.as_str()
    };
    format!("#[fg=#7c8495]^Q detach · S-←→ cycle · {} 1-9 jump ", prefix)
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

/// Run a tmux command quietly — capture stdout/stderr so they don't
/// bleed into bosun's alt-screen TUI. This is called continuously
/// from the actor on every refresh; if the tmux server dies mid-run
/// (last session exited), subsequent calls fail with "no server
/// running..." on stderr, and we really don't want that text painted
/// over the UI.
fn run(socket: Option<&str>, args: &[&str]) -> Result<()> {
    let output = sync_tmux(socket, args).output().map_err(BosunError::Io)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(BosunError::Tmux(format!(
            "tmux {:?} failed: {}",
            args,
            stderr.trim()
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

    #[test]
    fn tmux_quote_wraps_and_escapes() {
        assert_eq!(tmux_quote("plain"), "\"plain\"");
        assert_eq!(tmux_quote("has space"), "\"has space\"");
        assert_eq!(tmux_quote("has\"quote"), "\"has\\\"quote\"");
    }

    #[test]
    fn status_left_is_empty() {
        // The pane's tmux status-left was dropped entirely: the session
        // name lives in bosun's tab strip and the brand in its TUI
        // footer, so a left segment here would just be redundant.
        assert_eq!(STATUS_LEFT, "");
    }

    #[test]
    fn hint_includes_shift_arrow_cycle() {
        let hint = build_hint(None);
        assert!(hint.contains("S-←→ cycle"));
        assert!(hint.contains("^Q detach"));
        assert!(hint.contains("1-9 jump"));
    }

    #[test]
    fn digit_key_is_bare_digit() {
        assert_eq!(digit_key(1), "1");
        assert_eq!(digit_key(9), "9");
    }
}
