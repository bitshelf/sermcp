//! Connection learner — verifies over repeated learning cycles (count
//! configurable 2–10 per request) that the serial connection is genuinely
//! established.
//!
//! # Hardware reset learning flow
//!
//! 1. Press reset → create a learning log file
//! 2. Release reset → capture the boot log
//! 3. Repeat for the requested cycle count (2–10; the run ends early once
//!    the similarity verdict is decided)
//! 4. Compare the text similarity of the first 50 lines of the cycle files
//! 5. All pairwise similarities > 93% → keep the last one as reference_log
//!
//! # Software reboot learning flow (fallback)
//!
//! 1. Send reboot → create a learning log (first line is fixed "reboot")
//! 2. Capture the reboot log
//! 3. Repeat for the requested cycle count (2–10)
//! 4. Similarity > 93% → connection is considered established
//!
//! # Relay-unavailable detection
//!
//! - First-50-line similarity < 10% after the hardware-reset cycles → the
//!   reset relay is deemed unavailable
//! - Fall back to software reboot learning

use std::path::PathBuf;
use std::sync::LazyLock;

/// Per-DUT retention limits for transient connection-learning captures.
/// Reference logs are stored separately and are not subject to this cleanup.
pub const MAX_LEARN_LOG_FILES: usize = 20;
pub const MAX_LEARN_LOG_FILE_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_LEARN_LOG_TOTAL_BYTES: u64 = 16 * 1024 * 1024;

static RE_VOLATILE_HEX: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)0x[0-9a-f]+").unwrap());
// Mask every digit run including its decimal fraction. DDR training
// values are mostly SINGLE-digit (`p0 n3`, `Die BW=8`) and fractional
// (`Vref:24.9%`); a 2+-digit-only mask left every DQ roc / DQS roc /
// Vref line unmasked, so comparing a fresh boot against a days-old
// reference failed on pure per-boot drift.
static RE_VOLATILE_NUMBER: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\d+(?:\.\d+)?").unwrap());

/// Result of a single learning cycle.
#[derive(Debug, Clone)]
pub struct LearnCycle {
    /// Path to the captured log file.
    pub log_path: PathBuf,
    /// First 50 lines (normalized) for similarity comparison.
    pub first_50: String,
    /// Full captured content.
    pub full_content: String,
    /// Whether this was a hardware reset or software reboot.
    pub method: LearnMethod,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LearnMethod {
    HardwareReset,
    SoftwareReboot,
}

/// Overall learning result.
#[derive(Debug)]
pub struct LearnResult {
    /// Whether the connection is verified.
    pub connected: bool,
    /// The learning cycles performed (count configurable 2–10).
    pub cycles: Vec<LearnCycle>,
    /// Best similarity score across all pairwise comparisons.
    pub best_similarity: f64,
    /// Path to the generated reference log (if learning succeeded).
    pub reference_log: Option<PathBuf>,
    /// Learning method used.
    pub method: LearnMethod,
    /// Whether relay is verified working.
    pub relay_verified: bool,
    /// Error message if learning failed.
    pub error: Option<String>,
}

/// Configuration for the learning process.
#[derive(Debug, Clone)]
pub struct LearnConfig {
    /// Directory for learning logs: `.dut-serial/learn/`
    pub learn_dir: PathBuf,
    /// Where to write the reference log on success.
    pub reference_log: PathBuf,
    /// Similarity threshold for success (0.0–1.0, default 0.93).
    pub similarity_threshold: f64,
    /// Similarity threshold below which relay is considered broken (default 0.10).
    pub relay_broken_threshold: f64,
    /// Number of learning cycles (default 3).
    pub cycles: usize,
    /// Number of lines to compare from the top of each log.
    pub compare_lines: usize,
    /// Relay reset pulse duration in ms (default 500).
    pub reset_pulse_ms: u64,
    /// Post-reset capture duration in seconds (default 30).
    pub capture_timeout_secs: f64,
    /// Optional meaningful-line limit for a prefix-only capture. Normal
    /// learning records a complete boot window; the init TEST profile only
    /// needs the exact prefix consumed by the similarity comparison.
    pub capture_line_limit: Option<usize>,
}

/// Compile-time defaults from Cargo.toml `[package.metadata.learn]`.
/// These are set by build.rs as env vars; if missing, use hardcoded fallbacks.
macro_rules! learn_default {
    ($env:expr, $default:expr) => {
        option_env!($env)
            .and_then(|s| s.parse().ok())
            .unwrap_or($default)
    };
}

impl Default for LearnConfig {
    fn default() -> Self {
        Self {
            learn_dir: PathBuf::from(".dut-serial/learn"),
            reference_log: PathBuf::from(".dut-serial/reference-boot.log"),
            similarity_threshold: learn_default!("LEARN_SIMILARITY_THRESHOLD", 0.93),
            relay_broken_threshold: learn_default!("LEARN_RELAY_BROKEN_THRESHOLD", 0.10),
            cycles: learn_default!("LEARN_CYCLES", 3),
            compare_lines: learn_default!("LEARN_COMPARE_LINES", 50),
            reset_pulse_ms: learn_default!("LEARN_RESET_PULSE_MS", 500),
            capture_timeout_secs: learn_default!("LEARN_CAPTURE_TIMEOUT_SECS", 30.0),
            capture_line_limit: None,
        }
    }
}

/// The connection learner orchestrates multi-cycle learning.
pub struct ConnectionLearner {
    config: LearnConfig,
}

impl ConnectionLearner {
    pub fn new(config: LearnConfig) -> Self {
        Self { config }
    }

