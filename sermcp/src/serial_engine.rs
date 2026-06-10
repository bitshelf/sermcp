//! SerialEngine — central orchestrator for the MCP serial debug system.
//!
//! # Architecture
//!
//! Owns every subsystem as a field. All MCP tool calls and dutabo commands
//! flow through a single `Arc<Mutex<SerialEngine>>` (type alias `SharedEngine`).
//!
//! ```text
//! ┌──────────────────────────────────────────────────┐
//! │ SerialEngine                                     │
//! │  ├─ console:  SerialConsoleDriver  (TCP ↔ ser2net)
//! │  ├─ detector: BootStageDetector    (regex + learner)
//! │  ├─ state:    StateManager         (3-tier state machine)
//! │  ├─ logs:     LogManager           (ring buffer + writer thread)
//! │  ├─ commands: CommandQueue         (marker-echo pattern)
//! │  ├─ power:    dut-ctrl extension selected by .target.jsonc
//! │  └─ reconnect: ReconnectManager    (deferred backoff)
//! └──────────────────────────────────────────────────┘
//! ```
//!
//! # Lifecycle
//!
//! 1. `start()` — serial endpoint lock → console.connect()
//!    → probe_initial_state() → write PID → launch read loop + watchdog
//! 2. `read_loop_iter()` — called ~100x/sec by HTTP read task. Drains writes,
//!    handles reconnects, reads serial data, feeds detector + command queue.
//!    **Lock is NOT held during backoff sleeps** (see `reconnect_after`).
//! 3. `stop()` — abort background tasks, close console/relay, release locks.
//!
//! # State flow
//!
//! ```text
//! Stopped → Connecting → Active ←→ Booting ↔ UBoot
//!                 ↓          ↓         ↓
//!            Disconnected  Crashed   DUT-off
//!                             ↓
//!                          (watchdog reboot) → Booting → ...
//! ```
//!
//! # Disconnected handling
//!
//! 1. **Fast retry**: immediate reconnect (2s timeout). Success → user never sees it.
//! 2. **Ping-pong guard**: skip fast retry if last attempt was < 10s ago.
//! 3. **Deferred backoff**: backoff sleep happens OUTSIDE the engine Mutex
//!    via `reconnect_after`. HTTP requests are never blocked by reconnect waits.
//!
//! # Engine lock policy
//!
//! - `Arc<Mutex<SerialEngine>>` serialises all access.
//! - Read loop acquires lock each iteration (~100 Hz), does work, drops lock.
//! - Backoff sleeps (1s–30s) are deferred: a timestamp is set, the lock is
//!   released, and each read-loop iteration checks the deadline without
//!   holding the lock during the wait.
//! - Guards that must not contend with the read loop (MCP `call_tool`) read
//!   the lock-free `EngineFlags` mirror instead of `try_lock()`; only tool
//!   bodies queue on the mutex.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
use std::time::Duration;

use tokio::sync::Mutex;

use crate::boot_detector::{BootEvent, BootStageDetector};
use crate::command_queue::CommandQueue;
use crate::config::Config;
use crate::connection_learner::{
    ConnectionLearner, LearnConfig, LearnMethod, LearnResult, MAX_LEARN_LOG_FILE_BYTES,
    prune_learn_logs,
};
use crate::console::{ConsoleMode, SerialConsoleDriver};
use crate::error::SerialError;
use crate::lock_manager;
use crate::log_manager::LogManager;
use crate::power_control::{Button, ButtonChannels, PowerControl, PowerControlConfig};
use crate::reconnect::ReconnectManager;
use crate::state_manager::{StateManager, TargetState};

// ── Timing ─────────────────────────────────────────────────────────────────
//
// Instead of scattered magic numbers, every timeout is a named `Duration`
// constant that documents *what* it measures.  The expect-style poll loop
// (`send_and_expect`) returns as soon as data arrives so these are ceiling
// values, not fixed sleeps.
//
// Community reference: labgrid SerialDriver, pexpect expect().

/// Ceiling for send-and-read-response round-trip (TCP → ser2net → UART → back).
/// Typical LAN response is 20-80 ms.
const COMMAND_RESPONSE_CEILING: Duration = Duration::from_secs(30);
/// Probe poll interval used by send_and_expect — longer than EXPECT_POLL
/// so slow serial targets have time to respond.
const PROBE_POLL: Duration = Duration::from_millis(200);
/// Poll tick for the expect loop — how often we check for new serial data.
const EXPECT_POLL: Duration = Duration::from_millis(50);

/// U-Boot interrupt strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptStrategy {
    /// Continuously send the configured interrupt character.
    Flood,
    /// Wait for a configured serial-output regex, then interrupt once.
    Pattern,
}

impl InterruptStrategy {
    /// Parse the two supported values exactly. Callers use `flood` only
    /// when the setting is absent, never to accept old aliases.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "" => None,
            "flood" => Some(Self::Flood),
            "pattern" => Some(Self::Pattern),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Flood => "flood",
            Self::Pattern => "pattern",
        }
    }
}

pub struct SerialEngine {
    pub console: SerialConsoleDriver,
    pub detector: BootStageDetector,
    pub state: StateManager,
    pub logs: LogManager,
    pub commands: CommandQueue,
    pub config: Config,
    running: bool,
    /// Set after cleanup so repeated HTTP shutdown paths are true no-ops.
    stopped: bool,
    read_handle: Option<tokio::task::JoinHandle<()>>,
    watchdog_handle: Option<tokio::task::JoinHandle<()>>,
    host: String,
    serial_target: String,
    login_user: String,
    login_pass: String,
    /// Auto-login attempt counter for the current boot cycle. The first
    /// prompt sends the username plainly; a SECOND prompt (getty re-prints
    /// after a failed login) retries with tx_repeat=2 — links that drop
    /// alternating TX bytes (WCH dual-serial CDC adapters) are rescued
    /// without any configuration. Confirmed by a shell prompt → sticky.
    login_attempts: u32,
    /// Debounce for LoginPrompt events: getty re-prints and our own echoed
    /// username can re-match the login regex within one prompt instance.
    last_login_send: Option<std::time::Instant>,
    pub interrupt_char: u8,
    /// File position tracking for poll_logs
    poll_position: u64,
    /// Reconnection manager with exponential backoff.
    reconnect: ReconnectManager,
    /// WebSocket broadcast: serial output relayed to dutabo clients.
    /// Set when a WebSocket session is active, cleared on disconnect.
    ws_tx: Option<tokio::sync::broadcast::Sender<Vec<u8>>>,
    /// Passive monitor feed: ALWAYS present; WebSocket observers (e.g.
    /// the init TUI during TEST) subscribe without claiming the manual
    /// session — serial tools keep working while they watch.
    monitor_tx: tokio::sync::broadcast::Sender<Vec<u8>>,
    /// Timestamp of the last fast-retry attempt. Used to prevent ping-pong:
    /// if we fast-retried within the last 10s and the connection dropped again,
    /// skip the fast retry and go straight to backoff.
    last_fast_retry: Option<std::time::Instant>,
    /// Earliest time to attempt the next reconnection. Set by the backoff logic
    /// so the sleep happens OUTSIDE the engine lock: the read loop releases
    /// the lock and only reconnects once this deadline elapses.
    /// `None` means reconnect is allowed now.
    reconnect_after: Option<std::time::Instant>,
    /// Structured startup/configuration failure surfaced by serial_get_state.
    /// This deliberately lives outside TargetState: a wrong endpoint is not a
    /// physical DUT power state.
    serial_error: Option<serde_json::Value>,
    /// Metadata advertised by ser2net on connect, when available.
    ser2net_banner: Option<Ser2netBanner>,
    /// Trailing incomplete line held while filtering the ser2net banner.
    /// This is per engine/DUT; parser state must never leak across engines.
    ser2net_partial: Vec<u8>,
    /// True only after this process atomically acquires the host:port lock.
    /// Non-owner MCP instances must not release the lock or publish `stopped`.
    owns_serial_lock: bool,
    /// Lock-free ownership mirror for the MCP tool guard (see [`EngineFlags`]).
    /// Cloned out of the engine once by `new_shared_engine` so `call_tool`
    /// can read it without contending for the engine mutex.
    pub flags: Arc<EngineFlags>,
}

#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
struct Ser2netBanner {
    port: String,
    device: String,
    baudrate: Option<u32>,
}

/// RAII holder for the relay endpoint lock acquired by the learn cycle:
/// released on every exit path (early error, capture done, panic unwind)
/// without manual bookkeeping.
struct RelayLockGuard {
    host: String,
    port: String,
    lock_dir: String,
}

impl Drop for RelayLockGuard {
    fn drop(&mut self) {
        crate::lock_manager::release_lock(&self.host, &self.port, &self.lock_dir);
    }
}

impl SerialEngine {
    pub fn new(config: Config) -> Self {
        let project_dir = config
            .project_dir
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap());
        let host = config.dev_host_ip();
        let serial_target = config.serial_target();
        let dut_dir = config.dut_dir();
        // Extract all needed config values (before `config` is moved)
        let login_user = config.login_user();
        let login_pass = config.login_pass();
        let login_prompt = config.login_prompt();
        let interrupt_char = config.uboot_interrupt_char();

        let mut detector = BootStageDetector::new();
        if !login_prompt.is_empty() {
            detector.set_login_regex(&login_prompt);
        }
        // Apply custom crash keywords from .target.jsonc
        let crash_keywords = config.crash_patterns();
        if !crash_keywords.is_empty() {
            detector.set_crash_keywords(crash_keywords);
        }

