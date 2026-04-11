//! Modal form for creating a new bosun-managed tmux session.
//!
//! Fields: name (auto-prefixed with bosun-), working directory, agent
//! (dropdown), extra args. Tab/Shift-Tab move between fields, Enter
//! submits, Esc cancels. The modal emits a `Command::CreateSession`
//! on submit and lets the tmux actor handle the actual `tmux
//! new-session` invocation.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use ratatui::Frame;

use crate::events::{
    ClaudeOptions, ClaudeSessionMode, CodexOptions, Command, SessionSpec, SpecOptions,
};
use crate::store::Recent;

use super::recents::RecentsModal;
use super::{center_rect, Modal, ModalData, ModalResult};

// --- Visual tokens (will move to theme in Phase 4) -------------------

const BG: Color = Color::Rgb(19, 23, 34);
const ACCENT: Color = Color::Rgb(124, 92, 255);
const TEXT: Color = Color::Rgb(230, 233, 239);
const MUTED: Color = Color::Rgb(124, 132, 149);
const FIELD_BG: Color = Color::Rgb(11, 13, 18);
const FIELD_BG_FOCUS: Color = Color::Rgb(30, 36, 51);
const ERROR: Color = Color::Rgb(255, 93, 107);
const SHADOW: Color = Color::Rgb(5, 7, 11);

const MODAL_WIDTH: u16 = 64;
// Sized for the largest agent (claude with all options visible) plus
// space for up to `PATH_SUGGESTION_CAP` rows of recent-path dropdown.
// Smaller agents / no suggestions render with trailing blank space.
const MODAL_HEIGHT: u16 = 32;

const PATH_SUGGESTION_CAP: usize = 5;

// --- Agent dropdown --------------------------------------------------

pub const AGENTS: &[&str] = &["claude", "codex", "terminal"];

// --- Modal state -----------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Name,
    Path,
    Agent,
    Args,
    // Claude-only
    ClaudeSession,
    ClaudeSkipPerm,
    // Codex-only
    CodexYolo,
}

impl Field {
    /// Ordered list of fields visible for the currently-selected agent.
    fn visible_for(agent: &str) -> Vec<Field> {
        let mut v = vec![Field::Name, Field::Path, Field::Agent, Field::Args];
        match agent {
            "claude" => {
                v.push(Field::ClaudeSession);
                v.push(Field::ClaudeSkipPerm);
            }
            "codex" => {
                v.push(Field::CodexYolo);
            }
            _ => {}
        }
        v
    }
}

pub struct NewSessionModal {
    name: String,
    path: String,
    agent_idx: usize,
    args: String,
    claude: ClaudeOptions,
    codex: CodexOptions,
    field: Field,
    error: Option<String>,
    /// Recents cached at modal construction time, used when the user
    /// hits Ctrl+R to open the RecentsModal. Fresh on every new
    /// modal open.
    recents: Vec<Recent>,
    /// Index into `path_suggestions()` when the user has arrowed
    /// down into the filesystem dropdown. `None` means the user is
    /// typing freely (no dropdown entry highlighted).
    path_suggestion_idx: Option<usize>,
}

/// One row in the filesystem dropdown. `name` is the last path
/// segment; `is_dir` drives trailing-slash decoration and Enter's
/// "dive in vs commit" behavior.
#[derive(Debug, Clone)]
struct PathEntry {
    name: String,
    is_dir: bool,
}

