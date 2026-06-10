//! Boot stage detector — dual mode: regex hard matching + text-similarity adaptive
//!
//! Mode 1 (default): precompiled regexes detect known SoC boot stages
//! Mode 2 (StageLearner): learns stage anchors from a reference log and
//! matches new SoCs via text similarity
//!
//! Usage:
//!   let learner = StageLearner::from_reference("ref-boot.log");
//!   let stage = learner.classify_line("U-Boot 2024.01 (Jan 01 2025)");

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::bytes::Regex;

// ANSI stripping (CSI/ESC removal, controls kept) and 3-gram Jaccard both
// live in their shared modules — byte-identical copies were consolidated.
use crate::ansi_strip::strip_str;
use crate::similarity::{jaccard_similarity, trigram_hashes};

/// Boot stage definition
struct BootStage {
    name: &'static str,
    pattern: &'static Regex,
    action: Option<&'static str>,
}

static BOOT_STAGES: LazyLock<Vec<BootStage>> = LazyLock::new(|| {
    vec![
        // Bootloader stages
        BootStage {
            name: "spl",
            pattern: &SPL_RE,
            action: Some("rotate_log"),
        },
        BootStage {
            name: "tpl",
            pattern: &TPL_RE,
            action: None,
        },
        BootStage {
            name: "bl31",
            pattern: &BL31_RE,
            action: None,
        },
        BootStage {
            name: "optee",
            pattern: &OPTEE_RE,
            action: None,
        },
        BootStage {
            name: "ddr",
            pattern: &DDR_RE,
            action: Some("rotate_log"),
        },
        BootStage {
            name: "uboot",
            pattern: &UBOOT_RE,
            action: None,
        },
        BootStage {
            name: "autoboot",
            pattern: &AUTOBOOT_RE,
            action: Some("send_ctrl_c"),
        },
        // Kernel
        BootStage {
            name: "kernel",
            pattern: &KERNEL_RE,
            action: None,
        },
        BootStage {
            name: "start",
            pattern: &START_RE,
            action: None,
        },
        // Linux login/shell (Debian/Ubuntu)
        BootStage {
            name: "login",
            pattern: &LOGIN_RE,
            action: Some("send_login"),
        },
        BootStage {
            name: "password",
            pattern: &PASSWD_RE,
            action: Some("send_password"),
        },
        BootStage {
            name: "shell",
            pattern: &SHELL_RE,
            action: None,
        },
        // Android boot completion signals
        BootStage {
            name: "android_shell",
            pattern: &ANDROID_SHELL_RE,
            action: None,
        },
        BootStage {
            name: "android_init",
            pattern: &ANDROID_INIT_RE,
            action: None,
        },
        BootStage {
            name: "android_adbd",
            pattern: &ANDROID_ADBD_RE,
            action: None,
        },
        BootStage {
            name: "android_bootanim",
            pattern: &ANDROID_BOOTANIM_RE,
            action: None,
        },
        BootStage {
            name: "android_surfaceflinger",
            pattern: &ANDROID_SF_RE,
            action: None,
        },
        BootStage {
            name: "android_boot_completed",
            pattern: &ANDROID_BOOTED_RE,
            action: None,
        },
    ]
});

struct CrashPattern {
    pattern: &'static Regex,
    ctype: &'static str,
}

static CRASH_PATTERNS: LazyLock<Vec<CrashPattern>> = LazyLock::new(|| {
    vec![
        CrashPattern {
            pattern: &PANIC_RE,
            ctype: "panic",
        },
        CrashPattern {
            pattern: &BUG_RE,
            ctype: "BUG",
        },
        CrashPattern {
            pattern: &OOPS_RE,
            ctype: "Oops",
        },
        CrashPattern {
            pattern: &UNABLE_RE,
            ctype: "kernel-fault",
        },
        CrashPattern {
            pattern: &BUG_HANDLE_RE,
            ctype: "BUG",
        },
        CrashPattern {
            pattern: &SEGFAULT_RE,
            ctype: "segfault",
        },
        CrashPattern {
            pattern: &END_TRACE_RE,
            ctype: "end-trace",
        },
    ]
});

// ── Precompiled regexes ──
// The U-Boot anchors come from the loader abstraction (strategy pattern):
// the default loader (uboot) supplies the SPL/TPL/banner/prompt/autoboot
// patterns — switching the loader only swaps the implementation, the
// detector stays untouched.
static SPL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(crate::loader::default().spl_pattern()).unwrap());
static TPL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(crate::loader::default().tpl_pattern()).unwrap());
static BL31_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"BL31:").unwrap());
static OPTEE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"OP-TEE").unwrap());
static DDR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"DDR\s+(?:Version|f[0-9a-f]{8}|Init)").unwrap());
static UBOOT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(crate::loader::default().banner_or_prompt_pattern()).unwrap());
static AUTOBOOT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(crate::loader::default().autoboot_pattern()).unwrap());
/// Announcement of a reboot initiated by the DUT itself:
/// - systemd:        `reboot: Restarting system`
/// - vendor inits:   `reboot: Restarting system with command 'loader'`
/// - busybox init:   `Restarting system`
/// - sysvinit:       `The system is going down for reboot NOW`
///
/// Does NOT match shutdown (`reboot: Power down` / `System halted`) — power
/// down goes through `maybe_detect_shutdown` onto the DUT-off path.
static RESTART_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(restarting system|going down for reboot)").unwrap());
static KERNEL_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"Linux\s+version").unwrap());
static START_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"Starting\s+kernel").unwrap());
static LOGIN_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?:.*\s)?login:\s*$").unwrap());
static PASSWD_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"Password:\s*$").unwrap());
static SHELL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:[#\$]\s*$|^\S+@\S+:\S*[#\$]\s)").unwrap());
// Android boot detection — no end-of-line requirement because the Android
// serial shell prompt is interleaved with kernel log output
static ANDROID_SHELL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"console:/\s*[#\$]").unwrap());
static ANDROID_INIT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"init:\s*(?:started service|starting service)").unwrap());
static ANDROID_ADBD_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"adbd?\s+(?:.*?\s)?(?:starting|started|ready)").unwrap());
static ANDROID_BOOTANIM_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"bootanim\s+(?:service\s+)?(?:started|stopped|done)").unwrap());
static ANDROID_SF_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"surfaceflinger\s+.*?(?:started|ready)").unwrap());
static ANDROID_BOOTED_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:sys\.)?boot_completed\s*[=:]\s*(?:1|true|done)").unwrap());

