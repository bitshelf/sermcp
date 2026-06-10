//! Host keyboard → serial byte encoding.
//!
//! All key mapping lives here — exactly one place defines what Enter,
//! Backspace, arrows or Ctrl keys mean on the wire. The encoder is a pure
//! function of the key event plus the terminal's current APP_CURSOR mode
//! (arrow/function keys flip between CSI and SS3 forms when the remote
//! application enabled application cursor keys via DECCKM).
//!
//! Backspace is NOT a local edit: it maps to a byte (DEL 0x7f or BS 0x08),
//! is transmitted, and the target's tty erases and echoes — the echo comes
//! back through the normal RX path. Nothing is ever drawn locally.
//!
//! Ctrl-T is the local transport escape prefix: `Ctrl-T q` quits without
//! sending anything; `Ctrl-T <key>` sends a literal Ctrl-T followed by the
//! key's encoding.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// What the Backspace key transmits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BackspaceMode {
    /// DEL (`0x7f`, `^?`) — the serial-console default.
    #[default]
    Delete,
    /// BS (`0x08`, `^H`) — for legacy tty drivers expecting Ctrl-H.
    CtrlH,
}

impl BackspaceMode {
    pub fn parse(v: &str) -> Option<Self> {
        match v.trim().to_ascii_lowercase().as_str() {
            "delete" | "del" | "0x7f" => Some(Self::Delete),
            "ctrl-h" | "ctrl_h" | "ctrlh" | "bs" | "0x08" => Some(Self::CtrlH),
            _ => None,
        }
    }

    fn byte(self) -> u8 {
        match self {
            Self::Delete => 0x7f,
            Self::CtrlH => 0x08,
        }
    }
}

/// What the Enter key transmits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EnterMode {
    /// CR (`0x0d`) — the serial-console default.
    #[default]
    Cr,
    /// LF (`0x0a`).
    Lf,
    /// CR LF (`0x0d 0x0a`).
    CrLf,
}

impl EnterMode {
    pub fn parse(v: &str) -> Option<Self> {
        match v.trim().to_ascii_lowercase().as_str() {
            "cr" | "\r" => Some(Self::Cr),
            "lf" | "\n" => Some(Self::Lf),
            "crlf" | "cr+lf" => Some(Self::CrLf),
            _ => None,
        }
    }

    fn bytes(self, out: &mut Vec<u8>) {
        match self {
            Self::Cr => out.push(0x0d),
            Self::Lf => out.push(0x0a),
            Self::CrLf => out.extend_from_slice(&[0x0d, 0x0a]),
        }
    }
}

/// Encoder state for the Ctrl-T local transport escape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LocalEscape {
    #[default]
    Idle,
    CtrlT,
}

/// Host key event → serial bytes, with the Ctrl-T local escape layered on
/// top (the escape state belongs to transport control, not the VT parser).
#[derive(Debug)]
pub struct LocalInput {
    escape: LocalEscape,
    encoder: InputEncoder,
}

/// What the input layer decided for one key event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalAction {
    /// Bytes to transmit immediately (fast path — render never gates this).
    Bytes(Vec<u8>),
    /// `Ctrl-T q` — quit the session without sending anything.
    Quit,
    /// Local viewport scroll (Shift+PageUp/PageDown/Home/End) — display
    /// state only, never a single byte to the DUT.
    Scroll(ScrollKey),
    /// Key has no serial encoding (modifier-only events, media keys, …).
    None,
}

/// Local scrollback navigation keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollKey {
    PageUp,
    PageDown,
    Top,
    Bottom,
}

impl LocalInput {
    pub fn new(encoder: InputEncoder) -> Self {
        Self {
            escape: LocalEscape::Idle,
            encoder,
        }
    }

    /// Handle one key press. `app_cursor` mirrors the VT core's DECCKM
    /// state so arrows follow the remote application's expectation.
    pub fn prefix_active(&self) -> bool {
        matches!(self.escape, LocalEscape::CtrlT)
    }

