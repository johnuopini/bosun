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
use crate::sidebar::{reconcile as reconcile_sidebar, SidebarEntry};
use crate::store::Store;
use crate::tmux::attach::attach_with_ctrl_q_detach;
use crate::tmux::session::SessionView;
use crate::tmux::TmuxClient;
use crate::ui;
use crate::ui::layout;
use crate::ui::modal::confirm::ConfirmModal;
use crate::ui::modal::new_session::NewSessionModal;
use crate::ui::modal::rename::RenameModal;
use crate::ui::modal::section::SectionModal;
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
    /// The ordered sidebar: section headers and session references
    /// interleaved. `selected` indexes into this list. Reconciled on
    /// every `SessionsRefreshed` (dead sessions dropped, new sessions
    /// appended). Persisted to `config.toml` via `Command::SaveSidebar`.
    pub sidebar_entries: Vec<SidebarEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModalRequest {
    NewSession,
    /// Open the theme picker. The app loop fills in the list of
    /// currently-available themes (built-ins + user dir) before
    /// constructing `ThemeModal`, so `AppState::apply` doesn't need
    /// to touch the filesystem.
    Theme,
    /// Open the section-name modal. `None` creates a new section;
    /// `Some { id, name }` renames an existing one.
    Section {
        editing: Option<(String, String)>,
    },
}

impl AppState {
    /// Emit a `SaveSidebar` command with the current entries. Called
    /// whenever the sidebar is mutated (reorder, add section, rename,
    /// delete).
    fn save_sidebar(&self, out: &mut Vec<Command>) {
        out.push(Command::SaveSidebar(self.sidebar_entries.clone()));
    }

    fn clamp_selection(&mut self) {
        if self.sidebar_entries.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.sidebar_entries.len() {
            self.selected = self.sidebar_entries.len() - 1;
        }
    }

    /// The entry under the cursor, if any.
    pub fn selected_entry(&self) -> Option<&SidebarEntry> {
        self.sidebar_entries.get(self.selected)
    }

    /// Look up the `SessionView` for the selected entry, if it's a session.
    pub fn selected_session(&self) -> Option<&SessionView> {
        let entry = self.selected_entry()?;
        let internal = match entry {
            SidebarEntry::Session { internal } => internal,
            SidebarEntry::Section { .. } => return None,
        };
        self.sessions.iter().find(|v| v.name() == internal)
    }

    /// The preview buffer for the currently selected session, if the
    /// cursor is on a session (not a section header).
    pub fn selected_preview(&self) -> Option<&[u8]> {
        self.selected_session().and_then(|v| v.preview.as_deref())
    }

    /// Look up the SessionView for a given internal name.
    pub fn session_by_name(&self, name: &str) -> Option<&SessionView> {
        self.sessions.iter().find(|v| v.name() == name)
    }

    /// Find the end index of the group starting at `header_idx` — i.e.
    /// one past the last entry that belongs to this section (next header
    /// or list end). `header_idx` must point at a `Section` entry.
    fn group_end(&self, header_idx: usize) -> usize {
        let mut i = header_idx + 1;
        while i < self.sidebar_entries.len() {
            if self.sidebar_entries[i].is_section() {
                break;
            }
            i += 1;
        }
        i
    }

