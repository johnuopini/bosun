//! Long-lived `tmux -C` subprocess wrapper. Feeds stdout lines
//! through [`ControlParser`] and exposes a `Notification` stream to
//! consumers.
//!
//! ## Lifecycle
//!
//! Bosun keeps a dedicated "monitor" tmux session (`__bosun_monitor`)
//! that the control subprocess attaches to in control mode. The
//! session is filtered out of the main UI by the usual
//! `BOSUN_PREFIX` rule (`bosun-` by default — `__bosun_monitor`
//! starts with `__` and never matches). It's created idempotently on
//! first start and persists across bosun runs; tmux itself handles
//! cleanup when the server exits.
//!
//! Spawn a client with [`ControlClient::spawn`], then poll
//! [`ControlClient::recv`] for notifications. Dropping the client
//! kills the subprocess (best effort via `start_kill`; tmux exits
//! cleanly on `%exit` which we send by closing stdin).

use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::error::{BosunError, Result};
use crate::tmux::control::{ControlParser, Notification};

/// Session name bosun uses for its control-mode monitor. Chosen so
/// the `bosun-` prefix filter in `Config::manages` doesn't include
/// it — users never see this session in the bosun UI.
pub const MONITOR_SESSION: &str = "__bosun_monitor";

/// Drop guard for a running tmux -C subprocess. Owns the child
/// handle and the stdout-reader task. Dropping the guard kills
/// the subprocess (best-effort SIGKILL via `start_kill`) — the
/// reader task observes stdout EOF and exits naturally.
///
/// The notification stream is **deliberately not** a field of this
/// struct. Splitting it out lets a consumer put the receiver
/// directly into `tokio::select!` alongside other channels without
/// borrow-checker gymnastics.
pub struct ControlClient {
    child: Child,
    _reader: JoinHandle<()>,
}

impl ControlClient {
    /// Ensure the monitor session exists, then spawn a `tmux -C
    /// attach-session` subprocess against it. Returns `(guard,
    /// notifications)` — the guard keeps the subprocess alive and
    /// the receiver yields parsed notifications until the
    /// subprocess exits or the guard is dropped.
    pub async fn spawn(
        socket: Option<&str>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Notification>)> {
        ensure_monitor_session(socket).await?;

        let mut cmd = Command::new("tmux");
        if let Some(s) = socket {
            cmd.arg("-L").arg(s);
        }
        cmd.arg("-C")
            .arg("attach-session")
            .arg("-t")
            .arg(MONITOR_SESSION)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        let mut child = cmd
            .spawn()
            .map_err(|e| BosunError::Tmux(format!("failed to spawn tmux -C monitor: {e}")))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| BosunError::Tmux("tmux -C monitor: missing stdout".into()))?;

        let (tx, rx) = mpsc::unbounded_channel::<Notification>();

        let reader = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            let mut parser = ControlParser::new();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        if let Some(notif) = parser.feed(&line) {
                            if tx.send(notif).is_err() {
                                // Receiver dropped — caller is gone.
                                break;
                            }
                        }
                    }
                    Ok(None) => {
                        // EOF — tmux control subprocess exited.
                        // Emit a synthetic Exit so the consumer knows.
                        let _ = tx.send(Notification::Exit);
                        break;
                    }
                    Err(e) => {
                        tracing::warn!("tmux -C stdout read error: {}", e);
                        let _ = tx.send(Notification::Exit);
                        break;
                    }
                }
            }
        });

        Ok((
            Self {
                child,
                _reader: reader,
            },
            rx,
        ))
    }
}

impl Drop for ControlClient {
    fn drop(&mut self) {
        // Best-effort teardown. `start_kill` is fire-and-forget: it
        // sends SIGKILL to the tmux -C subprocess. We don't `await`
        // because Drop isn't async — the child will be reaped by
        // the tokio runtime's process handling once the signal is
        // delivered.
        let _ = self.child.start_kill();
    }
}

/// Create the monitor session if it doesn't already exist. Treats
/// "duplicate session" / "already exists" errors as success — this
/// function is idempotent so bosun can call it every startup.
async fn ensure_monitor_session(socket: Option<&str>) -> Result<()> {
    let mut cmd = Command::new("tmux");
    if let Some(s) = socket {
        cmd.arg("-L").arg(s);
    }
    cmd.args(["new-session", "-d", "-s", MONITOR_SESSION]);

    let output = cmd
        .output()
        .await
        .map_err(|e| BosunError::Tmux(format!("create monitor session: {e}")))?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("already exists") || stderr.contains("duplicate session") {
        // Idempotent: session already there, that's what we want.
        return Ok(());
    }

    Err(BosunError::Tmux(format!(
        "create monitor session: {}",
        stderr.trim()
    )))
}
