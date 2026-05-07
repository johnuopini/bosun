//! Live pane preview rendered from `tmux capture-pane -e` output via
//! `ansi-to-tui`. The preview buffer for the selected session lives in
//! `AppState`; this module is pure rendering.
//!
//! Background handling: we deliberately do NOT paint a background color
//! on the preview area. TUIs like Claude Code emit cells with "default"
//! background (Color::Reset), which ratatui treats as "leave whatever
//! was there". If we painted our own panel color underneath, the reset
//! cells would show our color and clash with the cells that *did* carry
//! an explicit background (like Claude Code's status line). By leaving
//! the area's buffer at Color::Reset, all reset cells fall through to
//! the terminal's own default and stay visually consistent with what
//! the user sees when they actually attach to the session.

use ansi_to_tui::IntoText;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Widget};
use ratatui::Frame;

use crate::app::AppState;
use crate::sidebar::{Location, VisibleKind};
use crate::ui::{banner, section_preview, Theme};

pub fn render(frame: &mut Frame<'_>, area: Rect, state: &AppState, theme: &Theme) {
    // Reset every cell in the preview area to Color::Reset before drawing
    // so any leftover styling from a previous frame (e.g. the placeholder
    // text) doesn't bleed through into a later TUI capture.
    reset_area(frame.buffer_mut(), area);

    // Empty state (no managed sessions): paint the Bosun splash —
    // banner + version — instead of leaving the pane silent. Lets the
    // user cycle the global font even before they have any sessions.
    if state.sessions.is_empty() && state.sidebar.is_empty() {
        let font = banner::canonical(&state.banner_font);
        section_preview::render_empty(frame.buffer_mut(), area, font, theme);
        return;
    }

    // Section header selected: render banner + per-section table.
    // The cursor location tells us which section, regardless of how
    // many sessions live elsewhere.
    if let Some(VisibleKind::Header) = state.selected_kind() {
        if let Some(Location::Header(si)) = state.selected_location() {
            if let Some(sec) = state.sidebar.sections.get(si) {
                let font = sec
                    .banner_font
                    .as_deref()
                    .map(banner::canonical)
                    .unwrap_or_else(|| banner::canonical(&state.banner_font));
                let members: Vec<&crate::tmux::session::SessionView> = sec
                    .members
                    .iter()
                    .filter_map(|n| state.session_by_name(n))
                    .collect();
                section_preview::render_section(
                    frame.buffer_mut(),
                    area,
                    sec,
                    &members,
                    font,
                    theme,
                );
                return;
            }
        }
    }

    let text: Text<'_> = if state.sessions.is_empty() {
        // Sidebar has sections but no live sessions yet — stay quiet
        // rather than saying "capturing…" which is misleading.
        placeholder("", theme)
    } else {
        match state.selected_preview() {
            Some(bytes) if !bytes.is_empty() => bytes
                .into_text()
                .unwrap_or_else(|_| placeholder("preview: (ansi parse failed)", theme)),
            _ => placeholder("preview: capturing…", theme),
        }
    };

    // Scroll so the bottom of the captured pane aligns with the bottom
    // of the preview area. The tmux pane may be taller than our preview
    // viewport, and the user always wants to see the most-recent output.
    let text_lines = text.lines.len() as u16;
    let scroll_y = text_lines.saturating_sub(area.height);

    // No wrap, no background. Lines wider than the area are clipped at
    // the right edge. Wrapping throws off the scroll math because wrapped
    // lines count as multiple visual rows and we can't tell how many
    // without measuring the rendered output.
    Paragraph::new(text)
        .scroll((scroll_y, 0))
        .render(area, frame.buffer_mut());
}

fn reset_area(buf: &mut Buffer, area: Rect) {
    let reset_style = Style::default().fg(Color::Reset).bg(Color::Reset);
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let cell = &mut buf[(x, y)];
            cell.set_char(' ');
            cell.set_style(reset_style);
        }
    }
}

fn placeholder(msg: &str, theme: &Theme) -> Text<'static> {
    Text::from(vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("  {}", msg),
            Style::default().fg(theme.text_muted),
        )),
    ])
}
