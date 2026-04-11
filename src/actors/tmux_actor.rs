//! The sole owner of the tmux client + per-session smoothing state.
//!
//! Each `Command::ListNow` triggers a full refresh pass: list sessions,
//! capture each pane, run the detector registry, smooth via per-session
//! hysteresis, and send a single `SessionsRefreshed` back to the app.
//!
//! This actor also owns the lifecycle of the bosun-branded tmux status
//! bar. Per-session status-* options are written on each change; the
//! global prefix-1..9 jump bindings are installed when bosun first
//! sees a managed session and removed by `GlobalsGuard::drop` when the
//! actor's task ends.
//!
//! `Command::FocusPreview` lets the app prioritize capturing a specific
//! session ahead of its next scheduled tick — e.g. when the user moves
//! the selection and we want the preview to update now rather than in
//! up to a second.
//!
//! Attach stays handled inline by the app task (needs the controlling
//! tty). This actor only handles read-only operations and the status
//! bar side effects.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::SystemTime;

use tokio::sync::mpsc;

use crate::config::Config;
use crate::events::{AppMsg, ClaudeSessionMode, Command, SessionSpec, SpecOptions};
use crate::tmux::detector::{DetectContext, DetectorRegistry, Status};
use crate::tmux::session::SessionView;
use crate::tmux::status_bar::{self, BarSession};
use crate::tmux::{CreateSpec, TmuxClient};
use crate::util::hysteresis::Smoother;

/// RAII cleanup for globals installed by the status bar (prefix-1..9
/// bindings). Per-session status-* options are left in place when the
/// actor exits — they die with their sessions, and leaving them means
/// a restarting bosun can reuse them without a reinit flash.
struct GlobalsGuard {
    socket: Option<String>,
    installed: bool,
}

impl Drop for GlobalsGuard {
    fn drop(&mut self) {
        if self.installed {
            status_bar::uninstall_globals(self.socket.as_deref());
        }
    }
}

pub fn spawn(
    client: Arc<dyn TmuxClient>,
    socket: Option<String>,
    config: Config,
    mut cmd_rx: mpsc::Receiver<Command>,
    evt_tx: mpsc::Sender<AppMsg>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let registry = DetectorRegistry::default_stack();
        let mut smoothers: HashMap<String, Smoother> = HashMap::new();
        let mut focused: Option<String> = None;
        let mut last_bar_state: Vec<BarSession> = Vec::new();
        let mut globals = GlobalsGuard {
            socket: socket.clone(),
            installed: false,
        };

        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                Command::ListNow => {
                    let views = refresh_all(
                        &*client,
                        &config,
                        &registry,
                        &mut smoothers,
                        focused.as_deref(),
                    )
                    .await;
                    match views {
                        Ok(views) => {
                            smoothers.retain(|name, _| views.iter().any(|v| v.name() == name));

                            // Only sync the status bar when the set of
                            // (internal, display, attached) tuples has
                            // actually changed. Skips the ~N*7 set-option
                            // calls on ticks where nothing's moved.
                            let state: Vec<BarSession> = views
                                .iter()
                                .map(|v| BarSession {
                                    internal: v.name().to_string(),
                                    display: v.display().to_string(),
                                    attached: v.session.attached,
                                })
                                .collect();
                            if !bar_state_equal(&state, &last_bar_state) {
                                sync_status_bar(socket.as_deref(), &state, &mut globals);
                                last_bar_state = state;
                            }

                            if evt_tx.send(AppMsg::SessionsRefreshed(views)).await.is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            if evt_tx
                                .send(AppMsg::Warn(format!("list: {}", e)))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                }
                Command::FocusPreview { name } => {
                    focused = Some(name);
                }
                Command::CreateSession(spec) => {
                    match create_session(&*client, &config, spec).await {
                        Ok(internal_name) => {
                            focused = Some(internal_name);
                            // Force an immediate refresh so the new session
                            // pops into the list without waiting for the
                            // next tick.
                            let _ = evt_tx
                                .send(AppMsg::Warn(format!(
                                    "created {}",
                                    focused.as_deref().unwrap_or("")
                                )))
                                .await;
                            // Re-enter the loop head; the app loop will
                            // issue a ListNow on the next tick, and our
                            // own debounced refresh handling takes it from
                            // there. To avoid a 1s delay, kick one now:
                            let _ = do_refresh(
                                &*client,
                                &config,
                                &registry,
                                &mut smoothers,
                                focused.as_deref(),
                                socket.as_deref(),
                                &mut last_bar_state,
                                &mut globals,
                                &evt_tx,
                            )
                            .await;
                        }
                        Err(e) => {
                            let _ = evt_tx.send(AppMsg::Warn(format!("create: {}", e))).await;
                        }
                    }
                }
                Command::Attach { .. } => {
                    tracing::warn!("tmux_actor received Attach — ignored; app task handles attach");
                }
                Command::Shutdown => break,
            }
        }

        // `globals` drops here → uninstall_globals runs.
        drop(globals);
    })
}

