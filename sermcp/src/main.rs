//! Embedded Debug MCP Server — Rust implementation
//!
//! MCP serial debugging tool for embedded Linux target boards connected via
//! the Dev Host ser2net. Talks to ser2net directly over TCP (socket://
//! protocol) — no socat, no SSH.
//!
//! Protocol: MCP (Model Context Protocol)
//! Transport: stdio (framed JSON-RPC 2.0 via rmcp) or Streamable HTTP

// All modules are in lib.rs — use the library crate.
use sermcp as lib;
use std::path::PathBuf;

const HELP: &str = "\
sermcp — MCP serial console debugger for embedded Linux targets

Usage:
  sermcp [OPTIONS]
  sermcp --help
  sermcp --version

Description:
  An MCP (Model Context Protocol) server that connects to embedded Linux
  target boards via Dev Host ser2net. Provides per-power-cycle log capture,
  automatic U-Boot interrupt, boot-completion login, kernel crash detection,
  and relay reset control.

  Default transport is stdio (JSON-RPC 2.0 newline-delimited).
  Use --http to expose as a Streamable HTTP server.

  Supports: tools, resources (log files), prompts (debug templates).

Options:
  -h, --help         Show this help message and exit.
  -V, --version      Print version and exit.
  -v, --verbose      Increase log verbosity (debug level on stderr).
                     Default: info level to {project}/.dut-serial/mcp.log.
      --log-to-stderr  Log to stderr instead of file (useful for debugging).
      --http [HOST:PORT]  Run as Streamable HTTP server (default: 127.0.0.1:3000).
      --idle-timeout N   When --http, exit after N seconds with no HTTP/WS activity
                         (default: 600; pass 0 to disable). GET /health counts as
                         activity, so a live session keeps the server alive.
      --dry-run          Validate config and exit without connecting.
      --diagnose         Test ser2net connectivity/reference log and exit.

Environment:
  TARGET_CONF        Path to .target.jsonc file (alternative to CWD search).
  RUST_LOG           tracing filter (e.g. RUST_LOG=debug).
                     Overridden by --verbose.

Configuration:
  The server searches recursively upward from CWD for .target.jsonc.
  See README.md for the full configuration reference.

  Run `dutabo init` to create .target.jsonc. DUT host, serial port, baudrate,
  and relay settings are required from that file; the server has no lab-specific
  endpoint fallback.

MCP Tools:
  serial_send_command  - Execute shell command on target
  serial_get_state     - Get target state and metadata
  serial_get_logs      - Retrieve serial logs with pattern filtering
  serial_list_logs     - List archived boot logs
  serial_reset         - Hardware reset via relay + log rotation
  serial_enter_uboot   - Enter U-Boot via auto/hardware/software/none restart
  serial_button        - Control a DUT button (reset/download) — any SoC's
                         boot-mode sequence is composed from this generic tool.
  serial_wait_pattern  - Wait for regex pattern in serial output
  serial_uboot_command - Send command at U-Boot prompt
  serial_poll_logs     - Get new serial output since last poll
  serial_get_config    - Show current target configuration
  serial_claim         - Claim serial ownership
  serial_load_reference - Load reference log for adaptive stage detection

Files:
  {project}/.target.jsonc         Target configuration
  {project}/.dut-serial/inventory.json   Inventory for the Claude hooks
  {project}/.dut-serial/logs/     Boot log archives
  {project}/.dut-serial/target-state   Current state file
  {project}/.dut-serial/mcp.log   Server log
  /tmp/sermcp/locks/      Per host:port mutual exclusion

Quick Start:
  # Start in stdio mode (Claude Code spawns automatically):
  sermcp

  # Start in HTTP mode for dutabo CLI access:
  sermcp --http

  # Debug with verbose logging:
  sermcp --log-to-stderr --verbose

Troubleshooting:
  # Check hardware connectivity:
  bash scripts/diagnose.sh

  # Restart this project's MCP server (never pkill globally — other project
  # paths have independent servers):
  kill $(cat .dut-serial/mcp.pid)

  # Install CLI tool for manual debug:
  rustup target add x86_64-unknown-linux-musl
  cargo install --git https://github.com/bitshelf/sermcp --locked --target x86_64-unknown-linux-musl

Version: sermcp v{}\n";

fn print_help() {
    let msg = HELP.replace("{}", env!("CARGO_PKG_VERSION"));
    eprintln!("{msg}");
}

fn print_version() {
    eprintln!("sermcp v{}", env!("CARGO_PKG_VERSION"));
}