impl NewSessionModal {
    pub fn new(recents: Vec<Recent>) -> Self {
        let path = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "~".to_string());
        Self {
            name: String::new(),
            path,
            agent_idx: 0,
            args: String::new(),
            claude: ClaudeOptions::default(),
            codex: CodexOptions::default(),
            field: Field::Name,
            error: None,
            recents,
            path_suggestion_idx: None,
        }
    }

    /// Filesystem entries that match the current `self.path`.
    /// Reads the directory portion of the typed path and filters by
    /// the trailing segment. Capped at `PATH_SUGGESTION_CAP` for UI.
    fn path_suggestions(&self) -> Vec<PathEntry> {
        read_dir_filtered(&self.path, PATH_SUGGESTION_CAP)
    }

    /// Uncapped list of filesystem matches. Used by Tab's longest-
    /// common-prefix completion so we don't miss matches beyond the
    /// display window.
    fn path_suggestions_all(&self) -> Vec<PathEntry> {
        read_dir_filtered(&self.path, usize::MAX)
    }

    /// Commit a filesystem entry into `self.path`. Directories get a
    /// trailing slash so the dropdown refreshes with their contents
    /// on the next render.
    fn commit_path_entry(&mut self, entry: &PathEntry) {
        let (dir, _prefix) = split_path(&self.path);
        let mut new_path = format!("{}{}", dir, entry.name);
        if entry.is_dir {
            new_path.push('/');
        }
        self.path = new_path;
        self.path_suggestion_idx = None;
    }

    /// Shell-style Tab completion. Returns true if the path was
    /// extended (caller should stay on the Path field); false means
    /// "nothing to do, advance to next field".
    fn tab_complete_path(&mut self) -> bool {
        let suggestions = self.path_suggestions_all();
        if suggestions.is_empty() {
            return false;
        }
        let (dir, prefix) = split_path(&self.path);

        // One match: commit it outright (with trailing slash for
        // dirs so the user can dive further).
        if suggestions.len() == 1 {
            self.commit_path_entry(&suggestions[0]);
            return true;
        }

        // Many matches: extend to the longest common prefix.
        let names: Vec<&str> = suggestions.iter().map(|e| e.name.as_str()).collect();
        let lcp = longest_common_prefix(&names);
        if lcp.chars().count() > prefix.chars().count() {
            self.path = format!("{}{}", dir, lcp);
            self.path_suggestion_idx = None;
            return true;
        }
        false
    }

    /// Overwrite all form fields from a selected recent. Called by
    /// `on_child_closed` when the RecentsModal returns a
    /// `FillSessionSpec`.
    fn fill_from_spec(&mut self, spec: SessionSpec) {
        self.name = spec.name;
        self.path = spec.path;
        self.args = spec.args;
        self.claude = spec.options.claude;
        self.codex = spec.options.codex;
        if let Some(idx) = AGENTS.iter().position(|a| *a == spec.agent) {
            self.agent_idx = idx;
        }
        self.error = None;
        self.field = Field::Name;
    }

    fn agent(&self) -> &'static str {
        AGENTS[self.agent_idx]
    }

    fn next_field(&mut self) {
        let visible = Field::visible_for(self.agent());
        let idx = visible.iter().position(|f| *f == self.field).unwrap_or(0);
        self.field = visible[(idx + 1) % visible.len()];
    }

    fn prev_field(&mut self) {
        let visible = Field::visible_for(self.agent());
        let idx = visible.iter().position(|f| *f == self.field).unwrap_or(0);
        self.field = visible[(idx + visible.len() - 1) % visible.len()];
    }

    /// When the agent changes, snap the focused field to something
    /// that actually exists in the new agent's option set. This only
    /// matters if the user is mid-navigation on an agent-specific
    /// field when the agent changes, which currently can't happen
    /// (agent can only change while on Field::Agent) — but the clamp
    /// is cheap and keeps the invariant obvious.
    fn clamp_field_for_agent(&mut self) {
        let visible = Field::visible_for(self.agent());
        if !visible.contains(&self.field) {
            self.field = Field::Agent;
        }
    }

    fn build_spec(&self) -> Result<SessionSpec, String> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err("name is required".into());
        }
        // Don't allow the user to type `bosun-foo` — we prepend the
        // prefix in the actor based on Config. Strip it here if they
        // typed it, so the stored form is always the bare name.
        let name = name.strip_prefix("bosun-").unwrap_or(name);
        // The internal tmux session name is slugified from this, so
        // we need at least one alphanumeric character to work with.
        if !name.chars().any(|c| c.is_alphanumeric()) {
            return Err("name must contain at least one letter or digit".into());
        }

        let path = self.path.trim();
        if path.is_empty() {
            return Err("path is required".into());
        }

        Ok(SessionSpec {
            name: name.to_string(),
            path: path.to_string(),
            agent: self.agent().to_string(),
            args: self.args.trim().to_string(),
            options: SpecOptions {
                claude: self.claude.clone(),
                codex: self.codex.clone(),
            },
        })
    }
}

impl Default for NewSessionModal {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl Modal for NewSessionModal {
    fn id(&self) -> &'static str {
        "new_session"
    }

    fn handle(&mut self, key: KeyEvent) -> ModalResult {
        // Let Ctrl-C close the modal as a convenience.
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return ModalResult::Close(None);
        }