    /// Access the learning configuration.
    pub fn config(&self) -> &LearnConfig {
        &self.config
    }

    /// Evaluate learning cycles (public entry point for inline learning in serial_engine).
    pub fn evaluate(&self, cycles: Vec<LearnCycle>, method: LearnMethod) -> LearnResult {
        self.evaluate_cycles(cycles, method)
    }

    /// Minimum pairwise similarity across the cycles collected SO FAR.
    /// Returns None while the samples are not yet comparable — fewer than 2
    /// cycles, or any cycle captured nothing (the same conditions
    /// [`Self::evaluate_cycles`] treats as failures).
    pub fn similarity_so_far(&self, cycles: &[LearnCycle]) -> Option<f64> {
        if cycles.len() < 2 {
            return None;
        }
        if cycles.iter().any(|c| c.first_50.trim().is_empty()) {
            return None;
        }
        let mut min_similarity = 1.0f64;
        for i in 0..cycles.len() {
            for j in (i + 1)..cycles.len() {
                let sim = compute_similarity(&cycles[i].first_50, &cycles[j].first_50);
                if sim < min_similarity {
                    min_similarity = sim;
                }
            }
        }
        Some(min_similarity)
    }

    /// Early-exit gate for the learning loop: the cycle COUNT is never the
    /// pass criterion — learning succeeds the moment the collected cycles'
    /// pairwise similarity reaches the threshold (≥93% default), so the
    /// engine can stop capturing and skip the remaining cycles.
    pub fn threshold_met(&self, cycles: &[LearnCycle]) -> bool {
        self.similarity_so_far(cycles)
            .is_some_and(|sim| sim >= self.config.similarity_threshold)
    }

    /// Evaluate learning cycles: compute pairwise similarity, check thresholds.
    fn evaluate_cycles(&self, cycles: Vec<LearnCycle>, method: LearnMethod) -> LearnResult {
        let min_similarity = match self.similarity_so_far(&cycles) {
            Some(sim) => sim,
            None if cycles.len() < 2 => {
                return LearnResult {
                    connected: false,
                    cycles,
                    best_similarity: 0.0,
                    reference_log: None,
                    method,
                    relay_verified: false,
                    error: Some("Need at least 2 cycles for comparison".into()),
                };
            }
            // A cycle that captured NOTHING is a broken cycle (serial
            // console disconnected, DUT off, reset not wired) — learning
            // from silence would produce an empty reference that
            // OVERWRITES a good one (empty-vs-empty similarity is 100%).
            // Fail loudly instead.
            None => {
                return LearnResult {
                    connected: false,
                    cycles,
                    best_similarity: 0.0,
                    reference_log: None,
                    method,
                    relay_verified: false,
                    error: Some(
                        "No serial data captured in at least one learn cycle — check the serial \
                         console connection"
                            .into(),
                    ),
                };
            }
        };

        let relay_verified = match method {
            LearnMethod::HardwareReset => min_similarity >= self.config.relay_broken_threshold,
            LearnMethod::SoftwareReboot => false, // software reboot has no relay
        };

        let connected = min_similarity >= self.config.similarity_threshold;

        // If successful, write the last cycle as the reference — truncated
        // at the "start kernel" marker and capped at REFERENCE_MAX_LINES
        // (the boot STRUCTURE worth segmenting on, without the kernel log
        // tail).
        let reference_log = if connected {
            let last = cycles.last().unwrap();
            let window = reference_window(&last.full_content);
            if let Some(parent) = self.config.reference_log.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            match std::fs::write(&self.config.reference_log, &window) {
                Ok(()) => {
                    tracing::info!(
                        "Reference log written: {} ({} bytes)",
                        self.config.reference_log.display(),
                        window.len()
                    );
                    Some(self.config.reference_log.clone())
                }
                Err(e) => {
                    tracing::error!("Failed to write reference log: {e}");
                    None
                }
            }
        } else {
            None
        };

        let error = if !connected {
            Some(format!(
                "Similarity {:.1}% below threshold {:.1}%",
                min_similarity * 100.0,
                self.config.similarity_threshold * 100.0
            ))
        } else {
            None
        };

        LearnResult {
            connected,
            best_similarity: min_similarity,
            reference_log,
            method,
            relay_verified,
            error,
            cycles,
        }
    }

