//! Strongly-typed parameter structs for every MCP tool exposed by
//! `McpHandler`. Each struct derives `serde::Deserialize` +
//! `schemars::JsonSchema`, so the `#[tool]` macro can auto-generate
//! the JSON Schema advertised to clients (Claude Code, Codex, pi.dev,
//! ...). This replaces the hand-written schemas in `mcp.rs::tool_definitions()`.
//!
//! Conventions:
//! * Optional fields use `Option<T>` so the schema marks them as non-required.
//! * `serde(default = "...")` lets callers omit the field; the matching
//!   `schemars(default = "...")` keeps the generated JSON Schema's
//!   `"default"` hint in sync (both are present on every defaulted field).
//! * Units stay close to the earlier `mcp.rs` semantics: integers as `i64`,
//!   floats as `f64`, durations in seconds / milliseconds as documented.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ── no-argument tools ────────────────────────────────────────────────────────

/// Empty parameter object for tools that take no arguments.
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct NoArgs {}

// ── query / state ────────────────────────────────────────────────────────────

/// `serial_get_logs` — retrieve serial log content with optional filtering.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetLogsArgs {
    /// Number of lines to return.
    #[serde(default = "default_lines")]
    #[schemars(default = "default_lines")]
    pub lines: i64,
    /// Regex filter pattern (optional).
    pub pattern: Option<String>,
    /// Archive index (0 = current cycle).
    #[serde(default = "default_archive")]
    #[schemars(default = "default_archive")]
    pub archive: i64,
    /// Apply shell prompt syntax highlighting (user@host → green,
    /// path → blue, $/# → white). Default false — the returned text goes to
    /// the agent, and injecting ANSI codes into it changes the payload, not
    /// just its look. Opt in only for a terminal consumer.
    #[serde(default = "default_false")]
    #[schemars(default = "default_false")]
    pub highlighted: bool,
}

fn default_lines() -> i64 {
    50
}
fn default_archive() -> i64 {
    0
}
fn default_true() -> bool {
    true
}
fn default_false() -> bool {
    false
}

// ── commands ─────────────────────────────────────────────────────────────────

/// `serial_send_command` — send a shell command to the target and return output.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SendCommandArgs {
    /// Shell command to execute on the target.
    pub command: String,
    /// Timeout in seconds.
    #[serde(default = "default_timeout_90")]
    #[schemars(default = "default_timeout_90")]
    pub timeout: i64,
}

fn default_timeout_90() -> i64 {
    90
}

fn default_exec_mode() -> ExecMode {
    ExecMode::Auto
}

/// `serial_uboot_command` — send a raw command at the U-Boot prompt (=> ).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct UbootCommandArgs {
    /// U-Boot command (e.g. 'version', 'help', 'printenv').
    pub command: String,
    /// Seconds to wait for the output to settle: returns early once the
    /// U-Boot prompt reappears or the output goes quiet, at the latest
    /// after this deadline.
    #[serde(default = "default_timeout_15")]
    #[schemars(default = "default_timeout_15")]
    pub timeout: i64,
}

/// Execution environment for `serial_exec`. Variant docs stay as plain
/// comments: schemars turns documented variants into a `oneOf`-of-consts
/// schema, while a bare unit enum renders the cross-host friendlier
/// plain `enum` list.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[schemars(inline)]
#[serde(rename_all = "snake_case")]
pub enum ExecMode {
    // Pick the protocol from the detected target state: U-Boot prompt →
    // U-Boot prompt/idle-gap collection, anything else → POSIX shell
    // marker protocol with exit code.
    #[default]
    Auto,
    // POSIX shell: marker-echo protocol, returns exit_code.
    Shell,
    // U-Boot prompt: no POSIX shell assumed, waits for the prompt or an
    // output silence gap.
    Uboot,
}

