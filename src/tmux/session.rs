use std::time::SystemTime;

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
    pub fn display(&self) -> &str {
        &self.name
    }
}
