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
}

/// Renders VT-core snapshots to a host surface.
pub struct TerminalRenderer {
    highlight: HighlightStyle,
    highlighter: PromptSemanticHighlighter,
    presented: Option<Frame>,
}

impl TerminalRenderer {
    pub fn new(highlight: HighlightStyle) -> Self {
        Self {
            highlight,
            highlighter: PromptSemanticHighlighter,
            presented: None,
        }
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

        // Cursor: positioned at the end of the frame; hidden while the
        // viewport is scrolled into history (it is not on screen).
        let cursor_visible = snap.cursor_visible && snap.display_offset == 0;
        let position_cursor = cursor_visible
            && (full_frame
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
