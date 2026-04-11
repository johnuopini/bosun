//! Shell-out layer for tmux. Every byte of tmux I/O lives here or in
//! `attach.rs`. Exposing a trait lets us plug a mock for unit tests.

use std::ffi::OsStr;
use std::process::Stdio;

use async_trait::async_trait;
use tokio::process::Command;

use crate::error::{BosunError, Result};
use crate::tmux::parse::{parse_list_sessions, LIST_SESSIONS_FORMAT};
use crate::tmux::session::TmuxSession;

/// Spec for creating a new tmux session. All strings are expected to
/// already be shell-safe (no unescaped quotes, no interior control
/// characters); the actor is responsible for building this from the
/// form modal's output.
#[derive(Debug, Clone)]
pub struct CreateSpec {
    /// Full tmux session name, including any prefix like `bosun-` and
    /// a uniqueness suffix. This is the name tmux actually uses.
    pub name: String,
    /// Pretty name for the UI. If `Some`, bosun sets the per-session
    /// tmux user option `@bosun_display` to this value so the UI can
    /// show "rasterfox" even though the internal name is
    /// `bosun-rasterfox-a1b2c3d4`.
    pub display_name: Option<String>,
    /// Working directory for the new session. Must exist.
    pub path: String,
    /// Shell command to run as the initial process. Empty means use
    /// the user's default shell.
    pub command: String,
}

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

    /// Create a detached tmux session. The session appears in
    /// subsequent `list_sessions` calls. Returns the name of the
    /// newly-created session on success.
    async fn create_session(&self, spec: &CreateSpec) -> Result<String>;

    /// Kill a tmux session by its internal name. Missing sessions
    /// are treated as success (idempotent).
    async fn kill_session(&self, session: &str) -> Result<()>;

    /// Update the `@bosun_display` per-session user option so the UI
    /// picks up a new pretty label on the next refresh. Does not
    /// change the internal tmux session name.
    async fn set_display_name(&self, session: &str, display: &str) -> Result<()>;
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

    async fn create_session(&self, spec: &CreateSpec) -> Result<String> {
        // Create the session with NO initial command. This starts the
        // user's default login shell, which sources their rc files
        // (zshrc / bashrc) and sets up the environment the way manual
        // `tmux new` + typing the command would. Running the command
        // directly via `new-session -d -s name command` would skip
        // shell init entirely, and agents like Claude rely on that
        // init for things like PATH and (historically) env vars.
        //
        // We deliberately do NOT pass `-e KEY=VALUE` env passthrough
        // here — it inflates the command to dozens of args and didn't
        // resolve the Claude auth issue in testing. Claude reads its
        // credentials from a file or the macOS Keychain, not from env.
        let mut cmd = self.cmd();
        cmd.arg("new-session").arg("-d").arg("-s").arg(&spec.name);
        if !spec.path.is_empty() {
            cmd.arg("-c").arg(&spec.path);
        }
        let output = cmd.output().await.map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => BosunError::TmuxNotInstalled,
            _ => BosunError::Io(e),
        })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(BosunError::Tmux(format!(
                "new-session -s {} failed: {}",
                spec.name,
                stderr.trim()
            )));
        }

        // Step 2: set the pretty display name on the freshly-created
        // session via a per-session user option. Best-effort — if
        // this fails, the UI falls back to the internal name.
        if let Some(display) = &spec.display_name {
            let mut set = self.cmd();
            set.arg("set-option")
                .arg("-t")
                .arg(&spec.name)
                .arg("@bosun_display")
                .arg(display);
            if let Err(e) = set.output().await {
                tracing::warn!("set @bosun_display on {}: {}", spec.name, e);
            }
        }

        // Step 3: type the agent command via send-keys so it runs
        // inside the user's shell with their full environment set up.
        //
        // We match agent-deck's idiom here:
        //   * `send-keys -l -- <cmd>` for the literal characters, so
        //     tmux doesn't interpret things like `C-c` or `Space` in
        //     the command as key-name shortcuts.
        //   * A brief sleep (100ms) so tmux's bracketed-paste handler
        //     finishes processing the literal chunk before Enter lands.
        //   * A separate `send-keys Enter` to submit. Sending Enter in
        //     the same call as `-l` would make it a literal "Enter"
        //     string instead of a newline.
        if !spec.command.is_empty() {
            let mut literal = self.cmd();
            literal
                .arg("send-keys")
                .arg("-l")
                .arg("-t")
                .arg(&spec.name)
                .arg("--")
                .arg(&spec.command);
            if let Err(e) = literal.output().await {
                tracing::warn!("send-keys -l to {}: {}", spec.name, e);
            }

            tokio::time::sleep(std::time::Duration::from_millis(100)).await;

            let mut enter = self.cmd();
            enter
                .arg("send-keys")
                .arg("-t")
                .arg(&spec.name)
                .arg("Enter");
            if let Err(e) = enter.output().await {
                tracing::warn!("send-keys Enter to {}: {}", spec.name, e);
            }
        }

        Ok(spec.name.clone())
    }

    async fn kill_session(&self, session: &str) -> Result<()> {
        let mut cmd = self.cmd();
        cmd.arg("kill-session").arg("-t").arg(session);
        let output = cmd.output().await.map_err(BosunError::Io)?;
        if output.status.success() {
            return Ok(());
        }
        // If the session is already gone, treat as idempotent success.
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("can't find session") || stderr.contains("no server running") {
            return Ok(());
        }
        Err(BosunError::Tmux(format!(
            "kill-session {} failed: {}",
            session,
            stderr.trim()
        )))
    }

    async fn set_display_name(&self, session: &str, display: &str) -> Result<()> {
        let mut cmd = self.cmd();
        cmd.arg("set-option")
            .arg("-t")
            .arg(session)
            .arg("@bosun_display")
            .arg(display);
        let output = cmd.output().await.map_err(BosunError::Io)?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(BosunError::Tmux(format!(
            "set @bosun_display on {}: {}",
            session,
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
