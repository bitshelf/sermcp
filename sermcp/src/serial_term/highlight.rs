//! Semantic prompt highlighting — a render-time overlay, never a byte
//! transformation.
//!
//! The highlighter only READS terminal rows: it receives the row's visible
//! text (plain chars from the VT-core cells, no ANSI to strip) and returns
//! column spans tagged with a semantic style. The renderer merges those
//! spans into the frame's SGR; RX bytes, TX bytes, screen text, cursor and
//! geometry are unaffected. The old `PromptHighlighter` (ANSI injection
//! into the stream, with a DEBOUNCE hold) stays for static/MCP log paths
//! and is banned from the interactive display.

use crate::highlight::prompt_line_spans;

/// Interactive highlight selector (`[dut.serial] highlight`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HighlightStyle {
    /// Shell prompt spans only (default).
    #[default]
    Prompt,
    /// No semantic highlighting.
    Off,
}

impl HighlightStyle {
    pub fn parse(v: &str) -> Option<Self> {
        match v.trim().to_ascii_lowercase().as_str() {
            "prompt" => Some(Self::Prompt),
            "off" | "none" | "false" => Some(Self::Off),
            _ => None,
        }
    }

    pub fn enabled(self) -> bool {
        !matches!(self, Self::Off)
    }
}

/// Semantic meaning of a highlighted span.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemanticStyle {
    /// `user@host` — green bold.
    PromptUserHost,
    /// `/path` — blue bold.
    PromptPath,
    /// `$` / `#` sigil — white bold.
    PromptSigil,
}

/// One highlighted column range; `start_col`/`end_col` are cell columns,
/// `end_col` exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HighlightSpan {
    pub start_col: usize,
    pub end_col: usize,
    pub style: SemanticStyle,
}

/// Highlights of one row.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RowHighlight {
    pub spans: Vec<HighlightSpan>,
}

/// Stateless per-row prompt analyzer.
#[derive(Debug, Clone, Copy, Default)]
pub struct PromptSemanticHighlighter;

impl PromptSemanticHighlighter {
    /// Analyze one visible row. `row` is the viewport row index (part of
    /// the stable overlay API); `text` is the row's full visible text
    /// (trailing spaces included — the sigil match needs them).
    pub fn analyze(&self, row: usize, text: &str) -> RowHighlight {
        let _ = row;
        let Some((user, dir, sigil)) = prompt_line_spans(text.as_bytes()) else {
            return RowHighlight::default();
        };
        // Map byte ranges (regex coordinates) to cell columns (renderer
        // coordinates) through the UTF-8 boundary table.
        let byte_to_col: Vec<usize> = text
            .char_indices()
            .map(|(i, _)| i)
            .chain(std::iter::once(text.len()))
            .collect();
        let col = |byte: usize| -> Option<usize> { byte_to_col.iter().position(|&b| b == byte) };
        let (Some(su), Some(eu), Some(sd), Some(ed), Some(ss), Some(es)) = (
            col(user.start),
            col(user.end),
            col(dir.start),
            col(dir.end),
            col(sigil.start),
            col(sigil.end),
        ) else {
            return RowHighlight::default();
        };
        RowHighlight {
            spans: vec![
                HighlightSpan {
                    start_col: su,
                    end_col: eu,
                    style: SemanticStyle::PromptUserHost,
                },
                HighlightSpan {
                    start_col: sd,
                    end_col: ed,
                    style: SemanticStyle::PromptPath,
                },
                HighlightSpan {
                    start_col: ss,
                    end_col: es,
                    style: SemanticStyle::PromptSigil,
                },
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans(text: &str) -> Vec<(usize, usize, SemanticStyle)> {
        PromptSemanticHighlighter
            .analyze(0, text)
            .spans
            .iter()
            .map(|s| (s.start_col, s.end_col, s.style))
            .collect()
    }

    #[test]
    fn colon_prompt_spans() {
        let s = spans("root@rk3576:/# ");
        assert_eq!(
            s,
            vec![
                (0, 11, SemanticStyle::PromptUserHost),
                (12, 13, SemanticStyle::PromptPath),
                (13, 15, SemanticStyle::PromptSigil),
            ]
        );
    }

    #[test]
    fn non_prompt_row_is_empty() {
        assert!(spans("[    0.000000] Booting Linux on physical CPU").is_empty());
        assert!(spans("").is_empty());
    }

    #[test]
    fn utf8_prompt_columns_are_cell_columns() {
        // Multi-byte characters in the dir path: columns must count chars,
        // not bytes ("/数据" is 6 bytes but 3 cells).
        let s = spans("root@host:/数据# ");
        assert_eq!(s.len(), 3);
        assert_eq!(s[0], (0, 9, SemanticStyle::PromptUserHost));
        assert_eq!(s[1], (10, 13, SemanticStyle::PromptPath));
        assert_eq!(s[2], (13, 15, SemanticStyle::PromptSigil));
    }
}