        // Ctrl-R opens the recents picker.
        if key.code == KeyCode::Char('r') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return ModalResult::Push(Box::new(RecentsModal::new(self.recents.clone())));
        }

        match key.code {
            KeyCode::Esc => ModalResult::Close(None),
            KeyCode::Tab => {
                if self.field == Field::Path {
                    // 1. If the user arrowed into the dropdown, Tab
                    //    commits the highlighted entry. For dirs we
                    //    stay on the field so they can dive further.
                    if let Some(idx) = self.path_suggestion_idx {
                        let entries = self.path_suggestions();
                        if let Some(entry) = entries.get(idx).cloned() {
                            self.commit_path_entry(&entry);
                            return ModalResult::Consumed;
                        }
                    }
                    // 2. Shell-style LCP completion against the live
                    //    filesystem.
                    if self.tab_complete_path() {
                        return ModalResult::Consumed;
                    }
                }
                self.next_field();
                ModalResult::Consumed
            }
            KeyCode::BackTab => {
                self.prev_field();
                ModalResult::Consumed
            }
            KeyCode::Enter => {
                // Enter on Path with a highlighted dropdown entry:
                // commit it. Directories → stay on Path so the user
                // keeps browsing into subfolders. Files → advance to
                // the next field (so Enter feels like "pick this").
                if self.field == Field::Path {
                    if let Some(idx) = self.path_suggestion_idx {
                        let entries = self.path_suggestions();
                        if let Some(entry) = entries.get(idx).cloned() {
                            let was_dir = entry.is_dir;
                            self.commit_path_entry(&entry);
                            if !was_dir {
                                self.next_field();
                            }
                            return ModalResult::Consumed;
                        }
                    }
                }
                match self.build_spec() {
                    Ok(spec) => ModalResult::Close(Some(Command::CreateSession(spec))),
                    Err(e) => {
                        self.error = Some(e);
                        ModalResult::Consumed
                    }
                }
            }
            KeyCode::Left => {
                match self.field {
                    Field::Agent => {
                        self.agent_idx = (self.agent_idx + AGENTS.len() - 1) % AGENTS.len();
                        self.clamp_field_for_agent();
                    }
                    Field::ClaudeSession => {
                        self.claude.session_mode = self.claude.session_mode.prev();
                    }
                    _ => {}
                }
                ModalResult::Consumed
            }
            KeyCode::Right => {
                match self.field {
                    Field::Agent => {
                        self.agent_idx = (self.agent_idx + 1) % AGENTS.len();
                        self.clamp_field_for_agent();
                    }
                    Field::ClaudeSession => {
                        self.claude.session_mode = self.claude.session_mode.next();
                    }
                    _ => {}
                }
                ModalResult::Consumed
            }
            KeyCode::Down if self.field == Field::Path => {
                let suggestions = self.path_suggestions();
                if !suggestions.is_empty() {
                    self.path_suggestion_idx = Some(match self.path_suggestion_idx {
                        None => 0,
                        Some(i) if i + 1 < suggestions.len() => i + 1,
                        Some(i) => i,
                    });
                }
                ModalResult::Consumed
            }
            KeyCode::Up if self.field == Field::Path => {
                self.path_suggestion_idx = match self.path_suggestion_idx {
                    None | Some(0) => None,
                    Some(i) => Some(i - 1),
                };
                ModalResult::Consumed
            }
            KeyCode::Backspace => {
                self.error = None;
                match self.field {
                    Field::Name => {
                        self.name.pop();
                    }
                    Field::Path => {
                        self.path.pop();
                        self.path_suggestion_idx = None;
                    }
                    Field::Args => {
                        self.args.pop();
                    }
                    _ => {}
                }
                ModalResult::Consumed
            }
            KeyCode::Char(' ') => {
                // Space: toggle boolean option fields, cycle agent on
                // the Agent field, or type a literal space in text
                // fields.
                self.error = None;
                match self.field {
                    Field::Name => self.name.push(' '),
                    Field::Path => {
                        self.path.push(' ');
                        self.path_suggestion_idx = None;
                    }
                    Field::Args => self.args.push(' '),
                    Field::Agent => {
                        self.agent_idx = (self.agent_idx + 1) % AGENTS.len();
                        self.clamp_field_for_agent();
                    }
                    Field::ClaudeSkipPerm => {
                        self.claude.skip_permissions = !self.claude.skip_permissions;
                    }
                    Field::CodexYolo => {
                        self.codex.yolo = !self.codex.yolo;
                    }
                    Field::ClaudeSession => {
                        // Space on a radio cycles forward, matching Right.
                        self.claude.session_mode = self.claude.session_mode.next();
                    }
                }
                ModalResult::Consumed
            }
            KeyCode::Char(c) => {
                self.error = None;
                match self.field {
                    Field::Name => self.name.push(c),
                    Field::Path => {
                        self.path.push(c);
                        self.path_suggestion_idx = None;
                    }
                    Field::Args => self.args.push(c),
                    _ => {}
                }
                ModalResult::Consumed
            }
            _ => ModalResult::Consumed,
        }
    }

    fn on_child_closed(&mut self, data: ModalData) {
        let ModalData::FillSessionSpec(spec) = data;
        self.fill_from_spec(spec);
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let rect = center_rect(area, MODAL_WIDTH, MODAL_HEIGHT);
        let buf = frame.buffer_mut();

        // Drop shadow: 1 row below + 1 col right in near-black.
        if rect.x + rect.width < area.x + area.width && rect.y + rect.height < area.y + area.height
        {
            let shadow = Rect::new(rect.x + 1, rect.y + 1, rect.width, rect.height);
            let style = Style::default().bg(SHADOW);
            for y in shadow.top()..shadow.bottom() {
                for x in shadow.left()..shadow.right() {
                    let cell = &mut buf[(x, y)];
                    cell.set_style(style);
                }
            }
        }

        // Modal body: solid panel fill.
        let body_style = Style::default().bg(BG);
        for y in rect.top()..rect.bottom() {
            for x in rect.left()..rect.right() {
                let cell = &mut buf[(x, y)];
                cell.set_char(' ');
                cell.set_style(body_style);
            }
        }

        // Left accent bar — 1 col wide, full height.
        let accent_style = Style::default().bg(ACCENT);
        for y in rect.top()..rect.bottom() {
            let cell = &mut buf[(rect.left(), y)];
            cell.set_char(' ');
            cell.set_style(accent_style);
        }

        // Content inset from the accent bar + padding.
        let inner = Rect::new(
            rect.x + 3,
            rect.y + 1,
            rect.width.saturating_sub(4),
            rect.height.saturating_sub(2),
        );

        let mut lines: Vec<Line<'static>> = vec![
            Line::from(vec![
                Span::styled(
                    "New session",
                    Style::default()
                        .fg(TEXT)
                        .bg(BG)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    "    tab next · ^r recents · esc cancel · enter create",
                    Style::default().fg(MUTED).bg(BG),
                ),
            ]),
            Line::from(""),
            label_line("name", self.field == Field::Name),
            input_line(&self.name, self.field == Field::Name, inner.width),
            Line::from(""),
            label_line("path", self.field == Field::Path),
            input_line(&self.path, self.field == Field::Path, inner.width),
        ];

        // Filesystem dropdown — visible when the Path field is
        // focused and the current directory has matching entries.
        if self.field == Field::Path {
            let suggestions = self.path_suggestions();
            for (i, entry) in suggestions.iter().enumerate() {
                let highlighted = self.path_suggestion_idx == Some(i);
                lines.push(path_suggestion_line(entry, highlighted, inner.width));
            }
        }

        lines.extend([
            Line::from(""),
            label_line("agent", self.field == Field::Agent),
            agent_line(self.agent_idx, self.field == Field::Agent),
            Line::from(""),
            label_line("args (optional)", self.field == Field::Args),
            input_line(&self.args, self.field == Field::Args, inner.width),
        ]);

        // Agent-specific options section.
        match self.agent() {
            "claude" => {
                lines.push(Line::from(""));
                lines.push(section_header("— Claude options —"));
                lines.push(session_radio_line(
                    self.claude.session_mode,
                    self.field == Field::ClaudeSession,
                ));
                lines.push(checkbox_line(
                    "Skip permissions (--dangerously-skip-permissions)",
                    self.claude.skip_permissions,
                    self.field == Field::ClaudeSkipPerm,
                ));
            }
            "codex" => {
                lines.push(Line::from(""));
                lines.push(section_header("— Codex options —"));
                lines.push(checkbox_line(
                    "YOLO mode (--yolo · bypass approvals & sandbox)",
                    self.codex.yolo,
                    self.field == Field::CodexYolo,
                ));
            }
            _ => {}
        }

        if let Some(e) = &self.error {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!("  ! {}", e),
                Style::default().fg(ERROR).bg(BG),
            )));
        }

        Paragraph::new(lines)
            .style(Style::default().bg(BG))
            .render(inner, frame.buffer_mut());
    }
}