    pub fn handle(&mut self, key: KeyEvent, app_cursor: bool) -> LocalAction {
        if key.kind != KeyEventKind::Press {
            return LocalAction::None;
        }
        if let Some(scroll) = scroll_key(&key) {
            // Display-only local navigation — takes precedence over both
            // the Ctrl-T escape state and serial encoding.
            return LocalAction::Scroll(scroll);
        }
        match self.escape {
            LocalEscape::Idle => {
                if is_ctrl_t(&key) {
                    self.escape = LocalEscape::CtrlT;
                    return LocalAction::None;
                }
                let mut out = Vec::with_capacity(8);
                self.encoder
                    .encode(key.code, key.modifiers, app_cursor, &mut out);
                if out.is_empty() {
                    LocalAction::None
                } else {
                    LocalAction::Bytes(out)
                }
            }
            LocalEscape::CtrlT => {
                self.escape = LocalEscape::Idle;
                if matches!(key.code, KeyCode::Char('q' | 'Q')) {
                    return LocalAction::Quit;
                }
                if is_ctrl_t(&key) {
                    return LocalAction::Bytes(vec![0x14]);
                }
                // `Ctrl-T <navigation key>` pages the local scrollback.
                // Most host terminals (Xshell, xterm, VTE, Alacritty,
                // Windows Terminal) consume Shift+PageUp for their own
                // scrollback and never forward it — a Ctrl-prefixed chord
                // always reaches the application.
                match key.code {
                    KeyCode::PageUp => return LocalAction::Scroll(ScrollKey::PageUp),
                    KeyCode::PageDown => return LocalAction::Scroll(ScrollKey::PageDown),
                    KeyCode::Home => return LocalAction::Scroll(ScrollKey::Top),
                    KeyCode::End => return LocalAction::Scroll(ScrollKey::Bottom),
                    _ => {}
                }
                let mut out = vec![0x14];
                self.encoder
                    .encode(key.code, key.modifiers, app_cursor, &mut out);
                if out.len() == 1 {
                    LocalAction::None
                } else {
                    LocalAction::Bytes(out)
                }
            }
        }
    }
}

/// One mouse-wheel notch resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WheelAction {
    /// Scroll the local scrollback (main screen).
    Scroll(super::terminal::ViewScroll),
    /// Bytes for the DUT (full-screen application on the remote side —
    /// the xterm convention: the wheel drives arrow keys).
    Send(Vec<u8>),
}

/// Lines scrolled per wheel notch (the common terminal default).
pub const WHEEL_LINES: i32 = 3;

/// Resolve one wheel notch. On the main screen the wheel pages the local
/// scrollback (display-only); inside a remote alternate-screen application
/// (vim/top/less on the DUT) it sends arrow keys instead — those apps have
/// their own scrolling, and the emulated alt screen has no history.
pub fn wheel_action(up: bool, alt_screen: bool) -> WheelAction {
    if alt_screen {
        let seq: &[u8] = if up { b"\x1b[A" } else { b"\x1b[B" };
        let mut bytes = Vec::with_capacity(seq.len() * WHEEL_LINES as usize);
        for _ in 0..WHEEL_LINES {
            bytes.extend_from_slice(seq);
        }
        WheelAction::Send(bytes)
    } else {
        let delta = if up { WHEEL_LINES } else { -WHEEL_LINES };
        WheelAction::Scroll(crate::serial_term::ViewScroll::Delta(delta))
    }
}

fn is_ctrl_t(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('t' | 'T')) && key.modifiers.contains(KeyModifiers::CONTROL)
}

