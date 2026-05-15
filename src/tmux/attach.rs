//! Attach / detach orchestration.
//!
//! The trick: we install `tmux bind-key -T root C-q detach-client` on the
//! bosun socket, and re-assert it on every refresh tick (see
//! `tmux_actor::do_refresh`). Re-asserting matters because some workflows
//! re-source tmux config or otherwise clobber the root key table mid-session;
//! a one-shot bind can silently disappear over the course of a long attach.
//! The binding is cleared when the tmux actor exits and by the panic hook.
//!
//! Alongside C-q we also install prefix-less S-Left / S-Right bindings that
//! walk recently-used sessions in MRU order (see `ensure_session_cycle_bound`).
//! Same lifecycle: re-asserted every refresh tick, cleared on shutdown and
//! by the panic hook.
//!
//! This module uses synchronous `std::process::Command` for attach because
//! we're handing the controlling tty over to tmux — there is no async to do
//! while blocked in `attach-session`.

use std::process::Command;

use crate::error::{BosunError, Result};
use crate::tmux::client::sync_tmux;

/// Block on `tmux attach-session -t <name>`. The `C-q -> detach-client`
/// root binding is owned by the tmux actor's lifecycle — it's re-asserted
/// on every refresh tick — so this function no longer installs/removes it.
///
/// This function **takes over the controlling tty** until the user detaches.
/// The caller must have torn down its ratatui Terminal (`disable_raw_mode`,
/// `LeaveAlternateScreen`) before calling, and restored it after.
pub fn attach_with_ctrl_q_detach(socket: Option<&str>, name: &str) -> Result<()> {
    // Belt-and-braces: re-assert right before attach in case the last
    // tick was long enough ago that the binding could have been lost.
    // `bind-key` is idempotent in tmux (repeated binds overwrite).
    ensure_ctrl_q_bound(socket);
    run_attach(socket, name)
}

/// Install (or re-assert) the `C-q -> detach-client` root binding.
/// Idempotent — `bind-key` silently overwrites an existing binding.
/// Failure is logged but not returned: we don't want a transient tmux
/// hiccup to turn into a surfaced error on every refresh tick.
pub fn ensure_ctrl_q_bound(socket: Option<&str>) {
    let out = sync_tmux(socket, ["bind-key", "-T", "root", "C-q", "detach-client"]).output();
    match out {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            tracing::warn!("bind-key C-q: {}", stderr.trim());
        }
        Err(e) => tracing::warn!("bind-key C-q: {}", e),
    }
}

/// Remove the `C-q -> detach-client` root binding. Called on clean
/// actor shutdown so we don't leave the bosun socket's tmux server
/// with a stray binding.
pub fn clear_ctrl_q_bound(socket: Option<&str>) {
    let out = sync_tmux(socket, ["unbind-key", "-T", "root", "C-q"]).output();
    if let Ok(o) = out {
        if !o.status.success() {
            let stderr = String::from_utf8_lossy(&o.stderr);
            tracing::warn!("unbind-key C-q: {}", stderr.trim());
        }
    }
}

fn run_attach(socket: Option<&str>, name: &str) -> Result<()> {
    let status = sync_tmux(socket, ["attach-session", "-t", name])
        .status()
        .map_err(BosunError::Io)?;
    if !status.success() {
        return Err(BosunError::Tmux(format!(
            "attach-session -t {} failed: {}",
            name, status
        )));
    }
    Ok(())
}

/// Panic-safe cleanup: call this from a `std::panic::set_hook` to make sure
/// we don't leave a dangling `C-q` binding (or the S-Left / S-Right cycle
/// binds) if Bosun crashes.
/// Uses `output()` so any error text is captured instead of spilled.
pub fn emergency_unbind(socket: Option<&str>) {
    let runs: &[&[&str]] = &[
        &["unbind-key", "-T", "root", "C-q"],
        &["unbind-key", "-n", "S-Left"],
        &["unbind-key", "-n", "S-Right"],
        &["unbind-key", "-T", "prefix", "o"],
    ];
    for args in runs {
        let mut argv: Vec<&str> = Vec::with_capacity(args.len() + 2);
        if let Some(s) = socket {
            argv.push("-L");
            argv.push(s);
        }
        argv.extend_from_slice(args);
        let _ = Command::new("tmux").args(&argv).output();
    }
}

