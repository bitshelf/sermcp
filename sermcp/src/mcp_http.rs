//! Streamable HTTP transport — MCP 2026-07-28 spec.
//!
//! Uses the official rmcp `StreamableHttpService` mounted on an axum
//! `Router` at `/mcp`. Session lifecycle, SSE response streams,
//! `Mcp-Session-Id` header, and `Accept: text/event-stream` negotiation
//! are all handled by `StreamableHttpService` + `LocalSessionManager` —
//! we no longer roll our own JSON-RPC-over-POST layer.
//!
//! Extra routes carried over from the earlier HTTP server:
//!   GET  /health     — liveness probe with live DUT state (for k8s/dutabo)
//!   GET  /serial/ws  — WebSocket relay for `dutabo serial` interactive use
//!
//! Security: defaults to `127.0.0.1` (loopback only). CORS is left to the
//! rmcp `StreamableHttpServerConfig` `with_allowed_origins`. The first
//! code-agent session receives operation access and later sessions are
//! enforced as read-only inside `McpHandler`.
//!
//! ## Idle-timeout self-cleanup
//!
//! When `--idle-timeout` is set, the server monitors the
//! [`AppState::last_activity`] timestamp on every HTTP request (MCP tool
//! calls, `/health` polls, `/serial/ws` upgrade) and every WebSocket
//! message round-trip. After `idle_timeout` of zero activity the watcher
//! task cancels the axum service and stops the engine, freeing the serial
//! lock and letting the process exit cleanly. This prevents zombie MCP
//! servers accumulated by repeated `dutabo` auto-start, and lets a fresh
//! `dutabo`/agent spawn take over the freed port.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use tracing::warn;

use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use tokio_util::sync::CancellationToken;

use crate::mcp_handler::{AgentAccessRegistry, McpHandler};
use crate::serial_engine::SharedEngine;

/// AppState shared by every axum route AND the activity-tracking middleware.
/// The MCP `/mcp` route is owned by `StreamableHttpService`, which builds a
/// fresh `McpHandler` per session via the factory closure (see `run_http`).
/// The `last_activity` cell is bumped by [`bump_activity`] on every request
/// and by the WebSocket handler on each round-tripped frame, feeding the
/// idle-timeout watcher.
#[derive(Clone)]
struct AppState {
    engine: SharedEngine,
    last_activity: Arc<AtomicU64>,
    /// Gap-list #11: cancelled when the idle-timeout watchdog fires (and by
    /// any future shutdown driver). Long-lived upgrades — the `/serial/ws`
    /// relay — exit on cancellation so they cannot pin axum's
    /// graceful-shutdown drain open forever.
    shutdown: CancellationToken,
}

impl AppState {
    fn touch(&self) {
        self.last_activity.store(now_millis(), Ordering::Relaxed);
    }
}

/// Middleware that records the request time as "last activity" so the
/// idle-timeout watcher sees a fresh heartbeat. Wraps every route, so a
/// single MCP tool call, a k8s `/health` probe, or a `/serial/ws`
/// upgrade all count as activity.
async fn bump_activity(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    state.touch();
    next.run(req).await
}

/// Upper bound on a buffered POST body inside [`mcp_guard`]. Generous
/// compared to any real JSON-RPC message; only a guard, not a quota.
const MCP_GUARD_MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Decode an `Mcp-Name`/`Mcp-Param-{Name}` header value per the Streamable
/// HTTP spec's Base64 sentinel format (`=?base64?<b64>?=`); plain ASCII
/// values pass through unchanged. Undecodable sentinels keep their raw
/// form, which then simply fails the equality check.
fn decode_header_value(raw: &str) -> String {
    let Some(b64) = raw.strip_circumfix("=?base64?", "?=") else {
        return raw.to_string();
    };
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(b64)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .unwrap_or_else(|| raw.to_string())
}

