//! Integration test for the per-session tmux status bar API.
//!
//! Spawns an isolated tmux server on a unique `-L socket`, asserts
//! that `configure_session` writes per-session status-* options, that
//! `install_globals` binds prefix-1..9, and that `uninstall_globals`
//! removes those bindings. Also verifies that configuring one session
//! does NOT touch the global status-left (agent-deck isolation).

#![cfg(feature = "tmux-it")]

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use bosun::tmux::status_bar::{configure_session, install_globals, uninstall_globals, BarSession};

fn unique_socket(tag: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("bosun-sb-{}-{}-{}", tag, std::process::id(), nanos)
}

fn tmux(socket: &str, args: &[&str]) -> std::process::Output {
    Command::new("tmux")
        .arg("-L")
        .arg(socket)
        .args(args)
        .output()
        .expect("spawn tmux")
}

fn show_global(socket: &str, opt: &str) -> String {
    let out = tmux(socket, &["show-options", "-gqv", opt]);
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn show_session(socket: &str, session: &str, opt: &str) -> String {
    let out = tmux(socket, &["show-options", "-qv", "-t", session, opt]);
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn list_prefix_keys(socket: &str) -> String {
    let out = tmux(socket, &["list-keys", "-T", "prefix"]);
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn kill_server(socket: &str) {
    let _ = tmux(socket, &["kill-server"]);
}

#[test]
fn configure_session_writes_per_session_options_only() {
    let sock = unique_socket("persession");
    tmux(&sock, &["new-session", "-d", "-s", "bosun-main"]);
    tmux(&sock, &["new-session", "-d", "-s", "legacy"]);

    // Capture the global status-left baseline (should be tmux default)
    // and the legacy session's status-left (which inherits it).
    let global_before = show_global(&sock, "status-left");
    let legacy_before = show_session(&sock, "legacy", "status-left");

    let sessions = vec![BarSession {
        internal: "bosun-main".to_string(),
        display: "main".to_string(),
        attached: true,
    }];
    configure_session(Some(&sock), "bosun-main", &sessions).expect("configure ok");

    // bosun-main should now carry bosun's per-session status bar. The
    // left segment was dropped in 2.0 (the session name lives in the
    // tab strip and the brand in bosun's own TUI footer), so the
    // distinctive per-session signal is the status-right hint.
    let bosun_left = show_session(&sock, "bosun-main", "status-left");
    assert_eq!(
        bosun_left, "",
        "bosun-main status-left is empty by design in 2.0, got: {}",
        bosun_left
    );
    let bosun_right = show_session(&sock, "bosun-main", "status-right");
    assert!(
        bosun_right.contains("detach") && bosun_right.contains("jump"),
        "bosun-main status-right should carry the per-session hint, got: {}",
        bosun_right
    );

    // Global status-left should be UNCHANGED. This is the core of the
    // per-session refactor — we don't pollute the user's globals.
    let global_after = show_global(&sock, "status-left");
    assert_eq!(
        global_before, global_after,
        "configure_session must not touch global status-left"
    );

    // The legacy session should still show its inherited (global)
    // status-left, which is unchanged from the baseline.
    let legacy_after = show_session(&sock, "legacy", "status-left");
    assert_eq!(
        legacy_before, legacy_after,
        "legacy session's status-left must not change"
    );
    assert!(
        !legacy_after.contains("bosun"),
        "legacy session should not get bosun branding, got: {}",
        legacy_after
    );

    kill_server(&sock);
}

#[test]
fn install_and_uninstall_globals_bind_prefix_digits() {
    let sock = unique_socket("globals");
    tmux(&sock, &["new-session", "-d", "-s", "bosun-one"]);
    tmux(&sock, &["new-session", "-d", "-s", "bosun-two"]);

    let sessions = vec![
        BarSession {
            internal: "bosun-one".to_string(),
            display: "one".to_string(),
            attached: true,
        },
        BarSession {
            internal: "bosun-two".to_string(),
            display: "two".to_string(),
            attached: false,
        },
    ];
    install_globals(Some(&sock), &sessions).expect("install globals ok");

    let keys = list_prefix_keys(&sock);
    assert!(
        keys.lines()
            .any(|l| l.contains("prefix") && l.contains(" 1 ") && l.contains("switch-client")),
        "expected a prefix-1 switch-client binding, got:\n{}",
        keys
    );
    assert!(
        keys.lines()
            .any(|l| l.contains("prefix") && l.contains(" 2 ") && l.contains("switch-client")),
        "expected a prefix-2 switch-client binding, got:\n{}",
        keys
    );

    // The bindings should target the INTERNAL names (bosun-one,
    // bosun-two), not display names. This is the whole point of
    // BarSession vs the old (display, attached) tuple.
    // tmux list-keys may rendertargets with or without quotes, so
    // match flexibly.
    assert!(
        keys.contains("bosun-one") && !keys.contains("switch-client -t \"one\""),
        "expected binding to target internal name bosun-one, got:\n{}",
        keys
    );
    assert!(
        keys.contains("bosun-two") && !keys.contains("switch-client -t \"two\""),
        "expected binding to target internal name bosun-two, got:\n{}",
        keys
    );

    uninstall_globals(Some(&sock));

    let keys_after = list_prefix_keys(&sock);
    let jump_bindings = keys_after
        .lines()
        .filter(|l| l.contains("switch-client -t \"bosun-"))
        .count();
    assert_eq!(
        jump_bindings, 0,
        "bosun jump bindings should be gone after uninstall, got:\n{}",
        keys_after
    );

    kill_server(&sock);
}
