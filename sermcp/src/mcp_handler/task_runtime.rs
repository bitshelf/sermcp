use std::time::Duration;

use rmcp::{
    model::{CallToolRequestParams, CallToolResponse, ErrorData},
    task_manager::{TaskContext, TaskExit},
};
use serde_json::{Value, json};

use super::{McpHandler, text_error, tool_result};
use crate::boot_detector::BootEvent;
use crate::serial_engine::SharedEngine;
use crate::state_manager::TargetState;
use crate::tools::params::ResetArgs;

/// Liveness-check budget for `probe_after_command_timeout` — mirrors the old
/// 300ms settle + 800ms direct-read window, but the probe now releases the
/// engine lock between checks instead of holding it while reading the socket.
const PROBE_WINDOW: Duration = Duration::from_millis(1100);
/// How often `probe_after_command_timeout` re-checks the shared byte stream.
const PROBE_POLL: Duration = Duration::from_millis(150);

// ── serial_reset → SEP-2663 task ─────────────────────────────────────────────

impl McpHandler {
    /// Convert a `serial_reset(wait_boot=true)` call into a background
    /// `Task` (SEP-2663). The fast path (`wait_boot=false`) and clients that
    /// do not declare Tasks receive a normal synchronous tool result.
    pub(super) async fn handle_reset_with_task(
        &self,
        request: CallToolRequestParams,
        supports_tasks: bool,
    ) -> Result<CallToolResponse, ErrorData> {
        // Parse the raw JSON arguments via the strong-typed struct so the
        // schema stays the source of truth.
        let args_value =
            serde_json::to_value(request.arguments.unwrap_or_default()).unwrap_or(Value::Null);
        let parsed: ResetArgs = serde_json::from_value(args_value)
            .map_err(|error| ErrorData::invalid_params(error.to_string(), None))?;

        // SEP-2663 capability negotiation is request-scoped. A task must not
        // be created before this check: rmcp's response guard runs only after
        // the handler returns, which would be too late to prevent a reset.
        if !parsed.wait_boot || !supports_tasks {
            let result = self.serial_reset_sync(&parsed).await;
            return Ok(CallToolResponse::Complete(tool_result(result)));
        }

        let engine = self.engine.clone();
        let create_result = match self.tasks.spawn(
            "Reset triggered, waiting for boot to complete...",
            move |context| async move {
                let result = McpHandler::run_reset_sync(engine, &parsed, Some(&context)).await;
                if result.get("cancelled").and_then(Value::as_bool) == Some(true) {
                    Err(TaskExit::Cancelled)
                } else {
                    Ok(tool_result(result))
                }
            },
        ) {
            Some(result) => result,
            None => {
                return Ok(CallToolResponse::Complete(text_error(json!({
                    "success": false,
                    "error": "DUT already has a running task",
                }))));
            }
        };
        Ok(CallToolResponse::Task(create_result))
    }

    /// Synchronous `serial_reset` (used for `wait_boot=false`, and as the
    /// inner body for the Task variant).
    pub(super) async fn serial_reset_sync(&self, args: &ResetArgs) -> Value {
        McpHandler::run_reset_sync(self.engine.clone(), args, None).await
    }

