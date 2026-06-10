//! Heuristics for deciding whether raw serial bytes look like console text.
//!
//! This is deliberately not a baud-rate detector: garbage is negative
//! evidence for the currently selected UART settings.  Callers must pass the
//! original bytes (before `from_utf8_lossy`) so invalid UTF-8 remains visible.

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TextVerdict {
    Indeterminate,
    Normal,
    Suspicious,
    Garbage,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TextReport {
    pub verdict: TextVerdict,
    /// 0 means text-like, 100 means garbage-like.
    pub garbage_score: u8,
    pub bytes: usize,
    pub invalid_utf8_ratio: f32,
    pub control_ratio: f32,
    pub nul_ratio: f32,
    pub ansi_sequences: usize,
    pub bigram_coherence: Option<f32>,
    pub reasons: Vec<&'static str>,
}

pub fn analyze_serial_text(data: &[u8]) -> TextReport {
    if data.is_empty() {
        return TextReport {
            verdict: TextVerdict::Indeterminate,
            garbage_score: 0,
            bytes: 0,
            invalid_utf8_ratio: 0.0,
            control_ratio: 0.0,
            nul_ratio: 0.0,
            ansi_sequences: 0,
            bigram_coherence: None,
            reasons: vec!["empty input"],
        };
    }

    let mut i = 0usize;
    let mut invalid = 0usize;
    let mut controls = 0usize;
    let mut nuls = 0usize;
    let mut ansi_sequences = 0usize;
    let mut ansi_bytes = 0usize;
    while i < data.len() {
        if data[i] == 0x1b
            && let Some(end) = skip_ansi_sequence(data, i)
        {
            ansi_sequences += 1;
            ansi_bytes += end - i;
            i = end;
            continue;
        }
        match decode_one_utf8(&data[i..]) {
            Some((ch, len)) => {
                if ch == '\0' {
                    nuls += len;
                    controls += len;
                } else if ch.is_control() && !matches!(ch, '\r' | '\n' | '\t' | '\u{0008}') {
                    controls += len;
                }
                i += len;
            }
            None => {
                invalid += 1;
                if data[i] == 0 {
                    nuls += 1;
                }
                i += 1;
            }
        }
    }

    let denom = data.len() as f32;
    let invalid_utf8_ratio = invalid as f32 / denom;
    let control_ratio = controls as f32 / denom;
    let nul_ratio = nuls as f32 / denom;
    let unexplained_ratio =
        ((invalid + controls) as f32 / denom - ansi_bytes as f32 / denom).max(0.0);
    let (coherence, bigrams) = ascii_bigram_coherence(data);
    let bigram_coherence = (bigrams >= 24).then_some(coherence);

    let mut score = 0.0f32;
    let mut reasons = Vec::new();
    if invalid_utf8_ratio > 0.0 {
        score += 45.0 * (invalid_utf8_ratio / 0.08).clamp(0.0, 1.0);
        if invalid_utf8_ratio >= 0.01 {
            reasons.push("invalid UTF-8 byte sequences");
        }
    }
    if control_ratio > 0.0 {
        score += 30.0 * (control_ratio / 0.06).clamp(0.0, 1.0);
        if control_ratio >= 0.01 {
            reasons.push("unexpected control characters");
        }
    }
    if nul_ratio > 0.0 {
        score += 30.0 * (nul_ratio / 0.015).clamp(0.0, 1.0);
        reasons.push("NUL bytes present");
    }
    if unexplained_ratio > 0.02 {
        score += 20.0 * (unexplained_ratio / 0.15).clamp(0.0, 1.0);
    }
    if let Some(c) = bigram_coherence {
        if c < 0.10 {
            score += 30.0 * ((0.10 - c) / 0.10).clamp(0.0, 1.0);
            reasons.push("very low ASCII bigram coherence");
        } else if c < 0.18 {
            score += 12.0 * ((0.18 - c) / 0.08).clamp(0.0, 1.0);
            reasons.push("low ASCII bigram coherence");
        }
    }

    let garbage_score = score.clamp(0.0, 100.0).round() as u8;
    let strong_short = garbage_score >= 75 || nul_ratio >= 0.05 || invalid_utf8_ratio >= 0.20;
    // A complete interactive prompt is useful evidence even without boot text.
    // Require structure and clean bytes; a blank echo or lone punctuation
    // must not turn a quiet or disconnected console into a passing test.
    static PROMPT: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r"^(?:[a-zA-Z0-9_.-]+@[a-zA-Z0-9_.-]+:[^\r\n]*[#$]|[a-zA-Z0-9_.-]+ login:|=>) *$",
        )
        .unwrap()
    });
    let short_prompt = invalid == 0
        && controls == 0
        && garbage_score < 25
        && std::str::from_utf8(data)
            .ok()
            .is_some_and(|text| PROMPT.is_match(crate::ansi_strip::strip_str(text).trim()));
    let verdict = if data.len() < 64 && !strong_short && !short_prompt {
        TextVerdict::Indeterminate
    } else if garbage_score >= 55 {
        TextVerdict::Garbage
    } else if garbage_score >= 25 {
        TextVerdict::Suspicious
    } else {
        TextVerdict::Normal
    };
    TextReport {
        verdict,
        garbage_score,
        bytes: data.len(),
        invalid_utf8_ratio,
        control_ratio,
        nul_ratio,
        ansi_sequences,
        bigram_coherence,
        reasons,
    }
}

