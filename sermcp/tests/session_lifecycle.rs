//! Process-level session exit checks using scratch projects and loopback only.
#![cfg(target_os = "linux")]

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Guard(Child);
impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start_ticks(pid: u32) -> String {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .unwrap()
        .rsplit_once(") ")
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap()
        .to_string()
}

fn start_server(project: &Path, owner: &Child, http: bool) -> Guard {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let config = project.join(".target.jsonc");
    std::fs::write(&config, format!(r#"{{"dev_hosts":[{{"ip":"127.0.0.1","user":"root","duts":[{{"dut_name":"session-test","serial":{{"port":{port},"baudrate":115200}},"control":{{"dev_ctrl":"none"}}}}]}}]}}"#)).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_sermcp"));
    command
        .current_dir(project)
        .env("TARGET_CONF", config)
        .env_remove("TARGET_DUT_NAME")
        .env("SERMCP_SESSION_PID", owner.id().to_string())
        .env("SERMCP_SESSION_START_TICKS", start_ticks(owner.id()))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if http {
        let control = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = control.local_addr().unwrap().to_string();
        drop(control);
        command.args(["--http", &address, "--idle-timeout", "1"]);
    }
    let server = Guard(command.spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(10);
    while !project.join(".dut-serial/mcp.pid").exists() {
        assert!(
            Instant::now() < deadline,
            "server did not acquire project ownership"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    server
}

fn assert_exits(server: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        if let Some(status) = server.try_wait().unwrap() {
            assert!(status.success(), "server exited with {status}");
            return;
        }
        assert!(Instant::now() < deadline, "server outlived its session");
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn session_death_stops_stdio_even_with_stdin_held_open_and_stops_http_without_idle_timeout() {
    for http in [false, true] {
        let project = tempfile::tempdir().unwrap();
        let mut owner = Guard(Command::new("sleep").arg("30").spawn().unwrap());
        let mut server = start_server(project.path(), &owner.0, http);
        owner.0.kill().unwrap();
        owner.0.wait().unwrap();
        assert_exits(&mut server.0);
        assert!(!project.path().join(".dut-serial/mcp.pid").exists());
    }
}

#[test]
fn stdio_eof_stops_engine_while_session_process_is_alive() {
    let project = tempfile::tempdir().unwrap();
    let owner = Guard(Command::new("sleep").arg("30").spawn().unwrap());
    let mut server = start_server(project.path(), &owner.0, false);
    drop(server.0.stdin.take());
    assert_exits(&mut server.0);
    assert!(!project.path().join(".dut-serial/mcp.pid").exists());
}

#[test]
fn http_survives_idle_and_original_owner_exit_with_another_session() {
    let project = tempfile::tempdir().unwrap();
    let mut first = Guard(Command::new("sleep").arg("30").spawn().unwrap());
    let mut second = Guard(Command::new("sleep").arg("30").spawn().unwrap());
    let mut server = start_server(project.path(), &first.0, true);
    let sessions = project.path().join(".dut-serial/sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(
        sessions.join("second"),
        format!(
            "timestamp\nguest\n{} {}\n",
            second.0.id(),
            start_ticks(second.0.id())
        ),
    )
    .unwrap();
    first.0.kill().unwrap();
    first.0.wait().unwrap();
    std::thread::sleep(Duration::from_secs(4));
    assert!(server.0.try_wait().unwrap().is_none());
    second.0.kill().unwrap();
    second.0.wait().unwrap();
    assert_exits(&mut server.0);
}

#[test]
fn dutabo_stop_releases_project_and_prevents_automatic_restart() {
    let project = tempfile::tempdir().unwrap();
    let owner = Guard(Command::new("sleep").arg("60").spawn().unwrap());
    let mut server = start_server(project.path(), &owner.0, true);
    let run = |command: &str| {
        Command::new(env!("CARGO_BIN_EXE_dutabo"))
            .current_dir(project.path())
            .env("TARGET_CONF", project.path().join(".target.jsonc"))
            .arg(command)
            .output()
            .unwrap()
    };
    // Startup publishes its PID before the control listener is ready.
    std::thread::sleep(Duration::from_secs(2));
    let port = sermcp::ports::recorded_mcp_port(project.path()).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let foreign = tempfile::tempdir().unwrap();
        let foreign_config = foreign.path().join(".target.jsonc");
        std::fs::write(&foreign_config, "{}").unwrap();
        for (config, pid) in [
            (foreign_config, server.0.id()),
            (project.path().join(".target.jsonc"), 0),
        ] {
            let response = client
                .post(format!("http://127.0.0.1:{port}/project/stop"))
                .json(&serde_json::json!({
                    "config_path": config,
                    "expected_pid": pid,
                }))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), reqwest::StatusCode::CONFLICT);
        }
    });
    assert!(server.0.try_wait().unwrap().is_none());
    let stopped = run("stop");
    assert!(
        stopped.status.success(),
        "{}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    assert_exits(&mut server.0);
    assert!(project.path().join(".dut-serial/stopped").exists());
    assert!(run("stop").status.success());
    let refused = lifecycle_command(project.path(), &["start", "--agent"])
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("dutabo resume"));
    assert!(project.path().join(".dut-serial/stopped").exists());
    let refused = Command::new(env!("CARGO_BIN_EXE_sermcp"))
        .current_dir(project.path())
        .env("TARGET_CONF", project.path().join(".target.jsonc"))
        .args(["--http", "127.0.0.1:0"])
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("dutabo resume"));
    assert!(run("resume").status.success());
    assert!(!project.path().join(".dut-serial/stopped").exists());
}

#[test]
fn persistent_start_ignores_agent_lifetime_and_reuses_service() {
    let project = tempfile::tempdir().unwrap();
    let mut owner = Guard(Command::new("sleep").arg("60").spawn().unwrap());
    let mut old = start_server(project.path(), &owner.0, true);
    let run = |action: &str| {
        Command::new(env!("CARGO_BIN_EXE_dutabo"))
            .current_dir(project.path())
            .env("TARGET_CONF", project.path().join(".target.jsonc"))
            .env("SERMCP_SESSION_PID", owner.0.id().to_string())
            .env("SERMCP_SESSION_START_TICKS", start_ticks(owner.0.id()))
            .arg(action)
            .output()
            .unwrap()
    };
    std::thread::sleep(Duration::from_secs(2));
    assert!(run("stop").status.success());
    assert_exits(&mut old.0);
    assert!(project.path().join(".dut-serial/stopped").exists());
    let started = run("start");
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    let pid_path = project.path().join(".dut-serial/mcp.pid");
    let pid = std::fs::read_to_string(&pid_path).unwrap();
    assert!(!project.path().join(".dut-serial/stopped").exists());
    assert!(run("start").status.success());
    assert_eq!(std::fs::read_to_string(&pid_path).unwrap(), pid);
    let hook_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("hooks/claude");
    let cleanup = Command::new("python3").arg("-c")
        .arg("import sys, importlib.util; sys.path.insert(0, sys.argv[1]); spec=importlib.util.spec_from_file_location('stop_hook', sys.argv[1]+'/session-stop.py'); module=importlib.util.module_from_spec(spec); spec.loader.exec_module(module); assert module._terminate_project_servers(sys.argv[2]) == []")
        .arg(hook_dir).arg(project.path()).status().unwrap();
    assert!(cleanup.success());

    owner.0.kill().unwrap();
    owner.0.wait().unwrap();
    let challenger = Command::new(env!("CARGO_BIN_EXE_sermcp"))
        .current_dir(project.path())
        .env("TARGET_CONF", project.path().join(".target.jsonc"))
        .args(["--http", "127.0.0.1:0"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(!challenger.status.success());
    assert_eq!(std::fs::read_to_string(&pid_path).unwrap(), pid);
    assert!(sermcp::lock_manager::process_alive(
        pid.trim().parse().unwrap()
    ));
    let stopped = Command::new(env!("CARGO_BIN_EXE_dutabo"))
        .current_dir(project.path())
        .env("TARGET_CONF", project.path().join(".target.jsonc"))
        .arg("stop")
        .output()
        .unwrap();
    assert!(
        stopped.status.success(),
        "{}",
        String::from_utf8_lossy(&stopped.stderr)
    );
}

fn lifecycle_command(project: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dutabo"));
    command
        .current_dir(project)
        .env("TARGET_CONF", project.join(".target.jsonc"))
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    command
}

fn assert_start_refused(child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(
                !status.success(),
                "inhibited startup unexpectedly succeeded"
            );
            return;
        }
        assert!(Instant::now() < deadline, "inhibited startup did not exit");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn write_lifecycle_config(project: &Path) {
    let serial = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = serial.local_addr().unwrap().port();
    std::fs::write(
        project.join(".target.jsonc"),
        format!(r#"{{"dev_hosts":[{{"ip":"127.0.0.1","user":"root","duts":[{{"dut_name":"race-test","serial":{{"port":{port},"baudrate":115200}},"control":{{"dev_ctrl":"none"}}}}]}}]}}"#),
    ).unwrap();
}

#[tokio::test]
async fn agent_start_waiting_at_gate_cannot_clear_a_later_human_stop() {
    let project = tempfile::tempdir().unwrap();
    write_lifecycle_config(project.path());
    let gate = sermcp::lifecycle::StartupGate::acquire(project.path())
        .await
        .unwrap();
    let mut child = Guard(
        lifecycle_command(project.path(), &["start", "--agent"])
            .spawn()
            .unwrap(),
    );
    // Observe that dutabo has taken the control lock and is waiting for
    // startup admission, rather than relying on a timing-only race.
    let control_path = project.path().join(".dut-serial/start.lock");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(control) = std::fs::File::open(&control_path)
            && control.try_lock().is_err()
        {
            break;
        }
        assert!(Instant::now() < deadline, "agent did not reach the gate");
        assert!(child.0.try_wait().unwrap().is_none());
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    gate.inhibit().unwrap();
    drop(gate);
    assert_start_refused(&mut child.0);
    assert!(project.path().join(".dut-serial/stopped").exists());
    assert!(!project.path().join(".dut-serial/mcp.pid").exists());
}

#[test]
fn agent_start_preserves_inhibit_even_when_a_persistent_server_is_ready() {
    let project = tempfile::tempdir().unwrap();
    write_lifecycle_config(project.path());
    let started = lifecycle_command(project.path(), &["start", "--agent"])
        .output()
        .unwrap();
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    let pid: u32 = std::fs::read_to_string(project.path().join(".dut-serial/mcp.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // A persistent child is detached from dutabo. Always clean it up even
    // if a subsequent assertion fails.
    struct Cleanup(u32, std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            if std::fs::read_to_string(self.1.join(".dut-serial/mcp.pid"))
                .ok()
                .is_none_or(|pid| pid.trim() != self.0.to_string())
            {
                return;
            }
            // SAFETY: SIGTERM addresses only the server PID captured by this test.
            unsafe {
                libc::kill(self.0 as i32, libc::SIGTERM);
            }
        }
    }
    let _cleanup = Cleanup(pid, project.path().to_path_buf());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let gate = sermcp::lifecycle::StartupGate::acquire(project.path())
            .await
            .unwrap();
        gate.inhibit().unwrap();
    });
    let refused = lifecycle_command(project.path(), &["start", "--agent"])
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("human"));
    assert!(project.path().join(".dut-serial/stopped").exists());
    assert_eq!(
        std::fs::read_to_string(project.path().join(".dut-serial/mcp.pid"))
            .unwrap()
            .trim(),
        pid.to_string()
    );
    // `resume --agent` stays rejected: an Agent must never clear the inhibit.
    let refused = lifecycle_command(project.path(), &["resume", "--agent"])
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(project.path().join(".dut-serial/stopped").exists());
    assert!(
        lifecycle_command(project.path(), &["stop"])
            .status()
            .unwrap()
            .success()
    );
}

/// `dutabo stop --agent` stops the service and releases project ownership
/// WITHOUT writing or clearing the startup inhibit: a human inhibit survives
/// it, so Agent start stays refused until a human resumes.
#[test]
fn agent_stop_releases_ownership_without_touching_the_inhibit() {
    let project = tempfile::tempdir().unwrap();
    write_lifecycle_config(project.path());
    let started = lifecycle_command(project.path(), &["start", "--agent"])
        .output()
        .unwrap();
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    let pid: u32 = std::fs::read_to_string(project.path().join(".dut-serial/mcp.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // A persistent child is detached from dutabo. Always clean it up even
    // if a subsequent assertion fails.
    struct Cleanup(u32);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            if sermcp::lock_manager::process_alive(self.0) {
                // SAFETY: SIGTERM addresses only the server PID captured by this test.
                unsafe {
                    libc::kill(self.0 as i32, libc::SIGTERM);
                }
            }
        }
    }
    let _cleanup = Cleanup(pid);
    // A human inhibit must survive an Agent stop untouched.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let gate = sermcp::lifecycle::StartupGate::acquire(project.path())
            .await
            .unwrap();
        gate.inhibit().unwrap();
    });
    let stopped = lifecycle_command(project.path(), &["stop", "--agent"])
        .output()
        .unwrap();
    assert!(
        stopped.status.success(),
        "{}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    let deadline = Instant::now() + Duration::from_secs(6);
    while sermcp::lock_manager::process_alive(pid) {
        assert!(
            Instant::now() < deadline,
            "agent stop did not stop the service"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(project.path().join(".dut-serial/stopped").exists());
    // The surviving inhibit still refuses Agent startup.
    let refused = lifecycle_command(project.path(), &["start", "--agent"])
        .output()
        .unwrap();
    assert!(!refused.status.success());
    // A human resume re-enables startup and the Agent can take ownership again.
    assert!(
        lifecycle_command(project.path(), &["resume"])
            .status()
            .unwrap()
            .success()
    );
    let restarted = lifecycle_command(project.path(), &["start", "--agent"])
        .output()
        .unwrap();
    assert!(
        restarted.status.success(),
        "{}",
        String::from_utf8_lossy(&restarted.stderr)
    );
    let replacement: u32 = std::fs::read_to_string(project.path().join(".dut-serial/mcp.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let _replacement_cleanup = Cleanup(replacement);
    // The final human stop parks the replacement service with the inhibit.
    assert!(
        lifecycle_command(project.path(), &["stop"])
            .status()
            .unwrap()
            .success()
    );
    assert!(project.path().join(".dut-serial/stopped").exists());
}

#[tokio::test]
async fn direct_server_rechecks_human_stop_after_waiting_for_startup_gate() {
    let project = tempfile::tempdir().unwrap();
    write_lifecycle_config(project.path());
    let gate = sermcp::lifecycle::StartupGate::acquire(project.path())
        .await
        .unwrap();
    let mut server = Guard(
        Command::new(env!("CARGO_BIN_EXE_sermcp"))
            .current_dir(project.path())
            .env("TARGET_CONF", project.path().join(".target.jsonc"))
            .args(["--http", "127.0.0.1:0", "--idle-timeout", "0"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(server.0.try_wait().unwrap().is_none());
    assert!(!project.path().join(".dut-serial/mcp.pid").exists());
    gate.inhibit().unwrap();
    drop(gate);
    assert_start_refused(&mut server.0);
    assert!(!project.path().join(".dut-serial/mcp.pid").exists());
    assert!(project.path().join(".dut-serial/stopped").exists());
}
