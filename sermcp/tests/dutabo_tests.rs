//! Integration tests for dutabo CLI.
//! Run with: cargo test --test dutabo_tests

use std::process::Command;

fn dutabo_bin() -> std::path::PathBuf {
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_dutabo") {
        let p = std::path::PathBuf::from(&path);
        if p.exists() {
            return p;
        }
    }
    if let Ok(manifest) = std::env::var("CARGO_MANIFEST_DIR") {
        for subdir in &["target/debug/dutabo", "target/release/dutabo"] {
            let p = std::path::PathBuf::from(&manifest).join(subdir);
            if p.exists() {
                return p;
            }
        }
    }
    std::path::PathBuf::from("target/debug/dutabo")
}

fn make_config(dir: &std::path::Path, content: &str) {
    std::fs::write(dir.join(".target.jsonc"), content).unwrap();
}

/// Reserve a loopback port the OS currently reports free, release it, and
/// return it. Pins `--mcp-port` for the "no server" tests: the per-project
/// md5-derived probe ports (3001..3099) can collide with real per-DUT
/// sermcp servers running on this machine (session-start hook),
/// which makes the probe connect to a LIVE server and hang for minutes
/// instead of failing fast.
fn free_loopback_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("failed to reserve a loopback port")
        .local_addr()
        .expect("failed to read local addr")
        .port()
}

/// Minimal single-host / single-DUT JSONC fixture (no duts → default DUT).
const SIMPLE_JSONC: &str =
    r#"{"dev_hosts":[{"ip":"10.0.0.1","user":"test"}],"serial":{"port":41000}}"#;

// ── list ────────────────────────────────────────────────────────────────

#[test]
fn test_list_single_dut() {
    let tmp = tempfile::TempDir::new().unwrap();
    make_config(
        tmp.path(),
        r#"{"dev_hosts":[{"ip":"10.0.0.1","user":"test"}],"serial":{"port":41000},"target":{"login_user":"root"}}"#,
    );
    let out = Command::new(dutabo_bin())
        .arg("list")
        .current_dir(tmp.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success());
    assert!(stdout.contains("NAME"), "{stdout}");
    assert!(stdout.contains("41000"), "{stdout}");
}

#[test]
fn test_list_multi_dut() {
    let tmp = tempfile::TempDir::new().unwrap();
    make_config(
        tmp.path(),
        r#"{
"dev_hosts": [
  {
    "ip": "10.0.0.1",
    "user": "test",
    "duts": [
      { "dut_name": "board-a", "serial": { "port": 41000 }, "target": { "login_user": "root" } },
      { "dut_name": "board-b", "serial": { "port": 2010 }, "target": { "login_user": "admin" } }
    ]
  }
]
}"#,
    );
    let out = Command::new(dutabo_bin())
        .arg("list")
        .current_dir(tmp.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success());
    assert!(stdout.contains("board-a"), "{stdout}");
    assert!(stdout.contains("board-b"), "{stdout}");
    assert!(stdout.contains("test@10.0.0.1"), "{stdout}");
}

#[test]
fn test_list_backward_compat() {
    // The single-dev-host shape: no duts[] → one default DUT that
    // inherits the global serial.port.
    let tmp = tempfile::TempDir::new().unwrap();
    make_config(
        tmp.path(),
        r#"{"dev_hosts":[{"ip":"host-a.invalid","user":"linaro"}],"serial":{"port":9999}}"#,
    );
    let out = Command::new(dutabo_bin())
        .arg("list")
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("9999"));
}

#[test]
fn test_list_no_config() {
    let tmp = tempfile::TempDir::new().unwrap();
    let out = Command::new(dutabo_bin())
        .arg("list")
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stderr).contains(".target.jsonc"));
    assert_eq!(out.status.code(), Some(1));
}

/// A directory holding only an unrelated file (no .target.jsonc) gets the
/// plain config-missing error — only `.target.jsonc` is ever consulted.
#[test]
fn test_list_stray_file_plain_missing_error() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".target.yaml"), "dev_host: [x]\n").unwrap();
    let out = Command::new(dutabo_bin())
        .arg("list")
        .current_dir(tmp.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("No .target.jsonc found."), "{stderr}");
    assert!(!stderr.contains(".target.yaml"), "{stderr}");
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn test_list_empty_dut_default() {
    let tmp = tempfile::TempDir::new().unwrap();
    make_config(tmp.path(), SIMPLE_JSONC);
    let out = Command::new(dutabo_bin())
        .arg("list")
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("default"));
}

