//! Transport-frame decoding for the interactive serial display path.
//!
//! `WireDecoder` turns raw transport bytes into clean serial payload:
//!
//! * telnet IAC (RFC 854/2217) protocol traffic is removed — ser2net's
//!   telnet/rfc2217 modes open with WILL/DO negotiations and
//!   subnegotiations that would otherwise render as garbage;
//! * the ser2net connect banner line is suppressed (field-observed: the
//!   banner must not display, and the blank CRLF lines around it must not
//!   pile up before the first prompt).
//!
//! It stops there: payload bytes — including every ANSI/VT escape — reach
//! the terminal core untouched. Banner suppression is display-path-only;
//! it never runs against a raw log and never touches TX.

use crate::telnet_filter::{TelnetDecoder, TelnetEvent};

/// Which transport-framing layers the wire decoder applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WireMode {
    /// Strip telnet IAC framing (direct ser2net TCP connections).
    pub telnet: bool,
    /// Suppress the ser2net connect banner (direct connects only — the
    /// MCP WebSocket relay already publishes clean payload).
    pub banner: bool,
}

impl WireMode {
    /// Direct ser2net TCP: both layers on.
    pub const DIRECT: WireMode = WireMode {
        telnet: true,
        banner: true,
    };

    /// MCP WebSocket relay: payload already framing-clean.
    pub const RELAY: WireMode = WireMode {
        telnet: false,
        banner: false,
    };
}

pub struct WireDecoder {
    telnet: Option<TelnetDecoder>,
    banner: Option<BannerSuppressor>,
}

impl WireDecoder {
    pub fn new(mode: WireMode) -> Self {
        Self {
            telnet: mode.telnet.then(TelnetDecoder::new),
            banner: mode.banner.then(BannerSuppressor::new),
        }
    }

    /// Decode one transport chunk; payload bytes are appended to `out`.
    pub fn decode(&mut self, input: &[u8], out: &mut Vec<u8>) {
        match (&mut self.telnet, &mut self.banner) {
            (Some(telnet), Some(banner)) => {
                for ev in telnet.decode(input) {
                    if let TelnetEvent::Payload(data) = ev {
                        banner.feed(&data, out);
                    }
                    // Will/Wont/Do/Dont/Subnegotiation: protocol noise for a
                    // passive passthrough client — dropped.
                }
            }
            (Some(telnet), None) => {
                for ev in telnet.decode(input) {
                    if let TelnetEvent::Payload(data) = ev {
                        out.extend_from_slice(&data);
                    }
                }
            }
            (None, Some(banner)) => banner.feed(input, out),
            (None, None) => out.extend_from_slice(input),
        }
    }
}

/// Ser2net banner suppressor.
///
/// Bytes are held only while the line-so-far is still a PREFIX of
/// "ser2net port" (after optional leading `\r`/`\n`); the first byte that
/// cannot extend the banner prefix releases everything held and switches
/// to permanent passthrough. An ESC byte can never extend the prefix, so
/// escape sequences always release — they are never held or reordered.
/// Once the full marker matched, the line is held until its newline and
/// dropped (a banner can never be a prefix of real content).
struct BannerSuppressor {
    line_buf: Vec<u8>,
    banner_passthrough: bool,
    /// Real content has been emitted — leading blank lines (the banner's
    /// surrounding CRLFs) are dropped until then.
    saw_content: bool,
}

impl BannerSuppressor {
    const MARKER: &'static [u8] = b"ser2net port";

    fn new() -> Self {
        Self {
            line_buf: Vec::new(),
            banner_passthrough: false,
            saw_content: false,
        }
    }

    fn feed(&mut self, data: &[u8], out: &mut Vec<u8>) {
        for &b in data {
            if self.banner_passthrough {
                out.push(b);
                continue;
            }
            self.line_buf.push(b);
            let content: &[u8] = {
                let mut i = 0;
                while i < self.line_buf.len()
                    && (self.line_buf[i] == b'\r' || self.line_buf[i] == b'\n')
                {
                    i += 1;
                }
                &self.line_buf[i..]
            };
            if b == b'\n' {
                let is_banner = content
                    .windows(Self::MARKER.len())
                    .any(|w| w == Self::MARKER);
                let is_blank = content
                    .iter()
                    .all(|&c| c == b'\r' || c == b'\n' || c == b' ');
                if !is_banner && !(is_blank && !self.saw_content) {
                    if !is_blank {
                        self.saw_content = true;
                    }
                    out.extend_from_slice(&self.line_buf);
                }
                self.line_buf.clear();
                continue;
            }
            let still_prefix =
                content.len() <= Self::MARKER.len() && Self::MARKER[..content.len()] == *content;
            if still_prefix || content.starts_with(Self::MARKER) {
                // Still plausibly the banner (or the banner itself growing).
                continue;
            }
            // Not a banner — release the held bytes and stop holding.
            self.saw_content = true;
            out.extend_from_slice(&self.line_buf);
            self.line_buf.clear();
            self.banner_passthrough = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(mode: WireMode, chunks: &[&[u8]]) -> Vec<u8> {
        let mut d = WireDecoder::new(mode);
        let mut out = Vec::new();
        for c in chunks {
            d.decode(c, &mut out);
        }
        out
    }

    #[test]
    fn banner_line_is_suppressed() {
        let out = decode(
            WireMode {
                telnet: false,
                banner: true,
            },
            &[b"ser2net port 41000\r\n\r\nroot@rk3576:/# "],
        );
        assert_eq!(out, b"root@rk3576:/# ");
    }

    #[test]
    fn banner_split_across_chunks_is_suppressed() {
        let out = decode(
            WireMode {
                telnet: false,
                banner: true,
            },
            &[b"ser2net ", b"port 41", b"000\r\n", b"\r\n# "],
        );
        assert_eq!(out, b"# ");
    }

    #[test]
    fn non_banner_first_line_passes_with_escapes() {
        // An escape sequence right at connect time must release the held
        // prefix in order — bracketed-paste marker then prompt.
        let out = decode(
            WireMode {
                telnet: false,
                banner: true,
            },
            &[b"\x1b[?2004hroot@board:/# "],
        );
        assert_eq!(out, b"\x1b[?2004hroot@board:/# ");
    }

    #[test]
    fn plain_passthrough_when_disabled() {
        assert_eq!(decode(WireMode::RELAY, &[b"abc"]), b"abc");
    }

    #[test]
    fn telnet_negotiation_is_stripped() {
        // IAC WILL COM-PORT-OPTION (0xFF 0xFB 0x2C) around payload.
        let out = decode(
            WireMode {
                telnet: true,
                banner: false,
            },
            &[b"\xff\xfb\x2cha\xff\xfa\x2c\x03\x00\x00\x1c\x20\xff\xf0i"],
        );
        assert_eq!(out, b"hai");
    }

    #[test]
    fn telnet_escaped_iac_becomes_data_ff() {
        let out = decode(
            WireMode {
                telnet: true,
                banner: false,
            },
            &[b"a\xff\xffb"],
        );
        assert_eq!(out, b"a\xffb");
    }

    #[test]
    fn telnet_and_banner_combined() {
        let out = decode(
            WireMode::DIRECT,
            &[b"\xff\xfb\x2cser2net port 7\r\nreal\r\n"],
        );
        assert_eq!(out, b"real\r\n");
    }
}
