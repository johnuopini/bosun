use crossterm::event::{KeyEvent, MouseEvent};

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
    /// Kill and recreate a session using the metadata we persisted
    /// as `@bosun_*` tmux user options when it was first created.
    /// The new session gets a fresh internal name (new hex suffix)
    /// but keeps the same display name, path, agent and options.
    RestartSession(String),
    /// Delete a single recent entry from the SQLite store by its
    /// primary key. The RecentsModal emits this when the user hits
    /// `d` on a highlighted row.
    DeleteRecent(i64),
    /// Persist the divider position to config.toml. Intercepted by
    /// the app loop — never forwarded to the tmux actor.
    SaveDivider(Option<u16>),
    /// Persist the user-defined sidebar (sections + session order) to
    /// config.toml. Intercepted by the app loop — never forwarded to
    /// the tmux actor.
    SaveSidebar(crate::sidebar::SidebarModel),
    /// Persist the display_name → section_name history to config.toml.
    /// Intercepted by the app loop — never forwarded to the tmux actor.
    SaveSessionHistory(std::collections::HashMap<String, String>),
    /// Persist the global TDF banner font to config.toml. Intercepted
    /// by the app loop — never forwarded to the tmux actor.
    SaveBannerFont(String),
    /// Insert a new section header above the cursor with the given name.
    /// Intercepted by the app loop — never forwarded to the tmux actor.
    InsertSection { name: String },
    /// Rename a section header by its id.
    /// Intercepted by the app loop — never forwarded to the tmux actor.
    RenameSection { id: String, new_name: String },
    /// Set the active theme. Intercepted by the app loop — this
    /// command is NEVER forwarded to the tmux actor (it's a pure UI
    /// state change). `persist=false` is a transient live preview
    /// (sent by the theme picker on every arrow key); `persist=true`
    /// also writes `theme = "<name>"` to `config.toml`. On cancel,
    /// the picker emits `SetTheme { original, persist: false }` to
    /// revert the UI without touching disk.
    SetTheme { name: String, persist: bool },
    /// Spawn the configured external editor (`bosun editor <cmd>` /
    /// `editor = "..."` in config.toml) against the highlighted
    /// session's path. Intercepted by the app loop — runs
    /// `<editor> <path>` detached so the TUI keeps the foreground.
    /// The reducer pre-resolves both fields so the loop just calls
    /// `Command::new(...).spawn()`.
    OpenEditor { editor: String, path: String },
    /// Graceful shutdown signal.
    #[allow(dead_code)]
    Shutdown,
}

/// Messages flow from actors (input, tmux) back to the app task.
/// The app task is the single writer of `AppState`.
///
/// There's no periodic `Tick` variant any more. As of the tmux -C
/// rewrite, session-list refreshes are push-driven by control-mode
/// notifications from the `tmux_actor`'s monitor subprocess, not by
/// a 1Hz poller. Main no longer generates `ListNow` commands from a
/// timer — it just waits for `SessionsRefreshed` to arrive.
#[derive(Debug)]
pub enum AppMsg {
    /// A key from the terminal.
    Key(KeyEvent),
    /// A mouse event from the terminal. Used by the draggable divider
    /// between the session list and preview. Dropped by non-mouse
    /// consumers.
    Mouse(MouseEvent),
    /// Terminal was resized.
    Resize(u16, u16),
    /// Fresh session list from tmux, with smoothed status and optional
    /// preview buffer per entry. `select_after` carries an internal
    /// session name that the app should jump the selection to — set
    /// when this refresh is the result of a create (so the new session
    /// auto-highlights). `None` for notification-driven refreshes where
    /// the app preserves its existing selection.
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