static PANIC_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"Kernel\s+panic\s*[-:]").unwrap());
static BUG_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"BUG:\s").unwrap());
static OOPS_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"Oops:\s").unwrap());
static UNABLE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"Unable\s+to\s+handle\s+kernel").unwrap());
static BUG_HANDLE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"BUG:\s+unable\s+to\s+handle").unwrap());
static SEGFAULT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"Segmentation\s+fault").unwrap());
static END_TRACE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"---\[\s*end\s+trace\s+[0-9a-f]+\s*\]---").unwrap());

/// Detected boot event
#[derive(Debug)]
pub enum BootEvent {
    /// SPL/DDR detected → log rotation + booting state
    BootStart,
    /// Reboot initiated by the DUT itself (reboot command / watchdog /
    /// crash auto-restart). Unlike BootStart: re-arms the dedup cycle so the
    /// next SPL/DDR triggers BootStart again (otherwise a real reboot is
    /// swallowed by the `boot_detected` dedup).
    RebootStart,
    /// Autoboot countdown → send Ctrl-C
    Autoboot,
    /// login: prompt → send the username
    LoginPrompt,
    /// Password: prompt → send the password
    PasswordPrompt,
    /// Kernel crash → (crash_type, line)
    Crash(String, String),
    /// Stage change → stage_name
    Stage(String),
    /// StageLearner-unclassified lines (batched, for Agent self-learning)
    Unclassified(Vec<String>),
}

/// Temporary watcher (for `serial_wait_pattern`). Carries an optional
/// `pattern_index` so a multi-pattern wait can report *which* pattern matched
/// (mirrors labgrid's `expect([p1, p2, TIMEOUT])` return index).
struct Watcher {
    pattern: Regex,
    pattern_index: usize,
    sender: tokio::sync::mpsc::UnboundedSender<WatcherMatch>,
}

/// A watcher match result: which pattern index fired and the matching line.
#[derive(Debug, Clone)]
pub struct WatcherMatch {
    pub pattern_index: usize,
    pub line: String,
}

// ── StageLearner: text-similarity adaptive stage matching ──

/// Structured error for StageLearner operations.
#[derive(Debug)]
pub enum StageLearnerError {
    /// Reference log file not found or unreadable.
    Io {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    /// Reference log has too few lines for meaningful fingerprint extraction.
    TooFewLines { count: usize, required: usize },
    /// No stage fingerprints could be extracted from the reference.
    NoFingerprints,
}

impl std::fmt::Display for StageLearnerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(
                    f,
                    "cannot read reference log '{}': {source}",
                    path.display()
                )
            }
            Self::TooFewLines { count, required } => {
                write!(
                    f,
                    "reference log has only {count} lines (need at least {required})"
                )
            }
            Self::NoFingerprints => {
                write!(
                    f,
                    "no stage fingerprints extracted from reference log — \
                     check that the log contains known boot stage markers \
                     (DDR, U-Boot SPL, BL31, kernel, login, etc.)"
                )
            }
        }
    }
}

impl std::error::Error for StageLearnerError {}

/// Stage fingerprint: anchor line + precomputed 3-gram hash set (avoids
/// recomputing at runtime)
#[derive(Debug, Clone)]
pub struct StageFingerprint {
    pub stage: String,
    pub anchor: String,
    pub prefix: String,
    pub suffix: String,
    /// Precomputed anchor 3-gram hashes (u64)
    anchor_grams: Vec<u64>,
}

/// Text-similarity stage learner
///
/// Extracts each stage's "anchor lines" from the reference boot log as
/// fingerprints. For every line of new logs, computes the similarity against
/// all fingerprints and picks the best-matching stage.
///
/// # Example
/// ```ignore
/// let learner = StageLearner::from_reference("ref-soc-boot.log");
/// learner.classify("U-Boot 2024.01 (Jan 01 2025)"); // → "uboot"
/// ```
pub struct StageLearner {
    pub fingerprints: Vec<StageFingerprint>,
    /// Full reference log text (used for text-similarity comparison)
    reference_text: String,
    /// Stage name → match threshold (no match below this similarity)
    thresholds: HashMap<String, f64>,
    /// Most recently matched stage (used for ordering constraints)
    last_stage: Option<String>,
    /// stage → order (0 = earliest)
    stage_order: HashMap<String, usize>,
    /// was a crash detected?
    crashed: bool,
}

impl StageLearner {
    /// Build a learner from a reference log file
    pub fn from_reference(path: &std::path::Path) -> Result<Self, String> {
        let data = std::fs::read(path).map_err(|e| {
            let err = StageLearnerError::Io {
                path: path.to_path_buf(),
                source: e,
            };
            err.to_string()
        })?;
        let text = String::from_utf8_lossy(&data);
        Self::from_reference_text(&text)
    }

