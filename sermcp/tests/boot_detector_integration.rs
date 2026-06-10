//! Integration tests for boot stage detection.
//!
//! Verifies that the regex-based BootStageDetector correctly identifies each
//! stage in a complete RK3576 boot log, and that the StageLearner can extract
//! fingerprints from a reference log and classify novel lines.

use sermcp::boot_detector::{BootEvent, BootStageDetector, StageLearner};
use std::path::Path;

/// Extract stage names from events in order, filtering to Stage events only.
fn extract_stage_names(events: &[BootEvent]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|e| match e {
            BootEvent::Stage(name) => Some(name.as_str()),
            _ => None,
        })
        .collect()
}

/// Check whether `events` contains a BootStart event.
fn has_boot_start(events: &[BootEvent]) -> bool {
    events.iter().any(|e| matches!(e, BootEvent::BootStart))
}

// ── Regex-based detection ──────────────────────────────────────────────

#[test]
fn test_rk3576_full_boot_sequence() {
    let log = include_str!("fixtures/rk3576-boot.log");
    let mut detector = BootStageDetector::new();
    let events = detector.feed(log.as_bytes());

    let stages = extract_stage_names(&events);
    // DDR and SPL both trigger BootStart (rotate_log action), not Stage.
    // The rest should appear in order: bl31 → optee → uboot → kernel → shell
    assert!(
        stages.contains(&"bl31"),
        "Expected BL31 stage, got: {stages:?}"
    );
    assert!(
        stages.contains(&"optee"),
        "Expected OP-TEE stage, got: {stages:?}"
    );
    assert!(
        stages.contains(&"uboot"),
        "Expected U-Boot stage, got: {stages:?}"
    );
    assert!(
        stages.contains(&"kernel"),
        "Expected kernel stage, got: {stages:?}"
    );
    assert!(
        stages.contains(&"shell"),
        "Expected shell stage, got: {stages:?}"
    );
    assert!(has_boot_start(&events), "Expected BootStart from DDR/SPL");
}

#[test]
fn test_kernel_panic_detected_in_full_log() {
    let log = "Linux version 6.1.0\nKernel panic - not syncing: Fatal exception\n---[ end trace 0000000000000000 ]---\n";
    let mut detector = BootStageDetector::new();
    let events = detector.feed(log.as_bytes());

    let crash_count = events
        .iter()
        .filter(|e| matches!(e, BootEvent::Crash(_, _)))
        .count();
    // "Kernel panic" triggers one crash; "end trace" triggers another
    // (both are crash patterns), but the throttle prevents duplicates
    // within 2s. In a single feed call they arrive in the same batch,
    // so both fire.
    assert!(
        crash_count <= 2,
        "Expected ≤2 crash events, got {crash_count}"
    );
    assert!(crash_count >= 1, "Expected at least 1 crash event");
}

// ── StageLearner integration ───────────────────────────────────────────

#[test]
fn test_stage_learner_from_fixture() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rk3576-boot.log");
    let mut learner = StageLearner::from_reference(&path).expect("Should load reference log");

    assert!(
        !learner.fingerprints.is_empty(),
        "Should extract fingerprints"
    );

    // Classify known lines.
    assert_eq!(
        learner.classify_line("U-Boot SPL 2024.01"),
        Some("spl".to_string())
    );
    assert_eq!(
        learner.classify_line("U-Boot 2024.01 (Jan 01 2025 - 00:00:00)"),
        Some("uboot".to_string())
    );
    assert_eq!(
        learner.classify_line("Linux version 6.1.0 (gcc 12.2.0)"),
        Some("kernel".to_string())
    );
    assert_eq!(
        learner.classify_line("Kernel panic - not syncing: Fatal exception"),
        Some("crash".to_string())
    );
}

#[test]
fn test_stage_learner_rejects_noise() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rk3576-boot.log");
    let mut learner = StageLearner::from_reference(&path).unwrap();

    // Noise should not match any stage.
    assert_eq!(learner.classify_line("random noise 0xDEADBEEF"), None);
    assert_eq!(learner.classify_line(""), None);
}

#[test]
fn test_stage_learner_order_constraint() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rk3576-boot.log");
    let mut learner = StageLearner::from_reference(&path).unwrap();

    // Normal forward progression.
    assert_eq!(
        learner.classify_line("DDR Version 1.08 20230501"),
        Some("ddr".to_string())
    );
    assert_eq!(
        learner.classify_line("U-Boot SPL 2024.01"),
        Some("spl".to_string())
    );

    // Try to go backwards from SPL(order=1) to DDR(order=0).
    // cur_order(0) + 1 = 1, last_order(1) = 1, 1 < 1 is false → allowed.
    // This is the "small regression" carve-out (1 stage).
    let ddr_again = learner.classify_line("DDR Version 1.08 20230501");
    assert_eq!(ddr_again, Some("ddr".to_string()));

    // Crash bypasses order constraint.
    assert_eq!(
        learner.classify_line("Kernel panic - not syncing: No working init found"),
        Some("crash".to_string())
    );
}
