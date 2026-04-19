//! Session list panel. Walks the explicit-membership `SidebarModel`,
//! emitting one row per visible entry (ungrouped session, section
//! header, or section member).

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;

use crate::app::AppState;
use crate::sidebar::{Section, VisibleEntry};
use crate::tmux::detector::Status;
use crate::tmux::session::SessionView;
use crate::ui::Theme;

pub fn render(frame: &mut Frame<'_>, area: Rect, state: &AppState, theme: &Theme) {
    let block = Block::default().style(Style::default().bg(theme.bg));

    let visible = state.sidebar.visible();

    let lines: Vec<Line<'_>> = if visible.is_empty() {
        vec![
            Line::from(""),
            Line::from(Span::styled(
                "  no tmux sessions",
                Style::default().fg(theme.text_muted),
            )),
            Line::from(Span::styled(
                "  (press n to create · g for a section)",
                Style::default().fg(theme.text_muted),
            )),
        ]
    } else {
        // Track the section-index (0-based) as we walk the visible
        // list so section headers can show their numeric jump key.
        let mut section_idx: usize = 0;
        let mut out: Vec<Line<'_>> = Vec::with_capacity(visible.len() * 2);
        for (i, entry) in visible.iter().enumerate() {
            let selected = i == state.selected;
            match entry {
                VisibleEntry::UngroupedSession(n) => match state.session_by_name(n) {
                    Some(v) => {
                        out.push(render_primary_line(v, selected, false, area.width, theme));
                        out.push(render_meta_line(v, selected, false, area.width, theme));
                    }
                    None => {
                        out.push(render_missing_line(n, selected, false, area.width, theme));
                    }
                },
                VisibleEntry::SectionHeader(s) => {
                    // Jump key: 1..9 for the first nine sections, blank for any
                    // extras. Matches the digit bindings in app::handle_key.
                    let jump_key = if section_idx < 9 {
                        Some((section_idx as u8 + b'1') as char)
                    } else {
                        None
                    };
                    out.push(render_section_line(
                        s, selected, jump_key, area.width, theme,
                    ));
                    section_idx += 1;
                }
                VisibleEntry::SectionMember { internal, .. } => {
                    match state.session_by_name(internal) {
                        Some(v) => {
                            out.push(render_primary_line(v, selected, true, area.width, theme));
                            out.push(render_meta_line(v, selected, true, area.width, theme));
                        }
                        None => {
                            out.push(render_missing_line(
                                internal, selected, true, area.width, theme,
                            ));
                        }
                    }
                }
            }
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

fn render_section_line(
    section: &Section,
    selected: bool,
    jump_key: Option<char>,
    width: u16,
    theme: &Theme,
) -> Line<'static> {
    let bg = row_bg(selected, theme);
    let marker = if selected { "▌" } else { " " };
    let marker_style = if selected {
        Style::default().fg(theme.accent).bg(bg)
    } else {
        Style::default().fg(bg).bg(bg)
    };
    let title_style = Style::default()
        .fg(theme.accent)
        .bg(bg)
        .add_modifier(Modifier::BOLD);
    let key_style = Style::default().fg(theme.text_muted).bg(bg);

    let key_prefix = match jump_key {
        Some(k) => format!("{} ", k),
        None => "  ".to_string(),
    };
    let count_label = format!("  ({})", section.members.len());
    let count_style = Style::default().fg(theme.text_muted).bg(bg);

    let label = format!("▸ {}", section.name.to_uppercase());
    let used = 3 + key_prefix.chars().count() + label.chars().count() + count_label.chars().count();
    let pad = (width as usize).saturating_sub(used);

    Line::from(vec![
        Span::styled(format!(" {} ", marker), marker_style),
        Span::styled(key_prefix, key_style),
        Span::styled(label, title_style),
        Span::styled(count_label, count_style),
        Span::styled(" ".repeat(pad), Style::default().bg(bg)),
    ])
}

fn render_primary_line(
    view: &SessionView,
    selected: bool,
    indented: bool,
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
    let status_style = Style::default().fg(status_color(view.status, theme)).bg(bg);

    let glyph = view.status.glyph().to_string();
    let name = view.display().to_string();
    let windows_label = format!("  {}w", view.session.windows);
    let attached_label = if view.session.attached {
        "  •attached"
    } else {
        ""
    };
    let indent = if indented { "  " } else { "" };

    let used = 3
        + indent.chars().count()
        + glyph.chars().count()
        + 2
        + name.chars().count()
        + windows_label.chars().count()
        + attached_label.chars().count();
    let pad = (width as usize).saturating_sub(used);

    let mut spans = vec![
        Span::styled(format!(" {} ", marker), marker_style),
        Span::styled(indent, Style::default().bg(bg)),
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
    indented: bool,
    width: u16,
    theme: &Theme,
) -> Line<'static> {
    let bg = row_bg(selected, theme);
    let meta_style = Style::default().fg(theme.text_muted).bg(bg);

    // Base indent aligns under the session name ("  ▌  ○  " = 3+1+2=6 cols
    // of leading padding after the marker); indented rows add 2 more.
    let base_indent: &str = "       ";
    let extra = if indented { "  " } else { "" };

    let agent = view.session.agent.as_deref();
    let path = view.session.best_path();

    let body = match (agent, path) {
        (Some(a), Some(p)) => format!("{a} · {}", shorten_path(p)),
        (Some(a), None) => a.to_string(),
        (None, Some(p)) => shorten_path(p),
        (None, None) => String::new(),
    };

    let lead = base_indent.chars().count() + extra.chars().count();
    let max_body = (width as usize).saturating_sub(lead).saturating_sub(1);
    let body = truncate_to(&body, max_body);

    let used = lead + body.chars().count();
    let pad = (width as usize).saturating_sub(used);

    Line::from(vec![
        Span::styled(base_indent, Style::default().bg(bg)),
        Span::styled(extra, Style::default().bg(bg)),
        Span::styled(body, meta_style),
        Span::styled(" ".repeat(pad), Style::default().bg(bg)),
    ])
}

fn render_missing_line(
    internal: &str,
    selected: bool,
    indented: bool,
    width: u16,
    theme: &Theme,
) -> Line<'static> {
    let bg = row_bg(selected, theme);
    let extra = if indented { "  " } else { "" };
    let body = format!("  ? {}", internal);
    let used = extra.chars().count() + body.chars().count();
    let pad = (width as usize).saturating_sub(used);
    Line::from(vec![
        Span::styled(extra, Style::default().bg(bg)),
        Span::styled(body, Style::default().fg(theme.text_muted).bg(bg)),
        Span::styled(" ".repeat(pad), Style::default().bg(bg)),
    ])
}

fn shorten_path(p: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    if !home.is_empty() && p.starts_with(&home) {
        format!("~{}", &p[home.len()..])
    } else {
        p.to_string()
    }
}

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
