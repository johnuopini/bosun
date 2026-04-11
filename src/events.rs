use std::time::Instant;

use crossterm::event::KeyEvent;

use crate::tmux::session::SessionView;

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
    /// preview buffer per entry.
    SessionsRefreshed(Vec<SessionView>),
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