/// `serial_exec` — unified console command entry. The server absorbs the
/// protocol difference between the Linux shell and the U-Boot prompt so the
/// model understands a single "run a console command" tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExecArgs {
    /// Console command to execute on the target.
    pub command: String,
    /// Execution environment: auto (default) selects from the detected
    /// target state; pass shell/uboot explicitly when that state is stale.
    #[serde(default = "default_exec_mode")]
    #[schemars(default = "default_exec_mode")]
    pub mode: ExecMode,
    /// Timeout in seconds (deadline cap; both protocols return earlier
    /// when the command finishes).
    #[serde(default = "default_timeout_90")]
    #[schemars(default = "default_timeout_90")]
    pub timeout: i64,
}

fn default_timeout_15() -> i64 {
    15
}

/// `serial_send_raw` — send raw bytes to the serial port without markers.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SendRawArgs {
    /// Raw data to send (supports \n, \r, \x03 for Ctrl-C etc.).
    pub data: String,
}

// ── power / reset / boot entry ─────────────────────────────────────────────

/// `serial_reset` — hardware reset target via relay and rotate log.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ResetArgs {
    /// Wait for boot to complete.
    #[serde(default = "default_true")]
    #[schemars(default = "default_true")]
    pub wait_boot: bool,
    /// Retry the reset+wait on timeout (lava RetryAction semantics).
    #[serde(default = "default_retry_3")]
    #[schemars(default = "default_retry_3")]
    pub failure_retry: i64,
    /// Seconds between retries.
    #[serde(default = "default_retry_interval")]
    #[schemars(default = "default_retry_interval")]
    pub failure_retry_interval: f64,
}

fn default_retry_3() -> i64 {
    3
}
fn default_retry_interval() -> f64 {
    1.0
}

/// `serial_power_cycle` — power cycle the target via relay.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct PowerCycleArgs {
    /// Wait for boot to complete after power-on.
    #[serde(default = "default_true")]
    #[schemars(default = "default_true")]
    pub wait_boot: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UbootRestart {
    /// Prefer a hardware reset when reset control exists, otherwise reboot
    /// through the current serial shell.
    #[default]
    Auto,
    /// Require reset control and restart the target through it.
    Hardware,
    /// Send `reboot` through the current serial shell.
    Software,
    /// Do not restart; only interrupt a boot that is already in progress.
    None,
}

impl UbootRestart {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Hardware => "hardware",
            Self::Software => "software",
            Self::None => "none",
        }
    }
}

/// `serial_enter_uboot` — restart as requested and interrupt autoboot.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct EnterUbootArgs {
    /// Restart method: auto (hardware when configured, otherwise software),
    /// hardware, software, or none (interrupt an existing boot only).
    #[serde(default)]
    #[schemars(default)]
    pub restart: UbootRestart,
    /// Interrupt byte override for this call (`ctrl_c`, `^C`, `esc`, `space`,
    /// `f1`..`f12`, `0xNN`, a decimal byte, or one ASCII character). Function
    /// keys resolve to their leading ESC byte. Omit to use config.
    pub interrupt_char: Option<String>,
    /// Number of retry attempts on timeout (default 3). Interactive agent
    /// calls get the conservative count — bootdelay=0 boards routinely need
    /// the extra attempt; budget-constrained callers (`dutabo init` TEST,
    /// flash flows) pass an explicit smaller value.
    #[serde(default = "default_retry_3")]
    #[schemars(default = "default_retry_3")]
    pub failure_retry: i64,
    /// Seconds between retries.
    #[serde(default = "default_retry_interval")]
    #[schemars(default = "default_retry_interval")]
    pub failure_retry_interval: f64,
    /// Total interrupt-character flood duration (must cover SPL→U-Boot
    /// window, typically 3-8s).
    #[serde(default = "default_flood_duration")]
    #[schemars(default = "default_flood_duration")]
    pub flood_duration_secs: f64,
    /// Interval between interrupt bytes (100ms = 10 bytes/s, avoids UART
    /// FIFO overflow).
    #[serde(default = "default_flood_interval_ms")]
    #[schemars(default = "default_flood_interval_ms")]
    pub flood_interval_ms: i64,
    /// U-Boot interrupt strategy: flood (default) or pattern.
    #[serde(default)]
    #[schemars(default)]
    pub interrupt_strategy: Option<String>,
    /// Regex to wait for before sending one interrupt byte in pattern mode.
    /// Omit to use the per-DUT `uboot.pattern` value.
    #[serde(default)]
    #[schemars(default)]
    pub interrupt_pattern: Option<String>,
}

