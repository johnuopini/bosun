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
use crate::sidebar::{Location, SidebarModel, VisibleKind};
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
    /// The sidebar state: explicit `ungrouped` bucket + ordered
    /// `sections` list with per-section `members`. `selected` indexes
    /// into the flattened visible list (`sidebar.visible()`), not
    /// into any one bucket. Reconciled on every `SessionsRefreshed`
    /// (dead sessions dropped, new sessions appended to `ungrouped`).
    /// Persisted to `config.toml` via `Command::SaveSidebar`.
    pub sidebar: SidebarModel,
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
    /// Emit a `SaveSidebar` command with the current model. Called
    /// whenever the sidebar is mutated (reorder, add section, rename,
    /// delete).
    fn save_sidebar(&self, out: &mut Vec<Command>) {
        out.push(Command::SaveSidebar(self.sidebar.clone()));
    }

    fn clamp_selection(&mut self) {
        let len = self.sidebar.len();
        if len == 0 {
            self.selected = 0;
        } else if self.selected >= len {
            self.selected = len - 1;
        }
    }

    /// The location in the model under the cursor, if any.
    pub fn selected_location(&self) -> Option<Location> {
        self.sidebar.locate(self.selected)
    }

    /// The kind of entry under the cursor, if any.
    pub fn selected_kind(&self) -> Option<VisibleKind> {
        self.sidebar.visible().get(self.selected).map(|v| v.kind())
    }

    /// The internal session name under the cursor, if the cursor is
    /// on a session (ungrouped or a member). `None` for section headers.
    pub fn selected_session_name(&self) -> Option<String> {
        let visible = self.sidebar.visible();
        visible
            .get(self.selected)?
            .session_name()
            .map(|s| s.to_string())
    }

    /// Look up the `SessionView` under the cursor (if it's a session).
    pub fn selected_session(&self) -> Option<&SessionView> {
        let name = self.selected_session_name()?;
        self.sessions.iter().find(|v| v.name() == name)
    }

    /// Preview buffer for the currently selected session, if any.
    pub fn selected_preview(&self) -> Option<&[u8]> {
        self.selected_session().and_then(|v| v.preview.as_deref())
    }

    /// Look up the SessionView for a given internal name.
    pub fn session_by_name(&self, name: &str) -> Option<&SessionView> {
        self.sessions.iter().find(|v| v.name() == name)
    }

    /// Pure reducer. Returns a list of Commands the caller should dispatch.
    pub fn apply(&mut self, msg: AppMsg) -> Vec<Command> {
        let mut out = Vec::new();
        match msg {
            AppMsg::SessionsRefreshed {
                sessions,
                select_after,
            } => {
                // Preserve selection by entry identity across
                // refreshes — section id if a header was selected,
                // internal name if a session was selected. Unless
                // `select_after` is set (fresh create), in which
                // case jump to the new session.
                let prior_identity = self
                    .sidebar
                    .visible()
                    .get(self.selected)
                    .map(|v| v.identity().to_string());

                self.sessions = sessions;

                let live: Vec<String> =
                    self.sessions.iter().map(|v| v.name().to_string()).collect();
                self.sidebar.reconcile(&live);

                if let Some(target) = select_after {
                    if let Some(idx) = self.sidebar.find_identity(&target) {
                        self.selected = idx;
                    }
                } else if let Some(id) = prior_identity {
                    if let Some(idx) = self.sidebar.find_identity(&id) {
                        self.selected = idx;
                    }
                }
                self.clamp_selection();
                if let Some(w) = &self.warning {
                    if w.starts_with("list:") {
                        self.warning = None;
                    }
                }
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
            // Shift+Down / Shift+J: reorder within bucket (session)
            // or move whole group (section header).
            (KeyCode::Down, KeyModifiers::SHIFT) | (KeyCode::Char('J'), _) => {
                self.move_down_within(out);
            }
            (KeyCode::Up, KeyModifiers::SHIFT) | (KeyCode::Char('K'), _) => {
                self.move_up_within(out);
            }
            // Shift+Right / Shift+Left: cross-bucket moves. Only
            // meaningful on session rows.
            (KeyCode::Right, KeyModifiers::SHIFT) => {
                self.move_to_next_bucket(out);
            }
            (KeyCode::Left, KeyModifiers::SHIFT) => {
                self.move_to_prev_bucket(out);
            }
            (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                let len = self.sidebar.len();
                if len > 0 {
                    self.selected = (self.selected + 1).min(len - 1);
                }
            }
            (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                self.selected = self.selected.saturating_sub(1);
            }
            // Enter OR plain Right = attach the selected session.
            (KeyCode::Enter, _) | (KeyCode::Right, KeyModifiers::NONE) => {
                if let Some(s) = self.selected_session() {
                    self.pending_attach = Some(s.name().to_string());
                }
            }
            (KeyCode::Char('r'), KeyModifiers::CONTROL) => {
                out.push(Command::ListNow);
            }
            (KeyCode::Char('r'), KeyModifiers::NONE) => match self.selected_location() {
                Some(Location::Header(si)) => {
                    let s = &self.sidebar.sections[si];
                    if self.modals.top_id() != Some("section") {
                        self.pending_modal = Some(ModalRequest::Section {
                            editing: Some((s.id.clone(), s.name.clone())),
                        });
                    }
                }
                Some(_) => {
                    if let Some(sel) = self.selected_session() {
                        let internal = sel.name().to_string();
                        let display = sel.display().to_string();
                        self.modals
                            .push(Box::new(RenameModal::new(internal, display)));
                    }
                }
                None => {}
            },
            (KeyCode::Char('d'), KeyModifiers::NONE) => match self.selected_location() {
                Some(Location::Header(si)) => {
                    // Delete the section header; its members flow
                    // back into ungrouped. No confirm — trivial to
                    // re-add with `g`.
                    self.sidebar.delete_section_at(si);
                    self.clamp_selection();
                    self.save_sidebar(out);
                }
                Some(_) => {
                    if let Some(sel) = self.selected_session() {
                        let internal = sel.name().to_string();
                        let display = sel.display().to_string();
                        let title = "Kill session?";
                        let msg = format!("This will kill '{}' and its pane.", display);
                        self.modals.push(Box::new(
                            ConfirmModal::new(title, msg, Command::KillSession(internal))
                                .destructive(),
                        ));
                    }
                }
                None => {}
            },
            (KeyCode::Char('R'), _) => {
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
            (KeyCode::Char('n'), KeyModifiers::NONE)
                if self.modals.top_id() != Some("new_session") =>
            {
                self.pending_modal = Some(ModalRequest::NewSession);
            }
            (KeyCode::Char('g'), KeyModifiers::NONE) if self.modals.top_id() != Some("section") => {
                self.pending_modal = Some(ModalRequest::Section { editing: None });
            }
            (KeyCode::Char('t'), KeyModifiers::NONE) if self.modals.top_id() != Some("theme") => {
                self.pending_modal = Some(ModalRequest::Theme);
            }
            // Direct-jump: 0 → ungrouped, 1..=9 → sections[0..=8]. Only
            // meaningful when the cursor is on a session; the move
            // helper no-ops on section headers and out-of-range targets.
            (KeyCode::Char(c @ '0'..='9'), KeyModifiers::NONE) => {
                let target = if c == '0' {
                    None
                } else {
                    Some((c as u8 - b'1') as usize)
                };
                self.move_session_to_bucket(target, out);
            }
            _ => {}
        }
    }

    /// Shift-J / Shift-Down. Sessions reorder within their own
    /// bucket only (ungrouped or a specific section). Sections move
    /// as a block (header + all members) among the sections list.
    fn move_down_within(&mut self, out: &mut Vec<Command>) {
        let loc = match self.selected_location() {
            Some(l) => l,
            None => return,
        };
        match loc {
            Location::Ungrouped(i) => {
                if i + 1 < self.sidebar.ungrouped.len() {
                    self.sidebar.ungrouped.swap(i, i + 1);
                    self.selected = self.sidebar.flat_index(Location::Ungrouped(i + 1));
                    self.save_sidebar(out);
                }
            }
            Location::Member(si, mi) => {
                let members = &mut self.sidebar.sections[si].members;
                if mi + 1 < members.len() {
                    members.swap(mi, mi + 1);
                    self.selected = self.sidebar.flat_index(Location::Member(si, mi + 1));
                    self.save_sidebar(out);
                }
            }
            Location::Header(si) => {
                if si + 1 < self.sidebar.sections.len() {
                    self.sidebar.sections.swap(si, si + 1);
                    self.selected = self.sidebar.flat_index(Location::Header(si + 1));
                    self.save_sidebar(out);
                }
            }
        }
    }

    /// Shift-K / Shift-Up. Mirror of `move_down_within`.
    fn move_up_within(&mut self, out: &mut Vec<Command>) {
        let loc = match self.selected_location() {
            Some(l) => l,
            None => return,
        };
        match loc {
            Location::Ungrouped(i) => {
                if i > 0 {
                    self.sidebar.ungrouped.swap(i, i - 1);
                    self.selected = self.sidebar.flat_index(Location::Ungrouped(i - 1));
                    self.save_sidebar(out);
                }
            }
            Location::Member(si, mi) => {
                if mi > 0 {
                    self.sidebar.sections[si].members.swap(mi, mi - 1);
                    self.selected = self.sidebar.flat_index(Location::Member(si, mi - 1));
                    self.save_sidebar(out);
                }
            }
            Location::Header(si) => {
                if si > 0 {
                    self.sidebar.sections.swap(si, si - 1);
                    self.selected = self.sidebar.flat_index(Location::Header(si - 1));
                    self.save_sidebar(out);
                }
            }
        }
    }

    /// Move the selected session directly into a named bucket.
    /// `target = None` → ungrouped; `target = Some(si)` → sections[si].
    /// Inserts at the END of the target. No-op if cursor isn't on a
    /// session or the target is the session's current bucket.
    pub fn move_session_to_bucket(&mut self, target: Option<usize>, out: &mut Vec<Command>) {
        let loc = match self.selected_location() {
            Some(l) => l,
            None => return,
        };
        // Resolve target, bail if out of range or same bucket.
        let name = match (loc, target) {
            (Location::Ungrouped(_), None) => return,
            (Location::Member(cur, _), Some(t)) if cur == t => return,
            (Location::Header(_), _) => return,
            (Location::Ungrouped(i), Some(t)) => {
                if t >= self.sidebar.sections.len() {
                    return;
                }
                self.sidebar.ungrouped.remove(i)
            }
            (Location::Member(si, mi), None) => self.sidebar.sections[si].members.remove(mi),
            (Location::Member(si, mi), Some(t)) => {
                if t >= self.sidebar.sections.len() {
                    return;
                }
                self.sidebar.sections[si].members.remove(mi)
            }
        };
        match target {
            None => {
                self.sidebar.ungrouped.push(name);
                let new_idx = self.sidebar.ungrouped.len() - 1;
                self.selected = self.sidebar.flat_index(Location::Ungrouped(new_idx));
            }
            Some(si) => {
                self.sidebar.sections[si].members.push(name);
                let new_mi = self.sidebar.sections[si].members.len() - 1;
                self.selected = self.sidebar.flat_index(Location::Member(si, new_mi));
            }
        }
        self.save_sidebar(out);
    }

    /// Shift-Right. Move a session one bucket forward: ungrouped →
    /// first section → next section → …. Inserts at the START of the
    /// target bucket (nearest edge). No-op on section headers or at
    /// the last bucket.
    fn move_to_next_bucket(&mut self, out: &mut Vec<Command>) {
        let loc = match self.selected_location() {
            Some(l) => l,
            None => return,
        };
        match loc {
            Location::Ungrouped(i) => {
                if self.sidebar.sections.is_empty() {
                    return;
                }
                let name = self.sidebar.ungrouped.remove(i);
                self.sidebar.sections[0].members.insert(0, name);
                self.selected = self.sidebar.flat_index(Location::Member(0, 0));
                self.save_sidebar(out);
            }
            Location::Member(si, mi) => {
                if si + 1 >= self.sidebar.sections.len() {
                    return;
                }
                let name = self.sidebar.sections[si].members.remove(mi);
                self.sidebar.sections[si + 1].members.insert(0, name);
                self.selected = self.sidebar.flat_index(Location::Member(si + 1, 0));
                self.save_sidebar(out);
            }
            Location::Header(_) => {}
        }
    }

    /// Shift-Left. Mirror of `move_to_next_bucket`: last section →
    /// previous section → … → ungrouped. Inserts at the END of the
    /// target bucket (nearest edge). No-op on section headers or at
    /// the first bucket.
    fn move_to_prev_bucket(&mut self, out: &mut Vec<Command>) {
        let loc = match self.selected_location() {
            Some(l) => l,
            None => return,
        };
        match loc {
            Location::Ungrouped(_) => {} // already at leftmost bucket
            Location::Member(si, mi) => {
                let name = self.sidebar.sections[si].members.remove(mi);
                if si == 0 {
                    // Out of group 0 → ungrouped (end).
                    self.sidebar.ungrouped.push(name);
                    let new_idx = self.sidebar.ungrouped.len() - 1;
                    self.selected = self.sidebar.flat_index(Location::Ungrouped(new_idx));
                } else {
                    let target = si - 1;
                    self.sidebar.sections[target].members.push(name);
                    let new_mi = self.sidebar.sections[target].members.len() - 1;
                    self.selected = self.sidebar.flat_index(Location::Member(target, new_mi));
                }
                self.save_sidebar(out);
            }
            Location::Header(_) => {}
        }
    }

    /// Insert a new empty section at the end of the sections list.
    /// Called by the app loop after the SectionModal submits. Cursor
    /// jumps to the new header.
    pub fn insert_section(&mut self, name: String, out: &mut Vec<Command>) {
        let id = self.sidebar.insert_section_at_end(name);
        if let Some(idx) = self.sidebar.find_identity(&id) {
            self.selected = idx;
        }
        self.save_sidebar(out);
    }

    /// Rename an existing section by id. No-op if the id isn't found.
    pub fn rename_section(&mut self, id: &str, new_name: String, out: &mut Vec<Command>) {
        if self.sidebar.rename_section(id, new_name) {
            self.save_sidebar(out);
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
            MouseEventKind::Down(MouseButton::Left)
                if layout::is_divider_col(&layouts, m.column) =>
            {
                self.dragging_divider = true;
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
            sidebar: config.sidebar.clone(),
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
#[allow(clippy::field_reassign_with_default)]
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
        let ungrouped = sessions.iter().map(|s| s.name().to_string()).collect();
        AppState {
            sessions,
            selected,
            sidebar: SidebarModel {
                ungrouped,
                sections: Vec::new(),
            },
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

    use crate::sidebar::Section;

    fn section(id: &str, name: &str, members: &[&str]) -> Section {
        Section {
            id: id.into(),
            name: name.into(),
            members: members.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn model(ungrouped: &[&str], sections: Vec<Section>) -> SidebarModel {
        SidebarModel {
            ungrouped: ungrouped.iter().map(|s| s.to_string()).collect(),
            sections,
        }
    }

    /// Shift-J on a section header moves only that section among the
    /// sections list (its members come along because they're owned by
    /// the section struct).
    #[test]
    fn shift_j_on_section_moves_whole_group() {
        let mut s = AppState::default();
        s.sessions = vec![ses("a"), ses("b"), ses("c"), ses("d")];
        s.sidebar = model(
            &[],
            vec![
                section("g1", "First", &["a", "b"]),
                section("g2", "Second", &["c", "d"]),
            ],
        );
        // Flat index of g1 header: ungrouped(0) + 0 = 0
        s.selected = 0;

        let shift_j = KeyEvent::new(KeyCode::Char('J'), KeyModifiers::SHIFT);
        s.apply(AppMsg::Key(shift_j));

        assert_eq!(
            s.sidebar,
            model(
                &[],
                vec![
                    section("g2", "Second", &["c", "d"]),
                    section("g1", "First", &["a", "b"]),
                ],
            )
        );
        // g1 is now the second section; its header flat index = 3
        // (0..=2 are g2 header + its two members).
        assert_eq!(s.selected, 3);
    }

    /// Shift-J on an ungrouped session swaps within the ungrouped
    /// bucket. Hits a floor at the end — does NOT fall into a section.
    #[test]
    fn shift_j_on_ungrouped_floors_at_bucket_end() {
        let mut s = AppState::default();
        s.sessions = vec![ses("a"), ses("b"), ses("c")];
        s.sidebar = model(&["a", "b"], vec![section("g1", "First", &["c"])]);
        s.selected = 1; // ungrouped b

        let shift_j = KeyEvent::new(KeyCode::Char('J'), KeyModifiers::SHIFT);
        s.apply(AppMsg::Key(shift_j));

        // b didn't move — it's at the end of ungrouped.
        assert_eq!(
            s.sidebar,
            model(&["a", "b"], vec![section("g1", "First", &["c"])])
        );
    }

    /// Shift-Right moves an ungrouped session into the first section
    /// (start of that section's members).
    #[test]
    fn shift_right_moves_ungrouped_into_first_section() {
        let mut s = AppState::default();
        s.sessions = vec![ses("a"), ses("b"), ses("c")];
        s.sidebar = model(&["a", "b"], vec![section("g1", "First", &["c"])]);
        s.selected = 0; // ungrouped a

        let shift_right = KeyEvent::new(KeyCode::Right, KeyModifiers::SHIFT);
        s.apply(AppMsg::Key(shift_right));

        assert_eq!(
            s.sidebar,
            model(&["b"], vec![section("g1", "First", &["a", "c"])])
        );
        // cursor follows to new member index: ungrouped has 1 entry,
        // then header, then a at member index 0 → flat index 2.
        assert_eq!(s.selected, 2);
    }

    /// Shift-Left moves a session out of its section back to the
    /// end of the previous bucket (ungrouped if it was in section 0).
    #[test]
    fn shift_left_moves_out_of_first_section_to_ungrouped() {
        let mut s = AppState::default();
        s.sessions = vec![ses("a"), ses("b")];
        s.sidebar = model(&["a"], vec![section("g1", "First", &["b"])]);
        // flat: 0=a, 1=g1 header, 2=b
        s.selected = 2;

        let shift_left = KeyEvent::new(KeyCode::Left, KeyModifiers::SHIFT);
        s.apply(AppMsg::Key(shift_left));

        assert_eq!(
            s.sidebar,
            model(&["a", "b"], vec![section("g1", "First", &[])])
        );
        // b is now ungrouped at index 1.
        assert_eq!(s.selected, 1);
    }

    /// Creating a new section does NOT claim any sessions — it's empty.
    #[test]
    fn new_section_is_empty() {
        let mut s = AppState::default();
        s.sessions = vec![ses("a"), ses("b")];
        s.sidebar = model(&["a", "b"], vec![]);
        s.selected = 0;

        let mut out = Vec::new();
        s.insert_section("Work".to_string(), &mut out);

        assert_eq!(s.sidebar.ungrouped, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(s.sidebar.sections.len(), 1);
        assert_eq!(s.sidebar.sections[0].name, "Work");
        assert!(s.sidebar.sections[0].members.is_empty());
    }

    /// `d` on a section header dissolves it — members go to ungrouped.
    #[test]
    fn d_on_section_dissolves_members_to_ungrouped() {
        let mut s = AppState::default();
        s.sessions = vec![ses("a"), ses("b")];
        s.sidebar = model(&["a"], vec![section("g1", "Work", &["b"])]);
        s.selected = 1; // g1 header

        s.apply(AppMsg::Key(key(KeyCode::Char('d'))));

        assert_eq!(s.sidebar, model(&["a", "b"], vec![]));
        assert_eq!(s.selected, 1); // stays at the old header position (now b)
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
        s.sidebar = model(&[], vec![section("g1", "Work", &[])]);
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

    /// Right arrow (no shift) attaches the selected session.
    #[test]
    fn right_arrow_attaches_session() {
        let mut s = state_with(vec![ses("main")], 0);
        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
        s.apply(AppMsg::Key(right));
        assert_eq!(s.pending_attach.as_deref(), Some("main"));
    }

    /// Pressing `2` on an ungrouped session jumps it directly to
    /// sections[1] — no cycling required.
    #[test]
    fn digit_jumps_session_directly_to_section() {
        let mut s = AppState::default();
        s.sessions = vec![ses("bosun")];
        s.sidebar = model(
            &["bosun"],
            vec![section("g1", "SKULK", &[]), section("g2", "YETI", &[])],
        );
        s.selected = 0;

        s.apply(AppMsg::Key(key(KeyCode::Char('2'))));

        assert!(s.sidebar.ungrouped.is_empty());
        assert!(s.sidebar.sections[0].members.is_empty());
        assert_eq!(s.sidebar.sections[1].members, vec!["bosun".to_string()]);
        assert_eq!(
            s.selected_session().map(|v| v.name().to_string()),
            Some("bosun".to_string())
        );
    }

    /// Pressing `0` sends the session back to ungrouped.
    #[test]
    fn digit_zero_returns_session_to_ungrouped() {
        let mut s = AppState::default();
        s.sessions = vec![ses("bosun")];
        s.sidebar = model(&[], vec![section("g1", "W", &["bosun"])]);
        // flat: 0=header, 1=bosun
        s.selected = 1;

        s.apply(AppMsg::Key(key(KeyCode::Char('0'))));

        assert_eq!(s.sidebar.ungrouped, vec!["bosun".to_string()]);
        assert!(s.sidebar.sections[0].members.is_empty());
    }

    /// Digit for a nonexistent section is a no-op (doesn't move).
    #[test]
    fn digit_out_of_range_is_noop() {
        let mut s = AppState::default();
        s.sessions = vec![ses("bosun")];
        s.sidebar = model(&["bosun"], vec![section("g1", "W", &[])]);
        s.selected = 0;

        // Only one section → `2` is out of range.
        s.apply(AppMsg::Key(key(KeyCode::Char('2'))));
        assert_eq!(s.sidebar.ungrouped, vec!["bosun".to_string()]);
    }

    /// Shift-Right cycles through sections: pressing it again after
    /// a move jumps from section 0 to section 1, etc.
    #[test]
    fn shift_right_cycles_to_further_sections() {
        let mut s = AppState::default();
        s.sessions = vec![ses("bosun")];
        s.sidebar = model(
            &["bosun"],
            vec![section("g1", "SKULK", &[]), section("g2", "YETI", &[])],
        );
        s.selected = 0; // bosun in ungrouped

        let sr = KeyEvent::new(KeyCode::Right, KeyModifiers::SHIFT);

        s.apply(AppMsg::Key(sr));
        assert!(s.sidebar.ungrouped.is_empty());
        assert_eq!(s.sidebar.sections[0].members, vec!["bosun".to_string()]);
        assert!(s.sidebar.sections[1].members.is_empty());
        assert_eq!(
            s.selected_session().map(|v| v.name().to_string()),
            Some("bosun".to_string()),
            "cursor should track bosun into SKULK"
        );

        s.apply(AppMsg::Key(sr));
        assert!(s.sidebar.sections[0].members.is_empty());
        assert_eq!(s.sidebar.sections[1].members, vec!["bosun".to_string()]);
        assert_eq!(
            s.selected_session().map(|v| v.name().to_string()),
            Some("bosun".to_string()),
            "cursor should track bosun into YETI"
        );
    }
}
