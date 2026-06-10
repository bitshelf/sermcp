//! stdio → Streamable-HTTP bridge for coexisting with the project's
//! singleton engine.
//!
//! A project admits exactly one sermcp engine (project lock, see
//! `lock_manager`). When a stdio client — Claude Code / Codex / ZCode /
//! pi spawning `sermcp` from `.mcp.json` — starts while a live instance
//! already holds the lock, the old behavior (exit with "refusing to start
//! a second instance") surfaced to the agent host as a dead connection
//! (`-32000` on Claude Code's `/mcp` reconnect). The documented workaround
//! was "never mix stdio and HTTP modes in one project".
//!
//! This bridge removes the mode exclusivity instead: the late stdio
//! process becomes a transparent proxy for the running instance's
//! Streamable HTTP endpoint,
//!
//! ```text
//! stdin line ──POST /mcp──→ existing singleton ──SSE data:──→ stdout line
//! ```
//!
//! Protocol-opaque by design:
//! * the client's own `initialize` is forwarded verbatim, so `clientInfo`
//!   stays truthful and the server's per-session roles (owner / read-only
//!   guest → `busy` state) apply to the real client, not to the bridge;
//! * the `mcp-Session-Id` response header is captured on first sight and
//!   attached to every later POST (rmcp answers headerless calls with an
//!   error and terminated sessions with 404);
//! * responses are re-framed from SSE `data:` payloads back to plain
//!   JSON lines — including intermediate progress notifications, which
//!   arrive as separate SSE events before the final result;
//! * notification POSTs (no `id`) and client responses to server
//!   requests (no `method`) legitimately get `202` with no body — the
//!   bridge emits nothing for them;
//! * stdin EOF sends `DELETE /mcp` so the upstream session — and its
//!   agent-role slot — is released promptly instead of leaking until the
//!   server idles out.
//!
//! Endpoint discovery prefers the port recorded in the holder's own
//! `inventory.json` (works for any bind it published) and falls back to
//! the deterministic per-project control port every stdio instance binds.
//! Both are identity-checked via `GET /health`: the reported server pid
//! must equal the project-lock owner, so the bridge can never attach to
//! another project's server that happens to sit on a colliding port.

use std::path::Path;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

/// The rmcp Streamable HTTP server requires this exact Accept header —
/// a bare request gets HTTP 406 Not Acceptable (same contract as the
/// dutabo CLI client).
const MCP_ACCEPT: &str = "application/json, text/event-stream";

/// Health-probe budget for endpoint discovery. Loopback only: a dead or
/// wrong server must fail fast, never delay the agent's startup.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(2);

/// Session-teardown budget on stdin EOF. Best effort — a stuck DELETE
/// must not keep the (already useless) bridge process alive.
const DELETE_TIMEOUT: Duration = Duration::from_secs(3);

