//! Session list panel. Phase 2: status glyphs + color-coded per state.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;

use crate::app::AppState;
use crate::tmux::detector::Status;
use crate::tmux::session::SessionView;
use crate::ui::Theme;

pub fn render(frame: &mut Frame<'_>, area: Rect, state: &AppState, theme: &Theme) {
    let block = Block::default().style(Style::default().bg(theme.bg));

    let lines: Vec<Line<'_>> = if state.sessions.is_empty() {
        vec![
            Line::from(""),
            Line::from(Span::styled(
                "  no tmux sessions",
                Style::default().fg(theme.text_muted),
            )),
            Line::from(Span::styled(
                "  (press r to refresh)",
                Style::default().fg(theme.text_muted),
            )),
        ]
    } else {
        // Each session produces two lines: the primary row (marker +
        // status glyph + display name + window count + attached flag)
        // and a secondary meta row ("agent · path", muted). Both lines
        // share the same row background so the selection highlight is
        // one contiguous block.
        let mut out: Vec<Line<'_>> = Vec::with_capacity(state.sessions.len() * 2);
        for (i, v) in state.sessions.iter().enumerate() {
            let selected = i == state.selected;
            out.push(render_primary_line(v, selected, area.width, theme));
            out.push(render_meta_line(v, selected, area.width, theme));
        }
        out
    };

    let p = Paragraph::new(lines).block(block);
    frame.render_widget(p, area);
}

fn status_color(status: Status, theme: &Theme) -> Color {
    match status {
        Status::Running => theme.status_running,
        Status::Waiting => theme.status_waiting,
        Status::Idle | Status::Unknown => theme.status_idle,
        Status::Error => theme.status_error,
    }
}

fn row_bg(selected: bool, theme: &Theme) -> Color {
    if selected {
        theme.selection_bg
    } else {
        theme.panel
    }
}

fn render_primary_line(
    view: &SessionView,
    selected: bool,
    width: u16,
    theme: &Theme,
) -> Line<'static> {
    let marker = if selected { "▌" } else { " " };
    let bg = row_bg(selected, theme);

    let marker_style = if selected {
        Style::default().fg(theme.accent).bg(bg)
    } else {
        Style::default().fg(bg).bg(bg)
    };
    let name_style = if selected {
        Style::default()
            .fg(theme.text)
            .bg(bg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.text).bg(bg)
    };
    let status_style = Style::default()
        .fg(status_color(view.status, theme))
        .bg(bg);

    let glyph = view.status.glyph().to_string();
    let name = view.display().to_string();
    let windows_label = format!("  {}w", view.session.windows);
    let attached_label = if view.session.attached {
        "  •attached"
    } else {
        ""
    };

    // Width consumed: " ▌ " (3) + glyph (1) + "  " (2) + name + "  Nw"
    // [+ "  •attached"]. Trailing pad extends row_bg to the right edge.
    let used = 3
        + glyph.chars().count()
        + 2
        + name.chars().count()
        + windows_label.chars().count()
        + attached_label.chars().count();
    let pad = (width as usize).saturating_sub(used);

    let mut spans = vec![
        Span::styled(format!(" {} ", marker), marker_style),
        Span::styled(glyph, status_style),
        Span::styled("  ", Style::default().bg(bg)),
        Span::styled(name, name_style),
        Span::styled(windows_label, Style::default().fg(theme.text_muted).bg(bg)),
    ];
    if view.session.attached {
        spans.push(Span::styled(
            attached_label,
            Style::default().fg(theme.status_running).bg(bg),
        ));
    }
    spans.push(Span::styled(" ".repeat(pad), Style::default().bg(bg)));

    Line::from(spans)
}

fn render_meta_line(
    view: &SessionView,
    selected: bool,
    width: u16,
    theme: &Theme,
) -> Line<'static> {
    let bg = row_bg(selected, theme);
    let meta_style = Style::default().fg(theme.text_muted).bg(bg);

    // Indent to align under the session name — matches the primary
    // line's " ▌  ○  " prefix (3 + 1 + 2 = 6 cols).
    const INDENT: &str = "       ";

    let agent = view.session.agent.as_deref();
    let path = view.session.best_path();

    let body = match (agent, path) {
        (Some(a), Some(p)) => format!("{a} · {}", shorten_path(p)),
        (Some(a), None) => a.to_string(),
        (None, Some(p)) => shorten_path(p),
        // Non-bosun session with no cwd info — keep the row shape but
        // leave the meta line blank so the selection highlight still
        // spans both rows.
        (None, None) => String::new(),
    };

    // Truncate body if it'd overflow the panel. Reserve the INDENT
    // columns and 1 trailing column so the selection bg stays clean.
    let max_body = (width as usize)
        .saturating_sub(INDENT.chars().count())
        .saturating_sub(1);
    let body = truncate_to(&body, max_body);

    let used = INDENT.chars().count() + body.chars().count();
    let pad = (width as usize).saturating_sub(used);

    Line::from(vec![
        Span::styled(INDENT, Style::default().bg(bg)),
        Span::styled(body, meta_style),
        Span::styled(" ".repeat(pad), Style::default().bg(bg)),
    ])
}

/// Replace `$HOME` with `~` for display. No-op if HOME isn't set or
/// isn't a prefix of `p`.
fn shorten_path(p: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    if !home.is_empty() && p.starts_with(&home) {
        format!("~{}", &p[home.len()..])
    } else {
        p.to_string()
    }
}

/// Truncate to `max` chars with a leading ellipsis if clipped, so the
/// tail of the path (most informative part) stays visible.
fn truncate_to(s: &str, max: usize) -> String {
    let len = s.chars().count();
    if len <= max {
        return s.to_string();
    }
    if max <= 1 {
        return "…".to_string();
    }
    let tail: String = s.chars().skip(len - (max - 1)).collect();
    format!("…{tail}")
}
