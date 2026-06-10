//! Config loading — JSONC (`.target.jsonc`).
//! Searches upward from CWD. `TARGET_CONF` env var overrides the path.
//! This module parses JSONC exclusively; a missing `.target.jsonc` yields
//! the plain config-missing error.
//!
//! The public contract (never changes): `Config.values` stays the flat
//! `HashMap<String, String>` map — every engine consumer reads stringified
//! values, and literal `Config { values, .., format: ConfigFormat::None }`
//! test constructions compile untouched.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Config file name discovered by walk-up and written by `dutabo init`.
pub const CONFIG_FILE_NAME: &str = ".target.jsonc";

/// Resolve the power-control backend. An explicit `relay.type` wins; when
/// it is absent, `control.dev_ctrl` doubles as the selector — the only
/// selectable values are the built-in ch340-relay, or none/disabled to
/// disable power control entirely (usb-relay and the external
/// dev-ctl program backend were removed).
fn resolve_relay_type(relay_type: &str, dev_ctl: &str) -> String {
    let explicit = relay_type.trim();
    if !explicit.is_empty() {
        return explicit.to_string();
    }
    let dev_ctl = dev_ctl.trim();
    match dev_ctl.to_ascii_lowercase().as_str() {
        "" => "ch340".into(),
        "ch340" | "ch340-relay" | "none" | "disabled" => dev_ctl.to_ascii_lowercase(),
        // Anything else is NOT a valid backend selector anymore — hand it
        // back verbatim so validation rejects it as unsupported instead of
        // silently mapping it to the external program backend (removed).
        other => other.to_string(),
    }
}

/// Default config values (flat keys shared by every consumer).
pub fn defaults() -> HashMap<String, String> {
    HashMap::from([
        ("DEV_HOST_IP".into(), String::new()),
        ("DEV_HOST_USER".into(), String::new()),
        ("DEV_HOST_PASS".into(), String::new()),
        ("SERIAL_PORT".into(), String::new()),
        ("SERIAL_RFC2217".into(), "false".into()),
        ("SERIAL_BAUDRATE".into(), String::new()),
        ("SERIAL_TX_REPEAT".into(), "1".into()),
        ("SERIAL_TX_GAP_MS".into(), "0".into()),
        ("SERIAL_BACKSPACE".into(), "delete".into()),
        ("SERIAL_ENTER".into(), "cr".into()),
        ("SERIAL_RX_NEWLINE".into(), "lf-implies-cr".into()),
        ("SERIAL_HIGHLIGHT".into(), "prompt".into()),
        ("SERIAL_MOUSE".into(), "true".into()),
        ("LOGIN_USER".into(), "root".into()),
        ("LOGIN_PASS".into(), String::new()),
        ("LOGIN_PROMPT".into(), String::new()),
        ("RELAY_PORT".into(), "0".into()),
        ("RELAY_IP".into(), String::new()),
        ("RESET_CHANNEL".into(), "0".into()),
        ("DOWNLOAD_CHANNEL".into(), "0".into()),
        ("DEV_CTL".into(), String::new()),
        ("HANG_TIMEOUT".into(), "60".into()),
        ("HANG_HYSTERESIS".into(), "3".into()),
        ("BOOT_WINDOW_SECS".into(), "30".into()),
        ("MAX_ARCHIVED_LOGS".into(), "10".into()),
        ("MAX_LOG_FILE_SIZE".into(), "100".into()),
        ("DUT_DIR".into(), ".dut-serial".into()),
        ("UBOOT_INTERRUPT_STRATEGY".into(), "flood".into()),
        ("UBOOT_INTERRUPT_CHAR".into(), "ctrl_c".into()),
        ("UBOOT_PATTERN".into(), String::new()),
        ("LOCK_DIR".into(), "/tmp/sermcp/locks".into()),
        ("REFERENCE_LOG".into(), String::new()),
        ("FLASH_LOADER_CMD".into(), String::new()),
        ("FLASH_LIST_DEVICES_CMD".into(), String::new()),
        ("RESET_TIME_MS".into(), String::new()),
        ("LEARNER_STAGE_THRESHOLD".into(), String::new()),
        ("LEARNER_CRASH_THRESHOLD".into(), String::new()),
    ])
}

/// Loaded configuration (flat key-value map).
#[derive(Debug, Clone)]
pub struct Config {
    pub values: HashMap<String, String>,
    pub config_path: Option<PathBuf>,
    pub project_dir: Option<PathBuf>,
    pub format: ConfigFormat,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConfigFormat {
    /// Loaded from a `.target.jsonc` file.
    Jsonc,
    /// No config file (literal test constructions; defaults only).
    None,
}

/// Shared test fixture: the flat-values baseline every engine/handler test
/// needs (loopback dev host, relay off unless overridden, 60s hang timeout,
/// flood U-Boot interrupt). Previously five hand-rolled copies drifted.
#[cfg(test)]
pub(crate) fn test_config(
    project_dir: &std::path::Path,
    serial_port: &str,
    relay_port: &str,
    dut_name: Option<&str>,
    dut_dir: &str,
    lock_dir: &str,
) -> Config {
    let mut values = HashMap::from([
        ("DEV_HOST_IP".to_string(), "127.0.0.1".to_string()),
        ("RESET_CHANNEL".to_string(), "0".to_string()),
        ("DOWNLOAD_CHANNEL".to_string(), "0".to_string()),
        ("HANG_TIMEOUT".to_string(), "60".to_string()),
        ("HANG_HYSTERESIS".to_string(), "3".to_string()),
        ("BOOT_WINDOW_SECS".to_string(), "30".to_string()),
        ("MAX_ARCHIVED_LOGS".to_string(), "10".to_string()),
        ("MAX_LOG_FILE_SIZE".to_string(), "100".to_string()),
        ("LOGIN_USER".to_string(), "root".to_string()),
        ("LOGIN_PASS".to_string(), String::new()),
        ("SERIAL_BAUDRATE".to_string(), "9600".to_string()),
        ("UBOOT_INTERRUPT_STRATEGY".to_string(), "flood".to_string()),
    ]);
    values.insert("SERIAL_PORT".to_string(), serial_port.to_string());
    values.insert("RELAY_PORT".to_string(), relay_port.to_string());
    if let Some(name) = dut_name {
        values.insert("DUT_NAME".to_string(), name.to_string());
    }
    values.insert("DUT_DIR".to_string(), dut_dir.to_string());
    values.insert("LOCK_DIR".to_string(), lock_dir.to_string());
    Config {
        values,
        config_path: None,
        project_dir: Some(project_dir.to_path_buf()),
        format: ConfigFormat::None,
    }
}

impl Config {
    pub fn get(&self, key: &str) -> &str {
        self.values.get(key).map(|s| s.as_str()).unwrap_or("")
    }

    pub fn get_str_or(&self, key: &str, default: &str) -> String {
        let v = self.get(key);
        if v.is_empty() {
            default.to_string()
        } else {
            v.to_string()
        }
    }

    pub fn dev_host_ip(&self) -> String {
        self.get_str_or("DEV_HOST_IP", "")
    }
    pub fn serial_target(&self) -> String {
        self.get("SERIAL_PORT").trim().to_string()
    }
    pub fn relay_port(&self) -> u16 {
        self.get("RELAY_PORT").parse().unwrap_or(0)
    }
    /// true when `rfc2217 = true` in `serial`; default false = raw passthrough.
    pub fn serial_rfc2217(&self) -> bool {
        matches!(
            self.get("SERIAL_RFC2217").to_ascii_lowercase().as_str(),
            "true" | "1" | "yes"
        )
    }
    /// Copies emitted per logical serial payload byte; default is one.
    pub fn serial_tx_repeat(&self) -> usize {
        self.get("SERIAL_TX_REPEAT").parse().unwrap_or(1)
    }
    /// Delay between logical serial payload bytes; default is no extra delay.
    pub fn serial_tx_gap(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.get("SERIAL_TX_GAP_MS").parse().unwrap_or(0))
    }
    pub fn relay_type(&self) -> String {
        resolve_relay_type(self.get("RELAY_TYPE"), &self.dev_ctl())
    }
    pub fn reset_channel(&self) -> u8 {
        self.get("RESET_CHANNEL").parse().unwrap_or(0)
    }
    pub fn download_channel(&self) -> u8 {
        self.get("DOWNLOAD_CHANNEL").parse().unwrap_or(0)
    }
    pub fn power_channel(&self) -> u8 {
        self.get("POWER_CHANNEL").parse().unwrap_or(0)
    }
    pub fn power_off_time_ms(&self) -> u64 {
        self.get("POWER_OFF_TIME_MS")
            .parse()
            .unwrap_or(3000)
            .max(3000)
    }
    pub fn relay_ip(&self) -> String {
        let v = self.get("RELAY_IP");
        if v.is_empty() {
            self.dev_host_ip()
        } else {
            v.to_string()
        }
    }
    pub fn hang_timeout(&self) -> u64 {
        self.get("HANG_TIMEOUT").parse().unwrap_or(60)
    }
    /// Max seconds a Booting state may last. Boards finish booting well
    /// inside this window; on expiry the state machine moves to Active even
    /// without shell-stage console evidence (a quiet console must not pin
    /// the display on `booting` forever). Active-state hang/heartbeat
    /// detection takes over from there.
    pub fn boot_window_secs(&self) -> u64 {
        self.get("BOOT_WINDOW_SECS").parse().unwrap_or(30)
    }
    pub fn hang_hysteresis(&self) -> u32 {
        // 0 would mean "DUT-off on the first unanswered probe round" — clamp
        // to 1 so at least one probe is always sent before a hang verdict.
        self.get("HANG_HYSTERESIS").parse().unwrap_or(3).max(1)
    }
    pub fn max_archived_logs(&self) -> usize {
        self.get("MAX_ARCHIVED_LOGS").parse().unwrap_or(10)
    }
    pub fn max_log_file_size_mb(&self) -> u64 {
        self.get("MAX_LOG_FILE_SIZE").parse().unwrap_or(100)
    }
    pub fn dut_dir(&self) -> String {
        self.get_str_or("DUT_DIR", ".dut-serial")
    }
    pub fn uboot_interrupt_char(&self) -> u8 {
        parse_interrupt_char(self.get("UBOOT_INTERRUPT_CHAR"))
    }
    pub fn lock_dir(&self) -> String {
        self.get_str_or("LOCK_DIR", "/tmp/sermcp/locks")
    }
    pub fn login_user(&self) -> String {
        self.get_str_or("LOGIN_USER", "root")
    }
    pub fn login_pass(&self) -> String {
        self.get("LOGIN_PASS").to_string()
    }
    /// Custom login prompt regex. Empty string → use default `(?:.*\s)?login:\s*$`.
    pub fn login_prompt(&self) -> String {
        self.get("LOGIN_PROMPT").to_string()
    }
    pub fn reference_log(&self) -> String {
        self.get("REFERENCE_LOG").to_string()
    }
    /// Minimum USB relay reset pulse time in ms. Returns 0 if not
    /// configured in `.target.jsonc` (→ use compile-time default, 3000).
    pub fn reset_time_ms(&self) -> u64 {
        self.get("RESET_TIME_MS").parse().unwrap_or(0)
    }
    /// StageLearner similarity threshold for normal boot stages (0.0–1.0).
    /// Falls back to Cargo.toml `[package.metadata.learn]` → 0.45.
    pub fn learner_stage_threshold(&self) -> f64 {
        let default: f64 = option_env!("LEARN_STAGE_THRESHOLD")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.45);
        self.get("LEARNER_STAGE_THRESHOLD")
            .parse()
            .unwrap_or(default)
    }
    /// StageLearner similarity threshold for crash detection (0.0–1.0).
    /// Falls back to Cargo.toml `[package.metadata.learn]` → 0.50.
    pub fn learner_crash_threshold(&self) -> f64 {
        let default: f64 = option_env!("LEARN_CRASH_THRESHOLD")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.50);
        self.get("LEARNER_CRASH_THRESHOLD")
            .parse()
            .unwrap_or(default)
    }
    /// Custom crash patterns for boot log scanning. Each entry is a substring
    /// (not regex). Falls back to Cargo.toml `[package.metadata.learn]`.
    pub fn crash_patterns(&self) -> Vec<String> {
        let val = self.get("CRASH_PATTERNS");
        if !val.is_empty() {
            val.split('\n')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        } else {
            option_env!("LEARN_CRASH_PATTERNS")
                .unwrap_or("Kernel panic\nBUG:\nOops:\nUnable to handle kernel\nSegmentation fault\n---[ end trace\npanic - not syncing")
                .split('\n')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        }
    }

    pub fn dev_ctl(&self) -> String {
        self.get("DEV_CTL").to_string()
    }
}

/// Parse the user-facing U-Boot interrupt character consistently across the
/// runtime and the init TEST button.
pub fn parse_interrupt_char(value: &str) -> u8 {
    parse_interrupt_char_checked(value).unwrap_or(0x03)
}

/// Strictly parse a U-Boot interrupt key before any reset or serial write.
///
/// A single ASCII character is always interpreted literally: `"2"` sends
/// ASCII `2` (`0x32`), never the control byte `0x02`. Use an explicit byte
/// spelling such as `"0x02"` or `"\\x02"` for a non-printable byte.
///
/// Function-key aliases intentionally resolve to ESC: terminal F-key sequences
/// all start with ESC, and U-Boot's autoboot handler consumes the first byte as
/// the interrupt.  This keeps the runtime representation a single byte while
/// allowing users to type `f1` through `f12` without a giant selection list.
pub fn parse_interrupt_char_checked(value: &str) -> Result<u8, String> {
    let value = value.trim();
    let lower = value.to_ascii_lowercase();
    if value.is_empty() {
        return Err("interrupt character cannot be empty".into());
    }

    match lower.as_str() {
        "esc" | "escape" | "f1" | "f2" | "f3" | "f4" | "f5" | "f6" | "f7" | "f8" | "f9" | "f10"
        | "f11" | "f12" => return Ok(0x1b),
        "space" => return Ok(b' '),
        "tab" => return Ok(b'\t'),
        "enter" | "return" | "cr" => return Ok(b'\r'),
        "lf" | "newline" => return Ok(b'\n'),
        _ => {}
    }

    if let Some(letter) = lower
        .strip_prefix("ctrl_")
        .or_else(|| lower.strip_prefix("ctrl-"))
        .or_else(|| lower.strip_prefix("ctrl+"))
        .or_else(|| lower.strip_prefix('^'))
    {
        let bytes = letter.as_bytes();
        if bytes.len() == 1 && bytes[0].is_ascii_alphabetic() {
            return Ok(bytes[0].to_ascii_uppercase() - b'@');
        }
        return Err(format!("invalid control-key name: {value}"));
    }

    if let Some(hex) = lower
        .strip_prefix("0x")
        .or_else(|| lower.strip_prefix("\\x"))
    {
        return u8::from_str_radix(hex, 16).map_err(|_| format!("invalid byte value: {value}"));
    }
    if value.len() == 1 && value.is_ascii() {
        return Ok(value.as_bytes()[0]);
    }
    if lower.bytes().all(|b| b.is_ascii_digit()) {
        return lower
            .parse::<u8>()
            .map_err(|_| format!("byte value is outside 0..255: {value}"));
    }

    Err(format!(
        "unsupported interrupt character {value:?}; use one ASCII character, ctrl_c, esc, space, f1..f12, 0xNN, or 0..255"
    ))
}

// ── Multi-DUT support ─────────────────────────────────────────────────

/// Dev host configuration extracted from a `dev_hosts[]` entry.
#[derive(Debug, Clone)]
pub struct DevHostConfig {
    pub ip: String,
    pub user: String,
    pub pass: String,
    /// Display name — the JSONC host_name when set, else the host ip.
    /// Resolved once here (never a lookup key).
    pub host_name: String,
}

/// Per-DUT configuration extracted from a host's nested `duts[]` entry.
#[derive(Debug, Clone)]
pub struct DutConfig {
    pub dut_name: String,
    pub dev_host_ip: String,
    pub dev_host_user: String,
    pub dev_host_pass: String,
    pub serial_port: String,
    pub relay_ip: String,
    pub relay_type: String,
    pub dev_ctl: String,
    pub relay_port: u16,
    pub reset_ch: u8,
    pub download_ch: u8,
    pub power_ch: u8,
    pub power_off_time_ms: u64,
    pub reference_log: String,
    pub dut_dir: String,
    pub login_user: String,
    pub login_pass: String,
    pub learner_stage_threshold: f64,
    pub learner_crash_threshold: f64,
    pub crash_patterns: Vec<String>,
    /// Per-DUT section values as flat config keys (SERIAL_BAUDRATE,
    /// SERIAL_RFC2217, UBOOT_INTERRUPT_CHAR/STRATEGY, FLASH_*, HANG_*,
    /// MAX_ARCHIVED_LOGS, MAX_LOG_FILE_SIZE, RESET_TIME_MS, LOGIN_PROMPT,
    /// POWER_CHANNEL, POWER_OFF_TIME_MS, LEARNER_*, CRASH_PATTERNS, ...).
    /// Built by `JsoncSections::to_map` — the same key list the global
    /// parser uses — and applied key-by-key in `to_config_map`, so a per-DUT
    /// override can never drop a key the global config understands.
    pub section_values: HashMap<String, String>,
}

impl DutConfig {
    /// Build a `Config` map from this DUT config, inheriting from the global config.
    pub fn to_config_map(&self, global: &HashMap<String, String>) -> HashMap<String, String> {
        let mut cfg = global.clone();
        // Per-DUT section keys first (single key list, see
        // JsoncSections::to_map); the resolved typed values below override
        // wherever both exist.
        cfg.extend(self.section_values.clone());
        // Override with DUT-specific values
        cfg.insert("DUT_NAME".into(), self.dut_name.clone());
        if !self.dev_host_ip.is_empty() {
            cfg.insert("DEV_HOST_IP".into(), self.dev_host_ip.clone());
        }
        if !self.dev_host_user.is_empty() {
            cfg.insert("DEV_HOST_USER".into(), self.dev_host_user.clone());
        }
        if !self.dev_host_pass.is_empty() {
            cfg.insert("DEV_HOST_PASS".into(), self.dev_host_pass.clone());
        }
        if !self.serial_port.is_empty() {
            cfg.insert("SERIAL_PORT".into(), self.serial_port.clone());
        }
        if !self.relay_ip.is_empty() {
            cfg.insert("RELAY_IP".into(), self.relay_ip.clone());
        }
        if !self.relay_type.is_empty() {
            cfg.insert("RELAY_TYPE".into(), self.relay_type.clone());
        }
        if !self.dev_ctl.is_empty() {
            cfg.insert("DEV_CTL".into(), self.dev_ctl.clone());
        }
        if self.relay_port > 0 {
            cfg.insert("RELAY_PORT".into(), self.relay_port.to_string());
        }
        if self.reset_ch > 0 {
            cfg.insert("RESET_CHANNEL".into(), self.reset_ch.to_string());
        }
        if self.download_ch > 0 {
            cfg.insert("DOWNLOAD_CHANNEL".into(), self.download_ch.to_string());
        }
        if self.power_ch > 0 {
            cfg.insert("POWER_CHANNEL".into(), self.power_ch.to_string());
        }
        if self.power_off_time_ms > 0 {
            cfg.insert(
                "POWER_OFF_TIME_MS".into(),
                self.power_off_time_ms.to_string(),
            );
        }
        if !self.reference_log.is_empty() {
            cfg.insert("REFERENCE_LOG".into(), self.reference_log.clone());
        }
        if !self.dut_dir.is_empty() {
            cfg.insert("DUT_DIR".into(), self.dut_dir.clone());
        }
        if !self.login_user.is_empty() {
            cfg.insert("LOGIN_USER".into(), self.login_user.clone());
        }
        if !self.login_pass.is_empty() {
            cfg.insert("LOGIN_PASS".into(), self.login_pass.clone());
        }
        if self.learner_stage_threshold > 0.0 {
            cfg.insert(
                "LEARNER_STAGE_THRESHOLD".into(),
                self.learner_stage_threshold.to_string(),
            );
        }
        if self.learner_crash_threshold > 0.0 {
            cfg.insert(
                "LEARNER_CRASH_THRESHOLD".into(),
                self.learner_crash_threshold.to_string(),
            );
        }
        if !self.crash_patterns.is_empty() {
            cfg.insert("CRASH_PATTERNS".into(), self.crash_patterns.join("\n"));
        }
        cfg
    }
}

