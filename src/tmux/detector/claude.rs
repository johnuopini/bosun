//! Claude Code detector.
//!
//! Heuristic stack, cheapest first:
//!   1. OSC title scan: Claude Code writes `ESC ] 0 ; ...` title updates.
//!      When it's waiting on a prompt, the title often contains braille
//!      spinner characters (e.g. ⠋⠙⠹). We look at the most recent OSC
//!      title in the capture.
//!   2. Trailing prompt markers in the plain text — "Do you want to"
//!      confirmation prompt, "❯" selection marker, etc.
//!   3. Thinking markers — Claude prints ``· Thinking…`` animations.
//!
//! If none match we return `Unknown` and let the generic detector take
//! over. Returning Unknown is fine — the registry will fall through.
//!
//! Tested against real Claude Code `capture-pane -e` fixtures under
//! `tests/fixtures/detector/`.

use super::{DetectContext, Status, StatusDetector};

pub struct ClaudeDetector;

impl StatusDetector for ClaudeDetector {
    fn name(&self) -> &'static str {
        "claude"
    }

    fn priority(&self) -> u8 {
        100
    }

    fn detect(&self, ctx: &DetectContext<'_>) -> Status {
        if !looks_like_claude(ctx.plain) {
            return Status::Unknown;
        }

        // Prompt markers — user needs to answer.
        if has_prompt_marker(ctx.plain) {
            return Status::Waiting;
        }

        // Thinking / spinner markers — busy.
        if has_thinking_marker(ctx.plain) || has_spinner_title(ctx.ansi) {
            return Status::Running;
        }

        // Looks like Claude but no explicit marker — use the last line
        // to guess between Waiting (empty prompt line) and Idle (shell).
        let last = tail_non_empty_line(ctx.plain);
        if last.is_empty() || last.trim_start().starts_with('>') || last.contains('❯') {
            return Status::Waiting;
        }

        Status::Idle
    }
}

fn looks_like_claude(plain: &str) -> bool {
    // Anchors unique enough that we won't collide with plain shell output.
    plain.contains("claude")
        || plain.contains("Claude")
        || plain.contains("? for shortcuts")
        || plain.contains("▐▛███▜▌") // Claude splash art
}

fn has_prompt_marker(plain: &str) -> bool {
    const PROMPTS: &[&str] = &[
        "Do you want to",
        "Would you like to",
        "Choose an option",
        "Press any key to continue",
        "(y/n)",
        "(Y/n)",
        "(y/N)",
    ];
    PROMPTS.iter().any(|p| plain.contains(p))
}

fn has_thinking_marker(plain: &str) -> bool {
    // Claude prints rotating verbs: "Thinking", "Pondering", etc., often
    // preceded by a middle-dot bullet. We look for any of the common
    // verbs combined with an ellipsis to avoid false positives from
    // normal text.
    const VERBS: &[&str] = &[
        "Thinking",
        "Pondering",
        "Reviewing",
        "Synthesizing",
        "Computing",
        "Formulating",
        "Contemplating",
        "Analyzing",
    ];
    VERBS
        .iter()
        .any(|v| plain.contains(&format!("{}…", v)) || plain.contains(&format!("{}...", v)))
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
    fn prompt_marker_yields_waiting() {
        let ctx =
            ctx_plain("Claude is ready\n> write some code\n\nDo you want to proceed? (y/n)\n");
        assert_eq!(ClaudeDetector.detect(&ctx), Status::Waiting);
    }

    #[test]
    fn thinking_yields_running() {
        let ctx = ctx_plain("claude is working\n· Thinking…\n");
        assert_eq!(ClaudeDetector.detect(&ctx), Status::Running);
    }

    #[test]
    fn spinner_title_yields_running() {
        let ansi = b"Claude\n\x1b]0;\xe2\xa0\x8b Working\x07$ ";
        let plain = strip_ansi(ansi);
        let now = SystemTime::now();
        let ctx = DetectContext::from_parts(ansi, &plain, Some(now), now, None, "test");
        assert_eq!(ClaudeDetector.detect(&ctx), Status::Running);
    }

    #[test]
    fn bare_prompt_line_is_waiting() {
        let ctx = ctx_plain("Claude Code session\n\n> \n");
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