        let serial_error = endpoint_conflict(&config);
        let console_mode = if config.serial_rfc2217() {
            ConsoleMode::Rfc2217
        } else {
            ConsoleMode::Raw
        };
        Self {
            console: SerialConsoleDriver::new_with_transport(
                host.clone(),
                serial_target.clone(),
                console_mode,
                config.serial_tx_repeat(),
                config.serial_tx_gap(),
            ),
            detector,
            state: StateManager::new(
                &project_dir,
                config.hang_timeout(),
                config.hang_hysteresis(),
                &dut_dir,
                config.get("DUT_NAME"),
                config.boot_window_secs(),
            ),
            logs: LogManager::new(
                &project_dir,
                config.max_archived_logs(),
                config.max_log_file_size_mb(),
                &dut_dir,
            ),
            commands: CommandQueue::new(),
            config,
            running: false,
            stopped: false,
            read_handle: None,
            watchdog_handle: None,
            host,
            serial_target,
            login_user,
            login_pass,
            login_attempts: 0,
            last_login_send: None,
            interrupt_char,
            poll_position: 0,
            reconnect: ReconnectManager::new(),
            ws_tx: None,
            monitor_tx: tokio::sync::broadcast::channel(256).0,
            last_fast_retry: None,
            reconnect_after: None,
            serial_error,
            ser2net_banner: None,
            ser2net_partial: Vec::new(),
            owns_serial_lock: false,
            flags: Arc::new(EngineFlags {
                // Engine not started yet: MCP tools observe the connecting
                // window until start_shared finishes the startup probe.
                startup_in_progress: AtomicBool::new(true),
                dutabo_taken: AtomicU8::new(0),
                owns_serial_lock: AtomicBool::new(false),
                serial_owner_pid: AtomicU32::new(0),
            }),
        }
    }

    /// Install an on-transition hook on the internal StateManager (invoked
    /// after every real transition, state files already written/deleted).
    /// Used by the server binary to refresh `.dut-serial/inventory.json` on
    /// every state change so inventory consumers never render stale state.
    pub fn set_state_change_hook(&mut self, hook: impl Fn(TargetState) + Send + 'static) {
        self.state.set_on_state_change(hook);
    }

    /// Start the engine: acquire the serial lock → open the serial port → spawn background tasks
    #[tracing::instrument(skip(self), fields(host, target))]
    pub async fn start(&mut self) -> Result<(), String> {
        self.stopped = false;
        tracing::Span::current().record("host", &self.host);
        tracing::Span::current().record("target", &self.serial_target);
        let project_dir = self
            .config
            .project_dir
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap());
        let dut_dir = self.config.dut_dir();

        // A TCP endpoint cannot simultaneously be the serial console and the
        // relay controller. Keep the MCP alive so agents can inspect the
        // structured error instead of losing every tool during initialize.
        if let Some(error) = &self.serial_error {
            tracing::error!(error = %error, "[AGENT-NOTIFY] Serial endpoint configuration error");
            self.running = true;
            return Ok(());
        }

        // 0. Validate config — catch common misconfigurations early
        {
            let mut issues: Vec<String> = Vec::new();
            if self.config.dev_host_ip().is_empty() {
                issues.push("dev_host.ip not set".into());
            }
            let port = self.config.serial_target();
            if port.is_empty() || port == "0" {
                issues.push("serial.port not set".into());
            }
            if self.config.login_user().is_empty() {
                tracing::warn!("[AGENT-NOTIFY] login_user not set — auto-login disabled");
            }
            let ref_log = self.config.reference_log();
            if !ref_log.is_empty() {
                let ref_path = std::path::PathBuf::from(&ref_log);
                let abs_path = if ref_path.is_relative() {
                    self.config
                        .project_dir
                        .as_ref()
                        .map(|p| p.join(&ref_path))
                        .unwrap_or(ref_path)
                } else {
                    ref_path
                };
                if !abs_path.exists() {
                    tracing::warn!(
                        "[AGENT-NOTIFY] reference_log '{}' not found. \
                         Run serial_reset(wait_boot=true) to auto-capture.",
                        abs_path.display()
                    );
                }
            }
            if !issues.is_empty() {
                for issue in &issues {
                    tracing::error!("[AGENT-NOTIFY] Config error: {issue}");
                }
                return Err(format!(
                    "Config errors: {}. Fix .target.jsonc.",
                    issues.join("; ")
                ));
            }
        }

        // 1. Acquire the serial mutex
        let lock_dir = self.config.lock_dir();
        if let Some(conflicting_pid) =
            lock_manager::acquire_lock(&self.host, &self.serial_target, &lock_dir)
        {
            self.serial_error = Some(serde_json::json!({
                "code": "serial_endpoint_busy",
                "message": format!(
                    "Target {}:{} is already in use by another MCP session",
                    self.host, self.serial_target
                ),
                "configured": { "host": self.host, "port": self.serial_target },
                "owner_pid": conflicting_pid,
                "hint": "Stop the process outside this project, then call serial_claim; live sessions are never terminated automatically",
            }));
            self.flags.owns_serial_lock.store(false, Ordering::Relaxed);
            self.flags
                .serial_owner_pid
                .store(conflicting_pid, Ordering::Relaxed);
            self.running = true;
            return Ok(());
        }
        self.owns_serial_lock = true;
        self.flags.owns_serial_lock.store(true, Ordering::Relaxed);
        self.flags.serial_owner_pid.store(0, Ordering::Relaxed);

        // 2. Write the PID file
        self.state.write_pid(&project_dir, &dut_dir);

        // 3. Set write_fn for CommandQueue
        let write_tx = self.console.write_sender();
        self.commands.set_write_fn(Box::new(move |data| {
            write_tx.try_send_command(data.to_vec()).ok();
        }));

        // 4. Ensure the log file is open (no rotation — the board may have been running for a while)
        self.logs.ensure_current_file();

        // 5. Connect the serial port + probe the initial state
        match self.console.connect().await {
            Ok(()) => {
                tracing::info!("Serial connected to {}:{}", self.host, self.serial_target);
                // Terminal stays in getty defaults (echo on, icanon, …) — same
                // behaviour as PuTTY serial. The command queue handles echo.
                self.probe_initial_state().await;
            }
            Err(e) => {
                tracing::warn!("Cannot open serial: {e}");
                self.state.transition(TargetState::Disconnected);
            }
        }

        // 6. Auto-load reference boot log if configured (StageLearner adaptive mode)
        let ref_log = self.config.reference_log();
        if !ref_log.is_empty() {
            let ref_path = std::path::PathBuf::from(&ref_log);
            if ref_path.exists() {
                let st = self.config.learner_stage_threshold();
                let ct = self.config.learner_crash_threshold();
                match self.detector.load_reference(&ref_path, st, ct) {
                    Ok(()) => {
                        tracing::info!("Auto-loaded reference log: {}", ref_log);
                    }
                    Err(e) => {
                        tracing::warn!("Failed to load reference log {}: {e}", ref_log);
                    }
                }
            } else {
                tracing::warn!(
                    "[AGENT-NOTIFY] Reference boot log missing: {}. \
                     The StageLearner needs a reference boot log for accurate stage detection. \
                     Run: serial_reset(wait_boot=true) to auto-capture one.",
                    ref_log
                );
            }
        }

        // 7. Mark running (background tasks are spawned by mcp/mcp_http and
        //    registered via set_background_tasks) — must be set before any
        //    potentially blocking operation so the MCP responds promptly.
        self.running = true;

        tracing::info!(
            "[{}:{}] SerialEngine started",
            self.host,
            self.serial_target
        );

        Ok(())
    }

    pub fn owns_serial_lock(&self) -> bool {
        self.owns_serial_lock
    }

    /// Claim serial ownership (used by the serial_claim MCP tool).
    ///
    /// Relay endpoint mutex (multi-agent isolation): the relay is physical
    /// power/reset control — two agents pulsing concurrently would reboot
    /// each other's DUT. Same lock dir as the serial lock, different key
    /// prefix (`relay:<ip>`); the lock is held only for the single pulse and
    /// conflicts fail immediately.
    pub fn try_acquire_relay_lock(&self) -> Result<(), serde_json::Value> {
        let host = self.config.relay_ip();
        let port = self.config.relay_port().to_string();
        if let Some(owner_pid) =
            lock_manager::acquire_lock(&format!("relay:{host}"), &port, &self.config.lock_dir())
        {
            return Err(serde_json::json!({
                "success": false,
                "code": "relay_endpoint_busy",
                "owner_pid": owner_pid,
                "error": format!("Relay endpoint {host}:{port} is held by another live MCP process ({owner_pid})"),
                "hint": "Another agent's server controls this DUT's relay; wait, or use serial_verify_relay to inspect",
            }));
        }
        Ok(())
    }

    /// Release the relay endpoint lock (only when this process holds it).
    pub fn release_relay_lock(&self) {
        let host = self.config.relay_ip();
        let port = self.config.relay_port().to_string();
        lock_manager::release_lock(&format!("relay:{host}"), &port, &self.config.lock_dir());
    }

    /// Claim serial ownership (used by the serial_claim MCP tool).
    pub async fn claim_serial(&mut self) -> serde_json::Value {
        self.stopped = false;
        let lock_dir = self.config.lock_dir();
        if self.serial_error.as_ref().and_then(|e| e["code"].as_str())
            == Some("serial_relay_endpoint_conflict")
        {
            return serde_json::json!({
                "success": false,
                "code": "serial_relay_endpoint_conflict",
                "error": "Refusing to claim an endpoint also configured for relay control",
            });
        }
        if !self.owns_serial_lock {
            if let Some(conflicting) =
                lock_manager::acquire_lock(&self.host, &self.serial_target, &lock_dir)
            {
                self.flags
                    .serial_owner_pid
                    .store(conflicting, Ordering::Relaxed);
                return serde_json::json!({
                    "success": false,
                    "code": "serial_endpoint_busy",
                    "owner_pid": conflicting,
                    "error": format!("Serial lock is held by live PID {conflicting}")
                });
            }
            self.owns_serial_lock = true;
            self.flags.owns_serial_lock.store(true, Ordering::Relaxed);
            self.flags.serial_owner_pid.store(0, Ordering::Relaxed);

            // Install the command-queue write path. A server that started
            // under a conflicting lock returned early from connect() BEFORE
            // the startup path installed it (deliberately — a non-owner must
            // never write to the serial line), so the claim — the designed
            // recovery from exactly that state — has to install it here, or
            // every subsequent serial_send_command is silently dropped
            // ("Command cancelled").
            let write_tx = self.console.write_sender();
            self.commands.set_write_fn(Box::new(move |data| {
                write_tx.try_send_command(data.to_vec()).ok();
            }));
        }
        // Write the new mcp.pid
        let project_dir = self
            .config
            .project_dir
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap());
        self.state.write_pid(&project_dir, &self.config.dut_dir());
        self.serial_error = None;
        // Reconnect the serial console
        match self.console.connect().await {
            Ok(()) => {
                self.probe_initial_state().await;
                self.running = true;
                serde_json::json!({"success": true, "message": "Serial claimed"})
            }
            Err(e) => {
                serde_json::json!({"success": false, "error": format!("{e}")})
            }
        }
    }

    /// Human direct preemption (the SIGUSR1 `dutabo serial` sends before
    /// connecting directly): yield serial ownership.
    ///
    /// Rule: `dutabo serial` must be able to reach the DUT in ANY state —
    /// when an MCP holds the endpoint, it first asks this engine to yield.
    /// Yield = close the connection + release the endpoint lock + transition
    /// to dutabo (visible to every display); the engine and the HTTP server
    /// stay alive and wait for SIGUSR2 to resume. A non-owner receiving the
    /// signal is a no-op (never releases someone else's lock).
    pub async fn yield_serial_for_human(&mut self) {
        // The lock file is the authority dutabo consulted when it sent the
        // signal, so it also arbitrates here: a mirrored flag that went stale
        // while the file still names this engine must heal and yield, not
        // wedge the takeover. A file naming someone else is a hard no-op —
        // never release another process's lock.
        let lock_path = lock_manager::endpoint_lock_path(
            &self.host,
            &self.serial_target,
            &self.config.lock_dir(),
        );
        if !self.owns_serial_lock
            && lock_manager::endpoint_lock_owner(&lock_path) != Some(std::process::id())
        {
            tracing::warn!(
                "[{}:{}] HUMAN-TAKEOVER requested but this engine does not own the endpoint",
                self.host,
                self.serial_target
            );
            return;
        }
        tracing::warn!(
            "[{}:{}] HUMAN-TAKEOVER: yielding serial endpoint to dutabo",
            self.host,
            self.serial_target
        );
        if let Some(h) = self.read_handle.take() {
            h.abort();
        }
        if let Some(h) = self.watchdog_handle.take() {
            h.abort();
        }
        self.console.close();
        self.owns_serial_lock = false;
        let lock_dir = self.config.lock_dir();
        lock_manager::release_lock(&self.host, &self.serial_target, &lock_dir);
        self.flags.owns_serial_lock.store(false, Ordering::Relaxed);
        self.flags.serial_owner_pid.store(0, Ordering::Relaxed);
        self.begin_manual_session();
    }

    /// SIGUSR2: after the dutabo direct session ends, resume ownership
    /// (re-acquire the lock + reconnect + probe). If a third party holds the
    /// lock, degrade to `serial_endpoint_busy` honestly — never kill a live
    /// process.
    pub async fn resume_serial(&mut self) {
        self.stopped = false;
        self.running = false;
        self.flags.dutabo_taken.store(0, Ordering::Relaxed);
        let lock_dir = self.config.lock_dir();
        if let Some(conflicting) =
            lock_manager::acquire_lock(&self.host, &self.serial_target, &lock_dir)
        {
            self.flags.owns_serial_lock.store(false, Ordering::Relaxed);
            self.flags
                .serial_owner_pid
                .store(conflicting, Ordering::Relaxed);
            self.serial_error = Some(serde_json::json!({
                "code": "serial_endpoint_busy",
                "message": "Serial endpoint busy after dutabo resume",
                "configured": { "host": self.host, "port": self.serial_target },
                "owner_pid": conflicting,
                "hint": "Call serial_claim once the other process releases the endpoint",
            }));
            return;
        }
        self.owns_serial_lock = true;
        self.flags.owns_serial_lock.store(true, Ordering::Relaxed);
        self.flags.serial_owner_pid.store(0, Ordering::Relaxed);
        self.serial_error = None;
        match self.console.connect().await {
            Ok(()) => {
                self.probe_initial_state().await;
                self.running = true;
            }
            Err(e) => {
                self.serial_error = Some(serde_json::json!({
                    "code": "not_connected",
                    "message": format!("Reconnect after dutabo resume failed: {e}"),
                }));
            }
        }
    }

    /// Change the DUT console baud rate dynamically via RFC 2217 (engine lock held
    /// by the caller; the whole call is bounded at ~1s by the ack timeout).
    pub async fn set_baud(&mut self, baud: u32) -> serde_json::Value {
        if self.state.current() == TargetState::Dutabo {
            return serde_json::json!({"success": false, "code": "dutabo_session",
                "error": "Serial console owned by interactive dutabo session; end it first"});
        }
        if !self.console.is_open() {
            return serde_json::json!({"success": false, "code": "not_connected",
                "error": "Serial console not connected", "hint": "Wait for reconnect or call serial_claim"});
        }
        match self.console.set_baud(baud).await {
            Ok(actual) => {
                self.config
                    .values
                    .insert("SERIAL_BAUDRATE".into(), actual.to_string());
                tracing::info!(
                    baud = actual,
                    "[AGENT-NOTIFY] Console baud changed to {} via RFC 2217",
                    actual
                );
                serde_json::json!({"success": true, "baud": actual, "mode": "rfc2217"})
            }
            Err(e) => {
                let code = match e {
                    SerialError::BaudMismatch { .. } => "baud_mismatch",
                    SerialError::NegotiationTimeout => "ack_timeout",
                    SerialError::TelnetNegotiation(_) => "not_negotiated",
                    SerialError::InvalidConfig(_) => "rfc2217_disabled",
                    _ => "error",
                };
                serde_json::json!({"success": false, "code": code, "error": e.to_string(),
                     "hint": "Requires rfc2217 = true in .target.jsonc [dut.serial] and ser2net telnet(rfc2217) port"})
            }
        }
    }

    /// Verify one candidate baud from a fresh raw boot capture.
    ///
    /// With reset control the ordering is intentional: press reset first,
    /// rotate/create the capture text while the DUT is quiet, then release
    /// reset and collect only bytes from the new boot. This prevents stale
    /// good text from making a wrong baud rate pass.
    ///
    /// `use_reset=false` (the `dutabo init` TEST with dev_ctrl=none) never
    /// touches the relay: the capture just samples whatever the console
    /// currently emits.
    pub async fn test_baud(
        &mut self,
        baud: u32,
        capture_secs: f64,
        use_reset: bool,
    ) -> serde_json::Value {
        if self.state.current() == TargetState::Dutabo {
            return serde_json::json!({
                "success": false,
                "code": "dutabo_session",
                "error": "Serial console owned by interactive dutabo session; end it first"
            });
        }
        if !self.console.is_open() {
            return serde_json::json!({
                "success": false,
                "code": "not_connected",
                "error": "Serial console not connected"
            });
        }

        let configured_baud = self.config.get("SERIAL_BAUDRATE").parse::<u32>().ok();
        let advertised_baud = self.ser2net_banner.as_ref().and_then(|b| b.baudrate);
        let rfc2217 = self.config.serial_rfc2217();
        let mut backend = self.build_power_control();
        let has_reset = use_reset && backend.has_button(Button::Reset);

        if has_reset {
            if let Err(rejection) = self.try_acquire_relay_lock() {
                return rejection;
            }
            if let Err(e) = backend.press(Button::Reset).await {
                self.release_relay_lock();
                return serde_json::json!({
                    "success": false,
                    "code": "reset_press_failed",
                    "error": format!("reset press failed via {}: {e}", backend.name())
                });
            }
        }

        // Recreate the text/log only AFTER reset is asserted. Drain stale
        // bytes while held so the classifier sees this boot, not old text.
        if has_reset {
            self.mark_boot_cycle();
        }
        for _ in 0..3 {
            match self
                .console
                .read_available(Duration::from_millis(100), 4096)
                .await
            {
                Ok(data) if !data.is_empty() => continue,
                _ => break,
            }
        }

        if rfc2217 && let Err(e) = self.console.set_baud(baud).await {
            if has_reset {
                let release = backend.release(Button::Reset).await;
                self.release_relay_lock();
                if let Err(release_err) = release {
                    return serde_json::json!({
                        "success": false,
                        "code": "baud_and_reset_release_failed",
                        "error": format!("baud change failed: {e}; reset release also failed: {release_err}")
                    });
                }
            }
            return serde_json::json!({
                "success": false,
                "code": "baud_change_failed",
                "error": e.to_string()
            });
        }

        if has_reset {
            // Honour the configured minimum hold time before release.
            let hold_ms = self.config.reset_time_ms().max(500);
            tokio::time::sleep(Duration::from_millis(hold_ms)).await;
            if let Err(e) = backend.release(Button::Reset).await {
                self.release_relay_lock();
                return serde_json::json!({
                    "success": false,
                    "code": "reset_release_failed",
                    "error": format!("reset release failed via {}: {e}", backend.name())
                });
            }
            self.release_relay_lock();
        } else {
            self.console.write_raw(b"\r").await;
        }

        let raw = self
            .capture_boot_output(capture_secs.clamp(1.0, 60.0))
            .await;
        let mut analysis_partial = Vec::new();
        let mut clean = strip_ser2net_banner(&raw, &mut analysis_partial);
        // Capture is complete. Interactive prompts usually have no newline,
        // so retain the trailing line unless it is transport metadata.
        if !String::from_utf8_lossy(&analysis_partial).contains("ser2net port") {
            clean.extend_from_slice(&analysis_partial);
        }
        let report = crate::serial_text_detector::analyze_serial_text(&clean);

        // A test must not leave later U-Boot/reference probes at a candidate
        // rate. Restore the server's configured rate after sampling.
        let restore_error = if rfc2217 && configured_baud.is_some_and(|old| old != baud) {
            self.console
                .set_baud(configured_baud.unwrap_or(baud))
                .await
                .err()
                .map(|e| e.to_string())
        } else {
            None
        };

        let numeric_baud_source = if rfc2217 {
            "rfc2217_ack"
        } else if advertised_baud.is_some() {
            "ser2net_banner"
        } else {
            "mcp_config"
        };
        let numeric_baud_match = if rfc2217 {
            None
        } else {
            advertised_baud
                .map(|actual| actual == baud)
                .or_else(|| configured_baud.map(|actual| actual == baud))
        };
        let text_ok = report.verdict == crate::serial_text_detector::TextVerdict::Normal;
        // A raw endpoint cannot change the host UART rate. When ser2net does
        // not advertise metadata, TEST verifies that the form value matches
        // the MCP's loaded config AND that a fresh boot is normal text. This
        // is a configuration/link check, not a dynamic baud ACK.
        let numeric_baud_ok = rfc2217 || numeric_baud_match == Some(true);
        let success = text_ok && numeric_baud_ok && restore_error.is_none();
        let code = if numeric_baud_match == Some(false) {
            "baud_mismatch"
        } else if !rfc2217 && numeric_baud_match.is_none() {
            "baud_unverifiable_raw"
        } else if restore_error.is_some() {
            "restore_failed"
        } else if report.verdict == crate::serial_text_detector::TextVerdict::Indeterminate {
            "insufficient_text"
        } else if !text_ok {
            "garbage_text"
        } else {
            "ok"
        };
        serde_json::json!({
            "success": success,
            "code": code,
            "requested_baud": baud,
            "configured_baud": configured_baud,
            "advertised_baud": advertised_baud,
            "numeric_baud_source": numeric_baud_source,
            "numeric_baud_match": numeric_baud_match,
            "rfc2217": rfc2217,
            "reset_used": has_reset,
            "capture_bytes": clean.len(),
            "text": report,
            "restore_error": restore_error,
        })
    }

    /// Stop the engine
    #[tracing::instrument(skip(self))]
    pub async fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        self.running = false;
        if let Some(h) = self.read_handle.take() {
            h.abort();
        }
        if let Some(h) = self.watchdog_handle.take() {
            h.abort();
        }
        self.console.close();
        self.logs.close();
        if self.owns_serial_lock {
            let lock_dir = self.config.lock_dir();
            lock_manager::release_lock(&self.host, &self.serial_target, &lock_dir);
            self.owns_serial_lock = false;
            self.flags.owns_serial_lock.store(false, Ordering::Relaxed);
            self.flags.serial_owner_pid.store(0, Ordering::Relaxed);
        }
        // D11: state/cache files are deleted unconditionally on stop — a
        // server that never owned the endpoint still leaves no
        // target-state/statusline residue (the per-DUT variants included).
        self.state.transition(TargetState::Stopped);
        if let Some(project_dir) = &self.config.project_dir {
            self.state.remove_pid(project_dir, &self.config.dut_dir());
        }
        tracing::info!(
            "[{}:{}] SerialEngine stopped",
            self.host,
            self.serial_target
        );
    }

    /// Store the background task handles (called after mcp/mcp_http spawns them)
    pub fn set_background_tasks(
        &mut self,
        read_handle: tokio::task::JoinHandle<()>,
        watchdog_handle: tokio::task::JoinHandle<()>,
    ) {
        self.read_handle = Some(read_handle);
        self.watchdog_handle = Some(watchdog_handle);
    }

    /// Check whether the event list contains a boot-complete signal
    fn is_boot_complete(events: &[BootEvent]) -> bool {
        events
            .iter()
            .any(|e| matches!(e, BootEvent::Stage(s) if crate::dut_state::is_shell_stage(s)))
    }

    /// Send a line to the board and read back the response using an
    /// expect-style poll loop.  Checks for data every `EXPECT_POLL_INTERVAL_MS`,
    /// returns immediately when data arrives.  `COMMAND_RESPONSE_CEILING_MS`
    /// is only a failsafe for a dead board.
    async fn send_and_expect(&mut self, cmd: &str) -> Result<Vec<u8>, std::io::Error> {
        self.console.sendline(cmd);
        self.console.drain_writes().await;
        // `dutabo init` starts a short-lived validation server which must
        // reach reset control even when the DUT begins in loader/MASKROM and
        // cannot answer a prompt. Normal project servers keep the conservative
        // ceiling; only the explicit child-process override shortens it.
        let ceiling = std::env::var("SERMCP_STARTUP_PROBE_TIMEOUT_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .map(|millis| Duration::from_millis(millis.clamp(100, 30_000)))
            .unwrap_or(COMMAND_RESPONSE_CEILING);
        let deadline = std::time::Instant::now() + ceiling;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Ok(Vec::new());
            }
            let poll = remaining.min(EXPECT_POLL);
            match self.console.read_available(poll, 4096).await {
                Ok(data) if Self::has_probe_response(&data) => return Ok(data),
                Ok(data) if !data.is_empty() => self.logs.write(&data),
                Err(e) => return Err(e),
                _ => {} // no data yet, keep polling
            }
        }
    }

    /// A blank-line echo is transport activity, not evidence of a live shell.
    fn has_probe_response(data: &[u8]) -> bool {
        data.iter().any(|byte| !byte.is_ascii_whitespace())
    }

    /// Probe the serial port to determine the board's current state.
    ///
    /// Uses an expect pattern: quick non-blocking drain first, then
    /// send-and-expect to trigger a prompt if the board appears idle.
    /// No hardcoded sleeps — poll loop returns as soon as data arrives.
    #[tracing::instrument(skip(self), fields(result))]
    async fn probe_initial_state(&mut self) {
        let _start_state = self.state.current();
        // Step 1: non-blocking drain of kernel buffer
        match self.console.read_available(PROBE_POLL, 4096).await {
            Ok(data) if !data.is_empty() => {
                let hex_preview = hex_preview(&data, 128);
                tracing::info!(len = data.len(), hex = %hex_preview, "Probe: initial data");
                if let Some(banner) = parse_ser2net_banner(&data) {
                    tracing::info!(
                        port = %banner.port,
                        device = %banner.device,
                        baudrate = ?banner.baudrate,
                        "ser2net endpoint metadata"
                    );
                    if banner.port != self.serial_target {
                        self.serial_error = Some(serde_json::json!({
                            "code": "ser2net_endpoint_mismatch",
                            "message": format!(
                                "Connected to configured TCP port {}, but ser2net advertised port {}",
                                self.serial_target, banner.port
                            ),
                            "configured": {
                                "host": self.host,
                                "port": self.serial_target,
                            },
                            "observed": banner.clone(),
                        }));
                        self.ser2net_banner = Some(banner);
                        self.state.transition(TargetState::Disconnected);
                        return;
                    }
                    self.ser2net_banner = Some(banner);
                }
                let filtered = strip_ser2net_banner(&data, &mut self.ser2net_partial);
                if !filtered.is_empty() {
                    self.logs.write(&filtered);
                }
                if Self::has_probe_response(&filtered) {
                    let events = self.detector.feed(&filtered);
                    self.state.on_activity();

                    if Self::is_boot_complete(&events) {
                        self.state.transition(TargetState::Active);
                        tracing::info!("Probe: boot complete → active");
                        return;
                    }
                    if events.iter().any(|e| matches!(e, BootEvent::BootStart)) {
                        self.state.transition(TargetState::Booting);
                        tracing::info!("Probe: SPL → booting");
                        return;
                    }
                    if events
                        .iter()
                        .any(|e| matches!(e, BootEvent::Stage(s) if s == "uboot"))
                    {
                        self.state.transition(TargetState::UBoot);
                        tracing::info!("Probe: at U-Boot prompt");
                        return;
                    }
                }

                // Buffered data inconclusive — trigger fresh prompt
                match self.send_and_expect("\r").await {
                    Ok(d2) if !d2.is_empty() => {
                        self.logs.write(&d2);
                        let e2 = self.detector.feed(&d2);
                        self.state.on_activity();
                        if Self::is_boot_complete(&e2) {
                            self.state.transition(TargetState::Active);
                            tracing::info!("Probe: 2nd pass boot complete → active");
                        } else if e2
                            .iter()
                            .any(|e| matches!(e, BootEvent::Stage(s) if s == "uboot"))
                        {
                            self.state.transition(TargetState::UBoot);
                            tracing::info!("Probe: 2nd pass → U-Boot");
                        } else {
                            self.state.transition(TargetState::Active);
                            tracing::info!("Probe: responding → active");
                        }
                    }
                    Err(_) => {
                        self.state.transition(TargetState::Disconnected);
                        tracing::warn!("Probe: 2nd read error → disconnected");
                    }
                    _ => {
                        self.state.transition(TargetState::DutOff);
                        tracing::warn!("Probe: not responding → DUT-off");
                    }
                }
            }
            Ok(_) => {
                // Board appears idle — send empty line to verify it's alive
                match self.send_and_expect("\r").await {
                    Ok(d2) if !d2.is_empty() => {
                        self.logs.write(&d2);
                        let e2 = self.detector.feed(&d2);
                        self.state.on_activity();
                        if Self::is_boot_complete(&e2) {
                            self.state.transition(TargetState::Active);
                            tracing::info!("Probe: late response → boot complete → active");
                        } else {
                            self.state.transition(TargetState::Active);
                            tracing::info!("Probe: late response → active");
                        }
                    }
                    Err(_) => {
                        self.state.transition(TargetState::Disconnected);
                        tracing::warn!("Probe: 2nd read error after probe → disconnected");
                    }
                    _ => {
                        self.state.transition(TargetState::DutOff);
                        tracing::warn!("Probe: no data, no probe response → DUT-off");
                    }
                }
            }
            Err(e) => {
                self.state.transition(TargetState::Disconnected);
                tracing::warn!("Probe: read error ({e}) → disconnected");
            }
        }
        tracing::Span::current().record(
            "result",
            tracing::field::display(
                self.state
                    .external_state()
                    .map(|s| s.as_str())
                    .unwrap_or("unknown"),
            ),
        );
    }

    /// Check if serial data contains a shutdown message → transition to DUT-off.
    fn maybe_detect_shutdown(&mut self, data: &[u8]) {
        let text = String::from_utf8_lossy(data);
        if text.contains("Power down")
            || text.contains("System halted")
            || text.contains("reboot: Power down")
        {
            self.state.transition(TargetState::DutOff);
            tracing::info!("Shutdown detected → DUT-off");
        }
    }

    /// Event-driven read loop: wait for data (epoll/kqueue, no polling), handle watchdog.
    ///
    /// The engine lock is NOT held during backoff sleeps — see
    /// `reconnect_after`.
    pub async fn read_loop_iter(&mut self) {
        if !self.running {
            return;
        }
        // Always drain pending writes
        self.console.drain_writes().await;

        // If a previous drain_writes marked the connection as broken, surface
        // the error promptly instead of waiting for wait_readable to time out.
        // (ser2net may keep the TCP socket open even after the serial device
        // disappears, so wait_readable never signals EOF.)
        let cur = self.state.current();
        if !self.console.is_open() && matches!(cur, TargetState::Active | TargetState::Booting) {
            // Probe the socket once — if it returns an error, handle it now.
            if let Err(e) = self
                .console
                .read_available(std::time::Duration::from_millis(0), 128)
                .await
            {
                self.handle_read_error(e).await;
                return;
            }
        }
        // reconnect_after has elapsed, try the actual reconnect now.
        // The deadline check (not a bare take()) is what makes the backoff
        // real: while now < reconnect_after this branch is skipped entirely,
        // so the read loop releases the engine lock instead of reconnecting
        // (and holding the lock in connect()) on every ~20ms iteration.
        if self
            .reconnect_after
            .is_some_and(|after| std::time::Instant::now() >= after)
        {
            self.reconnect_after = None;
            // Re-check the endpoint lock first: reconnecting without it would
            // co-attach a `dutabo serial` human session. On conflict the
            // backoff below re-arms and self-heals once the endpoint frees.
            if self.reacquire_endpoint_lock()
                && let Ok(()) = self.console.connect().await
            {
                tokio::time::sleep(Duration::from_millis(500)).await;
                self.probe_initial_state().await;
                self.reconnect.reset();
                tracing::info!("Auto-reconnected after backoff");
            }
            return;
        }

        self.console.drain_writes().await;
        // Command deadlines are wall-clock based and must advance even when
        // the serial transport is completely silent. Keeping this only in the
        // read_available(empty) branch leaves a timed-out command in `current`
        // forever because wait_readable(false) returns before that branch.
        self.commands.check_timeouts();
        // Disconnected state → actively retry reconnection (exponential backoff).
        // Backoff sleep is deferred via reconnect_after — engine lock NOT held.
        if self.state.current() == TargetState::Disconnected {
            // Check if we're in a backoff waiting period.
            if let Some(after) = self.reconnect_after
                && std::time::Instant::now() < after
            {
                return; // backoff window hasn't elapsed yet
            }
            // No pending window: first disconnect, or the previous window
            // elapsed and its reconnect attempt (consumed above) failed.
            // (Re-)arm the next backoff window.
            let delay = self.reconnect.next_delay();
            self.reconnect_after = Some(std::time::Instant::now() + delay);
            tracing::info!(
                "[AGENT-NOTIFY] Serial disconnected, reconnecting in {:.1}s",
                delay.as_secs_f64()
            );
            return;
        }
        // Event-driven wait for data (100ms timeout, releases the lock quickly for HTTP).
        // A queued write (e.g. a dutabo keystroke relayed over WebSocket) pokes
        // the console's write notify and is flushed to the wire immediately —
        // without this arm it would sit in the write channel until the
        // readability timeout expires, adding up to ~110ms of typing lag.
        tokio::select! {
            readable = self.console.wait_readable(Duration::from_millis(100)) => {
                if !readable {
                    return;
                }
            }
            _ = self.console.wait_writable() => {
                self.console.drain_writes().await;
                return;
            }
        }
        // Data ready, read and process it
        match self
            .console
            .read_available(Duration::from_millis(0), 4096)
            .await
        {
            Ok(data) if !data.is_empty() => {
                // Broadcast to WebSocket clients (dutabo serial) and to
                // passive monitor subscribers (no manual-session claim).
                if let Some(ref tx) = self.ws_tx {
                    let _ = tx.send(data.clone());
                }
                let _ = self.monitor_tx.send(data.clone());
                // Log raw hex for debugging: first 64 bytes of each read
                let hex_preview = hex_preview(&data, 64);
                tracing::info!(len = data.len(), hex = %hex_preview, "Serial read");
                let clean_data = strip_ser2net_banner(&data, &mut self.ser2net_partial);
                // Prompts need not end with a newline; the banner filter may
                // still be buffering them, so judge liveness from raw bytes.
                let responsive = Self::has_probe_response(&data);
                if responsive {
                    self.state.on_activity();
                }
                if !clean_data.is_empty() {
                    self.logs.write(&clean_data);
                }
                // Human terminal ownership is exclusive. Continue relaying and
                // logging bytes, but do not run Agent detectors, auto-login,
                // command markers, heartbeat actions, or state transitions.
                if self.state.current() == TargetState::Dutabo {
                    return;
                }
                self.maybe_detect_shutdown(&data);
                if responsive && self.state.current() == TargetState::DutOff {
                    self.state.transition(TargetState::Active);
                    tracing::info!("Device resumed from DUT-off → active");
                }
                // Catch panics in detector.feed() — a malformed regex or unexpected
                // input should not crash the MCP server.
                let events = {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        self.detector.feed(&clean_data)
                    }));
                    match result {
                        Ok(events) => events,
                        Err(e) => {
                            let msg = panic_message(e);
                            tracing::error!(
                                "[AGENT-NOTIFY] SerialEngine panicked in detector.feed(): {}. Reconnecting...",
                                msg
                            );
                            self.state.transition(TargetState::Disconnected);
                            return;
                        }
                    }
                };
                self.handle_boot_events(events).await;
                let clean = strip_android_klog(&data);
                // Catch panics in feed_serial_data() — marker parsing or buffer
                // management bugs should not crash the MCP server.
                {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        self.commands.feed_serial_data(&clean);
                    }));
                    if let Err(e) = result {
                        let msg = panic_message(e);
                        tracing::error!(
                            "[AGENT-NOTIFY] SerialEngine panicked in commands.feed_serial_data(): {}. Reconnecting...",
                            msg
                        );
                        self.state.transition(TargetState::Disconnected);
                    }
                }
            }
            Ok(_) => {
                self.commands.check_timeouts();
            }
            Err(e) => {
                self.handle_read_error(e).await;
            }
        }

        // Drain command metrics from CommandQueue → StateManager
        let completed = self.commands.completed_count;
        let errors = self.commands.error_count;
        for _ in 0..completed {
            self.state.inc_command();
        }
        for _ in 0..errors {
            self.state.inc_error();
        }
        // Record latencies from recently completed commands.
        for ms in self.commands.drain_latencies() {
            self.state.record_latency(ms);
        }
        self.commands.completed_count = 0;
        self.commands.error_count = 0;
    }

    /// Re-claim the endpoint lock before any reconnect after a connection
    /// drop. While this engine was disconnected, a `dutabo serial` human
    /// session (or another MCP) may have taken the endpoint — reconnecting
    /// without the lock would silently co-attach the Agent during a human
    /// session. On conflict this demotes the engine to non-owner and stores
    /// the structured busy error; the backoff path re-checks, so the engine
    /// self-heals the moment the endpoint frees up.
    fn reacquire_endpoint_lock(&mut self) -> bool {
        let lock_dir = self.config.lock_dir();
        if self.owns_serial_lock {
            // Never dropped ownership: the lock file must still name us. If
            // it does not, the endpoint was taken over (dutabo escalation)
            // and we re-contend like any other outsider.
            let lock_path =
                lock_manager::endpoint_lock_path(&self.host, &self.serial_target, &lock_dir);
            if lock_manager::endpoint_lock_owner(&lock_path) == Some(std::process::id()) {
                return true;
            }
        }
        match lock_manager::acquire_lock(&self.host, &self.serial_target, &lock_dir) {
            None => {
                self.owns_serial_lock = true;
                self.flags.owns_serial_lock.store(true, Ordering::Relaxed);
                self.flags.serial_owner_pid.store(0, Ordering::Relaxed);
                self.serial_error = None;
                true
            }
            Some(conflicting_pid) => {
                self.owns_serial_lock = false;
                self.flags.owns_serial_lock.store(false, Ordering::Relaxed);
                self.flags
                    .serial_owner_pid
                    .store(conflicting_pid, Ordering::Relaxed);
                self.serial_error = Some(serde_json::json!({
                    "code": "serial_endpoint_busy",
                    "message": format!(
                        "Target {}:{} was taken over while this engine was disconnected",
                        self.host, self.serial_target
                    ),
                    "configured": { "host": self.host, "port": self.serial_target },
                    "owner_pid": conflicting_pid,
                    "hint": "Human direct session in progress; retry once it releases the endpoint",
                }));
                false
            }
        }
    }

    async fn handle_read_error(&mut self, e: std::io::Error) {
        tracing::warn!("Serial read error: {e}");
        let cur = self.state.current();

        // Close the dead connection so the next reconnect attempt starts clean.
        self.console.close();

        // ═══════════════════════════════════════════════════════════════════
        // Fast-retry guard: prevent ping-pong when ser2net accepts TCP but
        // there's no real serial device behind it (board unplugged / USB gone).
        //
        // If we fast-retried within the last 10 seconds and the connection
        // dropped again, skip the fast retry — the board is likely physically
        // disconnected. Go straight to backoff.
        // ═══════════════════════════════════════════════════════════════════
        let now = std::time::Instant::now();
        let skip_fast = self
            .last_fast_retry
            .map(|t| now.duration_since(t).as_secs() < 10)
            .unwrap_or(false);

        if !skip_fast {
            self.last_fast_retry = Some(now);

            // Fast retry: try to reconnect immediately (2s timeout).
            // Most "Connection closed by ser2net" events are brief USB
            // re-enumeration glitches (< 500ms) during board reboots.
            if !self.reacquire_endpoint_lock() {
                self.state.transition(TargetState::Disconnected);
                return;
            }
            match self.console.connect_with_timeout(2).await {
                Ok(()) => {
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    self.probe_initial_state().await;
                    self.reconnect.reset();
                    // The connection is healthy again — a pending backoff
                    // deadline (if any) is stale and would otherwise trigger
                    // a spurious reconnect once it elapses.
                    self.reconnect_after = None;
                    tracing::info!(
                        "Fast reconnect succeeded (was {:?}), user never saw disconnected",
                        cur
                    );
                    return;
                }
                Err(ref e2) => {
                    tracing::warn!(
                        "Fast reconnect failed: {e2}. Falling back to backoff reconnect."
                    );
                }
            }
        } else {
            tracing::info!(
                "Skipping fast retry: already retried < 10s ago (was {:?})",
                cur
            );
        }

        // Fast retry skipped or failed — now we really are disconnected.
        self.state.transition(TargetState::Disconnected);
        // Reconnection is deferred to read_loop_iter() — sole reconnection path
        // avoids duplicate probe race when read loop also tries to reconnect.
        tracing::info!(
            "Disconnected (was {:?}), read_loop_iter will reconnect",
            cur
        );
    }

    /// Watchdog iteration (called periodically by a dedicated spawned task)
    pub fn watchdog_once(&mut self) {
        if !self.running {
            return;
        }
        if self.state.current() == TargetState::Dutabo {
            return;
        }
        self.state.check_hang();
        let cur = self.state.current();
        // Heartbeat probe: no data for a long time in booting/active state
        if self.state.heartbeat_pending
            && (cur == TargetState::Active || cur == TargetState::Booting)
            && self.console.is_open()
        {
            tracing::info!("Heartbeat probe");
            // A probe counts as "sent" only when it was actually enqueued —
            // a dropped probe keeps `heartbeat_pending` true so the next
            // tick retries, and can never count toward the DUT-off verdict.
            if self.console.sendline("") {
                self.state.mark_probe_sent();
            } else {
                tracing::warn!(
                    "Heartbeat probe dropped (write channel full); will retry next tick"
                );
            }
        }
        // Flush the detector buffer every 1s (prompts without \n)
        if self.state.last_data_elapsed().as_secs() >= 1 {
            let events = self.detector.flush_line_buf();
            for event in events {
                if let BootEvent::Stage(ref s) = event
                    && crate::dut_state::is_shell_stage(s)
                {
                    self.state.transition(TargetState::Active);
                }
            }
        }
        // Text similarity detection: data looks like the reference log → switch to booting/uboot immediately
        if let Some(ref learner) = self.detector.learner
            && self.logs.ring_buffer.len() > 512
        {
            let recent = &self.logs.ring_buffer[self.logs.ring_buffer.len().saturating_sub(2048)..];
            let text = String::from_utf8_lossy(recent);
            if learner.is_boot_like(&text, 0.25) && cur == TargetState::Active {
                // Text similarity detected a reboot → full log rotation
                self.logs.flush_boot_log();
                self.logs.mark_boot_start();
                self.detector.reset_cycle();
                self.state.transition(TargetState::Booting);
                // Clear the ring buffer: it still holds the SAME bytes that
                // just matched — without this the detector re-triggers
                // "reboot detected" every watchdog cycle (~30s) while the
                // board sits idle, endlessly flapping booting↔active.
                self.logs.ring_buffer.clear();
                tracing::info!("Text similarity: reboot detected → log rotated + booting");
            }
        }
    }

    /// Handle boot detection events
    async fn handle_boot_events(&mut self, events: Vec<BootEvent>) {
        for event in events {
            match event {
                BootEvent::BootStart => {
                    // DDR/SPL appearing = board rebooted = new boot cycle
                    // Split the log regardless of current state (boot_detected dedupes the same cycle)
                    let cur = self.state.current();
                    self.logs.flush_boot_log();
                    self.logs.mark_boot_start();
                    self.logs.truncate_unclassified();
                    tracing::info!("BootStart: new boot cycle (was {:?})", cur);
                    // Fresh login cycle; a sticky tx_repeat (confirmed link
                    // defect) deliberately survives reboots.
                    self.login_attempts = 0;
                    self.state.transition(TargetState::Booting);
                    self.detector.reset_cycle();
                }
                BootEvent::RebootStart => {
                    // Reboot initiated by the DUT itself (reboot command / watchdog / crash auto-restart).
                    // Re-arm the dedup cycle so the next SPL/DDR triggers BootStart to complete the
                    // log rotation + booting transition — otherwise a real reboot is swallowed by
                    // dedup and the state wrongly stays active while the board is actually rebooting.
                    self.detector.reset_cycle();
                    self.state.transition(TargetState::Booting);
                }
                BootEvent::Autoboot => {
                    tracing::debug!("Autoboot detected — letting board boot normally");
                }
                BootEvent::LoginPrompt => {
                    if self.login_user.is_empty() {
                        tracing::warn!("[AGENT-NOTIFY] Login prompt but no login_user set");
                    } else {
                        self.send_login().await;
                    }
                }
                BootEvent::PasswordPrompt => {
                    if !self.login_pass.is_empty() {
                        tracing::info!("[AGENT-NOTIFY] Password prompt — sending password");
                        self.console.sendline(&self.login_pass);
                    }
                }
                BootEvent::Crash(crash_type, line) => {
                    self.state.transition(TargetState::Crashed);
                    tracing::warn!("CRASH [{crash_type}]: {line}");
                }
                BootEvent::Unclassified(lines) => {
                    // Append unclassified lines to unclassified.log (for Agent self-learning analysis)
                    if !lines.is_empty()
                        && let Err(e) = self.logs.append_unclassified(&lines)
                    {
                        tracing::warn!("Failed to write unclassified lines: {e}");
                    }
                }
                BootEvent::Stage(stage) => {
                    let new_state = if matches!(stage.as_str(), "uboot" | "autoboot") {
                        Some(TargetState::UBoot)
                    } else if crate::dut_state::is_shell_stage(&stage) {
                        Some(TargetState::Active)
                    } else {
                        Some(TargetState::Booting)
                    };
                    if let Some(ns) = new_state {
                        // Shell prompt seen → switch to active immediately, don't wait
                        if ns == TargetState::Active {
                            self.login_attempts = 0;
                            self.state.transition(TargetState::Active);
                        } else {
                            let cur = self.state.current();
                            if cur != ns && cur != TargetState::Crashed {
                                self.state.transition(ns);
                            }
                        }
                    }
                }
            }
        }
    }

    // ── MCP Tool interface ──

    /// Snapshot position in the shared serial byte stream (`logs.ring_buffer`).
    ///
    /// Every byte the read loop consumes is written here, making this the
    /// race-free observation point for liveness probes. A probe must watch
    /// this stream — never `console.read_available()` — because the read
    /// loop usually consumes the probe response (prompt echo) before the
    /// probe can take the engine lock (A2 false DUT-off regression).
    pub fn shared_stream_snapshot(&self) -> u64 {
        self.logs.stream_cursor()
    }

    /// Whether new bytes landed in the shared serial stream after `snapshot`.
    ///
    /// The ring buffer is append-only except for the >2MB overflow drain, so
    /// any length change means the read loop consumed new data (a drain
    /// implies an even larger burst arrived). Used by the post-timeout probe
    /// to decide DUT alive vs DUT-off without racing the read loop for raw
    /// socket bytes.
    pub fn shared_stream_has_new(&self, snapshot: u64) -> bool {
        self.logs.stream_has_new(snapshot)
    }

    /// Bytes that landed in the shared serial stream after `snapshot`.
    ///
    /// Output-collection companion to `shared_stream_snapshot`: the read
    /// loop is the sole socket reader, so a tool waiting for a command's
    /// output watches this tail instead of reading the console directly.
    /// Returns the retained clean stream plus the current incomplete line.
    /// The boolean reports that the ring overflowed or was cleared after the
    /// snapshot, so the caller can surface truncation instead of silently
    /// presenting an incomplete response as whole.
    pub fn serial_output_since(&self, snapshot: u64) -> (Vec<u8>, bool) {
        let (mut tail, truncated) = self.logs.stream_tail_since(snapshot);
        tail.extend_from_slice(&self.ser2net_partial);
        (tail, truncated)
    }

    /// Commit bytes that preceded a new U-Boot command before taking its
    /// stream snapshot. A quiet prompt has no newline and otherwise remains
    /// in the banner filter's partial-line buffer, where it could be mistaken
    /// for the prompt produced by the next command.
    pub fn commit_ser2net_partial(&mut self) {
        let held = std::mem::take(&mut self.ser2net_partial);
        if !held.is_empty() {
            self.logs.write(&held);
        }
    }

    /// Auto-login, pexpect/LAVA style: prompt-driven and bounded.
    ///
    /// On every `login:` prompt send the configured username PLAINLY;
    /// the separate PasswordPrompt handler sends the password. Mature
    /// serial harnesses (pyserial/pexpect, LAVA, labgrid) never modify
    /// the byte stream to compensate for a lossy adapter — link loss is
    /// infrastructure failure and belongs to hardware or, as a manual
    /// escape hatch, to `serial.tx_repeat` in `.target.jsonc`. After
    /// `MAX_LOGIN_ATTEMPTS` rejections we STOP (some systems lock
    /// accounts after repeated failures) and tell the operator exactly
    /// what to check.
    async fn send_login(&mut self) {
        const MAX_LOGIN_ATTEMPTS: u32 = 3;
        let now = std::time::Instant::now();
        if let Some(last) = self.last_login_send
            && now.duration_since(last) < Duration::from_millis(500)
        {
            // Same prompt instance matched twice (ANSI split, re-print).
            return;
        }
        self.last_login_send = Some(now);
        if self.login_attempts >= MAX_LOGIN_ATTEMPTS {
            // One warning per boot cycle: after the first exhaustion
            // message, stay silent but also stop sending credentials.
            if self.login_attempts == MAX_LOGIN_ATTEMPTS {
                self.login_attempts += 1;
                tracing::warn!(
                    "[AGENT-NOTIFY] auto-login rejected {} times — giving up this \
                     cycle. The username WAS delivered (a Password: prompt was seen), \
                     so check target.login_user / target.login_pass in .target.jsonc. \
                     For an adapter that really drops bytes, set serial.tx_repeat=2.",
                    MAX_LOGIN_ATTEMPTS
                );
            }
            return;
        }
        self.login_attempts += 1;
        tracing::info!(
            "[AGENT-NOTIFY] Login prompt (attempt {}/{}) — auto-login as '{}'",
            self.login_attempts,
            MAX_LOGIN_ATTEMPTS,
            self.login_user
        );
        self.console.sendline(&self.login_user);
    }

    /// Queue a command and return the receiver — the caller must await it after releasing the engine lock
    #[tracing::instrument(skip(self), fields(cmd = %command, timeout))]
    pub fn queue_command(
        &mut self,
        command: &str,
        timeout: f64,
    ) -> tokio::sync::oneshot::Receiver<crate::command_queue::CommandResult> {
        self.commands.execute(command.to_string(), timeout)
    }

    /// The observability block for no-dev_ctrl guidance: the
    /// agent can see WHY power control is unavailable without scraping the
    /// raw config dump.
    fn power_control_state_block(&self) -> serde_json::Value {
        let backend = self.build_power_control();
        let configured = self.power_control_configured();
        let (reason, _) = self.power_control_reason();
        serde_json::json!({
            "configured": configured,
            "backend": backend.name(),
            "buttons": {
                "reset": backend.has_button(Button::Reset),
                "power": backend.has_button(Button::Power),
                "download": backend.has_button(Button::Download),
            },
            "reason": if configured { serde_json::Value::Null } else { serde_json::json!(reason) },
        })
    }

    /// Current serial log path as a JSON-ready string ("" before the first
    /// rotation opens a file) — shared by every tool response that embeds
    /// `log_path`.
    pub fn log_path_string(&self) -> String {
        self.logs
            .current_path()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default()
    }

    pub fn get_state_dict(&self) -> serde_json::Value {
        let mut state = serde_json::json!({
            "state": self.state.external_state().map(|s| s.as_str()).unwrap_or(""),
            "dut_name": self.config.get_str_or("DUT_NAME", "default"),
            "serial_endpoint": { "host": self.host, "port": self.serial_target },
            "boot_number": self.logs.boot_number(),
            "last_data_seconds": self.state.last_data_elapsed().as_secs_f64(),
            "log_path": self.log_path_string(),
            "relay_configured": self.power_control_configured(),
            "power_control": self.power_control_state_block(),
            "login_configured": !self.login_user.is_empty(),
            "uptime_secs": self.state.uptime_secs(),
            "command_count": self.state.command_count(),
            "error_count": self.state.error_count(),
            "pending_commands": self.commands.pending_len(),
        });
        if let Some(banner) = &self.ser2net_banner {
            state["ser2net"] = serde_json::to_value(banner).unwrap_or_default();
        }
        if let Some(error) = &self.serial_error {
            // An endpoint-busy error is NOT a configuration problem: two
            // projects may legitimately share one board, and the loser of
            // the lock race must not scare the operator with a false
            // "configuration-error" label.
            let label =
                if error.get("code").and_then(|c| c.as_str()) == Some("serial_endpoint_busy") {
                    "endpoint-busy"
                } else {
                    "configuration-error"
                };
            state["state"] = serde_json::Value::String(label.into());
            state["error"] = error.clone();
        }
        state
    }

    /// Lock-free fallback copied into McpHandler before startup begins.
    pub fn connecting_state_dict(&self) -> serde_json::Value {
        serde_json::json!({
            "state": "connecting",
            "probe": "in_progress",
            "dut_name": self.config.get_str_or("DUT_NAME", "default"),
            "serial_endpoint": { "host": self.host, "port": self.serial_target },
            "log_path": self.log_path_string(),
        })
    }

    pub fn has_serial_error(&self) -> bool {
        self.serial_error.is_some()
    }

    fn set_startup_error(&mut self, message: String) {
        self.serial_error = Some(serde_json::json!({
            "code": "serial_startup_failed",
            "message": message,
            "configured": { "host": self.host, "port": self.serial_target },
        }));
    }

    pub fn read_log(
        &self,
        archive_index: usize,
        lines: usize,
        pattern: Option<&str>,
    ) -> serde_json::Value {
        let result = self.logs.read_log(archive_index, lines, pattern);
        serde_json::json!({
            "content": result.content,
            "filename": result.filename,
            "total_lines": result.total_lines,
            "filtered_lines": result.filtered_lines,
        })
    }

    pub fn list_logs(&self) -> serde_json::Value {
        let archives = self.logs.list_archives();
        let current = self
            .logs
            .current_path()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        serde_json::json!({
            "archives": archives.iter().map(|a| serde_json::json!({
                "index": a.index,
                "filename": a.filename,
                "size_bytes": a.size_bytes,
            })).collect::<Vec<_>>(),
            "current": current,
        })
    }

    /// Power cycle target via relay: OFF → wait (≥ power_off_ms) → ON.
    pub async fn power_cycle_target(&mut self, wait_boot: bool) -> serde_json::Value {
        let mut backend = self.build_power_control();
        if !backend.has_button(Button::Power) {
            return self.power_control_failure("power");
        }
        if let Err(rejection) = self.try_acquire_relay_lock() {
            return rejection;
        }
        // Remember the state before the cycle: if the relay backend errors
        // out, roll back so a possibly-dark DUT isn't reported as "booting".
        let prev_state = self.state.current();
        self.state.transition(TargetState::Booting);
        self.logs.flush_boot_log();
        self.logs.mark_boot_start();
        self.detector.reset_cycle();

        if let Err(error) = backend
            .pulse(Button::Power, self.config.power_off_time_ms())
            .await
        {
            self.state.transition(prev_state);
            self.release_relay_lock();
            return serde_json::json!({
                "success": false,
                "error": format!("Power cycle failed via {}: {error} Relay state unknown — DUT may be unpowered; run serial_verify_relay.", backend.name()),
            });
        }

        if !wait_boot {
            self.release_relay_lock();
            return serde_json::json!({
                "success": true,
                "new_boot_number": self.logs.boot_number(),
                "log_path": self.log_path_string(),
            });
        }

        // Pulse done — the relay lock only guards the physical pulse.
        self.release_relay_lock();

        // Wait for login prompt
        let result = self.wait_pattern_internal_opts("login:", 120.0, true).await;
        if result["matched"].as_bool().unwrap_or(false) {
            return serde_json::json!({
                "success": true,
                "new_boot_number": self.logs.boot_number(),
                "log_path": self.log_path_string(),
                "boot_complete": true,
            });
        }
        serde_json::json!({
            "success": false,
            "error": "Boot did not complete within timeout",
            "new_boot_number": self.logs.boot_number(),
        })
    }

    pub async fn reset_target(
        &mut self,
        wait_boot: bool,
        failure_retry: usize,
        failure_retry_interval: f64,
    ) -> serde_json::Value {
        if let Err(rejection) = self.try_acquire_relay_lock() {
            return rejection;
        }
        let mut backend = self.build_power_control();
        if !backend.has_button(Button::Reset) {
            self.release_relay_lock();
            return self.power_control_failure("reset");
        }
        // Immediate state transition (don't wait for relay).
        let prev_state = self.state.current();
        self.state.transition(TargetState::Booting);
        if let Err(e) = backend.pulse(Button::Reset, 500).await {
            self.state.transition(prev_state);
            self.release_relay_lock();
            return serde_json::json!({
                "success": false,
                "error": format!("reset control failed via {}: {e} Relay state unknown — DUT may be held in reset; run serial_verify_relay.", backend.name()),
                "new_boot_number": self.logs.boot_number(),
            });
        }
        self.logs.flush_boot_log();
        self.logs.mark_boot_start();
        self.detector.reset_cycle();

        if !wait_boot {
            self.release_relay_lock();
            return serde_json::json!({
                "success": true,
                "new_boot_number": self.logs.boot_number(),
                "log_path": self.log_path_string(),
            });
        }

        // Wait for login: with force_prompt_wait (lava semantics). Retry the
        // whole reset+wait up to `failure_retry` times on timeout.
        let mut attempts = 0usize;
        loop {
            attempts += 1;
            let result = self.wait_pattern_internal_opts("login:", 120.0, true).await;
            if result["matched"].as_bool().unwrap_or(false) {
                self.release_relay_lock();
                return serde_json::json!({
                    "success": true,
                    "new_boot_number": self.logs.boot_number(),
                    "log_path": self.log_path_string(),
                    "boot_complete": true,
                    "attempts": attempts,
                });
            }
            if attempts >= failure_retry {
                self.release_relay_lock();
                return serde_json::json!({
                    "success": true,
                    "new_boot_number": self.logs.boot_number(),
                    "log_path": self.log_path_string(),
                    "boot_complete": false,
                    "attempts": attempts,
                    "error": "login prompt not detected within timeout",
                });
            }
            // Retry: re-assert relay reset and re-arm.
            tracing::info!("reset_target retry {}/{}", attempts, failure_retry);
            tokio::time::sleep(Duration::from_secs_f64(failure_retry_interval)).await;
            self.state.transition(TargetState::Booting);
            let mut backend = self.build_power_control();
            if backend.pulse(Button::Reset, 500).await.is_ok() {
                self.logs.flush_boot_log();
                self.logs.mark_boot_start();
                self.detector.reset_cycle();
            }
        }
    }

    /// Set up the U-Boot prompt watcher and return a receiver (caller must
    /// release the engine lock before awaiting).
    pub fn queue_enter_uboot(
        &mut self,
    ) -> tokio::sync::mpsc::UnboundedReceiver<crate::boot_detector::WatcherMatch> {
        let pattern = crate::loader::default().prompt_watch_pattern().to_string();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.detector.add_watcher(&pattern, tx);
        rx
    }

    /// Set up a single-pattern wait and return a receiver (caller must
    /// release the engine lock before awaiting). Kept for tools that need
    /// the lock-released await pattern (e.g. `serial_enter_uboot`).
    pub fn queue_wait_pattern(
        &mut self,
        pattern: &str,
    ) -> tokio::sync::mpsc::UnboundedReceiver<crate::boot_detector::WatcherMatch> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.detector.add_watcher(pattern, tx);
        rx
    }

    /// Relay reset with flood-only priming (holds the lock, ~1 second).
    ///
    /// Pattern mode sends no interrupt byte here. The caller watches the
    /// configured regex and sends once after it matches.
    ///
    /// The key insight for bootdelay=0: SPL/BL31/OP-TEE don't read the
    /// serial port, so Ctrl-C chars just sit in the UART FIFO. When
    /// U-Boot's `abortboot()` finally calls `tstc()`, even ONE pending
    /// Ctrl-C is enough to interrupt. We send 1 byte per 100ms (not
    /// 100-byte floods) to avoid overflowing the 16-byte UART FIFO.
    pub async fn do_relay_reset(&mut self, strategy: InterruptStrategy) -> Result<(), String> {
        self.state.transition(TargetState::Booting);
        let ch = self.interrupt_char;
        let mut backend = self.build_power_control();
        if !backend.has_button(Button::Reset) {
            return Err("No reset control configured".into());
        }

        // Pattern mode must remain silent until its configured output regex
        // matches. Flood mode primes the UART before the reset.
        if strategy == InterruptStrategy::Flood && self.console.is_open() {
            let pre: Vec<u8> = vec![ch; 4];
            self.console.write_raw(&pre).await;
        }

        // Hold reset first, recreate the boot text/detector state, then
        // release. The first interrupt bytes therefore target only this boot.
        backend
            .press(Button::Reset)
            .await
            .map_err(|e| format!("reset press failed via {}: {e}", backend.name()))?;
        self.logs.flush_boot_log();
        self.logs.mark_boot_start();
        self.detector.reset_cycle();
        tokio::time::sleep(Duration::from_millis(self.config.reset_time_ms().max(500))).await;
        backend
            .release(Button::Reset)
            .await
            .map_err(|e| format!("reset release failed via {}: {e}", backend.name()))?;

        if strategy == InterruptStrategy::Flood {
            // Short post-reset burst (the long flood is done separately).
            for _ in 0..5 {
                if self.console.is_open() {
                    self.console.write_raw(&[ch]).await;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        Ok(())
    }

    /// Send one Ctrl-C byte (called in a loop by the enter-uboot tool,
    /// with the lock released between calls so the read loop can process
    /// U-Boot banners and trigger watchers).
    pub async fn flood_one(&mut self) {
        if self.console.is_open() {
            self.console.write_raw(&[self.interrupt_char]).await;
        }
    }

    /// Wait for a pattern with optional `force_prompt_wait` semantics
    /// (labgrid/lava: on timeout, send a newline to provoke a prompt and
    /// retry up to `max_probes` times at `timeout/10` each).
    ///
    /// When `probe_on_timeout` is true and the wait times out, this sends
    /// `"\n"` to the console and retries — useful for noisy consoles where
    /// kernel logs overlap the prompt. Mirrors lava's `force_prompt_wait`
    /// (shell.py:332) 6× `timeout/10` cadence.
    pub async fn wait_pattern_internal_opts(
        &mut self,
        pattern: &str,
        timeout: f64,
        probe_on_timeout: bool,
    ) -> serde_json::Value {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        self.detector.add_watcher(pattern, tx.clone());

        let max_probes = if probe_on_timeout { 6usize } else { 0 };
        let partial = if probe_on_timeout {
            timeout / 10.0
        } else {
            timeout
        };
        let mut elapsed = 0.0f64;
        let mut probes = 0usize;

        loop {
            let remaining = (timeout - elapsed).max(0.0);
            let wait = partial.min(remaining);
            let result = tokio::time::timeout(Duration::from_secs_f64(wait), rx.recv()).await;

            match result {
                Ok(Some(m)) => {
                    self.detector.remove_watcher_group(&tx);
                    return serde_json::json!({
                        "matched": true,
                        "matched_line": m.line,
                        "pattern_index": m.pattern_index,
                    });
                }
                Ok(None) => {
                    self.detector.remove_watcher_group(&tx);
                    return serde_json::json!({
                        "matched": false,
                        "matched_line": null,
                    });
                }
                Err(_) => {
                    elapsed += wait;
                    if elapsed >= timeout || probes >= max_probes {
                        self.detector.remove_watcher_group(&tx);
                        tracing::warn!(
                            "Pattern '{}' not detected within {:.0}s. Target state: {:?}. Check serial_get_state.",
                            pattern,
                            timeout,
                            self.state.current()
                        );
                        return serde_json::json!({
                            "matched": false,
                            "matched_line": null,
                            "probes_sent": probes,
                        });
                    }
                    // Provoke a fresh prompt (lava force_prompt_wait).
                    if self.console.is_open() {
                        tracing::info!(
                            "wait_pattern timeout {:.1}s, probing with newline (attempt {}/{})",
                            elapsed,
                            probes + 1,
                            max_probes
                        );
                        self.console.sendline("");
                        self.console.drain_writes().await;
                    }
                    probes += 1;
                }
            }
        }
    }

    /// Incrementally fetch new output (file-position based)
    pub fn poll_logs(&mut self) -> serde_json::Value {
        let path = match self.logs.current_path() {
            Some(p) => p.to_path_buf(),
            None => {
                return serde_json::json!({"lines": [], "since": 0});
            }
        };

        match std::fs::metadata(&path) {
            Ok(meta) => {
                let size = meta.len();
                if size <= self.poll_position {
                    return serde_json::json!({"lines": [], "since": self.poll_position});
                }
                match std::fs::File::open(&path) {
                    Ok(mut file) => {
                        use std::io::{Read, Seek, SeekFrom};
                        file.seek(SeekFrom::Start(self.poll_position)).ok();
                        let bytes_to_read = (size - self.poll_position) as usize;
                        let mut buf = vec![0u8; bytes_to_read];
                        let n = file.read(&mut buf).unwrap_or(0);
                        buf.truncate(n);
                        self.poll_position = size;
                        let content = String::from_utf8_lossy(&buf);
                        let lines: Vec<&str> = content.lines().collect();
                        serde_json::json!({
                            "lines": lines,
                            "since": self.poll_position,
                        })
                    }
                    Err(_) => serde_json::json!({"lines": [], "since": self.poll_position}),
                }
            }
            Err(_) => serde_json::json!({"lines": [], "since": self.poll_position}),
        }
    }

    /// Append key anchor lines to reference boot log and hot-reload StageLearner.
    pub fn append_reference(&mut self, lines: &str) -> serde_json::Value {
        let ref_path_str = self.config.reference_log();
        if ref_path_str.is_empty() {
            return serde_json::json!({"success": false, "error": "No reference_log configured. Add `reference_log` to .target.jsonc."});
        }
        let ref_path = std::path::PathBuf::from(&ref_path_str);
        // Ensure parent dir exists
        if let Some(parent) = ref_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        // Append lines to reference log
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&ref_path)
        {
            Ok(mut file) => {
                use std::io::Write;
                for line in lines.lines() {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        writeln!(file, "{trimmed}").ok();
                    }
                }
            }
            Err(e) => {
                return serde_json::json!({"success": false, "error": format!("Cannot open reference log: {e}")});
            }
        }
        // Hot-reload StageLearner
        let st = self.config.learner_stage_threshold();
        let ct = self.config.learner_crash_threshold();
        match self.detector.load_reference(&ref_path, st, ct) {
            Ok(()) => {
                let fp_count = self
                    .detector
                    .learner
                    .as_ref()
                    .map(|l| l.fingerprints.len())
                    .unwrap_or(0);
                serde_json::json!({
                    "success": true,
                    "message": format!("Appended lines, reloaded reference log ({fp_count} fingerprints)"),
                    "fingerprints": fp_count,
                    "reference_log_path": ref_path_str,
                })
            }
            Err(e) => {
                // Append succeeded but reload failed — still report partial success
                serde_json::json!({
                    "success": true,
                    "warning": format!("Lines appended but reload failed: {e}"),
                    "reference_log_path": ref_path_str,
                })
            }
        }
    }

    pub fn get_config(&self) -> serde_json::Value {
        let mut map = serde_json::Map::new();
        for (k, v) in &self.config.values {
            map.insert(k.clone(), serde_json::Value::String(v.clone()));
        }
        serde_json::Value::Object(map)
    }

    // ── Connection Learning ──────────────────────────────────────────────

    /// Run the hardware reset connection learning process.
    ///
    /// Requires relay configured. Executes reset→capture cycles (default 3,
    /// `cycles` override for the TEST budget — minimum 2, the learner's
    /// comparison floor), compares first 50 lines similarity. If ≥93%,
    /// generates reference_log. If <10%, relay is marked broken.
    #[tracing::instrument(skip(self))]
    pub async fn learn_connection_hardware(
        &mut self,
        reference_override: Option<&std::path::Path>,
        cycles: Option<usize>,
        quick: bool,
        use_power: bool,
    ) -> serde_json::Value {
        let mut power_ctrl = self.build_power_control();
        let cycle_button = if use_power {
            Button::Power
        } else {
            Button::Reset
        };
        if !power_ctrl.has_button(cycle_button) {
            return self.power_control_failure(if use_power { "power" } else { "reset" });
        }

        let dut_dir = self.config.dut_dir();
        let learn_dir = std::path::PathBuf::from(&dut_dir).join("learn");
        let ref_log = self.config.reference_log();
        let reference_log_path = if let Some(path) = reference_override {
            path.to_path_buf()
        } else if ref_log.is_empty() {
            std::path::PathBuf::from(&dut_dir).join("reference-boot.log")
        } else {
            std::path::PathBuf::from(&ref_log)
        };

        let mut learn_cfg = LearnConfig {
            learn_dir,
            reference_log: reference_log_path,
            ..Default::default()
        };
        if let Some(cycles) = cycles {
            // Defense in depth: the tool layer clamps via
            // LearnConnectionArgs::cycles_bounded(); clamp again here so
            // no other caller can drive unbounded reboot loops.
            learn_cfg.cycles = cycles.clamp(2, crate::tools::params::MAX_LEARN_CYCLES);
        }
        if quick {
            // The TEST button compares only this prefix. The meaningful
            // boot lines are front-loaded right after reset, so a short
            // upper bound plus a line-count early exit avoids waiting for
            // the continuously-chatty tail of the boot log.
            learn_cfg.capture_timeout_secs = 5.0;
            learn_cfg.capture_line_limit = Some(learn_cfg.compare_lines);
        }

        let existing_reference = quick
            .then(|| std::fs::read_to_string(&learn_cfg.reference_log).ok())
            .flatten();

        let learner = ConnectionLearner::new(learn_cfg);

        // Verify relay (non-fatal — relay may work even if verify fails)
        if let Err(e) = power_ctrl.verify().await {
            tracing::warn!("Relay verify failed: {e} — continuing anyway");
        }

        let result = if let Some(reference) = existing_reference {
            self.run_hardware_learning_with_ref(&learner, &reference, use_power)
                .await
        } else {
            self.run_hardware_learning(&learner, use_power).await
        };
        Self::format_learn_result(&result)
    }

    /// Run the software reboot connection learning process (fallback).
    ///
    /// Two modes:
    /// 1. No relay, no existing reference_log → N reboots (default 3), compare against each other
    /// 2. No relay, HAS existing reference_log → N reboots, compare each against existing ref
    pub async fn learn_connection_software(
        &mut self,
        reference_override: Option<&std::path::Path>,
        cycles: Option<usize>,
    ) -> serde_json::Value {
        let dut_dir = self.config.dut_dir();
        let learn_dir = std::path::PathBuf::from(&dut_dir).join("learn");
        let ref_log = self.config.reference_log();
        let reference_log_path = if let Some(path) = reference_override {
            path.to_path_buf()
        } else if ref_log.is_empty() {
            std::path::PathBuf::from(&dut_dir).join("reference-boot.log")
        } else {
            std::path::PathBuf::from(&ref_log)
        };

        // Check if an existing reference log exists for comparison
        let existing_ref = if reference_log_path.exists() {
            std::fs::read_to_string(&reference_log_path).ok()
        } else {
            None
        };

        let mut learn_cfg = LearnConfig {
            learn_dir,
            reference_log: reference_log_path,
            ..Default::default()
        };
        if let Some(cycles) = cycles {
            // Defense in depth: the tool layer clamps via
            // LearnConnectionArgs::cycles_bounded(); clamp again here so
            // no other caller can drive unbounded reboot loops.
            learn_cfg.cycles = cycles.clamp(2, crate::tools::params::MAX_LEARN_CYCLES);
        }

        let learner = ConnectionLearner::new(learn_cfg);

        let result = if let Some(existing_ref) = existing_ref {
            // Mode 2: compare each reboot cycle against existing reference log
            self.run_software_learning_with_ref(&learner, &existing_ref)
                .await
        } else {
            // Mode 1: compare reboot cycles against each other
            self.run_software_learning(&learner).await
        };
        Self::format_learn_result(&result)
    }

    /// Strictly verify the reset control selected by `.target.jsonc`.
    ///
    /// A protocol read-back alone is insufficient: assert reset, require a
    /// quiet serial window while held, then release and require fresh boot
    /// output. This catches a responsive relay wired to the wrong channel.
    #[tracing::instrument(skip(self))]
    pub async fn verify_relay(&mut self) -> serde_json::Value {
        let mut power_ctrl = self.build_power_control();
        if !power_ctrl.has_button(Button::Reset) {
            let mut body = self.power_control_failure("reset");
            body["relay_configured"] = serde_json::json!(false);
            return body;
        }

        let protocol_verified = match power_ctrl.verify().await {
            Ok(verified) => verified,
            Err(e) => {
                return serde_json::json!({
                    "success": false,
                    "verified": false,
                    "code": "relay_protocol_failed",
                    "error": e.to_string(),
                    "backend": power_ctrl.name(),
                });
            }
        };
        if !self.console.is_open() {
            return serde_json::json!({
                "success": false,
                "verified": false,
                "code": "serial_not_connected",
                "error": "Serial console must be connected for strict reset verification",
                "backend": power_ctrl.name(),
            });
        }
        if let Err(rejection) = self.try_acquire_relay_lock() {
            return rejection;
        }
        if let Err(e) = power_ctrl.press(Button::Reset).await {
            self.release_relay_lock();
            return serde_json::json!({
                "success": false,
                "verified": false,
                "code": "reset_press_failed",
                "error": e.to_string(),
                "backend": power_ctrl.name(),
            });
        }

        // Recreate the boot text only after reset is asserted, then discard
        // bytes already queued before the press.
        self.mark_boot_cycle();
        for _ in 0..3 {
            match self
                .console
                .read_available(Duration::from_millis(100), 4096)
                .await
            {
                Ok(data) if !data.is_empty() => continue,
                _ => break,
            }
        }

        let quiet_deadline = tokio::time::Instant::now() + Duration::from_millis(800);
        let mut held_output = Vec::new();
        let mut held_partial = Vec::new();
        while tokio::time::Instant::now() < quiet_deadline {
            if let Ok(data) = self
                .console
                .read_available(Duration::from_millis(100), 4096)
                .await
                && !data.is_empty()
            {
                held_output.extend_from_slice(&strip_ser2net_banner(&data, &mut held_partial));
            }
        }
        let output_while_held = held_output
            .iter()
            .any(|byte| !matches!(byte, b'\0' | b'\r' | b'\n' | b' ' | b'\t'));
        if output_while_held {
            let release_error = power_ctrl
                .release(Button::Reset)
                .await
                .err()
                .map(|error| error.to_string());
            self.release_relay_lock();
            return serde_json::json!({
                "success": false,
                "verified": false,
                "code": "reset_not_silent",
                "error": "DUT serial output continued while reset was held; reset channel or wiring is incorrect",
                "held_output_bytes": held_output.len(),
                "release_error": release_error,
                "backend": power_ctrl.name(),
            });
        }

        if let Err(e) = power_ctrl.release(Button::Reset).await {
            self.release_relay_lock();
            return serde_json::json!({
                "success": false,
                "verified": false,
                "code": "reset_release_failed",
                "error": e.to_string(),
                "backend": power_ctrl.name(),
            });
        }
        self.release_relay_lock();

        // One meaningful line proves that release produced fresh UART data.
        // Keep the 8s deadline for boards with a slow first byte, but stop
        // immediately once that proof arrives instead of recording a boot
        // window that this check never consumes.
        let released_output = self.capture_boot_output_until(8.0, Some(1)).await;
        let mut released_partial = Vec::new();
        let clean = strip_ser2net_banner(&released_output, &mut released_partial);
        let output_after_release = clean
            .iter()
            .any(|byte| !matches!(byte, b'\0' | b'\r' | b'\n' | b' ' | b'\t'));
        if !output_after_release {
            return serde_json::json!({
                "success": false,
                "verified": false,
                "code": "no_output_after_reset_release",
                "error": "No DUT serial output observed after reset release",
                "backend": power_ctrl.name(),
            });
        }

        serde_json::json!({
            "success": true,
            "verified": true,
            "protocol_verified": protocol_verified,
            "serial_silent_while_held": true,
            "serial_output_after_release": true,
            "released_output_bytes": clean.len(),
            "message": "Reset verified: serial quiet while held and boot output present after release.",
            "backend": power_ctrl.name(),
        })
    }

    /// Control a button (press/release/pulse) via the power control abstraction.
    pub async fn control_button(
        &mut self,
        button: &str,
        action: &str,
        delay_ms: Option<u64>,
    ) -> serde_json::Value {
        let btn = match button {
            "reset" => Button::Reset,
            "download" => Button::Download,
            _ => {
                return serde_json::json!({
                    "success": false,
                    "error": format!("Unknown button: {button}. Valid: reset, download")
                });
            }
        };

        let mut backend = self.build_power_control();
        if !backend.has_button(btn) {
            return self.power_control_failure(button);
        }
        if let Err(rejection) = self.try_acquire_relay_lock() {
            return rejection;
        }

        match action {
            // Feedback-confirming variants: Ok is returned only when the
            // HARDWARE reports the new state — a lost command (rapid
            // consecutive switching, flaky ser2net) is retried underneath
            // instead of silently dropped.
            "press" => match backend.press_verified(btn).await {
                Ok(()) => {
                    self.release_relay_lock();
                    serde_json::json!({"success": true, "button": button, "action": "press", "backend": backend.name()})
                }
                Err(e) => {
                    self.release_relay_lock();
                    serde_json::json!({"success": false, "error": format!("{e}")})
                }
            },
            "release" => match backend.release_verified(btn).await {
                Ok(()) => {
                    self.release_relay_lock();
                    serde_json::json!({"success": true, "button": button, "action": "release", "backend": backend.name()})
                }
                Err(e) => {
                    self.release_relay_lock();
                    serde_json::json!({"success": false, "error": format!("{e}")})
                }
            },
            "pulse" => {
                let ms = delay_ms.unwrap_or(500);
                match backend.pulse(btn, ms).await {
                    Ok(()) => {
                        self.release_relay_lock();
                        serde_json::json!({"success": true, "button": button, "action": "pulse", "delay_ms": ms, "backend": backend.name()})
                    }
                    Err(e) => {
                        self.release_relay_lock();
                        serde_json::json!({"success": false, "error": format!("{e}")})
                    }
                }
            }
            _ => {
                self.release_relay_lock();
                serde_json::json!({
                    "success": false,
                    "error": format!("Unknown action: {action}. Valid: press, release, pulse")
                })
            }
        }
    }

    // ── Internal learning helpers ───────────────────────────────────────

    /// The effective interrupt strategy: per-call override → per-DUT config
    /// → FLOOD. Explicit unsupported values are errors.
    pub fn resolve_interrupt_strategy(
        &self,
        arg: Option<&str>,
    ) -> Result<InterruptStrategy, String> {
        if let Some(a) = arg {
            return InterruptStrategy::parse(a).ok_or_else(|| {
                format!("unsupported interrupt_strategy {a:?}; valid values: flood, pattern")
            });
        }
        let cfg = self.config.get("UBOOT_INTERRUPT_STRATEGY");
        match InterruptStrategy::parse(cfg) {
            Some(s) => Ok(s),
            None if cfg.trim().is_empty() => Ok(InterruptStrategy::Flood),
            None => Err(format!(
                "unsupported UBOOT_INTERRUPT_STRATEGY {cfg:?}; valid values: flood, pattern"
            )),
        }
    }

    /// Build a power control backend from the current config.
    ///
    /// Goes through the dut-ctrl PLUGIN registry: the
    /// `.target.jsonc` `relay.type` name selects a registered backend
    /// factory; unknown names yield the disabled backend. The ch340 plugin
    /// is built-in; future backends (SNMP PDU, GPIO, ...) register
    /// themselves without touching this code.
    fn build_power_control(&self) -> Box<dyn PowerControl> {
        crate::power_control::build_backend(&PowerControlConfig {
            backend: self.config.relay_type(),
            host: self.config.relay_ip(),
            port: self.config.relay_port(),
            channels: ButtonChannels {
                reset: self.config.reset_channel(),
                download: self.config.download_channel(),
                power: self.config.power_channel(),
            },
            reset_time_ms: self.config.reset_time_ms(),
            power_off_time_ms: self.config.power_off_time_ms(),
            dut_name: self.config.get_str_or("DUT_NAME", "default"),
        })
    }

    pub(crate) fn power_control_configured(&self) -> bool {
        let backend = self.build_power_control();
        [Button::Reset, Button::Download, Button::Power]
            .into_iter()
            .any(|button| backend.has_button(button))
    }

    pub(crate) fn reset_control_configured(&self) -> bool {
        self.build_power_control().has_button(Button::Reset)
    }

    /// A dedicated power channel exists, so `serial_power_cycle`'s
    /// OFF → wait → ON sequence can actually run.
    pub(crate) fn power_cycle_configured(&self) -> bool {
        self.build_power_control().has_button(Button::Power)
    }

    /// WHY power control is unavailable, for the unified no-dev_ctrl
    /// failure contract (the agent must see the
    /// DUT name, the reason and the exact fix instead of guessing).
    fn power_control_reason(&self) -> (&'static str, String) {
        let name = self.config.get_str_or("DUT_NAME", "default");
        let raw = self.config.get("RELAY_TYPE");
        let raw = if raw.is_empty() { None } else { Some(raw) };
        match raw {
            None => (
                "unset",
                format!(
                    "DUT '{name}' has no [dut.relay] section in .target.jsonc — no power-control backend is selected."
                ),
            ),
            Some(t @ ("none" | "disabled")) => (
                "disabled",
                format!(
                    "DUT '{name}' has power control explicitly disabled (relay.type='{t}' in .target.jsonc). To enable it, set relay.type to ch340-relay and its channel(s)."
                ),
            ),
            Some(t) => (
                "no_channels",
                format!(
                    "DUT '{name}' uses relay.type='{t}' but no channel is configured for this button in [dut.relay]."
                ),
            ),
        }
    }

    /// The unified structured body every control tool returns when no power
    /// control is configured. Tool-level isError (NOT a JSON-RPC error):
    /// the tool ran and answered a domain question — a soft failure.
    fn power_control_failure(&self, button: &str) -> serde_json::Value {
        let name = self.config.get_str_or("DUT_NAME", "default");
        let (reason, error) = self.power_control_reason();
        serde_json::json!({
            "success": false,
            "code": "power_control_not_configured",
            "error": error,
            "dut_name": name,
            "relay_type": self.config.relay_type(),
            "reason": reason,
            "button": button,
            "fix": {
                "config_file": ".target.jsonc",
                "sections": ["dut.relay"],
                "example": "\"relay\": { \"type\": \"ch340-relay\", \"reset_ch\": 1, \"power_ch\": 2 }"
            },
            "hint": "Works without power control: serial_enter_uboot with restart=software or restart=none. Configure [dut.relay], then restart the MCP server."
        })
    }

    /// Send raw bytes to the serial port — no markers, no command wrapping.
    /// Data goes to write channel; drained by read loop on next iteration.
    pub fn send_raw(&mut self, data: &str) -> serde_json::Value {
        let bytes = data.as_bytes().to_vec();
        let len = bytes.len();
        let _ = self.console.write_sender().try_send(bytes);
        serde_json::json!({"success": true, "bytes_sent": len})
    }

    /// Get a cloned write sender for WebSocket relay (no mutex needed).
    pub fn get_write_sender(&self) -> crate::console::ConsoleWriteSender {
        self.console.write_sender()
    }

    /// Block Agent tools as soon as a human WebSocket takeover begins, before
    /// any terminal byte is written or stale input is drained.
    pub fn begin_manual_session(&mut self) {
        self.state
            .transition(crate::state_manager::TargetState::Dutabo);
        self.state.write_dutabo_forced();
        // Mirror the takeover into the lock-free guard flags: call_tool reads
        // this atomic because it cannot take the engine mutex while the read
        // loop parks on wait_readable.
        self.flags.dutabo_taken.store(1, Ordering::Relaxed);
    }

    /// Subscribe to the passive serial monitor feed (broadcast clone).
    pub fn monitor_sender(&self) -> tokio::sync::broadcast::Sender<Vec<u8>> {
        self.monitor_tx.clone()
    }

    /// Set WebSocket broadcast channel for dutabo serial relay.
    /// Transitions state to Dutabo so the statusline reflects "in use".
    pub fn set_ws_tx(&mut self, tx: tokio::sync::broadcast::Sender<Vec<u8>>) {
        self.ws_tx = Some(tx);
        self.begin_manual_session();
    }

    /// Clear WebSocket broadcast channel and restore to Active.
    pub fn clear_ws_tx(&mut self) {
        self.ws_tx = None;
        self.state
            .transition(crate::state_manager::TargetState::Active);
        // Ownership returns to the engine — open the MCP tool guard again.
        self.flags.dutabo_taken.store(0, Ordering::Relaxed);
    }

    /// Mark the start of a learn boot cycle: state transition + per-cycle
    /// log/detector rotation (shared by all three learning drivers).
    fn mark_boot_cycle(&mut self) {
        self.state.transition(TargetState::Booting);
        self.logs.flush_boot_log();
        self.logs.mark_boot_start();
        self.detector.reset_cycle();
    }

    /// Record one learning cycle: write the raw capture to
    /// `learn-<ts>-<n>.log`, extract + normalize the first-N comparison
    /// window, and push the LearnCycle (shared tail of every learning
    /// driver — previously three near-identical copies).
    fn finish_learn_cycle(
        learner: &ConnectionLearner,
        cycles: &mut Vec<crate::connection_learner::LearnCycle>,
        index: usize,
        method: LearnMethod,
        raw: &[u8],
        label: &str,
    ) {
        let cfg = learner.config();
        let ts = chrono::Local::now().format("%Y%m%d_%H%M%S");
        let log_path = cfg.learn_dir.join(format!("learn-{ts}-{}.log", index + 1));
        if let Err(error) = prune_learn_logs(&cfg.learn_dir) {
            tracing::warn!("Failed to prune learning logs before capture: {error}");
        }
        let stored = &raw[..raw.len().min(MAX_LEARN_LOG_FILE_BYTES)];
        if let Err(error) = std::fs::write(&log_path, stored) {
            tracing::warn!(path = %log_path.display(), "Failed to write learning log: {error}");
        }
        if let Err(error) = prune_learn_logs(&cfg.learn_dir) {
            tracing::warn!("Failed to prune learning logs after capture: {error}");
        }

        let full_content = String::from_utf8_lossy(stored).to_string();
        let first_50 = ConnectionLearner::extract_first_n_lines(&full_content, cfg.compare_lines);
        let normalized = ConnectionLearner::normalize(&first_50);

        cycles.push(crate::connection_learner::LearnCycle {
            log_path,
            first_50: normalized,
            full_content,
            method,
        });

        tracing::info!(
            "{label} learn cycle {}/{}: {} bytes captured",
            index + 1,
            cfg.cycles,
            stored.len()
        );
    }

    /// Run hardware reset learning cycles inline (holds engine lock).
    ///
    /// The cycle count is only the sampling budget — the loop stops as soon
    /// as the collected cycles' similarity reaches the threshold (≥93%),
    /// skipping the remaining cycles (user rule: the cycle count is
    /// irrelevant; similarity ≥93% is the pass criterion).
    #[tracing::instrument(skip(self, learner))]
    async fn run_hardware_learning(
        &mut self,
        learner: &ConnectionLearner,
        use_power: bool,
    ) -> LearnResult {
        let cfg = learner.config();
        let num_cycles = cfg.cycles;
        let mut cycles = Vec::with_capacity(num_cycles);
        std::fs::create_dir_all(&cfg.learn_dir).ok();

        for i in 0..num_cycles {
            let mut captured = match self
                .run_hardware_cycle(i + 1, num_cycles, learner, use_power)
                .await
            {
                Ok(captured) => captured,
                Err(error) => {
                    return LearnResult {
                        connected: false,
                        cycles,
                        best_similarity: 0.0,
                        reference_log: None,
                        method: LearnMethod::HardwareReset,
                        relay_verified: false,
                        error: Some(error),
                    };
                }
            };
            // An EMPTY capture means the reset pulse did not take (a relay
            // command can get lost in rapid consecutive switching): the
            // board kept running silently while the capture window waited.
            // Comparing 60KB against 0 bytes is garbage — retake the whole
            // cycle once after a settle gap.
            if captured.is_empty() {
                tracing::warn!(
                    "Learn cycle {}/{} captured 0 bytes — reset did not take, retaking",
                    i + 1,
                    num_cycles
                );
                tokio::time::sleep(Duration::from_secs(2)).await;
                captured = match self
                    .run_hardware_cycle(i + 1, num_cycles, learner, use_power)
                    .await
                {
                    Ok(captured) => captured,
                    Err(error) => {
                        return LearnResult {
                            connected: false,
                            cycles,
                            best_similarity: 0.0,
                            reference_log: None,
                            method: LearnMethod::HardwareReset,
                            relay_verified: false,
                            error: Some(error),
                        };
                    }
                };
            }
            Self::finish_learn_cycle(
                learner,
                &mut cycles,
                i,
                LearnMethod::HardwareReset,
                &captured,
                "Hardware",
            );
            if learner.threshold_met(&cycles) {
                tracing::info!(
                    "Hardware learn threshold met after {}/{} cycles — stopping early",
                    i + 1,
                    num_cycles
                );
                break;
            }
            // Settle gap between cycles: rapid consecutive press/release
            // pairs can lose a relay command (press issued while the
            // previous release is still settling = the reset never takes).
            if i + 1 < num_cycles {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }

        learner.evaluate(cycles, LearnMethod::HardwareReset)
    }

    /// TEST-button profile: capture one bounded boot prefix and compare it to
    /// the existing reference. This validates the physical reset and serial
    /// path without spending two full boot windows or rewriting a good
    /// reference during a diagnostic run.
    #[tracing::instrument(skip(self, learner, reference))]
    async fn run_hardware_learning_with_ref(
        &mut self,
        learner: &ConnectionLearner,
        reference: &str,
        use_power: bool,
    ) -> LearnResult {
        let cfg = learner.config();
        std::fs::create_dir_all(&cfg.learn_dir).ok();

        let captured = match self.run_hardware_cycle(1, 1, learner, use_power).await {
            Ok(captured) => captured,
            Err(error) => {
                return LearnResult {
                    connected: false,
                    cycles: Vec::new(),
                    best_similarity: 0.0,
                    reference_log: None,
                    method: LearnMethod::HardwareReset,
                    relay_verified: false,
                    error: Some(error),
                };
            }
        };
        if captured.is_empty() {
            return LearnResult {
                connected: false,
                cycles: Vec::new(),
                best_similarity: 0.0,
                reference_log: None,
                method: LearnMethod::HardwareReset,
                relay_verified: false,
                error: Some("No serial data captured after hardware reset".into()),
            };
        }

        let mut cycles = Vec::with_capacity(1);
        Self::finish_learn_cycle(
            learner,
            &mut cycles,
            0,
            LearnMethod::HardwareReset,
            &captured,
            "Quick hardware",
        );
        let reference_prefix =
            ConnectionLearner::extract_first_n_lines(reference, cfg.compare_lines);
        let normalized_reference = ConnectionLearner::normalize(&reference_prefix);
        let similarity = crate::connection_learner::compute_similarity(
            &normalized_reference,
            &cycles[0].first_50,
        );
        let connected = similarity >= cfg.similarity_threshold;
        let relay_verified = similarity >= cfg.relay_broken_threshold;
        tracing::info!(
            similarity_percent = similarity * 100.0,
            threshold_percent = cfg.similarity_threshold * 100.0,
            "Quick reference validation complete"
        );

        LearnResult {
            connected,
            cycles,
            best_similarity: similarity,
            reference_log: connected.then(|| cfg.reference_log.clone()),
            method: LearnMethod::HardwareReset,
            relay_verified,
            error: (!connected).then(|| {
                format!(
                    "Similarity {:.1}% below threshold {:.1}%",
                    similarity * 100.0,
                    cfg.similarity_threshold * 100.0
                )
            }),
        }
    }

    /// One reset→capture cycle. Its own span so the Perfetto trace shows each
    /// cycle's duration individually (cycle N of M).
    #[tracing::instrument(skip(self, learner), fields(cycle = %cycle, of = %of))]
    async fn run_hardware_cycle(
        &mut self,
        cycle: usize,
        of: usize,
        learner: &ConnectionLearner,
        use_power: bool,
    ) -> Result<Vec<u8>, String> {
        // Relay endpoint lock: the learn cycle spans seconds (press, hold,
        // capture) and MUST hold the relay exclusively — every other relay
        // operation (any MCP's serial_button/verify/reset) acquires the
        // same lock and will be told the relay is busy instead of silently
        // interleaving CH340 frames (the "Port already in use"/lost-command
        // class of failures).
        let lock_dir = self.config.lock_dir();
        let lock_host = format!("relay:{}", self.config.relay_ip());
        if let Some(owner) =
            lock_manager::acquire_lock(&lock_host, &self.config.relay_port().to_string(), &lock_dir)
        {
            return Err(format!(
                "relay endpoint busy: held by another live process (pid {owner}) — \
                 the reference learn needs exclusive relay access"
            ));
        }
        let _relay_guard = RelayLockGuard {
            host: lock_host,
            port: self.config.relay_port().to_string(),
            lock_dir,
        };
        // Start the configured hardware cycle. Reset can be held while stale
        // bytes are drained; power uses the backend's OFF-wait-ON pulse.
        let mut backend = self.build_power_control();
        let cycle_button = if use_power {
            Button::Power
        } else {
            Button::Reset
        };
        if !backend.has_button(cycle_button) {
            return Err("Power control not configured (see serial_get_state.power_control)".into());
        }
        if use_power {
            backend
                .press_verified(Button::Power)
                .await
                .map_err(|error| format!("Power-off failed via {}: {error}", backend.name()))?;
            self.mark_boot_cycle();
            for _ in 0..3 {
                match self
                    .console
                    .read_available(Duration::from_millis(100), 4096)
                    .await
                {
                    Ok(data) if !data.is_empty() => continue,
                    _ => break,
                }
            }
            tokio::time::sleep(Duration::from_millis(
                self.config.power_off_time_ms().max(3000),
            ))
            .await;
            backend
                .release_verified(Button::Power)
                .await
                .map_err(|error| format!("Power-on failed via {}: {error}", backend.name()))?;
        } else {
            // Press with backoff-retries: an immediate re-connect after the
            // PREVIOUS relay op just closed can be rejected while ser2net
            // finishes cleaning up (observed: 0.5s-gap learn → press failed).
            let mut press_failed: Option<String> = None;
            for attempt in 0u32..4 {
                match backend.press_verified(Button::Reset).await {
                    Ok(()) => {
                        press_failed = None;
                        break;
                    }
                    Err(e) => {
                        press_failed = Some(e.to_string());
                        let backoff = 1500u64 * u64::from(attempt + 1);
                        tracing::warn!(
                            "Reset press attempt {}/{} failed ({e}) — retrying in {backoff}ms",
                            attempt + 1,
                            4
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(backoff)).await;
                    }
                }
            }
            if let Some(e) = press_failed {
                return Err(format!("Reset press failed via {}: {e}", backend.name()));
            }
            // Only now is the DUT held quiet. Recreate the capture boundary
            // and drain bytes queued from the previous boot before release.
            self.mark_boot_cycle();
            for _ in 0..3 {
                match self
                    .console
                    .read_available(Duration::from_millis(100), 4096)
                    .await
                {
                    Ok(data) if !data.is_empty() => continue,
                    _ => break,
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(
                // 500ms (the old floor) did not reliably reset the board: the
                // second learn cycle then captured the PREVIOUS boot's
                // continuation and the pairwise similarity was garbage. Hold at
                // least 1.5s.
                learner.config().reset_pulse_ms.max(1500),
            ))
            .await;
            if let Err(e) = backend.release_verified(Button::Reset).await {
                return Err(format!("Reset release failed via {}: {e}", backend.name()));
            }
        }

        // Phase 2: capture the newly released boot.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let captured = self
            .capture_boot_output_until(
                learner.config().capture_timeout_secs,
                learner.config().capture_line_limit,
            )
            .await;
        Ok(captured)
    }

    /// Run software reboot learning cycles inline.
    async fn run_software_learning(&mut self, learner: &ConnectionLearner) -> LearnResult {
        let cfg = learner.config();
        let num_cycles = cfg.cycles;
        let mut cycles = Vec::with_capacity(num_cycles);
        std::fs::create_dir_all(&cfg.learn_dir).ok();

        for i in 0..num_cycles {
            // Phase 1: send reboot
            self.mark_boot_cycle();
            self.console.sendline("reboot");
            self.console.drain_writes().await;

            // Phase 2: wait for boot to settle then capture
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let captured = self
                .capture_boot_output(learner.config().capture_timeout_secs)
                .await;

            // Prepend "reboot" marker
            let mut full_data = b"reboot\n".to_vec();
            full_data.extend_from_slice(&captured);
            Self::finish_learn_cycle(
                learner,
                &mut cycles,
                i,
                LearnMethod::SoftwareReboot,
                &full_data,
                "Software",
            );
            // Early exit: the cycle count is only the budget — stop the
            // moment the similarity threshold (≥93%) is met.
            if learner.threshold_met(&cycles) {
                tracing::info!(
                    "Software learn threshold met after {}/{} cycles — stopping early",
                    i + 1,
                    num_cycles
                );
                break;
            }
        }

        learner.evaluate(cycles, LearnMethod::SoftwareReboot)
    }

    /// Run software reboot learning comparing each cycle against an existing reference log.
    async fn run_software_learning_with_ref(
        &mut self,
        learner: &ConnectionLearner,
        reference_text: &str,
    ) -> LearnResult {
        let cfg = learner.config();
        let num_cycles = cfg.cycles;
        let mut cycles = Vec::with_capacity(num_cycles);
        std::fs::create_dir_all(&cfg.learn_dir).ok();

        let ref_normalized = ConnectionLearner::normalize(
            &ConnectionLearner::extract_first_n_lines(reference_text, cfg.compare_lines),
        );

        for i in 0..num_cycles {
            // Send reboot
            self.mark_boot_cycle();
            self.console.sendline("reboot");
            self.console.drain_writes().await;

            // Wait for boot then capture
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let captured = self.capture_boot_output(cfg.capture_timeout_secs).await;

            let mut full_data = b"reboot\n".to_vec();
            full_data.extend_from_slice(&captured);
            Self::finish_learn_cycle(
                learner,
                &mut cycles,
                i,
                LearnMethod::SoftwareReboot,
                &full_data,
                "Software+ref",
            );
            // Early exit: every cycle so far matches the reference at ≥93%
            // — more cycles cannot raise the minimum, so stop (the cycle
            // count is only the budget, the similarity is the criterion).
            let min_so_far = cycles
                .iter()
                .map(|c| {
                    crate::connection_learner::compute_similarity(&c.first_50, &ref_normalized)
                })
                .fold(1.0f64, f64::min);
            if min_so_far >= cfg.similarity_threshold {
                tracing::info!(
                    "Software+ref learn threshold met after {}/{} cycles — stopping early",
                    i + 1,
                    num_cycles
                );
                break;
            }
        }

        // Evaluate: compute similarity of each cycle against the reference
        let min_similarity = cycles
            .iter()
            .map(|c| crate::connection_learner::compute_similarity(&c.first_50, &ref_normalized))
            .fold(1.0f64, f64::min);

        let connected = min_similarity >= cfg.similarity_threshold;
        let reference_log = if connected {
            // Don't overwrite existing reference — it's our ground truth
            Some(cfg.reference_log.clone())
        } else {
            None
        };

        LearnResult {
            connected,
            cycles,
            best_similarity: min_similarity,
            reference_log,
            method: LearnMethod::SoftwareReboot,
            relay_verified: false,
            error: if !connected {
                Some(format!(
                    "Similarity {:.1}% below threshold {:.1}% against existing reference",
                    min_similarity * 100.0,
                    cfg.similarity_threshold * 100.0
                ))
            } else {
                None
            },
        }
    }

    /// Count of non-empty lines in raw serial output (CR stripped, blank
    /// lines skipped) — the learner's "meaningful line" notion for its
    /// compare window. PURE.
    pub fn meaningful_line_count(data: &[u8]) -> usize {
        String::from_utf8_lossy(data)
            .split('\n')
            .map(|l| l.trim_end_matches('\r').trim())
            .filter(|l| !l.is_empty())
            .count()
    }

    /// Capture serial output until silence or max timeout.
    /// Returns all bytes read during the capture window.
    ///
    /// `capture_boot_output_until` additionally accepts a meaningful-line
    /// limit for callers that consume only a fixed boot prefix.
    #[tracing::instrument(skip(self), fields(timeout = timeout_secs))]
    async fn capture_boot_output(&mut self, timeout_secs: f64) -> Vec<u8> {
        self.capture_boot_output_until(timeout_secs, None).await
    }

    #[tracing::instrument(skip(self), fields(timeout = timeout_secs, line_limit))]
    async fn capture_boot_output_until(
        &mut self,
        timeout_secs: f64,
        line_limit: Option<usize>,
    ) -> Vec<u8> {
        let mut captured = Vec::new();
        let started = tokio::time::Instant::now();
        let deadline = started + std::time::Duration::from_secs_f64(timeout_secs);
        let silence_timeout = std::time::Duration::from_secs(5); // 5s silence = boot done
        // Minimum window: a boot front-loads 50+ lines of
        // DDR/Download spam in its FIRST second, so an early silence/exit
        // would capture only the early-boot burst (learn cycles then
        // compare a 2s fragment against a 25s full boot → similarity
        // garbage). Every cycle covers at least this much boot.
        const MIN_CAPTURE_SECS: u64 = 10;

        let mut last_data = tokio::time::Instant::now();

        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }

            let read_timeout = remaining.min(std::time::Duration::from_millis(500));
            match self.console.read_available(read_timeout, 4096).await {
                Ok(data) if !data.is_empty() => {
                    last_data = tokio::time::Instant::now();
                    let clean = crate::serial_engine::strip_ser2net_banner(
                        &data,
                        &mut self.ser2net_partial,
                    );
                    if !clean.is_empty() {
                        self.logs.write(&clean);
                    }
                    self.state.on_activity();
                    let events = self.detector.feed(&clean);
                    self.handle_boot_events(events).await;
                    captured.extend_from_slice(&data);
                    if line_limit.is_some_and(|limit| {
                        limit > 0 && Self::meaningful_line_count(&captured) >= limit
                    }) {
                        break;
                    }
                }
                Ok(_) => {
                    // Boot complete: data HAS arrived and the board went
                    // quiet past the minimum window. A fixed-time or
                    // line-count exit would sample different boot phases per
                    // cycle (a 6KB early-boot fragment vs a 46KB mid-boot
                    // window) and turn the learn similarity verdict into
                    // garbage — only natural boot completion is comparable.
                    if !captured.is_empty()
                        && started.elapsed().as_secs() >= MIN_CAPTURE_SECS
                        && last_data.elapsed() > silence_timeout
                    {
                        break;
                    }
                }
                Err(_) => break,
            }
        }

        captured
    }

    /// Format LearnResult into JSON.
    fn format_learn_result(result: &LearnResult) -> serde_json::Value {
        let cycles_json: Vec<serde_json::Value> = result
            .cycles
            .iter()
            .map(|c| {
                serde_json::json!({
                    "log_path": c.log_path.to_string_lossy(),
                    "method": match c.method {
                        LearnMethod::HardwareReset => "hardware_reset",
                        LearnMethod::SoftwareReboot => "software_reboot",
                    },
                    "size_bytes": c.full_content.len(),
                })
            })
            .collect();

        let method_str = match result.method {
            LearnMethod::HardwareReset => "hardware_reset",
            LearnMethod::SoftwareReboot => "software_reboot",
        };

        let mut resp = serde_json::json!({
            "success": result.connected,
            "connected": result.connected,
            "method": method_str,
            "best_similarity": format!("{:.1}%", result.best_similarity * 100.0),
            "similarity_threshold": "93.0%",
            "cycles": cycles_json,
            "relay_verified": result.relay_verified,
        });

        if let Some(ref path) = result.reference_log {
            resp["reference_log"] = serde_json::Value::String(path.to_string_lossy().to_string());
        }
        if let Some(ref err) = result.error {
            resp["error"] = serde_json::Value::String(err.clone());
        }

        resp
    }
}

