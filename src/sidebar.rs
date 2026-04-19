//! Sidebar ordering + grouping.
//!
//! The sidebar is a flat ordered list of `SidebarEntry` values —
//! either a `Section` header or a `Session` reference. Membership is
//! implicit: a session belongs to the nearest section header above
//! it, or is "ungrouped" if there's no header above it. This keeps
//! the data model trivial (one Vec, no group-id bookkeeping) while
//! still supporting group moves: moving a Section header with
//! Shift-J/K carries its contiguous session entries along.
//!
//! Persisted in `config.toml` as `sidebar = [...]`. The tmux actor
//! doesn't touch this — it's pure UI state owned by `AppState`.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// One row in the sidebar — either a section header or a session row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SidebarEntry {
    /// A section header. `id` is a stable unique key so the selection
    /// survives rename; `name` is what the user sees.
    Section { id: String, name: String },
    /// A reference to a managed tmux session by its internal name.
    /// The matching `SessionView` is looked up in `AppState::sessions`.
    Session { internal: String },
}

impl SidebarEntry {
    /// Create a new section header with a random id and the given name.
    pub fn new_section(name: impl Into<String>) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let id = format!("sec-{:08x}", nanos as u32);
        Self::Section {
            id,
            name: name.into(),
        }
    }

    pub fn session(internal: impl Into<String>) -> Self {
        Self::Session {
            internal: internal.into(),
        }
    }

    pub fn is_section(&self) -> bool {
        matches!(self, Self::Section { .. })
    }

    pub fn is_session(&self) -> bool {
        matches!(self, Self::Session { .. })
    }

    /// Stable identity for selection-preservation across refreshes.
    /// Sections use their id; sessions use their internal name.
    pub fn identity(&self) -> &str {
        match self {
            Self::Section { id, .. } => id,
            Self::Session { internal } => internal,
        }
    }
}

/// Reconcile `entries` with the current live set of session names.
/// - Drops `Session` entries whose internal name is no longer in `live`.
/// - Appends a `Session` entry at the end for any live name not already
///   referenced.
/// `Section` entries are always kept.
pub fn reconcile(entries: &mut Vec<SidebarEntry>, live: &[String]) {
    entries.retain(|e| match e {
        SidebarEntry::Session { internal } => live.iter().any(|n| n == internal),
        SidebarEntry::Section { .. } => true,
    });
    for name in live {
        let already = entries.iter().any(|e| match e {
            SidebarEntry::Session { internal } => internal == name,
            _ => false,
        });
        if !already {
            entries.push(SidebarEntry::session(name));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ses(name: &str) -> SidebarEntry {
        SidebarEntry::session(name)
    }

    fn sec(id: &str, name: &str) -> SidebarEntry {
        SidebarEntry::Section {
            id: id.to_string(),
            name: name.to_string(),
        }
    }

    #[test]
    fn reconcile_drops_dead_sessions_keeps_sections() {
        let mut e = vec![sec("g1", "Work"), ses("alpha"), ses("beta")];
        reconcile(&mut e, &["alpha".to_string()]);
        assert_eq!(e, vec![sec("g1", "Work"), ses("alpha")]);
    }

    #[test]
    fn reconcile_appends_new_sessions() {
        let mut e = vec![sec("g1", "Work"), ses("alpha")];
        reconcile(&mut e, &["alpha".to_string(), "beta".to_string()]);
        assert_eq!(e, vec![sec("g1", "Work"), ses("alpha"), ses("beta")]);
    }

    #[test]
    fn reconcile_preserves_order() {
        let mut e = vec![ses("b"), sec("g1", "Work"), ses("a")];
        reconcile(&mut e, &["a".to_string(), "b".to_string(), "c".to_string()]);
        // b stays first, group header second, a third, new c appended.
        assert_eq!(
            e,
            vec![ses("b"), sec("g1", "Work"), ses("a"), ses("c")]
        );
    }

    #[test]
    fn roundtrip_toml() {
        let entries = vec![
            sec("g1", "Premium"),
            ses("bosun-alpha-abc"),
            ses("bosun-beta-def"),
            sec("g2", "YetiDevWorks"),
            ses("bosun-gamma-ghi"),
        ];
        let toml = toml::to_string(&Wrap {
            sidebar: entries.clone(),
        })
        .expect("serialize");
        let parsed: Wrap = toml::from_str(&toml).expect("parse");
        assert_eq!(parsed.sidebar, entries);
    }

    #[derive(Serialize, Deserialize)]
    struct Wrap {
        sidebar: Vec<SidebarEntry>,
    }
}
