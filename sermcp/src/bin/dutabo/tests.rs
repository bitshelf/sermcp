use super::*;
use sermcp::highlight::highlight_serial_prompt;

/// Telnet IAC negotiations (ser2net rfc2217 open) must be dropped —
/// they rendered as "ÿ…" garbage (field-observed). Escaped
/// literal 0xFF data (IAC IAC) still passes.
#[test]
fn sanitizer_drops_telnet_iac_negotiations() {
    let mut san = TerminalSanitizer::new(false);
    let mut out = Vec::new();
    // Real capture: IAC WILL SUPPRESS-GO-AHEAD / DO / NAWS preamble.
    san.filter(b"\xff\xfb\x03\xff\xfd\x03hello", &mut out);
    assert_eq!(out, b"hello", "IAC bytes dropped, data kept: {out:?}");
    // Subnegotiation (SB … SE) dropped too.
    out.clear();
    san.filter(b"\xff\xfa\x2c\x01\x00\xff\xf0ok", &mut out);
    assert_eq!(out, b"ok", "subnegotiation dropped: {out:?}");
    // Escaped literal 0xFF passes as data.
    out.clear();
    san.filter(b"\xff\xff!", &mut out);
    assert_eq!(out, b"\xff!", "IAC IAC escapes to a literal data byte");
}

/// The ser2net banner line ("ser2net port … device …") is suppressed
/// — even when split across filter() chunks; normal lines pass.
#[test]
fn sanitizer_suppresses_ser2net_banner_lines() {
    let mut san = TerminalSanitizer::new(false);
    let mut out = Vec::new();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let serial_port = listener.local_addr().unwrap().port();
    drop(listener);
    let banner = format!(
        "\r\nser2net port telnet(rfc2217),tcp,{serial_port} device serialdev, /dev/ttyACM0"
    );
    san.filter(banner.as_bytes(), &mut out);
    let baud = "configured-baud";
    san.filter(
        format!(", {baud}n81,local (Debian GNU/Linux)\r\n\r\n").as_bytes(),
        &mut out,
    );
    san.filter(b"root@rk3576:~# \r\n", &mut out);
    let text = String::from_utf8_lossy(&out).to_string();
    assert!(!text.contains("ser2net"), "banner dropped: {text:?}");
    assert!(text.contains("root@rk3576"), "payload kept: {text:?}");
    // Leading blank lines (the banner's surrounding CRLFs) are dropped
    // — the first emitted content IS the prompt line.
    assert!(
        text.starts_with("root@rk3576"),
        "the FIRST emitted bytes are the prompt — no leading blank lines: {text:?}"
    );
}

// The lone-LF display normalization this test used to cover moved to the
// library (`serial_term::newline::tests`) together with the interactive
// serial rewrite.

/// Serializes the tests that touch the process-global MCP_SESSION cache:
/// the cache is keyed by port, so a test that sets/clears it races a test
/// that asserts on it when both run on parallel tokio workers.
/// tokio::sync::Mutex (not std) because the guard must survive awaits in
/// multi-threaded tests (std::sync::MutexGuard is !Send). pub(crate):
/// the flash-probe tests in tui_init/flash.rs take it too.
pub(crate) static SESSION_TEST_MUTEX: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

// ── `dutabo list` table helpers (TDD-14) ────────────────────────────

/// Serialize COLUMNS manipulation — env is process-global and test
/// threads share the process.
static COLUMNS_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn test_visible_width_strips_ansi() {
    assert_eq!(visible_width("abc"), 3);
    assert_eq!(visible_width("\x1b[31m✗ crashed\x1b[0m"), 9);
    assert_eq!(visible_width(""), 0);
}

/// --plain/--tui override the auto TUI dispatch gate; --tui demands a
/// terminal on BOTH stdin and stdout (a piped stdin can never feed the
/// event loop); --plain never errors.
#[test]
fn test_transport_for_flags() {
    let opts = |tui: bool, plain: bool| sermcp::init::InitOptions {
        tui,
        plain,
        ..Default::default()
    };
    // Auto: terminals + xterm + usable size -> TUI.
    assert_eq!(
        transport_for(&opts(false, false), true, true, "xterm", (80, 24)).unwrap(),
        Transport::Tui
    );
    // Auto: piped stdin -> plain, even with a tty stdout.
    assert_eq!(
        transport_for(&opts(false, false), false, true, "xterm", (80, 24)).unwrap(),
        Transport::Plain
    );
    // --plain forces plain on a real terminal.
    assert_eq!(
        transport_for(&opts(false, true), true, true, "xterm", (80, 24)).unwrap(),
        Transport::Plain
    );
    // --tui forces TUI past the TERM=dumb / small-size gates.
    assert_eq!(
        transport_for(&opts(true, false), true, true, "dumb", (80, 24)).unwrap(),
        Transport::Tui
    );
    assert_eq!(
        transport_for(&opts(true, false), true, true, "xterm", (20, 5)).unwrap(),
        Transport::Tui
    );
    // --tui without terminals is a loud error (never a mid-wizard hang).
    let err = transport_for(&opts(true, false), false, true, "xterm", (80, 24)).unwrap_err();
    assert!(err.contains("--tui"), "{err}");
    let err = transport_for(&opts(true, false), true, false, "xterm", (80, 24)).unwrap_err();
    assert!(err.contains("--tui"), "{err}");
    // --plain + --tui can never reach this fn (from_args rejects it).
}

#[test]
fn test_user_at_ip_variants() {
    let mut dut = DutConfig {
        dut_name: "d".into(),
        dev_host_ip: "10.0.0.1".into(),
        dev_host_user: "test".into(),
        dev_host_pass: String::new(),
        serial_port: String::new(),
        relay_ip: String::new(),
        relay_type: String::new(),
        dev_ctl: String::new(),
        relay_port: 0,
        reset_ch: 0,
        download_ch: 0,
        power_ch: 0,
        power_off_time_ms: 3000,
        reference_log: String::new(),
        dut_dir: String::new(),
        login_user: String::new(),
        login_pass: String::new(),
        learner_stage_threshold: 0.0,
        learner_crash_threshold: 0.0,
        crash_patterns: Vec::new(),
        section_values: std::collections::HashMap::new(),
    };
    assert_eq!(user_at_ip(&dut), "test@10.0.0.1");
    dut.dev_host_user.clear();
    assert_eq!(user_at_ip(&dut), "10.0.0.1");
    dut.dev_host_ip.clear();
    assert_eq!(user_at_ip(&dut), "-");
}

#[test]
fn test_column_widths_no_overflow_uses_natural_maxes() {
    let rows = [
        [
            "NAME".into(),
            "HOST".into(),
            "USER@IP".into(),
            "PORT".into(),
            "STATE".into(),
        ],
        [
            "board-a".into(),
            "-".into(),
            "user@host.dev".into(),
            "PORT".into(),
            "\x1b[31m✗ crashed\x1b[0m".into(),
        ],
    ];
    let widths = column_widths(&rows, 120);
    // Natural maxes win at a wide terminal; HOST and PORT bind at their
    // header widths.
    assert_eq!(widths, [7, 4, 13, 4, 9]);
}

