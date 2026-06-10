//! Smoke tests — verify basic functionality works end-to-end.
//!
//! These tests validate that the MCP server's `ToolRouter` exposes the
//! expected tools with valid schemas, and config validation catches
//! common errors. They run without hardware (no serial port needed).

use std::collections::HashMap;

use rmcp::ServerHandler;
use rmcp::model::Tool;
use sermcp::config::Config;
use sermcp::mcp_handler::McpHandler;
use sermcp::serial_engine::new_shared_engine;
use sermcp::task_manager::TaskManager;

/// Verify the auto-generated `ToolRouter` lists every expected tool with a
/// non-empty name + description and an object-shaped input schema. Exercises
/// the `#[tool_router]` macro end-to-end without going on the wire.
#[tokio::test]
async fn test_all_tools_have_valid_schema() {
    let router = make_router();

    // Sanity: the router exposes a "tool" capability via ServerHandler::get_info.
    let handler = make_handler();
    let info = handler.get_info();
    assert!(
        info.capabilities.tools.is_some(),
        "ServerHandler::get_info must advertise the tools capability"
    );

    let tools = router.list_all();
    assert!(
        tools.len() >= 23,
        "Expected >= 23 MCP tools, got {}",
        tools.len()
    );

    for tool in &tools {
        assert!(!tool.name.is_empty(), "Tool has empty name");
        assert!(
            tool.description.is_some(),
            "Tool '{}' missing description",
            tool.name
        );
        let annotations = tool
            .annotations
            .as_ref()
            .unwrap_or_else(|| panic!("Tool '{}' missing safety annotations", tool.name));
        assert!(
            annotations.read_only_hint.is_some()
                && annotations.destructive_hint.is_some()
                && annotations.open_world_hint.is_some(),
            "Tool '{}' has incomplete safety annotations",
            tool.name
        );
    }

    let state = find_tool(&router, "serial_get_state")
        .annotations
        .as_ref()
        .unwrap();
    assert_eq!(state.read_only_hint, Some(true));
    assert_eq!(state.destructive_hint, Some(false));

    let command = find_tool(&router, "serial_send_command")
        .annotations
        .as_ref()
        .unwrap();
    assert_eq!(command.read_only_hint, Some(false));
    assert_eq!(command.destructive_hint, Some(true));
}

/// serial_list_duts is the read-only inventory tool agents are told to call
/// BEFORE destructive operations — presence, safety annotations, and the
/// load-bearing part of its description are all contract.
#[tokio::test]
async fn test_serial_list_duts_is_read_only_inventory_tool() {
    let router = make_router();
    let tool = find_tool(&router, "serial_list_duts");
    let annotations = tool
        .annotations
        .as_ref()
        .unwrap_or_else(|| panic!("serial_list_duts missing safety annotations"));
    assert_eq!(annotations.read_only_hint, Some(true));
    assert_eq!(annotations.destructive_hint, Some(false));
    assert_eq!(annotations.open_world_hint, Some(false));
    // The description tells agents to check state before reset/flash —
    // if this phrasing changes, the safety workflow breaks silently.
    let desc = tool.description.as_deref().unwrap();
    assert!(
        desc.contains("BEFORE destructive operations"),
        "description must carry the destructive-op guard: {desc}"
    );
    assert!(
        desc.contains(".target.jsonc"),
        "description must name the config format: {desc}"
    );
}