#[test]
fn test_list_rejects_unknown_positional() {
    // Only --json is a valid extra argument for `list` — anything else is a
    // usage error (scripts must be able to detect typos).
    let tmp = tempfile::TempDir::new().unwrap();
    make_config(tmp.path(), SIMPLE_JSONC);
    let out = Command::new(dutabo_bin())
        .args(["list", "bogus"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("does not accept positional"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ── list table / --json / names (TDD-15) ───────────────────────────────

/// Three DUTs nested under one host — the state-fixture base.
const THREE_DUTS_JSONC: &str = r#"{
"dev_hosts": [
  {
    "ip": "10.0.0.1",
    "user": "test",
    "duts": [
      { "dut_name": "board-a", "serial": { "port": 41000 } },
      { "dut_name": "board-b", "serial": { "port": 2010 } },
      { "dut_name": "board-c", "serial": { "port": 2020 } }
    ]
  }
]
}"#;

/// Project with per-DUT state files: board-a crashed (fresh), board-b active
/// (fresh — tests backdate it), board-c has NO state file.
fn stateful_project() -> tempfile::TempDir {
    let tmp = tempfile::TempDir::new().unwrap();
    make_config(tmp.path(), THREE_DUTS_JSONC);
    let a = tmp.path().join(".dut-serial/board-a");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::write(a.join("target-state"), "crashed").unwrap();
    let b = tmp.path().join(".dut-serial/board-b");
    std::fs::create_dir_all(&b).unwrap();
    std::fs::write(b.join("target-state"), "active").unwrap();
    tmp
}

/// Backdate a state file with GNU touch (Linux dev/CI environment); returns
/// false when touch is unavailable so assertions degrade gracefully.
fn backdate(path: &std::path::Path, when: &str) -> bool {
    Command::new("touch")
        .args(["-d", when])
        .arg(path)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[test]
fn test_list_table_states() {
    let tmp = stateful_project();
    let b_state = tmp.path().join(".dut-serial/board-b/target-state");
    let touched = backdate(&b_state, "42 minutes ago");
    let out = Command::new(dutabo_bin())
        .arg("list")
        .current_dir(tmp.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success());
    assert!(stdout.contains("NAME"), "{stdout}");
    // Kernel-crash state is prominent; a missing file renders explicit
    // "unknown" — never a healthy state.
    assert!(stdout.contains("✗ crashed"), "{stdout}");
    assert!(stdout.contains("unknown"), "{stdout}");
    // USER@IP carries the dev host user and ip.
    assert!(stdout.contains("test@10.0.0.1"), "{stdout}");
    assert!(stdout.contains("2010"), "{stdout}");
    // Age is never hidden: a 42-minute-old state file gets a suffix.
    if touched {
        assert!(stdout.contains("42m ago"), "{stdout}");
    }
    // Piped output carries no ANSI escapes (machine-greppable).
    assert!(
        !stdout.contains('\x1b'),
        "piped table must be plain: {stdout}"
    );
    // Aligned columns: every row has the same total width.
    let lines: Vec<&str> = stdout.lines().collect();
    assert!(lines.len() >= 4, "{stdout}");
    let header_len = lines[0].chars().count();
    for line in &lines[1..] {
        assert_eq!(line.chars().count(), header_len, "misaligned: {stdout}");
    }
}

#[test]
fn test_list_json_output() {
    let tmp = stateful_project();
    let out = Command::new(dutabo_bin())
        .arg("list")
        .arg("--json")
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let json: serde_json::Value =
        serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("invalid JSON: {e}\n{stdout}"));
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["written_by"], "dutabo");
    assert_eq!(json["dev_host_count"], 1);
    assert_eq!(json["dut_count"], 3);
    // TOML-era alias keys are gone from the wire shape: no host alias,
    // no host dut_aliases grouping, no DUT dev_host_alias back-reference.
    assert!(json["dev_hosts"][0].get("alias").is_none(), "{stdout}");
    assert!(
        json["dev_hosts"][0].get("dut_aliases").is_none(),
        "{stdout}"
    );
    let crashed = &json["duts"][0];
    assert_eq!(crashed["dut_name"], "board-a");
    assert!(crashed.get("dev_host_alias").is_none(), "{stdout}");
    assert_eq!(crashed["dev_host_ip"], "10.0.0.1");
    assert_eq!(crashed["ssh_user"], "test");
    assert_eq!(crashed["state"], "crashed");
    assert_eq!(crashed["state_label"], "✗ crashed");
    assert_eq!(crashed["critical"], true);
    assert!(
        crashed["age_secs"].as_u64().is_some(),
        "fresh state file has an age"
    );
    assert!(
        crashed["state_text"]
            .as_str()
            .unwrap()
            .contains("board-a:crashed")
    );
    // mcp_port comes from ports::project_dut_mcp_port — the value the
    // SessionStart hook will consume after it switches to this JSON.
    assert_eq!(
        crashed["mcp_port"].as_u64().unwrap(),
        u64::from(sermcp::ports::project_dut_mcp_port(tmp.path(), "board-a"))
    );
    // Missing state file → unknown, age null, not critical (never a
    // silently-healthy state).
    let missing = &json["duts"][2];
    assert_eq!(missing["dut_name"], "board-c");
    assert_eq!(missing["state"], "unknown");
    assert_eq!(missing["state_label"], "unknown");
    assert_eq!(missing["critical"], false);
    assert!(missing["age_secs"].is_null(), "missing file has no age");
    assert_eq!(missing["state_text"], "");
}

#[test]
fn test_list_names_machine_output() {
    // Hidden subcommand: one DUT name per line, no header/color — consumed
    // by completions/dutabo.bash.
    let tmp = stateful_project();
    let out = Command::new(dutabo_bin())
        .arg("names")
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(stdout, "board-a\nboard-b\nboard-c\n", "{stdout}");
}

// ── help / unknown ──────────────────────────────────────────────────────

#[test]
fn test_help_output() {
    let out = Command::new(dutabo_bin()).output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    for cmd in &[
        "list",
        "status",
        "serial",
        "reboot",
        "uboot",
        "uf",
        "flash-kernel",
    ] {
        assert!(stderr.contains(cmd), "help missing '{cmd}'");
    }
}

#[test]
fn test_state_command_is_removed() {
    let out = Command::new(dutabo_bin()).arg("state").output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("Unknown command: state"), "{stderr}");
    assert!(stderr.contains("status"), "{stderr}");
}

// ── version ─────────────────────────────────────────────────────────────

#[test]
fn test_version_flag() {
    // Both spellings print "dutabo v<CARGO_PKG_VERSION>" to stdout and exit 0
    // — without dumping the help text (A3 acceptance).
    for flag in ["--version", "-V"] {
        let out = Command::new(dutabo_bin()).arg(flag).output().unwrap();
        assert!(out.status.success(), "{flag}: status={:?}", out.status);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert_eq!(
            stdout.trim(),
            format!("dutabo v{}", env!("CARGO_PKG_VERSION")),
            "{flag}: stdout={stdout}"
        );
        assert!(
            !stdout.contains("Usage"),
            "{flag}: must not print the help text"
        );
    }
}

#[test]
fn test_version_does_not_need_config() {
    // --version is answered before any .target.jsonc lookup or MCP probing,
    // so it must work (and exit 0) even in an empty directory.
    let tmp = tempfile::TempDir::new().unwrap();
    let out = Command::new(dutabo_bin())
        .arg("--version")
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "status={:?}", out.status);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        stdout.trim(),
        format!("dutabo v{}", env!("CARGO_PKG_VERSION")),
        "{stdout}"
    );
}