    /// Build a learner from reference log text.
    ///
    /// `stage_threshold` / `crash_threshold` override the hardcoded defaults
    /// (0.45 / 0.50). Pass `None` to use the defaults.
    pub fn from_reference_text(text: &str) -> Result<Self, String> {
        Self::from_reference_text_with_thresholds(text, 0.45, 0.50)
    }

    /// Like `from_reference_text` but with explicit thresholds.
    pub fn from_reference_text_with_thresholds(
        text: &str,
        stage_threshold: f64,
        crash_threshold: f64,
    ) -> Result<Self, String> {
        // Strip ANSI escape sequences and bracketed paste markers before learning
        let cleaned = strip_str(text);
        let lines: Vec<&str> = cleaned.lines().collect();
        if lines.len() < 10 {
            return Err(StageLearnerError::TooFewLines {
                count: lines.len(),
                required: 10,
            }
            .to_string());
        }

        // Stage definitions: (stage_name, [anchor_patterns])
        let stage_defs: Vec<(&str, Vec<&str>)> = vec![
            ("ddr", vec!["DDR ", "DDR Version", "DDR Init"]),
            ("spl", vec!["U-Boot SPL", "SPL board init"]),
            ("bl31", vec!["BL31:", "ARM Trusted Firmware"]),
            ("optee", vec!["OP-TEE", "I/TC:"]),
            ("uboot", vec!["U-Boot 20", "U-Boot 202"]),
            (
                "kernel",
                vec!["Linux version", "Booting Linux", "Starting kernel"],
            ),
            ("init", vec!["init: ", "init:]"]),
            ("shell", vec!["console:/", "login:", "# $", "$ $"]),
            ("booted", vec!["boot_completed", "Boot completed"]),
        ];

        let crash_patterns = [
            "Kernel panic",
            "BUG:",
            "Oops:",
            "Unable to handle kernel",
            "Segmentation fault",
            "---[ end trace",
            "panic - not syncing",
        ];
        let mut fingerprints = Vec::new();
        let mut thresholds = HashMap::new();
        let mut stage_order = HashMap::new();

        for (order, (stage, patterns)) in stage_defs.iter().enumerate() {
            thresholds.insert(stage.to_string(), stage_threshold);
            stage_order.insert(stage.to_string(), order);

            for i in 0..lines.len() {
                let line = lines[i].trim();
                if line.is_empty() {
                    continue;
                }
                for pat in patterns {
                    if line.contains(pat) {
                        let prefix = if i > 0 {
                            lines[i - 1].trim().to_string()
                        } else {
                            String::new()
                        };
                        let suffix = if i + 1 < lines.len() {
                            lines[i + 1].trim().to_string()
                        } else {
                            String::new()
                        };
                        fingerprints.push(StageFingerprint {
                            stage: stage.to_string(),
                            anchor: line.to_string(),
                            prefix,
                            suffix,
                            anchor_grams: compute_3gram_hashes(line),
                        });
                        break;
                    }
                }
            }
        }

        // Add crash fingerprints
        thresholds.insert("crash".to_string(), crash_threshold);
        stage_order.insert("crash".to_string(), 999); // crashes can happen anytime

        for i in 0..lines.len() {
            let line = lines[i].trim();
            if line.is_empty() {
                continue;
            }
            for pat in &crash_patterns {
                if line.contains(pat) {
                    let prefix = if i > 0 {
                        lines[i - 1].trim().to_string()
                    } else {
                        String::new()
                    };
                    let suffix = if i + 1 < lines.len() {
                        lines[i + 1].trim().to_string()
                    } else {
                        String::new()
                    };
                    fingerprints.push(StageFingerprint {
                        stage: "crash".to_string(),
                        anchor: line.to_string(),
                        prefix,
                        suffix,
                        anchor_grams: compute_3gram_hashes(line),
                    });
                    break;
                }
            }
        }

        if fingerprints.is_empty() {
            return Err(StageLearnerError::NoFingerprints.to_string());
        }

        let reference_text = cleaned;
        Ok(Self {
            fingerprints,
            reference_text,
            thresholds,
            last_stage: None,
            stage_order,
            crashed: false,
        })
    }

    /// Classify one text line → stage name, or None (unclear)
    /// Uses the combined score: 3-gram Jaccard (0.6) + Jaro-Winkler (0.4)
    pub fn classify_line(&mut self, line: &str) -> Option<String> {
        let line = line.trim();
        if line.is_empty() {
            return None;
        }

        let mut best_stage: Option<String> = None;
        let mut best_score = 0.0;

        let line_grams = compute_3gram_hashes(line);
        for fp in &self.fingerprints {
            let jaccard = jaccard_similarity(&line_grams, &fp.anchor_grams);
            let jaro = strsim::jaro_winkler(line, &fp.anchor);
            let score = jaccard * 0.6 + jaro * 0.4;

            if score > best_score {
                best_score = score;
                best_stage = Some(fp.stage.clone());
            }
        }

        // Threshold filter
        let threshold = self
            .thresholds
            .get(best_stage.as_deref().unwrap_or(""))
            .copied()
            .unwrap_or(0.4);
        if best_score < threshold {
            tracing::debug!(
                stage = %best_stage.as_deref().unwrap_or("?"),
                score = %best_score,
                threshold = %threshold,
                line = %line,
                "Stage classification below threshold"
            );
            return None;
        }
        tracing::debug!(
            stage = %best_stage.as_deref().unwrap_or("?"),
            score = %best_score,
            "Stage classified"
        );

        // Ordering constraint: no going backwards (except after a crash)
        let stage = best_stage.as_deref()?;
        if stage == "crash" {
            self.crashed = true;
            self.last_stage = Some("crash".into());
            return Some("crash".into());
        }

        let cur_order = self.stage_order.get(stage).copied().unwrap_or(0);
        if let Some(ref last) = self.last_stage {
            let last_order = self.stage_order.get(last.as_str()).copied().unwrap_or(0);
            // Allow a small step backwards (1 stage)
            if cur_order + 1 < last_order && !self.crashed {
                return None;
            }
        }

        self.last_stage = Some(stage.to_string());
        Some(stage.to_string())
    }

