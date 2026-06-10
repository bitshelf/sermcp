//! Damage-driven renderer: VT-core snapshot + semantic overlay → host
//! terminal bytes.
//!
//! Everything visual — text, wide characters, wrapping geometry, cursor —
//! comes from the alacritty core's snapshot. Highlighting only contributes
//! SGR on top of cells the remote left unstyled. Effective damage is the
//! union of terminal damage and semantic damage: when a row's highlight
//! overlay changes (a prompt completed retroactively), the whole row is
//! repainted even though the VT core only damaged the new cells.
//!
//! The renderer emits one ANSI byte buffer per frame through a
//! [`HostSurface`] — stdout in production, an in-memory sink in tests.

use std::io;

use super::highlight::{HighlightSpan, HighlightStyle, PromptSemanticHighlighter, SemanticStyle};
use super::scrollbar::Scrollbar;
use super::terminal::{CellView, ColorView, DamageReport, ScreenSnapshot};

/// Sink for rendered host-terminal bytes.
pub trait HostSurface {
    fn write_all(&mut self, data: &[u8]) -> io::Result<()>;
    fn flush(&mut self) -> io::Result<()>;
}

/// Standard-output surface (one buffered write + flush per frame).
#[derive(Default)]
pub struct StdoutSurface {
    buf: Vec<u8>,
}

impl StdoutSurface {
    pub fn new() -> Self {
        Self::default()
    }
}

impl HostSurface for StdoutSurface {
    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        self.buf.extend_from_slice(data);
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        use std::io::Write as _;
        let mut out = io::stdout().lock();
        out.write_all(&self.buf)?;
        out.flush()?;
        self.buf.clear();
        Ok(())
    }
}

/// Resolved style of one cell slot (`None` = host default for the slot).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct RenderStyle {
    fg: Option<ColorView>,
    bg: Option<ColorView>,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
}

/// The previous frame, for damage unioning.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Frame {
    cursor: (usize, usize),
    cursor_visible: bool,
    overlay_sig: Vec<Vec<HighlightSpan>>,
    cols: usize,
    lines: usize,
    scrollbar: Option<Scrollbar>,
}

/// Renders VT-core snapshots to a host surface.
pub struct TerminalRenderer {
    highlight: HighlightStyle,
    highlighter: PromptSemanticHighlighter,
    presented: Option<Frame>,
    scrollbar: bool,
}

impl TerminalRenderer {
    pub fn new(highlight: HighlightStyle) -> Self {
        Self {
            highlight,
            highlighter: PromptSemanticHighlighter,
            presented: None,
            scrollbar: false,
        }
    }

    /// Reserve one column shared by the arrows and filled thumb.
    pub fn set_scrollbar(&mut self, enabled: bool) {
        if self.scrollbar != enabled {
            self.presented = None;
        }
        self.scrollbar = enabled;
    }

