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
        .env_remove("TARGET_DUT_ALIAS")
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
                .json(&serde_json::json!({"config_path": config, "expected_pid": pid}))
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
    let started = run("start");
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    let pid_path = project.path().join(".dut-serial/mcp.pid");
    let pid = std::fs::read_to_string(&pid_path).unwrap();
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
    std::thread::sleep(Duration::from_secs(4));
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

#[test]
fn agent_start_rejects_only_the_same_host_and_port() {
    let project = tempfile::tempdir().unwrap();
    let locks = tempfile::tempdir().unwrap();
    let host = "127.0.0.2";
    let port = "45671";
    let lock_dir = locks.path().to_str().unwrap();
    assert!(sermcp::lock_manager::acquire_lock(host, port, lock_dir).is_none());
    for (candidate_host, candidate_port, conflict) in [
        (host, port, true),
        ("127.0.0.3", port, false),
        (host, "45672", false),
    ] {
        let config = project.path().join(".target.jsonc");
        std::fs::write(&config, serde_json::json!({"lock_dir": lock_dir,
            "dev_hosts": [{"ip": candidate_host, "duts": [{"dut_name": "test", "serial": {"port": candidate_port, "baudrate": 115200}, "control": {"dev_ctrl": "none"}}]}]
        }).to_string()).unwrap();
        let mut server = Guard(
            Command::new(env!("CARGO_BIN_EXE_sermcp"))
                .current_dir(project.path())
                .env("TARGET_CONF", config)
                .args(["--http", "127.0.0.1:0"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        std::thread::sleep(Duration::from_secs(1));
        if conflict {
            assert!(server.0.try_wait().unwrap().is_some_and(|s| !s.success()));
            let mut error = String::new();
            use std::io::Read;
            server
                .0
                .stderr
                .take()
                .unwrap()
                .read_to_string(&mut error)
                .unwrap();
            assert!(error.contains("Serial endpoint conflict"), "{error}");
            assert!(error.contains("project") && error.contains("PID"));
        } else {
            assert!(server.0.try_wait().unwrap().is_none());
        }
    }
    sermcp::lock_manager::release_lock(host, port, lock_dir);
}
