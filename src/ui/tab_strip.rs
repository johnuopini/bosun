//! Browser-style tab strip rendered above the embedded terminal
//! whenever the selected sidebar row is a container. Single-tab
//! containers show the strip too so the `+` add-tab button is
//! always reachable without having to first create a second tab.
//!
//! The layout is computed by [`compute`] (a pure function over the
//! preview rect, the container, and per-tab `SessionView`s); the
//! result is used by both [`render`] and the mouse-click path in
//! `app.rs` so the click hit-test always matches what was drawn.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;

use crate::sidebar::Container;
use crate::tmux::detector::Status;
use crate::tmux::session::SessionView;
use crate::ui::Theme;

/// Padding spaces around each tab's `<glyph> <label>` core.
const TAB_PAD: u16 = 1;
/// On-screen text for the add-tab button. Three columns wide so
/// it's easy to click and visually separates from the last tab.
const PLUS_LABEL: &str = " + ";

/// One clickable region in the tab strip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    /// Tmux internal name of the tab, or the literal `"+"` for
    /// the add-tab button.
    pub key: String,
    pub rect: Rect,
}

#[derive(Debug, Clone, Default)]
pub struct Layout {
    pub tabs: Vec<Slot>,
    pub plus: Option<Slot>,
}

impl Layout {
    /// Resolve a click on `(col, row)` to the slot under the
    /// pointer, if any. Plus button takes precedence (it's
    /// rightmost and the layout never makes a tab overlap it).
    pub fn hit(&self, col: u16, row: u16) -> Option<&Slot> {
        if let Some(p) = &self.plus {
            if contains(p.rect, col, row) {
                return Some(p);
            }
        }
        self.tabs.iter().find(|t| contains(t.rect, col, row))
    }
}

fn contains(r: Rect, col: u16, row: u16) -> bool {
    col >= r.x
        && col < r.x.saturating_add(r.width)
        && row >= r.y
        && row < r.y.saturating_add(r.height)
}

/// Compute the on-screen rectangles for each tab pill and the
/// add-tab `+` button. Tabs are laid out left-to-right starting at
/// `area.x`; the `+` button always reserves its 3 columns at the
/// right edge so overflow can't push it off-screen. Any tab that
/// won't fit in the remaining width is dropped from the layout —
/// for phase 2 that's an acceptable degradation; phase 3 can add
/// scroll arrows.
pub fn compute(area: Rect, tab_labels: &[&str]) -> Layout {
    let mut out = Layout::default();
    if area.width == 0 || area.height == 0 {
        return out;
    }
    let plus_w = PLUS_LABEL.chars().count() as u16;
    if area.width <= plus_w {
        return out;
    }
    let plus_rect = Rect::new(area.right().saturating_sub(plus_w), area.y, plus_w, 1);
    let mut available = area.width.saturating_sub(plus_w);
    let mut x = area.x;
    for label in tab_labels {
        let label_w = label.chars().count() as u16;
        // Pill content: " <glyph> <label> " — 1 pad + 1 glyph + 1
        // space + label_w + 1 pad.
        let tab_w = TAB_PAD * 2 + 2 + label_w;
        if tab_w > available {
            break;
        }
        out.tabs.push(Slot {
            key: String::new(), // caller stamps the internal name in render()
            rect: Rect::new(x, area.y, tab_w, 1),
        });
        x = x.saturating_add(tab_w);
        available = available.saturating_sub(tab_w);
    }
    out.plus = Some(Slot {
        key: "+".to_string(),
        rect: plus_rect,
    });
    out
}

