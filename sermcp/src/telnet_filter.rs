//! Minimal RFC 854/2217 IAC filter — design ported from esp-rs/rfc2217-rs
//! (Port negotiation enum, byte-slicing decode) and pyserial serial/rfc2217.py
//! (M_NORMAL/M_NEGOTIATE/M_SB). No I/O: feed raw TCP bytes in, get events out.
//!
//! Why hand-written instead of a crate: `rfc2217-rs` is
//! sync, bound to a local serialport, and long unmaintained;
//! `telnet-rs` is sync with no COM-PORT-OPTION (RFC 2217) semantics at all;
//! no maintained tokio-native RFC 2217 crate exists on crates.io. The
//! required subset here is tiny (negotiate option 44, one SET-BAUDRATE
//! subneg, ack parsing, stuff/unstuff), so a pure state machine that slots
//! into console.rs's existing byte loop is smaller and testable without
//! sockets — no new dependencies.

/// Decoder state (RFC 854 receiver automaton).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TelnetState {
    #[default]
    Normal,
    Iac,
    Negotiate,
    Subnegotiation,
}

/// One decoded unit. Payload is the serial byte stream for the engine;
/// the rest are telnet-protocol events the driver consumes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TelnetEvent {
    Payload(Vec<u8>),
    Will(u8),
    Wont(u8),
    Do(u8),
    Dont(u8),
    Subnegotiation {
        option: u8,
        data: Vec<u8>,
    },
    /// Emitted once when WILL(44) or DO(44) is seen in either direction.
    PortOptionActive,
}

pub struct TelnetDecoder {
    state: TelnetState,
    subneg_buf: Vec<u8>, // cap MAX_SUBNEG (4096); overflow => reset()
    pub port_option_active: bool,
    /// WILL/WONT/DO/DONT byte remembered across chunk boundaries while in
    /// the Negotiate state (the option byte may arrive in a later chunk).
    negotiate_cmd: u8,
    /// Option byte captured right after SB (IAC 0xFA).
    subneg_option: Option<u8>,
    /// True while inside a subnegotiation and the previous byte was an
    /// IAC escape (0xFF). Disambiguates IAC-in-Normal from IAC-in-Subneg.
    subneg_escaped: bool,
}

impl Default for TelnetDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl TelnetDecoder {
    pub const MAX_SUBNEG: usize = 4096;

    pub fn new() -> Self {
        Self {
            state: TelnetState::Normal,
            subneg_buf: Vec::new(),
            port_option_active: false,
            negotiate_cmd: 0,
            subneg_option: None,
            subneg_escaped: false,
        }
    }

