//! Central app state + event loop.
//!
//! Single-writer invariant: `AppState` is owned by the one task that runs
//! [`App::run`]. Nothing else mutates it. Everything else sends messages.

use std::sync::Arc;

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use ratatui::layout::Rect;
use ratatui::Terminal;
use tokio::sync::mpsc;

use crate::actors::{input_actor, tmux_actor};
use crate::config::Config;
use crate::error::{BosunError, Result};
use crate::events::{AppMsg, Command};
use crate::store::Store;
use crate::tmux::attach::attach_with_ctrl_q_detach;
use crate::tmux::session::SessionView;
use crate::tmux::TmuxClient;
use crate::ui;
use crate::ui::layout;
use crate::ui::modal::confirm::ConfirmModal;
use crate::ui::modal::new_session::NewSessionModal;
use crate::ui::modal::rename::RenameModal;
use crate::ui::modal::theme::ThemeModal;
use crate::ui::modal::{ModalStack, StackDispatch};
use crate::ui::Theme;

fn term_err<E: std::fmt::Display>(e: E) -> BosunError {
    BosunError::Io(std::io::Error::other(e.to_string()))
}

/// Set the terminal window/tab title via the OSC 0 escape sequence.
/// Works in iTerm2, Terminal.app, Alacritty, kitty, WezTerm, etc.
fn set_terminal_title(title: &str) {
    // OSC 0 ; <title> BEL
    print!("\x1b]0;{title}\x07");
}

/// Everything the UI renders from. Pure data; no locks.
#[derive(Debug, Default)]
pub struct AppState {
    pub sessions: Vec<SessionView>,
    pub selected: usize,
    pub warning: Option<String>,
    pub quit: bool,
    /// Set when the user hit Enter on a session — the event loop drains
    /// this on the next turn, tears down the terminal, and performs the
    /// blocking `tmux attach` on the controlling tty.
    pub pending_attach: Option<String>,
    /// Last session name we told the tmux actor to prioritize for preview
    /// capture. Used to debounce FocusPreview commands.
    pub focus_sent: Option<String>,
    /// Stack of open modals. `ui::draw` renders them over the main list
    /// on every frame; `handle_key` routes key events to the top modal
    /// first.
    pub modals: ModalStack,
    /// Internal signal from the reducer to the app loop: "I want a
    /// modal opened". The app loop reads this after each `apply()`
    /// and pushes the modal (with store-loaded recents etc) since
    /// `AppState` doesn't hold the store itself.
    pub pending_modal: Option<ModalRequest>,
    /// Cached terminal size, updated on every `AppMsg::Resize` and
    /// on the initial sync in `App::run`. Used by mouse handling to
    /// map a column click back to the current divider position
    /// (`layout::compute` needs the area to resolve the split).
    pub term_size: (u16, u16),
    /// User's preferred x-column for the divider between session
    /// list and preview. `None` means "use the default 38% split".
    /// Updated live while the user drags the divider with the mouse.
    pub divider_x: Option<u16>,
    /// True while the user is mid-drag on the divider (mouse button
    /// held down after a Down on the divider column). Render uses
    /// this to highlight the divider glyph.
    pub dragging_divider: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModalRequest {
    NewSession,
    /// Open the theme picker. The app loop fills in the list of
    /// currently-available themes (built-ins + user dir) before
    /// constructing `ThemeModal`, so `AppState::apply` doesn't need
    /// to touch the filesystem.
    Theme,
}

impl AppState {
    fn clamp_selection(&mut self) {
        if self.sessions.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.sessions.len() {
            self.selected = self.sessions.len() - 1;
        }
    }

