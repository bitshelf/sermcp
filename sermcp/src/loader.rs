//! Boot loader abstraction — strategy pattern.
//!
//! Each supported boot loader implements the [`Loader`] trait in its own
//! top-level module (`src/uboot.rs`, `src/uefi.rs`, …). The registry
//! [`all()`] is the ONLY place that knows which loaders exist: adding a
//! boot loader is a new module + one registry entry, and the boot
//! detector / serial engine / init wizard keep working unchanged.

use std::sync::LazyLock;

/// One boot loader's behavior. `Uboot` (implemented) and `Uefi` (stub)
/// are the current implementations; the trait is object-safe so the
/// registry can hand out `&dyn Loader` values.
pub trait Loader: Send + Sync {
    /// The `loader` config value / wizard option text ("uboot", "uefi").
    fn name(&self) -> &'static str;

    // ── Boot-stage anchors (regex sources, compiled by boot_detector) ──

    /// SPL handoff line — the earliest boot signal.
    fn spl_pattern(&self) -> &'static str;
    /// TPL handoff line.
    fn tpl_pattern(&self) -> &'static str;
    /// Version banner OR the interactive command-line prompt.
    fn banner_or_prompt_pattern(&self) -> &'static str;
    /// Autoboot countdown line — the interrupt window.
    fn autoboot_pattern(&self) -> &'static str;

    // ── Interactive command line ────────────────────────────────────────

    /// Watcher queued while entering the loader's interactive prompt.
    fn prompt_watch_pattern(&self) -> &'static str;
    /// Default interrupt character for autoboot interruption.
    fn default_interrupt_char(&self) -> &'static str;
    /// Default autoboot-interrupt strategy.
    fn default_interrupt_strategy(&self) -> &'static str;
    /// Is this a loader interactive command-line prompt line?
    fn is_prompt(&self, line: &str) -> bool;
    /// Is this the loader's version banner line?
    fn is_banner(&self, line: &str) -> bool;
    /// Is this the autoboot countdown line (the interrupt window)?
    fn is_autoboot(&self, line: &str) -> bool;

    // ── Implementation status ───────────────────────────────────────────

    /// Is this loader actually implemented? Stub loaders return false and
    /// the wizard renders their [`Loader::not_implemented_note`] below the
    /// field instead of pretending the loader works.
    fn is_implemented(&self) -> bool;

    /// The "not implemented" note for stub loaders (None when implemented).
    fn not_implemented_note(&self) -> Option<&'static str> {
        None
    }
}

/// The known loader instances — the registry. Add a new loader here.
pub fn all() -> &'static [&'static dyn Loader] {
    static ALL: LazyLock<Vec<&'static dyn Loader>> =
        LazyLock::new(|| vec![&crate::uboot::UBOOT, &crate::uefi::UEFI]);
    ALL.as_slice()
}

/// The default loader (U-Boot).
pub fn default() -> &'static dyn Loader {
    by_name(crate::uboot::NAME).expect("uboot is always registered")
}

/// Look up a loader by config value ("uboot" | "uefi"), case-insensitive.
pub fn by_name(name: &str) -> Option<&'static dyn Loader> {
    let t = name.trim();
    all()
        .iter()
        .copied()
        .find(|l| l.name().eq_ignore_ascii_case(t))
}

/// The wizard option values, in display order.
pub const OPTIONS: &[&str] = &[crate::uboot::NAME, crate::uefi::NAME];

/// The "not implemented" note for a stub loader choice, if any.
pub fn not_implemented_note(value: &str) -> Option<&'static str> {
    by_name(value).and_then(|l| l.not_implemented_note())
}