    /// Feed raw TCP bytes; returns events in order. A run of plain bytes is
    /// coalesced into one Payload event. 0xFF -> Iac; Iac: 0xFF => Payload[0xFF]
    /// back to Normal, 0xFA => Subnegotiation, 0xFB|0xFC|0xFD|0xFE => Negotiate
    /// (cmd remembered), 0xF0 => ignored, any other single-byte command
    /// (NOP/DM/BRK/IP/AO/AYT/EC/EL/GA) => ignored, back to Normal.
    /// Negotiate: consume option byte => emit Will/Wont/Do/Dont; if option==44
    ///   and cmd is WILL or DO => port_option_active = true + emit PortOptionActive.
    /// Subnegotiation: 0xFF => IAC-escape (next byte: 0xF0 => emit
    ///   Subnegotiation + Normal; 0xFF => push 0xFF; else protocol error => reset());
    ///   other bytes pushed to subneg_buf; > MAX_SUBNEG => reset() (no SE seen).
    pub fn decode(&mut self, bytes: &[u8]) -> Vec<TelnetEvent> {
        let mut events = Vec::new();
        let mut payload: Vec<u8> = Vec::new();
        let was_active = self.port_option_active;

        for &b in bytes {
            match self.state {
                TelnetState::Normal => {
                    if b == iac::IAC {
                        if !payload.is_empty() {
                            events.push(TelnetEvent::Payload(std::mem::take(&mut payload)));
                        }
                        self.state = TelnetState::Iac;
                    } else {
                        payload.push(b);
                    }
                }
                TelnetState::Iac => {
                    if self.subneg_escaped {
                        match b {
                            iac::SE => {
                                if let Some(option) = self.subneg_option.take() {
                                    events.push(TelnetEvent::Subnegotiation {
                                        option,
                                        data: std::mem::take(&mut self.subneg_buf),
                                    });
                                }
                                self.subneg_escaped = false;
                                self.state = TelnetState::Normal;
                            }
                            iac::IAC => {
                                self.subneg_buf.push(iac::IAC);
                                if self.subneg_buf.len() > Self::MAX_SUBNEG {
                                    self.reset();
                                } else {
                                    self.state = TelnetState::Subnegotiation;
                                }
                                self.subneg_escaped = false;
                            }
                            _ => {
                                // Protocol error: escape not followed by SE/IAC.
                                self.reset();
                            }
                        }
                    } else {
                        match b {
                            iac::IAC => {
                                payload.push(iac::IAC);
                                self.state = TelnetState::Normal;
                            }
                            iac::SB => {
                                self.subneg_buf.clear();
                                self.subneg_option = None;
                                self.state = TelnetState::Subnegotiation;
                            }
                            iac::WILL | iac::WONT | iac::DO | iac::DONT => {
                                self.negotiate_cmd = b;
                                self.state = TelnetState::Negotiate;
                            }
                            iac::SE => {
                                // Stray SE outside subnegotiation — tolerated.
                                self.state = TelnetState::Normal;
                            }
                            _ => {
                                // Single-byte commands (NOP/DM/BRK/IP/AO/AYT/
                                // EC/EL/GA) — ignored.
                                self.state = TelnetState::Normal;
                            }
                        }
                    }
                }
                TelnetState::Negotiate => {
                    let cmd = self.negotiate_cmd;
                    self.state = TelnetState::Normal;
                    match cmd {
                        iac::WILL => events.push(TelnetEvent::Will(b)),
                        iac::WONT => events.push(TelnetEvent::Wont(b)),
                        iac::DO => events.push(TelnetEvent::Do(b)),
                        iac::DONT => events.push(TelnetEvent::Dont(b)),
                        _ => {}
                    }
                    if b == iac::COM_PORT_OPTION && (cmd == iac::WILL || cmd == iac::DO) {
                        self.port_option_active = true;
                    }
                }
                TelnetState::Subnegotiation => {
                    if self.subneg_option.is_none() {
                        // First byte after SB is the option.
                        self.subneg_option = Some(b);
                    } else if b == iac::IAC {
                        self.subneg_escaped = true;
                        self.state = TelnetState::Iac;
                    } else {
                        self.subneg_buf.push(b);
                        if self.subneg_buf.len() > Self::MAX_SUBNEG {
                            self.reset();
                        }
                    }
                }
            }
        }

        if !payload.is_empty() {
            events.push(TelnetEvent::Payload(payload));
        }
        if self.port_option_active && !was_active {
            events.push(TelnetEvent::PortOptionActive);
        }
        events
    }

    /// Back to Normal, clear buffer and port_option_active. Call on every (re)connect.
    pub fn reset(&mut self) {
        self.state = TelnetState::Normal;
        self.subneg_buf.clear();
        self.port_option_active = false;
        self.negotiate_cmd = 0;
        self.subneg_option = None;
        self.subneg_escaped = false;
    }
}

// ── encode helpers ────────────────────────────────────────────────

/// IAC-escape 0xFF -> FF FF. Use on ALL outgoing data AFTER negotiation.
pub fn stuff(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    for &b in bytes {
        if b == iac::IAC {
            out.push(iac::IAC);
        }
        out.push(b);
    }
    out
}

/// Build `FF FA <option> <cmd> <data..> FF F0`.
pub fn encode_subneg(option: u8, cmd: u8, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 5);
    out.push(iac::IAC);
    out.push(iac::SB);
    out.push(option);
    out.push(cmd);
    out.extend_from_slice(data);
    out.push(iac::IAC);
    out.push(iac::SE);
    out
}

/// Split subneg payload into (cmd, rest). None on empty.
pub fn parse_rfc2217_subneg(data: &[u8]) -> Option<(u8, Vec<u8>)> {
    data.split_first().map(|(&cmd, rest)| (cmd, rest.to_vec()))
}

// ── RFC 2217 command builders (COM-PORT-OPTION = 44) ─────────────

/// cmd 1, 4-byte big-endian baud rate.
pub fn rfc2217_set_baudrate(baud: u32) -> Vec<u8> {
    encode_subneg(
        iac::COM_PORT_OPTION,
        iac::CMD_SET_BAUDRATE,
        &baud.to_be_bytes(),
    )
}

// ── constants ─────────────────────────────────────────────────────

pub mod iac {
    pub const IAC: u8 = 0xFF;
    pub const SB: u8 = 0xFA;
    pub const SE: u8 = 0xF0;
    pub const WILL: u8 = 0xFB;
    pub const WONT: u8 = 0xFC;
    pub const DO: u8 = 0xFD;
    pub const DONT: u8 = 0xFE;
    pub const COM_PORT_OPTION: u8 = 44;
    pub const CMD_SET_BAUDRATE: u8 = 1;
    pub const CMD_NOTIFY_BAUDRATE: u8 = 101;
    pub const CMD_FLOWCONTROL_SUSPEND: u8 = 108;
    pub const CMD_FLOWCONTROL_RESUME: u8 = 109;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_passthrough() {
        let mut d = TelnetDecoder::new();
        let events = d.decode(b"hello");
        assert_eq!(events, vec![TelnetEvent::Payload(b"hello".to_vec())]);
        assert_eq!(d.state, TelnetState::Normal);
    }

