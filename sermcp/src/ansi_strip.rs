//! ANSI escape-sequence stripping for serial/boot-log output.
//!
//! This is a direct replacement for the now-unmaintained `strip-ansi-escapes`
//! crate (repo dormant for years) — which was itself a thin
//! wrapper over `vte`. Depending on `vte` (maintained by the Alacritty
//! project) directly keeps identical semantics with one actively-maintained
//! dependency instead of two, one of them abandoned.
//!
//! Semantics (mirrored from `strip_ansi_escapes::strip`): printable characters
//! pass through as UTF-8; every C0/C1 control byte is dropped except LF;
//! CSI/OSC/DCS/ESC sequences are removed entirely.

use vte::{Parser, Perform};

/// Strip ANSI escape sequences from `bytes`; only printable text and
/// newlines survive.
pub fn strip(bytes: &[u8]) -> Vec<u8> {
    strip_with_offsets(bytes).0
}

/// [`strip`] keeping a byte-offset map of every surviving byte back to its
/// original position: `offsets[i]` is the index in `bytes` that produced the
/// i-th output byte (the k-th byte of a multi-byte UTF-8 character maps to
/// the character's k-th input byte — byte-precise correspondence).
///
/// Use this when stripped text must be re-correlated with the original
/// stream (e.g. the prompt highlighter inserts SGR codes around spans of the
/// untouched original). Regex-derived clean-text ranges align on char
/// boundaries, so mapping a range boundary back to the original never lands
/// mid-character.
pub fn strip_with_offsets(bytes: &[u8]) -> (Vec<u8>, Vec<usize>) {
    struct Performer<'a> {
        out: &'a mut Vec<u8>,
        offsets: &'a mut Vec<usize>,
        // Input index of the byte currently being fed to the parser.
        cur: usize,
    }

    impl Perform for Performer<'_> {
        fn print(&mut self, c: char) {
            // print fires once the character's final byte has been fed, so
            // the character starts at cur+1-len_utf8. Malformed input can
            // decode to a replacement char wider than the bytes consumed —
            // saturate rather than underflow.
            let start = (self.cur + 1).saturating_sub(c.len_utf8());
            let mut buf = [0u8; 4];
            let encoded = c.encode_utf8(&mut buf);
            for (i, b) in encoded.bytes().enumerate() {
                self.out.push(b);
                self.offsets.push(start + i);
            }
        }

        fn execute(&mut self, byte: u8) {
            // Only newlines are kept; every other C0/C1 control is dropped.
            if byte == b'\n' {
                self.out.push(byte);
                self.offsets.push(self.cur);
            }
        }
    }

    let mut out = Vec::with_capacity(bytes.len());
    let mut offsets = Vec::with_capacity(bytes.len());
    let mut parser = Parser::new();
    for (i, b) in bytes.iter().enumerate() {
        let mut performer = Performer {
            out: &mut out,
            offsets: &mut offsets,
            cur: i,
        };
        parser.advance(&mut performer, std::slice::from_ref(b));
    }
    (out, offsets)
}

/// Strip ANSI escape sequences from a `&str`, keeping every other
/// character (including C0 controls such as `\r` — unlike [`strip`],
/// which drops all controls except LF).
///
/// The canonical CSI/ESC consumer for line-oriented text (boot-detector
/// input, connection-learner normalization, command-queue echo stripping);
/// previously three byte-identical hand-rolled copies lived in those
/// modules.
pub fn strip_str(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Consume CSI sequence: ESC [ ... <letter>  or  ESC [ ... ~
            if let Some(&'[') = chars.peek() {
                chars.next();
                while let Some(&nc) = chars.peek() {
                    if nc.is_ascii_alphabetic() || nc == '~' {
                        chars.next();
                        break;
                    }
                    chars.next();
                }
                continue;
            }
            // Consume lone ESC + following char
            chars.next();
            continue;
        }
        result.push(c);
    }
    result
}

