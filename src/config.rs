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

/// Default tmux `-L <socket>` that bosun uses. Putting bosun on its
/// own socket means bosun's tmux server is a **child of the bosun
/// process**, which inherits whatever shell context bosun was
/// launched from — critically, including the macOS Keychain lineage
/// that lets Claude Code see its cached credentials. With the default
/// socket, bosun's sessions would live on some ancient server started
/// by some other context and Claude wouldn't see the user's auth.
///
/// Set `BOSUN_TMUX_SOCKET=default` to opt back into the shared
/// default socket (at the cost of the auth issue and of seeing every
/// other tmux session on the machine).
pub const DEFAULT_TMUX_SOCKET: &str = "bosun";

#[derive(Debug, Clone, Default)]
pub struct Config {
    /// Only sessions whose name starts with this prefix are shown in
    /// bosun's UI and get the bosun status bar applied. Empty string
    /// means "show everything".
    pub session_prefix: String,
    /// Tmux `-L` socket name. `None` means use tmux's default socket.
    /// `Some("bosun")` (the default) means `tmux -L bosun ...`.
    pub tmux_socket: Option<String>,
    /// Name of the tmux session bosun is currently running inside,
    /// if any. `None` if bosun was launched outside tmux. We exclude
    /// this session from bosun's own list so the preview doesn't
    /// capture bosun itself (which would create a visual feedback
    /// loop: bosun renders a preview of itself, which shows bosun
    /// rendering a preview of itself, etc).
    pub self_session: Option<String>,
}

impl Config {
    pub fn from_env() -> Self {
        let session_prefix =
            env::var("BOSUN_PREFIX").unwrap_or_else(|_| DEFAULT_SESSION_PREFIX.to_string());
        let tmux_socket = match env::var("BOSUN_TMUX_SOCKET") {
            Ok(s) if s.is_empty() || s == "default" => None,
            Ok(s) => Some(s),
            Err(_) => Some(DEFAULT_TMUX_SOCKET.to_string()),
        };
        // Only detect self-session if we're on the same socket as
        // the caller's tmux. If bosun uses a dedicated socket, the
        // parent tmux (if any) is on a different server and bosun
        // isn't "inside" any session on its own socket.
        let self_session = if tmux_socket.is_none() {
            detect_self_session()
        } else {
            None
        };
        Self {
            session_prefix,
            tmux_socket,
            self_session,
        }
    }

    /// Does `name` pass the managed-session filter?
    pub fn manages(&self, name: &str) -> bool {
        // Never manage the session bosun is running in — that causes
        // the recursive preview feedback loop.
        if self.self_session.as_deref() == Some(name) {
            return false;
        }
        self.session_prefix.is_empty() || name.starts_with(&self.session_prefix)
    }
}

/// If `$TMUX` is set, ask tmux for the current session name. Used to
/// exclude bosun's own session from its list.
fn detect_self_session() -> Option<String> {
    if env::var("TMUX").is_err() {
        return None;
    }
    let out = std::process::Command::new("tmux")
        .args(["display-message", "-p", "#{session_name}"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(prefix: &str) -> Config {
        Config {
            session_prefix: prefix.to_string(),
            tmux_socket: Some(DEFAULT_TMUX_SOCKET.to_string()),
            self_session: None,
        }
    }

    #[test]
    fn default_prefix_matches_bosun_sessions() {
        let c = cfg(DEFAULT_SESSION_PREFIX);
        assert!(c.manages("bosun-work"));
        assert!(c.manages("bosun-"));
        assert!(!c.manages("agentdeck-work"));
        assert!(!c.manages("main"));
    }

    #[test]
    fn empty_prefix_matches_everything() {
        let c = cfg("");
        assert!(c.manages("anything"));
        assert!(c.manages(""));
    }

    #[test]
    fn custom_prefix_matches_its_namespace() {
        let c = cfg("work-");
        assert!(c.manages("work-api"));
        assert!(!c.manages("bosun-api"));
    }

    #[test]
    fn self_session_is_excluded_even_when_prefix_matches() {
        let c = Config {
            session_prefix: DEFAULT_SESSION_PREFIX.to_string(),
            tmux_socket: None,
            self_session: Some("bosun-mine-abc".to_string()),
        };
        assert!(!c.manages("bosun-mine-abc"));
        assert!(c.manages("bosun-other-xyz"));
    }
}