// ── JSONC schema ──────────────────────────────────────────────────────

/// Strict parse options for `.target.jsonc`: comments and trailing commas
/// (the JSONC dialect), everything else strict JSON. A typo — single quotes,
/// hex numbers, unquoted property names, missing commas — must fail loudly,
/// never be silently normalized away.
pub fn jsonc_options() -> jsonc_parser::ParseOptions {
    jsonc_parser::ParseOptions {
        allow_comments: true,
        allow_trailing_commas: true,
        allow_loose_object_property_names: false,
        allow_missing_commas: false,
        allow_single_quoted_strings: false,
        allow_hexadecimal_numbers: false,
        allow_unary_plus_numbers: false,
    }
}

/// JSON number-or-string scalar: serial `port`/`baudrate` accept either an
/// integer or a quoted integer; serial.port may also be a device path string.
/// Floats are accepted so an explicit `0.0` renders as the `"0"` unset
/// sentinel instead of a parse error.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(untagged)]
pub enum NumberOrString {
    Int(u64),
    Float(f64),
    String(String),
}

impl NumberOrString {
    /// Render the value: integers as digits, floats via Rust float
    /// formatting (0.0 → "0"), strings verbatim.
    pub fn to_value_string(&self) -> String {
        match self {
            Self::Int(n) => n.to_string(),
            Self::Float(f) => f.to_string(),
            Self::String(s) => s.clone(),
        }
    }
}

/// The section fields shared by the top level and every DUT. The fields are
/// spelled out in `TargetJsoncConfig` AND `JsoncDut` (must stay in sync with
/// this struct) because `serde(flatten)` is deliberately avoided — it
/// silently swallows unknown keys, defeating `deny_unknown_fields`. Both
/// containers convert into `JsoncSections` via `From` for key extraction.
#[derive(Debug, Clone, Default)]
pub struct JsoncSections {
    /// Serial console section.
    pub serial: Option<JsoncSerial>,
    /// Login/console credentials.
    pub target: Option<JsoncTarget>,
    /// U-Boot interaction knobs.
    pub uboot: Option<JsoncUboot>,
    /// Power-control relay section.
    pub relay: Option<JsoncRelay>,
    /// Boot monitoring section.
    pub monitor: Option<JsoncMonitor>,
    /// Firmware flashing section.
    pub flash: Option<JsoncFlash>,
    /// Download-mode entry overlay.
    pub download: Option<JsoncDownload>,
    /// External device control section.
    pub control: Option<JsoncControl>,
    /// Boot loader type ("uboot" | "uefi"). "uboot" is the current
    /// behavior; "uefi" is a not-yet-implemented stub (the wizard shows
    /// "Not implemented" when selected). Not yet consumed by the runtime.
    pub loader: Option<String>,
    /// Backward-compat `reference_log` key (file top level or inside a
    /// DUT). Overrides `monitor.reference_log` when both are set,
    /// matching the historical global parser precedence.
    pub reference_log: Option<String>,
    /// Backward-compat `dut_dir` key (file top level or inside a DUT).
    pub dut_dir: Option<String>,
}

/// Root `.target.jsonc` document. Top-level sections are GLOBAL fallbacks
/// for every DUT that does not override them.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetJsoncConfig {
    #[serde(default)]
    pub dev_hosts: Vec<JsoncDevHost>,
    /// Serial console section (global fallback).
    pub serial: Option<JsoncSerial>,
    /// Login/console credentials (global fallback).
    pub target: Option<JsoncTarget>,
    /// U-Boot interaction knobs (global fallback).
    pub uboot: Option<JsoncUboot>,
    /// Power-control relay section (global fallback).
    pub relay: Option<JsoncRelay>,
    /// Boot monitoring section (global fallback).
    pub monitor: Option<JsoncMonitor>,
    /// Firmware flashing section (global fallback).
    pub flash: Option<JsoncFlash>,
    /// Download-mode entry overlay (global fallback).
    pub download: Option<JsoncDownload>,
    /// External device control section (global fallback).
    pub control: Option<JsoncControl>,
    /// Boot loader type (global fallback): "uboot" | "uefi". "uefi" is a
    /// not-yet-implemented stub; "uboot" is the current behavior.
    pub loader: Option<String>,
    /// Backward-compat `reference_log` key (top level).
    pub reference_log: Option<String>,
    /// Backward-compat `dut_dir` key (top level).
    pub dut_dir: Option<String>,
    /// Top-level ONLY — never valid inside a DUT (denied by the DUT's own
    /// `deny_unknown_fields`).
    pub lock_dir: Option<String>,
}

/// One `dev_hosts[]` entry. Its `duts[]` NEST under it — a DUT inherits its
/// parent host's ip/user/pass. `host_name` is display-only and is NEVER a
/// lookup key; the host ip is the linking key.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JsoncDevHost {
    pub host_name: Option<String>,
    pub ip: Option<String>,
    pub user: Option<String>,
    pub pass: Option<String>,
    #[serde(default)]
    pub duts: Vec<JsoncDut>,
}

/// One DUT entry nested inside a dev host. `dut_name` is required and unique
/// across ALL DUTs — it is the `.dut-serial/<dut_name>` identity.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JsoncDut {
    pub dut_name: String,
    /// Serial console section (DUT override).
    pub serial: Option<JsoncSerial>,
    /// Login/console credentials (DUT override).
    pub target: Option<JsoncTarget>,
    /// U-Boot interaction knobs (DUT override).
    pub uboot: Option<JsoncUboot>,
    /// Power-control relay section (DUT override).
    pub relay: Option<JsoncRelay>,
    /// Boot monitoring section (DUT override).
    pub monitor: Option<JsoncMonitor>,
    /// Firmware flashing section (DUT override).
    pub flash: Option<JsoncFlash>,
    /// Download-mode entry overlay (DUT override).
    pub download: Option<JsoncDownload>,
    /// External device control section (DUT override).
    pub control: Option<JsoncControl>,
    /// Boot loader type (DUT override): "uboot" | "uefi". "uefi" is a
    /// not-yet-implemented stub; "uboot" is the current behavior.
    pub loader: Option<String>,
    /// Backward-compat `reference_log` key (inside a DUT).
    pub reference_log: Option<String>,
    /// Backward-compat `dut_dir` key (inside a DUT).
    pub dut_dir: Option<String>,
}

#[derive(Default, Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JsoncSerial {
    /// ser2net port OR device path "/dev/ttyUSB0"; number or string.
    /// 0 / "0" / 0.0 = unset sentinel (validation error, as before).
    pub port: Option<NumberOrString>,
    pub baudrate: Option<NumberOrString>,
    pub rfc2217: Option<bool>,
    /// Copies emitted per logical payload byte. Valid range: 1..=8.
    pub tx_repeat: Option<u8>,
    /// Delay between logical payload-byte groups. Valid range: 0..=1000 ms.
    pub tx_gap_ms: Option<u64>,
    /// Backspace key encoding for `dutabo serial`: "delete" (0x7f, default)
    /// or "ctrl-h" (0x08).
    pub backspace: Option<String>,
    /// Enter key encoding: "cr" (default), "lf" or "crlf".
    pub enter: Option<String>,
    /// Display handling of a lone LF in RX: "lf-implies-cr" (default,
    /// matches the classic relay behavior) or "raw" (true VT column-keeping
    /// LF).
    pub rx_newline: Option<String>,
    /// Semantic prompt highlighting in `dutabo serial`: "prompt" (default)
    /// or "off".
    pub highlight: Option<String>,
    /// Capture mouse reporting in `dutabo serial` so the wheel drives the
    /// local scrollback: true (default) or false.
    pub mouse: Option<bool>,
}

#[derive(Default, Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JsoncTarget {
    pub login_user: Option<String>,
    pub login_pass: Option<String>,
    /// Custom login prompt regex. If not set, defaults to:
    /// `(?m)(?:.*\\s)?login:\\s*$`
    pub login_prompt: Option<String>,
}

#[derive(Default, Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JsoncUboot {
    pub interrupt_char: Option<String>,
    pub interrupt_strategy: Option<String>,
    pub pattern: Option<String>,
}

#[derive(Default, Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JsoncRelay {
    #[serde(rename = "type")]
    pub relay_type: Option<String>,
    pub port: Option<u16>,
    pub ip: Option<String>,
    #[serde(alias = "reset_channel")]
    pub reset_ch: Option<u8>,
    #[serde(alias = "download_channel")]
    pub download_ch: Option<u8>,
    #[serde(alias = "power_channel")]
    pub power_ch: Option<u8>,
    /// Minimum USB relay reset pulse time in ms. Overrides Cargo.toml
    /// default (3000). 0 or missing → use compile-time default.
    pub reset_time_ms: Option<u64>,
    /// Minimum power-off duration in ms. Clamped to >= 3000.
    pub power_off_time_ms: Option<u64>,
}

#[derive(Default, Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JsoncMonitor {
    pub hang_timeout: Option<u64>,
    pub hang_hysteresis: Option<u32>,
    pub boot_window_secs: Option<u64>,
    pub max_archived_logs: Option<usize>,
    pub max_log_file_size: Option<u64>,
    pub reference_log: Option<String>,
    /// StageLearner similarity threshold for normal boot stages (0.0–1.0).
    /// Default: 0.45 (from Cargo.toml [package.metadata.learn]).
    pub learner_stage_threshold: Option<f64>,
    /// StageLearner similarity threshold for crash detection (0.0–1.0).
    /// Default: 0.50 (from Cargo.toml [package.metadata.learn]).
    pub learner_crash_threshold: Option<f64>,
    /// Custom crash keywords for boot log scanning. Each string is matched as
    /// a substring (not regex). Defaults from Cargo.toml [package.metadata.learn].
    /// The old spelling `crash_patterns` keeps parsing via the serde alias.
    #[serde(alias = "crash_patterns")]
    pub crash_keywords: Option<Vec<String>>,
}

#[derive(Default, Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JsoncFlash {
    pub full_image_cmd: Option<String>,
    pub kernel_image_cmd: Option<String>,
    pub loader_bin: Option<String>,
    /// Legacy spelling of `download.loader_cmd` (migration fallback).
    pub loader_cmd: Option<String>,
    /// Legacy spelling of `download.list_devices_cmd`.
    pub list_devices_cmd: Option<String>,
}

/// Download-mode entry overlay. Only the provider's platform commands
/// are overlay-able here; a missing field uses the selected
/// DownloadProvider's default. (Legacy `flash.loader_cmd` /
/// `flash.list_devices_cmd` are read as a migration fallback.)
#[derive(Default, Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JsoncDownload {
    pub loader_cmd: Option<String>,
    pub list_devices_cmd: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JsoncControl {
    pub dev_ctrl: Option<String>,
}

impl From<&TargetJsoncConfig> for JsoncSections {
    fn from(t: &TargetJsoncConfig) -> Self {
        Self {
            serial: t.serial.clone(),
            target: t.target.clone(),
            uboot: t.uboot.clone(),
            relay: t.relay.clone(),
            monitor: t.monitor.clone(),
            flash: t.flash.clone(),
            download: t.download.clone(),
            control: t.control.clone(),
            loader: t.loader.clone(),
            reference_log: t.reference_log.clone(),
            dut_dir: t.dut_dir.clone(),
        }
    }
}

impl From<&JsoncDut> for JsoncSections {
    fn from(dut: &JsoncDut) -> Self {
        Self {
            serial: dut.serial.clone(),
            target: dut.target.clone(),
            uboot: dut.uboot.clone(),
            relay: dut.relay.clone(),
            monitor: dut.monitor.clone(),
            flash: dut.flash.clone(),
            download: dut.download.clone(),
            control: dut.control.clone(),
            loader: dut.loader.clone(),
            reference_log: dut.reference_log.clone(),
            dut_dir: dut.dut_dir.clone(),
        }
    }
}

impl JsoncSections {
    /// Extract every configured key into a flat config map.
    ///
    /// This is the ONE key list for section extraction: the global parser,
    /// the first-DUT leakage and the per-DUT `section_values` (applied by
    /// `DutConfig::to_config_map`) all go through here, so the three paths
    /// cannot drift apart. Includes every global key (`relay.power_ch`,
    /// `relay.power_off_time_ms`, `monitor.learner_stage_threshold`,
    /// `monitor.learner_crash_threshold`, `monitor.crash_keywords`), so
    /// global settings for them take effect.
    ///
    /// Zero-guarded insertion: an explicit 0 / "" / 0.0 is NEVER inserted
    /// for relay port/channels/reset-time/power-off/thresholds, so it
    /// inherits the (global) value instead of shadowing it. `serial.port`
    /// is deliberately NOT zero-guarded: its 0 is a validation sentinel.
    fn to_map(&self) -> HashMap<String, String> {
        let mut cfg = HashMap::new();
        if let Some(ref s) = self.serial {
            if let Some(ref v) = s.port {
                cfg.insert("SERIAL_PORT".into(), v.to_value_string());
            }
            if let Some(ref v) = s.baudrate {
                cfg.insert("SERIAL_BAUDRATE".into(), v.to_value_string());
            }
            if let Some(v) = s.rfc2217 {
                cfg.insert("SERIAL_RFC2217".into(), v.to_string());
            }
            if let Some(v) = s.tx_repeat {
                cfg.insert("SERIAL_TX_REPEAT".into(), v.to_string());
            }
            if let Some(v) = s.tx_gap_ms {
                cfg.insert("SERIAL_TX_GAP_MS".into(), v.to_string());
            }
            if let Some(v) = s.mouse {
                cfg.insert("SERIAL_MOUSE".into(), v.to_string());
            }
            for (field, key) in [
                (&s.backspace, "SERIAL_BACKSPACE"),
                (&s.enter, "SERIAL_ENTER"),
                (&s.rx_newline, "SERIAL_RX_NEWLINE"),
                (&s.highlight, "SERIAL_HIGHLIGHT"),
            ] {
                if let Some(v) = field
                    && !v.trim().is_empty()
                {
                    cfg.insert(key.into(), v.trim().to_string());
                }
            }
        }
        if let Some(ref tg) = self.target {
            if let Some(ref v) = tg.login_user {
                cfg.insert("LOGIN_USER".into(), v.clone());
            }
            if let Some(ref v) = tg.login_pass {
                cfg.insert("LOGIN_PASS".into(), v.clone());
            }
            if let Some(ref v) = tg.login_prompt {
                cfg.insert("LOGIN_PROMPT".into(), v.clone());
            }
        }
        if let Some(ref ub) = self.uboot {
            if let Some(ref v) = ub.interrupt_char {
                cfg.insert("UBOOT_INTERRUPT_CHAR".into(), v.clone());
            }
            if let Some(ref v) = ub.interrupt_strategy {
                cfg.insert("UBOOT_INTERRUPT_STRATEGY".into(), v.clone());
            }
            if let Some(ref v) = ub.pattern {
                cfg.insert("UBOOT_PATTERN".into(), v.clone());
            }
        }
        if let Some(ref r) = self.relay {
            if let Some(ref v) = r.relay_type {
                cfg.insert("RELAY_TYPE".into(), v.clone());
            }
            if let Some(v) = r.port.filter(|&p| p > 0) {
                cfg.insert("RELAY_PORT".into(), v.to_string());
            }
            if let Some(ref v) = r.ip {
                cfg.insert("RELAY_IP".into(), v.clone());
            }
            // ch/channel spellings collapse into ONE field via the serde
            // alias (both present = loud duplicate-field error). The
            // ZERO-FILTERED rule applies on BOTH paths: an explicit 0 can
            // never shadow a non-zero global channel.
            if let Some(v) = r.reset_ch.filter(|&c| c > 0) {
                cfg.insert("RESET_CHANNEL".into(), v.to_string());
            }
            if let Some(v) = r.download_ch.filter(|&c| c > 0) {
                cfg.insert("DOWNLOAD_CHANNEL".into(), v.to_string());
            }
            if let Some(v) = r.power_ch.filter(|&c| c > 0) {
                cfg.insert("POWER_CHANNEL".into(), v.to_string());
            }
            if let Some(v) = r.reset_time_ms.filter(|&v| v > 0) {
                cfg.insert("RESET_TIME_MS".into(), v.to_string());
            }
            if let Some(v) = r.power_off_time_ms.filter(|&v| v > 0) {
                cfg.insert("POWER_OFF_TIME_MS".into(), v.to_string());
            }
        }
        if let Some(ref m) = self.monitor {
            if let Some(v) = m.hang_timeout {
                cfg.insert("HANG_TIMEOUT".into(), v.to_string());
            }
            if let Some(v) = m.hang_hysteresis {
                cfg.insert("HANG_HYSTERESIS".into(), v.to_string());
            }
            if let Some(v) = m.boot_window_secs {
                cfg.insert("BOOT_WINDOW_SECS".into(), v.to_string());
            }
            if let Some(v) = m.max_archived_logs {
                cfg.insert("MAX_ARCHIVED_LOGS".into(), v.to_string());
            }
            if let Some(v) = m.max_log_file_size {
                cfg.insert("MAX_LOG_FILE_SIZE".into(), v.to_string());
            }
            if let Some(ref v) = m.reference_log {
                cfg.insert("REFERENCE_LOG".into(), v.clone());
            }
            if let Some(v) = m.learner_stage_threshold.filter(|&v| v > 0.0) {
                cfg.insert("LEARNER_STAGE_THRESHOLD".into(), v.to_string());
            }
            if let Some(v) = m.learner_crash_threshold.filter(|&v| v > 0.0) {
                cfg.insert("LEARNER_CRASH_THRESHOLD".into(), v.to_string());
            }
            if let Some(ref v) = m.crash_keywords {
                // Joined with \n — the flat-map era format. An empty array
                // yields "" which falls back to LEARN_CRASH_PATTERNS / builtin.
                cfg.insert("CRASH_PATTERNS".into(), v.join("\n"));
            }
        }
        if let Some(ref f) = self.flash {
            if let Some(ref v) = f.full_image_cmd {
                cfg.insert("FLASH_FULL_IMAGE_CMD".into(), v.clone());
            }
            if let Some(ref v) = f.kernel_image_cmd {
                cfg.insert("FLASH_KERNEL_IMAGE_CMD".into(), v.clone());
            }
            if let Some(ref v) = f.loader_bin {
                cfg.insert("FLASH_LOADER_BIN".into(), v.clone());
            }
            if let Some(ref v) = f.loader_cmd {
                cfg.insert("FLASH_LOADER_CMD".into(), v.clone());
            }
            if let Some(ref v) = f.list_devices_cmd {
                cfg.insert("FLASH_LIST_DEVICES_CMD".into(), v.clone());
            }
        }
        // The download overlay wins over the legacy flash spelling at the
        // SAME level; the DUT/global merge order resolves the rest.
        if let Some(ref d) = self.download {
            if let Some(ref v) = d.loader_cmd {
                cfg.insert("FLASH_LOADER_CMD".into(), v.clone());
            }
            if let Some(ref v) = d.list_devices_cmd {
                cfg.insert("FLASH_LIST_DEVICES_CMD".into(), v.clone());
            }
        }
        if let Some(ref c) = self.control
            && let Some(ref v) = c.dev_ctrl
        {
            cfg.insert("DEV_CTL".into(), v.clone());
        }
        // Back-compat top-level/per-DUT keys override the section spelling.
        if let Some(ref v) = self.reference_log {
            cfg.insert("REFERENCE_LOG".into(), v.clone());
        }
        if let Some(ref v) = self.dut_dir {
            cfg.insert("DUT_DIR".into(), v.clone());
        }
        cfg
    }
}

