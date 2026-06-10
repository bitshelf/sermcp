//! The VT/ANSI terminal core for the interactive serial display.
//!
//! `alacritty_terminal::Term` is the single source of terminal truth: it
//! parses every escape sequence, owns cursor/grid/scrollback/alternate
//! screen state, and reports incremental damage. This module is the only
//! place `alacritty_terminal` types appear — everything the renderer and
//! highlighter consume goes through the plain snapshots defined here.
//!
//! The connection is a serial transport, not a local PTY: bytes from the
//! wire are fed through the VTE `Processor` into the `Term`, and
//! terminal-generated replies to remote queries (DA, DSR, …) surface via
//! the [`EventListener`] as `PtyWrite` — never silently discarded. OSC 52
//! clipboard access stays disabled: the serial peer must not gain host
//! clipboard capabilities.

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{Config as TermConfig, Term, TermDamage, TermMode};
use alacritty_terminal::vte::ansi::{Color as VteColor, NamedColor, Processor};

/// One cell of a viewport snapshot — a plain copy, free of alacritty types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellView {
    pub c: char,
    pub fg: ColorView,
    pub bg: ColorView,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
    pub hidden: bool,
    /// Wide (double-width) character — occupies two host columns.
    pub wide: bool,
    /// Continuation cell of a wide character — emits nothing.
    pub wide_spacer: bool,
    /// Zero-width characters (combining marks) rendered after `c`.
    pub zerowidth: Vec<char>,
}

/// Color of a cell slot, independent of named-color bookkeeping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorView {
    /// The host default for this slot (SGR 39/49).
    #[default]
    Default,
    /// Standard 16-color palette index (0..=16).
    Palette(u8),
    /// 256-color palette index.
    Indexed(u8),
    /// Direct truecolor.
    Rgb(u8, u8, u8),
}

/// Viewport snapshot handed to the renderer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenSnapshot {
    /// Visible rows, top to bottom; each row has exactly `cols` cells.
    pub rows: Vec<Vec<CellView>>,
    /// Cursor as `(column, row)`, row 0 = top.
    pub cursor: (usize, usize),
    pub cursor_visible: bool,
    /// Lines the viewport is scrolled up into scrollback (0 = live view).
    pub display_offset: usize,
    /// Remote entered an alternate-screen full-screen application.
    pub alt_screen: bool,
    /// DECCKM application cursor keys are active.
    pub app_cursor: bool,
    pub cols: usize,
    pub lines: usize,
}

/// Local viewport scroll command — display state only, never sent to the
/// DUT. While scrolled up, new output keeps the view anchored to the same
/// history (the grid grows below it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewScroll {
    PageUp,
    PageDown,
    Top,
    Bottom,
    /// Relative line delta (positive scrolls TOWARD the history top).
    Delta(i32),
}

/// Damaged viewport region of one row; `left`/`right` are inclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowDamage {
    pub row: usize,
    pub left: usize,
    pub right: usize,
}

/// Terminal damage since the last [`TerminalCore::take_damage`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DamageReport {
    Full,
    Rows(Vec<RowDamage>),
}

/// `Dimensions` impl describing the emulated screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TermSize {
    cols: usize,
    lines: usize,
}

impl TermSize {
    pub fn new(cols: usize, lines: usize) -> Self {
        Self { cols, lines }
    }
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.lines
    }
    fn screen_lines(&self) -> usize {
        self.lines
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// Forwards terminal-generated events (VT query replies, bell, wakeup) to
/// the terminal thread. `&self` receive + `Sender` cloneability is exactly
/// the alacritty listener shape.
struct ReplyListener {
    tx: std::sync::mpsc::Sender<Event>,
}

impl EventListener for ReplyListener {
    fn send_event(&self, event: Event) {
        let _ = self.tx.send(event);
    }
}

/// The VT core. Single-owner only — never wrap in a lock shared across
/// tasks; the session's terminal thread is the sole owner.
/// Viewport policy on new output / typed input (see
/// `prefs::ScrollPrefs` — `~/.config/dutabo/terminal.toml`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollPolicy {
    /// At the bottom the view stays glued to live output; scrolled UP it
    /// stays anchored to the same content while output arrives. `false`
    /// makes a scrolled view ride along with the output instead.
    pub follow_output: bool,
    /// Typing jumps the view back to the live line.
    pub jump_on_input: bool,
}

