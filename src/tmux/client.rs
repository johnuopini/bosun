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
#[derive(Debug, Clone, Default)]
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
    /// Full session spec (agent, args, options) to persist as
    /// per-session `@bosun_*` tmux user options. Used by restart to
    /// recover the original spec. `None` skips persistence (useful
    /// for tests and for callers that don't care about restart).
    pub metadata: Option<SessionMetadata>,
}

/// The subset of `SessionSpec` that bosun persists as tmux user
/// options on each managed session so that `RestartSession` can
/// rebuild the spec without an external store.
#[derive(Debug, Clone, Default)]
pub struct SessionMetadata {
    pub display_name: String,
    pub path: String,
    pub agent: String,
    pub args: String,
    pub claude_session_mode: String,
    pub claude_skip_permissions: bool,
    pub codex_yolo: bool,
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

    /// Read bosun's persisted `@bosun_*` metadata off a session, or
    /// `Ok(None)` if the session has no agent set (pre-dates the
    /// feature or wasn't created by bosun). Used by restart to
    /// rebuild the original spec.
    async fn get_session_metadata(&self, session: &str) -> Result<Option<SessionMetadata>>;

    /// Overwrite the `@bosun_*` metadata user options on a live
    /// session. Used by the modify-session modal to update the
    /// stored spec without recreating the session. The next
    /// `RestartSession` will read these back via
    /// `get_session_metadata` and spawn the agent with the new
    /// flags.
    async fn set_session_metadata(&self, session: &str, metadata: &SessionMetadata) -> Result<()>;

    /// Restart the agent inside a live session without killing the
    /// session itself. Sends Ctrl-C twice (covers agents that swallow
    /// the first interrupt to confirm), then types the new launch
    /// command and Enter. The pane stays alive (the shell keeps
    /// running underneath), the session's internal name doesn't
    /// change, and bosun's sidebar position is preserved with zero
    /// model churn.
    async fn restart_in_place(&self, session: &str, command: &str) -> Result<()>;
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

    /// Read the basename of the pane's foreground process, e.g. `zsh`
    /// while sitting at a prompt or `node` / `claude` / `codex` /
    /// `python3` while an agent is running. Returns an empty string if
    /// the session has gone away. Used by `restart_in_place` to poll
    /// for "agent has died" and "agent has started" without relying on
    /// fixed-duration sleeps.
    async fn pane_current_command(&self, session: &str) -> String {
        let mut cmd = self.cmd();
        cmd.arg("display-message")
            .arg("-p")
            .arg("-t")
            .arg(session)
            .arg("#{pane_current_command}");
        match cmd.output().await {
            Ok(out) if out.status.success() => {
                String::from_utf8_lossy(&out.stdout).trim().to_string()
            }
            _ => String::new(),
        }
    }
}