    /// Normalize text for similarity comparison:
    /// - Strip ANSI escape codes
    /// - Strip NUL bytes
    /// - Strip timestamps (e.g. `[    0.000000]`)
    /// - Compress whitespace
    /// - Lowercase
    pub fn normalize(text: &str) -> String {
        // Strip ANSI escapes
        let cleaned = strip_str(text);
        // Strip NUL bytes
        let cleaned = cleaned.replace('\0', "");
        // Strip kernel timestamps: [   12.345678]
        let re_ts = regex::Regex::new(r"\[\s*\d+\.\d+\]").unwrap();
        let cleaned = re_ts.replace_all(&cleaned, "").to_string();
        // Strip Android log timestamps: [ 1234.567890][  T123]
        let re_android_ts = regex::Regex::new(r"\[\s*\d+\.\d+\]\[\s*T\d+\]\s*").unwrap();
        let cleaned = re_android_ts.replace_all(&cleaned, "").to_string();
        // DDR training values, addresses, counters, checksums and timings
        // change on every healthy boot. Mask them using the same rules as the
        // init direct probe before measuring structural similarity.
        let cleaned = RE_VOLATILE_HEX.replace_all(&cleaned, "#H");
        let cleaned = RE_VOLATILE_NUMBER.replace_all(&cleaned, "#N");
        // Compress whitespace
        let re_ws = regex::Regex::new(r"\s+").unwrap();
        let cleaned = re_ws.replace_all(&cleaned, " ").to_string();
        cleaned.trim().to_lowercase()
    }

    /// Extract the first `n` lines from text.
    pub fn extract_first_n_lines(text: &str, n: usize) -> String {
        text.lines().take(n).collect::<Vec<_>>().join("\n")
    }
}

/// Remove the oldest transient learning logs until both the file-count and
/// total-size limits are satisfied. Unrelated files are never touched.
pub fn prune_learn_logs(learn_dir: &std::path::Path) -> std::io::Result<usize> {
    let mut logs = Vec::new();
    let entries = match std::fs::read_dir(learn_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };

    for entry in entries {
        let entry = entry?;
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        if !name.starts_with("learn-") || !name.ends_with(".log") {
            continue;
        }
        let metadata = entry.metadata()?;
        if metadata.is_file() {
            logs.push((file_name, entry.path(), metadata.len()));
        }
    }

    logs.sort_by(|left, right| left.0.cmp(&right.0));
    let mut total_bytes: u64 = logs.iter().map(|entry| entry.2).sum();
    let mut remaining = logs.len();
    let mut removed = 0;
    for (_, path, size) in logs {
        if remaining <= MAX_LEARN_LOG_FILES && total_bytes <= MAX_LEARN_LOG_TOTAL_BYTES {
            break;
        }
        std::fs::remove_file(path)?;
        remaining -= 1;
        total_bytes = total_bytes.saturating_sub(size);
        removed += 1;
    }
    Ok(removed)
}

/// Maximum number of lines the reference boot log keeps.
pub const REFERENCE_MAX_LINES: usize = 100;

/// Truncate a captured boot log to the reference window: keep lines up to
/// (and including) the "start kernel" marker, then cap at
/// [`REFERENCE_MAX_LINES`] — the boot STRUCTURE worth segmenting on, without
/// the kernel log tail (reference_log keeps at
/// most up to start-kernel, no more than 100 lines).
pub fn reference_window(full: &str) -> String {
    let mut lines: Vec<&str> = full.lines().collect();
    if let Some(pos) = lines.iter().position(|l| {
        let t = l.trim().to_ascii_lowercase();
        t.contains("starting kernel") || t.contains("start kernel")
    }) {
        lines.truncate(pos + 1);
    }
    lines.truncate(REFERENCE_MAX_LINES);
    lines.join("\n")
}

/// Compute combined similarity score between two normalized texts.
///
/// `score = jaro_winkler * 0.6 + jaccard_3gram * 0.4`
pub fn compute_similarity(a: &str, b: &str) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }

    let jaro = strsim::jaro_winkler(a, b);
    let jaccard = similarity::jaccard_3gram(a, b);
    jaro * 0.6 + jaccard * 0.4
}

