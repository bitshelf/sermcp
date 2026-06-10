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
        command.args(["--http", "127.0.0.1:0", "--idle-timeout", "0"]);
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
