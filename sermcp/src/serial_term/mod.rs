//! Interactive serial terminal for `dutabo serial`.
//!
//! Architecture (the interactive display path only — raw serial semantics
//! never depend on any display processing here):
//!
//! ```text
//! ser2net TCP / MCP WebSocket → WireDecoder (telnet IAC + ser2net banner)
//!                              → RxNewlineNormalizer (display-only)
//!                              → TerminalCore (alacritty_terminal VT core)
//!                              → TerminalRenderer (damage + semantic overlay)
//!                              → host terminal (crossterm raw mode, alt screen)
//!
//! host keyboard/mouse → crossterm input thread → LocalInput (Ctrl-T escape)
//!                    → InputEncoder → transport TX immediately
//! ```
//!
//! Invariants (enforced by `tests.rs`):
//! * highlighting is a pure render-time overlay — RX/TX bytes, screen text,
//!   cursor and geometry are identical with highlighting on or off;
//! * raw serial data is logged/tapped before every display transformation;
//! * terminal semantics come from the alacritty VT core, never re-implemented.

pub mod highlight;
pub mod input;
pub mod newline;
pub mod renderer;
pub mod session;
pub mod terminal;
pub mod wire;

#[cfg(test)]
mod tests;

pub use highlight::HighlightStyle;
pub use input::{BackspaceMode, EnterMode, InputEncoder, LocalAction, LocalInput, WheelAction};
pub use newline::{RxNewlineMode, RxNewlineNormalizer};
pub use renderer::{HostSurface, StdoutSurface, TerminalRenderer};
pub use session::{
    SessionEvent, SessionFlags, SessionOutcome, TerminalGuard, spawn_input_thread,
    spawn_terminal_thread,
};
pub mod prefs;
pub use terminal::{ColorView, DamageReport, ScrollPolicy, TerminalCore, ViewScroll};
pub use wire::{WireDecoder, WireMode};

/// Interactive serial knobs resolved from the flat config map
/// (`[dut.serial]` keys `backspace` / `enter` / `rx_newline` / `highlight`
/// / `mouse`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SerialSettings {
    pub backspace: BackspaceMode,
    pub enter: EnterMode,
    pub rx_newline: RxNewlineMode,
    pub highlight: HighlightStyle,
    /// Capture mouse reporting so wheel notches drive the local
    /// scrollback (off keeps the host's default mouse behavior).
    pub mouse: bool,
}

impl Default for SerialSettings {
    fn default() -> Self {
        Self {
            backspace: BackspaceMode::Delete,
            enter: EnterMode::Cr,
            // Matches the previous interactive relay: a lone LF on the wire
            // starts the next display line at column zero.
            rx_newline: RxNewlineMode::LfImpliesCr,
            highlight: HighlightStyle::Prompt,
            mouse: true,
        }
    }
}

impl SerialSettings {
    /// Resolve from a flat-key lookup (`SERIAL_BACKSPACE`, `SERIAL_ENTER`,
    /// `SERIAL_RX_NEWLINE`, `SERIAL_HIGHLIGHT`, `SERIAL_MOUSE`). Unknown
    /// values keep the default — config validation reports them loudly at
    /// load time; this path must never panic mid-session.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let mut s = Self::default();
        if let Some(v) = get("SERIAL_BACKSPACE")
            && let Some(m) = BackspaceMode::parse(&v)
        {
            s.backspace = m;
        }
        if let Some(v) = get("SERIAL_ENTER")
            && let Some(m) = EnterMode::parse(&v)
        {
            s.enter = m;
        }
        if let Some(v) = get("SERIAL_RX_NEWLINE")
            && let Some(m) = RxNewlineMode::parse(&v)
        {
            s.rx_newline = m;
        }
        if let Some(v) = get("SERIAL_HIGHLIGHT")
            && let Some(m) = HighlightStyle::parse(&v)
        {
            s.highlight = m;
        }
        if let Some(v) = get("SERIAL_MOUSE")
            && let Some(m) = parse_bool(&v)
        {
            s.mouse = m;
        }
        s
    }
}

/// Boolean config value: `true/false`, `on/off`, `1/0`, `yes/no`. Public
/// so config validation reuses the exact accepted spellings.
pub fn parse_bool(v: &str) -> Option<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "true" | "on" | "1" | "yes" => Some(true),
        "false" | "off" | "0" | "no" => Some(false),
        _ => None,
    }
}
