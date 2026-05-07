//! Sidebar ordering + grouping (explicit-membership model).
//!
//! The sidebar is two ordered lists: an `ungrouped` bucket and a
//! `sections` list. Each section has its own ordered `members` list
//! of internal tmux session names. Membership is explicit — creating
//! a new section produces an empty header that claims no existing
//! sessions. A session is in exactly one bucket.
//!
//! The rendered sidebar flattens this model into a single list:
//! every ungrouped session, followed by each section's header and
//! its members. `AppState::selected` indexes into that flattened
//! list.
//!
//! Persisted in `config.toml` as `[sidebar]` (tables + arrays). The
//! tmux actor doesn't touch this — it's pure UI state owned by
//! `AppState`.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// A named section that groups a set of sessions. `members` holds
/// internal tmux names in the user's chosen order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Section {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub members: Vec<String>,
    /// When true, the section's members are hidden from the rendered
    /// sidebar (only the header is visible). Toggled by Tab on the
    /// header. Persisted in `config.toml` so the open/closed state
    /// survives restarts. Default false (expanded) — old configs
    /// without this field stay expanded on first read.
    #[serde(default, skip_serializing_if = "is_false")]
    pub collapsed: bool,
    /// Per-section override for the TDF banner font shown in the
    /// preview pane. `None` falls back to `Config::banner_font`
    /// (the global default). Toggled by `f` on the header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub banner_font: Option<String>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl Section {
    pub fn new(name: impl Into<String>) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        Self {
            id: format!("sec-{:08x}", nanos as u32),
            name: name.into(),
            members: Vec::new(),
            collapsed: false,
            banner_font: None,
        }
    }
}

/// Full sidebar state. `ungrouped` holds session names with no
/// section; `sections` is an ordered list of sections.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SidebarModel {
    #[serde(default)]
    pub ungrouped: Vec<String>,
    #[serde(default)]
    pub sections: Vec<Section>,
}

/// One row in the rendered sidebar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisibleKind {
    Ungrouped,
    Header,
    Member,
}

/// A location inside the model — used to mutate after resolving a
/// selection index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Location {
    /// `ungrouped[idx]`
    Ungrouped(usize),
    /// `sections[si]` (the header)
    Header(usize),
    /// `sections[si].members[mi]`
    Member(usize, usize),
}

/// A single visible entry produced by flattening the model. Carries
/// references into the model for rendering.
#[derive(Debug, Clone, Copy)]
pub enum VisibleEntry<'a> {
    UngroupedSession(&'a str),
    SectionHeader(&'a Section),
    SectionMember {
        section: &'a Section,
        internal: &'a str,
    },
}

impl<'a> VisibleEntry<'a> {
    pub fn kind(&self) -> VisibleKind {
        match self {
            Self::UngroupedSession(_) => VisibleKind::Ungrouped,
            Self::SectionHeader(_) => VisibleKind::Header,
            Self::SectionMember { .. } => VisibleKind::Member,
        }
    }

    /// For session rows, the internal tmux name. `None` for headers.
    pub fn session_name(&self) -> Option<&'a str> {
        match self {
            Self::UngroupedSession(n) => Some(n),
            Self::SectionMember { internal, .. } => Some(internal),
            Self::SectionHeader(_) => None,
        }
    }

    /// Stable identity for selection-preservation across refreshes.
    pub fn identity(&self) -> &'a str {
        match self {
            Self::UngroupedSession(n) => n,
            Self::SectionHeader(s) => &s.id,
            Self::SectionMember { internal, .. } => internal,
        }
    }
}

impl SidebarModel {
    /// How many of `s.members` actually contribute visible rows. Zero
    /// when the section is collapsed; full count when expanded.
    fn visible_member_count(s: &Section) -> usize {
        if s.collapsed {
            0
        } else {
            s.members.len()
        }
    }

