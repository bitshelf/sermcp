//! UEFI loader — NOT IMPLEMENTED yet.
//!
//! The wizard exposes the `uefi` option so the config shape is ready;
//! selecting it shows a "not implemented" note below the field. The
//! pattern/prompt methods below are PLACEHOLDERS (delegating to U-Boot's
//! shapes) until real UEFI Shell interaction lands — replace them with
//! UEFI's own boot-stage anchors and prompt detection.

use crate::loader::Loader;

/// The `loader` config value / wizard option text.
pub const NAME: &str = "uefi";

/// The UEFI loader instance (stub).
pub static UEFI: Uefi = Uefi;

/// UEFI — not yet implemented.
pub struct Uefi;

impl Loader for Uefi {
    fn name(&self) -> &'static str {
        NAME
    }

    // ── Placeholders until UEFI is implemented ─────────────────────────

    fn spl_pattern(&self) -> &'static str {
        crate::uboot::UBOOT.spl_pattern()
    }
    fn tpl_pattern(&self) -> &'static str {
        crate::uboot::UBOOT.tpl_pattern()
    }
    fn banner_or_prompt_pattern(&self) -> &'static str {
        crate::uboot::UBOOT.banner_or_prompt_pattern()
    }
    fn autoboot_pattern(&self) -> &'static str {
        crate::uboot::UBOOT.autoboot_pattern()
    }
    fn prompt_watch_pattern(&self) -> &'static str {
        crate::uboot::UBOOT.prompt_watch_pattern()
    }
    fn default_interrupt_char(&self) -> &'static str {
        crate::uboot::UBOOT.default_interrupt_char()
    }
    fn default_interrupt_strategy(&self) -> &'static str {
        crate::uboot::UBOOT.default_interrupt_strategy()
    }
    fn is_prompt(&self, line: &str) -> bool {
        crate::uboot::UBOOT.is_prompt(line)
    }
    fn is_banner(&self, line: &str) -> bool {
        crate::uboot::UBOOT.is_banner(line)
    }
    fn is_autoboot(&self, line: &str) -> bool {
        crate::uboot::UBOOT.is_autoboot(line)
    }

    // ── Implementation status ───────────────────────────────────────────

    fn is_implemented(&self) -> bool {
        false
    }

    fn not_implemented_note(&self) -> Option<&'static str> {
        // English, per the original "show English 'to be implemented'".
        Some("Not implemented")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::Loader;

    #[test]
    fn uefi_is_a_named_not_implemented_stub() {
        assert_eq!(UEFI.name(), "uefi");
        assert!(!UEFI.is_implemented());
        assert_eq!(UEFI.not_implemented_note(), Some("Not implemented"));
    }
}
