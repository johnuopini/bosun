//! Subcommands invoked from `main.rs` argv parsing. Kept separate
//! from the library surface in `lib.rs` so the TUI code path doesn't
//! pull in HTTP / archive-extraction deps unless `bosun update` is
//! actually invoked.

pub mod editor;
pub mod release_notes;
pub mod update;