#[test]
fn test_no_args_is_a_usage_error() {
    // A bare `dutabo` prints usage but is a usage error — exit 1.
    let out = Command::new(dutabo_bin()).output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("Usage"), "{stderr}");
    assert_eq!(out.status.code(), Some(1), "{stderr}");
}

#[test]
fn test_bare_help_flag_exits_zero() {
    // Explicit help is a success path and follows the CLI convention of
    // writing help to stdout rather than stderr.
    let cases: [&[&str]; 4] = [&["--help"], &["-h"], &["help"], &["serial", "--help"]];
    for args in cases {
        let out = Command::new(dutabo_bin()).args(args).output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stdout.contains("Usage"), "{args:?}: {stdout}");
        assert!(stderr.is_empty(), "{args:?}: {stderr}");
        assert!(out.status.success(), "{args:?}: status={:?}", out.status);
    }
}

#[test]
fn test_unknown_command() {
    // Unknown commands — including the never-a-subcommand `version` — must
    // print an error AND exit non-zero so scripts can detect failure.
    for cmd in ["nonexistent_cmd", "version"] {
        let out = Command::new(dutabo_bin()).arg(cmd).output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("Unknown command"), "{stderr}");
        assert_eq!(out.status.code(), Some(1), "{cmd}: stderr={stderr}");
    }
}