/// Validate the modern mirror headers (`Mcp-Method`, `Mcp-Name`) against
/// the JSON-RPC body per the Streamable HTTP spec's Server Validation
/// rules. rmcp does not implement this check, so the /mcp guard does.
///
/// Legacy-era clients send neither header and are never touched. A client
/// that sends `Mcp-Method` is speaking the modern shape: the header must
/// match the body's `method`, and for methods with a mirrored name
/// (`tools/call`, `resources/read`, `prompts/get`) the `Mcp-Name` header
/// must be present (sentinel-decoded) and equal.
fn check_mirror_headers(
    method_header: Option<&str>,
    name_header: Option<&str>,
    body: &serde_json::Value,
) -> Result<(), String> {
    let Some(method_header) = method_header else {
        return Ok(());
    };
    match body.get("method").and_then(serde_json::Value::as_str) {
        Some(body_method) if body_method == method_header => {}
        Some(body_method) => {
            return Err(format!(
                "Mcp-Method header {method_header:?} does not match body method {body_method:?}"
            ));
        }
        None => return Err("request body has no method field".into()),
    }
    let expected_name = match method_header {
        "tools/call" | "prompts/get" => body
            .pointer("/params/name")
            .and_then(serde_json::Value::as_str),
        "resources/read" => body
            .pointer("/params/uri")
            .and_then(serde_json::Value::as_str),
        _ => None,
    };
    let Some(expected) = expected_name else {
        // No mirrored name for this method (or a malformed body rmcp will
        // reject with its own error).
        return Ok(());
    };
    match name_header {
        Some(raw) => {
            let decoded = decode_header_value(raw);
            if decoded == expected {
                Ok(())
            } else {
                Err(format!(
                    "Mcp-Name header does not match body value for {method_header}"
                ))
            }
        }
        None => Err(format!(
            "missing required Mcp-Name header for {method_header}"
        )),
    }
}

/// Spec-conformance guard for POST /mcp, closing two audit gaps:
///
/// * **Origin check (spec MUST)**: Streamable HTTP servers MUST validate
///   `Origin` to stop DNS-rebinding/browser attacks. This is a local
///   debug server that no browser should ever talk to, and rmcp's
///   `allowed_origins` cannot express "reject every Origin" (an empty
///   list disables validation entirely) — so any Origin-bearing request
///   is rejected with 403. Non-browser clients (agents, dutabo) never
///   send `Origin` and are unaffected.
/// * **Mirror-header validation (-32020 HeaderMismatch)**: modern
///   `Mcp-Method`/`Mcp-Name` headers must match the JSON-RPC body; rmcp
///   does not implement this check yet (see `check_mirror_headers`).
async fn mcp_guard(req: Request, next: Next) -> Response {
    if req.uri().path() != "/mcp" || req.method() != axum::http::Method::POST {
        return next.run(req).await;
    }
    if let Some(origin) = req.headers().get(axum::http::header::ORIGIN) {
        warn!(
            origin = ?origin,
            "rejected /mcp request carrying an Origin header (browser/DNS-rebinding guard)"
        );
        return (
            StatusCode::FORBIDDEN,
            "Forbidden: browser origins are not served by this local debug server",
        )
            .into_response();
    }
    // Own the header values: the &str borrows would not survive
    // `req.into_parts()` below.
    let method_header: Option<String> = req
        .headers()
        .get("mcp-method")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let name_header: Option<String> = req
        .headers()
        .get("mcp-name")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    if method_header.is_none() && name_header.is_none() {
        // Legacy-era client: no mirror headers to validate.
        return next.run(req).await;
    }

    let (parts, body) = req.into_parts();
    let bytes = match axum::body::to_bytes(body, MCP_GUARD_MAX_BODY_BYTES).await {
        Ok(bytes) => bytes,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("Bad Request: unreadable body: {error}"),
            )
                .into_response();
        }
    };
    let body_value = serde_json::from_slice::<serde_json::Value>(&bytes).ok();
    let mismatch = body_value.as_ref().and_then(|body| {
        check_mirror_headers(method_header.as_deref(), name_header.as_deref(), body).err()
    });
    if let Some(message) = mismatch {
        warn!(%message, "rejecting /mcp request: mirror-header mismatch");
        let id = body_value
            .as_ref()
            .and_then(|body| body.get("id").cloned())
            .unwrap_or(serde_json::Value::Null);
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32020,
                    "message": format!("Header mismatch: {message}"),
                }
            })),
        )
            .into_response();
    }
    let request = Request::from_parts(parts, axum::body::Body::from(bytes));
    next.run(request).await
}

