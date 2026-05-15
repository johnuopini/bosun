//! Integration test for the Ctrl-Q root-keytable binding dance.
//!
//! This is the riskiest production code path — if Bosun ever leaves a stray
//! `bind-key -T root C-q` in a user's tmux server, their Ctrl-Q is hijacked
//! until they manually unbind or restart the server. So: actually spawn a
//! throwaway tmux server, install the binding, check list-keys shows it,
//! clear it, confirm it's gone.

#![cfg(feature = "tmux-it")]

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use bosun::tmux::attach::{
    clear_ctrl_q_bound, clear_session_cycle_bound, ensure_ctrl_q_bound,
    ensure_session_cycle_bound,
};

fn unique_socket(tag: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("bosun-bind-{}-{}-{}", tag, std::process::id(), nanos)
}

fn tmux(socket: &str, args: &[&str]) -> std::process::Output {
    Command::new("tmux")
        .arg("-L")
        .arg(socket)
        .args(args)
        .output()
        .expect("spawn tmux")
}

fn list_keys_root(socket: &str) -> String {
    let out = tmux(socket, &["list-keys", "-T", "root"]);
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn count_ctrl_q_bindings(socket: &str) -> usize {
    let keys = list_keys_root(socket);
    keys.lines().filter(|l| l.contains("C-q")).count()
}

fn list_keys_prefix(socket: &str) -> String {
    // `bind-key -n` aliases to `bind-key -T root`, so the cycle binds
    // live in the root table. List that one specifically — the
    // default `list-keys` also dumps `copy-mode` and `copy-mode-vi`,
    // which ship with their own built-in S-Left / S-Right bindings
    // for word-wise selection and would skew our count.
    let out = tmux(socket, &["list-keys", "-T", "root"]);
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// Count bindings that look like bosun's MRU cycle install — i.e. an
/// S-Left or S-Right entry whose body is `run-shell …`. tmux's defaults
/// bind these keys to `previous-window`/`next-window`, which our install
/// overwrites, so the bare-name count starts at 2 even on a fresh server.
fn count_cycle_bindings(socket: &str) -> usize {
    let keys = list_keys_prefix(socket);
    keys.lines()
        .filter(|l| (l.contains("S-Left") || l.contains("S-Right")) && l.contains("run-shell"))
        .count()
}

fn kill_server(socket: &str) {
    let _ = tmux(socket, &["kill-server"]);
}

#[test]
fn ensure_then_clear_leaves_no_binding() {
    let sock = unique_socket("solo");
    // Need a server to bind against — new-session -d starts one and a session.
    tmux(&sock, &["new-session", "-d", "-s", "dummy"]);

    let before = count_ctrl_q_bindings(&sock);
    assert_eq!(before, 0, "expected no C-q binding before test");

    ensure_ctrl_q_bound(Some(&sock));
    assert!(
        count_ctrl_q_bindings(&sock) >= 1,
        "expected C-q binding to be installed, got: {}",
        list_keys_root(&sock)
    );

    clear_ctrl_q_bound(Some(&sock));
    assert_eq!(
        count_ctrl_q_bindings(&sock),
        0,
        "expected C-q binding gone after clear, got:\n{}",
        list_keys_root(&sock)
    );

    kill_server(&sock);
}

#[test]
fn ensure_is_idempotent() {
    let sock = unique_socket("idem");
    tmux(&sock, &["new-session", "-d", "-s", "dummy"]);

    // Re-asserting the binding repeatedly (as the refresh tick does)
    // must stay at exactly one binding — tmux's bind-key overwrites.
    for _ in 0..5 {
        ensure_ctrl_q_bound(Some(&sock));
    }
    assert_eq!(
        count_ctrl_q_bindings(&sock),
        1,
        "repeated ensure_ctrl_q_bound should produce exactly 1 binding, got:\n{}",
        list_keys_root(&sock)
    );

    clear_ctrl_q_bound(Some(&sock));
    assert_eq!(count_ctrl_q_bindings(&sock), 0);

    kill_server(&sock);
}

#[test]
fn cycle_bindings_install_and_clear() {
    let sock = unique_socket("cycle");
    tmux(&sock, &["new-session", "-d", "-s", "dummy"]);

    assert_eq!(
        count_cycle_bindings(&sock),
        0,
        "expected no S-Left/S-Right bindings before test"
    );

    ensure_session_cycle_bound(Some(&sock));
    assert_eq!(
        count_cycle_bindings(&sock),
        2,
        "expected exactly 2 cycle bindings (S-Left + S-Right) after install, got:\n{}",
        list_keys_prefix(&sock)
    );

    // Idempotent re-assert keeps the count at 2.
    for _ in 0..3 {
        ensure_session_cycle_bound(Some(&sock));
    }
    assert_eq!(
        count_cycle_bindings(&sock),
        2,
        "repeated ensure_session_cycle_bound must not duplicate bindings, got:\n{}",
        list_keys_prefix(&sock)
    );

    clear_session_cycle_bound(Some(&sock));
    assert_eq!(
        count_cycle_bindings(&sock),
        0,
        "expected cycle bindings gone after clear, got:\n{}",
        list_keys_prefix(&sock)
    );

    kill_server(&sock);
}