/// Render the tab strip into `area` and return the layout for
/// click handling. The caller supplies `tab_views` indexed in
/// container-member order; `None` for any tab whose tmux session
/// no longer exists (dead-row case — rare; the tab still renders
/// using its internal name and a `Status::Unknown` glyph).
pub fn render(
    buf: &mut Buffer,
    area: Rect,
    container: &Container,
    tab_views: &[Option<&SessionView>],
    theme: &Theme,
) -> Layout {
    // Resolve display label + status for each tab, then compute
    // the geometric layout in one shot. The labels are owned
    // strings so the slice can be reused for the (pure) compute
    // call below.
    let resolved: Vec<(String, Status)> = container
        .members
        .iter()
        .enumerate()
        .map(|(i, internal)| match tab_views.get(i).and_then(|v| *v) {
            Some(v) => (v.display().to_string(), v.status),
            None => (internal.clone(), Status::Unknown),
        })
        .collect();
    let label_refs: Vec<&str> = resolved.iter().map(|(l, _)| l.as_str()).collect();
    let mut layout = compute(area, &label_refs);

    // Paint the strip background so we never bleed onto whatever
    // was under it previously (the embed redraws every frame too,
    // but the strip lives outside the embed rect).
    let bg_style = Style::default().bg(theme.panel).fg(theme.text_muted);
    for x in area.left()..area.right() {
        let cell = &mut buf[(x, area.y)];
        cell.set_char(' ');
        cell.set_style(bg_style);
    }

    // Draw each tab pill in slot order.
    for (slot, internal) in layout.tabs.iter_mut().zip(container.members.iter()) {
        slot.key = internal.clone();
        let idx = container
            .members
            .iter()
            .position(|m| m == internal)
            .unwrap_or(0);
        let (label, status) = &resolved[idx];
        let active = internal == &container.active;
        let style = if active {
            Style::default().bg(theme.selection_bg).fg(theme.text)
        } else {
            Style::default().bg(theme.panel).fg(theme.text_muted)
        };
        let glyph = status.glyph();
        let pill = format!(" {} {} ", glyph, label);
        let mut col = slot.rect.x;
        for ch in pill.chars() {
            if col >= slot.rect.x.saturating_add(slot.rect.width) {
                break;
            }
            let cell = &mut buf[(col, slot.rect.y)];
            cell.set_char(ch);
            cell.set_style(style);
            col = col.saturating_add(1);
        }
    }

    // Draw the `+` button at the right edge in accent color so
    // it reads as a control, not a label.
    if let Some(plus) = &layout.plus {
        let style = Style::default().bg(theme.panel).fg(theme.accent);
        let mut col = plus.rect.x;
        for ch in PLUS_LABEL.chars() {
            if col >= plus.rect.x.saturating_add(plus.rect.width) {
                break;
            }
            let cell = &mut buf[(col, plus.rect.y)];
            cell.set_char(ch);
            cell.set_style(style);
            col = col.saturating_add(1);
        }
    }
    layout
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_lays_tabs_left_to_right_with_plus_at_right() {
        let area = Rect::new(0, 5, 40, 1);
        let layout = compute(area, &["one", "two"]);
        assert_eq!(layout.tabs.len(), 2);
        // " ● one " = 1+1+1+3+1 = 7 cols; same for "two".
        assert_eq!(layout.tabs[0].rect, Rect::new(0, 5, 7, 1));
        assert_eq!(layout.tabs[1].rect, Rect::new(7, 5, 7, 1));
        // " + " always at the right edge (3 cols).
        assert_eq!(layout.plus.as_ref().unwrap().rect, Rect::new(37, 5, 3, 1));
    }

    #[test]
    fn compute_truncates_overflow_keeping_plus() {
        // Five tabs at 7 cols each = 35, plus button = 3 → need
        // 38 cols. With only 25 cols, only 3 tabs fit.
        let area = Rect::new(0, 0, 25, 1);
        let layout = compute(area, &["one", "two", "thr", "fou", "fiv"]);
        assert_eq!(layout.tabs.len(), 3);
        assert!(layout.plus.is_some());
    }

    #[test]
    fn hit_resolves_plus_then_tabs() {
        let area = Rect::new(0, 0, 40, 1);
        let layout = compute(area, &["one", "two"]);
        // Click inside the first tab.
        let hit = layout.hit(3, 0).unwrap();
        assert_eq!(hit.rect, Rect::new(0, 0, 7, 1));
        // Click on +.
        let hit = layout.hit(38, 0).unwrap();
        assert_eq!(hit.key, "+");
        // Click in dead space.
        assert!(layout.hit(20, 0).is_none());
    }
}