#[test]
fn test_column_widths_truncation_order_and_mins() {
    // Natural width 93 (22+11+39+4+9 + 4 gaps x2). At 56 cols the
    // overflow (37) is absorbed HOST first (→ its min 4, 7 cut), then
    // USER@IP: USER@IP shrinks to 9, NAME and STATE stay untouched.
    let rows = [
        [
            "NAME".into(),
            "HOST".into(),
            "USER@IP".into(),
            "PORT".into(),
            "STATE".into(),
        ],
        [
            "a-very-long-alias-name".into(),
            "lab-pc-host".into(),
            "verylonguser@very.long.hostname.example".into(),
            "PORT".into(),
            "\x1b[31m✗ crashed\x1b[0m".into(),
        ],
    ];
    let natural = column_maxes(&rows);
    assert_eq!(natural[0], 22, "a-very-long-alias-name is 22 chars");
    assert_eq!(natural[1], 11, "lab-pc-host is 11 chars");
    assert_eq!(natural[2], 39, "the user@ip cell is 39 chars");
    let widths = column_widths(&rows, 56);
    let total = widths.iter().sum::<usize>() + COLUMN_GAP * 4;
    assert!(total <= 56, "fits the terminal: {widths:?}");
    assert_eq!(widths[0], 22, "name untouched by host cuts");
    assert_eq!(widths[1], 4, "host absorbs first, down to its min");
    assert_eq!(widths[2], 9, "user@ip absorbs the rest of the overflow");
    assert_eq!(widths[3], 4, "port untouched");
    assert_eq!(widths[4], 9, "state never truncated");
}

#[test]
fn test_column_widths_name_min_12_and_state_glyph_survives() {
    let rows = [
        [
            "NAME".into(),
            "HOST".into(),
            "USER@IP".into(),
            "PORT".into(),
            "STATE".into(),
        ],
        [
            "abcdefghijklmnopqrstuvwxyz".into(),
            "a-very-long-hostname".into(),
            "verylonguser@verylonghost".into(),
            "PORT".into(),
            "\x1b[31m✗ crashed\x1b[0m".into(),
        ],
    ];
    // Extreme narrowness: every column is at its floor; STATE still keeps
    // the "✗" glyph (2 chars) and NAME never drops below 12.
    let widths = column_widths(&rows, 34);
    assert_eq!(widths[0], 12, "name min 12");
    assert_eq!(widths[1], 4);
    assert_eq!(widths[2], 4);
    assert_eq!(widths[3], 4);
    assert_eq!(widths[4], 2, "state keeps the ✗ glyph");
}

#[test]
fn test_truncate_cell_keeps_crash_glyph() {
    assert_eq!(truncate_cell("board-a", 7), "board-a");
    assert_eq!(truncate_cell("a-very-long-host-alias", 4), "a...");
    // The "✗" glyph is the first char — truncation must not drop it.
    assert_eq!(truncate_cell("✗ crashed", 4), "✗...");
    // Below 4 chars there is no room for "...": keep the raw prefix,
    // which still starts with the glyph.
    assert_eq!(truncate_cell("✗ crashed", 3), "✗ c");
    assert_eq!(truncate_cell("✗ crashed", 2), "✗ ");
    assert_eq!(truncate_cell("\x1b[31m✗ crashed\x1b[0m", 4), "✗...");
}

#[test]
fn test_fit_cell_pads_and_truncates() {
    assert_eq!(fit_cell("ab", 4), "ab  ");
    assert_eq!(fit_cell("abcdef", 4), "a...");
    // Colored cell that fits keeps its ANSI and pads on the visible width.
    let colored = fit_cell("\x1b[31m✗ crashed\x1b[0m", 11);
    assert!(
        colored.contains('\x1b'),
        "color survives when the cell fits"
    );
    assert_eq!(visible_width(&colored), 11);
}

#[test]
fn test_format_row_aligned_columns() {
    let row = [
        "board-a".into(),
        "-".into(),
        "user@host.dev".into(),
        "PORT".into(),
        "crashed".into(),
    ];
    let dash = format!("-{}", " ".repeat(3));
    let crashed = format!("crashed{}", " ".repeat(2));
    let expected = format!(
        "{}  {}  {}  {}  {}",
        "board-a", dash, "user@host.dev", "PORT", crashed
    );
    assert_eq!(format_row(&row, &[7, 4, 13, 4, 9]), expected);
    // Same widths for a second row → every column starts at the same
    // position (the alignment contract of the table).
    let row2 = [
        "b".into(),
        "-".into(),
        "u@i".into(),
        "2".into(),
        "active".into(),
    ];
    let b = format!("b{}", " ".repeat(6));
    let u = format!("u@i{}", " ".repeat(10));
    let two = format!("2{}", " ".repeat(3));
    let active = format!("active{}", " ".repeat(3));
    let expected2 = format!("{}  {}  {}  {}  {}", b, dash, u, two, active);
    assert_eq!(format_row(&row2, &[7, 4, 13, 4, 9]), expected2);
    // The aligned starts: columns 1..3 begin at the same offset in both rows.
    let starts = |line: &str, name: &str| line.find(name).unwrap();
    assert_eq!(starts(&expected, "user@"), starts(&expected2, "u@i"));
    assert_eq!(starts(&expected, "PORT"), starts(&expected2, "2"));
    assert_eq!(starts(&expected, "crashed"), starts(&expected2, "active"));
}

#[test]
fn test_terminal_width_env() {
    let _guard = COLUMNS_MUTEX.lock().unwrap();
    // env mutation is process-global; the mutex serializes against other
    // COLUMNS-touching tests (edition 2024: set_var/remove_var are unsafe).
    unsafe {
        std::env::set_var("COLUMNS", "abc");
        assert_eq!(terminal_width(), 120);
        std::env::set_var("COLUMNS", "0");
        assert_eq!(terminal_width(), 120);
        std::env::set_var("COLUMNS", "40");
        assert_eq!(terminal_width(), 40);
        std::env::remove_var("COLUMNS");
    }
    assert_eq!(terminal_width(), 120);
}

#[test]
fn test_build_rows_reads_state_files() {
    let tmp = tempfile::TempDir::new().unwrap();
    let state_dir = tmp.path().join(".dut-serial/board-a");
    std::fs::create_dir_all(&state_dir).unwrap();
    std::fs::write(state_dir.join("target-state"), "crashed").unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let serial_port = listener.local_addr().unwrap().port().to_string();
    drop(listener);
    let duts = vec![DutConfig {
        dut_name: "board-a".into(),
        dev_host_ip: "host.invalid".into(),
        dev_host_user: "test".into(),
        dev_host_pass: String::new(),
        serial_port,
        relay_ip: String::new(),
        relay_type: String::new(),
        dev_ctl: String::new(),
        relay_port: 0,
        reset_ch: 0,
        download_ch: 0,
        power_ch: 0,
        power_off_time_ms: 3000,
        reference_log: String::new(),
        dut_dir: ".dut-serial/board-a".into(),
        login_user: String::new(),
        login_pass: String::new(),
        learner_stage_threshold: 0.0,
        learner_crash_threshold: 0.0,
        crash_patterns: Vec::new(),
        section_values: std::collections::HashMap::new(),
    }];
    // Non-TTY (test harness): no ANSI, "✗ crashed" plainly visible.
    let rows = build_rows(&duts, &[], tmp.path(), false);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], "NAME");
    assert_eq!(rows[1][0], "board-a");
    assert_eq!(rows[1][1], "-", "no host match → HOST renders '-'");
    assert_eq!(rows[1][4], "✗ crashed");
    assert!(!rows[1][4].contains('\x1b'), "piped output has no ANSI");
    // TTY mode colors the critical state red.
    let rows = build_rows(&duts, &[], tmp.path(), true);
    assert!(rows[1][4].contains("\x1b[31m"), "critical state is red");
}

