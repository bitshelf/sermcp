//! U-Boot loader implementation (the `Loader` strategy).
//!
//! U-Boot is the currently implemented boot loader: it owns the boot-stage
//! anchors, the interactive `=>` command-line prompt, and the autoboot
//! countdown. The boot detector / serial engine / init wizard consume all
//! of it through the [`Loader`] trait, so U-Boot behavior lives in this
//! ONE file.

use crate::loader::Loader;

/// The `loader` config value / wizard option text.
pub const NAME: &str = "uboot";

/// The U-Boot loader instance (registry entry).
pub static UBOOT: Uboot = Uboot;

/// U-Boot — implemented.
pub struct Uboot;

impl Loader for Uboot {
    fn name(&self) -> &'static str {
        NAME
    }

    fn spl_pattern(&self) -> &'static str {
        r"U-Boot\s+SPL"
    }

    fn tpl_pattern(&self) -> &'static str {
        r"TL[123]\s"
    }

    fn banner_or_prompt_pattern(&self) -> &'static str {
        r"(?:U-Boot\s+20\d{2}|^=>\s)"
    }

    fn autoboot_pattern(&self) -> &'static str {
        r"Hit\s+(?:any\s+)?key\s+to\s+stop\s+autoboot"
    }

    fn prompt_watch_pattern(&self) -> &'static str {
        r"=>|U-Boot[>#]"
    }

    fn default_interrupt_char(&self) -> &'static str {
        "ctrl_c"
    }

    fn default_interrupt_strategy(&self) -> &'static str {
        "flood"
    }

    fn is_prompt(&self, line: &str) -> bool {
        let t = line.trim();
        t == "=>" || t.starts_with("=> ") || t.starts_with("U-Boot>") || t.starts_with("U-Boot#")
    }

    fn is_banner(&self, line: &str) -> bool {
        line.trim().starts_with("U-Boot 20")
    }

    fn is_autoboot(&self, line: &str) -> bool {
        let t = line.trim().to_ascii_lowercase();
        t.contains("hit") && t.contains("key to stop autoboot")
    }

    fn is_implemented(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_is_the_line_itself() {
        assert!(UBOOT.is_prompt("=> "));
        assert!(UBOOT.is_prompt("=>"));
        assert!(UBOOT.is_prompt("=> boot"));
        assert!(UBOOT.is_prompt("U-Boot> "));
        assert!(UBOOT.is_prompt("U-Boot# "));
        assert!(!UBOOT.is_prompt("the boot log merely mentions => mid-line"));
        assert!(!UBOOT.is_prompt("board=>something debris"));
        assert!(!UBOOT.is_prompt("ser2net port"));
        assert!(!UBOOT.is_prompt(""));
    }

    #[test]
    fn banner_is_the_version_line() {
        assert!(UBOOT.is_banner("U-Boot 2024.01 (Jan 01 2025 - 00:00:00)"));
        assert!(!UBOOT.is_banner("U-Boot SPL 2024.01"));
        assert!(!UBOOT.is_banner("=> "));
    }

    #[test]
    fn autoboot_countdown_is_the_interrupt_window() {
        assert!(UBOOT.is_autoboot("Hit any key to stop autoboot:  3"));
        assert!(UBOOT.is_autoboot("Hit key to stop autoboot:  2"));
        assert!(!UBOOT.is_autoboot("=> "));
        assert!(!UBOOT.is_autoboot(""));
    }

    #[test]
    fn uboot_is_implemented_and_named() {
        assert_eq!(UBOOT.name(), "uboot");
        assert!(UBOOT.is_implemented());
        assert_eq!(UBOOT.not_implemented_note(), None);
    }
}
