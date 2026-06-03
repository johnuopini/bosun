//! spike_term — Step 1 perf spike for the Bosun 2.0 embedded-terminal stack.
//!
//! Job: prove or disprove that `portable-pty + vt100 + tui-term` keeps up with
//! realistic burst loads coming out of a real tmux session, and produce hard
//! frame-time / throughput numbers before we commit to wiring this into the
//! main bin. Standalone on purpose — nothing under `src/` is touched; this
//! spike informed the embedded-terminal stack that later shipped in 2.0.
//!
//! ## How to run
//!
//! ```text
//! cargo run --example spike_term -- --session <name>
//! cargo run --example spike_term -- --socket bosun --session work
//! cargo run --example spike_term -- --socket-default --session 0
//! ```
//!
//! Telemetry is appended to `/tmp/bosun-spike.log` (override with
//! `--log <path>`). Each draw line records bytes-since-last-draw, frame-time
//! in ms, and grid rows/cols; a totals line is written on exit. Quit with `q`
//! or Ctrl-C.
//!
//! ## How to bench
//!
//! Inside the *real* tmux session (in another terminal, attached normally),
//! run each workload and watch `/tmp/bosun-spike.log` for hitches:
//!
//!   1. `yes | head -100000`     — worst-case flood
//!   2. `cargo build -p ratatui` — realistic burst
//!   3. a live Claude Code session — target workload
//!
//! While the spike is running, also attach a *second* real client to the same
//! session at a much larger size to answer the `attach -r` window-size
//! negotiation question. Try `setw -g aggressive-resize on` and
//! `set -g window-size largest|smallest|manual` and note which combo lets the
//! real client keep its native size.

use std::fs::OpenOptions;
use std::io::{self, Read, Stdout, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;
use tui_term::widget::PseudoTerminal;

#[derive(Debug)]
struct Args {
    session: String,
    socket: SocketChoice,
    log_path: PathBuf,
}

#[derive(Debug)]
enum SocketChoice {
    /// `-L <name>`
    Named(String),
    /// Use tmux's default socket (no `-L` flag).
    Default,
}

fn parse_args() -> Result<Args> {
    let mut session: Option<String> = None;
    let mut socket: SocketChoice = SocketChoice::Named("bosun".to_string());
    let mut log_path = PathBuf::from("/tmp/bosun-spike.log");

    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--session" => {
                session = Some(
                    it.next()
                        .ok_or_else(|| anyhow!("--session requires a value"))?,
                );
            }
            "--socket" => {
                let v = it
                    .next()
                    .ok_or_else(|| anyhow!("--socket requires a value"))?;
                socket = SocketChoice::Named(v);
            }
            "--socket-default" => {
                socket = SocketChoice::Default;
            }
            "--log" => {
                log_path =
                    PathBuf::from(it.next().ok_or_else(|| anyhow!("--log requires a path"))?);
            }
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            other => return Err(anyhow!("unknown arg: {other}")),
        }
    }

    Ok(Args {
        session: session.ok_or_else(|| anyhow!("--session <name> is required"))?,
        socket,
        log_path,
    })
}

