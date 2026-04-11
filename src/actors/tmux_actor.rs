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
use crate::events::{AppMsg, Command};
use crate::tmux::detector::{DetectContext, DetectorRegistry, Status};
use crate::tmux::session::SessionView;
use crate::tmux::status_bar;
use crate::tmux::TmuxClient;
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
        let mut last_bar_state: Vec<(String, bool)> = Vec::new();
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
                            // (name, attached) pairs has actually
                            // changed. This skips the ~N*7 set-option
                            // calls on ticks where nothing's moved.
                            let state: Vec<(String, bool)> = views
                                .iter()
                                .map(|v| (v.name().to_string(), v.session.attached))
                                .collect();
                            if state != last_bar_state {
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

fn sync_status_bar(socket: Option<&str>, sessions: &[(String, bool)], globals: &mut GlobalsGuard) {
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
    for (name, _) in sessions {
        if let Err(e) = status_bar::configure_session(socket, name, sessions) {
            tracing::warn!("status bar: configure_session {} failed: {}", name, e);
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