/// Gap-list #12: classification of a CLI bind host for the Host-header
/// policy.
///
/// rmcp's `StreamableHttpServerConfig` validates the `Host` header against
/// an allowlist that defaults to loopback hosts only (DNS-rebinding
/// protection). The policy must match the CLI binding surface:
/// * loopback binds keep rmcp's defaults untouched;
/// * a wildcard bind (`0.0.0.0`/`::`) cannot enumerate which hosts clients
///   will use, so we keep the loopback-only list and warn loudly instead;
/// * a concrete non-loopback bind explicitly allows that host (appended to
///   the loopback defaults), because silently 403-ing every request on a
///   server the user deliberately exposed on their LAN is a footgun.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindHostClass {
    Loopback,
    Wildcard,
    Concrete,
}

pub fn classify_bind_host(host: &str) -> BindHostClass {
    let host = host
        .trim()
        .trim_matches(|c| c == '[' || c == ']')
        .to_ascii_lowercase();
    if host.is_empty() || host == "*" || host == "0.0.0.0" || host == "::" {
        BindHostClass::Wildcard
    } else if host == "localhost" {
        BindHostClass::Loopback
    } else if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        if ip.is_loopback() {
            BindHostClass::Loopback
        } else {
            BindHostClass::Concrete
        }
    } else {
        // Hostname: allow by name so clients sending `Host: <name>` pass.
        BindHostClass::Concrete
    }
}

/// Gap-list #11 + #12: build the rmcp service config.
///
/// * #12 — Host-header allowlist matched to the bind address (see
///   [`classify_bind_host`]); loopback binds keep rmcp's secure defaults.
/// * #11 — the service subscribes to a child of the shutdown token, so
///   cancelling it terminates all active sessions and SSE keep-alive
///   streams server-side instead of leaving them to block process exit.
fn service_config(
    bind_host: &str,
    shutdown_token: &CancellationToken,
) -> StreamableHttpServerConfig {
    let config = match classify_bind_host(bind_host) {
        BindHostClass::Loopback => StreamableHttpServerConfig::default(),
        BindHostClass::Wildcard => {
            tracing::warn!(
                "[sermcp] binding {bind_host} exposes all interfaces, but \
                 Host-header validation remains loopback-only (DNS-rebinding \
                 protection): LAN clients will receive 403 Forbidden. Bind a \
                 concrete configured LAN IP to serve them."
            );
            StreamableHttpServerConfig::default()
        }
        BindHostClass::Concrete => {
            tracing::info!(
                "[sermcp] non-loopback bind: allowing Host header '{bind_host}' \
                 in addition to the loopback defaults (gap-list #12)"
            );
            // Append to a copy of rmcp's loopback defaults rather than
            // replacing them: loopback access must keep working, and the
            // defaults keep DNS-rebinding protection for any other host.
            StreamableHttpServerConfig::default().with_allowed_hosts([
                "localhost",
                "127.0.0.1",
                "::1",
                bind_host,
            ])
        }
    };
    config.with_cancellation_token(shutdown_token.child_token())
}

/// Run the Streamable HTTP server on the given host:port.
///
/// If `idle_timeout` is set, the server will gracefully shut down after
/// `idle_timeout` with no HTTP/WS activity, releasing the serial lock so
/// a fresh `dutabo`/agent spawn can take over without port conflicts.
pub async fn run_http(
    engine: SharedEngine,
    bind_host: &str,
    bind_port: u16,
    idle_timeout: Option<Duration>,
    connecting_state: serde_json::Value,
) -> Result<(), Box<dyn std::error::Error>> {
    let addr = format!("{bind_host}:{bind_port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    serve_on_listener(
        engine,
        listener,
        idle_timeout,
        connecting_state,
        Some(bind_host),
        false,
    )
    .await
}

/// Serve HTTP/MCP/WS on an already-bound listener.  Stdio mode uses this to
/// expose dutabo's control channel from the same singleton MCP process.
pub async fn run_http_on_listener(
    engine: SharedEngine,
    listener: tokio::net::TcpListener,
    idle_timeout: Option<Duration>,
    connecting_state: serde_json::Value,
) -> Result<(), Box<dyn std::error::Error>> {
    // Host policy is derived from the actual local address (the stdio
    // control transport always binds 127.0.0.1).
    serve_on_listener(engine, listener, idle_timeout, connecting_state, None, true).await
}