#[test]
fn tool_payload_parses_success_and_flags_success_false() {
    let ok = serde_json::json!({
        "content": [{"type": "text", "text": "{\"success\":true,\"boot_complete\":true}"}]
    });
    assert_eq!(
        tool_payload(&ok).unwrap()["boot_complete"],
        serde_json::json!(true)
    );

    let failed = serde_json::json!({
        "content": [{"type": "text", "text": "{\"success\":false,\"error\":\"relay offline\"}"}]
    });
    assert_eq!(tool_payload(&failed).unwrap_err(), "relay offline");
}

#[test]
fn tool_payload_missing_success_field_counts_as_success() {
    // serial_get_state and serial_send_command carry no `success` field.
    let result = serde_json::json!({
        "content": [{"type": "text", "text": "{\"state\":\"idle\",\"boot_number\":7}"}]
    });
    let v = tool_payload(&result).unwrap();
    assert_eq!(v["state"], serde_json::json!("idle"));
    assert_eq!(v["boot_number"], serde_json::json!(7));
}

#[test]
fn tool_payload_failure_without_error_field_reports_payload() {
    let result = serde_json::json!({
        "content": [{"type": "text", "text": "{\"success\":false}"}]
    });
    assert_eq!(tool_payload(&result).unwrap_err(), "{\"success\":false}");
}

#[test]
fn tool_payload_rejects_unparseable_result() {
    let result = serde_json::json!({
        "content": [{"type": "text", "text": "not json"}]
    });
    assert!(
        tool_payload(&result)
            .unwrap_err()
            .contains("unparseable tool result")
    );
}

#[test]
fn project_port_is_stable_and_in_reserved_range() {
    let path = std::path::Path::new("/tmp/dutabo-project-port-test");
    let port = sermcp::ports::project_mcp_port(path);
    assert_eq!(port, sermcp::ports::project_mcp_port(path));
    assert!(
        (sermcp::ports::MC_PORT_BASE..sermcp::ports::MC_PORT_BASE + sermcp::ports::MC_PORT_RANGE)
            .contains(&port)
    );
}

#[test]
fn dut_inventory_ports_match_the_project_port() {
    let project = std::path::Path::new("/path/to/sermcp");
    let project_port = sermcp::ports::project_mcp_port(project);
    assert_eq!(
        sermcp::ports::project_dut_mcp_port(project, "rk3576"),
        project_port
    );
    assert_eq!(
        sermcp::ports::project_dut_mcp_port(project, "rk3576-2"),
        project_port
    );
}

#[test]
fn all_duts_share_one_stable_project_port() {
    let project = std::path::Path::new("/tmp/dutabo-per-dut-port-test");
    let a = sermcp::ports::project_dut_mcp_port(project, "dut-a");
    let b = sermcp::ports::project_dut_mcp_port(project, "dut-b");
    let range =
        sermcp::ports::MC_PORT_BASE..sermcp::ports::MC_PORT_BASE + sermcp::ports::MC_PORT_RANGE;
    assert!(range.contains(&a), "a={a} outside reserved range");
    assert!(range.contains(&b), "b={b} outside reserved range");
    assert_eq!(a, b, "DUT names must not create another MCP endpoint");
    // Deterministic across calls.
    assert_eq!(a, sermcp::ports::project_dut_mcp_port(project, "dut-a"));
    assert_eq!(b, sermcp::ports::project_dut_mcp_port(project, "dut-b"));
}

/// Minimal DUT config for probe/message tests.
fn test_dut(name: &str, serial_host: &str, serial_port: &str) -> sermcp::config::DutConfig {
    sermcp::config::DutConfig {
        dut_name: name.to_string(),
        dev_host_ip: serial_host.to_string(),
        dev_host_user: "root".into(),
        dev_host_pass: String::new(),
        serial_port: serial_port.to_string(),
        relay_ip: String::new(),
        relay_type: String::new(),
        dev_ctl: String::new(),
        relay_port: 0,
        reset_ch: 0,
        download_ch: 0,
        power_ch: 0,
        power_off_time_ms: 0,
        reference_log: String::new(),
        dut_dir: name.to_string(),
        login_user: "root".into(),
        login_pass: String::new(),
        learner_stage_threshold: 0.45,
        learner_crash_threshold: 0.5,
        crash_patterns: Vec::new(),
        section_values: std::collections::HashMap::new(),
    }
}

fn health_json(host: &str, port: &str, owned: bool) -> String {
    format!(
        r#"{{"status":"ok","serial":{{"host":"{host}","port":"{port}","owned":{owned}}},"server":{{"pid":1,"version":"test"}}}}"#
    )
}

/// Serve a canned HTTP response on a dynamically bound port.
/// Returns (join handle, port).
async fn spawn_health_server(body: String) -> (tokio::task::JoinHandle<()>, u16) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(response.as_bytes()).await;
        }
    });
    (handle, port)
}

async fn unused_port() -> u16 {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

/// Regression: ser2net accepts more than one TCP client, so a successful
/// connect must never bypass a live endpoint lock. A non-MCP holder is
/// not preemptible and no TCP connection is attempted.
#[tokio::test]
async fn direct_serial_respects_lock_before_tcp_connect() {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let temp = tempfile::TempDir::new().unwrap();
    let lock_dir = temp.path().to_string_lossy().into_owned();
    let target = port.to_string();
    // A REAL foreign holder: util-linux `flock(1)` takes the same kernel
    // lock `File::try_lock` uses. (Same-process re-acquire is idempotent
    // under flock, so in-process self-blocking can no longer be simulated.)
    let lock_path = sermcp::lock_manager::endpoint_lock_path("127.0.0.1", &target, &lock_dir);
    let mut holder = std::process::Command::new("flock")
        .arg(&lock_path)
        .arg("-c")
        .arg("sleep 300")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn flock(1) holder");
    // flock(1) holds the lock once the child command starts.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let mut yielded = None;
    assert!(
        matches!(
            connect_serial_direct("127.0.0.1", port, &lock_dir, &mut yielded).await,
            DirectTakeover::ForeignOwner(_)
        ),
        "a live non-MCP lock holder cannot be bypassed"
    );
    assert!(yielded.is_none());
    let _ = holder.kill();
    let _ = holder.wait();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), listener.accept())
            .await
            .is_err(),
        "no TCP connection may precede endpoint-lock ownership"
    );
    sermcp::lock_manager::release_lock("127.0.0.1", &target, &lock_dir);
}