// ── JSONC parsing ─────────────────────────────────────────────────────

/// Parse JSONC text into the schema struct. Errors name the offending key
/// (serde `deny_unknown_fields` / duplicate-field / type errors); the renamed
/// TOML-era `alias` keys additionally get a what-to-do hint.
pub fn parse_jsonc_text(text: &str) -> Result<TargetJsoncConfig, String> {
    jsonc_parser::parse_to_serde_value::<TargetJsoncConfig>(text, &jsonc_options()).map_err(|e| {
        let mut msg = format!("JSONC parse error: {e}");
        // Quote-style-agnostic: jsonc_parser may render serde's field
        // lists with backticks or apostrophes; the base error is
        // normalized so both the probe and the caller see plain
        // "unknown field <key>" text (the hint keeps its quotes). Two
        // static probes pin the rejector's expected-fields list: only
        // the dev-host struct's list contains `host_name`, only the DUT
        // struct's contains `dut_name` — so a stray `alias` inside a
        // DUT sub-section (e.g. `target`) gets neither hint and stays
        // position-blind.
        let base = msg.replace(['`', '\''], "");
        let hint = if base.contains("unknown field alias")
            && base.contains("expected one of host_name")
        {
            Some(
                " — dev_hosts[].alias was renamed to host_name (optional display name); \
                     rename the key in the file or re-run 'dutabo init' to regenerate",
            )
        } else if base.contains("unknown field alias") && base.contains("expected one of dut_name")
        {
            Some(
                " — duts[].alias was renamed to dut_name (required, unique across ALL DUTs); \
                     rename the key in the file or re-run 'dutabo init' to regenerate",
            )
        } else {
            None
        };
        msg = base;
        if let Some(hint) = hint {
            msg.push_str(hint);
        }
        msg
    })
}

/// Parse JSONC text into a generic `serde_json::Value` (schema-less — used
/// by the `.mcp.json` reconciler in `init.rs`). Empty/whitespace-only input
/// deserializes as `null` and is normalized to an empty object so a blank
/// `.mcp.json` behaves as "exists but has no entry".
pub fn parse_jsonc_value(text: &str) -> Result<serde_json::Value, String> {
    let parsed = jsonc_parser::parse_to_serde_value::<serde_json::Value>(text, &jsonc_options())
        .map_err(|e| format!("JSONC parse error: {e}"))?;
    if parsed.is_null() {
        return Ok(serde_json::Value::Object(serde_json::Map::new()));
    }
    Ok(parsed)
}

fn read_and_parse(path: &Path) -> Result<TargetJsoncConfig, String> {
    let content =
        std::fs::read_to_string(path).map_err(|e| format!("Cannot read {path:?}: {e}"))?;
    parse_jsonc_text(&content)
}

/// Parse the global flat config map from a `.target.jsonc` file (file keys
/// only — callers layer `defaults()` underneath). First-host-is-global and
/// first-DUT section leakage into the global map are preserved
/// (test-locked): the first DUT's sections, `DUT_NAME` and a derived
/// `DUT_DIR` land in the global map so single-DUT consumers read the same
/// keys they always did.
pub fn parse_jsonc_config(path: &Path) -> Result<HashMap<String, String>, String> {
    let t = read_and_parse(path)?;
    Ok(flatten_global(&t))
}

fn flatten_global(t: &TargetJsoncConfig) -> HashMap<String, String> {
    let mut cfg = HashMap::new();
    // Populate global config from the FIRST dev host (first-host-is-global).
    if let Some(dh) = t.dev_hosts.first() {
        if let Some(ref v) = dh.ip {
            cfg.insert("DEV_HOST_IP".into(), v.clone());
        }
        if let Some(ref v) = dh.user {
            cfg.insert("DEV_HOST_USER".into(), v.clone());
        }
        if let Some(ref v) = dh.pass {
            cfg.insert("DEV_HOST_PASS".into(), v.clone());
        }
    }
    // All global section blocks — the same key list as the per-DUT merge
    // (JsoncSections::to_map), so the two paths cannot drift apart.
    cfg.extend(JsoncSections::from(t).to_map());
    // top-level key not shared with DUT entries
    if let Some(ref v) = t.lock_dir {
        cfg.insert("LOCK_DIR".into(), v.clone());
    }
    // First-DUT leakage: the global map carries the first DUT's sections so
    // a single-DUT Config.values carries the full flat key set.
    if let Some(first) = t.dev_hosts.iter().find_map(|h| h.duts.first()) {
        cfg.insert("DUT_NAME".into(), first.dut_name.clone());
        // Auto-derive dut_dir from the name if not explicitly set
        if first.dut_dir.is_none() {
            cfg.insert("DUT_DIR".into(), format!(".dut-serial/{}", first.dut_name));
        }
        cfg.extend(JsoncSections::from(first).to_map());
    }
    cfg
}

/// Parsed `.target.jsonc`: the dev host registry plus the flattened DUTs.
/// The host registry is part of the public API because the inventory
/// (`dutabo list` / inventory.json) groups DUTs by host — a host with no
/// `duts[]` contributes a host entry but no DUT.
#[derive(Debug, Clone)]
pub struct ConfigFile {
    pub dev_hosts: Vec<DevHostConfig>,
    pub duts: Vec<DutConfig>,
}

fn host_config(host: &JsoncDevHost) -> DevHostConfig {
    DevHostConfig {
        ip: host.ip.clone().unwrap_or_default(),
        user: host.user.clone().unwrap_or_default(),
        pass: host.pass.clone().unwrap_or_default(),
        host_name: host
            .host_name
            .clone()
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| host.ip.clone().unwrap_or_default()),
    }
}

/// Parse all dev hosts and DUT entries from a `.target.jsonc` file.
/// If no DUTs exist anywhere, returns a single default DUT (never an empty
/// Vec — the old invariant) attached to the first host.
pub fn parse_config_file(path: &Path) -> Result<ConfigFile, String> {
    let t = read_and_parse(path)?;

    let global = flatten_global(&t);
    let dev_hosts: Vec<DevHostConfig> = t.dev_hosts.iter().map(host_config).collect();

    let mut duts: Vec<DutConfig> = Vec::new();
    for host in &t.dev_hosts {
        for dut in &host.duts {
            duts.push(dut_jsonc_to_config(dut, host, &global));
        }
    }
    if duts.is_empty() {
        let host = dev_hosts.first().cloned().unwrap_or_else(|| DevHostConfig {
            ip: String::new(),
            user: String::new(),
            pass: String::new(),
            host_name: String::new(),
        });
        duts.push(default_dut_config(&global, &host));
    }

    // Check for port conflicts between DUTs (warning only, preserved)
    let mut ports_seen: HashMap<String, Vec<String>> = HashMap::new();
    for dut in &duts {
        let key = format!("{}:{}", dut.dev_host_ip, dut.serial_port);
        ports_seen
            .entry(key)
            .or_default()
            .push(dut.dut_name.clone());
    }
    for (key, names) in &ports_seen {
        if names.len() > 1 {
            tracing::warn!(
                "[AGENT-NOTIFY] Port conflict: {} shared by DUTs: {:?}. \
                 Each DUT needs a unique ser2net port.",
                key,
                names
            );
        }
    }

    Ok(ConfigFile { dev_hosts, duts })
}

/// Parse all DUT entries from a `.target.jsonc` file, returning a list of
/// `DutConfig`. If no DUTs exist, returns a single default DUT with
/// "default" name (or `TARGET_DUT_NAME`).
pub fn parse_dut_configs(path: &Path) -> Result<Vec<DutConfig>, String> {
    parse_config_file(path).map(|config| config.duts)
}

/// Default DUT for configs without any `duts[]` (host fields resolved from
/// the given host with the file-global fallback, section values from the
/// global map only).
fn default_dut_config(global: &HashMap<String, String>, host: &DevHostConfig) -> DutConfig {
    let name = std::env::var("TARGET_DUT_NAME")
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "default".into());
    let dut_dir = global
        .get("DUT_DIR")
        .cloned()
        .unwrap_or_else(|| ".dut-serial".into());
    let dev_host_ip = if host.ip.is_empty() {
        global.get("DEV_HOST_IP").cloned().unwrap_or_default()
    } else {
        host.ip.clone()
    };
    let dev_host_user = if host.user.is_empty() {
        global.get("DEV_HOST_USER").cloned().unwrap_or_default()
    } else {
        host.user.clone()
    };
    let dev_host_pass = if host.pass.is_empty() {
        global.get("DEV_HOST_PASS").cloned().unwrap_or_default()
    } else {
        host.pass.clone()
    };
    DutConfig {
        dut_name: name,
        dev_host_ip,
        dev_host_user,
        dev_host_pass,
        serial_port: global.get("SERIAL_PORT").cloned().unwrap_or_default(),
        relay_ip: global.get("RELAY_IP").cloned().unwrap_or_default(),
        relay_type: resolve_relay_type(
            global.get("RELAY_TYPE").map(String::as_str).unwrap_or(""),
            global.get("DEV_CTL").map(String::as_str).unwrap_or(""),
        ),
        dev_ctl: global.get("DEV_CTL").cloned().unwrap_or_default(),
        relay_port: global
            .get("RELAY_PORT")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
        reset_ch: global
            .get("RESET_CHANNEL")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
        download_ch: global
            .get("DOWNLOAD_CHANNEL")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
        power_ch: global
            .get("POWER_CHANNEL")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
        power_off_time_ms: global
            .get("POWER_OFF_TIME_MS")
            .and_then(|v| v.parse().ok())
            .unwrap_or(3000)
            .max(3000),
        reference_log: global.get("REFERENCE_LOG").cloned().unwrap_or_default(),
        dut_dir,
        login_user: global.get("LOGIN_USER").cloned().unwrap_or_default(),
        login_pass: global.get("LOGIN_PASS").cloned().unwrap_or_default(),
        learner_stage_threshold: 0.0,
        learner_crash_threshold: 0.0,
        crash_patterns: Vec::new(),
        section_values: HashMap::new(),
    }
}

/// Flatten one nested DUT entry into a `DutConfig`, resolving every
/// preserved fallback chain against the parent host and the file-global map.
fn dut_jsonc_to_config(
    dut: &JsoncDut,
    host: &JsoncDevHost,
    global: &HashMap<String, String>,
) -> DutConfig {
    // Per-DUT section keys (single key list, see JsoncSections::to_map).
    // Must be built before the typed resolution below overrides select keys.
    let section_values = JsoncSections::from(dut).to_map();
    let name = dut.dut_name.clone();
    let dut_dir = dut
        .dut_dir
        .clone()
        .unwrap_or_else(|| format!(".dut-serial/{name}"));

    // serial.port: DUT value, else the file-global value; stays empty when
    // neither is set. Validation then reports the missing required field;
    // there is deliberately no environment-specific fallback port.
    let serial_port = dut
        .serial
        .as_ref()
        .and_then(|s| s.port.as_ref())
        .map(NumberOrString::to_value_string)
        .filter(|s| !s.is_empty())
        .or_else(|| global.get("SERIAL_PORT").cloned())
        .unwrap_or_default();

    let mut relay_port: u16 = 0;
    let mut relay_ip = String::new();
    let mut reset_ch: u8 = 0;
    let mut download_ch: u8 = 0;
    let mut power_ch: u8 = 0;
    let mut power_off_time_ms: u64 = 3000;
    if let Some(ref r) = dut.relay {
        relay_port = r.port.unwrap_or(0);
        relay_ip =
            r.ip.as_deref()
                .filter(|v| !v.is_empty())
                .map(str::to_string)
                .unwrap_or_default();
        // ch/channel spellings collapse into ONE field via the serde alias;
        // the ZERO-FILTERED rule applies here too — an explicit 0 inherits
        // the global channel instead of shadowing it (same rule the global
        // path uses).
        reset_ch = r.reset_ch.filter(|&c| c > 0).unwrap_or(0);
        download_ch = r.download_ch.filter(|&c| c > 0).unwrap_or(0);
        power_ch = r.power_ch.filter(|&c| c > 0).unwrap_or(0);
        // Explicit 0 = inherit the global value (zero-guarded, via the flat
        // map); absent = 3000; present = clamp to >= 3000.
        power_off_time_ms = match r.power_off_time_ms {
            Some(0) => 0,
            Some(n) => n.max(3000),
            None => 3000,
        };
    }
    if relay_port == 0 {
        relay_port = global
            .get("RELAY_PORT")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
    }
    if relay_ip.is_empty() {
        relay_ip = global.get("RELAY_IP").cloned().unwrap_or_default();
    }
    if relay_ip.is_empty() {
        relay_ip = host.ip.clone().unwrap_or_default(); // fall back to dev host IP
    }
    if reset_ch == 0 {
        reset_ch = global
            .get("RESET_CHANNEL")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
    }
    if power_ch == 0 {
        power_ch = global
            .get("POWER_CHANNEL")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
    }

    let mut learner_stage_threshold = 0.0f64;
    let mut learner_crash_threshold = 0.0f64;
    let mut crash_patterns: Vec<String> = Vec::new();

    // reference_log precedence: dut.monitor.reference_log -> dut back-compat
    // reference_log -> derived "{dut_dir}/reference-boot.log" -> global.
    // The derivation is always non-empty, so the global value is effectively
    // shadowed (preserved; do not 'fix').
    let mut reference_log = String::new();
    if let Some(ref m) = dut.monitor {
        reference_log = m.reference_log.clone().unwrap_or_default();
        learner_stage_threshold = m.learner_stage_threshold.unwrap_or(0.0);
        learner_crash_threshold = m.learner_crash_threshold.unwrap_or(0.0);
        crash_patterns = m.crash_keywords.clone().unwrap_or_default();
    }
    if reference_log.is_empty() {
        reference_log = match &dut.reference_log {
            Some(s) => s.clone(),
            None => format!("{dut_dir}/reference-boot.log"),
        };
    }
    if reference_log.is_empty() {
        reference_log = global.get("REFERENCE_LOG").cloned().unwrap_or_default();
    }

    let login_user = dut
        .target
        .as_ref()
        .and_then(|t| t.login_user.clone())
        .unwrap_or_else(|| global.get("LOGIN_USER").cloned().unwrap_or_default());
    let login_pass = dut
        .target
        .as_ref()
        .and_then(|t| t.login_pass.clone())
        .unwrap_or_else(|| global.get("LOGIN_PASS").cloned().unwrap_or_default());
    let dev_ctl = dut
        .control
        .as_ref()
        .and_then(|control| control.dev_ctrl.clone())
        .unwrap_or_else(|| global.get("DEV_CTL").cloned().unwrap_or_default());

    DutConfig {
        dut_name: name,
        dev_host_ip: host.ip.clone().unwrap_or_default(),
        dev_host_user: host.user.clone().unwrap_or_default(),
        dev_host_pass: host.pass.clone().unwrap_or_default(),
        relay_type: dut
            .relay
            .as_ref()
            .and_then(|r| r.relay_type.clone())
            .unwrap_or_else(|| resolve_relay_type("", &dev_ctl)),
        dev_ctl,
        serial_port,
        relay_ip,
        relay_port,
        reset_ch,
        download_ch,
        power_ch,
        power_off_time_ms,
        reference_log,
        dut_dir,
        login_user,
        login_pass,
        learner_stage_threshold,
        learner_crash_threshold,
        crash_patterns,
        section_values,
    }
}

// ── Config path discovery ─────────────────────────────────────────────

/// Find the config file: `TARGET_CONF` env override (the file is honored as
/// given; parse errors surface later), else walk-up from CWD for
/// `.target.jsonc`. Returns a guided `Err` (never a silent fallback) when
/// `TARGET_CONF` is set but the file does not exist (a stale TARGET_CONF
/// silently degrading to defaults was flagged as a defect). A missing
/// `.target.jsonc` yields `Ok(None)` — the plain config-missing path.
pub fn find_config_path() -> Result<Option<PathBuf>, String> {
    // TARGET_CONF env var overrides
    if let Ok(env) = std::env::var("TARGET_CONF") {
        let p = PathBuf::from(&env);
        if !p.exists() {
            return Err(format!("TARGET_CONF={env} does not exist"));
        }
        return Ok(Some(p));
    }
    if let Some(p) = walk_up_for(CONFIG_FILE_NAME) {
        return Ok(Some(p));
    }
    Ok(None)
}

/// Walk up from CWD looking for `file_name`. Returns the first existing file.
fn walk_up_for(file_name: &str) -> Option<PathBuf> {
    let mut d = std::env::current_dir().ok()?;
    loop {
        let candidate = d.join(file_name);
        if candidate.exists() {
            return Some(candidate);
        }
        if !d.pop() {
            break;
        }
    }
    None
}

/// Load config: defaults + `.target.jsonc` overrides.
pub fn load_config() -> Config {
    let mut values = defaults();
    let path = match find_config_path() {
        Ok(p) => p,
        Err(e) => {
            tracing::error!("{e}");
            None
        }
    };

    let format = match path {
        Some(ref p) if p.extension().is_some_and(|e| e == "jsonc") => ConfigFormat::Jsonc,
        _ => ConfigFormat::None,
    };

    if let Some(ref p) = path {
        match parse_jsonc_config(p) {
            Ok(file_cfg) => {
                values.extend(file_cfg);
                if let Ok(name) = std::env::var("TARGET_DUT_NAME")
                    && !name.trim().is_empty()
                {
                    match parse_dut_configs(p) {
                        Ok(duts) => {
                            if let Some(dut) = duts.into_iter().find(|d| d.dut_name == name) {
                                values = dut.to_config_map(&values);
                                // Per-DUT servers must write pid/state into the name
                                // dir, never the project root. Without this, DUT_DIR
                                // falls back to ".dut-serial" and the engine's
                                // write_pid lands in .dut-serial/mcp.pid — which
                                // check_project_singleton then mistakes for the
                                // project singleton, blocking the stdio server.
                                values.insert("DUT_DIR".into(), format!(".dut-serial/{name}"));
                            } else {
                                tracing::warn!(
                                    "TARGET_DUT_NAME={} not found in {}; using default DUT",
                                    name,
                                    p.display()
                                );
                            }
                        }
                        Err(e) => {
                            tracing::warn!("Cannot parse DUT configs for TARGET_DUT_NAME: {e}")
                        }
                    }
                }
            }
            Err(e) => {
                // LOUD: a strict-parse failure never falls back silently —
                // validate() re-parses and re-surfaces the error.
                tracing::error!("Cannot parse {}: {e}", p.display());
            }
        }
    }

    let config_path = path;
    let project_dir = config_path
        .as_ref()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));

    Config {
        values,
        config_path,
        project_dir,
        format,
    }
}

