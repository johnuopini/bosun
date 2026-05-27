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
use crate::store::{Recent, Store};
use crate::tmux::attach::attach_with_ctrl_q_detach;
use crate::tmux::session::SessionView;
use crate::tmux::TmuxClient;
use crate::ui;
use crate::ui::layout;
use crate::ui::modal::confirm::ConfirmModal;
use crate::ui::modal::help::HelpModal;
use crate::ui::modal::new_session::NewSessionModal;
use crate::ui::modal::quickjump::{QuickJumpModal, QuickJumpRow};
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
    /// Map from display name → last-known section name. Updated
    /// whenever the user moves a session into/out of a section.
    /// Used to auto-place a newly-appearing session (e.g. after a
    /// restart or when opened from recents) back into the same
    /// section, as long as a section with that name still exists.
    /// Persisted via `Command::SaveSessionHistory`.
    pub session_history: std::collections::HashMap<String, String>,
    /// Captured when the user opens the new-session modal: the section
    /// the cursor was on (or in). When the resulting session lands in
    /// the next refresh, it gets placed in this section instead of
    /// the default ungrouped bucket. Cleared on consume; overwritten
    /// each time the modal is opened.
    pub pending_new_session_section: Option<String>,
    /// Global TDF banner font used by the section/empty preview when
    /// no per-section override is set. Cycled by pressing `f` on a
    /// section header (per-section override) or on the empty splash
    /// (this global default). Persisted via `Command::SaveBannerFont`.
    pub banner_font: String,
    /// Managed-session prefix (e.g. `bosun-`). Snapshot of
    /// `Config::session_prefix` at startup. Used to extract the slug
    /// from an internal name when rendering missing-session rows in
    /// the sidebar and when matching a dead row back to a `Recent`
    /// for `R`-to-restart.
    pub session_prefix: String,
    /// Configured external editor command (`zed`, `code`, `subl`, ...).
    /// `None` means no editor is configured; pressing `e` warns. Loaded
    /// once at startup from `Config::editor`. The TUI doesn't currently
    /// hot-reload this — the user re-runs `bosun editor <cmd>` and
    /// restarts bosun.
    pub editor: Option<String>,
    /// Last-loaded snapshot of the SQLite recents store. Used to
    /// resolve internal-name → display-name for dead sidebar entries
    /// (so the row reads `Raycast` instead of `bosun-raycast-1e18ae00`)
    /// and to look up the full `SessionSpec` when restarting a dead
    /// session with `R`. Refreshed on every `SessionsRefreshed`.
    pub recents: Vec<Recent>,
    /// Old internal name to swap out of the sidebar on the next
    /// `SessionsRefreshed`. Set when the user confirms a restart
    /// (live `R` or dead-row recents-restart) so the new internal
    /// inherits the old row's slot and section instead of leaving
    /// a "? <name>" ghost above the freshly-created session.
    pub pending_restart_swap: Option<String>,
    /// Running accumulator for scroll-wheel events. A trackpad gesture
    /// fires many wheel events per swipe, so we only step the selection
    /// once every `SCROLL_TICKS_PER_STEP` events. Positive = pending
    /// downward steps, negative = pending upward steps; resets on
    /// direction change so a flick the other way feels immediate.
    pub scroll_accum: i32,
    /// Single-window mode (2.0+). When true, the App's `pending_attach`
    /// handler routes through `enter_focus` (preview pane becomes
    /// the interactive surface) instead of `perform_attach` (full-
    /// screen `tmux attach` with ratatui torn down). The sidebar
    /// stays visible the whole time. Toggled live with `s`;
    /// persisted via `Command::SaveSingleWindow` so the preference
    /// survives across bosun restarts.
    pub single_window_mode: bool,
}

/// Number of wheel events that must accumulate in one direction before
/// the selection steps. Tuned for macOS trackpads, which fire ~10
/// events per modest two-finger swipe.
const SCROLL_TICKS_PER_STEP: i32 = 2;

impl AppState {
    /// Resolve a dead session's internal name into the friendliest
    /// label we can produce — usually the original display name from
    /// the Recents store, falling back to the slug if no Recent
    /// matches, and ultimately to the raw internal name. Used by the
    /// sidebar's missing-row renderer so users see `Raycast` instead
    /// of `bosun-raycast-1e18ae00`.
    pub fn dead_display_for(&self, internal: &str) -> String {
        match self.recent_for_internal(internal) {
            Some(r) => r.name.clone(),
            None => {
                match crate::actors::tmux_actor::slug_from_internal(internal, &self.session_prefix)
                {
                    Some(slug) if !slug.is_empty() => slug.to_string(),
                    _ => internal.to_string(),
                }
            }
        }
    }

    /// Look up the persisted spec for a dead sidebar entry. Matches
    /// by slug equivalence: `slugify(recent.name) == slug(internal)`.
    /// Slug collisions are theoretically possible (two recents that
    /// slugify identically) but in practice unlikely; first match
    /// wins. Returns `None` for live entries — call `selected_session`
    /// for those.
    pub fn recent_for_internal(&self, internal: &str) -> Option<&Recent> {
        let slug = crate::actors::tmux_actor::slug_from_internal(internal, &self.session_prefix)?;
        self.recents
            .iter()
            .find(|r| crate::actors::tmux_actor::slugify(&r.name) == slug)
    }
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
    /// Open the type-ahead quick-jump session picker. Populated by
    /// the app loop with the current managed sessions.
    QuickJump,
    /// Open the key-bindings help / cheat-sheet modal. Pure UI; the
    /// app loop just constructs a `HelpModal` with no extra data.
    Help,
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

    /// If the cursor is on a section header or one of its members,
    /// return that section's name. Otherwise (ungrouped or empty), None.
    /// Used to remember which group a new session should land in.
    fn current_section_name(&self) -> Option<String> {
        match self.selected_location()? {
            Location::Header(si) | Location::Member(si, _) => {
                self.sidebar.sections.get(si).map(|s| s.name.clone())
            }
            Location::Ungrouped(_) => None,
        }
    }

    /// Update `session_history` from a single moved session. Looks up
    /// the session's display name from `self.sessions` and stores the
    /// current section it lives in (or clears the entry for ungrouped).
    /// No-op if the session isn't currently live.
    fn update_history_for(&mut self, internal: &str) -> bool {
        let display = match self.sessions.iter().find(|v| v.name() == internal) {
            Some(v) => v.display().to_string(),
            None => return false,
        };
        // In a section?
        for sec in &self.sidebar.sections {
            if sec.members.iter().any(|n| n == internal) {
                let prev = self.session_history.insert(display, sec.name.clone());
                return prev.as_deref() != Some(sec.name.as_str());
            }
        }
        // Otherwise ungrouped → drop the history entry.
        self.session_history.remove(&display).is_some()
    }

