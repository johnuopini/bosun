//! Parser for `tmux -C` control-mode output.
//!
//! Control mode runs a long-lived `tmux -C attach-session` subprocess
//! with stdin + stdout piped. Tmux writes lines of two kinds:
//!
//! 1. **Notifications** — asynchronous events starting with `%`:
//!    - `%sessions-changed`
//!    - `%session-changed $<id> <name>`
//!    - `%session-renamed $<id> <name>`
//!    - `%session-window-changed $<id> @<id>`
//!    - `%session-closed $<id>`
//!    - `%window-add @<id>`
//!    - `%window-close @<id>`
//!    - `%window-renamed @<id> <name>`
//!    - `%layout-change @<id> <layout>`
//!    - `%pane-mode-changed %<id>`
//!    - `%output %<pane-id> <octal-escaped-bytes>`
//!    - `%exit` — tmux is about to close the connection
//!
//! 2. **Command responses** — when the client writes a command to
//!    stdin, tmux wraps its response in `%begin <ts> <id> <flags>`,
//!    the response lines themselves, then `%end <ts> <id> <flags>`
//!    (or `%error <ts> <id> <flags>` on failure).
//!
//! Notifications can interleave with command response lines in
//! theory, but in practice tmux buffers notifications until a
//! response block closes. The parser supports both; lines inside a
//! `%begin..%end` block are emitted as [`Notification::CommandOutput`]
//! so the wrapper around the parser can stitch them back to the
//! originating command via the id field.
//!
//! Control-char encoding in `%output` data: tmux escapes bytes
//! `< 0x20` and `0x5c` (backslash) as `\nnn` (three octal digits).
//! So an ESC byte `0x1b` arrives as the four characters `\033`. The
//! [`unescape_output`] helper converts these back to raw bytes so
//! the data can be fed to `ansi-to-tui` unchanged.

use std::str::FromStr;

/// A single parsed event from tmux control mode. The parser is
/// pure (no I/O, no state beyond "am I inside a %begin block") and
/// emits one of these per input line. Unknown `%foo` lines become
/// [`Notification::Unknown`] so the caller can log them rather than
/// crashing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notification {
    /// Something about the session list changed. Coalesce-and-refresh
    /// is the expected handling.
    SessionsChanged,
    /// A single session's focus / active state changed.
    SessionChanged { id: String, name: String },
    /// A session was renamed (display name via `rename-session`, not
    /// our `@bosun_display` option — those still require polling).
    SessionRenamed { id: String, name: String },
    /// A session was closed / killed.
    SessionClosed { id: String },
    /// A session's active window changed.
    SessionWindowChanged { session: String, window: String },
    /// A new window was created.
    WindowAdd { id: String },
    /// A window was closed.
    WindowClose { id: String },
    /// A window was renamed.
    WindowRenamed { id: String, name: String },
    /// Pane output. `data` is the raw bytes with octal control-char
    /// escapes already undone — ready to feed to `ansi-to-tui`.
    Output { pane: String, data: Vec<u8> },
    /// Tmux is shutting the control connection down.
    Exit,
    /// `%begin` — start of a command response block.
    CommandBegin { id: u32 },
    /// `%end` — successful end of a command response block.
    CommandEnd { id: u32 },
    /// `%error` — failed end of a command response block.
    CommandError { id: u32 },
    /// A line that arrived between `%begin` and its matching `%end`
    /// or `%error`. Carries the in-flight command id so the caller
    /// can route it.
    CommandOutput { id: u32, line: String },
    /// Any notification line whose shape we don't recognize. Not an
    /// error — tmux may add new notification types and we shouldn't
    /// crash on encountering one.
    Unknown(String),
}

/// Line-based control-mode parser. Maintains just enough state to
/// know whether the current line is inside a `%begin..%end` block.
#[derive(Debug, Default)]
pub struct ControlParser {
    /// `Some(id)` while we're between a `%begin id` and the matching
    /// `%end id` / `%error id`. Lines in this state are emitted as
    /// [`Notification::CommandOutput`] rather than treated as raw
    /// notifications.
    current_command: Option<u32>,
}