/// Total budget for one forwarded POST, covering connect AND the full SSE
/// body: a wedged upstream that accepts the connection but never answers
/// must surface as an error frame to the client, not an indefinite hang.
/// The ceiling is deliberately generous — a legitimate
/// `serial_wait_pattern` call streams its SSE body for minutes. Override
/// with `SERMCP_BRIDGE_REQUEST_TIMEOUT_SECS` (clamped to 30s..24h).
fn bridge_request_timeout() -> Duration {
    static TIMEOUT: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *TIMEOUT.get_or_init(|| {
        let secs = std::env::var("SERMCP_BRIDGE_REQUEST_TIMEOUT_SECS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(900);
        Duration::from_secs(secs.clamp(30, 86_400))
    })
}

/// How an established bridge session ended.
#[derive(Debug, PartialEq)]
pub enum BridgeOutcome {
    /// stdin closed — the client went away; nothing left to serve.
    ClientClosed,
    /// A session was live and the upstream died or hung mid-session. Every
    /// in-flight request was answered with a JSON-RPC error frame; the
    /// host must respawn this process for a fresh handshake (the client
    /// will not re-send its `initialize`).
    UpstreamGone(String),
}

/// Bridge a real stdio client into the live singleton for `project_dir`.
///
/// `owner_pid` is the project-lock holder reported by
/// [`crate::lock_manager::acquire_project_lock`]; `0` means unknown (no
/// bridgeable candidate can then be identity-verified on the control
/// port, so only a matching `inventory.json` entry is trusted).
///
/// `Err` means NO endpoint could be established (the caller may retry or
/// take the singleton role once the owner dies); `Ok` reports how the
/// session ended.
pub async fn run(project_dir: &Path, owner_pid: u32) -> Result<BridgeOutcome, String> {
    let candidates = upstream_candidates(project_dir, owner_pid);
    if candidates.is_empty() {
        return Err("no identity-verifiable upstream endpoint".to_string());
    }
    let client = reqwest::Client::builder()
        .connect_timeout(HEALTH_TIMEOUT)
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    for (port, expect_pid) in candidates {
        let base = format!("http://127.0.0.1:{port}");
        if !endpoint_matches(&client, &base, expect_pid).await {
            continue;
        }
        tracing::info!(
            "bridging stdio client into the running instance on 127.0.0.1:{port} (pid {expect_pid})"
        );
        return run_bridge(
            tokio::io::stdin(),
            tokio::io::stdout(),
            format!("{base}/mcp"),
        )
        .await;
    }
    Err("lock holder has no reachable HTTP endpoint".to_string())
}

/// A candidate upstream: port plus the server pid it must prove via
/// `/health` before the bridge may attach.
type Candidate = (u16, u32);

/// Resolve bridgeable endpoints for the lock holder, strongest first.
///
/// 1. `inventory.json` `mcp_http_port` — but only when the file was
///    written for THIS project and records the lock owner (or, when the
///    owner is unknown, any pid that is still alive: the inventory's own
///    project scoping then carries the identity check);
/// 2. the deterministic project control port every stdio instance binds
///    (`ports::project_mcp_port`) — only usable when the lock reported a
///    concrete owner pid to verify against.
pub(crate) fn upstream_candidates(project_dir: &Path, owner_pid: u32) -> Vec<Candidate> {
    let mut candidates: Vec<Candidate> = Vec::new();
    let canonical = project_dir
        .canonicalize()
        .unwrap_or_else(|_| project_dir.to_path_buf());
    let inventory = std::fs::read_to_string(canonical.join(".dut-serial/inventory.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok());
    if let Some(inv) = inventory {
        let same_project = inv["project_dir"].as_str() == Some(canonical.to_str().unwrap_or(""));
        let pid = inv["mcp_pid"].as_u64().and_then(|p| u32::try_from(p).ok());
        let trustable = same_project
            && pid.is_some_and(|pid| {
                owner_pid != 0 && pid == owner_pid
                    || owner_pid == 0 && crate::lock_manager::is_live_server_holder(pid)
            });
        if trustable
            && let (Some(port), Some(pid)) = (
                inv["mcp_http_port"]
                    .as_u64()
                    .and_then(|p| u16::try_from(p).ok()),
                pid,
            )
        {
            candidates.push((port, pid));
        }
    }
    if owner_pid != 0 {
        let port = crate::ports::project_mcp_port(project_dir);
        if !candidates.iter().any(|(p, _)| *p == port) {
            candidates.push((port, owner_pid));
        }
    }
    candidates
}

/// Does `GET {base}/health` describe the expected live sermcp?
async fn endpoint_matches(client: &reqwest::Client, base: &str, expect_pid: u32) -> bool {
    let Ok(response) = client
        .get(format!("{base}/health"))
        .timeout(HEALTH_TIMEOUT)
        .send()
        .await
    else {
        return false;
    };
    let Ok(health) = response.json::<serde_json::Value>().await else {
        return false;
    };
    health["status"] == "ok" && health["server"]["pid"].as_u64() == Some(expect_pid as u64)
}

/// Stamp this bridging process's agent-session identity into the
/// client's `initialize` request (`_meta`, spec-permitted extension).
///
/// The env is the one thing the spawning agent host sets identically on
/// the singleton spawn AND every bridge respawn of the same session, so
/// it is the only reliable "same agent" signal that can travel from here
/// into the server process (whose own env is the FIRST spawn's).
/// Non-initialize lines and unparseable lines pass through verbatim;
/// `serde_json`'s preserve_order keeps every other field order stable.
fn inject_agent_identity(line: &str, identity: Option<&str>) -> String {
    let Some(identity) = identity else {
        return line.to_string();
    };
    // Cheap pre-filter: avoid parsing the (possibly large) tool-call lines.
    if !line.contains("initialize") {
        return line.to_string();
    }
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(line) else {
        return line.to_string();
    };
    if value.get("method").and_then(serde_json::Value::as_str) != Some("initialize") {
        return line.to_string();
    }
    // `_meta` lives INSIDE `params` (it is a field of the params object,
    // not of the JSON-RPC envelope) — a top-level stamp is silently
    // dropped by the server's params deserialization.
    let Some(params) = value
        .get_mut("params")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return line.to_string();
    };
    let meta = params
        .entry("_meta")
        .or_insert_with(|| serde_json::json!({}));
    let Some(meta) = meta.as_object_mut() else {
        return line.to_string();
    };
    meta.insert(
        crate::mcp_handler::META_AGENT_SESSION.to_string(),
        serde_json::Value::String(identity.to_string()),
    );
    serde_json::to_string(&value).unwrap_or_else(|_| line.to_string())
}

/// The forwarding loop, with IO injected for tests.
///
/// Returns `Ok(ClientClosed)` when stdin closed (normal shutdown: the
/// teardown DELETE was attempted) and `Ok(UpstreamGone(..))` when the
/// upstream became unreachable or hung mid-session — after answering
/// every in-flight request with a JSON-RPC error frame so the client
/// sees a protocol-level failure, not a silent dropped connection.
pub(crate) async fn run_bridge<R, W>(
    reader: R,
    writer: W,
    url: String,
) -> Result<BridgeOutcome, String>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    run_bridge_with_timeout(reader, writer, url, bridge_request_timeout()).await
}

async fn run_bridge_with_timeout<R, W>(
    reader: R,
    mut writer: W,
    url: String,
    timeout: Duration,
) -> Result<BridgeOutcome, String>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let client = reqwest::Client::builder()
        .connect_timeout(HEALTH_TIMEOUT)
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    let identity = crate::mcp_handler::agent_session_identity();
    let mut session: Option<String> = None;
    let mut lines = BufReader::new(reader).lines();
    let mut outcome = BridgeOutcome::ClientClosed;
    'forward: loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(error) => {
                tracing::warn!(%error, "stdio bridge client input closed");
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let payload = inject_agent_identity(&line, identity.as_deref());
        let mut request = client
            .post(&url)
            .header("Accept", MCP_ACCEPT)
            .header("Content-Type", "application/json")
            .timeout(timeout)
            .body(payload);
        if let Some(session) = session.as_deref() {
            request = request.header("Mcp-Session-Id", session);
        }
        let response = match request.send().await {
            Ok(response) => response,
            Err(error) => {
                // Upstream unreachable or hung past the request budget:
                // answer the in-flight request, then end the session.
                write_error_frame(
                    &mut writer,
                    &line,
                    &format!("sermcp bridge: upstream unreachable ({error})"),
                )
                .await;
                outcome = BridgeOutcome::UpstreamGone(error.to_string());
                break;
            }
        };
        if session.is_none() {
            session = response
                .headers()
                .get("mcp-session-id")
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
        }
        let status = response.status();
        if !status.is_success() {
            // e.g. 404 after the upstream restarted and recycled sessions,
            // or 400 on a malformed line: surfaced as a protocol-level
            // error frame, never silently dropped.
            write_error_frame(
                &mut writer,
                &line,
                &format!("sermcp bridge: upstream answered HTTP {status}"),
            )
            .await;
            outcome = BridgeOutcome::UpstreamGone(format!("HTTP {status}"));
            break;
        }
        let text = match response.text().await {
            Ok(text) => text,
            Err(error) => {
                write_error_frame(
                    &mut writer,
                    &line,
                    &format!("sermcp bridge: upstream body failed ({error})"),
                )
                .await;
                outcome = BridgeOutcome::UpstreamGone(error.to_string());
                break;
            }
        };
        for frame in sse_frames(&text) {
            if writer
                .write_all(format!("{frame}\n").as_bytes())
                .await
                .is_err()
            {
                break 'forward;
            }
        }
        if writer.flush().await.is_err() {
            break;
        }
    }
    // Teardown: release the upstream session (and its agent-role slot).
    if let Some(session) = session.as_deref() {
        let _ = client
            .delete(&url)
            .header("Mcp-Session-Id", session)
            .timeout(DELETE_TIMEOUT)
            .send()
            .await;
    }
    Ok(outcome)
}

