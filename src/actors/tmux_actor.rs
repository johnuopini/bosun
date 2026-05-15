//! The sole owner of the tmux client + per-session smoothing state.
//!
//! ## Architecture (post tmux -C rewrite)
//!
//! Prior to the v0.2.0 rewrite, refreshes were driven by a 1Hz
//! `poller` task that fired `Tick` events into the main loop, which
//! in turn generated `Command::ListNow` for this actor. That had two
//! problems: (1) wasted work for idle sessions, and (2) during a long
//! `perform_attach` the tick backlog could fill bounded channels and
//! cascade into a mutual-wait deadlock between main and this actor.
//!
//! Both problems went away with the move to tmux control mode. Now
//! this actor owns a long-lived [`ControlClient`] subprocess
//! (`tmux -C attach-session -t __bosun_monitor`) and uses
//! `tokio::select!` to wait on **either** a command from main **or**
//! an asynchronous notification from tmux. Session-list refreshes
//! run on relevant notifications (session added/closed/renamed,
//! window added/closed) instead of on a timer. Zero work on an idle
//! server, zero tick backlog during long attaches.
//!
//! `Command::FocusPreview` still lets the app prioritize capturing a
//! specific session's pane immediately — useful on selection change
//! so the preview updates without waiting for a notification.
//!
//! Attach stays handled inline by the app task (needs the controlling
//! tty). This actor only handles read-only operations, command
//! execution, and the status bar side effects.
//!
//! ## Fallback
//!
//! If the control client fails to spawn at startup (e.g. tmux not
//! installed or a permissions issue), this actor emits a `Warn`
//! message and continues in **commands-only** mode — refreshes still
//! run when main sends `Command::ListNow` or any lifecycle command,
//! but there are no push updates. It's degraded, not dead.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::sync::mpsc;
use tokio::time::{self, MissedTickBehavior};

use crate::config::Config;
use crate::events::{AppMsg, ClaudeSessionMode, Command, SessionSpec, SpecOptions};
use crate::store::Store;
use crate::tmux::attach::{
    clear_ctrl_q_bound, clear_quick_jump_bound, clear_session_cycle_bound, ensure_ctrl_q_bound,
    ensure_quick_jump_bound, ensure_session_cycle_bound,
};
use crate::tmux::control::Notification;
use crate::tmux::control_client::ControlClient;
use crate::tmux::detector::{DetectContext, DetectorRegistry, Status};
use crate::tmux::session::SessionView;
use crate::tmux::status_bar::{self, BarSession};
use crate::tmux::{CreateSpec, SessionMetadata, TmuxClient};
use crate::util::collision::resolve_name_collision;
use crate::util::hysteresis::Smoother;

/// RAII cleanup for globals installed by the status bar (prefix-1..9
/// bindings), the C-q detach binding, the S-Left / S-Right session
/// cycle bindings, and the M-O quick-jump popup binding. Per-session
/// status-* options are left in place when the actor exits — they die
/// with their sessions, and leaving them means a restarting bosun can
/// reuse them without a reinit flash.
struct GlobalsGuard {
    socket: Option<String>,
    installed: bool,
    cq_installed: bool,
    cycle_installed: bool,
    quick_jump_installed: bool,
}

impl Drop for GlobalsGuard {
    fn drop(&mut self) {
        if self.installed {
            status_bar::uninstall_globals(self.socket.as_deref());
        }
        if self.cq_installed {
            clear_ctrl_q_bound(self.socket.as_deref());
        }
        if self.cycle_installed {
            clear_session_cycle_bound(self.socket.as_deref());
        }
        if self.quick_jump_installed {
            clear_quick_jump_bound(self.socket.as_deref());
        }
    }
}

