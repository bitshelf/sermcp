//! Shell prompt syntax highlighting for serial console output.
//!
//! Detects Linux shell prompts (bash, ash, zsh) in serial data and applies
//! ANSI SGR color codes for readability:
//!   - `user@host` → green (bold)
//!   - `/path`       → blue (bold)
//!   - `$` / `#`     → white (bold)
//!
//! Highlighting is a pure DISPLAY OVERLAY: color codes are inserted around
//! spans of the original byte stream and nothing else is altered — escapes
//! the target itself sent (bracketed-paste markers, its own PS1 colors)
//! pass through verbatim. It must never feed the DUT, the logs, or any
//! consumer of the text content.
//!
//! Supports two common prompt formats:
//!   - `user@host:/path$ `       (colon-separated)
//!   - `[user@host /path]# `     (bracketed)

use std::ops::Range;
use std::sync::LazyLock;

// ── ANSI codes ──────────────────────────────────────────────────────────────

const GREEN_BOLD: &[u8] = b"\x1b[1;32m";
const BLUE_BOLD: &[u8] = b"\x1b[1;34m";
const WHITE_BOLD: &[u8] = b"\x1b[1;37m";
const RESET: &[u8] = b"\x1b[0m";

// ── Public API ──────────────────────────────────────────────────────────────

/// Apply shell prompt syntax highlighting to serial output data.
///
/// Each line is examined; if it matches a known prompt pattern, ANSI colors are
/// injected. Non-prompt lines pass through unchanged.
pub fn highlight_serial_prompt(data: &[u8], out: &mut Vec<u8>) {
    let mut start = 0;
    while start < data.len() {
        let end = data[start..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|pos| start + pos + 1)
            .unwrap_or(data.len());
        highlight_serial_prompt_line(&data[start..end], out);
        start = end;
    }
}

/// Prompt parts of a CLEAN (ANSI-free) line as `(user, dir, sigil)` byte
/// ranges. Public entry for the interactive terminal's semantic overlay
/// (`serial_term::highlight`), which matches against VT-core cell text —
/// inherently ANSI-free — instead of injecting SGR into the byte stream.
///
/// The former stateful `PromptHighlighter` (ANSI injection into the live
/// stream, with a DEBOUNCE hold and quiet-flush) was removed: the
/// interactive serial path renders through the VT core and retroactively
/// recolors via overlay damage, so nothing streams SGR into serial bytes
/// anymore.
pub fn prompt_line_spans(clean: &[u8]) -> Option<(Range<usize>, Range<usize>, Range<usize>)> {
    let parts = prompt_parts_from(clean)?;
    Some((parts.user, parts.dir, parts.sigil))
}

// ── Internal types ──────────────────────────────────────────────────────────

/// Byte ranges of the prompt parts, in ANSI-stripped (clean) coordinates.
struct PromptParts {
    user: Range<usize>,
    dir: Range<usize>,
    sigil: Range<usize>,
}

// ── Per-line highlighting ───────────────────────────────────────────────────

fn highlight_serial_prompt_line(line: &[u8], out: &mut Vec<u8>) {
    let (clean, offsets) = crate::ansi_strip::strip_with_offsets(line);
    let Some(parts) = prompt_parts_from(&clean) else {
        out.extend_from_slice(line);
        return;
    };
    // Map the clean-text match ranges back to spans of the ORIGINAL line and
    // insert SGR codes around them. Every original byte — including escapes
    // the target sent (bracketed-paste markers, its own PS1 colors) — passes
    // through verbatim; the line is never rebuilt from stripped text.
    let (Some(user), Some(dir), Some(sigil)) = (
        original_span(&offsets, &parts.user),
        original_span(&offsets, &parts.dir),
        original_span(&offsets, &parts.sigil),
    ) else {
        out.extend_from_slice(line);
        return;
    };
    out.extend_from_slice(&line[..user.start]);
    out.extend_from_slice(GREEN_BOLD);
    out.extend_from_slice(&line[user.clone()]);
    out.extend_from_slice(RESET);
    out.extend_from_slice(&line[user.end..dir.start]);
    out.extend_from_slice(BLUE_BOLD);
    out.extend_from_slice(&line[dir.clone()]);
    out.extend_from_slice(RESET);
    out.extend_from_slice(&line[dir.end..sigil.start]);
    out.extend_from_slice(WHITE_BOLD);
    out.extend_from_slice(&line[sigil.clone()]);
    out.extend_from_slice(RESET);
    out.extend_from_slice(&line[sigil.end..]);
}

/// Map a clean-text range to the original byte span it came from.
/// Empty or out-of-bounds ranges map to None (caller passes through raw).
fn original_span(offsets: &[usize], range: &Range<usize>) -> Option<Range<usize>> {
    if range.is_empty() || range.end > offsets.len() {
        return None;
    }
    Some(offsets[range.start]..offsets[range.end - 1] + 1)
}

// ── Prompt detection ────────────────────────────────────────────────────────

