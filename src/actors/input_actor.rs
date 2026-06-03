//! crossterm event reader → AppMsg pump.
//!
//! Owns the keyboard. The only place in the codebase allowed to read
//! from crossterm's event source.
//!
//! ## Why not `crossterm::event::EventStream`?
//!
//! The obvious async-ergonomic choice would be `EventStream::next().await`
//! from the `event-stream` feature. We *did* use that originally and it
//! worked until we started aborting and respawning the input actor
//! across `tmux attach` cycles (to stop crossterm from racing tmux for
//! stdin bytes during an attach). At that point every few attaches
//! bosun would deadlock with the whole tokio runtime idle and one
//! unnamed `std::thread` parked in
//! `std::sync::mpmc::array::Channel<T>::recv` ->
//! `_dispatch_semaphore_wait_slow`.
//!
//! That's crossterm's internal `EventStream` background thread
//! (see `event/stream.rs` in crossterm 0.29). It uses a bounded
//! `std::sync::mpsc::sync_channel::<Task>(1)` to receive poll requests
//! from `Stream::poll_next`. On macOS the bounded-channel implementation
//! sits on top of libdispatch semaphores, and dropping the last
//! `SyncSender` (which is what `EventStream::drop` eventually does)
//! doesn't always wake a blocked `recv()` — there's a race between the
//! semaphore signal and the sender drop. The result is that our
//! abort/respawn pattern leaves crossterm's reader thread stranded,
//! tokio task wakeups stop flowing, and the TUI freezes.
//!
//! So we bypass `EventStream` entirely. A `spawn_blocking` worker runs
//! a simple loop of `event::poll(100ms)` + `event::read()` and forwards
//! events to the app via a tokio mpsc.
//!
//! ## Why `UnboundedSender` and not `blocking_send`?
//!
//! Earlier revisions of this file used `tokio::sync::mpsc::Sender`
//! (bounded) with `blocking_send`, and then a hand-rolled try_send +
//! backoff + timeout loop. Both were variants of the same bug: if the
//! event channel is full for any reason (e.g. main is wedged in
//! `perform_attach` and the poller is spamming ticks), the blocking
//! side parks the spawn_blocking thread on a condvar that the
//! shutdown `AtomicBool` can't interrupt. We've burned three debug
//! sessions on this exact shape of issue.
//!
//! The real fix, landed together with the tmux -C control-mode
//! rewrite, is to make `evt_tx` an `UnboundedSender`. Unbounded
//! `send` is synchronous and returns immediately — no parking, no
//! capacity wait, no way for this reader to get stuck trying to
//! deliver. Memory growth in practice is negligible because there's
//! no longer a 1Hz poller filling the channel during long attaches
//! (tmux control-mode notifications drive refreshes instead), and
//! `AppMsg` is small.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{self, Event};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::events::AppMsg;

/// Handle returned by [`spawn`]. Drop it (or call [`Handle::shutdown`]
/// explicitly) to stop the input reader. The shutdown flag is checked
/// between every poll iteration (max ~100ms latency).
pub struct Handle {
    shutdown: Arc<AtomicBool>,
    join: JoinHandle<()>,
}

impl Handle {
    /// Request shutdown and wait for the reader loop to exit.
    pub async fn shutdown(self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = self.join.await;
    }
}

/// How long `event::poll` waits per iteration for stdin activity.
/// Shorter = more responsive to shutdown, longer = less wake-up
/// overhead. 100ms is comfortably under human perception.
const POLL_TIMEOUT: Duration = Duration::from_millis(100);

/// Spawn the input reader. Events are translated to `AppMsg` and
/// forwarded to `tx` via the sync unbounded `send`. The returned
/// [`Handle`] owns the shutdown flag and the blocking-task join
/// handle; shut it down before handing the tty to another process
/// (e.g. `tmux attach`) so crossterm stops polling stdin.
pub fn spawn(tx: mpsc::UnboundedSender<AppMsg>) -> Handle {
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_inner = shutdown.clone();

    let join = tokio::task::spawn_blocking(move || {
        loop {
            if shutdown_inner.load(Ordering::SeqCst) {
                break;
            }

            match event::poll(POLL_TIMEOUT) {
                Ok(true) => match event::read() {
                    Ok(Event::Key(k)) => {
                        if tx.send(AppMsg::Key(k)).is_err() {
                            break;
                        }
                    }
                    Ok(Event::Resize(w, h)) => {
                        if tx.send(AppMsg::Resize(w, h)).is_err() {
                            break;
                        }
                    }
                    Ok(Event::Mouse(m)) => {
                        if tx.send(AppMsg::Mouse(m)).is_err() {
                            break;
                        }
                    }
                    Ok(Event::Paste(text)) => {
                        if tx.send(AppMsg::Paste(text)).is_err() {
                            break;
                        }
                    }
                    Ok(Event::FocusGained) => {
                        // iTerm's Cmd+R "reset" clears the screen and
                        // exits alt screen without telling ratatui;
                        // when the user re-focuses the pane we force a
                        // full repaint to recover. See `App::run`.
                        if tx.send(AppMsg::FocusGained).is_err() {
                            break;
                        }
                    }
                    Ok(Event::FocusLost) => {
                        // Track focus loss so the next `FocusGained`
                        // is treated as a genuine refocus (recovery
                        // runs once) rather than the echo a terminal
                        // emits when focus reporting is re-enabled.
                        if tx.send(AppMsg::FocusLost).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(AppMsg::Fatal(format!("input read: {e}")));
                        break;
                    }
                },
                Ok(false) => {
                    // Timeout — no event this round. Loop back and
                    // re-check the shutdown flag.
                }
                Err(e) => {
                    let _ = tx.send(AppMsg::Fatal(format!("input poll: {e}")));
                    break;
                }
            }
        }
    });

    Handle { shutdown, join }
}
