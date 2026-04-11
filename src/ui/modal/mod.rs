//! Modal dialog infrastructure.
//!
//! Phase 3 ships a single-modal stack that can render + handle input
//! for one modal at a time. Phase 4 expands to a proper stack so the
//! fuzzy-search modal can layer over the new-session modal etc.
//!
//! Design rules:
//!   * Modals own their own state (form fields, selection, etc).
//!   * Modals render pure — take `&self` + a Frame + Rect.
//!   * Input handling returns a `ModalResult` describing what the
//!     app loop should do: keep the modal open, pass the key through
//!     to the main list, or close the modal (optionally emitting a
//!     `Command` to the tmux actor).

pub mod new_session;

use crossterm::event::KeyEvent;
use ratatui::layout::Rect;
use ratatui::Frame;

use crate::events::Command;

/// Result of dispatching a key event to a modal.
pub enum ModalResult {
    /// Modal handled the key; keep it open and don't propagate.
    Consumed,
    /// Modal didn't care about this key; let the caller route it
    /// elsewhere (main list, etc). Rare — most modals want to eat
    /// every key while they're open so the user can't navigate the
    /// background while typing.
    #[allow(dead_code)]
    PassThrough,
    /// Close the modal. If `Some(cmd)`, the command is emitted to
    /// the tmux actor after the modal is popped.
    Close(Option<Command>),
}

pub trait Modal: Send {
    /// Stable identifier for the modal kind. Useful for tests and
    /// for de-duplicating repeat opens ("don't push another
    /// new_session modal if one is already on top").
    fn id(&self) -> &'static str;
    fn render(&self, frame: &mut Frame<'_>, area: Rect);
    fn handle(&mut self, key: KeyEvent) -> ModalResult;
}

#[derive(Default)]
pub struct ModalStack {
    stack: Vec<Box<dyn Modal>>,
}

impl std::fmt::Debug for ModalStack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModalStack")
            .field("depth", &self.stack.len())
            .field(
                "top",
                &self.stack.last().map(|m| m.id()).unwrap_or("<empty>"),
            )
            .finish()
    }
}

impl ModalStack {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.stack.is_empty()
    }

    pub fn len(&self) -> usize {
        self.stack.len()
    }

    pub fn push(&mut self, modal: Box<dyn Modal>) {
        self.stack.push(modal);
    }

    pub fn pop(&mut self) -> Option<Box<dyn Modal>> {
        self.stack.pop()
    }

    pub fn top_id(&self) -> Option<&'static str> {
        self.stack.last().map(|m| m.id())
    }

    pub fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        // Dim the background, then render the top modal (plus any
        // stacked ones once we support that). Bottom-up so the top
        // modal paints last.
        if self.stack.is_empty() {
            return;
        }
        dim_background(frame, area);
        for modal in &self.stack {
            modal.render(frame, area);
        }
    }

    pub fn handle(&mut self, key: KeyEvent) -> ModalResult {
        match self.stack.last_mut() {
            Some(top) => top.handle(key),
            None => ModalResult::PassThrough,
        }
    }
}

fn dim_background(frame: &mut Frame<'_>, area: Rect) {
    use ratatui::style::Color;
    let buf = frame.buffer_mut();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let cell = &mut buf[(x, y)];
            // Drop foreground brightness and wash over the existing bg
            // with a muted gray. This preserves the underlying glyph
            // layout so the modal looks like it's floating above a
            // real UI rather than painted onto a blank rectangle.
            cell.set_fg(Color::Rgb(60, 66, 84));
            cell.set_bg(Color::Rgb(11, 13, 18));
        }
    }
}

/// Center a `width`x`height` rect inside `outer`, clamping to outer
/// bounds. Used by modals to self-center.
pub fn center_rect(outer: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(outer.width);
    let h = height.min(outer.height);
    let x = outer.x + outer.width.saturating_sub(w) / 2;
    let y = outer.y + outer.height.saturating_sub(h) / 2;
    Rect::new(x, y, w, h)
}
