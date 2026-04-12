//! Codex CLI detector.
//!
//! Heuristic stack, cheapest first:
//!   1. Plain-text anchors that identify the pane as a Codex session.
//!   2. Prompt markers — Codex waiting for user input.
//!   3. Activity markers — Codex actively working.
//!
//! If none match we return `Unknown` and let the generic detector take
//! over.

use super::{DetectContext, Status, StatusDetector};

pub struct CodexDetector;

impl StatusDetector for CodexDetector {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn priority(&self) -> u8 {
        90
    }

    fn detect(&self, ctx: &DetectContext<'_>) -> Status {
        if !looks_like_codex(ctx.plain) {
            return Status::Unknown;
        }

        // Prompt markers — user needs to answer.
        if has_prompt_marker(ctx.plain) {
            return Status::Waiting;
        }

        // Activity markers — busy working.
        if has_activity_marker(ctx.plain) || has_spinner_title(ctx.ansi) {
            return Status::Running;
        }

        // Looks like Codex but no explicit marker — use activity age
        // as a tie-breaker.
        if ctx.activity_age < std::time::Duration::from_secs(3) {
            return Status::Running;
        }

        Status::Idle
    }
}

fn looks_like_codex(plain: &str) -> bool {
    plain.contains("codex")
        || plain.contains("Codex")
        || plain.contains("OpenAI Codex")
        || plain.contains("codex-cli")
}

fn has_prompt_marker(plain: &str) -> bool {
    const PROMPTS: &[&str] = &[
        "Do you want to",
        "Would you like to",
        "(y/n)",
        "(Y/n)",
        "(y/N)",
        "approve",
        "Approve",
        "deny",
        "Deny",
    ];
    PROMPTS.iter().any(|p| plain.contains(p))
}

fn has_activity_marker(plain: &str) -> bool {
    const MARKERS: &[&str] = &[
        "Thinking",
        "Working",
        "Running",
        "Executing",
        "Generating",
        "Applying",
        "Searching",
        "Reading",
        "Writing",
    ];
    MARKERS
        .iter()
        .any(|v| plain.contains(&format!("{v}…")) || plain.contains(&format!("{v}...")))
}

fn has_spinner_title(ansi: &[u8]) -> bool {
    // Reuse the same braille-spinner-in-OSC-title trick as the Claude
    // detector. Codex CLI also sets terminal titles while working.
    let s = String::from_utf8_lossy(ansi);
    let mut in_title = false;
    let mut title = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.next() == Some(']') {
                for _ in 0..2 {
                    chars.next();
                }
                in_title = true;
                title.clear();
            }
        } else if in_title {
            if c == '\x07' || c == '\x1b' {
                if title
                    .chars()
                    .any(|c| ('\u{2800}'..='\u{28ff}').contains(&c))
                {
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

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use super::*;
    use crate::tmux::detector::DetectContext;

    fn ctx_plain(s: &str) -> DetectContext<'_> {
        let now = SystemTime::now();
        DetectContext::from_parts(s.as_bytes(), s, Some(now), now, None, "test")
    }

    #[test]
    fn non_codex_returns_unknown() {
        let ctx = ctx_plain("$ ls -la\ntotal 42\n");
        assert_eq!(CodexDetector.detect(&ctx), Status::Unknown);
    }

    #[test]
    fn codex_prompt_yields_waiting() {
        let ctx = ctx_plain("OpenAI Codex\n\nDo you want to proceed? (y/n)\n");
        assert_eq!(CodexDetector.detect(&ctx), Status::Waiting);
    }

    #[test]
    fn codex_thinking_yields_running() {
        let ctx = ctx_plain("codex session\n· Thinking…\n");
        assert_eq!(CodexDetector.detect(&ctx), Status::Running);
    }

    #[test]
    fn codex_idle_when_settled() {
        let ctx = {
            let now = SystemTime::now();
            let ago = now - std::time::Duration::from_secs(60);
            DetectContext::from_parts(
                b"codex session done",
                "codex session done",
                Some(ago),
                now,
                None,
                "test",
            )
        };
        assert_eq!(CodexDetector.detect(&ctx), Status::Idle);
    }
}