    /// Pure reducer. Returns a list of Commands the caller should dispatch.
    pub fn apply(&mut self, msg: AppMsg) -> Vec<Command> {
        let mut out = Vec::new();
        match msg {
            AppMsg::SessionsRefreshed {
                sessions,
                select_after,
            } => {
                // Preserve selection by name across refreshes — unless
                // `select_after` is Some, in which case the refresh was
                // triggered by a create and the app should jump to the
                // newly-created session.
                let prior_name = self
                    .sessions
                    .get(self.selected)
                    .map(|v| v.name().to_string());
                self.sessions = sessions;
                if let Some(target) = select_after {
                    if let Some(idx) = self.sessions.iter().position(|v| v.name() == target) {
                        self.selected = idx;
                    }
                } else if let Some(name) = prior_name {
                    if let Some(idx) = self.sessions.iter().position(|v| v.name() == name) {
                        self.selected = idx;
                    }
                }
                self.clamp_selection();
                // A successful refresh clears any stale list warning.
                if let Some(w) = &self.warning {
                    if w.starts_with("list:") {
                        self.warning = None;
                    }
                }
                // Make sure the actor has the right focused session.
                self.sync_focus(&mut out);
            }
            AppMsg::Key(k) => {
                // Route through open modals first. Most modals consume
                // everything they see so typing in a text field doesn't
                // leak into the main list.
                if !self.modals.is_empty() {
                    match self.modals.dispatch(k) {
                        StackDispatch::Consumed => {}
                        StackDispatch::PassThrough => self.handle_key(k, &mut out),
                        StackDispatch::Closed(cmd) => {
                            if let Some(c) = cmd {
                                out.push(c);
                            }
                        }
                        StackDispatch::Emit(cmd) => out.push(cmd),
                    }
                } else {
                    self.handle_key(k, &mut out);
                }
                self.sync_focus(&mut out);
            }
            AppMsg::Mouse(m) => {
                // Mouse events are only used by the draggable divider
                // between the session list and preview pane. No modal
                // dispatching — modals don't react to mouse yet.
                self.handle_mouse(m, &mut out);
            }
            AppMsg::Resize(w, h) => {
                // Keep a cached terminal size for mouse handling —
                // `handle_mouse` needs the current area to compute
                // the divider column, and it can't ask the terminal
                // directly from inside a pure reducer.
                self.term_size = (w, h);
                // ratatui auto-redraws next frame, no command to emit.
            }
            AppMsg::Warn(w) => self.warning = Some(w),
            AppMsg::Fatal(w) => {
                self.warning = Some(w);
                self.quit = true;
            }
            AppMsg::Shutdown => self.quit = true,
            AppMsg::Resume => { /* redraw happens unconditionally below */ }
            AppMsg::AttachStarted { .. } | AppMsg::AttachEnded { .. } => {
                // Phase 1: attach is done inline; these arms are for future use.
            }
        }
        out
    }

    fn sync_focus(&mut self, out: &mut Vec<Command>) {
        let current = self
            .sessions
            .get(self.selected)
            .map(|v| v.name().to_string());
        if current != self.focus_sent {
            if let Some(name) = &current {
                out.push(Command::FocusPreview { name: name.clone() });
            }
            self.focus_sent = current;
        }
    }

    /// The preview buffer for the currently selected session, if any.
    pub fn selected_preview(&self) -> Option<&[u8]> {
        self.sessions
            .get(self.selected)
            .and_then(|v| v.preview.as_deref())
    }

