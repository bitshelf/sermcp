//! Read + classify per-DUT `target-state` files
//! (`.dut-serial/<dut_name>/target-state`).
//!
//! The file is written atomically by the [`StateManager`](crate::state_manager)
//! with the raw [`TargetState::as_str`] vocabulary ("active", "crashed",
//! "DUT-off", ...). This module is the ONE shared reader/classifier for
//! every consumer that needs a DUT's live state without an engine:
//! the `serial_list_duts` MCP tool, `dutabo list` (table + `--json`), and
//! the server-side `inventory.json` writer. Keeping the vocabulary mapping
//! in one place means those three outputs can never disagree about what
//! "crashed" means.
//!
//! Rules (test-locked):
//! * missing file → state "unknown", `missing: true`, age `None` — a DUT
//!   with no state file is NEVER rendered as a healthy state;
//! * unreadable / empty / unrecognized text → "unknown";
//! * `critical` is true ONLY for crashed / DUT-off / disconnected — a kernel
//!   crash must be visually prominent ("✗ crashed"), never mistaken for
//!   healthy;
//! * `stale` at >= [`STALE_THRESHOLD_SECS`]; consumers append an age suffix
//!   from [`AGE_SUFFIX_THRESHOLD_SECS`] so old state is never silently
//!   presented as fresh.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::state_manager::TargetState;

/// A state file older than this many seconds is flagged `stale: true`.
pub const STALE_THRESHOLD_SECS: u64 = 3600;
/// State-file age at which consumers start appending an age suffix
/// ("active (42m ago)") so old state is never presented as fresh.
pub const AGE_SUFFIX_THRESHOLD_SECS: u64 = 600;

/// The recognized state vocabulary — exactly [`TargetState::as_str`].
/// "unknown" is a derived fallback (never written by the state manager).
pub const KNOWN_STATES: [&str; 8] = [
    "active",
    "booting",
    "uboot",
    "crashed",
    "DUT-off",
    "disconnected",
    "dutabo",
    "stopped",
];

/// Boot-detector stage names that mean "a login shell is reachable" —
/// plain Linux shells and the Android boot-completion markers. The single
/// source of truth for boot-complete / state-Active decisions (previously
/// four copies across serial_engine and mcp_handler::task_runtime).
pub const SHELL_STAGES: [&str; 6] = [
    "shell",
    "android_shell",
    "android_adbd",
    "android_bootanim",
    "android_surfaceflinger",
    "android_boot_completed",
];

/// Whether a boot-detector stage name is a shell-like stage
/// (see [`SHELL_STAGES`]).
pub fn is_shell_stage(stage: &str) -> bool {
    SHELL_STAGES.contains(&stage)
}

/// Canonical classification of raw state-file text. Known vocabulary
/// strings are returned as-is (trimmed); anything else — including an empty
/// string — classifies as "unknown". Case-sensitive: "ACTIVE" is unknown.
pub fn classify_state(text: &str) -> String {
    let trimmed = text.trim();
    if KNOWN_STATES.contains(&trimmed) {
        trimmed.to_string()
    } else {
        "unknown".to_string()
    }
}

/// Coarse semantic bucket for styling/priority. Groups mirror the statusline
/// color scheme: critical (red), transitional (yellow/cyan), online
/// (green/magenta), offline (dim), unknown (dim).
pub fn classification_of(state: &str) -> &'static str {
    match state {
        "crashed" | "DUT-off" | "disconnected" => "critical",
        "booting" | "uboot" => "transitional",
        "active" | "dutabo" => "online",
        "stopped" => "offline",
        _ => "unknown",
    }
}

/// Critical = the three red states (crashed / DUT-off / disconnected).
pub fn is_critical(state: &str) -> bool {
    matches!(state, "crashed" | "DUT-off" | "disconnected")
}

/// STATE-cell label: critical states carry a "✗ " prefix so a kernel crash
/// can never be mistaken for a healthy state. Age suffixes are appended by
/// the renderer (see [`AGE_SUFFIX_THRESHOLD_SECS`]), not here.
pub fn state_label(state: &str) -> String {
    if is_critical(state) {
        format!("✗ {state}")
    } else {
        state.to_string()
    }
}

