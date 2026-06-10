//! Multi-agent isolation integration tests (plan §6: R8, R9, R11, R12).
//!
//! Spawn the real `sermcp` binary against unique scratch
//! projects under a TempDir. The derived /dev/shm filenames hash the
//! canonical scratch project path, so the two LIVE locks (the rockchip and
//! tina_v14 projects) can never be touched. Panic-safe guards kill the
//! spawned servers and unlink the scratch lock/status files even when an
//! assertion fails (R15 residue hygiene).
//!
//! These are process-level regressions for project-scoped lifecycle ownership,
//! cross-project state isolation, global physical-endpoint exclusion, stale
//! residue cleanup, and strict same-project process singleton behavior.

use md5::{Digest, Md5};
use sermcp::ports;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ExitStatus};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

fn test_serial_port() -> u16 {
    *TEST_SERIAL_PORT.get_or_init(|| {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.local_addr().unwrap().port()
    })
}

static TEST_SERIAL_PORT: OnceLock<u16> = OnceLock::new();

/// Canonical project hash: md5(canonical project dir)[:8] — byte-identical
/// to the Python `md5(str(Path(p).resolve()))[:8]` definition (D8).
fn project_hash(project: &Path) -> String {
    let canonical = std::fs::canonicalize(project).unwrap_or_else(|_| project.to_path_buf());
    let digest = Md5::digest(canonical.to_string_lossy().as_bytes());
    ports::md5_hex(&digest).chars().take(8).collect()
}

fn session_lock_path(project: &Path) -> PathBuf {
    PathBuf::from(format!(
        "/dev/shm/claude-serial-{}.lock",
        project_hash(project)
    ))
}

fn shm_cache_path(project: &Path) -> PathBuf {
    PathBuf::from(format!("/dev/shm/claude-status-{}", project_hash(project)))
}

/// Unlinks the scratch lock + shm cache files for a project on drop.
struct ScratchResidueGuard {
    paths: Vec<PathBuf>,
}

impl ScratchResidueGuard {
    fn for_project(project: &Path) -> Self {
        Self {
            paths: vec![session_lock_path(project), shm_cache_path(project)],
        }
    }
}