    fn handle_key(&mut self, k: KeyEvent, out: &mut Vec<Command>) {
        // Only react to Press events. crossterm reports Repeat and Release too.
        if k.kind != KeyEventKind::Press && k.kind != KeyEventKind::Repeat {
            return;
        }
        // Explicitly never consume Ctrl-Z so the terminal can deliver SIGTSTP.
        if k.code == KeyCode::Char('z') && k.modifiers.contains(KeyModifiers::CONTROL) {
            return;
        }

        match (k.code, k.modifiers) {
            (KeyCode::Char('q'), KeyModifiers::NONE)
            | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                self.quit = true;
            }
            (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                if !self.sessions.is_empty() {
                    self.selected = (self.selected + 1).min(self.sessions.len() - 1);
                }
            }
            (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                self.selected = self.selected.saturating_sub(1);
            }
            (KeyCode::Enter, _) => {
                if let Some(s) = self.sessions.get(self.selected) {
                    self.pending_attach = Some(s.name().to_string());
                }
            }
            (KeyCode::Char('r'), KeyModifiers::CONTROL) => {
                // Manual refresh. Regular refresh happens on every 1s
                // tick, but Ctrl-R is here as an escape hatch if the
                // user wants instant.
                out.push(Command::ListNow);
            }
            (KeyCode::Char('r'), KeyModifiers::NONE) => {
                if let Some(sel) = self.sessions.get(self.selected) {
                    let internal = sel.name().to_string();
                    let current_display = sel.display().to_string();
                    self.modals
                        .push(Box::new(RenameModal::new(internal, current_display)));
                }
            }
            (KeyCode::Char('d'), KeyModifiers::NONE) => {
                if let Some(sel) = self.sessions.get(self.selected) {
                    let internal = sel.name().to_string();
                    let display = sel.display().to_string();
                    let title = "Kill session?";
                    let msg = format!("This will kill '{}' and its pane.", display);
                    self.modals.push(Box::new(
                        ConfirmModal::new(title, msg, Command::KillSession(internal)).destructive(),
                    ));
                }
            }
            (KeyCode::Char('R'), _) => {
                // Shift-R restarts: kill + recreate the selected
                // session using the metadata persisted to @bosun_*
                // tmux options at create time.
                if let Some(sel) = self.sessions.get(self.selected) {
                    let internal = sel.name().to_string();
                    let display = sel.display().to_string();
                    let title = "Restart session?";
                    let msg = format!(
                        "This kills and recreates '{}' with the same config.",
                        display
                    );
                    self.modals.push(Box::new(ConfirmModal::new(
                        title,
                        msg,
                        Command::RestartSession(internal),
                    )));
                }
            }
            (KeyCode::Char('n'), KeyModifiers::NONE) => {
                // We can't push the modal directly here because
                // AppState doesn't hold the store — signal the app
                // loop via pending_modal and it'll load recents +
                // push.
                if self.modals.top_id() != Some("new_session") {
                    self.pending_modal = Some(ModalRequest::NewSession);
                }
            }
            (KeyCode::Char('t'), KeyModifiers::NONE) => {
                // Same reason as NewSession: AppState can't read the
                // filesystem to build the theme list, so we signal
                // the app loop to do it.
                if self.modals.top_id() != Some("theme") {
                    self.pending_modal = Some(ModalRequest::Theme);
                }
            }
            _ => {}
        }
    }

    /// Map a mouse event onto the draggable divider between the
    /// session list and preview pane.
    ///
    /// - `Down(Left)` on the divider column starts a drag.
    /// - `Drag(Left)` while `dragging_divider` updates `divider_x`
    ///   to the new column; `layout::compute` clamps it to sane
    ///   min-widths on the next render.
    /// - `Up(Left)` clears the drag flag regardless of location —
    ///   releasing the button anywhere ends the gesture.
    ///
    /// Non-left-button events and any event while `term_size` is
    /// unset (pre-first-draw) are ignored.
    fn handle_mouse(&mut self, m: MouseEvent, out: &mut Vec<Command>) {
        if self.term_size.0 == 0 {
            return;
        }
        let area = Rect::new(0, 0, self.term_size.0, self.term_size.1);
        let layouts = layout::compute(area, self.divider_x);

        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if layout::is_divider_col(&layouts, m.column) {
                    self.dragging_divider = true;
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.dragging_divider => {
                // Raw column — `layout::compute` clamps it to the
                // allowed range (MIN_LIST_WIDTH..body - MIN_PREVIEW_WIDTH - 1).
                self.divider_x = Some(m.column);
            }
            MouseEventKind::Up(MouseButton::Left) if self.dragging_divider => {
                self.dragging_divider = false;
                out.push(Command::SaveDivider(self.divider_x));
            }
            MouseEventKind::Up(MouseButton::Left) => {}
            _ => {}
        }
    }
}

pub struct App {
    pub state: AppState,
    pub cmd_tx: mpsc::UnboundedSender<Command>,
    pub evt_rx: mpsc::UnboundedReceiver<AppMsg>,
    pub evt_tx: mpsc::UnboundedSender<AppMsg>,
    pub socket: Option<String>,
    pub store: Arc<Store>,
    /// Active theme. Resolved once at startup from the config's
    /// theme name; render code reads it via `ui::draw`.
    pub theme: Theme,
    /// Handle to the running input actor. Held here so we can stop it
    /// before handing stdin to tmux during an attach — otherwise the
    /// actor's crossterm reader races tmux for each stdin byte, and
    /// the user ends up needing to press Ctrl-Q twice because the
    /// first press is read by Bosun and silently dropped.
    input_handle: Option<input_actor::Handle>,
}