impl ControlParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one line (no trailing newline) and return the resulting
    /// notification, if any. Blank lines inside a command block are
    /// preserved as empty `CommandOutput`; blank lines outside a
    /// block are ignored.
    pub fn feed(&mut self, line: &str) -> Option<Notification> {
        // Inside a %begin block: everything except the matching
        // %end/%error is part of the command's output. Nested
        // notifications are in principle possible but we treat
        // the whole interior as opaque text for the command.
        if let Some(id) = self.current_command {
            if let Some(rest) = line.strip_prefix("%end ") {
                if let Some(end_id) = parse_cmd_id(rest) {
                    if end_id == id {
                        self.current_command = None;
                        return Some(Notification::CommandEnd { id });
                    }
                }
            }
            if let Some(rest) = line.strip_prefix("%error ") {
                if let Some(err_id) = parse_cmd_id(rest) {
                    if err_id == id {
                        self.current_command = None;
                        return Some(Notification::CommandError { id });
                    }
                }
            }
            return Some(Notification::CommandOutput {
                id,
                line: line.to_string(),
            });
        }

        // Outside a command block: blank lines are silently ignored.
        if line.is_empty() {
            return None;
        }

        // Notifications always start with `%`. Anything else is
        // stray output — treat it as Unknown so it can be logged.
        if !line.starts_with('%') {
            return Some(Notification::Unknown(line.to_string()));
        }

        // `%begin timestamp id flags`
        if let Some(rest) = line.strip_prefix("%begin ") {
            if let Some(id) = parse_cmd_id(rest) {
                self.current_command = Some(id);
                return Some(Notification::CommandBegin { id });
            }
            return Some(Notification::Unknown(line.to_string()));
        }

        // Standalone shape matches
        match line {
            "%sessions-changed" => return Some(Notification::SessionsChanged),
            "%exit" => return Some(Notification::Exit),
            _ => {}
        }

        if let Some(rest) = line.strip_prefix("%session-changed ") {
            if let Some((id, name)) = split_once_space(rest) {
                return Some(Notification::SessionChanged {
                    id: id.to_string(),
                    name: name.to_string(),
                });
            }
        }
        if let Some(rest) = line.strip_prefix("%session-renamed ") {
            if let Some((id, name)) = split_once_space(rest) {
                return Some(Notification::SessionRenamed {
                    id: id.to_string(),
                    name: name.to_string(),
                });
            }
        }
        if let Some(rest) = line.strip_prefix("%session-closed ") {
            return Some(Notification::SessionClosed {
                id: rest.trim().to_string(),
            });
        }
        if let Some(rest) = line.strip_prefix("%session-window-changed ") {
            if let Some((session, window)) = split_once_space(rest) {
                return Some(Notification::SessionWindowChanged {
                    session: session.to_string(),
                    window: window.to_string(),
                });
            }
        }
        if let Some(rest) = line.strip_prefix("%window-add ") {
            return Some(Notification::WindowAdd {
                id: rest.trim().to_string(),
            });
        }
        if let Some(rest) = line.strip_prefix("%window-close ") {
            return Some(Notification::WindowClose {
                id: rest.trim().to_string(),
            });
        }
        if let Some(rest) = line.strip_prefix("%window-renamed ") {
            if let Some((id, name)) = split_once_space(rest) {
                return Some(Notification::WindowRenamed {
                    id: id.to_string(),
                    name: name.to_string(),
                });
            }
        }
        if let Some(rest) = line.strip_prefix("%output ") {
            if let Some((pane, data)) = split_once_space(rest) {
                return Some(Notification::Output {
                    pane: pane.to_string(),
                    data: unescape_output(data),
                });
            }
        }

        // Fall through: unknown notification type.
        Some(Notification::Unknown(line.to_string()))
    }
}

/// Parse the `id` field out of a `%begin`/`%end`/`%error` line.
/// Tmux writes these as `%begin <timestamp> <id> <flags>` — we only
/// care about the id for matching responses.
fn parse_cmd_id(rest: &str) -> Option<u32> {
    let mut parts = rest.split_whitespace();
    let _timestamp = parts.next()?;
    let id_str = parts.next()?;
    u32::from_str(id_str).ok()
}

fn split_once_space(s: &str) -> Option<(&str, &str)> {
    s.split_once(' ')
}

