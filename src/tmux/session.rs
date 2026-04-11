use std::sync::Arc;
use std::time::SystemTime;

use crate::tmux::detector::Status;

/// A tmux session as observed by Bosun. This is NOT our authoritative
/// source of truth for persistence — tmux itself is. We rebuild these
/// structs on every poll from `tmux list-sessions -F ...`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxSession {
    pub name: String,
    pub windows: u32,
    pub attached: bool,
    pub created: Option<SystemTime>,
    pub last_activity: Option<SystemTime>,
    pub current_path: Option<String>,
}

impl TmuxSession {
    /// Convenience: stable display title. For Phase 1 this is just the name.
    /// Phase 3 will overlay metadata from the store.
    #[allow(dead_code)]
    pub fn display(&self) -> &str {
        &self.name
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

    pub fn name(&self) -> &str {
        &self.session.name
    }
}