pub fn spawn(
    client: Arc<dyn TmuxClient>,
    socket: Option<String>,
    config: Config,
    store: Arc<Store>,
    mut cmd_rx: mpsc::UnboundedReceiver<Command>,
    evt_tx: mpsc::UnboundedSender<AppMsg>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let registry = DetectorRegistry::default_stack();
        let mut smoothers: HashMap<String, Smoother> = HashMap::new();
        let mut focused: Option<String> = None;
        let mut last_bar_state: Vec<BarSession> = Vec::new();
        let mut globals = GlobalsGuard {
            socket: socket.clone(),
            installed: false,
            cq_installed: false,
            cycle_installed: false,
            quick_jump_installed: false,
        };

        // Install the C-q detach binding up-front so it's live even
        // before the first tmux notification arrives. `do_refresh`
        // re-asserts it on every tick — cheap, and guards against
        // anything that clobbers the root key table mid-session.
        ensure_ctrl_q_bound(socket.as_deref());
        globals.cq_installed = true;

        // Install the S-Left / S-Right MRU session cycle bindings. Same
        // self-heal pattern as C-q: do_refresh re-asserts every tick.
        ensure_session_cycle_bound(socket.as_deref());
        globals.cycle_installed = true;

        // Install the M-O quick-jump popup binding. Same self-heal.
        ensure_quick_jump_bound(socket.as_deref());
        globals.quick_jump_installed = true;

        // Start the control-mode monitor subprocess. The guard is
        // held for the lifetime of the actor — dropping it on exit
        // kills the subprocess. `notifs` is the receive side of a
        // channel the reader task pushes parsed notifications onto.
        //
        // Fallback: if spawn fails, we log a warning and run in
        // commands-only mode (notifs = None, the select! branch
        // falls through to std::future::pending).
        let (_control_guard, mut notifs) = match ControlClient::spawn(socket.as_deref()).await {
            Ok((guard, rx)) => (Some(guard), Some(rx)),
            Err(e) => {
                tracing::warn!("tmux control mode unavailable: {}", e);
                let _ = evt_tx.send(AppMsg::Warn(format!("live refresh off: {}", e)));
                (None, None)
            }
        };

        // Internal 1Hz refresh timer. Control-mode notifications
        // drive session/window lifecycle updates, but tmux doesn't
        // notify on plain pane content changes — so without a timer,
        // the preview for the focused session would never update
        // while the underlying pane is writing output (the exact
        // "preview: capturing…" stuck state we hit on first v0.2.0
        // build). `Skip` missed-tick behavior means a slow host or
        // a long refresh doesn't produce a burst of catch-up ticks
        // afterwards — at most one tick per wake-up.
        //
        // Unlike the old standalone `poller` task, this timer lives
        // *inside* `tmux_actor` and triggers `do_refresh` directly.
        // No tick flows through `main`'s event loop, no
        // `cmd_tx`/`evt_tx` cross-channel handoff, so the back-
        // pressure deadlock that killed v0.1.x can't manifest here.
        let mut preview_tick = time::interval(Duration::from_millis(1000));
        preview_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // Skip the immediate first tick — we're about to do a
        // refresh explicitly just below.
        preview_tick.tick().await;

        // Initial refresh so the UI populates without waiting for a
        // notification. Otherwise a user starting bosun against an
        // already-quiet tmux server would see an empty list until
        // something changed.
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
            None,
        )
        .await;

        loop {
            tokio::select! {
                maybe_cmd = cmd_rx.recv() => {
                    let Some(cmd) = maybe_cmd else { break };
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

                            if evt_tx
                                .send(AppMsg::SessionsRefreshed {
                                    sessions: views,
                                    select_after: None,
                                })
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(e) => {
                            if evt_tx.send(AppMsg::Warn(format!("list: {}", e))).is_err() {
                                break;
                            }
                        }
                    }
                }
                Command::FocusPreview { name } => {
                    // Set focus, then refresh immediately so the
                    // preview catches up to the new selection
                    // without waiting up to 1s for the next
                    // preview_tick. Without this the user sees a
                    // stuck "preview: capturing…" when switching
                    // between sessions quickly.
                    focused = Some(name);
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
                        None,
                    )
                    .await;
                }
                Command::KillSession(internal) => {
                    match client.kill_session(&internal).await {
                        Ok(()) => {
                            // If we killed the focused session, drop
                            // the focus so the preview doesn't keep
                            // trying to capture a dead pane.
                            if focused.as_deref() == Some(internal.as_str()) {
                                focused = None;
                            }
                            // Force a refresh so the session disappears
                            // from the UI without a 1s wait.
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
                                None,
                            )
                            .await;
                        }
                        Err(e) => {
                            let _ = evt_tx.send(AppMsg::Warn(format!("kill: {}", e)));
                        }
                    }
                }
                Command::DeleteRecent(id) => {
                    if let Err(e) = store.delete_recent(id) {
                        tracing::warn!("delete_recent({}): {}", id, e);
                    }
                }
                Command::RestartSession(internal) => {
                    match client.get_session_metadata(&internal).await {
                        Ok(Some(meta)) => {
                            // Rebuild the spec, kill the old session,
                            // then create a new one with the same
                            // display name + options (new internal
                            // hex suffix).
                            let spec = metadata_to_spec(meta);
                            if let Err(e) = client.kill_session(&internal).await {
                                let _ = evt_tx.send(AppMsg::Warn(format!("restart kill: {}", e)));
                                continue;
                            }
                            if focused.as_deref() == Some(internal.as_str()) {
                                focused = None;
                            }
                            match create_session(&*client, &config, spec.clone()).await {
                                Ok(new_internal) => {
                                    focused = Some(new_internal.clone());
                                    if let Err(e) = store.upsert_recent(&spec) {
                                        tracing::warn!("store upsert on restart: {}", e);
                                    }
                                    let _ = evt_tx
                                        .send(AppMsg::Warn(format!("restarted {}", spec.name)));
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
                                        Some(new_internal),
                                    )
                                    .await;
                                }
                                Err(e) => {
                                    let _ =
                                        evt_tx.send(AppMsg::Warn(format!("restart create: {}", e)));
                                }
                            }
                        }
                        Ok(None) => {
                            let _ = evt_tx.send(AppMsg::Warn(
                                "cannot restart: session predates metadata support".to_string(),
                            ));
                        }
                        Err(e) => {
                            let _ = evt_tx.send(AppMsg::Warn(format!("restart read: {}", e)));
                        }
                    }
                }
                Command::RenameSession {
                    internal,
                    new_display,
                } => match client.set_display_name(&internal, &new_display).await {
                    Ok(()) => {
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
                            None,
                        )
                        .await;
                    }
                    Err(e) => {
                        let _ = evt_tx.send(AppMsg::Warn(format!("rename: {}", e)));
                    }
                },
                Command::CreateSession(spec) => {
                    // Collision-check against the CURRENT live sessions
                    // so "Bosun" auto-becomes "Bosun 2" when a session
                    // with the same display name already exists.
                    let spec = match resolve_collision(&*client, &config, spec).await {
                        Ok(resolved) => resolved,
                        Err(e) => {
                            let _ = evt_tx.send(AppMsg::Warn(format!("create: {}", e)));
                            continue;
                        }
                    };

                    match create_session(&*client, &config, spec.clone()).await {
                        Ok(internal_name) => {
                            focused = Some(internal_name.clone());
                            // Save the recent (on the resolved spec —
                            // so if "Bosun" became "Bosun 2", the
                            // recents store remembers "Bosun 2").
                            if let Err(e) = store.upsert_recent(&spec) {
                                tracing::warn!("store upsert_recent: {}", e);
                            }
                            let _ = evt_tx.send(AppMsg::Warn(format!("created {}", internal_name)));
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
                                Some(internal_name),
                            )
                            .await;
                        }
                        Err(e) => {
                            let _ = evt_tx.send(AppMsg::Warn(format!("create: {}", e)));
                        }
                    }
                }
                Command::Attach { .. } => {
                    tracing::warn!("tmux_actor received Attach — ignored; app task handles attach");
                }
                Command::SetTheme { .. }
                | Command::SaveDivider(_)
                | Command::SaveSidebar(_)
                | Command::SaveSessionHistory(_)
                | Command::SaveBannerFont(_)
                | Command::InsertSection { .. }
                | Command::RenameSection { .. } => {
                    // Pure UI state — the app loop intercepts these
                    // before forwarding. If one makes it here the
                    // intercept path is broken.
                    tracing::warn!("tmux_actor received UI-only command — should be intercepted by app");
                }
                        Command::Shutdown => break,
                    }
                }
                maybe_notif = async {
                    // If the control client failed at spawn or has
                    // since closed, disable this branch by awaiting
                    // a future that never resolves. select! will
                    // then only poll the cmd branch.
                    match notifs.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    let Some(notif) = maybe_notif else {
                        // Reader task exited — monitor is gone. Fall
                        // back to commands-only mode for the rest of
                        // this actor's lifetime.
                        tracing::warn!("tmux control notification stream closed");
                        notifs = None;
                        continue;
                    };

                    // Lifecycle notifications trigger a full refresh.
                    // Pane `%output` and layout changes are ignored
                    // for now — status detection still runs against
                    // pane captures on refresh, and preview updates
                    // come via FocusPreview commands. (A future
                    // improvement can wire %output into the
                    // detectors for push-based status + preview.)
                    let should_refresh = matches!(
                        notif,
                        Notification::SessionsChanged
                            | Notification::SessionChanged { .. }
                            | Notification::SessionRenamed { .. }
                            | Notification::SessionClosed { .. }
                            | Notification::SessionWindowChanged { .. }
                            | Notification::WindowAdd { .. }
                            | Notification::WindowClose { .. }
                            | Notification::WindowRenamed { .. }
                    );

                    if matches!(notif, Notification::Exit) {
                        tracing::warn!(
                            "tmux control subprocess exited — commands-only mode"
                        );
                        notifs = None;
                        continue;
                    }

                    if should_refresh {
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
                            None,
                        )
                        .await;
                    }
                }
                _ = preview_tick.tick() => {
                    // Periodic refresh for preview + status
                    // detection. See the comment on `preview_tick`
                    // above for why this lives inside the actor
                    // rather than in a separate task.
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
                        None,
                    )
                    .await;
                }
            }
        }

        // `globals` drops here → uninstall_globals runs.
        drop(globals);
    })
}

