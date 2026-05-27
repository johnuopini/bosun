//! Embedded terminal preview for the focused session.
//!
//! Owns a `tmux attach -r -t <session>` PTY, a vt100 parser fed by a
//! background reader thread, and the screen state the preview render
//! path samples each frame. Replaces the v0.4 `capture-pane` snapshot
//! preview for the focused session (only). Section / empty-state /
//! non-focused previews still go through the snapshot path —
//! `Config::embed_enabled` gates the embed entirely, falling back to
//! the snapshot path if the user disables it.
//!
//! ## Threading
//!
//! The PTY's reader is a blocking `std::io::Read`. We pump it on a
//! dedicated `std::thread` and forward every chunk through the same
//! `mpsc::UnboundedSender<AppMsg>` the input + tmux actors use. Each
//! chunk becomes an `AppMsg::EmbedBytes { session, bytes }` so the
//! single-writer app task processes it on its normal main loop. The
//! `session` field on the message lets the app discard bytes from a
//! stale embed when the user has already switched focus.
//!
//! ## Cleanup
//!
//! `Drop` flips the stop flag and `kill()`s the tmux child. The
//! child's death closes the master fd, the reader hits EOF on the
//! next `read`, and the thread exits. We also `drop(pair.slave)` at
//! spawn time — the child still holds an fd to it; this just removes
//! our local handle so we're not the last referent.

use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;
use tokio::sync::mpsc;
use tui_term::widget::PseudoTerminal;

use crate::events::AppMsg;

/// PTY read buffer size. 8 KiB is large enough that a flood (e.g.
/// `cargo build` warnings, a `yes` flood) doesn't death-spiral into
/// 1-byte reads, and small enough that a typical agent response
/// arrives in one or two chunks.
const READ_BUF_SIZE: usize = 8192;

/// Minimum PTY grid size. tmux refuses to size a session under
/// (rows=2, cols=10) on some configurations, and vt100's screen
/// would render a useless sliver anyway. We clamp at (4, 20).
const MIN_ROWS: u16 = 4;
const MIN_COLS: u16 = 20;

pub struct EmbedTerminal {
    /// Internal tmux session name (matches `SessionView.name()`).
    /// The reader thread tags every byte chunk with this so the app
    /// can recognize and discard stale messages from a previous
    /// embed instance after a focus switch.
    session: String,
    parser: vt100::Parser,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    /// Belt-and-braces signal for the reader thread. The reliable
    /// stop is the child's death (master fd closes → reader sees
    /// EOF), but the flag lets the loop exit at the next read
    /// boundary even if the child is briefly slow to die.
    stop: Arc<AtomicBool>,
    rows: u16,
    cols: u16,
}

