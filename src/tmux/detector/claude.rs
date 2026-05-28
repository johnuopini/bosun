//! Claude Code detector.
//!
//! Strategy: Claude Code has a recognizable bottom-of-screen UI (a
//! box-drawn prompt with `╭─/│ > /╰─`) and emits known patterns
//! around it. Confining most heuristics to the bottom region of the
//! visible capture is the single biggest reliability win — it filters
//! out stale "Thinking…" strings sitting in scrollback above the
//! prompt and stops random shell output from triggering false
//! positives.
//!
//! Stack, cheapest first:
//!   1. Strong Claude anchor: the prompt-box corners (`╭` … `╮` …
//!      `╰` … `╯`) appear in the bottom region. Falls back to the
//!      classic substring anchors if the box isn't currently visible
//!      (e.g. during a full-screen response).
//!   2. Confirmation prompts ("Do you want to", `❯` option marker,
//!      `(y/n)`) anywhere on screen → Waiting.
//!   3. Active spinner: braille glyph in the OSC title OR a
//!      "Thinking / Pondering / …" verb with ellipsis in the bottom
//!      region → Running.
//!   4. Idle fallback when none of the above fire.
//!
//! If the pane doesn't look like Claude at all, return Unknown so
//! the registry falls through to the next detector.
//!
//! Tested against real Claude Code `capture-pane -e` fixtures under
//! `tests/fixtures/detector/`.

use super::{DetectContext, Status, StatusDetector};

/// How many trailing non-empty lines to consider "the prompt region."
/// Claude's prompt box is 3 lines; the surrounding hint / spinner /
/// confirmation rows live within ~10 lines of it. Wider than that and
/// we start matching stale output again.
const BOTTOM_REGION_LINES: usize = 12;

pub struct ClaudeDetector;

impl StatusDetector for ClaudeDetector {
    fn name(&self) -> &'static str {
        "claude"
    }

    fn priority(&self) -> u8 {
        100
    }

    fn detect(&self, ctx: &DetectContext<'_>) -> Status {
        let bottom = bottom_region(ctx.plain);

        if !looks_like_claude(ctx.plain, &bottom) {
            return Status::Unknown;
        }

        // Prompt markers — user needs to answer. Scoped to the bottom
        // region so a "(y/n)" in code output doesn't pin the glyph to
        // Waiting forever.
        if has_prompt_marker(&bottom) {
            return Status::Waiting;
        }

        // Thinking / spinner markers — busy. Spinner title scan stays
        // whole-capture (the OSC sequence lives outside the visible
        // grid); the verb scan is bottom-region only.
        if has_thinking_marker(&bottom) || has_spinner_title(ctx.ansi) {
            return Status::Running;
        }

        // Looks like Claude but no explicit marker. Two cases:
        //   - The prompt box is visible at the bottom → Waiting for
        //     the user to type.
        //   - The bottom is shell output → Idle (Claude exited and
        //     we're back at a prompt).
        if has_prompt_box(&bottom) {
            return Status::Waiting;
        }

        let last = tail_non_empty_line(ctx.plain);
        if last.is_empty() || last.trim_start().starts_with('>') || last.contains('❯') {
            return Status::Waiting;
        }

        Status::Idle
    }
}

/// Last N non-empty lines of `plain`, joined with `\n` in source
/// order. Cheap to re-scan in subsequent helpers and small enough
/// that substring checks stay free.
fn bottom_region(plain: &str) -> String {
    let mut lines: Vec<&str> = plain
        .lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take(BOTTOM_REGION_LINES)
        .collect();
    lines.reverse();
    lines.join("\n")
}

fn looks_like_claude(plain: &str, bottom: &str) -> bool {
    // Strongest signal: Claude's box-drawn prompt corners visible in
    // the bottom region. Unique enough that no plain shell pane will
    // hit it by accident.
    if bottom.contains('╭') && bottom.contains('╰') {
        return true;
    }
    // Fall back to broader anchors for transient states where the
    // box isn't currently rendered (full-screen modal, splash, etc.).
    plain.contains("? for shortcuts")
        || plain.contains("▐▛███▜▌") // splash art
        || plain.contains("Claude Code")
}

fn has_prompt_marker(region: &str) -> bool {
    const PROMPTS: &[&str] = &[
        "Do you want to",
        "Would you like to",
        "Choose an option",
        "Press any key to continue",
        "(y/n)",
        "(Y/n)",
        "(y/N)",
    ];
    // `❯` is Claude's selected-option arrow in confirmation menus.
    // Restricting it to the bottom region avoids false positives from
    // prompts pasted into the conversation.
    if region.contains('❯') {
        return true;
    }
    PROMPTS.iter().any(|p| region.contains(p))
}

fn has_thinking_marker(region: &str) -> bool {
    // Claude prints rotating verbs: "Thinking", "Pondering", etc., often
    // preceded by a `✻` / middle-dot bullet. We look for any of the
    // common verbs combined with an ellipsis to avoid false positives
    // from normal text. Scoped to the bottom region so a "Thinking…"
    // line that scrolled past the prompt no longer pegs the glyph to
    // Running.
    const VERBS: &[&str] = &[
        "Thinking",
        "Pondering",
        "Reviewing",
        "Synthesizing",
        "Computing",
        "Formulating",
        "Contemplating",
        "Analyzing",
        "Reasoning",
        "Crafting",
        "Considering",
        "Working",
    ];
    VERBS
        .iter()
        .any(|v| region.contains(&format!("{}…", v)) || region.contains(&format!("{}...", v)))
}