/// Assemble the internal tmux session name from the user's typed
/// display name. Internal format: `<prefix><slug>-<hex-suffix>`,
/// e.g. `bosun-my-rocket-fox-a1b2c3d4`. The display name can contain
/// caps, spaces, punctuation — anything — but the tmux-visible name
/// is a lowercase dashed slug + unique hex suffix so it's safe to
/// pass to `-t` and always unique even for duplicate display names.
fn build_internal_name(prefix: &str, display: &str) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let suffix = format!("{:08x}", nanos as u32);
    let slug = slugify(display);
    // If the slug somehow ends up empty (e.g. display was all symbols,
    // which the modal should reject but be defensive), fall back to
    // "session".
    let slug = if slug.is_empty() {
        "session".to_string()
    } else {
        slug
    };
    format!("{}{}-{}", prefix, slug, suffix)
}

/// Lowercase slug: alphanumeric and underscores are kept (underscore
/// is valid in tmux session names); everything else collapses to
/// single dashes; leading/trailing dashes are trimmed.
pub(crate) fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_dash = false;
    for c in s.chars() {
        if c.is_alphanumeric() || c == '_' {
            for lower in c.to_lowercase() {
                out.push(lower);
            }
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
    }
    out.trim_end_matches('-').to_string()
}

/// Reverse of `build_internal_name`: extract the slug portion from an
/// internal session name shaped like `<prefix><slug>-<8-hex>`. Returns
/// `None` if the input doesn't match the expected shape — caller can
/// then fall back to showing the raw internal name.
///
/// Used by the sidebar to render a friendlier label on "missing" rows
/// (sessions that died with a tmux server restart) and to match those
/// rows back to a `Recent` so `R` can recreate them.
pub(crate) fn slug_from_internal<'a>(internal: &'a str, prefix: &str) -> Option<&'a str> {
    let after_prefix = if prefix.is_empty() {
        internal
    } else {
        internal.strip_prefix(prefix)?
    };
    // Last `-` separates slug from the 8-hex suffix.
    let dash = after_prefix.rfind('-')?;
    let (slug, rest) = after_prefix.split_at(dash);
    let suffix = rest.strip_prefix('-')?;
    if suffix.len() == 8 && suffix.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(slug)
    } else {
        None
    }
}