fn default_flood_duration() -> f64 {
    15.0
}
fn default_flood_interval_ms() -> i64 {
    100
}

/// `serial_wait_pattern` — wait until a regex pattern appears in serial
/// output.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct WaitPatternArgs {
    /// Regex pattern to match.
    pub pattern: String,
    /// Timeout in seconds.
    #[serde(default = "default_timeout_60")]
    #[schemars(default = "default_timeout_60")]
    pub timeout: i64,
    /// Optional action (e.g. send_ctrl_c).
    pub action: Option<String>,
}

fn default_timeout_60() -> i64 {
    60
}

// ── logs ─────────────────────────────────────────────────────────────────────

/// `serial_poll_logs` — get new serial output since last poll (long-polling).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct PollLogsArgs {
    /// Byte-offset cursor echoed back from a previous poll's `since` response
    /// field. Resume polling from this cursor to receive only new output
    /// (log rotation resets the cursor transparently). Omit for the server's
    /// own positional cursor.
    pub since: Option<f64>,
    /// Long-poll timeout in seconds: block up to this long waiting for new
    /// output (0 = single non-blocking check).
    #[serde(default = "default_timeout_10")]
    #[schemars(default = "default_timeout_10")]
    pub timeout: i64,
}

fn default_timeout_10() -> i64 {
    10
}

// ── reference / learning ────────────────────────────────────────────────────

/// `serial_load_reference` — load a reference boot log to enable adaptive
/// stage detection.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct LoadReferenceArgs {
    /// Absolute path to the reference boot log file.
    pub reference_log_path: String,
}

/// `serial_append_reference` — append key anchor lines to the reference log
/// and hot-reload StageLearner.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct AppendReferenceArgs {
    /// Key anchor lines to append (newline-separated). Pick distinctive
    /// lines that mark a boot stage boundary.
    pub lines: String,
}

/// `serial_learn_connection` — run connection learning to verify serial
/// connectivity.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct LearnConnectionArgs {
    /// Learning method: 'hardware' (relay reset, default), 'software'
    /// (reboot command), or 'auto' (try hardware first, fallback to
    /// software).
    pub method: Option<String>,
    /// Hardware cycle source: "reset" (default) or "power". TEST uses
    /// power whenever a power relay channel is configured.
    #[serde(default)]
    #[schemars(default)]
    pub hardware_action: Option<String>,
    /// Reference-log output path override for this learning run. Omit to use
    /// the configured monitor.reference_log.
    pub reference_log_path: Option<String>,
    /// Maximum number of reset/reboot capture cycles (default 3, clamped to
    /// 2..=10 — the learner needs at least two cycles to compare, and an
    /// unbounded count would let one tool call power-cycle the board for
    /// hours while holding the engine lock). The count is only the sampling
    /// BUDGET: learning stops early the moment the pairwise similarity
    /// reaches the threshold (≥93%), so extra cycles only matter when the
    /// similarity never gets there. `dutabo init`'s TEST passes 2 to bound
    /// the failure path; production calls keep the default 3.
    #[serde(default)]
    #[schemars(default)]
    pub cycles: Option<usize>,
    /// Use the bounded TEST-button profile: capture only the comparable boot
    /// prefix and, when a reference already exists, validate one fresh reset
    /// against it instead of generating two new samples. Normal MCP calls
    /// keep the exhaustive full-boot learning behavior.
    #[serde(default)]
    #[schemars(default)]
    pub quick: bool,
}

/// Upper bound for `LearnConnectionArgs::cycles` — the single constant
/// behind the 2..=10 clamp contract (defense in depth: the handler clamps
/// via `cycles_bounded()`, the engine clamps again at entry).
pub const MAX_LEARN_CYCLES: usize = 10;

