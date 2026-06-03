# Bosun 2.0 — Open-Question Research

**Branch:** `2.0`
**Date:** 2026-05-27
**Companion docs:** [`EMBEDDED_TERMINAL_FEASIBILITY.md`](./EMBEDDED_TERMINAL_FEASIBILITY.md), [`PLAN_2_0.md`](./PLAN_2_0.md)

Three decision-relevant questions from `PLAN_2_0.md` Step 1: (1) tmux window-size negotiation under `attach -r`, (2) `tui-term` maintenance posture, (3) parser choice between `vt100`, `alacritty_terminal`, and `wezterm-term`.

---

## Q1 — tmux window-size negotiation under `attach -r`

**Source:** [`tmux.1` from tmux/tmux master](https://github.com/tmux/tmux/blob/master/tmux.1) — same source rendered by [OpenBSD man.1](https://man.openbsd.org/tmux.1).

### Is `-r` purely about input, or does it touch sizing?

It touches sizing. In current tmux source, `-r` is defined as:

> `-r` is an alias for `-f read-only,ignore-size`. When a client is read-only, only keys bound to the `detach-client` or `switch-client` commands have any effect.

And the `ignore-size` client flag is documented as:

> `ignore-size` — the client does not affect the size of other clients.

So `-r` does two distinct things in one switch: read-only input **and** the size-isolation flag. The latter is the lever we want. It can also be set separately via `-f ignore-size` (without read-only), or toggled at runtime via `switch-client -r` which "toggles the client `read-only` and `ignore-size` flags". This means a Bosun-spawned client could attach as `-f ignore-size` *without* `-r` later for Level 2 focus mode, instead of detaching and re-attaching.

### `set-option -g window-size <largest|smallest|latest|manual>` — verbatim

> Configure how tmux determines the window size. If set to `largest`, the size of the largest attached session is used; if `smallest`, the size of the smallest. If `manual`, the size of a new window is set from the `default-size` option and windows are resized automatically. With `latest`, tmux uses the size of the client that had the most recent activity. See also the `resize-window` command and the `aggressive-resize` option.

Companion option `default-size XxY` (used by `manual`):

> Set the default size of new windows when the `window-size` option is set to manual or when a session is created with `new-session -d`. The value is the width and height separated by an `x` character. The default is 80x24.

### `setw -g aggressive-resize on` — verbatim

> Aggressively resize the chosen window. This means that tmux will resize the window to the size of the smallest or largest session (see the `window-size` option) for which it is the current window, rather than the session to which it is attached. The window may resize when the current window is changed on another session; this option is good for full-screen programs which support SIGWINCH and poor for interactive programs such as shells.

`aggressive-resize` operates per-window inside the *same* session size policy. It does not bypass `window-size`; it just changes which session the window honors when a window is linked into multiple sessions. For our single-session-attach scenario it is essentially a no-op — relevant only if Bosun ever links one window into multiple sessions for broadcast purposes.

### Configuration that lets a small read-only client attach without shrinking the large client

Yes. The `ignore-size` client flag (which `-r` already sets) means *this client does not count toward window-size negotiation at all*. So with the default `window-size latest` policy or with `largest`, the real user's full-size attach drives the window dimensions and the Bosun preview attach is ignored for sizing purposes. Bosun's PTY will then receive a stream sized to the real client's geometry, which the embedded `vt100::Parser` must be told to match (the parser's grid size, not the PTY's, is what matters for rendering — and the PTY can stay sized to whatever Bosun renders into).

Recommended combo:

- Spawn with `tmux attach -r -t <session>` (read-only + ignore-size, both included in `-r`).
- Leave `window-size` at its default `latest`, or set `largest` belt-and-braces.
- Do **not** set `aggressive-resize on`; it's orthogonal and risks resize churn on window-change.

This is also what `switch-client -r`'s ability to *toggle* `ignore-size` implies — tmux exposes the flag as a first-class concept precisely for cases like ours.

### Bonus: does `tmux attach -x <cols> -y <rows>` work?

**No.** This was a premise error in the question. In the `attach-session` synopsis, `-x` means SIGHUP-the-parent-on-detach:

> If `-x` is given, send SIGHUP to the parent process of the client as well as detaching the client, typically causing it to exit.

There is no `-y` on `attach-session`. The `-x width / -y height` flags exist on **`new-session`** ("With `-d`, the initial size comes from the global `default-size` option; `-x` and `-y` can be used to specify a different size"), not `attach-session`. For an existing session there is no attach-time geometry override; size comes from negotiation among non-ignore-size clients. (`refresh-client -C widthxheight` exists but is documented as "sets the width and height of a control mode client" — i.e. only useful if Bosun later runs `-C` control-mode, not for a plain PTY attach.)

**Bottom line for Q1:** the `ignore-size` flag (already implied by `-r`) is the entire answer. Bosun's `-r` preview attach should not affect the real user's geometry. Validate empirically in the Step 1 spike anyway, because window-size policy on the user's machine is configurable.

---

## Q2 — `tui-term` maintenance status

**Sources:** [crates.io API for tui-term](https://crates.io/api/v1/crates/tui-term), [GitHub a-kenji/tui-term](https://github.com/a-kenji/tui-term).

- **Latest published:** `0.3.4`, **published 2026-04-07** by `a-kenji` (Kenji Berthold). 21 versions; first published 2023-05-17. ([crates.io](https://crates.io/crates/tui-term))
- **Last commit on default branch:** 2026-05-02 (`chore(clippy): Apply clippy suggestions`, PR #371). Repo last pushed 2026-05-02. Not archived.
- **Compatibility with our target stack:** `tui-term 0.3.4` declares runtime deps on `ratatui-core ^0.1.0`, `ratatui-widgets ^0.3.0`, `portable-pty ^0.9.0`, and **`vt100 ^0.16.2`**. The dev-deps include `ratatui ^0.30.0` and `crossterm ^0.29`. So Bosun's planned `ratatui = "0.30"` + `vt100 = "0.16"` line up with what upstream actually tests against — no version-pinning gymnastics required.
- **Open issues:** 2 (per GitHub API):
  - [#365](https://github.com/a-kenji/tui-term/issues/365) — feature request: gate the `block` functionality behind an optional feature.
  - [#258](https://github.com/a-kenji/tui-term/issues/258) — doc improvement.
  - Neither is a correctness blocker for our workload.
- **Maintainers / bus factor:** Single primary maintainer, `a-kenji`. Recent activity is dependabot PRs + clippy cleanups + the maintainer responding to issues; healthy but one-person. Bus factor: 1.
- **License:** MIT. Compatible with Bosun.

### Verdict

Safe to depend on for Level 1. The widget is small (66 KB crate, single `pseudoterminal::PseudoTerminal` widget over a `vt100::Parser`), so if `a-kenji` ever stops maintaining it the *vendor-and-fix* path is cheap — we'd be inheriting maybe a few hundred lines of glue. Recommend pinning to `=0.3.4` (or `~0.3.4`) in `Cargo.toml` plus keeping an internal `terminal_widget.rs` module that owns the `tui_term` import surface, so swapping to a vendored copy is a one-file change.

---

## Q3 — `vt100` vs `alacritty_terminal` vs `wezterm-term`

**Sources:** crates.io API, GitHub repo metadata for each.

### `vt100` ([doy/vt100-rust](https://github.com/doy/vt100-rust))

- **Latest:** `0.16.2`, published **2025-07-12** by `doy` (Jesse Luehrs). ([crates.io](https://crates.io/crates/vt100))
- **Repo last pushed:** 2025-07-12. **10 months idle.** Not archived. 113 stars.
- **Open issues:** 14, including a couple worth flagging for our workload:
  - [#28](https://github.com/doy/vt100-rust/issues/28) — `Row::clear_wide` panic on resize-truncate of wide char at last column.
  - [#17](https://github.com/doy/vt100-rust/issues/17) — Missing modes in `MouseProtocolEncoding`. Direct hit on PLAN_2_0 Step 4 requirements (SGR 1006, focus 1004).
  - [#26](https://github.com/doy/vt100-rust/issues/26) — Missing HVP (`CSI f`); treated as unhandled.
- **Throughput:** No published bench. The architecture is single-pass byte-by-byte through a small state machine into a `Screen` (Vec-of-rows-of-cells). Anecdotally fast enough for hundreds of MB/s — `tui-term`'s `criterion`/`divan` benches in its repo are the closest published numbers.
- **Mouse / bracketed paste / focus:** Bracketed paste is parsed. SGR mouse encoding 1006 is in `MouseProtocolEncoding` *but* issue #17 says modes are incomplete. Focus events (`1004`) are best-effort.
- **Ratatui integration:** `tui-term` (above) is the bridge — already production-grade glue.
- **License:** MIT. Compatible.

### `alacritty_terminal` ([alacritty/alacritty `alacritty_terminal` crate](https://github.com/alacritty/alacritty/tree/master/alacritty_terminal))

- **Latest:** `0.26.0`, published **2026-04-06**. ([crates.io](https://crates.io/crates/alacritty_terminal))
- **Repo:** alive (last push 2026-05-23, 64k stars, used by Alacritty + Zed editor).
- **Throughput:** This is the parser that drives a real GPU terminal at hundreds of MB/s sustained. Not a question.
- **Mouse / bracketed paste / focus:** Full support; this is what an actual terminal emulator ships.
- **Ratatui integration:** **No published widget.** We'd write the bridge (walk the `Term` grid, emit `ratatui::buffer::Cell`s with SGR translated). Estimate: a few hundred lines, but not throwaway — needs care around alt screen, scrollback, and dirty-region tracking.
- **License:** Apache-2.0. Compatible with Bosun's MIT (Apache-2.0 → MIT relicense is fine for an MIT-licensed consumer).

### `wezterm-term` ([wezterm/wezterm `term` crate](https://github.com/wezterm/wezterm/tree/main/term))

- **Not published on crates.io under that name.** The `wezterm-term` slot on crates.io does not exist; only a community fork [`tattoy-wezterm-term`](https://crates.io/crates/tattoy-wezterm-term) (0.1.0-fork.5, 2025-07) is published. The first-party crate lives in the wezterm workspace and would have to be consumed as a git dependency.
- **License:** MIT (verified from [wezterm's `term/Cargo.toml`](https://github.com/wezterm/wezterm/blob/main/term/Cargo.toml) and root `LICENSE.md`). GitHub's API reports NOASSERTION at the repo root because of how multiple files claim the license, but the package itself is MIT.
- **Repo:** alive (last push 2026-05-01, 26k stars, 1737 open issues — typical for a sprawling project).
- **Mouse / bracketed paste / focus:** Very complete; supersedes vt100 in feature coverage.
- **Ratatui integration:** None published. Would write the bridge ourselves; harder than `alacritty_terminal` because the dependency graph is larger (pulls in `image`, `termwiz`, etc.).

### Recommendation

**Use `vt100` + `tui-term` for Level 1. Plan to swap to `alacritty_terminal` if Level 2 lands on the roadmap.**

Reasoning:

- For a read-only preview pane (Step 3 in `PLAN_2_0.md`), vt100's coverage is sufficient and `tui-term` removes the entire integration cost. The 10-month idle gap on `doy/vt100-rust` is a yellow flag but `tui-term` is actively maintained and tracking `vt100 0.16.2`, so the de-facto stewardship is healthier than the repo dates suggest.
- For Level 2 focus mode (Step 4), `vt100` issues #17 (mouse encoding modes) and #26 (HVP) become real correctness problems. By that point we'd want `alacritty_terminal`. Writing the ratatui bridge is the cost; in exchange we get a parser that's tested by a real terminal emulator at scale.
- `wezterm-term` offers slightly higher fidelity than `alacritty_terminal` for niche sequences (images we don't need, ligatures we don't need) but is heavier, has a larger dep graph, and isn't published on crates.io under a name we'd want to depend on. Skip.

Concretely:

| Decision point | Pick |
|---|---|
| Step 1 spike | `vt100 = "0.16"` + `tui-term = "0.3.4"` |
| Step 3 Level 1 ship | same |
| Step 4 Level 2 ship | reassess; if mouse / SGR-1006 / focus 1004 correctness bites, swap to `alacritty_terminal = "0.26"` with a hand-written ratatui bridge |

Keep the `EmbedTerminal` module's public surface parser-agnostic so the swap stays one-file.

---

## Caveats / things I couldn't pin down

- **Empirical confirmation of `ignore-size`** under tmux 3.6 with a small read-only client + large real client: docs are clear, but the Step 1 spike should still confirm it on the user's actual machine. Uncertain — best evidence is the man-page text quoted above plus tmux's own `switch-client -r` toggle semantics.
- **`vt100` throughput in absolute numbers**: no first-party bench published. Best evidence is `tui-term`'s own criterion benches in [its `benches/` directory](https://github.com/a-kenji/tui-term/tree/main/benches), which exercise vt100 indirectly.
- **GitHub `open_issues_count`** includes PRs; the per-issue lists above filter PRs out.