/// True iff the bottom region currently shows Claude's prompt box —
/// at minimum a `│ >` row sandwiched between `╭` and `╰` corners.
fn has_prompt_box(region: &str) -> bool {
    let has_top = region.contains('╭');
    let has_bot = region.contains('╰');
    let has_input = region
        .lines()
        .any(|l| l.trim_start().starts_with("│ >") || l.trim_start().starts_with("│>"));
    has_top && has_bot && has_input
}

fn has_spinner_title(ansi: &[u8]) -> bool {
    // OSC 0 / OSC 2 titles look like `ESC ] 0 ; <title> BEL` or
    // `ESC ] 0 ; <title> ESC \`. Scan the raw bytes for a title
    // containing braille spinner glyphs (U+2800..U+28FF).
    let s = String::from_utf8_lossy(ansi);
    let mut in_title = false;
    let mut title = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.next() == Some(']') {
                // consume "0;" or "2;"
                for _ in 0..2 {
                    chars.next();
                }
                in_title = true;
                title.clear();
            }
        } else if in_title {
            if c == '\x07' || c == '\x1b' {
                if title.chars().any(is_braille) {
                    return true;
                }
                in_title = false;
            } else {
                title.push(c);
            }
        }
    }
    false
}

fn is_braille(c: char) -> bool {
    ('\u{2800}'..='\u{28ff}').contains(&c)
}

fn tail_non_empty_line(plain: &str) -> &str {
    plain
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use super::*;
    use crate::tmux::detector::{strip_ansi, DetectContext};

    fn ctx_plain(s: &str) -> DetectContext<'_> {
        let now = SystemTime::now();
        DetectContext::from_parts(s.as_bytes(), s, Some(now), now, None, "test")
    }

    #[test]
    fn non_claude_returns_unknown() {
        let ctx = ctx_plain("$ ls -la\ntotal 42\n");
        assert_eq!(ClaudeDetector.detect(&ctx), Status::Unknown);
    }

    #[test]
    fn prompt_box_alone_is_waiting() {
        // Steady-state Claude UI: prompt box visible, no spinner.
        let pane = "\
Claude Code v2.1.152
some prior output

╭──────────────────────────────╮
│ >                             │
╰──────────────────────────────╯
  ? for shortcuts
";
        let ctx = ctx_plain(pane);
        assert_eq!(ClaudeDetector.detect(&ctx), Status::Waiting);
    }

    #[test]
    fn confirm_prompt_yields_waiting() {
        let pane = "\
Claude Code v2.1.152
running tool…

Do you want to proceed? (y/n)
❯ 1. Yes
  2. No
";
        let ctx = ctx_plain(pane);
        assert_eq!(ClaudeDetector.detect(&ctx), Status::Waiting);
    }

    #[test]
    fn thinking_in_bottom_region_yields_running() {
        let pane = "\
Claude Code session
some output

✻ Thinking… (3s · esc to interrupt)
╭──────────────────────────────╮
│ > my prompt                   │
╰──────────────────────────────╯
";
        let ctx = ctx_plain(pane);
        assert_eq!(ClaudeDetector.detect(&ctx), Status::Running);
    }

    #[test]
    fn stale_thinking_in_scrollback_does_not_trigger_running() {
        // "Thinking…" appears far above the visible prompt, simulating
        // a stale line that scrolled almost-off (still in the visible
        // capture but well outside the bottom region). The new
        // detector should treat the pane as Waiting because the
        // prompt box is what's actually live.
        let pane = format!(
            "Claude Code v2.1.152\n· Thinking…\n{}\n╭──────╮\n│ >     │\n╰──────╯\n",
            "filler line\n".repeat(20)
        );
        let ctx = ctx_plain(&pane);
        assert_eq!(ClaudeDetector.detect(&ctx), Status::Waiting);
    }

    #[test]
    fn spinner_title_yields_running() {
        // Build the byte stream by hand: OSC title with a braille
        // spinner glyph, followed by a Claude prompt box. Mixing
        // multi-byte UTF-8 into a byte string literal isn't allowed,
        // so we concatenate.
        let mut ansi: Vec<u8> = Vec::new();
        ansi.extend_from_slice(b"Claude Code\n\x1b]0;\xe2\xa0\x8b Working\x07");
        ansi.extend_from_slice("╭──╮\n│ > │\n╰──╯".as_bytes());
        let plain = strip_ansi(&ansi);
        let now = SystemTime::now();
        let ctx = DetectContext::from_parts(&ansi, &plain, Some(now), now, None, "test");
        assert_eq!(ClaudeDetector.detect(&ctx), Status::Running);
    }

    #[test]
    fn bare_prompt_line_is_waiting() {
        // Pre-box fallback: still recognize Claude via the splash /
        // shortcuts anchor + the trailing `>` line.
        let ctx = ctx_plain("Claude Code session\n? for shortcuts\n\n> \n");
        assert_eq!(ClaudeDetector.detect(&ctx), Status::Waiting);
    }

    #[test]
    fn settled_output_is_idle() {
        let ctx = ctx_plain("Claude Code session\nDone.\n$ ");
        // "$ " is non-prompt shell line — plain terminal state after exit
        let got = ClaudeDetector.detect(&ctx);
        assert!(
            got == Status::Idle || got == Status::Unknown,
            "expected Idle or Unknown, got {:?}",
            got
        );
    }
}