/// Map the canonical vocabulary back to [`TargetState`] for consumers that
/// need the enum (ANSI statusline text via
/// `StateManager::format_statusline_labeled`, color tables). "unknown" has
/// no `TargetState` — there is no live statusline text for it.
pub fn target_state_of(state: &str) -> Option<TargetState> {
    match state {
        "stopped" => Some(TargetState::Stopped),
        "active" => Some(TargetState::Active),
        "booting" => Some(TargetState::Booting),
        "uboot" => Some(TargetState::UBoot),
        "crashed" => Some(TargetState::Crashed),
        "DUT-off" => Some(TargetState::DutOff),
        "disconnected" => Some(TargetState::Disconnected),
        "dutabo" => Some(TargetState::Dutabo),
        _ => None,
    }
}

/// One DUT's classified state-file snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DutStateView {
    /// Canonical vocabulary string ("active", "crashed", ... or "unknown").
    pub state: String,
    /// Coarse bucket for styling/priority (see [`classification_of`]).
    pub classification: String,
    /// State-file age from mtime; None when the file is missing, its
    /// metadata is unreadable, or the mtime is in the future (clock skew).
    pub age_secs: Option<u64>,
    /// True when age >= [`STALE_THRESHOLD_SECS`]. Never true for missing files.
    pub stale: bool,
    /// True when the state file does not exist.
    pub missing: bool,
    /// True only for crashed / DUT-off / disconnected.
    pub critical: bool,
    /// Human display label for the STATE cell ("active", "✗ crashed", "unknown").
    pub label: String,
}

/// Read and classify one `target-state` file. Never fails and never renders
/// a missing/unreadable/unknown file as a healthy state.
pub fn read_dut_state(state_file: &Path) -> DutStateView {
    let metadata = std::fs::metadata(state_file).ok();
    let missing = metadata.is_none();
    let age_secs = metadata
        .as_ref()
        .and_then(|meta| meta.modified().ok())
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .map(|elapsed| elapsed.as_secs());
    // Unreadable-but-present files (e.g. wrong permissions) are "unknown",
    // never healthy. The read is attempted independently of metadata so a
    // path that exists but cannot be read (e.g. a directory) is covered too.
    let text = std::fs::read_to_string(state_file)
        .ok()
        .filter(|text| !text.trim().is_empty());
    let state = text
        .as_deref()
        .map(classify_state)
        .unwrap_or_else(|| "unknown".to_string());
    DutStateView {
        classification: classification_of(&state).to_string(),
        stale: age_secs.is_some_and(|age| age >= STALE_THRESHOLD_SECS),
        critical: is_critical(&state),
        label: state_label(&state),
        state,
        age_secs,
        missing,
    }
}

/// Render a compact age suffix for the STATE cell, e.g. "42m ago", "5h ago",
/// "3d ago". Returns None below [`AGE_SUFFIX_THRESHOLD_SECS`] — a fresh
/// state file is presumed current and shows no suffix.
pub fn format_age_suffix(age_secs: u64) -> Option<String> {
    if age_secs < AGE_SUFFIX_THRESHOLD_SECS {
        return None;
    }
    const MINUTE: u64 = 60;
    const HOUR: u64 = 3_600;
    const DAY: u64 = 86_400;
    Some(if age_secs >= DAY {
        format!("{}d ago", age_secs / DAY)
    } else if age_secs >= HOUR {
        format!("{}h ago", age_secs / HOUR)
    } else {
        format!("{}m ago", age_secs / MINUTE)
    })
}

/// STATE cell text for the `dutabo list` table: the label plus an age suffix
/// when the state file is old enough. A missing file renders plain "unknown"
/// (its age is None — never "unknown (missing)").
pub fn format_state_cell(view: &DutStateView) -> String {
    let suffix = view
        .age_secs
        .filter(|_| !view.missing)
        .and_then(format_age_suffix);
    match suffix {
        Some(suffix) => format!("{} ({suffix})", view.label),
        None => view.label.clone(),
    }
}

