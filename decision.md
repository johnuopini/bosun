# Bosun 2.0 — Step 2 Decision

**Date:** 2026-05-27
**Branch:** `2.0`
**Result:** **Go on Step 3 (Level 1 embed).**

## What was tested

### Step 0 — focused-session fast preview tick at 200ms

Built and ran `target/release/bosun` against the user's live sessions.

> "preview is faster, doesn't feel 100% native fast, but faster than before"

Read: 5 fps polled preview is a clear improvement over the v0.x 1Hz
behavior, but there's still a perceptible gap between "fast polled
snapshot" and "live terminal". 5 fps isn't enough to close the embed
question on its own.

### Step 1 — `examples/spike_term.rs` against a real tmux session

Ran the spike against a live Claude Code session.

> "ran some concurrent stuff in claude and can't really tell the
> difference between native tmux and spike_term"

Read: the `portable-pty + vt100 + tui-term` stack delivers native-feel
performance on the target workload. `vt100`'s throughput is not a
limiter; `tui-term`'s rendering hits 60Hz; the architecture works.

No window-size negotiation hiccups were reported, consistent with the
`-r → ignore-size` finding in `RESEARCH_2_0.md`. (Empirical
confirmation against a deliberately larger second client is still
worth a one-off check before the Level 1 implementation lands, but
the architectural risk is now backed by both docs and a real run.)

## Decision

**Proceed to Step 3 (Level 1 embed).** The combination of evidence is
unambiguous: Step 0 helps but doesn't get us all the way; the spike
confirms the embed path gets us the rest of the way without
correctness compromises.

Step 0 stays in. It's cheap, useful even with the embed (e.g. for the
non-focused sessions whose preview still polls), and gives users
without the embed-enabled build a noticeable upgrade.

## What Step 0 retains

The fast preview tick is now load-bearing for two future cases the
embed doesn't cover:

1. **Sidebar hover / section-header preview** — the embed only runs
   for the focused session. Sections and the empty state still
   render section-preview tables, which don't need a PTY but benefit
   from the faster cadence elsewhere in the UI.
2. **Embed fallback** — if Step 3 hits a blocker on a specific user's
   tmux/terminal combo (think tmux 2.x, exotic terminal emulators
   without alt-screen, etc.), `BOSUN_PREVIEW_TICK_MS=200` plus
   `BOSUN_EMBED=off` (TBD config knob) keeps a working v0.x-ish
   preview for them.

## Step 3 scope reminder

From `PLAN_2_0.md`:

- New module `src/ui/embed_terminal.rs` owning PTY + vt100 parser +
  tui-term widget instance.
- Dedicated reader task (`spawn_blocking` over the master fd), bytes
  through mpsc, render loop coalesces to 60Hz.
- Lifecycle: spawn on session selection, kill on switch, prime
  parser with `capture-pane -p -S -` so scrollback survives.
- Resize handling: propagate preview-area `(rows, cols)` to the PTY
  and the vt100 parser on layout change.
- Replace the snapshot preview path in `src/ui/preview.rs` for the
  focused-session case only. Section / empty-state preview stays
  polled.

Estimate: ~1.5 weeks of focused work. Not a same-session task.
Modular though — the new module lands behind a feature flag or a
config knob so it can be toggled off if regressions show up in
preflight.

## Open follow-ups still warranted

- Empirical `attach -r` window-size confirmation: attach the spike at
  a small size, then attach a real client at a much larger size to
  the same session. Watch what happens to both displayed sizes. This
  is a one-off `tmux` verification, not a code task.
- Scrollback priming smoke test: when Step 3's `capture-pane -p -S -`
  prime is in place, switch between sessions rapidly and confirm
  pre-attach history shows up correctly in the embedded grid.
- `BOSUN_EMBED` opt-out knob: design before Step 3 lands so the
  fallback path is visible to users from day one of the embed beta.