    /// Stateless implementation owned only by `SharedEngine` so it can run
    /// both inline and inside the spawned task worker.
    async fn run_reset_sync(
        engine: SharedEngine,
        args: &ResetArgs,
        task: Option<&TaskContext>,
    ) -> Value {
        let attempts_limit = args.failure_retry.max(1) as usize;
        let mut attempts = 0usize;
        loop {
            if task.is_some_and(TaskContext::is_cancel_requested) {
                return json!({"success": false, "cancelled": true, "error": "Task cancelled"});
            }
            attempts += 1;
            let reset_result = {
                let mut eng = engine.lock().await;
                eng.reset_target(false, 1, args.failure_retry_interval)
                    .await
            };
            if !reset_result["success"].as_bool().unwrap_or(false) || !args.wait_boot {
                return reset_result;
            }

            // Wait for kernel start.
            let kernel = Self::wait_for_pattern_static(
                engine.clone(),
                r"(Starting kernel|Linux version|Booting Linux)",
                60.0,
                task,
            )
            .await;
            if kernel["matched"].as_bool().unwrap_or(false) {
                // Scope the guard tightly: a bare binding here would stay
                // alive (shadowing does not drop it) across the login wait
                // and the re-lock below, deadlocking the non-reentrant
                // engine Mutex against itself — the whole engine (read loop,
                // heartbeat, tools, human-takeover signals) starves forever.
                let reference_saved = {
                    let eng = engine.lock().await;
                    let ref_log = eng.config.reference_log();
                    if ref_log.is_empty() {
                        false
                    } else {
                        let ref_path = std::path::PathBuf::from(&ref_log);
                        let path = if ref_path.is_relative() {
                            eng.config
                                .project_dir
                                .as_ref()
                                .map(|p| p.join(&ref_path))
                                .unwrap_or(ref_path)
                        } else {
                            ref_path
                        };
                        if !path.exists()
                            && let Some(current) = eng.logs.current_path()
                        {
                            match std::fs::copy(current, &path) {
                                Err(e) => {
                                    tracing::warn!(
                                        "Failed to save reference log to {}: {e}",
                                        path.display()
                                    );
                                }
                                Ok(_) => {
                                    tracing::info!(
                                        "Auto-saved reference boot log to {} (kernel start)",
                                        path.display()
                                    );
                                }
                            }
                        }
                        true
                    }
                };
                let login =
                    Self::wait_for_pattern_static(engine.clone(), "login:", 60.0, task).await;
                let eng = engine.lock().await;
                return json!({
                    "success": true,
                    "new_boot_number": eng.logs.boot_number(),
                    "log_path": eng.log_path_string(),
                    "boot_complete": login["matched"].as_bool().unwrap_or(false),
                    "attempts": attempts,
                    "reference_saved": reference_saved,
                });
            }

            let wait = Self::wait_for_pattern_static(engine.clone(), "login:", 120.0, task).await;
            if wait["matched"].as_bool().unwrap_or(false) {
                let eng = engine.lock().await;
                return json!({
                    "success": true,
                    "new_boot_number": eng.logs.boot_number(),
                    "log_path": eng.log_path_string(),
                    "boot_complete": true,
                    "attempts": attempts,
                });
            }

            if attempts >= attempts_limit {
                let eng = engine.lock().await;
                return json!({
                    "success": true,
                    "new_boot_number": eng.logs.boot_number(),
                    "log_path": eng.log_path_string(),
                    "boot_complete": false,
                    "attempts": attempts,
                    "error": "login prompt not detected within timeout",
                });
            }
            if let Some(context) = task {
                tokio::select! {
                    () = context.cancelled() => {
                        return json!({"success": false, "cancelled": true, "error": "Task cancelled"});
                    }
                    () = tokio::time::sleep(Duration::from_secs_f64(args.failure_retry_interval)) => {}
                }
            } else {
                tokio::time::sleep(Duration::from_secs_f64(args.failure_retry_interval)).await;
            }
        }
    }

    /// Static, owned-by-engine pattern waiter used by `run_reset_sync`
    /// (which has no `&self`). Releases the lock between watcher setup and
    /// wait so the read loop can fire.
    async fn wait_for_pattern_static(
        engine: SharedEngine,
        pattern: &str,
        timeout: f64,
        task: Option<&TaskContext>,
    ) -> Value {
        let (mut rx, console_tx) = {
            let mut eng = engine.lock().await;
            let rx = eng.queue_wait_pattern(pattern);
            let tx = eng.console.write_sender();
            (rx, tx)
        };
        let wait =
            crate::serial_engine::wait_pattern_with_probe(&mut rx, timeout, true, &console_tx);
        let result = if let Some(context) = task {
            tokio::select! {
                () = context.cancelled() => {
                    json!({"matched": false, "cancelled": true})
                }
                result = wait => result,
            }
        } else {
            wait.await
        };
        {
            let mut eng = engine.lock().await;
            eng.detector.remove_watcher_by_pattern(pattern);
        }
        result
    }

    /// Instance-bound login waiter (used by `serial_power_cycle`).
    pub(super) async fn wait_for_login_without_engine_lock(&self, timeout: f64) -> Value {
        self.wait_for_pattern_without_engine_lock("login:", timeout)
            .await
    }

    async fn wait_for_pattern_without_engine_lock(&self, pattern: &str, timeout: f64) -> Value {
        let (rx, console_tx) = {
            let mut eng = self.engine.lock().await;
            let rx = eng.queue_wait_pattern(pattern);
            let tx = eng.console.write_sender();
            (rx, tx)
        };
        let mut rx = rx;
        let result =
            crate::serial_engine::wait_pattern_with_probe(&mut rx, timeout, true, &console_tx)
                .await;
        {
            let mut eng = self.engine.lock().await;
            eng.detector.remove_watcher_by_pattern(pattern);
        }
        result
    }