    /// Total number of visible rows in the flattened sidebar.
    pub fn len(&self) -> usize {
        self.ungrouped.len()
            + self
                .sections
                .iter()
                .map(|s| 1 + Self::visible_member_count(s))
                .sum::<usize>()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Flatten the model into an ordered list of visible entries.
    /// Members of collapsed sections are skipped — only the header row
    /// is emitted for those.
    pub fn visible(&self) -> Vec<VisibleEntry<'_>> {
        let mut out = Vec::with_capacity(self.len());
        for n in &self.ungrouped {
            out.push(VisibleEntry::UngroupedSession(n.as_str()));
        }
        for s in &self.sections {
            out.push(VisibleEntry::SectionHeader(s));
            if !s.collapsed {
                for m in &s.members {
                    out.push(VisibleEntry::SectionMember {
                        section: s,
                        internal: m.as_str(),
                    });
                }
            }
        }
        out
    }

    /// Resolve a flattened index to a mutable location in the model.
    /// Returns `None` if `idx` is out of range.
    pub fn locate(&self, idx: usize) -> Option<Location> {
        if idx < self.ungrouped.len() {
            return Some(Location::Ungrouped(idx));
        }
        let mut cursor = self.ungrouped.len();
        for (si, sec) in self.sections.iter().enumerate() {
            if idx == cursor {
                return Some(Location::Header(si));
            }
            let next = cursor + 1 + Self::visible_member_count(sec);
            if idx < next {
                return Some(Location::Member(si, idx - cursor - 1));
            }
            cursor = next;
        }
        None
    }

    /// Convert a location back into a flattened index. Saturates to
    /// `len()` if the location is out of bounds. A collapsed section's
    /// members aren't visible — `Member(si, _)` returns the header's
    /// index in that case.
    pub fn flat_index(&self, loc: Location) -> usize {
        match loc {
            Location::Ungrouped(i) => i.min(self.ungrouped.len()),
            Location::Header(si) => {
                let mut idx = self.ungrouped.len();
                let bound = si.min(self.sections.len());
                for s in &self.sections[..bound] {
                    idx += 1 + Self::visible_member_count(s);
                }
                idx
            }
            Location::Member(si, mi) => {
                if si >= self.sections.len() {
                    return self.len();
                }
                let mut idx = self.ungrouped.len();
                for s in &self.sections[..si] {
                    idx += 1 + Self::visible_member_count(s);
                }
                let header = idx;
                if self.sections[si].collapsed {
                    return header;
                }
                header + 1 + mi.min(self.sections[si].members.len())
            }
        }
    }

    /// Find an entry by identity (section id OR session internal name).
    /// Returns the flattened index. Section ids beat session names if
    /// both exist (they shouldn't — ids use a `sec-` prefix).
    pub fn find_identity(&self, ident: &str) -> Option<usize> {
        for (si, s) in self.sections.iter().enumerate() {
            if s.id == ident {
                return Some(self.flat_index(Location::Header(si)));
            }
        }
        for (i, n) in self.ungrouped.iter().enumerate() {
            if n == ident {
                return Some(self.flat_index(Location::Ungrouped(i)));
            }
        }
        for (si, s) in self.sections.iter().enumerate() {
            for (mi, n) in s.members.iter().enumerate() {
                if n == ident {
                    return Some(self.flat_index(Location::Member(si, mi)));
                }
            }
        }
        None
    }

    /// Reconcile against the current live set of tmux session names.
    /// - Drops any session name not in `live` from every bucket.
    /// - Dedupes sessions that appear in multiple buckets (keeps the
    ///   first occurrence in visible order — ungrouped > section 0 > ...).
    /// - Appends any live session not already present to `ungrouped`.
    ///
    /// Sections are preserved even if they end up empty.
    pub fn reconcile(&mut self, live: &[String]) {
        // 1. Drop dead sessions.
        self.ungrouped.retain(|n| live.iter().any(|l| l == n));
        for s in &mut self.sections {
            s.members.retain(|n| live.iter().any(|l| l == n));
        }
        // 2. Dedupe — if a name appears in multiple places, keep the
        //    earliest in visible order.
        let mut seen = std::collections::HashSet::new();
        self.ungrouped.retain(|n| seen.insert(n.clone()));
        for s in &mut self.sections {
            s.members.retain(|n| seen.insert(n.clone()));
        }
        // 3. Append new live sessions to ungrouped.
        for n in live {
            if !seen.contains(n) {
                self.ungrouped.push(n.clone());
                seen.insert(n.clone());
            }
        }
    }

    /// Append a new empty section at the end of the sections list.
    /// Returns the new section's id.
    pub fn insert_section_at_end(&mut self, name: String) -> String {
        let s = Section::new(name);
        let id = s.id.clone();
        self.sections.push(s);
        id
    }

