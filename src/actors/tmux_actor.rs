//! The sole owner of the tmux client.
//!
//! Receives [`Command`]s from the app task and translates them into calls
//! on the [`TmuxClient`] trait. Results flow back to the app task as
//! [`AppMsg`]s on a separate channel.
//!
//! Attach is special: it needs the controlling tty, so the actor notifies
//! the app that an attach is starting, returns control to the app task
//! (which tears down ratatui), and then the **app task itself** (not the
//! actor) performs the blocking `tmux attach`. The actor therefore only
//! handles list/refresh Commands in Phase 1.

use std::sync::Arc;

use tokio::sync::mpsc;

use crate::events::{AppMsg, Command};
use crate::tmux::TmuxClient;

pub fn spawn(
    client: Arc<dyn TmuxClient>,
    mut cmd_rx: mpsc::Receiver<Command>,
    evt_tx: mpsc::Sender<AppMsg>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                Command::ListNow => match client.list_sessions().await {
                    Ok(sessions) => {
                        if evt_tx
                            .send(AppMsg::SessionsRefreshed(sessions))
                            .await
                            .is_err()
                        {
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
                },
                Command::Attach { .. } => {
                    // Phase 1: attach is performed by the app task itself,
                    // not by this actor. This arm exists for forward-compat.
                    tracing::warn!("tmux_actor received Attach — ignored; app task handles attach");
                }
                Command::Shutdown => break,
            }
        }
    })
}