fn label_line(label: &str, focused: bool) -> Line<'static> {
    let marker = if focused { "▸" } else { " " };
    let label_style = if focused {
        Style::default()
            .fg(ACCENT)
            .bg(BG)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(MUTED).bg(BG)
    };
    Line::from(vec![
        Span::styled(format!(" {} ", marker), label_style),
        Span::styled(label.to_string(), label_style),
    ])
}

/// One row in the filesystem dropdown. Directories get a trailing
/// slash so they're visually distinct from files. Highlighted rows
/// get the accent background and a ▸ marker.
fn path_suggestion_line(entry: &PathEntry, highlighted: bool, width: u16) -> Line<'static> {
    let bg = if highlighted {
        FIELD_BG_FOCUS
    } else {
        FIELD_BG
    };
    let fg = if highlighted { TEXT } else { MUTED };
    let marker = if highlighted { "▸" } else { " " };
    let suffix = if entry.is_dir { "/" } else { "" };

    let full = format!(" {} {}{}", marker, entry.name, suffix);
    let field_width = width.saturating_sub(3) as usize;
    let mut padded = full;
    while padded.chars().count() < field_width {
        padded.push(' ');
    }

    Line::from(vec![
        Span::styled("   ", Style::default().bg(BG)),
        Span::styled(padded, Style::default().fg(fg).bg(bg)),
    ])
}

