//! Pure parsers for tmux CLI output. No I/O, no allocations we can avoid.
//! Every parser gets unit-tested against fixtures in `#[cfg(test)]` so the
//! shell-out layer (`client.rs`) can stay thin.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::{BosunError, Result};
use crate::tmux::session::TmuxSession;

/// The format string we pass to `tmux list-sessions -F`. Fields are separated
/// by `\x1f` (ASCII unit separator) so session names can safely contain `|`,
/// `:`, tabs, etc. without colliding with the delimiter.
///
/// The trailing `@bosun_*` fields read user options we set at create time:
/// - `@bosun_display` — pretty UI name (e.g. "rasterfox" for internal `bosun-rasterfox-a1b2c3d4`)
/// - `@bosun_agent`   — agent kind (claude / codex / terminal)
/// - `@bosun_path`    — spec path the user typed into the new-session modal
///
/// All three are empty strings for non-bosun sessions and get parsed as
/// `None` so the UI renders them only when available.
pub const LIST_SESSIONS_FORMAT: &str = "#{session_name}\x1f#{session_windows}\x1f#{session_attached}\x1f#{session_created}\x1f#{session_activity}\x1f#{session_path}\x1f#{@bosun_display}\x1f#{@bosun_agent}\x1f#{@bosun_path}";

const FIELD_SEP: char = '\x1f';

/// Parse the full `tmux list-sessions -F <LIST_SESSIONS_FORMAT>` output.
/// One session per line; empty input is valid (no sessions).
pub fn parse_list_sessions(input: &str) -> Result<Vec<TmuxSession>> {
    let mut out = Vec::new();
    for (idx, line) in input.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        out.push(
            parse_session_line(line)
                .map_err(|e| BosunError::Parse(format!("line {}: {}", idx + 1, e)))?,
        );
    }
    Ok(out)
}

fn parse_session_line(line: &str) -> std::result::Result<TmuxSession, String> {
    let mut parts = line.split(FIELD_SEP);

    let name = parts
        .next()
        .ok_or_else(|| "missing session name".to_string())?
        .to_string();
    let windows_raw = parts
        .next()
        .ok_or_else(|| "missing session_windows".to_string())?;
    let attached_raw = parts
        .next()
        .ok_or_else(|| "missing session_attached".to_string())?;
    let created_raw = parts
        .next()
        .ok_or_else(|| "missing session_created".to_string())?;
    let activity_raw = parts
        .next()
        .ok_or_else(|| "missing session_activity".to_string())?;
    let path = parts.next().map(|s| s.to_string());
    let display_raw = parts.next().map(|s| s.to_string());
    // `@bosun_agent` and `@bosun_path` are optional trailing fields
    // — older tmux list-sessions outputs (from before we added them
    // to LIST_SESSIONS_FORMAT) may not include them, and fixtures
    // in tests sometimes omit them for brevity.
    let agent_raw = parts.next().map(|s| s.to_string());
    let spec_path_raw = parts.next().map(|s| s.to_string());

    if parts.next().is_some() {
        return Err("unexpected extra field".into());
    }

    let windows: u32 = windows_raw
        .parse()
        .map_err(|e| format!("session_windows '{}': {}", windows_raw, e))?;

    let attached = match attached_raw {
        "0" => false,
        "1" => true,
        // tmux gives the attached-client count; >0 means attached.
        other => other.parse::<u32>().map(|n| n > 0).unwrap_or(false),
    };

    let created = parse_epoch(created_raw);
    let last_activity = parse_epoch(activity_raw);

    Ok(TmuxSession {
        name,
        windows,
        attached,
        created,
        last_activity,
        current_path: path.filter(|p| !p.is_empty()),
        display_name: display_raw.filter(|s| !s.is_empty()),
        agent: agent_raw.filter(|s| !s.is_empty()),
        spec_path: spec_path_raw.filter(|s| !s.is_empty()),
    })
}

