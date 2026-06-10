//! Shared scrollbar geometry for rendering and mouse hit testing.

use super::terminal::ScreenSnapshot;

/// The arrows and filled thumb share one terminal column.
pub(super) const WIDTH: usize = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Scrollbar {
    pub height: usize,
    pub start: usize,
    pub thumb: usize,
    pub history: usize,
}

impl Scrollbar {
    pub fn new(snap: &ScreenSnapshot) -> Option<Self> {
        if snap.alt_screen || snap.lines < 4 || snap.history_size == 0 {
            return None;
        }
        let track = snap.lines - 2;
        let thumb = (track * snap.lines / (snap.history_size + snap.lines)).clamp(2, track);
        let travel = track - thumb;
        let position = snap.history_size.saturating_sub(snap.display_offset);
        let start = 1
            + (position * travel)
                .checked_div(snap.history_size)
                .unwrap_or(0);
        Some(Self {
            height: snap.lines,
            start,
            thumb,
            history: snap.history_size,
        })
    }

    pub fn is_thumb(self, row: usize) -> bool {
        (self.start..self.start + self.thumb).contains(&row)
    }

    pub fn symbol(self, row: usize) -> &'static str {
        if row == 0 {
            "▲"
        } else if row + 1 == self.height {
            "▼"
        } else {
            " "
        }
    }

    /// Convert a dragged thumb's top row to the core's distance from live.
    pub fn offset(self, row: usize, grab: usize) -> usize {
        if row == 0 {
            return self.history;
        }
        if row >= self.height - 1 {
            return 0;
        }
        let travel = self.height - 2 - self.thumb;
        if travel == 0 {
            return 0;
        }
        let top = row.saturating_sub(grab + 1).min(travel);
        self.history - (top * self.history + travel / 2) / travel
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serial_term::{TerminalCore, ViewScroll};

    #[test]
    fn thumb_geometry_maps_both_endpoints_and_clamps_drag() {
        let mut term = TerminalCore::new(30, 10);
        for n in 0..100 {
            term.process(format!("line {n}\r\n").as_bytes());
        }
        let live = Scrollbar::new(&term.snapshot()).unwrap();
        assert_eq!(live.start + live.thumb, 9);
        assert_eq!(live.offset(0, 0), live.history);
        assert_eq!(live.offset(usize::MAX, 0), 0);
        term.scroll(ViewScroll::Top);
        let top = Scrollbar::new(&term.snapshot()).unwrap();
        assert_eq!(top.start, 1);
        assert_eq!(top.offset(top.start, 0), top.history);
        term.process(b"\x1b[?1049h");
        assert!(Scrollbar::new(&term.snapshot()).is_none());
    }
}
