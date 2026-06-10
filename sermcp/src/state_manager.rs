//! State manager — hysteresis-debounced finite state machine with three-tier
//! visibility (internal → MCP API → statusline file).
//!
//! # State diagram
//!
//! ```text
//!                    ┌──────────┐
//!                    │  Stopped │  (internal — engine not started)
//!                    └────┬─────┘
//!                         │ start/connect
//!                    ┌────┴──────────────┐
//!                    │                   │
//!               ┌────▼─────┐       ┌─────▼──────┐
//!               │  Active  │◄──────│Disconnected │  ✗ visible
//!               └──┬───┬───┘       └─────┬──────┘
//!                  │   │                 │
//!      ┌───────────┘   └──────────┐      │
//!      │              reconnect() │      │ read error /
//!      │                          │      │ connect fail
//! ┌────▼─────┐               ┌────▼─────┐
//! │ Booting  │──────────────►│  UBoot   │  ◐/● visible
//! └────┬─────┘               └────┬─────┘
//!      │                          │
//!      │ kernel panic             │ Ctrl-C interrupt
//! ┌────▼─────┐                    │
//! │ Crashed  │◄───────────────────┘  ✗ visible
//! └────┬─────┘
//!      │ (watchdog reboot)
//!      └──────► Booting → ...
//!
//! ┌──────────┐
//! │  DUT-off │  ✗ visible  (heartbeat timeout / no probe response)
//! └────┬─────┘
//!      │ (data arrives) → Active
//!      │ (read error)   → Disconnected
//!
//! ┌──────────┐
//! │  Dutabo  │  ● visible  (serial taken over by dutabo CLI)
//! └──────────┘
//!      │ (dutabo exits) → reconnect → probe → Active
//! ```
//!
//! # Event sources and transitions
//!
//! | Event | Source | Old → New |
//! |-------|--------|-----------|
//! | TCP connect OK + probe finds shell/login | `probe_initial_state()` | any → Active |
//! | SPL/DDR detected | `BootStageDetector::feed()` | any → Booting |
//! | U-Boot prompt detected | `BootStageDetector::feed()` | Booting → UBoot |
//! | Kernel panic / BUG / Oops | `BootStageDetector::feed()` | any → Crashed |
//! | Heartbeat timeout (60s + 3× hysteresis) | `check_hang()` | Active/Booting → DUT-off |
//! | Blank-line probe returned data | `read_loop_iter()` | DUT-off → Active |
//! | TCP read error ("Connection closed") | `handle_read_error()` | any → Disconnected |
//! | TCP connect fails (backoff retry) | `read_loop_iter()` | Disconnected → Disconnected |
//! | TCP connect succeeds after backoff | `read_loop_iter()` | Disconnected → *probed* |
//! | dutabo sentinel file created | `read_loop_iter()` | any → Dutabo |
//! | dutabo sentinel removed | `read_loop_iter()` | Dutabo → *reconnected* |
//!
//! # Hysteresis / debounce
//!
//! - **Hang detection** (`check_hang`): state must be in the hang-candidate
//!   set (Active, Booting, Dutabo). Silence past `hang_timeout` (60s) first
//!   requests a blank-line probe; only an actually-sent probe (one that
//!   `mark_probe_sent` stamped after the last data) that goes unanswered
//!   within its 5s response window counts a miss (`heartbeat_missed++`).
//!   After `hysteresis` (default 3) unanswered probe rounds → DUT-off.
//! - **Crash throttle**: crash events within 2s of each other are suppressed
//!   (a single panic often produces multiple log lines matching crash patterns).
//!
//! # Visibility tiers
//!
//! | Tier | States visible | Consumers |
//! |------|---------------|-----------|
//! | Internal | All 8 states | `serial_engine.rs` logic |
//! | MCP API | 7 states (excl. Stopped) | `serial_get_state` tool |
//! | Statusline | 7 states (excl. Stopped) | `statusline.py` hook |
//!
//! `Disconnected` is written to both the state file and statusline cache so
//! Codex/Claude integrations can surface a lost serial connection immediately.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Target board state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetState {
    Stopped,
    Active,
    Booting,
    UBoot,
    Crashed,
    DutOff,
    Disconnected,
    /// Serial is taken over by dutabo interactive session
    Dutabo,
}

impl TargetState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Active => "active",
            Self::Booting => "booting",
            Self::UBoot => "uboot",
            Self::Crashed => "crashed",
            Self::DutOff => "DUT-off",
            Self::Disconnected => "disconnected",
            Self::Dutabo => "dutabo",
        }
    }

    /// MCP-visible states (excludes stopped).
    pub fn is_external(&self) -> bool {
        !matches!(self, Self::Stopped)
    }

    /// Hang/heartbeat detection candidates: booting + active.
    /// Long silence → send heartbeat probe to check liveness; a DUT is only
    /// declared hung when `hysteresis` probe rounds go unanswered.
    fn is_hang_candidate(&self) -> bool {
        matches!(self, Self::Booting | Self::Active | Self::Dutabo)
    }
}