// --- Filesystem helpers ---------------------------------------------

/// Split a path into its directory portion (with trailing `/`) and
/// the trailing segment the user is typing. Preserves a leading `~`
/// so the stored path keeps its original form.
fn split_path(path: &str) -> (String, String) {
    if path.is_empty() {
        return (String::new(), String::new());
    }
    if path.ends_with('/') {
        return (path.to_string(), String::new());
    }
    match path.rfind('/') {
        Some(idx) => (path[..=idx].to_string(), path[idx + 1..].to_string()),
        None => (String::new(), path.to_string()),
    }
}

/// Expand a leading `~` or `~/` to `$HOME`. Only used for the actual
/// `read_dir` call; the stored path retains the user's form.
fn expand_tilde(path: &str) -> String {
    if path == "~" {
        return std::env::var("HOME").unwrap_or_default();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_default();
        return format!("{}/{}", home, rest);
    }
    path.to_string()
}

/// Read the directory implied by `path` and return entries whose
/// names start with the trailing segment of `path`. Dirs come first,
/// then files, alphabetically within each group. Hidden entries
/// (starting with `.`) are excluded unless the user's typed prefix
/// also starts with `.`. Capped at `limit` entries.
fn read_dir_filtered(path: &str, limit: usize) -> Vec<PathEntry> {
    let (dir, prefix) = split_path(path);
    // Empty dir = CWD. Otherwise expand ~ for the filesystem lookup.
    let lookup = if dir.is_empty() {
        ".".to_string()
    } else {
        expand_tilde(&dir)
    };
    let Ok(read) = std::fs::read_dir(&lookup) else {
        return Vec::new();
    };
    let show_hidden = prefix.starts_with('.');
    let mut out: Vec<PathEntry> = Vec::new();
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        if !name.starts_with(&prefix) {
            continue;
        }
        let is_dir = entry.file_type().ok().map(|t| t.is_dir()).unwrap_or(false);
        out.push(PathEntry { name, is_dir });
    }
    out.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.cmp(&b.name),
    });
    out.truncate(limit);
    out
}

/// Longest common prefix of a set of strings (character-wise, so
/// multi-byte Unicode is handled correctly).
fn longest_common_prefix(strs: &[&str]) -> String {
    if strs.is_empty() {
        return String::new();
    }
    let mut prefix: Vec<char> = strs[0].chars().collect();
    for s in &strs[1..] {
        let common_len = prefix
            .iter()
            .zip(s.chars())
            .take_while(|(a, b)| **a == *b)
            .count();
        prefix.truncate(common_len);
        if prefix.is_empty() {
            break;
        }
    }
    prefix.into_iter().collect()
}

fn input_line(value: &str, focused: bool, width: u16) -> Line<'static> {
    let bg = if focused { FIELD_BG_FOCUS } else { FIELD_BG };
    let fg = if value.is_empty() { MUTED } else { TEXT };
    let cursor = if focused { "│" } else { "" };
    let content = format!(" {}{} ", value, cursor);
    // Pad content to field width so the bg extends cleanly.
    let field_width = width.saturating_sub(3) as usize;
    let padded = if content.chars().count() < field_width {
        let mut s = content;
        while s.chars().count() < field_width {
            s.push(' ');
        }
        s
    } else {
        content
    };
    Line::from(vec![
        Span::styled("   ", Style::default().bg(BG)),
        Span::styled(padded, Style::default().fg(fg).bg(bg)),
    ])
}