/// Map the agent + options + extra args into a shell command to type
/// into the session's pane.
///
/// We type the command directly into the user's login shell — no
/// `bash -c 'exec ...'` wrapping. Bosun runs its own tmux server on
/// a dedicated `-L bosun` socket, which is a child of the bosun
/// process, so pane shells inherit the right environment (including
/// Keychain lineage for Claude Code). The agent runs as a child of
/// the shell; Ctrl-Z suspends the agent directly, fg resumes it, and
/// when the agent exits the shell stays alive so the session doesn't
/// die.
///
/// `terminal` just types whatever extra args the user provided (or
/// nothing — you get a plain shell).
fn build_agent_command(agent: &str, options: &SpecOptions, args: &str) -> String {
    let args = args.trim();
    match agent {
        "claude" => {
            let mut parts: Vec<String> = vec!["claude".into()];
            match options.claude.session_mode {
                ClaudeSessionMode::New => {}
                ClaudeSessionMode::Continue => parts.push("--continue".into()),
                ClaudeSessionMode::Resume => parts.push("--resume".into()),
            }
            if options.claude.skip_permissions {
                parts.push("--dangerously-skip-permissions".into());
            }
            if !args.is_empty() {
                parts.push(args.to_string());
            }
            parts.join(" ")
        }
        "codex" => {
            let mut parts: Vec<String> = vec!["codex".into()];
            if options.codex.yolo {
                parts.push("--yolo".into());
            }
            if !args.is_empty() {
                parts.push(args.to_string());
            }
            parts.join(" ")
        }
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
    let metadata = Some(spec_to_metadata(&spec));
    let create = CreateSpec {
        name: internal.clone(),
        display_name: Some(spec.name.clone()),
        path: spec.path.clone(),
        command,
        metadata,
    };
    client.create_session(&create).await.map(|_| internal)
}

/// Project a `SessionSpec` into the persisted tmux-options shape.
fn spec_to_metadata(spec: &SessionSpec) -> SessionMetadata {
    SessionMetadata {
        display_name: spec.name.clone(),
        path: spec.path.clone(),
        agent: spec.agent.clone(),
        args: spec.args.clone(),
        claude_session_mode: match spec.options.claude.session_mode {
            ClaudeSessionMode::New => "New".to_string(),
            ClaudeSessionMode::Continue => "Continue".to_string(),
            ClaudeSessionMode::Resume => "Resume".to_string(),
        },
        claude_skip_permissions: spec.options.claude.skip_permissions,
        codex_yolo: spec.options.codex.yolo,
    }
}

/// Inverse of `spec_to_metadata` — rebuild a SessionSpec from the
/// metadata we read off a live tmux session during restart.
fn metadata_to_spec(meta: SessionMetadata) -> SessionSpec {
    use crate::events::{ClaudeOptions, CodexOptions};
    SessionSpec {
        name: meta.display_name,
        path: meta.path,
        agent: meta.agent,
        args: meta.args,
        options: SpecOptions {
            claude: ClaudeOptions {
                session_mode: match meta.claude_session_mode.as_str() {
                    "Continue" => ClaudeSessionMode::Continue,
                    "Resume" => ClaudeSessionMode::Resume,
                    _ => ClaudeSessionMode::New,
                },
                skip_permissions: meta.claude_skip_permissions,
            },
            codex: CodexOptions {
                yolo: meta.codex_yolo,
            },
        },
    }
}

/// Query the live session list, extract display names, and rename
/// `spec.name` via `resolve_name_collision` if needed. Pure-ish
/// wrapper; the one side-effect is the tmux list-sessions roundtrip.
async fn resolve_collision(
    client: &dyn TmuxClient,
    config: &Config,
    mut spec: SessionSpec,
) -> crate::error::Result<SessionSpec> {
    let sessions = client.list_sessions().await?;
    let existing: Vec<String> = sessions
        .into_iter()
        .filter(|s| config.manages(&s.name))
        .map(|s| s.display_name.unwrap_or(s.name))
        .collect();
    spec.name = resolve_name_collision(&spec.name, &existing);
    Ok(spec)
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
    evt_tx: &mpsc::UnboundedSender<AppMsg>,
    select_after: Option<String>,
) -> crate::error::Result<()> {
    // Re-assert the Ctrl-Q detach binding on every refresh. `bind-key`
    // is idempotent, but running it once per tick means the binding
    // self-heals if anything clobbers the root key table during a
    // long-running session (source-file, another tool's hook, etc).
    ensure_ctrl_q_bound(socket);
    // Same self-heal for the S-Left / S-Right cycle bindings.
    ensure_session_cycle_bound(socket);
    // And for the M-O quick-jump popup binding.
    ensure_quick_jump_bound(socket);

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

    let _ = evt_tx.send(AppMsg::SessionsRefreshed {
        sessions: views,
        select_after,
    });
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

    #[test]
    fn slugify_lowercases_and_dashes() {
        assert_eq!(slugify("My Rocket Fox"), "my-rocket-fox");
        assert_eq!(slugify("Foo.Bar_baz"), "foo-bar_baz");
        assert_eq!(slugify("  leading space"), "leading-space");
        assert_eq!(slugify("multi   spaces"), "multi-spaces");
        assert_eq!(slugify("trailing!!!"), "trailing");
    }

    #[test]
    fn slug_from_internal_strips_prefix_and_hex_suffix() {
        assert_eq!(
            slug_from_internal("bosun-raycast-1e18ae00", "bosun-"),
            Some("raycast")
        );
        assert_eq!(
            slug_from_internal("bosun-my-rocket-fox-a1b2c3d4", "bosun-"),
            Some("my-rocket-fox")
        );
        // Empty prefix (BOSUN_PREFIX="") is allowed.
        assert_eq!(slug_from_internal("raycast-1e18ae00", ""), Some("raycast"));
    }

    #[test]
    fn slug_from_internal_rejects_non_hex_suffix() {
        // Last 8 chars after `-` aren't hex → not bosun-shaped, decline.
        assert_eq!(slug_from_internal("bosun-foo-zzzzzzzz", "bosun-"), None);
        // Suffix is hex but wrong length.
        assert_eq!(slug_from_internal("bosun-foo-abc", "bosun-"), None);
        // No prefix match.
        assert_eq!(slug_from_internal("other-foo-12345678", "bosun-"), None);
    }
}