// 3-gram Jaccard and ANSI stripping live in their shared modules
// (`similarity`, `ansi_strip`) — byte-identical copies were consolidated.
use crate::ansi_strip::strip_str;
use crate::similarity;

#[cfg(test)]
mod tests {
    use super::*;

    /// First-N prefix the quick reference comparison uses (mirrors
    /// LearnConfig::compare_lines' default).
    const DEFAULT_COMPARE_LINES_FOR_TEST: usize = 50;

    #[test]
    fn test_normalize_strips_ansi() {
        let input = "\x1b[31mERROR\x1b[0m: something";
        let result = ConnectionLearner::normalize(input);
        assert!(!result.contains("\x1b"));
        assert!(result.contains("error"));
    }

    #[test]
    fn test_normalize_strips_timestamps() {
        let input = "[    0.000000] Booting Linux on CPU 0";
        let result = ConnectionLearner::normalize(input);
        assert!(!result.contains("[    0.000000]"));
        assert!(result.contains("booting linux"));
    }

    #[test]
    fn test_normalize_masks_volatile_boot_values() {
        let a = ConnectionLearner::normalize(
            "DDR 2f85 build 20260824 addr=0xfde00000 Vref:29.2% time 170.406ms",
        );
        let b = ConnectionLearner::normalize(
            "DDR 2f85 build 20260825 addr=0xabc01234 Vref:31.2% time 171.923ms",
        );
        assert_eq!(a, b);
    }

    #[test]
    fn test_normalize_compresses_whitespace() {
        let input = "hello    world  \t  foo";
        let result = ConnectionLearner::normalize(input);
        assert_eq!(result, "hello world foo");
    }

    use proptest::prelude::*;

    proptest! {
        /// Normalization is idempotent: masking an already-masked text
        /// changes nothing (digit runs become `#n` and stay `#n`).
        #[test]
        fn normalize_is_idempotent(raw in "[a-z0-9 .:%x,\n]{0,200}") {
            let once = ConnectionLearner::normalize(&raw);
            prop_assert_eq!(ConnectionLearner::normalize(&once), once);
        }

        /// Similarity is symmetric and bounded — a pure distance-like
        /// function over normalized text.
        #[test]
        fn similarity_symmetric_and_bounded(
            a in "[a-z0-9 .:\n]{1,120}",
            b in "[a-z0-9 .:\n]{1,120}",
        ) {
            let (x, y) = (ConnectionLearner::normalize(&a), ConnectionLearner::normalize(&b));
            let ab = compute_similarity(&x, &y);
            let ba = compute_similarity(&y, &x);
            prop_assert!((ab - ba).abs() < 1e-9);
            prop_assert!((0.0..=1.0).contains(&ab));
        }

    }

    #[test]
    fn test_normalize_lowercases() {
        let input = "U-Boot SPL 2024.01";
        let result = ConnectionLearner::normalize(input);
        // Digit runs mask including the decimal fraction: 2024.01 is one
        // volatile number, not two.
        assert_eq!(result, "u-boot spl #n");
    }

    #[test]
    fn test_extract_first_n_lines() {
        let text = "line1\nline2\nline3\nline4\nline5\n";
        let result = ConnectionLearner::extract_first_n_lines(text, 3);
        assert_eq!(result.lines().count(), 3);
        assert!(result.starts_with("line1"));
    }