fn parse_epoch(s: &str) -> Option<SystemTime> {
    if s.is_empty() {
        return None;
    }
    let secs: u64 = s.parse().ok()?;
    Some(UNIX_EPOCH + Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_empty_input() {
        assert!(parse_list_sessions("").unwrap().is_empty());
    }

    #[test]
    fn parses_single_session() {
        let line = "main\x1f3\x1f1\x1f1712000000\x1f1712003600\x1f/home/rhuk/code";
        let sessions = parse_list_sessions(line).unwrap();
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s.name, "main");
        assert_eq!(s.windows, 3);
        assert!(s.attached);
        assert_eq!(s.current_path.as_deref(), Some("/home/rhuk/code"));
        assert!(s.created.is_some());
        assert!(s.last_activity.is_some());
    }

    #[test]
    fn parses_multiple_sessions() {
        let input = concat!(
            "alpha\x1f1\x1f0\x1f1700000000\x1f1700000100\x1f/tmp\n",
            "beta\x1f2\x1f1\x1f1700001000\x1f1700002000\x1f/home/rhuk\n",
            "gamma\x1f5\x1f0\x1f1700003000\x1f1700004000\x1f\n",
        );
        let sessions = parse_list_sessions(input).unwrap();
        assert_eq!(sessions.len(), 3);
        assert_eq!(sessions[0].name, "alpha");
        assert!(!sessions[0].attached);
        assert_eq!(sessions[1].name, "beta");
        assert!(sessions[1].attached);
        assert_eq!(sessions[2].name, "gamma");
        assert!(sessions[2].current_path.is_none());
    }

    #[test]
    fn names_with_special_chars_survive_unit_separator() {
        let line = "work: proj | v2\x1f1\x1f1\x1f1700000000\x1f1700000100\x1f/srv";
        let sessions = parse_list_sessions(line).unwrap();
        assert_eq!(sessions[0].name, "work: proj | v2");
    }

    #[test]
    fn unicode_name_preserved() {
        let line = "日本語セッション\x1f1\x1f0\x1f1700000000\x1f1700000100\x1f/tmp";
        let sessions = parse_list_sessions(line).unwrap();
        assert_eq!(sessions[0].name, "日本語セッション");
    }

    #[test]
    fn attached_client_count_treated_as_bool() {
        let line = "multi\x1f1\x1f3\x1f1700000000\x1f1700000100\x1f/tmp";
        let sessions = parse_list_sessions(line).unwrap();
        assert!(sessions[0].attached);
    }

    #[test]
    fn empty_lines_skipped() {
        let input = "\nalpha\x1f1\x1f0\x1f1700000000\x1f1700000100\x1f/tmp\n\n";
        let sessions = parse_list_sessions(input).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].name, "alpha");
    }

    #[test]
    fn malformed_line_errors_with_line_number() {
        let input = "alpha\x1f1\x1f0\nbroken_but_no_seps";
        let err = parse_list_sessions(input).unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("line 1") || msg.contains("line 2"),
            "msg was: {}",
            msg
        );
    }

    #[test]
    fn missing_activity_ok_but_none() {
        let line = "alpha\x1f1\x1f0\x1f1700000000\x1f\x1f/tmp";
        let sessions = parse_list_sessions(line).unwrap();
        assert!(sessions[0].created.is_some());
        assert!(sessions[0].last_activity.is_none());
    }

    #[test]
    fn bosun_user_options_parse_when_present() {
        let line = "bosun-foo\x1f1\x1f0\x1f1700000000\x1f1700000100\x1f/srv\x1ffoo\x1fclaude\x1f~/proj";
        let sessions = parse_list_sessions(line).unwrap();
        assert_eq!(sessions[0].display_name.as_deref(), Some("foo"));
        assert_eq!(sessions[0].agent.as_deref(), Some("claude"));
        assert_eq!(sessions[0].spec_path.as_deref(), Some("~/proj"));
    }

    #[test]
    fn missing_bosun_options_are_none_not_error() {
        // Non-bosun session: `@bosun_*` all return empty strings,
        // which we parse as None. Also covers the back-compat case
        // where `@bosun_agent`/`@bosun_path` were added later.
        let line = "plain\x1f1\x1f0\x1f1700000000\x1f1700000100\x1f/srv\x1f\x1f\x1f";
        let sessions = parse_list_sessions(line).unwrap();
        assert!(sessions[0].display_name.is_none());
        assert!(sessions[0].agent.is_none());
        assert!(sessions[0].spec_path.is_none());
    }
}
