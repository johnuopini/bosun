use std::sync::Arc;
use std::time::SystemTime;

use crate::tmux::detector::Status;

/// A tmux session as observed by Bosun. This is NOT our authoritative
/// source of truth for persistence — tmux itself is. We rebuild these
/// structs on every poll from `tmux list-sessions -F ...`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxSession {
    /// Actual tmux session name — includes prefix + uniq suffix for
    /// bosun-managed sessions, e.g. `bosun-rasterfox-a1b2c3d4`. This
    /// is what we pass to `tmux attach-session -t`, `capture-pane -t`,
    /// etc.
    pub name: String,
    /// Pretty name shown in bosun's UI. Populated from the tmux user
    /// option `@bosun_display` that we set at create time. `None` for
    /// sessions bosun didn't create (or ones set by an older bosun).
    pub display_name: Option<String>,
    pub windows: u32,
    pub attached: bool,
    pub created: Option<SystemTime>,
    pub last_activity: Option<SystemTime>,
    pub current_path: Option<String>,
}

impl TmuxSession {
    /// Pretty name for the UI. Falls back to the internal session name
    /// if no display name was set.
    pub fn display(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.name)
    }
}

/// The session as the UI wants to see it: the raw tmux view plus the
/// smoothed status and (optionally) the latest pane capture for preview.
/// The actor produces these and hands them to the app.
#[derive(Debug, Clone)]
pub struct SessionView {
    pub session: TmuxSession,
    pub status: Status,
    /// Raw ANSI capture of the session's pane. `Arc<[u8]>` so the UI
    /// can render without cloning the whole buffer on every frame.
    /// `None` for sessions we skipped capturing on this tick.
    pub preview: Option<Arc<[u8]>>,
}

impl SessionView {
    pub fn new(session: TmuxSession, status: Status, preview: Option<Arc<[u8]>>) -> Self {
        Self {
            session,
            status,
            preview,
        }
    }

    /// The internal tmux name — use this when talking to tmux.
    pub fn name(&self) -> &str {
        &self.session.name
    }

    /// The pretty display name — use this in the UI.
    pub fn display(&self) -> &str {
        self.session.display()
    }
}