/// Gap-list #9: tools that physically reset / power-cycle / interrupt the
/// target must advertise `destructive_hint = true` (rmcp's
/// `ToolAnnotations` default is already true — the tools had it explicitly
/// overridden to false). Clients that gate on annotations treat
/// `destructive_hint: false` as "additive update only", which is a lie for
/// a relay reset.
///
/// The most-dangerous-path rule extends this to tools whose default path
/// is passive but whose parameters can physically disturb the target:
/// `serial_learn_connection` resets/power-cycles to sample boots, and
/// `serial_wait_pattern(action=send_interrupt)` transmits the interrupt
/// byte.
///
/// NOTE: this asserts the post-fix state; the annotation fix itself lives in
/// mcp_handler/boot_tools.rs, mcp_handler/learning_tools.rs, and
/// mcp_handler/control_tools.rs (gap-list #9, handler scope).
#[tokio::test]
async fn test_physical_state_changers_mark_destructive() {
    let router = make_router();
    for name in [
        "serial_reset",
        "serial_power_cycle",
        "serial_enter_uboot",
        "serial_button",
        "serial_learn_connection",
        "serial_wait_pattern",
    ] {
        let tool = find_tool(&router, name);
        let annotations = tool
            .annotations
            .as_ref()
            .unwrap_or_else(|| panic!("Tool '{name}' missing safety annotations"));
        assert_eq!(
            annotations.destructive_hint,
            Some(true),
            "tool '{name}' physically resets/powers/interrupts the target; \
             destructive_hint must be true (gap-list #9)"
        );
    }
}

/// Verify essential tools still exist after the `serial_enter_download`
/// removal — the SoC-agnostic `serial_button` tool takes its place.
#[tokio::test]
async fn test_essential_tools_present() {
    let router = make_router();
    let names: Vec<String> = router
        .list_all()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();

    let required = &[
        "serial_exec",
        "serial_send_command",
        "serial_get_state",
        "serial_get_logs",
        "serial_reset",
        "serial_enter_uboot",
        "serial_wait_pattern",
        "serial_uboot_command",
        "serial_button",
        "serial_flash_plan",
        "serial_get_config",
        "serial_load_reference",
        "serial_append_reference",
        "serial_learn_connection",
        "serial_verify_relay",
    ];

    for r in required {
        assert!(
            names.iter().any(|n| n == r),
            "Essential tool '{r}' is missing (have {names:?})"
        );
    }
}

#[tokio::test]
async fn test_removed_observers_and_manual_log_rotation_are_not_exposed_as_tools() {
    let router = make_router();
    let names: Vec<String> = router
        .list_all()
        .into_iter()
        .map(|tool| tool.name.to_string())
        .collect();

    for removed in [
        "serial_get_stages",
        "serial_get_unclassified",
        "serial_new_log",
    ] {
        assert!(
            !names.iter().any(|name| name == removed),
            "Removed observer tool '{removed}' is still exposed: {names:?}"
        );
    }
}

/// Verify `serial_enter_download` was intentionally removed (SoC-agnostic
/// API). If it ever comes back, this test should be updated with intent.
#[tokio::test]
async fn test_no_vendor_specific_download_tool() {
    let router = make_router();
    let names: Vec<String> = router
        .list_all()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    assert!(
        !names.iter().any(|n| n == "serial_enter_download"),
        "`serial_enter_download` must NOT be a tool — it hard-codes a Rockchip-specific sequence. \
         Use the SoC-agnostic `serial_button` tool to compose boot-mode sequences."
    );
}

/// Interactive ownership is handled by the dutabo WebSocket transport; the
/// old pause/resume tools exposed that implementation detail to agents.
#[tokio::test]
async fn test_no_pause_resume_tools() {
    let names: Vec<String> = make_router()
        .list_all()
        .into_iter()
        .map(|tool| tool.name.to_string())
        .collect();
    for removed in ["serial_pause", "serial_resume", "serial_reboot_uboot"] {
        assert!(!names.iter().any(|name| name == removed));
    }
}