/// Install (or re-assert) the S-Left / S-Right prefix-less bindings that
/// cycle between sessions in most-recently-attached order.
///
/// We use shift+arrow instead of option+arrow because tmux's default
/// `M-Left`/`M-Right` bindings (pane navigation) are still useful even
/// inside bosun's tmux server. `S-Left`/`S-Right` default to
/// `previous-window`/`next-window` — bosun sessions don't use multiple
/// windows, so reclaiming those keys costs us nothing.
///
/// - `S-Left` (shift+left) → the single most-recently-attached session
///   other than the current one. Acts as a fast A↔B toggle.
/// - `S-Right` (shift+right) → the **second** most-recently-attached
///   non-current session, so repeated taps walk a 3-session active set.
///   Falls back to the most-recent-non-current if only one candidate
///   exists.
///
/// The bindings are prefix-less (`-n`) so they work from inside any tmux
/// session without needing the bosun prefix. The socket flag is baked into
/// the `run-shell` body so the inner tmux invocations talk to the same
/// server the bind lives on.
///
/// Idempotent — `bind-key` overwrites. Failures are logged but not
/// returned: a transient hiccup on a refresh tick shouldn't bubble up.
pub fn ensure_session_cycle_bound(socket: Option<&str>) {
    let tmux = match socket {
        Some(s) => format!("tmux -L {}", shell_quote(s)),
        None => "tmux".to_string(),
    };

    // tmux's `session_last_attached` is empty for never-attached
    // sessions, which makes default awk field-splitting return the
    // name in $1 instead of $2. Wrap it in a `?:,0` conditional so
    // every row starts with a numeric timestamp, even if it's just 0.
    //
    // Why the doubled `##`: `run-shell` format-expands its argument
    // when the key fires, so a literal `#{session_name}` in the bind
    // body would get substituted against the current session before
    // /bin/sh ever runs. We need the inner `list-sessions -F` to do
    // the per-row expansion, so we write `##{...}` here — tmux's
    // expansion at trigger time collapses each `##` to `#`, leaving
    // the actual format string intact for the nested tmux invocation.
    // Same trick for `##S` in display-message.
    let fmt = "##{?session_last_attached,##{session_last_attached},0} ##{session_name}";
    // Excluded from cycle nav: bosun's internal control-mode monitor
    // session (see `tmux::control_client::MONITOR_SESSION`). Users
    // should never end up looking at its single inert pane.
    let exclude = crate::tmux::control_client::MONITOR_SESSION;

    // shift+Left → 1st non-current in MRU desc order.
    let left_cmd = format!(
        "T=$({tmux} list-sessions -F '{fmt}' \
         | sort -rnk1 \
         | awk -v cur=\"$({tmux} display-message -p '##S')\" \
               '$2 != cur && $2 != \"{exclude}\" {{print $2; exit}}'); \
         [ -n \"$T\" ] && {tmux} switch-client -t \"$T\""
    );

    // shift+Right → 2nd non-current in MRU desc order, fall back to 1st.
    let right_cmd = format!(
        "cur=$({tmux} display-message -p '##S'); \
         L=$({tmux} list-sessions -F '{fmt}' \
         | sort -rnk1 \
         | awk -v cur=\"$cur\" \
               '$2 != cur && $2 != \"{exclude}\" {{print $2}}'); \
         T=$(printf '%s\\n' \"$L\" | sed -n '2p'); \
         [ -z \"$T\" ] && T=$(printf '%s\\n' \"$L\" | sed -n '1p'); \
         [ -n \"$T\" ] && {tmux} switch-client -t \"$T\""
    );

    for (key, body) in [("S-Left", &left_cmd), ("S-Right", &right_cmd)] {
        let out = sync_tmux(socket, ["bind-key", "-n", key, "run-shell", body]).output();
        match out {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                tracing::warn!("bind-key {}: {}", key, stderr.trim());
            }
            Err(e) => tracing::warn!("bind-key {}: {}", key, e),
        }
    }
}