impl Default for ScrollPolicy {
    fn default() -> Self {
        Self {
            follow_output: true,
            jump_on_input: false,
        }
    }
}

pub struct TerminalCore {
    term: Term<ReplyListener>,
    processor: Processor,
    size: TermSize,
    replies: std::sync::mpsc::Receiver<Event>,
    scroll_policy: ScrollPolicy,
}

impl TerminalCore {
    pub fn new(cols: usize, lines: usize) -> Self {
        let config = TermConfig {
            // Scrollback for the (future) scroll display; the host
            // alternate screen carries none of its own.
            scrolling_history: 10_000,
            // The serial peer must never touch the host clipboard.
            osc52: alacritty_terminal::term::Osc52::Disabled,
            ..TermConfig::default()
        };
        let (tx, replies) = std::sync::mpsc::channel();
        Self {
            term: Term::new(config, &TermSize::new(cols, lines), ReplyListener { tx }),
            processor: Processor::new(),
            size: TermSize::new(cols, lines),
            replies,
            scroll_policy: ScrollPolicy::default(),
        }
    }

    /// The active viewport policy (`~/.config/dutabo/terminal.toml`).
    pub fn scroll_policy(&self) -> ScrollPolicy {
        self.scroll_policy
    }

    pub fn set_scroll_policy(&mut self, policy: ScrollPolicy) {
        self.scroll_policy = policy;
    }

    pub fn cols(&self) -> usize {
        self.size.cols
    }

    pub fn lines(&self) -> usize {
        self.size.lines
    }

    /// DECCKM state (arrow-key encoding form) without a full snapshot.
    pub fn encode_paste(&self, text: &str) -> Vec<u8> {
        if self.term.mode().contains(TermMode::BRACKETED_PASTE) {
            format!("\x1b[200~{text}\x1b[201~").into_bytes()
        } else {
            text.as_bytes().to_vec()
        }
    }

    pub fn app_cursor(&self) -> bool {
        self.term.mode().contains(TermMode::APP_CURSOR)
    }

    /// Remote alternate-screen application active (wheel behavior).
    pub fn alt_screen(&self) -> bool {
        self.term.mode().contains(TermMode::ALT_SCREEN)
    }

    /// Viewport scroll distance from the live view (0 = at bottom).
    pub fn display_offset(&self) -> usize {
        self.term.renderable_content().display_offset
    }

    /// Feed serial payload (post transport-framing, post display-newline
    /// normalization) into the VT core, chunk at a time. The VT grid's
    /// own display-offset anchoring gives the follow_output=true
    /// behavior; drift mode (follow_output=false) un-anchors it so a
    /// scrolled view rides along with the output instead.
    pub fn process(&mut self, bytes: &[u8]) {
        let drift = !self.scroll_policy.follow_output && self.display_offset() > 0;
        let before = drift.then(|| self.display_offset());
        self.processor.advance(&mut self.term, bytes);
        if let Some(before) = before
            && let grown = self.display_offset()
            && grown > before
        {
            self.scroll(ViewScroll::Delta(-((grown - before) as i32)));
        }
    }

    /// Apply the input policy on TX (typing): jump_on_input pulls a
    /// scrolled view back to the live line; the default keeps the view
    /// wherever the user scrolled it.
    pub fn note_input(&mut self) {
        if self.scroll_policy.jump_on_input && self.display_offset() > 0 {
            self.scroll(ViewScroll::Bottom);
        }
    }