/// Verify `serial_enter_uboot` description does not hard-code "Ctrl-C" —
/// the interrupt byte (`UBOOT_INTERRUPT_CHAR`) may differ across boards.
/// Regression guard for the platform-agnostic flood naming.
#[tokio::test]
async fn test_enter_uboot_description_is_platform_agnostic() {
    let router = make_router();
    let enter_uboot = find_tool(&router, "serial_enter_uboot");
    let desc = enter_uboot.description.as_deref().unwrap();
    assert!(
        desc.contains("interrupt-character") || desc.contains("UBOOT_INTERRUPT_CHAR"),
        "description must reference the configurable interrupt byte: {desc}"
    );
    assert!(
        !desc.contains("continuous Ctrl-C flood"),
        "description must not hard-code Ctrl-C: {desc}"
    );
    assert!(
        enter_uboot
            .input_schema
            .get("properties")
            .and_then(|properties| properties.as_object())
            .is_some_and(|properties| properties.contains_key("restart")),
        "schema must expose the merged restart selector: {:?}",
        enter_uboot.input_schema
    );
}

/// `serial_exec` is the unified console-command entry for agent sessions:
/// its schema must expose the plain `mode` enum (no root union — the
/// cross-host compatible shape) with auto as the documented default.
#[tokio::test]
async fn test_serial_exec_schema_uses_mode_enum() {
    let router = make_router();
    let tool = find_tool(&router, "serial_exec");
    let schema = &tool.input_schema;
    assert_eq!(
        schema.get("type").and_then(|t| t.as_str()),
        Some("object"),
        "input schema root must be a plain object: {schema:?}"
    );
    let mode = schema
        .get("properties")
        .and_then(|p| p.get("mode"))
        .expect("mode property");
    assert_eq!(
        mode.get("enum"),
        Some(&serde_json::json!(["auto", "shell", "uboot"])),
        "mode must be a closed enum: {mode:?}"
    );
    assert_eq!(mode.get("default"), Some(&serde_json::json!("auto")));
}

/// Server instructions must lead with tool-selection rules (Codex reads the
/// first ~512 chars as self-contained), not a feature list — hosts that
/// drop instructions still work because the rules mirror tool descriptions
/// and server-side guards.
#[tokio::test]
async fn test_server_instructions_lead_with_tool_selection_rules() {
    let handler = make_handler();
    let instructions = handler
        .get_info()
        .instructions
        .expect("server must advertise instructions");
    assert!(
        instructions.contains("serial_get_state"),
        "instructions must name the preflight rule: {instructions}"
    );
    assert!(
        instructions.contains("serial_exec"),
        "instructions must name the unified command entry: {instructions}"
    );
}

/// The advertised catalog must be deterministic across handler builds so
/// clients can cache it (and prompt caches stay warm).
#[tokio::test]
async fn test_tool_catalog_is_deterministic_across_builds() {
    let first: Vec<String> = make_router()
        .list_all()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    let second: Vec<String> = make_router()
        .list_all()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    assert_eq!(first, second);
    let mut sorted = first.clone();
    sorted.sort();
    assert_eq!(first, sorted, "catalog must be name-sorted");
}

/// Verify config validation catches missing host IP.
#[test]
fn test_config_validation_missing_host() {
    let mut values = HashMap::new();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    values.insert(
        "SERIAL_PORT".into(),
        listener.local_addr().unwrap().port().to_string(),
    );
    values.insert("DUT_DIR".into(), ".dut-serial".into());
    // DEV_HOST_IP is intentionally empty

    let cfg = Config {
        values,
        config_path: None,
        project_dir: None,
        format: sermcp::config::ConfigFormat::None,
    };

    let result = cfg.validate();
    assert!(result.is_err(), "Should error when dev_host_ip is empty");
    let errors = result.unwrap_err();
    assert!(
        errors.iter().any(|e| e.contains("dev_host")),
        "Error should mention dev_host, got: {errors:?}"
    );
}

