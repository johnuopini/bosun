# Changelog

All notable changes to bosun are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the project
uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.3.1] — 2026-05-15

### Fixed
- CI: `cargo fmt` and `cargo clippy -D warnings` formatting / lint nits
  in v0.3.0 that broke the release workflow on tag push. No
  user-visible behavior change.

## [0.3.0] — 2026-05-15

### Added
- **MRU session cycle bindings.** `shift+←` and `shift+→` (prefix-less)
  jump between recently-attached sessions without going back to the
  bosun TUI. Left toggles to the previous session; right walks one
  step further back in MRU order. `__bosun_monitor` is excluded so the
  cycle never lands on the internal control-mode session.
- **Quick-jump session picker (tmux).** `prefix + o` opens a floating
  tmux popup running `choose-tree -Zs` — type-ahead fuzzy session
  chooser. Enter switches, Escape closes. `__bosun_monitor` is
  filtered out.
- **Quick-jump session picker (TUI).** Press `/` in the bosun session
  list to open a centered modal with a live filter. Matches against
  session display name, agent, and path. Up/Down to highlight, Enter
  attaches, Esc cancels.

### Changed
- **Status bar overhaul.** Bar now reads
  `⚓ bosun · <current-session-name>` on the left and
  `^Q detach · S-←→ cycle · C-a 1-9 jump` on the right. The current
  session name is pulled from each session's `@bosun_display` user
  option (fallback to `#S`). Replaced the per-session chip strip
  (`1:foo 2:bar …`) which became unhelpful past ~5 sessions. The
  prefix+1..9 jump bindings still install and work — just no longer
  rendered in the bar.

### Internal
- `attach.rs` lifecycle now owns three runtime binding sets: the
  existing `C-q detach`, the new `S-Left`/`S-Right` cycle, and the new
  `prefix + o` quick-jump. All three self-heal on every refresh tick
  via `do_refresh` and are torn down on `GlobalsGuard::drop` plus the
  panic-hook `emergency_unbind`.
- Format-expansion escaping: tmux re-expands `run-shell` and
  `display-popup` arguments at trigger time, so the bindings that
  contain `#{…}` formats now double them as `##{…}` to survive the
  expansion intact. Documented inline next to each binding.
- `Command::Attach` from a closing modal is now intercepted by the app
  loop and re-routed to `pending_attach` (the actor ignores
  `Command::Attach` — only the app loop can do the tty handover).

## [0.2.14] — 2026-05-14

Last release before the navigation overhaul. See git history for
prior changes.
