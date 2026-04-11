//! The sole owner of the tmux client + per-session smoothing state.
//!
//! Each `Command::ListNow` triggers a full refresh pass: list sessions,
//! capture each pane, run the detector registry, smooth via per-session
//! hysteresis, and send a single `SessionsRefreshed` back to the app.
//!
//! This actor also owns the lifecycle of the bosun-branded tmux status
//! bar. On the first successful (non-empty) refresh it installs the
//! status bar and holds the `StatusBarGuard` in its local state, so
//! when the actor's task ends (channels close on shutdown) the guard
//! drops and the user's original status options are restored.
//!
//! `Command::FocusPreview` lets the app prioritize capturing a specific
//! session ahead of its next scheduled tick — e.g. when the user moves
//! the selection and we want the preview to update now rather than in
//! up to a second.
//!
//! Attach stays handled inline by the app task (needs the controlling
//! tty). This actor only handles read-only operations.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::SystemTime;

use tokio::sync::mpsc;

use crate::events::{AppMsg, Command};
use crate::tmux::detector::{DetectContext, DetectorRegistry, Status};
use crate::tmux::session::SessionView;
use crate::tmux::status_bar::{self, StatusBarGuard};
use crate::tmux::TmuxClient;
use crate::util::hysteresis::Smoother;

pub fn spawn(
    client: Arc<dyn TmuxClient>,
    socket: Option<String>,
    mut cmd_rx: mpsc::Receiver<Command>,
    evt_tx: mpsc::Sender<AppMsg>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let registry = DetectorRegistry::default_stack();
        let mut smoothers: HashMap<String, Smoother> = HashMap::new();
        let mut focused: Option<String> = None;
        let mut status_bar: Option<StatusBarGuard> = None;
        let mut last_bar_state: Vec<(String, bool)> = Vec::new();

        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                Command::ListNow => {
                    let views =
                        refresh_all(&*client, &registry, &mut smoothers, focused.as_deref()).await;
                    match views {
                        Ok(views) => {
                            smoothers.retain(|name, _| views.iter().any(|v| v.name() == name));

                            // Lazy install: first successful non-empty
                            // refresh is when we know a tmux server is
                            // up and has sessions to put in the bar.
                            if status_bar.is_none() && !views.is_empty() {
                                match status_bar::install(socket.as_deref()) {
                                    Ok(guard) => status_bar = Some(guard),
                                    Err(e) => tracing::warn!("status bar install: {}", e),
                                }
                            }

                            // Sync the status bar's session list only
                            // when something the bar cares about has
                            // changed (name or attached state). Skipping
                            // no-op syncs saves ~9 tmux calls per tick.
                            if status_bar.is_some() {
                                let state: Vec<(String, bool)> = views
                                    .iter()
                                    .map(|v| (v.name().to_string(), v.session.attached))
                                    .collect();
                                if state != last_bar_state {
                                    if let Err(e) =
                                        status_bar::sync_sessions(socket.as_deref(), &state)
                                    {
                                        tracing::warn!("status bar sync: {}", e);
                                    }
                                    last_bar_state = state;
                                }
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
                    // Don't immediately fire a refresh — the next tick will
                    // include the focused session's preview. A forced refresh
                    // here would multiply subprocess pressure on rapid nav.
                }
                Command::Attach { .. } => {
                    tracing::warn!("tmux_actor received Attach — ignored; app task handles attach");
                }
                Command::Shutdown => break,
            }
        }

        // Explicitly drop the status bar guard here so the restore
        // happens before the task ends and the tokio runtime starts
        // tearing down. Without this the guard drop might race the
        // runtime shutdown on the path where main returns.
        drop(status_bar);
    })
}

/// One full refresh pass: list, capture (with preview for focused),
/// detect, smooth. Returns a ready-to-ship Vec<SessionView>.
async fn refresh_all(
    client: &dyn TmuxClient,
    registry: &DetectorRegistry,
    smoothers: &mut HashMap<String, Smoother>,
    focused: Option<&str>,
) -> crate::error::Result<Vec<SessionView>> {
    let sessions = client.list_sessions().await?;
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
        let raw = registry.detect(&ctx);
        let smoothed = smoothers.entry(s.name.clone()).or_default().observe(raw);

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
