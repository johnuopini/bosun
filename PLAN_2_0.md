# Bosun 2.0 — Implementation Plan

**Branch:** `2.0`
**Status:** Active
**Companion doc:** `EMBEDDED_TERMINAL_FEASIBILITY.md`

## What 2.0 is about

A focusable embedded terminal in the preview pane — bytes streaming
from a real PTY, parsed by `vt100`, rendered with `tui-term` — so the
preview becomes live and the focused session becomes interactive from
inside bosun. tmux stays as the persistence/multiplexing backend.

This plan is the sequencing I'm executing. Each step has a concrete
deliverable and an explicit exit criterion so we can bail early if a
cheaper approach already meets the bar.

## Why this sequencing

The feasibility doc jumps straight to embedding. The 1 fps preview cap
is a polling artifact, not an architectural limit — `capture-pane` is
cheap (~1-3ms) and we only need to hit it faster for the *focused*
session. If a 200ms polled preview already feels live enough, Level 1
isn't worth the complexity it adds (PTY lifecycle, window-size
negotiation, scrollback priming, tui-term maintenance dependency).

Step 0 is the cheap A/B that proves or disproves that thesis before
we commit to weeks of embed work.

## Steps

### Step 0 — Focused-session fast polling (½ day)

**Goal:** prove or disprove that 4-5 fps polled preview already feels
live enough that the embed becomes a nice-to-have rather than a
necessity.

**Work:**
- Add `preview_tick_ms` to `Config` (default `200`).
- Split `tmux_actor`'s current 1Hz `preview_tick` into two timers:
  - `full_refresh_tick` (1000ms): existing `do_refresh` — list,
    detector, statusbar, smoothing. Unchanged.
  - `preview_fast_tick` (config, default 200ms): captures *only* the
    focused session's pane. Emits a new lightweight `AppMsg::PreviewRefreshed`.
- Add `AppMsg::PreviewRefreshed { name, bytes }`. The app handler
  updates just the preview-bytes Arc for that session, leaving status
  and the rest of the SessionView alone.
- Avoid double-firing: when `full_refresh_tick` ran, suppress the
  next `preview_fast_tick`. Or just let them both fire — `capture-pane`
  is cheap and the work is idempotent.

**Exit criterion:** Run for a day with `preview_tick_ms = 200`. If the
preview feels indistinguishable from "live", ship 2.0 with this + the
non-embed features and revisit the embed for a future release. If
there's still a visible "blink-update" feel, proceed to Step 1.

### Step 1 — Perf spike (½ day, parallel with Step 0 evaluation)

**Goal:** validate that the `portable-pty + vt100 + tui-term` stack
keeps up with realistic burst loads, and *answer the window-size
question* (which is the single biggest correctness unknown about the
whole approach).

**Work:**
- New `examples/spike_term.rs` (or `bins/spike-term`) in this repo.
  Standalone, not wired into the main binary.
- Spawn `tmux -L bosun attach -r -t <session>` via `portable-pty`.
  Stream bytes into `vt100::Parser`. Render with `tui-term` in a
  ratatui Frame. Cap redraws at 60Hz. Log frame times + bytes/sec to
  a file.
- Bench against three workloads:
  - `yes | head -100000` (worst-case flood)
  - `cargo build -p ratatui` (realistic burst)
  - A live Claude Code session (target workload)
- **The window-size test:** while the spike is attached read-only,
  open a real `tmux attach -t <same-session>` in another terminal at
  a much larger size. Observe what happens to the displayed size in
  both clients. Try `setw -g aggressive-resize on`, `set -g window-size
  largest|smallest|manual`. Document what works.

**Exit criterion:** vt100 sustains the bursts at 60Hz without
hitching, AND there's a tmux option combo that keeps a real attached
client at its native size while the bosun preview attaches read-only.
If either fails, see the contingency below.

### Step 2 — Decision (15 min)

**Goal:** record go/no-go in `decision.md` on this branch, citing the
specific evidence from Steps 0 and 1.

Possible outcomes:
1. **Step 0 alone is enough.** Ship 2.0 as a polish release + Step 0 +
   the "2.0 ideas" backlog (workspaces, broadcast, RPC, etc).
2. **Step 0 helps but the embed is still worth doing.** Proceed to
   Step 3.
3. **Step 1 hit a blocker.** Either fall back to `alacritty_terminal`
   for the spike, or pivot to a non-`-r` design (e.g. dedicated
   pre-attach for the preview), or drop the embed.