    /// Compute the text similarity between the buffer and the reference log
    /// (word-level Jaccard via strsim)
    pub fn buffer_similarity(&self, buffer: &str) -> f64 {
        if buffer.len() < 50 || self.reference_text.len() < 50 {
            return 0.0;
        }
        strsim::sorensen_dice(buffer, &self.reference_text)
    }

    /// Whether the buffer looks like a boot log (similarity > threshold)
    pub fn is_boot_like(&self, buffer: &str, threshold: f64) -> bool {
        self.buffer_similarity(buffer) > threshold
    }

    /// Reset state (new boot cycle)
    pub fn reset(&mut self) {
        self.last_stage = None;
        self.crashed = false;
    }

    /// Export fingerprints (for debugging/review)
    pub fn export_fingerprints(&self) -> Vec<(String, String)> {
        self.fingerprints
            .iter()
            .map(|fp| (fp.stage.clone(), fp.anchor.clone()))
            .collect()
    }
}

/// Precompute a string's 3-gram hashes (for efficient Jaccard comparison) — see `similarity`.
fn compute_3gram_hashes(s: &str) -> Vec<u64> {
    trigram_hashes(s)
}

pub struct BootStageDetector {
    line_buf: Vec<u8>,
    boot_detected: bool,
    password_sent: bool,
    last_crash_time: std::time::Instant,
    watchers: Vec<Watcher>,
    /// Optional: text-similarity learner for unknown SoC adaptation
    pub learner: Option<StageLearner>,
    /// Custom login prompt regex (from `.target.jsonc` `[dut.target]` `login_prompt`).
    /// `None` → use the built-in default `LOGIN_RE`.
    custom_login_re: Option<Regex>,
    /// StageLearner-unclassified line collection (for Agent self-learning)
    pub unclassified_lines: Vec<String>,
    /// Custom crash substrings from `.target.jsonc` `[monitor] crash_keywords`.
    custom_crash_keywords: Vec<String>,
}

impl Default for BootStageDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl BootStageDetector {
    pub fn new() -> Self {
        Self {
            line_buf: Vec::new(),
            boot_detected: false,
            password_sent: false,
            last_crash_time: std::time::Instant::now() - std::time::Duration::from_secs(10),
            watchers: Vec::new(),
            learner: None,
            custom_login_re: None,
            unclassified_lines: Vec::new(),
            custom_crash_keywords: Vec::new(),
        }
    }

    /// Set custom crash keywords from `.target.jsonc` `[monitor] crash_keywords`.
    /// Each entry is a substring (not regex). The built-in regex patterns
    /// (`PANIC_RE`, `BUG_RE`, etc.) are always checked first.
    pub fn set_crash_keywords(&mut self, keywords: Vec<String>) {
        self.custom_crash_keywords = keywords;
    }

    /// Set a custom login prompt regex (from `.target.jsonc` `[dut.target]`
    /// `login_prompt`). If unset or empty, the built-in default is used.
    pub fn set_login_regex(&mut self, pattern: &str) {
        if pattern.is_empty() {
            self.custom_login_re = None;
        } else {
            match Regex::new(pattern) {
                Ok(re) => {
                    tracing::info!("Custom login prompt regex set: {pattern}");
                    self.custom_login_re = Some(re);
                }
                Err(e) => {
                    tracing::warn!("Invalid login prompt regex '{pattern}': {e}. Using default.");
                    self.custom_login_re = None;
                }
            }
        }
    }

    /// Load a reference log to enable adaptive stage detection (for new SoCs)
    /// Hot-reload safe: preserves last_stage/crashed state from previous learner.
    ///
    /// `stage_threshold` / `crash_threshold` override the hardcoded defaults
    /// (0.45 / 0.50) when > 0.0. Pass 0.0 to use defaults.
    pub fn load_reference(
        &mut self,
        path: &std::path::Path,
        stage_threshold: f64,
        crash_threshold: f64,
    ) -> Result<(), String> {
        let prev_last_stage = self.learner.as_ref().and_then(|l| l.last_stage.clone());
        let prev_crashed = self.learner.as_ref().map(|l| l.crashed).unwrap_or(false);
        let prev_unclassified = std::mem::take(&mut self.unclassified_lines);

        let mut learner = if stage_threshold > 0.0 || crash_threshold > 0.0 {
            let st = if stage_threshold > 0.0 {
                stage_threshold
            } else {
                0.45
            };
            let ct = if crash_threshold > 0.0 {
                crash_threshold
            } else {
                0.50
            };
            let text = std::fs::read_to_string(path).map_err(|e| format!("read: {e}"))?;
            StageLearner::from_reference_text_with_thresholds(&text, st, ct)?
        } else {
            StageLearner::from_reference(path)?
        };
        tracing::info!(
            "StageLearner loaded: {} fingerprints from {}",
            learner.fingerprints.len(),
            path.display()
        );

        // Preserve runtime state on hot-reload
        learner.last_stage = prev_last_stage;
        learner.crashed = prev_crashed;
        self.learner = Some(learner);
        self.unclassified_lines = prev_unclassified;

        Ok(())
    }

