//! Periodic tick pump. Every `interval` the poller sends a `Tick` to the
//! app task, which decides whether to issue a `Command::ListNow`. This is
//! deliberately simple for Phase 1; adaptive/control-mode refresh lives
//! in later phases but reuses the same channel shape.

use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio::time;

use crate::events::AppMsg;

pub fn spawn(tx: mpsc::Sender<AppMsg>, interval: Duration) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = time::interval(interval);
        // Skip the first immediate tick — the app will `ListNow` on startup itself.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            if tx.send(AppMsg::Tick(Instant::now())).await.is_err() {
                break;
            }
        }
    })
}
