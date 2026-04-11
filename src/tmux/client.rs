//! Shell-out layer for tmux. Every byte of tmux I/O lives here or in
//! `attach.rs`. Exposing a trait lets us plug a mock for unit tests.

use std::ffi::OsStr;
use std::process::Stdio;

use async_trait::async_trait;
use tokio::process::Command;

use crate::error::{BosunError, Result};
use crate::tmux::parse::{parse_list_sessions, LIST_SESSIONS_FORMAT};
use crate::tmux::session::TmuxSession;

/// Abstraction over the tmux CLI. Real impl shells out; mocks record calls.
#[async_trait]
pub trait TmuxClient: Send + Sync {
    /// Run `tmux list-sessions` and return parsed sessions. An empty
    /// server (exit code 1, "no server running") returns `Ok(vec![])`.
    async fn list_sessions(&self) -> Result<Vec<TmuxSession>>;

    /// Capture the current visible pane (what the user actually sees
    /// right now — no scrollback history), preserving ANSI escape
    /// sequences so we can render them with `ansi-to-tui` and pass
    /// them to detectors. Dead sessions return `Ok(vec![])`.
    async fn capture_pane(&self, session: &str) -> Result<Vec<u8>>;
}

/// Production implementation backed by `tokio::process::Command`.
/// Supports an optional `-L <socket>` for test isolation.
#[derive(Debug, Clone)]
pub struct TokioTmuxClient {
    socket: Option<String>,
}

impl TokioTmuxClient {
    pub fn new() -> Self {
        Self { socket: None }
    }

    #[allow(dead_code)]
    pub fn with_socket(socket: impl Into<String>) -> Self {
        Self {
            socket: Some(socket.into()),
        }
    }

    /// Build a tmux command with the configured socket prefix.
    pub(crate) fn cmd(&self) -> Command {
        let mut c = Command::new("tmux");
        if let Some(sock) = &self.socket {
            c.arg("-L").arg(sock);
        }
        c.stdin(Stdio::null());
        c.kill_on_drop(true);
        c
    }

    /// Pull the socket flag for use by `attach.rs` when it needs to spawn
    /// its own non-`tokio` process (attach must be synchronous on the
    /// controlling tty).
    #[allow(dead_code)]
    pub fn socket(&self) -> Option<&str> {
        self.socket.as_deref()
    }
}

impl Default for TokioTmuxClient {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TmuxClient for TokioTmuxClient {
    async fn list_sessions(&self) -> Result<Vec<TmuxSession>> {
        let mut cmd = self.cmd();
        cmd.arg("list-sessions").arg("-F").arg(LIST_SESSIONS_FORMAT);
        let output = cmd.output().await.map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => BosunError::TmuxNotInstalled,
            _ => BosunError::Io(e),
        })?;

        if output.status.success() {
            let s = String::from_utf8_lossy(&output.stdout);
            return parse_list_sessions(&s);
        }

        // tmux exits non-zero when there are no sessions. The phrasing varies
        // by how we got there:
        //   * Attached but zero sessions: "no server running on /tmp/tmux-501/default"
        //   * Custom -L socket that was never created:
        //     "error connecting to /private/tmp/tmux-501/<name> (No such file or directory)"
        //   * Some versions: "no sessions"
        // All three mean "empty" for our purposes.
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("no server running")
            || stderr.contains("no sessions")
            || (stderr.contains("error connecting") && stderr.contains("No such file or directory"))
        {
            return Ok(Vec::new());
        }

        Err(BosunError::Tmux(format!(
            "list-sessions failed ({}): {}",
            output.status,
            stderr.trim()
        )))
    }

    async fn capture_pane(&self, session: &str) -> Result<Vec<u8>> {
        let mut cmd = self.cmd();
        // -p : stdout
        // -e : include escape sequences
        // -J : join wrapped lines (so we don't split in the middle of an
        //      ANSI sequence)
        // No -S/-E flags: we want just the currently visible pane — no
        // scrollback history. Scrollback would pick up whatever the user
        // typed earlier (e.g. literal `printf '\033[32m...'` source),
        // which looks like escape code garbage in the preview.
        cmd.arg("capture-pane")
            .arg("-p")
            .arg("-e")
            .arg("-J")
            .arg("-t")
            .arg(session);

        let output = cmd.output().await.map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => BosunError::TmuxNotInstalled,
            _ => BosunError::Io(e),
        })?;

        if output.status.success() {
            return Ok(output.stdout);
        }

        // Session may have just been killed — treat as empty capture.
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("can't find session") || stderr.contains("no server running") {
            return Ok(Vec::new());
        }
        Err(BosunError::Tmux(format!(
            "capture-pane {} failed ({}): {}",
            session,
            output.status,
            stderr.trim()
        )))
    }
}

/// Build a synchronous `std::process::Command` for tmux with the given args.
/// Used by `attach.rs` and other places that need blocking semantics.
#[allow(dead_code)]
pub(crate) fn sync_tmux<I, S>(socket: Option<&str>, args: I) -> std::process::Command
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut c = std::process::Command::new("tmux");
    if let Some(sock) = socket {
        c.arg("-L").arg(sock);
    }
    for a in args {
        c.arg(a);
    }
    c
}
