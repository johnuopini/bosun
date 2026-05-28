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

    if visible.is_empty() {
        let lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                "  no tmux sessions",
                Style::default().fg(theme.text_muted),
            )),
            Line::from(Span::styled(
                "  (press n to create · g for a section)",
                Style::default().fg(theme.text_muted),
            )),
        ];
        frame.render_widget(Paragraph::new(lines).block(block), area);
        return;
    }

    // Track the section-index (0-based) as we walk the visible list so
    // section headers can show their numeric jump key.
    let mut section_idx: usize = 0;
    let mut out: Vec<Line<'_>> = Vec::with_capacity(visible.len() * 2);
    // For each entry, record the first/last line index it produced.
    // Used after the build to compute a scroll offset that keeps the
    // selected entry fully visible — without this, on small screens
    // (mobile / mosh / phone) the list never scrolls and entries past
    // the viewport bottom are unreachable.
    let mut entry_first_line: Vec<usize> = Vec::with_capacity(visible.len());
    let mut entry_last_line: Vec<usize> = Vec::with_capacity(visible.len());
    for (i, entry) in visible.iter().enumerate() {
        let selected = i == state.selected;
        let start = out.len();
        match entry {
            VisibleEntry::Ungrouped(c) => match state.session_by_name(&c.active) {
                Some(v) => {
                    out.push(render_primary_line(v, selected, false, area.width, theme));
                    out.push(render_meta_line(v, selected, false, area.width, theme));
                }
                None => {
                    let label = state.dead_display_for(&c.active);
                    out.push(render_missing_line(
                        &label, selected, false, area.width, theme,
                    ));
                }
            },
            VisibleEntry::SectionHeader(s) => {
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
            VisibleEntry::Member { container, .. } => {
                match state.session_by_name(&container.active) {
                    Some(v) => {
                        out.push(render_primary_line(v, selected, true, area.width, theme));
                        out.push(render_meta_line(v, selected, true, area.width, theme));
                    }
                    None => {
                        let label = state.dead_display_for(&container.active);
                        out.push(render_missing_line(
                            &label, selected, true, area.width, theme,
                        ));
                    }
                }
            }
        }
        entry_first_line.push(start);
        entry_last_line.push(out.len().saturating_sub(1));
    }

    let total_lines = out.len();
    let viewport = area.height as usize;
    // Compute a scroll offset that keeps the selected entry fully
    // visible. No persistent scroll state — derive every frame from
    // the current selection so j/k, jump keys, and section toggles
    // all stay consistent without sync logic. Without this, on small
    // screens (mobile / mosh) entries past the viewport bottom were
    // unreachable.
    let scroll: u16 = if total_lines <= viewport || viewport == 0 {
        0
    } else {
        let sel = state.selected.min(entry_first_line.len().saturating_sub(1));
        let first = entry_first_line[sel];
        let last = entry_last_line[sel];
        let max_scroll = total_lines.saturating_sub(viewport);
        // Center-ish: aim for the selection at ~1/3 from the top so
        // there's context above and below as the user navigates.
        let target = viewport / 3;
        let mut s = first.saturating_sub(target);
        // Always keep the entry's last line on-screen — matters for
        // 2-line session entries near the bottom of the list.
        if last >= s + viewport {
            s = (last + 1).saturating_sub(viewport);
        }
        s.min(max_scroll) as u16
    };

    let p = Paragraph::new(out).block(block).scroll((scroll, 0));
    frame.render_widget(p, area);
}

/// Lines a single visible entry occupies in the rendered list.
/// Sessions render as two lines (primary + meta); section headers
/// and dead-row sessions are single-line. Kept as a small shared
/// helper so [`entry_at_row`] stays in lockstep with [`render`].
fn entry_line_count(state: &AppState, entry: &VisibleEntry<'_>) -> u16 {
    match entry {
        VisibleEntry::Ungrouped(c) => {
            if state.session_by_name(&c.active).is_some() {
                2
            } else {
                1
            }
        }
        VisibleEntry::SectionHeader(_) => 1,
        VisibleEntry::Member { container, .. } => {
            if state.session_by_name(&container.active).is_some() {
                2
            } else {
                1
            }
        }
    }
}

/// Same scroll heuristic as [`render`] but computed from line
/// counts only — no rendered `Line`s needed. Centers the selected
/// entry at roughly the top third of the viewport while keeping
/// its last line on-screen.
fn compute_scroll(counts: &[u16], selected: usize, viewport: u16) -> u16 {
    let total_lines: u16 = counts.iter().copied().fold(0u16, u16::saturating_add);
    if total_lines <= viewport || viewport == 0 {
        return 0;
    }
    let sel = selected.min(counts.len().saturating_sub(1));
    let first: u16 = counts
        .iter()
        .take(sel)
        .copied()
        .fold(0u16, u16::saturating_add);
    let last = first.saturating_add(counts.get(sel).copied().unwrap_or(0).saturating_sub(1));
    let max_scroll = total_lines.saturating_sub(viewport);
    let target = viewport / 3;
    let mut s = first.saturating_sub(target);
    if last >= s.saturating_add(viewport) {
        s = last.saturating_add(1).saturating_sub(viewport);
    }
    s.min(max_scroll)
}

/// Map an absolute terminal row (mouse Y coord) to a visible-entry
/// index in the sidebar. Returns `None` when the row is outside the
/// list rect or past the last entry's last line. Used by the click
/// handler so a mouse-down on a session row jumps the selection
/// straight to it (same effect as pressing j/k until the cursor
/// lands).
pub fn entry_at_row(state: &AppState, list_area: Rect, abs_row: u16) -> Option<usize> {
    if abs_row < list_area.y || abs_row >= list_area.y.saturating_add(list_area.height) {
        return None;
    }
    let visible = state.sidebar.visible();
    if visible.is_empty() {
        return None;
    }
    let counts: Vec<u16> = visible.iter().map(|e| entry_line_count(state, e)).collect();
    let scroll = compute_scroll(&counts, state.selected, list_area.height);
    let local_row = abs_row - list_area.y;
    let target_line = local_row.saturating_add(scroll);
    let mut acc: u16 = 0;
    for (i, c) in counts.iter().enumerate() {
        if target_line >= acc && target_line < acc + c {
            return Some(i);
        }
        acc = acc.saturating_add(*c);
    }
    None
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

    // Disclosure glyph follows the standard tree convention: ▸ when
    // the section is closed (members hidden), ▾ when open. An empty
    // section always shows ▸ since there's nothing to collapse.
    let glyph = if section.collapsed || section.members.is_empty() {
        "▸"
    } else {
        "▾"
    };
    let label = format!("{glyph} {}", section.name.to_uppercase());
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
    label: &str,
    selected: bool,
    indented: bool,
    width: u16,
    theme: &Theme,
) -> Line<'static> {
    let bg = row_bg(selected, theme);
    let extra = if indented { "  " } else { "" };
    // Label is the resolved display name (from Recents lookup) or
    // a slug fallback — see `AppState::dead_display_for`. `R` on this
    // row recreates the session from its persisted spec.
    let body = format!("  ? {}", label);
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