/// Shift+PageUp/PageDown page through the local scrollback; Shift+Home/End
/// jump to its top/bottom. Exact SHIFT only — unmodified PageUp/Home/End
/// keep their serial encodings for the DUT, and Ctrl/Ctrl+Shift combos
/// pass through untouched.
fn scroll_key(key: &KeyEvent) -> Option<ScrollKey> {
    if key.modifiers != KeyModifiers::SHIFT {
        return None;
    }
    match key.code {
        KeyCode::PageUp => Some(ScrollKey::PageUp),
        KeyCode::PageDown => Some(ScrollKey::PageDown),
        KeyCode::Home => Some(ScrollKey::Top),
        KeyCode::End => Some(ScrollKey::Bottom),
        _ => None,
    }
}

/// Stateless key → bytes encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InputEncoder {
    pub backspace: BackspaceMode,
    pub enter: EnterMode,
}

impl InputEncoder {
    pub fn new(backspace: BackspaceMode, enter: EnterMode) -> Self {
        Self { backspace, enter }
    }

    /// Append the serial encoding of one key. Emits nothing for keys with
    /// no meaningful serial form — the caller treats empty as "ignore".
    pub fn encode(
        &self,
        code: KeyCode,
        modifiers: KeyModifiers,
        app_cursor: bool,
        out: &mut Vec<u8>,
    ) {
        if modifiers.contains(KeyModifiers::CONTROL)
            && let KeyCode::Char(c) = code
            && let Some(b) = ctrl_byte(c)
        {
            out.push(b);
            return;
        }
        if modifiers.contains(KeyModifiers::ALT)
            && let KeyCode::Char(c) = code
        {
            // Meta is the classic ESC-prefix.
            out.push(0x1b);
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            return;
        }
        match code {
            KeyCode::Char(c) => {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
            KeyCode::Enter => self.enter.bytes(out),
            KeyCode::Backspace => out.push(self.backspace.byte()),
            KeyCode::Tab => out.push(0x09),
            KeyCode::BackTab => out.extend_from_slice(b"\x1b[Z"),
            KeyCode::Esc => out.push(0x1b),
            KeyCode::Left => arrow(out, b'D', app_cursor),
            KeyCode::Right => arrow(out, b'C', app_cursor),
            KeyCode::Up => arrow(out, b'A', app_cursor),
            KeyCode::Down => arrow(out, b'B', app_cursor),
            KeyCode::Home => move_key(out, b'H', app_cursor),
            KeyCode::End => move_key(out, b'F', app_cursor),
            KeyCode::Delete => out.extend_from_slice(b"\x1b[3~"),
            KeyCode::Insert => out.extend_from_slice(b"\x1b[2~"),
            KeyCode::PageUp => out.extend_from_slice(b"\x1b[5~"),
            KeyCode::PageDown => out.extend_from_slice(b"\x1b[6~"),
            KeyCode::F(1) => out.extend_from_slice(b"\x1bOP"),
            KeyCode::F(2) => out.extend_from_slice(b"\x1bOQ"),
            KeyCode::F(3) => out.extend_from_slice(b"\x1bOR"),
            KeyCode::F(4) => out.extend_from_slice(b"\x1bOS"),
            KeyCode::F(5) => out.extend_from_slice(b"\x1b[15~"),
            KeyCode::F(6) => out.extend_from_slice(b"\x1b[17~"),
            KeyCode::F(7) => out.extend_from_slice(b"\x1b[18~"),
            KeyCode::F(8) => out.extend_from_slice(b"\x1b[19~"),
            KeyCode::F(9) => out.extend_from_slice(b"\x1b[20~"),
            KeyCode::F(10) => out.extend_from_slice(b"\x1b[21~"),
            KeyCode::F(11) => out.extend_from_slice(b"\x1b[23~"),
            KeyCode::F(12) => out.extend_from_slice(b"\x1b[24~"),
            // Media keys, keyboard-enhancement pseudo keys, null key: no
            // serial representation.
            _ => {}
        }
    }
}

fn arrow(out: &mut Vec<u8>, final_byte: u8, app_cursor: bool) {
    if app_cursor {
        out.extend_from_slice(&[0x1b, b'O', final_byte]);
    } else {
        out.extend_from_slice(&[0x1b, b'[', final_byte]);
    }
}

fn move_key(out: &mut Vec<u8>, final_byte: u8, app_cursor: bool) {
    if app_cursor {
        out.extend_from_slice(&[0x1b, b'O', final_byte]);
    } else {
        out.extend_from_slice(&[0x1b, b'[', final_byte]);
    }
}

/// Control-byte mapping for Ctrl combos (ASCII control set). Returns None
/// for combos with no standard byte.
fn ctrl_byte(c: char) -> Option<u8> {
    let b = c.to_ascii_lowercase() as u8;
    match b {
        b'@' => Some(0x00),
        b'a'..=b'z' => Some(b - b'a' + 1),
        b'[' => Some(0x1b),
        b'\\' => Some(0x1c),
        b']' => Some(0x1d),
        b'^' => Some(0x1e),
        b'_' => Some(0x1f),
        b' ' => Some(0x00),
        b'?' => Some(0x7f),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(encoder: &InputEncoder, code: KeyCode, mods: KeyModifiers) -> Vec<u8> {
        let mut out = Vec::new();
        encoder.encode(code, mods, false, &mut out);
        out
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn key_mod(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn enter_modes() {
        let mut out = Vec::new();
        InputEncoder::new(BackspaceMode::Delete, EnterMode::Cr).encode(
            KeyCode::Enter,
            KeyModifiers::NONE,
            false,
            &mut out,
        );
        assert_eq!(out, vec![0x0d]);

        let mut out = Vec::new();
        InputEncoder::new(BackspaceMode::Delete, EnterMode::Lf).encode(
            KeyCode::Enter,
            KeyModifiers::NONE,
            false,
            &mut out,
        );
        assert_eq!(out, vec![0x0a]);

        let mut out = Vec::new();
        InputEncoder::new(BackspaceMode::Delete, EnterMode::CrLf).encode(
            KeyCode::Enter,
            KeyModifiers::NONE,
            false,
            &mut out,
        );
        assert_eq!(out, vec![0x0d, 0x0a]);
    }

    #[test]
    fn backspace_modes() {
        let encoder = InputEncoder::new(BackspaceMode::Delete, EnterMode::Cr);
        assert_eq!(
            enc(&encoder, KeyCode::Backspace, KeyModifiers::NONE),
            vec![0x7f]
        );
        let encoder = InputEncoder::new(BackspaceMode::CtrlH, EnterMode::Cr);
        assert_eq!(
            enc(&encoder, KeyCode::Backspace, KeyModifiers::NONE),
            vec![0x08]
        );
    }

    #[test]
    fn ctrl_keys_keep_serial_semantics() {
        let encoder = InputEncoder::default();
        assert_eq!(
            enc(&encoder, KeyCode::Char('c'), KeyModifiers::CONTROL),
            vec![0x03]
        );
        assert_eq!(
            enc(&encoder, KeyCode::Char('d'), KeyModifiers::CONTROL),
            vec![0x04]
        );
        assert_eq!(
            enc(&encoder, KeyCode::Char('z'), KeyModifiers::CONTROL),
            vec![0x1a]
        );
        assert_eq!(
            enc(&encoder, KeyCode::Char('l'), KeyModifiers::CONTROL),
            vec![0x0c]
        );
        assert_eq!(
            enc(&encoder, KeyCode::Char('u'), KeyModifiers::CONTROL),
            vec![0x15]
        );
        assert_eq!(
            enc(&encoder, KeyCode::Char('w'), KeyModifiers::CONTROL),
            vec![0x17]
        );
        assert_eq!(
            enc(&encoder, KeyCode::Char('t'), KeyModifiers::CONTROL),
            vec![0x14]
        );
    }

    #[test]
    fn tab_esc_alt_and_arrows() {
        let encoder = InputEncoder::default();
        assert_eq!(enc(&encoder, KeyCode::Tab, KeyModifiers::NONE), vec![0x09]);
        assert_eq!(
            enc(&encoder, KeyCode::BackTab, KeyModifiers::NONE),
            b"\x1b[Z"
        );
        assert_eq!(enc(&encoder, KeyCode::Esc, KeyModifiers::NONE), vec![0x1b]);
        assert_eq!(
            enc(&encoder, KeyCode::Char('x'), KeyModifiers::ALT),
            b"\x1bx"
        );
        assert_eq!(enc(&encoder, KeyCode::Up, KeyModifiers::NONE), b"\x1b[A");
        assert_eq!(enc(&encoder, KeyCode::Down, KeyModifiers::NONE), b"\x1b[B");
        assert_eq!(enc(&encoder, KeyCode::Right, KeyModifiers::NONE), b"\x1b[C");
        assert_eq!(enc(&encoder, KeyCode::Left, KeyModifiers::NONE), b"\x1b[D");
    }

    #[test]
    fn arrows_follow_app_cursor_mode() {
        let encoder = InputEncoder::default();
        let mut out = Vec::new();
        encoder.encode(KeyCode::Up, KeyModifiers::NONE, true, &mut out);
        assert_eq!(out, b"\x1bOA");
        let mut out = Vec::new();
        encoder.encode(KeyCode::Home, KeyModifiers::NONE, true, &mut out);
        assert_eq!(out, b"\x1bOH");
    }

    #[test]
    fn editing_and_function_keys() {
        let encoder = InputEncoder::default();
        assert_eq!(
            enc(&encoder, KeyCode::Delete, KeyModifiers::NONE),
            b"\x1b[3~"
        );
        assert_eq!(
            enc(&encoder, KeyCode::Insert, KeyModifiers::NONE),
            b"\x1b[2~"
        );
        assert_eq!(enc(&encoder, KeyCode::Home, KeyModifiers::NONE), b"\x1b[H");
        assert_eq!(enc(&encoder, KeyCode::End, KeyModifiers::NONE), b"\x1b[F");
        assert_eq!(
            enc(&encoder, KeyCode::PageUp, KeyModifiers::NONE),
            b"\x1b[5~"
        );
        assert_eq!(
            enc(&encoder, KeyCode::PageDown, KeyModifiers::NONE),
            b"\x1b[6~"
        );
        assert_eq!(enc(&encoder, KeyCode::F(1), KeyModifiers::NONE), b"\x1bOP");
        assert_eq!(
            enc(&encoder, KeyCode::F(12), KeyModifiers::NONE),
            b"\x1b[24~"
        );
    }

    #[test]
    fn ctrl_t_q_quits_locally() {
        let mut li = LocalInput::new(InputEncoder::default());
        assert_eq!(
            li.handle(key_mod(KeyCode::Char('t'), KeyModifiers::CONTROL), false),
            LocalAction::None
        );
        assert_eq!(li.handle(key(KeyCode::Char('q')), false), LocalAction::Quit);
    }

    #[test]
    fn ctrl_t_other_sends_literal_then_key() {
        let mut li = LocalInput::new(InputEncoder::default());
        li.handle(key_mod(KeyCode::Char('t'), KeyModifiers::CONTROL), false);
        assert_eq!(
            li.handle(key(KeyCode::Char('a')), false),
            LocalAction::Bytes(vec![0x14, b'a'])
        );
        // Escape state consumed — the next key is a normal key again.
        assert_eq!(
            li.handle(key(KeyCode::Char('b')), false),
            LocalAction::Bytes(vec![b'b'])
        );
    }

    #[test]
    fn release_events_are_ignored() {
        let mut li = LocalInput::new(InputEncoder::default());
        let mut ev = key(KeyCode::Char('a'));
        ev.kind = KeyEventKind::Release;
        assert_eq!(li.handle(ev, false), LocalAction::None);
    }
}

#[cfg(test)]
mod scroll_tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn shift(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::SHIFT)
    }

    #[test]
    fn scroll_keys_are_local_only() {
        let mut li = LocalInput::new(InputEncoder::default());
        assert_eq!(
            li.handle(shift(KeyCode::PageUp), false),
            LocalAction::Scroll(ScrollKey::PageUp)
        );
        assert_eq!(
            li.handle(shift(KeyCode::PageDown), false),
            LocalAction::Scroll(ScrollKey::PageDown)
        );
        assert_eq!(
            li.handle(shift(KeyCode::Home), false),
            LocalAction::Scroll(ScrollKey::Top)
        );
        assert_eq!(
            li.handle(shift(KeyCode::End), false),
            LocalAction::Scroll(ScrollKey::Bottom)
        );
    }

    #[test]
    fn unmodified_and_ctrl_shift_page_keys_still_reach_the_dut() {
        let mut li = LocalInput::new(InputEncoder::default());
        // Unmodified PageUp: serial sequence for the DUT.
        assert_eq!(
            li.handle(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE), false),
            LocalAction::Bytes(b"\x1b[5~".to_vec())
        );
        // Ctrl+Shift+PageUp is NOT the scroll chord — falls through to the
        // encoder (PageUp encoding, Ctrl ignored by the serial encoding).
        let mut out = Vec::new();
        InputEncoder::default().encode(
            KeyCode::PageUp,
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            false,
            &mut out,
        );
        assert_eq!(out, b"\x1b[5~");
    }

    #[test]
    fn scroll_key_wins_over_ctrl_t_escape_state() {
        let mut li = LocalInput::new(InputEncoder::default());
        // Enter the Ctrl-T prefix, then scroll: display navigation wins.
        li.handle(
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
            false,
        );
        assert_eq!(
            li.handle(shift(KeyCode::PageUp), false),
            LocalAction::Scroll(ScrollKey::PageUp)
        );
    }
}

#[cfg(test)]
mod wheel_tests {
    use super::*;

