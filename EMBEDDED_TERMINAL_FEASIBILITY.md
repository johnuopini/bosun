# Embedded Terminal in Bosun — Feasibility Study

**Date:** 2026-05-27
**Status:** Research only — no code changes yet
**Author:** Andy Miller (with Claude as research assistant)

## Why this exists

Bosun today is a tmux orchestrator: select a session, jump out into `tmux attach`,
do the work, come back. The "preview pane" is a periodic snapshot of the session
contents (effectively `tmux capture-pane`), which is fine as a chooser hint but
caps at roughly 1 fps and has no color/cursor/scrollback fidelity.

After running across **Herdr** — a terminal-native agent multiplexer that runs
real PTYs inside its own TUI — the question is whether Bosun could replace its
snapshot preview with a real embedded terminal, so the same pane that today
shows a stale screenshot becomes a live, interactive view of the underlying
tmux session. No more "jump out to tmux to actually work."

The goal of this doc is to capture the findings so a future implementation
session can start from a known baseline.

## Stack comparison

| | Bosun (today) | Herdr |
|---|---|---|
| Language | Rust | Rust |
| UI | ratatui (TUI) | TUI |
| Multiplexer | None — shells out to `tmux attach` | Built-in — owns PTYs |
| Persistence | tmux | Its own daemon |
| Agent API | None | Newline-delimited JSON over Unix socket |
| License | MIT | AGPL-3.0-or-later (dual-licensed; commercial available) |

Good news: same stack. No language/runtime mismatch.

## Licensing — Herdr code reuse is off the table

**AGPL + MIT is the worst-case combo for code reuse.** Pulling AGPL'd source
into an MIT project forces the combined work to be relicensed AGPL. That's a
one-way door once contributors have agreed to MIT terms.

What's allowed:

- Reading Herdr's source for *architectural ideas* (concepts aren't copyrightable, only specific code expression).
- Using the same upstream crates Herdr uses — those are independently MIT/Apache-licensed.
- Spawning the `herdr` binary as a subprocess and talking to its socket API from Bosun (separate programs; FSF's "mere aggregation" position).

What's not allowed without relicensing Bosun to AGPL:

- Copying any function or file from Herdr's source.
- Vendoring Herdr as a Rust crate dep, or forking it.
- Statically linking any AGPL'd code into the Bosun binary.

**Conclusion: build clean-room on the same upstream crates Herdr uses. We don't
actually need Herdr's code — the building blocks are all permissively licensed
and well-documented.**

Future tripwire to remember: AGPL §13 ("network use is distribution") would
apply if a Herdr-bundled feature ever gained remote-access semantics
(SSH-in, web UI, network-exposed socket). Not an issue for local TUI use.

## The crate stack we'd actually use

All MIT or Apache-2.0 licensed:

- **`portable-pty`** (wezterm) — spawn PTYs, own their fds, handle resize. The de-facto standard in the Rust ecosystem.
- **`vt100`** — pure-Rust ANSI parser that turns a PTY byte stream into a virtual terminal grid (cursor, SGR attributes, alt screen, scrollback). Easiest integration, good enough for most workloads.
- **`tui-term`** — ratatui widget that wraps a `vt100::Parser` and renders the grid into a ratatui `Buffer`. This is the *exact* missing piece between Bosun's current preview and a live embedded terminal.
- **Optional upgrade path:** `wezterm-term` (higher fidelity, heavier) or `alacritty_terminal` (highest fidelity, what Alacritty/Zed use). Drop in if `vt100` proves too slow or too lossy.

Existence proofs that the architecture works at production quality: **mprocs**
and **zellij** are both Rust TUI multiplexers built on this pattern. Both feel
native-fast.

## Architecture — three escalating levels

We picked Level 2 ("tmux + custom renderer") as the target. Levels 1 and 3 are
documented for context.

### Level 1 — Live read-only preview pane

- Replace snapshot polling with a real embedded terminal in the preview pane.
- Spawn a PTY that runs `tmux attach -r -t <session>` (read-only attach).
- Feed PTY bytes into `vt100::Parser`, render with `tui-term`.
- Streaming reads; no polling; full color/cursor/scrollback.
- **Effort:** ~1 week. No architectural changes. tmux still owns persistence.
- **Payoff:** preview pane goes from stale screenshot to live, real-time view.

### Level 2 — Focusable embedded terminal (recommended target)

- Everything in Level 1, plus a "focus" keybind that switches the embedded pane to read-write.
- Forward crossterm key events to the PTY's input fd (re-encoded as the right escape sequences).
- Handle resize, mouse mode toggling, bracketed paste, alt-screen apps.
- **Effort:** Additional 1-3 weeks on top of Level 1.
- **Payoff:** Bosun *is* the terminal UI for the active session. No more jumping out to tmux. tmux still does detach/reattach/SSH-resume — we keep Bosun's strongest feature.

### Level 3 — Full Herdr parity (workspaces + multiple agents + socket API)

- Add a "project" abstraction above sessions; N agents per project; tabs/splits owned by Bosun.
- Replace tmux entirely OR keep tmux but layer workspace metadata on top.
- Build a socket API so agents can drive their own panes.
- **Effort:** Months. Owning the multiplexer correctness story (pane resize, scrollback eviction, alt-screen edge cases) is a big commitment.
- **Verdict:** Only worth it if we'd rather be Zellij than Bosun. **Not recommended.**

## Why "1 fps" is not the actual cap

The current preview's ~1 fps comes from the *polling/snapshot* model
(`tmux capture-pane` on a timer). A true embedded terminal isn't polled and
has no fps ceiling.

**Polling model (current):**

```
timer → tmux capture-pane → parse text grid → render → repeat
```

**Streaming model (proposed):**

```
spawn PTY running `tmux attach -r -t <session>`
async loop: read bytes from PTY fd → vt100::Parser → mark dirty
render loop (60Hz cap): if dirty, walk grid → tui-term widget → ratatui diff
```

tmux relays its child process's output in real time during `attach` — that's
tmux's actual job. The `-r` read-only flag doesn't change the relay model.
Bosun reads bytes the instant they arrive; the parser/renderer is byte-streaming,
not sampling.

### Real performance risks (in rough order of likelihood)

1. **Burst floods.** Claude dumping a 500KB diff, `cargo build` warnings, etc. `vt100::Parser` chews through this fine (hundreds of MB/s) but a naive "re-render on every byte" loop can hitch. **Mitigation:** read on a dedicated reader (thread or tokio task), coalesce updates, cap redraws at 60Hz.
2. **Parser choice.** `vt100` has rough edges around mouse modes, bracketed paste, and some uncommon sequences. **Mitigation:** if the spike falls short, drop to `wezterm-term` or `alacritty_terminal`. Alacritty handles tens of MB/s sustained at hundreds of fps.
3. **Ratatui render cost on a big pane.** 80x24 ≈ 2000 cells — trivial. 200x60 ≈ 12000 — still fine; ratatui only flushes diffs to the user's terminal.
4. **tmux relay hop.** Adds ~1-2ms localhost round-trip on top of native. Imperceptible.

Things we explicitly do *not* pay for:

- Snapshot/capture cost (no snapshots — streaming)
- JSON or protocol parsing (raw PTY bytes)
- IPC overhead (PTY `read()` is the same cost as a normal terminal's)

## Recommended next steps

### Step 1 — Perf spike (½ day)

Stand up a throwaway ratatui demo that:

1. Uses `portable-pty` to spawn `tmux attach -r -t <some-session>`.
2. Pipes the PTY byte stream into `vt100::Parser`.
3. Renders with `tui-term` inside a ratatui `Frame`.
4. Caps redraws at 60Hz; logs frame times and bytes-per-second to a file.

Bench against:

- `yes | head -100000` (worst-case flood) — most likely to expose hitching
- `cargo build -p some-heavy-crate` (realistic agent-style burst)
- A live Claude Code session doing real work (actual target workload)

Outcome: a go/no-go on `vt100` as the parser. If it hitches, retry with
`alacritty_terminal`.

### Step 2 — Level 1 implementation (~1 week)

Once the spike is green:

- Add a `terminal_pane` module owning the PTY + parser + widget.
- Replace the existing snapshot preview with the streaming widget.
- Make sure the preview rebuilds cleanly on session switch (kill PTY, respawn on new target).
- Handle terminal resize (propagate rows/cols to the PTY via `portable-pty`).

### Step 3 — Level 2 (focus mode)

- Add a focus state to the pane.
- When focused, forward crossterm `KeyEvent`s to the PTY's input fd, re-encoded.
- Mouse passthrough; alt-screen handling; bracketed paste.
- Exit-focus keybind (e.g. Ctrl-B Esc or a dedicated chord) to return control to Bosun's own keymap.

## Risks worth flagging

1. **Terminal correctness tail.** Mouse modes, bracketed paste, OSC sequences, true color, italics, alt-screen apps (`vim`, `less`, `fzf`) all have edge cases. tmux already handles these; pushing them through our own embed adds bug surface. Mitigation: lean on `alacritty_terminal` if `vt100` shows gaps.
2. **Theme coherence.** Bosun has 15 themes. The embedded pane gets its colors from the child process, not from Bosun. Need a design pass on borders/chrome around the embed so themes still feel cohesive.
3. **Image protocols.** Sixel, iTerm inline images, Kitty graphics — `vt100` doesn't support these well. Most agents don't emit them, so likely OK, but worth a one-line check during the spike.
4. **Input latency budget.** Every keystroke goes user → crossterm → PTY → tmux → child → tmux → us → vt100 → tui-term → ratatui → user's terminal. Locally still <16ms in practice, but slower than running tmux natively. Watch for this during the spike.

## Open questions to settle before implementation

- Keep tmux as the persistence layer indefinitely, or eventually move to our own daemon? Recommend: keep tmux. It's the moat.
- Do we want one embedded pane (the focused session) or multiple visible at once? Recommend: start with one; revisit splits in a later release.
- Do we expose a socket API for agents (Herdr-style)? Not in 2.0. Keep scope tight.
- Where does the focus-exit chord live in the keymap?

## TL;DR for future-Andy

- Same stack as Herdr (Rust + ratatui), but their AGPL means we build clean-room on the same upstream crates: `portable-pty` + `vt100` + `tui-term`.
- The 1 fps fear is about polling; a streaming PTY embed has no fps ceiling.
- Recommended target: **Level 2** — keep tmux as the session backend, build a custom embedded-terminal widget in Bosun for both preview and active work.
- First concrete step: half-day perf spike to validate `vt100` throughput against a noisy session. If green, ~1 week to Level 1, another 1-3 weeks to Level 2.
- Don't go to Level 3 (full Herdr parity). That's a different product.
