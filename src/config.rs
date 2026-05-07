//! Runtime configuration. Values are read once at startup and passed
//! around by value, so the rest of the code never touches `std::env`
//! or the config file on disk.
//!
//! Sources, in order of precedence (lowest to highest):
//!   1. Built-in defaults.
//!   2. `$XDG_CONFIG_HOME/bosun/config.toml` (`~/Library/Application
//!      Support/dev.yetidevworks.bosun/config.toml` on macOS).
//!   3. Environment variables (`BOSUN_PREFIX`, `BOSUN_TMUX_SOCKET`,
//!      `BOSUN_THEME`).
//!
//! Env vars always win. A missing or malformed config file is
//! non-fatal — we log a warning and fall through to defaults.

use std::env;
use std::path::PathBuf;

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::sidebar::{Section, SidebarModel};

/// Legacy Vec<SidebarEntry> shape from v0.2.8. Read-only, used for
/// one-time migration to the new explicit-membership `SidebarModel`.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum LegacySidebarEntry {
    Section { id: String, name: String },
    Session { internal: String },
}

fn migrate_legacy_sidebar(old: Vec<LegacySidebarEntry>) -> SidebarModel {
    // Pre-0.2.9 membership was implicit (session belongs to the
    // nearest section above). The new model is explicit — creating a
    // section claims no one. Safest upgrade: put every session in
    // ungrouped and preserve section headers as empty. The user can
    // then populate them with Shift-Right.
    let mut model = SidebarModel::default();
    for e in old {
        match e {
            LegacySidebarEntry::Section { id, name } => {
                model.sections.push(Section {
                    id,
                    name,
                    members: Vec::new(),
                    collapsed: false,
                    banner_font: None,
                });
            }
            LegacySidebarEntry::Session { internal } => {
                model.ungrouped.push(internal);
            }
        }
    }
    model
}

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

/// Default theme name — must match a built-in in `ui::theme`.
pub const DEFAULT_THEME: &str = "opencode";

#[derive(Debug, Clone)]
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
    /// Theme name. Resolved against user themes first, then
    /// built-ins (`opencode`, `tokyonight`), then a hard-fallback.
    pub theme: String,
    /// Persisted divider position (absolute terminal column). `None`
    /// means use the default 38% split.
    pub divider_x: Option<u16>,
    /// User-defined sidebar ordering with explicit section
    /// membership. Empty on first launch. Persisted as the
    /// `[sidebar]` table in `config.toml` so ordering and group
    /// structure survive bosun restarts.
    pub sidebar: SidebarModel,
    /// Map from session display name → last-known section name.
    /// When a session is killed/restarted or a recent is opened,
    /// the new session is placed back into that section if one with
    /// the same name still exists. Persisted as `[session_history]`.
    pub session_history: std::collections::HashMap<String, String>,
    /// Global TDF banner font used for the section/empty preview
    /// banner. Per-section overrides live on `Section.banner_font`.
    /// Persisted in `config.toml` as `banner_font = "metalix"`.
    pub banner_font: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            session_prefix: DEFAULT_SESSION_PREFIX.to_string(),
            tmux_socket: Some(DEFAULT_TMUX_SOCKET.to_string()),
            self_session: None,
            theme: DEFAULT_THEME.to_string(),
            divider_x: None,
            sidebar: SidebarModel::default(),
            session_history: std::collections::HashMap::new(),
            banner_font: crate::ui::banner::default_name().to_string(),
        }
    }
}

/// Shape of `config.toml` on disk. All fields are optional and
/// defaulted independently so a half-written file still loads.
/// `Serialize` is used by `write_theme` to round-trip the file on
/// disk: read → update one field → write.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct ConfigFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tmux_socket: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    theme: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    divider_x: Option<u16>,
    /// Explicit-membership sidebar (v0.2.9+). Tables + arrays.
    /// `read_config_file` preprocesses the raw TOML so a legacy v0.2.8
    /// `sidebar = [...]` array is migrated in-place to the new shape
    /// before this field is deserialized.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sidebar: Option<SidebarModel>,
    /// Last-known section name per display name. Used to restore
    /// group membership across kill/restart/recents flows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_history: Option<std::collections::HashMap<String, String>>,
    /// Legacy pre-0.2.8 flat list of internal names. Read-only for
    /// migration; never written back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_order: Option<Vec<String>>,
    /// Global TDF banner font name (e.g. `"metalix"`). When absent,
    /// `Config::load` falls back to `banner::default_name()`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    banner_font: Option<String>,
}

