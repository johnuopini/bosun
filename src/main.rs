use std::io::{self, Stdout};
use std::sync::Arc;

use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use bosun::app::App;
use bosun::config::Config;
use bosun::error::{BosunError, Result};
use bosun::store::Store;
use bosun::tmux::{
    attach::emergency_unbind, status_bar::emergency_uninstall as emergency_status_bar,
    TokioTmuxClient,
};

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("bosun {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    // `bosun update [--check]` runs synchronously, prints to stderr,
    // and exits before any TUI/tmux machinery starts. Failures
    // surface as a non-zero exit code with the anyhow chain printed.
    let mut args = std::env::args().skip(1);
    if let Some(first) = args.next() {
        if first == "update" {
            let check_only = args.any(|a| a == "--check");
            return match bosun::commands::update::run(check_only) {
                Ok(()) => Ok(()),
                Err(e) => {
                    eprintln!("bosun update: {:#}", e);
                    std::process::exit(1);
                }
            };
        }
        if first == "help" || first == "--help" || first == "-h" {
            print_help();
            return Ok(());
        }
    }

    init_tracing();

    let config = Config::from_env();
    let socket = config.tmux_socket.clone();
    let client: Arc<TokioTmuxClient> = match &socket {
        Some(s) => Arc::new(TokioTmuxClient::with_socket(s.clone())),
        None => Arc::new(TokioTmuxClient::new()),
    };

    // Open the SQLite store for recents + future metadata. Failure
    // here is non-fatal — we fall back to an in-memory store so bosun
    // still runs, just without persistence across launches.
    let store = Arc::new(match Store::open_default() {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("store open failed, using in-memory: {}", e);
            Store::in_memory().expect("in-memory store cannot fail")
        }
    });

    // Panic hook: restore terminal + clean up C-q binding + restore
    // the user's tmux status bar before we die.
    let socket_for_hook = socket.clone();
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
        emergency_unbind(socket_for_hook.as_deref());
        emergency_status_bar(socket_for_hook.as_deref());
        default_hook(info);
    }));

    let mut terminal = setup_terminal()?;
    let mut app = App::new(client, socket, config, store);
    let run_result = app.run(&mut terminal).await;
    restore_terminal(&mut terminal)?;
    run_result
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode().map_err(BosunError::Io)?;
    let mut stdout = io::stdout();
    // Mouse capture is needed so the draggable divider between the
    // session list and preview pane can see clicks and drags. We
    // tear it down around `tmux attach` so tmux owns the mouse
    // during an attach (see `App::perform_attach`).
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture).map_err(BosunError::Io)?;
    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend).map_err(BosunError::Io)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode().map_err(BosunError::Io)?;
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen,
    )
    .map_err(BosunError::Io)?;
    terminal.show_cursor().map_err(BosunError::Io)?;
    Ok(())
}

fn print_help() {
    println!(
        "bosun {version} — tmux-native orchestrator for AI agent sessions

USAGE:
    bosun                Launch the TUI (default)
    bosun update         Check for and install the latest release
    bosun update --check Check for an update without installing
    bosun --version      Print version and exit
    bosun --help         Print this message

ENVIRONMENT:
    BOSUN_LOG    Tracing filter (e.g. `info`, `bosun=debug`). Off by default.

See https://github.com/yetidevworks/bosun for full docs.",
        version = env!("CARGO_PKG_VERSION")
    );
}

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    // Send logs to stderr. During TUI mode this may scramble the alt-screen,
    // so default filter is OFF — enable with BOSUN_LOG=info.
    let filter = EnvFilter::try_from_env("BOSUN_LOG").unwrap_or_else(|_| EnvFilter::new("off"));
    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(io::stderr)
        .try_init();
}