fn decode_one_utf8(input: &[u8]) -> Option<(char, usize)> {
    let max = input.len().min(4);
    for len in 1..=max {
        if let Ok(s) = std::str::from_utf8(&input[..len])
            && let Some(ch) = s.chars().next()
            && ch.len_utf8() == len
        {
            return Some((ch, len));
        }
    }
    None
}

fn skip_ansi_sequence(data: &[u8], start: usize) -> Option<usize> {
    let next = *data.get(start + 1)?;
    match next {
        b'[' => {
            let mut i = start + 2;
            while let Some(&b) = data.get(i) {
                if (0x40..=0x7e).contains(&b) {
                    return Some(i + 1);
                }
                if !(0x20..=0x3f).contains(&b) {
                    return None;
                }
                i += 1;
            }
            None
        }
        b']' => {
            let mut i = start + 2;
            while i < data.len() {
                if data[i] == 0x07 {
                    return Some(i + 1);
                }
                if data[i] == 0x1b && data.get(i + 1) == Some(&b'\\') {
                    return Some(i + 2);
                }
                i += 1;
            }
            None
        }
        0x30..=0x7e => Some(start + 2),
        _ => None,
    }
}

fn ascii_bigram_coherence(data: &[u8]) -> (f32, usize) {
    let mut common = 0usize;
    let mut total = 0usize;
    let mut previous = None;
    let mut i = 0usize;
    while i < data.len() {
        if data[i] == 0x1b
            && let Some(end) = skip_ansi_sequence(data, i)
        {
            previous = None;
            i = end;
            continue;
        }
        let b = data[i].to_ascii_lowercase();
        if b.is_ascii_alphabetic() {
            if let Some(a) = previous {
                total += 1;
                if is_common_bigram(a, b) {
                    common += 1;
                }
            }
            previous = Some(b);
        } else {
            previous = None;
        }
        i += 1;
    }
    if total == 0 {
        (0.0, 0)
    } else {
        (common as f32 / total as f32, total)
    }
}

fn is_common_bigram(a: u8, b: u8) -> bool {
    matches!(
        (a, b),
        (b't', b'h')
            | (b'h', b'e')
            | (b'i', b'n')
            | (b'e', b'r')
            | (b'a', b'n')
            | (b'r', b'e')
            | (b'o', b'n')
            | (b'a', b't')
            | (b'e', b'n')
            | (b'n', b'd')
            | (b't', b'i')
            | (b'e', b's')
            | (b'o', b'r')
            | (b't', b'e')
            | (b'o', b'f')
            | (b'e', b'd')
            | (b'i', b's')
            | (b'i', b't')
            | (b'a', b'l')
            | (b'a', b'r')
            | (b's', b't')
            | (b't', b'o')
            | (b'n', b'g')
            | (b's', b'e')
            | (b'h', b'a')
            | (b'a', b's')
            | (b'o', b'u')
            | (b'i', b'o')
            | (b'l', b'e')
            | (b'c', b'o')
            | (b'm', b'e')
            | (b'd', b'e')
            | (b'n', b'e')
            | (b'e', b'a')
            | (b'c', b'e')
            | (b'l', b'i')
            | (b'l', b'l')
            | (b'b', b'e')
            | (b'm', b'a')
            | (b's', b'i')
            | (b'p', b'r')
            | (b'u', b's')
            | (b'w', b'a')
            | (b'w', b'i')
            | (b'a', b'c')
            | (b'a', b'd')
            | (b'n', b'o')
            | (b'p', b'e')
            | (b'r', b't')
            | (b's', b'o')
            | (b's', b'y')
            | (b't', b'r')
            | (b'u', b'n')
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_boot_text_passes() {
        let r = analyze_serial_text(b"U-Boot 2024.01\r\nStarting kernel ...\r\nLinux version 6.1.99\r\nsystemd started network manager\r\nroot@board:~# ");
        assert_eq!(r.verdict, TextVerdict::Normal, "{r:#?}");
    }

    #[test]
    fn invalid_uart_bytes_are_garbage() {
        let bad = [
            0xff, 0xfe, 0x83, 0xc0, 0, 0x91, 0x82, 0xf8, 1, 3, 0xff, 0xaa, 0x80, 0xbf, 0xc1, 0xf7,
        ]
        .repeat(4);
        assert_eq!(analyze_serial_text(&bad).verdict, TextVerdict::Garbage);
    }

    #[test]
    fn ansi_and_utf8_are_not_garbage() {
        let r = analyze_serial_text(
            "\x1b[32mINFO\x1b[0m 系统启动成功，网络连接正常。\r\n"
                .repeat(4)
                .as_bytes(),
        );
        assert_ne!(r.verdict, TextVerdict::Garbage, "{r:#?}");
        assert!(r.ansi_sequences >= 8);
    }
}

#[cfg(test)]
mod prompt_tests {
    use super::*;

    #[test]
    fn short_interactive_prompts_are_text_evidence() {
        for data in [
            b"\r\nroot@myd-lr3568x-buildroot:/# ".as_slice(),
            b"user@dut:~$ ",
            b"\x1b[32mroot@dut:/# \x1b[0m",
            b"dut login: ",
            b"=> ",
        ] {
            assert_eq!(analyze_serial_text(data).verdict, TextVerdict::Normal);
        }
        for data in [
            b"\r\n".as_slice(),
            b"# ",
            b"random text",
            b"root@dut:/# \xff",
        ] {
            assert_ne!(analyze_serial_text(data).verdict, TextVerdict::Normal);
        }
    }
}