/// Remove the S-Left / S-Right cycle bindings. Called on clean actor
/// shutdown.
pub fn clear_session_cycle_bound(socket: Option<&str>) {
    for key in ["S-Left", "S-Right"] {
        let out = sync_tmux(socket, ["unbind-key", "-n", key]).output();
        if let Ok(o) = out {
            if !o.status.success() {
                let stderr = String::from_utf8_lossy(&o.stderr);
                tracing::warn!("unbind-key {}: {}", key, stderr.trim());
            }
        }
    }
}

/// Install (or re-assert) the `prefix + o` quick-jump binding. Opens
/// a floating tmux popup running `choose-tree -Zs` — built-in fuzzy
/// session picker with type-ahead. Enter switches to the chosen
/// session, Escape closes the popup.
///
/// We bind under the user's prefix (typically C-a) rather than a
/// modifier-key combo so it works without any terminal-emulator
/// configuration. shift+option+o would require iTerm's "Left Option
/// Key" to be set to Esc+/Meta, which we can't expect — and on most
/// macOS terminals it renders as `Ø` by default.
///
/// `__bosun_monitor` is filtered out so it never appears in the
/// chooser.
///
/// Idempotent — `bind-key` overwrites. Failures are logged, not
/// returned.
pub fn ensure_quick_jump_bound(socket: Option<&str>) {
    let tmux = match socket {
        Some(s) => format!("tmux -L {}", shell_quote(s)),
        None => "tmux".to_string(),
    };
    let exclude = crate::tmux::control_client::MONITOR_SESSION;
    // `display-popup -E` format-expands its argument at trigger time
    // (same gotcha as `run-shell` for S-Left/S-Right). We want the
    // `#{!=:#{session_name},__bosun_monitor}` filter to reach
    // `choose-tree -f` literally and be evaluated per-row there, so
    // we double-hash. tmux's expansion collapses each `##` to `#`,
    // leaving the format spec intact for the inner tmux invocation.
    let cmd = format!("{tmux} choose-tree -Zs -f '##{{!=:##{{session_name}},{exclude}}}'");
    let out = sync_tmux(
        socket,
        [
            "bind-key",
            "-T",
            "prefix",
            "o",
            "display-popup",
            "-E",
            "-h",
            "70%",
            "-w",
            "60%",
            "-T",
            " bosun · quick switch ",
            &cmd,
        ],
    )
    .output();
    match out {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            tracing::warn!("bind-key prefix o: {}", stderr.trim());
        }
        Err(e) => tracing::warn!("bind-key prefix o: {}", e),
    }
}

/// Remove the `prefix + o` quick-jump binding.
pub fn clear_quick_jump_bound(socket: Option<&str>) {
    let out = sync_tmux(socket, ["unbind-key", "-T", "prefix", "o"]).output();
    if let Ok(o) = out {
        if !o.status.success() {
            let stderr = String::from_utf8_lossy(&o.stderr);
            tracing::warn!("unbind-key prefix o: {}", stderr.trim());
        }
    }
}

/// Minimal POSIX shell single-quote escaper. Wraps the value in `'…'`,
/// turning any embedded `'` into `'\''`. Good enough for tmux socket
/// names — typically just `[A-Za-z0-9_-]+`, but we don't want a surprise
/// command injection if someone puts a quote in `BOSUN_TMUX_SOCKET`.
fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}