/// `strip_str` on raw bytes (lossy UTF-8). Byte-level callers that must
/// keep `\r` and other controls use this, not [`strip`].
pub fn strip_bytes_lossy(data: &[u8]) -> Vec<u8> {
    strip_str(&String::from_utf8_lossy(data)).into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_parsed(input: &[u8], expected: &[u8]) {
        assert_eq!(strip(input), expected);
    }

    #[test]
    fn simple_sgr_sequences() {
        assert_parsed(
            b"\x1b[m\x1b[m\x1b[32m\x1b[1m    Finished\x1b[m dev [unoptimized + debuginfo] target(s) in 0.0 secs",
            b"    Finished dev [unoptimized + debuginfo] target(s) in 0.0 secs",
        );
    }

    #[test]
    fn plain_newlines_survive() {
        assert_parsed(b"foo\nbar\n", b"foo\nbar\n");
    }

    #[test]
    fn escapes_between_cargo_lines() {
        assert_parsed(
            b"\x1b[m\x1b[m\x1b[32m\x1b[1m   Compiling\x1b[m utf8parse v0.1.0
\x1b[m\x1b[m\x1b[32m\x1b[1m   Compiling\x1b[m vte v0.3.2
\x1b[m\x1b[m\x1b[32m\x1b[1m    Finished\x1b[m dev [unoptimized + debuginfo] target(s) in 0.66 secs
",
            b"   Compiling utf8parse v0.1.0
   Compiling vte v0.3.2
    Finished dev [unoptimized + debuginfo] target(s) in 0.66 secs
",
        );
    }

    #[test]
    fn osc_and_esc_sequences_removed() {
        assert_parsed(b"a\x1b]0;title\x07b\x1b(Bc", b"abc");
    }

    #[test]
    fn c1_control_bytes_dropped() {
        // 8-bit C1 controls (0x80-0x9F, e.g. 0x9B single-byte CSI and 0x8D
        // reverse index) are dispatched to `execute` and dropped, matching
        // the behaviour the strip-ansi-escapes wrapper relied on.
        assert_parsed(b"a\x8db", b"ab");
        assert_parsed(b"a\x9bb", b"ab");
    }

    #[test]
    fn control_bytes_dropped_except_lf() {
        // BEL, TAB and CR are C0 controls — dropped; only LF survives.
        assert_parsed(b"a\x07b\tc\rd\ne", b"abcd\ne");
        assert_parsed(b"a\rb\n", b"ab\n");
    }

    #[test]
    fn multibyte_utf8_passthrough() {
        assert_parsed(
            "boot \x1b[31m启动\x1b[m ok".as_bytes(),
            "boot 启动 ok".as_bytes(),
        );
    }

    #[test]
    fn truncated_escape_dropped() {
        // Unterminated CSI at end of buffer must not leak.
        assert_parsed(b"foo\x1b[31", b"foo");
    }

    #[test]
    fn offsets_map_clean_bytes_back_to_origins() {
        // ESC[?2004h = 8 bytes, then "ab", then 启 (3 bytes), then \n.
        let (clean, offsets) = strip_with_offsets(b"\x1b[?2004hab\xe5\x90\xaf\n");
        assert_eq!(clean, b"ab\xe5\x90\xaf\n");
        assert_eq!(offsets[0], 8, "'a' origin");
        assert_eq!(offsets[1], 9, "'b' origin");
        assert_eq!(offsets[2], 10, "first UTF-8 byte of 启");
        assert_eq!(offsets[3], 11, "second UTF-8 byte of 启");
        assert_eq!(offsets[4], 12, "third UTF-8 byte of 启");
        assert_eq!(offsets[5], 13, "newline origin");
        // round-trip: every clean byte equals its original byte
        for (i, &b) in clean.iter().enumerate() {
            assert_eq!(b, b"\x1b[?2004hab\xe5\x90\xaf\n"[offsets[i]]);
        }
    }

    #[test]
    fn offsets_survive_malformed_leading_utf8() {
        // A stray continuation byte is dropped outright (no U+FFFD) — must
        // not underflow nor desynchronize the offset map.
        let (clean, offsets) = strip_with_offsets(&[0x80, b'x']);
        assert_eq!(clean, b"x");
        assert_eq!(offsets, vec![1]);
    }
}