fn prompt_parts_from(clean: &[u8]) -> Option<PromptParts> {
    if clean.is_empty() {
        return None;
    }
    let end = clean
        .iter()
        .position(|&b| b == b'\r' || b == b'\n')
        .unwrap_or(clean.len());
    let text = std::str::from_utf8(&clean[..end]).ok()?;

    // Match prompt pattern on clean text
    parse_colon_prompt(text, clean).or_else(|| parse_bracket_prompt(text, clean))
}

// ── Colon format: `user@host:/path$ ` ───────────────────────────────────────

fn parse_colon_prompt(text: &str, clean: &[u8]) -> Option<PromptParts> {
    static COLON_USER_RE: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
        fancy_regex::Regex::new(r"^[^/#\-\s]\S*\s?[^\s.:\[\]]*?(?=:\S[^\r\n$#]*[$#]\s)").unwrap()
    });
    static COLON_DIR_RE: LazyLock<fancy_regex::Regex> =
        LazyLock::new(|| fancy_regex::Regex::new(r"(?<=:)\S[^\r\n$#]*?(?=[$#]\s)").unwrap());
    static COLON_SIGIL_RE: LazyLock<fancy_regex::Regex> =
        LazyLock::new(|| fancy_regex::Regex::new(r"(?<=\S)[$#]\s").unwrap());
    let user = match_range(&COLON_USER_RE, text)?;
    let dir = match_range(&COLON_DIR_RE, text)?;
    let sigil = match_range(&COLON_SIGIL_RE, text)?;
    // Greedy \S* backtracking in COLON_USER_RE can end the user match after
    // COLON_DIR_RE's leftmost dir start (e.g. "12:30:45# cmd"), and an
    // earlier sigil can end before dir.end (e.g. "a$ x:y# ") — either would
    // make the separator slice reversed or the content slices out of range.
    if user.end > dir.start || dir.end > sigil.end {
        return None;
    }
    let separator = user.end..dir.start;
    if &clean[separator] != b":" {
        return None;
    }
    if !valid_prompt_user(&clean[user.clone()]) || !valid_prompt_dir(&clean[dir.clone()]) {
        return None;
    }
    Some(PromptParts { user, dir, sigil })
}

// ── Bracket format: `[user@host /path]# ` ───────────────────────────────────

fn parse_bracket_prompt(text: &str, clean: &[u8]) -> Option<PromptParts> {
    static BRACKET_USER_RE: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
        fancy_regex::Regex::new(r"(?<=^\[)[^/#\-\s]\S*\s?[^\s.\[\]]+?(?=\s+\S[^\[\]\r\n]*\][$#]\s)")
            .unwrap()
    });
    static BRACKET_DIR_RE: LazyLock<fancy_regex::Regex> =
        LazyLock::new(|| fancy_regex::Regex::new(r"(?<=\s)\S[^\]\r\n]*?(?=\][$#]\s)").unwrap());
    static BRACKET_SIGIL_RE: LazyLock<fancy_regex::Regex> =
        LazyLock::new(|| fancy_regex::Regex::new(r"(?<=\])[$#]\s").unwrap());
    let user = match_range(&BRACKET_USER_RE, text)?;
    let dir = match_range(&BRACKET_DIR_RE, text)?;
    let sigil = match_range(&BRACKET_SIGIL_RE, text)?;
    if text.as_bytes().first() != Some(&b'[') || dir.end >= sigil.start {
        return None;
    }
    // Greedy \s? in BRACKET_USER_RE can swallow the user/dir separator so
    // user.end lands after BRACKET_DIR_RE's leftmost dir start (e.g.
    // "[root@host /a /b]# cmd"), reversing the separator slice below.
    if user.end > dir.start {
        return None;
    }
    let separator = user.end..dir.start;
    let suffix = dir.end..sigil.start;
    if !clean[separator].iter().all(|b| b.is_ascii_whitespace()) || &clean[suffix] != b"]" {
        return None;
    }
    if !valid_prompt_user(&clean[user.clone()]) || !valid_prompt_dir(&clean[dir.clone()]) {
        return None;
    }
    Some(PromptParts { user, dir, sigil })
}

// ── Regex helper ────────────────────────────────────────────────────────────

fn match_range(re: &fancy_regex::Regex, text: &str) -> Option<Range<usize>> {
    re.find(text).ok().flatten().map(|m| m.start()..m.end())
}

// ── Validation ──────────────────────────────────────────────────────────────

fn valid_prompt_user(user: &[u8]) -> bool {
    !user.is_empty()
        && !matches!(user[0], b'/' | b'#' | b'-' | b' ' | b'\t')
        && user
            .iter()
            .all(|b| !b.is_ascii_control() && !matches!(*b, b':' | b'[' | b']' | b'/' | b'#'))
        && user.iter().any(|b| b.is_ascii_alphanumeric())
}

