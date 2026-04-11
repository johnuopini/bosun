//! Snapshot test: render the session list panel against a `TestBackend`
//! and compare the visible text grid against an `insta` snapshot. This is
//! the regression net for UI layout — any unintended change in row content,
//! selection rendering, or layout math trips the snapshot.

use std::time::SystemTime;

use bosun::app::AppState;
use bosun::tmux::TmuxSession;
use ratatui::backend::TestBackend;
use ratatui::Terminal;

fn ses(name: &str, attached: bool) -> TmuxSession {
    TmuxSession {
        name: name.into(),
        windows: 1,
        attached,
        created: Some(SystemTime::UNIX_EPOCH),
        last_activity: Some(SystemTime::UNIX_EPOCH),
        current_path: Some("/tmp".into()),
    }
}

fn render(state: &AppState, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| bosun::ui::draw(f, state)).unwrap();
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
    let state = AppState {
        sessions: vec![ses("alpha", false), ses("beta", true), ses("gamma", false)],
        selected: 1,
        ..Default::default()
    };
    let frame = render(&state, 80, 10);
    insta::assert_snapshot!("three_sessions_middle_selected", frame);
}

#[test]
fn warning_shows_in_statusbar() {
    let state = AppState {
        sessions: vec![ses("alpha", false)],
        warning: Some("list: tmux not running".to_string()),
        ..Default::default()
    };
    let frame = render(&state, 80, 6);
    insta::assert_snapshot!("warning_in_statusbar", frame);
}
