//! Smoke test against a real tmux server, isolated on a throwaway socket.
//! Guarded behind `--features tmux-it` so CI without tmux doesn't explode.

#![cfg(feature = "tmux-it")]

use bosun::tmux::{TmuxClient, TokioTmuxClient};

fn unique_socket() -> String {
    format!("bosun-it-{}-{}", std::process::id(), rand_suffix())
}

fn rand_suffix() -> String {
    // No rand dep — use nanoseconds from the monotonic clock.
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
        .to_string()
}

fn sh(args: &[&str]) {
    let status = std::process::Command::new("tmux")
        .args(args)
        .status()
        .expect("spawn tmux");
    assert!(status.success(), "tmux {:?} failed", args);
}

fn sh_ignore(args: &[&str]) {
    let _ = std::process::Command::new("tmux").args(args).status();
}

#[tokio::test(flavor = "current_thread")]
async fn list_sessions_against_real_tmux() {
    let sock = unique_socket();
    // Set up.
    sh(&["-L", &sock, "new-session", "-d", "-s", "alpha"]);
    sh(&["-L", &sock, "new-session", "-d", "-s", "beta"]);

    let client = TokioTmuxClient::with_socket(sock.clone());
    let sessions = client
        .list_sessions()
        .await
        .expect("list_sessions should succeed");

    let names: Vec<_> = sessions.iter().map(|s| s.name.clone()).collect();
    assert!(names.contains(&"alpha".to_string()), "got: {:?}", names);
    assert!(names.contains(&"beta".to_string()), "got: {:?}", names);

    // Tear down.
    sh_ignore(&["-L", &sock, "kill-server"]);
}

#[tokio::test(flavor = "current_thread")]
async fn empty_server_returns_empty_list_not_error() {
    let sock = unique_socket();
    // Don't start any sessions. `list-sessions` against an empty server
    // exits non-zero; our client must coerce that to Ok(vec![]).
    let client = TokioTmuxClient::with_socket(sock);
    let sessions = client
        .list_sessions()
        .await
        .expect("no-server is Ok(empty)");
    assert!(sessions.is_empty());
}