/// Assemble the internal tmux session name from the user's typed
/// display name. Internal format: `<prefix><display>-<hex-suffix>`,
/// e.g. `bosun-rasterfox-a1b2c3d4`. The hex suffix makes the internal
/// name unique even if the user creates two sessions with the same
/// display name.
fn build_internal_name(prefix: &str, display: &str) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let suffix = format!("{:08x}", nanos as u32);
    let sanitized: String = display
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!("{}{}-{}", prefix, sanitized, suffix)
}

/// Map the agent + options + extra args into a shell command to run
/// as the new session's initial process. `terminal` means "just the
/// shell" — options are ignored, and any `args` are run as a raw
/// command (empty = default shell).
fn build_agent_command(agent: &str, options: &SpecOptions, args: &str) -> String {
    let args = args.trim();
    match agent {
        "claude" => {
            let mut parts: Vec<String> = vec!["claude".to_string()];
            match options.claude.session_mode {
                ClaudeSessionMode::New => {}
                ClaudeSessionMode::Continue => parts.push("--continue".to_string()),
                ClaudeSessionMode::Resume => parts.push("--resume".to_string()),
            }
            if options.claude.skip_permissions {
                parts.push("--dangerously-skip-permissions".to_string());
            }
            if !args.is_empty() {
                parts.push(args.to_string());
            }
            parts.join(" ")
        }
        "codex" => {
            let mut parts: Vec<String> = vec!["codex".to_string()];
            if options.codex.yolo {
                parts.push("--yolo".to_string());
            }
            if !args.is_empty() {
                parts.push(args.to_string());
            }
            parts.join(" ")
        }
        // "terminal" and anything unknown → whatever extra args the
        // user typed, or nothing (tmux uses the default shell).
        _ => args.to_string(),
    }
}

async fn create_session(
    client: &dyn TmuxClient,
    config: &Config,
    spec: SessionSpec,
) -> crate::error::Result<String> {
    let internal = build_internal_name(&config.session_prefix, &spec.name);
    let command = build_agent_command(&spec.agent, &spec.options, &spec.args);
    let create = CreateSpec {
        name: internal.clone(),
        display_name: Some(spec.name.clone()),
        path: spec.path.clone(),
        command,
    };
    client.create_session(&create).await.map(|_| internal)
}

#[allow(clippy::too_many_arguments)]
async fn do_refresh(
    client: &dyn TmuxClient,
    config: &Config,
    registry: &DetectorRegistry,
    smoothers: &mut HashMap<String, Smoother>,
    focused: Option<&str>,
    socket: Option<&str>,
    last_bar_state: &mut Vec<BarSession>,
    globals: &mut GlobalsGuard,
    evt_tx: &mpsc::Sender<AppMsg>,
) -> crate::error::Result<()> {
    let views = refresh_all(client, config, registry, smoothers, focused).await?;
    smoothers.retain(|name, _| views.iter().any(|v| v.name() == name));

    let state: Vec<BarSession> = views
        .iter()
        .map(|v| BarSession {
            internal: v.name().to_string(),
            display: v.display().to_string(),
            attached: v.session.attached,
        })
        .collect();
    if !bar_state_equal(&state, last_bar_state) {
        sync_status_bar(socket, &state, globals);
        *last_bar_state = state;
    }

    let _ = evt_tx.send(AppMsg::SessionsRefreshed(views)).await;
    Ok(())
}

fn bar_state_equal(a: &[BarSession], b: &[BarSession]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b.iter()).all(|(x, y)| {
        x.internal == y.internal && x.display == y.display && x.attached == y.attached
    })
}

