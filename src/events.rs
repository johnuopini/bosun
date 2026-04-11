use std::time::Instant;

use crossterm::event::KeyEvent;

use crate::tmux::session::SessionView;

/// Data the new-session modal gathers and hands off to the tmux actor.
/// The `name` is the unprefixed user-entered name; the actor prepends
/// `Config::session_prefix` (e.g. `bosun-`) before calling tmux.
#[derive(Debug, Clone)]
pub struct SessionSpec {
    pub name: String,
    pub path: String,
    pub agent: String,
    pub args: String,
    pub options: SpecOptions,
}

/// Agent-specific flags the user toggled in the new-session modal.
/// The actor's `build_agent_command` reads these and produces the
/// right CLI flags when spawning the agent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpecOptions {
    pub claude: ClaudeOptions,
    pub codex: CodexOptions,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaudeOptions {
    pub session_mode: ClaudeSessionMode,
    pub skip_permissions: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ClaudeSessionMode {
    #[default]
    New,
    Continue,
    Resume,
}

impl ClaudeSessionMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::New => "New",
            Self::Continue => "Continue",
            Self::Resume => "Resume",
        }
    }
    pub fn next(self) -> Self {
        match self {
            Self::New => Self::Continue,
            Self::Continue => Self::Resume,
            Self::Resume => Self::New,
        }
    }
    pub fn prev(self) -> Self {
        match self {
            Self::New => Self::Resume,
            Self::Continue => Self::New,
            Self::Resume => Self::Continue,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CodexOptions {
    /// Codex `--yolo` — bypass approvals and sandbox. Dangerous.
    pub yolo: bool,
}

/// Commands flow from the UI/app task into the tmux actor.
#[derive(Debug)]
pub enum Command {
    /// Refresh the session list immediately (out of schedule).
    ListNow,
    /// Attach to the selected session. The actor takes care of
    /// installing the Ctrl-Q binding before attach and removing it after.
    #[allow(dead_code)]
    Attach { name: String },
    /// The user selected a different session; the actor should capture
    /// its pane with priority so the preview updates quickly. The name
    /// is owned so the command can cross the mpsc boundary.
    FocusPreview { name: String },
    /// Create a new tmux session from the new-session modal's form data.
    CreateSession(SessionSpec),
    /// Kill a session by its internal tmux name. `tmux kill-session -t`.
    KillSession(String),
    /// Rename the pretty display name of a session. The internal tmux
    /// name never changes (we only update the `@bosun_display` user
    /// option); the UI picks up the new label on the next refresh.
    RenameSession {
        internal: String,
        new_display: String,
    },
    /// Graceful shutdown signal.
    #[allow(dead_code)]
    Shutdown,
}

/// Messages flow from actors (input, tmux) back to the app task.
/// The app task is the single writer of `AppState`.
#[derive(Debug)]
pub enum AppMsg {
    /// A periodic tick from the poller.
    Tick(Instant),
    /// A key from the terminal.
    Key(KeyEvent),
    /// Terminal was resized.
    Resize(u16, u16),
    /// Fresh session list from tmux, with smoothed status and optional
    /// preview buffer per entry. `select_after` carries an internal
    /// session name that the app should jump the selection to — set
    /// when this refresh is the result of a create (so the new session
    /// auto-highlights). `None` for regular tick-driven refreshes,
    /// where the app preserves its existing selection.
    SessionsRefreshed {
        sessions: Vec<SessionView>,
        select_after: Option<String>,
    },
    /// An attach just started — the UI should render a placeholder
    /// while we block in `tmux attach`.
    AttachStarted { name: String },
    /// The attach returned (user detached).
    AttachEnded { name: String },
    /// A non-fatal error to surface in the status bar.
    Warn(String),
    /// A fatal error — bail out of the event loop.
    Fatal(String),
    /// Explicit shutdown request (Ctrl-C, SIGTERM).
    Shutdown,
    /// SIGCONT — we came back from Ctrl-Z suspend, re-enter raw mode.
    Resume,
}
