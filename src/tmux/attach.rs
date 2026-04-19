//! Attach / detach orchestration.
//!
//! The trick: we install `tmux bind-key -T root C-q detach-client` on the
//! bosun socket, and re-assert it on every refresh tick (see
//! `tmux_actor::do_refresh`). Re-asserting matters because some workflows
//! re-source tmux config or otherwise clobber the root key table mid-session;
//! a one-shot bind can silently disappear over the course of a long attach.
//! The binding is cleared when the tmux actor exits and by the panic hook.
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
/// we don't leave a dangling `C-q` binding if Bosun crashes.
/// Uses `output()` so any error text is captured instead of spilled.
pub fn emergency_unbind(socket: Option<&str>) {
    let _ = Command::new("tmux")
        .args(match socket {
            Some(s) => vec!["-L", s, "unbind-key", "-T", "root", "C-q"],
            None => vec!["unbind-key", "-T", "root", "C-q"],
        })
        .output();
}