/// Verify config validation catches missing serial port.
#[test]
fn test_config_validation_missing_port() {
    let mut values = HashMap::new();
    values.insert("DEV_HOST_IP".into(), "host.invalid".into());
    values.insert("SERIAL_PORT".into(), "0".into());
    values.insert("DUT_DIR".into(), ".dut-serial".into());

    let cfg = Config {
        values,
        config_path: None,
        project_dir: None,
        format: sermcp::config::ConfigFormat::None,
    };

    let result = cfg.validate();
    assert!(result.is_err(), "Should error when serial_port is 0");
    let errors = result.unwrap_err();
    assert!(
        errors
            .iter()
            .any(|e| e.contains("serial.port") || e.contains("port")),
        "Error should mention port, got: {errors:?}"
    );
}

/// Verify the vte-based ANSI stripper works on prompt-like data.
#[test]
fn test_strip_ansi_escapes_on_prompt() {
    let input = b"\x1b[?2004hroot@board:/# ls\r\n";
    let clean = sermcp::ansi_strip::strip(input);
    let text = String::from_utf8_lossy(&clean);
    assert!(
        !text.contains('\x1b'),
        "ANSI escapes should be stripped, got: {text:?}"
    );
    assert!(
        text.contains("root@board"),
        "Prompt text should be preserved"
    );
    assert!(text.contains("/#"), "Sigil should be preserved");
}

/// Verify highlight module works.
#[test]
fn test_highlight_prompt() {
    let mut out = Vec::new();
    sermcp::highlight::highlight_serial_prompt(b"root@board:/# ls\r\n", &mut out);
    let rendered = String::from_utf8(out).unwrap();
    assert!(
        rendered.contains("\x1b[1;32mroot@board\x1b[0m"),
        "user should be green"
    );
    assert!(
        rendered.contains("\x1b[1;34m/\x1b[0m"),
        "dir should be blue"
    );
    assert!(
        rendered.contains("\x1b[1;37m# \x1b[0m"),
        "sigil should be white"
    );
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn base_values() -> HashMap<String, String> {
    let mut values = HashMap::new();
    values.insert("DEV_HOST_IP".into(), "127.0.0.1".into());
    values.insert("SERIAL_PORT".into(), "59999".into());
    values.insert("RELAY_PORT".into(), "0".into());
    values.insert("RESET_CHANNEL".into(), "0".into());
    values.insert("DOWNLOAD_CHANNEL".into(), "0".into());
    values.insert("HANG_TIMEOUT".into(), "60".into());
    values.insert("HANG_HYSTERESIS".into(), "3".into());
    values.insert("MAX_ARCHIVED_LOGS".into(), "10".into());
    values.insert("MAX_LOG_FILE_SIZE".into(), "100".into());
    // DUT name "test-dut" → DUT_DIR = `.dut-serial/test-dut`.
    values.insert("DUT_DIR".into(), ".dut-serial/test-dut".into());
    values.insert("LOCK_DIR".into(), "/tmp/sermcp-test-locks".into());
    values.insert("LOGIN_USER".into(), "root".into());
    values.insert("LOGIN_PASS".into(), "".into());
    values.insert("UBOOT_INTERRUPT_STRATEGY".into(), "flood".into());
    values
}

fn make_handler() -> McpHandler {
    let config = Config {
        values: base_values(),
        config_path: None,
        project_dir: None,
        format: sermcp::config::ConfigFormat::None,
    };
    let engine = new_shared_engine(config);
    let tasks = TaskManager::new();
    McpHandler::new(engine, tasks)
}

/// Extract the macro-generated `ToolRouter` from the `McpHandler` to inspect
/// the registered tools without going through the live `list_tools` trait
/// method (which requires a populated `RequestContext`).
fn make_router() -> rmcp::handler::server::router::tool::ToolRouter<McpHandler> {
    // `ToolRouter::clone` keeps the same handler closures — cheap enough for
    // tests.
    make_handler().tool_router.clone()
}

fn find_tool<'a>(
    router: &'a rmcp::handler::server::router::tool::ToolRouter<McpHandler>,
    name: &str,
) -> &'a Tool {
    router
        .get(name)
        .unwrap_or_else(|| panic!("tool '{name}' not in router"))
}