/// The live state file for one DUT — the ONE shared path derivation used by
/// `inventory_state_for` and the `dutabo list` table.
///
/// The name dir (`.dut-serial/<dut_name>/target-state`) wins when it holds a
/// file: the engine writes it on EVERY transition when the DUT has a name,
/// and a `TARGET_DUT_NAME` server FORCES its dut_dir to that same path — so
/// with an explicit `dut_dir` key the configured dir can hold a stale
/// leftover while the live state sits in the name dir (the source the
/// Python statusline reads). The configured `dut_dir` file is the fallback
/// (name-less engines write only there). A name-less DUT always uses the
/// configured dir — there is no name dir to consult.
pub fn dut_state_file(project_dir: &Path, dut: &crate::config::DutConfig) -> PathBuf {
    let configured = project_dir.join(&dut.dut_dir).join("target-state");
    if dut.dut_name.trim().is_empty() {
        return configured;
    }
    let name_file = project_dir
        .join(".dut-serial")
        .join(&dut.dut_name)
        .join("target-state");
    if name_file.exists() {
        name_file
    } else {
        configured
    }
}

/// Session-effective state for one DUT: the occupancy state when it
/// applies ("dutabo", "busy"), otherwise the device state. Occupancy is a
/// real state for the session — a guest cannot operate the console — not a
/// cosmetic mask layered over the device state.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionDisplay {
    pub state: String,
    pub text: String,
    pub plain: String,
}

/// Resolve the state a session effectively acts on — the ONE shared rule
/// for MCP responses and every inventory consumer:
/// * `dutabo` — the interactive CLI holds the port; applies to EVERY
///   session (guest or owner);
/// * `busy` — another agent session owns the MCP and this one is
///   read-only; applies to guest sessions only;
/// * otherwise the device state as-is.
pub fn session_display(state: &str, label: &str, guest: bool) -> SessionDisplay {
    let effective = if state == "dutabo" {
        "dutabo"
    } else if guest {
        "busy"
    } else {
        state
    };
    let text = if effective == "busy" {
        format!("\x1b[33m● {label}:busy\x1b[0m")
    } else {
        target_state_of(effective)
            .map(|state| {
                crate::state_manager::StateManager::format_statusline_labeled(state, label)
            })
            .unwrap_or_default()
    };
    let plain = String::from_utf8_lossy(&crate::ansi_strip::strip(text.as_bytes())).into_owned();
    SessionDisplay {
        state: effective.to_string(),
        text,
        plain,
    }
}