impl Drop for ScratchResidueGuard {
    fn drop(&mut self) {
        for path in &self.paths {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Kills the spawned server on drop (panic-safe).
struct ChildGuard {
    child: Child,
    stdin: Option<ChildStdin>,
}

impl ChildGuard {
    fn close_stdin(&mut self) {
        self.stdin.take();
    }

    fn try_wait(&mut self) -> Option<ExitStatus> {
        match self.child.try_wait() {
            Ok(status) => status,
            Err(e) => panic!("try_wait failed: {e}"),
        }
    }

    fn wait_timeout(&mut self, secs: u64) -> Option<ExitStatus> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            if let Some(status) = self.try_wait() {
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn write_target_jsonc(project: &Path, dut_name: &str) -> PathBuf {
    let conf = project.join(".target.jsonc");
    let serial_port = test_serial_port();
    std::fs::write(
        &conf,
        format!(
            r#"{{
  "dev_hosts": [
    {{
      "ip": "127.0.0.1",
      "user": "root",
      "duts": [
        {{"dut_name": "{dut_name}", "serial": {{"port": {serial_port}, "baudrate": 460800}}}}
      ]
    }}
  ]
}}
"#
        ),
    )
    .unwrap();
    conf
}

fn spawn_server(conf: &Path, project: &Path) -> ChildGuard {
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_sermcp"))
        .env("TARGET_CONF", conf)
        .current_dir(project)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn sermcp");
    let stdin = child.stdin.take();
    ChildGuard { child, stdin }
}

/// A second stdio spawn that is expected to BRIDGE into the running
/// singleton: stdout must be readable (the bridged MCP frames).
fn spawn_stdio_bridge_client(conf: &Path, project: &Path) -> (ChildGuard, LineReader) {
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_sermcp"))
        .env("TARGET_CONF", conf)
        .current_dir(project)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn sermcp");
    let stdin = child.stdin.take();
    let reader = LineReader::spawn(child.stdout.take().expect("piped stdout"));
    (ChildGuard { child, stdin }, reader)
}

/// Background line reader with a per-line deadline, so a silent bridge
/// fails the test instead of hanging it.
struct LineReader {
    lines: std::sync::mpsc::Receiver<String>,
}

impl LineReader {
    fn spawn(stdout: std::process::ChildStdout) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(stdout).lines() {
                match line {
                    Ok(line) if !line.trim().is_empty() => {
                        if tx.send(line).is_err() {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        });
        Self { lines: rx }
    }

    fn next_frame(&self, secs: u64) -> serde_json::Value {
        let line = self
            .lines
            .recv_timeout(Duration::from_secs(secs))
            .expect("bridge must answer, not stay silent");
        serde_json::from_str(line.trim()).expect("one JSON-RPC frame per line")
    }
}

/// Wait until a loopback port accepts connections (the singleton's HTTP
/// control listener is bound slightly AFTER inventory.json is written).
fn wait_for_port(port: u16, secs: u64) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("timed out waiting for 127.0.0.1:{port}");
}

fn wait_for_file(path: &Path, secs: u64) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if path.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("timed out waiting for {}", path.display());
}

/// R8: the winner server writes the 4-line session lock
/// (project_dir, owner_pid, session_id, ts) keyed by the canonical project
/// hash, and unlinks it on clean exit.
#[test]
fn session_lock_is_new_format_and_unlinked_on_clean_exit() {
    let tmp = tempfile::TempDir::new().unwrap();
    let project = tmp.path();
    let conf = write_target_jsonc(project, "t8");
    let _residue = ScratchResidueGuard::for_project(project);
    let lock_path = session_lock_path(project);

    let mut child = spawn_server(&conf, project);
    wait_for_file(&project.join(".dut-serial/mcp.pid"), 30);
    assert!(
        lock_path.exists(),
        "server must claim a session lock at startup: {}",
        lock_path.display()
    );
    let content = std::fs::read_to_string(&lock_path).unwrap_or_default();
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(
        lines.len(),
        4,
        "session lock must be 4 lines (project_dir, owner_pid, session_id, ts); got {content:?}"
    );
    assert_eq!(
        lines[0],
        std::fs::canonicalize(project).unwrap().to_string_lossy()
    );
    let owner_pid: u32 = lines[1].parse().expect("owner_pid must be numeric");
    assert_eq!(
        owner_pid,
        child.child.id(),
        "owner_pid must be the server pid"
    );
    assert!(
        !lines[2].is_empty(),
        "session_id line must not be empty ('-' when unknown)"
    );
    let _ts: i64 = lines[3].parse().expect("timestamp must be numeric");

    // Clean exit (stdin EOF): the server must unlink its own session lock.
    child.close_stdin();
    let status = child
        .wait_timeout(30)
        .expect("server must exit after stdin EOF");
    assert!(status.success(), "clean stdio exit expected");
    assert!(
        !lock_path.exists(),
        "clean exit must unlink the session lock"
    );
}

/// R11/R12b (D5/D6): with a live owner holding the project lock, a second
/// stdio server does NOT exit with the lock-refusal error (the pre-bridge
/// behavior surfaced as `-32000` in agent hosts) — it bridges into the
/// running instance, serves a full MCP session over stdio, and never
/// creates an engine or rewrites project state.
#[test]
fn second_stdio_server_bridges_without_touching_state() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path();
    let conf = write_target_jsonc(project, "t11");
    let _residue = ScratchResidueGuard::for_project(project);

    let mut first = spawn_server(&conf, project);
    wait_for_file(&project.join(".dut-serial/mcp.pid"), 30);
    wait_for_file(&project.join(".dut-serial/inventory.json"), 30);
    wait_for_port(ports::project_mcp_port(project), 30);
    let inv_before = std::fs::read(project.join(".dut-serial/inventory.json")).unwrap();
    let mt_before = std::fs::metadata(project.join(".dut-serial/inventory.json"))
        .unwrap()
        .modified()
        .unwrap();
    let owner_pid = first.child.id().to_string();

    let (mut second, frames) = spawn_stdio_bridge_client(&conf, project);
    use std::io::Write as _;
    second
        .stdin
        .as_mut()
        .expect("bridge stdin")
        .write_all(
            concat!(
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":"#,
                r#"{"protocolVersion":"2025-06-18","capabilities":{},"#,
                r#""clientInfo":{"name":"isolation-test","version":"1"}}}"#,
                "\n"
            )
            .as_bytes(),
        )
        .expect("write initialize");
    let init = frames.next_frame(30);
    assert_eq!(
        init["id"], 1,
        "initialize answered through the bridge: {init}"
    );
    assert!(
        init["result"]["serverInfo"].is_object(),
        "bridged session reached the real server: {init}"
    );
    assert!(
        second.try_wait().is_none(),
        "the bridged process must stay alive while its stdio client is connected"
    );
    assert!(first.try_wait().is_none(), "first server remains alive");
    assert_eq!(
        std::fs::read_to_string(project.join(".dut-serial/mcp.pid")).unwrap(),
        owner_pid,
        "the bridged process must not replace the owner's PID"
    );

    assert_eq!(
        std::fs::read(project.join(".dut-serial/inventory.json")).unwrap(),
        inv_before,
        "the bridged process must never rewrite inventory"
    );
    let mt_after = std::fs::metadata(project.join(".dut-serial/inventory.json"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(
        mt_after, mt_before,
        "the bridged process must not touch inventory mtime"
    );

    second.close_stdin();
    let status = second
        .wait_timeout(15)
        .expect("bridge exits after stdin EOF");
    assert!(
        status.success(),
        "bridge clean exit expected, got {status:?}"
    );

    first.close_stdin();
    let _ = first.wait_timeout(15);
}

/// Different project paths have independent lifecycle ownership. If they are
/// accidentally configured for the same physical endpoint, the later project
/// must become a serial non-owner without killing, rewriting, or stealing from
/// the first project. Physical host:port exclusivity is global by necessity;
/// project lifecycle and files remain strictly path-scoped.
#[test]
fn different_projects_never_steal_same_serial_endpoint() {
    let tmp = tempfile::TempDir::new().unwrap();
    let first_project = tmp.path().join("first");
    let second_project = tmp.path().join("second");
    std::fs::create_dir_all(&first_project).unwrap();
    std::fs::create_dir_all(&second_project).unwrap();
    let first_conf = write_target_jsonc(&first_project, "first-dut");
    let second_conf = write_target_jsonc(&second_project, "second-dut");
    let _first_residue = ScratchResidueGuard::for_project(&first_project);
    let _second_residue = ScratchResidueGuard::for_project(&second_project);

    let mut first = spawn_server(&first_conf, &first_project);
    wait_for_file(&first_project.join(".dut-serial/mcp.pid"), 30);
    let lock_dir = sermcp::lock_manager::default_lock_dir();
    let serial_port = test_serial_port().to_string();
    let endpoint_lock =
        sermcp::lock_manager::endpoint_lock_path("127.0.0.1", &serial_port, &lock_dir);
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline
        && sermcp::lock_manager::endpoint_lock_owner(&endpoint_lock) != Some(first.child.id())
    {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(
        sermcp::lock_manager::endpoint_lock_owner(&endpoint_lock),
        Some(first.child.id()),
        "first project owns the physical endpoint"
    );

    let mut second = spawn_server(&second_conf, &second_project);
    wait_for_file(&second_project.join(".dut-serial/mcp.pid"), 30);
    std::thread::sleep(Duration::from_secs(2));
    assert!(first.try_wait().is_none(), "first project remains alive");
    assert!(
        second.try_wait().is_none(),
        "second project remains an observer"
    );
    assert_eq!(
        sermcp::lock_manager::endpoint_lock_owner(&endpoint_lock),
        Some(first.child.id()),
        "second project cannot steal or replace endpoint ownership"
    );
    assert!(
        first_project.join(".dut-serial/inventory.json").exists(),
        "second project did not delete first project state"
    );

    second.close_stdin();
    let _ = second.wait_timeout(15);
    assert_eq!(
        sermcp::lock_manager::endpoint_lock_owner(&endpoint_lock),
        Some(first.child.id()),
        "stopping the second project cannot release the first project's lock"
    );
    first.close_stdin();
    let _ = first.wait_timeout(15);
}

/// R12 (B4): startup sweeps `.inventory.json.tmp-<pid>-<seq>` files whose
/// pid is dead; a tmp file whose pid is alive must survive. The unsequenced
/// form predates the per-call sequence and is swept unconditionally (no
/// back-compatibility — every live writer stamps the sequence).
#[test]
fn startup_sweeps_dead_pid_inventory_tmp_files() {
    let tmp = tempfile::TempDir::new().unwrap();
    let project = tmp.path();
    std::fs::create_dir_all(project.join(".dut-serial")).unwrap();
    let dead_tmp = project.join(".dut-serial/.inventory.json.tmp-99999999-5");
    let live_tmp = project
        .join(".dut-serial")
        .join(format!(".inventory.json.tmp-{}-1", std::process::id()));
    let unsequenced_tmp = project.join(".dut-serial/.inventory.json.tmp-99999998");
    std::fs::write(&dead_tmp, "dead").unwrap();
    std::fs::write(&live_tmp, "live").unwrap();
    std::fs::write(&unsequenced_tmp, "unsequenced").unwrap();
    std::fs::write(&dead_tmp, "dead").unwrap();
    std::fs::write(&live_tmp, "live").unwrap();

    let conf = write_target_jsonc(project, "t12");
    let _residue = ScratchResidueGuard::for_project(project);
    let mut child = spawn_server(&conf, project);
    wait_for_file(&project.join(".dut-serial/inventory.json"), 30);

    assert!(
        !dead_tmp.exists(),
        "inventory tmp with a dead suffix pid must be swept at startup"
    );
    assert!(
        live_tmp.exists(),
        "inventory tmp with a live suffix pid must survive the sweep"
    );
    assert!(
        !unsequenced_tmp.exists(),
        "unsequenced tmp names are stale pre-sequence leftovers and must be swept"
    );

    child.close_stdin();
    let _ = child.wait_timeout(15);
}