// multi_thread: fetch_health shells out to blocking `curl`, which would
// deadlock the current-thread runtime against the spawned test server.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_prefers_per_dut_port_when_alive() {
    let serial_host = "127.0.0.1";
    let serial_port = unused_port().await.to_string();
    let dut = test_dut("probe-preferred", serial_host, &serial_port);
    let (s1, per_dut_port) =
        spawn_health_server(health_json(serial_host, &serial_port, true)).await;
    let (s2, project_port) =
        spawn_health_server(health_json(serial_host, &serial_port, true)).await;
    assert_eq!(
        probe_serial_server(&dut, &[per_dut_port, project_port]).await,
        Some(per_dut_port)
    );
    s1.abort();
    s2.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_falls_back_to_project_port_when_per_dut_down() {
    let serial_host = "127.0.0.1";
    let serial_port = unused_port().await.to_string();
    let dut = test_dut("probe-fallback", serial_host, &serial_port);
    let dead_port = unused_port().await;
    let (s2, project_port) =
        spawn_health_server(health_json(serial_host, &serial_port, true)).await;
    assert_eq!(
        probe_serial_server(&dut, &[dead_port, project_port]).await,
        Some(project_port)
    );
    s2.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_falls_back_on_owner_mismatch() {
    let serial_host = "127.0.0.1";
    let serial_port = unused_port().await.to_string();
    let mut other_serial_port = unused_port().await.to_string();
    while other_serial_port == serial_port {
        other_serial_port = unused_port().await.to_string();
    }
    let dut = test_dut("probe-owner", serial_host, &serial_port);
    // Per-DUT port is up but owns a different serial endpoint.
    let (s1, wrong_owner_port) =
        spawn_health_server(health_json(serial_host, &other_serial_port, true)).await;
    let (s2, project_port) =
        spawn_health_server(health_json(serial_host, &serial_port, true)).await;
    assert_eq!(
        probe_serial_server(&dut, &[wrong_owner_port, project_port]).await,
        Some(project_port)
    );
    // The detailed variant must record the actual owner for the message.
    let (winner, mismatch) =
        probe_serial_server_detailed(&dut, &[wrong_owner_port, project_port]).await;
    assert_eq!(winner, Some(project_port));
    let (mismatch_port, other_host, other_port) = mismatch.expect("mismatch recorded");
    assert_eq!(mismatch_port, wrong_owner_port);
    assert_eq!(other_host, serial_host);
    assert_eq!(other_port, other_serial_port);
    s1.abort();
    s2.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_returns_none_when_all_down() {
    let serial_port = unused_port().await.to_string();
    let dut = test_dut("probe-down", "127.0.0.1", &serial_port);
    let p1 = unused_port().await;
    let p2 = unused_port().await;
    assert_eq!(probe_serial_server(&dut, &[p1, p2]).await, None);
}

#[test]
fn code_agent_sessions_are_sorted_and_owner_is_current() {
    let tmp = tempfile::TempDir::new().unwrap();
    let state_dir = tmp.path().join(".dut-serial");
    let sessions_dir = state_dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir).unwrap();
    std::fs::write(
        sessions_dir.join("guest-session"),
        "2026-09-04T05:00:00+00:00\nowner\n",
    )
    .unwrap();
    std::fs::write(
        sessions_dir.join("owner-session"),
        "2026-09-04T05:01:00+00:00\nguest\n",
    )
    .unwrap();
    std::fs::write(sessions_dir.join(".lock"), "").unwrap();
    std::fs::write(
        state_dir.join("owner.json"),
        r#"{"schema_version":1,"owner_session_id":"owner-session"}"#,
    )
    .unwrap();

    let sessions = code_agent_sessions(tmp.path());
    assert_eq!(sessions.len(), 2);
    assert_eq!(sessions[0].session_id, "owner-session");
    assert_eq!(sessions[0].role, "owner");
    assert_eq!(sessions[0].started, "2026-09-04 05:01:00Z");
    assert_eq!(sessions[1].session_id, "guest-session");
    assert_eq!(sessions[1].role, "guest");
}

#[test]
fn code_agent_sessions_are_empty_without_registry() {
    let tmp = tempfile::TempDir::new().unwrap();
    assert!(code_agent_sessions(tmp.path()).is_empty());
}

#[test]
fn missing_server_message_is_actionable() {
    let endpoint = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let serial_port = endpoint.local_addr().unwrap().port().to_string();
    let mcp_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let first_mcp_port = mcp_listener.local_addr().unwrap().port();
    let second_mcp_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let second_mcp_port = second_mcp_listener.local_addr().unwrap().port();
    let other_endpoint = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let other_serial_port = other_endpoint.local_addr().unwrap().port().to_string();
    drop((endpoint, mcp_listener, second_mcp_listener, other_endpoint));
    let dut = test_dut("message-dut", "127.0.0.1", &serial_port);
    let candidates = [first_mcp_port, second_mcp_port];
    let msg = missing_server_message(&dut, &candidates, None);
    assert!(msg.contains("message-dut"), "{msg}");
    assert!(msg.contains(&first_mcp_port.to_string()), "{msg}");
    assert!(msg.contains("Agent sessions share"), "{msg}");
    assert!(!msg.contains("TARGET_DUT_NAME"), "{msg}");
    assert!(msg.contains("sermcp --http"), "{msg}");
    assert!(!msg.contains("Claude"), "{msg}");

    // Mismatch variant names the actual owner.
    let mismatch_msg = missing_server_message(
        &dut,
        &candidates,
        Some((
            first_mcp_port,
            "127.0.0.1".into(),
            other_serial_port.clone(),
        )),
    );
    assert!(
        mismatch_msg.contains(&format!("127.0.0.1:{other_serial_port}")),
        "{mismatch_msg}"
    );
    assert!(
        mismatch_msg.contains(&first_mcp_port.to_string()),
        "{mismatch_msg}"
    );
    assert!(
        mismatch_msg.contains("Agent sessions share"),
        "{mismatch_msg}"
    );
    assert!(!mismatch_msg.contains("TARGET_DUT_NAME"), "{mismatch_msg}");
    assert!(mismatch_msg.contains("sermcp --http"), "{mismatch_msg}");
    assert!(!mismatch_msg.contains("Claude"), "{mismatch_msg}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_call_requests_accepts_both_mime_types() {
    let _session_guard = SESSION_TEST_MUTEX.lock().await;
    // The fake answers every request with a proper initialize result and
    // a session header, so dutabo exercises all three requests:
    // initialize → notifications/initialized → tools/call.
    *MCP_SESSION.lock().unwrap() = None;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let cap = captured.clone();
    let server = tokio::spawn(async move {
        for _ in 0..3 {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            cap.lock()
                .unwrap()
                .push(String::from_utf8_lossy(&buf[..n]).to_string());
            let body = r#"data: {"jsonrpc":"2.0","id":0,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"fake","version":"0.0.0"}}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nMcp-Session-Id: s1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(response.as_bytes()).await;
        }
    });
    let _ = mcp_call(
        "tools/call",
        serde_json::json!({"name":"serial_get_state","arguments":{}}),
        &[port],
    )
    .await;
    server.abort();
    let requests = captured.lock().unwrap();
    assert!(!requests.is_empty(), "no requests captured");
    for request in requests.iter() {
        assert!(
            request
                .to_lowercase()
                .contains("accept: application/json, text/event-stream"),
            "missing Accept header in request: {request}"
        );
    }
}

/// What a fake MCP server answers to `tasks/get` after handing out a
/// `tools/call` task handle: `working` for N polls then `completed` with
/// a canned tool result, or always `failed` with a canned error object.
enum FakeTaskTerminal {
    CompletedAfter {
        working_polls: usize,
        tool_text: &'static str,
    },
    Failed(&'static str),
}

/// Fake Streamable-HTTP MCP server answering `initialize` (with an
/// `Mcp-Session-Id` header), `notifications/initialized`, `tools/call`
/// (always with a SEP-2663 task handle) and `tasks/get`. Captures every
/// raw request for assertions. Returns (server handle, port, captured
/// requests, tasks/get counter).
async fn spawn_fake_mcp_server(
    terminal: FakeTaskTerminal,
) -> (
    tokio::task::JoinHandle<()>,
    u16,
    std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let task_gets = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (cap_requests, cap_task_gets) = (requests.clone(), task_gets.clone());
    let handle = tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            use tokio::io::AsyncWriteExt;
            let Some(raw) = read_http_request(&mut sock).await else {
                break;
            };
            cap_requests.lock().unwrap().push(raw.clone());
            let body = raw.split("\r\n\r\n").nth(1).unwrap_or("");
            let Ok(json) = serde_json::from_str::<serde_json::Value>(body) else {
                break;
            };
            let now = chrono::Utc::now().to_rfc3339();
            let (status, extra_headers, response_body) = match json["method"].as_str() {
                    Some("initialize") => (
                        "200 OK",
                        "Mcp-Session-Id: fake-session\r\n",
                        r#"data: {"jsonrpc":"2.0","id":0,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"fake","version":"0.0.0"}}}"#
                            .to_string(),
                    ),
                    Some("notifications/initialized") => ("202 Accepted", "", String::new()),
                    Some("tools/call") => {
                        let task = serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": 1,
                            "result": {
                                "resultType": "task",
                                "taskId": "fake-task-1",
                                "status": "working",
                                "createdAt": now,
                                "lastUpdatedAt": now,
                                "ttlMs": serde_json::Value::Null,
                            },
                        });
                        (
                            "200 OK",
                            "",
                            format!("data: {}", serde_json::to_string(&task).unwrap()),
                        )
                    }
                    Some("tasks/get") => {
                        let poll = cap_task_gets
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                            + 1;
                        let mut result = serde_json::json!({
                            "resultType": "complete",
                            "taskId": "fake-task-1",
                            "status": "working",
                            "createdAt": now,
                            "lastUpdatedAt": now,
                            "ttlMs": serde_json::Value::Null,
                            "pollIntervalMs": 200,
                        });
                        match &terminal {
                            FakeTaskTerminal::CompletedAfter {
                                working_polls,
                                tool_text,
                            } => {
                                if poll > *working_polls {
                                    result["status"] = serde_json::json!("completed");
                                    result["result"] = serde_json::json!({
                                        "content": [{"type": "text", "text": tool_text}],
                                        "isError": false,
                                    });
                                }
                            }
                            FakeTaskTerminal::Failed(error) => {
                                result["status"] = serde_json::json!("failed");
                                result["error"] = serde_json::json!({
                                    "code": -32000,
                                    "message": error,
                                });
                            }
                        }
                        let frame = serde_json::json!({"jsonrpc": "2.0", "id": 2, "result": result});
                        (
                            "200 OK",
                            "",
                            format!("data: {}", serde_json::to_string(&frame).unwrap()),
                        )
                    }
                    _ => (
                        "200 OK",
                        "",
                        r#"data: {"jsonrpc":"2.0","id":1,"result":{}}"#.to_string(),
                    ),
                };
            let response = if response_body.is_empty() {
                format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            } else {
                format!(
                    "HTTP/1.1 {status}\r\nContent-Type: text/event-stream\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                    response_body.len()
                )
            };
            let _ = sock.write_all(response.as_bytes()).await;
        }
    });
    (handle, port, requests, task_gets)
}

