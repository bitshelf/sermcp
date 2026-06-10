//! Terminal preference file — `~/.config/dutabo/terminal.jsonc`.
//!
//! JSONC is the house format (`.target.jsonc`, `config.jsonc`): one
//! parser, comments + trailing commas allowed, unknown keys REJECTED so
//! typos surface instead of silently defaulting. `DUTABO_TERMINAL`
//! overrides the path (tests/CI), mirroring TARGET_CONF / DUTABO_CONFIG.
//!
//! ```jsonc
//! {
//!   // Scroll behavior — shared by `dutabo serial` and the embedded
//!   // terminal of `dutabo init`.
//!   "scroll": {
//!     // Real-time ser2net tracking: at the bottom the view stays glued
//!     // to the live output; scrolled UP it stays anchored to the same
//!     // content while output keeps arriving (Xshell-style scroll
//!     // lock). false = the scrolled view rides along with the output.
//!     "follow_output": true,
//!     // Typing jumps the view back to the live line. Default OFF —
//!     // the view never moves just because you typed (Xshell-like);
//!     // Home/End/PgUp/PgDn always scroll manually.
//!     "jump_on_input": false
//!   }
//! }
//! ```

use std::path::PathBuf;

/// Env override for the preference file path.
pub const TERMINAL_PREFS_ENV: &str = "DUTABO_TERMINAL";

/// Scroll knobs (see the module doc for semantics).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollPrefs {
    pub follow_output: bool,
    pub jump_on_input: bool,
}

impl Default for ScrollPrefs {
    fn default() -> Self {
        Self {
            follow_output: true,
            jump_on_input: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TerminalPrefs {
    pub scroll: ScrollPrefs,
}

impl TerminalPrefs {
    /// Parse + validate JSONC text (unknown keys rejected).
    pub fn parse(text: &str) -> Result<Self, String> {
        let value = crate::config::parse_jsonc_value(text)?;
        let object = value
            .as_object()
            .ok_or("terminal preferences must be an object")?;
        let mut prefs = Self::default();
        for (key, section) in object {
            if key != "scroll" {
                return Err(format!("unknown key {key:?} (expected \"scroll\")"));
            }
            let scroll = section.as_object().ok_or("\"scroll\" must be an object")?;
            for (name, raw) in scroll {
                let value = raw
                    .as_bool()
                    .ok_or(format!("scroll.{name} must be a boolean"))?;
                match name.as_str() {
                    "follow_output" => prefs.scroll.follow_output = value,
                    "jump_on_input" => prefs.scroll.jump_on_input = value,
                    other => {
                        return Err(format!(
                            "unknown key {other:?} (expected follow_output/jump_on_input)"
                        ));
                    }
                }
            }
        }
        Ok(prefs)
    }

    /// The preference file path (`DUTABO_TERMINAL` → XDG → HOME).
    pub fn path() -> Option<PathBuf> {
        if let Some(path) = std::env::var_os(TERMINAL_PREFS_ENV) {
            return Some(PathBuf::from(path));
        }
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .map(|p| p.join("dutabo").join("terminal.jsonc"))
    }

    /// Load from disk. A MISSING file is the default prefs (not an
    /// error); a malformed file returns the error alongside the defaults
    /// so the caller can surface it without losing the terminal.
    pub fn load() -> (Self, Option<String>) {
        let Some(path) = Self::path() else {
            return (Self::default(), None);
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return (Self::default(), None);
        };
        match Self::parse(&text) {
            Ok(prefs) => (prefs, None),
            Err(reason) => (
                Self::default(),
                Some(format!("{}: {reason}", path.display())),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_documented_knobs() {
        assert_eq!(
            TerminalPrefs::default(),
            TerminalPrefs {
                scroll: ScrollPrefs {
                    follow_output: true,
                    jump_on_input: false,
                }
            }
        );
        let prefs = TerminalPrefs::parse(
            r#"{
          // Xshell-style drift + input jump, both off-default.
          "scroll": { "follow_output": false, "jump_on_input": true, },
        }"#,
        )
        .unwrap();
        assert!(!prefs.scroll.follow_output);
        assert!(prefs.scroll.jump_on_input);
        // Empty document = defaults.
        assert_eq!(
            TerminalPrefs::parse("{ }").unwrap(),
            TerminalPrefs::default()
        );
    }

    #[test]
    fn unknown_keys_and_wrong_types_are_rejected() {
        for text in [
            r#"{"typo": 1}"#,
            r#"{"scroll": {"drift": true}}"#,
            r#"{"scroll": {"follow_output": "yes"}}"#,
            r#"{"scroll": true}"#,
            "not jsonc {{{",
        ] {
            assert!(TerminalPrefs::parse(text).is_err(), "{text}");
        }
    }
}