### Step 3 — Level 1 embed (~1.5 weeks)

Only execute if Step 2 says go.

**Work:**
- Add deps to main `Cargo.toml`: `portable-pty`, `vt100`, `tui-term`.
- New module `src/ui/embed_terminal.rs` owning:
  - PTY handle (`portable_pty::PtyPair`)
  - vt100 parser
  - tui-term widget instance
  - Reader task (dedicated tokio task; `spawn_blocking` for the
    blocking PTY read fd, push bytes through an mpsc channel).
- Lifecycle: spawn PTY on session selection, kill PTY on session
  switch or session-list change. Prime the parser with `capture-pane
  -p -S -` so scrollback is preserved across switches.
- Resize: when the preview rect changes, propagate `(rows, cols)` to
  the PTY via `portable-pty`.
- Replace the snapshot preview in `src/ui/preview.rs` for the focused
  session. Section / empty-state branches stay polled (they don't
  need a PTY).
- Theme integration: the embed renders into the buffer, but the
  surrounding chrome (borders, status hints) still comes from
  ratatui. Confirm theming feels coherent.

**Exit criterion:** smooth live preview at 60Hz with a real Claude
Code session, clean session-switch (no PTY leaks, no stale frames),
and no regressions in the status detector / statusbar.

### Step 4 — Level 2 focus mode (3-5 weeks)

Only execute if Step 3 is solid and we still want focus.

**Work:**
- New "focused" state on the embed pane. Bound to a key (proposal:
  `f` to enter, `Ctrl-B Esc` or `Ctrl-Q` to exit — the latter
  collides with the existing detach binding, needs a design pass).
- Swap the read-only attach for a real attach when entering focus.
  Two strategies:
  - (a) Detach the `-r` client and spawn a new client without `-r`.
    Simpler. One client at a time. Race window during the swap.
  - (b) Keep a long-lived non-`-r` client and gate input forwarding
    on focus state. Cleaner runtime but tmux attach-mode juggling is
    fiddly.
- Forward crossterm `KeyEvent` to PTY's input fd as the right escape
  sequences. Account for:
  - Cursor key application mode (`DECCKM`)
  - Application keypad mode (`DECPAM`)
  - SGR mouse mode (`1006`)
  - Focus in/out events (`1004`)
  - Bracketed paste (`2004`)
  - modifyOtherKeys / kitty keyboard (stretch)
- Exit-focus chord; restore bosun's keymap.

**Exit criterion:** `vim`, `claude`, `fzf`, `less` all work inside the
focused embed without obvious correctness regressions vs running them
in a real terminal.

## Risks called out from the review

These were flagged in the review of the feasibility doc. Each is
something we explicitly de-risk before committing to the next step.

1. **tmux `attach -r` window-size negotiation.** Probably the single
   biggest correctness risk. Validated in Step 1.
2. **Level 2 needs a real attach, not `-r`.** Designed for in Step 4.
3. **Scrollback is not free.** Prime parser with `capture-pane -p -S -`
   on session switch. Designed for in Step 3.
4. **`tui-term` maintenance.** Pinned + reviewed before Step 3 starts.
5. **Input correctness tail.** Acknowledged. Estimate for Step 4 is
   3-5 weeks, not the doc's optimistic 1-3.

## "2.0 ideas" backlog (not embed-related)

If Step 0 closes out the embed question or we want extra wins
alongside it, these are the candidates from the review:

- Workspaces — `bosun.toml` per project, auto-launch N agents with
  paths/args. Strongest "headline" feature after embed.
- Broadcast / macros — send keys to multiple selected sessions.
- Cross-session search (post-embed; needs the grid).
- Smarter status detection from the vt100 grid (post-embed).
- Headless RPC for editor integrations.
- Per-session token/cost telemetry (post-embed).
- Snapshot scrubber — keep last N captures, scrub backward.

Sequencing recommendation for 2.0 as a release: **Step 0 + embed
(Step 3, optionally Step 4) + workspaces** is the strongest narrative.
Broadcast/macros is the cheapest add and could ship alongside.

## Rollback

Every step is its own commit on the `2.0` branch. Step 0 in
particular is one config knob + a few dozen lines in `tmux_actor` —
trivial to revert if it misbehaves. The spike in Step 1 is throwaway
code in `examples/`, doesn't ship to users. Step 3/4 changes are
isolated to a new module + the preview module. Branch can be
abandoned without affecting main.