    /// Render one frame. `damage` is the VT core's damage report for this
    /// frame; overlay changes and geometry changes extend it.
    pub fn render(
        &mut self,
        snap: &ScreenSnapshot,
        damage: &DamageReport,
        surface: &mut dyn HostSurface,
    ) -> io::Result<()> {
        let rows = snap.rows.len();
        let overlays = self.compute_overlays(snap);

        // Effective damage: (left, right) inclusive per row, None = clean.
        let mut ranges: Vec<Option<(usize, usize)>> = vec![None; rows];
        let full_frame = matches!(damage, DamageReport::Full)
            || self
                .presented
                .as_ref()
                .is_none_or(|f| f.cols != snap.cols || f.lines != snap.lines);
        if !full_frame {
            if let DamageReport::Rows(list) = damage {
                for d in list {
                    if d.row < rows {
                        let r = &mut ranges[d.row];
                        *r = Some(match *r {
                            Some((l, rr)) => (l.min(d.left), rr.max(d.right)),
                            None => (d.left, d.right),
                        });
                    }
                }
            }
            // Semantic damage: an overlay difference repaints the whole row.
            let prev = self.presented.as_ref().unwrap();
            for row in 0..rows {
                let changed =
                    row >= prev.overlay_sig.len() || prev.overlay_sig[row] != overlays[row].spans;
                if changed {
                    ranges[row] = Some((0, snap.cols.saturating_sub(1)));
                }
            }
        }
        if full_frame {
            ranges = vec![Some((0, snap.cols.saturating_sub(1))); rows];
        }

        let mut buf: Vec<u8> = Vec::with_capacity(4096);
        for (row, range) in ranges.iter().enumerate() {
            let Some((left, right)) = *range else {
                continue;
            };
            let Some(cells) = snap.rows.get(row) else {
                continue;
            };
            write_move_to(&mut buf, left, row);
            let mut current: Option<RenderStyle> = None;
            let right = right.min(cells.len().saturating_sub(1));
            for (col, cell) in cells.iter().enumerate().take(right + 1).skip(left) {
                if cell.wide_spacer {
                    // The wide char printed before us already advanced the
                    // host cursor past this column.
                    continue;
                }
                let style = effective_style(cell, overlay_at(&overlays[row], col));
                if current != Some(style) {
                    write_sgr(&mut buf, &style);
                    current = Some(style);
                }
                write_char(&mut buf, cell);
            }
        }
        if full_frame || ranges.iter().any(Option::is_some) {
            buf.extend_from_slice(b"\x1b[m");
        }

        let scrollbar = self.scrollbar.then(|| Scrollbar::new(snap)).flatten();
        let bar_dirty = self.scrollbar
            && (full_frame
                || self
                    .presented
                    .as_ref()
                    .is_none_or(|f| f.scrollbar != scrollbar));
        if bar_dirty {
            for row in 0..snap.lines {
                write_move_to(&mut buf, snap.cols, row);
                // Background fills avoid glyph outlines and line-spacing gaps.
                if scrollbar.is_some_and(|bar| bar.is_thumb(row)) {
                    buf.extend_from_slice(b"\x1b[0;48;2;76;76;76m ");
                } else {
                    buf.extend_from_slice(b"\x1b[0;38;2;112;112;112m");
                    buf.extend_from_slice(scrollbar.map_or(" ", |bar| bar.symbol(row)).as_bytes());
                }
            }
            buf.extend_from_slice(b"\x1b[m");
        }

        // Cursor: positioned at the end of the frame; hidden while the
        // viewport is scrolled into history (it is not on screen).
        let cursor_visible = snap.cursor_visible && snap.display_offset == 0;
        let position_cursor = cursor_visible
            && (full_frame
                || bar_dirty
                || ranges.iter().any(Option::is_some)
                || self
                    .presented
                    .as_ref()
                    .is_none_or(|f| f.cursor != snap.cursor));
        if position_cursor {
            write_move_to(&mut buf, snap.cursor.0, snap.cursor.1);
        }
        if self
            .presented
            .as_ref()
            .is_none_or(|f| f.cursor_visible != cursor_visible)
        {
            buf.extend_from_slice(if cursor_visible {
                b"\x1b[?25h"
            } else {
                b"\x1b[?25l"
            });
        }

        if !buf.is_empty() {
            surface.write_all(&buf)?;
            surface.flush()?;
        }

        self.presented = Some(Frame {
            cursor: snap.cursor,
            cursor_visible,
            overlay_sig: overlays.iter().map(|o| o.spans.clone()).collect(),
            cols: snap.cols,
            lines: snap.lines,
            scrollbar,
        });
        Ok(())
    }

    /// Compute the frame's overlays: empty in alternate-screen applications
    /// (their own SGR output is the final UI) and when highlighting is off.
    fn compute_overlays(&self, snap: &ScreenSnapshot) -> Vec<super::highlight::RowHighlight> {
        let empty = vec![super::highlight::RowHighlight::default(); snap.rows.len()];
        if !self.highlight.enabled() || snap.alt_screen {
            return empty;
        }
        snap.rows
            .iter()
            .enumerate()
            .map(|(row, cells)| {
                let mut text = String::with_capacity(cells.len());
                for cell in cells {
                    text.push(cell.c);
                }
                self.highlighter.analyze(row, &text)
            })
            .collect()
    }

    /// Previous frame's overlay signature for a row (test/inspection hook).
    #[cfg(test)]
    pub fn presented_overlay(&self, row: usize) -> Vec<HighlightSpan> {
        self.presented
            .as_ref()
            .and_then(|f| f.overlay_sig.get(row))
            .cloned()
            .unwrap_or_default()
    }
}

fn overlay_at(overlay: &super::highlight::RowHighlight, col: usize) -> Option<SemanticStyle> {
    overlay
        .spans
        .iter()
        .find(|s| col >= s.start_col && col < s.end_col)
        .map(|s| s.style)
}

/// Overlay-merged style of a cell for painting.
fn effective_style(cell: &CellView, overlay: Option<SemanticStyle>) -> RenderStyle {
    let mut style = effective_style_pure(cell);
    // Remote application SGR always wins: only unstyled cells are recolored.
    if let Some(semantic) = overlay
        && style.fg.is_none()
        && style.bg.is_none()
        && !cell.inverse
    {
        let (color, bold) = match semantic {
            SemanticStyle::PromptUserHost => (ColorView::Palette(2), true),
            SemanticStyle::PromptPath => (ColorView::Palette(4), true),
            SemanticStyle::PromptSigil => (ColorView::Palette(7), true),
        };
        style.fg = Some(color);
        style.bold = bold;
    }
    style
}

/// Style of a cell ignoring any overlay (stored for frame diffing).
fn effective_style_pure(cell: &CellView) -> RenderStyle {
    let (fg, bg) = if cell.inverse {
        // Inverse renders by swapping the slots.
        (slot_of(cell.bg), slot_of(cell.fg))
    } else {
        (slot_of(cell.fg), slot_of(cell.bg))
    };
    RenderStyle {
        fg,
        bg,
        bold: cell.bold,
        dim: cell.dim,
        italic: cell.italic,
        underline: cell.underline,
    }
}

fn slot_of(color: ColorView) -> Option<ColorView> {
    match color {
        ColorView::Default => None,
        other => Some(other),
    }
}