    #[test]
    fn test_reference_window_truncates_at_start_kernel() {
        // 60 pre-kernel lines, marker at line 61, then a long kernel tail:
        // the window must end at the marker (within the 100-line cap).
        let boot = format!(
            "{}\nStarting kernel ...\n{}",
            "DDR init\nU-Boot SPL\n".repeat(30),
            "kernel log line\n".repeat(200)
        );
        let window = reference_window(&boot);
        let lines: Vec<&str> = window.lines().collect();
        assert!(
            window.contains("Starting kernel ..."),
            "the marker line itself is kept"
        );
        let pos = lines
            .iter()
            .position(|l| l.contains("Starting kernel"))
            .unwrap();
        assert_eq!(
            pos + 1,
            lines.len(),
            "nothing after the start-kernel marker is kept"
        );
        assert!(lines.len() <= REFERENCE_MAX_LINES, "within the line budget");
    }

    #[test]
    fn test_reference_window_caps_at_100_when_marker_is_late() {
        // The marker sits beyond the 100-line cap: only the first
        // REFERENCE_MAX_LINES pre-kernel lines are kept.
        let boot = format!(
            "{}\nStarting kernel ...\n{}",
            "DDR init\nU-Boot SPL\n".repeat(200),
            "kernel log line\n".repeat(50)
        );
        let window = reference_window(&boot);
        assert_eq!(
            window.lines().count(),
            REFERENCE_MAX_LINES,
            "hard 100-line cap"
        );
        assert!(!window.contains("Starting kernel"), "late marker cut off");
    }

    #[test]
    fn test_reference_window_short_boot_kept_whole() {
        let boot = "DDR init\nU-Boot SPL\nU-Boot 2024.01\n";
        assert_eq!(reference_window(boot), boot.trim_end_matches('\n'));
    }

    #[test]
    fn test_compute_similarity_identical() {
        let text = "u-boot spl 2024.01 ddr version 1.08";
        let score = compute_similarity(text, text);
        assert!((score - 1.0).abs() < 0.01, "expected ~1.0, got {score}");
    }

    #[test]
    fn test_compute_similarity_different() {
        let a = "u-boot spl 2024.01 ddr version 1.08";
        let b = "linux version 6.1.0 starting kernel";
        let score = compute_similarity(a, b);
        assert!(score < 0.5, "expected <0.5, got {score}");
    }

    #[test]
    fn test_normalize_masks_per_boot_ddr_drift() {
        // Real samples from the same DUT, boots three days apart: DDR
        // training values (DQ roc / DQS roc / Vref) drift on every
        // healthy boot, structurally the lines are identical.
        let reference = "p0 n3, p2 n0, p1 n1, p0 n1, p5 n1, p0 n1, p0 n0, p0 n0, p0 n0,";
        let fresh = "p0 n2, p2 n0, p0 n1, p0 n2, p4 n1, p0 n1, p0 n0, p1 n4, p1 n0,";
        assert_eq!(
            ConnectionLearner::normalize(reference),
            ConnectionLearner::normalize(fresh),
            "unit-digit training values must mask to the same line"
        );
        assert_eq!(
            ConnectionLearner::normalize(
                "CH0 RX Vref:24.9%, RX DQS Vref:29.2%, TX Vref:24.8%,23.8%"
            ),
            ConnectionLearner::normalize(
                "CH0 RX Vref:25.7%, RX DQS Vref:30.0%, TX Vref:21.8%,21.8%"
            ),
            "fractional Vref values must mask as one run"
        );
        assert_eq!(
            ConnectionLearner::normalize("DQS roc: p2, n0, p2, n0"),
            ConnectionLearner::normalize("DQS roc: p2, n0, p1, n0"),
        );
    }