/// Wait for a watcher pattern with optional force_prompt_wait (lava semantics).
///
/// This is a **free function** — no engine lock is held during the `.await`.
/// The caller owns the watcher lifecycle (add watcher before calling, remove after).
/// On timeout with `probe_on_timeout=true`, sends `"\n"` via `console_tx`
/// to provoke a fresh prompt, retrying up to 6 times at `timeout/10` each.
/// Mirrors lava's `force_prompt_wait` (shell.py:332).
pub async fn wait_pattern_with_probe(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<crate::boot_detector::WatcherMatch>,
    timeout: f64,
    probe_on_timeout: bool,
    console_tx: &crate::console::ConsoleWriteSender,
) -> serde_json::Value {
    let max_probes = if probe_on_timeout { 6usize } else { 0 };
    let partial = if probe_on_timeout {
        timeout / 10.0
    } else {
        timeout
    };
    let mut elapsed = 0.0f64;
    let mut probes = 0usize;

    loop {
        let remaining = (timeout - elapsed).max(0.0);
        let wait = partial.min(remaining);
        let result =
            tokio::time::timeout(std::time::Duration::from_secs_f64(wait), rx.recv()).await;

        match result {
            Ok(Some(m)) => {
                return serde_json::json!({
                    "matched": true,
                    "matched_line": m.line,
                    "pattern_index": m.pattern_index,
                });
            }
            Ok(None) => {
                return serde_json::json!({
                    "matched": false,
                    "matched_line": null,
                });
            }
            Err(_) => {
                elapsed += wait;
                if elapsed >= timeout || probes >= max_probes {
                    return serde_json::json!({
                        "matched": false,
                        "matched_line": null,
                        "probes_sent": probes,
                    });
                }
                // Provoke a fresh prompt (lava force_prompt_wait).
                probes += 1;
                tracing::info!(
                    "wait_pattern timeout {:.1}s, probing with newline (attempt {}/{})",
                    elapsed,
                    probes,
                    max_probes
                );
                let _ = console_tx.try_send(b"\n".to_vec());
            }
        }
    }
}