// ── Validation ────────────────────────────────────────────────────────

impl Config {
    /// Validate required fields at startup. Returns Err with human-readable
    /// message listing all missing/invalid fields. Pattern from serial-mcp-server.
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut errors: Vec<String> = Vec::new();

        // A config file with an EXPLICIT empty dev_hosts array
        // (`"dev_hosts": []` — the wizard's zero-host answer) is a valid
        // end state: no hosts, so no DUTs (DUTs cannot exist without a
        // dev host). Only then is the missing host-ip NOT an error — a
        // file with NO dev_hosts key at all still errors.
        let zero_hosts = self
            .config_path
            .as_ref()
            .filter(|p| p.exists())
            .is_some_and(|p| {
                parse_config_file(p)
                    .map(|parsed| {
                        parsed.dev_hosts.is_empty()
                            && std::fs::read_to_string(p)
                                .is_ok_and(|text| text.contains("\"dev_hosts\""))
                    })
                    .unwrap_or(false)
            });

        // Required: dev host IP
        if self.dev_host_ip().is_empty() && !zero_hosts {
            errors.push(
                "dev_host.ip: required field is empty. Set dev_hosts[].ip in .target.jsonc".into(),
            );
        }

        // Required: serial port. It must come from .target.jsonc; never
        // substitute a lab-specific ser2net port when absent or malformed.
        let port = self.serial_target();
        let port_valid = (port.starts_with("/dev/") || port.starts_with("COM"))
            || port.parse::<u16>().is_ok_and(|value| value > 0);
        if !port_valid && !self.dev_host_ip().is_empty() {
            // The empty-host check above already errors when there is no
            // host — don't double-error (port is meaningless without one).
            errors.push(format!(
                "dut.serial.port: required non-zero TCP port or device path is missing/invalid \
                 (current: {port:?}). Set dut[].serial.port in .target.jsonc with dutabo init"
            ));
        }

        let baudrate = self.get("SERIAL_BAUDRATE").trim();
        if port_valid
            && !baudrate
                .parse::<u32>()
                .is_ok_and(|configured| configured > 0)
        {
            errors.push(format!(
                "dut.serial.baudrate: required positive integer is missing/invalid \
                 (current: {baudrate:?}). Set dut[].serial.baudrate in .target.jsonc with dutabo init"
            ));
        }

        let tx_repeat = self.get("SERIAL_TX_REPEAT").trim();
        if !tx_repeat
            .parse::<u8>()
            .is_ok_and(|configured| (1..=8).contains(&configured))
        {
            errors.push(format!(
                "dut.serial.tx_repeat: must be an integer in 1..=8 (current: {tx_repeat:?})"
            ));
        }

        let tx_gap_ms = self.get("SERIAL_TX_GAP_MS").trim();
        if !tx_gap_ms
            .parse::<u64>()
            .is_ok_and(|configured| configured <= 1000)
        {
            errors.push(format!(
                "dut.serial.tx_gap_ms: must be an integer in 0..=1000 (current: {tx_gap_ms:?})"
            ));
        }

        // Interactive serial knobs: typos must fail loudly at load time —
        // the session falls back to defaults on unknown values otherwise.
        if let Some(v) = self.values.get("SERIAL_MOUSE")
            && crate::serial_term::parse_bool(v).is_none()
        {
            errors.push(format!(
                "dut.serial.mouse: unsupported value {v:?} (valid: true | false)"
            ));
        }
        for (key, kind, valid) in [
            ("SERIAL_BACKSPACE", "backspace", "delete | ctrl-h"),
            ("SERIAL_ENTER", "enter", "cr | lf | crlf"),
            ("SERIAL_RX_NEWLINE", "rx_newline", "raw | lf-implies-cr"),
            ("SERIAL_HIGHLIGHT", "highlight", "prompt | off"),
        ] {
            let value = self.get(key).trim();
            if value.is_empty() {
                continue; // defaults() always fills these — empty means absent map
            }
            let known = match kind {
                "backspace" => crate::serial_term::BackspaceMode::parse(value).is_some(),
                "enter" => crate::serial_term::EnterMode::parse(value).is_some(),
                "rx_newline" => crate::serial_term::RxNewlineMode::parse(value).is_some(),
                _ => crate::serial_term::highlight::HighlightStyle::parse(value).is_some(),
            };
            if !known {
                errors.push(format!(
                    "dut.serial.{kind}: unsupported value {value:?} (valid: {valid})"
                ));
            }
        }

        // Optional but warn: login_user
        if self.login_user().is_empty() {
            tracing::warn!("login_user not set — boot-to-shell detection will not work");
        }

        // Reject malformed interrupt keys during startup, before an Agent can
        // reset the target. Do not silently turn a typo into Ctrl-C.
        if let Err(error) = parse_interrupt_char_checked(self.get("UBOOT_INTERRUPT_CHAR")) {
            errors.push(format!("uboot.interrupt_char: {error}"));
        }
        validate_uboot_interrupt_settings(&self.values, "uboot", &mut errors);

        // Validate DUT configs if a config file is present
        if let Some(ref p) = self.config_path
            && p.exists()
        {
            match parse_dut_configs(p) {
                Ok(duts) => {
                    // Duplicate names break the .dut-serial/<dut_name> identity —
                    // the per-DUT pid/state directory would collide (new rule:
                    // dut_name is unique across ALL DUTs).
                    let mut names_seen = std::collections::HashSet::new();
                    for dut in &duts {
                        if !names_seen.insert(dut.dut_name.clone()) {
                            errors.push(format!(
                                "Duplicate DUT name '{}' in .target.jsonc",
                                dut.dut_name
                            ));
                        }
                        if self.dev_host_ip().is_empty() {
                            // The single actionable error is the missing host
                            // above — the port sentinel is meaningless
                            // without one (no double-error).
                        } else if dut.serial_port.is_empty() || dut.serial_port == "0" {
                            errors.push(format!("DUT '{}': serial.port is not set", dut.dut_name));
                        }
                        if !crate::power_control::supports(&dut.relay_type) {
                            errors.push(format!(
                                "DUT '{}': unsupported relay.type '{}'. Valid values: {} \
                                 (usb-relay was removed — migrate to control.dev_ctrl = \
                                 \"ch340-relay\", or \"none\" to disable power control)",
                                dut.dut_name,
                                dut.relay_type,
                                supported_relay_types().join(", ")
                            ));
                        }
                        if !dut.dev_ctl.is_empty()
                            && !matches!(
                                dut.dev_ctl.trim().to_ascii_lowercase().as_str(),
                                "ch340" | "ch340-relay" | "none" | "disabled"
                            )
                        {
                            errors.push(format!(
                                "DUT '{}': unsupported control.dev_ctrl '{}'. Valid values: \
                                 ch340-relay, none (usb-relay and external dev-ctl program \
                                 backends were removed — set \"ch340-relay\" for the CH340 \
                                 TCP relay, or \"none\" to disable power control)",
                                dut.dut_name, dut.dev_ctl
                            ));
                        }
                        let mut values = self.values.clone();
                        values.extend(dut.section_values.clone());
                        validate_uboot_interrupt_settings(
                            &values,
                            &format!("DUT '{}'.uboot", dut.dut_name),
                            &mut errors,
                        );
                    }
                }
                Err(e) => {
                    errors.push(format!("Failed to parse .target.jsonc: {e}"));
                }
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

fn validate_uboot_interrupt_settings(
    values: &HashMap<String, String>,
    label: &str,
    errors: &mut Vec<String>,
) {
    let strategy = values
        .get("UBOOT_INTERRUPT_STRATEGY")
        .map(String::as_str)
        .unwrap_or("flood")
        .trim();
    if !matches!(strategy, "flood" | "pattern") {
        errors.push(format!(
            "{label}.interrupt_strategy: unsupported value {strategy:?}; valid values: flood, pattern"
        ));
        return;
    }
    if strategy == "pattern" {
        let pattern = values
            .get("UBOOT_PATTERN")
            .map(String::as_str)
            .unwrap_or("")
            .trim();
        if pattern.is_empty() {
            errors.push(format!(
                "{label}.pattern: required when interrupt_strategy is pattern"
            ));
        } else if let Err(error) = regex::Regex::new(pattern) {
            errors.push(format!("{label}.pattern: invalid regex: {error}"));
        }
    }
}

/// Human-readable list of relay types accepted by the COMPILED power-control
/// backend — derived by filtering candidates through the dut-ctrl plugin
/// registry (`supports()`), so the accepted set can never drift from the
/// error text (the ch340 plugin is feature-gated). The candidate list only
/// affects hint completeness, never the accepted set. Public: the
/// `dutabo init` TUI uses it as the single source for its relay-type picker
/// options. (usb-relay and the external dev-ctl backends were
/// removed — only ch340-relay and none/disabled remain.)
pub fn supported_relay_types() -> Vec<&'static str> {
    ["ch340", "ch340-relay", "none", "disabled"]
        .into_iter()
        .filter(|name| crate::power_control::supports(name))
        .collect()
}

// ── Inventory building (pure, reused by inventory.rs / dutabo list) ────

/// Per-DUT state supplied by the caller (resolved from
/// `dut_state::read_dut_state` + `state_manager::format_statusline_labeled`).
/// Kept as a plain data struct so the config module never depends on the
/// state manager — both output paths (inventory.json, `dutabo list --json`)
/// must derive their state through the ONE shared helper.
#[derive(Debug, Clone, serde::Serialize)]
pub struct InventoryDutState {
    /// Raw state text, e.g. "active", "crashed", "unknown".
    pub state: String,
    /// Human classification label for the state cell.
    pub state_label: String,
    /// ANSI-formatted statusline text (format_statusline_labeled).
    pub state_text: String,
    /// ANSI-stripped plain text for non-terminal consumers.
    pub state_plain: String,
    /// State-file age in seconds; None when the file is missing.
    pub age_secs: Option<u64>,
    /// True only for crashed / DUT-off / disconnected.
    pub critical: bool,
}

/// One `dev_hosts[]` entry in the inventory schema — ip is the linking key;
/// `host_name` is display-only. Each `InventoryDut` carries its own
/// `dev_host_ip`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct InventoryDevHost {
    pub ip: String,
    pub user: String,
    pub host_name: String,
}

/// One DUT entry in the inventory schema — identical shape for
/// `inventory.json`, `dutabo list --json` and the `serial_list_duts` MCP tool.
#[derive(Debug, Clone, serde::Serialize)]
pub struct InventoryDut {
    pub dut_name: String,
    pub dev_host_ip: String,
    pub ssh_user: String,
    pub serial_port: String,
    pub dut_dir: String,
    /// Project HTTP MCP port, repeated for compatibility in each DUT entry.
    pub mcp_port: u16,
    pub state: String,
    pub state_label: String,
    pub state_text: String,
    pub state_plain: String,
    /// Pre-rendered guest-session view: the occupancy state (`dutabo`
    /// takeover, or `busy` while another agent session owns the MCP)
    /// instead of the device state.
    pub guest_display: crate::dut_state::SessionDisplay,
    pub age_secs: Option<u64>,
    pub critical: bool,
}

/// Canonical inventory schema (schema_version 1) — the single data source
/// for the Python hooks after the migration.
#[derive(Debug, Clone, serde::Serialize)]
pub struct InventoryJson {
    pub schema_version: u32,
    pub written_by: String,
    pub written_at_unix: i64,
    pub project_dir: Option<String>,
    pub config_path: Option<String>,
    pub mcp_pid: Option<u32>,
    pub mcp_http_port: Option<u16>,
    /// Heartbeat interval (seconds) this server commits to: the server
    /// rewrites the inventory (refreshing `written_at_unix`) every
    /// heartbeat_secs even when no state changed. Consumers use it to bound
    /// freshness: an inventory whose `written_at_unix` is older than
    /// `3 × heartbeat_secs` is STALE — the server died (or was killed -9,
    /// which leaves no cleanup hook), and the snapshot must not render as
    /// live even if the pid was recycled. `null` = legacy server that only
    /// writes on startup/state-change (no freshness contract).
    pub heartbeat_secs: Option<u32>,
    /// The TARGET_DUT_NAME value when set AND found in `duts`; null otherwise.
    pub current_dut: Option<String>,
    pub dev_host_count: usize,
    pub dut_count: usize,
    pub dev_hosts: Vec<InventoryDevHost>,
    pub duts: Vec<InventoryDut>,
    /// Multi-agent ownership record (from `.dut-serial/owner.json`): which
    /// agent session owns this project's MCP lifecycle. Display layers use
    /// it to render the guest/owner marker — cross-session display is
    /// forbidden.
    pub owner: Option<InventoryOwner>,
}

/// Ownership record carried into the inventory header.
#[derive(Debug, Clone, serde::Serialize)]
pub struct InventoryOwner {
    pub owner_kind: String,
    pub owner_session_id: Option<String>,
    pub owner_mcp_pid: Option<u32>,
    pub claimed_at_unix: Option<i64>,
}

/// Write metadata stamped into the inventory JSON header.
#[derive(Debug, Clone)]
pub struct InventoryMeta<'a> {
    pub written_by: &'a str,
    pub written_at_unix: i64,
    pub mcp_pid: Option<u32>,
    pub mcp_http_port: Option<u16>,
    pub heartbeat_secs: Option<u32>,
    pub owner: Option<InventoryOwner>,
}