    /// Real-data drift check: compare two REAL boot captures (a stale
    /// reference vs a fresh boot of the same firmware) through the same
    /// pipeline the quick TEST uses. Point SERMCP_DRIFT_SAMPLES at
    /// a directory holding `reference.log` and `fresh.log`:
    ///   cp <dut-dir>/reference-boot.log  $DIR/reference.log
    ///   cp <dut-dir>/learn/<latest>.log  $DIR/fresh.log
    /// Without the directory the test is skipped with this hint — prepare
    /// the pair manually from the DUT's logs.
    #[test]
    fn test_quick_reference_similarity_survives_real_drift_samples() {
        let Some(dir) = std::env::var_os("SERMCP_DRIFT_SAMPLES") else {
            eprintln!(
                "SKIPPED: set SERMCP_DRIFT_SAMPLES=<dir with reference.log + fresh.log> \
                 to run the real-data drift check"
            );
            return;
        };
        let paths = [
            std::path::Path::new(&dir).join("reference.log"),
            std::path::Path::new(&dir).join("fresh.log"),
        ];
        for path in &paths {
            if !path.is_file() {
                eprintln!(
                    "SKIPPED: {} not found — copy the DUT's reference-boot.log and a \
                     learn-*.log there manually",
                    path.display()
                );
                return;
            }
        }
        let reference = std::fs::read_to_string(&paths[0]).expect("read reference.log");
        let fresh = std::fs::read_to_string(&paths[1]).expect("read fresh.log");
        // Same shape as run_hardware_learning_with_ref: first 50 lines,
        // normalized, compared against the 0.93 quick threshold.
        let a = ConnectionLearner::normalize(&ConnectionLearner::extract_first_n_lines(
            &reference,
            DEFAULT_COMPARE_LINES_FOR_TEST,
        ));
        let b = ConnectionLearner::normalize(&ConnectionLearner::extract_first_n_lines(
            &fresh,
            DEFAULT_COMPARE_LINES_FOR_TEST,
        ));
        let score = compute_similarity(&a, &b);
        assert!(
            score >= 0.93,
            "per-boot DDR drift must not fail the reference comparison: {score}"
        );
    }

    #[test]
    fn test_compute_similarity_empty() {
        assert_eq!(compute_similarity("", ""), 1.0);
        assert_eq!(compute_similarity("hello", ""), 0.0);
        assert_eq!(compute_similarity("", "world"), 0.0);
    }

    #[test]
    fn test_jaccard_3gram() {
        let a = "hello world";
        let b = "hello world";
        assert!((similarity::jaccard_3gram(a, b) - 1.0).abs() < 0.01);

        let c = "completely different";
        let score = similarity::jaccard_3gram(a, c);
        assert!(score < 0.3, "expected low similarity, got {score}");
    }

    #[test]
    fn test_normalize_strips_nul() {
        let input = "hello\0world\0";
        let result = ConnectionLearner::normalize(input);
        assert!(!result.contains('\0'));
        assert_eq!(result, "helloworld");
    }

    #[test]
    fn test_learn_config_defaults() {
        let cfg = LearnConfig::default();
        assert_eq!(cfg.cycles, 3);
        assert_eq!(cfg.compare_lines, 50);
        assert!((cfg.similarity_threshold - 0.93).abs() < 0.001);
        assert!((cfg.relay_broken_threshold - 0.10).abs() < 0.001);
    }

    #[test]
    fn test_prune_learn_logs_limits_count_without_touching_other_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        for index in 0..(MAX_LEARN_LOG_FILES + 5) {
            std::fs::write(tmp.path().join(format!("learn-{index:03}.log")), b"data").unwrap();
        }
        std::fs::write(tmp.path().join("reference-boot.log"), b"keep").unwrap();

