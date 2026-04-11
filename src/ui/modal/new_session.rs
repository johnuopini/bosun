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

use crate::events::{Command, SessionSpec};

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
const MODAL_HEIGHT: u16 = 18;

// --- Agent dropdown --------------------------------------------------

pub const AGENTS: &[&str] = &["claude", "codex", "terminal"];

// --- Modal state -----------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Name,
    Path,
    Agent,
    Args,
}

impl Field {
    fn next(self) -> Self {
        match self {
            Field::Name => Field::Path,
            Field::Path => Field::Agent,
            Field::Agent => Field::Args,
            Field::Args => Field::Name,
        }
    }
    fn prev(self) -> Self {
        match self {
            Field::Name => Field::Args,
            Field::Path => Field::Name,
            Field::Agent => Field::Path,
            Field::Args => Field::Agent,
        }
    }
}

pub struct NewSessionModal {
    name: String,
    path: String,
    agent_idx: usize,
    args: String,
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
            field: Field::Name,
            error: None,
        }
    }

    fn agent(&self) -> &'static str {
        AGENTS[self.agent_idx]
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
                self.field = self.field.next();
                ModalResult::Consumed
            }
            KeyCode::BackTab => {
                self.field = self.field.prev();
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
                if self.field == Field::Agent {
                    self.agent_idx = (self.agent_idx + AGENTS.len() - 1) % AGENTS.len();
                }
                ModalResult::Consumed
            }
            KeyCode::Right => {
                if self.field == Field::Agent {
                    self.agent_idx = (self.agent_idx + 1) % AGENTS.len();
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
                    Field::Agent => {}
                }
                ModalResult::Consumed
            }
            KeyCode::Char(c) => {
                self.error = None;
                match self.field {
                    Field::Name => self.name.push(c),
                    Field::Path => self.path.push(c),
                    Field::Args => self.args.push(c),
                    Field::Agent => {
                        if c == ' ' {
                            self.agent_idx = (self.agent_idx + 1) % AGENTS.len();
                        }
                    }
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
                    "    tab next · shift-tab prev · esc cancel · enter create",
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
    fn tab_cycles_fields() {
        let mut m = NewSessionModal::new();
        assert_eq!(m.field, Field::Name);
        m.handle(key(KeyCode::Tab));
        assert_eq!(m.field, Field::Path);
        m.handle(key(KeyCode::Tab));
        assert_eq!(m.field, Field::Agent);
        m.handle(key(KeyCode::Tab));
        assert_eq!(m.field, Field::Args);
        m.handle(key(KeyCode::Tab));
        assert_eq!(m.field, Field::Name);
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
