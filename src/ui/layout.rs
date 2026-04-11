//! Compute UI region rectangles. No rendering here.

use ratatui::layout::{Constraint, Direction, Layout, Rect};

pub struct Layouts {
    pub list: Rect,
    pub preview: Rect,
    pub statusbar: Rect,
}

pub fn compute(area: Rect) -> Layouts {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);
    let body = vertical[0];
    let statusbar = vertical[1];

    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
        .split(body);

    Layouts {
        list: horizontal[0],
        preview: horizontal[1],
        statusbar,
    }
}