    /// Terminal-generated replies awaiting transmission (responses to
    /// remote DA/DSR-style queries). Draining keeps `PtyWrite` flowing.
    pub fn drain_replies(&mut self) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        while let Ok(event) = self.replies.try_recv() {
            if let Event::PtyWrite(data) = event {
                out.push(data.into_bytes());
            }
            // Wakeup/Bell/Title/Clipboard… have no transport meaning here.
        }
        out
    }

    /// Resize the emulated screen; marks everything damaged.
    pub fn resize(&mut self, cols: usize, lines: usize) {
        if self.size == TermSize::new(cols, lines) {
            return;
        }
        self.size = TermSize::new(cols, lines);
        self.term.resize(self.size);
    }

    /// Scroll the viewport through the scrollback history. Pure display
    /// state — never produces TX bytes or grid edits; marks full damage.
    pub fn scroll(&mut self, view: ViewScroll) {
        use alacritty_terminal::grid::Scroll;
        self.term.scroll_display(match view {
            ViewScroll::PageUp => Scroll::PageUp,
            ViewScroll::PageDown => Scroll::PageDown,
            ViewScroll::Top => Scroll::Top,
            ViewScroll::Bottom => Scroll::Bottom,
            ViewScroll::Delta(lines) => Scroll::Delta(lines),
        });
    }

    /// Consume damage accumulated since the previous call.
    pub fn take_damage(&mut self) -> DamageReport {
        let report = match self.term.damage() {
            TermDamage::Full => DamageReport::Full,
            TermDamage::Partial(iter) => DamageReport::Rows(
                iter.map(|b| RowDamage {
                    row: b.line,
                    left: b.left,
                    right: b.right,
                })
                .collect(),
            ),
        };
        self.term.reset_damage();
        report
    }

    /// Copy the current viewport for rendering.
    pub fn snapshot(&self) -> ScreenSnapshot {
        let mode = *self.term.mode();
        let content = self.term.renderable_content();
        let mut rows: Vec<Vec<CellView>> = Vec::with_capacity(self.size.lines);
        let mut current_line: Option<i32> = None;
        for indexed in content.display_iter {
            if current_line != Some(indexed.point.line.0) {
                current_line = Some(indexed.point.line.0);
                rows.push(Vec::with_capacity(self.size.cols));
            }
            if let Some(row) = rows.last_mut() {
                row.push(cell_view(indexed.cell));
            }
        }
        let cursor = content.cursor.point;
        ScreenSnapshot {
            rows,
            cursor: (
                cursor.column.0.min(self.size.cols.saturating_sub(1)),
                cursor.line.0.max(0) as usize,
            ),
            cursor_visible: mode.contains(TermMode::SHOW_CURSOR),
            display_offset: content.display_offset,
            alt_screen: mode.contains(TermMode::ALT_SCREEN),
            app_cursor: mode.contains(TermMode::APP_CURSOR),
            cols: self.size.cols,
            lines: self.size.lines,
        }
    }
}

fn cell_view(cell: &Cell) -> CellView {
    let flags = cell.flags;
    CellView {
        c: cell.c,
        fg: color_view(cell.fg),
        bg: color_view(cell.bg),
        bold: flags.contains(Flags::BOLD),
        dim: flags.contains(Flags::DIM),
        italic: flags.contains(Flags::ITALIC),
        underline: flags.contains(Flags::UNDERLINE),
        inverse: flags.contains(Flags::INVERSE),
        hidden: flags.contains(Flags::HIDDEN),
        wide: flags.contains(Flags::WIDE_CHAR),
        wide_spacer: flags.contains(Flags::WIDE_CHAR_SPACER)
            || flags.contains(Flags::LEADING_WIDE_CHAR_SPACER),
        zerowidth: cell.zerowidth().unwrap_or(&[]).to_vec(),
    }
}

fn color_view(color: VteColor) -> ColorView {
    match color {
        VteColor::Named(named) => match named {
            NamedColor::Foreground
            | NamedColor::Background
            | NamedColor::Cursor
            | NamedColor::BrightForeground
            | NamedColor::DimForeground => ColorView::Default,
            NamedColor::DimBlack
            | NamedColor::DimRed
            | NamedColor::DimGreen
            | NamedColor::DimYellow
            | NamedColor::DimBlue
            | NamedColor::DimMagenta
            | NamedColor::DimCyan
            | NamedColor::DimWhite => {
                ColorView::Palette((named as usize - NamedColor::DimBlack as usize) as u8)
            }
            other => ColorView::Palette(other as u8),
        },
        VteColor::Indexed(i) => ColorView::Indexed(i),
        VteColor::Spec(rgb) => ColorView::Rgb(rgb.r, rgb.g, rgb.b),
    }
}
