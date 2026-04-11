use std::io::{self, Stdout};
use std::sync::Arc;

use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use bosun::app::App;
use bosun::config::Config;
use bosun::error::{BosunError, Result};
use bosun::tmux::{
    attach::emergency_unbind, status_bar::emergency_uninstall as emergency_status_bar,
    TokioTmuxClient,
};

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let config = Config::from_env();
    let client = Arc::new(TokioTmuxClient::new());
    let socket: Option<String> = None;

    // Panic hook: restore terminal + clean up C-q binding + restore
    // the user's tmux status bar before we die.
    let socket_for_hook = socket.clone();
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        emergency_unbind(socket_for_hook.as_deref());
        emergency_status_bar(socket_for_hook.as_deref());
        default_hook(info);
    }));

    let mut terminal = setup_terminal()?;
    let mut app = App::new(client, socket, config);
    let run_result = app.run(&mut terminal).await;
    restore_terminal(&mut terminal)?;
    run_result
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode().map_err(BosunError::Io)?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).map_err(BosunError::Io)?;
    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend).map_err(BosunError::Io)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode().map_err(BosunError::Io)?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen).map_err(BosunError::Io)?;
    terminal.show_cursor().map_err(BosunError::Io)?;
    Ok(())
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
