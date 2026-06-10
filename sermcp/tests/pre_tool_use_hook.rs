//! Regression tests for the Claude PreToolUse hook (hooks/claude/pre-tool-use.py).
//! Run with: cargo test --test pre_tool_use_hook
//!
//! CR-8: the hook used to emit `continue: false` for dangerous Dev Host
//! commands and `dutabo serial`, which halts the ENTIRE Claude turn after
//! the hook runs (the agent never sees the guidance and retries the same
//! command after the user resumes). The fixed output is `continue: true`
//! plus a PreToolUse `permissionDecision: "deny"` — only the one tool call
//! is blocked and the reason is injected into Claude's context.
//!
//! These tests drive the real Python hook over stdin/stdout with a
//! synthetic project dir (TARGET_CONF -> .target.jsonc + project marker)
//! so the deny and hint paths are exercised end-to-end without hardware.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

const DEV_HOST_IP: &str = "10.1.2.3";

fn test_relay_port() -> u16 {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.local_addr().unwrap().port()
}

fn hook_path() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../hooks/claude/pre-tool-use.py")
}

/// Create a synthetic project dir (a minimal `.target.jsonc` plus a
/// `.claude` dir) that the hook accepts as a project. The hook parses ZERO
/// config: with no inventory written by a live MCP server, every dangerous
/// dev-host-shaped command hits the fail-closed deny path (A4) — dev-host
/// IPs cannot be verified, so nothing is silently allowed.
fn make_project(dir: &Path) {
    std::fs::write(dir.join(".target.jsonc"), "{}\n").unwrap();
    std::fs::create_dir(dir.join(".claude")).unwrap();
}

/// Run the hook with a PreToolUse Bash payload and return its parsed stdout.
fn run_hook(project: &Path, command: &str) -> Value {
    let input = json!({
        "tool_name": "Bash",
        "tool_input": {"command": command},
    });
    run_hook_raw(project, &input.to_string())
}

fn run_hook_raw(project: &Path, stdin_json: &str) -> Value {
    let mut child = Command::new("python3")
        .arg(hook_path())
        .current_dir(project)
        .env("TARGET_CONF", project.join(".target.jsonc"))
        .env_remove("TARGET_EXCLUDE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn python3 hook");
    child
        .stdin
        .as_mut()
        .expect("hook stdin")
        .write_all(stdin_json.as_bytes())
        .expect("write hook stdin");
    let out = child.wait_with_output().expect("wait for hook");
    assert!(
        out.status.success(),
        "hook exited {:?}, stderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("hook stdout is not valid JSON: {e}; stdout={stdout:?}"))
}

/// Assert the PreToolUse deny envelope: the turn continues, only this one
/// tool call is denied, and the reason steers the agent to `reason_marker`.
fn assert_deny(output: &Value, reason_marker: &str) {
    assert_eq!(
        output["continue"],
        json!(true),
        "CR-8: deny must be continue: true — continue: false halts the whole turn: {output}"
    );
    let specific = output
        .get("hookSpecificOutput")
        .unwrap_or_else(|| panic!("expected hookSpecificOutput, got {output}"));
    assert_eq!(specific["hookEventName"], "PreToolUse", "{output}");
    assert_eq!(specific["permissionDecision"], "deny", "{output}");
    let reason = specific["permissionDecisionReason"]
        .as_str()
        .unwrap_or_else(|| panic!("deny reason missing: {output}"));
    assert!(
        reason.contains(reason_marker),
        "deny reason must steer the agent to {reason_marker}: {reason}"
    );
}

/// CR-8 regression: a dangerous Dev Host command must be denied as a single
/// tool call (continue: true + permissionDecision: deny), not by stopping
/// the whole turn (the old bug emitted continue: false here).
#[test]
fn test_dangerous_dev_host_command_is_single_call_deny() {
    let tmp = tempfile::TempDir::new().unwrap();
    make_project(tmp.path());
    let output = run_hook(tmp.path(), &format!("ssh root@{DEV_HOST_IP} \"reboot\""));
    assert_deny(&output, "dutabo uf");
    assert!(
        output["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .unwrap()
            .contains("PRODUCTION"),
        "{output}"
    );
}

/// CR-8 regression: `dutabo serial` must be denied as a single tool call
/// with MCP-tool guidance (the old code let it fall through to the raw
/// serial hint, so the deny envelope never fired).
#[test]
fn test_dutabo_serial_is_single_call_deny() {
    let tmp = tempfile::TempDir::new().unwrap();
    make_project(tmp.path());
    let output = run_hook(tmp.path(), "dutabo serial -d rk3576");
    assert_deny(&output, "serial_send_command");
}

/// Raw serial access is denied per tool call. The Agent continues working but
/// must route the operation through the project-owned MCP.
#[test]
fn test_raw_serial_access_is_single_call_deny() {
    let tmp = tempfile::TempDir::new().unwrap();
    make_project(tmp.path());
    let output = run_hook(tmp.path(), "tio /dev/ttyUSB0");
    assert_deny(&output, "serial_send_command");
}

/// Direct relay TCP access is also denied; physical reset/power control is
/// serialized by MCP ownership.
#[test]
fn test_relay_tcp_access_is_single_call_deny() {
    let tmp = tempfile::TempDir::new().unwrap();
    make_project(tmp.path());
    let relay_port = test_relay_port();
    let output = run_hook(
        tmp.path(),
        &format!(r"printf '\xa0\x01\x01\xa2' > /dev/tcp/{DEV_HOST_IP}/{relay_port}"),
    );
    assert_deny(&output, "serial_reset");
}

/// Commands that touch nothing serial-related pass through unchanged.
#[test]
fn test_safe_command_passes_through() {
    let tmp = tempfile::TempDir::new().unwrap();
    make_project(tmp.path());
    let output = run_hook(tmp.path(), "echo hello");
    assert_eq!(output, json!({"continue": true}), "{output}");
}

/// Defensive contract: unparseable stdin must degrade to continue: true —
/// a broken hook input must never block the tool call.
#[test]
fn test_unparseable_input_degrades_to_continue() {
    let tmp = tempfile::TempDir::new().unwrap();
    make_project(tmp.path());
    let output = run_hook_raw(tmp.path(), "not-json{");
    assert_eq!(output, json!({"continue": true}), "{output}");
}