impl LearnConnectionArgs {
    pub fn cycles_bounded(&self) -> Option<usize> {
        self.cycles.map(|c| c.clamp(2, MAX_LEARN_CYCLES))
    }
}

// ── buttons / flash ─────────────────────────────────────────────────────────

/// `serial_button` — control a DUT button via the power control abstraction.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ButtonArgs {
    /// Button name: 'reset' or 'download'.
    pub button: String,
    /// Action: 'press', 'release', or 'pulse' (press+delay+release).
    pub action: String,
    /// Delay in milliseconds for pulse action (default: 500).
    pub delay_ms: Option<i64>,
}

/// `serial_flash_plan` — generate a flash plan from config. Does NOT execute.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct FlashPlanArgs {
    /// Local path to the firmware image (e.g. update.img or boot.img).
    pub image_path: String,
    /// Image type: 'full' (complete firmware) or 'kernel' (kernel/boot
    /// only). Default: auto-detect from flash config.
    pub image_type: Option<String>,
}

/// `serial_set_baud` — change the DUT console baud rate dynamically via RFC 2217.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetBaudArgs {
    /// Target baud rate, e.g. 1500000, 115200, 921600, 9600.
    pub baud: u32,
}

/// `serial_test_baud` — verify one candidate rate using freshly captured raw
/// boot bytes plus configured/advertised ser2net baud metadata.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct TestBaudArgs {
    pub baud: u32,
    /// Maximum boot capture duration. Five seconds of silence ends it early.
    /// Default 12s covers slow-booting boards whose kernel output starts
    /// late; budget-constrained callers (`dutabo init` TEST) pass an
    /// explicit shorter value.
    #[serde(default = "default_baud_capture_secs")]
    #[schemars(default = "default_baud_capture_secs")]
    pub capture_secs: f64,
    /// Operate the reset relay to obtain a fresh boot capture (default
    /// true). `dutabo init`'s TEST passes false when the form's dev_ctrl is
    /// none — the probe must never touch a relay the user disabled.
    #[serde(default = "default_true")]
    #[schemars(default = "default_true")]
    pub use_reset: bool,
}

fn default_baud_capture_secs() -> f64 {
    12.0
}

// ── structured output shapes ─────────────────────────────────────────────────
//
// Advertised via the tool `outputSchema` and mirrored into
// `structuredContent` (MCP enhancement, always alongside the TextContent
// JSON fallback). Schemas stay intentionally loose — only fields every
// response carries are declared; schemars does not emit
// `additionalProperties: false`, so the richer live payload still validates.

/// `serial_get_state` output shape.
#[derive(Debug, schemars::JsonSchema, serde::Serialize)]
pub struct StateOutput {
    /// State this session effectively acts on: the occupancy state
    /// ("busy" when another agent session owns the MCP, "dutabo" while
    /// the interactive CLI holds the port) or, for an owner session with
    /// the port free, the device state.
    pub state: String,
    /// Underlying device state (active, booting, uboot, crashed, DUT-off,
    /// disconnected, stopped). Equals `state` when no occupancy applies.
    pub device_state: String,
    /// Configured DUT name, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dut_name: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_schema_contains_runtime_defaults() {
        let schema = schemars::schema_for!(GetLogsArgs);
        let value = serde_json::to_value(schema).unwrap();
        assert_eq!(value["properties"]["lines"]["default"], 50);
        assert_eq!(value["properties"]["archive"]["default"], 0);
        assert_eq!(value["properties"]["highlighted"]["default"], false);
    }

    #[test]
    fn enter_uboot_restart_defaults_to_auto_and_accepts_all_modes() {
        let args: EnterUbootArgs = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(args.restart, UbootRestart::Auto);

        for (name, expected) in [
            ("auto", UbootRestart::Auto),
            ("hardware", UbootRestart::Hardware),
            ("software", UbootRestart::Software),
            ("none", UbootRestart::None),
        ] {
            let args: EnterUbootArgs =
                serde_json::from_value(serde_json::json!({"restart": name})).unwrap();
            assert_eq!(args.restart, expected);
        }
    }
}