/// Heuristic: is this pane's foreground process a shell prompt, i.e.
/// safe to type a launch command into? Matches the common login
/// shells. False negatives just mean `restart_in_place` waits a bit
/// longer for shell detection before falling through on timeout.
fn is_shell(cmd: &str) -> bool {
    matches!(
        cmd,
        "zsh" | "bash" | "fish" | "sh" | "dash" | "ksh" | "tcsh" | "csh" | "nu" | "pwsh"
    )
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

        // Step 2b: persist the full session metadata as @bosun_*
        // user options so RestartSession can recover the spec later.
        // Best-effort; failures just mean restart won't work for
        // this session.
        if let Some(meta) = &spec.metadata {
            for (key, value) in metadata_options(meta) {
                let mut set = self.cmd();
                set.arg("set-option")
                    .arg("-t")
                    .arg(&spec.name)
                    .arg(key)
                    .arg(&value);
                if let Err(e) = set.output().await {
                    tracing::warn!("set {} on {}: {}", key, spec.name, e);
                }
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

    async fn get_session_metadata(&self, session: &str) -> Result<Option<SessionMetadata>> {
        // Single display-message call returns all 7 fields separated
        // by `|||`. We can't use a control character (the old `\x1f`
        // unit separator) because tmux 3.4+ escapes control chars in
        // format output as octal sequences (`\037`), which the parser
        // would never see as a real separator — that was breaking the
        // Ubuntu CI lifecycle integration test. `|||` is printable so
        // tmux passes it through untouched. See the matching fix in
        // `tmux::parse::LIST_SESSIONS_FORMAT`.
        const SEP: &str = "|||";
        let fmt = format!(
            "#{{@bosun_display}}{SEP}#{{@bosun_path}}{SEP}#{{@bosun_agent}}{SEP}#{{@bosun_args}}{SEP}#{{@bosun_claude_session_mode}}{SEP}#{{@bosun_claude_skip_permissions}}{SEP}#{{@bosun_codex_yolo}}",
            SEP = SEP
        );
        let mut cmd = self.cmd();
        cmd.arg("display-message")
            .arg("-p")
            .arg("-t")
            .arg(session)
            .arg(&fmt);
        let output = cmd.output().await.map_err(BosunError::Io)?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(BosunError::Tmux(format!(
                "display-message on {}: {}",
                session,
                stderr.trim()
            )));
        }

        let raw = String::from_utf8_lossy(&output.stdout);
        let line = raw.trim_end_matches('\n');
        let parts: Vec<&str> = line.split(SEP).collect();
        if parts.len() != 7 {
            return Ok(None);
        }
        // Agent is the required anchor — if it's empty, this session
        // wasn't created by a metadata-aware bosun.
        if parts[2].is_empty() {
            return Ok(None);
        }
        Ok(Some(SessionMetadata {
            display_name: parts[0].to_string(),
            path: parts[1].to_string(),
            agent: parts[2].to_string(),
            args: parts[3].to_string(),
            claude_session_mode: if parts[4].is_empty() {
                "New".to_string()
            } else {
                parts[4].to_string()
            },
            claude_skip_permissions: parts[5] == "1",
            codex_yolo: parts[6] == "1",
        }))
    }

    async fn set_session_metadata(&self, session: &str, metadata: &SessionMetadata) -> Result<()> {
        // Re-uses the same key/value mapping the create path writes
        // on session birth, so a modify produces options
        // byte-identical to what the create path would have
        // produced for the same spec. Errors on the first failed
        // option write so the caller can surface a single message
        // — partial-update state is rare enough (it would mean
        // tmux died mid-call) that we'd rather fail loudly.
        for (key, value) in metadata_options(metadata) {
            let mut cmd = self.cmd();
            cmd.arg("set-option")
                .arg("-t")
                .arg(session)
                .arg(key)
                .arg(&value);
            let output = cmd.output().await.map_err(BosunError::Io)?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(BosunError::Tmux(format!(
                    "set {} on {}: {}",
                    key,
                    session,
                    stderr.trim()
                )));
            }
        }
        Ok(())
    }

    async fn restart_in_place(&self, session: &str, command: &str) -> Result<()> {
        // Strategy: poll `#{pane_current_command}` instead of guessing
        // timings with fixed sleeps. The two questions we need answered
        // are "has the old agent actually exited?" and "has the new
        // agent actually started?" — both are directly observable via
        // tmux's display-message format, so we wait for the actual
        // state transition rather than hoping a sleep was long enough.
        //
        //   1. Send C-c, poll until pane_current_command is a shell.
        //      Re-send C-c periodically while the agent is still up
        //      (claude / codex sometimes swallow the first one to ask
        //      for confirmation, etc.). Bounded by a hard timeout so
        //      we never wedge the actor.
        //   2. Once we observe a shell, prep the line: Enter (forces
        //      any async prompt framework to finish painting), C-u
        //      (wipe residue from the shutdown banner).
        //   3. send-keys -l <command> + Enter to launch the new agent.
        //   4. Poll again until pane_current_command leaves the shell
        //      — i.e. the agent process is actually the foreground
        //      process. Only then send C-l. Sending C-l while still
        //      at the shell (the old behavior's failure mode) just
        //      clears the shell screen, which is exactly the empty
        //      starship prompt we'd see in failed restarts.
        //   5. The C-l forces alt-screen TUIs (claude, codex) to fully
        //      repaint, which capture-pane then picks up cleanly for
        //      the sidebar preview.
        use std::time::Duration;
        use tokio::time::Instant;

        let send_keys = |args: Vec<&str>| {
            let mut c = self.cmd();
            c.arg("send-keys").arg("-t").arg(session);
            for a in args {
                c.arg(a);
            }
            async move {
                if let Err(e) = c.output().await {
                    tracing::warn!("restart_in_place send-keys to {}: {}", session, e);
                }
            }
        };

        // ── Phase 1: kill the running agent, wait for shell ──────────
        send_keys(vec!["C-c"]).await;
        let kill_deadline = Instant::now() + Duration::from_millis(3500);
        let mut next_cc = Instant::now() + Duration::from_millis(250);
        let mut at_shell = false;
        loop {
            tokio::time::sleep(Duration::from_millis(80)).await;
            let cur = self.pane_current_command(session).await;
            if cur.is_empty() {
                // Session went away — nothing to restart.
                return Ok(());
            }
            if is_shell(&cur) {
                at_shell = true;
                break;
            }
            if Instant::now() >= kill_deadline {
                tracing::warn!(
                    "restart_in_place: gave up waiting for shell on {} (still running {})",
                    session,
                    cur
                );
                break;
            }
            if Instant::now() >= next_cc {
                send_keys(vec!["C-c"]).await;
                next_cc = Instant::now() + Duration::from_millis(400);
            }
        }

        // Tiny settle so the shell's line editor is fully primed before
        // we type. Even after pane_current_command flips to "zsh",
        // async prompt frameworks (powerlevel10k, spaceship) may still
        // be painting; ~100ms is enough in practice.
        tokio::time::sleep(Duration::from_millis(120)).await;

        // ── Phase 2: prep the shell line for input ───────────────────
        send_keys(vec!["C-u"]).await;
        tokio::time::sleep(Duration::from_millis(40)).await;
        send_keys(vec!["Enter"]).await;
        tokio::time::sleep(Duration::from_millis(120)).await;
        send_keys(vec!["C-u"]).await;
        tokio::time::sleep(Duration::from_millis(60)).await;

        // Empty command means "leave a clean shell prompt" (terminal
        // agent with no args). Nothing else to do.
        if command.is_empty() {
            return Ok(());
        }

        // ── Phase 3: type and submit the launch command ──────────────
        let mut literal = self.cmd();
        literal
            .arg("send-keys")
            .arg("-l")
            .arg("-t")
            .arg(session)
            .arg("--")
            .arg(command);
        if let Err(e) = literal.output().await {
            tracing::warn!("restart_in_place send-keys -l to {}: {}", session, e);
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
        send_keys(vec!["Enter"]).await;

        // ── Phase 4: wait for the new agent to actually start ────────
        // If we couldn't confirm we were at a shell, don't loop forever
        // — fall through after a single deadline check. Shell-start has
        // its own deadline.
        let start_deadline = Instant::now() + Duration::from_millis(6000);
        let mut agent_up = false;
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let cur = self.pane_current_command(session).await;
            if cur.is_empty() {
                // Session vanished.
                return Ok(());
            }
            if !is_shell(&cur) {
                agent_up = true;
                break;
            }
            if Instant::now() >= start_deadline {
                tracing::warn!(
                    "restart_in_place: agent didn't appear in pane_current_command on {} \
                     (still showing shell {}); skipping C-l",
                    session,
                    cur
                );
                break;
            }
        }
        let _ = at_shell;

        // ── Phase 5: force a redraw inside the new agent ─────────────
        // Only send C-l once we've confirmed the foreground process is
        // no longer a shell — otherwise C-l would clear the shell's
        // screen and capture-pane would snapshot an empty prompt.
        if agent_up {
            // Let the TUI claim the alt-screen before nudging it.
            tokio::time::sleep(Duration::from_millis(250)).await;
            send_keys(vec!["C-l"]).await;
        }

        Ok(())
    }
}

/// Map a `SessionMetadata` into the `(key, value)` pairs that should
/// be written via `set-option -t <session>`.
fn metadata_options(m: &SessionMetadata) -> Vec<(&'static str, String)> {
    vec![
        ("@bosun_path", m.path.clone()),
        ("@bosun_agent", m.agent.clone()),
        ("@bosun_args", m.args.clone()),
        ("@bosun_claude_session_mode", m.claude_session_mode.clone()),
        (
            "@bosun_claude_skip_permissions",
            if m.claude_skip_permissions { "1" } else { "0" }.to_string(),
        ),
        (
            "@bosun_codex_yolo",
            if m.codex_yolo { "1" } else { "0" }.to_string(),
        ),
    ]
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