    /// Probe the DUT after a command timeout: send a newline, wait briefly,
    /// then check for a response — if the DUT responds we keep it Active,
    /// else downgrade.
    ///
    /// A2 regression fix: the probe must NEVER read the TCP socket directly.
    /// `console.read_available()` races the engine read loop, which usually
    /// consumes the probe response (CRLF + prompt echo) during the wait
    /// window, so the direct read comes up empty even though the target
    /// answered — and a live DUT was wrongly downgraded to DUT-off. Instead
    /// we snapshot the shared serial stream (every byte the read loop
    /// consumes is written to `logs.ring_buffer`) before sending the probe,
    /// then watch for new bytes after the snapshot. Only when the shared
    /// stream stays silent is the target declared unresponsive.
    pub(super) async fn probe_after_command_timeout(&self) -> Value {
        let (console_tx, host, port, snapshot, was_open) = {
            let eng = self.engine.lock().await;
            (
                eng.console.write_sender(),
                eng.config.dev_host_ip(),
                eng.config.serial_target(),
                eng.shared_stream_snapshot(),
                eng.console.is_open(),
            )
        };

        // No open connection → the probe cannot even be delivered. Preserve
        // the old read-error semantics without waiting out the whole window.
        if !was_open {
            self.engine
                .lock()
                .await
                .state
                .transition(TargetState::Disconnected);
            return json!({
                "state_after": "disconnected",
                "hint": format!("Command timed out and the serial connection is closed on {host}:{port}. Check ser2net or cabling."),
            });
        }

        // Deliver the probe newline through the write channel — the same path
        // every other write takes; the read loop drains it to the socket. We
        // never touch the socket ourselves.
        let _ =
            tokio::time::timeout(Duration::from_millis(200), console_tx.send(b"\n".to_vec())).await;

        // Watch the shared stream for a response. The ring buffer is
        // append-only (apart from the >2MB overflow drain), so any length
        // change means the read loop consumed new bytes.
        let deadline = tokio::time::Instant::now() + PROBE_WINDOW;
        let mut responded = false;
        while !responded && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(PROBE_POLL).await;
            let eng = self.engine.lock().await;
            responded = eng.shared_stream_has_new(snapshot);
        }

        let mut eng = self.engine.lock().await;
        if responded {
            // The read loop already fed the response bytes to the detector;
            // only a trailing partial line (prompt without newline) can still
            // sit in the detector's line buffer — flush it so a shell prompt
            // upgrades the state without waiting for the 1s watchdog flush.
            let events = eng.detector.flush_line_buf();
            let shell_like = events
                .iter()
                .any(|e| matches!(e, BootEvent::Stage(s) if crate::dut_state::is_shell_stage(s)));
            eng.state.on_activity();
            // A live DUT must never stay in DUT-off (e.g. after an earlier
            // false transition) — restore Active. Mirrors the read loop's
            // "Device resumed from DUT-off → active" recovery on any data.
            if shell_like || eng.state.current() == TargetState::DutOff {
                eng.state.transition(TargetState::Active);
            }
            json!({
                "state_after": eng.state.current().as_str(),
                "hint": format!("Command timed out on {host}:{port}, but the DUT responded to a probe. Check serial_get_state and retry when it is ready."),
            })
        } else if eng.console.is_open() {
            eng.state.transition(TargetState::DutOff);
            json!({
                "state_after": "DUT-off",
                "hint": format!("Command timed out after no probe response on {host}:{port}; state changed to DUT-off. Power on or reset the DUT."),
            })
        } else {
            eng.state.transition(TargetState::Disconnected);
            json!({
                "state_after": "disconnected",
                "hint": format!("Command timed out and the serial connection dropped on {host}:{port}. Check ser2net or cabling."),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serial_engine::new_shared_engine;
    use crate::task_manager::TaskManager;
    use tempfile::TempDir;

    /// Handler bound to an in-memory engine (mock TCP console, tempdir logs)
    /// — no hardware, no dev host, mirroring the mcp_handler test setup.
    fn make_handler(serial_port: &str) -> (McpHandler, TempDir) {
        let tmp = TempDir::new().unwrap();
        let config = crate::config::test_config(
            tmp.path(),
            serial_port,
            "0",
            Some("test-dut"),
            ".dut-serial/test-dut",
            "/tmp/sermcp-test-locks",
        );
        let engine = new_shared_engine(config);
        let tasks = TaskManager::new();
        (McpHandler::new(engine, tasks), tmp)
    }

    /// A2 regression: the probe must observe the shared ring buffer, not the
    /// raw socket. In the failing flow the read loop consumed the prompt echo
    /// (CRLF + `root@acc-dut:/# `) before the probe took the engine lock; the
    /// old direct `read_available()` then read empty and a live DUT was
    /// transitioned to DUT-off. Here the read loop is simulated by feeding
    /// the prompt bytes into the ring buffer while the probe waits.
    #[tokio::test]
    async fn probe_after_timeout_keeps_alive_dut_active_when_read_loop_consumed_prompt() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (_sock, _) = listener.accept().await.unwrap();
            // The engine read loop is not running in this test, so nothing
            // drains the probe newline from the write channel; stay silent
            // until the probe finishes.
            tokio::time::sleep(Duration::from_millis(500)).await;
        });

        let (handler, _tmp) = make_handler(&port.to_string());
        {
            let mut eng = handler.engine.lock().await;
            eng.console.connect().await.unwrap();
            eng.state.transition(TargetState::Active);
        }

        let probe = tokio::spawn({
            let handler = handler.clone();
            async move { handler.probe_after_command_timeout().await }
        });

        // Simulate the read loop consuming the probe response during the
        // probe's wait window — the prompt lands in the shared stream.
        tokio::time::sleep(Duration::from_millis(150)).await;
        {
            let mut eng = handler.engine.lock().await;
            let prompt = b"\r\nroot@acc-dut:/# ";
            eng.logs.write(prompt);
            eng.state.on_activity();
            let _ = eng.detector.feed(prompt);
        }

        let result = probe.await.unwrap();
        assert_ne!(
            result["state_after"].as_str(),
            Some("DUT-off"),
            "live DUT must not be downgraded: {result}"
        );
        {
            let eng = handler.engine.lock().await;
            assert_eq!(
                eng.state.current(),
                TargetState::Active,
                "state must stay active when the probe response went through the read loop"
            );
        }
        server.await.unwrap();
    }