impl Config {
    /// Full load path: defaults → config.toml → env vars. See the
    /// module-level doc comment for the precedence order.
    pub fn load() -> Self {
        let file = read_config_file().unwrap_or_default();

        let session_prefix = env::var("BOSUN_PREFIX")
            .ok()
            .or(file.session_prefix)
            .unwrap_or_else(|| DEFAULT_SESSION_PREFIX.to_string());

        let tmux_socket = match env::var("BOSUN_TMUX_SOCKET") {
            Ok(s) if s.is_empty() || s == "default" => None,
            Ok(s) => Some(s),
            Err(_) => match file.tmux_socket.as_deref() {
                Some("") | Some("default") => None,
                Some(s) => Some(s.to_string()),
                None => Some(DEFAULT_TMUX_SOCKET.to_string()),
            },
        };

        let theme = env::var("BOSUN_THEME")
            .ok()
            .or(file.theme)
            .unwrap_or_else(|| DEFAULT_THEME.to_string());

        // Only detect self-session if we're on the same socket as
        // the caller's tmux. If bosun uses a dedicated socket, the
        // parent tmux (if any) is on a different server and bosun
        // isn't "inside" any session on its own socket.
        let self_session = if tmux_socket.is_none() {
            detect_self_session()
        } else {
            None
        };

        let divider_x = file.divider_x;
        // Prefer the new `sidebar` field (explicit-membership model).
        // If absent, migrate the pre-0.2.8 flat `session_order` list.
        // Legacy v0.2.8 `sidebar = [...]` arrays are migrated by
        // `read_config_file` before we get here.
        let sidebar = match file.sidebar {
            Some(s) => s,
            None => {
                let ungrouped = file.session_order.unwrap_or_default();
                SidebarModel {
                    ungrouped,
                    sections: Vec::new(),
                }
            }
        };

        let session_history = file.session_history.unwrap_or_default();
        let banner_font = file
            .banner_font
            .unwrap_or_else(|| crate::ui::banner::default_name().to_string());

        Self {
            session_prefix,
            tmux_socket,
            self_session,
            theme,
            divider_x,
            sidebar,
            session_history,
            banner_font,
        }
    }

    /// Back-compat shim for callers that only want env-driven config.
    /// Retained so tests and a few internal paths don't need to
    /// touch the filesystem.
    pub fn from_env() -> Self {
        Self::load()
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

/// Location of bosun's config directory. Same `ProjectDirs` entry
/// the SQLite store uses, so `config.toml` lives alongside
/// `bosun.db` on macOS.
pub fn config_dir() -> Option<PathBuf> {
    ProjectDirs::from("dev", "yetidevworks", "bosun").map(|d| d.config_dir().to_path_buf())
}

/// Where user-defined themes live — one `.toml` per theme, file name
/// without extension is the theme name.
pub fn user_themes_dir() -> Option<PathBuf> {
    config_dir().map(|d| d.join("themes"))
}

fn read_config_file() -> Option<ConfigFile> {
    let path = config_dir()?.join("config.toml");
    let s = std::fs::read_to_string(&path).ok()?;

    // Parse as a generic Value first so we can detect + migrate the
    // legacy v0.2.8 `sidebar = [...]` array shape. The v0.2.9 shape
    // is a `[sidebar]` table, so a sidebar Value that's an Array is
    // unambiguously the old form.
    let mut value: toml::Value = match toml::from_str(&s) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("failed to parse {:?}: {}", path, e);
            return None;
        }
    };