    /// Add a temporary watcher (for `serial_wait_pattern`).
    pub fn add_watcher(
        &mut self,
        pattern: &str,
        sender: tokio::sync::mpsc::UnboundedSender<WatcherMatch>,
    ) {
        if let Ok(re) = Regex::new(pattern) {
            self.watchers.push(Watcher {
                pattern: re,
                pattern_index: 0,
                sender,
            });
        }
    }

    /// Remove watchers matching the given pattern string.
    pub fn remove_watcher_by_pattern(&mut self, pattern: &str) {
        if let Ok(re) = Regex::new(pattern) {
            let re_str = re.as_str().to_string();
            self.watchers.retain(|w| w.pattern.as_str() != re_str);
        }
    }

    /// Remove every watcher registered through the same channel.
    pub fn remove_watcher_group(
        &mut self,
        sample: &tokio::sync::mpsc::UnboundedSender<WatcherMatch>,
    ) {
        self.watchers.retain(|w| !w.sender.same_channel(sample));
    }

    /// Feed input data, return the list of detected events
    pub fn feed(&mut self, data: &[u8]) -> Vec<BootEvent> {
        let mut events = Vec::new();
        self.line_buf.extend_from_slice(data);

        // Prevent unbounded buffer growth
        if self.line_buf.len() > 65536 {
            let drain = self.line_buf.len() - 32768;
            self.line_buf.drain(..drain);
        }

        while self.line_buf.contains(&b'\n') || self.line_buf.contains(&b'\r') {
            if let Some((line, rest)) = self.split_line() {
                self.line_buf = rest; // ALWAYS advance buffer
                if line.is_empty() {
                    continue;
                }
                let line_str = String::from_utf8_lossy(&line).to_string();

                // Check for crashes
                if let Some(ce) = self.check_crash(&line, &line_str) {
                    events.push(ce);
                }
                // Check boot stages
                events.extend(self.check_stages(&line));
                // Check watchers
                self.check_watchers(&line_str);
            } else {
                break;
            }
        }

        // Interactive prompts commonly have no trailing CR/LF (`=> `,
        // `root@board:~# `). Watchers must see the current partial line now;
        // waiting for the 1s watchdog flush made U-Boot entry race or time out.
        if !self.line_buf.is_empty() {
            let partial = String::from_utf8_lossy(&self.line_buf).into_owned();
            self.check_watchers(&partial);
        }

        events
    }

    /// Force-process leftover buffer data (lines without a trailing \n, e.g. shell prompts)
    pub fn flush_line_buf(&mut self) -> Vec<BootEvent> {
        let mut events = Vec::new();
        if self.line_buf.is_empty() {
            return events;
        }
        // Treat the whole leftover buffer as a single line
        let line = self.line_buf.clone();
        self.line_buf.clear();
        if line.iter().all(|&b| b.is_ascii_whitespace()) {
            return events;
        }
        let trimmed: Vec<u8> = line
            .iter()
            .copied()
            .skip_while(|b| b.is_ascii_whitespace())
            .collect();
        let line_str = String::from_utf8_lossy(&trimmed).to_string();
        if let Some(ce) = self.check_crash(&trimmed, &line_str) {
            events.push(ce);
        }
        events.extend(self.check_stages(&trimmed));
        self.check_watchers(&line_str);
        events
    }

    /// Full reset — called when a new power-on cycle begins
    pub fn reset_cycle(&mut self) {
        self.boot_detected = false;
        self.password_sent = false;
        self.unclassified_lines.clear();
        if let Some(ref mut learner) = self.learner {
            learner.reset();
        }
    }

    // ── internal ──

    fn split_line(&mut self) -> Option<(Vec<u8>, Vec<u8>)> {
        let idx_n = self.line_buf.iter().position(|&b| b == b'\n');
        let idx_r = self.line_buf.iter().position(|&b| b == b'\r');

        let (idx, sep_len) = match (idx_n, idx_r) {
            (Some(n), Some(r)) if n < r => (n, 1),
            (Some(_n), Some(r)) => (r, 1),
            (Some(n), None) => (n, 1),
            (None, Some(r)) => (r, 1),
            (None, None) => return None,
        };

        let line = self.line_buf[..idx].to_vec();
        let rest = self.line_buf[idx + sep_len..].to_vec();
        // trim whitespace from line
        let trimmed: Vec<u8> = line
            .iter()
            .copied()
            .skip_while(|b| b.is_ascii_whitespace())
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .skip_while(|b| b.is_ascii_whitespace())
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        Some((trimmed, rest))
    }