    /// Rename a section by id. Returns true if found.
    pub fn rename_section(&mut self, id: &str, new_name: String) -> bool {
        for s in &mut self.sections {
            if s.id == id {
                s.name = new_name;
                return true;
            }
        }
        false
    }

    /// Delete a section by its sections-index. Members are appended
    /// to `ungrouped` in their current order.
    pub fn delete_section_at(&mut self, si: usize) {
        if si >= self.sections.len() {
            return;
        }
        let mut sec = self.sections.remove(si);
        self.ungrouped.append(&mut sec.members);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sec(id: &str, name: &str, members: &[&str]) -> Section {
        Section {
            id: id.into(),
            name: name.into(),
            members: members.iter().map(|s| s.to_string()).collect(),
            collapsed: false,
            banner_font: None,
        }
    }

    fn model(ungrouped: &[&str], sections: Vec<Section>) -> SidebarModel {
        SidebarModel {
            ungrouped: ungrouped.iter().map(|s| s.to_string()).collect(),
            sections,
        }
    }

    #[test]
    fn flat_index_matches_visible_iteration() {
        let m = model(
            &["a", "b"],
            vec![sec("g1", "Work", &["c"]), sec("g2", "Play", &["d", "e"])],
        );
        let visible = m.visible();
        assert_eq!(visible.len(), m.len());
        // Locate every index and round-trip through flat_index.
        for i in 0..m.len() {
            let loc = m.locate(i).expect("locate");
            assert_eq!(m.flat_index(loc), i, "round-trip failed at {}", i);
        }
    }

    #[test]
    fn locate_covers_all_zones() {
        let m = model(&["a", "b"], vec![sec("g1", "W", &["c"])]);
        assert!(matches!(m.locate(0), Some(Location::Ungrouped(0))));
        assert!(matches!(m.locate(1), Some(Location::Ungrouped(1))));
        assert!(matches!(m.locate(2), Some(Location::Header(0))));
        assert!(matches!(m.locate(3), Some(Location::Member(0, 0))));
        assert!(m.locate(4).is_none());
    }

    #[test]
    fn reconcile_drops_dead_keeps_sections_appends_new() {
        let mut m = model(&["a"], vec![sec("g1", "W", &["b", "gone"])]);
        m.reconcile(&["a".into(), "b".into(), "newbie".into()]);
        // "gone" is removed from the section. "newbie" lands in ungrouped.
        assert_eq!(m.ungrouped, vec!["a".to_string(), "newbie".to_string()]);
        assert_eq!(m.sections[0].members, vec!["b".to_string()]);
    }

    #[test]
    fn reconcile_dedupes_across_buckets() {
        let mut m = model(&["a", "b"], vec![sec("g1", "W", &["b", "c"])]);
        // b appears in both ungrouped and g1; reconcile should leave
        // it only in ungrouped (earliest in visible order wins).
        m.reconcile(&["a".into(), "b".into(), "c".into()]);
        assert_eq!(m.ungrouped, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(m.sections[0].members, vec!["c".to_string()]);
    }

    #[test]
    fn delete_section_moves_members_to_ungrouped() {
        let mut m = model(&["a"], vec![sec("g1", "W", &["b", "c"])]);
        m.delete_section_at(0);
        assert_eq!(
            m.ungrouped,
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
        assert!(m.sections.is_empty());
    }

    #[test]
    fn find_identity_returns_flat_index() {
        let m = model(&["a"], vec![sec("g1", "W", &["b"])]);
        assert_eq!(m.find_identity("a"), Some(0));
        assert_eq!(m.find_identity("g1"), Some(1));
        assert_eq!(m.find_identity("b"), Some(2));
        assert!(m.find_identity("nope").is_none());
    }

    #[test]
    fn roundtrip_toml() {
        let m = model(
            &["bosun-alpha"],
            vec![
                sec("g1", "Premium", &["bosun-beta", "bosun-gamma"]),
                sec("g2", "YetiDev", &[]),
            ],
        );
        let toml = toml::to_string(&m).expect("serialize");
        let parsed: SidebarModel = toml::from_str(&toml).expect("parse");
        assert_eq!(parsed, m);
    }
}