fn section_header(text: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled("   ", Style::default().bg(BG)),
        Span::styled(
            text.to_string(),
            Style::default()
                .fg(MUTED)
                .bg(BG)
                .add_modifier(Modifier::BOLD),
        ),
    ])
}

fn checkbox_line(label: &str, checked: bool, focused: bool) -> Line<'static> {
    let marker = if focused { "▸" } else { " " };
    let box_glyph = if checked { "[x]" } else { "[ ]" };
    let label_style = if focused {
        Style::default()
            .fg(ACCENT)
            .bg(BG)
            .add_modifier(Modifier::BOLD)
    } else if checked {
        Style::default().fg(TEXT).bg(BG)
    } else {
        Style::default().fg(MUTED).bg(BG)
    };
    let box_style = if checked {
        Style::default()
            .fg(ACCENT)
            .bg(BG)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(MUTED).bg(BG)
    };
    Line::from(vec![
        Span::styled(format!(" {} ", marker), label_style),
        Span::styled(box_glyph.to_string(), box_style),
        Span::styled(" ", Style::default().bg(BG)),
        Span::styled(label.to_string(), label_style),
    ])
}

fn session_radio_line(mode: ClaudeSessionMode, focused: bool) -> Line<'static> {
    let marker = if focused { "▸" } else { " " };
    let marker_style = if focused {
        Style::default()
            .fg(ACCENT)
            .bg(BG)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(MUTED).bg(BG)
    };
    let label_style = if focused {
        Style::default()
            .fg(ACCENT)
            .bg(BG)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(MUTED).bg(BG)
    };

    let mut spans: Vec<Span<'static>> = vec![
        Span::styled(format!(" {} ", marker), marker_style),
        Span::styled("Session  ", label_style),
    ];
    for option in [
        ClaudeSessionMode::New,
        ClaudeSessionMode::Continue,
        ClaudeSessionMode::Resume,
    ] {
        let selected = option == mode;
        let (dot, val_style) = if selected {
            let style = if focused {
                Style::default()
                    .fg(ACCENT)
                    .bg(BG)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(TEXT)
                    .bg(BG)
                    .add_modifier(Modifier::BOLD)
            };
            ("(•)", style)
        } else {
            ("( )", Style::default().fg(MUTED).bg(BG))
        };
        spans.push(Span::styled(format!(" {} ", dot), val_style));
        spans.push(Span::styled(option.label().to_string(), val_style));
        spans.push(Span::styled(" ", Style::default().bg(BG)));
    }
    Line::from(spans)
}