/// Write `.dut-serial/inventory.json`: the canonical inventory schema built
/// by `config::build_inventory_json` with per-DUT state from the ONE shared
/// derivation `dut_state::inventory_state_for` (the same reader
/// `dutabo list` uses, so the two can never diverge).
/// Atomic tmp+rename (last-writer-wins) — never a partially-written file.
///
/// Called at startup, from the engine's on_state_change hook after every
/// StateManager transition, AND from the periodic heartbeat task (owner
/// only) — the Python hooks and the dsh statusline read ONLY this file, so
/// a refresh here is what surfaces a mid-session crash/transition on the
/// statusline and in user-prompt alerts. The heartbeat keeps
/// `written_at_unix` fresh even when no state changed: consumers treat a
/// stale written_at as a DEAD server (a killed server leaves no cleanup
/// hook, so without the heartbeat a stale snapshot would render forever).
///
/// Heartbeat interval: the freshness window consumers must tolerate. 30s is
/// a compromise — dsh re-renders within 30s of a server death, and the idle
/// write cost is one tiny atomic rename per interval.
const INVENTORY_HEARTBEAT_SECS: u32 = 30;
/// Default `--idle-timeout` for HTTP mode (seconds). 10 minutes covers the
/// gaps between dutabo invocations and dsh/hook health probes inside a
/// working session, while guaranteeing an abandoned server terminates.
const DEFAULT_HTTP_IDLE_TIMEOUT_SECS: u64 = 600;
/// Read the shared lifecycle identity used by file-based status consumers.
fn read_owner_record(project_dir: &std::path::Path) -> Option<lib::config::InventoryOwner> {
    let path = project_dir.join(".dut-serial").join("owner.json");
    let text = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    if value.get("schema_version")?.as_u64()? != 1 {
        return None;
    }
    Some(lib::config::InventoryOwner {
        owner_kind: value
            .get("owner_kind")
            .and_then(|v| v.as_str())
            .unwrap_or("agent")
            .to_string(),
        owner_session_id: value
            .get("owner_session_id")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        owner_mcp_pid: value
            .get("owner_mcp_pid")
            .and_then(|v| v.as_u64())
            .map(|p| p as u32),
        claimed_at_unix: value.get("claimed_at_unix").and_then(|v| v.as_i64()),
    })
}

/// A stdio server belongs to its launching agent, even without Python hooks.
/// Use the same exclusive lock as hook claims to preserve first-opener order.
fn claim_stdio_display_owner(project_dir: &std::path::Path) {
    let dir = project_dir.join(".dut-serial");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let lock = dir.join("owner.lock");
    let Ok(_guard) = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock)
    else {
        return;
    };
    let path = dir.join("owner.json");
    let existing = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok());
    let now = chrono::Utc::now().timestamp();
    let live = existing.as_ref().is_some_and(|record| {
        if let Some(pid) = record["owner_mcp_pid"].as_u64() {
            pid > 0
                && pid <= i32::MAX as u64
                && std::path::Path::new(&format!("/proc/{pid}")).exists()
        } else {
            record["claimed_at_unix"]
                .as_i64()
                .is_some_and(|time| now - time < 60)
        }
    });
    if !live {
        let session = std::env::var("SERMCP_AGENT_SESSION_ID")
            .ok()
            .or_else(|| std::env::var("CODEX_THREAD_ID").ok());
        let record = serde_json::json!({
            "schema_version": 1,
            "owner_kind": "stdio",
            "owner_pid": std::process::id(),
            "owner_session_id": session,
            "owner_mcp_pid": std::process::id(),
            "owner_cwd": project_dir,
            "claimed_at_unix": now,
        });
        let tmp = dir.join(format!(".owner.json.tmp-{}", std::process::id()));
        if std::fs::write(&tmp, record.to_string()).is_ok() {
            let _ = std::fs::rename(tmp, path);
        }
    }
    let _ = std::fs::remove_file(lock);
}

/// RAII guard for the 4-line session lock
/// `/dev/shm/claude-serial-{md5(canonical_dir)[:8]}.lock` (D8):
/// line0 canonical project dir, line1 owner pid, line2 session id
/// (`-` when unknown), line3 unix ts. Unlinked on drop (clean exit).
struct SessionLockGuard {
    path: PathBuf,
}

impl Drop for SessionLockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn session_lock_path_for(project_dir: &std::path::Path) -> PathBuf {
    use md5::{Digest, Md5};
    let mut hasher = Md5::new();
    hasher.update(project_dir.to_string_lossy().as_bytes());
    let h = lib::ports::md5_hex(&hasher.finalize())
        .chars()
        .take(8)
        .collect::<String>();
    PathBuf::from("/dev/shm").join(format!("claude-serial-{h}.lock"))
}