/// Undo tmux's `\nnn` octal control-char escaping in `%output` data.
/// Tmux encodes bytes `< 0x20` and `0x5c` (backslash) as a
/// backslash followed by three octal digits. Everything else passes
/// through as-is. Invalid escapes (short or non-octal) are left
/// literal rather than erroring — the parser's contract is "best
/// effort, never crash on weird input".
pub fn unescape_output(s: &str) -> Vec<u8> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            let b1 = bytes[i + 1];
            let b2 = bytes[i + 2];
            let b3 = bytes[i + 3];
            if b1.is_ascii_digit() && b2.is_ascii_digit() && b3.is_ascii_digit() {
                let v = ((b1 - b'0') as u32) * 64 + ((b2 - b'0') as u32) * 8 + ((b3 - b'0') as u32);
                if v <= 0xff {
                    out.push(v as u8);
                    i += 4;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_all(input: &str) -> Vec<Notification> {
        let mut p = ControlParser::new();
        input.lines().filter_map(|l| p.feed(l)).collect()
    }

    #[test]
    fn parses_sessions_changed() {
        assert_eq!(
            parse_all("%sessions-changed"),
            vec![Notification::SessionsChanged]
        );
    }

    #[test]
    fn parses_session_changed_with_id_and_name() {
        assert_eq!(
            parse_all("%session-changed $3 work"),
            vec![Notification::SessionChanged {
                id: "$3".into(),
                name: "work".into(),
            }]
        );
    }

    #[test]
    fn parses_session_renamed() {
        assert_eq!(
            parse_all("%session-renamed $0 new-name"),
            vec![Notification::SessionRenamed {
                id: "$0".into(),
                name: "new-name".into(),
            }]
        );
    }

    #[test]
    fn parses_session_closed() {
        assert_eq!(
            parse_all("%session-closed $4"),
            vec![Notification::SessionClosed { id: "$4".into() }]
        );
    }

    #[test]
    fn parses_session_window_changed() {
        assert_eq!(
            parse_all("%session-window-changed $0 @1"),
            vec![Notification::SessionWindowChanged {
                session: "$0".into(),
                window: "@1".into(),
            }]
        );
    }

    #[test]
    fn parses_window_add_and_close() {
        let got = parse_all("%window-add @7\n%window-close @7");
        assert_eq!(
            got,
            vec![
                Notification::WindowAdd { id: "@7".into() },
                Notification::WindowClose { id: "@7".into() },
            ]
        );
    }

    #[test]
    fn parses_exit() {
        assert_eq!(parse_all("%exit"), vec![Notification::Exit]);
    }

    #[test]
    fn unknown_notification_preserved_verbatim() {
        let got = parse_all("%something-brand-new $1 @2");
        assert_eq!(
            got,
            vec![Notification::Unknown("%something-brand-new $1 @2".into())]
        );
    }

    #[test]
    fn begin_end_command_block_emits_begin_output_end() {
        let input = "%begin 1775949678 397 0\nresponse line\n%end 1775949678 397 0";
        let got = parse_all(input);
        assert_eq!(
            got,
            vec![
                Notification::CommandBegin { id: 397 },
                Notification::CommandOutput {
                    id: 397,
                    line: "response line".into(),
                },
                Notification::CommandEnd { id: 397 },
            ]
        );
    }

    #[test]
    fn empty_command_block_still_brackets_cleanly() {
        let input = "%begin 1 100 0\n%end 1 100 0";
        let got = parse_all(input);
        assert_eq!(
            got,
            vec![
                Notification::CommandBegin { id: 100 },
                Notification::CommandEnd { id: 100 },
            ]
        );
    }

    #[test]
    fn error_block_bracketed_as_command_error() {
        let input = "%begin 1 200 0\nerr: whatever\n%error 1 200 0";
        let got = parse_all(input);
        assert_eq!(
            got,
            vec![
                Notification::CommandBegin { id: 200 },
                Notification::CommandOutput {
                    id: 200,
                    line: "err: whatever".into(),
                },
                Notification::CommandError { id: 200 },
            ]
        );
    }

    #[test]
    fn notification_between_commands_stays_outside_block() {
        // Real tmux transcript: finish a command block, then emit a
        // bare notification before the next %begin.
        let input = "\
%begin 1 1 0
%end 1 1 0
%sessions-changed
%begin 2 2 0
%end 2 2 0";
        let got = parse_all(input);
        assert_eq!(
            got,
            vec![
                Notification::CommandBegin { id: 1 },
                Notification::CommandEnd { id: 1 },
                Notification::SessionsChanged,
                Notification::CommandBegin { id: 2 },
                Notification::CommandEnd { id: 2 },
            ]
        );
    }

    #[test]
    fn output_unescapes_octal_control_chars() {
        // The pane emits ESC[1m (bold on) followed by 'A'. Tmux
        // wraps the ESC as \033 in the wire format.
        let input = "%output %0 \\033[1mA";
        let got = parse_all(input);
        let expected_bytes = vec![0x1b, b'[', b'1', b'm', b'A'];
        assert_eq!(
            got,
            vec![Notification::Output {
                pane: "%0".into(),
                data: expected_bytes,
            }]
        );
    }

    #[test]
    fn output_preserves_literal_text() {
        let input = "%output %3 hello world";
        let got = parse_all(input);
        assert_eq!(
            got,
            vec![Notification::Output {
                pane: "%3".into(),
                data: b"hello world".to_vec(),
            }]
        );
    }

    #[test]
    fn output_handles_mix_of_literals_and_escapes() {
        let input = "%output %2 foo\\033[31mbar";
        let got = parse_all(input);
        let mut expected = b"foo".to_vec();
        expected.extend([0x1b, b'[', b'3', b'1', b'm']);
        expected.extend(b"bar");
        assert_eq!(
            got,
            vec![Notification::Output {
                pane: "%2".into(),
                data: expected,
            }]
        );
    }

    #[test]
    fn unescape_output_leaves_invalid_escapes_literal() {
        // `\9` isn't a valid octal digit — the scan leaves the
        // backslash in place rather than corrupting the stream.
        assert_eq!(unescape_output("a\\9b"), b"a\\9b".to_vec());
    }

    #[test]
    fn unescape_output_handles_trailing_backslash() {
        // Shouldn't panic on a backslash at end-of-string.
        assert_eq!(unescape_output("abc\\"), b"abc\\".to_vec());
    }

    #[test]
    fn blank_lines_outside_commands_are_ignored() {
        let input = "%sessions-changed\n\n%exit";
        let got = parse_all(input);
        assert_eq!(got, vec![Notification::SessionsChanged, Notification::Exit]);
    }

    #[test]
    fn blank_lines_inside_command_block_preserved() {
        let input = "%begin 1 42 0\n\n%end 1 42 0";
        let got = parse_all(input);
        assert_eq!(
            got,
            vec![
                Notification::CommandBegin { id: 42 },
                Notification::CommandOutput {
                    id: 42,
                    line: "".into(),
                },
                Notification::CommandEnd { id: 42 },
            ]
        );
    }

    #[test]
    fn real_tmux_transcript_parses_end_to_end() {
        // Lifted verbatim from an actual `tmux -C attach-session`
        // run against a freshly-created session.
        let transcript = "\
%begin 1775949678 397 0
%end 1775949678 397 0
%window-add @0
%sessions-changed
%session-changed $0 probe
%begin 1775949678 403 1
%end 1775949678 403 1
%begin 1775949678 404 1
probe: 2 windows (created Sun Apr 12 00:21:18 2026) (attached)
%end 1775949678 404 1
%session-window-changed $0 @1
%window-add @1
%output %0 hi
%exit";
        let got = parse_all(transcript);

        // Spot-check structural markers rather than transcribing the
        // whole vec: it exercises command blocks, standalone
        // notifications, and %output in one run.
        assert!(matches!(got[0], Notification::CommandBegin { id: 397 }));
        assert!(matches!(got[1], Notification::CommandEnd { id: 397 }));
        assert_eq!(got[2], Notification::WindowAdd { id: "@0".into() });
        assert_eq!(got[3], Notification::SessionsChanged);
        assert_eq!(
            got[4],
            Notification::SessionChanged {
                id: "$0".into(),
                name: "probe".into(),
            }
        );
        // Find the %output notification and the %exit at the end.
        let has_output = got.iter().any(|n| {
            matches!(
                n,
                Notification::Output { pane, data }
                if pane == "%0" && data == b"hi"
            )
        });
        assert!(has_output, "expected Output notification in transcript");
        assert_eq!(got.last(), Some(&Notification::Exit));
    }
}
