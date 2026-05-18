//! `bosun release-notes` — page the embedded CHANGELOG.
//!
//! `CHANGELOG.md` is embedded at compile time via `include_str!`, so
//! the version baked into the running binary is always the one shown
//! (no network, no filesystem lookup, no risk of drifting against a
//! locally-edited copy). The full file is piped to `$PAGER` if set,
//! otherwise `less -R`, otherwise `more`, otherwise printed directly
//! to stdout. When stdout isn't a TTY (piping into `grep`, redirecting
//! to a file, etc.) we always print directly.

use anyhow::{Context, Result};
use std::io::{IsTerminal, Write};
use std::process::{Command, Stdio};

const CHANGELOG: &str = include_str!("../../CHANGELOG.md");

pub fn run() -> Result<()> {
    if !std::io::stdout().is_terminal() {
        print!("{}", CHANGELOG);
        return Ok(());
    }

    // Try $PAGER first, then a couple of well-known fallbacks. If
    // every candidate is missing (or refuses to spawn), fall through
    // to plain stdout so the user still sees the notes.
    let candidates: Vec<Vec<String>> = pager_candidates();
    for argv in &candidates {
        if let Some((cmd, args)) = argv.split_first() {
            if try_pipe_to_pager(cmd, args, CHANGELOG).is_ok() {
                return Ok(());
            }
        }
    }

    print!("{}", CHANGELOG);
    Ok(())
}

fn pager_candidates() -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = Vec::new();
    if let Ok(pager) = std::env::var("PAGER") {
        // `$PAGER` may include args (`less -RFX`, `bat --paging=always`),
        // so we split on whitespace and trust the user's expansion.
        let parts: Vec<String> = pager.split_whitespace().map(|s| s.to_string()).collect();
        if !parts.is_empty() {
            out.push(parts);
        }
    }
    // `-R` passes ANSI through; `-F` quits immediately when the
    // content fits on one screen; `-X` skips the alt-screen swap so
    // the changelog stays visible after the pager exits.
    out.push(vec!["less".into(), "-RFX".into()]);
    out.push(vec!["more".into()]);
    out
}

fn try_pipe_to_pager(cmd: &str, args: &[String], content: &str) -> Result<()> {
    let mut child = Command::new(cmd)
        .args(args)
        .stdin(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn pager `{}`", cmd))?;

    if let Some(mut stdin) = child.stdin.take() {
        // Ignore broken-pipe writes: the user may quit the pager
        // before we finish piping. That's a clean exit, not a fail.
        let _ = stdin.write_all(content.as_bytes());
    }

    let status = child.wait().context("pager did not exit cleanly")?;
    if !status.success() {
        anyhow::bail!("pager `{}` exited with {}", cmd, status);
    }
    Ok(())
}
