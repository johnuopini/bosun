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
//! events to the app via a tokio mpsc. Shutdown is a shared
//! `AtomicBool` that the loop checks every iteration — so pausing
//! during `tmux attach` is synchronous, predictable, and has zero
//! background threads to clean up.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{self, Event};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::events::AppMsg;

/// Handle returned by [`spawn`]. Drop it (or call [`Handle::shutdown`]
/// explicitly) to stop the input reader. The shutdown flag is checked
/// between every 100ms poll, so the reader exits within 100ms of the
/// request.
pub struct Handle {
    shutdown: Arc<AtomicBool>,
    join: JoinHandle<()>,
}

impl Handle {
    /// Request shutdown and wait for the reader loop to exit.
    /// Returns once the blocking task has finished — at most about
    /// 100ms after the flag is set (one poll round).
    pub async fn shutdown(self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = self.join.await;
    }
}

/// Spawn the input reader. Events are translated to `AppMsg` and
/// forwarded to `tx`. The returned [`Handle`] owns the shutdown flag
/// and the blocking-task join handle; shut it down before handing the
/// tty to another process (e.g. `tmux attach`) so crossterm stops
/// polling stdin.
pub fn spawn(tx: mpsc::Sender<AppMsg>) -> Handle {
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_inner = shutdown.clone();

    let join = tokio::task::spawn_blocking(move || {
        // 100ms is short enough that a shutdown request is picked up
        // near-instantly by a human, long enough that the loop doesn't
        // spin hot when nothing is happening.
        const POLL_TIMEOUT: Duration = Duration::from_millis(100);

        loop {
            if shutdown_inner.load(Ordering::SeqCst) {
                break;
            }

            match event::poll(POLL_TIMEOUT) {
                Ok(true) => match event::read() {
                    Ok(Event::Key(k)) => {
                        if tx.blocking_send(AppMsg::Key(k)).is_err() {
                            break;
                        }
                    }
                    Ok(Event::Resize(w, h)) => {
                        if tx.blocking_send(AppMsg::Resize(w, h)).is_err() {
                            break;
                        }
                    }
                    Ok(Event::FocusGained)
                    | Ok(Event::FocusLost)
                    | Ok(Event::Mouse(_))
                    | Ok(Event::Paste(_)) => {
                        // Not interested — drop the event and keep polling.
                    }
                    Err(e) => {
                        let _ = tx.blocking_send(AppMsg::Fatal(format!("input read: {e}")));
                        break;
                    }
                },
                Ok(false) => {
                    // Timeout — no event this round. Loop back and
                    // re-check the shutdown flag.
                }
                Err(e) => {
                    let _ = tx.blocking_send(AppMsg::Fatal(format!("input poll: {e}")));
                    break;
                }
            }
        }
    });

    Handle { shutdown, join }
}