impl App {
    pub fn new(
        client: Arc<dyn TmuxClient>,
        socket: Option<String>,
        config: Config,
        store: Arc<Store>,
    ) -> Self {
        // Unbounded channels. Rationale: every flavor of freeze we've
        // hit has been a variant of channel-backpressure deadlock —
        // producer parks on a full channel while consumer is blocked
        // on something else, and the two form a circular wait. The
        // producer rates in bosun are trivial (1Hz poller, human
        // typing, occasional tmux refresh fan-out) and AppMsg/Command
        // are small, so the memory pressure from "unbounded in
        // theory" is unbounded in the same way a vec of ints is — a
        // few MB worst case, trivially paid. Taking back-pressure
        // out of the picture makes the runtime deadlock-free by
        // construction.
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Command>();
        let (evt_tx, evt_rx) = mpsc::unbounded_channel::<AppMsg>();

        let theme = Theme::load(&config.theme, crate::config::user_themes_dir().as_deref());

        tmux_actor::spawn(
            client.clone(),
            socket.clone(),
            config.clone(),
            store.clone(),
            cmd_rx,
            evt_tx.clone(),
        );
        let input_handle = input_actor::spawn(evt_tx.clone());

        let mut state = AppState::default();
        state.divider_x = config.divider_x;

        Self {
            state,
            cmd_tx,
            evt_rx,
            evt_tx,
            socket,
            store,
            theme,
            input_handle: Some(input_handle),
        }
    }

    pub async fn run<B: ratatui::backend::Backend + std::io::Write>(
        &mut self,
        terminal: &mut Terminal<B>,
    ) -> Result<()> {
        set_terminal_title("bosun");

        // Initial refresh kick. Unbounded `send` is sync and can only
        // fail if the receiver has been dropped — meaning the tmux
        // actor has already exited, at which point there's nothing
        // to do but let the event loop unwind naturally.
        let _ = self.cmd_tx.send(Command::ListNow);

        // Seed the cached term_size before the first draw. Mouse
        // handling (divider drag) needs it to compute the current
        // divider column without calling back into ratatui.
        if let Ok(size) = terminal.size() {
            self.state.term_size = (size.width, size.height);
        }

        terminal
            .draw(|f| ui::draw(f, &self.state, &self.theme))
            .map_err(term_err)?;

        while !self.state.quit {
            let msg = match self.evt_rx.recv().await {
                Some(m) => m,
                None => break,
            };

            let cmds = self.state.apply(msg);
            for c in cmds {
                // Intercept SetTheme here — it's a pure UI action and
                // must not reach the tmux actor. `persist: true`
                // additionally writes the choice to config.toml so
                // it survives restart.
                match c {
                    Command::SetTheme { name, persist } => {
                        self.theme =
                            Theme::load(&name, crate::config::user_themes_dir().as_deref());
                        if persist {
                            if let Err(e) = crate::config::write_theme(&name) {
                                self.state.warning = Some(format!("theme: failed to save: {e}"));
                            }
                        }
                    }
                    Command::SaveDivider(x) => {
                        if let Err(e) = crate::config::write_divider_x(x) {
                            self.state.warning = Some(format!("divider: failed to save: {e}"));
                        }
                    }
                    other => {
                        // Sync send: unbounded, never blocks, never
                        // parks a task. The only failure is "tmux
                        // actor has exited" which we ignore — the
                        // event loop will unwind on the next recv.
                        let _ = self.cmd_tx.send(other);
                    }
                }
            }

            // Handle any modal-open requests from the reducer. This
            // is where we load store-backed data (recents) and
            // construct the actual modal.
            if let Some(req) = self.state.pending_modal.take() {
                match req {
                    ModalRequest::NewSession => {
                        let recents = self.store.list_recents(8).unwrap_or_default();
                        self.state
                            .modals
                            .push(Box::new(NewSessionModal::new(recents)));
                    }
                    ModalRequest::Theme => {
                        let names = Theme::available(crate::config::user_themes_dir().as_deref());
                        let original = self.theme.name.clone();
                        self.state
                            .modals
                            .push(Box::new(ThemeModal::new(names, original)));
                    }
                }
            }

            // If the reducer queued an attach, perform it now: tear down the
            // terminal, hand the tty to tmux, install/remove the Ctrl-Q binding.
            if let Some(name) = self.state.pending_attach.take() {
                // Stop the input actor so tmux has stdin to itself. Without
                // this, Bosun's crossterm reader and tmux race for each key
                // byte and the user has to press Ctrl-Q twice to detach.
                // `shutdown().await` sets an atomic flag and waits for the
                // blocking reader task to notice on its next ~100ms poll
                // cycle — no tokio cancellation involved, so there's no
                // way for the reader thread to end up stranded on a
                // stuck channel (the freeze that prompted this rewrite).
                if let Some(h) = self.input_handle.take() {
                    h.shutdown().await;
                }

                // Update the terminal title to reflect the attached session.
                let display = self
                    .state
                    .sessions
                    .iter()
                    .find(|s| s.name() == name)
                    .map(|s| s.display().to_string())
                    .unwrap_or_else(|| name.clone());
                set_terminal_title(&format!("bosun — {display}"));

                let attach_result = self.perform_attach(terminal, &name);

                set_terminal_title("bosun");

                // Respawn the input actor now that the terminal is back.
                self.input_handle = Some(input_actor::spawn(self.evt_tx.clone()));

                attach_result?;
                // After return, kick a refresh — the session may have been killed.
                let _ = self.cmd_tx.send(Command::ListNow);
            }

            terminal
                .draw(|f| ui::draw(f, &self.state, &self.theme))
                .map_err(term_err)?;
        }

        // Shut down the input actor cleanly before returning. Its
        // blocking task polls crossterm with a 100ms timeout between
        // shutdown-flag checks — without this explicit shutdown, the
        // thread keeps spinning after main exits and the tokio
        // runtime's drop blocks waiting for it (blocking threads
        // can't be cancelled, only cooperatively signalled). That
        // manifests as "bosun hangs for a few seconds after pressing
        // q before returning to the shell prompt".
        if let Some(h) = self.input_handle.take() {
            h.shutdown().await;
        }

        // Clear the terminal title so the shell can set its own.
        set_terminal_title("");

        Ok(())
    }

