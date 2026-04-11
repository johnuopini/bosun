//! Integration test for the tmux status bar install/sync/uninstall cycle.
//!
//! Spawns an isolated tmux server on a unique `-L socket`, asserts that
//! install overwrites status-left with the bosun brand, that sync_sessions
//! pushes a session list into status-right, that prefix-1 gets bound to
//! the first session, and that uninstall restores the original options
//! and removes the bindings.

#![cfg(feature = "tmux-it")]

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use bosun::tmux::status_bar::{install, sync_sessions};

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

fn show_opt(socket: &str, opt: &str) -> String {
    let out = tmux(socket, &["show-options", "-gqv", opt]);
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
fn install_overwrites_status_left_and_uninstall_restores() {
    let sock = unique_socket("cycle");
    tmux(&sock, &["new-session", "-d", "-s", "main"]);

    // Baseline: tmux default status-left is "[#S] " on 3.6a — we don't
    // assert on the exact value, just capture it for comparison later.
    let before_left = show_opt(&sock, "status-left");

    let guard = install(Some(&sock)).expect("install ok");

    // After install, status-left should contain our brand and refcount=1.
    let after_left = show_opt(&sock, "status-left");
    assert!(
        after_left.contains("bosun"),
        "status-left should contain 'bosun', got: {}",
        after_left
    );
    assert_eq!(show_opt(&sock, "@bosun_sb_refcount"), "1");

    // Saved options should hold the original values.
    assert_eq!(show_opt(&sock, "@bosun_saved_status_left"), before_left);

    // sync_sessions: push a two-session list and assert bindings land.
    sync_sessions(
        Some(&sock),
        &[("main".to_string(), true), ("other".to_string(), false)],
    )
    .expect("sync ok");

    let status_right = show_opt(&sock, "status-right");
    assert!(
        status_right.contains("1:main"),
        "status-right missing '1:main', got: {}",
        status_right
    );
    assert!(
        status_right.contains("2:other"),
        "status-right missing '2:other', got: {}",
        status_right
    );

    let keys = list_prefix_keys(&sock);
    assert!(
        keys.contains("bind-key -T prefix 1")
            || keys
                .lines()
                .any(|l| l.contains("prefix") && l.contains(" 1 ")),
        "expected prefix-1 binding, list-keys output:\n{}",
        keys
    );

    // Release the guard — should decrement refcount to 0 and restore.
    guard.release().expect("release ok");

    assert_eq!(
        show_opt(&sock, "@bosun_sb_refcount"),
        "",
        "refcount should be unset after last release"
    );

    let restored_left = show_opt(&sock, "status-left");
    assert!(
        !restored_left.contains("bosun"),
        "status-left should be restored, still contains 'bosun': {}",
        restored_left
    );

    // Bindings should be gone from the prefix table.
    let keys_after = list_prefix_keys(&sock);
    let bosun_bindings_left = keys_after
        .lines()
        .filter(|l| l.contains("switch-client -t \"main\""))
        .count();
    assert_eq!(
        bosun_bindings_left, 0,
        "bosun jump bindings should be unbound after release, got:\n{}",
        keys_after
    );

    kill_server(&sock);
}

#[test]
fn nested_install_refcount_holds_bar_until_last_release() {
    let sock = unique_socket("nested");
    tmux(&sock, &["new-session", "-d", "-s", "main"]);

    let g1 = install(Some(&sock)).expect("install 1");
    assert_eq!(show_opt(&sock, "@bosun_sb_refcount"), "1");

    let g2 = install(Some(&sock)).expect("install 2");
    assert_eq!(show_opt(&sock, "@bosun_sb_refcount"), "2");
    assert!(show_opt(&sock, "status-left").contains("bosun"));

    // First release decrements but does not restore.
    g2.release().expect("release 2");
    assert_eq!(show_opt(&sock, "@bosun_sb_refcount"), "1");
    assert!(
        show_opt(&sock, "status-left").contains("bosun"),
        "bar should still be active with refcount=1"
    );

    // Second release restores.
    g1.release().expect("release 1");
    assert_eq!(show_opt(&sock, "@bosun_sb_refcount"), "");
    assert!(!show_opt(&sock, "status-left").contains("bosun"));

    kill_server(&sock);
}