    /// Truly dead target (mock ser2net accepts but never answers): no new
    /// bytes in the shared stream → DUT-off, preserving the old semantics.
    #[tokio::test]
    async fn probe_after_timeout_transitions_to_dutoff_when_target_silent() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (_sock, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_millis(1500)).await;
        });

        let (handler, _tmp) = make_handler(&port.to_string());
        {
            let mut eng = handler.engine.lock().await;
            eng.console.connect().await.unwrap();
            eng.state.transition(TargetState::Active);
        }

        let start = std::time::Instant::now();
        let result = handler.probe_after_command_timeout().await;
        let elapsed = start.elapsed();

        assert_eq!(result["state_after"].as_str(), Some("DUT-off"));
        {
            let eng = handler.engine.lock().await;
            assert_eq!(eng.state.current(), TargetState::DutOff);
        }
        assert!(
            elapsed >= Duration::from_millis(900),
            "probe must wait out the full window before declaring DUT-off: {elapsed:?}"
        );
        server.await.unwrap();
    }

    /// A live response must also restore a DUT that was previously (falsely)
    /// marked DUT-off — the false transition must not gate later flows.
    #[tokio::test]
    async fn probe_after_timeout_restores_dutoff_to_active_on_response() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (_sock, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_millis(500)).await;
        });

        let (handler, _tmp) = make_handler(&port.to_string());
        {
            let mut eng = handler.engine.lock().await;
            eng.console.connect().await.unwrap();
            eng.state.transition(TargetState::Active);
            eng.state.transition(TargetState::DutOff);
        }

        let probe = tokio::spawn({
            let handler = handler.clone();
            async move { handler.probe_after_command_timeout().await }
        });

        tokio::time::sleep(Duration::from_millis(150)).await;
        {
            let mut eng = handler.engine.lock().await;
            let prompt = b"\r\nroot@acc-dut:/# ";
            eng.logs.write(prompt);
            eng.state.on_activity();
            let _ = eng.detector.feed(prompt);
        }

        let result = probe.await.unwrap();
        assert_eq!(
            result["state_after"].as_str(),
            Some("active"),
            "live response must restore active: {result}"
        );
        {
            let eng = handler.engine.lock().await;
            assert_eq!(
                eng.state.current(),
                TargetState::Active,
                "false DUT-off must be corrected when the DUT answers the probe"
            );
        }
        server.await.unwrap();
    }

    /// Closed console → disconnected (preserves the old read-error path).
    #[tokio::test]
    async fn probe_after_timeout_reports_disconnected_when_console_closed() {
        let (handler, _tmp) = make_handler("59999"); // nothing listening
        {
            let mut eng = handler.engine.lock().await;
            eng.state.transition(TargetState::Active);
        }

        let result = handler.probe_after_command_timeout().await;

        assert_eq!(result["state_after"].as_str(), Some("disconnected"));
        {
            let eng = handler.engine.lock().await;
            assert_eq!(eng.state.current(), TargetState::Disconnected);
        }
    }
}