    fn perform_attach<B: ratatui::backend::Backend + std::io::Write>(
        &mut self,
        terminal: &mut Terminal<B>,
        name: &str,
    ) -> Result<()> {
        // 1. Tear down ratatui's grip on the terminal so tmux can own it.
        crossterm::terminal::disable_raw_mode().map_err(BosunError::Io)?;
        execute!(
            terminal.backend_mut(),
            crossterm::terminal::LeaveAlternateScreen,
            crossterm::event::DisableMouseCapture,
        )
        .map_err(BosunError::Io)?;

        // 2. Install binding + run attach (blocking).
        let result = attach_with_ctrl_q_detach(self.socket.as_deref(), name);

        // 3. Re-enter raw mode / alt screen / mouse capture
        //    regardless of attach result.
        crossterm::terminal::enable_raw_mode().map_err(BosunError::Io)?;
        execute!(
            terminal.backend_mut(),
            crossterm::terminal::EnterAlternateScreen,
            crossterm::event::EnableMouseCapture,
        )
        .map_err(BosunError::Io)?;
        terminal.clear().map_err(term_err)?;

        if let Err(e) = result {
            self.state.warning = Some(format!("attach: {}", e));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmux::detector::Status;
    use crate::tmux::TmuxSession;
    use std::time::SystemTime;

    fn ses(name: &str) -> SessionView {
        SessionView::new(
            TmuxSession {
                name: name.into(),
                display_name: None,
                windows: 1,
                attached: false,
                created: Some(SystemTime::now()),
                last_activity: Some(SystemTime::now()),
                current_path: None,
                agent: None,
                spec_path: None,
            },
            Status::Idle,
            None,
        )
    }

    fn state_with(sessions: Vec<SessionView>, selected: usize) -> AppState {
        AppState {
            sessions,
            selected,
            ..Default::default()
        }
    }

    fn refreshed(sessions: Vec<SessionView>) -> AppMsg {
        AppMsg::SessionsRefreshed {
            sessions,
            select_after: None,
        }
    }

    #[test]
    fn selection_clamps_after_refresh() {
        let mut s = state_with(vec![ses("a"), ses("b"), ses("c")], 2);
        s.apply(refreshed(vec![ses("a")]));
        assert_eq!(s.selected, 0);
    }

    #[test]
    fn selection_preserved_by_name() {
        let mut s = state_with(vec![ses("a"), ses("b"), ses("c")], 1);
        s.apply(refreshed(vec![ses("c"), ses("b"), ses("a")]));
        assert_eq!(s.selected, 1); // still "b"
        assert_eq!(s.sessions[s.selected].name(), "b");
    }

    #[test]
    fn select_after_jumps_to_new_session() {
        let mut s = state_with(vec![ses("a")], 0);
        s.apply(AppMsg::SessionsRefreshed {
            sessions: vec![ses("a"), ses("b")],
            select_after: Some("b".to_string()),
        });
        assert_eq!(s.selected, 1);
        assert_eq!(s.sessions[s.selected].name(), "b");
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn arrow_keys_navigate() {
        let mut s = state_with(vec![ses("a"), ses("b"), ses("c")], 0);
        s.apply(AppMsg::Key(key(KeyCode::Down)));
        assert_eq!(s.selected, 1);
        s.apply(AppMsg::Key(key(KeyCode::Down)));
        assert_eq!(s.selected, 2);
        s.apply(AppMsg::Key(key(KeyCode::Down)));
        assert_eq!(s.selected, 2); // clamped
        s.apply(AppMsg::Key(key(KeyCode::Up)));
        assert_eq!(s.selected, 1);
    }

    #[test]
    fn q_quits() {
        let mut s = AppState::default();
        s.apply(AppMsg::Key(key(KeyCode::Char('q'))));
        assert!(s.quit);
    }

    #[test]
    fn ctrl_z_is_not_consumed() {
        let mut s = state_with(vec![ses("a")], 0);
        let k = KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL);
        s.apply(AppMsg::Key(k));
        assert!(!s.quit);
        assert_eq!(s.selected, 0);
        assert!(s.pending_attach.is_none());
    }

    #[test]
    fn enter_queues_attach() {
        let mut s = state_with(vec![ses("main")], 0);
        s.apply(AppMsg::Key(key(KeyCode::Enter)));
        assert_eq!(s.pending_attach.as_deref(), Some("main"));
    }

    fn mouse(kind: MouseEventKind, col: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column: col,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// A state wide enough for the split view, with a fresh term_size
    /// set. The default 38% split at 120 cols puts the divider at
    /// column 45.
    fn wide_state() -> AppState {
        AppState {
            term_size: (120, 30),
            ..Default::default()
        }
    }

    #[test]
    fn mouse_down_on_default_divider_starts_drag() {
        let mut s = wide_state();
        s.apply(AppMsg::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            45, // matches 120 * 38% default
        )));
        assert!(s.dragging_divider);
    }

    #[test]
    fn mouse_down_off_divider_does_nothing() {
        let mut s = wide_state();
        s.apply(AppMsg::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            10,
        )));
        assert!(!s.dragging_divider);
        assert!(s.divider_x.is_none());
    }

    #[test]
    fn drag_updates_divider_x_while_dragging() {
        let mut s = wide_state();
        s.dragging_divider = true;
        s.apply(AppMsg::Mouse(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            70,
        )));
        assert_eq!(s.divider_x, Some(70));
    }

    #[test]
    fn drag_ignored_when_not_dragging() {
        let mut s = wide_state();
        s.apply(AppMsg::Mouse(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            70,
        )));
        assert!(s.divider_x.is_none());
    }

    #[test]
    fn mouse_up_ends_drag() {
        let mut s = wide_state();
        s.dragging_divider = true;
        s.apply(AppMsg::Mouse(mouse(
            MouseEventKind::Up(MouseButton::Left),
            70,
        )));
        assert!(!s.dragging_divider);
    }

    #[test]
    fn resize_updates_cached_term_size() {
        let mut s = AppState::default();
        s.apply(AppMsg::Resize(100, 30));
        assert_eq!(s.term_size, (100, 30));
    }

    #[test]
    fn divider_ignored_before_first_resize() {
        // Fresh state has term_size = (0, 0). Mouse events must
        // no-op rather than panic or guess a divider position.
        let mut s = AppState::default();
        s.apply(AppMsg::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            45,
        )));
        assert!(!s.dragging_divider);
    }
}
