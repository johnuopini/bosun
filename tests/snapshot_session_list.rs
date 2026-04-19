//! Snapshot test: render the session list panel against a `TestBackend`
//! and compare the visible text grid against an `insta` snapshot. This is
//! the regression net for UI layout — any unintended change in row content,
//! selection rendering, or layout math trips the snapshot.

use std::time::SystemTime;

use bosun::app::AppState;
use bosun::sidebar::SidebarEntry;
use bosun::tmux::detector::Status;
use bosun::tmux::session::SessionView;
use bosun::tmux::TmuxSession;
use bosun::ui::Theme;
use ratatui::backend::TestBackend;
use ratatui::Terminal;

/// Build an AppState where the sidebar ordering mirrors the given
/// sessions (each as a `Session` entry, no headers). For snapshot
/// tests that only care about pre-grouping rendering.
fn state_with(sessions: Vec<SessionView>) -> AppState {
    let sidebar_entries: Vec<SidebarEntry> = sessions
        .iter()
        .map(|s| SidebarEntry::session(s.name()))
        .collect();
    AppState {
        sessions,
        sidebar_entries,
        ..Default::default()
    }
}

fn ses(name: &str, attached: bool) -> SessionView {
    ses_with_status(name, attached, Status::Idle)
}

fn ses_with_status(name: &str, attached: bool, status: Status) -> SessionView {
    SessionView::new(
        TmuxSession {
            name: name.into(),
            display_name: None,
            windows: 1,
            attached,
            created: Some(SystemTime::UNIX_EPOCH),
            last_activity: Some(SystemTime::UNIX_EPOCH),
            current_path: Some("/tmp".into()),
            agent: Some("claude".into()),
            // `/tmp/work` is chosen deliberately: the session_list meta
            // line runs `shorten_path` which replaces a `$HOME` prefix
            // with `~`. Any path under a real home dir would become
            // `~/...` on the dev machine and stay literal on CI, so the
            // snapshot would drift across machines. `/tmp` can never be
            // anyone's HOME, so both environments render identically.
            spec_path: Some("/tmp/work".into()),
        },
        status,
        None,
    )
}

fn render(state: &AppState, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = Theme::default_opencode();
    terminal
        .draw(|f| bosun::ui::draw(f, state, &theme))
        .unwrap();
    // TestBackend exposes a Buffer; dump the visible characters.
    let buf = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..buf.area().height {
        for x in 0..buf.area().width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

#[test]
fn empty_session_list() {
    let state = AppState::default();
    let frame = render(&state, 80, 10);
    insta::assert_snapshot!("empty_session_list", frame);
}

#[test]
fn three_sessions_with_middle_selected() {
    let mut state = state_with(vec![
        ses("alpha", false),
        ses("beta", true),
        ses("gamma", false),
    ]);
    state.selected = 1;
    let frame = render(&state, 80, 10);
    insta::assert_snapshot!("three_sessions_middle_selected", frame);
}

#[test]
fn warning_shows_in_statusbar() {
    let mut state = state_with(vec![ses("alpha", false)]);
    state.warning = Some("list: tmux not running".to_string());
    let frame = render(&state, 80, 6);
    insta::assert_snapshot!("warning_in_statusbar", frame);
}

#[test]
fn mixed_statuses_render_glyphs() {
    let mut state = state_with(vec![
        ses_with_status("build", false, Status::Running),
        ses_with_status("review", true, Status::Waiting),
        ses_with_status("shell", false, Status::Idle),
        ses_with_status("crashed", false, Status::Error),
    ]);
    state.selected = 1;
    let frame = render(&state, 80, 10);
    insta::assert_snapshot!("mixed_statuses", frame);
}

#[test]
fn sections_group_sessions() {
    let mut state = AppState {
        sessions: vec![
            ses("alpha", false),
            ses("beta", false),
            ses("gamma", true),
        ],
        selected: 1, // on the section header
        ..Default::default()
    };
    state.sidebar_entries = vec![
        SidebarEntry::session("alpha"),
        SidebarEntry::Section {
            id: "g1".into(),
            name: "Premium Products".into(),
        },
        SidebarEntry::session("beta"),
        SidebarEntry::session("gamma"),
    ];
    let frame = render(&state, 80, 12);
    insta::assert_snapshot!("sections_group_sessions", frame);
}
