//! Runtime configuration. Phase 1-2 uses env vars for all knobs — a
//! proper TOML config file lands in Phase 4. Everything is read once
//! at startup and passed around by value, so the rest of the code
//! never touches `std::env`.

use std::env;

/// The default prefix that bosun considers "managed". Only tmux
/// sessions whose name starts with this prefix appear in bosun's UI
/// and get the bosun status bar applied. Set `BOSUN_PREFIX=` (empty)
/// to see every session on the server.
pub const DEFAULT_SESSION_PREFIX: &str = "bosun-";

#[derive(Debug, Clone)]
pub struct Config {
    /// Only sessions whose name starts with this prefix are shown in
    /// bosun's UI and get the bosun status bar applied. Empty string
    /// means "show everything".
    pub session_prefix: String,
}

impl Config {
    pub fn from_env() -> Self {
        let session_prefix =
            env::var("BOSUN_PREFIX").unwrap_or_else(|_| DEFAULT_SESSION_PREFIX.to_string());
        Self { session_prefix }
    }

    /// Does `name` pass the managed-session filter?
    pub fn manages(&self, name: &str) -> bool {
        self.session_prefix.is_empty() || name.starts_with(&self.session_prefix)
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            session_prefix: DEFAULT_SESSION_PREFIX.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_prefix_matches_bosun_sessions() {
        let cfg = Config::default();
        assert!(cfg.manages("bosun-work"));
        assert!(cfg.manages("bosun-"));
        assert!(!cfg.manages("agentdeck-work"));
        assert!(!cfg.manages("main"));
    }

    #[test]
    fn empty_prefix_matches_everything() {
        let cfg = Config {
            session_prefix: String::new(),
        };
        assert!(cfg.manages("anything"));
        assert!(cfg.manages(""));
    }

    #[test]
    fn custom_prefix_matches_its_namespace() {
        let cfg = Config {
            session_prefix: "work-".to_string(),
        };
        assert!(cfg.manages("work-api"));
        assert!(!cfg.manages("bosun-api"));
    }
}