/// Read one HTTP/1.1 request (headers + Content-Length body) from a
/// socket, returning the raw request text.
async fn read_http_request(sock: &mut tokio::net::TcpStream) -> Option<String> {
    use tokio::io::AsyncReadExt;
    let mut data = Vec::new();
    let mut buf = [0u8; 4096];
    let header_end = loop {
        let n = sock.read(&mut buf).await.ok()?;
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
        let n = sock.read(&mut buf).await.ok()?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
    }
    Some(String::from_utf8_lossy(&data).to_string())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initialize_declares_tasks_extension() {
    let _session_guard = SESSION_TEST_MUTEX.lock().await;
    // The MCP_SESSION cache is process-global and keyed by port; clear it
    // so this test always exercises the real initialize request.
    *MCP_SESSION.lock().unwrap() = None;
    let (server, port, requests, _) = spawn_fake_mcp_server(FakeTaskTerminal::CompletedAfter {
        working_polls: 0,
        tool_text: "{}",
    })
    .await;
    let _ = mcp_call(
        "tools/call",
        serde_json::json!({"name":"serial_get_state","arguments":{}}),
        &[port],
    )
    .await;
    server.abort();
    let requests = requests.lock().unwrap();
    let initialize = requests
        .iter()
        .find(|r| r.contains("\"method\":\"initialize\""))
        .expect("initialize request captured");
    // The server only routes long tools through SEP-2663 tasks when the
    // client declares the extension at initialize time; without it a
    // >30s reset is answered synchronously and dutabo abandons the
    // request with a misleading "server not found".
    assert!(
        initialize.contains("io.modelcontextprotocol/tasks"),
        "initialize must declare the tasks extension, got: {initialize}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_call_polls_task_and_returns_completed_result() {
    let _session_guard = SESSION_TEST_MUTEX.lock().await;
    *MCP_SESSION.lock().unwrap() = None;
    let (server, port, requests, task_gets) =
        spawn_fake_mcp_server(FakeTaskTerminal::CompletedAfter {
            working_polls: 2,
            tool_text: r#"{"success":true,"boot_complete":true}"#,
        })
        .await;
    let McpOutcome::Result(result) = mcp_call(
        "tools/call",
        serde_json::json!({"name":"serial_reset","arguments":{"wait_boot":true}}),
        &[port],
    )
    .await
    else {
        panic!("task-capable call must return the completed result");
    };
    server.abort();
    // The completed task payload is the original CallToolResult — callers
    // keep parsing `content[0].text` exactly like the synchronous path.
    assert_eq!(
        result["content"][0]["text"].as_str(),
        Some(r#"{"success":true,"boot_complete":true}"#)
    );
    // Two working polls plus one terminal poll.
    assert_eq!(
        task_gets.load(std::sync::atomic::Ordering::Relaxed),
        3,
        "expected 2 working + 1 completed tasks/get polls"
    );
    // The polls must reuse the session established by initialize.
    let requests = requests.lock().unwrap();
    let polls: Vec<&String> = requests
        .iter()
        .filter(|r| r.contains("\"method\":\"tasks/get\""))
        .collect();
    assert_eq!(polls.len(), 3, "expected exactly 3 tasks/get requests");
    for poll in polls {
        assert!(
            poll.to_lowercase().contains("mcp-session-id: fake-session"),
            "tasks/get must carry the initialize session header, got: {poll}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_call_returns_error_when_task_failed() {
    let _session_guard = SESSION_TEST_MUTEX.lock().await;
    *MCP_SESSION.lock().unwrap() = None;
    let (server, port, _requests, task_gets) =
        spawn_fake_mcp_server(FakeTaskTerminal::Failed("reset failed")).await;
    let McpOutcome::Result(result) = mcp_call(
        "tools/call",
        serde_json::json!({"name":"serial_reset","arguments":{"wait_boot":true}}),
        &[port],
    )
    .await
    else {
        panic!("failed task must surface its error payload");
    };
    server.abort();
    assert_eq!(task_gets.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert_eq!(result["message"].as_str(), Some("reset failed"));
    assert_eq!(result["code"].as_i64(), Some(-32000));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initialized_notification_carries_session_header() {
    let _session_guard = SESSION_TEST_MUTEX.lock().await;
    // Gap §1.1: every request after initialize — the notification
    // included — must carry Mcp-Session-Id, or the server places it in
    // a fresh, unrelated session.
    *MCP_SESSION.lock().unwrap() = None;
    let (server, port, requests, _) = spawn_fake_mcp_server(FakeTaskTerminal::CompletedAfter {
        working_polls: 0,
        tool_text: "{}",
    })
    .await;
    let _ = mcp_call(
        "tools/call",
        serde_json::json!({"name":"serial_get_state","arguments":{}}),
        &[port],
    )
    .await;
    server.abort();
    let requests = requests.lock().unwrap();
    let notification = requests
        .iter()
        .find(|r| r.contains("\"method\":\"notifications/initialized\""))
        .expect("notifications/initialized request captured");
    assert!(
        notification
            .to_lowercase()
            .contains("mcp-session-id: fake-session"),
        "notification must carry the initialize session header, got: {notification}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initialize_error_frame_rejects_handshake() {
    let _session_guard = SESSION_TEST_MUTEX.lock().await;
    // Gap §1.2: an initialize answered with a JSON-RPC error frame must
    // not be cached as "initialized" and must not trigger
    // notifications/initialized; the follow-up tools/call error is then
    // surfaced verbatim instead of "no MCP server".
    *MCP_SESSION.lock().unwrap() = None;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let cap = requests.clone();
    let server = tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let Some(raw) = read_http_request(&mut sock).await else {
                break;
            };
            cap.lock().unwrap().push(raw.clone());
            let body = raw.split("\r\n\r\n").nth(1).unwrap_or("");
            let Ok(json) = serde_json::from_str::<serde_json::Value>(body) else {
                break;
            };
            let response_body = match json["method"].as_str() {
                Some("initialize") => {
                    r#"data: {"jsonrpc":"2.0","id":0,"error":{"code":-32022,"message":"unsupported protocol version"}}"#
                }
                Some("tools/call") => {
                    r#"data: {"jsonrpc":"2.0","id":1,"error":{"code":-32002,"message":"session not initialized"}}"#
                }
                _ => "",
            };
            write_http_response(&mut sock, "200 OK", "", response_body).await;
        }
    });
    let outcome = mcp_call(
        "tools/call",
        serde_json::json!({"name":"serial_get_state","arguments":{}}),
        &[port],
    )
    .await;
    server.abort();
    let requests = requests.lock().unwrap();
    assert!(
        !requests
            .iter()
            .any(|r| r.contains("\"method\":\"notifications/initialized\"")),
        "notification must be skipped when the initialize handshake fails"
    );
    assert!(
        MCP_SESSION.lock().unwrap().is_none(),
        "a failed handshake must not be cached as initialized"
    );
    let McpOutcome::RpcError { code, message } = outcome else {
        panic!("server answered with a JSON-RPC error — must be classified as RpcError");
    };
    assert_eq!(code, -32002);
    assert!(message.contains("session not initialized"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_session_cache_is_invalidated_and_retried() {
    let _session_guard = SESSION_TEST_MUTEX.lock().await;
    // Gap §1.5: the server restarted and recycled the port — the cached
    // session id answers 404. dutabo must drop the cache, re-initialize
    // once, and retry the call.
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    // Simulate a session cached before the (fake) restart.
    *MCP_SESSION.lock().unwrap() = Some((port, "s1".to_string()));
    let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let cap = requests.clone();
    let server = tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let Some(raw) = read_http_request(&mut sock).await else {
                break;
            };
            cap.lock().unwrap().push(raw.clone());
            let body = raw.split("\r\n\r\n").nth(1).unwrap_or("");
            let Ok(json) = serde_json::from_str::<serde_json::Value>(body) else {
                break;
            };
            let has_stale_session = raw.to_lowercase().contains("mcp-session-id: s1");
            match json["method"].as_str() {
                Some("initialize") => {
                    write_http_response(
                            &mut sock,
                            "200 OK",
                            "Mcp-Session-Id: s2\r\n",
                            r#"data: {"jsonrpc":"2.0","id":0,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"fake","version":"0.0.0"}}}"#,
                        )
                        .await;
                }
                Some("tools/call") if has_stale_session => {
                    write_http_response(
                        &mut sock,
                        "404 Not Found",
                        "",
                        "Not Found: Session not found",
                    )
                    .await;
                }
                Some("tools/call") => {
                    write_http_response(
                            &mut sock,
                            "200 OK",
                            "",
                            r#"data: {"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"{\"success\":true}"}],"isError":false}}"#,
                        )
                        .await;
                }
                _ => write_http_response(&mut sock, "202 Accepted", "", "").await,
            }
        }
    });
    let outcome = mcp_call(
        "tools/call",
        serde_json::json!({"name":"serial_get_state","arguments":{}}),
        &[port],
    )
    .await;
    server.abort();
    let requests = requests.lock().unwrap();
    let initializes = requests
        .iter()
        .filter(|r| r.contains("\"method\":\"initialize\""))
        .count();
    assert_eq!(
        initializes, 1,
        "stale cache must be dropped and initialize re-run once"
    );
    let tools_calls = requests
        .iter()
        .filter(|r| r.contains("\"method\":\"tools/call\""))
        .count();
    assert_eq!(
        tools_calls, 2,
        "first call (stale session) 404s, retry with fresh session succeeds"
    );
    let McpOutcome::Result(result) = outcome else {
        panic!("retried call must succeed");
    };
    assert_eq!(
        result["content"][0]["text"].as_str(),
        Some("{\"success\":true}")
    );
    // The cache now holds the fresh session id.
    let cache = MCP_SESSION.lock().unwrap();
    assert_eq!(
        cache
            .as_ref()
            .map(|(cached_port, id)| (*cached_port, id.as_str())),
        Some((port, "s2"))
    );
}

/// Write one minimal HTTP/1.1 response (for the inline fake servers).
async fn write_http_response(
    sock: &mut tokio::net::TcpStream,
    status: &str,
    extra_headers: &str,
    body: &str,
) {
    use tokio::io::AsyncWriteExt;
    let response = if body.is_empty() {
        format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
    } else {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/event-stream\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    };
    let _ = sock.write_all(response.as_bytes()).await;
}

#[test]
fn parse_sse_accepts_bare_json_body() {
    let frame = parse_sse_payload("{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n")
        .expect("bare JSON body parses");
    assert_eq!(frame["result"]["ok"], serde_json::json!(true));
}

#[test]
fn parse_sse_joins_multiline_data_lines() {
    // SSE spec: consecutive data: lines of one event are joined with \n.
    let text =
        "data: {\"jsonrpc\":\"2.0\",\ndata: \"id\":1,\"result\":{\"ok\":true}}\n\n".to_string();
    let frame = parse_sse_payload(&text).expect("multiline data joined");
    assert_eq!(frame["result"]["ok"], serde_json::json!(true));
}

#[test]
fn parse_sse_skips_comments_and_control_fields() {
    let frame = parse_sse_payload(
            ": keep-alive comment\n\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"n\":1}}\n\n",
        )
        .expect("data frame parsed");
    assert_eq!(frame["result"]["n"], serde_json::json!(1));
}

#[test]
fn parse_sse_returns_last_frame() {
    let frame = parse_sse_payload(
            "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"n\":1}}\n\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"n\":2}}\n\n",
        )
        .expect("frame parsed");
    assert_eq!(frame["result"]["n"], serde_json::json!(2));
}

#[test]
fn parse_sse_garbage_returns_none() {
    assert_eq!(parse_sse_payload(""), None);
    assert_eq!(parse_sse_payload(": comment only"), None);
    assert_eq!(parse_sse_payload("not json at all"), None);
}

/// Result of feeding a byte into the Ctrl-T escape detector.
#[derive(Debug, PartialEq, Eq)]
enum EscapeAction {
    Send(Vec<u8>),
    None,
    Quit,
}

struct CtrlTEscape {
    esc: bool,
}

impl CtrlTEscape {
    fn new() -> Self {
        Self { esc: false }
    }

    fn feed(&mut self, b: u8) -> EscapeAction {
        if self.esc {
            self.esc = false;
            if b == b'q' || b == b'Q' {
                return EscapeAction::Quit;
            }
            return EscapeAction::Send(vec![0x14, b]);
        }
        if b == 0x14 {
            self.esc = true;
            return EscapeAction::None;
        }
        EscapeAction::Send(vec![b])
    }
}

#[test]
fn test_ctrl_t_q_quits() {
    let mut esc = CtrlTEscape::new();
    assert_eq!(esc.feed(0x14), EscapeAction::None); // Ctrl-T
    assert_eq!(esc.feed(b'q'), EscapeAction::Quit); // q → quit
}

#[test]
fn test_ctrl_t_capital_q_quits() {
    let mut esc = CtrlTEscape::new();
    assert_eq!(esc.feed(0x14), EscapeAction::None);
    assert_eq!(esc.feed(b'Q'), EscapeAction::Quit);
}

#[test]
fn test_ctrl_t_other_key_passthrough() {
    let mut esc = CtrlTEscape::new();
    assert_eq!(esc.feed(0x14), EscapeAction::None); // Ctrl-T
    // Pressing 'x' after Ctrl-T → both bytes sent
    assert_eq!(esc.feed(b'x'), EscapeAction::Send(vec![0x14, b'x']));
}

#[test]
fn test_double_ctrl_t() {
    let mut esc = CtrlTEscape::new();
    assert_eq!(esc.feed(0x14), EscapeAction::None); // first Ctrl-T
    assert_eq!(esc.feed(0x14), EscapeAction::Send(vec![0x14, 0x14])); // second sent through
}

#[test]
fn test_normal_keys_passthrough() {
    let mut esc = CtrlTEscape::new();
    assert_eq!(esc.feed(b'a'), EscapeAction::Send(vec![b'a']));
    assert_eq!(esc.feed(b'\n'), EscapeAction::Send(vec![b'\n']));
    assert_eq!(esc.feed(0x03), EscapeAction::Send(vec![0x03])); // Ctrl-C
}

#[test]
fn test_esc_resets_after_quit() {
    // After a quit sequence, a new machine should be clean.
    // Verify that esc flag doesn't persist between sequences.
    let mut esc = CtrlTEscape::new();
    assert_eq!(esc.feed(0x14), EscapeAction::None);
    assert_eq!(esc.feed(b'q'), EscapeAction::Quit);
    // esc flag is now false (reset by feed)
    assert!(!esc.esc);
    // Next byte after quit is a normal key
    assert_eq!(esc.feed(b'a'), EscapeAction::Send(vec![b'a']));
}

#[test]
fn sanitizer_preserves_split_sgr_without_leaking_final_byte() {
    let mut sanitizer = TerminalSanitizer::new(true);
    let mut out = Vec::new();

    sanitizer.filter(b"2: eth0: <BROADCAST> mtu 1500 state \x1b[32", &mut out);
    sanitizer.filter(b"mUP\x1b[0", &mut out);
    sanitizer.filter(b"m group default\n    inet \x1b[32", &mut out);
    sanitizer.filter(b"m127.0.0.1\x1b[0", &mut out);
    sanitizer.filter(b"m/8 scope host lo\n", &mut out);

    let rendered = String::from_utf8(out).unwrap();
    assert_eq!(
        rendered,
        "2: eth0: <BROADCAST> mtu 1500 state \x1b[32mUP\x1b[0m group default\n    inet \x1b[32m127.0.0.1\x1b[0m/8 scope host lo\n"
    );
    assert!(!rendered.contains("state mUP"));
    assert!(!rendered.contains("inet m127"));
}

#[test]
/// CSI sequences like \x1b[2J (erase screen) are *preserved* (not stripped)
/// because readline needs cursor movement / erase / insert-delete
/// sequences for correct visual feedback. Only SGR color codes (\x1b[..m)
/// and OSC sequences (\x1b]..BEL) are filtered.
fn sanitizer_strips_osc_preserves_csi() {
    let mut sanitizer = TerminalSanitizer::new(true);
    let mut out = Vec::new();

    sanitizer.filter(b"a\x1b[2Jb\x1b]0;title\x07c", &mut out);

    // OSC sequence stripped, CSI \x1b[2J preserved for readline
    assert_eq!(String::from_utf8(out).unwrap(), "a\x1b[2Jbc");
}

#[test]
fn sanitizer_preserves_readline_redraw_sequences() {
    let mut sanitizer = TerminalSanitizer::new(true);
    let mut out = Vec::new();

    sanitizer.filter(b"abc\x1b[D\x1b[Kd\x1b[P", &mut out);

    assert_eq!(String::from_utf8(out).unwrap(), "abc\x1b[D\x1b[Kd\x1b[P");
}

#[test]
fn prompt_highlight_colors_user_dir_and_sigil() {
    let mut out = Vec::new();

    highlight_serial_prompt(b"root@dut1:/# ip -c a\r\n", &mut out);
    let rendered = String::from_utf8(out).unwrap();

    assert!(rendered.contains("\x1b[1;32mroot@dut1\x1b[0m:"));
    assert!(rendered.contains("\x1b[1;34m/\x1b[0m"));
    assert!(rendered.contains("\x1b[1;37m# \x1b[0mip -c a"));
}

#[test]
fn prompt_highlight_handles_root_without_host() {
    let mut out = Vec::new();

    highlight_serial_prompt(b"root:/# ls\r\n", &mut out);
    let rendered = String::from_utf8(out).unwrap();

    assert!(rendered.contains("\x1b[1;32mroot\x1b[0m:"));
    assert!(rendered.contains("\x1b[1;34m/\x1b[0m"));
    assert!(rendered.contains("\x1b[1;37m# \x1b[0mls"));
}

#[test]
fn prompt_highlight_handles_bracket_prompt() {
    let mut out = Vec::new();

    highlight_serial_prompt(b"[root@dut1 ~]# pwd\r\n", &mut out);
    let rendered = String::from_utf8(out).unwrap();

    assert!(rendered.contains("[\x1b[1;32mroot@dut1\x1b[0m "));
    assert!(rendered.contains("\x1b[1;34m~\x1b[0m]"));
    assert!(rendered.contains("\x1b[1;37m# \x1b[0mpwd"));
}

#[test]
fn prompt_highlight_handles_user_shell_prompt() {
    let mut out = Vec::new();

    highlight_serial_prompt(b"ubuntu@board:~/work tree$ git status\r\n", &mut out);
    let rendered = String::from_utf8(out).unwrap();

    assert!(rendered.contains("\x1b[1;32mubuntu@board\x1b[0m:"));
    assert!(rendered.contains("\x1b[1;34m~/work tree\x1b[0m"));
    assert!(rendered.contains("\x1b[1;37m$ \x1b[0mgit status"));
}

#[test]
fn prompt_highlight_bracketed_paste_prefix() {
    // ANSI-prefixed prompt: strip ANSI, highlight clean text
    let mut out = Vec::new();
    highlight_serial_prompt(b"\x1b[?2004hroot@board:/# ls\r\n", &mut out);
    let rendered = String::from_utf8(out).unwrap();
    assert!(rendered.contains("\x1b[1;32mroot@board\x1b[0m:"));
    assert!(rendered.contains("\x1b[1;34m/\x1b[0m"));
    assert!(rendered.contains("\x1b[1;37m# \x1b[0mls"));
}

#[test]
fn prompt_highlight_colored_prompt() {
    // Target-colored prompt: all ANSI stripped, our highlighting applied
    let mut out = Vec::new();
    highlight_serial_prompt(b"\x1b[32mroot@dut1:/\x1b[0m# ls\r\n", &mut out);
    let rendered = String::from_utf8(out).unwrap();
    assert!(
        rendered.contains("\x1b[1;32mroot@dut1"),
        "user not highlighted: {rendered:?}"
    );
    assert!(
        rendered.contains("\x1b[1;34m/"),
        "dir not highlighted: {rendered:?}"
    );
    assert!(
        rendered.contains("\x1b[1;37m# "),
        "sigil not highlighted: {rendered:?}"
    );
}

#[test]
fn prompt_highlight_handles_bracket_user_without_host() {
    let mut out = Vec::new();

    highlight_serial_prompt(b"[root /var/log]# tail messages\r\n", &mut out);
    let rendered = String::from_utf8(out).unwrap();

    assert!(rendered.contains("[\x1b[1;32mroot\x1b[0m "));
    assert!(rendered.contains("\x1b[1;34m/var/log\x1b[0m]"));
    assert!(rendered.contains("\x1b[1;37m# \x1b[0mtail messages"));
}

#[test]
fn prompt_highlight_short_colon_prompt() {
    // Regression: user@hostname directly followed by :/ without extra chars
    // e.g. root@board-name:/#
    let mut out = Vec::new();
    highlight_serial_prompt(b"root@board-name:/# ls\r\n", &mut out);
    let rendered = String::from_utf8(out).unwrap();
    assert!(rendered.contains("\x1b[1;32mroot@board-name\x1b[0m:"));
    assert!(rendered.contains("\x1b[1;34m/\x1b[0m"));
    assert!(rendered.contains("\x1b[1;37m# \x1b[0mls"));
}

#[test]
fn prompt_highlight_does_not_color_non_prompt_lines() {
    let mut out = Vec::new();

    highlight_serial_prompt(b"cat: /tmp# not a prompt\r\n", &mut out);

    assert_eq!(
        String::from_utf8(out).unwrap(),
        "cat: /tmp# not a prompt\r\n"
    );
}

#[test]
fn prompt_highlight_does_not_panic_on_timestamp_like_line() {
    // Regression: greedy \S* backtracking in COLON_USER_RE ends the user
    // match at "12:30" while COLON_DIR_RE matches "30:45" from the first
    // ':', producing a reversed user.end..dir.start slice.
    let mut out = Vec::new();
    highlight_serial_prompt(b"12:30:45# cmd\r\n", &mut out);
    assert_eq!(String::from_utf8(out).unwrap(), "12:30:45# cmd\r\n");
}

#[test]
fn prompt_highlight_does_not_panic_on_short_multi_colon_line() {
    // Minimal repro: "a:b:c$ x" also reverses the separator slice.
    let mut out = Vec::new();
    highlight_serial_prompt(b"a:b:c$ x\r\n", &mut out);
    assert_eq!(String::from_utf8(out).unwrap(), "a:b:c$ x\r\n");
}

#[test]
fn prompt_highlight_does_not_panic_on_bracket_dir_with_spaces() {
    // Regression: BRACKET_USER_RE's greedy \s? swallows the first space
    // of "[root@host /a /b]# cmd" so user.end lands after dir.start.
    let mut out = Vec::new();
    highlight_serial_prompt(b"[root@host /a /b]# cmd\r\n", &mut out);
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "[root@host /a /b]# cmd\r\n"
    );
}

#[test]
fn prompt_highlight_does_not_panic_when_sigil_precedes_dir() {
    // Regression: "a$ x:y# " matches the sigil at index 1 while user and
    // dir extend past it, so highlight sliced past content[..sigil.end].
    let mut out = Vec::new();
    highlight_serial_prompt(b"a$ x:y# \r\n", &mut out);
    assert_eq!(String::from_utf8(out).unwrap(), "a$ x:y# \r\n");
}

#[test]
fn help_has_no_removed_agent_subcommand() {
    assert!(!HELP.contains("agent deploy"));
    assert!(!HELP.contains("agent [--select]"));
}