    if let Some(table) = value.as_table_mut() {
        if let Some(sidebar) = table.get("sidebar") {
            if sidebar.is_array() {
                let cloned = sidebar.clone();
                match cloned.try_into::<Vec<LegacySidebarEntry>>() {
                    Ok(legacy) => {
                        let migrated = migrate_legacy_sidebar(legacy);
                        match toml::Value::try_from(&migrated) {
                            Ok(v) => {
                                table.insert("sidebar".to_string(), v);
                            }
                            Err(e) => {
                                tracing::warn!("failed to serialize migrated sidebar: {}", e);
                                table.remove("sidebar");
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("failed to parse legacy sidebar: {}", e);
                        table.remove("sidebar");
                    }
                }
            }
        }
    }

    match value.try_into::<ConfigFile>() {
        Ok(f) => Some(f),
        Err(e) => {
            tracing::warn!("failed to deserialize {:?}: {}", path, e);
            None
        }
    }
}

/// Persist a new theme choice to `config.toml`. Read-modify-write:
/// existing file fields are preserved, only `theme` is updated. If
/// the file doesn't exist it's created. Returns `Err` if the config
/// dir can't be resolved or writing fails — callers (the theme
/// picker) surface this as a warning in the status bar but still
/// apply the change to the live UI.
pub fn write_theme(name: &str) -> std::io::Result<()> {
    let dir =
        config_dir().ok_or_else(|| std::io::Error::other("cannot resolve bosun config dir"))?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("config.toml");

    let mut file = match std::fs::read_to_string(&path) {
        Ok(s) => toml::from_str::<ConfigFile>(&s).unwrap_or_default(),
        Err(_) => ConfigFile::default(),
    };
    file.theme = Some(name.to_string());

    let body = toml::to_string(&file)
        .map_err(|e| std::io::Error::other(format!("toml serialize: {e}")))?;

    // Atomic write: temp file + rename. Avoids a half-written
    // config.toml if bosun is killed mid-write.
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Persist the global banner font to `config.toml`. Same
/// read-modify-write approach as `write_theme`.
pub fn write_banner_font(name: &str) -> std::io::Result<()> {
    let dir =
        config_dir().ok_or_else(|| std::io::Error::other("cannot resolve bosun config dir"))?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("config.toml");

    let mut file = match std::fs::read_to_string(&path) {
        Ok(s) => toml::from_str::<ConfigFile>(&s).unwrap_or_default(),
        Err(_) => ConfigFile::default(),
    };
    file.banner_font = Some(name.to_string());

    let body = toml::to_string(&file)
        .map_err(|e| std::io::Error::other(format!("toml serialize: {e}")))?;

    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Persist the divider position to `config.toml`. Same
/// read-modify-write approach as `write_theme`.
pub fn write_divider_x(x: Option<u16>) -> std::io::Result<()> {
    let dir =
        config_dir().ok_or_else(|| std::io::Error::other("cannot resolve bosun config dir"))?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("config.toml");

    let mut file = match std::fs::read_to_string(&path) {
        Ok(s) => toml::from_str::<ConfigFile>(&s).unwrap_or_default(),
        Err(_) => ConfigFile::default(),
    };
    file.divider_x = x;

    let body = toml::to_string(&file)
        .map_err(|e| std::io::Error::other(format!("toml serialize: {e}")))?;

    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Persist the session-history map (display_name → section_name) to
/// `config.toml`. An empty map clears the field.
pub fn write_session_history(
    history: &std::collections::HashMap<String, String>,
) -> std::io::Result<()> {
    let dir =
        config_dir().ok_or_else(|| std::io::Error::other("cannot resolve bosun config dir"))?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("config.toml");

    let mut file = read_config_file().unwrap_or_default();
    file.session_history = if history.is_empty() {
        None
    } else {
        Some(history.clone())
    };

    let body = toml::to_string(&file)
        .map_err(|e| std::io::Error::other(format!("toml serialize: {e}")))?;

    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Persist the user-defined sidebar (explicit-membership model) to
/// `config.toml`. Same read-modify-write approach as `write_theme`.
/// An empty model clears the field from the file. Also drops the
/// legacy `session_order` so the config converges on the new shape.
pub fn write_sidebar(model: &SidebarModel) -> std::io::Result<()> {
    let dir =
        config_dir().ok_or_else(|| std::io::Error::other("cannot resolve bosun config dir"))?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("config.toml");

    // Read via the migrating reader so a pre-existing legacy sidebar
    // is converted before we overwrite it. Otherwise the first save
    // after a legacy read-through would wipe the user's groups.
    let mut file = read_config_file().unwrap_or_default();
    file.sidebar = if model.ungrouped.is_empty() && model.sections.is_empty() {
        None
    } else {
        Some(model.clone())
    };
    file.session_order = None;

    let body = toml::to_string(&file)
        .map_err(|e| std::io::Error::other(format!("toml serialize: {e}")))?;

    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
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
            theme: DEFAULT_THEME.to_string(),
            divider_x: None,
            sidebar: SidebarModel::default(),
            session_history: std::collections::HashMap::new(),
            banner_font: crate::ui::banner::default_name().to_string(),
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
            theme: DEFAULT_THEME.to_string(),
            divider_x: None,
            sidebar: SidebarModel::default(),
            session_history: std::collections::HashMap::new(),
            banner_font: crate::ui::banner::default_name().to_string(),
        };
        assert!(!c.manages("bosun-mine-abc"));
        assert!(c.manages("bosun-other-xyz"));
    }

    #[test]
    fn config_file_fields_parse() {
        let src = r#"
            session_prefix = "work-"
            tmux_socket = "scratch"
            theme = "tokyonight"
        "#;
        let parsed: ConfigFile = toml::from_str(src).unwrap();
        assert_eq!(parsed.session_prefix.as_deref(), Some("work-"));
        assert_eq!(parsed.tmux_socket.as_deref(), Some("scratch"));
        assert_eq!(parsed.theme.as_deref(), Some("tokyonight"));
    }

    #[test]
    fn empty_config_file_is_all_defaults() {
        let parsed: ConfigFile = toml::from_str("").unwrap();
        assert!(parsed.session_prefix.is_none());
        assert!(parsed.tmux_socket.is_none());
        assert!(parsed.theme.is_none());
    }
}