    /// Walk `ungrouped` and move each session with a matching
    /// `session_history` entry into the section of that name, if such a
    /// section exists. Returns true if the sidebar was mutated.
    fn restore_from_history(&mut self) -> bool {
        let mut changed = false;
        // Iterate over a snapshot of ungrouped so we can mutate during the loop.
        let ungrouped = self.sidebar.ungrouped.clone();
        for internal in ungrouped {
            let display = match self.sessions.iter().find(|v| v.name() == internal) {
                Some(v) => v.display().to_string(),
                None => continue,
            };
            let section_name = match self.session_history.get(&display).cloned() {
                Some(n) => n,
                None => continue,
            };
            let si = match self
                .sidebar
                .sections
                .iter()
                .position(|s| s.name == section_name)
            {
                Some(i) => i,
                None => continue,
            };
            if let Some(pos) = self.sidebar.ungrouped.iter().position(|n| n == &internal) {
                let n = self.sidebar.ungrouped.remove(pos);
                self.sidebar.sections[si].members.push(n);
                changed = true;
            }
        }
        changed
    }

    /// Emit a `SaveSessionHistory` command with the current history.
    fn save_session_history(&self, out: &mut Vec<Command>) {
        out.push(Command::SaveSessionHistory(self.session_history.clone()));
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

                // Restart-swap (dead-row restart-from-recents only —
                // live restart is in-place and never changes the
                // internal name): if the user confirmed a recreate
                // from a dead row, replace the old (still-dead)
                // internal name with the new one in place so
                // reconcile sees the new session already present and
                // doesn't append it. Only fire when this refresh
                // actually corresponds to the recreate (`select_after`
                // set) — intermediate refreshes from tmux monitor
                // notifications (e.g. a separate kill elsewhere)
                // must NOT consume the pending swap.
                let swap_applied = if let (Some(old), Some(new)) =
                    (self.pending_restart_swap.as_deref(), select_after.as_ref())
                {
                    let did = self.sidebar.replace_session(old, new);
                    self.pending_restart_swap = None;
                    did
                } else {
                    false
                };

                let live: Vec<String> =
                    self.sessions.iter().map(|v| v.name().to_string()).collect();
                self.sidebar.reconcile(&live);
                if swap_applied {
                    self.save_sidebar(&mut out);
                }

                // If this refresh is the result of a session create
                // and the user opened the new-session modal while
                // their cursor was on a section, seed the history
                // map so `restore_from_history` places the new
                // session there instead of leaving it in ungrouped.
                if let Some(target) = select_after.as_deref() {
                    if let Some(section_name) = self.pending_new_session_section.take() {
                        if self.sidebar.sections.iter().any(|s| s.name == section_name) {
                            if let Some(display) = self
                                .sessions
                                .iter()
                                .find(|v| v.name() == target)
                                .map(|v| v.display().to_string())
                            {
                                self.session_history.insert(display, section_name);
                                self.save_session_history(&mut out);
                            }
                        }
                    }
                }

                // Auto-place new sessions into their last-known
                // section by display-name match. Handles both
                // restart (same display name, new internal name)
                // and recents (same display name, fresh internal).
                if self.restore_from_history() {
                    self.save_sidebar(&mut out);
                }

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
            AppMsg::PreviewRefreshed { name, bytes } => {
                // Hot path for the 2.0 fast preview tick. Update the
                // preview bytes on the matching SessionView in place
                // and return no commands — no detector run, no sidebar
                // reconcile, no statusbar sync. A no-op if the named
                // session was killed between capture and delivery.
                if let Some(view) = self.sessions.iter_mut().find(|v| v.name() == name) {
                    view.preview = Some(bytes);
                }
            }
            AppMsg::EmbedBytes { .. } => {
                // The reducer is pure and AppState doesn't own the
                // embed (the App struct does — embed has runtime
                // resources that don't belong in pure state). The
                // App::run loop intercepts EmbedBytes before calling
                // apply() and feeds bytes into the embed directly,
                // so reaching here is a code-path bug, not a runtime
                // problem.
                tracing::warn!("EmbedBytes reached reducer — App::run intercept is broken");
            }
            AppMsg::Paste(_) => {
                // Paste handling lives on the App side too — the
                // only currently-meaningful target is the embed
                // PTY when focused. App::run intercepts before
                // calling apply(). Reaching here means no embed
                // (or not focused), in which case dropping is the
                // right move; no modal currently expects pasted
                // text directly.
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
                                // Command::Attach from a closing modal
                                // (QuickJump) is handled inline by the
                                // app loop — the tmux actor ignores it.
                                // Redirect to pending_attach so the
                                // standard attach flow runs next turn.
                                if let Command::Attach { name } = c {
                                    self.pending_attach = Some(name);
                                } else {
                                    if matches!(c, Command::CreateSession(_)) {
                                        self.pending_new_session_section =
                                            self.current_section_name();
                                    }
                                    // Explicit kill: drop the sidebar
                                    // entry locally too. Reconcile no
                                    // longer auto-removes dead sessions
                                    // (so a tmux restart doesn't wipe
                                    // the user's groups), so the only
                                    // way an entry leaves the sidebar
                                    // is via this explicit-action path.
                                    if let Command::KillSession(internal) = &c {
                                        self.sidebar.remove_session(internal);
                                        self.clamp_selection();
                                        self.save_sidebar(&mut out);
                                    }
                                    // Dead-row restart-from-recents:
                                    // selection is on a dead entry
                                    // whose display matches the spec
                                    // we're about to create. Capture
                                    // the dead internal so the next
                                    // `SessionsRefreshed` can splice
                                    // the new internal into the dead
                                    // row's slot. Modals block
                                    // selection movement, so the
                                    // cursor is still on the row the
                                    // user originally pressed R on.
                                    //
                                    // Live restart goes through
                                    // `Command::RestartSession`, which
                                    // is now in-place (same internal
                                    // name, same pane, no sidebar
                                    // churn), so no swap is needed
                                    // for that path.
                                    if let Command::CreateSession(spec) = &c {
                                        if self.selected_session().is_none() {
                                            if let Some(dead) = self.selected_session_name() {
                                                if self.dead_display_for(&dead) == spec.name {
                                                    self.pending_restart_swap = Some(dead);
                                                }
                                            }
                                        }
                                    }
                                    out.push(c);
                                }
                            }
                        }
                        StackDispatch::Emit(cmd) => {
                            if matches!(cmd, Command::CreateSession(_)) {
                                self.pending_new_session_section = self.current_section_name();
                            }
                            out.push(cmd);
                        }
                    }
                } else {
                    self.handle_key(k, &mut out);
                }
                self.sync_focus(&mut out);
            }
            AppMsg::Mouse(m) => {
                // Mouse: divider drag + scroll-wheel nav in the list.
                // Modals don't react to mouse yet, but we suppress
                // scroll-wheel selection changes while a modal is open
                // so the wheel can't shift the list underneath a
                // confirm dialog.
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
                    // re-add with `g`. Also drop any session_history
                    // entries that pointed at this section name so a
                    // later recreate doesn't re-place them into a
                    // section the user just tore down.
                    let gone_name = self.sidebar.sections[si].name.clone();
                    self.sidebar.delete_section_at(si);
                    self.clamp_selection();
                    self.save_sidebar(out);
                    let before = self.session_history.len();
                    self.session_history.retain(|_, v| v != &gone_name);
                    if self.session_history.len() != before {
                        self.save_session_history(out);
                    }
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
                    } else if let Some(internal) = self.selected_session_name() {
                        // Dead/missing entry — the underlying tmux session
                        // is gone (e.g. server restarted), but the sidebar
                        // row remains so the user can decide whether to
                        // remove it. Same command path; `kill_session` is
                        // idempotent on missing sessions.
                        let title = "Remove from sidebar?";
                        let msg = format!(
                            "'{}' is no longer a live tmux session. Remove the entry?",
                            internal
                        );
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
                    // Live session — restart in place via the actor,
                    // which reads metadata off the live tmux session.
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
                } else if let Some(internal) = self.selected_session_name() {
                    // Dead/missing entry — the tmux session and its
                    // stored metadata are gone, so we can't use
                    // `RestartSession` (the actor would fail at
                    // `get_session_metadata`). Instead, look up the
                    // persisted spec from the Recents store via slug
                    // match and fire `CreateSession`. The reducer's
                    // existing placement logic (session_history)
                    // drops the new session back into its old section.
                    //
                    // We leave the dead row in place; once the new
                    // session lands the user can `d` the old row.
                    // Pre-removing on confirm would be lost if the
                    // user hit Esc and the data isn't trivially
                    // recoverable from inside the modal flow.
                    if let Some(recent) = self.recent_for_internal(&internal) {
                        let spec = recent.to_spec();
                        let display = spec.name.clone();
                        let title = "Restart from recents?";
                        let msg = format!(
                            "Recreate '{}' from its last-saved spec? \
                             The old dead row stays — `d` to remove it after.",
                            display
                        );
                        self.modals.push(Box::new(ConfirmModal::new(
                            title,
                            msg,
                            Command::CreateSession(spec),
                        )));
                    } else {
                        self.warning = Some(format!(
                            "no recent found for '{}' — can't restart",
                            internal
                        ));
                    }
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
            // `s`: toggle single-window mode (2.0+). When on,
            // `Enter` / `Right` on a session opens it inside the
            // preview pane (focused embed) instead of doing a
            // full-screen `tmux attach`. Persisted to `config.toml`
            // so the preference sticks across restarts. The actual
            // mode switch in the live embed is handled by the app
            // loop on the next iteration via `sync_embed`-style
            // reconciliation.
            (KeyCode::Char('s'), KeyModifiers::NONE) => {
                self.single_window_mode = !self.single_window_mode;
                out.push(Command::SaveSingleWindow(self.single_window_mode));
                self.warning = Some(if self.single_window_mode {
                    "single-window mode ON — Enter opens in preview pane".to_string()
                } else {
                    "single-window mode OFF — Enter attaches full-screen".to_string()
                });
            }
            // `/` opens the type-ahead session picker. Mirrors fzf/
            // vim's convention for "start a filter". The app loop
            // populates it with the current managed sessions.
            (KeyCode::Char('/'), KeyModifiers::NONE)
                if self.modals.top_id() != Some("quickjump") =>
            {
                self.pending_modal = Some(ModalRequest::QuickJump);
            }
            // Tab: toggle collapse on a section header. Hides the
            // section's members in the rendered sidebar; the open/
            // closed state is persisted in `config.toml` so it
            // survives restarts. No-op when the cursor isn't on a
            // header.
            (KeyCode::Tab, _) => {
                if let Some(Location::Header(si)) = self.selected_location() {
                    let s = &mut self.sidebar.sections[si];
                    s.collapsed = !s.collapsed;
                    self.save_sidebar(out);
                    self.clamp_selection();
                }
            }
            // f: cycle the TDF banner font. On a section header it
            // sets that section's override (and clears it when the
            // override would equal the global). With no sessions yet
            // (empty splash), it cycles the global default. No-op
            // elsewhere — the cursor is on a session and there's no
            // banner being shown.
            (KeyCode::Char('f'), KeyModifiers::NONE) => {
                if let Some(Location::Header(si)) = self.selected_location() {
                    let global = crate::ui::banner::canonical(&self.banner_font);
                    let cur = self.sidebar.sections[si]
                        .banner_font
                        .as_deref()
                        .unwrap_or(global);
                    let nxt = crate::ui::banner::next(cur);
                    let s = &mut self.sidebar.sections[si];
                    s.banner_font = if nxt == global {
                        None
                    } else {
                        Some(nxt.to_string())
                    };
                    self.save_sidebar(out);
                } else if self.sessions.is_empty() && self.sidebar.is_empty() {
                    let nxt = crate::ui::banner::next(&self.banner_font);
                    self.banner_font = nxt.to_string();
                    out.push(Command::SaveBannerFont(nxt.to_string()));
                }
            }
            // `?` and `h` open the key-bindings cheat sheet. `h`
            // doesn't collide with anything else on the main list
            // (we use arrows / j-k for navigation, not h-l), so it's
            // free to double as a "help" mnemonic alongside `?`.
            (KeyCode::Char('?'), _) | (KeyCode::Char('h'), KeyModifiers::NONE)
                if self.modals.top_id() != Some("help") =>
            {
                self.pending_modal = Some(ModalRequest::Help);
            }
            // `e` opens the configured editor at the selected session's
            // path. Requires both an editor configured (`bosun editor
            // <cmd>` or `editor = "..."` in config.toml) and a session
            // with a known path — section headers and path-less rows
            // produce a status-bar warning instead.
            (KeyCode::Char('e'), KeyModifiers::NONE) => {
                let editor = match self.editor.clone() {
                    Some(e) => e,
                    None => {
                        self.warning = Some(
                            "no editor configured — run `bosun editor <cmd>` (e.g. zed, code)"
                                .into(),
                        );
                        return;
                    }
                };
                match self
                    .selected_session()
                    .and_then(|s| s.session.best_path().map(str::to_string))
                {
                    Some(path) => {
                        out.push(Command::OpenEditor { editor, path });
                    }
                    None => {
                        self.warning =
                            Some("no path on selected row — pick a session, not a header".into());
                    }
                }
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
        let moved = name.clone();
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
        if self.update_history_for(&moved) {
            self.save_session_history(out);
        }
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
        let moved = match loc {
            Location::Ungrouped(i) => {
                if self.sidebar.sections.is_empty() {
                    return;
                }
                let name = self.sidebar.ungrouped.remove(i);
                let m = name.clone();
                self.sidebar.sections[0].members.insert(0, name);
                self.selected = self.sidebar.flat_index(Location::Member(0, 0));
                Some(m)
            }
            Location::Member(si, mi) => {
                if si + 1 >= self.sidebar.sections.len() {
                    return;
                }
                let name = self.sidebar.sections[si].members.remove(mi);
                let m = name.clone();
                self.sidebar.sections[si + 1].members.insert(0, name);
                self.selected = self.sidebar.flat_index(Location::Member(si + 1, 0));
                Some(m)
            }
            Location::Header(_) => None,
        };
        if let Some(name) = moved {
            self.save_sidebar(out);
            if self.update_history_for(&name) {
                self.save_session_history(out);
            }
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
        let moved = match loc {
            Location::Ungrouped(_) => None, // already at leftmost bucket
            Location::Member(si, mi) => {
                let name = self.sidebar.sections[si].members.remove(mi);
                let m = name.clone();
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
                Some(m)
            }
            Location::Header(_) => None,
        };
        if let Some(name) = moved {
            self.save_sidebar(out);
            if self.update_history_for(&name) {
                self.save_session_history(out);
            }
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
    /// Also rewrites matching `session_history` entries so members keep
    /// their auto-restore association through the rename.
    pub fn rename_section(&mut self, id: &str, new_name: String, out: &mut Vec<Command>) {
        // Look up the old name before the rename so we can migrate
        // history entries from old → new.
        let old_name = self
            .sidebar
            .sections
            .iter()
            .find(|s| s.id == id)
            .map(|s| s.name.clone());
        if self.sidebar.rename_section(id, new_name.clone()) {
            self.save_sidebar(out);
            if let Some(old) = old_name {
                if old != new_name {
                    let mut changed = false;
                    for val in self.session_history.values_mut() {
                        if *val == old {
                            *val = new_name.clone();
                            changed = true;
                        }
                    }
                    if changed {
                        self.save_session_history(out);
                    }
                }
            }
        }
    }

    /// Map a mouse event onto the draggable divider or the session
    /// list scroll wheel.
    ///
    /// - `Down(Left)` on the divider column starts a drag.
    /// - `Drag(Left)` while `dragging_divider` updates `divider_x`
    ///   to the new column; `layout::compute` clamps it to sane
    ///   min-widths on the next render.
    /// - `Up(Left)` clears the drag flag regardless of location —
    ///   releasing the button anywhere ends the gesture.
    /// - `ScrollDown` / `ScrollUp` over the list rect step the
    ///   selection (same as j/k), throttled through `tick_scroll`
    ///   so a single trackpad gesture doesn't fly through the
    ///   list. Scroll-follows-selection in
    ///   `ui::session_list` makes the viewport scroll naturally,
    ///   which gives mobile clients (Termius one-finger pan, Blink
    ///   two-finger pan) a way to reach off-screen sessions when
    ///   the keyboard isn't ideal. Suppressed while a modal is
    ///   open so the wheel can't shift selection underneath it.
    ///
    /// Non-handled events and any event while `term_size` is unset
    /// (pre-first-draw) are ignored.
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
            // Inverted vs. crossterm's labels so trackpad gestures
            // feel natural on macOS (and on iOS/Android Termius +
            // Blink, where vertical pans report the same direction as
            // desktop natural scroll): swiping content downward shows
            // earlier items, swiping upward shows later items.
            MouseEventKind::ScrollDown if self.point_in_list(&layouts, m) => {
                self.tick_scroll(-1);
            }
            MouseEventKind::ScrollUp if self.point_in_list(&layouts, m) => {
                self.tick_scroll(1);
            }
            _ => {}
        }
    }

    /// Accumulate one wheel tick in the given direction (+1 = down,
    /// -1 = up). Every `SCROLL_TICKS_PER_STEP` ticks in one direction
    /// advances the selection by one row; the accumulator resets on
    /// direction change so a counter-flick takes effect immediately.
    fn tick_scroll(&mut self, dir: i32) {
        if dir.signum() != self.scroll_accum.signum() && self.scroll_accum != 0 {
            self.scroll_accum = 0;
        }
        self.scroll_accum += dir;
        while self.scroll_accum >= SCROLL_TICKS_PER_STEP {
            let len = self.sidebar.len();
            if len > 0 {
                self.selected = (self.selected + 1).min(len - 1);
            }
            self.scroll_accum -= SCROLL_TICKS_PER_STEP;
        }
        while self.scroll_accum <= -SCROLL_TICKS_PER_STEP {
            self.selected = self.selected.saturating_sub(1);
            self.scroll_accum += SCROLL_TICKS_PER_STEP;
        }
    }

    /// True iff the mouse event lands inside the session-list rect
    /// and no modal is open. Scroll-wheel nav uses this to ignore
    /// wheel events that happen over the preview pane or while a
    /// confirm/rename dialog is up.
    fn point_in_list(&self, layouts: &layout::Layouts, m: MouseEvent) -> bool {
        if !self.modals.is_empty() {
            return false;
        }
        let r = layouts.list;
        m.column >= r.x
            && m.column < r.x.saturating_add(r.width)
            && m.row >= r.y
            && m.row < r.y.saturating_add(r.height)
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
    /// Embedded terminal for the focused session's preview (2.0+).
    /// `None` when no session is focused, when the user has opted
    /// out via `embed_enabled = false`, or when the embed spawn
    /// failed (in which case the preview path falls back to the
    /// v0.4 polled snapshot — bosun stays useful even if PTY/tmux
    /// negotiation hits an edge case).
    embed: Option<crate::ui::embed_terminal::EmbedTerminal>,
    /// Sticky copy of `Config::embed_enabled`. `App::sync_embed`
    /// reads this on every iteration to decide whether to spawn.
    embed_enabled: bool,
    /// Step 4 focus mode (2.0+). When true, the embed is running
    /// in `AttachMode::Focused` (real attach, ignore-size) and the
    /// app loop routes all `AppMsg::Key` events straight into the
    /// embed's PTY writer instead of bosun's reducer. Ctrl-Q is
    /// intercepted to exit focus.
    embed_focused: bool,
    /// Tmux client. The tmux actor owns the primary copy and runs
    /// all timed / notification-driven tmux work; we keep this
    /// secondary handle so the app task itself can do synchronous
    /// `capture_pane` calls — currently used at embed spawn to
    /// prime the parser with the session's current screen, and at
    /// detach exit (v0.4.1) to snap the polled preview to current
    /// state before the next draw.
    client: Arc<dyn TmuxClient>,
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

        // Seed the recents cache from the store so dead sidebar rows
        // can render their proper display name and `R` can restart
        // them from their stored spec on first paint. Refreshed on
        // every `SessionsRefreshed`.
        let recents = store.list_recents(200).unwrap_or_default();

        let state = AppState {
            divider_x: config.divider_x,
            sidebar: config.sidebar.clone(),
            session_history: config.session_history.clone(),
            banner_font: config.banner_font.clone(),
            session_prefix: config.session_prefix.clone(),
            editor: config.editor.clone(),
            recents,
            single_window_mode: config.single_window_mode,
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
            embed: None,
            embed_enabled: config.embed_enabled,
            embed_focused: false,
            client,
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
            .draw(|f| ui::draw(f, &self.state, &self.theme, self.embed.as_ref()))
            .map_err(term_err)?;

        while !self.state.quit {
            let msg = match self.evt_rx.recv().await {
                Some(m) => m,
                None => break,
            };

            // Step 4 focus mode: while the embed is focused, all
            // `AppMsg::Key` events go directly into the embed's PTY
            // writer instead of bosun's reducer. Ctrl-Q is the
            // exit-focus chord (mirrors the existing tmux-attach
            // detach key). Non-key AppMsgs (Resize, refresh,
            // EmbedBytes, etc.) still flow through the normal paths
            // so layout / state stay current.
            if self.embed_focused {
                if let AppMsg::Key(k) = &msg {
                    use crossterm::event::{KeyCode, KeyModifiers};
                    let is_ctrl_q = matches!(k.code, KeyCode::Char('q'))
                        && k.modifiers.contains(KeyModifiers::CONTROL);
                    if is_ctrl_q {
                        self.exit_focus().await;
                    } else if let Some(bytes) = crate::ui::key_encode::encode(*k) {
                        if let Some(embed) = self.embed.as_mut() {
                            if let Err(e) = embed.write(&bytes) {
                                tracing::warn!("embed write: {}", e);
                                self.state.warning = Some(format!("focus: write failed: {e}"));
                            }
                        }
                    }
                    // Don't draw here — the next EmbedBytes chunk
                    // from the agent's echo / response will trigger
                    // the redraw. If the keystroke produces no echo
                    // (unusual), the screen is unchanged anyway.
                    continue;
                }
                if let AppMsg::Paste(text) = &msg {
                    // Wrap in bracketed-paste markers so apps that
                    // opted in (most modern shells, vim, Claude
                    // Code, etc.) treat the whole block as a paste
                    // rather than executing line-by-line. Outer
                    // terminals deliver drag-dropped file paths
                    // and image markers via this same path, so
                    // this is also "I dropped an image onto bosun"
                    // working correctly.
                    if let Some(embed) = self.embed.as_mut() {
                        let mut buf = Vec::with_capacity(text.len() + b"\x1b[200~\x1b[201~".len());
                        buf.extend_from_slice(b"\x1b[200~");
                        buf.extend_from_slice(text.as_bytes());
                        buf.extend_from_slice(b"\x1b[201~");
                        if let Err(e) = embed.write(&buf) {
                            tracing::warn!("embed paste write: {}", e);
                        }
                    }
                    continue;
                }
                if let AppMsg::Mouse(m) = &msg {
                    // Forward mouse events to the PTY only when:
                    //   (a) the inner app has enabled mouse tracking
                    //       (otherwise we'd dump SGR-1006 escape
                    //       bytes into a shell that interprets them
                    //       as literal text), and
                    //   (b) the event lands inside the preview /
                    //       embed rectangle (mouse over the sidebar
                    //       or status bar still goes to bosun for
                    //       divider drag etc).
                    // Coordinates are translated to embed-local
                    // 0-based; the encoder converts to the 1-based
                    // form SGR 1006 expects.
                    let wants = self.embed.as_ref().is_some_and(|e| e.wants_mouse());
                    if wants {
                        if let Some(area) = self.preview_rect() {
                            if point_in_rect(area, m.column, m.row) {
                                let local_col = m.column - area.x;
                                let local_row = m.row - area.y;
                                if let Some(bytes) =
                                    crate::ui::mouse_encode::encode(*m, local_col, local_row)
                                {
                                    if let Some(embed) = self.embed.as_mut() {
                                        if let Err(e) = embed.write(&bytes) {
                                            tracing::warn!("embed mouse write: {}", e);
                                        }
                                    }
                                }
                                continue;
                            }
                        }
                    }
                    // Mouse outside the embed area (or app doesn't
                    // want mouse): fall through to bosun's normal
                    // handler so divider drag etc. still works
                    // even while focused.
                }
            }

            // Fast path for embed PTY bytes. The reducer is pure and
            // AppState doesn't own the embed (it's runtime state on
            // the App struct), so we feed bytes here instead of
            // routing through `apply()`. Stale chunks from a previous
            // embed (session was switched between read and delivery)
            // are silently dropped. Render still happens at the
            // bottom of the branch so the new vt100 grid state shows
            // up on screen.
            //
            // Burst coalescing: when this chunk is the first of many
            // (tmux attach -r's initial pane repaint, a `cargo build`
            // flood, a Claude response that arrives in 20 chunks),
            // draining the rest of the queue into the parser before
            // drawing collapses the burst into one repaint instead
            // of N. Without coalescing the user sees the burst
            // animate over a couple of seconds; with it the final
            // screen state appears in a single frame. Non-embed
            // messages encountered during the drain are preserved
            // and re-sent so the normal flow handles them on the
            // next iteration.
            if let AppMsg::EmbedBytes { session, bytes } = msg {
                if let Some(embed) = self.embed.as_mut() {
                    if embed.session() == session {
                        embed.feed(&bytes);
                    }
                }
                let mut preserved: Vec<AppMsg> = Vec::new();
                use tokio::sync::mpsc::error::TryRecvError;
                loop {
                    match self.evt_rx.try_recv() {
                        Ok(AppMsg::EmbedBytes {
                            session: s2,
                            bytes: b2,
                        }) => {
                            if let Some(embed) = self.embed.as_mut() {
                                if embed.session() == s2 {
                                    embed.feed(&b2);
                                }
                            }
                        }
                        Ok(other) => preserved.push(other),
                        Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
                    }
                }
                for m in preserved {
                    let _ = self.evt_tx.send(m);
                }
                terminal
                    .draw(|f| ui::draw(f, &self.state, &self.theme, self.embed.as_ref()))
                    .map_err(term_err)?;
                continue;
            }

            // Intercept UI-only commands here before anything reaches
            // the tmux actor. Some commands (InsertSection, RenameSection)
            // emit follow-up commands (e.g. SaveSidebar) as part of
            // their handler; `queue` lets us re-enter the dispatch
            // without a recursive call.
            //
            // Recents change asynchronously (CreateSession upserts via
            // the actor; DeleteRecent runs in the actor too) and we
            // need them fresh in `AppState` so dead sidebar rows
            // resolve to display names and `R` can find the right
            // spec. Every `SessionsRefreshed` already runs after any
            // command that could mutate the recents table, so it's
            // the right edge to re-cache on.
            let should_reload_recents = matches!(msg, AppMsg::SessionsRefreshed { .. });
            let mut queue: Vec<Command> = self.state.apply(msg);
            if should_reload_recents {
                self.state.recents = self.store.list_recents(200).unwrap_or_default();
            }
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
                    Command::SaveSingleWindow(on) => {
                        if let Err(e) = crate::config::write_single_window(on) {
                            self.state.warning =
                                Some(format!("single-window: failed to save: {e}"));
                        }
                    }
                    Command::SaveSidebar(entries) => {
                        if let Err(e) = crate::config::write_sidebar(&entries) {
                            self.state.warning = Some(format!("sidebar: failed to save: {e}"));
                        }
                    }
                    Command::SaveSessionHistory(history) => {
                        if let Err(e) = crate::config::write_session_history(&history) {
                            self.state.warning = Some(format!("history: failed to save: {e}"));
                        }
                    }
                    Command::SaveBannerFont(name) => {
                        if let Err(e) = crate::config::write_banner_font(&name) {
                            self.state.warning = Some(format!("banner: failed to save: {e}"));
                        }
                    }
                    Command::InsertSection { name } => {
                        self.state.insert_section(name, &mut queue);
                    }
                    Command::RenameSection { id, new_name } => {
                        self.state.rename_section(&id, new_name, &mut queue);
                    }
                    Command::OpenEditor { editor, path } => {
                        // Fire-and-forget. Child stdio is detached to
                        // /dev/null so a chatty editor (`code .` prints
                        // to stderr on first launch) doesn't scribble
                        // over the alt-screen. The `Child` is dropped
                        // immediately — modern GUI editors fork their
                        // own daemon and the launcher exits in <50ms,
                        // so there's nothing to reap; the kernel
                        // reparents to init. Failures are surfaced as
                        // status-bar warnings.
                        use std::process::{Command as ProcCommand, Stdio};
                        let spawn = ProcCommand::new(&editor)
                            .arg(&path)
                            .stdin(Stdio::null())
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .spawn();
                        match spawn {
                            Ok(_child) => {
                                self.state.warning = Some(format!("opened {} in {}", path, editor));
                            }
                            Err(e) => {
                                self.state.warning = Some(format!("editor `{editor}` failed: {e}"));
                            }
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
                        let recents = self.store.list_recents(50).unwrap_or_default();
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
                    ModalRequest::QuickJump => {
                        // Snapshot the current managed sessions into
                        // QuickJumpRows. The modal owns its data — we
                        // don't re-query on refresh; the picker shows
                        // the list as of the moment it was opened.
                        let rows: Vec<QuickJumpRow> = self
                            .state
                            .sessions
                            .iter()
                            .map(|v| QuickJumpRow {
                                internal: v.name().to_string(),
                                display: v.display().to_string(),
                                agent: v.session.agent.clone(),
                                path: v.session.best_path().map(String::from),
                                attached: v.session.attached,
                            })
                            .collect();
                        self.state.modals.push(Box::new(QuickJumpModal::new(rows)));
                    }
                    ModalRequest::Help => {
                        self.state.modals.push(Box::new(HelpModal::new()));
                    }
                }
            }

            // If the reducer queued an attach, perform it now.
            //
            // Two paths depending on `single_window_mode`:
            //
            // - OFF (default): tear down the terminal, hand the tty
            //   to tmux, run a full-screen `tmux attach`. Sidebar
            //   disappears until the user detaches with Ctrl-Q.
            //   Matches v0.4 behavior.
            // - ON: route through `enter_focus`, which respawns the
            //   preview-pane embed in writable mode. Sidebar stays
            //   visible the whole time. The user's keys flow into
            //   the session through bosun's PTY writer. Ctrl-Q
            //   exits focus, same chord.
            //
            // The embed must be live (embed_enabled + spawn
            // succeeded) for the single-window path to make sense.
            // If it isn't, fall back to the full-screen path so
            // `Enter` still has a useful behavior.
            if let Some(name) = self.state.pending_attach.take() {
                let want_single_window = self.state.single_window_mode
                    && self.embed_enabled
                    && self
                        .embed
                        .as_ref()
                        .map(|e| e.session() == name)
                        .unwrap_or(false);
                if want_single_window {
                    self.enter_focus().await;
                    terminal
                        .draw(|f| ui::draw(f, &self.state, &self.theme, self.embed.as_ref()))
                        .map_err(term_err)?;
                    continue;
                }

                // Full-screen path — same as v0.4.
                //
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

                // Drop the embed before handing the terminal to tmux.
                // Two reasons: (1) the embed's reader thread would
                // otherwise keep queueing EmbedBytes into evt_rx for
                // the entire attach session — an attach to a busy
                // pane could accumulate hundreds of MB in the channel
                // before the user detaches. (2) On detach we want a
                // clean reattach with the parser cleared, so the
                // returning preview shows current state, not an
                // out-of-date scrollback. `sync_embed` re-spawns
                // automatically after the attach returns.
                self.embed = None;

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
                        // Bytes from the embed we just dropped (or
                        // from the brief window before the reader
                        // saw EOF) — silently discarded. The new
                        // embed `sync_embed` spawns will have its
                        // own clean parser.
                        Ok(AppMsg::EmbedBytes { .. }) => {}
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

            // Reconcile the embed against the current selection
            // (spawn / drop / resize on focus change or terminal
            // resize). Runs once per AppMsg, which covers every
            // selection-changing key + every Resize event. Awaits
            // because spawn now primes the parser with a
            // synchronous capture-pane snapshot.
            self.sync_embed().await;

            terminal
                .draw(|f| ui::draw(f, &self.state, &self.theme, self.embed.as_ref()))
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
            crossterm::event::DisableBracketedPaste,
        )
        .map_err(BosunError::Io)?;

        // 2. Install binding + run attach (blocking).
        let result = attach_with_ctrl_q_detach(self.socket.as_deref(), name);

        // 3. Re-enter raw mode / alt screen / mouse capture /
        //    bracketed paste regardless of attach result.
        crossterm::terminal::enable_raw_mode().map_err(BosunError::Io)?;
        execute!(
            terminal.backend_mut(),
            crossterm::terminal::EnterAlternateScreen,
            crossterm::event::EnableMouseCapture,
            crossterm::event::EnableBracketedPaste,
        )
        .map_err(BosunError::Io)?;
        terminal.clear().map_err(term_err)?;

        if let Err(e) = result {
            self.state.warning = Some(format!("attach: {}", e));
        }
        Ok(())
    }

    /// Reconcile the embed against the current selection. Called
    /// once per main-loop iteration after `apply()` returns, plus
    /// just after `perform_attach` returns. Decisions:
    /// - `embed_enabled == false` → no embed, drop any current one.
    /// - cursor not on a live session → no embed.
    /// - cursor on the same session as the current embed → resize
    ///   to the current preview area dims (idempotent if unchanged).
    /// - cursor on a different live session → drop old, spawn new.
    ///
    /// Spawn failure is logged and surfaced as a status-bar warning
    /// but is non-fatal — the preview falls back to the v0.4 polled
    /// snapshot path automatically (it's still drawn from
    /// `SessionView.preview`, which the fast-tick keeps populated).
    async fn sync_embed(&mut self) {
        if !self.embed_enabled {
            if self.embed.is_some() {
                self.embed = None;
            }
            return;
        }

        // `selected_session()` returns Some only when the cursor is
        // on a row that maps to a live SessionView — dead rows,
        // section headers, and the empty state all yield None,
        // which is the right "no embed" answer.
        let target = self.state.selected_session().map(|v| v.name().to_string());
        let current = self.embed.as_ref().map(|e| e.session().to_string());

        if target != current {
            self.embed = None;
            if let Some(t) = target {
                let (rows, cols) = self.preview_dims();
                // Synchronously snapshot the session's current pane
                // before spawning the embed, then prime the parser
                // with those bytes. Without this, the parser would
                // start blank and tmux's `attach -r` would stream
                // its initial repaint of the existing pane content
                // — the user sees that repaint render top-to-bottom
                // over a couple of seconds (visible "scrollback
                // replay" animation). Priming makes the very first
                // post-switch frame show the current state. Any
                // intermediate redraws caused by tmux's repaint
                // bytes resolve to the same final screen, so the
                // animation is invisible.
                let snapshot = match self.client.capture_pane(&t).await {
                    Ok(bytes) => Some(bytes),
                    Err(e) => {
                        tracing::debug!("embed prime capture-pane({t}): {e}");
                        None
                    }
                };
                // sync_embed always spawns in Preview mode. Focus
                // entry/exit (Step 4) is handled separately by
                // `App::set_embed_focus`, which respawns with
                // `AttachMode::Focused` while preserving the
                // currently-focused session.
                match crate::ui::embed_terminal::EmbedTerminal::spawn(
                    self.socket.as_deref(),
                    &t,
                    rows,
                    cols,
                    crate::ui::embed_terminal::AttachMode::Preview,
                    snapshot.as_deref(),
                    self.evt_tx.clone(),
                ) {
                    Ok(e) => self.embed = Some(e),
                    Err(err) => {
                        tracing::warn!("embed spawn failed for {}: {}", t, err);
                        self.state.warning = Some(format!("embed: {err}"));
                    }
                }
            }
            return;
        }

        // Same embed; ensure it's sized to the current preview area.
        // resize() short-circuits if dims are unchanged so this is
        // free on the steady-state path. Compute dims first so we
        // don't borrow self both mutably and immutably.
        let (rows, cols) = self.preview_dims();
        if let Some(embed) = self.embed.as_mut() {
            embed.resize(rows, cols);
        }
    }

    /// Switch the embed for the currently-selected session into
    /// `AttachMode::Focused`. Idempotent if already focused; no-op
    /// if there's no embed (focus has nothing to grab) or no live
    /// session under the cursor. Captures a fresh snapshot before
    /// the respawn so the focused embed's first frame is the same
    /// stable view the user just had in preview mode.
    async fn enter_focus(&mut self) {
        if self.embed_focused {
            return;
        }
        let Some(session) = self.state.selected_session().map(|v| v.name().to_string()) else {
            return;
        };
        if self.embed.is_none() {
            // Without an embed (embed_enabled=false, or spawn
            // failed), focus mode has nothing to attach to.
            return;
        }
        if let Err(e) = self
            .respawn_embed(&session, crate::ui::embed_terminal::AttachMode::Focused)
            .await
        {
            self.state.warning = Some(format!("focus: {e}"));
            return;
        }
        self.embed_focused = true;
        self.state.warning = Some("focus mode — Ctrl-Q to exit".to_string());
    }

    /// Switch the embed back to `AttachMode::Preview`. Mirrors
    /// `enter_focus`. Always clears `embed_focused`, even if the
    /// respawn itself failed — the user is no longer trying to
    /// drive the session through bosun, so we'd rather fall back
    /// to a polled preview than leave them stuck.
    async fn exit_focus(&mut self) {
        if !self.embed_focused {
            return;
        }
        self.embed_focused = false;
        let Some(session) = self.state.selected_session().map(|v| v.name().to_string()) else {
            // Session disappeared while focused — drop the embed
            // entirely; sync_embed will recreate it on the next
            // selection change.
            self.embed = None;
            return;
        };
        if let Err(e) = self
            .respawn_embed(&session, crate::ui::embed_terminal::AttachMode::Preview)
            .await
        {
            // Best-effort fallback to the polled path — drop the
            // embed and let the normal `sync_embed` flow on the
            // next iteration try to bring one back in Preview mode.
            tracing::warn!("exit_focus respawn: {e}");
            self.embed = None;
        }
        self.state.warning = None;
    }

    /// Internal: drop the current embed and spawn a fresh one for
    /// `session` in the given mode, priming with a synchronous
    /// capture-pane snapshot so the transition is a single repaint
    /// rather than the visible attach-replay animation. Used by
    /// `enter_focus` / `exit_focus` — `sync_embed` has its own
    /// inline spawn path because it also handles the no-target and
    /// resize-only cases.
    async fn respawn_embed(
        &mut self,
        session: &str,
        mode: crate::ui::embed_terminal::AttachMode,
    ) -> std::io::Result<()> {
        let (rows, cols) = self.preview_dims();
        let snapshot = self.client.capture_pane(session).await.ok();
        // Drop the old embed *before* spawning the new one. Both
        // attaches would otherwise briefly coexist on the same
        // tmux session, which works fine but pointlessly fans out
        // tmux's relay.
        self.embed = None;
        let embed = crate::ui::embed_terminal::EmbedTerminal::spawn(
            self.socket.as_deref(),
            session,
            rows,
            cols,
            mode,
            snapshot.as_deref(),
            self.evt_tx.clone(),
        )?;
        self.embed = Some(embed);
        Ok(())
    }

    /// Compute the current preview area dimensions in (rows, cols)
    /// from cached `term_size` + `divider_x`. Returns the minimums
    /// in the narrow-terminal case where there's no preview area at
    /// all — the embed grid stays sized to something `vt100` accepts
    /// even though no rendering happens.
    fn preview_dims(&self) -> (u16, u16) {
        match self.preview_rect() {
            Some(p) => (p.height, p.width),
            None => (4, 20),
        }
    }

    /// Full preview rectangle (in terminal coords) for the current
    /// layout. `None` on narrow terminals where the preview is
    /// hidden. Used by mouse forwarding to decide whether an event
    /// lands inside the embed area and to translate to local
    /// coordinates.
    fn preview_rect(&self) -> Option<ratatui::layout::Rect> {
        use ratatui::layout::Rect;
        let area = Rect {
            x: 0,
            y: 0,
            width: self.state.term_size.0,
            height: self.state.term_size.1,
        };
        crate::ui::layout::compute(area, self.state.divider_x).preview
    }
}

/// True iff `(col, row)` lands inside `rect`. Both ratatui `Rect`
/// and crossterm coords are 0-based + half-open, so this is the
/// standard containment check.
fn point_in_rect(rect: ratatui::layout::Rect, col: u16, row: u16) -> bool {
    col >= rect.x
        && col < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
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
    fn dead_sessions_persist_in_sidebar_across_refresh() {
        // Reboot scenario: tmux server died, the next refresh sees zero
        // live sessions. The sidebar must NOT shrink — entries are only
        // removed via explicit user action (kill / `d`). Selection
        // stays put because the row it points at still exists.
        let mut s = state_with(vec![ses("a"), ses("b"), ses("c")], 2);
        s.apply(refreshed(vec![ses("a")]));
        assert_eq!(s.sidebar.len(), 3, "dead entries must persist");
        assert_eq!(s.selected, 2, "selection stays on the same row");
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
    fn scroll_up_in_list_advances_selection() {
        // Direction inverted vs. crossterm's labels: ScrollUp advances.
        // Throttled at SCROLL_TICKS_PER_STEP events per row step.
        let mut s = state_with(vec![ses("a"), ses("b"), ses("c")], 0);
        s.term_size = (120, 30);
        // col 10 is comfortably inside the list rect at 120-col width.
        for _ in 0..SCROLL_TICKS_PER_STEP {
            s.apply(AppMsg::Mouse(mouse(MouseEventKind::ScrollUp, 10)));
        }
        assert_eq!(s.selected, 1);
        for _ in 0..(SCROLL_TICKS_PER_STEP * 5) {
            s.apply(AppMsg::Mouse(mouse(MouseEventKind::ScrollUp, 10)));
        }
        assert_eq!(s.selected, 2, "saturates at len-1");
    }

    #[test]
    fn scroll_down_in_list_retreats_selection() {
        let mut s = state_with(vec![ses("a"), ses("b"), ses("c")], 2);
        s.term_size = (120, 30);
        for _ in 0..SCROLL_TICKS_PER_STEP {
            s.apply(AppMsg::Mouse(mouse(MouseEventKind::ScrollDown, 10)));
        }
        assert_eq!(s.selected, 1);
        for _ in 0..(SCROLL_TICKS_PER_STEP * 5) {
            s.apply(AppMsg::Mouse(mouse(MouseEventKind::ScrollDown, 10)));
        }
        assert_eq!(s.selected, 0, "saturates at 0");
    }

    #[test]
    fn scroll_below_step_threshold_does_not_move() {
        let mut s = state_with(vec![ses("a"), ses("b"), ses("c")], 0);
        s.term_size = (120, 30);
        for _ in 0..(SCROLL_TICKS_PER_STEP - 1) {
            s.apply(AppMsg::Mouse(mouse(MouseEventKind::ScrollUp, 10)));
        }
        assert_eq!(s.selected, 0, "sub-threshold gesture must not step");
    }

    #[test]
    fn scroll_direction_change_resets_accumulator() {
        let mut s = state_with(vec![ses("a"), ses("b"), ses("c")], 0);
        s.term_size = (120, 30);
        // Build up almost a step forward, then flick the other way.
        for _ in 0..(SCROLL_TICKS_PER_STEP - 1) {
            s.apply(AppMsg::Mouse(mouse(MouseEventKind::ScrollUp, 10)));
        }
        s.apply(AppMsg::Mouse(mouse(MouseEventKind::ScrollDown, 10)));
        assert_eq!(s.selected, 0, "counter-flick wipes pending ticks");
    }

    #[test]
    fn scroll_over_preview_pane_ignored() {
        // At 120 cols with default split, the list ends at col 45 and
        // the preview starts at col 46. Wheel events over the preview
        // must not move the list selection.
        let mut s = state_with(vec![ses("a"), ses("b"), ses("c")], 0);
        s.term_size = (120, 30);
        for _ in 0..(SCROLL_TICKS_PER_STEP * 2) {
            s.apply(AppMsg::Mouse(mouse(MouseEventKind::ScrollUp, 80)));
        }
        assert_eq!(s.selected, 0);
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

    /// `?` opens the help modal.
    #[test]
    fn question_mark_requests_help_modal() {
        let mut s = state_with(vec![ses("a")], 0);
        s.apply(AppMsg::Key(key(KeyCode::Char('?'))));
        assert!(matches!(s.pending_modal, Some(ModalRequest::Help)));
    }

    /// `h` (with no modifiers) also opens the help modal.
    #[test]
    fn h_requests_help_modal() {
        let mut s = state_with(vec![ses("a")], 0);
        s.apply(AppMsg::Key(key(KeyCode::Char('h'))));
        assert!(matches!(s.pending_modal, Some(ModalRequest::Help)));
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

    /// Moving a session into a section records its display name in
    /// `session_history`.
    #[test]
    fn move_into_section_updates_history() {
        let mut s = AppState::default();
        s.sessions = vec![ses("bosun-abc")];
        s.sidebar = model(&["bosun-abc"], vec![section("g1", "Work", &[])]);
        s.selected = 0;

        // `1` jumps ungrouped bosun-abc into "Work".
        s.apply(AppMsg::Key(key(KeyCode::Char('1'))));

        // `sessions[0].display()` falls back to the internal name when no
        // display is set, so we check against that.
        assert_eq!(
            s.session_history.get("bosun-abc"),
            Some(&"Work".to_string())
        );
    }

    /// After a restart, a new session with the same display name as
    /// the old one lands back in its original section.
    #[test]
    fn restart_restores_section_via_history() {
        let mut s = AppState::default();
        // Simulate the post-restart `SessionsRefreshed`: the old
        // bosun-abc is gone, a new bosun-def appears with the same
        // display name. History already says "bosun-abc" was in "Work".
        s.session_history
            .insert("bosun-abc".to_string(), "Work".to_string());
        s.sidebar = model(&[], vec![section("g1", "Work", &[])]);

        s.apply(AppMsg::SessionsRefreshed {
            sessions: vec![ses("bosun-abc")],
            select_after: Some("bosun-abc".to_string()),
        });

        assert!(s.sidebar.ungrouped.is_empty());
        assert_eq!(s.sidebar.sections[0].members, vec!["bosun-abc".to_string()]);
    }

    /// Restart-swap: a pending swap captured at modal-confirm time
    /// rewrites the dead row's internal name to the new internal name
    /// in place on the next `SessionsRefreshed`, so the dead "? <name>"
    /// ghost doesn't survive above the freshly-created session.
    #[test]
    fn restart_swap_replaces_dead_row_in_place() {
        let mut s = AppState::default();
        s.sidebar = model(
            &["bosun-other"],
            vec![section("g1", "Work", &["bosun-abc"])],
        );
        s.pending_restart_swap = Some("bosun-abc".to_string());

        s.apply(AppMsg::SessionsRefreshed {
            sessions: vec![ses("bosun-other"), ses("bosun-def")],
            select_after: Some("bosun-def".to_string()),
        });

        assert_eq!(
            s.sidebar.sections[0].members,
            vec!["bosun-def".to_string()],
            "new internal inherits the dead row's slot"
        );
        assert_eq!(
            s.sidebar.ungrouped,
            vec!["bosun-other".to_string()],
            "no append of bosun-def to ungrouped"
        );
        assert!(s.pending_restart_swap.is_none(), "swap is consumed");
    }

    /// A pending swap survives intermediate `SessionsRefreshed`
    /// events that have no `select_after` (e.g. the refresh fired by
    /// the tmux monitor when the actor kills the old session, before
    /// it creates the replacement). Consuming the swap on those would
    /// strand the new session at the bottom of ungrouped instead of
    /// dropping it into the dead row's slot.
    #[test]
    fn restart_swap_survives_intermediate_refresh() {
        let mut s = AppState::default();
        s.sidebar = model(&["bosun-abc"], vec![]);
        s.pending_restart_swap = Some("bosun-abc".to_string());

        // First refresh: actor has killed the old session but not yet
        // created the new one. No `select_after`.
        s.apply(AppMsg::SessionsRefreshed {
            sessions: vec![],
            select_after: None,
        });
        assert_eq!(
            s.pending_restart_swap.as_deref(),
            Some("bosun-abc"),
            "swap must survive an intermediate refresh"
        );

        // Second refresh: new session created, `select_after` set.
        s.apply(AppMsg::SessionsRefreshed {
            sessions: vec![ses("bosun-def")],
            select_after: Some("bosun-def".to_string()),
        });
        assert!(s.pending_restart_swap.is_none(), "swap consumed");
        assert_eq!(
            s.sidebar.ungrouped,
            vec!["bosun-def".to_string()],
            "new internal landed in the old slot"
        );
    }

    /// Renaming a section rewrites matching history entries so the
    /// auto-restore association survives the rename.
    #[test]
    fn section_rename_migrates_history_entries() {
        let mut s = AppState::default();
        s.sidebar = model(&[], vec![section("g1", "Work", &[])]);
        s.session_history
            .insert("bosun-abc".to_string(), "Work".to_string());

        let mut out = Vec::new();
        s.rename_section("g1", "WorkStuff".to_string(), &mut out);

        assert_eq!(
            s.session_history.get("bosun-abc"),
            Some(&"WorkStuff".to_string())
        );
    }

    /// Deleting a section drops matching history entries (so a later
    /// recreate doesn't try to put them into a non-existent section).
    #[test]
    fn section_delete_drops_history_entries() {
        let mut s = AppState::default();
        s.sidebar = model(&[], vec![section("g1", "Work", &[])]);
        s.session_history
            .insert("bosun-abc".to_string(), "Work".to_string());
        s.selected = 0;

        s.apply(AppMsg::Key(key(KeyCode::Char('d'))));

        assert!(s.session_history.is_empty());
    }
}
