//! Display-only CR/LF compatibility for the interactive serial screen.
//!
//! Raw terminal mode disables the host tty's ONLCR translation, but many
//! DUT consoles emit lone LF after CR. On a real VT a lone LF moves down
//! WITHOUT returning to column zero, so successive prompts would staircase
//! across the screen. `LfImpliesCr` restores the expected display behavior
//! by expanding a lone LF to CR LF — display path only: the raw log, the
//! transport RX stream and every TX byte are untouched.

/// How lone LF in the RX stream is interpreted for display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RxNewlineMode {
    /// Payload reaches the VT core verbatim (true VT semantics: a lone LF
    /// keeps the current column).
    Raw,
    /// A lone LF is displayed as CR LF. `\r\n` on the wire stays `\r\n` —
    /// the CR-insertion decision carries across chunk boundaries so a
    /// `\r` in one chunk and the `\n` in the next never becomes `\r\r\n`.
    #[default]
    LfImpliesCr,
}

impl RxNewlineMode {
    pub fn parse(v: &str) -> Option<Self> {
        match v.trim().to_ascii_lowercase().as_str() {
            "raw" => Some(Self::Raw),
            "lf-implies-cr" | "lf_implies_cr" | "lfimpliescr" => Some(Self::LfImpliesCr),
            _ => None,
        }
    }
}

/// Streaming newline normalizer — state must survive chunk boundaries.
#[derive(Debug)]
pub struct RxNewlineNormalizer {
    mode: RxNewlineMode,
    previous_was_cr: bool,
}

impl RxNewlineNormalizer {
    pub fn new(mode: RxNewlineMode) -> Self {
        Self {
            mode,
            previous_was_cr: false,
        }
    }

    pub fn mode(&self) -> RxNewlineMode {
        self.mode
    }

    /// Append the display-safe form of `input` to `output`.
    pub fn normalize(&mut self, input: &[u8], output: &mut Vec<u8>) {
        match self.mode {
            RxNewlineMode::Raw => output.extend_from_slice(input),
            RxNewlineMode::LfImpliesCr => {
                for &byte in input {
                    if byte == b'\n' && !self.previous_was_cr {
                        output.push(b'\r');
                    }
                    output.push(byte);
                    self.previous_was_cr = byte == b'\r';
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(mode: RxNewlineMode, chunks: &[&[u8]]) -> Vec<u8> {
        let mut n = RxNewlineNormalizer::new(mode);
        let mut out = Vec::new();
        for c in chunks {
            n.normalize(c, &mut out);
        }
        out
    }

    #[test]
    fn raw_leaves_every_byte_alone() {
        assert_eq!(norm(RxNewlineMode::Raw, &[b"a\nb"]), b"a\nb");
    }

    #[test]
    fn lone_lf_becomes_crlf() {
        assert_eq!(norm(RxNewlineMode::LfImpliesCr, &[b"a\nb"]), b"a\r\nb");
    }

    #[test]
    fn crlf_is_never_doubled() {
        assert_eq!(norm(RxNewlineMode::LfImpliesCr, &[b"a\r\nb"]), b"a\r\nb");
    }

    #[test]
    fn cr_only_passes_through() {
        assert_eq!(norm(RxNewlineMode::LfImpliesCr, &[b"a\rb"]), b"a\rb");
    }

    #[test]
    fn chunk_split_at_cr_lf_boundary() {
        // "\r" in one chunk, "\n" in the next: the CR state must carry over
        // so the LF does not get a second CR inserted.
        assert_eq!(
            norm(RxNewlineMode::LfImpliesCr, &[b"a\r", b"\nb"]),
            b"a\r\nb"
        );
        // Consecutive LFs each expand.
        assert_eq!(
            norm(RxNewlineMode::LfImpliesCr, &[b"a\n", b"\nb"]),
            b"a\r\n\r\nb"
        );
    }
}
