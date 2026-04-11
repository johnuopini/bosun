//! Rename the pretty display name of a session. Single text field,
//! pre-filled with the current display name. Submit with Enter,
//! cancel with Esc. Only updates `@bosun_display`; the internal tmux
//! session name never changes.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use ratatui::Frame;

use crate::events::Command;

use super::{center_rect, Modal, ModalResult};

const BG: Color = Color::Rgb(19, 23, 34);
const ACCENT: Color = Color::Rgb(124, 92, 255);
const TEXT: Color = Color::Rgb(230, 233, 239);
const MUTED: Color = Color::Rgb(124, 132, 149);
const FIELD_BG: Color = Color::Rgb(30, 36, 51);
const SHADOW: Color = Color::Rgb(5, 7, 11);
const ERROR: Color = Color::Rgb(255, 93, 107);

const MODAL_WIDTH: u16 = 54;
const MODAL_HEIGHT: u16 = 10;

pub struct RenameModal {
    internal: String,
    new_display: String,
    error: Option<String>,
}

impl RenameModal {
    pub fn new(internal: impl Into<String>, current_display: impl Into<String>) -> Self {
        Self {
            internal: internal.into(),
            new_display: current_display.into(),
            error: None,
        }
    }
}

impl Modal for RenameModal {
    fn id(&self) -> &'static str {
        "rename"
    }

    fn handle(&mut self, key: KeyEvent) -> ModalResult {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return ModalResult::Close(None);
        }
        match key.code {
            KeyCode::Esc => ModalResult::Close(None),
            KeyCode::Enter => {
                let trimmed = self.new_display.trim();
                if trimmed.is_empty() {
                    self.error = Some("name is required".to_string());
                    return ModalResult::Consumed;
                }
                if !trimmed.chars().any(|c| c.is_alphanumeric()) {
                    self.error = Some("name must contain at least one letter or digit".to_string());
                    return ModalResult::Consumed;
                }
                ModalResult::Close(Some(Command::RenameSession {
                    internal: self.internal.clone(),
                    new_display: trimmed.to_string(),
                }))
            }
            KeyCode::Backspace => {
                self.error = None;
                self.new_display.pop();
                ModalResult::Consumed
            }
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.error = None;
                self.new_display.push(c);
                ModalResult::Consumed
            }
            _ => ModalResult::Consumed,
        }
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let rect = center_rect(area, MODAL_WIDTH, MODAL_HEIGHT);
        let buf = frame.buffer_mut();

        if rect.x + rect.width < area.x + area.width && rect.y + rect.height < area.y + area.height
        {
            let shadow = Rect::new(rect.x + 1, rect.y + 1, rect.width, rect.height);
            let style = Style::default().bg(SHADOW);
            for y in shadow.top()..shadow.bottom() {
                for x in shadow.left()..shadow.right() {
                    buf[(x, y)].set_style(style);
                }
            }
        }

        let body_style = Style::default().bg(BG);
        for y in rect.top()..rect.bottom() {
            for x in rect.left()..rect.right() {
                let cell = &mut buf[(x, y)];
                cell.set_char(' ');
                cell.set_style(body_style);
            }
        }

        let accent_style = Style::default().bg(ACCENT);
        for y in rect.top()..rect.bottom() {
            let cell = &mut buf[(rect.left(), y)];
            cell.set_char(' ');
            cell.set_style(accent_style);
        }

        let inner = Rect::new(
            rect.x + 3,
            rect.y + 1,
            rect.width.saturating_sub(4),
            rect.height.saturating_sub(2),
        );

        let title_style = Style::default()
            .fg(TEXT)
            .bg(BG)
            .add_modifier(Modifier::BOLD);

        let field_width = (inner.width as usize).saturating_sub(2);
        let mut value_padded = format!(" {}▎", self.new_display);
        while value_padded.chars().count() < field_width {
            value_padded.push(' ');
        }

        let mut lines: Vec<Line<'static>> = vec![
            Line::from(vec![
                Span::styled("Rename session", title_style),
                Span::styled(
                    "     esc · cancel     enter · save",
                    Style::default().fg(MUTED).bg(BG),
                ),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                " new display name",
                Style::default()
                    .fg(ACCENT)
                    .bg(BG)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                value_padded,
                Style::default().fg(TEXT).bg(FIELD_BG),
            )),
        ];

        if let Some(e) = &self.error {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!(" ! {}", e),
                Style::default().fg(ERROR).bg(BG),
            )));
        }

        Paragraph::new(lines)
            .style(Style::default().bg(BG))
            .render(inner, frame.buffer_mut());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn initial_new_display_is_prefilled_from_current() {
        let m = RenameModal::new("bosun-abc", "Old Name");
        assert_eq!(m.new_display, "Old Name");
    }

    #[test]
    fn enter_submits_rename_command() {
        let mut m = RenameModal::new("bosun-abc", "Old Name");
        m.new_display.clear();
        for c in "New Name".chars() {
            m.handle(key(KeyCode::Char(c)));
        }
        match m.handle(key(KeyCode::Enter)) {
            ModalResult::Close(Some(Command::RenameSession {
                internal,
                new_display,
            })) => {
                assert_eq!(internal, "bosun-abc");
                assert_eq!(new_display, "New Name");
            }
            _ => panic!("expected Close with RenameSession"),
        }
    }

    #[test]
    fn empty_name_errors_instead_of_submitting() {
        let mut m = RenameModal::new("bosun-abc", "");
        assert!(matches!(
            m.handle(key(KeyCode::Enter)),
            ModalResult::Consumed
        ));
        assert!(m.error.is_some());
    }

    #[test]
    fn all_symbols_errors() {
        let mut m = RenameModal::new("bosun-abc", "!!!");
        assert!(matches!(
            m.handle(key(KeyCode::Enter)),
            ModalResult::Consumed
        ));
        assert!(m.error.as_deref().unwrap().contains("letter"));
    }

    #[test]
    fn backspace_removes_char() {
        let mut m = RenameModal::new("bosun-abc", "Abc");
        m.handle(key(KeyCode::Backspace));
        assert_eq!(m.new_display, "Ab");
    }

    #[test]
    fn esc_cancels() {
        let mut m = RenameModal::new("bosun-abc", "Abc");
        assert!(matches!(
            m.handle(key(KeyCode::Esc)),
            ModalResult::Close(None)
        ));
    }
}