/// Answer one in-flight JSON-RPC request with a protocol-level error
/// frame so the client sees the failure instead of waiting forever.
async fn write_error_frame<W: AsyncWrite + Unpin>(writer: &mut W, line: &str, message: &str) {
    if let Some(id) = request_id(line) {
        let frame = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32000,
                "message": message,
            }
        });
        let _ = writer.write_all(format!("{frame}\n").as_bytes()).await;
        let _ = writer.flush().await;
    }
}

/// The `id` of a JSON-RPC request line, when it carries one (requests and
/// client responses do; notifications do not). Used only to address the
/// error frame when the upstream dies mid-request.
fn request_id(line: &str) -> Option<serde_json::Value> {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()?
        .get("id")
        .cloned()
}

/// Extract every JSON-RPC message from a Streamable HTTP response body.
///
/// Bodies are SSE event streams (`data:` payload lines, possibly several
/// events — progress notifications before the final result — plus
/// keep-alive comment/id/retry fields), but a bare JSON document is also
/// accepted defensively (same framing rules as the dutabo CLI client,
/// except ALL events are kept, not just the last).
pub(crate) fn sse_frames(text: &str) -> Vec<serde_json::Value> {
    if let Ok(value) = serde_json::from_str(text.trim()) {
        return vec![value];
    }
    let mut frames = Vec::new();
    let mut data: Vec<&str> = Vec::new();
    let flush = |data: &mut Vec<&str>, frames: &mut Vec<serde_json::Value>| {
        if !data.is_empty() {
            if let Ok(value) = serde_json::from_str(&data.join("\n")) {
                frames.push(value);
            }
            data.clear();
        }
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            flush(&mut data, &mut frames);
        } else if let Some(payload) = line.strip_prefix("data:") {
            data.push(payload.trim_start());
        }
        // Comments (`:`), `event:`/`id:`/`retry:` control fields carry no
        // JSON payload — skipped.
    }
    flush(&mut data, &mut frames);
    frames
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Keep-alive frames, control fields and multi-event bodies (progress
    /// notification + final result) all survive the re-framing; the empty
    /// `data:` keep-alive from rmcp's initialize response drops out.
    #[test]
    fn sse_frames_keeps_every_event_and_drops_keepalives() {
        let body = "data: \nid: 0\nretry: 3000\n\ndata: {\"jsonrpc\":\"2.0\",\
                    \"method\":\"notifications/progress\",\"params\":{\"progress\":1}}\n\n\
                    data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n";
        let frames = sse_frames(body);
        assert_eq!(frames.len(), 2, "keep-alive must not become a frame");
        assert_eq!(frames[0]["method"], "notifications/progress");
        assert_eq!(frames[1]["id"], 1);
        assert_eq!(frames[1]["result"]["ok"], true);
    }

    /// A bare JSON body (no SSE framing) passes through as one frame.
    #[test]
    fn sse_frames_accepts_bare_json_body() {
        let frames = sse_frames("{\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{}}");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0]["id"], 7);
    }

    /// SSE multi-line `data:` concatenation (SSE spec) still parses.
    #[test]
    fn sse_frames_joins_multiline_data() {
        let frames = sse_frames("data: {\"jsonrpc\":\"2.0\",\ndata: \"id\":9}\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0]["id"], 9);
    }

    /// Garbage yields no frames — never a made-up message on stdout.
    #[test]
    fn sse_frames_ignores_garbage() {
        assert!(sse_frames("event: message\nretry: 100\n\n").is_empty());
        assert!(sse_frames("not json at all").is_empty());
    }

    #[test]
    fn request_id_reads_request_and_skips_notifications() {
        assert_eq!(
            request_id(r#"{"jsonrpc":"2.0","id":42,"method":"tools/call"}"#),
            Some(serde_json::json!(42))
        );
        assert_eq!(
            request_id(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#),
            None
        );
    }

    /// Discovery: an inventory written for THIS project by the lock owner
    /// wins; a foreign project's inventory is never trusted. Unknown
    /// owner (0) falls back to a live-pid inventory check and drops the
    /// unverifiable control-port candidate.
    #[test]
    fn upstream_candidates_trusts_only_matching_inventory() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("proj");
        std::fs::create_dir_all(dir.join(".dut-serial")).unwrap();
        let inventory = serde_json::json!({
            "schema_version": 1,
            "project_dir": dir.canonicalize().unwrap(),
            "mcp_pid": 4242u32,
            "mcp_http_port": 3009u16,
        });
        std::fs::write(
            dir.join(".dut-serial/inventory.json"),
            inventory.to_string(),
        )
        .unwrap();

        // Owner matches the inventory pid: inventory port first, then the
        // deterministic control port for the same pid.
        let candidates = upstream_candidates(&dir, 4242);
        assert_eq!(candidates[0], (3009, 4242));
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[1].1, 4242);

        // Foreign project's inventory: not trusted; only the control port
        // for the known owner remains.
        let foreign = serde_json::json!({
            "schema_version": 1,
            "project_dir": "/some/other/project",
            "mcp_pid": 4242u32,
            "mcp_http_port": 3009u16,
        });
        std::fs::write(dir.join(".dut-serial/inventory.json"), foreign.to_string()).unwrap();
        let candidates = upstream_candidates(&dir, 4242);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].1, 4242);

        // Unknown owner: only a live-pid inventory is trusted, and the
        // control-port candidate disappears (no pid to verify against).
        let candidates = upstream_candidates(&dir, 0);
        assert!(candidates.is_empty(), "dead pid inventory: {candidates:?}");
        let inventory = serde_json::json!({
            "schema_version": 1,
            "project_dir": dir.canonicalize().unwrap(),
            "mcp_pid": std::process::id(),
            "mcp_http_port": 3009u16,
        });
        std::fs::write(
            dir.join(".dut-serial/inventory.json"),
            inventory.to_string(),
        )
        .unwrap();
        assert_eq!(
            upstream_candidates(&dir, 0),
            vec![(3009, std::process::id())]
        );
    }

    // ── end to end: a stdio client proxied into a real HTTP server ──────

    /// Drive one full MCP session through the bridge against a live
    /// Streamable HTTP server: initialize (SSE + session header capture),
    /// a notification (202, no stdout frame), tools/list, a tool call,
    /// then stdin EOF — the bridge must shut down cleanly (teardown DELETE).
    #[tokio::test]
    async fn bridge_proxies_stdio_client_into_http_server_end_to_end() {
        use tokio::io::BufReader;

        let tmp = tempfile::tempdir().unwrap();
        let config = crate::config::test_config(
            tmp.path(),
            "59999",
            "0",
            None,
            ".dut-serial",
            "/tmp/sermcp-test-locks",
        );
        let engine = crate::serial_engine::new_shared_engine(config);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let connecting_state = engine.lock().await.connecting_state_dict();
        let server_engine = engine.clone();
        let server = tokio::spawn(async move {
            let _ = crate::mcp_http::run_http_on_listener(
                server_engine,
                listener,
                None,
                connecting_state,
            )
            .await;
        });

        // The bridge's stdio side; the test drives the client half.
        let (client, bridge_io) = tokio::io::duplex(8192);
        let (bridge_read, bridge_write) = tokio::io::split(bridge_io);
        let (client_read, mut client_write) = tokio::io::split(client);
        let bridge = tokio::spawn(run_bridge(
            bridge_read,
            bridge_write,
            format!("http://127.0.0.1:{port}/mcp"),
        ));

        let mut responses = BufReader::new(client_read);
        let mut buffer = Vec::new();

        // 1. initialize → one frame back (keep-alive SSE dropped).
        send_line(&mut client_write, r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"bridge-test","version":"1"}}}"#).await;
        let init = read_frame(&mut responses, &mut buffer).await;
        assert_eq!(init["id"], 1);
        assert!(
            init["result"]["serverInfo"].is_object(),
            "initialize result: {init}"
        );

        // 2. notification → 202, NO frame on stdout.
        send_line(
            &mut client_write,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        )
        .await;

        // 3. tools/list → the session-header capture is proven by this
        //    call succeeding (headerless calls are rejected by rmcp).
        send_line(
            &mut client_write,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
        )
        .await;
        let tools = read_frame(&mut responses, &mut buffer).await;
        assert_eq!(tools["id"], 2);
        assert!(
            tools["result"]["tools"]
                .as_array()
                .is_some_and(|t| !t.is_empty()),
            "tools via bridge: {tools}"
        );

        // 4. tools/call → real tool result through the bridge.
        send_line(&mut client_write, r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"serial_get_state","arguments":{}}}"#).await;
        let call = read_frame(&mut responses, &mut buffer).await;
        assert_eq!(call["id"], 3);
        assert!(
            call["result"]["content"]
                .as_array()
                .is_some_and(|c| !c.is_empty()),
            "tool call via bridge: {call}"
        );

        // 5. stdin EOF → clean shutdown (the teardown DELETE fires; a
        //    stuck one is bounded by DELETE_TIMEOUT). tokio's split()
        //    halves share one stream via a BiLock, so EOF reaches the
        //    bridge only when BOTH client halves are dropped.
        drop(client_write);
        drop(responses);
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), bridge)
            .await
            .expect("bridge exits on stdin EOF")
            .expect("bridge task joinable")
            .expect("bridge session ends cleanly");
        assert_eq!(outcome, BridgeOutcome::ClientClosed);

        server.abort();
        let _ = server.await;
    }

    /// Identity stamping: the initialize line gains
    /// `_meta.sermcp_agent_session`; other lines (including tool calls that
    /// merely CONTAIN the word "initialize") pass through verbatim.
    #[test]
    fn inject_agent_identity_only_touches_initialize() {
        let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"claude-code","version":"2"}}}"#;
        let stamped = inject_agent_identity(init, Some("sess-42"));
        let value: serde_json::Value = serde_json::from_str(&stamped).unwrap();
        assert_eq!(
            value["params"]["_meta"][crate::mcp_handler::META_AGENT_SESSION],
            serde_json::json!("sess-42")
        );
        // The client-visible fields survive the round-trip untouched.
        assert_eq!(value["method"], "initialize");
        assert_eq!(value["params"]["clientInfo"]["name"], "claude-code");

        // A tool call mentioning "initialize" is not rewritten.
        let tool = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"serial_exec","arguments":{"command":"initialize-dut"}}}"#;
        assert_eq!(inject_agent_identity(tool, Some("sess-42")), tool);
        // No identity → verbatim pass-through of the initialize line too.
        assert_eq!(inject_agent_identity(init, None), init);
    }

    #[test]
    fn identity_stamping_preserves_invalid_params_for_protocol_rejection() {
        for params in ["null", "[]", "42", r#"{"_meta":false}"#] {
            let line =
                format!(r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{params}}}"#);
            assert_eq!(inject_agent_identity(&line, Some("agent")), line);
        }
    }

    #[tokio::test]
    async fn bridge_reports_body_timeout_and_releases_session() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let deleted = std::sync::Arc::new(AtomicBool::new(false));
        let delete_flag = deleted.clone();
        let app = axum::Router::new().route(
            "/mcp",
            axum::routing::post(|| async {
                let stream =
                    futures_util::stream::pending::<Result<axum::body::Bytes, std::io::Error>>();
                (
                    [
                        ("content-type", "text/event-stream"),
                        ("mcp-session-id", "test-session"),
                    ],
                    axum::body::Body::from_stream(stream),
                )
            })
            .delete(move |headers: axum::http::HeaderMap| async move {
                assert_eq!(headers["mcp-session-id"], "test-session");
                delete_flag.store(true, Ordering::SeqCst);
                axum::http::StatusCode::NO_CONTENT
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let (client, bridge_io) = tokio::io::duplex(4096);
        let (reader, writer) = tokio::io::split(bridge_io);
        let (client_read, mut client_write) = tokio::io::split(client);
        let bridge = tokio::spawn(run_bridge_with_timeout(
            reader,
            writer,
            format!("http://{addr}/mcp"),
            Duration::from_millis(100),
        ));
        send_line(
            &mut client_write,
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/list"}"#,
        )
        .await;
        let mut responses = BufReader::new(client_read);
        let frame = read_frame(&mut responses, &mut Vec::new()).await;
        assert_eq!(frame["id"], 7);
        assert_eq!(frame["error"]["code"], -32000);
        assert!(
            frame["error"]["message"]
                .as_str()
                .unwrap()
                .contains("body failed")
        );
        let outcome = tokio::time::timeout(Duration::from_secs(5), bridge)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(outcome, BridgeOutcome::UpstreamGone(_)));
        assert!(deleted.load(Ordering::SeqCst));
        server.abort();
        let _ = server.await;
    }

    /// Write one JSON-RPC line to the bridge's stdin half.
    async fn send_line<W: tokio::io::AsyncWrite + Unpin>(writer: &mut W, line: &str) {
        writer
            .write_all(format!("{line}\n").as_bytes())
            .await
            .expect("bridge stdin writable");
    }

    /// Read one JSON-RPC frame line from the bridge's stdout half with a
    /// hard deadline — a silent bridge must fail the test, not hang it.
    async fn read_frame<R: tokio::io::AsyncRead + Unpin>(
        responses: &mut BufReader<R>,
        buffer: &mut Vec<u8>,
    ) -> serde_json::Value {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            buffer.clear();
            let n = responses
                .read_until(b'\n', buffer)
                .await
                .expect("bridge stdout readable");
            assert!(n > 0, "bridge must answer, not close early");
            serde_json::from_str::<serde_json::Value>(
                std::str::from_utf8(&buffer[..n])
                    .expect("bridge frames are utf-8")
                    .trim(),
            )
            .expect("one JSON-RPC frame per line")
        })
        .await
        .expect("bridge frame within deadline")
    }
}