    fn check_stages(&mut self, line: &[u8]) -> Vec<BootEvent> {
        let mut events = Vec::new();
        let text = String::from_utf8_lossy(line);
        let trimmed = text.trim().to_string();
        if trimmed.is_empty() {
            return events;
        }

        // ── 1. StageLearner first (adaptive, when a reference log is loaded) ──
        let mut learner_matched = false;
        if let Some(ref mut learner) = self.learner
            && let Some(stage_name) = learner.classify_line(&trimmed)
        {
            learner_matched = true;
            match stage_name.as_str() {
                "ddr" | "spl" => {
                    if !self.boot_detected {
                        self.boot_detected = true;
                        self.password_sent = false;
                        events.push(BootEvent::BootStart);
                    }
                }
                "uboot" if !self.boot_detected => {
                    self.boot_detected = true;
                    self.password_sent = false;
                    events.push(BootEvent::BootStart);
                }
                _ => {}
            }
            events.push(BootEvent::Stage(stage_name));
        }

        // ── 1.5 DUT-initiated reboot announcement (always runs — independent of the learner) ──
        // Re-arms the dedup cycle: otherwise, after a `reboot` / watchdog
        // restart, the next SPL/DDR is swallowed by the `boot_detected` dedup
        // and the real reboot is treated as ordinary noise.
        if RESTART_RE.is_match(line) {
            events.push(BootEvent::RebootStart);
            self.reset_cycle();
        }

        // ── 2. login/password regex (always runs — actions need triggering) ──
        // No one-shot dedup on login: after a FAILED auto-login getty
        // re-prints the prompt, and that repetition is exactly the signal
        // the engine's login-retry / tx-repeat auto-probe keys on.
        // Debounce lives in the engine (`last_login_send`), not here.
        let login_re = self.custom_login_re.as_ref().unwrap_or(&LOGIN_RE);
        if login_re.is_match(line) {
            events.push(BootEvent::LoginPrompt);
        }
        if PASSWD_RE.is_match(line) && !self.password_sent {
            self.password_sent = true;
            events.push(BootEvent::PasswordPrompt);
        }

        // ── 3. Regex fallback (only when the learner did not match) ──
        if !learner_matched {
            for stage in BOOT_STAGES.iter() {
                // login/password already handled in step 2
                if stage.name == "login" || stage.name == "password" {
                    continue;
                }
                if stage.pattern.is_match(line)
                    && let Some(ev) = self.handle_stage(stage, line)
                {
                    events.push(ev);
                }
            }
        }

        // ── 4. Unclassified line collection ──
        if events.is_empty() {
            self.unclassified_lines.push(trimmed);
            if self.unclassified_lines.len() >= 20 {
                let drained: Vec<String> = std::mem::take(&mut self.unclassified_lines);
                events.push(BootEvent::Unclassified(drained));
            }
        } else if !self.unclassified_lines.is_empty() {
            // Stage boundary → flush the backlog of unclassified lines
            let drained: Vec<String> = std::mem::take(&mut self.unclassified_lines);
            events.push(BootEvent::Unclassified(drained));
        }

        events
    }

    fn handle_stage(&mut self, stage: &BootStage, _line: &[u8]) -> Option<BootEvent> {
        // SPL/DDR dedup: only one BootStart per boot cycle
        if (stage.name == "spl" || stage.name == "ddr") && self.boot_detected {
            return None;
        }
        if stage.name == "spl" || stage.name == "ddr" {
            self.boot_detected = true;
            self.password_sent = false;
        }

        match stage.action {
            Some("rotate_log") => {
                return Some(BootEvent::BootStart);
            }
            Some("send_ctrl_c") => {
                return Some(BootEvent::Autoboot);
            }
            // No one-shot dedup (see check_stages): the engine debounces
            // and uses repeated prompts as its retry signal.
            Some("send_login") => {
                return Some(BootEvent::LoginPrompt);
            }
            Some("send_password") if !self.password_sent => {
                self.password_sent = true;
                return Some(BootEvent::PasswordPrompt);
            }
            _ => {}
        }

        Some(BootEvent::Stage(stage.name.to_string()))
    }

    fn check_crash(&mut self, line: &[u8], line_str: &str) -> Option<BootEvent> {
        // 1. Built-in regex patterns (fast, precise).
        for cp in CRASH_PATTERNS.iter() {
            if cp.pattern.is_match(line) {
                let now = std::time::Instant::now();
                if now.duration_since(self.last_crash_time).as_secs_f64() > 2.0 {
                    self.last_crash_time = now;
                    return Some(BootEvent::Crash(cp.ctype.to_string(), line_str.to_string()));
                }
            }
        }
        // 2. Custom substring keywords from .target.jsonc [monitor] crash_keywords.
        for pat in &self.custom_crash_keywords {
            if line_str.contains(pat.as_str()) {
                let now = std::time::Instant::now();
                if now.duration_since(self.last_crash_time).as_secs_f64() > 2.0 {
                    self.last_crash_time = now;
                    return Some(BootEvent::Crash("custom".to_string(), line_str.to_string()));
                }
            }
        }
        None
    }