fn write_move_to(buf: &mut Vec<u8>, col: usize, row: usize) {
    buf.extend_from_slice(b"\x1b[");
    push_usize(buf, row + 1);
    buf.push(b';');
    push_usize(buf, col + 1);
    buf.push(b'H');
}

fn write_char(buf: &mut Vec<u8>, cell: &CellView) {
    if cell.hidden {
        buf.push(b' ');
        return;
    }
    let mut encoded = [0u8; 4];
    buf.extend_from_slice(cell.c.encode_utf8(&mut encoded).as_bytes());
    for c in &cell.zerowidth {
        buf.extend_from_slice(c.encode_utf8(&mut encoded).as_bytes());
    }
}

fn write_sgr(buf: &mut Vec<u8>, style: &RenderStyle) {
    buf.extend_from_slice(b"\x1b[0");
    if let Some(color) = style.fg {
        write_color(buf, color, false);
    }
    if let Some(color) = style.bg {
        write_color(buf, color, true);
    }
    if style.bold {
        buf.extend_from_slice(b";1");
    }
    if style.dim {
        buf.extend_from_slice(b";2");
    }
    if style.italic {
        buf.extend_from_slice(b";3");
    }
    if style.underline {
        buf.extend_from_slice(b";4");
    }
    buf.push(b'm');
}

fn write_color(buf: &mut Vec<u8>, color: ColorView, background: bool) {
    let base = if background { 40 } else { 30 };
    match color {
        ColorView::Default => {}
        ColorView::Palette(i) if i < 8 => {
            buf.push(b';');
            push_usize(buf, base + i as usize);
        }
        ColorView::Palette(i) if i < 16 => {
            buf.extend_from_slice(if background { b";10" } else { b";9" });
            push_usize(buf, i as usize - 8);
        }
        ColorView::Palette(i) => {
            extended_color_prefix(buf, background);
            push_usize(buf, i as usize);
        }
        ColorView::Indexed(i) => {
            extended_color_prefix(buf, background);
            push_usize(buf, i as usize);
        }
        ColorView::Rgb(r, g, b) => {
            buf.extend_from_slice(if background { b";48;2;" } else { b";38;2;" });
            push_usize(buf, r as usize);
            buf.push(b';');
            push_usize(buf, g as usize);
            buf.push(b';');
            push_usize(buf, b as usize);
        }
    }
}

fn extended_color_prefix(buf: &mut Vec<u8>, background: bool) {
    buf.extend_from_slice(if background { b";48;5;" } else { b";38;5;" });
}

fn push_usize(buf: &mut Vec<u8>, mut v: usize) {
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    loop {
        i -= 1;
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    buf.extend_from_slice(&tmp[i..]);
}

#[cfg(test)]
mod scrollbar_tests {
    use super::*;
    use crate::serial_term::{TerminalCore, ViewScroll};

    #[derive(Default)]
    struct Surface(Vec<u8>);
    impl HostSurface for Surface {
        fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
            self.0.extend_from_slice(bytes);
            Ok(())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn gutter_round_trip_preserves_text_cursor_and_clears_for_alt_screen() {
        let mut source = TerminalCore::new(19, 10);
        let mut host = TerminalCore::new(20, 10);
        let mut renderer = TerminalRenderer::new(HighlightStyle::Prompt);
        renderer.set_scrollbar(true);
        let mut surface = Surface::default();
        for n in 0..50 {
            source.process(format!("line {n} 中文\r\n").as_bytes());
        }
        for step in 0..5 {
            match step {
                1 => source.scroll(ViewScroll::Top),
                2 => source.process(b"another line\r\n"),
                3 => source.scroll(ViewScroll::Bottom),
                4 => source.process(b"\x1b[?1049hfull screen"),
                _ => {}
            }
            let snap = source.snapshot();
            let damage = source.take_damage();
            renderer.render(&snap, &damage, &mut surface).unwrap();
            host.process(&surface.0);
            surface.0.clear();
            let painted = host.snapshot();
            for (actual, expected) in painted.rows.iter().zip(&snap.rows) {
                assert_eq!(
                    actual[..19].iter().map(|c| c.c).collect::<String>(),
                    expected.iter().map(|c| c.c).collect::<String>()
                );
            }
            if snap.alt_screen {
                assert!(
                    painted
                        .rows
                        .iter()
                        .all(|row| row[19..].iter().all(|cell| cell.c == ' '))
                );
            } else {
                let bar = Scrollbar::new(&snap).unwrap();
                for (row, cells) in painted.rows.iter().enumerate() {
                    let actual: String = cells[19..].iter().map(|cell| cell.c).collect();
                    assert_eq!(actual, bar.symbol(row));
                    assert_eq!(
                        cells[19].bg,
                        if bar.is_thumb(row) {
                            ColorView::Rgb(76, 76, 76)
                        } else {
                            ColorView::Default
                        }
                    );
                }
            }
            assert_eq!(
                painted.cursor_visible,
                snap.cursor_visible && snap.display_offset == 0
            );
            if painted.cursor_visible {
                assert_eq!(painted.cursor, snap.cursor);
            }
        }
    }
}