/// Write a file with restricted permissions (0600) to prevent other users
/// from reading potentially sensitive target information.
fn write_secure(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    std::fs::write(path, content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(())
}

/// Manages target device state with hysteresis debounce, atomic file writes,
/// hang/heartbeat detection, and three-tier state visibility (internal, MCP-API, statusline).
pub struct StateManager {
    current: TargetState,
    state_file: PathBuf,
    /// Project-local cache (fallback, read by Python hook)
    cache_file: PathBuf,
    /// Legacy `/dev/shm/claude-status-*` cache path — D10: no longer written
    /// (dead weight, no reader); kept only so `delete_shm_cache` can reap
    /// residue left by older builds on Stopped.
    shm_cache_file: PathBuf,
    /// Notification directory for Agent proactive alerts
    notify_dir: PathBuf,
    hang_timeout_secs: u64,
    /// Max consecutive unanswered probe rounds before DUT-off.
    hysteresis: u32,
    /// Max seconds a Booting state may last (see `config.boot_window_secs`).
    boot_window_secs: u64,
    /// When the current Booting episode started (`None` outside Booting;
    /// re-set on every entry into Booting so each boot gets a fresh window).
    boot_started_at: Option<Instant>,
    last_data_time: Instant,
    /// Heartbeat probe: set to true when the booting/active state has no
    /// data, triggers serial_engine to send a probe.
    pub heartbeat_pending: bool,
    /// Time of last probe sent (gives the board a response window).
    last_probe_time: Instant,
    /// Number of consecutive probe misses (exceeds hysteresis → DUT-off).
    heartbeat_missed: u32,
    /// DUT name for per-DUT state file paths (empty = single DUT, uses root .dut-serial/).
    dut_name: String,
    /// Project root directory (for constructing name-specific state paths).
    project_dir: PathBuf,
    /// Metrics: total commands successfully executed.
    command_count: u64,
    /// Metrics: total command errors (timeout, disconnection).
    error_count: u64,
    /// Metrics: engine start timestamp.
    start_time: Instant,
    /// Metrics: command latencies in ms (circular buffer, max 1000 samples).
    latencies_ms: VecDeque<u64>,
    /// Optional hook invoked after EVERY real transition, once the state
    /// files reflect the new state (written or deleted). Wired by the server
    /// binary to refresh `.dut-serial/inventory.json` so consumers of the
    /// inventory (Python hooks) never render stale state.
    on_state_change: Option<Box<dyn Fn(TargetState) + Send>>,
}

impl StateManager {
    pub fn new(
        project_dir: &Path,
        hang_timeout: u64,
        hysteresis: u32,
        dut_dir: &str,
        dut_name: &str,
        boot_window_secs: u64,
    ) -> Self {
        let dut_dir_path = project_dir.join(dut_dir);
        let state_file = dut_dir_path.join("target-state");
        let cache_file = dut_dir_path.join("statusline-cache");
        let notify_dir = dut_dir_path.join("notifications");

        // /dev/shm cache: keyed by project path hash so multiple projects don't collide
        let shm_dir = if std::path::Path::new("/dev/shm").is_dir() {
            "/dev/shm"
        } else {
            "/tmp"
        };
        let project_hash = Self::project_hash(project_dir);
        let shm_cache_file =
            std::path::PathBuf::from(format!("{}/claude-status-{}", shm_dir, project_hash));

        std::fs::create_dir_all(&dut_dir_path).ok();
        std::fs::create_dir_all(&notify_dir).ok();
        // Per-DUT state files always live under the project runtime root. The
        // resolved dut_dir may already include the DUT name, so appending the
        // name to dut_dir would create `.dut-serial/<name>/<name>`.
        if !dut_name.is_empty() {
            std::fs::create_dir_all(project_dir.join(".dut-serial").join(dut_name)).ok();
        }

        // One shared `now`: equal timestamps mean "no probe in flight"
        // (see `check_hang` for the in-flight discriminator).
        let now = Instant::now();
        Self {
            current: TargetState::Stopped,
            state_file,
            cache_file,
            shm_cache_file,
            notify_dir,
            hang_timeout_secs: hang_timeout,
            hysteresis,
            boot_window_secs,
            boot_started_at: None,
            last_data_time: now,
            heartbeat_pending: false,
            last_probe_time: now,
            heartbeat_missed: 0,
            dut_name: dut_name.to_string(),
            project_dir: project_dir.to_path_buf(),
            command_count: 0,
            error_count: 0,
            start_time: Instant::now(),
            latencies_ms: VecDeque::new(),
            on_state_change: None,
        }
    }

    /// Install the on-transition hook. It runs synchronously at the end of
    /// every real `transition()` (after the state files are written or, for
    /// Stopped, deleted) — never for a no-op same-state transition. Only one
    /// hook may be installed (a second call replaces the first).
    pub fn set_on_state_change(&mut self, hook: impl Fn(TargetState) + Send + 'static) {
        self.on_state_change = Some(Box::new(hook));
    }

    /// Stable 8-char hex hash matching Python's get_session_id() (md5).
    /// Both MCP and statusline hook use the same project_dir → same hash.
    fn project_hash(project_dir: &Path) -> String {
        use md5::{Digest, Md5};
        let canonical = project_dir
            .canonicalize()
            .unwrap_or_else(|_| project_dir.to_path_buf());
        let mut hasher = Md5::new();
        hasher.update(canonical.to_string_lossy().as_bytes());
        let digest = hasher.finalize();
        crate::ports::md5_hex(&digest).chars().take(8).collect()
    }

    /// Write PID file with restricted permissions (0600).
    /// Call only after the project-level lock is acquired.
    pub fn write_pid(&self, project_dir: &Path, dut_dir: &str) {
        let pid_file = project_dir.join(dut_dir).join("mcp.pid");
        let _ = write_secure(&pid_file, &std::process::id().to_string());
    }

    /// Remove the per-DUT PID only when this process wrote it.
    pub fn remove_pid(&self, project_dir: &Path, dut_dir: &str) {
        let pid_file = project_dir.join(dut_dir).join("mcp.pid");
        let owner = std::fs::read_to_string(&pid_file)
            .ok()
            .and_then(|value| value.trim().parse::<u32>().ok());
        if owner == Some(std::process::id()) {
            let _ = std::fs::remove_file(pid_file);
        }
    }

    /// Return the current internal target state.
    pub fn current(&self) -> TargetState {
        self.current
    }

    /// MCP API state (stopped/connecting → None)
    pub fn external_state(&self) -> Option<TargetState> {
        if self.current.is_external() {
            Some(self.current)
        } else {
            None
        }
    }

    /// Increment the successful command counter.
    pub fn inc_command(&mut self) {
        self.command_count += 1;
    }

    /// Increment the command error counter.
    pub fn inc_error(&mut self) {
        self.error_count += 1;
    }

    /// Engine uptime in seconds since start.
    pub fn uptime_secs(&self) -> f64 {
        self.start_time.elapsed().as_secs_f64()
    }

    /// Number of commands successfully executed.
    pub fn command_count(&self) -> u64 {
        self.command_count
    }

    /// Number of command errors (timeout, disconnection).
    pub fn error_count(&self) -> u64 {
        self.error_count
    }

    /// Record a command latency in milliseconds.
    pub fn record_latency(&mut self, ms: u64) {
        if self.latencies_ms.len() >= 1000 {
            self.latencies_ms.pop_front();
        }
        self.latencies_ms.push_back(ms);
    }

    /// Compute p50/p95/p99 latency from recorded samples.
    pub fn latency_percentiles(&self) -> (u64, u64, u64) {
        if self.latencies_ms.is_empty() {
            return (0, 0, 0);
        }
        let mut sorted: Vec<_> = self.latencies_ms.iter().copied().collect();
        sorted.sort_unstable();
        let p50 = sorted[(sorted.len() - 1) * 50 / 100];
        let p95 = sorted[(sorted.len() - 1) * 95 / 100];
        let p99 = sorted[(sorted.len() - 1) * 99 / 100];
        (p50, p95, p99)
    }

    /// Transition to a new target state. No-op if `new` is the same as current.
    /// Writes state files atomically for external states; deletes files on Stopped;
    /// removes files on Stopped.
    pub fn transition(&mut self, new: TargetState) {
        if new == self.current {
            return;
        }
        // Warn on suspicious transitions — don't block, the external world
        // can produce unexpected state sequences (e.g. board watchdog reboot
        // during a crash). These warnings help diagnose bugs in the detector.
        let from = self.current;
        match (from, new) {
            // Crashed → Active without an intervening boot is suspicious.
            (TargetState::Crashed, TargetState::Active) => {
                tracing::warn!("Suspicious transition: crashed → active (no boot cycle detected)");
            }
            // Dutabo takeover should come from a connected state.
            (TargetState::Disconnected, TargetState::Dutabo) => {
                tracing::warn!("Suspicious transition: disconnected → dutabo");
            }
            _ => {}
        }
        tracing::info!("StateManager: {} → {}", from.as_str(), new.as_str());
        self.current = new;
        // Boot-window bookkeeping: a fresh window per entry into Booting
        // (a Booting→Booting no-op never resets a running window), cleared
        // on every exit so the deadline only ever applies to Booting.
        if new == TargetState::Booting {
            if from != TargetState::Booting {
                self.boot_started_at = Some(Instant::now());
            }
        } else {
            self.boot_started_at = None;
        }

        match new {
            TargetState::Stopped => {
                self.delete_state_file();
                self.delete_cache_file();
                self.delete_shm_cache();
                // D11: the per-DUT name-dir files written on the
                // Active/booting path are deleted too — nothing else reaps
                // them, so a clean stop must leave no state residue.
                if !self.dut_name.is_empty() {
                    let _ = std::fs::remove_file(self.per_dut_file("target-state"));
                }
                if !self.dut_name.is_empty() && self.dut_name != "default" {
                    let _ = std::fs::remove_file(self.per_dut_file("statusline-cache"));
                }
            }
            TargetState::Active
            | TargetState::Booting
            | TargetState::UBoot
            | TargetState::Crashed
            | TargetState::Disconnected
            | TargetState::DutOff
            | TargetState::Dutabo => {
                self.atomic_write(&self.state_file, new.as_str());
                // Also write to per-DUT name directory when the name is configured
                if !self.dut_name.is_empty() {
                    let name_state_file = self.per_dut_file("target-state");
                    self.atomic_write(&name_state_file, new.as_str());
                }
                let text = self.format_statusline(new);
                self.atomic_write(&self.cache_file, &text);
                // Sync to per-DUT name directory (preferred by statusline)
                if !self.dut_name.is_empty() && self.dut_name != "default" {
                    let name_cache = self.per_dut_file("statusline-cache");
                    self.atomic_write(&name_cache, &text);
                }
                // Don't blindly sync to all per-DUT dirs — that would overwrite
                // states written by other MCP servers (e.g. dutabo, disconnected).
                // Write Agent notification for critical states
                if matches!(
                    new,
                    TargetState::Crashed | TargetState::DutOff | TargetState::Disconnected
                ) {
                    self.write_notification(new);
                }
            }
        }
        // Refresh consumers AFTER the files reflect the new state (written,
        // or deleted for Stopped) so a hook re-reading them can never
        // observe the pre-transition state.
        if let Some(ref hook) = self.on_state_change {
            hook(new);
        }
    }

    /// `.dut-serial/<dut_name>/<file>` — the per-DUT state file path (callers
    /// keep the guards: non-empty name, and `!= "default"` for statusline-cache).
    fn per_dut_file(&self, file: &str) -> PathBuf {
        self.project_dir
            .join(".dut-serial")
            .join(&self.dut_name)
            .join(file)
    }

    /// Produce an ANSI-formatted statusline string for a given target state.
    /// Format a statusline string with ANSI color. Uses the DUT name
    /// from .target.jsonc if available, otherwise "serial".
    pub(crate) fn format_statusline(&self, state: TargetState) -> String {
        let label = if self.dut_name.is_empty() || self.dut_name == "default" {
            "serial"
        } else {
            &self.dut_name
        };
        Self::format_statusline_labeled(state, label)
    }

    /// Format statusline with explicit label (for testing, or when no
    /// StateManager instance is available).
    pub(crate) fn format_statusline_labeled(state: TargetState, label: &str) -> String {
        match state {
            TargetState::Active => format!("\x1b[32m● {}:active\x1b[0m", label),
            TargetState::Booting => format!("\x1b[33m◐ {}:booting\x1b[0m", label),
            TargetState::UBoot => format!("\x1b[36m● {}:uboot\x1b[0m", label),
            TargetState::Crashed => format!("\x1b[31m✗ {}:crashed\x1b[0m", label),
            TargetState::Disconnected => format!("\x1b[31m✗ {}:disconnected\x1b[0m", label),
            TargetState::DutOff => format!("\x1b[31m✗ {}:DUT-off\x1b[0m", label),
            TargetState::Dutabo => format!("\x1b[35m● {}:dutabo\x1b[0m", label),
            TargetState::Stopped => String::new(),
        }
    }

    /// Force-write `dutabo status` data to board directories that have a target-state file.
    /// Called from set_ws_tx() so the statusline reflects "dutabo" even when
    /// multiple MCP servers share the same serial port.
    pub(crate) fn write_dutabo_forced(&self) {
        let text = self.format_statusline(TargetState::Dutabo);
        self.atomic_write(&self.cache_file, &text);
        // Only write to the name-specific dir if we know which DUT
        if !self.dut_name.is_empty() && self.dut_name != "default" {
            let name_cache = self.per_dut_file("statusline-cache");
            self.atomic_write(&name_cache, &text);
            let name_state = self.per_dut_file("target-state");
            self.atomic_write(&name_state, "dutabo");
        }
    }

    /// Called on every serial data arrival — resets hang and heartbeat counters.
    /// Both timestamps share one `now` so `last_probe_time > last_data_time`
    /// unambiguously means a probe is in flight (see `check_hang`).
    pub fn on_activity(&mut self) {
        let now = Instant::now();
        self.last_data_time = now;
        self.last_probe_time = now;
        self.heartbeat_pending = false;
        self.heartbeat_missed = 0;
    }

    /// Mark that a heartbeat probe was sent (called by serial_engine).
    pub fn mark_probe_sent(&mut self) {
        self.last_probe_time = Instant::now();
        self.heartbeat_pending = false;
    }

    /// Test hook: age the last-data timestamp so hang/heartbeat tests can
    /// simulate silence without real-time sleeps.
    #[cfg(test)]
    pub fn age_last_data_for_test(&mut self, age: std::time::Duration) {
        self.last_data_time = self
            .last_data_time
            .checked_sub(age)
            .unwrap_or(self.last_data_time);
    }

    /// Test hook: number of consecutive unanswered probe rounds scored.
    #[cfg(test)]
    pub fn heartbeat_missed(&self) -> u32 {
        self.heartbeat_missed
    }

    /// Response window for a sent heartbeat probe: if no serial data arrives
    /// within this many seconds of `mark_probe_sent`, the round is missed.
    const PROBE_RESPONSE_WINDOW_SECS: f64 = 5.0;

    /// Check for hang / heartbeat timeout.
    /// booting/active: long silence → request a newline probe (no command);
    ///          if no response within the probe window → miss++; after
    ///          `hysteresis` unanswered probe rounds → DUT-off.
    ///
    /// A probe counts as "in flight" only when it was sent AFTER the last
    /// data arrival (`last_probe_time > last_data_time`). Misses are only
    /// counted for probes actually in flight — the first check after silence
    /// begins requests a probe instead of scoring a phantom miss, and a
    /// `hang_timeout` below the probe window waits out the window rather
    /// than re-sending probes forever.
    pub fn check_hang(&mut self) {
        if !self.current.is_hang_candidate() {
            self.heartbeat_pending = false;
            return;
        }
        // Boot-window deadline: a board's boot finishes well inside the
        // window, so once it elapses the state MUST leave Booting even
        // without shell-stage console evidence (a quiet console — kernel
        // `quiet` cmdline, desktop image without serial getty — must not
        // pin the display on `booting` forever). Active-state hang and
        // heartbeat detection takes over from there, so a board that truly
        // failed to boot still converges to a critical state.
        if self.current == TargetState::Booting
            && let Some(started) = self.boot_started_at
            && started.elapsed().as_secs() >= self.boot_window_secs
        {
            tracing::warn!(
                "[AGENT-NOTIFY] boot window ({}s) elapsed without boot-complete evidence — assuming active",
                self.boot_window_secs
            );
            self.transition(TargetState::Active);
            return;
        }
        let data_elapsed = self.last_data_time.elapsed().as_secs_f64();
        if data_elapsed <= self.hang_timeout_secs as f64 {
            return;
        }
        // `on_activity` stamps both timestamps with the same `now`, while
        // `mark_probe_sent` advances only `last_probe_time` — so a strictly
        // newer probe timestamp means a probe was sent since the last data.
        let probe_in_flight = self.last_probe_time > self.last_data_time;
        let probe_elapsed = self.last_probe_time.elapsed().as_secs_f64();
        // Probe sent and no response within the window → this round is a miss
        if probe_in_flight && probe_elapsed > Self::PROBE_RESPONSE_WINDOW_SECS {
            self.heartbeat_missed += 1;
            tracing::warn!(
                "Heartbeat miss #{}: no data for {:.0}s, probe sent {:.0}s ago",
                self.heartbeat_missed,
                data_elapsed,
                probe_elapsed
            );
            // Round consumed — no probe is in flight until the watchdog
            // sends the re-armed one.
            self.last_probe_time = self.last_data_time;
            if self.heartbeat_missed >= self.hysteresis {
                tracing::warn!("Heartbeat: {} misses → DUT-off", self.heartbeat_missed);
                self.transition(TargetState::DutOff);
            } else {
                // Request another probe
                self.heartbeat_pending = true;
            }
        } else if !probe_in_flight {
            // First timeout — request a probe
            self.heartbeat_pending = true;
        }
        // else: probe in flight, still inside its response window — wait.
    }

    pub fn last_data_elapsed(&self) -> std::time::Duration {
        self.last_data_time.elapsed()
    }

    /// Write a notification JSON file for Agent proactive alerts.
    /// Notifications are written to `.dut-serial/notifications/<timestamp>-<state>.json`.
    fn write_notification(&self, state: TargetState) {
        let ts = chrono::Local::now().format("%Y%m%d_%H%M%S");
        let fname = format!("{ts}-{}.json", state.as_str());
        let path = self.notify_dir.join(&fname);

        let alert = match state {
            TargetState::Crashed => serde_json::json!({
                "type": "target_alert",
                "state": "crashed",
                "severity": "critical",
                "message": "Target has crashed (Kernel panic/BUG/Oops detected). Check serial logs for crash details.",
                "suggested_action": "Use serial_get_logs with pattern='panic|BUG|Oops' to analyze the crash.",
                "timestamp": chrono::Local::now().to_rfc3339(),
            }),
            TargetState::DutOff => {
                // Only hang-verdict transitions ran probe rounds; non-hang
                // DUT-off flows (user poweroff, initial probe, command
                // timeout) have zero misses and keep the neutral wording.
                let message = if self.heartbeat_missed > 0 {
                    format!(
                        "Target is not responding — no serial output and no response to {} probe attempts. May be powered off or hung.",
                        self.heartbeat_missed
                    )
                } else {
                    "Target is not responding (no serial output). May be powered off or hung."
                        .to_string()
                };
                serde_json::json!({
                    "type": "target_alert",
                    "state": "DUT-off",
                    "severity": "warning",
                    "message": message,
                    "suggested_action": "Try serial_reset to reboot the target, or check power supply.",
                    "timestamp": chrono::Local::now().to_rfc3339(),
                })
            }
            TargetState::Disconnected => serde_json::json!({
                "type": "target_alert",
                "state": "disconnected",
                "severity": "warning",
                "message": "Serial connection lost. ser2net may be down or network issue.",
                "suggested_action": "Check ser2net on dev host, or use serial_claim to reconnect.",
                "timestamp": chrono::Local::now().to_rfc3339(),
            }),
            _ => return,
        };

        if let Err(e) = write_secure(
            &path,
            &serde_json::to_string_pretty(&alert).unwrap_or_default(),
        ) {
            tracing::error!("Failed to write notification {path:?}: {e}");
        } else {
            tracing::info!("Agent notification written: {fname}");
            // Also append to a consolidated alert log
            let alert_log = self.notify_dir.join("alerts.log");
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&alert_log)
                .ok();
            if let Some(ref mut file) = f {
                use std::io::Write;
                writeln!(
                    file,
                    "{}",
                    serde_json::to_string(&alert).unwrap_or_default()
                )
                .ok();
            }
        }
    }

    fn atomic_write(&self, path: &Path, content: &str) {
        let tmp = path.with_extension("tmp");
        if let Err(e) = write_secure(&tmp, content) {
            tracing::error!("StateManager: write failed: {e}");
            return;
        }
        if let Err(e) = std::fs::rename(&tmp, path) {
            tracing::error!("StateManager: rename failed: {e}");
        }
    }

    fn delete_state_file(&self) {
        let _ = std::fs::remove_file(&self.state_file);
    }

    fn delete_cache_file(&self) {
        let _ = std::fs::remove_file(&self.cache_file);
    }

    fn delete_shm_cache(&self) {
        let _ = std::fs::remove_file(&self.shm_cache_file);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_test_state_manager(hang_timeout: u64, hysteresis: u32) -> (StateManager, TempDir) {
        let tmp = TempDir::new().unwrap();
        let sm = StateManager::new(tmp.path(), hang_timeout, hysteresis, ".dut-serial", "", 30);
        (sm, tmp)
    }

    #[test]
    fn test_initial_state() {
        let (sm, _tmp) = create_test_state_manager(60, 3);
        assert_eq!(sm.current(), TargetState::Stopped);
        assert_eq!(sm.external_state(), None); // Stopped is not external
    }

    #[test]
    fn test_state_transitions() {
        let (mut sm, _tmp) = create_test_state_manager(60, 3);

        sm.transition(TargetState::Active);
        assert_eq!(sm.current(), TargetState::Active);
        assert_eq!(sm.external_state(), Some(TargetState::Active));

        sm.transition(TargetState::Booting);
        assert_eq!(sm.current(), TargetState::Booting);
        assert_eq!(sm.external_state(), Some(TargetState::Booting));

        sm.transition(TargetState::UBoot);
        assert_eq!(sm.current(), TargetState::UBoot);
    }

    #[test]
    fn test_state_file_written() {
        let (mut sm, tmp) = create_test_state_manager(60, 3);
        let state_file = tmp.path().join(".dut-serial/target-state");

        sm.transition(TargetState::Active);
        assert_eq!(std::fs::read_to_string(&state_file).unwrap(), "active");

        sm.transition(TargetState::UBoot);
        assert_eq!(std::fs::read_to_string(&state_file).unwrap(), "uboot");

        sm.transition(TargetState::Crashed);
        assert_eq!(std::fs::read_to_string(&state_file).unwrap(), "crashed");
    }

    #[test]
    fn test_hang_detection_not_candidate() {
        let (mut sm, _tmp) = create_test_state_manager(1, 2);

        // Active is a candidate now, but without timeout it stays Active
        sm.transition(TargetState::Active);
        sm.check_hang();
        assert_eq!(sm.current(), TargetState::Active);

        // UBoot/Crashed/Disconnected are NOT candidates
        sm.transition(TargetState::UBoot);
        sm.check_hang();
        assert_eq!(sm.current(), TargetState::UBoot);
    }

    #[test]
    fn test_hang_detection_booting() {
        let (mut sm, _tmp) = create_test_state_manager(0, 2); // 0s timeout for fast test

        // Small sleep to ensure elapsed > 0 (as_secs_f64() allows sub-second precision)
        std::thread::sleep(std::time::Duration::from_millis(50));
        sm.transition(TargetState::Booting);
        assert_eq!(sm.current(), TargetState::Booting);

        // First check requests a probe — silence alone must NOT declare DUT-off.
        sm.check_hang();
        assert_eq!(sm.current(), TargetState::Booting);
        assert!(
            sm.heartbeat_pending,
            "silence must request a probe before any hang verdict"
        );

        // Unanswered probe rounds accumulate until hysteresis → DUT-off.
        // Each round models the real watchdog flow: the probe is sent
        // (mark_probe_sent), then the window expires with no response.
        for round in 1..=2 {
            sm.mark_probe_sent();
            sm.last_data_time = std::time::Instant::now() - std::time::Duration::from_secs(10);
            sm.last_probe_time = std::time::Instant::now() - std::time::Duration::from_secs(6);
            sm.check_hang();
            if round < 2 {
                assert_eq!(sm.current(), TargetState::Booting);
            }
        }
        assert_eq!(sm.current(), TargetState::DutOff);
    }

    #[test]
    fn test_booting_silence_requests_probe_not_dutoff() {
        let (mut sm, _tmp) = create_test_state_manager(0, 2); // 0s timeout, hysteresis 2

        std::thread::sleep(std::time::Duration::from_millis(50));
        sm.transition(TargetState::Booting);

        // First check after silence: probe requested, NOT a hang verdict.
        sm.check_hang();
        assert_eq!(sm.current(), TargetState::Booting);
        assert!(
            sm.heartbeat_pending,
            "first silence check must request a probe"
        );
        assert_eq!(sm.heartbeat_missed, 0, "no miss before a probe was sent");
    }

    #[test]
    fn test_booting_probe_response_resets_cycle() {
        let (mut sm, _tmp) = create_test_state_manager(0, 3);

        sm.transition(TargetState::Booting);
        // One unanswered probe round: probe sent, window expired, no data.
        sm.mark_probe_sent();
        sm.last_data_time = std::time::Instant::now() - std::time::Duration::from_secs(10);
        sm.last_probe_time = std::time::Instant::now() - std::time::Duration::from_secs(6);
        sm.check_hang();
        assert_eq!(sm.heartbeat_missed, 1);

        // ...then the DUT answers: the whole probe cycle resets.
        sm.mark_probe_sent();
        sm.on_activity();
        assert_eq!(sm.heartbeat_missed, 0);
        assert!(!sm.heartbeat_pending);

        // Repeated checks with a live DUT never transition.
        for _ in 0..5 {
            sm.check_hang();
            assert_eq!(sm.current(), TargetState::Booting);
        }
        assert_eq!(sm.heartbeat_missed, 0);
    }

    #[test]
    fn test_booting_unanswered_probe_rounds_declare_dutoff() {
        let (mut sm, _tmp) = create_test_state_manager(0, 2); // hysteresis = 2

        sm.transition(TargetState::Booting);

        // Round 1: watchdog sends the probe; window expires unanswered → miss 1.
        sm.mark_probe_sent();
        sm.last_data_time = std::time::Instant::now() - std::time::Duration::from_secs(10);
        sm.last_probe_time = std::time::Instant::now() - std::time::Duration::from_secs(6);
        sm.check_hang();
        assert_eq!(sm.current(), TargetState::Booting);
        assert_eq!(sm.heartbeat_missed, 1);
        assert!(
            sm.heartbeat_pending,
            "miss below hysteresis must re-arm the probe"
        );

        // Round 2: watchdog sends the re-armed probe; still unanswered → miss 2 → DUT-off.
        sm.mark_probe_sent();
        sm.last_data_time = std::time::Instant::now() - std::time::Duration::from_secs(10);
        sm.last_probe_time = std::time::Instant::now() - std::time::Duration::from_secs(6);
        sm.check_hang();
        assert_eq!(sm.current(), TargetState::DutOff);
        assert_eq!(sm.heartbeat_missed, 2);
    }

    // ── Boot window: Booting must never outlive the configured budget ─────

    /// The operator-facing contract: boards finish booting inside the
    /// window, so once it elapses the state leaves `booting` for `active`
    /// even when the console never showed a shell (quiet kernel cmdline, a
    /// desktop image without serial getty). Active-state hang and
    /// heartbeat detection takes over from there.
    #[test]
    fn booting_expires_to_active_after_the_window() {
        let (mut sm, _tmp) = create_test_state_manager(60, 3);
        sm.transition(TargetState::Active);
        sm.transition(TargetState::Booting);
        assert_eq!(sm.current(), TargetState::Booting);
        // The boot episode started 31s ago; the default window is 30s.
        sm.boot_started_at = Some(Instant::now() - std::time::Duration::from_secs(31));

        sm.check_hang();

        assert_eq!(sm.current(), TargetState::Active);
        assert_eq!(sm.boot_started_at, None, "window cleared on expiry");
    }

    /// Inside the window Booting is untouched — a normal boot (which the
    /// detector confirms mid-window on a talkative console) must never be
    /// cut short by the deadline.
    #[test]
    fn booting_survives_inside_the_window() {
        let (mut sm, _tmp) = create_test_state_manager(60, 3);
        sm.transition(TargetState::Booting);
        sm.boot_started_at = Some(Instant::now() - std::time::Duration::from_secs(5));

        sm.check_hang();

        assert_eq!(sm.current(), TargetState::Booting);
    }

    /// Every entry into Booting starts a FRESH window: learn cycles and
    /// real reboots must not inherit a half-spent deadline from the
    /// previous boot, and leaving Booting clears the timer entirely.
    #[test]
    fn boot_window_resets_per_boot_episode() {
        let (mut sm, _tmp) = create_test_state_manager(60, 3);
        sm.transition(TargetState::Booting);
        // First episode half-spent (25s of a 30s window), then the board
        // completes and later boots again.
        sm.boot_started_at = Some(Instant::now() - std::time::Duration::from_secs(25));
        sm.transition(TargetState::Active);
        assert_eq!(
            sm.boot_started_at, None,
            "leaving Booting clears the window"
        );

        sm.transition(TargetState::Booting);
        let started = sm.boot_started_at.expect("fresh window on boot entry");
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn test_probe_rearms_after_miss() {
        let (mut sm, _tmp) = create_test_state_manager(0, 3); // hysteresis = 3

        sm.transition(TargetState::Booting);
        sm.mark_probe_sent();
        sm.last_data_time = std::time::Instant::now() - std::time::Duration::from_secs(10);
        sm.last_probe_time = std::time::Instant::now() - std::time::Duration::from_secs(6);
        sm.check_hang();
        assert_eq!(sm.heartbeat_missed, 1);
        assert!(
            sm.heartbeat_pending,
            "miss below hysteresis re-arms the probe"
        );

        // The watchdog sends the re-armed probe; pending is cleared.
        sm.mark_probe_sent();
        assert!(!sm.heartbeat_pending);
        assert_eq!(sm.current(), TargetState::Booting);
    }

    /// Regression for the phantom "miss #1" seen in production (mcp.log):
    /// the deployed pre-rework binary treated the DUT's probe
    /// ANSWER as an unanswered probe one hang_timeout later, scoring a miss
    /// against a healthy, responding DUT. After `on_activity` credits the
    /// response, the probe is no longer in flight — the next timeout must
    /// request a FRESH probe and must never count a miss.
    #[test]
    fn test_probe_response_after_mark_never_counts_miss() {
        let (mut sm, _tmp) = create_test_state_manager(60, 3);

        sm.transition(TargetState::Active);
        // Watchdog sends a probe, the DUT answers: on_activity stamps both
        // timestamps with the same `now` (the response time).
        sm.mark_probe_sent();
        sm.on_activity();
        assert!(
            !(sm.last_probe_time > sm.last_data_time),
            "a response must clear the in-flight probe"
        );

        // hang_timeout (60s) passes with no further data.
        let aged = std::time::Instant::now() - std::time::Duration::from_secs(61);
        sm.last_data_time = aged;
        sm.last_probe_time = aged;
        sm.check_hang();

        assert_eq!(
            sm.heartbeat_missed, 0,
            "the answered probe must never be scored as a miss"
        );
        assert!(
            sm.heartbeat_pending,
            "silence after a credited response must request a fresh probe"
        );
        assert_eq!(sm.current(), TargetState::Active);
    }

    /// A DUT-off reached through a REAL hang (unanswered probe rounds) must
    /// report the actual number of probe attempts in the alert.
    #[test]
    fn test_dutoff_notification_after_hang_mentions_probe_count() {
        let (mut sm, tmp) = create_test_state_manager(0, 2); // hysteresis = 2

        sm.transition(TargetState::Booting);
        // Drive a real hang: probe requested, sent, unanswered twice → DUT-off.
        sm.last_data_time = std::time::Instant::now() - std::time::Duration::from_secs(10);
        sm.last_probe_time = sm.last_data_time;
        sm.check_hang(); // requests probe #1
        sm.mark_probe_sent();
        sm.last_data_time = std::time::Instant::now() - std::time::Duration::from_secs(10);
        sm.last_probe_time = std::time::Instant::now() - std::time::Duration::from_secs(6);
        sm.check_hang(); // miss 1
        sm.mark_probe_sent(); // probe #2
        sm.last_data_time = std::time::Instant::now() - std::time::Duration::from_secs(10);
        sm.last_probe_time = std::time::Instant::now() - std::time::Duration::from_secs(6);
        sm.check_hang(); // miss 2 → DUT-off
        assert_eq!(sm.current(), TargetState::DutOff);

        let notify_dir = tmp.path().join(".dut-serial/notifications");
        let mut found = false;
        for entry in std::fs::read_dir(&notify_dir).unwrap() {
            let path = entry.unwrap().path();
            let fname = path.file_name().unwrap().to_string_lossy().to_string();
            if !fname.contains("DUT-off") {
                continue;
            }
            let content = std::fs::read_to_string(&path).unwrap();
            assert!(
                content.contains("2 probe attempts"),
                "DUT-off alert must count the actual probe rounds: {content}"
            );
            found = true;
        }
        assert!(found, "no DUT-off notification written");
    }

    /// Non-hang DUT-off transitions (user poweroff, initial probe, command
    /// timeout) run zero probe rounds — the alert must not invent a count.
    #[test]
    fn test_dutoff_notification_without_hang_uses_neutral_wording() {
        let (mut sm, tmp) = create_test_state_manager(60, 3);

        sm.transition(TargetState::DutOff);
        let notify_dir = tmp.path().join(".dut-serial/notifications");
        let mut found = false;
        for entry in std::fs::read_dir(&notify_dir).unwrap() {
            let path = entry.unwrap().path();
            let fname = path.file_name().unwrap().to_string_lossy().to_string();
            if !fname.contains("DUT-off") {
                continue;
            }
            let content = std::fs::read_to_string(&path).unwrap();
            assert!(
                !content.contains("probe attempts"),
                "non-hang DUT-off alert must not mention probe attempts: {content}"
            );
            assert!(
                content.contains("no serial output"),
                "non-hang DUT-off alert keeps the neutral wording: {content}"
            );
            found = true;
        }
        assert!(found, "no DUT-off notification written");
    }

    /// F1: the first watchdog check after silence begins must REQUEST a
    /// probe, not count a miss — a miss requires a probe actually in flight.
    #[test]
    fn test_silence_without_probe_counts_no_miss() {
        let (mut sm, _tmp) = create_test_state_manager(0, 2);

        sm.transition(TargetState::Booting);

        // Long silence: both timestamps age together (no probe has been sent),
        // even past the probe response window.
        sm.last_data_time = std::time::Instant::now() - std::time::Duration::from_secs(10);
        sm.last_probe_time = sm.last_data_time;
        sm.check_hang();

        assert_eq!(sm.current(), TargetState::Booting);
        assert_eq!(sm.heartbeat_missed, 0, "no miss before a probe was sent");
        assert!(sm.heartbeat_pending, "silence must request a probe");
    }

    /// F2: with `hang_timeout` BELOW the probe response window, the watchdog
    /// must wait out the window once a probe is in flight — not re-send a
    /// probe every 2s tick forever. Termination is bounded for every
    /// hang_timeout.
    #[test]
    fn test_small_hang_timeout_below_probe_window_still_declares_dutoff() {
        // hang_timeout (2s) < probe response window (5s).
        let (mut sm, _tmp) = create_test_state_manager(2, 3);

        sm.transition(TargetState::Booting);
        // Silence for 10s so the first check is already past the timeout.
        sm.last_data_time = std::time::Instant::now() - std::time::Duration::from_secs(10);
        sm.last_probe_time = sm.last_data_time;

        // Simulate the 2s watchdog loop: check, send if pending, advance 2s.
        let mut iterations = 0;
        loop {
            sm.check_hang();
            if sm.current() == TargetState::DutOff {
                break;
            }
            if sm.heartbeat_pending {
                sm.mark_probe_sent(); // watchdog sends the blank-line probe
            }
            // Advance both clocks by one watchdog tick.
            sm.last_data_time -= std::time::Duration::from_secs(2);
            sm.last_probe_time -= std::time::Duration::from_secs(2);
            iterations += 1;
            assert!(
                iterations < 30,
                "hang must be declared within bounded time, iterations={iterations}"
            );
        }
        assert_eq!(sm.current(), TargetState::DutOff);
        assert_eq!(sm.heartbeat_missed, 3, "all three probes went unanswered");
    }

    #[test]
    fn test_activity_resets_hang() {
        let (mut sm, _tmp) = create_test_state_manager(0, 3);

        sm.transition(TargetState::Booting);
        // One unanswered probe round: probe sent, window expired, no data.
        sm.mark_probe_sent();
        sm.last_data_time = std::time::Instant::now() - std::time::Duration::from_secs(10);
        sm.last_probe_time = std::time::Instant::now() - std::time::Duration::from_secs(6);
        sm.check_hang();
        assert_eq!(sm.heartbeat_missed, 1);

        // Activity resets the probe-miss cycle.
        sm.on_activity();
        assert_eq!(sm.heartbeat_missed, 0);
        assert!(!sm.heartbeat_pending);
        sm.check_hang();
        assert_eq!(sm.heartbeat_missed, 0);

        // Should not transition (activity reset the accumulation).
        assert_eq!(sm.current(), TargetState::Booting);
    }

    #[test]
    fn test_pid_file_write() {
        let tmp = TempDir::new().unwrap();
        let sm = StateManager::new(tmp.path(), 60, 3, ".dut-serial", "", 30);
        // PID not written in new() anymore — only after lock
        let pid_file = tmp.path().join(".dut-serial/mcp.pid");
        assert!(!pid_file.exists());
        // Now write it
        sm.write_pid(tmp.path(), ".dut-serial");
        assert!(pid_file.exists());
        let pid: u32 = std::fs::read_to_string(&pid_file).unwrap().parse().unwrap();
        assert_eq!(pid, std::process::id());
        sm.remove_pid(tmp.path(), ".dut-serial");
        assert!(!pid_file.exists());
    }

    #[test]
    fn test_target_state_as_str() {
        assert_eq!(TargetState::Stopped.as_str(), "stopped");
        assert_eq!(TargetState::Active.as_str(), "active");
        assert_eq!(TargetState::Booting.as_str(), "booting");
        assert_eq!(TargetState::UBoot.as_str(), "uboot");
        assert_eq!(TargetState::Crashed.as_str(), "crashed");
        assert_eq!(TargetState::DutOff.as_str(), "DUT-off");
        assert_eq!(TargetState::Disconnected.as_str(), "disconnected");
    }

    #[test]
    fn test_hang_candidate() {
        assert!(!TargetState::Stopped.is_hang_candidate());
        assert!(TargetState::Active.is_hang_candidate()); // heartbeat probe
        assert!(TargetState::Booting.is_hang_candidate());
        assert!(!TargetState::UBoot.is_hang_candidate());
        assert!(!TargetState::Crashed.is_hang_candidate());
        assert!(!TargetState::DutOff.is_hang_candidate());
        assert!(!TargetState::Disconnected.is_hang_candidate());
        assert!(TargetState::Dutabo.is_hang_candidate()); // Dutabo: keep watchdog alive
    }

    #[test]
    fn test_atomic_write() {
        let (sm, tmp) = create_test_state_manager(60, 3);
        let state_file = tmp.path().join(".dut-serial/target-state");

        sm.atomic_write(&state_file, "test-state");
        assert_eq!(std::fs::read_to_string(&state_file).unwrap(), "test-state");

        sm.atomic_write(&state_file, "another-state");
        assert_eq!(
            std::fs::read_to_string(&state_file).unwrap(),
            "another-state"
        );
    }

    /// R15/D10: the /dev/shm claude-status-* cache is write-only dead weight
    /// (no reader anywhere) — the StateManager must never write it. The
    /// project-local statusline-cache remains the single state file.
    #[test]
    fn test_statusline_cache_write() {
        let (mut sm, tmp) = create_test_state_manager(60, 3);
        let cache_file = tmp.path().join(".dut-serial/statusline-cache");
        let shm_file = std::path::PathBuf::from(format!(
            "/dev/shm/claude-status-{}",
            StateManager::project_hash(tmp.path())
        ));
        // Hermetic: residue from older versions must not mask the check.
        if std::path::Path::new("/dev/shm").is_dir() {
            let _ = std::fs::remove_file(&shm_file);
        }

        sm.transition(TargetState::Active);
        assert!(cache_file.exists());
        let content = std::fs::read_to_string(&cache_file).unwrap();
        assert!(content.contains("serial:active"));
        if std::path::Path::new("/dev/shm").is_dir() {
            assert!(
                !shm_file.exists(),
                "claude-status-* shm cache is deleted (D10): nothing may be written"
            );
        }

        sm.transition(TargetState::DutOff);
        let content = std::fs::read_to_string(&cache_file).unwrap();
        assert!(content.contains("serial:DUT-off"));
    }

    #[test]
    fn test_format_statusline_all_states_have_text() {
        let (sm, _tmp) = create_test_state_manager(60, 3);
        for state in &[
            TargetState::Active,
            TargetState::Booting,
            TargetState::UBoot,
            TargetState::Crashed,
            TargetState::Disconnected,
            TargetState::DutOff,
        ] {
            let text = sm.format_statusline(*state);
            assert!(!text.is_empty(), "{:?} should have statusline text", state);
        }
        // Stopped produces an empty string (no display)
        assert_eq!(sm.format_statusline(TargetState::Stopped), "");
    }

    #[test]
    fn test_same_state_no_transition() {
        let (mut sm, tmp) = create_test_state_manager(60, 3);
        let state_file = tmp.path().join(".dut-serial/target-state");

        sm.transition(TargetState::Active);
        let content1 = std::fs::read_to_string(&state_file).unwrap();

        // Transition to same state should not update file
        sm.transition(TargetState::Active);
        let content2 = std::fs::read_to_string(&state_file).unwrap();
        assert_eq!(content1, content2);
    }

    /// The on_state_change hook fires after EVERY real transition, AFTER the
    /// state files are written — so a hook that re-reads the file (the
    /// inventory refresh) always observes the new state. A no-op transition
    /// (same state) does not fire it.
    #[test]
    fn test_on_state_change_hook_fires_after_files_written() {
        let (mut sm, tmp) = create_test_state_manager(60, 3);
        let state_file = tmp.path().join(".dut-serial/target-state");
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let hook_seen = seen.clone();
        let hook_file = state_file.clone();
        sm.set_on_state_change(Box::new(move |state| {
            hook_seen.lock().unwrap().push((
                state,
                std::fs::read_to_string(&hook_file).unwrap_or_default(),
            ));
        }));

        sm.transition(TargetState::Active);
        sm.transition(TargetState::Crashed);
        sm.transition(TargetState::Crashed); // no-op — no hook call

        let calls = seen.lock().unwrap().clone();
        assert_eq!(calls.len(), 2, "no-op transitions must not fire the hook");
        assert_eq!(calls[0], (TargetState::Active, "active".to_string()));
        assert_eq!(calls[1], (TargetState::Crashed, "crashed".to_string()));
    }

    /// Stopped deletes the state file BEFORE the hook fires, so an
    /// inventory-refresh hook reads a missing file and renders "unknown" —
    /// never the pre-stop state.
    #[test]
    fn test_on_state_change_hook_fires_on_stopped_after_file_deletion() {
        let (mut sm, tmp) = create_test_state_manager(60, 3);
        let state_file = tmp.path().join(".dut-serial/target-state");
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let hook_seen = seen.clone();
        let hook_file = state_file.clone();
        sm.set_on_state_change(Box::new(move |state| {
            hook_seen
                .lock()
                .unwrap()
                .push((state, std::fs::read_to_string(&hook_file).is_ok()));
        }));

        sm.transition(TargetState::Active);
        sm.transition(TargetState::Stopped);

        let calls = seen.lock().unwrap().clone();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].0, TargetState::Stopped);
        assert!(
            !calls[1].1,
            "state file must be deleted before the hook observes it"
        );
    }

    #[test]
    fn test_disconnected_state() {
        let (mut sm, _tmp) = create_test_state_manager(60, 3);

        sm.transition(TargetState::Disconnected);
        assert_eq!(sm.current(), TargetState::Disconnected);
        assert_eq!(sm.external_state(), Some(TargetState::Disconnected));
    }

    #[test]
    fn test_format_statusline_has_ansi_colors() {
        let sm = StateManager::new(
            &std::env::temp_dir(),
            60,
            3,
            ".dut-serial",
            "test-board",
            30,
        );
        let text = sm.format_statusline(TargetState::Active);
        assert!(
            text.contains("\x1b[32m"),
            "Active state must have green ANSI code, got: {:?}",
            text
        );
        assert!(text.contains("\x1b[0m"), "Must have ANSI reset code");
        assert!(
            text.contains("test-board"),
            "Must use DUT name 'test-board'"
        );

        let crash = sm.format_statusline(TargetState::Crashed);
        assert!(
            crash.contains("\x1b[31m"),
            "Crashed state must have red ANSI code"
        );
        assert!(crash.contains("\x1b[0m"), "Must have ANSI reset code");

        let dutoff = sm.format_statusline(TargetState::DutOff);
        assert!(
            dutoff.contains("\x1b[31m"),
            "DUT-off state must have red ANSI code"
        );
        assert!(dutoff.contains("\x1b[0m"), "Must have ANSI reset code");

        // "default" name should use "serial" as label (backward compat)
        let sm_default =
            StateManager::new(&std::env::temp_dir(), 60, 3, ".dut-serial", "default", 30);
        let text2 = sm_default.format_statusline(TargetState::Active);
        assert!(
            text2.contains("serial"),
            "Default name must use 'serial' label"
        );

        // Empty name should also use "serial"
        let sm_empty = StateManager::new(&std::env::temp_dir(), 60, 3, ".dut-serial", "", 30);
        let text3 = sm_empty.format_statusline(TargetState::Active);
        assert!(
            text3.contains("serial"),
            "Empty name must use 'serial' label"
        );
    }
}