fn valid_prompt_dir(dir: &[u8]) -> bool {
    !dir.is_empty()
        && !dir[0].is_ascii_whitespace()
        && dir
            .iter()
            .all(|b| !b.is_ascii_control() && !matches!(*b, b'[' | b']'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_highlight_colors_user_dir_and_sigil() {
        let mut out = Vec::new();
        highlight_serial_prompt(b"root@dut1:/# ip -c a\r\n", &mut out);
        let rendered = String::from_utf8(out).unwrap();
        assert!(rendered.contains("\x1b[1;32mroot@dut1\x1b[0m:"));
        assert!(rendered.contains("\x1b[1;34m/\x1b[0m"));
        assert!(rendered.contains("\x1b[1;37m# \x1b[0mip -c a"));
    }

    #[test]
    fn prompt_highlight_handles_bracket_prompt() {
        let mut out = Vec::new();
        highlight_serial_prompt(b"[root@dut1 ~]# pwd\r\n", &mut out);
        let rendered = String::from_utf8(out).unwrap();
        assert!(rendered.contains("[\x1b[1;32mroot@dut1\x1b[0m "));
        assert!(rendered.contains("\x1b[1;34m~\x1b[0m]"));
        assert!(rendered.contains("\x1b[1;37m# \x1b[0mpwd"));
    }

    #[test]
    fn prompt_highlight_short_colon_prompt() {
        // Regression: user@hostname directly followed by :/ without extra chars
        // e.g. root@board-name:/#
        let mut out = Vec::new();
        highlight_serial_prompt(b"root@board-name:/# ls\r\n", &mut out);
        let rendered = String::from_utf8(out).unwrap();
        assert!(rendered.contains("\x1b[1;32mroot@board-name\x1b[0m:"));
        assert!(rendered.contains("\x1b[1;34m/\x1b[0m"));
        assert!(rendered.contains("\x1b[1;37m# \x1b[0mls"));
    }

    #[test]
    fn prompt_highlight_bracketed_paste_prefix() {
        // real-world: \x1b[?2004hroot@board:/#
        // Leading ANSI escapes preserved verbatim — overlay never rewrites
        let mut out = Vec::new();
        highlight_serial_prompt(b"\x1b[?2004hroot@board:/# ls\r\n", &mut out);
        let rendered = String::from_utf8(out).unwrap();
        assert!(rendered.contains("\x1b[?2004h\x1b[1;32mroot@board\x1b[0m:"));
        assert!(rendered.contains("\x1b[1;34m/\x1b[0m"));
        assert!(rendered.contains("\x1b[1;37m# \x1b[0mls"));
    }

    /// Highlighting is a pure display overlay: SGR escapes the TARGET sent
    /// inside the prompt survive verbatim (the line is never rebuilt from
    /// ANSI-stripped text), with our color codes inserted around the spans.
    #[test]
    fn target_sgr_inside_prompt_is_preserved() {
        let mut out = Vec::new();
        highlight_serial_prompt(b"\x1b[32mroot@dut1\x1b[0m:/# ls\r\n", &mut out);
        let rendered = String::from_utf8(out).unwrap();
        assert!(
            rendered.contains("\x1b[32m\x1b[1;32mroot@dut1\x1b[0m\x1b[0m:"),
            "original target SGR survives inside the colored span: {rendered:?}"
        );
        assert!(rendered.contains("\x1b[1;37m# \x1b[0mls"));
        // No byte of the original line is lost: stripping both streams'
        // escapes yields identical text.
        let stripped = crate::ansi_strip::strip(rendered.as_bytes());
        assert_eq!(
            stripped,
            crate::ansi_strip::strip(b"\x1b[32mroot@dut1\x1b[0m:/# ls\r\n")
        );
    }

    /// Overlay must not disturb escapes in the trailing command text either.
    #[test]
    fn target_sgr_in_tail_is_preserved() {
        let mut out = Vec::new();
        highlight_serial_prompt(b"root@dut1:/# ip -c a\x1b[m\r\n", &mut out);
        let rendered = String::from_utf8(out).unwrap();
        assert!(rendered.contains("\x1b[1;37m# \x1b[0mip -c a\x1b[m"));
    }

    /// Regression (field-observed): a prompt arriving SPLIT across
    /// chunk boundaries — e.g. right after an `ls` output burst — must
    /// still be highlighted. The stateless per-chunk call rendered it
    /// white because neither half matched the pattern alone.
    /// Rapid echo and editing bytes must be visible without waiting or Enter.
    /// A burst split ACROSS the debounce window still highlights: the
    /// second fragment resets nothing — as long as it arrives before the
    /// quiet flush, the merged line matches.
    /// Regression (live-board report): the IDLE prompt after an
    /// `ls` burst never sees a newline — it is the stream's last line. It
    /// must be highlighted the moment it already matches, not held white
    /// in the carry until the next command.
    #[test]
    fn prompt_highlight_does_not_color_non_prompt_lines() {
        let mut out = Vec::new();
        highlight_serial_prompt(b"[    0.000] Booting Linux\n", &mut out);
        let rendered = String::from_utf8(out).unwrap();
        // No ANSI codes should be present
        assert!(!rendered.contains("\x1b["));
    }
}