        assert_eq!(prune_learn_logs(tmp.path()).unwrap(), 5);
        assert!(!tmp.path().join("learn-000.log").exists());
        assert!(tmp.path().join("learn-024.log").exists());
        assert!(tmp.path().join("reference-boot.log").exists());
    }

    #[test]
    fn test_prune_learn_logs_limits_total_bytes() {
        let tmp = tempfile::TempDir::new().unwrap();
        let file_size = 2 * 1024 * 1024;
        for index in 0..10 {
            let file =
                std::fs::File::create(tmp.path().join(format!("learn-{index:03}.log"))).unwrap();
            file.set_len(file_size).unwrap();
        }

        assert_eq!(prune_learn_logs(tmp.path()).unwrap(), 2);
        let retained_bytes: u64 = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|entry| entry.unwrap().metadata().unwrap().len())
            .sum();
        assert_eq!(retained_bytes, MAX_LEARN_LOG_TOTAL_BYTES);
    }

    #[test]
    fn test_learn_result_no_cycles() {
        let learner = ConnectionLearner::new(LearnConfig::default());
        let result = learner.evaluate_cycles(vec![], LearnMethod::HardwareReset);
        assert!(!result.connected);
        // Exact message: fewer-than-2 cycles must take the "need two
        // cycles" branch, not the silent-cycle branch (killed the
        // match-guard mutant flagged by cargo-mutants).
        assert_eq!(
            result.error.as_deref(),
            Some("Need at least 2 cycles for comparison")
        );
    }

    /// cargo-mutants: `!` deletion in the `!connected` error branch —
    /// a SUCCESSFUL learn must carry no error text.
    #[test]
    fn test_learn_success_carries_no_error() {
        let learner = ConnectionLearner::new(LearnConfig::default());
        let text = ConnectionLearner::normalize("DDR init ok\nU-Boot SPL ok");
        let cycles = vec![
            LearnCycle {
                log_path: PathBuf::from("a.log"),
                first_50: text.clone(),
                full_content: String::new(),
                method: LearnMethod::HardwareReset,
            },
            LearnCycle {
                log_path: PathBuf::from("b.log"),
                first_50: text,
                full_content: String::new(),
                method: LearnMethod::HardwareReset,
            },
        ];
        let result = learner.evaluate_cycles(cycles, LearnMethod::HardwareReset);
        assert!(result.connected);
        assert!(result.error.is_none(), "success must not carry an error");
    }

    /// cargo-mutants: `==`→`!=` on the NotFound guard — pruning a
    /// directory that does not exist is Ok(0), not an error.
    #[test]
    fn test_prune_missing_dir_is_ok_zero() {
        let missing =
            std::env::temp_dir().join(format!("mutants-prune-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&missing);
        assert_eq!(prune_learn_logs(&missing).unwrap(), 0);
    }

    /// Exact retention-limit constants (cargo-mutants killed the
    /// `16 * 1024 * 1024` arithmetic mutations that relative
    /// assertions could not see).
    #[test]
    fn test_retention_limit_constants_exact() {
        assert_eq!(MAX_LEARN_LOG_FILES, 20);
        assert_eq!(MAX_LEARN_LOG_FILE_BYTES, 2_097_152);
        assert_eq!(MAX_LEARN_LOG_TOTAL_BYTES, 16_777_216);
    }

    /// All cycles silent (serial console dead / DUT off): the learn must
    /// FAIL with an explicit error and MUST NOT write an (empty) reference
    /// over a good one — empty-vs-empty similarity is 100% and would
    /// otherwise "pass" and zero the reference log.
    #[test]
    fn test_learn_result_all_cycles_empty_never_writes_reference() {
        let tmp = std::env::temp_dir().join(format!("dutabo-learn-empty-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let ref_path = tmp.join("reference-boot.log");

        let cfg = LearnConfig {
            reference_log: ref_path.clone(),
            ..Default::default()
        };
        let learner = ConnectionLearner::new(cfg);
        let cycles: Vec<LearnCycle> = (0..3)
            .map(|_| LearnCycle {
                log_path: PathBuf::from("/tmp/empty.log"),
                first_50: String::new(),
                full_content: String::new(),
                method: LearnMethod::HardwareReset,
            })
            .collect();

        let result = learner.evaluate_cycles(cycles, LearnMethod::HardwareReset);
        assert!(!result.connected, "silent cycles must not pass");
        assert!(
            result.reference_log.is_none(),
            "no reference may be written"
        );
        let err = result.error.unwrap_or_default();
        assert!(
            err.contains("No serial data captured"),
            "error must explain silence: {err}"
        );
        assert!(
            !ref_path.exists(),
            "empty reference must not be written over a good one"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_learn_result_high_similarity() {
        let learner = ConnectionLearner::new(LearnConfig::default());
        let text = "u-boot spl 2024.01\nddr version 1.08\nbl31: v2.10\n";
        let normalized = ConnectionLearner::normalize(text);

        let cycles: Vec<LearnCycle> = (0..3)
            .map(|_| LearnCycle {
                log_path: PathBuf::from("/tmp/test.log"),
                first_50: normalized.clone(),
                full_content: text.to_string(),
                method: LearnMethod::HardwareReset,
            })
            .collect();

        let result = learner.evaluate_cycles(cycles, LearnMethod::HardwareReset);
        assert!(result.connected);
        assert!(result.best_similarity > 0.93);
    }

    #[test]
    fn test_learn_result_low_similarity() {
        let learner = ConnectionLearner::new(LearnConfig::default());

        let cycles = vec![
            LearnCycle {
                log_path: PathBuf::from("/tmp/test1.log"),
                first_50: ConnectionLearner::normalize("completely random garbage text"),
                full_content: String::new(),
                method: LearnMethod::HardwareReset,
            },
            LearnCycle {
                log_path: PathBuf::from("/tmp/test2.log"),
                first_50: ConnectionLearner::normalize("totally different unrelated output"),
                full_content: String::new(),
                method: LearnMethod::HardwareReset,
            },
            LearnCycle {
                log_path: PathBuf::from("/tmp/test3.log"),
                first_50: ConnectionLearner::normalize("yet another distinct sequence here"),
                full_content: String::new(),
                method: LearnMethod::HardwareReset,
            },
        ];

        let result = learner.evaluate_cycles(cycles, LearnMethod::HardwareReset);
        assert!(!result.connected);
        assert!(result.best_similarity < 0.93);
    }

    // ── Early-exit gate (cycle count is irrelevant; similarity ≥93% is the pass criterion) ──────

    fn cycle_with(first_50: &str) -> LearnCycle {
        LearnCycle {
            log_path: PathBuf::from("/tmp/early.log"),
            first_50: first_50.to_string(),
            full_content: String::new(),
            method: LearnMethod::HardwareReset,
        }
    }

    /// Two identical captures meet the threshold → the loop may stop after
    /// the SECOND cycle (early exit before the third).
    #[test]
    fn test_threshold_met_two_identical_cycles_early_exits() {
        let learner = ConnectionLearner::new(LearnConfig::default());
        let text = ConnectionLearner::normalize("u-boot spl 2024.01\nddr version 1.08\n");
        let cycles = vec![cycle_with(&text), cycle_with(&text)];
        assert!(
            learner.threshold_met(&cycles),
            "≥93% after two cycles must pass (cycle count is not the criterion)"
        );
        assert_eq!(learner.similarity_so_far(&cycles), Some(1.0));
    }

    /// A single cycle is not yet comparable — the gate stays closed.
    #[test]
    fn test_threshold_met_single_cycle_not_comparable() {
        let learner = ConnectionLearner::new(LearnConfig::default());
        let text = ConnectionLearner::normalize("u-boot spl 2024.01");
        assert!(!learner.threshold_met(&[cycle_with(&text)]));
        assert_eq!(learner.similarity_so_far(&[cycle_with(&text)]), None);
    }

    /// Divergent captures never meet the threshold — the loop must keep
    /// capturing (and eventually fail via evaluate).
    #[test]
    fn test_threshold_met_divergent_cycles_stay_open() {
        let learner = ConnectionLearner::new(LearnConfig::default());
        let cycles = vec![
            cycle_with(&ConnectionLearner::normalize("completely random garbage")),
            cycle_with(&ConnectionLearner::normalize(
                "totally different unrelated output",
            )),
        ];
        assert!(!learner.threshold_met(&cycles));
        assert!(
            learner.similarity_so_far(&cycles).unwrap() < 0.93,
            "divergent cycles score below the threshold"
        );
    }

    /// An empty cycle is a broken capture — the gate must NOT treat it as
    /// "met" (empty-vs-empty similarity is 100%; evaluate fails loudly).
    #[test]
    fn test_threshold_met_empty_cycle_never_passes() {
        let learner = ConnectionLearner::new(LearnConfig::default());
        let cycles = vec![cycle_with(""), cycle_with("")];
        assert!(
            !learner.threshold_met(&cycles),
            "silent cycles must not early-exit into a pass"
        );
        assert_eq!(learner.similarity_so_far(&cycles), None);
        let result = learner.evaluate_cycles(cycles, LearnMethod::HardwareReset);
        assert!(!result.connected);
        assert!(
            result.error.unwrap_or_default().contains("No serial data"),
            "evaluate still reports the broken capture"
        );
    }

    #[test]
    fn test_relay_broken_detection() {
        let cfg = LearnConfig {
            relay_broken_threshold: 0.10,
            similarity_threshold: 0.93,
            ..Default::default()
        };
        let learner = ConnectionLearner::new(cfg);

        let cycles = vec![
            LearnCycle {
                log_path: PathBuf::from("/tmp/t1.log"),
                first_50: ConnectionLearner::normalize("aaaaaaaaaa"),
                full_content: String::new(),
                method: LearnMethod::HardwareReset,
            },
            LearnCycle {
                log_path: PathBuf::from("/tmp/t2.log"),
                first_50: ConnectionLearner::normalize("bbbbbbbbbb"),
                full_content: String::new(),
                method: LearnMethod::HardwareReset,
            },
        ];

        let result = learner.evaluate_cycles(cycles, LearnMethod::HardwareReset);
        // Similarity very low → relay is broken
        assert!(!result.relay_verified);
        assert!(!result.connected);
    }
}
