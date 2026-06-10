//! Terminal preference file — `~/.config/dutabo/terminal.toml`.
//!
//! TOML, deliberately unlike the project's other configs
//! (`~/.config/dutabo/config.jsonc`, the per-project `.target.jsonc`): a
//! flat two-boolean table reads better in TOML and comments are native,
//! while serde's derived `deny_unknown_fields` still REJECTS unknown keys
//! so typos surface instead of silently defaulting. `DUTABO_TERMINAL`
//! overrides the path (tests/CI), mirroring TARGET_CONF / DUTABO_CONFIG.
//!
//! ```toml
//! # Scroll behavior — shared by `dutabo serial` and the embedded
//! # terminal of `dutabo init`.
//! [scroll]
//! # Real-time ser2net tracking: at the bottom the view stays glued to
//! # the live output; scrolled UP it stays anchored to the same content
//! # while output keeps arriving (Xshell-style scroll lock). false = the
//! # scrolled view rides along with the output.
//! follow_output = true
//! # Typing jumps the view back to the live line. Default OFF — the view
//! # never moves just because you typed (Xshell-like); Home/End/PgUp/PgDn
//! # always scroll manually.
//! jump_on_input = false
//! ```

use std::path::PathBuf;

/// Env override for the preference file path.
pub const TERMINAL_PREFS_ENV: &str = "DUTABO_TERMINAL";

/// Scroll knobs (see the module doc for semantics).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TerminalPrefs {
    pub scroll: ScrollPrefs,
}

impl TerminalPrefs {
    /// Parse + validate TOML text (unknown keys rejected).
    pub fn parse(text: &str) -> Result<Self, String> {
        toml::from_str::<Self>(text).map_err(|e| format!("TOML parse error: {e}"))
    }

    /// The preference file path (`DUTABO_TERMINAL` → XDG → HOME).
    pub fn path() -> Option<PathBuf> {
        if let Some(path) = std::env::var_os(TERMINAL_PREFS_ENV) {
            return Some(PathBuf::from(path));
        }
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .map(|p| p.join("dutabo").join("terminal.toml"))
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
            r#"# Xshell-style drift + input jump, both off-default.
            [scroll]
            follow_output = false
            jump_on_input = true
            "#,
        )
        .unwrap();
        assert!(!prefs.scroll.follow_output);
        assert!(prefs.scroll.jump_on_input);
        // Any subset of the knobs may be spelled out; the rest default.
        let partial = TerminalPrefs::parse("[scroll]\njump_on_input = true\n").unwrap();
        assert!(partial.scroll.jump_on_input);
        assert!(partial.scroll.follow_output);
        // Empty / comment-only document = defaults.
        assert_eq!(
            TerminalPrefs::parse("# nothing here\n").unwrap(),
            TerminalPrefs::default()
        );
    }

    #[test]
    fn unknown_keys_and_wrong_types_are_rejected() {
        for text in [
            "typo = 1\n",
            "[scroll]\ndrift = true\n",
            "[scroll]\nfollow_output = \"yes\"\n",
            "scroll = true\n",
            "not toml {{{",
        ] {
            assert!(TerminalPrefs::parse(text).is_err(), "{text}");
        }
    }

    #[test]
    fn rejects_are_named() {
        let err = TerminalPrefs::parse("[scroll]\ndrift = true\n").unwrap_err();
        assert!(err.contains("drift"), "error must name the key: {err}");
        let err = TerminalPrefs::parse("typo = 1\n").unwrap_err();
        assert!(err.contains("typo"), "error must name the key: {err}");
    }
}