    /// Pure reducer. Returns a list of Commands the caller should dispatch.
    pub fn apply(&mut self, msg: AppMsg) -> Vec<Command> {
        let mut out = Vec::new();
        match msg {
            AppMsg::SessionsRefreshed {
                sessions,
                select_after,
            } => {
                // Preserve selection by entry identity (section id or
                // session internal name) across refreshes — unless
                // `select_after` is Some, in which case the refresh was
                // triggered by a create and the app should jump to the
                // newly-created session.
                let prior_identity = self
                    .sidebar_entries
                    .get(self.selected)
                    .map(|e| e.identity().to_string());

                self.sessions = sessions;

                // Reconcile the sidebar with the live session set:
                // drop Session entries for killed sessions, append
                // Session entries for new ones. Section entries are
                // untouched.
                let live: Vec<String> = self.sessions.iter().map(|v| v.name().to_string()).collect();
                reconcile_sidebar(&mut self.sidebar_entries, &live);

                if let Some(target) = select_after {
                    if let Some(idx) = self
                        .sidebar_entries
                        .iter()
                        .position(|e| matches!(e, SidebarEntry::Session { internal } if internal == &target))
                    {
                        self.selected = idx;
                    }
                } else if let Some(id) = prior_identity {
                    if let Some(idx) = self
                        .sidebar_entries
                        .iter()
                        .position(|e| e.identity() == id)
                    {
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
        // Only request preview capture when a session is selected.
        // On a section header we keep the previous focus so switching
        // off/onto a header doesn't churn capture work.
        let current = self.selected_session().map(|v| v.name().to_string());
        if let Some(name) = &current {
            if self.focus_sent.as_deref() != Some(name.as_str()) {
                out.push(Command::FocusPreview { name: name.clone() });
                self.focus_sent = Some(name.clone());
            }
        }
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
            (KeyCode::Down, KeyModifiers::SHIFT) | (KeyCode::Char('J'), _) => {
                self.move_down(out);
            }
            (KeyCode::Up, KeyModifiers::SHIFT) | (KeyCode::Char('K'), _) => {
                self.move_up(out);
            }
            (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                if !self.sidebar_entries.is_empty() {
                    self.selected = (self.selected + 1).min(self.sidebar_entries.len() - 1);
                }
            }
            (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                self.selected = self.selected.saturating_sub(1);
            }
            (KeyCode::Enter, _) => {
                // Enter attaches — only meaningful on a session row.
                if let Some(s) = self.selected_session() {
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
                match self.selected_entry().cloned() {
                    Some(SidebarEntry::Session { internal }) => {
                        let display = self
                            .session_by_name(&internal)
                            .map(|s| s.display().to_string())
                            .unwrap_or_else(|| internal.clone());
                        self.modals
                            .push(Box::new(RenameModal::new(internal, display)));
                    }
                    Some(SidebarEntry::Section { id, name }) => {
                        if self.modals.top_id() != Some("section") {
                            self.pending_modal = Some(ModalRequest::Section {
                                editing: Some((id, name)),
                            });
                        }
                    }
                    None => {}
                }
            }
            (KeyCode::Char('d'), KeyModifiers::NONE) => {
                match self.selected_entry().cloned() {
                    Some(SidebarEntry::Session { internal }) => {
                        let display = self
                            .session_by_name(&internal)
                            .map(|s| s.display().to_string())
                            .unwrap_or_else(|| internal.clone());
                        let title = "Kill session?";
                        let msg = format!("This will kill '{}' and its pane.", display);
                        self.modals.push(Box::new(
                            ConfirmModal::new(title, msg, Command::KillSession(internal))
                                .destructive(),
                        ));
                    }
                    Some(SidebarEntry::Section { .. }) => {
                        // Delete the section header in place. Its
                        // members fall through into the group above
                        // (or become ungrouped if none). No confirm —
                        // trivial to re-add with `g`.
                        self.sidebar_entries.remove(self.selected);
                        self.clamp_selection();
                        self.save_sidebar(out);
                    }
                    None => {}
                }
            }
            (KeyCode::Char('R'), _) => {
                // Shift-R restarts: kill + recreate the selected
                // session using the metadata persisted to @bosun_*
                // tmux options at create time.
                if let Some(sel) = self.selected_session() {
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
            (KeyCode::Char('g'), KeyModifiers::NONE) => {
                // Insert a new section header above the cursor.
                if self.modals.top_id() != Some("section") {
                    self.pending_modal = Some(ModalRequest::Section { editing: None });
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

    /// Shift-J: move the selected entry down by one. For a Session,
    /// it's a single-item swap (crossing a Section header naturally
    /// moves the session into the next group). For a Section, the
    /// whole group (header + following sessions up to the next
    /// header) moves as a block, swapping places with the block below.
    fn move_down(&mut self, out: &mut Vec<Command>) {
        let len = self.sidebar_entries.len();
        if len < 2 || self.selected >= len {
            return;
        }
        let entry = self.sidebar_entries[self.selected].clone();
        match entry {
            SidebarEntry::Session { .. } => {
                if self.selected + 1 < len {
                    self.sidebar_entries.swap(self.selected, self.selected + 1);
                    self.selected += 1;
                    self.save_sidebar(out);
                }
            }
            SidebarEntry::Section { .. } => {
                let start = self.selected;
                let mid = self.group_end(start); // first idx of next block
                if mid >= len {
                    return; // already last group
                }
                let end = self.group_end(mid); // one past next block
                // Rotate [start..end) so the second block comes first.
                self.sidebar_entries[start..end].rotate_left(mid - start);
                self.selected = start + (end - mid);
                self.save_sidebar(out);
            }
        }
    }

    /// Shift-K: mirror of `move_down`.
    fn move_up(&mut self, out: &mut Vec<Command>) {
        if self.selected == 0 || self.sidebar_entries.is_empty() {
            return;
        }
        let entry = self.sidebar_entries[self.selected].clone();
        match entry {
            SidebarEntry::Session { .. } => {
                self.sidebar_entries.swap(self.selected, self.selected - 1);
                self.selected -= 1;
                self.save_sidebar(out);
            }
            SidebarEntry::Section { .. } => {
                // Find the start of the block above by walking back
                // until we hit another section header (or index 0).
                let cur = self.selected;
                let mut prev_start = cur;
                while prev_start > 0 {
                    prev_start -= 1;
                    if self.sidebar_entries[prev_start].is_section() {
                        break;
                    }
                }
                // `prev_start` now points at the header of the block
                // above — unless there's no header above (i.e. the
                // block above is the ungrouped head of the list), in
                // which case prev_start is 0 and the entry at 0 is
                // a Session.
                let end = self.group_end(cur);
                self.sidebar_entries[prev_start..end].rotate_right(end - cur);
                self.selected = prev_start;
                self.save_sidebar(out);
            }
        }
    }

    /// Insert a new section header above the cursor with the given name.
    /// Called by the app loop after the SectionModal submits.
    pub fn insert_section(&mut self, name: String, out: &mut Vec<Command>) {
        let entry = SidebarEntry::new_section(name);
        let idx = self.selected.min(self.sidebar_entries.len());
        self.sidebar_entries.insert(idx, entry);
        self.selected = idx;
        self.save_sidebar(out);
    }

    /// Rename an existing section by id. No-op if the id isn't found.
    pub fn rename_section(&mut self, id: &str, new_name: String, out: &mut Vec<Command>) {
        for e in self.sidebar_entries.iter_mut() {
            if let SidebarEntry::Section { id: eid, name } = e {
                if eid == id {
                    *name = new_name;
                    self.save_sidebar(out);
                    return;
                }
            }
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

        let state = AppState {
            divider_x: config.divider_x,
            sidebar_entries: config.sidebar.clone(),
            ..Default::default()
        };

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

            // Intercept UI-only commands here before anything reaches
            // the tmux actor. Some commands (InsertSection, RenameSection)
            // emit follow-up commands (e.g. SaveSidebar) as part of
            // their handler; `queue` lets us re-enter the dispatch
            // without a recursive call.
            let mut queue: Vec<Command> = self.state.apply(msg);
            while let Some(c) = queue.pop() {
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
                    Command::SaveSidebar(entries) => {
                        if let Err(e) = crate::config::write_sidebar(&entries) {
                            self.state.warning = Some(format!("sidebar: failed to save: {e}"));
                        }
                    }
                    Command::InsertSection { name } => {
                        self.state.insert_section(name, &mut queue);
                    }
                    Command::RenameSection { id, new_name } => {
                        self.state.rename_section(&id, new_name, &mut queue);
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
                    ModalRequest::Section { editing } => {
                        let modal = match editing {
                            Some((id, name)) => SectionModal::rename_section(id, name),
                            None => SectionModal::new_section(),
                        };
                        self.state.modals.push(Box::new(modal));
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

                // While we were blocked in attach, the tmux actor's 1Hz
                // preview_tick kept queuing SessionsRefreshed messages
                // into `evt_rx` (one per second of attach). If we didn't
                // drain them here, the main loop would process each one
                // — redrawing the preview for every stale capture — and
                // the user would see a "flipbook" scroll while new key
                // events sat at the tail of the backlog, unprocessed.
                // Non-refresh messages (Warn, Fatal, etc) are preserved
                // by re-sending them via evt_tx so they're still seen.
                use tokio::sync::mpsc::error::TryRecvError;
                let mut preserved: Vec<AppMsg> = Vec::new();
                loop {
                    match self.evt_rx.try_recv() {
                        Ok(AppMsg::SessionsRefreshed { .. }) => {}
                        Ok(other) => preserved.push(other),
                        Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
                    }
                }
                for m in preserved {
                    let _ = self.evt_tx.send(m);
                }

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
        let sidebar_entries: Vec<SidebarEntry> = sessions
            .iter()
            .map(|s| SidebarEntry::session(s.name()))
            .collect();
        AppState {
            sessions,
            selected,
            sidebar_entries,
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

    fn sec(id: &str, name: &str) -> SidebarEntry {
        SidebarEntry::Section {
            id: id.into(),
            name: name.into(),
        }
    }

    fn ses_entry(name: &str) -> SidebarEntry {
        SidebarEntry::session(name)
    }

    /// Shift-J on a section header moves the whole group as a block,
    /// swapping positions with the group below it.
    #[test]
    fn shift_j_on_section_moves_whole_group() {
        let mut s = AppState::default();
        s.sessions = vec![ses("a"), ses("b"), ses("c"), ses("d")];
        s.sidebar_entries = vec![
            sec("g1", "First"),
            ses_entry("a"),
            ses_entry("b"),
            sec("g2", "Second"),
            ses_entry("c"),
            ses_entry("d"),
        ];
        s.selected = 0; // on "First" header

        let shift_j = KeyEvent::new(KeyCode::Char('J'), KeyModifiers::SHIFT);
        s.apply(AppMsg::Key(shift_j));

        assert_eq!(
            s.sidebar_entries,
            vec![
                sec("g2", "Second"),
                ses_entry("c"),
                ses_entry("d"),
                sec("g1", "First"),
                ses_entry("a"),
                ses_entry("b"),
            ]
        );
        // Cursor follows the moved header.
        assert_eq!(s.selected, 3);
    }

    /// Shift-K on a section header moves it above the previous group.
    #[test]
    fn shift_k_on_section_moves_whole_group_up() {
        let mut s = AppState::default();
        s.sessions = vec![ses("a"), ses("b"), ses("c"), ses("d")];
        s.sidebar_entries = vec![
            sec("g1", "First"),
            ses_entry("a"),
            ses_entry("b"),
            sec("g2", "Second"),
            ses_entry("c"),
            ses_entry("d"),
        ];
        s.selected = 3; // on "Second" header

        let shift_k = KeyEvent::new(KeyCode::Char('K'), KeyModifiers::SHIFT);
        s.apply(AppMsg::Key(shift_k));

        assert_eq!(
            s.sidebar_entries,
            vec![
                sec("g2", "Second"),
                ses_entry("c"),
                ses_entry("d"),
                sec("g1", "First"),
                ses_entry("a"),
                ses_entry("b"),
            ]
        );
        assert_eq!(s.selected, 0);
    }

    /// Shift-J on a session moves it by one — crossing a section
    /// header naturally re-parents it into the next group.
    #[test]
    fn shift_j_on_session_crosses_section_boundary() {
        let mut s = AppState::default();
        s.sessions = vec![ses("a"), ses("b")];
        s.sidebar_entries = vec![
            sec("g1", "First"),
            ses_entry("a"),
            sec("g2", "Second"),
            ses_entry("b"),
        ];
        s.selected = 1; // on session "a" inside First

        let shift_j = KeyEvent::new(KeyCode::Char('J'), KeyModifiers::SHIFT);
        s.apply(AppMsg::Key(shift_j));

        // "a" hopped past the "Second" header — now it's inside Second.
        assert_eq!(
            s.sidebar_entries,
            vec![
                sec("g1", "First"),
                sec("g2", "Second"),
                ses_entry("a"),
                ses_entry("b"),
            ]
        );
        assert_eq!(s.selected, 2);
    }

    /// `d` on a section header deletes it without confirm — members
    /// fall through into the previous group.
    #[test]
    fn d_on_section_deletes_header() {
        let mut s = AppState::default();
        s.sessions = vec![ses("a")];
        s.sidebar_entries = vec![sec("g1", "Work"), ses_entry("a")];
        s.selected = 0;

        s.apply(AppMsg::Key(key(KeyCode::Char('d'))));

        assert_eq!(s.sidebar_entries, vec![ses_entry("a")]);
        assert_eq!(s.selected, 0);
        // No ConfirmModal should have opened.
        assert!(s.modals.is_empty());
    }

    /// `g` opens the new-section modal (routed via pending_modal).
    #[test]
    fn g_requests_section_modal() {
        let mut s = state_with(vec![ses("a")], 0);
        s.apply(AppMsg::Key(key(KeyCode::Char('g'))));
        assert!(matches!(
            s.pending_modal,
            Some(ModalRequest::Section { editing: None })
        ));
    }

    /// `r` on a selected section requests the rename modal in edit mode.
    #[test]
    fn r_on_section_requests_rename() {
        let mut s = AppState::default();
        s.sidebar_entries = vec![sec("g1", "Work")];
        s.selected = 0;
        s.apply(AppMsg::Key(key(KeyCode::Char('r'))));
        match &s.pending_modal {
            Some(ModalRequest::Section {
                editing: Some((id, name)),
            }) => {
                assert_eq!(id, "g1");
                assert_eq!(name, "Work");
            }
            other => panic!("expected Section editing modal, got {:?}", other),
        }
    }
}