/// Build the inventory state entry for one DUT — the ONE shared derivation
/// used by `.dut-serial/inventory.json` (main.rs), `dutabo list --json` and
/// the `serial_list_duts` MCP tool. `state_text` is pre-formatted ANSI via
/// `StateManager::format_statusline_labeled` so the Python hooks render it
/// verbatim and never map state→color themselves. "unknown" has no
/// `TargetState`, so its statusline text is empty — the hooks must show
/// nothing, never a made-up healthy state.
pub fn inventory_state_for(
    project_dir: &Path,
    dut: &crate::config::DutConfig,
) -> crate::config::InventoryDutState {
    let state_file = dut_state_file(project_dir, dut);
    let view = read_dut_state(&state_file);
    let display = session_display(&view.state, &dut.dut_name, false);
    crate::config::InventoryDutState {
        state: view.state.clone(),
        state_label: view.label.clone(),
        state_text: display.text,
        state_plain: display.plain,
        age_secs: view.age_secs,
        critical: view.critical,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn session_display_resolves_the_occupancy_state_per_session() {
        for state in [
            "active",
            "booting",
            "uboot",
            "crashed",
            "DUT-off",
            "disconnected",
            "stopped",
            "unknown",
        ] {
            let owner = session_display(state, "rk3568-switch", false);
            assert_eq!(owner.state, state);
            let guest = session_display(state, "rk3568-switch", true);
            assert_eq!(guest.state, "busy");
            assert_eq!(guest.plain, "● rk3568-switch:busy");
            assert!(!guest.plain.contains('\x1b'));
        }
        for guest in [false, true] {
            let display = session_display("dutabo", "rk3568-switch", guest);
            assert_eq!(display.state, "dutabo");
            assert_eq!(display.plain, "● rk3568-switch:dutabo");
        }
    }

    use std::fs::File;
    use std::time::Duration;
    use tempfile::TempDir;

    fn state_file(dir: &TempDir, name: &str) -> std::path::PathBuf {
        dir.path().join(name)
    }

    fn write_state(dir: &TempDir, name: &str, content: &str) -> std::path::PathBuf {
        let path = state_file(dir, name);
        std::fs::write(&path, content).unwrap();
        path
    }

    fn set_mtime_ago(path: &std::path::Path, secs: u64) {
        let file = File::open(path).unwrap();
        file.set_modified(SystemTime::now() - Duration::from_secs(secs))
            .unwrap();
    }

    fn set_mtime_future(path: &std::path::Path, secs: u64) {
        let file = File::open(path).unwrap();
        file.set_modified(SystemTime::now() + Duration::from_secs(secs))
            .unwrap();
    }

    /// TDD-1: every TargetState::as_str value maps to itself through
    /// classify_state, with whitespace trimmed.
    #[test]
    fn classify_known_state_vocabulary_maps_to_itself() {
        for state in KNOWN_STATES {
            assert_eq!(classify_state(state), state, "exact match failed");
            // Files may carry a trailing newline or surrounding whitespace.
            assert_eq!(
                classify_state(&format!("  {state}\n")),
                state,
                "whitespace-trimmed match failed"
            );
        }
    }

    /// TDD-1: empty / unrecognized / case-mismatched text is never a state.
    #[test]
    fn classify_unknown_text_falls_back_to_unknown() {
        for text in ["", "   ", "\n", "garbage", "ACTIVE", "Active", "crashed!"] {
            assert_eq!(classify_state(text), "unknown", "input {text:?}");
        }
    }

    /// The classification buckets and critical flag must agree with the
    /// statusline color groups (red = the three critical states).
    #[test]
    fn classification_buckets_and_critical_flag() {
        for state in ["crashed", "DUT-off", "disconnected"] {
            assert_eq!(classification_of(state), "critical", "{state}");
            assert!(is_critical(state), "{state}");
            assert_eq!(state_label(state), format!("✗ {state}"));
        }
        for (state, bucket) in [
            ("booting", "transitional"),
            ("uboot", "transitional"),
            ("active", "online"),
            ("dutabo", "online"),
            ("stopped", "offline"),
            ("unknown", "unknown"),
        ] {
            assert_eq!(classification_of(state), bucket, "{state}");
            assert!(!is_critical(state), "{state}");
            assert_eq!(state_label(state), state, "{state}");
        }
    }

    /// target_state_of must round-trip the whole vocabulary back to
    /// TargetState (consumers need the enum for ANSI statusline text).
    #[test]
    fn target_state_of_round_trips_vocabulary() {
        for state in KNOWN_STATES {
            let target = target_state_of(state).unwrap_or_else(|| panic!("{state} unmapped"));
            assert_eq!(target.as_str(), state);
        }
        assert_eq!(target_state_of("unknown"), None);
        assert_eq!(target_state_of(""), None);
    }

    /// TDD-1: missing file → "unknown", missing flag, no age, never stale,
    /// never critical.
    #[test]
    fn read_missing_file_is_unknown_and_never_healthy() {
        let dir = TempDir::new().unwrap();
        let view = read_dut_state(&state_file(&dir, "target-state"));
        assert_eq!(view.state, "unknown");
        assert_eq!(view.classification, "unknown");
        assert!(view.missing);
        assert_eq!(view.age_secs, None);
        assert!(!view.stale);
        assert!(!view.critical);
        assert_eq!(view.label, "unknown");
    }

    /// TDD-1: an empty file (exists but no content) is unknown, not healthy,
    /// and not "missing".
    #[test]
    fn read_empty_file_is_unknown_but_not_missing() {
        let dir = TempDir::new().unwrap();
        let path = write_state(&dir, "target-state", "");
        let view = read_dut_state(&path);
        assert_eq!(view.state, "unknown");
        assert!(!view.missing);
        assert!(view.age_secs.is_some());
        assert!(!view.stale);
        assert!(!view.critical);
    }

    /// TDD-1: unrecognized text in an existing file is unknown.
    #[test]
    fn read_unrecognized_text_is_unknown() {
        let dir = TempDir::new().unwrap();
        let path = write_state(&dir, "target-state", "kernel-panic-loop");
        let view = read_dut_state(&path);
        assert_eq!(view.state, "unknown");
        assert!(!view.missing);
        assert!(!view.critical);
    }

    /// TDD-1: a path that exists but cannot be read (directory here — works
    /// even as root, unlike a chmod 000 file) is unknown, not missing.
    #[test]
    fn read_unreadable_path_is_unknown_but_not_missing() {
        let dir = TempDir::new().unwrap();
        let view = read_dut_state(dir.path());
        assert_eq!(view.state, "unknown");
        assert!(!view.missing);
        assert!(!view.critical);
    }

    /// TDD-1: age comes from mtime; the three critical states stay critical
    /// regardless of age (a crash is always prominent).
    #[test]
    fn read_computes_age_and_marks_critical_states() {
        let dir = TempDir::new().unwrap();
        let path = write_state(&dir, "target-state", "crashed");
        set_mtime_ago(&path, 42);
        let view = read_dut_state(&path);
        assert_eq!(view.state, "crashed");
        assert_eq!(view.classification, "critical");
        assert!(view.critical);
        assert_eq!(view.label, "✗ crashed");
        let age = view.age_secs.expect("existing file must have an age");
        // Small slack for the time between set_mtime and read.
        assert!((42..=44).contains(&age), "age {age} not ~42s");
        assert!(!view.stale);
    }

    /// TDD-1: staleness flips at exactly STALE_THRESHOLD_SECS.
    #[test]
    fn read_marks_stale_at_threshold() {
        let dir = TempDir::new().unwrap();
        let just_fresh = write_state(&dir, "just-fresh", "active");
        let stale = write_state(&dir, "stale", "active");
        let way_stale = write_state(&dir, "way-stale", "active");
        set_mtime_ago(&just_fresh, STALE_THRESHOLD_SECS - 1);
        set_mtime_ago(&stale, STALE_THRESHOLD_SECS);
        set_mtime_ago(&way_stale, STALE_THRESHOLD_SECS + 1);
        assert!(!read_dut_state(&just_fresh).stale);
        assert!(read_dut_state(&stale).stale);
        assert!(read_dut_state(&way_stale).stale);
        assert_eq!(AGE_SUFFIX_THRESHOLD_SECS, 600);
    }

    /// TDD-1: an mtime in the future (clock skew) yields no age and never
    /// stale — must not be reported as an ancient file.
    #[test]
    fn read_future_mtime_yields_no_age() {
        let dir = TempDir::new().unwrap();
        let path = write_state(&dir, "target-state", "active");
        set_mtime_future(&path, 3600);
        let view = read_dut_state(&path);
        assert_eq!(view.state, "active");
        assert_eq!(view.age_secs, None);
        assert!(!view.stale);
    }

    /// Every known state file round-trips through read_dut_state unchanged.
    #[test]
    fn read_known_states_round_trip() {
        let dir = TempDir::new().unwrap();
        for state in KNOWN_STATES {
            let path = write_state(&dir, &format!("target-{state}"), state);
            let view = read_dut_state(&path);
            assert_eq!(view.state, state, "round-trip failed");
            assert!(!view.missing);
            assert_eq!(view.critical, is_critical(state));
        }
    }

    // ── age suffix + STATE cell (table rendering helpers) ───────────────

    /// TDD-14: the age suffix appears at >= 600s and never below; units
    /// progress minutes → hours → days.
    #[test]
    fn age_suffix_threshold_and_units() {
        assert_eq!(format_age_suffix(0), None);
        assert_eq!(format_age_suffix(AGE_SUFFIX_THRESHOLD_SECS - 1), None);
        assert_eq!(
            format_age_suffix(AGE_SUFFIX_THRESHOLD_SECS),
            Some("10m ago".into())
        );
        assert_eq!(format_age_suffix(2_520), Some("42m ago".into()));
        assert_eq!(format_age_suffix(3_600), Some("1h ago".into()));
        assert_eq!(format_age_suffix(5 * 3_600), Some("5h ago".into()));
        assert_eq!(format_age_suffix(86_400), Some("1d ago".into()));
    }

    /// TDD-14: STATE cells append the age suffix, but a missing file renders
    /// plain "unknown" (its age is None — never "unknown (missing)").
    #[test]
    fn state_cell_appends_age_suffix() {
        let dir = TempDir::new().unwrap();
        let active = write_state(&dir, "active", "active");
        set_mtime_ago(&active, 2_520);
        assert_eq!(
            format_state_cell(&read_dut_state(&active)),
            "active (42m ago)"
        );
        let crashed = write_state(&dir, "crashed", "crashed");
        set_mtime_ago(&crashed, 5 * 3_600);
        assert_eq!(
            format_state_cell(&read_dut_state(&crashed)),
            "✗ crashed (5h ago)"
        );
        // Fresh file: no suffix.
        let fresh = write_state(&dir, "fresh", "active");
        assert_eq!(format_state_cell(&read_dut_state(&fresh)), "active");
        // Missing file: plain "unknown".
        let view = read_dut_state(&dir.path().join("nope"));
        assert_eq!(format_state_cell(&view), "unknown");
    }

    /// Test-local DutConfig — only dut_name/dut_dir matter for state derivation.
    fn test_dut(name: &str, dut_dir: &str) -> crate::config::DutConfig {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let serial_port = listener.local_addr().unwrap().port().to_string();
        drop(listener);
        crate::config::DutConfig {
            dut_name: name.into(),
            dev_host_ip: "10.0.0.1".into(),
            dev_host_user: "test".into(),
            dev_host_pass: String::new(),
            serial_port,
            relay_ip: String::new(),
            relay_type: "ch340".into(),
            dev_ctl: String::new(),
            relay_port: 0,
            reset_ch: 0,
            download_ch: 0,
            power_ch: 0,
            power_off_time_ms: 3000,
            reference_log: String::new(),
            dut_dir: dut_dir.into(),
            login_user: "root".into(),
            login_pass: String::new(),
            learner_stage_threshold: 0.0,
            learner_crash_threshold: 0.0,
            crash_patterns: Vec::new(),
            section_values: std::collections::HashMap::new(),
        }
    }

    /// The ONE shared derivation behind `dutabo list --json` and the server
    /// inventory: state_text equals the statusline ANSI text, state_plain
    /// strips every escape, and "unknown" has empty statusline text.
    #[test]
    fn inventory_state_for_uses_statusline_text() {
        let dir = TempDir::new().unwrap();
        let name_dir = dir.path().join(".dut-serial/rk3576");
        std::fs::create_dir_all(&name_dir).unwrap();
        write_state(&dir, ".dut-serial/rk3576/target-state", "crashed");
        let dut = test_dut("rk3576", ".dut-serial/rk3576");
        let state = inventory_state_for(dir.path(), &dut);
        assert_eq!(state.state, "crashed");
        assert_eq!(state.state_label, "✗ crashed");
        assert!(state.critical);
        let expected = crate::state_manager::StateManager::format_statusline_labeled(
            crate::state_manager::TargetState::Crashed,
            "rk3576",
        );
        assert_eq!(state.state_text, expected);
        assert!(!state.state_plain.contains('\x1b'));
        assert!(state.state_plain.contains("rk3576:crashed"));

        // Missing state file → unknown, age None, not critical, empty text.
        let ghost = test_dut("ghost", ".dut-serial/ghost");
        let state = inventory_state_for(dir.path(), &ghost);
        assert_eq!(state.state, "unknown");
        assert_eq!(state.state_label, "unknown");
        assert_eq!(state.age_secs, None);
        assert!(!state.critical);
        assert_eq!(state.state_text, "");
    }

    /// Stopped renders nothing on the statusline (format_statusline_labeled
    /// returns ""), but the JSON still carries state="stopped" so consumers
    /// can distinguish it from unknown.
    #[test]
    fn inventory_state_for_stopped_has_empty_statusline() {
        let dir = TempDir::new().unwrap();
        let name_dir = dir.path().join(".dut-serial/d1");
        std::fs::create_dir_all(&name_dir).unwrap();
        write_state(&dir, ".dut-serial/d1/target-state", "stopped");
        let dut = test_dut("d1", ".dut-serial/d1");
        let state = inventory_state_for(dir.path(), &dut);
        assert_eq!(state.state, "stopped");
        assert_eq!(state.state_text, "");
        assert_eq!(state.state_plain, "");
        assert!(!state.critical);
    }

    // ── live state file preference (name dir vs configured dut_dir) ────

    /// A TARGET_DUT_NAME server FORCES dut_dir to `.dut-serial/<name>`, so
    /// with an explicit `dut_dir` key the engine's live state lives in the
    /// name dir — the shared reader must prefer it over the configured dir
    /// (the source the Python statusline reads).
    #[test]
    fn inventory_state_for_prefers_name_dir_over_explicit_dut_dir() {
        let dir = TempDir::new().unwrap();
        // The live server (TARGET_DUT_NAME=board, forced dut_dir) writes
        // here; the explicit dir holds a stale leftover.
        let name_dir = dir.path().join(".dut-serial/board");
        std::fs::create_dir_all(&name_dir).unwrap();
        write_state(&dir, ".dut-serial/board/target-state", "active");
        let explicit_dir = dir.path().join("explicitdir");
        std::fs::create_dir_all(&explicit_dir).unwrap();
        write_state(&dir, "explicitdir/target-state", "crashed");
        let dut = test_dut("board", "explicitdir");
        let state = inventory_state_for(dir.path(), &dut);
        assert_eq!(state.state, "active", "name dir must win over dut_dir");
        assert!(!state.critical);
        assert_eq!(state.state_label, "active");
    }

    /// Without a name-dir file the configured `dut_dir` is the fallback
    /// (name-less engines write only there).
    #[test]
    fn inventory_state_for_falls_back_to_dut_dir_when_name_dir_missing() {
        let dir = TempDir::new().unwrap();
        let explicit_dir = dir.path().join("explicitdir");
        std::fs::create_dir_all(&explicit_dir).unwrap();
        write_state(&dir, "explicitdir/target-state", "crashed");
        let dut = test_dut("board", "explicitdir");
        let state = inventory_state_for(dir.path(), &dut);
        assert_eq!(state.state, "crashed");
        assert!(state.critical);
        assert_eq!(state.state_label, "✗ crashed");
    }

    /// The path helper itself: name dir when the file exists, configured
    /// dir otherwise, and configured dir always for a name-less DUT.
    #[test]
    fn dut_state_file_preference_rules() {
        let dir = TempDir::new().unwrap();
        let name_dir = dir.path().join(".dut-serial/board");
        std::fs::create_dir_all(&name_dir).unwrap();
        write_state(&dir, ".dut-serial/board/target-state", "active");
        let dut = test_dut("board", "explicitdir");
        assert_eq!(
            dut_state_file(dir.path(), &dut),
            name_dir.join("target-state")
        );
        let ghost = test_dut("ghost", "explicitdir2");
        assert_eq!(
            dut_state_file(dir.path(), &ghost),
            dir.path().join("explicitdir2/target-state")
        );
        // Name-less DUT: the configured dir, never a mangled name path.
        let anon = test_dut("", "custom");
        assert_eq!(
            dut_state_file(dir.path(), &anon),
            dir.path().join("custom/target-state")
        );
    }
}