fn agent_line(selected: usize, focused: bool) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::styled("   ", Style::default().bg(BG)));
    for (i, agent) in AGENTS.iter().enumerate() {
        let style = if i == selected && focused {
            Style::default()
                .fg(Color::Rgb(11, 13, 18))
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD)
        } else if i == selected {
            Style::default()
                .fg(ACCENT)
                .bg(BG)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(MUTED).bg(BG)
        };
        spans.push(Span::styled(format!(" {} ", agent), style));
        spans.push(Span::styled(" ", Style::default().bg(BG)));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn tab_cycles_fields_for_claude() {
        let mut m = NewSessionModal::new(Vec::new());
        assert_eq!(m.agent(), "claude");
        assert_eq!(m.field, Field::Name);
        m.handle(key(KeyCode::Tab));
        assert_eq!(m.field, Field::Path);
        m.handle(key(KeyCode::Tab));
        assert_eq!(m.field, Field::Agent);
        m.handle(key(KeyCode::Tab));
        assert_eq!(m.field, Field::Args);
        m.handle(key(KeyCode::Tab));
        assert_eq!(m.field, Field::ClaudeSession);
        m.handle(key(KeyCode::Tab));
        assert_eq!(m.field, Field::ClaudeSkipPerm);
        // Wraps back to Name.
        m.handle(key(KeyCode::Tab));
        assert_eq!(m.field, Field::Name);
    }

    #[test]
    fn tab_cycles_fields_for_codex() {
        let mut m = NewSessionModal::new(Vec::new());
        // Switch to codex (second in the list).
        m.agent_idx = 1;
        assert_eq!(m.agent(), "codex");
        m.handle(key(KeyCode::Tab)); // Name -> Path
        m.handle(key(KeyCode::Tab)); // Path -> Agent
        m.handle(key(KeyCode::Tab)); // Agent -> Args
        m.handle(key(KeyCode::Tab)); // Args -> CodexYolo
        assert_eq!(m.field, Field::CodexYolo);
        m.handle(key(KeyCode::Tab));
        assert_eq!(m.field, Field::Name);
    }

    #[test]
    fn tab_cycles_fields_for_terminal() {
        let mut m = NewSessionModal::new(Vec::new());
        m.agent_idx = 2;
        assert_eq!(m.agent(), "terminal");
        m.handle(key(KeyCode::Tab)); // Name -> Path
        m.handle(key(KeyCode::Tab)); // Path -> Agent
        m.handle(key(KeyCode::Tab)); // Agent -> Args
        assert_eq!(m.field, Field::Args);
        m.handle(key(KeyCode::Tab));
        assert_eq!(m.field, Field::Name);
    }

    #[test]
    fn space_toggles_skip_permissions_when_focused() {
        let mut m = NewSessionModal::new(Vec::new());
        m.field = Field::ClaudeSkipPerm;
        assert!(!m.claude.skip_permissions);
        m.handle(key(KeyCode::Char(' ')));
        assert!(m.claude.skip_permissions);
        m.handle(key(KeyCode::Char(' ')));
        assert!(!m.claude.skip_permissions);
    }

    #[test]
    fn left_right_cycles_claude_session_mode() {
        let mut m = NewSessionModal::new(Vec::new());
        m.field = Field::ClaudeSession;
        assert_eq!(m.claude.session_mode, ClaudeSessionMode::New);
        m.handle(key(KeyCode::Right));
        assert_eq!(m.claude.session_mode, ClaudeSessionMode::Continue);
        m.handle(key(KeyCode::Right));
        assert_eq!(m.claude.session_mode, ClaudeSessionMode::Resume);
        m.handle(key(KeyCode::Right));
        assert_eq!(m.claude.session_mode, ClaudeSessionMode::New);
        m.handle(key(KeyCode::Left));
        assert_eq!(m.claude.session_mode, ClaudeSessionMode::Resume);
    }

    #[test]
    fn space_toggles_codex_yolo_when_focused() {
        let mut m = NewSessionModal::new(Vec::new());
        m.agent_idx = 1;
        m.field = Field::CodexYolo;
        assert!(!m.codex.yolo);
        m.handle(key(KeyCode::Char(' ')));
        assert!(m.codex.yolo);
    }

    #[test]
    fn submit_spec_carries_claude_options() {
        let mut m = NewSessionModal::new(Vec::new());
        for c in "test".chars() {
            m.handle(key(KeyCode::Char(c)));
        }
        m.claude.skip_permissions = true;
        m.claude.session_mode = ClaudeSessionMode::Continue;
        let r = m.handle(key(KeyCode::Enter));
        match r {
            ModalResult::Close(Some(Command::CreateSession(spec))) => {
                assert!(spec.options.claude.skip_permissions);
                assert_eq!(
                    spec.options.claude.session_mode,
                    ClaudeSessionMode::Continue
                );
            }
            _ => panic!("expected CreateSession"),
        }
    }

    #[test]
    fn typing_fills_focused_field() {
        let mut m = NewSessionModal::new(Vec::new());
        for c in "api".chars() {
            m.handle(key(KeyCode::Char(c)));
        }
        assert_eq!(m.name, "api");
        m.handle(key(KeyCode::Tab));
        m.handle(key(KeyCode::Backspace));
        // Backspace on path removes from default path, not name.
        assert_eq!(m.name, "api");
    }

    #[test]
    fn left_right_on_agent_field_cycles_selection() {
        let mut m = NewSessionModal::new(Vec::new());
        m.field = Field::Agent;
        assert_eq!(m.agent(), "claude");
        m.handle(key(KeyCode::Right));
        assert_eq!(m.agent(), "codex");
        m.handle(key(KeyCode::Right));
        assert_eq!(m.agent(), "terminal");
        m.handle(key(KeyCode::Right));
        assert_eq!(m.agent(), "claude");
        m.handle(key(KeyCode::Left));
        assert_eq!(m.agent(), "terminal");
    }

    #[test]
    fn enter_with_empty_name_shows_error() {
        let mut m = NewSessionModal::new(Vec::new());
        let r = m.handle(key(KeyCode::Enter));
        assert!(matches!(r, ModalResult::Consumed));
        assert!(m.error.is_some());
    }

    #[test]
    fn enter_with_valid_data_closes_with_command() {
        let mut m = NewSessionModal::new(Vec::new());
        for c in "work".chars() {
            m.handle(key(KeyCode::Char(c)));
        }
        let r = m.handle(key(KeyCode::Enter));
        match r {
            ModalResult::Close(Some(Command::CreateSession(spec))) => {
                assert_eq!(spec.name, "work");
                assert_eq!(spec.agent, "claude");
            }
            _ => panic!("expected Close with CreateSession"),
        }
    }

    #[test]
    fn bosun_prefix_is_stripped_from_name_on_submit() {
        let mut m = NewSessionModal::new(Vec::new());
        for c in "bosun-work".chars() {
            m.handle(key(KeyCode::Char(c)));
        }
        let r = m.handle(key(KeyCode::Enter));
        match r {
            ModalResult::Close(Some(Command::CreateSession(spec))) => {
                assert_eq!(spec.name, "work");
            }
            _ => panic!("expected Close with CreateSession"),
        }
    }

    #[test]
    fn name_with_spaces_is_accepted() {
        let mut m = NewSessionModal::new(Vec::new());
        for c in "My Rocket Fox".chars() {
            m.handle(key(KeyCode::Char(c)));
        }
        let r = m.handle(key(KeyCode::Enter));
        match r {
            ModalResult::Close(Some(Command::CreateSession(spec))) => {
                // Display name preserved verbatim, caps + spaces included.
                assert_eq!(spec.name, "My Rocket Fox");
            }
            _ => panic!("expected CreateSession with 'My Rocket Fox'"),
        }
    }

    #[test]
    fn name_with_only_symbols_is_rejected() {
        let mut m = NewSessionModal::new(Vec::new());
        for c in "!!!".chars() {
            m.handle(key(KeyCode::Char(c)));
        }
        let r = m.handle(key(KeyCode::Enter));
        assert!(matches!(r, ModalResult::Consumed));
        assert!(m.error.as_deref().unwrap().contains("letter"));
    }

    #[test]
    fn esc_closes_without_command() {
        let mut m = NewSessionModal::new(Vec::new());
        let r = m.handle(key(KeyCode::Esc));
        assert!(matches!(r, ModalResult::Close(None)));
    }

    #[test]
    fn ctrl_r_pushes_recents_modal() {
        let recent = Recent {
            id: 1,
            name: "work".into(),
            path: "/srv".into(),
            agent: "claude".into(),
            args: String::new(),
            claude: ClaudeOptions::default(),
            codex: CodexOptions::default(),
            last_used_at: 0,
            use_count: 1,
        };
        let mut m = NewSessionModal::new(vec![recent]);
        let k = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL);
        let r = m.handle(k);
        assert!(matches!(r, ModalResult::Push(_)));
    }

    #[test]
    fn split_path_handles_absolute_and_relative() {
        assert_eq!(
            split_path("/home/rhuk/proj"),
            ("/home/rhuk/".to_string(), "proj".to_string())
        );
        assert_eq!(
            split_path("/home/rhuk/"),
            ("/home/rhuk/".to_string(), "".to_string())
        );
        assert_eq!(split_path("proj"), ("".to_string(), "proj".to_string()));
        assert_eq!(split_path(""), ("".to_string(), "".to_string()));
    }

    #[test]
    fn longest_common_prefix_handles_unicode() {
        assert_eq!(longest_common_prefix(&["abcd", "abce"]), "abc");
        assert_eq!(longest_common_prefix(&["abc", "xyz"]), "");
        assert_eq!(longest_common_prefix(&["same", "same"]), "same");
        assert_eq!(longest_common_prefix(&[]), "");
        // Multi-byte characters handled char-wise.
        assert_eq!(longest_common_prefix(&["日本語", "日本人"]), "日本");
    }

    #[test]
    fn on_child_closed_fills_all_fields_from_spec() {
        let mut m = NewSessionModal::new(Vec::new());
        let spec = SessionSpec {
            name: "api".into(),
            path: "/srv/api".into(),
            agent: "codex".into(),
            args: "--verbose".into(),
            options: SpecOptions {
                claude: ClaudeOptions::default(),
                codex: CodexOptions { yolo: true },
            },
        };
        m.on_child_closed(ModalData::FillSessionSpec(spec));
        assert_eq!(m.name, "api");
        assert_eq!(m.path, "/srv/api");
        assert_eq!(m.args, "--verbose");
        assert_eq!(m.agent(), "codex");
        assert!(m.codex.yolo);
        assert_eq!(m.field, Field::Name);
    }
}