    #[test]
    fn test_decode_iac_iac_unescapes_ff() {
        let mut d = TelnetDecoder::new();
        let events = d.decode(&[0xFF, 0xFF, b'a']);
        assert_eq!(
            events,
            vec![TelnetEvent::Payload(vec![0xFF, b'a'])],
            "IAC IAC must decode to a literal 0xFF byte"
        );
        assert_eq!(d.state, TelnetState::Normal);
    }

    #[test]
    fn test_negotiation_do_will_completes() {
        let mut d = TelnetDecoder::new();
        let events = d.decode(&[0xFF, 0xFD, 0x2C, 0xFF, 0xFB, 0x2C]);
        assert_eq!(
            events,
            vec![
                TelnetEvent::Do(44),
                TelnetEvent::Will(44),
                TelnetEvent::PortOptionActive,
            ]
        );
        assert!(d.port_option_active);
        assert_eq!(d.state, TelnetState::Normal);
    }

    #[test]
    fn test_subneg_set_baudrate_roundtrip() {
        let mut d = TelnetDecoder::new();
        let events = d.decode(&rfc2217_set_baudrate(1500000));
        assert_eq!(
            events,
            vec![TelnetEvent::Subnegotiation {
                option: 44,
                data: vec![1, 0x00, 0x16, 0xE3, 0x60],
            }]
        );
        let (cmd, rest) = parse_rfc2217_subneg(&[1, 0x00, 0x16, 0xE3, 0x60]).unwrap();
        assert_eq!(cmd, iac::CMD_SET_BAUDRATE);
        assert_eq!(
            u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]),
            1500000
        );
    }

    #[test]
    fn test_subneg_embedded_ff_is_literal() {
        let mut d = TelnetDecoder::new();
        let wire = [0xFF, 0xFA, 0x2C, 0x05, 0xFF, 0xFF, 0x07, 0xFF, 0xF0];
        let events = d.decode(&wire);
        assert_eq!(
            events,
            vec![TelnetEvent::Subnegotiation {
                option: 44,
                data: vec![5, 0xFF, 7],
            }],
            "FF FF inside subnegotiation is a literal 0xFF, not the terminator"
        );
        assert_eq!(d.state, TelnetState::Normal);
    }

    #[test]
    fn test_stray_se_is_tolerated() {
        let mut d = TelnetDecoder::new();
        let events = d.decode(&[0xFF, 0xF0]);
        assert!(events.is_empty(), "stray SE must not emit events");
        assert_eq!(d.state, TelnetState::Normal);
        assert!(!d.port_option_active);
    }

    #[test]
    fn test_oversized_subneg_resets() {
        let mut d = TelnetDecoder::new();
        let mut wire = vec![0xFF, 0xFA, 0x2C];
        wire.extend(std::iter::repeat_n(b'X', 5000));
        let _ = d.decode(&wire);
        assert_eq!(
            d.state,
            TelnetState::Normal,
            "overflow must reset to Normal"
        );
        assert!(d.subneg_buf.is_empty());
        // Subsequent payload decodes cleanly.
        let events = d.decode(b"hello");
        assert_eq!(events, vec![TelnetEvent::Payload(b"hello".to_vec())]);
    }

    #[test]
    fn test_stuff_doubles_ff_only() {
        assert_eq!(
            stuff(&[0xFF, 0x01, 0xFF]),
            vec![0xFF, 0xFF, 0x01, 0xFF, 0xFF]
        );
        assert_eq!(stuff(b"abc"), b"abc");
    }

    #[test]
    fn test_negotiation_split_across_chunks() {
        // Chunk boundary between IAC WILL and its option byte.
        let mut d = TelnetDecoder::new();
        let e1 = d.decode(&[0xFF, 0xFB]);
        assert!(e1.is_empty());
        assert_eq!(d.state, TelnetState::Negotiate);
        let e2 = d.decode(&[0x2C]);
        assert_eq!(
            e2,
            vec![TelnetEvent::Will(44), TelnetEvent::PortOptionActive]
        );
        assert_eq!(d.state, TelnetState::Normal);
    }

    #[test]
    fn test_encode_subneg_shape() {
        assert_eq!(
            encode_subneg(44, 1, &[0x00, 0x16, 0xE3, 0x60]),
            vec![0xFF, 0xFA, 0x2C, 0x01, 0x00, 0x16, 0xE3, 0x60, 0xFF, 0xF0]
        );
        assert_eq!(parse_rfc2217_subneg(&[]), None);
    }
}