impl EmbedTerminal {
    /// Spawn a new embedded terminal attached to `session` on
    /// `socket` (None = tmux default socket). Sized to (rows, cols),
    /// clamped to (MIN_ROWS, MIN_COLS). Forwards every PTY byte
    /// chunk to `evt_tx` as `AppMsg::EmbedBytes { session, bytes }`.
    ///
    /// `initial_snapshot` (typically the bytes from
    /// `tmux capture-pane -p -e -J`) is fed into the vt100 parser
    /// before the reader thread starts. The parser's screen begins
    /// at the session's current state, so the first frame the user
    /// sees after spawn is a coherent snapshot rather than an empty
    /// grid being filled in by tmux's initial `attach -r` repaint.
    /// Passing `None` is harmless — the parser just starts blank
    /// and tmux's relay paints it over the next few hundred ms.
    pub fn spawn(
        socket: Option<&str>,
        session: &str,
        rows: u16,
        cols: u16,
        initial_snapshot: Option<&[u8]>,
        evt_tx: mpsc::UnboundedSender<AppMsg>,
    ) -> std::io::Result<Self> {
        let rows = rows.max(MIN_ROWS);
        let cols = cols.max(MIN_COLS);

        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(io_err("openpty"))?;

        let mut cmd = CommandBuilder::new("tmux");
        if let Some(sock) = socket {
            cmd.arg("-L");
            cmd.arg(sock);
        }
        cmd.arg("attach");
        cmd.arg("-r");
        cmd.arg("-t");
        cmd.arg(session);
        // Hint to whatever shell tmux relays. tmux's own protocol
        // negotiates the real terminal type with its child apps, so
        // this is only the outer shell hint.
        cmd.env("TERM", "xterm-256color");

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(io_err("spawn tmux"))?;
        // Drop our slave handle. The child still owns one; dropping
        // ours means we won't accidentally keep the slave fd alive
        // past the child's death.
        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(io_err("clone reader"))?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_reader = stop.clone();
        let session_owned = session.to_string();
        let evt_tx_reader = evt_tx;
        thread::Builder::new()
            .name(format!("bosun-embed-{}", session))
            .spawn(move || {
                let mut buf = [0u8; READ_BUF_SIZE];
                loop {
                    if stop_reader.load(Ordering::Relaxed) {
                        break;
                    }
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            let chunk = buf[..n].to_vec();
                            if evt_tx_reader
                                .send(AppMsg::EmbedBytes {
                                    session: session_owned.clone(),
                                    bytes: chunk,
                                })
                                .is_err()
                            {
                                // Receiver dropped — app is shutting
                                // down. Nothing useful left to do.
                                break;
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
            })
            .map_err(io_err("spawn reader"))?;

        let mut parser = vt100::Parser::new(rows, cols, 0);
        if let Some(snap) = initial_snapshot {
            // Feed the capture-pane snapshot synchronously, before
            // the first frame is rendered. The parser's screen now
            // matches what the user would see if they attached
            // directly — so the immediate draw shows a coherent
            // view instead of an empty grid being filled in.
            parser.process(snap);
        }

        Ok(Self {
            session: session.to_string(),
            parser,
            master: pair.master,
            child,
            stop,
            rows,
            cols,
        })
    }

    pub fn session(&self) -> &str {
        &self.session
    }

    /// Feed a chunk of PTY bytes into the vt100 parser. Cheap —
    /// vt100 is a single-pass state machine.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    /// Resize both the parser grid and the PTY's window size. Cheap
    /// no-op when the dimensions are unchanged. The child sees a
    /// SIGWINCH and (for well-behaved TUI apps like Claude Code,
    /// vim, etc.) repaints. tmux relays the resize down to the
    /// session-attached pane.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let rows = rows.max(MIN_ROWS);
        let cols = cols.max(MIN_COLS);
        if rows == self.rows && cols == self.cols {
            return;
        }
        self.rows = rows;
        self.cols = cols;
        self.parser.screen_mut().set_size(rows, cols);
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
    }

    /// Render the current vt100 screen into `area` of `buf`. Uses
    /// `tui_term::widget::PseudoTerminal`, which walks the screen
    /// grid and emits ratatui `Cell`s with SGR attributes translated.
    pub fn render(&self, buf: &mut Buffer, area: Rect) {
        let widget = PseudoTerminal::new(self.parser.screen());
        widget.render(area, buf);
    }
}

impl Drop for EmbedTerminal {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Killing the child closes the slave end; the master's reader
        // then hits EOF and the reader thread exits naturally. We
        // intentionally do NOT join the thread here — if the child
        // wedges, joining would block the app's shutdown path.
        let _ = self.child.kill();
    }
}

/// Map a portable-pty / spawn error into a generic `std::io::Error`
/// so callers can propagate without taking a dep on portable_pty's
/// concrete error type. Accepts anything `Display` so it works
/// against both `anyhow::Error` (what portable_pty returns) and
/// `std::io::Error` (what `thread::spawn` returns).
fn io_err<E: std::fmt::Display>(what: &'static str) -> impl FnOnce(E) -> std::io::Error {
    move |e| std::io::Error::other(format!("{what}: {e}"))
}
