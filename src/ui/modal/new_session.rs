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

use super::{center_rect, Modal, ModalResult};

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
// Sized for the largest agent (claude with all options visible).
// Smaller agents render with trailing blank space.
const MODAL_HEIGHT: u16 = 26;

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
}

impl NewSessionModal {
    pub fn new() -> Self {
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
        }
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
        if name.contains(char::is_whitespace) {
            return Err("name cannot contain whitespace".into());
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
        Self::new()
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

        match key.code {
            KeyCode::Esc => ModalResult::Close(None),
            KeyCode::Tab => {
                self.next_field();
                ModalResult::Consumed
            }
            KeyCode::BackTab => {
                self.prev_field();
                ModalResult::Consumed
            }
            KeyCode::Enter => match self.build_spec() {
                Ok(spec) => ModalResult::Close(Some(Command::CreateSession(spec))),
                Err(e) => {
                    self.error = Some(e);
                    ModalResult::Consumed
                }
            },
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
            KeyCode::Backspace => {
                self.error = None;
                match self.field {
                    Field::Name => {
                        self.name.pop();
                    }
                    Field::Path => {
                        self.path.pop();
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
                    Field::Path => self.path.push(' '),
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
                    Field::Path => self.path.push(c),
                    Field::Args => self.args.push(c),
                    _ => {}
                }
                ModalResult::Consumed
            }
            _ => ModalResult::Consumed,
        }
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
                    "    tab next · esc cancel · enter create",
                    Style::default().fg(MUTED).bg(BG),
                ),
            ]),
            Line::from(""),
            label_line("name", self.field == Field::Name),
            input_line(&self.name, self.field == Field::Name, inner.width),
            Line::from(""),
            label_line("path", self.field == Field::Path),
            input_line(&self.path, self.field == Field::Path, inner.width),
            Line::from(""),
            label_line("agent", self.field == Field::Agent),
            agent_line(self.agent_idx, self.field == Field::Agent),
            Line::from(""),
            label_line("args (optional)", self.field == Field::Args),
            input_line(&self.args, self.field == Field::Args, inner.width),
        ];

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
        let mut m = NewSessionModal::new();
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
        let mut m = NewSessionModal::new();
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
        let mut m = NewSessionModal::new();
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
        let mut m = NewSessionModal::new();
        m.field = Field::ClaudeSkipPerm;
        assert!(!m.claude.skip_permissions);
        m.handle(key(KeyCode::Char(' ')));
        assert!(m.claude.skip_permissions);
        m.handle(key(KeyCode::Char(' ')));
        assert!(!m.claude.skip_permissions);
    }

    #[test]
    fn left_right_cycles_claude_session_mode() {
        let mut m = NewSessionModal::new();
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
        let mut m = NewSessionModal::new();
        m.agent_idx = 1;
        m.field = Field::CodexYolo;
        assert!(!m.codex.yolo);
        m.handle(key(KeyCode::Char(' ')));
        assert!(m.codex.yolo);
    }

    #[test]
    fn submit_spec_carries_claude_options() {
        let mut m = NewSessionModal::new();
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
        let mut m = NewSessionModal::new();
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
        let mut m = NewSessionModal::new();
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
        let mut m = NewSessionModal::new();
        let r = m.handle(key(KeyCode::Enter));
        assert!(matches!(r, ModalResult::Consumed));
        assert!(m.error.is_some());
    }

    #[test]
    fn enter_with_valid_data_closes_with_command() {
        let mut m = NewSessionModal::new();
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
        let mut m = NewSessionModal::new();
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
    fn name_with_whitespace_errors() {
        let mut m = NewSessionModal::new();
        for c in "bad name".chars() {
            m.handle(key(KeyCode::Char(c)));
        }
        let r = m.handle(key(KeyCode::Enter));
        assert!(matches!(r, ModalResult::Consumed));
        assert!(m.error.as_deref().unwrap().contains("whitespace"));
    }

    #[test]
    fn esc_closes_without_command() {
        let mut m = NewSessionModal::new();
        let r = m.handle(key(KeyCode::Esc));
        assert!(matches!(r, ModalResult::Close(None)));
    }
}
