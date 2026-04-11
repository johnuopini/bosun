//! Integration test: create a session via TokioTmuxClient and verify
//! tmux sees it, that @bosun_display is set, and that list-sessions
//! round-trips the display name field.

#![cfg(feature = "tmux-it")]

use std::time::{SystemTime, UNIX_EPOCH};

use bosun::tmux::{CreateSpec, TmuxClient, TokioTmuxClient};

fn unique_socket(tag: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("bosun-create-{}-{}-{}", tag, std::process::id(), nanos)
}

fn tmux(socket: &str, args: &[&str]) -> std::process::Output {
    std::process::Command::new("tmux")
        .arg("-L")
        .arg(socket)
        .args(args)
        .output()
        .expect("spawn tmux")
}

fn kill_server(socket: &str) {
    let _ = tmux(socket, &["kill-server"]);
}

#[tokio::test(flavor = "current_thread")]
async fn create_session_sets_display_name_and_appears_in_list() {
    let sock = unique_socket("basic");
    // Start with an empty server — create_session starts the first one.
    let client = TokioTmuxClient::with_socket(sock.clone());

    let spec = CreateSpec {
        name: "bosun-rasterfox-a1b2c3d4".to_string(),
        display_name: Some("rasterfox".to_string()),
        path: "/tmp".to_string(),
        command: String::new(), // default shell
        metadata: None,
    };

    let created = client.create_session(&spec).await.expect("create ok");
    assert_eq!(created, "bosun-rasterfox-a1b2c3d4");

    // list-sessions should return the session with the display_name populated.
    let sessions = client.list_sessions().await.expect("list ok");
    let ours = sessions
        .iter()
        .find(|s| s.name == "bosun-rasterfox-a1b2c3d4")
        .expect("session should exist");
    assert_eq!(ours.display_name.as_deref(), Some("rasterfox"));
    assert_eq!(ours.display(), "rasterfox");

    // Also verify via raw tmux that @bosun_display was set.
    let opt = tmux(
        &sock,
        &[
            "show-options",
            "-qv",
            "-t",
            "bosun-rasterfox-a1b2c3d4",
            "@bosun_display",
        ],
    );
    let value = String::from_utf8_lossy(&opt.stdout).trim().to_string();
    assert_eq!(value, "rasterfox");

    kill_server(&sock);
}

#[tokio::test(flavor = "current_thread")]
async fn create_session_without_display_name_does_not_set_option() {
    let sock = unique_socket("nodisplay");
    let client = TokioTmuxClient::with_socket(sock.clone());

    let spec = CreateSpec {
        name: "bosun-bare-deadbeef".to_string(),
        display_name: None,
        path: "/tmp".to_string(),
        command: String::new(),
        metadata: None,
    };
    client.create_session(&spec).await.expect("create ok");

    let sessions = client.list_sessions().await.expect("list ok");
    let ours = sessions
        .iter()
        .find(|s| s.name == "bosun-bare-deadbeef")
        .expect("session should exist");
    assert!(ours.display_name.is_none());
    // display() falls back to the internal name.
    assert_eq!(ours.display(), "bosun-bare-deadbeef");

    kill_server(&sock);
}