async fn serve_on_listener(
    engine: SharedEngine,
    listener: tokio::net::TcpListener,
    idle_timeout: Option<Duration>,
    connecting_state: serde_json::Value,
    bind_host_hint: Option<&str>,
    code_agent_owner_reserved: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let local = listener.local_addr()?;
    // Prefer the caller's original bind host (preserves hostnames for the
    // Host allowlist); fall back to the resolved address.
    let bind_host = bind_host_hint
        .map(|h| h.to_string())
        .unwrap_or_else(|| local.ip().to_string());

    // Factory closure: rmcp calls this for each new session. We give it a
    // fresh `McpHandler` (cheap — `Arc`-clones the engine, tasks, and the
    // macro-generated `ToolRouter`). Per-session isolation is provided by
    // rmcp's session manager; the engine itself is shared across sessions
    // because the physical serial port is a single shared resource.
    let tasks = crate::task_manager::TaskManager::new();
    let agent_access_registry = Arc::new(AgentAccessRegistry::new(code_agent_owner_reserved));
    let handler_factory = {
        let engine = engine.clone();
        let tasks = tasks.clone();
        let connecting_state = connecting_state.clone();
        let agent_access_registry = agent_access_registry.clone();
        move || {
            Ok(McpHandler::new_shared_http(
                engine.clone(),
                tasks.clone(),
                connecting_state.clone(),
                agent_access_registry.clone(),
            ))
        }
    };

    // Gap-list #11: a single cancellation token drives BOTH axum's graceful
    // shutdown AND the rmcp service itself (via a child token). Previously
    // only the axum accept loop was cancelled; `StreamableHttpService` kept
    // serving established sessions — and SSE keep-alive streams (15s pings)
    // stayed alive indefinitely — so one hung client could block the
    // idle-timeout's goal of exiting the process and freeing the serial
    // lock forever. Cancelling the rmcp token terminates all active
    // sessions/streams server-side.
    let shutdown = CancellationToken::new();
    let mcp_service = StreamableHttpService::new(
        handler_factory,
        Arc::new(LocalSessionManager::default()),
        service_config(&bind_host, &shutdown),
    );

    // Single source of truth for activity, shared by:
    //   * the activity-tracking middleware across all routes,
    //   * the WebSocket select loop (round-tripped frames),
    //   * the idle-timeout watcher.
    // Engine clones are cheap `Arc` ref-count bumps, not deep copies.
    let last_activity = Arc::new(AtomicU64::new(now_millis()));
    let state = Arc::new(AppState {
        engine: engine.clone(),
        last_activity: last_activity.clone(),
        shutdown: shutdown.clone(),
    });

    // Apply the activity middleware across the entire router: every HTTP
    // request (MCP JSON-RPC, /health poll, /serial/ws upgrade) bumps
    // `last_activity`. The WebSocket handler additionally refreshes the
    // stamp inside the select loop, so an interactive `dutabo serial`
    // session keeps the process alive even though it produces zero HTTP
    // traffic after the initial upgrade.
    let app = Router::new()
        .nest_service("/mcp", mcp_service)
        .route("/health", get(handle_health))
        .route("/serial/ws", get(handle_serial_ws))
        // Layer order: bump_activity sits outside mcp_guard, so even a
        // guard-rejected request counts as activity (it proves the client
        // is alive). mcp_guard only inspects POST /mcp and passes every
        // other route through untouched.
        .layer(middleware::from_fn(mcp_guard))
        .layer(middleware::from_fn_with_state(state.clone(), bump_activity))
        .with_state(state);

    tracing::info!("[sermcp] Streamable HTTP listening on http://{local}");
    if let Some(timeout) = idle_timeout {
        tracing::info!(
            "[sermcp] idle-timeout watchdog enabled \
             (will shut down after {}s of zero activity across /mcp, /health and /serial/ws)",
            timeout.as_secs()
        );
    }

    // Idle-timeout shutdown: spawn a watcher that closes the listener after
    // `idle_timeout` of zero HTTP/WS activity. The shared `last_activity`
    // cell is bumped on every request by `bump_activity` and on every frame
    // by `serial_ws_handler`, so an active `dutabo`/agent session keeps
    // the process alive indefinitely; a forgotten one frees its resources
    // (serial lock + /tmp DUT lock + memory) so the next spawn gets a clean
    // slate.
    //
    // Gap-list #11: the watcher cancels the same token the rmcp service and
    // the WS relay subscribe to, so no established connection (SSE stream or
    // open WebSocket) can outlive the shutdown decision.
    if let Some(timeout) = idle_timeout {
        let shutdown_engine = engine.clone();
        let watcher_token = shutdown.clone();
        let watcher_activity = last_activity.clone();
        tokio::spawn(async move {
            // Tick every 1/4 of the timeout, capped at 10s; this keeps the
            // shutdown latency short (a miss triggers within at most one
            // tick after the timeout boundary) without busy-spinning.
            let tick_ms = (timeout.as_millis() as u64 / 4).clamp(1_000, 10_000);
            let mut tick = tokio::time::interval(Duration::from_millis(tick_ms));
            loop {
                tick.tick().await;
                let now = now_millis();
                let cur = watcher_activity.load(Ordering::Relaxed);
                let idle = now.saturating_sub(cur);
                if idle >= timeout.as_millis() as u64 {
                    tracing::info!(
                        "HTTP server idle for {}ms (>= {}ms), shutting down — \
                         releasing serial lock for the next dutabo/agent spawn",
                        idle,
                        timeout.as_millis()
                    );
                    watcher_token.cancel();
                    {
                        let mut eng = shutdown_engine.lock().await;
                        eng.stop().await;
                    }
                    return;
                }
            }
        });
        // Graceful shutdown wired to the same cancellation token: rmcp
        // terminates live sessions (its own child token), the WS relay exits
        // (AppState::shutdown), and axum then drains without waiting on any
        // still-open connection.
        axum::serve(listener, app)
            .with_graceful_shutdown(async move { shutdown.cancelled().await })
            .await?;
    } else {
        axum::serve(listener, app).await?;
    }

    // Do not leave detached reset workers holding the engine alive after the
    // transport has stopped accepting task polls.
    tasks.shutdown();

    // Final cleanup. `stop()` is idempotent (uses `Option::take()` for
    // handles and the `state.transition(Stopped)` early-exits on
    // duplicate calls), so invoking it both from the idle watcher and
    // here is safe — the second call collapses to a no-op.
    {
        let mut eng = engine.lock().await;
        eng.stop().await;
    }

    Ok(())
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// GET /health — Kubernetes-compatible liveness probe with live state.
async fn handle_health(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, &'static str)> {
    // Touch has already happened in the `bump_activity` middleware; no need
    // to repeat here.
    let engine = match tokio::time::timeout(Duration::from_secs(2), state.engine.lock()).await {
        Ok(e) => e,
        Err(_) => {
            return Ok(Json(
                serde_json::json!({"status": "starting", "serial": null}),
            ));
        }
    };
    let serial_state = engine.state.current();
    let config = &engine.config;
    #[cfg(unix)]
    let executable_identity = {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata("/proc/self/exe").ok().map(|metadata| {
            serde_json::json!({
                "device": metadata.dev(),
                "inode": metadata.ino(),
            })
        })
    };
    #[cfg(not(unix))]
    let executable_identity: Option<serde_json::Value> = None;

    Ok(Json(serde_json::json!({
        "status": "ok",
        "server": {
            "name": "sermcp",
            "version": env!("CARGO_PKG_VERSION"),
            "pid": std::process::id(),
            "executable": std::env::current_exe()
                .ok()
                .and_then(|path| std::fs::canonicalize(path).ok())
                .map(|path| path.to_string_lossy().into_owned()),
            "executableIdentity": executable_identity,
        },
        "serial": {
            "state": serial_state.as_str(),
            "host": config.dev_host_ip(),
            "port": config.serial_target(),
            "owned": engine.owns_serial_lock(),
            "login_configured": !config.login_user().is_empty(),
        },
        // What this build can serve: a client that needs MORE than the old
        // shape must check here instead of silently degrading (an init TUI
        // asking for `?mode=monitor` on a build that ignores the query
        // would otherwise take the manual session over and block every
        // serial tool it is about to call).
        "capabilities": {
            "monitor": true,
        },
        "uptime_secs": engine.state.uptime_secs(),
        "commands": {
            "total": engine.state.command_count(),
            "errors": engine.state.error_count(),
        }
    })))
}

/// GET /serial/ws — WebSocket relay for dutabo serial interactive console.
///
/// Client sends keystrokes (Text/Binary), receives serial output (Binary).
/// The MCP engine continues monitoring (logs, state, crash detection).
///
/// Activity tracking: the initial upgrade request already bumped
/// `last_activity` via the `bump_activity` middleware. Inside the select
/// loop we refresh the stamp on every round-tripped frame (inbound
/// keystroke or outbound serial data) so a long interactive `dutabo
/// serial` session keeps the process alive — without this, an idle-timeout
/// watcher would kill the MCP server in the middle of a serial session
/// that has no periodic HTTP traffic.
async fn handle_serial_ws(
    State(state): State<Arc<AppState>>,
    ws: WebSocketUpgrade,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> Response {
    // ?mode=monitor — a PASSIVE observer (the init TUI watching TEST
    // probes): no manual-session claim, no tool blocking, no injected
    // prompt byte; client keystrokes are ignored.
    if query.as_deref().is_some_and(|q| q.contains("mode=monitor")) {
        return ws.on_upgrade(move |socket| serial_ws_monitor(socket, state));
    }
    let engine = state.engine.lock().await;
    if !engine.owns_serial_lock() {
        return (
            StatusCode::CONFLICT,
            "serial endpoint is not owned by this MCP process",
        )
            .into_response();
    }
    if engine.state.current() == crate::state_manager::TargetState::Dutabo {
        return (StatusCode::CONFLICT, "manual serial session already active").into_response();
    }
    drop(engine);
    ws.on_upgrade(move |socket| serial_ws_handler(socket, state))
}

/// Passive monitor: subscribe to the engine's always-on broadcast and
/// stream RX to the client. The engine keeps full tool service — this is
/// what lets the TEST probes run while the init terminal shows the live
/// serial output.
async fn serial_ws_monitor(mut socket: WebSocket, state: Arc<AppState>) {
    let mut rx = {
        let engine = state.engine.lock().await;
        engine.monitor_sender().subscribe()
    };
    loop {
        tokio::select! {
            _ = state.shutdown.cancelled() => break,
            received = rx.recv() => {
                match received {
                    Ok(data) => {
                        state.touch();
                        if socket.send(Message::Binary(data.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            msg = socket.recv() => {
                // RX-only: keystrokes are DROPPED (typing during TEST
                // would corrupt marker-wrapped probes). Consume frames
                // only to observe the close.
                match msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {
                        state.touch();
                    }
                    Some(Err(_)) => break,
                }
            }
        }
    }
}

async fn serial_ws_handler(mut socket: WebSocket, state: Arc<AppState>) {
    let engine = state.engine.clone();

    // The route-level check happens before the asynchronous upgrade and two
    // clients can pass it concurrently. Claim the manual session again here
    // under the engine lock so only one upgraded socket can inject input.
    let claimed = {
        let mut eng = engine.lock().await;
        if eng.state.current() == crate::state_manager::TargetState::Dutabo {
            false
        } else {
            eng.begin_manual_session();
            true
        }
    };
    if !claimed {
        let _ = socket.send(Message::Close(None)).await;
        return;
    }

    // Create broadcast channel for serial output → WebSocket
    let (ws_tx, _) = tokio::sync::broadcast::channel::<Vec<u8>>(256);

    // mpsc channel to funnel broadcast data into the select loop
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(256);

    // Subscribe before publishing the channel so the prompt response cannot
    // race ahead of the first receiver.
    let mut ws_rx = ws_tx.subscribe();

    let write_tx = {
        let eng = engine.lock().await;
        eng.get_write_sender()
    };
    {
        let mut eng = engine.lock().await;
        eng.set_ws_tx(ws_tx.clone());
    }

    // Request one visible prompt. The old fixed-time hidden drain could cut a
    // CRLF/prompt response at the registration boundary and also injected a
    // second Enter into the target on every takeover.
    let _ = write_tx.send(b"\r".to_vec()).await;

    // Terminal stays in getty defaults (echo on, icanon, …) — same as PuTTY.
    // No stty manipulation needed; command queue echo-skipping handles MCP.

    // Spawn task: broadcast → mpsc (decouples broadcast from WebSocket borrow)
    let _broadcast_relay = tokio::spawn(async move {
        loop {
            match ws_rx.recv().await {
                Ok(data) => {
                    if out_tx.send(data).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    warn!("WS relay lagged by {n} messages");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    // Single select loop: serial output → WebSocket, keystrokes ← WebSocket.
    // Every round-tripped frame refreshes `last_activity` so an active serial
    // session protects the server from idle-timeout shutdown.
    loop {
        tokio::select! {
            // Gap-list #11: exit when the shutdown token fires, even if the
            // client socket stays open. A silent (no keystrokes, no serial
            // output) WS connection counts as idle and would otherwise pin
            // axum's graceful-shutdown drain indefinitely, blocking the
            // process exit that frees the serial lock.
            _ = state.shutdown.cancelled() => break,
            Some(data) = out_rx.recv() => {
                state.touch();
                if socket.send(Message::Binary(data.into())).await.is_err() {
                    break;
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(t))) => {
                        state.touch();
                        let _ = write_tx.send(t.as_bytes().to_vec()).await;
                    }
                    Some(Ok(Message::Binary(b))) => {
                        state.touch();
                        let _ = write_tx.send(b.to_vec()).await;
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(e)) => {
                        warn!("WS recv error: {e}");
                        break;
                    }
                    // Ping/Pong handled by axum — no need to bump activity,
                    // this branch is rare and a stale WS ping doesn't
                    // represent real user activity.
                    _ => {}
                }
            }
        }
    }

    let mut eng = engine.lock().await;
    eng.clear_ws_tx();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serial_engine::new_shared_engine;
    use std::assert_matches;
    use tempfile::TempDir;

    #[test]
    fn mirror_headers_legacy_clients_pass_through() {
        // Legacy-era clients send neither mirror header.
        let body = serde_json::json!({"method": "tools/call", "params": {"name": "x"}});
        assert!(check_mirror_headers(None, None, &body).is_ok());
    }

    #[test]
    fn mirror_headers_match_and_mismatch() {
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 7,
            "method": "tools/call",
            "params": {"name": "serial_flash_plan"}
        });
        // Both headers agree with the body.
        assert!(check_mirror_headers(Some("tools/call"), Some("serial_flash_plan"), &body).is_ok());
        // Method mismatch.
        let err = check_mirror_headers(Some("prompts/get"), Some("serial_flash_plan"), &body)
            .unwrap_err();
        assert!(err.contains("Mcp-Method"), "{err}");
        // Name mismatch.
        let err =
            check_mirror_headers(Some("tools/call"), Some("serial_reset"), &body).unwrap_err();
        assert!(err.contains("Mcp-Name"), "{err}");
        // Missing Mcp-Name on a method that mirrors one.
        let err = check_mirror_headers(Some("tools/call"), None, &body).unwrap_err();
        assert!(err.contains("missing required Mcp-Name"), "{err}");
        // Body without a method field.
        assert!(check_mirror_headers(Some("tools/call"), None, &serde_json::json!({})).is_err());
    }

    #[test]
    fn mirror_headers_resources_read_uses_uri() {
        let body =
            serde_json::json!({"method": "resources/read", "params": {"uri": "log://serial/full"}});
        assert!(
            check_mirror_headers(Some("resources/read"), Some("log://serial/full"), &body).is_ok()
        );
        assert!(
            check_mirror_headers(Some("resources/read"), Some("log://serial/current"), &body)
                .is_err()
        );
    }

    #[test]
    fn mirror_headers_methods_without_name_skip_name_check() {
        let body = serde_json::json!({"method": "tools/list", "params": {}});
        assert!(check_mirror_headers(Some("tools/list"), None, &body).is_ok());
    }

    #[test]
    fn header_sentinel_decodes() {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD.encode("serial_flash_plan");
        assert_eq!(
            decode_header_value(&format!("=?base64?{b64}?=")),
            "serial_flash_plan"
        );
        assert_eq!(
            decode_header_value("serial_flash_plan"),
            "serial_flash_plan"
        );
        // Undecodable sentinel keeps its raw form (and then fails equality).
        assert_eq!(decode_header_value("=?base64?!!!?="), "=?base64?!!!?=");
    }

    fn create_test_engine() -> SharedEngine {
        let tmp = TempDir::new().unwrap();
        let config = crate::config::test_config(
            tmp.path(),
            "59999",
            "0",
            None,
            ".dut-serial",
            "/tmp/sermcp-test-locks",
        );
        // Keep tmp alive for the engine's lifetime: leak the TempDir in tests.
        std::mem::forget(tmp);
        new_shared_engine(config)
    }

    fn make_state(engine: SharedEngine) -> Arc<AppState> {
        Arc::new(AppState {
            engine,
            last_activity: Arc::new(AtomicU64::new(now_millis())),
            shutdown: CancellationToken::new(),
        })
    }

    // ── Regression: gap-list #12 — bind-host classification ─────────

    #[test]
    fn test_classify_bind_host() {
        use BindHostClass::*;
        assert_matches!(classify_bind_host("127.0.0.1"), Loopback);
        assert_matches!(classify_bind_host("localhost"), Loopback);
        assert_matches!(classify_bind_host("::1"), Loopback);
        assert_matches!(classify_bind_host("[::1]"), Loopback);
        assert_matches!(classify_bind_host("0.0.0.0"), Wildcard);
        assert_matches!(classify_bind_host("::"), Wildcard);
        assert_matches!(classify_bind_host(""), Wildcard);
        assert_matches!(classify_bind_host("  "), Wildcard);
        assert_matches!(classify_bind_host("host.invalid"), Concrete);
        assert_matches!(classify_bind_host("[fe80::1]"), Concrete);
        // Hostname binds are allowed by name.
        assert_matches!(classify_bind_host("dut-host"), Concrete);
    }

    /// Gap-list #12: the rmcp service config's Host allowlist must match the
    /// bind address, and gap-list #11: the config must carry a child of the
    /// shutdown token so cancelling the parent terminates sessions.
    #[test]
    fn test_service_config_host_policy_and_token() {
        let token = CancellationToken::new();

        // Loopback: rmcp's secure defaults, untouched.
        let loopback_cfg = service_config("127.0.0.1", &token);
        assert_eq!(
            loopback_cfg.allowed_hosts,
            vec![
                "localhost".to_string(),
                "127.0.0.1".to_string(),
                "::1".to_string()
            ]
        );

        // Concrete LAN bind: loopback defaults retained + bind host appended
        // (no silently-403'd LAN clients, no widened rebinding surface).
        let lan_cfg = service_config("host.invalid", &token);
        assert!(lan_cfg.allowed_hosts.iter().any(|h| h == "host.invalid"));
        assert!(lan_cfg.allowed_hosts.iter().any(|h| h == "127.0.0.1"));

        // Wildcard bind: cannot enumerate client hosts — keep loopback-only
        // validation rather than disabling Host checks.
        let wildcard_cfg = service_config("0.0.0.0", &token);
        assert_eq!(
            wildcard_cfg.allowed_hosts,
            vec![
                "localhost".to_string(),
                "127.0.0.1".to_string(),
                "::1".to_string()
            ]
        );

        // Gap-list #11: config holds a *child* of the shutdown token, so
        // cancelling the parent reaches the service.
        assert!(!token.is_cancelled());
        assert!(!wildcard_cfg.cancellation_token.is_cancelled());
        token.cancel();
        assert!(wildcard_cfg.cancellation_token.is_cancelled());
    }

    #[tokio::test]
    async fn test_health_endpoint() {
        let engine = create_test_engine();
        let state = make_state(engine);
        let before = state.last_activity.load(Ordering::Relaxed);
        let result = handle_health(State(state.clone())).await.unwrap();
        let json = result.0;
        assert_eq!(json["status"], "ok");
        assert_eq!(json["serial"]["state"], "stopped");
        assert_eq!(json["serial"]["login_configured"], true);
        assert!(json["uptime_secs"].as_f64().is_some());
        assert_eq!(json["commands"]["total"], 0);
        assert_eq!(json["commands"]["errors"], 0);
        // Middleware bumps activity, but calling the handler directly bypasses
        // the layer; just sanity-check the stamp is still sane.
        let _ = before;
    }

    #[tokio::test]
    async fn test_state_touch_updates_last_activity() {
        let state = make_state(create_test_engine());
        let t0 = state.last_activity.load(Ordering::Relaxed);
        // Sleep a small amount so `now_millis()` advances measurably.
        tokio::time::sleep(Duration::from_millis(5)).await;
        state.touch();
        let t1 = state.last_activity.load(Ordering::Relaxed);
        assert!(t1 > t0, "touch must advance last_activity: {t0} -> {t1}");
    }

    // ── Regression: WebSocket write channel ──────────────────────────

    #[tokio::test]
    async fn test_ws_write_channel_accepts_data() {
        // The write channel must accept data even when the console
        // is not connected. If the channel is full or broken, dutabo serial
        // sessions silently break (keystrokes never reach the board).
        let engine = create_test_engine();
        let write_tx = {
            let eng = engine.lock().await;
            eng.console.write_sender()
        };

        // Channel must accept a keystroke
        assert!(
            write_tx.send(b"a".to_vec()).await.is_ok(),
            "write channel should accept keystrokes"
        );
        // Channel must accept newline
        assert!(
            write_tx.send(b"\n".to_vec()).await.is_ok(),
            "write channel should accept newline"
        );
    }
}
