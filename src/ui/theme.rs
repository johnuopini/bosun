//! Theme system. A `Theme` is a set of semantic color slots
//! (backgrounds, text, accents, status) that every render site reads
//! from instead of hardcoding `Color::Rgb`. Themes are TOML files —
//! two ship built in (`opencode`, `tokyonight`), and users can drop
//! extra `.toml` files into `$XDG_CONFIG_HOME/bosun/themes/` which
//! take priority over the built-ins if the name matches.
//!
//! Wire format: TOML with hex strings like `"#7c5cff"`. A small
//! `hex_color` serde module parses them into `ratatui::style::Color`.

use std::path::Path;

use ratatui::style::Color;
use serde::Deserialize;

/// Semantic color slots. Every render site in bosun reads from one of
/// these slots — if you find yourself reaching for `Color::Rgb` in a
/// render function, add a slot here instead.
#[derive(Debug, Clone, Deserialize)]
pub struct Theme {
    pub name: String,

    /// Deepest background. Used for the session list panel, form
    /// fields, and the dim wash behind modals.
    #[serde(with = "hex_color")]
    pub bg: Color,
    /// Slightly lighter panel color for unselected session rows.
    #[serde(with = "hex_color")]
    pub panel: Color,
    /// Secondary panel color for the bottom status bar and modal
    /// bodies — one step up from `panel`.
    #[serde(with = "hex_color")]
    pub panel_alt: Color,
    /// Row / field highlight — applied to the selected session row
    /// and to focused form fields.
    #[serde(with = "hex_color")]
    pub selection_bg: Color,

    /// Primary foreground color.
    #[serde(with = "hex_color")]
    pub text: Color,
    /// Dim foreground for hints, muted labels, window counts.
    #[serde(with = "hex_color")]
    pub text_muted: Color,

    /// Primary accent (bosun purple in the opencode theme). Used for
    /// the "bosun" badge, modal left accent bar, and selection marker.
    #[serde(with = "hex_color")]
    pub accent: Color,
    /// Drop-shadow color behind modals.
    #[serde(with = "hex_color")]
    pub shadow: Color,
    /// Foreground color used when dimming the main UI behind a modal.
    #[serde(with = "hex_color")]
    pub dim_fg: Color,

    /// Session status: running / busy.
    #[serde(with = "hex_color")]
    pub status_running: Color,
    /// Session status: waiting for user input.
    #[serde(with = "hex_color")]
    pub status_waiting: Color,
    /// Session status: idle / no active work.
    #[serde(with = "hex_color")]
    pub status_idle: Color,
    /// Session status: error / destructive action accent.
    #[serde(with = "hex_color")]
    pub status_error: Color,
}

impl Theme {
    /// Resolve a theme by name. Checks the user theme directory
    /// first, then the built-in set, then falls back to opencode.
    pub fn load(name: &str, user_dir: Option<&Path>) -> Self {
        if let Some(dir) = user_dir {
            let path = dir.join(format!("{name}.toml"));
            if let Ok(s) = std::fs::read_to_string(&path) {
                match toml::from_str::<Theme>(&s) {
                    Ok(t) => return t,
                    Err(e) => {
                        tracing::warn!("failed to parse user theme {:?}: {}", path, e);
                    }
                }
            }
        }
        Self::builtin(name).unwrap_or_else(Self::default_opencode)
    }

    /// Try to load one of the compiled-in themes by name.
    pub fn builtin(name: &str) -> Option<Self> {
        let src = match name {
            "opencode" => include_str!("../../themes/opencode.toml"),
            "tokyonight" => include_str!("../../themes/tokyonight.toml"),
            _ => return None,
        };
        match toml::from_str::<Theme>(src) {
            Ok(t) => Some(t),
            Err(e) => {
                tracing::error!("built-in theme {} failed to parse: {}", name, e);
                None
            }
        }
    }

    /// Hard fallback. The built-in opencode theme should always
    /// parse — if it doesn't, we'd rather panic at startup than run
    /// with a garbled UI.
    pub fn default_opencode() -> Self {
        Self::builtin("opencode").expect("built-in opencode theme must parse")
    }
}

mod hex_color {
    use ratatui::style::Color;
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D>(de: D) -> Result<Color, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(de)?;
        parse_hex(&s).map_err(serde::de::Error::custom)
    }

    fn parse_hex(s: &str) -> Result<Color, String> {
        let hex = s.trim().trim_start_matches('#');
        if hex.len() != 6 {
            return Err(format!("expected #RRGGBB hex color, got {s:?}"));
        }
        let r = u8::from_str_radix(&hex[0..2], 16).map_err(|e| e.to_string())?;
        let g = u8::from_str_radix(&hex[2..4], 16).map_err(|e| e.to_string())?;
        let b = u8::from_str_radix(&hex[4..6], 16).map_err(|e| e.to_string())?;
        Ok(Color::Rgb(r, g, b))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_opencode_parses() {
        let t = Theme::builtin("opencode").expect("opencode must parse");
        assert_eq!(t.name, "opencode");
        assert_eq!(t.accent, Color::Rgb(0x7c, 0x5c, 0xff));
        assert_eq!(t.bg, Color::Rgb(0x0b, 0x0d, 0x12));
    }

    #[test]
    fn builtin_tokyonight_parses() {
        let t = Theme::builtin("tokyonight").expect("tokyonight must parse");
        assert_eq!(t.name, "tokyonight");
        assert_eq!(t.accent, Color::Rgb(0x7a, 0xa2, 0xf7));
    }

    #[test]
    fn unknown_builtin_is_none() {
        assert!(Theme::builtin("does-not-exist").is_none());
    }

    #[test]
    fn load_missing_theme_falls_back_to_opencode() {
        let t = Theme::load("nonexistent", None);
        assert_eq!(t.name, "opencode");
    }

    #[test]
    fn user_dir_override_takes_precedence() {
        let dir = tempdir();
        std::fs::write(
            dir.join("opencode.toml"),
            r##"name = "opencode-custom"
bg = "#000000"
panel = "#000000"
panel_alt = "#000000"
selection_bg = "#000000"
text = "#ffffff"
text_muted = "#ffffff"
accent = "#ff0000"
shadow = "#000000"
dim_fg = "#000000"
status_running = "#00ff00"
status_waiting = "#ffff00"
status_idle = "#888888"
status_error = "#ff0000"
"##,
        )
        .unwrap();
        let t = Theme::load("opencode", Some(&dir));
        assert_eq!(t.name, "opencode-custom");
        assert_eq!(t.accent, Color::Rgb(0xff, 0x00, 0x00));
    }

    fn tempdir() -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!(
            "bosun-theme-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }
}