fn sync_status_bar(socket: Option<&str>, sessions: &[BarSession], globals: &mut GlobalsGuard) {
    // Install the global prefix-1..9 bindings on first non-empty state.
    if !globals.installed && !sessions.is_empty() {
        if let Err(e) = status_bar::install_globals(socket, sessions) {
            tracing::warn!("status bar: install_globals failed: {}", e);
            return;
        }
        globals.installed = true;
    } else if globals.installed {
        // Already installed — rebind in case the list changed.
        if let Err(e) = status_bar::install_globals(socket, sessions) {
            tracing::warn!("status bar: rebind jump keys failed: {}", e);
        }
    }

    // Apply per-session status-* options. Bosun only touches sessions
    // it manages; everything else keeps whatever bar it had.
    for entry in sessions {
        if let Err(e) = status_bar::configure_session(socket, &entry.internal, sessions) {
            tracing::warn!(
                "status bar: configure_session {} failed: {}",
                entry.internal,
                e
            );
        }
    }
}

/// One full refresh pass: list, filter by the configured prefix,
/// capture (with preview for focused), detect, smooth. Returns a
/// ready-to-ship Vec<SessionView>.
async fn refresh_all(
    client: &dyn TmuxClient,
    config: &Config,
    registry: &DetectorRegistry,
    smoothers: &mut HashMap<String, Smoother>,
    focused: Option<&str>,
) -> crate::error::Result<Vec<SessionView>> {
    let raw = client.list_sessions().await?;
    // Drop anything that doesn't match the managed-session prefix.
    // Empty prefix → everything matches.
    let sessions: Vec<_> = raw
        .into_iter()
        .filter(|s| config.manages(&s.name))
        .collect();

    let now = SystemTime::now();
    let mut out = Vec::with_capacity(sessions.len());

    for s in sessions {
        // Capture the visible pane only (no scrollback) for both status
        // detection and preview rendering. Scrollback would pick up old
        // shell command history — not what the user expects to see.
        let ansi = match client.capture_pane(&s.name).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("capture-pane {} failed: {}", s.name, e);
                Vec::new()
            }
        };

        let plain = crate::tmux::detector::strip_ansi(&ansi);
        let prev = smoothers.get(&s.name).map(|sm| sm.current());
        let ctx = DetectContext::from_parts(&ansi, &plain, s.last_activity, now, prev, &s.name);
        let detected = registry.detect(&ctx);
        let smoothed = smoothers
            .entry(s.name.clone())
            .or_default()
            .observe(detected);

        // Only hold onto the preview buffer for the focused session — the
        // others get None so we don't keep megabytes of pane history alive.
        let preview = if Some(s.name.as_str()) == focused {
            Some(Arc::from(ansi.into_boxed_slice()))
        } else {
            None
        };
        out.push(SessionView::new(
            s,
            if smoothed == Status::Unknown {
                // Never surface Unknown to the UI — fall back to Idle so the
                // glyph is stable instead of blinking.
                Status::Idle
            } else {
                smoothed
            },
            preview,
        ));
    }

    Ok(out)
}

#[cfg(test)]
mod build_cmd_tests {
    use super::*;
    use crate::events::{ClaudeOptions, CodexOptions};

    fn opts() -> SpecOptions {
        SpecOptions::default()
    }

    #[test]
    fn claude_with_no_options_is_bare() {
        assert_eq!(build_agent_command("claude", &opts(), ""), "claude");
    }

    #[test]
    fn claude_continue_adds_flag() {
        let mut o = opts();
        o.claude.session_mode = ClaudeSessionMode::Continue;
        assert_eq!(build_agent_command("claude", &o, ""), "claude --continue");
    }

    #[test]
    fn claude_resume_skip_permissions_combines() {
        let o = SpecOptions {
            claude: ClaudeOptions {
                session_mode: ClaudeSessionMode::Resume,
                skip_permissions: true,
            },
            codex: CodexOptions::default(),
        };
        assert_eq!(
            build_agent_command("claude", &o, ""),
            "claude --resume --dangerously-skip-permissions"
        );
    }

    #[test]
    fn claude_with_extra_args_appends() {
        let o = SpecOptions {
            claude: ClaudeOptions {
                skip_permissions: true,
                ..Default::default()
            },
            codex: CodexOptions::default(),
        };
        assert_eq!(
            build_agent_command("claude", &o, "--model=opus"),
            "claude --dangerously-skip-permissions --model=opus"
        );
    }

    #[test]
    fn codex_yolo() {
        let o = SpecOptions {
            codex: CodexOptions { yolo: true },
            ..Default::default()
        };
        assert_eq!(build_agent_command("codex", &o, ""), "codex --yolo");
    }

    #[test]
    fn terminal_ignores_options_runs_args() {
        let o = SpecOptions {
            claude: ClaudeOptions {
                skip_permissions: true,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            build_agent_command("terminal", &o, "vim .zshrc"),
            "vim .zshrc"
        );
        assert_eq!(build_agent_command("terminal", &opts(), ""), "");
    }
}
