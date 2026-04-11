//! Generic yes/no confirmation modal. Takes a message and a Command
//! that fires if the user confirms (Enter or 'y'). Esc or 'n' cancels.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use ratatui::Frame;

use crate::events::Command;
use crate::ui::Theme;

use super::{center_rect, Modal, ModalResult};

const MODAL_WIDTH: u16 = 54;
const MODAL_HEIGHT: u16 = 9;

pub struct ConfirmModal {
    title: String,
    message: String,
    /// Wrapped in Option so we can `.take()` it on close — `Command`
    /// isn't Clone and we need to move it out of `&mut self`.
    on_yes: Option<Command>,
    /// If true, the accent color shifts to red to signal a destructive
    /// action (kill, delete).
    destructive: bool,
}

impl ConfirmModal {
    pub fn new(title: impl Into<String>, message: impl Into<String>, on_yes: Command) -> Self {
        Self {
            title: title.into(),
            message: message.into(),
            on_yes: Some(on_yes),
            destructive: false,
        }
    }

    pub fn destructive(mut self) -> Self {
        self.destructive = true;
        self
    }
}

impl Modal for ConfirmModal {
    fn id(&self) -> &'static str {
        "confirm"
    }

    fn handle(&mut self, key: KeyEvent) -> ModalResult {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return ModalResult::Close(None);
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => ModalResult::Close(None),
            KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                ModalResult::Close(self.on_yes.take())
            }
            _ => ModalResult::Consumed,
        }
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let rect = center_rect(area, MODAL_WIDTH, MODAL_HEIGHT);
        let body_bg = theme.panel_alt;
        let buf = frame.buffer_mut();

        if rect.x + rect.width < area.x + area.width && rect.y + rect.height < area.y + area.height
        {
            let shadow = Rect::new(rect.x + 1, rect.y + 1, rect.width, rect.height);
            let style = Style::default().bg(theme.shadow);
            for y in shadow.top()..shadow.bottom() {
                for x in shadow.left()..shadow.right() {
                    buf[(x, y)].set_style(style);
                }
            }
        }

        let body_style = Style::default().bg(body_bg);
        for y in rect.top()..rect.bottom() {
            for x in rect.left()..rect.right() {
                let cell = &mut buf[(x, y)];
                cell.set_char(' ');
                cell.set_style(body_style);
            }
        }

        let accent_color = if self.destructive {
            theme.status_error
        } else {
            theme.accent
        };
        let accent_style = Style::default().bg(accent_color);
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
            .fg(if self.destructive {
                theme.status_error
            } else {
                theme.text
            })
            .bg(body_bg)
            .add_modifier(Modifier::BOLD);

        let lines: Vec<Line<'static>> = vec![
            Line::from(Span::styled(self.title.clone(), title_style)),
            Line::from(""),
            Line::from(Span::styled(
                self.message.clone(),
                Style::default().fg(theme.text).bg(body_bg),
            )),
            Line::from(""),
            Line::from(Span::styled(
                " enter / y · confirm      esc / n · cancel",
                Style::default().fg(theme.text_muted).bg(body_bg),
            )),
        ];

        Paragraph::new(lines)
            .style(Style::default().bg(body_bg))
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
    fn enter_closes_with_command() {
        let mut m = ConfirmModal::new("Kill?", "Are you sure?", Command::KillSession("foo".into()));
        match m.handle(key(KeyCode::Enter)) {
            ModalResult::Close(Some(Command::KillSession(name))) => assert_eq!(name, "foo"),
            _ => panic!("expected Close with KillSession"),
        }
    }

    #[test]
    fn y_also_confirms() {
        let mut m = ConfirmModal::new("", "", Command::KillSession("bar".into()));
        match m.handle(key(KeyCode::Char('y'))) {
            ModalResult::Close(Some(Command::KillSession(name))) => assert_eq!(name, "bar"),
            _ => panic!("expected Close on y"),
        }
    }

    #[test]
    fn esc_cancels_without_command() {
        let mut m = ConfirmModal::new("", "", Command::KillSession("x".into()));
        assert!(matches!(
            m.handle(key(KeyCode::Esc)),
            ModalResult::Close(None)
        ));
    }

    #[test]
    fn n_also_cancels() {
        let mut m = ConfirmModal::new("", "", Command::KillSession("x".into()));
        assert!(matches!(
            m.handle(key(KeyCode::Char('n'))),
            ModalResult::Close(None)
        ));
    }

    #[test]
    fn other_keys_consumed() {
        let mut m = ConfirmModal::new("", "", Command::KillSession("x".into()));
        assert!(matches!(
            m.handle(key(KeyCode::Char('z'))),
            ModalResult::Consumed
        ));
    }
}