    fn check_watchers(&mut self, line: &str) {
        // Watchers are one-shot. Remove the whole sender group after the first
        // match so multi-pattern waits cannot keep firing on later lines.
        while let Some(index) = self
            .watchers
            .iter()
            .position(|watcher| watcher.pattern.is_match(line.as_bytes()))
        {
            let sender = self.watchers[index].sender.clone();
            let matched = WatcherMatch {
                pattern_index: self.watchers[index].pattern_index,
                line: line.to_string(),
            };
            let _ = sender.send(matched);
            self.watchers
                .retain(|watcher| !watcher.sender.same_channel(&sender));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has_event(events: &[BootEvent], variant_name: &str) -> bool {
        events.iter().any(|e| {
            matches!(
                (e, variant_name),
                (BootEvent::BootStart, "BootStart")
                    | (BootEvent::RebootStart, "RebootStart")
                    | (BootEvent::Autoboot, "Autoboot")
                    | (BootEvent::LoginPrompt, "LoginPrompt")
                    | (BootEvent::PasswordPrompt, "PasswordPrompt")
                    | (BootEvent::Crash(_, _), "Crash")
                    | (BootEvent::Stage(_), "Stage")
                    | (BootEvent::Unclassified(_), "Unclassified")
            )
        })
    }

    fn get_stage_name(events: &[BootEvent]) -> Option<&str> {
        events.iter().find_map(|e| match e {
            BootEvent::Stage(name) => Some(name.as_str()),
            _ => None,
        })
    }

    #[test]
    fn test_spl_detection() {
        let mut detector = BootStageDetector::new();
        let events = detector.feed(b"U-Boot SPL 2024.01\n");

        // SPL triggers BootStart (not Stage("spl")) because it has "rotate_log" action
        assert!(has_event(&events, "BootStart"));
    }

    #[test]
    fn test_ddr_version_detection() {
        let mut detector = BootStageDetector::new();
        let events = detector.feed(b"DDR Version 1.08\n");

        // DDR now triggers BootStart (rotate_log action) — verify via BootStart
        assert!(
            has_event(&events, "BootStart"),
            "DDR should trigger BootStart"
        );
    }

    #[test]
    fn test_uboot_detection() {
        let mut detector = BootStageDetector::new();
        let events = detector.feed(b"U-Boot 2024.01 (Jan 01 2024 - 00:00:00)\n");

        assert_eq!(get_stage_name(&events), Some("uboot"));
    }

    #[test]
    fn test_autoboot_detection() {
        let mut detector = BootStageDetector::new();
        let events = detector.feed(b"Hit any key to stop autoboot:  5\n");

        assert!(has_event(&events, "Autoboot"));
    }

    #[test]
    fn test_autoboot_alternate_format() {
        let mut detector = BootStageDetector::new();
        let events = detector.feed(b"Hit key to stop autoboot\n");

        assert!(has_event(&events, "Autoboot"));
    }

    #[test]
    fn test_linux_version_detection() {
        let mut detector = BootStageDetector::new();
        let events = detector.feed(b"Linux version 6.1.0 (gcc 12.2.0)\n");

        assert_eq!(get_stage_name(&events), Some("kernel"));
    }

    #[test]
    fn test_login_prompt_detection() {
        let mut detector = BootStageDetector::new();
        let events = detector.feed(b"debian login:\n");

        assert!(has_event(&events, "LoginPrompt"));
    }

    #[test]
    fn test_password_prompt_detection() {
        let mut detector = BootStageDetector::new();
        let events = detector.feed(b"Password:\n");

        assert!(has_event(&events, "PasswordPrompt"));
    }

    #[test]
    fn test_shell_prompt_detection() {
        let mut detector = BootStageDetector::new();
        let events = detector.feed(b"root@debian:~#\n");

        assert_eq!(get_stage_name(&events), Some("shell"));
    }

    #[test]
    fn test_crash_panic_detection() {
        let mut detector = BootStageDetector::new();
        let events = detector.feed(b"Kernel panic - not syncing: Fatal exception\n");

        assert!(has_event(&events, "Crash"));
        if let BootEvent::Crash(ctype, _) = &events[0] {
            assert_eq!(ctype, "panic");
        }
    }

    #[test]
    fn test_crash_bug_detection() {
        let mut detector = BootStageDetector::new();
        let events = detector.feed(b"BUG: unable to handle page fault\n");

        assert!(has_event(&events, "Crash"));
    }

    #[test]
    fn test_crash_oops_detection() {
        let mut detector = BootStageDetector::new();
        let events = detector.feed(b"Oops: 00000000 [#1] SMP\n");

        assert!(has_event(&events, "Crash"));
    }

    #[test]
    fn test_crash_throttle() {
        let mut detector = BootStageDetector::new();

        // First crash should be detected
        let events1 = detector.feed(b"Kernel panic - not syncing\n");
        assert!(has_event(&events1, "Crash"));

        // Second crash within 2 seconds should be throttled
        let events2 = detector.feed(b"Kernel panic - not syncing\n");
        assert!(!has_event(&events2, "Crash"));
    }

    #[test]
    fn test_spl_dedup() {
        let mut detector = BootStageDetector::new();

        // First SPL should trigger BootStart
        let events1 = detector.feed(b"U-Boot SPL 2024.01\n");
        assert!(has_event(&events1, "BootStart"));

        // Second SPL should not trigger BootStart again
        let events2 = detector.feed(b"U-Boot SPL 2024.01\n");
        assert!(!has_event(&events2, "BootStart"));
    }

    #[test]
    fn test_reset_cycle() {
        let mut detector = BootStageDetector::new();

        // Trigger SPL
        detector.feed(b"U-Boot SPL 2024.01\n");

        // Reset cycle
        detector.reset_cycle();

        // SPL should trigger BootStart again
        let events = detector.feed(b"U-Boot SPL 2024.01\n");
        assert!(has_event(&events, "BootStart"));
    }

    #[test]
    fn test_restart_line_rearms_cycle_for_next_spl() {
        let mut detector = BootStageDetector::new();

        // First SPL triggers BootStart and marks the cycle detected.
        let events1 = detector.feed(b"U-Boot SPL 2024.01\n");
        assert!(has_event(&events1, "BootStart"));

        // DUT-initiated restart announcement (android-style init).
        let events2 = detector.feed(b"reboot: Restarting system with command 'loader'\n");
        assert!(
            events2.iter().any(|e| matches!(e, BootEvent::RebootStart)),
            "restart announcement must emit RebootStart, got {events2:?}"
        );

        // The restart re-arms the cycle: the next DDR fires BootStart again
        // (without this the dedup swallows real DUT-initiated reboots).
        let events3 = detector.feed(b"DDR Version 1.08\n");
        assert!(has_event(&events3, "BootStart"));
    }

    #[test]
    fn test_restart_line_busybox_variant() {
        let mut detector = BootStageDetector::new();
        detector.feed(b"U-Boot SPL 2024.01\n");

        let events = detector.feed(b"Restarting system\n");
        assert!(
            events.iter().any(|e| matches!(e, BootEvent::RebootStart)),
            "busybox 'Restarting system' must emit RebootStart, got {events:?}"
        );
    }

    #[test]
    fn test_power_down_is_not_restart() {
        let mut detector = BootStageDetector::new();
        detector.feed(b"U-Boot SPL 2024.01\n");

        let events = detector.feed(b"reboot: Power down\n");
        assert!(
            !events.iter().any(|e| matches!(e, BootEvent::RebootStart)),
            "shutdown must not be mistaken for a restart: {events:?}"
        );
        // And the cycle must NOT be re-armed by a shutdown.
        let events2 = detector.feed(b"DDR Version 1.08\n");
        assert!(!has_event(&events2, "BootStart"));
    }

    #[test]
    fn test_login_prompt_repeats_signal_retry() {
        let mut detector = BootStageDetector::new();

        // First login prompt fires.
        let events1 = detector.feed(b"login:\n");
        assert!(has_event(&events1, "LoginPrompt"));

        // After a FAILED auto-login getty re-prints the prompt — the
        // repetition is the engine's retry / tx-repeat probe signal, so
        // it must fire again (debounce lives in the engine).
        let events2 = detector.feed(b"login:\n");
        assert!(has_event(&events2, "LoginPrompt"));
    }

    #[test]
    fn test_multiline_feed() {
        let mut detector = BootStageDetector::new();

        let data = b"line 1\nline 2\nU-Boot SPL 2024.01\nline 3\n";
        let events = detector.feed(data);

        // SPL triggers BootStart
        assert!(has_event(&events, "BootStart"));
    }

    #[test]
    fn test_chunked_feed() {
        let mut detector = BootStageDetector::new();

        // Feed in chunks
        detector.feed(b"U-Boot ");
        detector.feed(b"SPL ");
        detector.feed(b"2024.01\n");

        // Should still detect SPL
        // (Note: detector buffers until newline)
    }

    #[test]
    fn test_empty_feed() {
        let mut detector = BootStageDetector::new();
        let events = detector.feed(b"");
        assert!(events.is_empty());
    }

    #[test]
    fn test_no_newline_buffer() {
        let mut detector = BootStageDetector::new();
        let events = detector.feed(b"partial line without newline");
        assert!(events.is_empty());
    }

    #[test]
    fn test_watcher_add_remove() {
        let mut detector = BootStageDetector::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        detector.add_watcher("test.*pattern", tx);
        detector.remove_watcher_by_pattern("test.*pattern");

        // Watcher should be removed
        let _events = detector.feed(b"test_pattern_line\n");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn test_watcher_matches() {
        let mut detector = BootStageDetector::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        detector.add_watcher("custom.*marker", tx);

        detector.feed(b"custom_marker_line\n");
        let received = rx.try_recv().unwrap();
        assert!(received.line.contains("custom_marker_line"));
        assert_eq!(received.pattern_index, 0);

        detector.feed(b"custom_marker_again\n");
        assert!(
            rx.try_recv().is_err(),
            "a matched watcher must be removed after its first delivery"
        );
    }

    #[test]
    fn watcher_matches_prompt_without_newline() {
        let mut detector = BootStageDetector::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        detector.add_watcher(r"=>|U-Boot[>#]", tx);
        detector.feed(b"=> ");
        let matched = rx
            .try_recv()
            .expect("partial U-Boot prompt must wake watcher");
        assert_eq!(matched.line, "=> ");
    }

    #[test]
    fn test_line_split_cr() {
        let mut detector = BootStageDetector::new();
        let _events = detector.feed(b"line1\r\nline2\r\n");

        // Should process both lines
        // (exact events depend on content)
    }

    #[test]
    fn test_boot_sequence() {
        let mut detector = BootStageDetector::new();

        let boot_log = b"\
U-Boot SPL 2024.01\r\n\
DDR Version 1.08\r\n\
BL31: v2.10.0\r\n\
U-Boot 2024.01\r\n\
Hit any key to stop autoboot:  5\r\n\
Starting kernel ...\r\n\
Linux version 6.1.0\r\n\
login:\r\n\
root@debian:~#\r\n\
";

        let events = detector.feed(boot_log);

        assert!(has_event(&events, "BootStart")); // SPL
        assert!(has_event(&events, "Autoboot"));
        assert!(has_event(&events, "LoginPrompt"));
        // Check that shell stage was detected (may not be the first Stage event)
        assert!(
            events
                .iter()
                .any(|e| matches!(e, BootEvent::Stage(s) if s == "shell"))
        );
    }

    #[test]
    fn test_ddr_detection() {
        let mut detector = BootStageDetector::new();
        let events = detector.feed(b"DDR Version 1.25 20230501\n");
        assert!(!events.is_empty());
    }

    #[test]
    fn test_kernel_panic_detection() {
        let mut detector = BootStageDetector::new();
        let events = detector.feed(b"Kernel panic - not syncing: Attempted to kill init!\n");
        let has_crash = events.iter().any(|e| matches!(e, BootEvent::Crash(_, _)));
        assert!(has_crash, "Should detect kernel panic");
    }

    #[test]
    fn test_uboot_prompt_detection() {
        let mut detector = BootStageDetector::new();
        let events = detector.feed(b"U-Boot 2017.09 (Jan 01 2023 - 00:00:00)\n");
        assert!(!events.is_empty());
    }

    #[test]
    fn test_empty_input_no_events() {
        let mut detector = BootStageDetector::new();
        let events = detector.feed(b"\n");
        assert!(events.is_empty());
    }
}
