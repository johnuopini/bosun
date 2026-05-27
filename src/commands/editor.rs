//! `bosun editor [<cmd>]` — view or set the external editor that the
//! `e` key in the TUI launches against a highlighted session's path.
//!
//! With no argument: print the currently-configured editor (or
//! `(unset)` if `config.toml` has no `editor = "..."` line and the
//! user hasn't called `bosun editor <cmd>` yet). With an argument:
//! validate that it's non-empty and persist it via
//! `config::write_editor`. Pass an empty string (`bosun editor ""`)
//! to clear the field.
//!
//! Runs synchronously, prints to stdout, exits before any TUI
//! machinery starts — same pattern as `bosun update` and
//! `bosun release-notes`.

use anyhow::{Context, Result};

use crate::config::{self, Config};

/// Entry point invoked from `main.rs`. `arg` is the optional first
/// positional after `editor` (e.g. `bosun editor zed` → `Some("zed")`).
pub fn run(arg: Option<String>) -> Result<()> {
    match arg {
        None => {
            // No argument — just print the current value. We re-load
            // the config from disk rather than relying on anything
            // cached so this works in any process state.
            let cfg = Config::load();
            match cfg.editor.as_deref() {
                Some(e) => println!("{e}"),
                None => println!("(unset) — try `bosun editor zed` or `bosun editor code`"),
            }
            Ok(())
        }
        Some(cmd) => {
            let trimmed = cmd.trim();
            // Empty string is the "clear" sentinel. Anything else is
            // written verbatim — we don't try to validate that the
            // command exists on PATH because (a) PATH may differ
            // between the bosun process and the user's GUI launcher
            // env, and (b) the TUI surfaces the spawn error as a
            // status-bar warning if the command turns out to be bad.
            if trimmed.is_empty() {
                config::write_editor(None).context("write config.toml")?;
                println!("editor cleared");
            } else {
                config::write_editor(Some(trimmed)).context("write config.toml")?;
                println!("editor set to: {trimmed}");
                println!("(restart bosun for the TUI to pick up the change)");
            }
            Ok(())
        }
    }
}
