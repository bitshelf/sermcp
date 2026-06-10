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

/// Bridge a real stdio client into the live singleton for `project_dir`.
///
/// `owner_pid` is the project-lock holder reported by
/// [`crate::lock_manager::acquire_project_lock`]; `0` means unknown (no
/// bridgeable candidate can then be identity-verified on the control
/// port, so only a matching `inventory.json` entry is trusted).
pub async fn run(project_dir: &Path, owner_pid: u32) -> Result<(), String> {
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

/// The forwarding loop, with IO injected for tests.
///
/// Returns `Ok(())` when stdin closed (normal shutdown: the teardown
/// DELETE was attempted) or when the upstream became unreachable mid
/// session — after answering every in-flight request with a JSON-RPC
/// error frame so the client sees a protocol-level failure, not a silent
/// dropped connection.
pub(crate) async fn run_bridge<R, W>(reader: R, mut writer: W, url: String) -> Result<(), String>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let client = reqwest::Client::builder()
        .connect_timeout(HEALTH_TIMEOUT)
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    let mut session: Option<String> = None;
    let mut lines = BufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await.map_err(|e| format!("stdin: {e}"))? {
        if line.trim().is_empty() {
            continue;
        }
        let mut request = client
            .post(&url)
            .header("Accept", MCP_ACCEPT)
            .header("Content-Type", "application/json")
            .body(line.clone());
        if let Some(session) = session.as_deref() {
            request = request.header("Mcp-Session-Id", session);
        }
        let response = match request.send().await {
            Ok(response) => response,
            Err(error) => {
                // Upstream unreachable mid-session: answer the in-flight
                // request, then shut down cleanly.
                if let Some(id) = request_id(&line) {
                    let frame = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": -32000,
                            "message": format!("sermcp bridge: upstream unreachable ({error})"),
                        }
                    });
                    let _ = writer.write_all(format!("{frame}\n").as_bytes()).await;
                    let _ = writer.flush().await;
                }
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
            // or 400 on a malformed line: surfaced, not silently dropped.
            return Err(format!("upstream answered HTTP {status}"));
        }
        let Ok(text) = response.text().await else {
            continue;
        };
        for frame in sse_frames(&text) {
            let _ = writer.write_all(format!("{frame}\n").as_bytes()).await;
        }
        let _ = writer.flush().await;
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
    Ok(())
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
        tokio::time::timeout(std::time::Duration::from_secs(10), bridge)
            .await
            .expect("bridge exits on stdin EOF")
            .expect("bridge task joinable")
            .expect("bridge session ends cleanly");

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