    #[test]
    fn wheel_notches_scroll_the_local_view() {
        assert_eq!(
            wheel_action(true, false),
            WheelAction::Scroll(crate::serial_term::ViewScroll::Delta(WHEEL_LINES))
        );
        assert_eq!(
            wheel_action(false, false),
            WheelAction::Scroll(crate::serial_term::ViewScroll::Delta(-WHEEL_LINES))
        );
    }

    #[test]
    fn wheel_drives_arrow_keys_inside_remote_alt_screen() {
        let up = wheel_action(true, true);
        let WheelAction::Send(bytes) = up else {
            panic!("expected Send");
        };
        assert_eq!(bytes, b"\x1b[A\x1b[A\x1b[A");
        let WheelAction::Send(bytes) = wheel_action(false, true) else {
            panic!("expected Send");
        };
        assert_eq!(bytes, b"\x1b[B\x1b[B\x1b[B");
    }

    #[test]
    fn ctrl_t_chord_pages_the_scrollback() {
        let mut li = LocalInput::new(InputEncoder::default());
        li.handle(
            crossterm::event::KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
            false,
        );
        assert_eq!(
            li.handle(
                crossterm::event::KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
                false
            ),
            LocalAction::Scroll(ScrollKey::PageUp)
        );
        li.handle(
            crossterm::event::KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
            false,
        );
        assert_eq!(
            li.handle(
                crossterm::event::KeyEvent::new(KeyCode::Home, KeyModifiers::NONE),
                false
            ),
            LocalAction::Scroll(ScrollKey::Top)
        );
        // Plain PageUp (no chord) still reaches the DUT.
        assert_eq!(
            li.handle(
                crossterm::event::KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
                false
            ),
            LocalAction::Bytes(b"\x1b[5~".to_vec())
        );
    }
}
