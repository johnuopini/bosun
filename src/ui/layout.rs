//! Compute UI region rectangles. No rendering here.

use ratatui::layout::{Constraint, Direction, Layout, Rect};

/// Below this total terminal width, the preview pane is hidden and
/// the session list takes the full body width. The same threshold
/// also gates the embedded terminal (single-window mode, focused
/// attach) — there's no point spinning up a PTY for a pane that
/// won't render. Chosen at 80 cols so phones / small SSH clients
/// fall back to list-only; standard 80-col terminals still get the
/// split.
pub const PREVIEW_MIN_WIDTH: u16 = 80;

/// Minimum width for the session list pane when the user is
/// dragging the divider. Prevents the user from collapsing the
/// list to zero or a useless sliver.
pub const MIN_LIST_WIDTH: u16 = 20;

/// Minimum width for the preview pane when the user is dragging.
/// Below this the preview can't render anything meaningful.
pub const MIN_PREVIEW_WIDTH: u16 = 30;

/// Default split position expressed as a percentage of the body
/// width. Used when the user hasn't dragged the divider yet.
const DEFAULT_LIST_PERCENT: u16 = 38;

pub struct Layouts {
    pub list: Rect,
    /// `None` when the terminal is narrower than [`PREVIEW_MIN_WIDTH`].
    /// The session list expands to the full body width in that case
    /// and `ui::draw` skips rendering the preview pane entirely.
    pub preview: Option<Rect>,
    /// 1-col gutter between the list and the preview, rendered as a
    /// draggable divider glyph. `None` in narrow mode (no split).
    pub divider: Option<Rect>,
    pub statusbar: Rect,
}

/// Compute layout rects for the current terminal size.
///
/// `divider_col` is the user's preferred absolute column for the
/// divider (typically updated live during a mouse drag). `None`
/// means "use the default 38% list split". The value is clamped to
/// [`MIN_LIST_WIDTH`] and `body.width - MIN_PREVIEW_WIDTH - 1` so
/// the user can't drag the divider off the edge.
pub fn compute(area: Rect, divider_col: Option<u16>) -> Layouts {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);
    let body = vertical[0];
    let statusbar = vertical[1];

    if area.width < PREVIEW_MIN_WIDTH {
        // Narrow terminal (mobile / small mosh): list-only.
        return Layouts {
            list: body,
            preview: None,
            divider: None,
            statusbar,
        };
    }

    // `split_x` is the x-offset within `body` where the divider
    // glyph lives. The list occupies `[0, split_x)` and the preview
    // occupies `[split_x + 1, body.width)`.
    let max_split = body.width.saturating_sub(MIN_PREVIEW_WIDTH + 1);
    let default_split = body.width * DEFAULT_LIST_PERCENT / 100;
    let requested = divider_col
        .map(|abs| abs.saturating_sub(body.x))
        .unwrap_or(default_split);
    let split_x = requested.clamp(MIN_LIST_WIDTH, max_split);

    let list = Rect::new(body.x, body.y, split_x, body.height);
    let divider = Rect::new(body.x + split_x, body.y, 1, body.height);
    let preview_x = body.x + split_x + 1;
    let preview_width = body.width.saturating_sub(split_x + 1);
    let preview = Rect::new(preview_x, body.y, preview_width, body.height);

    Layouts {
        list,
        preview: Some(preview),
        divider: Some(divider),
        statusbar,
    }
}

/// True if the given absolute terminal column hits the divider for
/// the current layout. Used by mouse-event handling to decide
/// whether a Down event starts a drag.
pub fn is_divider_col(layouts: &Layouts, col: u16) -> bool {
    layouts.divider.map(|d| col == d.x).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(w: u16, h: u16) -> Rect {
        Rect::new(0, 0, w, h)
    }

    #[test]
    fn narrow_terminal_hides_preview_and_gives_full_width_to_list() {
        let l = compute(rect(50, 24), None);
        assert!(l.preview.is_none());
        assert!(l.divider.is_none());
        assert_eq!(l.list.width, 50);
        assert_eq!(l.list.height, 23); // body = total - 1 for statusbar
        assert_eq!(l.statusbar.y, 23);
    }

    #[test]
    fn exact_threshold_still_shows_preview_with_divider() {
        let l = compute(rect(PREVIEW_MIN_WIDTH, 24), None);
        let preview = l.preview.expect("preview at threshold");
        let divider = l.divider.expect("divider at threshold");
        assert_eq!(divider.width, 1);
        assert_eq!(
            l.list.width + divider.width + preview.width,
            PREVIEW_MIN_WIDTH
        );
        assert_eq!(divider.x, l.list.width);
    }

    #[test]
    fn just_below_threshold_is_list_only() {
        let l = compute(rect(PREVIEW_MIN_WIDTH - 1, 24), None);
        assert!(l.preview.is_none());
        assert!(l.divider.is_none());
    }

    #[test]
    fn wide_terminal_uses_default_38_percent_list_split() {
        let l = compute(rect(120, 30), None);
        let preview = l.preview.expect("preview on wide");
        let divider = l.divider.unwrap();
        // list width is ~38% of 120 ≈ 45
        assert_eq!(l.list.width + divider.width + preview.width, 120);
        assert!((44..=46).contains(&l.list.width));
    }

    #[test]
    fn user_divider_position_overrides_default() {
        let l = compute(rect(120, 30), Some(60));
        assert_eq!(l.list.width, 60);
        assert_eq!(l.divider.unwrap().x, 60);
        // Preview is everything right of the divider.
        assert_eq!(l.preview.unwrap().width, 120 - 60 - 1);
    }

    #[test]
    fn divider_clamped_to_min_list_width() {
        let l = compute(rect(120, 30), Some(5));
        assert_eq!(l.list.width, MIN_LIST_WIDTH);
    }

    #[test]
    fn divider_clamped_to_preserve_min_preview_width() {
        let l = compute(rect(120, 30), Some(200));
        let max_allowed = 120u16.saturating_sub(MIN_PREVIEW_WIDTH + 1);
        assert_eq!(l.list.width, max_allowed);
    }

    #[test]
    fn is_divider_col_matches_exact_column() {
        let l = compute(rect(120, 30), Some(50));
        assert!(is_divider_col(&l, 50));
        assert!(!is_divider_col(&l, 49));
        assert!(!is_divider_col(&l, 51));
    }

    #[test]
    fn is_divider_col_always_false_in_narrow_mode() {
        let l = compute(rect(40, 24), None);
        assert!(!is_divider_col(&l, 0));
        assert!(!is_divider_col(&l, 20));
    }
}
