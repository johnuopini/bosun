# Bosun

Tmux-native orchestrator for AI agent sessions. Written in Rust with ratatui.

Bosun is a reimagining of [agent-deck](https://github.com/yetidevworks/agent-deck) — the core workflow is the same (manage a set of tmux sessions running AI agents, with status, previews, and quick attach/detach) but the architecture is rebuilt around a few rules that keep it simple and robust:

- **Tmux is the source of truth.** Bosun polls tmux each tick. No shared database state to race on; multi-instance coexistence is trivial.
- **One actor owns tmux I/O. One task owns `AppState`.** No nested mutexes.
- **Pluggable status detection.** Claude is just one detector among many; you can drop in your own regex rules via config.
- **Opencode aesthetic.** Borderless, subtly shaded panels, bold accents, multi-modal dialogs with accent bars and depth.

This is a greenfield project under active development. The initial milestone ships a walking skeleton; later phases add previews, a new-session modal, recents, theming, and full lifecycle controls.

## Requirements

- Rust 1.80+ (tested on 1.94)
- tmux 3.x (tested on 3.6a)

## Build & run

```sh
cargo build --release
./target/release/bosun
```

During development:

```sh
cargo run
BOSUN_LOG=info cargo run    # enable tracing to stderr
```

## Phase 1 — walking skeleton (current)

What works right now:

- Lists running tmux sessions on the left
- Arrow keys / `j` / `k` navigate
- `Enter` attaches to the selected session (tears down the TUI, hands the tty to tmux, restores on return)
- `Ctrl-Q` inside a Bosun-managed attach detaches you back to Bosun without touching your tmux prefix
- `r` forces an immediate refresh
- `q` or `Ctrl-C` quits
- `Ctrl-Z` suspends Bosun normally; `fg` resumes cleanly

What's NOT yet implemented (tracked in the plan):

- Live pane preview on the right
- Status detection (running / waiting / idle / error)
- New-session / rename / kill modals
- Fuzzy search / recents
- Theming and config file
- SQLite metadata store

## Smoke test

```sh
# 1. Make sure you have a tmux session or two.
tmux new -d -s demo
tmux new -d -s scratch

# 2. Run Bosun. You should see both sessions listed.
cargo run

# 3. Arrow-key to `demo`, press Enter. You should be attached to tmux.
# 4. Press Ctrl-Q. You should be back in Bosun.
# 5. Confirm Bosun did not leave a stray binding behind:
tmux list-keys -T root | grep C-q || echo 'clean'

# 6. Suspend/resume job control should still work.
#    From inside Bosun, press Ctrl-Z, then `fg`.
```

## How the Ctrl-Q detach works

Bosun installs a temporary tmux root key-table binding just before each attach:

```
tmux bind-key -T root C-q detach-client
tmux attach-session -t <name>    # blocks until you detach
tmux unbind-key -T root C-q      # on return
```

A refcount kept in the tmux user option `@bosun_attach_refcount` makes this safe when two Bosun instances are both attached — only the last detach clears the binding. A `Drop` guard plus a panic hook make sure we never leave a dangling binding if Bosun crashes.

If you have your own `C-q` binding, Bosun will (Phase 5) detect the conflict on startup, save your binding, and restore it on exit. Configurable alternative detach keys will land alongside that work.

## Testing

```sh
cargo test                 # unit + snapshot tests
cargo clippy --all-targets -- -D warnings
cargo fmt --check

# Integration tests that spawn a real tmux server (needs tmux installed):
cargo test --features tmux-it
```

Snapshot tests use `insta`. When intentional UI changes cause failures:

```sh
cargo install cargo-insta    # once
cargo insta review           # accept/reject each diff
```

## Layout

```
src/
  main.rs                entry point, panic hook, terminal setup
  app.rs                 AppState, central event loop, attach orchestration
  events.rs              Command / AppMsg types
  error.rs
  tmux/
    client.rs            tokio::process wrapper (all tmux I/O lives here)
    parse.rs             pure parsers for tmux CLI output
    attach.rs            Ctrl-Q keytable + refcount + panic safety
    session.rs
  actors/
    tmux_actor.rs        owns TmuxClient, handles Commands
    poller.rs            periodic tick pump
    input_actor.rs       crossterm event stream -> AppMsg
  ui/
    mod.rs               draw(frame, state)
    layout.rs            region rects
    session_list.rs
    statusbar.rs
tests/
  snapshot_session_list.rs
```

## License

MIT
