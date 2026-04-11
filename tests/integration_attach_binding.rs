//! Integration test for the Ctrl-Q root-keytable binding dance.
//!
//! This is the riskiest production code path — if Bosun ever leaves a stray
//! `bind-key -T root C-q` in a user's tmux server, their Ctrl-Q is hijacked
//! until they manually unbind or restart the server. So: actually spawn a
//! throwaway tmux server, install the binding, check list-keys shows it,
//! uninstall, confirm it's gone. Twice, to check refcount semantics.

#![cfg(feature = "tmux-it")]

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use bosun::tmux::attach::install_detach_key_for_test;

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

fn kill_server(socket: &str) {
    let _ = tmux(socket, &["kill-server"]);
}

#[test]
fn install_then_release_leaves_no_binding() {
    let sock = unique_socket("solo");
    // Need a server to bind against — new-session -d starts one and a session.
    tmux(&sock, &["new-session", "-d", "-s", "dummy"]);

    let before = count_ctrl_q_bindings(&sock);
    assert_eq!(before, 0, "expected no C-q binding before test");

    let guard = install_detach_key_for_test(Some(&sock)).expect("install ok");
    assert!(
        count_ctrl_q_bindings(&sock) >= 1,
        "expected C-q binding to be installed, got: {}",
        list_keys_root(&sock)
    );

    guard.release().expect("release ok");
    assert_eq!(
        count_ctrl_q_bindings(&sock),
        0,
        "expected C-q binding gone after release, got:\n{}",
        list_keys_root(&sock)
    );

    kill_server(&sock);
}

// Phase 5 TODO: add a refcount-based test for multi-instance safety
// when we reintroduce `@bosun_attach_refcount`. Phase 1 uses a single
// bind/unbind per attach for lower latency.

#[test]
fn drop_without_release_still_unbinds() {
    let sock = unique_socket("drop");
    tmux(&sock, &["new-session", "-d", "-s", "dummy"]);

    {
        let _g = install_detach_key_for_test(Some(&sock)).expect("install ok");
        assert!(count_ctrl_q_bindings(&sock) >= 1);
        // _g dropped here — Drop impl must unbind.
    }
    assert_eq!(
        count_ctrl_q_bindings(&sock),
        0,
        "Drop didn't clean up binding; list-keys:\n{}",
        list_keys_root(&sock)
    );

    kill_server(&sock);
}