/// Filter the ser2net connection banner
///
/// Handles cross-chunk splits: if a ser2net banner line is split across two
/// read() boundaries, a partial-line buffer stores the trailing incomplete
/// line and prepends it to the next call, so the full line is filtered.
/// Extract a human-readable message from a caught panic payload
/// (`catch_unwind` recovery shared by the detector and command-queue feeds).
fn panic_message(e: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = e.downcast_ref::<String>() {
        s.clone()
    } else if let Some(s) = e.downcast_ref::<&str>() {
        s.to_string()
    } else {
        "unknown panic".to_string()
    }
}

/// Space-separated hex preview of the first `max` bytes (read-loop logging).
fn hex_preview(data: &[u8], max: usize) -> String {
    data.iter()
        .take(max)
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn strip_ser2net_banner(data: &[u8], partial: &mut Vec<u8>) -> Vec<u8> {
    // Prepend any buffered partial line from the previous chunk.
    let combined = if partial.is_empty() {
        data.to_vec()
    } else {
        let mut c = std::mem::take(partial);
        c.extend_from_slice(data);
        c
    };

    let mut output = Vec::with_capacity(combined.len());
    let mut start = 0;
    for (index, byte) in combined.iter().enumerate() {
        if *byte != b'\n' {
            continue;
        }
        let line = &combined[start..=index];
        if !String::from_utf8_lossy(line).contains("ser2net port") {
            output.extend_from_slice(line);
        }
        start = index + 1;
    }

    partial.extend_from_slice(&combined[start..]);
    output
}

/// Parse the metadata line emitted by ser2net's banner mode, for example:
/// `ser2net port tcp,<configured-port> device serialdev, /dev/ttyUSB0, 9600n81,local`.
fn parse_ser2net_banner(data: &[u8]) -> Option<Ser2netBanner> {
    let text = String::from_utf8_lossy(data);
    let line = text
        .lines()
        .find(|line| line.contains("ser2net port tcp,"))?;
    let after_port = line.split_once("ser2net port tcp,")?.1;
    let (port, after_device_label) = after_port.split_once(" device serialdev,")?;
    let mut fields = after_device_label.split(',').map(str::trim);
    let device = fields.next()?.to_string();
    let serial_mode = fields.next().unwrap_or_default();
    let baud_digits: String = serial_mode
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect();
    Some(Ser2netBanner {
        port: port.trim().to_string(),
        device,
        baudrate: baud_digits.parse().ok(),
    })
}

fn endpoint_conflict(config: &Config) -> Option<serde_json::Value> {
    let relay_port = config.relay_port();
    if relay_port == 0
        || config.serial_target() != relay_port.to_string()
        || config.dev_host_ip() != config.relay_ip()
    {
        return None;
    }
    Some(serde_json::json!({
        "code": "serial_relay_endpoint_conflict",
        "message": format!(
            "Serial and relay both select {}:{}; choose the ser2net console port for [dut.serial]",
            config.dev_host_ip(), config.serial_target()
        ),
        "configured": {
            "serial": { "host": config.dev_host_ip(), "port": config.serial_target() },
            "relay": { "host": config.relay_ip(), "port": relay_port },
        },
        "hint": ".target.jsonc is user-owned; correct serial.port to the actual ser2net console endpoint",
    }))
}

/// Strip the Android kernel log prefix `[ 1234.567890][  T123]`
fn strip_android_klog(data: &[u8]) -> Vec<u8> {
    use std::sync::LazyLock;
    static RE: LazyLock<regex::bytes::Regex> =
        LazyLock::new(|| regex::bytes::Regex::new(r"(?m)^\[\s*\d+\.\d+\]\[\s*T\d+\]\s*").unwrap());
    RE.replace_all(data, b"" as &[u8]).into_owned()
}

/// Lock-free mirror of the two engine-ownership windows that gate MCP tools.
///
/// The read loop parks on `wait_readable(100ms)` while holding the engine
/// mutex (~91% of every idle cycle), so `call_tool` must NOT gate on
/// `try_lock()` — a busy-but-healthy engine would look identical to a
/// startup probe in flight. The engine keeps these atomics current at every
/// transition, and the handler guard reads them without touching the mutex;
/// tool bodies themselves queue on `lock().await` as usual.
#[derive(Debug)]
pub struct EngineFlags {
    /// True from engine construction until `start_shared` finishes (success,
    /// failure, or timeout) — the genuine "connecting" window, not
    /// "read loop busy".
    pub startup_in_progress: AtomicBool,
    /// 1 while an interactive dutabo session owns the serial console
    /// (WebSocket takeover). Cleared when the session's WS channel closes,
    /// which is exactly when ownership returns to the engine.
    pub dutabo_taken: AtomicU8,
    /// True while THIS engine holds the serial endpoint lock — the lock-free
    /// mirror of `SerialEngine::owns_serial_lock`. Maintained at
    /// start/claim/stop/yield/resume so the tool guard can reject
    /// non-owner control without touching the engine mutex.
    pub owns_serial_lock: AtomicBool,
    /// PID of the process holding the endpoint lock when we do NOT own it
    /// (0 when we own it or no conflict is known) — surfaced in
    /// non-owner tool rejections so the agent sees who occupies the DUT.
    pub serial_owner_pid: AtomicU32,
}

/// Engine wrapper for shared access between read loop and MCP handler.
///
/// `Deref` to `Mutex<SerialEngine>` keeps every existing
/// `engine.lock().await` call site working unchanged; `flags` additionally
/// exposes the lock-free [`EngineFlags`] mirror for guards that must not
/// contend with the read loop.
#[derive(Clone)]
pub struct SharedEngine {
    inner: Arc<Mutex<SerialEngine>>,
    pub flags: Arc<EngineFlags>,
}

impl std::ops::Deref for SharedEngine {
    type Target = Mutex<SerialEngine>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

pub fn new_shared_engine(config: crate::config::Config) -> SharedEngine {
    // Guard and engine must write the SAME atomics: create the flags arc up
    // front and hand one clone to the engine, avoiding any lock at
    // construction (blocking_lock would panic when called from within a
    // tokio runtime, e.g. every #[tokio::test]).
    let flags = Arc::new(EngineFlags {
        startup_in_progress: AtomicBool::new(true),
        dutabo_taken: AtomicU8::new(0),
        owns_serial_lock: AtomicBool::new(false),
        serial_owner_pid: AtomicU32::new(0),
    });
    let mut engine = SerialEngine::new(config);
    engine.flags = flags.clone();
    let inner = Arc::new(Mutex::new(engine));
    SharedEngine { inner, flags }
}

/// Start the engine, then attach the transport-independent read/watchdog
/// loops. The caller should spawn this future so MCP discovery is available
/// while the initial serial probe is still running.
pub async fn start_shared(engine: SharedEngine) -> Result<(), String> {
    let serial_ready = {
        let mut eng = engine.lock().await;
        if let Err(error) = eng.start().await {
            eng.set_startup_error(error.clone());
            engine
                .flags
                .startup_in_progress
                .store(false, Ordering::Relaxed);
            return Err(error);
        }
        !eng.has_serial_error()
    };
    // The startup probe is over in every exit path (ready, misconfigured, or
    // failed) — close the connecting window so tools stop being rejected as
    // "initialization in progress" while the read loop idles.
    engine
        .flags
        .startup_in_progress
        .store(false, Ordering::Relaxed);
    if !serial_ready {
        // Keep the MCP transport alive for diagnostics, but never reconnect or
        // send probes to an endpoint already proven to be misconfigured.
        return Ok(());
    }

    ensure_background_tasks(engine).await;
    Ok(())
}

/// Attach the read/watchdog loops once serial ownership has been obtained.
/// This is also used after `serial_claim` succeeds in an MCP instance that
/// originally started while another live task owned the endpoint.
pub async fn ensure_background_tasks(engine: SharedEngine) {
    {
        let mut eng = engine.lock().await;
        let read_running = eng
            .read_handle
            .as_ref()
            .is_some_and(|handle| !handle.is_finished());
        let watchdog_running = eng
            .watchdog_handle
            .as_ref()
            .is_some_and(|handle| !handle.is_finished());
        if read_running && watchdog_running {
            return;
        }
        if let Some(handle) = eng.read_handle.take() {
            handle.abort();
        }
        if let Some(handle) = eng.watchdog_handle.take() {
            handle.abort();
        }
    }

    let read_engine = engine.clone();
    let read_handle = tokio::spawn(async move {
        loop {
            let mut eng = read_engine.lock().await;
            eng.read_loop_iter().await;
            drop(eng);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });
    let watchdog_engine = engine.clone();
    let watchdog_handle = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        loop {
            tick.tick().await;
            watchdog_engine.lock().await.watchdog_once();
        }
    });
    engine
        .lock()
        .await
        .set_background_tasks(read_handle, watchdog_handle);
}

/// Install SIGUSR1/SIGUSR2 handlers for `dutabo serial` direct takeover.
///
/// - SIGUSR1 → [`SerialEngine::yield_serial_for_human`]: `dutabo serial` wants
///   the endpoint now (explicit human preemption is always allowed);
/// - SIGUSR2 → [`SerialEngine::resume_serial`] + [`ensure_background_tasks`]:
///   the dutabo session ended, the engine re-claims and reconnects.
///
/// Unix-only; on other platforms this is a no-op. Wired for both HTTP and
/// stdio processes because either transport may own the physical endpoint.
pub fn spawn_human_takeover_signals(engine: SharedEngine) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let _installed = tokio::spawn(async move {
            let mut yield_sig = match signal(SignalKind::user_defined1()) {
                Ok(sig) => sig,
                Err(error) => {
                    tracing::warn!("cannot install SIGUSR1 takeover handler: {error}");
                    return;
                }
            };
            let mut resume_sig = match signal(SignalKind::user_defined2()) {
                Ok(sig) => sig,
                Err(error) => {
                    tracing::warn!("cannot install SIGUSR2 resume handler: {error}");
                    return;
                }
            };
            loop {
                tokio::select! {
                    _ = yield_sig.recv() => {
                        engine.lock().await.yield_serial_for_human().await;
                    }
                    _ = resume_sig.recv() => {
                        engine.lock().await.resume_serial().await;
                        ensure_background_tasks(engine.clone()).await;
                    }
                }
            }
        });
        // Detached on purpose: the handler lives for the whole MCP process;
        // process exit tears it down.
    }
    #[cfg(not(unix))]
    let _ = &engine;
}