/// Pure inventory builder: every input is a parameter, no env/fs reads.
/// `current_dut_name` is the raw TARGET_DUT_NAME value; it becomes
/// `current_dut` only when a DUT with that name exists. `state_for` is the
/// single per-DUT state resolver — inventory.rs (server), `dutabo list` and
/// `serial_list_duts` all call this so their output can never diverge.
pub fn build_inventory_json(
    config: &Config,
    dev_hosts: &[DevHostConfig],
    duts: &[DutConfig],
    current_dut_name: Option<&str>,
    meta: InventoryMeta<'_>,
    state_for: impl Fn(&DutConfig) -> InventoryDutState,
) -> InventoryJson {
    let project_dir = config
        .project_dir
        .as_deref()
        .unwrap_or_else(|| Path::new("."));

    let current_dut = current_dut_name
        .filter(|name| duts.iter().any(|d| d.dut_name == *name))
        .map(str::to_string);

    let dev_hosts = dev_hosts
        .iter()
        .map(|host| InventoryDevHost {
            ip: host.ip.clone(),
            user: host.user.clone(),
            host_name: host.host_name.clone(),
        })
        .collect::<Vec<_>>();

    let dut_entries = duts
        .iter()
        .map(|dut| {
            let state = state_for(dut);
            let guest_display =
                crate::dut_state::session_display(&state.state, &dut.dut_name, true);
            InventoryDut {
                guest_display,
                dut_name: dut.dut_name.clone(),
                dev_host_ip: dut.dev_host_ip.clone(),
                ssh_user: dut.dev_host_user.clone(),
                serial_port: dut.serial_port.clone(),
                dut_dir: dut.dut_dir.clone(),
                mcp_port: crate::ports::project_dut_mcp_port(project_dir, &dut.dut_name),
                state: state.state,
                state_label: state.state_label,
                state_text: state.state_text,
                state_plain: state.state_plain,
                age_secs: state.age_secs,
                critical: state.critical,
            }
        })
        .collect::<Vec<_>>();

    InventoryJson {
        schema_version: 1,
        written_by: meta.written_by.to_string(),
        written_at_unix: meta.written_at_unix,
        project_dir: config
            .project_dir
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
        config_path: config
            .config_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
        mcp_pid: meta.mcp_pid,
        mcp_http_port: meta.mcp_http_port,
        heartbeat_secs: meta.heartbeat_secs,
        current_dut,
        dev_host_count: dev_hosts.len(),
        dut_count: dut_entries.len(),
        dev_hosts,
        duts: dut_entries,
        owner: meta.owner,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tempfile::TempDir;

    /// Serialize tests that manipulate global env vars or the CWD to prevent
    /// races in parallel test execution (cargo test runs N threads).
    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    /// Restore the previous CWD on drop (walk-up discovery tests chdir into
    /// temp dirs and must never leak the CWD to sibling tests).
    struct CwdGuard(PathBuf);
    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.0);
        }
    }

    fn chdir_tmp(dir: &Path) -> CwdGuard {
        let old = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir).unwrap();
        CwdGuard(old)
    }

    fn write_jsonc(dir: &Path, content: &str) -> PathBuf {
        let conf = dir.join(".target.jsonc");
        std::fs::write(&conf, content).unwrap();
        conf
    }

    /// Minimal single-host / single-DUT fixture used across many tests.
    const SIMPLE_FIXTURE: &str = r#"{
  "dev_hosts": [
    {
      "ip": "host-a.invalid",
      "user": "linaro",
      "duts": [
        { "dut_name": "rk3576", "serial": { "port": 41002 } }
      ]
    }
  ]
}"#;

    #[test]
    fn test_defaults() {
        let d = defaults();
        assert_eq!(d.get("DEV_HOST_IP").unwrap(), "");
        assert_eq!(d.get("SERIAL_PORT").unwrap(), "");
        assert_eq!(d.get("HANG_TIMEOUT").unwrap(), "60");
        assert_eq!(
            d.get("BOOT_WINDOW_SECS").unwrap(),
            "30",
            "booting → active deadline defaults to 30s"
        );
        // SERIAL_PROTOCOL is a dead key (no readers).
        assert!(!d.contains_key("SERIAL_PROTOCOL"));
    }

    // ── Config parsing tests ───────────────────────────────────────────

    #[test]
    fn test_parse_jsonc_config() {
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{
  "dev_hosts": [
    { "ip": "10.0.0.99", "user": "linaro" }
  ],
  "serial": { "port": 9999 },
  "target": { "login_user": "admin", "login_pass": "secret" },
  "uboot": { "interrupt_char": "ctrl_c", "interrupt_strategy": "flood" },
  "relay": { "port": 41001, "reset_channel": 1, "download_channel": 2 },
  "monitor": { "hang_timeout": 120 }
}"#,
        );
        let cfg = parse_jsonc_config(&conf).unwrap();
        assert_eq!(cfg.get("DEV_HOST_IP").unwrap(), "10.0.0.99");
        assert_eq!(cfg.get("DEV_HOST_USER").unwrap(), "linaro");
        assert_eq!(cfg.get("SERIAL_PORT").unwrap(), "9999");
        assert_eq!(cfg.get("LOGIN_USER").unwrap(), "admin");
        assert_eq!(cfg.get("LOGIN_PASS").unwrap(), "secret");
        assert_eq!(cfg.get("UBOOT_INTERRUPT_CHAR").unwrap(), "ctrl_c");
        assert_eq!(cfg.get("UBOOT_INTERRUPT_STRATEGY").unwrap(), "flood");
        assert_eq!(cfg.get("RELAY_PORT").unwrap(), "41001");
        // _channel aliases work on the global path
        assert_eq!(cfg.get("RESET_CHANNEL").unwrap(), "1");
        assert_eq!(cfg.get("DOWNLOAD_CHANNEL").unwrap(), "2");
        assert_eq!(cfg.get("HANG_TIMEOUT").unwrap(), "120");
    }

    #[test]
    fn test_jsonc_config_partial() {
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{ "dev_hosts": [ { "ip": "host.invalid" } ], "serial": { "port": 41000 } }"#,
        );
        let cfg = parse_jsonc_config(&conf).unwrap();
        assert_eq!(cfg.get("DEV_HOST_IP").unwrap(), "host.invalid");
        assert_eq!(cfg.get("SERIAL_PORT").unwrap(), "41000");
        // Not set → absent
        assert!(!cfg.contains_key("LOGIN_USER"));
    }

    #[test]
    fn test_jsonc_config_single_host() {
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{
  "dev_hosts": [
    { "ip": "10.0.0.100", "user": "modern" }
  ],
  "serial": { "port": 3000 }
}"#,
        );
        let cfg = parse_jsonc_config(&conf).unwrap();
        assert_eq!(cfg.get("DEV_HOST_IP").unwrap(), "10.0.0.100");
        assert_eq!(cfg.get("DEV_HOST_USER").unwrap(), "modern");
        assert_eq!(cfg.get("SERIAL_PORT").unwrap(), "3000");
    }

    #[test]
    fn test_jsonc_config_multi_hosts_multi_dut() {
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{
  "dev_hosts": [
    {
      "ip": "host-a.invalid",
      "user": "linaro",
      "duts": [
        {
          "dut_name": "rk3576",
          "serial": { "port": 41000 },
          "target": { "login_user": "root" },
          "uboot": { "interrupt_char": "ctrl_c", "interrupt_strategy": "flood" },
          "relay": { "port": 41001, "reset_ch": 1 }
        }
      ]
    },
    {
      "ip": "host-b.invalid",
      "user": "root",
      "duts": [
        {
          "dut_name": "rk3588",
          "serial": { "port": 2010 },
          "target": { "login_user": "testuser", "login_pass": "pass" },
          "monitor": {
            "hang_timeout": 90,
            "reference_log": ".dut-serial/rk3588/reference-boot.log"
          }
        }
      ]
    }
  ]
}"#,
        );
        let result = parse_dut_configs(&conf).unwrap();
        assert_eq!(result.len(), 2);

        let rk3576 = &result[0];
        assert_eq!(rk3576.dut_name, "rk3576");
        assert_eq!(rk3576.dev_host_ip, "host-a.invalid");
        assert_eq!(rk3576.dev_host_user, "linaro");
        assert_eq!(rk3576.serial_port, "41000");
        assert_eq!(rk3576.login_user, "root");
        assert_eq!(rk3576.reset_ch, 1);
        assert_eq!(rk3576.relay_port, 41001);

        let rk3588 = &result[1];
        assert_eq!(rk3588.dut_name, "rk3588");
        assert_eq!(rk3588.dev_host_ip, "host-b.invalid");
        assert_eq!(rk3588.dev_host_user, "root");
        assert_eq!(rk3588.serial_port, "2010");
        assert_eq!(rk3588.login_user, "testuser");
        assert_eq!(rk3588.login_pass, "pass");
    }

    /// Regression test: single-DUT with global fallback sections.
    #[test]
    fn test_parse_dut_configs_single_host() {
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{
  "dev_hosts": [
    {
      "ip": "host-a.invalid",
      "user": "linaro",
      "duts": [ { "dut_name": "rk3576" } ]
    }
  ],
  "serial": { "port": 41002 },
  "target": { "login_user": "root", "login_pass": "" },
  "uboot": { "interrupt_char": "ctrl_c", "interrupt_strategy": "flood" },
  "relay": { "type": "ch340-relay", "port": 41000 },
  "monitor": { "hang_timeout": 60, "max_archived_logs": 10 },
  "reference_log": ".dut-serial/reference-boot.log"
}"#,
        );
        let result = parse_dut_configs(&conf).unwrap();
        assert_eq!(result.len(), 1);
        let dut = &result[0];
        assert_eq!(dut.dev_host_ip, "host-a.invalid");
        assert_eq!(dut.dev_host_user, "linaro");
        assert_eq!(dut.serial_port, "41002");
        assert_eq!(dut.login_user, "root");
        assert_eq!(dut.relay_port, 41000);
    }

    #[test]
    fn test_load_config_jsonc() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".dut-serial")).unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{ "dev_hosts": [ { "ip": "10.0.0.1" } ], "serial": { "port": 5000 } }"#,
        );

        unsafe {
            std::env::set_var("TARGET_CONF", conf.to_str().unwrap());
        }
        let cfg = load_config();
        unsafe {
            std::env::remove_var("TARGET_CONF");
        }

        assert_eq!(cfg.dev_host_ip(), "10.0.0.1");
        assert_eq!(cfg.format, ConfigFormat::Jsonc);
    }

    #[test]
    fn test_uboot_interrupt_char_ctrl_c() {
        let mut v = defaults();
        v.insert("UBOOT_INTERRUPT_CHAR".into(), "ctrl_c".into());
        let cfg = Config {
            values: v,
            config_path: None,
            project_dir: None,
            format: ConfigFormat::None,
        };
        assert_eq!(cfg.uboot_interrupt_char(), 0x03);
    }

    #[test]
    fn test_uboot_interrupt_char_single_digit_is_literal_ascii() {
        let mut v = defaults();
        v.insert("UBOOT_INTERRUPT_CHAR".into(), "2".into());
        let cfg = Config {
            values: v,
            config_path: None,
            project_dir: None,
            format: ConfigFormat::None,
        };
        assert_eq!(cfg.uboot_interrupt_char(), b'2');
        assert_ne!(cfg.uboot_interrupt_char(), 0x02);
    }

    // ── DUT IP fallback tests (JSONC fixtures) ─────────────────────────

    #[test]
    fn test_dut_serial_ip_key_is_rejected() {
        // `serial.ip` no longer exists anywhere in the schema — inside a
        // DUT serial block (deny_unknown_fields) it is a hard parse error.
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{
  "dev_hosts": [
    {
      "ip": "10.0.0.1",
      "duts": [ { "dut_name": "test-dut", "serial": { "port": 41000, "ip": "serial-host.invalid" } } ]
    }
  ]
}"#,
        );
        let err = parse_dut_configs(&conf).unwrap_err();
        assert!(err.contains("unknown field"), "unexpected error: {err}");
        assert!(err.contains("ip"), "unexpected error: {err}");
    }

    #[test]
    fn test_global_serial_ip_key_is_rejected() {
        // The file-global `serial.ip` is also gone — the serial endpoint
        // always runs on the parent dev host, so the key must not parse.
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{
  "dev_hosts": [
    {
      "ip": "10.0.0.1",
      "duts": [ { "dut_name": "test-dut", "serial": { "port": 41000 } } ]
    }
  ],
  "serial": { "ip": "10.9.9.9" }
}"#,
        );
        let err = parse_dut_configs(&conf).unwrap_err();
        assert!(err.contains("unknown field"), "unexpected error: {err}");
        assert!(err.contains("ip"), "unexpected error: {err}");
    }

    #[test]
    fn test_dut_relay_ip_falls_back_to_global_then_dev_host_ip() {
        // dut.relay.ip unset, global relay.ip set → dut -> global -> host.
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{
  "dev_hosts": [
    { "ip": "10.0.0.1", "duts": [ { "dut_name": "d", "relay": {} } ] }
  ],
  "relay": { "ip": "10.0.0.9" }
}"#,
        );
        let duts = parse_dut_configs(&conf).unwrap();
        assert_eq!(duts[0].relay_ip, "10.0.0.9");
    }

    #[test]
    fn test_dut_relay_ip_falls_back_to_dev_host_ip() {
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{
  "dev_hosts": [
    { "ip": "10.0.0.1", "duts": [ { "dut_name": "d", "relay": {} } ] }
  ]
}"#,
        );
        let duts = parse_dut_configs(&conf).unwrap();
        assert_eq!(duts[0].relay_ip, "10.0.0.1");
    }

    #[test]
    fn test_dut_relay_ip_uses_explicit_value() {
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{
  "dev_hosts": [
    {
      "ip": "10.0.0.1",
      "duts": [ { "dut_name": "d", "relay": { "ip": "relay-host.invalid" } } ]
    }
  ]
}"#,
        );
        let duts = parse_dut_configs(&conf).unwrap();
        assert_eq!(duts[0].dev_host_ip, "10.0.0.1");
        assert_eq!(
            duts[0].relay_ip, "relay-host.invalid",
            "explicit relay_ip should be preserved"
        );
    }

    #[test]
    fn test_dut_relay_ip_falls_back_to_dev_host_ip_when_both_unset() {
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{
  "dev_hosts": [
    { "ip": "10.0.0.1", "duts": [ { "dut_name": "d" } ] }
  ]
}"#,
        );
        let duts = parse_dut_configs(&conf).unwrap();
        assert_eq!(
            duts[0].dev_host_ip, "10.0.0.1",
            "dev_host_ip is the host ip"
        );
        assert_eq!(duts[0].relay_ip, "10.0.0.1", "relay_ip should fall back");
    }

    /// End-to-end: the DUT's Config map must NOT contain a SERIAL_IP key —
    /// the serial host is the dev host, full stop. DEV_HOST_IP and RELAY_IP
    /// still resolve to the parent host ip via to_config_map().
    #[test]
    fn test_dut_ip_fallback_in_full_config_map() {
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{
  "dev_hosts": [
    {
      "ip": "10.0.0.1",
      "duts": [ { "dut_name": "test-dut", "serial": { "port": 41000 } } ]
    }
  ]
}"#,
        );
        let duts = parse_dut_configs(&conf).unwrap();
        let global = defaults();
        let cfg_map = duts[0].to_config_map(&global);

        assert!(
            !cfg_map.contains_key("SERIAL_IP"),
            "SERIAL_IP must never be written into a DUT's config map"
        );
        assert_eq!(cfg_map.get("RELAY_IP").unwrap(), "10.0.0.1");
        assert_eq!(cfg_map.get("DEV_HOST_IP").unwrap(), "10.0.0.1");
    }

    #[test]
    fn test_malformed_jsonc_does_not_panic() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(tmp.path(), "this is not valid jsonc {{{");
        unsafe { std::env::set_var("TARGET_CONF", conf.to_str().unwrap()) };
        let cfg = load_config(); // must not panic
        unsafe { std::env::remove_var("TARGET_CONF") };
        // Should fall back to defaults (validate() re-surfaces the error)
        assert_eq!(cfg.dev_host_ip(), "");
    }

    #[test]
    fn test_missing_all_sections_uses_defaults() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(tmp.path(), r#"{ "dev_hosts": [ { "ip": "10.0.0.1" } ] }"#);
        unsafe { std::env::set_var("TARGET_CONF", conf.to_str().unwrap()) };
        let cfg = load_config();
        unsafe { std::env::remove_var("TARGET_CONF") };
        assert_eq!(cfg.dev_host_ip(), "10.0.0.1");
        assert_eq!(cfg.serial_target(), "");
        assert!(
            cfg.validate()
                .unwrap_err()
                .iter()
                .any(|error| error.contains("dut.serial.port")),
            "missing serial.port must be a configuration error"
        );
        assert_eq!(cfg.login_user(), "root"); // default
    }

    #[test]
    fn test_empty_string_values() {
        let mut v = defaults();
        v.insert("DEV_HOST_IP".into(), "".into());
        let cfg = Config {
            values: v,
            config_path: None,
            project_dir: None,
            format: ConfigFormat::None,
        };
        assert_eq!(cfg.dev_host_ip(), "");
    }

    #[test]
    fn test_jsonc_selects_external_power_backend() {
        let tmp = TempDir::new().unwrap();
        let path = write_jsonc(
            tmp.path(),
            r#"{
  "dev_hosts": [
    {
      "ip": "127.0.0.1",
      "duts": [
        {
          "dut_name": "board",
          "serial": { "port": 41000 },
          "relay": { "type": "ch340-relay", "reset_ch": 1 },
          "control": { "dev_ctrl": "ch340-relay" }
        }
      ]
    }
  ]
}"#,
        );

        let duts = parse_dut_configs(&path).unwrap();
        assert_eq!(duts[0].relay_type, "ch340-relay");
        assert_eq!(duts[0].dev_ctl, "ch340-relay");
        let values = duts[0].to_config_map(&defaults());
        assert_eq!(
            values.get("RELAY_TYPE").map(String::as_str),
            Some("ch340-relay")
        );
        assert_eq!(
            values.get("DEV_CTL").map(String::as_str),
            Some("ch340-relay")
        );
    }

    /// Regression: `control.dev_ctrl = "ch340-relay"` (what `dutabo init`
    /// writes) with no `relay.type` must select the BUILT-IN ch340 backend —
    /// not be misread as an external PROGRAM named "ch340-relay" (that bug
    /// made every relay/reset/uboot/reference test fail against working
    /// hardware).
    #[test]
    fn test_dev_ctrl_builtin_name_selects_builtin_relay_backend() {
        let tmp = TempDir::new().unwrap();
        let path = write_jsonc(
            tmp.path(),
            r#"{
  "dev_hosts": [
    {
      "ip": "127.0.0.1",
      "duts": [
        {
          "dut_name": "board",
          "serial": { "port": 41000 },
          "relay": { "port": 41001, "reset_ch": 1 },
          "control": { "dev_ctrl": "ch340-relay" }
        }
      ]
    }
  ]
}"#,
        );

        let duts = parse_dut_configs(&path).unwrap();
        let values = duts[0].to_config_map(&defaults());
        assert_eq!(
            values.get("RELAY_TYPE").map(String::as_str),
            Some("ch340-relay"),
            "dev_ctrl='ch340-relay' resolves to the built-in ch340 backend"
        );
        assert_eq!(
            values.get("DEV_CTL").map(String::as_str),
            Some("ch340-relay")
        );
        // The built-in backend is what `relay_type()` reports, so
        // `build_power_control` never execs "ch340-relay" as a program.
        let cfg = Config {
            values: HashMap::from([("DEV_CTL".to_string(), "ch340-relay".to_string())]),
            config_path: None,
            project_dir: None,
            format: ConfigFormat::None,
        };
        assert_eq!(cfg.relay_type(), "ch340-relay");
        // The backend is REPLACEABLE: only the two selectable choices
        // (ch340-relay / none) plus the legacy ch340 spelling resolve to
        // built-ins (usb-relay and external program names were
        // removed — unknown values pass through verbatim so validation
        // rejects them as unsupported instead of treating them as programs).
        for (dev_ctl, want) in [
            ("ch340", "ch340"),
            ("ch340-relay", "ch340-relay"),
            ("none", "none"),
            ("disabled", "disabled"),
            ("usb-relay", "usb-relay"),
            ("external-dev-ctl", "external-dev-ctl"),
            ("lab-pc-ctl", "lab-pc-ctl"),
        ] {
            let c = Config {
                values: HashMap::from([("DEV_CTL".to_string(), dev_ctl.to_string())]),
                config_path: None,
                project_dir: None,
                format: ConfigFormat::None,
            };
            assert_eq!(
                c.relay_type(),
                want,
                "dev_ctrl='{dev_ctl}' must resolve to '{want}'"
            );
        }
        // An explicit `relay.type` always wins over `control.dev_ctrl`.
        let explicit = Config {
            values: HashMap::from([
                ("RELAY_TYPE".to_string(), "ch340-relay".to_string()),
                ("DEV_CTL".to_string(), "none".to_string()),
            ]),
            config_path: None,
            project_dir: None,
            format: ConfigFormat::None,
        };
        assert_eq!(explicit.relay_type(), "ch340-relay");
    }

    /// Regression: DUT name config propagation (unchanged DutConfig test).
    #[test]
    fn test_dut_name_to_config_map_preserves_all_fields() {
        let global = defaults();
        let dut = DutConfig {
            dut_name: "my-dut".into(),
            dev_host_ip: "10.0.0.1".into(),
            dev_host_user: "linaro".into(),
            dev_host_pass: String::new(),
            serial_port: "41008".into(),
            relay_ip: String::new(),
            relay_type: "ch340-relay".into(),
            dev_ctl: "ch340-relay".into(),
            relay_port: 41002,
            reset_ch: 1,
            download_ch: 2,
            power_ch: 0,
            power_off_time_ms: 3000,
            reference_log: ".dut-serial/my-dut/reference-boot.log".into(),
            dut_dir: ".dut-serial/my-dut".into(),
            login_user: "root".into(),
            login_pass: String::new(),
            learner_stage_threshold: 0.0,
            learner_crash_threshold: 0.0,
            crash_patterns: Vec::new(),
            section_values: HashMap::from([
                ("SERIAL_BAUDRATE".into(), "921600".into()),
                ("SERIAL_RFC2217".into(), "true".into()),
                ("UBOOT_INTERRUPT_CHAR".into(), "2".into()),
                ("UBOOT_INTERRUPT_STRATEGY".into(), "flood".into()),
                ("HANG_TIMEOUT".into(), "90".into()),
                ("HANG_HYSTERESIS".into(), "5".into()),
                ("MAX_ARCHIVED_LOGS".into(), "7".into()),
                ("MAX_LOG_FILE_SIZE".into(), "42".into()),
                ("RESET_TIME_MS".into(), "5000".into()),
                ("LOGIN_PROMPT".into(), "my-dut login:".into()),
                ("FLASH_FULL_IMAGE_CMD".into(), "uf {image}".into()),
                ("FLASH_KERNEL_IMAGE_CMD".into(), "di -k {image}".into()),
                ("FLASH_LOADER_BIN".into(), "/opt/loader.bin".into()),
                ("FLASH_LOADER_CMD".into(), "db {loader}".into()),
                ("FLASH_LIST_DEVICES_CMD".into(), "ld".into()),
            ]),
        };

        let cfg = dut.to_config_map(&global);

        assert_eq!(cfg.get("DUT_NAME").map(|s| s.as_str()), Some("my-dut"));
        assert_eq!(cfg.get("DEV_HOST_IP").map(|s| s.as_str()), Some("10.0.0.1"));
        assert_eq!(cfg.get("SERIAL_PORT").map(|s| s.as_str()), Some("41008"));
        assert_eq!(cfg.get("RELAY_PORT").map(|s| s.as_str()), Some("41002"));
        assert_eq!(cfg.get("RESET_CHANNEL").map(|s| s.as_str()), Some("1"));
        assert_eq!(cfg.get("DOWNLOAD_CHANNEL").map(|s| s.as_str()), Some("2"));
        assert_eq!(
            cfg.get("RELAY_TYPE").map(String::as_str),
            Some("ch340-relay")
        );
        assert_eq!(cfg.get("DEV_CTL").map(String::as_str), Some("ch340-relay"));
        assert_eq!(
            cfg.get("SERIAL_BAUDRATE").map(|s| s.as_str()),
            Some("921600")
        );
        assert_eq!(cfg.get("SERIAL_RFC2217").map(|s| s.as_str()), Some("true"));
        assert_eq!(
            cfg.get("UBOOT_INTERRUPT_CHAR").map(|s| s.as_str()),
            Some("2")
        );
        assert_eq!(
            cfg.get("UBOOT_INTERRUPT_STRATEGY").map(|s| s.as_str()),
            Some("flood")
        );
        assert_eq!(cfg.get("HANG_TIMEOUT").map(|s| s.as_str()), Some("90"));
        assert_eq!(cfg.get("HANG_HYSTERESIS").map(|s| s.as_str()), Some("5"));
        assert_eq!(cfg.get("MAX_ARCHIVED_LOGS").map(|s| s.as_str()), Some("7"));
        assert_eq!(cfg.get("MAX_LOG_FILE_SIZE").map(|s| s.as_str()), Some("42"));
        assert_eq!(cfg.get("RESET_TIME_MS").map(|s| s.as_str()), Some("5000"));
        assert_eq!(
            cfg.get("LOGIN_PROMPT").map(|s| s.as_str()),
            Some("my-dut login:")
        );
        assert_eq!(
            cfg.get("FLASH_FULL_IMAGE_CMD").map(|s| s.as_str()),
            Some("uf {image}")
        );
        assert_eq!(
            cfg.get("FLASH_KERNEL_IMAGE_CMD").map(|s| s.as_str()),
            Some("di -k {image}")
        );
        assert_eq!(
            cfg.get("FLASH_LOADER_BIN").map(|s| s.as_str()),
            Some("/opt/loader.bin")
        );
        assert_eq!(
            cfg.get("FLASH_LOADER_CMD").map(|s| s.as_str()),
            Some("db {loader}")
        );
        assert_eq!(
            cfg.get("FLASH_LIST_DEVICES_CMD").map(|s| s.as_str()),
            Some("ld")
        );
    }

    /// Regression (multi-DUT config drift): with two DUTs carrying DIFFERENT
    /// flash/uboot/serial sections, selecting the SECOND DUT must yield the
    /// second DUT's values (the first DUT's sections leak into the global
    /// map; to_config_map must override them).
    #[test]
    fn test_second_dut_flash_and_uboot_override_first_dut() {
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{
  "dev_hosts": [
    {
      "ip": "10.0.0.1",
      "duts": [
        {
          "dut_name": "dut-a",
          "serial": { "port": 41000, "baudrate": 460800, "rfc2217": false },
          "uboot": { "interrupt_char": "ctrl_c" },
          "flash": { "loader_bin": "/opt/loader-a.bin", "full_image_cmd": "uf-a {image}" },
          "download": { "list_devices_cmd": "ld-a" }
        },
        {
          "dut_name": "dut-b",
          "serial": { "port": 41008, "baudrate": 1500000, "rfc2217": true },
          "uboot": { "interrupt_char": "2" },
          "flash": { "loader_bin": "/opt/loader-b.bin", "full_image_cmd": "uf-b {image}" },
          "download": { "list_devices_cmd": "ld-b" }
        }
      ]
    }
  ]
}"#,
        );

        let duts = parse_dut_configs(&conf).unwrap();
        assert_eq!(duts.len(), 2);

        // Reproduce load_config()'s TARGET_DUT_NAME path: the global map
        // already carries the FIRST DUT's sections via first-DUT leakage.
        let mut global = defaults();
        global.extend(parse_jsonc_config(&conf).unwrap());
        assert_eq!(
            global.get("FLASH_FULL_IMAGE_CMD").map(String::as_str),
            Some("uf-a {image}"),
            "precondition: global map leaks the first DUT's flash command"
        );

        let selected = duts.iter().find(|d| d.dut_name == "dut-b").unwrap();
        let cfg = selected.to_config_map(&global);

        assert_eq!(
            cfg.get("FLASH_LOADER_BIN").map(String::as_str),
            Some("/opt/loader-b.bin")
        );
        assert_eq!(
            cfg.get("FLASH_FULL_IMAGE_CMD").map(String::as_str),
            Some("uf-b {image}")
        );
        assert_eq!(
            cfg.get("FLASH_LIST_DEVICES_CMD").map(String::as_str),
            Some("ld-b")
        );
        assert_eq!(
            cfg.get("UBOOT_INTERRUPT_CHAR").map(String::as_str),
            Some("2")
        );
        assert_eq!(
            cfg.get("SERIAL_BAUDRATE").map(String::as_str),
            Some("1500000")
        );
        assert_eq!(cfg.get("SERIAL_RFC2217").map(String::as_str), Some("true"));
        assert_eq!(cfg.get("SERIAL_PORT").map(String::as_str), Some("41008"));
    }

    /// End-to-end regression: TARGET_DUT_NAME selects the second DUT;
    /// load_config must return the second DUT's per-DUT sections.
    #[test]
    fn test_load_config_target_dut_name_selects_second_dut_sections() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{
  "dev_hosts": [
    {
      "ip": "10.0.0.1",
      "duts": [
        {
          "dut_name": "dut-a",
          "serial": { "port": 41000 },
          "target": { "login_prompt": "dut-a login:" },
          "monitor": { "hang_timeout": 60, "max_log_file_size": 100 },
          "relay": { "reset_time_ms": 3000 },
          "flash": { "loader_bin": "/opt/loader-a.bin" }
        },
        {
          "dut_name": "dut-b",
          "serial": { "port": 41008 },
          "target": { "login_prompt": "dut-b login:" },
          "uboot": { "interrupt_char": "2", "interrupt_strategy": "flood" },
          "monitor": { "hang_timeout": 90, "max_log_file_size": 42 },
          "relay": { "reset_time_ms": 5000 },
          "flash": { "loader_bin": "/opt/loader-b.bin", "full_image_cmd": "uf-b {image}" }
        }
      ]
    }
  ]
}"#,
        );

        unsafe {
            std::env::set_var("TARGET_CONF", conf.to_str().unwrap());
            std::env::set_var("TARGET_DUT_NAME", "dut-b");
        }
        let cfg = load_config();
        unsafe {
            std::env::remove_var("TARGET_CONF");
            std::env::remove_var("TARGET_DUT_NAME");
        }

        assert_eq!(cfg.get("FLASH_LOADER_BIN"), "/opt/loader-b.bin");
        assert_eq!(cfg.get("FLASH_FULL_IMAGE_CMD"), "uf-b {image}");
        assert_eq!(cfg.get("UBOOT_INTERRUPT_CHAR"), "2");
        assert_eq!(cfg.uboot_interrupt_char(), b'2');
        assert_ne!(cfg.uboot_interrupt_char(), 0x02);
        assert_eq!(cfg.get("UBOOT_INTERRUPT_STRATEGY"), "flood");
        assert_eq!(cfg.get("LOGIN_PROMPT"), "dut-b login:");
        assert_eq!(cfg.get("HANG_TIMEOUT"), "90");
        assert_eq!(cfg.get("MAX_LOG_FILE_SIZE"), "42");
        assert_eq!(cfg.get("RESET_TIME_MS"), "5000");
        assert_eq!(cfg.get("SERIAL_PORT"), "41008");
    }

    // ── TDD-3: golden parse + preserved fallback chains ────────────────

    /// A comment- and trailing-comma-heavy fixture mirroring the canonical
    /// example (references/.target.jsonc.example) — used as the golden parse.
    const CANONICAL_FIXTURE: &str = r#"// Test fixture mirroring the canonical example.
{
  "dev_hosts": [
    {
      "ip": "host-a.invalid",        // REQUIRED — ssh / ser2net host
      "user": "linaro",
      "pass": "",
      "duts": [
        {
          "dut_name": "rk3576",        // REQUIRED — unique across ALL DUTs
          "serial": {
            "port": 41002,
            "baudrate": 1500000,
            "rfc2217": false,
          },
          "target": { "login_user": "root", "login_pass": "", "login_prompt": "", },
          "uboot": { "interrupt_char": "ctrl_c", "interrupt_strategy": "flood", },
          "relay": {
            "type": "ch340",
            "port": 41000,
            "reset_ch": 1,
            "download_ch": 2,
            "power_ch": 0,
            "reset_time_ms": 500,
            "power_off_time_ms": 3000,
          },
          "monitor": {
            "hang_timeout": 60,
            "hang_hysteresis": 3,
            "max_archived_logs": 10,
            "max_log_file_size": 100,
            "reference_log": ".dut-serial/rk3576/reference-boot.log",
            "learner_stage_threshold": 0.45,
            "learner_crash_threshold": 0.50,
            "crash_keywords": ["Kernel panic", "BUG:", "Oops:"],
          },
          "flash": {
            "full_image_cmd": "uf {image}",
            "kernel_image_cmd": "di -k {image}",
          },
        },
        {
          "dut_name": "rk3576-2",
          "serial": { "port": "41003" },
          "target": { "login_user": "ubuntu" },
          "relay": { "type": "ch340-relay", "port": 2011, "reset_ch": 1 },
          "flash": { "full_image_cmd": "uf {image}" },
        },
      ],
    },
    {
      "ip": "host-b.invalid",
      "user": "lab",
      "duts": [
        {
          "dut_name": "rk3588",
          "serial": { "port": 41004 },
          "relay": { "type": "ch340-relay" },
          "control": { "dev_ctrl": "ch340-relay" },
        },
      ],
    },
  ],
  "lock_dir": "/tmp/sermcp/locks",
}"#;

    #[test]
    fn test_golden_parse_canonical_fixture() {
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(tmp.path(), CANONICAL_FIXTURE);
        let config = parse_config_file(&conf).unwrap();

        assert_eq!(config.dev_hosts.len(), 2);
        assert_eq!(config.duts.len(), 3);

        let rk3576 = &config.duts[0];
        assert_eq!(rk3576.dut_name, "rk3576");
        assert_eq!(rk3576.dev_host_ip, "host-a.invalid");
        assert_eq!(rk3576.dev_host_user, "linaro");
        assert_eq!(rk3576.serial_port, "41002");
        assert_eq!(rk3576.login_user, "root");
        assert_eq!(rk3576.reset_ch, 1);
        assert_eq!(rk3576.download_ch, 2);
        assert_eq!(rk3576.power_ch, 0, "global power_ch unset → 0");
        assert_eq!(rk3576.power_off_time_ms, 3000);
        assert_eq!(rk3576.dut_dir, ".dut-serial/rk3576");
        assert_eq!(
            rk3576.reference_log,
            ".dut-serial/rk3576/reference-boot.log"
        );
        assert_eq!(rk3576.crash_patterns, vec!["Kernel panic", "BUG:", "Oops:"]);
        assert_eq!(rk3576.learner_stage_threshold, 0.45);
        assert_eq!(rk3576.learner_crash_threshold, 0.50);
        assert_eq!(
            rk3576.section_values.get("FLASH_FULL_IMAGE_CMD").unwrap(),
            "uf {image}"
        );

        let rk3576_2 = &config.duts[1];
        assert_eq!(rk3576_2.serial_port, "41003", "string port form accepted");
        assert_eq!(
            rk3576_2.dev_host_ip, "host-a.invalid",
            "serial host = dev host"
        );
        assert_eq!(rk3576_2.login_user, "ubuntu");
        assert_eq!(rk3576_2.relay_port, 2011);
        assert_eq!(
            rk3576_2.reference_log, ".dut-serial/rk3576-2/reference-boot.log",
            "reference_log derived from dut_dir"
        );
        assert_eq!(rk3576_2.dut_dir, ".dut-serial/rk3576-2");

        let rk3588 = &config.duts[2];
        assert_eq!(rk3588.dev_host_ip, "host-b.invalid");
        assert_eq!(rk3588.relay_type, "ch340-relay");
        assert_eq!(rk3588.dev_ctl, "ch340-relay");
        assert_eq!(rk3588.serial_port, "41004");
        // login_user falls back to the global map — which carries the FIRST
        // DUT's login_user via first-DUT leakage (rk3576 sets "root", so
        // the fallback is "root").
        assert_eq!(rk3588.login_user, "root");
    }

    #[test]
    fn test_lock_dir_top_level_only() {
        // lock_dir is valid at the top level…
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{
  "dev_hosts": [ { "ip": "10.0.0.1" } ],
  "lock_dir": "/tmp/custom-locks"
}"#,
        );
        let cfg = parse_jsonc_config(&conf).unwrap();
        assert_eq!(cfg.get("LOCK_DIR").unwrap(), "/tmp/custom-locks");

        // …but rejected inside a DUT (deny_unknown_fields).
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{
  "dev_hosts": [
    { "ip": "10.0.0.1", "duts": [ { "dut_name": "d", "lock_dir": "/tmp/x" } ] }
  ]
}"#,
        );
        let err = parse_dut_configs(&conf).unwrap_err();
        assert!(err.contains("lock_dir"), "error must name the key: {err}");
    }

    #[test]
    fn test_back_compat_reference_log_and_dut_dir() {
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{
  "dev_hosts": [
    {
      "ip": "10.0.0.1",
      "duts": [
        {
          "dut_name": "dut-x",
          "reference_log": "/opt/custom-ref.log",
          "dut_dir": "/opt/dut-x-dir",
          "monitor": { "reference_log": ".dut-serial/ignored.log" }
        }
      ]
    }
  ],
  "reference_log": "/opt/global-ref.log",
  "dut_dir": "/opt/global-dir"
}"#,
        );
        let duts = parse_dut_configs(&conf).unwrap();
        let dut = &duts[0];
        // Typed per-DUT precedence: monitor.reference_log first, back-compat
        // reference_log only when monitor's is empty. In the FLAT map the
        // back-compat key wins (to_map inserts it after the monitor
        // spelling).
        assert_eq!(
            dut.reference_log, ".dut-serial/ignored.log",
            "monitor.reference_log wins on the typed per-DUT chain"
        );
        assert_eq!(
            dut.section_values.get("REFERENCE_LOG").unwrap(),
            "/opt/custom-ref.log",
            "back-compat reference_log wins in the flat map"
        );
        assert_eq!(dut.dut_dir, "/opt/dut-x-dir");
        // The global map first carries the top-level back-compat keys, then
        // first-DUT leakage overrides them with the first DUT's values.
        let global = parse_jsonc_config(&conf).unwrap();
        assert_eq!(
            global.get("REFERENCE_LOG").unwrap(),
            "/opt/custom-ref.log",
            "first-DUT leakage overrides the top-level back-compat key"
        );
        assert_eq!(global.get("DUT_DIR").unwrap(), "/opt/dut-x-dir");
    }

    #[test]
    fn test_reference_log_derivation_shadows_global() {
        // No reference_log anywhere → derived "{dut_dir}/reference-boot.log"
        // wins even when the global map HAS one (derivation is always
        // non-empty, shadowing global — preserved).
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{
  "dev_hosts": [
    { "ip": "10.0.0.1", "duts": [ { "dut_name": "solo" } ] }
  ],
  "reference_log": "/opt/global-ref.log"
}"#,
        );
        let duts = parse_dut_configs(&conf).unwrap();
        assert_eq!(duts[0].reference_log, ".dut-serial/solo/reference-boot.log");
    }

    #[test]
    fn test_first_dut_leakage_byte_equality() {
        // The global flat map is built as: first-host keys, global
        // sections, then first-DUT leakage (which overrides SERIAL_PORT),
        // plus every global key.
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{
  "dev_hosts": [
    {
      "ip": "10.0.0.99",
      "user": "linaro",
      "duts": [
        {
          "dut_name": "dut-a",
          "serial": { "port": 41000 },
          "flash": { "full_image_cmd": "uf-a {image}" }
        }
      ]
    }
  ],
  "serial": { "port": 9999 },
  "relay": { "reset_channel": 2, "power_ch": 3, "power_off_time_ms": 5000 },
  // OLD `crash_patterns` spelling kept deliberately — alias regression coverage.
  "monitor": { "learner_stage_threshold": 0.45, "crash_patterns": ["Kernel panic", "BUG:"] },
  "lock_dir": "/tmp/sermcp/locks"
}"#,
        );
        let mut expected = HashMap::new();
        expected.insert("DEV_HOST_IP".into(), "10.0.0.99".into());
        expected.insert("DEV_HOST_USER".into(), "linaro".into());
        // global serial.port 9999 is overridden by the first-DUT leakage
        expected.insert("SERIAL_PORT".into(), "41000".into());
        // zero-filtered ch/channel pair on the GLOBAL path (reconciled)
        expected.insert("RESET_CHANNEL".into(), "2".into());
        expected.insert("POWER_CHANNEL".into(), "3".into());
        expected.insert("POWER_OFF_TIME_MS".into(), "5000".into());
        expected.insert("LEARNER_STAGE_THRESHOLD".into(), "0.45".into());
        expected.insert("CRASH_PATTERNS".into(), "Kernel panic\nBUG:".into());
        expected.insert("LOCK_DIR".into(), "/tmp/sermcp/locks".into());
        expected.insert("DUT_NAME".into(), "dut-a".into());
        expected.insert("DUT_DIR".into(), ".dut-serial/dut-a".into());
        expected.insert("FLASH_FULL_IMAGE_CMD".into(), "uf-a {image}".into());

        assert_eq!(parse_jsonc_config(&conf).unwrap(), expected);
    }

    #[test]
    fn test_zero_guarded_insertion_inherits_global() {
        // Explicit 0 / "" / 0.0 for relay port/channels/thresholds must
        // inherit the global value instead of shadowing it.
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{
  "dev_hosts": [
    {
      "ip": "10.0.0.1",
      "duts": [
        {
          "dut_name": "d",
          "relay": {
            "port": 0,
            "reset_ch": 0,
            "power_ch": 0,
            "power_off_time_ms": 0,
            "reset_time_ms": 0
          },
          "monitor": { "learner_stage_threshold": 0.0 }
        }
      ]
    }
  ],
  "relay": { "port": 41001, "reset_ch": 5, "power_ch": 6, "power_off_time_ms": 7000, "reset_time_ms": 400 },
  "monitor": { "learner_stage_threshold": 0.45 }
}"#,
        );
        let duts = parse_dut_configs(&conf).unwrap();
        let dut = &duts[0];
        assert_eq!(dut.relay_port, 41001, "explicit 0 inherits global");
        assert_eq!(dut.reset_ch, 5, "explicit 0 inherits global");
        assert_eq!(dut.power_ch, 6, "explicit 0 inherits global");
        assert_eq!(dut.power_off_time_ms, 0, "typed 0 → flat-map inheritance");
        assert_eq!(dut.learner_stage_threshold, 0.0);

        // Reproduce load_config()'s layering: defaults + file-global map.
        let mut base = defaults();
        base.extend(parse_jsonc_config(&conf).unwrap());
        let cfg_map = dut.to_config_map(&base);
        // The typed fields inherited the global values — they flow through
        // the flat map, never as zeros…
        assert_eq!(cfg_map.get("RESET_CHANNEL").unwrap(), "5");
        assert_eq!(cfg_map.get("POWER_CHANNEL").unwrap(), "6");
        assert_eq!(cfg_map.get("RELAY_PORT").unwrap(), "41001");
        assert_eq!(cfg_map.get("POWER_OFF_TIME_MS").unwrap(), "7000");
        assert_eq!(cfg_map.get("RESET_TIME_MS").unwrap(), "400");
        // …and section_values must NOT carry the zeros — they would shadow
        // the global values in the flat map
        assert_eq!(dut.section_values.get("RELAY_PORT"), None);
        assert_eq!(dut.section_values.get("RESET_CHANNEL"), None);
        assert_eq!(dut.section_values.get("POWER_CHANNEL"), None);
        assert_eq!(dut.section_values.get("POWER_OFF_TIME_MS"), None);
        assert_eq!(dut.section_values.get("RESET_TIME_MS"), None);
        assert_eq!(dut.section_values.get("LEARNER_STAGE_THRESHOLD"), None);
    }

    #[test]
    fn test_power_off_time_ms_clamped() {
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{
  "dev_hosts": [
    {
      "ip": "10.0.0.1",
      "duts": [ { "dut_name": "d", "relay": { "power_off_time_ms": 1000 } } ]
    }
  ]
}"#,
        );
        let duts = parse_dut_configs(&conf).unwrap();
        assert_eq!(duts[0].power_off_time_ms, 3000, "clamped to >= 3000");
    }

    #[test]
    fn test_crash_keywords_empty_array_falls_back_to_builtin() {
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{
  "dev_hosts": [
    { "ip": "10.0.0.1", "duts": [ { "dut_name": "d" } ] }
  ],
  "monitor": { "crash_keywords": [] }
}"#,
        );
        let global = parse_jsonc_config(&conf).unwrap();
        assert_eq!(global.get("CRASH_PATTERNS").unwrap(), "");
        let mut values = defaults();
        values.extend(global);
        let cfg = Config {
            values,
            config_path: None,
            project_dir: None,
            format: ConfigFormat::None,
        };
        // Empty array → "" → LEARN_CRASH_PATTERNS → builtin list (fallback
        // chain preserved).
        assert!(cfg.crash_patterns().contains(&"Kernel panic".to_string()));
    }

    #[test]
    fn test_monitor_crash_keywords_new_key_parses() {
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{
  "dev_hosts": [
    { "ip": "10.0.0.1", "duts": [ { "dut_name": "d" } ] }
  ],
  "monitor": { "crash_keywords": ["Kernel panic", "BUG:"] }
}"#,
        );
        let global = parse_jsonc_config(&conf).unwrap();
        assert_eq!(global.get("CRASH_PATTERNS").unwrap(), "Kernel panic\nBUG:");
    }

    #[test]
    fn test_monitor_crash_patterns_old_key_still_parses() {
        // Back-compat contract: the old `crash_patterns` spelling keeps
        // parsing via the serde alias after the rename to `crash_keywords`.
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{
  "dev_hosts": [
    { "ip": "10.0.0.1", "duts": [ { "dut_name": "d" } ] }
  ],
  "monitor": { "crash_patterns": ["Kernel panic", "BUG:"] }
}"#,
        );
        let global = parse_jsonc_config(&conf).unwrap();
        assert_eq!(global.get("CRASH_PATTERNS").unwrap(), "Kernel panic\nBUG:");
    }

    #[test]
    fn test_monitor_both_spellings_is_duplicate_error() {
        // Both spellings present → loud duplicate-field parse error
        // (consistent with the relay ch/channel alias policy), never silent
        // preference.
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{
  "monitor": { "crash_patterns": ["A"], "crash_keywords": ["B"] }
}"#,
        );
        let err = parse_jsonc_config(&conf).unwrap_err();
        assert!(
            err.contains("duplicate"),
            "expected duplicate-field error, got: {err}"
        );
        assert!(err.contains("crash"), "error must name the key: {err}");
    }

    #[test]
    fn test_hang_hysteresis_zero_clamps_to_one() {
        let values = HashMap::from([("HANG_HYSTERESIS".into(), "0".into())]);
        let cfg = Config {
            values,
            config_path: None,
            project_dir: None,
            format: ConfigFormat::None,
        };
        // 0 would mean "DUT-off on the first unanswered round" — clamp to 1.
        assert_eq!(cfg.hang_hysteresis(), 1);
    }

    #[test]
    fn test_target_dut_name_forced_dut_dir_overrides_explicit() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{
  "dev_hosts": [
    {
      "ip": "10.0.0.1",
      "duts": [
        { "dut_name": "dut-a", "dut_dir": "/explicit/a", "serial": { "port": 41000 } }
      ]
    }
  ]
}"#,
        );
        unsafe {
            std::env::set_var("TARGET_CONF", conf.to_str().unwrap());
            std::env::set_var("TARGET_DUT_NAME", "dut-a");
        }
        let cfg = load_config();
        unsafe {
            std::env::remove_var("TARGET_CONF");
            std::env::remove_var("TARGET_DUT_NAME");
        }
        // Load-bearing pid-singleton rationale: per-DUT servers write pid/
        // state into the name dir, never the project root.
        assert_eq!(cfg.get("DUT_DIR"), ".dut-serial/dut-a");
    }

    // ── TDD-4: strictness — typos fail loudly ──────────────────────────

    #[test]
    fn test_strict_parse_rejects_single_quoted_strings() {
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(tmp.path(), r#"{ 'dev_hosts': [ { 'ip': '10.0.0.1' } ] }"#);
        let err = parse_jsonc_config(&conf).unwrap_err();
        assert!(err.contains("JSONC parse error"), "{err}");
    }

    #[test]
    fn test_strict_parse_rejects_hex_numbers() {
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{ "dev_hosts": [ { "ip": "10.0.0.1" } ], "serial": { "port": 0xFF } }"#,
        );
        assert!(parse_jsonc_config(&conf).is_err());
    }

    #[test]
    fn test_strict_parse_rejects_loose_property_names() {
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(tmp.path(), r#"{ dev_hosts: [] }"#);
        assert!(parse_jsonc_config(&conf).is_err());
    }

    #[test]
    fn test_strict_parse_rejects_missing_commas() {
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{ "dev_hosts": [ { "ip": "10.0.0.1" } ] "serial": { "port": 41000 } }"#,
        );
        assert!(parse_jsonc_config(&conf).is_err());
    }

    #[test]
    fn test_unknown_key_names_the_key() {
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{ "dev_hosts": [ { "ip": "10.0.0.1" } ], "serialx": { "port": 41000 } }"#,
        );
        let err = parse_jsonc_config(&conf).unwrap_err();
        assert!(err.contains("serialx"), "error must name the key: {err}");
    }

    /// `recovery_ch` was removed (ONE download-family key: `download_ch` is
    /// the form's `download` row and the only download channel the engine
    /// presses). The unknown-key gate must reject it LOUDLY rather than
    /// silently ignore a relay channel someone still relies on.
    #[test]
    fn test_removed_recovery_ch_key_is_rejected() {
        let err = parse_jsonc_text(
            r#"{ "dev_hosts": [ { "ip": "10.0.0.1", "duts": [ { "dut_name": "d",
                 "relay": { "port": 2000, "reset_ch": 1, "recovery_ch": 3 } } ] } ] }"#,
        )
        .unwrap_err();
        assert!(
            err.contains("recovery_ch"),
            "the error must name the removed key: {err}"
        );
    }

    #[test]
    fn test_unknown_key_inside_dut_names_the_key() {
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{
  "dev_hosts": [
    { "ip": "10.0.0.1", "duts": [ { "dut_name": "d", "serial": { "potr": 41000 } } ] }
  ]
}"#,
        );
        let err = parse_dut_configs(&conf).unwrap_err();
        assert!(err.contains("potr"), "error must name the key: {err}");
    }

    #[test]
    fn test_host_alias_key_is_rejected_loudly() {
        // dev_hosts[].alias is the renamed TOML-era key — deny_unknown_fields
        // must turn it into a loud parse error that names the key AND the
        // new spelling, and says what to do about it.
        let err = parse_jsonc_text(r#"{ "dev_hosts": [ { "alias": "old", "ip": "10.0.0.1" } ] }"#)
            .unwrap_err();
        assert!(err.contains("alias"), "error must name the key: {err}");
        assert!(
            err.contains("unknown field"),
            "error must be a deny_unknown_fields rejection: {err}"
        );
        assert!(
            err.contains("renamed to host_name"),
            "error must name the NEW key: {err}"
        );
        assert!(
            err.contains("re-run 'dutabo init'"),
            "error must say what to do: {err}"
        );
    }

    #[test]
    fn test_stray_alias_in_dut_section_gets_dut_name_hint() {
        // A DUT-level stray 'alias' is also a deny_unknown_fields rejection,
        // but the hint must name the DUT-level new key (dut_name) — never
        // the host-level one.
        let err = parse_jsonc_text(
            r#"{ "dev_hosts": [ { "ip": "10.0.0.1", "duts": [ { "dut_name": "d", "alias": "x" } ] } ] }"#,
        )
        .unwrap_err();
        assert!(err.contains("unknown field"), "must still reject: {err}");
        assert!(
            err.contains("renamed to dut_name"),
            "the DUT-level stray must name the NEW key: {err}"
        );
        assert!(
            !err.contains("host_name"),
            "host-level advice must not leak into a DUT-level stray: {err}"
        );
        // Position-blindness guard: an 'alias' inside a DUT sub-section
        // gets NEITHER hint (neither struct's expected-list contains it).
        let err = parse_jsonc_text(
            r#"{ "dev_hosts": [ { "ip": "10.0.0.1", "duts": [ { "dut_name": "d", "target": { "alias": "x" } } ] } ] }"#,
        )
        .unwrap_err();
        assert!(
            err.contains("unknown field alias"),
            "must still reject: {err}"
        );
        assert!(
            !err.contains("renamed to"),
            "no section-level hint for a sub-section stray: {err}"
        );
    }

    #[test]
    fn test_missing_required_dut_name_is_a_parse_error() {
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{ "dev_hosts": [ { "ip": "10.0.0.1", "duts": [ { "serial": { "port": 41000 } } ] } ] }"#,
        );
        let err = parse_dut_configs(&conf).unwrap_err();
        assert!(err.contains("dut_name"), "error must name the key: {err}");
    }

    #[test]
    fn test_duplicate_ch_alias_fields_rejected() {
        // Both reset_ch AND reset_channel set → serde duplicate-field error.
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{
  "dev_hosts": [
    { "ip": "10.0.0.1", "duts": [ { "dut_name": "d", "relay": { "reset_ch": 1, "reset_channel": 2 } } ] }
  ]
}"#,
        );
        assert!(parse_dut_configs(&conf).is_err());
    }

    // ── TDD-5: number-or-string ────────────────────────────────────────

    #[test]
    fn test_port_number_or_string() {
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{
  "dev_hosts": [
    {
      "ip": "10.0.0.1",
      "duts": [
        { "dut_name": "a", "serial": { "port": 41002 } },
        { "dut_name": "b", "serial": { "port": "41002" } },
        { "dut_name": "c", "serial": { "port": 0 } },
        { "dut_name": "d", "serial": { "port": "0" } },
        { "dut_name": "e", "serial": { "port": 0.0 } },
        { "dut_name": "f", "serial": { "port": "/dev/ttyUSB0" } }
      ]
    }
  ]
}"#,
        );
        let duts = parse_dut_configs(&conf).unwrap();
        assert_eq!(duts[0].serial_port, "41002");
        assert_eq!(duts[1].serial_port, "41002", "string form accepted");
        assert_eq!(duts[2].serial_port, "0", "validation sentinel");
        assert_eq!(duts[3].serial_port, "0", "validation sentinel");
        assert_eq!(duts[4].serial_port, "0", "0.0 renders as the sentinel");
        assert_eq!(duts[5].serial_port, "/dev/ttyUSB0", "device path accepted");
    }

    #[test]
    fn test_baudrate_number_or_string() {
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{
  "dev_hosts": [
    {
      "ip": "10.0.0.1",
      "duts": [
        { "dut_name": "a", "serial": { "port": 41000, "baudrate": 1500000 } },
        { "dut_name": "b", "serial": { "port": 41000, "baudrate": "1500000" } }
      ]
    }
  ]
}"#,
        );
        let duts = parse_dut_configs(&conf).unwrap();
        assert_eq!(
            duts[0].section_values.get("SERIAL_BAUDRATE").unwrap(),
            "1500000"
        );
        assert_eq!(
            duts[1].section_values.get("SERIAL_BAUDRATE").unwrap(),
            "1500000"
        );
    }

    #[test]
    fn interrupt_char_accepts_common_ctrl_c_spellings() {
        for value in ["ctrl_c", "ctrl-c", "ctrl+C", "^C", "0x03"] {
            assert_eq!(
                parse_interrupt_char_checked(value).unwrap(),
                0x03,
                "{value}"
            );
        }
        assert_eq!(parse_interrupt_char_checked("3").unwrap(), b'3');
        assert_eq!(parse_interrupt_char_checked("x").unwrap(), b'x');
        assert_eq!(parse_interrupt_char_checked("0x1b").unwrap(), 0x1b);
        assert_eq!(parse_interrupt_char_checked("F12").unwrap(), 0x1b);
        assert!(parse_interrupt_char_checked("ctrl_shift_c").is_err());
        assert!(parse_interrupt_char_checked("0x100").is_err());
        assert!(parse_interrupt_char_checked("中").is_err());
    }

    // ── TDD-6: ch/channel reconciliation on BOTH paths ─────────────────

    #[test]
    fn test_ch_channel_alias_works_on_dut_path() {
        // `reset_channel` populates reset_ch via the serde alias.
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{
  "dev_hosts": [
    {
      "ip": "10.0.0.1",
      "duts": [
        { "dut_name": "d", "relay": { "reset_channel": 2 } }
      ]
    }
  ]
}"#,
        );
        let duts = parse_dut_configs(&conf).unwrap();
        assert_eq!(duts[0].reset_ch, 2);
    }

    #[test]
    fn test_ch_zero_inherits_global_on_dut_path() {
        // Explicit 0 inherits the global channel instead of shadowing it
        // (zero-guarded on the per-DUT typed path).
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{
  "dev_hosts": [
    { "ip": "10.0.0.1", "duts": [ { "dut_name": "d", "relay": { "reset_ch": 0 } } ] }
  ],
  "relay": { "reset_channel": 2 }
}"#,
        );
        let duts = parse_dut_configs(&conf).unwrap();
        assert_eq!(
            duts[0].reset_ch, 2,
            "explicit 0 must not shadow the global 2"
        );
    }

    #[test]
    fn test_ch_channel_alias_works_on_global_path() {
        // One field plus the zero-filtered rule means the alias spelling
        // always resolves correctly.
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{ "dev_hosts": [ { "ip": "10.0.0.1" } ], "relay": { "reset_channel": 2 } }"#,
        );
        let cfg = parse_jsonc_config(&conf).unwrap();
        assert_eq!(cfg.get("RESET_CHANNEL").unwrap(), "2");
    }

    // ── TDD-7: validate() ──────────────────────────────────────────────

    #[test]
    fn test_interrupt_strategy_rejects_removed_values_and_requires_pattern() {
        for removed in ["timed", "aggressive", "lava"] {
            let mut values = defaults();
            values.insert("UBOOT_INTERRUPT_STRATEGY".into(), removed.into());
            let mut errors = Vec::new();
            validate_uboot_interrupt_settings(&values, "uboot", &mut errors);
            assert_eq!(errors.len(), 1, "{removed}: {errors:?}");
            assert!(errors[0].contains("valid values: flood, pattern"));
        }

        let mut values = defaults();
        values.insert("UBOOT_INTERRUPT_STRATEGY".into(), "pattern".into());
        let mut errors = Vec::new();
        validate_uboot_interrupt_settings(&values, "uboot", &mut errors);
        assert!(errors.iter().any(|error| error.contains("required")));

        values.insert("UBOOT_PATTERN".into(), "[".into());
        errors.clear();
        validate_uboot_interrupt_settings(&values, "uboot", &mut errors);
        assert!(errors.iter().any(|error| error.contains("invalid regex")));

        values.insert("UBOOT_PATTERN".into(), "Hit any key".into());
        errors.clear();
        validate_uboot_interrupt_settings(&values, "uboot", &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn test_validate_error_names_jsonc() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(tmp.path(), r#"{ "dev_hosts": [ { "ip": "" } ] }"#);
        unsafe { std::env::set_var("TARGET_CONF", conf.to_str().unwrap()) };
        let cfg = load_config();
        unsafe { std::env::remove_var("TARGET_CONF") };
        let errors = cfg.validate().unwrap_err();
        assert!(
            errors.iter().any(|e| e.contains(".target.jsonc")),
            "{errors:?}"
        );
    }

    #[test]
    fn test_validate_port_zero_suppressed_when_host_missing() {
        // No host ip + port "0": only the host error (no double-error).
        let _guard = ENV_MUTEX.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(tmp.path(), r#"{ "serial": { "port": 0 } }"#);
        unsafe { std::env::set_var("TARGET_CONF", conf.to_str().unwrap()) };
        let cfg = load_config();
        unsafe { std::env::remove_var("TARGET_CONF") };
        let errors = cfg.validate().unwrap_err();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("dev_host.ip"), "{errors:?}");
    }

    #[test]
    fn test_validate_port_zero_errors_with_host() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{
  "dev_hosts": [ { "ip": "10.0.0.1", "duts": [ { "dut_name": "d", "serial": { "port": 0 } } ] } ]
}"#,
        );
        unsafe { std::env::set_var("TARGET_CONF", conf.to_str().unwrap()) };
        let cfg = load_config();
        unsafe { std::env::remove_var("TARGET_CONF") };
        let errors = cfg.validate().unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| e.contains("serial.port") && e.contains(".target.jsonc")),
            "{errors:?}"
        );
    }

    #[test]
    fn test_serial_repeated_byte_transport_config_and_validation() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{
  "dev_hosts": [ { "ip": "10.0.0.1", "duts": [ {
    "dut_name": "d",
    "serial": { "port": 2002, "baudrate": 115200, "tx_repeat": 2, "tx_gap_ms": 30 }
  } ] } ]
}"#,
        );
        unsafe { std::env::set_var("TARGET_CONF", conf.to_str().unwrap()) };
        let mut cfg = load_config();
        unsafe { std::env::remove_var("TARGET_CONF") };
        assert_eq!(cfg.serial_tx_repeat(), 2);
        assert_eq!(cfg.serial_tx_gap(), std::time::Duration::from_millis(30));
        assert!(cfg.validate().is_ok());

        cfg.values.insert("SERIAL_TX_REPEAT".into(), "0".into());
        let errors = cfg.validate().unwrap_err();
        assert!(errors.iter().any(|error| error.contains("tx_repeat")));
    }

    #[test]
    fn test_validate_rejects_invalid_interrupt_char_before_hardware_use() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{
  "dev_hosts": [
    {
      "ip": "10.0.0.1",
      "duts": [
        {
          "dut_name": "d",
          "serial": { "port": 41000 },
          "uboot": { "interrupt_char": "ctrl_shift_c" }
        }
      ]
    }
  ]
}"#,
        );
        unsafe { std::env::set_var("TARGET_CONF", conf.to_str().unwrap()) };
        let cfg = load_config();
        unsafe { std::env::remove_var("TARGET_CONF") };
        let errors = cfg.validate().unwrap_err();
        assert!(
            errors
                .iter()
                .any(|error| error.contains("uboot.interrupt_char")),
            "{errors:?}"
        );
    }

    #[test]
    fn test_validate_relay_type_error_lists_supported_dynamically() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{
  "dev_hosts": [
    {
      "ip": "10.0.0.1",
      "duts": [
        { "dut_name": "d", "serial": { "port": 41000 }, "relay": { "type": "quantum-relay" } }
      ]
    }
  ]
}"#,
        );
        unsafe { std::env::set_var("TARGET_CONF", conf.to_str().unwrap()) };
        let cfg = load_config();
        unsafe { std::env::remove_var("TARGET_CONF") };
        let errors = cfg.validate().unwrap_err();
        let relay_error = errors
            .iter()
            .find(|e| e.contains("unsupported relay.type"))
            .expect("relay error expected");
        assert!(relay_error.contains("quantum-relay"), "{relay_error}");
        // The accepted set is whatever the compiled plugin registry
        // supports — external was removed and must not be
        // listed as valid.
        assert!(
            !relay_error.contains("external"),
            "external backend was removed: {relay_error}"
        );
        assert!(
            !relay_error.contains("quantum-relay is not"),
            "{relay_error}"
        );
        assert!(
            !relay_error.contains("ch340") || crate::power_control::supports("ch340"),
            "text must match the compiled backend"
        );
    }

    /// Removed backends are rejected loudly: relay.type 'external' (and the
    /// usb-relay alias) no longer resolve to anything —
    /// validation must say so instead of silently mapping them.
    #[test]
    fn test_validate_removed_backends_are_rejected() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{
  "dev_hosts": [
    {
      "ip": "10.0.0.1",
      "duts": [
        { "dut_name": "d", "serial": { "port": 41000 }, "relay": { "type": "external" } },
        { "dut_name": "e", "serial": { "port": 41001 }, "relay": { "type": "usb-relay" } },
        { "dut_name": "f", "serial": { "port": 41002 }, "control": { "dev_ctrl": "my-prog" } }
      ]
    }
  ]
}"#,
        );
        unsafe { std::env::set_var("TARGET_CONF", conf.to_str().unwrap()) };
        let cfg = load_config();
        unsafe { std::env::remove_var("TARGET_CONF") };
        let errors = cfg.validate().unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| e.contains("unsupported relay.type 'external'")),
            "{errors:?}"
        );
        assert!(
            errors
                .iter()
                .any(|e| e.contains("unsupported relay.type 'usb-relay'")),
            "{errors:?}"
        );
        assert!(
            errors
                .iter()
                .any(|e| e.contains("unsupported control.dev_ctrl 'my-prog'")),
            "{errors:?}"
        );
    }

    #[test]
    fn test_validate_accumulates_errors() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{
  "dev_hosts": [
    {
      "ip": "10.0.0.1",
      "duts": [
        { "dut_name": "d", "serial": { "port": 0 }, "relay": { "type": "bogus" } }
      ]
    }
  ]
}"#,
        );
        unsafe { std::env::set_var("TARGET_CONF", conf.to_str().unwrap()) };
        let cfg = load_config();
        unsafe { std::env::remove_var("TARGET_CONF") };
        let errors = cfg.validate().unwrap_err();
        assert!(
            errors.len() >= 2,
            "port + relay errors accumulate: {errors:?}"
        );
    }

    #[test]
    fn test_validate_duplicate_dut_name_across_hosts() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{
  "dev_hosts": [
    { "ip": "10.0.0.1", "duts": [ { "dut_name": "same", "serial": { "port": 41000 } } ] },
    { "ip": "10.0.0.2", "duts": [ { "dut_name": "same", "serial": { "port": 41001 } } ] }
  ]
}"#,
        );
        unsafe { std::env::set_var("TARGET_CONF", conf.to_str().unwrap()) };
        let cfg = load_config();
        unsafe { std::env::remove_var("TARGET_CONF") };
        let errors = cfg.validate().unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| e.contains("Duplicate DUT name 'same'")),
            "{errors:?}"
        );
    }

    // ── TDD-8: discovery ───────────────────────────────────────────────

    #[test]
    fn test_find_config_walk_up_finds_jsonc() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        write_jsonc(tmp.path(), "{}");
        let sub = tmp.path().join("a").join("b");
        std::fs::create_dir_all(&sub).unwrap();
        let _cwd = chdir_tmp(&sub);
        let found = find_config_path().unwrap().unwrap();
        assert_eq!(
            found,
            tmp.path().join(".target.jsonc").canonicalize().unwrap()
        );
    }

    #[test]
    fn test_find_config_target_conf_honored() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(tmp.path(), "{}");
        let other = TempDir::new().unwrap();
        let _cwd = chdir_tmp(other.path());
        unsafe { std::env::set_var("TARGET_CONF", conf.to_str().unwrap()) };
        let found = find_config_path().unwrap().unwrap();
        unsafe { std::env::remove_var("TARGET_CONF") };
        assert_eq!(found, conf);
    }

    #[test]
    fn test_find_config_target_conf_missing_is_loud() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let _cwd = chdir_tmp(tmp.path());
        unsafe {
            std::env::set_var("TARGET_CONF", "/nonexistent/definitely/.target.jsonc");
        }
        let err = find_config_path().unwrap_err();
        unsafe { std::env::remove_var("TARGET_CONF") };
        assert!(
            err.contains("does not exist") && err.contains("TARGET_CONF"),
            "{err}"
        );
    }

    #[test]
    fn test_find_config_target_conf_any_extension_is_honored() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let conf = tmp.path().join("custom.toml");
        std::fs::write(&conf, "[serial]\nport = 1\n").unwrap();
        let _cwd = chdir_tmp(tmp.path());
        unsafe { std::env::set_var("TARGET_CONF", conf.to_str().unwrap()) };
        // No extension check: the file is honored as given; parse errors
        // surface later.
        let found = find_config_path().unwrap();
        unsafe { std::env::remove_var("TARGET_CONF") };
        assert_eq!(found.as_deref(), Some(conf.as_path()));
    }

    #[test]
    fn test_find_config_stray_file_only_dir_plain_none() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let stray = tmp.path().join("stray.toml");
        std::fs::write(&stray, "[serial]\nport = 1\n").unwrap();
        let sub = tmp.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let _cwd = chdir_tmp(&sub);
        // Other file names in the tree are never consulted: only
        // .target.jsonc is discovered, so this is the plain missing-config
        // path.
        assert_eq!(find_config_path().unwrap(), None);
    }

    #[test]
    fn test_find_config_neither_plain_none() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let _cwd = chdir_tmp(tmp.path());
        assert_eq!(find_config_path().unwrap(), None);
    }

    #[test]
    fn test_find_config_jsonc_wins_over_sibling_stray() {
        // .target.jsonc next to an unrelated sibling file → .target.jsonc
        // wins; nothing else is ever consulted.
        let _guard = ENV_MUTEX.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("stray.toml"), "[serial]\nport = 1\n").unwrap();
        let jsonc = write_jsonc(tmp.path(), r#"{ "serial": { "port": 41000 } }"#);
        let _cwd = chdir_tmp(tmp.path());
        assert_eq!(find_config_path().unwrap().unwrap(), jsonc);
    }

    #[test]
    fn test_load_config_no_config_returns_defaults() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let _cwd = chdir_tmp(tmp.path());
        let cfg = load_config();
        assert!(cfg.config_path.is_none());
        assert_eq!(cfg.format, ConfigFormat::None);
        assert_eq!(cfg.dev_host_ip(), "");
    }

    // ── Edge cases: empty hosts / no DUTs / host containers ────────────

    #[test]
    fn test_empty_dev_hosts_yields_default_dut() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{ "dev_hosts": [], "serial": { "port": 41000 } }"#,
        );
        let duts = parse_dut_configs(&conf).unwrap();
        assert_eq!(duts.len(), 1, "never an empty Vec");
        assert_eq!(duts[0].dut_name, "default");
        assert_eq!(duts[0].serial_port, "41000");
    }

    #[test]
    fn test_default_dut_uses_target_dut_name() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{ "dev_hosts": [], "serial": { "port": 41000 } }"#,
        );
        unsafe { std::env::set_var("TARGET_DUT_NAME", "solo-board") };
        let duts = parse_dut_configs(&conf).unwrap();
        unsafe { std::env::remove_var("TARGET_DUT_NAME") };
        assert_eq!(duts[0].dut_name, "solo-board");
    }

    #[test]
    fn test_hosts_without_duts_attaches_default_to_first_host() {
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{
  "dev_hosts": [
    { "ip": "10.0.0.1", "user": "u1" },
    { "ip": "10.0.0.2", "duts": [] }
  ]
}"#,
        );
        let config = parse_config_file(&conf).unwrap();
        assert_eq!(config.dev_hosts.len(), 2, "host registry keeps empty hosts");
        assert_eq!(config.duts.len(), 1, "one default DUT on the first host");
        assert_eq!(config.duts[0].dev_host_ip, "10.0.0.1");
        assert_eq!(config.duts[0].dev_host_user, "u1");
    }

    #[test]
    fn test_host_with_no_duts_contributes_no_dut() {
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{
  "dev_hosts": [
    {
      "ip": "10.0.0.1",
      "duts": [ { "dut_name": "only-dut", "serial": { "port": 41000 } } ]
    },
    { "ip": "10.0.0.2" }
  ]
}"#,
        );
        let config = parse_config_file(&conf).unwrap();
        assert_eq!(config.duts.len(), 1);
        assert_eq!(config.duts[0].dut_name, "only-dut");
        assert_eq!(config.dev_hosts.len(), 2);
    }

    #[test]
    fn test_empty_duts_array_yields_default_dut() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{ "dev_hosts": [ { "ip": "10.0.0.1", "duts": [] } ] }"#,
        );
        let duts = parse_dut_configs(&conf).unwrap();
        assert_eq!(duts.len(), 1);
        assert_eq!(duts[0].dut_name, "default");
        assert_eq!(duts[0].dev_host_ip, "10.0.0.1");
    }

    #[test]
    fn test_parse_dut_configs_non_jsonc_is_plain_parse_error() {
        // A non-JSONC file goes through the plain JSONC parse path: the
        // error names the parse failure, with no format guidance.
        let tmp = TempDir::new().unwrap();
        let stray = tmp.path().join("stray.toml");
        std::fs::write(&stray, "[serial]\nport = 41000\n").unwrap();
        let err = parse_dut_configs(&stray).unwrap_err();
        assert!(!err.contains("no longer supported"), "{err}");
        assert!(!err.contains("dutabo init"), "{err}");
        assert!(err.contains("JSONC parse error"), "{err}");
    }

    #[test]
    fn test_first_dut_leakage_sets_dut_name_and_dir() {
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(tmp.path(), SIMPLE_FIXTURE);
        let global = parse_jsonc_config(&conf).unwrap();
        assert_eq!(global.get("DUT_NAME").unwrap(), "rk3576");
        assert_eq!(global.get("DUT_DIR").unwrap(), ".dut-serial/rk3576");
        assert_eq!(global.get("SERIAL_PORT").unwrap(), "41002");
    }

    // ── Inventory helper ───────────────────────────────────────────────

    fn canned_state(state: &str, critical: bool) -> InventoryDutState {
        InventoryDutState {
            state: state.to_string(),
            state_label: state.to_string(),
            state_text: format!("ANSI[{state}]"),
            state_plain: state.to_string(),
            age_secs: Some(42),
            critical,
        }
    }

    #[test]
    fn test_build_inventory_json_golden() {
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{
  "dev_hosts": [
    {
      "ip": "10.0.0.1",
      "user": "linaro",
      "duts": [
        { "dut_name": "dut-a", "serial": { "port": 41000 } },
        { "dut_name": "dut-b", "serial": { "port": 41001 }, "dut_dir": "/opt/dut-b" }
      ]
    }
  ]
}"#,
        );
        let parsed = parse_config_file(&conf).unwrap();
        let cfg = Config {
            values: defaults(),
            config_path: Some(conf.clone()),
            project_dir: Some(tmp.path().to_path_buf()),
            format: ConfigFormat::Jsonc,
        };

        let inv = build_inventory_json(
            &cfg,
            &parsed.dev_hosts,
            &parsed.duts,
            Some("dut-b"),
            InventoryMeta {
                written_by: "mcp-server",
                written_at_unix: 1_700_000_000,
                heartbeat_secs: None,
                mcp_pid: Some(1234),
                mcp_http_port: Some(3001),
                owner: None,
            },
            |dut| {
                canned_state(
                    if dut.dut_name == "dut-a" {
                        "active"
                    } else {
                        "crashed"
                    },
                    true,
                )
            },
        );

        assert_eq!(inv.schema_version, 1);
        assert_eq!(inv.written_by, "mcp-server");
        assert_eq!(inv.written_at_unix, 1_700_000_000);
        assert_eq!(inv.dev_host_count, 1);
        assert_eq!(inv.dut_count, 2);
        assert_eq!(inv.current_dut.as_deref(), Some("dut-b"));
        assert_eq!(inv.mcp_pid, Some(1234));
        assert_eq!(inv.mcp_http_port, Some(3001));
        assert_eq!(inv.dev_hosts.len(), 1);
        assert_eq!(inv.dev_hosts[0].ip, "10.0.0.1");
        assert_eq!(inv.dev_hosts[0].user, "linaro");
        // host_name falls back to the host ip (the fixture host has no
        // explicit host_name key).
        assert_eq!(inv.dev_hosts[0].host_name, "10.0.0.1");

        assert_eq!(inv.duts[0].dut_name, "dut-a");
        assert_eq!(inv.duts[0].ssh_user, "linaro");
        assert_eq!(inv.duts[0].serial_port, "41000");
        assert_eq!(inv.duts[0].dut_dir, ".dut-serial/dut-a");
        assert_eq!(
            inv.duts[0].mcp_port,
            crate::ports::project_dut_mcp_port(tmp.path(), "dut-a"),
            "mcp_port must come from ports.rs, not a local copy"
        );
        assert_eq!(inv.duts[0].state, "active");
        assert_eq!(inv.duts[0].state_text, "ANSI[active]");
        assert_eq!(inv.duts[0].age_secs, Some(42));
        assert!(inv.duts[0].critical);

        assert_eq!(inv.duts[1].dut_name, "dut-b");
        assert_eq!(inv.duts[1].dut_dir, "/opt/dut-b");
        assert_eq!(inv.duts[1].state, "crashed");
    }

    #[test]
    fn test_inventory_json_omits_deleted_alias_keys() {
        // Wire-shape pin: the inventory carries NO alias spelling anywhere
        // (not the wire key `dut_alias`, not any `alias` JSON key), the
        // host object gains `host_name` (ip fallback here), and the DUT's
        // live identity key is `dut_name`.
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(tmp.path(), SIMPLE_FIXTURE);
        let parsed = parse_config_file(&conf).unwrap();
        let cfg = Config {
            values: defaults(),
            config_path: Some(conf),
            project_dir: Some(tmp.path().to_path_buf()),
            format: ConfigFormat::Jsonc,
        };
        let inv = build_inventory_json(
            &cfg,
            &parsed.dev_hosts,
            &parsed.duts,
            Some("rk3576"),
            InventoryMeta {
                written_by: "mcp-server",
                written_at_unix: 0,
                mcp_pid: None,
                mcp_http_port: None,
                heartbeat_secs: None,
                owner: None,
            },
            |_| canned_state("active", false),
        );
        let text = serde_json::to_string(&inv).unwrap();
        assert!(!text.contains("dev_host_alias"), "text: {text}");
        assert!(!text.contains("dut_aliases"), "text: {text}");
        assert!(!text.contains("dut_alias"), "text: {text}");
        assert!(!text.contains("\"alias\""), "text: {text}");
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        let host = &value["dev_hosts"][0];
        assert!(
            host.get("alias").is_none(),
            "host alias key must be gone: {text}"
        );
        assert_eq!(host["ip"], "host-a.invalid");
        // host_name is present (ip fallback — the fixture has no explicit
        // host_name key).
        assert_eq!(host["host_name"], "host-a.invalid", "text: {text}");
        // The DUT's dut_name is the live identity key.
        assert_eq!(value["duts"][0]["dut_name"], "rk3576");
    }

    #[test]
    fn test_build_inventory_current_dut_unset_or_not_found() {
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(tmp.path(), SIMPLE_FIXTURE);
        let parsed = parse_config_file(&conf).unwrap();
        let cfg = Config {
            values: defaults(),
            config_path: None,
            project_dir: None,
            format: ConfigFormat::None,
        };
        for name in [None, Some("no-such-dut")] {
            let inv = build_inventory_json(
                &cfg,
                &parsed.dev_hosts,
                &parsed.duts,
                name,
                InventoryMeta {
                    written_by: "dutabo",
                    written_at_unix: 0,
                    heartbeat_secs: None,
                    mcp_pid: None,
                    mcp_http_port: Some(crate::ports::project_mcp_port(Path::new("."))),
                    owner: None,
                },
                |_| canned_state("unknown", false),
            );
            assert_eq!(inv.current_dut, None, "name {name:?} must not resolve");
        }
    }

    #[test]
    fn test_parse_config_file_has_fixed_shape_for_later_consumers() {
        // Locks the ConfigFile/DutConfig public shape consumed by dutabo.rs
        // and the future inventory/dut_state modules.
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(tmp.path(), SIMPLE_FIXTURE);
        let config = parse_config_file(&conf).unwrap();
        assert_eq!(config.dev_hosts.len(), 1);
        let host = &config.dev_hosts[0];
        assert_eq!(host.ip, "host-a.invalid");
        // No host_name key in the fixture → host_name falls back to the ip.
        assert_eq!(host.host_name, host.ip);
        let dut = &config.duts[0];
        assert_eq!(dut.dut_name, "rk3576");
        assert_eq!(dut.dev_host_ip, "host-a.invalid");
        assert_eq!(dut.dev_host_user, "linaro");
        assert_eq!(dut.dev_host_pass, "");
    }

    #[test]
    fn test_host_name_explicit_value_wins_over_ip_fallback() {
        // An explicit host_name key is display-only: it resolves verbatim
        // and never participates in DUT grouping (grouping stays keyed on
        // dev_host_ip).
        let tmp = TempDir::new().unwrap();
        let conf = write_jsonc(
            tmp.path(),
            r#"{
  "dev_hosts": [
    {
      "ip": "host-a.invalid",
      "host_name": "lab-pc",
      "user": "linaro",
      "duts": [ { "dut_name": "rk3576", "serial": { "port": 41002 } } ]
    }
  ]
}"#,
        );
        let config = parse_config_file(&conf).unwrap();
        assert_eq!(config.dev_hosts[0].host_name, "lab-pc");
        assert_eq!(config.dev_hosts[0].ip, "host-a.invalid");
        // The DUT still groups by ip — host_name is never a lookup key.
        assert_eq!(config.duts[0].dev_host_ip, "host-a.invalid");
        assert_eq!(config.duts[0].dev_host_user, "linaro");
    }
}