fn claim_session_lock(project_dir: &std::path::Path) -> Option<SessionLockGuard> {
    let canonical =
        std::fs::canonicalize(project_dir).unwrap_or_else(|_| project_dir.to_path_buf());
    let path = session_lock_path_for(&canonical);
    let session_id = std::env::var("CLAUDE_SESSION_ID")
        .or_else(|_| std::env::var("ZCODE_SESSION_ID"))
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "-".to_string());
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let content = format!(
        "{}\n{}\n{}\n{}\n",
        canonical.to_string_lossy(),
        std::process::id(),
        session_id,
        ts
    );
    if let Err(e) = std::fs::write(&path, content) {
        tracing::warn!("Cannot write session lock {:?}: {e}", path);
        return None;
    }
    Some(SessionLockGuard { path })
}

/// Startup hygiene (own project only): sweep
/// `.inventory.json.tmp-<pid>-<seq>` / `.owner.json.tmp-<pid>-<seq>`
/// leftovers whose pid is dead. Names without the sequenced form predate
/// the per-call sequence and are swept unconditionally — no
/// back-compatibility: every live writer stamps the sequence.
fn startup_hygiene(project_dir: &std::path::Path) {
    let dut_serial = project_dir.join(".dut-serial");
    let Ok(entries) = std::fs::read_dir(&dut_serial) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(suffix) = name
            .strip_prefix(".inventory.json.tmp-")
            .or_else(|| name.strip_prefix(".owner.json.tmp-"))
        else {
            continue;
        };
        let sweep = match suffix.split_once('-') {
            Some((pid, _seq)) => !pid
                .parse::<u32>()
                .map(lib::lock_manager::process_alive)
                .unwrap_or(false),
            None => true,
        };
        if sweep {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Monotonic per-process sequence for atomic-rename tmp names: the pid
/// distinguishes processes, the sequence distinguishes CONCURRENT calls
/// inside one process (the 30s heartbeat task and a state-change hook can
/// race on the same name — a shared tmp would let one writer's rename
/// publish the other's half-written file).
fn tmp_sequence() -> u64 {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

fn refresh_inventory(
    cfg: &lib::config::Config,
    project_dir: &std::path::Path,
    http_mode: bool,
    http_bind: &str,
) {
    let (dev_hosts, duts) = match cfg
        .config_path
        .as_deref()
        .map(lib::config::parse_config_file)
    {
        Some(Ok(parsed)) => (parsed.dev_hosts, parsed.duts),
        Some(Err(e)) => {
            tracing::error!("Cannot build inventory: {e}");
            return;
        }
        None => return,
    };
    let current_dut_name = std::env::var("TARGET_DUT_NAME")
        .ok()
        .filter(|s| !s.trim().is_empty());
    let written_at_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // The port this server serves: the bound --http port, else the
    // deterministic project control port (stdio mode binds it too).
    let mcp_http_port = if http_mode {
        http_bind
            .rsplit_once(':')
            .and_then(|(_, port)| port.parse::<u16>().ok())
    } else {
        Some(lib::ports::project_mcp_port(project_dir))
    };
    let inventory = lib::config::build_inventory_json(
        cfg,
        &dev_hosts,
        &duts,
        current_dut_name.as_deref(),
        lib::config::InventoryMeta {
            written_by: "mcp-server",
            written_at_unix,
            mcp_pid: Some(std::process::id()),
            mcp_http_port,
            // Heartbeat contract: this server rewrites the inventory every
            // INVENTORY_HEARTBEAT_SECS even when no state changed, so
            // consumers can treat written_at_unix as a liveness signal (a
            // killed server leaves a stale snapshot that must not render).
            heartbeat_secs: Some(INVENTORY_HEARTBEAT_SECS),
            owner: read_owner_record(project_dir),
        },
        |dut| lib::dut_state::inventory_state_for(project_dir, dut),
    );
    let dut_serial = project_dir.join(".dut-serial");
    if let Err(e) = std::fs::create_dir_all(&dut_serial) {
        tracing::warn!("Cannot create {}: {e}", dut_serial.display());
        return;
    }
    let path = dut_serial.join("inventory.json");
    let text = match serde_json::to_string_pretty(&inventory) {
        Ok(text) => text,
        Err(e) => {
            tracing::warn!("Cannot serialize inventory: {e}");
            return;
        }
    };
    // tmp name carries pid + per-call sequence so concurrent writers never
    // collide on the temp file itself; rename is the atomic commit
    // (last-writer-wins).
    let tmp = dut_serial.join(format!(
        ".inventory.json.tmp-{}-{}",
        std::process::id(),
        tmp_sequence()
    ));
    if let Err(e) = std::fs::write(&tmp, text) {
        tracing::warn!("Cannot write inventory {}: {e}", tmp.display());
        let _ = std::fs::remove_file(&tmp);
    } else if let Err(e) = std::fs::rename(&tmp, &path) {
        tracing::warn!("Cannot rename inventory into {}: {e}", path.display());
        let _ = std::fs::remove_file(&tmp);
    }
    // Back-fill the ownership anchor: the project's single MCP records its
    // pid into the hooks-written owner record (never creates one) so claim
    // liveness follows THIS server — when it dies, the record goes vacant
    // and the next live agent re-claims (multi-agent isolation).
    backfill_owner_anchor(&dut_serial);
}

/// Update `owner.json`'s `owner_mcp_pid` to this server's pid, only when the
/// record already exists (the hooks own claiming; the server only anchors).
fn backfill_owner_anchor(dut_serial: &std::path::Path) {
    let path = dut_serial.join("owner.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(_) => return,
    };
    let mut value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(_) => return,
    };
    if value.get("schema_version").and_then(|v| v.as_u64()) != Some(1) {
        return;
    }
    // Dedup: the heartbeat calls this every 30s; rewriting owner.json when
    // the anchor pid is already ours would churn the file (and fire dsh's
    // fs.watch) for nothing.
    if value.get("owner_mcp_pid").and_then(|v| v.as_u64()) == Some(std::process::id() as u64) {
        return;
    }
    value["owner_mcp_pid"] = serde_json::json!(std::process::id());
    let tmp = dut_serial.join(format!(
        ".owner.json.tmp-{}-{}",
        std::process::id(),
        tmp_sequence()
    ));
    if let Ok(payload) = serde_json::to_string_pretty(&value)
        && std::fs::write(&tmp, payload).is_ok()
    {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Rotate mcp.log if it exceeds 10MB. Keeps at most 5 rotated files.
fn rotate_mcp_log(log_dir: &std::path::Path) {
    let log_path = log_dir.join("mcp.log");
    let max_size = 10 * 1024 * 1024; // 10 MB
    let max_files = 5;
    if let Ok(meta) = std::fs::metadata(&log_path)
        && meta.len() > max_size
    {
        // Rotate: mcp.log.4 → mcp.log.5, mcp.log.3 → mcp.log.4, etc.
        for i in (1..max_files).rev() {
            let old = log_dir.join(format!("mcp.log.{}", i));
            let new = log_dir.join(format!("mcp.log.{}", i + 1));
            let _ = std::fs::rename(&old, &new);
        }
        let _ = std::fs::rename(&log_path, log_dir.join("mcp.log.1"));
    }
}

#[tokio::main]
async fn main() {
    // ── CLI argument parsing ──
    let mut verbose = false;
    let mut log_to_stderr = false;
    let mut http_mode = false;
    let mut http_bind = "127.0.0.1:3000".to_string();
    // HTTP-mode self-cleanup (default ON): a detached `--http` server has no
    // parent to reap it, so without an idle timeout it outlives every client
    // forever — the orphaned-process class this default exists to prevent.
    // The hooks' session-stop kills promptly on last-session-out; this
    // watchdog is the backstop for crashed/killed clients that never ran
    // their cleanup. 0 explicitly disables.
    let mut idle_timeout: Option<u64> = Some(DEFAULT_HTTP_IDLE_TIMEOUT_SECS);
    let mut dry_run = false;
    let mut diagnose = false;

    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                print_help();
                lib::trace_chrome::exit_with_trace(0);
            }
            "-V" | "--version" => {
                print_version();
                lib::trace_chrome::exit_with_trace(0);
            }
            "-v" | "--verbose" => verbose = true,
            "--log-to-stderr" => log_to_stderr = true,
            "--diagnose" => diagnose = true,
            "--http" => {
                http_mode = true;
                // Check if next arg is a HOST:PORT (doesn't start with -)
                if i + 1 < args.len() && !args[i + 1].starts_with('-') {
                    i += 1;
                    http_bind = args[i].clone();
                }
            }
            "--idle-timeout" => {
                if i + 1 < args.len() && !args[i + 1].starts_with('-') {
                    i += 1;
                    match args[i].parse::<u64>() {
                        Ok(0) => idle_timeout = None,
                        Ok(n) => idle_timeout = Some(n),
                        Err(e) => {
                            eprintln!("Invalid --idle-timeout value '{}': {e}", args[i]);
                            lib::trace_chrome::exit_with_trace(1);
                        }
                    }
                } else {
                    eprintln!("--idle-timeout requires a numeric argument (seconds)");
                    lib::trace_chrome::exit_with_trace(1);
                }
            }
            "--dry-run" => dry_run = true,
            other => {
                eprintln!("Unknown option: {other}");
                eprintln!("Use --help for usage information.");
                lib::trace_chrome::exit_with_trace(1);
            }
        }
        i += 1;
    }

    // ── Initialize logging ──
    let log_level = if verbose { "debug" } else { "info" };

    use tracing_subscriber::fmt::format::FmtSpan;
    use tracing_subscriber::layer::Layer;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let filter = tracing_subscriber::EnvFilter::from_default_env()
        .add_directive(format!("sermcp={log_level}").parse().unwrap());
    let registry = tracing_subscriber::registry();
    // SERMCP_TRACE=<path>.pftrace enables the native Perfetto layer.
    // The guard lives for the whole process and releases it on drop.
    let perfetto_guard = if log_to_stderr {
        // Logs go to stderr (for debugging) — span lifecycles are printed for pipeline tracing
        let registry = registry.with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_ansi(true)
                .with_target(true)
                .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
                .with_filter(filter),
        );
        let (registry, guard) = lib::trace_perfetto::attach_perfetto(registry);
        registry.init();
        guard
    } else {
        // Logs go to a file (stdout is reserved for JSON-RPC) — JSON format for structured analysis
        let log_dir = std::path::PathBuf::from(
            lib::config::load_config()
                .project_dir
                .as_ref()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|| ".".to_string()),
        )
        .join(".dut-serial");
        std::fs::create_dir_all(&log_dir).ok();
        rotate_mcp_log(&log_dir);
        let log_file = log_dir.join("mcp.log");
        let registry = registry.with(
            tracing_subscriber::fmt::layer()
                .with_writer(move || {
                    std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&log_file)
                        .unwrap_or_else(|_| panic!("Cannot open log file: {log_file:?}"))
                })
                .with_ansi(false)
                .with_target(true)
                .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
                .with_filter(filter),
        );
        let (registry, guard) = lib::trace_perfetto::attach_perfetto(registry);
        registry.init();
        guard
    };
    let _perfetto_trace = perfetto_guard;

    tracing::info!("sermcp v{} starting", env!("CARGO_PKG_VERSION"));

    // ── Load configuration ──
    let cfg = lib::config::load_config();
    if cfg.config_path.is_none() {
        let cwd = std::env::current_dir().unwrap_or_default();
        let target_conf = std::env::var("TARGET_CONF").ok();
        // Re-run discovery on the error path to surface the loud message
        // (TARGET_CONF set but missing → its own error; otherwise the plain
        // config-missing message) on the terminal — fail closed, never
        // silent.
        let guidance = match lib::config::find_config_path() {
            Err(e) => e,
            _ => "No .target.jsonc found.".to_string(),
        };
        tracing::error!(
            "{guidance} cwd={}, TARGET_CONF={target_conf:?}",
            cwd.display()
        );
        eprintln!(
            "CONFIG ERROR: {guidance} cwd={}, TARGET_CONF={target_conf:?}",
            cwd.display()
        );
        lib::trace_chrome::exit_with_trace(1);
    }

    // ── Validate config ──
    if let Err(errors) = cfg.validate() {
        for e in &errors {
            eprintln!("CONFIG ERROR: {e}");
        }
        lib::trace_chrome::exit_with_trace(1);
    }
    // ── Dry-run: validate config and exit ──
    if dry_run {
        eprintln!("=== Config Validation ===");
        eprintln!("Dev Host:  {}", cfg.dev_host_ip());
        eprintln!("Serial:    {}:{}", cfg.dev_host_ip(), cfg.serial_target());
        let login_user = cfg.login_user();
        eprintln!(
            "Login:     {}",
            if login_user.is_empty() {
                "(not set)"
            } else {
                &login_user
            }
        );
        let reference_log = cfg.reference_log();
        eprintln!(
            "Reference: {}",
            if reference_log.is_empty() {
                "(not set)"
            } else {
                &reference_log
            }
        );
        eprintln!(
            "Relay:     port={} ch:reset={}/download={}",
            cfg.relay_port(),
            cfg.reset_channel(),
            cfg.download_channel(),
        );
        if cfg.dev_host_ip().is_empty() {
            eprintln!("\nERROR: dev_host.ip not set. Edit .target.jsonc");
            lib::trace_chrome::exit_with_trace(1);
        }
        if cfg.serial_target().is_empty() || cfg.serial_target() == "0" {
            eprintln!("\nERROR: serial.port not set. Edit .target.jsonc");
            lib::trace_chrome::exit_with_trace(1);
        }
        eprintln!("\nConfig OK. No errors found.");
        lib::trace_chrome::exit_with_trace(0);
    }

    // ── Diagnose: test connectivity and exit ──
    if diagnose {
        eprintln!("=== Diagnostics ===");
        let host = cfg.dev_host_ip();
        let port = cfg.serial_target();
        eprintln!("Testing ser2net: {host}:{port} ...");

        // Test TCP connectivity
        let addr = format!("{host}:{port}");
        match std::net::TcpStream::connect_timeout(
            &addr.parse().unwrap(),
            std::time::Duration::from_secs(5),
        ) {
            Ok(_) => eprintln!("  ✓ TCP connect OK"),
            Err(e) => eprintln!("  ✗ TCP connect FAILED: {e}"),
        }

        // Test reference log
        let ref_log = cfg.reference_log();
        if !ref_log.is_empty() {
            let path = std::path::PathBuf::from(&ref_log);
            eprintln!("Reference log: {ref_log}");
            if path.exists() {
                match lib::boot_detector::StageLearner::from_reference(&path) {
                    Ok(learner) => {
                        eprintln!("  ✓ Loaded ({} fingerprints)", learner.fingerprints.len());
                    }
                    Err(e) => eprintln!("  ✗ Load failed: {e}"),
                }
            } else {
                eprintln!("  ✗ File not found");
            }
        } else {
            eprintln!("Reference log: (not configured)");
        }

        // Test relay config
        let relay_port = cfg.relay_port();
        if relay_port > 0 {
            eprintln!(
                "Relay: port={relay_port} reset_ch={} download_ch={}",
                cfg.reset_channel(),
                cfg.download_channel()
            );
        } else {
            eprintln!("Relay: (not configured)");
        }

        eprintln!("\nDiagnostics complete.");
        lib::trace_chrome::exit_with_trace(0);
    }

    let project_dir = cfg
        .project_dir
        .clone()
        .expect("validated project directory");

    // A canonical project path is one MCP ownership domain. Transport mode,
    // client session, and TARGET_DUT_NAME never create another domain: a
    // second process could later reconnect or claim resources even if it
    // initially called itself an observer.
    let _project_lock = match lib::lock_manager::acquire_project_lock(&project_dir) {
        Ok(lock) => lock,
        Err(owner) => {
            // A stdio spawn that lost the singleton race used to exit(1),
            // which the agent host surfaced as a dead connection (Claude
            // Code: "Failed to reconnect to sermcp: -32000") and made
            // stdio (`.mcp.json` command) and HTTP (`dutabo` CLI) modes
            // mutually exclusive. Instead, transparently bridge this
            // stdio client into the running instance's HTTP endpoint —
            // the agent keeps a working MCP connection and simply becomes
            // another session of the shared server (owner or read-only
            // guest, same as every other late client).
            if !http_mode {
                match lib::stdio_bridge::run(&project_dir, owner).await {
                    Ok(()) => {
                        tracing::info!("stdio bridge session ended");
                        lib::trace_chrome::exit_with_trace(0);
                    }
                    Err(reason) => {
                        eprintln!(
                            "MCP service already running for project {} (PID {owner}); \
                             refusing to start a second instance (stdio bridge to the \
                             running instance unavailable: {reason})",
                            project_dir.display()
                        );
                        lib::trace_chrome::exit_with_trace(1);
                    }
                }
            }
            let owner = if owner == 0 {
                "unknown".to_string()
            } else {
                owner.to_string()
            };
            eprintln!(
                "MCP service already running for project {} (PID {owner}); refusing to start a second instance",
                project_dir.display()
            );
            lib::trace_chrome::exit_with_trace(1);
        }
    };

    startup_hygiene(&project_dir);
    if !http_mode {
        claim_stdio_display_owner(&project_dir);
    }
    // Write inventory only after acquiring the project lock. A rejected
    // process exits above before it can touch project state or hardware.
    refresh_inventory(&cfg, &project_dir, http_mode, &http_bind);
    let _session_lock = claim_session_lock(&project_dir);

    // ── Create the engine; serial connect/probing runs in the background ──
    // MCP transport must come up first so a bad or silent serial endpoint
    // cannot prevent tools/list and serial_get_state from being registered.
    let engine = lib::serial_engine::new_shared_engine(cfg.clone());
    {
        // Refresh the inventory on EVERY StateManager transition: the Python
        // hooks read only inventory.json, so without this hook a mid-session
        // crash would leave the statusline rendering the startup snapshot.
        let mut eng = engine.lock().await;
        let hook_cfg = cfg.clone();
        let hook_project = project_dir.clone();
        let hook_bind = http_bind.clone();
        eng.set_state_change_hook(Box::new(move |_state: lib::state_manager::TargetState| {
            refresh_inventory(&hook_cfg, &hook_project, http_mode, &hook_bind);
        }));
        // ── Inventory HEARTBEAT ──
        // The statusline consumers (Claude Code per-frame, dsh fs.watch)
        // gate on the recorded mcp_pid AND written_at freshness. State
        // transitions alone are not enough: a long-idle server never writes
        // inventory, so a stale snapshot is indistinguishable from a dead
        // server — and a killed server (kill -9, no cleanup hook) would
        // leave the statusline rendering the old state forever. This task
        // rewrites inventory every INVENTORY_HEARTBEAT_SECS even when no
        // state changed: the file mtime + written_at_unix become the
        // freshness signal, and dsh's fs.watch re-renders on each write.
        // The write is atomic (tmp+rename) and tiny; the interval is long
        // enough that idle CPU cost is negligible.
        let beat_cfg = cfg.clone();
        let beat_project = project_dir.clone();
        let beat_bind = http_bind.clone();
        let beat_mode = http_mode;
        tokio::spawn(async move {
            // interval_at (not interval): tokio's plain interval fires its
            // first tick IMMEDIATELY, which would double-write the inventory
            // milliseconds after the startup refresh above and spam every
            // fs.watch consumer with a spurious event.
            let period = std::time::Duration::from_secs(INVENTORY_HEARTBEAT_SECS.into());
            let mut interval =
                tokio::time::interval_at(tokio::time::Instant::now() + period, period);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                refresh_inventory(&beat_cfg, &beat_project, beat_mode, &beat_bind);
            }
        });
    }
    let connecting_state = engine.lock().await.connecting_state_dict();
    let startup_engine = engine.clone();
    let startup_handle = tokio::spawn(async move {
        match tokio::time::timeout(
            std::time::Duration::from_secs(35),
            lib::serial_engine::start_shared(startup_engine.clone()),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::error!("Engine start failed: {e}"),
            Err(_) => {
                tracing::error!("Engine start timed out after 35s");
                lib::serial_engine::record_startup_timeout(&startup_engine, 35).await;
            }
        }
    });

    // ── Create the TaskManager (MCP SEP-2663 Tasks Extension) ──
    let tasks = lib::task_manager::TaskManager::new();

    // Human direct-preemption signals (`dutabo serial`) are transport
    // independent. Stdio owners also expose an HTTP/WS control port, and a
    // dutabo launched from another project may need to preempt that physical
    // endpoint directly. Non-owner engines handle SIGUSR1 as a no-op.
    lib::serial_engine::spawn_human_takeover_signals(engine.clone());

    // ── Run the MCP server ──
    if http_mode {
        // Streamable HTTP transport (2025-03-26 spec)
        let (host, port) = if let Some((h, p)) = http_bind.rsplit_once(':') {
            (h.to_string(), p.parse::<u16>().unwrap_or(3000))
        } else {
            ("127.0.0.1".to_string(), 3000u16)
        };
        tracing::info!(
            "Starting Streamable HTTP on {host}:{port} (idle_timeout: {idle_timeout:?})"
        );
        // Gap-list #12: the MCP endpoint validates Host headers against a
        // loopback-only allowlist (DNS-rebinding protection; the list is
        // extended for a concrete non-loopback bind in mcp_http.rs). A
        // wildcard bind would 403 every LAN client while the 403s are only
        // visible in mcp.log — surface it on the terminal instead.
        if lib::mcp_http::classify_bind_host(&host) == lib::mcp_http::BindHostClass::Wildcard {
            eprintln!(
                "WARNING: --http {host}:{port} binds all interfaces, but the MCP \
                 endpoint only accepts loopback Host headers (DNS-rebinding \
                 protection) — LAN clients will get 403 Forbidden. Bind a \
                 concrete configured LAN IP instead."
            );
            // (mcp_http::service_config also records this in mcp.log.)
        }
        if let Err(e) = lib::mcp_http::run_http(
            engine.clone(),
            &host,
            port,
            idle_timeout.map(std::time::Duration::from_secs),
            connecting_state.clone(),
        )
        .await
        {
            tracing::error!("HTTP server error: {e}");
        }
    } else {
        // The singleton process serves Codex over stdio and exposes the same
        // engine on a deterministic loopback port for dutabo HTTP/WS access.
        let control_port = lib::ports::project_mcp_port(&project_dir);
        let listener = match tokio::net::TcpListener::bind(("127.0.0.1", control_port)).await {
            Ok(listener) => listener,
            Err(error) => {
                eprintln!("Cannot bind project MCP control port {control_port}: {error}");
                lib::trace_chrome::exit_with_trace(1);
            }
        };
        let http_engine = engine.clone();
        let http_state = connecting_state.clone();
        let http_handle = Some(tokio::spawn(async move {
            if let Err(error) =
                lib::mcp_http::run_http_on_listener(http_engine, listener, None, http_state).await
            {
                tracing::error!("Project MCP control transport stopped: {error}");
            }
        }));
        // Use a readiness-driven stdout writer. Tokio's standard stdout
        // adapter can accept a frame before its blocking worker encounters
        // EAGAIN, which makes rmcp drop the response during a later flush.
        let stdout = match lib::stdio_transport::reliable_stdout() {
            Ok(stdout) => stdout,
            Err(error) => {
                eprintln!("Cannot prepare MCP stdout transport: {error}");
                tracing::error!("Cannot prepare MCP stdout transport: {error}");
                tasks.shutdown();
                if let Some(handle) = http_handle {
                    handle.abort();
                    let _ = handle.await;
                }
                lib::trace_chrome::exit_with_trace(1);
            }
        };

        // Stdio transport via rmcp SDK
        use rmcp::ServiceExt;
        let handler = lib::mcp_handler::McpHandler::new_with_connecting_state(
            engine.clone(),
            tasks.clone(),
            connecting_state,
        );
        match handler.serve((tokio::io::stdin(), stdout)).await {
            Ok(service) => {
                tracing::info!("rmcp stdio server started");
                if let Err(e) = service.waiting().await {
                    tracing::error!("rmcp server stopped: {e:?}");
                }
            }
            Err(e) => {
                tracing::error!("rmcp serve failed: {e:?}");
            }
        }
        tasks.shutdown();
        if let Some(handle) = http_handle {
            handle.abort();
            let _ = handle.await;
        }
    }

    // ── Stop the engine ──
    startup_handle.abort();
    let _ = startup_handle.await;
    {
        let mut eng = engine.lock().await;
        eng.stop().await;
    }

    tracing::info!("sermcp stopped");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pid that was definitely alive and definitely is dead now: spawn a
    /// child, reap it, reuse its id. Guessing pids is flaky under pid
    /// namespaces and low pid_max limits.
    fn reaped_child_pid() -> u32 {
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn true");
        let status = child.wait().expect("reap child");
        assert!(status.success());
        child.id()
    }

    /// The tmp-file reaper must read the pid from the FIRST `pid[-seq]`
    /// field: dead owners are swept (both legacy and sequenced names), live
    /// owners and non-tmp files stay untouched.
    #[test]
    fn stdio_display_claim_preserves_live_hook_owner_and_replaces_dead_owner() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".dut-serial");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("owner.json");
        let live = serde_json::json!({
            "schema_version": 1,
            "owner_session_id": "first-agent",
            "owner_mcp_pid": std::process::id(),
        });
        std::fs::write(&path, live.to_string()).unwrap();
        claim_stdio_display_owner(tmp.path());
        let kept: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(kept, live);

        let dead = serde_json::json!({"schema_version": 1, "owner_mcp_pid": i32::MAX as u32});
        std::fs::write(&path, dead.to_string()).unwrap();
        claim_stdio_display_owner(tmp.path());
        let claimed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(claimed["owner_kind"], "stdio");
        assert_eq!(claimed["owner_mcp_pid"], std::process::id());
        assert!(!dir.join("owner.lock").exists());
    }

    #[test]
    fn stdio_display_claim_never_writes_through_a_contended_hook_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".dut-serial");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("owner.lock"), "").unwrap();
        claim_stdio_display_owner(tmp.path());
        assert!(!dir.join("owner.json").exists());
        assert!(dir.join("owner.lock").exists());
    }

    #[test]
    fn startup_hygiene_sweeps_only_dead_owner_tmp_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dut_serial = tmp.path().join(".dut-serial");
        std::fs::create_dir_all(&dut_serial).unwrap();

        let dead = reaped_child_pid();
        let alive = std::process::id();

        // Only the sequenced `tmp-<pid>-<seq>` form is honored. Unsequenced
        // names predate the per-call sequence and are swept unconditionally,
        // even when their pid is alive — every live writer stamps the seq.
        let cases = [
            (format!(".inventory.json.tmp-{dead}-17"), true),
            (format!(".inventory.json.tmp-{alive}-42"), false),
            (format!(".owner.json.tmp-{dead}-3"), true),
            (format!(".owner.json.tmp-{alive}-9"), false),
            (format!(".inventory.json.tmp-{dead}"), true),
            (format!(".inventory.json.tmp-{alive}"), true),
            (format!(".owner.json.tmp-{dead}"), true),
            (".inventory.json.tmp-not-a-pid-7".to_string(), true),
            // Not a tmp file: never swept.
            ("inventory.json".to_string(), false),
        ];
        let mut paths = Vec::new();
        for (name, _) in &cases {
            let path = dut_serial.join(name);
            std::fs::write(&path, "x").unwrap();
            paths.push(path);
        }

        startup_hygiene(tmp.path());

        for ((_, should_sweep), path) in cases.iter().zip(&paths) {
            assert_eq!(
                !path.exists(),
                *should_sweep,
                "wrong sweep decision for {}",
                path.display()
            );
        }
    }

    /// Concurrent writers in one process must never share a tmp name: the
    /// sequence suffix advances on every call, so a heartbeat write racing a
    /// state-change hook write cannot rename-publish each other's half file.
    #[test]
    fn tmp_sequence_is_unique_per_call() {
        let a = tmp_sequence();
        let b = tmp_sequence();
        assert_ne!(a, b, "sequence must advance per call");
    }
}