pub async fn record_startup_timeout(engine: &SharedEngine, seconds: u64) {
    // The startup window is over (timed out) — surface the structured error
    // to serial_get_state instead of keeping every tool rejected as
    // "connecting" forever.
    engine
        .flags
        .startup_in_progress
        .store(false, Ordering::Relaxed);
    engine
        .lock()
        .await
        .set_startup_error(format!("Serial startup timed out after {seconds}s"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfigFormat;
    use std::collections::HashMap;
    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn create_test_config(project_dir: &std::path::Path) -> Config {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let serial_port = listener.local_addr().unwrap().port().to_string();
        drop(listener);
        let lock_dir = project_dir.join("locks");
        crate::config::test_config(
            project_dir,
            &serial_port,
            "0",
            None,
            ".dut-serial",
            lock_dir.to_str().unwrap(),
        )
    }

    #[tokio::test]
    async fn baud_test_accepts_fresh_short_prompt_without_rotating_boot() {
        use tokio::io::AsyncReadExt;
        let tmp = TempDir::new().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut config = create_test_config(tmp.path());
        config.values.insert(
            "SERIAL_PORT".into(),
            listener.local_addr().unwrap().port().to_string(),
        );
        config
            .values
            .insert("SERIAL_BAUDRATE".into(), "115200".into());
        let mut engine = SerialEngine::new(config);
        engine.console.connect().await.unwrap();
        let (mut socket, _) = listener.accept().await.unwrap();
        let peer = tokio::spawn(async move {
            let mut byte = [0u8; 1];
            socket.read_exact(&mut byte).await.unwrap();
            assert_eq!(byte, [b'\r']);
            socket
                .write_all(b"\r\nroot@myd-lr3568x-buildroot:/# ")
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_secs(2)).await;
        });
        let before = engine.logs.boot_number();
        let result = engine.test_baud(115200, 1.0, false).await;
        assert_eq!(result["success"], true, "{result}");
        assert_eq!(result["reset_used"], false);
        assert!(result["capture_bytes"].as_u64().unwrap() < 64);
        assert_eq!(engine.logs.boot_number(), before);
        peer.await.unwrap();
    }

    #[tokio::test]
    async fn blank_echo_does_not_credit_heartbeat_or_revive_dutoff() {
        let tmp = TempDir::new().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut config = create_test_config(tmp.path());
        config.values.insert(
            "SERIAL_PORT".into(),
            listener.local_addr().unwrap().port().to_string(),
        );
        let mut engine = SerialEngine::new(config);
        engine.console.connect().await.unwrap();
        let (mut socket, _) = listener.accept().await.unwrap();
        engine.running = true;
        for state in [TargetState::Booting, TargetState::DutOff] {
            engine.state.transition(state);
            engine.state.age_last_data_for_test(Duration::from_secs(61));
            engine.state.mark_probe_sent();
            let before = engine.logs.ring_buffer.len();
            socket.write_all(b"\r\n\r\n").await.unwrap();
            tokio::time::timeout(Duration::from_secs(1), async {
                while engine.logs.ring_buffer.len() < before + 4 {
                    engine.read_loop_iter().await;
                }
            })
            .await
            .unwrap();
            assert!(engine.state.last_data_elapsed() >= Duration::from_secs(61));
            assert_eq!(engine.state.current(), state);
        }
        socket.write_all(b"root@dut:/# ").await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while engine.state.current() != TargetState::Active {
                engine.read_loop_iter().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(engine.state.current(), TargetState::Active);
        assert!(engine.state.last_data_elapsed() < Duration::from_secs(1));
        engine.console.close();
    }

    #[tokio::test]
    async fn startup_probe_waits_past_blank_echo_for_real_response() {
        let tmp = TempDir::new().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut config = create_test_config(tmp.path());
        config.values.insert(
            "SERIAL_PORT".into(),
            listener.local_addr().unwrap().port().to_string(),
        );
        let mut engine = SerialEngine::new(config);
        engine.console.connect().await.unwrap();
        let (mut socket, _) = listener.accept().await.unwrap();
        let server = tokio::spawn(async move {
            let mut buf = [0; 32];
            assert!(socket.read(&mut buf).await.unwrap() > 0);
            socket.write_all(b"\r\n").await.unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
            socket.write_all(b"root@dut:/# ").await.unwrap();
        });
        let response = engine.send_and_expect("\r").await.unwrap();
        assert!(String::from_utf8_lossy(&response).contains("root@dut:/#"));
        server.await.unwrap();
        engine.console.close();
    }

    #[test]
    fn relay_lock_guard_releases_the_prefixed_lock_key() {
        let tmp = TempDir::new().unwrap();
        let lock_dir = tmp.path().to_str().unwrap().to_string();
        let lock_host = "relay:192.0.2.1".to_string();
        let port = "2000".to_string();

        assert_eq!(
            crate::lock_manager::acquire_lock(&lock_host, &port, &lock_dir),
            None
        );
        {
            let _guard = RelayLockGuard {
                host: lock_host.clone(),
                port: port.clone(),
                lock_dir: lock_dir.clone(),
            };
        }
        assert_eq!(
            crate::lock_manager::acquire_lock(&lock_host, &port, &lock_dir),
            None
        );
        crate::lock_manager::release_lock(&lock_host, &port, &lock_dir);
    }

    #[tokio::test]
    async fn test_engine_new() {
        let tmp = TempDir::new().unwrap();
        let config = create_test_config(tmp.path());
        let engine = SerialEngine::new(config);

        assert!(!engine.console.is_open());
        assert_eq!(engine.login_user, "root");
    }

    /// LAVA/pexpect-style auto-login: plain username on every login
    /// prompt, debounce for same-prompt re-matches, bounded at
    /// MAX_LOGIN_ATTEMPTS rejections, and NEVER doubles the bytes (the
    /// tx_repeat escape hatch belongs to config, not to login logic).
    #[tokio::test]
    async fn login_is_prompt_driven_bounded_and_plain() {
        let tmp = TempDir::new().unwrap();
        let config = create_test_config(tmp.path());
        let mut engine = SerialEngine::new(config);

        // Prompt 1: first attempt, plain.
        engine.send_login().await;
        assert_eq!(engine.login_attempts, 1);

        // Immediate re-print of the SAME prompt is debounced away.
        engine.send_login().await;
        assert_eq!(engine.login_attempts, 1);

        // Prompt 2 and 3: further plain attempts.
        engine.last_login_send = None;
        engine.send_login().await;
        assert_eq!(engine.login_attempts, 2);
        engine.last_login_send = None;
        engine.send_login().await;
        assert_eq!(engine.login_attempts, 3);

        // Prompt 4+: exhausted — no further credential sends (the write
        // channel stays empty), protecting against account lockout.
        // Drain the three queued usernames first: poll_pending_write is
        // destructive.
        while engine.console.poll_pending_write().is_some() {}
        engine.last_login_send = None;
        engine.send_login().await;
        assert_eq!(engine.login_attempts, 4, "exhaustion marker advances once");
        assert!(
            engine.console.poll_pending_write().is_none(),
            "no bytes queued after exhaustion"
        );
        engine.last_login_send = None;
        engine.send_login().await;
        assert_eq!(engine.login_attempts, 4, "stays silent after the warning");
        assert!(engine.console.poll_pending_write().is_none());

        // The 500ms debounce also covers this tail: an immediate
        // send after the last call is a same-prompt re-match.
        engine.send_login().await;
        assert_eq!(engine.login_attempts, 4);
    }

    #[tokio::test]
    async fn start_does_not_append_dut_name_to_resolved_dut_dir() {
        let tmp = TempDir::new().unwrap();
        let mut config = create_test_config(tmp.path());
        config.values.insert("DUT_NAME".into(), "rk3568".into());
        config
            .values
            .insert("DUT_DIR".into(), ".dut-serial/rk3568".into());
        let mut engine = SerialEngine::new(config);

        engine.start().await.unwrap();

        assert!(
            tmp.path()
                .join(".dut-serial/rk3568/logs/current.serial.log")
                .exists()
        );
        assert!(!tmp.path().join(".dut-serial/rk3568/rk3568").exists());
        engine.stop().await;
    }

    /// `meaningful_line_count`: non-empty lines only, CR stripped — the
    /// capture window's early-exit gate counts the same lines the learner
    /// compares.
    #[test]
    fn test_meaningful_line_count() {
        let raw = b"DDR 2f85f4b2d4 cym\r\n\r\n  \nU-Boot 2023.10\nkernel [0.0] boot\n\n";
        assert_eq!(SerialEngine::meaningful_line_count(raw), 3);
        assert_eq!(SerialEngine::meaningful_line_count(b""), 0);
        assert_eq!(SerialEngine::meaningful_line_count(b"\r\n\r\n"), 0);
    }

    /// The engine's set_state_change_hook delegates to the internal
    /// StateManager: the hook fires after every real transition and never on
    /// a no-op (this is the wiring main.rs uses to refresh inventory.json).
    #[tokio::test]
    async fn test_engine_state_change_hook_fires_on_transition() {
        let tmp = TempDir::new().unwrap();
        let mut engine = SerialEngine::new(create_test_config(tmp.path()));
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let hook_seen = seen.clone();
        engine.set_state_change_hook(Box::new(move |state| {
            hook_seen.lock().unwrap().push(state);
        }));
        engine.state.transition(TargetState::Active);
        engine.state.transition(TargetState::Active); // no-op — no hook call
        engine.state.transition(TargetState::Crashed);
        let calls = seen.lock().unwrap().clone();
        assert_eq!(calls, vec![TargetState::Active, TargetState::Crashed]);
    }

    #[tokio::test]
    async fn test_serial_relay_endpoint_conflict_is_not_dut_off() {
        let tmp = TempDir::new().unwrap();
        let mut config = create_test_config(tmp.path());
        let configured_port = config.get("SERIAL_PORT").to_string();
        config.values.insert("RELAY_PORT".into(), configured_port);
        config.values.insert("RELAY_IP".into(), "127.0.0.1".into());
        config.values.insert("DUT_NAME".into(), "test-dut".into());

        let shared = new_shared_engine(config);
        start_shared(shared.clone()).await.unwrap();
        let engine = shared.lock().await;
        let state = engine.get_state_dict();
        assert!(engine.has_serial_error());
        assert!(!engine.console.is_open());
        assert_eq!(state["state"], "configuration-error");
        assert_eq!(state["error"]["code"], "serial_relay_endpoint_conflict");
        assert_eq!(state["dut_name"], "test-dut");
        assert_ne!(state["state"], "DUT-off");
    }

    #[test]
    fn test_parse_ser2net_banner_metadata() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let configured_port = listener.local_addr().unwrap().port().to_string();
        drop(listener);
        let line = format!(
            "\r\nser2net port tcp,{configured_port} device serialdev, /dev/ttyUSB0, 9600n81,local [,9600N81,CLOCAL] (Debian GNU/Linux)\r\n"
        );
        let banner = parse_ser2net_banner(line.as_bytes()).unwrap();
        assert_eq!(banner.port, configured_port);
        assert_eq!(banner.device, "/dev/ttyUSB0");
        assert_eq!(banner.baudrate, Some(9600));
    }

    #[test]
    fn test_strip_ser2net_banner_preserves_line_endings_and_partial() {
        let mut partial = Vec::new();
        let first = strip_ser2net_banner(b"value\r\nU-Boot 2024.07\r\n=> ", &mut partial);
        assert_eq!(first, b"value\r\nU-Boot 2024.07\r\n");
        assert_eq!(partial, b"=> ");

        let second = strip_ser2net_banner(b"next\r\n", &mut partial);
        assert_eq!(second, b"=> next\r\n");
        assert!(partial.is_empty());
    }

    #[test]
    fn test_strip_ser2net_banner_removes_only_banner_line() {
        let mut partial = Vec::new();
        let input = b"before\nser2net port tcp,2000 device serialdev, /dev/ttyUSB0\r\nafter\n";
        assert_eq!(
            strip_ser2net_banner(input, &mut partial),
            b"before\nafter\n"
        );
        assert!(partial.is_empty());
    }

    #[tokio::test]
    async fn test_get_state_dict() {
        let tmp = TempDir::new().unwrap();
        let config = create_test_config(tmp.path());
        let engine = SerialEngine::new(config);

        let state = engine.get_state_dict();
        assert_eq!(state["state"], ""); // Stopped → None → ""
        assert_eq!(state["boot_number"], 0);
        assert_eq!(state["relay_configured"], false);
        assert_eq!(state["login_configured"], true);
    }

    #[tokio::test]
    async fn test_read_log_empty() {
        let tmp = TempDir::new().unwrap();
        let config = create_test_config(tmp.path());
        let engine = SerialEngine::new(config);

        let result = engine.read_log(0, 50, None);
        assert_eq!(result["content"], "");
        assert_eq!(result["filename"], "");
    }

    #[tokio::test]
    async fn test_list_logs_empty() {
        let tmp = TempDir::new().unwrap();
        let config = create_test_config(tmp.path());
        let engine = SerialEngine::new(config);

        let result = engine.list_logs();
        assert_eq!(result["archives"].as_array().unwrap().len(), 0);
        // Since the fresh-log fix the live target is set from
        // construction — it names the file the writer WILL append to,
        // even before the first byte arrives.
        assert!(
            result["current"]
                .as_str()
                .unwrap()
                .ends_with("current.serial.log")
        );
    }

    #[tokio::test]
    async fn test_poll_logs_no_log() {
        let tmp = TempDir::new().unwrap();
        let config = create_test_config(tmp.path());
        let mut engine = SerialEngine::new(config);

        let result = engine.poll_logs();
        assert_eq!(result["lines"].as_array().unwrap().len(), 0);
        assert_eq!(result["since"], 0);
    }

    #[tokio::test]
    async fn test_get_config() {
        let tmp = TempDir::new().unwrap();
        let config = create_test_config(tmp.path());
        let engine = SerialEngine::new(config);

        let cfg = engine.get_config();
        assert_eq!(cfg["DEV_HOST_IP"], "127.0.0.1");
        assert_eq!(cfg["SERIAL_PORT"], engine.config.get("SERIAL_PORT"));
    }

    #[tokio::test]
    async fn test_engine_with_logs() {
        let tmp = TempDir::new().unwrap();
        let config = create_test_config(tmp.path());
        let mut engine = SerialEngine::new(config);

        engine.logs.open_current();
        engine.logs.write(b"test line 1\n");
        engine.logs.write(b"test line 2\n");
        engine.logs.flush_sync();

        let result = engine.read_log(0, 10, None);
        // Note: open_current writes a header line, so total_lines = 3 (header + 2 data lines)
        assert_eq!(result["total_lines"].as_u64().unwrap(), 3);
        assert!(result["content"].as_str().unwrap().contains("test line 1"));
    }

    #[tokio::test]
    async fn test_engine_poll_logs() {
        let tmp = TempDir::new().unwrap();
        let config = create_test_config(tmp.path());
        let mut engine = SerialEngine::new(config);

        engine.logs.open_current();
        engine.logs.write(b"line 1\n");
        engine.logs.write(b"line 2\n");
        engine.logs.flush_sync();

        // First poll should get all lines (including header)
        let result1 = engine.poll_logs();
        let lines1 = result1["lines"].as_array().unwrap().len();
        assert!(lines1 >= 2); // At least the 2 data lines (may include header)
        let since1 = result1["since"].as_u64().unwrap();
        assert!(since1 > 0);

        // Write more data
        engine.logs.write(b"line 3\n");
        engine.logs.flush_sync();

        // Second poll should get only new lines
        let result2 = engine.poll_logs();
        let lines2 = result2["lines"].as_array().unwrap().len();
        assert_eq!(lines2, 1); // Only the new line
        assert!(result2["since"].as_u64().unwrap() > since1);
    }

    #[tokio::test]
    async fn test_reset_target_no_relay() {
        let tmp = TempDir::new().unwrap();
        let config = create_test_config(tmp.path());
        let mut engine = SerialEngine::new(config);

        let result = engine.reset_target(false, 1, 1.0).await;
        assert_eq!(result["success"], false);
        assert_eq!(result["code"], "power_control_not_configured");
        assert_eq!(result["reason"], "unset");
        assert_eq!(result["button"], "reset");
        assert!(
            result["error"].as_str().unwrap().contains("DUT '"),
            "{result}"
        );
    }

    /// power_cycle_target must roll the state back when the relay backend
    /// errors out (regression: state stuck at Booting after a failed pulse).
    #[tokio::test]
    async fn test_power_cycle_relay_down_rolls_back_state() {
        let tmp = TempDir::new().unwrap();
        let mut config = create_test_config(tmp.path());
        config.values.insert("RELAY_TYPE".into(), "ch340".into());
        config.values.insert("RELAY_IP".into(), "127.0.0.1".into());
        // Closed port: the relay connect is refused immediately.
        config.values.insert("RELAY_PORT".into(), "59998".into());
        config.values.insert("POWER_CHANNEL".into(), "1".into());

        let mut engine = SerialEngine::new(config);
        let result = engine.power_cycle_target(false).await;

        assert_eq!(result["success"], false);
        let error = result["error"].as_str().unwrap();
        assert!(
            error.contains("serial_verify_relay"),
            "error should suggest relay verification: {error}"
        );
        // State must roll back to the pre-cycle state, not stay Booting.
        assert_eq!(engine.state.current(), TargetState::Stopped);
    }

    #[tokio::test]
    async fn test_shared_engine() {
        let tmp = TempDir::new().unwrap();
        let config = create_test_config(tmp.path());
        let shared = new_shared_engine(config);

        // Should be able to lock and access
        {
            let engine = shared.lock().await;
            assert!(!engine.console.is_open());
        }

        // Should be able to lock again
        {
            let mut engine = shared.lock().await;
            engine.logs.open_current();
            assert_eq!(engine.logs.boot_number(), 1);
        }
    }

    #[tokio::test]
    async fn test_watchdog_once_not_running() {
        let tmp = TempDir::new().unwrap();
        let config = create_test_config(tmp.path());
        let mut engine = SerialEngine::new(config);

        // Should not panic even when not running
        engine.watchdog_once();
    }

    #[tokio::test]
    async fn test_read_loop_iter_not_running() {
        let tmp = TempDir::new().unwrap();
        let config = create_test_config(tmp.path());
        let mut engine = SerialEngine::new(config);

        // Should return immediately when not running
        engine.read_loop_iter().await;
    }

    /// Smoke test: queue_command returns a valid receiver and check_timeouts resolves it
    #[tokio::test]
    async fn test_queue_command_returns_receiver() {
        let tmp = TempDir::new().unwrap();
        let config = create_test_config(tmp.path());
        let mut engine = SerialEngine::new(config);

        // Set up a write_fn so commands can be sent
        let write_tx = engine.console.write_sender();
        engine.commands.set_write_fn(Box::new(move |data| {
            write_tx.try_send_command(data.to_vec()).ok();
        }));

        // queue_command should return a valid receiver
        let rx = engine.queue_command("echo test", 0.1); // 100ms timeout for fast test

        // Simulate read loop calling check_timeouts (normally done by read_loop_iter)
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        engine.commands.check_timeouts();

        let result = rx.await;
        assert!(result.is_ok());
        let cmd_result = result.unwrap();
        assert!(cmd_result.timed_out);
    }

    #[tokio::test]
    async fn first_command_after_start_needs_no_warmup() {
        let tmp = TempDir::new().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            socket.write_all(b"root@dut:/# ").await.unwrap();

            let command = loop {
                let mut line = Vec::new();
                loop {
                    let byte = socket.read_u8().await.unwrap();
                    line.push(byte);
                    if byte == b'\n' {
                        break;
                    }
                }
                let line_text = String::from_utf8_lossy(&line);
                assert!(!line_text.contains("echo warmup"));
                if line_text.contains("echo first-command") {
                    break line;
                }
                socket.write_all(b"\r\nroot@dut:/# ").await.unwrap();
            };
            let command_text = String::from_utf8(command.clone()).unwrap();
            let parts: Vec<&str> = command_text.split('\'').collect();
            let marker = format!("{}{}", parts[1], parts[3]);

            socket.write_all(&command).await.unwrap();
            socket.write_all(b"\r\n").await.unwrap();
            socket.write_all(marker.as_bytes()).await.unwrap();
            socket
                .write_all(b"\r\nfirst-command-ok\r\nEXIT:0\r\n")
                .await
                .unwrap();
            socket.write_all(marker.as_bytes()).await.unwrap();
            socket.write_all(b"\r\nroot@dut:/# ").await.unwrap();
        });

        let mut config = create_test_config(tmp.path());
        config.values.insert("SERIAL_PORT".into(), port.to_string());
        let mut engine = SerialEngine::new(config);
        engine.start().await.unwrap();

        let mut result_rx = engine.queue_command("echo first-command", 1.0);
        let result = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                engine.read_loop_iter().await;
                if let Ok(result) = result_rx.try_recv() {
                    break result;
                }
            }
        })
        .await
        .expect("the first command must complete without a warmup command");

        assert_eq!(result.output, "first-command-ok");
        assert_eq!(result.exit_code, Some(0));
        assert!(!result.timed_out);
        engine.stop().await;
        server.await.unwrap();
    }

    /// Regression for a field failure after RFC2217 reconnect/claim: when the
    /// peer accepts writes but emits no serial bytes, wait_readable returns
    /// false. The read loop must still retire the timed-out marker command
    /// and start the command queued behind it. The write-notify arm flushes
    /// that write to the wire within the same iteration, so the assertion
    /// reads what the peer received instead of peeking the write channel.
    #[tokio::test]
    async fn test_silent_serial_timeout_unblocks_next_command() {
        let tmp = TempDir::new().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut received = Vec::new();
            let mut chunk = [0u8; 1024];
            let deadline = tokio::time::Instant::now() + Duration::from_millis(400);
            loop {
                let n = tokio::select! {
                    r = socket.read(&mut chunk) => match r {
                        Ok(0) => break,
                        Ok(n) => n,
                        Err(_) => break,
                    },
                    _ = tokio::time::sleep_until(deadline) => break,
                };
                received.extend_from_slice(&chunk[..n]);
            }
            received
        });

        let mut config = create_test_config(tmp.path());
        config.values.insert("SERIAL_PORT".into(), port.to_string());
        let mut engine = SerialEngine::new(config);
        engine.console.connect().await.unwrap();
        engine.running = true;
        engine.state.transition(TargetState::Active);

        let write_tx = engine.console.write_sender();
        engine.commands.set_write_fn(Box::new(move |data| {
            write_tx.try_send_command(data.to_vec()).ok();
        }));

        let first = engine.queue_command("echo first", 0.05);
        let _second = engine.queue_command("echo second", 5.0);
        tokio::time::sleep(Duration::from_millis(75)).await;

        tokio::time::timeout(Duration::from_millis(250), engine.read_loop_iter())
            .await
            .expect("silent read iteration must remain bounded");
        let result = tokio::time::timeout(Duration::from_millis(50), first)
            .await
            .expect("timed-out command must be resolved")
            .expect("command result sender must remain alive");
        assert!(result.timed_out);
        assert_eq!(engine.commands.pending_len(), 0);
        let received = server.await.unwrap();
        assert!(
            String::from_utf8_lossy(&received).contains("echo second"),
            "second command must reach the wire after retiring the zombie: {received:?}"
        );
    }

    // ── state transition tests (no hardware) ─────────────────────────────

    #[tokio::test]
    async fn test_reboot_start_event_tracks_dut_initiated_reboot() {
        let tmp = TempDir::new().unwrap();
        let config = create_test_config(tmp.path());
        let mut engine = SerialEngine::new(config);

        engine.state.transition(TargetState::Active);
        // A previous cycle already consumed the BootStart dedup slot:
        // feeding SPL only marks the cycle, no state change happens here.
        engine.detector.feed(b"U-Boot SPL 2024.01\n");
        assert_eq!(engine.state.current(), TargetState::Active);

        // The board announces a restart on its own (reboot cmd / watchdog /
        // panic auto-reboot) — this must move the state out of active.
        engine
            .handle_boot_events(vec![BootEvent::RebootStart])
            .await;

        assert_eq!(
            engine.state.current(),
            TargetState::Booting,
            "a DUT-initiated restart must leave the active state"
        );
        // The cycle is re-armed so the next SPL/DDR rotates the log and
        // fires BootStart instead of being swallowed by the dedup.
        let events = engine.detector.feed(b"U-Boot SPL 2024.01\n");
        assert!(events.iter().any(|e| matches!(e, BootEvent::BootStart)));
    }

    #[tokio::test]
    async fn test_get_state_dict_fields() {
        let tmp = TempDir::new().unwrap();
        let mut cfg = create_test_config(tmp.path());
        cfg.values.insert("LOGIN_USER".into(), "test".into());
        cfg.values.insert("DUT_NAME".into(), "test-dut".into());
        let engine = new_shared_engine(cfg);
        let eng = engine.lock().await;
        let state = eng.get_state_dict();
        assert!(state.get("state").is_some());
        assert!(state.get("login_configured").is_some());
        assert!(state.get("relay_configured").is_some());
        assert!(state.get("boot_number").is_some());
    }

    #[tokio::test]
    async fn test_engine_new_and_stop() {
        let mut values = std::collections::HashMap::new();
        values.insert("DEV_HOST_IP".into(), "127.0.0.1".into());
        values.insert("SERIAL_PORT".into(), "59999".into());
        values.insert("LOCK_DIR".into(), "/tmp/sermcp-test-locks".into());
        values.insert("LOGIN_USER".into(), "root".into());
        let cfg = Config {
            values,
            config_path: None,
            project_dir: Some(std::env::temp_dir()),
            format: crate::config::ConfigFormat::None,
        };
        // Engine should not panic on new()
        let engine = new_shared_engine(cfg);
        let mut eng = engine.lock().await;
        // start() will fail to connect (no ser2net at 127.0.0.1:59999) but that's OK
        let _result = eng.start().await;
        // Should either connect (if something on 59999) or transition to disconnected
        eng.stop().await;
        // HTTP idle shutdown and final cleanup both call stop(). The second
        // call must be a true no-op rather than releasing resources twice.
        eng.stop().await;
        assert!(eng.stopped);
    }

    #[tokio::test]
    async fn competing_engine_cannot_release_or_claim_live_serial_lock() {
        let tmp = TempDir::new().unwrap();
        let lock_dir = tmp.path().join("locks");
        let mut first_config = create_test_config(tmp.path());
        first_config
            .values
            .insert("LOCK_DIR".into(), lock_dir.to_string_lossy().into_owned());
        let second_config = first_config.clone();

        // flock locks are process-grained (same-process re-acquire is
        // idempotent), so the competing holder is a REAL child process —
        // mirroring two MCP servers on one endpoint. Spawn it on the
        // endpoint the second engine will contest BEFORE it starts.
        let second = SerialEngine::new(second_config);
        let mut holder = lock_manager::spawn_endpoint_holder(
            &second.host,
            &second.serial_target,
            lock_dir.to_str().unwrap(),
        );
        let lock_path = lock_manager::endpoint_lock_path(
            &second.host,
            &second.serial_target,
            lock_dir.to_str().unwrap(),
        );
        let holder_pid = lock_manager::wait_for_foreign_holder(&lock_path)
            .expect("child holder never took the lock");

        let mut second = second;
        second.start().await.unwrap();
        assert!(!second.owns_serial_lock);
        let second_state = second.get_state_dict();
        assert_eq!(second_state["error"]["code"], "serial_endpoint_busy");
        // A lost lock race is an endpoint-busy display, NOT a false
        // "configuration-error" (two projects sharing one board is normal).
        assert_eq!(second_state["state"], "endpoint-busy");

        let claim = second.claim_serial().await;
        assert_eq!(claim["success"], false);
        assert_eq!(claim["code"], "serial_endpoint_busy");

        second.stop().await;
        let conflict = lock_manager::acquire_lock(
            &second.host,
            &second.serial_target,
            lock_dir.to_str().unwrap(),
        );
        assert_eq!(conflict, Some(holder_pid));

        holder.kill().unwrap();
        let _ = holder.wait();

        // Once the live owner releases the endpoint, the non-owner recovery
        // path must both claim the lock and restore CommandQueue's writer.
        // The test endpoint has no listener, so connect itself fails; the
        // queued marker still has to reach the console writer channel.
        let claim = second.claim_serial().await;
        assert_eq!(claim["success"], false);
        assert!(second.owns_serial_lock);
        let _rx = second.queue_command("echo claimed", 0.1);
        let queued = second
            .console
            .poll_pending_write()
            .expect("claim must restore the command queue writer");
        assert!(String::from_utf8_lossy(&queued).contains("echo claimed"));
        second.stop().await;
    }

    /// The wedge that used to swallow `dutabo serial`'s SIGUSR1: the endpoint
    /// lock file names this engine but the mirrored flag went stale-false.
    /// The lock file is the authority — the yield must heal and release.
    #[tokio::test]
    async fn yield_serial_heals_desynced_owner_flag_from_lock_file() {
        let tmp = TempDir::new().unwrap();
        let mut engine = SerialEngine::new(create_test_config(tmp.path()));
        let lock_dir = engine.config.lock_dir();
        std::fs::create_dir_all(&lock_dir).unwrap();
        let lock_path =
            lock_manager::endpoint_lock_path(&engine.host, &engine.serial_target, &lock_dir);
        // Actually hold the endpoint lock, then desync the mirrored flag
        // backwards (flag false, lock held) — the flock is the authority.
        assert!(
            lock_manager::acquire_lock(&engine.host, &engine.serial_target, &lock_dir).is_none()
        );
        assert!(!engine.owns_serial_lock);

        engine.yield_serial_for_human().await;

        assert!(!engine.owns_serial_lock);
        assert!(
            !lock_path.exists(),
            "a lock this engine holds must be released on yield"
        );
    }

    #[tokio::test]
    async fn yield_serial_is_noop_for_a_foreign_lock_owner() {
        let tmp = TempDir::new().unwrap();
        let mut engine = SerialEngine::new(create_test_config(tmp.path()));
        let lock_dir = engine.config.lock_dir();
        std::fs::create_dir_all(&lock_dir).unwrap();
        let lock_path =
            lock_manager::endpoint_lock_path(&engine.host, &engine.serial_target, &lock_dir);
        // u32::MAX is never a live pid, so the liveness filter reads "foreign".
        std::fs::write(&lock_path, format!("{}\nhost:target\n", u32::MAX)).unwrap();

        engine.yield_serial_for_human().await;

        assert!(
            lock_path.exists(),
            "a foreign owner's lock must never be released"
        );
    }

    /// A disconnect+reconnect cycle must respect the endpoint lock: a dutabo
    /// human session that took over while the engine was down demotes the
    /// engine to non-owner, and the engine reclaims once the human releases.
    #[tokio::test]
    async fn reacquire_endpoint_lock_demotes_and_self_heals() {
        let tmp = TempDir::new().unwrap();
        let mut engine = SerialEngine::new(create_test_config(tmp.path()));
        let lock_dir = engine.config.lock_dir();
        std::fs::create_dir_all(&lock_dir).unwrap();
        let lock_path =
            lock_manager::endpoint_lock_path(&engine.host, &engine.serial_target, &lock_dir);
        engine.owns_serial_lock = true;

        // Lock file still names this engine → ownership confirmed, no churn.
        std::fs::write(&lock_path, format!("{}\nx:y\n", std::process::id())).unwrap();
        assert!(engine.reacquire_endpoint_lock());
        assert!(engine.owns_serial_lock);

        // A real foreign holder (flock is process-grained: same-process
        // re-acquire is idempotent) → demote to non-owner.
        let mut holder =
            lock_manager::spawn_endpoint_holder(&engine.host, &engine.serial_target, &lock_dir);
        lock_manager::wait_for_foreign_holder(&lock_path)
            .expect("child holder never took the lock");
        assert!(!engine.reacquire_endpoint_lock());
        assert!(!engine.owns_serial_lock);
        assert_eq!(
            engine.serial_error.as_ref().unwrap()["code"],
            "serial_endpoint_busy"
        );

        // The foreign holder dies → the kernel releases its flock and the
        // leftover file is just residue → reclaim cleanly (self-heal).
        holder.kill().unwrap();
        let _ = holder.wait();
        assert!(engine.reacquire_endpoint_lock());
        assert!(engine.owns_serial_lock);
        assert!(engine.serial_error.is_none());
        lock_manager::release_lock(&engine.host, &engine.serial_target, &lock_dir);
    }

    /// R13a (B5/D11): engine stop must delete the state/cache files
    /// UNCONDITIONALLY — even when this engine never owned the serial
    /// endpoint lock. Today the deletion is gated on owns_serial_lock, so
    /// a stop-without-lock leaks root AND per-DUT target-state /
    /// statusline-cache files (R3 residue).
    #[tokio::test]
    async fn engine_stop_without_endpoint_lock_deletes_state_files() {
        let tmp = TempDir::new().unwrap();
        let mut config = create_test_config(tmp.path());
        config.values.insert("DUT_NAME".into(), "dut-x".into());
        config.values.insert(
            "LOCK_DIR".into(),
            tmp.path().join("locks").to_string_lossy().into_owned(),
        );
        let mut engine = SerialEngine::new(config);

        engine.state.transition(TargetState::Active);
        let root_state = tmp.path().join(".dut-serial/target-state");
        let root_cache = tmp.path().join(".dut-serial/statusline-cache");
        let dut_state = tmp.path().join(".dut-serial/dut-x/target-state");
        let dut_cache = tmp.path().join(".dut-serial/dut-x/statusline-cache");
        for path in [&root_state, &root_cache, &dut_state, &dut_cache] {
            assert!(path.exists(), "{} must exist before stop", path.display());
        }
        assert!(!engine.owns_serial_lock);

        engine.stop().await;

        for path in [&root_state, &root_cache, &dut_state, &dut_cache] {
            assert!(
                !path.exists(),
                "{} must be deleted on stop even without the endpoint lock",
                path.display()
            );
        }
        // R15 hygiene: the transition above wrote the (to-be-deleted D10)
        // shm cache — remove it so the suite leaves no residue.
        use md5::{Digest, Md5};
        let canonical =
            std::fs::canonicalize(tmp.path()).unwrap_or_else(|_| tmp.path().to_path_buf());
        let digest = Md5::digest(canonical.to_string_lossy().as_bytes());
        let shm_file = format!(
            "/dev/shm/claude-status-{}",
            &crate::ports::md5_hex(&digest)[..8]
        );
        let _ = std::fs::remove_file(shm_file);
    }

    #[test]
    fn watchdog_never_writes_or_changes_state_during_manual_session() {
        let tmp = TempDir::new().unwrap();
        let config = create_test_config(tmp.path());
        let mut engine = SerialEngine::new(config);
        engine.running = true;
        engine.state.transition(TargetState::Active);
        engine.state.transition(TargetState::Dutabo);

        engine.watchdog_once();

        assert_eq!(engine.state.current(), TargetState::Dutabo);
    }

    /// Booting + silence past hang_timeout must request a probe and the
    /// watchdog must enqueue a blank-line probe on the write channel.
    #[test]
    fn test_watchdog_sends_probe_in_booting_state() {
        let tmp = TempDir::new().unwrap();
        let mut config = create_test_config(tmp.path());
        config.values.insert("HANG_TIMEOUT".into(), "0".into());
        let mut engine = SerialEngine::new(config);
        engine.running = true;
        engine.console.set_open_for_test(true);
        engine.state.transition(TargetState::Booting);
        std::thread::sleep(std::time::Duration::from_millis(50));

        engine.watchdog_once();

        let write = engine.console.poll_pending_write();
        assert_eq!(
            write.as_deref(),
            Some(&b"\n"[..]),
            "watchdog must enqueue a blank-line probe while booting"
        );
        assert!(
            !engine.state.heartbeat_pending,
            "sending the probe clears the pending flag"
        );
        assert_eq!(engine.state.current(), TargetState::Booting);
    }

    /// A probe that could NOT be enqueued (write channel full) must NOT be
    /// marked as sent — the pre-rework watchdog marked every probe "sent"
    /// even when `sendline` silently dropped it, so three dropped probes
    /// could declare a healthy DUT DUT-off with zero bytes ever reaching it.
    #[test]
    fn test_watchdog_dropped_probe_is_not_marked_sent() {
        let tmp = TempDir::new().unwrap();
        let mut config = create_test_config(tmp.path());
        config.values.insert("HANG_TIMEOUT".into(), "0".into());
        let mut engine = SerialEngine::new(config);
        engine.running = true;
        engine.console.set_open_for_test(true);
        engine.state.transition(TargetState::Booting);
        std::thread::sleep(std::time::Duration::from_millis(50));

        // Saturate the bounded write channel: every probe enqueue must fail.
        let sender = engine.console.write_sender();
        while sender.try_send(vec![0u8]).is_ok() {}

        engine.watchdog_once();

        assert!(
            engine.state.heartbeat_pending,
            "a dropped probe must stay pending so the watchdog retries it"
        );
        assert_eq!(
            engine.state.heartbeat_missed(),
            0,
            "a probe that never reached the DUT must never count toward DUT-off"
        );
        assert_eq!(engine.state.current(), TargetState::Booting);

        // Once the channel frees up, the next tick must deliver the probe and
        // only then mark it sent.
        while engine.console.poll_pending_write().is_some() {}
        engine.watchdog_once();
        assert_eq!(
            engine.console.poll_pending_write().as_deref(),
            Some(&b"\n"[..]),
            "after the channel frees up the probe must be enqueued"
        );
        assert!(
            !engine.state.heartbeat_pending,
            "a successfully enqueued probe clears the pending flag"
        );
        assert_eq!(engine.state.current(), TargetState::Booting);
    }

    /// Booting with recent data must produce no unsolicited prompt writes —
    /// the old fire-and-forget background `sendline("")` is removed.
    #[test]
    fn test_watchdog_booting_no_background_prompt_before_timeout() {
        let tmp = TempDir::new().unwrap();
        let config = create_test_config(tmp.path());
        let mut engine = SerialEngine::new(config);
        engine.running = true;
        engine.console.set_open_for_test(true);
        engine.state.transition(TargetState::Booting);
        // Data arrived recently, but long enough that the OLD background
        // prompt (elapsed >= 2s) would have fired.
        engine
            .state
            .age_last_data_for_test(std::time::Duration::from_secs(3));

        engine.watchdog_once();

        assert!(
            engine.console.poll_pending_write().is_none(),
            "no unsolicited prompt writes during booting"
        );
        assert_eq!(engine.state.current(), TargetState::Booting);
    }

    #[test]
    fn test_target_state_display() {
        assert_eq!(TargetState::Active.as_str(), "active");
        assert_eq!(TargetState::Booting.as_str(), "booting");
        assert_eq!(TargetState::Crashed.as_str(), "crashed");
        assert_eq!(TargetState::Disconnected.as_str(), "disconnected");
        assert_eq!(TargetState::Stopped.as_str(), "stopped");
    }

    #[test]
    fn test_format_statusline_all_states() {
        let tmp = TempDir::new().unwrap();
        let sm = StateManager::new(tmp.path(), 60, 3, ".dut-serial", "", 30);
        for state in &[
            TargetState::Active,
            TargetState::Booting,
            TargetState::UBoot,
            TargetState::Crashed,
            TargetState::Disconnected,
            TargetState::DutOff,
        ] {
            let text = sm.format_statusline(*state);
            assert!(!text.is_empty());
            assert!(text.contains('\x1b')); // ANSI color codes
        }
    }

    // ── Regression tests ───────────────────────────────────────────────

    /// Engine.start() must set `running = true` before any blocking operations
    /// (SSH udev check, slow reconnects). Even if serial is unreachable, the
    /// MCP server must be responsive — `running` must be true after start().
    #[tokio::test]
    async fn test_start_sets_running_despite_serial_refused() {
        let tmp = TempDir::new().unwrap();
        let mut values = HashMap::new();
        values.insert("DEV_HOST_IP".into(), "127.0.0.1".into());
        values.insert("SERIAL_PORT".into(), "1".into()); // port 1 → connection refused fast
        values.insert("RELAY_PORT".into(), "0".into());
        values.insert("RESET_CHANNEL".into(), "0".into());
        values.insert("DOWNLOAD_CHANNEL".into(), "0".into());
        values.insert("HANG_TIMEOUT".into(), "60".into());
        values.insert("HANG_HYSTERESIS".into(), "3".into());
        values.insert("MAX_ARCHIVED_LOGS".into(), "10".into());
        values.insert("MAX_LOG_FILE_SIZE".into(), "100".into());
        values.insert("DUT_DIR".into(), ".dut-serial".into());
        values.insert(
            "LOCK_DIR".into(),
            format!("/tmp/sermcp-test-{}", std::process::id()),
        );
        values.insert("LOGIN_USER".into(), "root".into());
        values.insert("LOGIN_PASS".into(), "".into());
        values.insert("UBOOT_INTERRUPT_STRATEGY".into(), "flood".into());

        let config = Config {
            values,
            config_path: None,
            project_dir: Some(tmp.path().to_path_buf()),
            format: ConfigFormat::None,
        };

        let mut engine = SerialEngine::new(config);
        let result = engine.start().await;

        // start() should complete (not timeout) and set running=true
        // even though serial connect was refused.
        assert!(result.is_ok(), "start() should succeed: {:?}", result.err());
        assert!(engine.running, "engine.running must be true after start()");
        assert_eq!(
            engine.state.current(),
            TargetState::Disconnected,
            "should be disconnected when serial is refused"
        );

        engine.stop().await;
    }

    /// start() must complete within a reasonable time even when DUT_NAME
    /// is configured (which triggers the SSH udev check). The SSH check
    /// runs as a background task and must not block the hot path.
    #[tokio::test]
    async fn test_start_completes_quickly() {
        let tmp = TempDir::new().unwrap();
        // Use unique ports per test to avoid lock conflicts from parallel runs.
        let test_port = format!("{}", 50000 + (std::process::id() % 10000));
        let lock_dir = format!("/tmp/sermcp-test-{}-quick", std::process::id());

        let mut values = HashMap::new();
        values.insert("DEV_HOST_IP".into(), "127.0.0.1".into());
        values.insert("SERIAL_PORT".into(), test_port);
        values.insert("RELAY_PORT".into(), "0".into());
        values.insert("RESET_CHANNEL".into(), "0".into());
        values.insert("DOWNLOAD_CHANNEL".into(), "0".into());
        values.insert("HANG_TIMEOUT".into(), "60".into());
        values.insert("HANG_HYSTERESIS".into(), "3".into());
        values.insert("MAX_ARCHIVED_LOGS".into(), "10".into());
        values.insert("MAX_LOG_FILE_SIZE".into(), "100".into());
        values.insert("DUT_DIR".into(), ".dut-serial".into());
        values.insert("LOCK_DIR".into(), lock_dir);
        values.insert("LOGIN_USER".into(), "root".into());
        values.insert("DUT_NAME".into(), "test-dut".into()); // triggers SSH background check
        values.insert("UBOOT_INTERRUPT_STRATEGY".into(), "flood".into());

        let config = Config {
            values,
            config_path: None,
            project_dir: Some(tmp.path().to_path_buf()),
            format: ConfigFormat::None,
        };

        let mut engine = SerialEngine::new(config);
        let start = std::time::Instant::now();
        let result = engine.start().await;
        let elapsed = start.elapsed();

        // start() must succeed even when DUT_NAME is configured
        assert!(result.is_ok(), "start() should succeed: {:?}", result.err());
        // start() must complete in under 2s — the SSH udev check is a
        // background task and must not block the hot path.
        assert!(
            elapsed.as_millis() < 2000,
            "start() took {}ms, should complete quickly (SSH check is background)",
            elapsed.as_millis()
        );
        assert!(engine.running);

        engine.stop().await;
    }

    // ── Reconnect backoff deadline (reconnect_after) ────────────────────

    /// Regression: while a backoff window is open (`reconnect_after` in the
    /// future), read_loop_iter must NOT attempt a reconnect. The old code
    /// consumed `reconnect_after` with an unconditional `take()` on every
    /// ~20ms iteration — a connect flood that starved the engine lock.
    #[tokio::test]
    async fn test_backoff_window_skips_reconnect_until_deadline() {
        let tmp = TempDir::new().unwrap();
        let config = create_test_config(tmp.path()); // port 59999 → refused
        let mut engine = SerialEngine::new(config);
        engine.running = true;
        engine.state.transition(TargetState::Disconnected);
        let after = std::time::Instant::now() + Duration::from_secs(30);
        engine.reconnect_after = Some(after);

        engine.read_loop_iter().await;

        assert_eq!(
            engine.reconnect_after,
            Some(after),
            "backoff deadline must not be consumed before it elapses"
        );
        assert!(
            !engine.console.is_open(),
            "no connect attempt may be made while the backoff window is open"
        );
    }

    /// Deadline in the past → read_loop_iter must actually reconnect and
    /// consume the deadline. Guards against over-correcting the previous bug
    /// into never reconnecting.
    #[tokio::test]
    async fn test_backoff_deadline_elapsed_reconnects() {
        let tmp = TempDir::new().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        // Mock ser2net: accept one connection, answer the probe's "\r" with
        // a shell prompt so probe_initial_state completes quickly.
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 256];
            let n = sock.read(&mut buf).await.unwrap();
            assert!(n > 0, "expected probe line, got EOF");
            let _ = sock.write_all(b"root@test:~# ").await;
        });

        let mut config = create_test_config(tmp.path());
        config.values.insert("SERIAL_PORT".into(), port.to_string());
        let mut engine = SerialEngine::new(config);
        engine.running = true;
        engine.state.transition(TargetState::Disconnected);
        engine.reconnect_after = Some(std::time::Instant::now() - Duration::from_secs(1));

        engine.read_loop_iter().await;

        assert!(
            engine.console.is_open(),
            "elapsed deadline → read_loop_iter must reconnect"
        );
        assert_eq!(
            engine.reconnect_after, None,
            "deadline must be consumed by the reconnect attempt"
        );
        server.await.unwrap();
    }

    /// Regression: a successful fast retry must clear a pending backoff
    /// deadline, otherwise the stale timestamp triggers a spurious reconnect
    /// once it elapses.
    #[tokio::test]
    async fn test_fast_retry_success_clears_reconnect_after() {
        let tmp = TempDir::new().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        // Mock ser2net: accept the initial connect (torn down by the error
        // handler) and the fast-retry connect, answering each probe.
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = [0u8; 256];
                // First accept sees EOF once the engine closes the session.
                let _ = sock.read(&mut buf).await;
                let _ = sock.write_all(b"root@test:~# ").await;
            }
        });

        let mut config = create_test_config(tmp.path());
        config.values.insert("SERIAL_PORT".into(), port.to_string());
        let mut engine = SerialEngine::new(config);
        engine.running = true;
        engine.state.transition(TargetState::Active);

        // Establish the session that the read error will tear down.
        engine.console.connect().await.unwrap();
        // Residual backoff deadline from an earlier disconnect cycle.
        engine.reconnect_after = Some(std::time::Instant::now() + Duration::from_secs(30));

        engine
            .handle_read_error(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "mock disconnect",
            ))
            .await;

        assert!(
            engine.console.is_open(),
            "fast retry should have reconnected"
        );
        assert_eq!(
            engine.reconnect_after, None,
            "successful fast retry must clear the stale backoff deadline"
        );
        server.await.unwrap();
    }

    // ── WebSocket Dutabo state transitions ───────────────────────────────

    /// A PASSIVE monitor subscription (the init TUI during TEST) must
    /// NEVER claim the manual session: the state stays Active and the
    /// tool-blocking flag stays clear — otherwise every serial probe of
    /// a TEST run gets rejected the moment the terminal attaches.
    #[tokio::test]
    async fn test_monitor_subscription_never_claims_manual_session() {
        let tmp = TempDir::new().unwrap();
        let config = create_test_config(tmp.path());
        let mut engine = SerialEngine::new(config);
        engine.running = true;
        engine.state.transition(TargetState::Active);

        let mut monitor = engine.monitor_sender().subscribe();
        assert_eq!(
            engine.state.current(),
            TargetState::Active,
            "a monitor subscription must not claim the manual session"
        );
        assert_eq!(
            engine
                .flags
                .dutabo_taken
                .load(std::sync::atomic::Ordering::Relaxed),
            0,
            "serial tools must stay available while a monitor watches"
        );

        // The read loop's monitor feed delivers to subscribers: feed one
        // chunk through the same broadcast the loop uses.
        let feed = engine.monitor_sender();
        feed.send(vec![b'o', b'k']).unwrap();
        let seen = tokio::time::timeout(std::time::Duration::from_secs(2), monitor.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(seen, vec![b'o', b'k']);
    }

    #[tokio::test]
    async fn test_ws_connect_transitions_to_dutabo() {
        let tmp = TempDir::new().unwrap();
        let config = create_test_config(tmp.path());
        let mut engine = SerialEngine::new(config);
        engine.running = true;
        engine.state.transition(TargetState::Active);

        // Simulate WS client connecting
        let (ws_tx, _) = tokio::sync::broadcast::channel::<Vec<u8>>(1);
        engine.set_ws_tx(ws_tx);

        assert_eq!(
            engine.state.current(),
            TargetState::Dutabo,
            "WS connect should transition to Dutabo"
        );
    }

    #[tokio::test]
    async fn test_ws_disconnect_restores_active() {
        let tmp = TempDir::new().unwrap();
        let config = create_test_config(tmp.path());
        let mut engine = SerialEngine::new(config);
        engine.running = true;
        engine.state.transition(TargetState::Active);

        // Connect WS
        let (ws_tx, _) = tokio::sync::broadcast::channel::<Vec<u8>>(1);
        engine.set_ws_tx(ws_tx);
        assert_eq!(engine.state.current(), TargetState::Dutabo);

        // Disconnect WS
        engine.clear_ws_tx();
        assert_eq!(
            engine.state.current(),
            TargetState::Active,
            "WS disconnect should restore Active"
        );
    }

    #[tokio::test]
    async fn test_ws_tx_none_after_clear() {
        let tmp = TempDir::new().unwrap();
        let config = create_test_config(tmp.path());
        let mut engine = SerialEngine::new(config);
        engine.running = true;
        engine.state.transition(TargetState::Active);

        let (ws_tx, _) = tokio::sync::broadcast::channel::<Vec<u8>>(1);
        engine.set_ws_tx(ws_tx.clone());
        assert!(engine.ws_tx.is_some());

        engine.clear_ws_tx();
        assert!(engine.ws_tx.is_none(), "ws_tx should be None after clear");
    }

    #[tokio::test]
    async fn test_read_loop_keeps_serial_open_in_ws_dutabo_mode() {
        // When in Dutabo via WS (not sentinel), the serial connection should
        // stay open — the MCP server continues to manage serial for the WS client.
        let tmp = TempDir::new().unwrap();
        let config = create_test_config(tmp.path());
        let mut engine = SerialEngine::new(config);
        engine.running = true;
        engine.state.transition(TargetState::Active);

        // Simulate WS connection
        let (ws_tx, _) = tokio::sync::broadcast::channel::<Vec<u8>>(1);
        engine.set_ws_tx(ws_tx);
        assert_eq!(engine.state.current(), TargetState::Dutabo);

        // read_loop_iter should NOT close the serial when ws_tx is set
        // (it's a no-op in test, but we verify state is preserved)
        engine.read_loop_iter().await;

        // State should remain Dutabo (not try to reconnect)
        assert_eq!(
            engine.state.current(),
            TargetState::Dutabo,
            "WS Dutabo should not trigger reconnect in read_loop_iter"
        );
        assert!(
            engine.ws_tx.is_some(),
            "ws_tx should still be set after read_loop_iter"
        );
    }

    #[tokio::test]
    async fn test_ws_connect_from_stopped_no_warning() {
        // Active → Dutabo should be clean (no suspicious transition)
        let tmp = TempDir::new().unwrap();
        let config = create_test_config(tmp.path());
        let mut engine = SerialEngine::new(config);
        engine.running = true;
        engine.state.transition(TargetState::Active);

        let (ws_tx, _) = tokio::sync::broadcast::channel::<Vec<u8>>(1);
        engine.set_ws_tx(ws_tx);

        assert_eq!(engine.state.current(), TargetState::Dutabo);
        // Should NOT produce a "suspicious transition" warning because
        // Active → Dutabo is not in the suspicious match list
    }

    #[tokio::test]
    async fn test_engine_set_baud_disconnected_no_panic() {
        let tmp = TempDir::new().unwrap();
        let config = create_test_config(tmp.path());
        let mut engine = SerialEngine::new(config);
        let state_before = engine.state.current();

        let configured_baud = engine.config.get("SERIAL_BAUDRATE").parse().unwrap();
        let result = engine.set_baud(configured_baud).await;

        assert_eq!(result["success"], false);
        assert_eq!(result["code"], "not_connected");
        assert_eq!(
            engine.state.current(),
            state_before,
            "engine state must be unchanged"
        );
    }

    /// Liveness observation must come from the shared stream the read loop
    /// writes to (A2 regression): bytes the read loop consumed from the
    /// socket must count as a probe response, or a live DUT is falsely
    /// downgraded to DUT-off.
    #[test]
    fn test_shared_stream_detects_read_loop_consumed_bytes() {
        let tmp = TempDir::new().unwrap();
        let config = create_test_config(tmp.path());
        let mut engine = SerialEngine::new(config);

        let snap0 = engine.shared_stream_snapshot();
        assert!(!engine.shared_stream_has_new(snap0), "no data yet");

        // Simulate the read loop consuming the probe response (prompt echo).
        engine.logs.write(b"\r\nroot@acc-dut:/# ");
        assert!(
            engine.shared_stream_has_new(snap0),
            "read-loop-consumed bytes must count as a probe response"
        );

        let snap1 = engine.shared_stream_snapshot();
        assert!(!engine.shared_stream_has_new(snap1));
        engine.logs.write(b"more\n");
        assert!(engine.shared_stream_has_new(snap1));
    }

    #[test]
    fn test_serial_output_cursor_survives_ring_overflow() {
        let tmp = TempDir::new().unwrap();
        let config = create_test_config(tmp.path());
        let mut engine = SerialEngine::new(config);

        engine.logs.write(b"before");
        let snapshot = engine.shared_stream_snapshot();
        let mut burst = vec![b'x'; 2 * 1024 * 1024 + 128];
        burst.extend_from_slice(b"final-marker");
        engine.logs.write(&burst);

        let (tail, truncated) = engine.serial_output_since(snapshot);
        assert!(truncated, "overflow must be visible to the caller");
        assert!(
            tail.ends_with(b"final-marker"),
            "retained tail must contain the newest command output"
        );
    }

    #[test]
    fn test_finish_learn_cycle_caps_persisted_capture() {
        let tmp = TempDir::new().unwrap();
        let learner = ConnectionLearner::new(LearnConfig {
            learn_dir: tmp.path().to_path_buf(),
            ..Default::default()
        });
        let mut cycles = Vec::new();
        let raw = vec![b'x'; MAX_LEARN_LOG_FILE_BYTES + 1024];

        SerialEngine::finish_learn_cycle(
            &learner,
            &mut cycles,
            0,
            LearnMethod::HardwareReset,
            &raw,
            "Test",
        );

        assert_eq!(cycles.len(), 1);
        assert_eq!(cycles[0].full_content.len(), MAX_LEARN_LOG_FILE_BYTES);
        assert_eq!(
            std::fs::metadata(&cycles[0].log_path).unwrap().len(),
            MAX_LEARN_LOG_FILE_BYTES as u64
        );
    }
    // ── No-dev_ctrl unified contract ────────────────────

    fn engine_with(project_dir: &std::path::Path, extra: &[(&str, &str)]) -> SerialEngine {
        let mut config = create_test_config(project_dir);
        for (k, v) in extra {
            config.values.insert((*k).into(), (*v).into());
        }
        SerialEngine::new(config)
    }

    #[tokio::test]
    async fn test_power_control_failure_contract_unset() {
        let tmp = TempDir::new().unwrap();
        let engine = engine_with(tmp.path(), &[("DUT_NAME", "boardX")]);
        let body = engine.power_control_failure("power");
        assert_eq!(body["success"], false);
        assert_eq!(body["code"], "power_control_not_configured");
        assert_eq!(body["reason"], "unset");
        assert_eq!(body["button"], "power");
        assert_eq!(body["dut_name"], "boardX");
        assert!(body["error"].as_str().unwrap().contains("boardX"), "{body}");
        assert!(body["fix"]["sections"].is_array(), "{body}");
        assert!(
            body["hint"].as_str().unwrap().contains("restart=software"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn test_power_control_failure_contract_disabled() {
        let tmp = TempDir::new().unwrap();
        let engine = engine_with(tmp.path(), &[("RELAY_TYPE", "none")]);
        let body = engine.power_control_failure("reset");
        assert_eq!(body["reason"], "disabled");
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .contains("explicitly disabled"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn test_power_control_failure_contract_no_channels() {
        let tmp = TempDir::new().unwrap();
        let engine = engine_with(tmp.path(), &[("RELAY_TYPE", "ch340-relay")]);
        let body = engine.power_control_failure("reset");
        assert_eq!(body["reason"], "no_channels");
        assert!(
            body["error"].as_str().unwrap().contains("ch340-relay"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn test_reset_and_power_cycle_return_the_contract() {
        let tmp = TempDir::new().unwrap();
        let mut engine = engine_with(tmp.path(), &[("DUT_NAME", "boardX")]);
        let reset = engine.reset_target(false, 1, 1.0).await;
        assert_eq!(reset["code"], "power_control_not_configured", "{reset}");
        assert_eq!(reset["reason"], "unset", "{reset}");
        let cycle = engine.power_cycle_target(false).await;
        assert_eq!(cycle["code"], "power_control_not_configured", "{cycle}");
        assert_eq!(cycle["button"], "power", "{cycle}");
    }

    #[tokio::test]
    async fn test_verify_relay_keeps_relay_configured_false_with_contract() {
        let tmp = TempDir::new().unwrap();
        let mut engine = engine_with(tmp.path(), &[]);
        let body = engine.verify_relay().await;
        assert_eq!(body["code"], "power_control_not_configured", "{body}");
        assert_eq!(body["relay_configured"], false, "{body}");
    }

    #[tokio::test]
    async fn test_get_state_dict_exposes_power_control_block() {
        let tmp = TempDir::new().unwrap();
        let engine = engine_with(tmp.path(), &[("RELAY_TYPE", "none")]);
        let state = engine.get_state_dict();
        let pc = &state["power_control"];
        assert_eq!(pc["configured"], false, "{state}");
        assert_eq!(pc["reason"], "disabled", "{state}");
        assert_eq!(pc["buttons"]["reset"], false, "{state}");
        assert_eq!(state["relay_configured"], false, "{state}");
    }

    // ── U-Boot interrupt strategy ────────────────────────

    #[test]
    fn test_interrupt_strategy_parse_supported_values_only() {
        use crate::serial_engine::InterruptStrategy as S;
        assert_eq!(S::parse("flood"), Some(S::Flood));
        assert_eq!(S::parse("pattern"), Some(S::Pattern));
        assert_eq!(S::parse("aggressive"), None);
        assert_eq!(S::parse("lava"), None);
        assert_eq!(S::parse("timed"), None);
        assert_eq!(S::parse(""), None);
        assert_eq!(S::parse("bogus"), None);
        assert_eq!(S::Flood.as_str(), "flood");
        assert_eq!(S::Pattern.as_str(), "pattern");
    }

    #[test]
    fn test_resolve_interrupt_strategy_rejects_invalid_values() {
        let tmp = TempDir::new().unwrap();
        // Arg override wins.
        let engine = engine_with(tmp.path(), &[]);
        assert_eq!(
            engine.resolve_interrupt_strategy(Some("pattern")).unwrap(),
            crate::serial_engine::InterruptStrategy::Pattern
        );
        assert!(engine.resolve_interrupt_strategy(Some("bogus")).is_err());
        // Config value.
        let engine = engine_with(tmp.path(), &[("UBOOT_INTERRUPT_STRATEGY", "pattern")]);
        assert_eq!(
            engine.resolve_interrupt_strategy(None).unwrap(),
            crate::serial_engine::InterruptStrategy::Pattern
        );
        // Empty config uses the flood default.
        let engine = engine_with(tmp.path(), &[]);
        assert_eq!(
            engine.resolve_interrupt_strategy(None).unwrap(),
            crate::serial_engine::InterruptStrategy::Flood
        );
    }
}