fn print_help() {
    println!(
        "spike_term — perf spike for portable-pty + vt100 + tui-term\n\
         \n\
         USAGE:\n    cargo run --example spike_term -- --session <name> [opts]\n\
         \n\
         OPTIONS:\n\
             --session <name>     tmux session to attach to (required)\n\
             --socket <name>      tmux -L socket name (default: bosun)\n\
             --socket-default     use tmux's default socket (no -L)\n\
             --log <path>         telemetry log path (default: /tmp/bosun-spike.log)\n\
             -h, --help           show this help\n"
    );
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args = parse_args()?;
    let log = LogSink::open(&args.log_path).context("opening telemetry log")?;
    log.line(&format!(
        "=== spike_term start session={} socket={:?} pid={} ===",
        args.session,
        args.socket,
        std::process::id()
    ));

    // ---- terminal setup ----
    enable_raw_mode().context("enable_raw_mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("enter alt screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("create ratatui Terminal")?;

    let result = run(&mut terminal, &args, &log).await;

    // ---- terminal teardown (always) ----
    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();

    if let Err(e) = &result {
        eprintln!("spike_term: {e:#}");
        log.line(&format!("=== spike_term error: {e:#} ==="));
    }
    log.line("=== spike_term exit ===");
    result
}

async fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    args: &Args,
    log: &LogSink,
) -> Result<()> {
    let size = terminal
        .size()
        .map_err(|e| anyhow!("query terminal size: {e}"))?;
    // Reserve 2 rows for a header line + the borderless render isn't worth it;
    // give the PTY the full terminal grid — this is a spike, the host TUI is
    // just the bench harness.
    let mut rows = size.height.max(4);
    let mut cols = size.width.max(20);

    // ---- spawn PTY running `tmux [-L sock] attach -r -t <session>` ----
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| anyhow!("openpty: {e}"))?;

    let mut cmd = CommandBuilder::new("tmux");
    match &args.socket {
        SocketChoice::Named(s) => {
            cmd.arg("-L");
            cmd.arg(s);
        }
        SocketChoice::Default => {}
    }
    cmd.arg("attach");
    cmd.arg("-r");
    cmd.arg("-t");
    cmd.arg(&args.session);
    // Hint to child apps. tmux will renegotiate via its own protocol but this
    // is a fine default for the outer shell.
    cmd.env("TERM", "xterm-256color");

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| anyhow!("spawn tmux: {e}"))?;
    // We don't need the slave handle anymore after spawn.
    drop(pair.slave);

    // Reader side: take a blocking Read from the master and pump bytes to the
    // main task over an mpsc channel. spawn_blocking because portable-pty's
    // reader is a blocking std::io::Read, not an async one.
    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| anyhow!("clone pty reader: {e}"))?;
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(256);
    let stop = Arc::new(AtomicBool::new(false));
    let stop_reader = stop.clone();
    let reader_handle = tokio::task::spawn_blocking(move || {
        let mut buf = [0u8; 8192];
        loop {
            if stop_reader.load(Ordering::Relaxed) {
                break;
            }
            match reader.read(&mut buf) {
                Ok(0) => break, // EOF
                Ok(n) => {
                    if tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });

    // ---- parser + render loop ----
    let mut parser = vt100::Parser::new(rows, cols, 0);
    let mut dirty = true;
    let mut bytes_since_draw: u64 = 0;
    let mut bytes_total: u64 = 0;
    let mut frames: u64 = 0;
    let frame_budget = Duration::from_millis(1000 / 60); // 60 Hz cap
    let mut last_draw = Instant::now() - frame_budget;
    let started = Instant::now();

    let mut exit = false;
    while !exit {
        // Drain any bytes that arrived since the last poll. Non-blocking.
        loop {
            match rx.try_recv() {
                Ok(chunk) => {
                    bytes_since_draw += chunk.len() as u64;
                    bytes_total += chunk.len() as u64;
                    parser.process(&chunk);
                    dirty = true;
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    exit = true;
                    break;
                }
            }
        }

        // Handle input/resize events without blocking. crossterm's poll is
        // synchronous but we cap the wait so the render loop stays responsive
        // even when the PTY is idle.
        if event::poll(Duration::from_millis(8)).unwrap_or(false) {
            match event::read() {
                Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => {
                    let is_q = matches!(k.code, KeyCode::Char('q'));
                    let is_ctrl_c = matches!(k.code, KeyCode::Char('c'))
                        && k.modifiers.contains(KeyModifiers::CONTROL);
                    if is_q || is_ctrl_c {
                        exit = true;
                    }
                }
                Ok(Event::Resize(new_cols, new_rows)) => {
                    cols = new_cols.max(20);
                    rows = new_rows.max(4);
                    parser.screen_mut().set_size(rows, cols);
                    let _ = pair.master.resize(PtySize {
                        rows,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                    dirty = true;
                }
                Ok(_) => {}
                Err(_) => {}
            }
        }

        // Cap redraws at 60Hz.
        let now = Instant::now();
        if dirty && now.duration_since(last_draw) >= frame_budget {
            let frame_started = Instant::now();
            terminal
                .draw(|f| {
                    let area = f.area();
                    let widget = PseudoTerminal::new(parser.screen());
                    f.render_widget(widget, area);
                })
                .map_err(|e| anyhow!("ratatui draw: {e}"))?;
            let frame_ms = frame_started.elapsed().as_secs_f64() * 1000.0;
            frames += 1;

            log.line(&format!(
                "{ts} draw bytes={bytes} frame_ms={ms:.2} rows={rows} cols={cols}",
                ts = unix_ms(),
                bytes = bytes_since_draw,
                ms = frame_ms,
                rows = rows,
                cols = cols,
            ));
            bytes_since_draw = 0;
            dirty = false;
            last_draw = Instant::now();
        }

        // Check on the child without blocking. If tmux exited (session killed,
        // server gone) we should bail.
        match child.try_wait() {
            Ok(Some(status)) => {
                log.line(&format!("=== child tmux exited status={status:?} ==="));
                exit = true;
            }
            Ok(None) => {}
            Err(_) => {}
        }
    }

    // ---- shutdown ----
    let elapsed = started.elapsed().as_secs_f64();
    let mb = bytes_total as f64 / (1024.0 * 1024.0);
    let mbps = if elapsed > 0.0 { mb / elapsed } else { 0.0 };
    log.line(&format!(
        "=== totals frames={frames} bytes={bytes_total} ({mb:.2} MiB) \
         elapsed_s={elapsed:.2} avg_MiB_s={mbps:.2} ===",
    ));

    stop.store(true, Ordering::Relaxed);
    let _ = child.kill();
    // Drop the master so the reader's read() returns EOF and the blocking
    // task can finish.
    drop(pair.master);
    let _ = reader_handle.await;

    Ok(())
}

// ---- logging ----

struct LogSink {
    path: PathBuf,
}

impl LogSink {
    fn open(path: &std::path::Path) -> Result<Self> {
        // Touch the file so a later append doesn't surprise the operator.
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("opening log at {}", path.display()))?;
        writeln!(f, "# spike_term log opened at {}", unix_ms()).ok();
        Ok(Self {
            path: path.to_path_buf(),
        })
    }

    fn line(&self, msg: &str) {
        if let Ok(mut f) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = writeln!(f, "{msg}");
        }
    }
}

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}
