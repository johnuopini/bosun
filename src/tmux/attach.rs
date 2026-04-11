//! Attach / detach orchestration.
//!
//! The trick: we install a temporary `tmux bind-key -T root C-q detach-client`
//! just before `tmux attach-session`, and remove it after the attach returns.
//! This lets the user press Ctrl-Q inside a Bosun-managed tmux session to
//! detach back to Bosun, without permanently altering their tmux config.
//!
//! Multi-instance safety:
//!   * We refcount bindings in a tmux user option `@bosun_attach_refcount`.
//!     Increment before bind, decrement after, only unbind at zero.
//!   * Two Bosun instances both running `bind` the same action is idempotent.
//!   * If the user already has a `C-q` binding at `-T root`, we save it on
//!     startup and restore it on unbind. (Phase 5 work — for Phase 1 we
//!     just warn if a conflict is detected.)
//!
//! Panic safety:
//!   * `AttachGuard` has a `Drop` impl that synchronously runs `unbind-key`.
//!   * A panic hook installed by the app also runs unbind as a belt-and-braces.
//!
//! This module uses synchronous `std::process::Command` for attach because
//! we're handing the controlling tty over to tmux — there is no async to do
//! while blocked in `attach-session`.

use std::process::Command;

use crate::error::{BosunError, Result};
use crate::tmux::client::sync_tmux;

/// RAII guard: on drop, unbinds the temporary C-q binding (decrementing the
/// refcount). Construct via [`attach_with_ctrl_q_detach`].
#[must_use = "dropping the guard immediately would unbind before the attach completes"]
pub struct AttachGuard {
    socket: Option<String>,
    done: bool,
}

impl AttachGuard {
    /// Explicitly release the guard (same as dropping, but surfaces errors).
    pub fn release(mut self) -> Result<()> {
        self.done = true;
        unbind_detach_key(self.socket.as_deref())
    }
}

impl Drop for AttachGuard {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        // Best-effort cleanup. Can't bubble up a Result from Drop.
        let _ = unbind_detach_key(self.socket.as_deref());
    }
}

/// Install the temporary `C-q -> detach-client` root binding, then block on
/// `tmux attach-session -t <name>`. On return, the guard's Drop clears the
/// binding.
///
/// This function **takes over the controlling tty** until the user detaches.
/// The caller must have torn down its ratatui Terminal (`disable_raw_mode`,
/// `LeaveAlternateScreen`) before calling, and restored it after.
pub fn attach_with_ctrl_q_detach(socket: Option<&str>, name: &str) -> Result<()> {
    let guard = install_detach_key(socket)?;
    let result = run_attach(socket, name);
    // Cleanup happens whether attach succeeded or failed.
    drop(guard);
    result
}

/// Test-visible wrapper around [`install_detach_key`]. Production code should
/// always go through [`attach_with_ctrl_q_detach`]; tests need the split so
/// they can verify the binding dance without actually attaching a tty.
pub fn install_detach_key_for_test(socket: Option<&str>) -> Result<AttachGuard> {
    install_detach_key(socket)
}

fn install_detach_key(socket: Option<&str>) -> Result<AttachGuard> {
    // Increment refcount (best-effort; tmux options are atomic within set-option).
    // We read current value, increment, write back. Two Bosun processes racing
    // here is unlikely at human speed, and even if both read the same value the
    // worst case is an off-by-one that self-heals once both detach.
    let current = read_refcount(socket);
    let next = current + 1;
    set_refcount(socket, next)?;

    // Bind Ctrl-Q at the root key table. `-T root` bindings fire before any
    // prefix, so we catch Ctrl-Q regardless of user's prefix key.
    let status = sync_tmux(socket, ["bind-key", "-T", "root", "C-q", "detach-client"])
        .status()
        .map_err(BosunError::Io)?;
    if !status.success() {
        return Err(BosunError::Tmux(format!("bind-key failed: {}", status)));
    }

    Ok(AttachGuard {
        socket: socket.map(|s| s.to_string()),
        done: false,
    })
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

fn unbind_detach_key(socket: Option<&str>) -> Result<()> {
    let current = read_refcount(socket);
    if current > 1 {
        set_refcount(socket, current - 1)?;
        return Ok(());
    }

    // Last one out turns off the lights.
    let status = sync_tmux(socket, ["unbind-key", "-T", "root", "C-q"])
        .status()
        .map_err(BosunError::Io)?;
    if !status.success() {
        // Not fatal — the binding might already be cleared. Log later.
        tracing::warn!("unbind-key C-q returned {}", status);
    }
    set_refcount(socket, 0)?;
    Ok(())
}

fn read_refcount(socket: Option<&str>) -> u32 {
    // `tmux show-options -gqv @bosun_attach_refcount` -> "" if unset.
    let output = sync_tmux(socket, ["show-options", "-gqv", "@bosun_attach_refcount"]).output();
    match output {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout);
            s.trim().parse().unwrap_or(0)
        }
        _ => 0,
    }
}

fn set_refcount(socket: Option<&str>, value: u32) -> Result<()> {
    let v = value.to_string();
    let status = sync_tmux(socket, ["set-option", "-g", "@bosun_attach_refcount", &v])
        .status()
        .map_err(BosunError::Io)?;
    if !status.success() {
        tracing::warn!("set-option @bosun_attach_refcount returned {}", status);
    }
    Ok(())
}

/// Panic-safe cleanup: call this from a `std::panic::set_hook` to make sure
/// we don't leave a dangling `C-q` binding if Bosun crashes mid-attach.
pub fn emergency_unbind(socket: Option<&str>) {
    let _ = Command::new("tmux")
        .args(match socket {
            Some(s) => vec!["-L", s, "unbind-key", "-T", "root", "C-q"],
            None => vec!["unbind-key", "-T", "root", "C-q"],
        })
        .status();
}