#[test]
fn test_serial_rejects_trailing_command() {
    let out = Command::new(dutabo_bin())
        .args(["serial", "list"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("does not accept positional arguments"),
        "{stderr}"
    );
    assert!(!String::from_utf8_lossy(&out.stdout).contains("Connected via MCP"));
    assert_eq!(out.status.code(), Some(1), "{stderr}");
}

#[test]
fn test_serial_help_does_not_open_console() {
    let out = Command::new(dutabo_bin())
        .args(["serial", "-h"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Usage:"), "{stdout}");
    assert!(!stdout.contains("Connected via MCP"));
}

// ── status / reboot / uboot (no server) ─────────────────────────────────

/// No-server fixture: 127.0.0.1 with an unused ser2net port.
const NO_SERVER_JSONC: &str = r#"{"dev_hosts":[{"ip":"127.0.0.1"}],"serial":{"port":59999}}"#;

#[test]
fn test_status_without_server() {
    let tmp = tempfile::TempDir::new().unwrap();
    make_config(tmp.path(), NO_SERVER_JSONC);
    let port = free_loopback_port();
    let out = Command::new(dutabo_bin())
        .args(["--mcp-port", &port.to_string(), "status"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Code agents:"), "{stdout}");
    assert!(
        stdout.contains("No registered code-agent sessions."),
        "{stdout}"
    );
    assert!(
        stderr.contains("not reachable") || stderr.contains("MCP") || stdout.contains("Status:"),
        "status={:?}\nstderr={stderr}\nstdout={stdout}",
        out.status
    );
}

#[test]
fn test_reboot_without_server() {
    let tmp = tempfile::TempDir::new().unwrap();
    make_config(tmp.path(), NO_SERVER_JSONC);
    let port = free_loopback_port();
    let out = Command::new(dutabo_bin())
        .args(["--mcp-port", &port.to_string(), "reboot"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stderr.contains("not reachable") || stderr.contains("MCP") || !stdout.trim().is_empty(),
        "status={:?}\nstderr={stderr}\nstdout={stdout}",
        out.status
    );
}

#[test]
fn test_uboot_without_server() {
    let tmp = tempfile::TempDir::new().unwrap();
    make_config(tmp.path(), NO_SERVER_JSONC);
    let port = free_loopback_port();
    let out = Command::new(dutabo_bin())
        .args(["--mcp-port", &port.to_string(), "uboot"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stderr.contains("not reachable") || stderr.contains("MCP") || !stdout.trim().is_empty(),
        "status={:?}\nstderr={stderr}\nstdout={stdout}",
        out.status
    );
}

// ── flash ───────────────────────────────────────────────────────────────

#[test]
fn test_uf_missing_arg() {
    let out = Command::new(dutabo_bin()).arg("uf").output().unwrap();
    assert!(String::from_utf8_lossy(&out.stderr).contains("Usage"));
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn test_flash_kernel_missing_arg() {
    let out = Command::new(dutabo_bin())
        .arg("flash-kernel")
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stderr).contains("Usage"));
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn test_uf_nonexistent_image() {
    let tmp = tempfile::TempDir::new().unwrap();
    make_config(
        tmp.path(),
        r#"{"dev_hosts":[{"ip":"127.0.0.1"}],"serial":{"port":59999},"flash":{"tool":"upgrade_tool","full_image_cmd":"uf {image}"}}"#,
    );
    let out = Command::new(dutabo_bin())
        .args(["uf", "/tmp/nonexistent_12345.img"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("not found") || stderr.contains("Error"),
        "status={:?}\nstderr={stderr}",
        out.status
    );
    // A missing image is an error path — scripts must see a non-zero exit.
    assert_eq!(
        out.status.code(),
        Some(1),
        "status={:?}\nstderr={stderr}",
        out.status
    );
}

// ── --dut flag ──────────────────────────────────────────────────────────

/// Two DUTs nested under one host.
const TWO_DUTS_JSONC: &str = r#"{
"dev_hosts": [
  {
    "ip": "10.0.0.1",
    "user": "test",
    "duts": [
      { "dut_name": "dut-a", "serial": { "port": 41000 } },
      { "dut_name": "dut-b", "serial": { "port": 2010 } }
    ]
  }
]
}"#;

#[test]
fn test_multi_dut_requires_selection() {
    let tmp = tempfile::TempDir::new().unwrap();
    make_config(tmp.path(), TWO_DUTS_JSONC);
    let out = Command::new(dutabo_bin())
        .arg("status")
        .current_dir(tmp.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Multiple") || stderr.contains("Select"),
        "{stderr}"
    );
    assert_eq!(out.status.code(), Some(1), "{stderr}");
}

#[test]
fn test_dut_flag_selects() {
    let tmp = tempfile::TempDir::new().unwrap();
    make_config(tmp.path(), TWO_DUTS_JSONC);
    let out = Command::new(dutabo_bin())
        .args(["--dut", "dut-a", "status"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("Multiple boards"), "{stderr}");
}

#[test]
fn test_dut_flag_invalid() {
    let tmp = tempfile::TempDir::new().unwrap();
    make_config(
        tmp.path(),
        r#"{"dev_hosts":[{"ip":"10.0.0.1","user":"test","duts":[{"dut_name":"real-dut","serial":{"port":41000}}]}]}"#,
    );
    let out = Command::new(dutabo_bin())
        .args(["--dut", "nonexistent", "status"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("not found") || stderr.contains("Available"),
        "{stderr}"
    );
    assert_eq!(out.status.code(), Some(1), "{stderr}");
}

// ── --mcp-port ──────────────────────────────────────────────────────────

#[test]
fn test_mcp_port_flag() {
    let tmp = tempfile::TempDir::new().unwrap();
    make_config(tmp.path(), NO_SERVER_JSONC);
    // A freshly released free port: pinning a random fixed port (e.g. 12345)
    // could collide with anything else on this host and hang the probe.
    let port = free_loopback_port();
    let out = Command::new(dutabo_bin())
        .args(["--mcp-port", &port.to_string(), "status"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(&port.to_string())
            || stderr.contains("MCP")
            || stderr.contains("not reachable"),
        "{stderr}"
    );
}

// ── tool-level failure exit codes ───────────────────────────────────────

/// Read one HTTP/1.1 request (headers + Content-Length body) from a socket.
fn read_http_request(sock: &mut std::net::TcpStream) -> Option<String> {
    use std::io::Read;
    let mut data = Vec::new();
    let mut buf = [0u8; 4096];
    let header_end = loop {
        let n = sock.read(&mut buf).ok()?;
        if n == 0 {
            return None;
        }
        data.extend_from_slice(&buf[..n]);
        if let Some(pos) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let content_length = String::from_utf8_lossy(&data[..header_end])
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    while data.len() < header_end + content_length {
        let n = sock.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
    }
    Some(String::from_utf8_lossy(&data).to_string())
}

/// Serve a minimal Streamable-HTTP MCP endpoint that answers `initialize`
/// (with a session header) and returns `tool_text` for every `tools/call`.
/// Returns the bound port. The thread is detached on purpose: it blocks on
/// `accept` once dutabo exits, and joining would hang the test — the thread
/// dies with the test binary.
fn spawn_failing_mcp_server(tool_text: &str) -> u16 {
    use std::io::Write;
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let tool_text = tool_text.to_string();
    std::thread::spawn(move || {
        for _ in 0..4 {
            let Ok((mut sock, _)) = listener.accept() else {
                break;
            };
            let Some(raw) = read_http_request(&mut sock) else {
                break;
            };
            let (status, extra_headers, resp_body) = if raw.contains("\"method\":\"initialize\"") {
                (
                    "200 OK",
                    "Mcp-Session-Id: fake-session\r\n",
                    r#"data: {"jsonrpc":"2.0","id":0,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"fake","version":"0.0.0"}}}"#.to_string(),
                )
            } else if raw.contains("\"method\":\"tools/call\"") {
                // The tool payload is a JSON *string* inside content[0].text —
                // build it with serde_json so it is escaped correctly.
                let frame = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "result": {
                        "content": [{"type": "text", "text": tool_text}],
                        "isError": false,
                    }
                });
                (
                    "200 OK",
                    "",
                    format!("data: {}", serde_json::to_string(&frame).unwrap()),
                )
            } else {
                ("202 Accepted", "", String::new())
            };
            let response = if resp_body.is_empty() {
                format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            } else {
                format!(
                    "HTTP/1.1 {status}\r\nContent-Type: text/event-stream\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n{resp_body}",
                    resp_body.len()
                )
            };
            let _ = sock.write_all(response.as_bytes());
        }
    });
    port
}

#[test]
fn test_button_tool_failure_exits_nonzero() {
    let tmp = tempfile::TempDir::new().unwrap();
    make_config(
        tmp.path(),
        r#"{"dev_hosts":[{"ip":"127.0.0.1"}],"serial":{"port":41000}}"#,
    );
    let port = spawn_failing_mcp_server(
        &serde_json::json!({"success": false, "error": "relay offline"}).to_string(),
    );
    let out = Command::new(dutabo_bin())
        .arg("--mcp-port")
        .arg(port.to_string())
        .arg("button")
        .arg("reset")
        .arg("pulse")
        .current_dir(tmp.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("button reset pulse failed"),
        "status={:?}\nstderr={stderr}",
        out.status
    );
    assert!(stderr.contains("relay offline"), "{stderr}");
    assert_eq!(out.status.code(), Some(1), "{stderr}");
}

#[test]
fn test_reboot_tool_failure_exits_nonzero() {
    let tmp = tempfile::TempDir::new().unwrap();
    make_config(
        tmp.path(),
        r#"{"dev_hosts":[{"ip":"127.0.0.1"}],"serial":{"port":41000}}"#,
    );
    let port = spawn_failing_mcp_server(
        &serde_json::json!({"success": false, "error": "relay offline"}).to_string(),
    );
    let out = Command::new(dutabo_bin())
        .arg("--mcp-port")
        .arg(port.to_string())
        .arg("reboot")
        .current_dir(tmp.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("reboot failed"), "{stderr}");
    assert!(stderr.contains("relay offline"), "{stderr}");
    assert_eq!(out.status.code(), Some(1), "{stderr}");
}

// ── relay disabled by default ───────────────────────────────────────────

#[test]
fn test_relay_disabled() {
    // An empty relay section (the JSONC spelling of "port commented out")
    // parses fine and the table renders — relay channels are not table columns.
    let tmp = tempfile::TempDir::new().unwrap();
    make_config(
        tmp.path(),
        r#"{"dev_hosts":[{"ip":"10.0.0.1"}],"serial":{"port":41000},"relay":{}}"#,
    );
    let out = Command::new(dutabo_bin())
        .arg("list")
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("41000"));
}

// ── help ────────────────────────────────────────────────────────────────

/// `dutabo --help` prints compact usage on stdout and exits 0. Spawns the already
/// built binary directly: `cargo run` here would (a) contend for the cargo
/// build lock the parent `cargo test` holds and (b) inherit the process-wide
/// CWD that parallel tests mutate via `set_current_dir` — both make the test
/// flaky in a full run.
#[test]
fn test_dutabo_help_output() {
    let out = Command::new(dutabo_bin()).arg("--help").output().unwrap();
    assert!(out.status.success(), "help must exit 0");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("dutabo") && stdout.contains("Usage"),
        "help output expected on stdout: {stdout}"
    );
    assert!(out.stderr.is_empty(), "stderr must be empty for help");
    assert!(stdout.lines().all(|line| line.chars().count() <= 88));
    assert!(stdout.contains("    help                            Show help"));
    for removed_section in [
        "Selection:",
        "Command options:",
        "Exit codes:",
        "General options:",
        "Examples:",
    ] {
        assert!(!stdout.contains(removed_section), "{stdout}");
    }
}

// ── config module unit tests ─────────────────────────────────────────────

#[test]
fn test_find_target_jsonc_nonexistent() {
    let tmp = tempfile::TempDir::new().unwrap();
    let old = std::env::current_dir().unwrap();
    std::env::set_current_dir(tmp.path()).unwrap();
    let cfg = sermcp::config::load_config();
    std::env::set_current_dir(&old).unwrap();
    assert!(cfg.config_path.is_none()); // no .target.jsonc in tmp
}

#[test]
fn test_dut_config_default_dut_name() {
    let tmp = tempfile::TempDir::new().unwrap();
    let conf = tmp.path().join(".target.jsonc");
    std::fs::write(
        &conf,
        r#"{
"dev_hosts": [
  {
    "ip": "10.0.0.1",
    "duts": [
      { "dut_name": "test-board", "serial": { "port": 41000 } }
    ]
  }
]
}"#,
    )
    .unwrap();
    let duts = sermcp::config::parse_dut_configs(&conf).unwrap();
    assert_eq!(duts.len(), 1);
    assert_eq!(duts[0].dut_name, "test-board");
    assert_eq!(duts[0].serial_port, "41000");
}

// ── Serial simulator tests ─────────────────────────────────────────────

// The serial-sim bin requires the `serial-sim` feature, so a plain
// `cargo test` builds no simulator binary; skip these tests there.
#[cfg(feature = "serial-sim")]
mod serial_sim {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::process::{Child, Command};
    use std::time::Duration;

    fn serial_sim_bin() -> std::path::PathBuf {
        if let Ok(path) = std::env::var("CARGO_BIN_EXE_serial-sim") {
            let p = std::path::PathBuf::from(&path);
            if p.exists() {
                return p;
            }
        }
        if let Ok(manifest) = std::env::var("CARGO_MANIFEST_DIR") {
            for subdir in &["target/debug/serial-sim", "target/release/serial-sim"] {
                let p = std::path::PathBuf::from(&manifest).join(subdir);
                if p.exists() {
                    return p;
                }
            }
        }
        std::path::PathBuf::from("target/debug/serial-sim")
    }

    struct SimGuard {
        child: Child,
        port: u16,
    }

    impl Drop for SimGuard {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    fn start_sim(no_login: bool) -> SimGuard {
        let bin = serial_sim_bin();
        // Use port 0 to let the OS pick a free port
        let mut cmd = Command::new(&bin);
        cmd.args(["--port", "0", "--print-port"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit());
        if no_login {
            cmd.arg("--no-login");
        }
        let mut child = cmd.spawn().expect("Failed to start serial-sim");

        // Read the dynamically-assigned port from stdout
        let port_str = {
            let stdout = child.stdout.as_mut().unwrap();
            let mut buf = [0u8; 16];
            let mut total = Vec::new();
            let start = std::time::Instant::now();
            while start.elapsed() < Duration::from_secs(3) {
                match stdout.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        total.extend_from_slice(&buf[..n]);
                        if total.contains(&b'\n') {
                            break;
                        }
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    Err(_) => break,
                }
            }
            String::from_utf8_lossy(&total).trim().to_string()
        };
        let port: u16 = port_str
            .parse()
            .unwrap_or_else(|_| panic!("serial-sim did not print a port, got: {port_str:?}"));
        assert!(port > 0, "Expected valid port, got {port}");

        // Give serial-sim time to start listening
        std::thread::sleep(Duration::from_millis(200));
        SimGuard { child, port }
    }

    fn read_until_timeout(stream: &mut TcpStream, timeout_ms: u64) -> Vec<u8> {
        let mut buf = [0u8; 4096];
        let mut all = Vec::new();
        let start = std::time::Instant::now();
        stream
            .set_read_timeout(Some(Duration::from_millis(200)))
            .ok();
        loop {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => all.extend_from_slice(&buf[..n]),
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if start.elapsed().as_millis() > timeout_ms as u128 {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(_) => break,
            }
        }
        all
    }

    #[test]
    fn test_prompt_starts_at_column_zero_no_login() {
        let sim = start_sim(true);
        let mut stream = TcpStream::connect(format!("127.0.0.1:{}", sim.port)).unwrap();

        // Read initial prompt
        let initial = read_until_timeout(&mut stream, 2000);
        let text = String::from_utf8_lossy(&initial);
        assert!(
            text.starts_with("root@"),
            "Initial prompt should start at column 0, got: {text:?}"
        );

        // Send empty line to get a fresh prompt
        stream.write_all(b"\n").unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let response = read_until_timeout(&mut stream, 2000);
        let text = String::from_utf8_lossy(&response);
        // Response: echo of Enter (\r\n) + prompt (\r\nroot@...)
        // The prompt itself must start at column 0 after a \r\n.
        assert!(
            text.contains("\r\nroot@"),
            "Prompt should appear at column 0 (after \\r\\n), got: {text:?}"
        );
    }

    #[test]
    fn test_command_echo_and_response() {
        let sim = start_sim(true);
        let mut stream = TcpStream::connect(format!("127.0.0.1:{}", sim.port)).unwrap();
        // Consume initial prompt
        read_until_timeout(&mut stream, 2000);

        // Send echo command
        stream.write_all(b"echo hello\n").unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let response = read_until_timeout(&mut stream, 2000);
        let text = String::from_utf8_lossy(&response);

        // Expected: echoed characters + \r\nhello\r\n<PROMPT>
        assert!(
            text.contains("hello"),
            "Should echo the command output, got: {text:?}"
        );
        assert!(
            text.contains("root@"),
            "Should end with a prompt, got: {text:?}"
        );
    }

    #[test]
    fn test_login_flow() {
        let sim = start_sim(false); // with login
        let mut stream = TcpStream::connect(format!("127.0.0.1:{}", sim.port)).unwrap();

        // Read initial login prompt
        let initial = read_until_timeout(&mut stream, 2000);
        let text = String::from_utf8_lossy(&initial);
        assert!(
            text.contains("login:"),
            "Should show login prompt, got: {text:?}"
        );

        // Send login username
        stream.write_all(b"root\n").unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let response = read_until_timeout(&mut stream, 2000);
        let text = String::from_utf8_lossy(&response);
        assert!(
            text.contains("root@"),
            "Should be logged in with root prompt, got: {text:?}"
        );
    }

    #[test]
    fn test_stty_echo_off_on() {
        let sim = start_sim(true);
        let mut stream = TcpStream::connect(format!("127.0.0.1:{}", sim.port)).unwrap();
        // Consume initial prompt
        read_until_timeout(&mut stream, 2000);

        // Turn echo off
        stream.write_all(b"stty -echo\n").unwrap();
        std::thread::sleep(Duration::from_millis(200));
        // Consume prompt
        read_until_timeout(&mut stream, 500);

        // Now send a command — should NOT echo characters back
        stream.write_all(b"uptime\n").unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let response = read_until_timeout(&mut stream, 2000);
        let text = String::from_utf8_lossy(&response);
        // Should NOT contain "uptime" (no echo), but should contain the output
        assert!(
            !text.contains("uptime"),
            "Command should not be echoed when echo is off, got: {text:?}"
        );
        assert!(
            text.contains("load average"),
            "Should still show command output, got: {text:?}"
        );
    }

    #[test]
    fn test_dmesg_output() {
        let sim = start_sim(true);
        let mut stream = TcpStream::connect(format!("127.0.0.1:{}", sim.port)).unwrap();
        read_until_timeout(&mut stream, 2000);

        stream.write_all(b"dmesg\n").unwrap();
        std::thread::sleep(Duration::from_millis(500));
        let response = read_until_timeout(&mut stream, 3000);
        let text = String::from_utf8_lossy(&response);
        assert!(
            text.contains("Linux version"),
            "dmesg should show kernel boot log, got: {text:?}"
        );
        assert!(
            text.contains("sun55iw3"),
            "Boot log should contain machine model, got: {text:?}"
        );
    }

    #[test]
    fn test_backspace_handling() {
        let sim = start_sim(true);
        let mut stream = TcpStream::connect(format!("127.0.0.1:{}", sim.port)).unwrap();
        read_until_timeout(&mut stream, 2000);

        // Type "echox" then backspace then "o hello\n"
        stream.write_all(b"echox").unwrap();
        std::thread::sleep(Duration::from_millis(50));
        stream.write_all(&[0x7f]).unwrap(); // backspace
        std::thread::sleep(Duration::from_millis(50));
        stream.write_all(b"o hello\n").unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let response = read_until_timeout(&mut stream, 2000);
        let text = String::from_utf8_lossy(&response);
        assert!(
            text.contains("hello"),
            "echo hello should succeed after backspace correction, got: {text:?}"
        );
    }

    #[test]
    fn test_xshell_style_inline_backspace_redraw() {
        let sim = start_sim(true);
        let mut stream = TcpStream::connect(format!("127.0.0.1:{}", sim.port)).unwrap();
        read_until_timeout(&mut stream, 2000);

        stream.write_all(b"ping baidu.com").unwrap();
        std::thread::sleep(Duration::from_millis(50));
        for _ in 0.." baidu.com".len() {
            stream.write_all(b"\x1b[D").unwrap();
        }
        std::thread::sleep(Duration::from_millis(50));
        stream.write_all(&[0x7f, 0x7f]).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        stream.write_all(b"\n").unwrap();
        std::thread::sleep(Duration::from_millis(300));

        let response = read_until_timeout(&mut stream, 2000);
        let text = String::from_utf8_lossy(&response);
        assert!(
            response.windows(3).any(|w| w == b"\x1b[D"),
            "left-arrow redraw should be echoed like a direct terminal, got: {text:?}"
        );
        assert!(
            response.windows(3).any(|w| w == b"\x1b[P"),
            "in-line backspace should emit delete-character redraw, got: {text:?}"
        );
        assert!(
            text.contains("/bin/sh: pi: not found"),
            "backspacing 'ng' inside ping should execute 'pi baidu.com', got: {text:?}"
        );
    }

    #[test]
    fn test_boot_log_mode() {
        // Start with --boot-log to simulate a board booting
        let bin = serial_sim_bin();
        let mut child = Command::new(&bin)
            .args([
                "--port",
                "0",
                "--hostname",
                "dut1",
                "--no-login",
                "--boot-log",
                "--print-port",
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .expect("Failed to start serial-sim with --boot-log");

        // Read dynamically-assigned port
        let port: u16 = {
            let stdout = child.stdout.as_mut().unwrap();
            let mut buf = [0u8; 16];
            let mut total = Vec::new();
            let start = std::time::Instant::now();
            while start.elapsed() < Duration::from_secs(3) {
                match stdout.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        total.extend_from_slice(&buf[..n]);
                        if total.contains(&b'\n') {
                            break;
                        }
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    Err(_) => break,
                }
            }
            String::from_utf8_lossy(&total).trim().parse().unwrap()
        };
        std::thread::sleep(Duration::from_millis(500));

        let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
        let data = read_until_timeout(&mut stream, 5000);
        let text = String::from_utf8_lossy(&data);

        // Should contain boot log AND a prompt at the end
        assert!(
            text.contains("Booting Linux"),
            "Should contain boot log, got: {text:?}"
        );
        assert!(
            text.ends_with("root@dut1:/# "),
            "Boot log should end with shell prompt, got: {text:?}"
        );

        let _ = child.kill();
        let _ = child.wait();
    }
}

// ── init (JSONC wizard) ───────────────────────────────────────────────

/// Run `dutabo init` with piped stdin answers and a scrubbed environment
/// (no TARGET_CONF / DUTABO_NON_INTERACTIVE leakage from the test process).
/// Piped stdin makes the binary take its plain-line prompt fallback (the
/// wizard core is TTY-free by design).
fn run_init(dir: &std::path::Path, args: &[&str], stdin: &str) -> std::process::Output {
    use std::io::Write as _;
    let mut child = Command::new(dutabo_bin())
        .arg("init")
        .args(args)
        .current_dir(dir)
        .env_remove("TARGET_CONF")
        .env_remove("DUTABO_NON_INTERACTIVE")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    drop(child.stdin.take());
    child.wait_with_output().unwrap()
}

/// Quick-flow answers: 1 host (10.0.0.1, user test), 1 DUT (dut1,
/// serial port 41002, root, ch340).
/// One bare Enter per DUT-owned advanced question (the walk asks the 17
/// per-DUT keys in every mode — the natural hints are informational).
const INIT_QUICK_STDIN: &str = concat!(
    "1\n10.0.0.1\ntest\n\n\n1\n\n41002\n\n\n",
    "\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n"
);

#[test]
fn test_init_creates_config_and_mcp_json() {
    let tmp = tempfile::TempDir::new().unwrap();
    let out = run_init(tmp.path(), &["--quick"], INIT_QUICK_STDIN);
    assert!(
        out.status.success(),
        "status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let config = tmp.path().join(".target.jsonc");
    assert!(config.exists(), ".target.jsonc must be created");
    let text = std::fs::read_to_string(&config).unwrap();
    assert!(text.contains("\"dut_name\": \"dut1\""), "{text}");
    assert!(text.contains("41002"), "{text}");
    // Canonical header/comment present.
    assert!(text.starts_with("// .target.jsonc"), "{text}");
    // .mcp.json generated pointing TARGET_CONF at the jsonc.
    let mcp = tmp.path().join(".mcp.json");
    assert!(mcp.exists(), ".mcp.json must be created");
    let mcp_text = std::fs::read_to_string(&mcp).unwrap();
    assert!(mcp_text.contains("TARGET_CONF"), "{mcp_text}");
    assert!(mcp_text.contains(".target.jsonc"), "{mcp_text}");
    // No tmp residue.
    assert!(!tmp.path().join(".target.jsonc.tmp").exists());
}

#[test]
fn test_init_count_enter_accepts_default_via_pipe() {
    // P0: bare newline (Enter) on BOTH count questions accepts the shown
    // default (1) and proceeds IMMEDIATELY — no silent re-ask of the same
    // screen, no misaligned answers (the old core loop consumed the next
    // piped line as a count and died with 'aborted (EOF on stdin)').
    let tmp = tempfile::TempDir::new().unwrap();
    let out = run_init(
        tmp.path(),
        &["--quick"],
        concat!(
            "\n\n10.0.0.1\ntest\n\n\n\n\n41002\n\n\n",
            "\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n"
        ),
    );
    assert!(
        out.status.success(),
        "status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let config = tmp.path().join(".target.jsonc");
    assert!(config.exists(), ".target.jsonc must be created");
    let text = std::fs::read_to_string(&config).unwrap();
    assert!(text.contains("\"dut_name\": \"dut1\""), "{text}");
    assert!(text.contains("41002"), "{text}");
    assert!(!tmp.path().join(".target.jsonc.tmp").exists());
}

#[test]
fn test_init_update_preserves_comments() {
    let tmp = tempfile::TempDir::new().unwrap();
    let first = run_init(tmp.path(), &["--quick"], INIT_QUICK_STDIN);
    assert!(first.status.success());
    let before = std::fs::read_to_string(tmp.path().join(".target.jsonc")).unwrap();

    // Second run changes the port to 41003 (everything else unchanged).
    let second = run_init(
        tmp.path(),
        &["--quick"],
        concat!(
            "1\n10.0.0.1\ntest\n\n\n1\n\n41003\n\n\n",
            "\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n"
        ),
    );
    assert!(
        second.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&second.stderr)
    );
    let after = std::fs::read_to_string(tmp.path().join(".target.jsonc")).unwrap();
    assert!(after.contains("\"port\": 41003"), "{after}");
    // Comment preservation: every COMMENT TEXT survives byte-identically
    // (the port line's VALUE changed, its comment must stay).
    for line in before.lines() {
        if let Some(comment) = line.split("//").nth(1)
            && !comment.trim().is_empty()
        {
            assert!(
                after.contains(comment.trim()),
                "comment lost: {comment}\n--- after:\n{after}"
            );
        }
    }
    // Diff summary printed.
    let stdout = String::from_utf8_lossy(&second.stdout);
    assert!(stdout.contains("serial.port: 41002 -> 41003"), "{stdout}");
    // .mcp.json untouched.
    let mcp = std::fs::read_to_string(tmp.path().join(".mcp.json")).unwrap();
    assert!(mcp.contains("TARGET_CONF"));
}

#[test]
fn test_init_non_interactive_empty_dir_fails_loudly() {
    let tmp = tempfile::TempDir::new().unwrap();
    let out = run_init(tmp.path(), &["--non-interactive"], "");
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("dev_hosts[0].ip"), "{stderr}");
    assert!(
        stderr.contains("dev_hosts[0].duts[0].serial.port"),
        "{stderr}"
    );
    assert!(!tmp.path().join(".target.jsonc").exists());
}

#[test]
fn test_init_unknown_flag_is_loud() {
    let tmp = tempfile::TempDir::new().unwrap();
    let out = run_init(tmp.path(), &["--bogus"], "");
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--bogus"), "{stderr}");
}

#[test]
fn test_init_usage_lists_init_flags() {
    let out = Command::new(dutabo_bin()).output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    for flag in ["--non-interactive", "--quick", "--advanced", "--force"] {
        assert!(stderr.contains(flag), "usage missing '{flag}'");
    }
}

#[test]
fn init_esc_restores_shell_arrow_key_input() {
    let tmp = tempfile::TempDir::new().unwrap();
    let script = r#"
import fcntl, os, pty, select, shlex, struct, sys, termios, time

binary, cwd = sys.argv[1], sys.argv[2]
pid, fd = pty.fork()
if pid == 0:
    os.environ['PS1'] = 'DUTABO_PROMPT> '
    os.execv('/bin/bash', ['bash', '--noprofile', '--norc', '-i'])

fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack('HHHH', 30, 100, 0, 0))

def read_until(needle, timeout):
    data = b''
    end = time.time() + timeout
    while needle not in data and time.time() < end:
        ready, _, _ = select.select([fd], [], [], 0.1)
        if ready:
            try:
                data += os.read(fd, 65536)
            except OSError:
                break
    return data

prompt = b'DUTABO_PROMPT> '
if prompt not in read_until(prompt, 5):
    os.kill(pid, 9); sys.exit(10)
cmd = 'cd -- {} && TERM=xterm-256color {} init --tui\r'.format(
    shlex.quote(cwd), shlex.quote(binary))
os.write(fd, cmd.encode())
time.sleep(0.5)
os.write(fd, b'\x1b')
if prompt not in read_until(prompt, 10):
    os.kill(pid, 9); sys.exit(11)

os.write(fd, b'echo DUTABO_ARROW_SENTINEL\r')
if prompt not in read_until(prompt, 5):
    os.kill(pid, 9); sys.exit(12)
os.write(fd, b'\x1b[A\r')
after = read_until(prompt, 5)
os.write(fd, b'exit\r')
os.waitpid(pid, 0)
if b'[A' in after or after.count(b'DUTABO_ARROW_SENTINEL') < 2:
    os.write(2, after)
    sys.exit(13)
"#;
    let output = Command::new("python3")
        .arg("-c")
        .arg(script)
        .arg(dutabo_bin())
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "PTY regression failed (status {:?}): {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
}
