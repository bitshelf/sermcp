use rmcp::{
    ErrorData, handler::server::wrapper::Parameters, model::CallToolResult, tool, tool_router,
};
use serde_json::json;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::{McpHandler, text, text_error, tool_result};
use crate::tools::params::{GetLogsArgs, NoArgs, PollLogsArgs};

#[tool_router(router = query_router, vis = "pub(super)")]
impl McpHandler {
    #[tool(
        name = "serial_get_state",
        description = "Get the current target state and metadata.",
        output_schema = rmcp::handler::server::common::schema_for_output::<
            crate::tools::params::StateOutput,
        >(),
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    pub(super) async fn get_state(
        &self,
        _p: Parameters<NoArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        if self.engine.flags.dutabo_taken.load(Ordering::Relaxed) != 0 {
            let mut state = self.connecting_state.clone();
            state["state"] = json!("dutabo");
            state.as_object_mut().unwrap().remove("probe");
            return Ok(super::text_structured(self.with_session_display(state)));
        }
        // Fast path: engine mutex free → answer immediately.
        let eng = if let Ok(eng) = self.engine.try_lock() {
            eng
        } else if self
            .engine
            .flags
            .startup_in_progress
            .load(Ordering::Relaxed)
        {
            // Genuine startup window: report the connecting snapshot without
            // queueing behind the probe (discovery stays responsive).
            return Ok(super::text_structured(
                self.with_session_display(self.connecting_state.clone()),
            ));
        } else {
            // Mutex held by the read loop (~100ms idles) or another tool —
            // queue briefly for the real state instead of misreporting a
            // healthy engine as "connecting" (the pre-flags try_lock
            // fallback did exactly that ~91% of every idle cycle).
            self.engine.lock().await
        };
        let state = self.with_session_display(eng.get_state_dict());
        if eng.has_serial_error() {
            Ok(text_error(state))
        } else {
            Ok(super::text_structured(state))
        }
    }

    #[tool(
        name = "serial_get_config",
        description = "Get current target configuration.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    pub(super) async fn get_config(
        &self,
        _p: Parameters<NoArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let eng = self.engine.lock().await;
        Ok(text(eng.get_config()))
    }

    #[tool(
        name = "serial_list_duts",
        description = "List all DUTs configured in .target.jsonc, grouped by dev host, with each DUT's current serial state and state-file age. Read-only. Call this BEFORE destructive operations (reset, reboot, flash, download, power cycle) to confirm the target name and state. Pass that name as dut on subsequent tool calls.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    pub(super) async fn list_duts(
        &self,
        _p: Parameters<NoArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        // Discovery must not wait behind any board's startup or command.
        let config = if let Some(config) = self
            .registry
            .as_ref()
            .and_then(|registry| registry.inventory_config.clone())
        {
            config
        } else {
            self.engine.lock().await.config.clone()
        };
        let current_dut_name = std::env::var("TARGET_DUT_NAME")
            .ok()
            .filter(|name| !name.trim().is_empty());
        let written_at_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs() as i64)
            .unwrap_or(0);
        match super::dut_inventory::list_duts_payload(
            &config,
            current_dut_name.as_deref(),
            written_at_unix,
        ) {
            Ok(mut value) => {
                if let Some(registry) = &self.registry {
                    value["managed_duts"] = json!(registry.names());
                    value["current_dut"] = if registry.is_multi() {
                        serde_json::Value::Null
                    } else {
                        json!(registry.names()[0])
                    };
                    if let Some(rows) = value["duts"].as_array_mut() {
                        for row in rows {
                            let Some(dut) = row["dut_name"]
                                .as_str()
                                .and_then(|name| registry.entries().get(name))
                            else {
                                row["managed"] = json!(false);
                                continue;
                            };
                            row["managed"] = json!(true);
                            if dut.engine.flags.dutabo_taken.load(Ordering::Relaxed) != 0 {
                                row["state"] = json!("dutabo");
                            } else if dut.engine.flags.startup_in_progress.load(Ordering::Relaxed) {
                                row["state"] = json!("connecting");
                            } else if let Ok(engine) = dut.engine.try_lock() {
                                let state = engine.get_state_dict();
                                row["state"] = state["state"].clone();
                                row["error"] = state["error"].clone();
                            }
                            if let Some(state) = row["state"].as_str().map(str::to_owned) {
                                row["state_label"] = json!(crate::dut_state::state_label(&state));
                                row["critical"] = json!(crate::dut_state::is_critical(&state));
                            }
                        }
                    }
                }
                Ok(text(value))
            }
            Err(message) => {
                // Config parse errors surface as tool errors — the tool must
                // never invent a single-DUT registry from a broken file.
                Ok(text_error(json!({"success": false, "error": message})))
            }
        }
    }

    #[tool(
        name = "serial_get_metrics",
        description = "Get engine metrics: uptime, command count, error count, pending commands.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn get_metrics(&self, _p: Parameters<NoArgs>) -> Result<CallToolResult, ErrorData> {
        let eng = self.engine.lock().await;
        let (p50, p95, p99) = eng.state.latency_percentiles();
        Ok(text(json!({
            "uptime_secs": eng.state.uptime_secs(),
            "command_count": eng.state.command_count(),
            "error_count": eng.state.error_count(),
            "pending_commands": eng.commands.pending_len(),
            "completed": eng.commands.completed_count,
            "cmd_errors": eng.commands.error_count,
            "latency_ms": { "p50": p50, "p95": p95, "p99": p99 },
        })))
    }

    #[tool(
        name = "serial_list_logs",
        description = "List all archived boot logs.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn list_logs(&self, _p: Parameters<NoArgs>) -> Result<CallToolResult, ErrorData> {
        let eng = self.engine.lock().await;
        Ok(text(eng.list_logs()))
    }

    #[tool(
        name = "serial_get_logs",
        description = "Retrieve serial log content with optional filtering.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn get_logs(
        &self,
        Parameters(a): Parameters<GetLogsArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        // Snapshot only the selected path under the engine mutex. Scanning a
        // large log and applying regex/highlighting are blocking work and must
        // not starve serial_get_state/metrics or the serial read loop.
        // archive=0 is the LIVE current cycle (the documented contract);
        // >=1 indexes boot archives newest-first. A fresh server has no
        // archives yet, but its live log must still be readable.
        enum Source {
            Live,
            Archive(usize),
        }
        let source = match a.archive {
            0 => Source::Live,
            n => Source::Archive(n as usize - 1),
        };
        let target = {
            let eng = self.engine.lock().await;
            match source {
                Source::Live => eng.logs.live_read_target(),
                Source::Archive(index) => eng.logs.read_target(index),
            }
        };
        let highlighted = a.highlighted;
        let lines = a.lines as usize;
        let pattern = a.pattern;
        let value = match tokio::task::spawn_blocking(move || {
            let Some(target) = target else {
                return json!({
                    "content": "",
                    "filename": "",
                    "total_lines": 0,
                    "filtered_lines": 0,
                });
            };
            let result = crate::log_manager::LogManager::read_target_content(
                target,
                lines,
                pattern.as_deref(),
            );
            let content = if highlighted {
                let mut rendered = Vec::with_capacity(result.content.len() * 2);
                crate::highlight::highlight_serial_prompt(result.content.as_bytes(), &mut rendered);
                String::from_utf8_lossy(&rendered).into_owned()
            } else {
                result.content
            };
            json!({
                "content": content,
                "filename": result.filename,
                "total_lines": result.total_lines,
                "filtered_lines": result.filtered_lines,
            })
        })
        .await
        {
            Ok(value) => value,
            Err(error) => {
                return Ok(text_error(json!({
                    "success": false,
                    "error": format!("serial log reader task failed: {error}"),
                })));
            }
        };
        Ok(text(value))
    }

    #[tool(
        name = "serial_poll_logs",
        description = "Get a bounded page of new serial output. Follow the returned since cursor immediately when more is true. An empty poll waits for output up to timeout seconds.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    pub(super) async fn poll_logs(
        &self,
        Parameters(a): Parameters<PollLogsArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        // Explicit offsets read the raw log independently of the shared
        // cursor, which follows the filtered log for legacy callers.
        let cursor: Option<u64> = a.since.filter(|s| s.is_finite()).map(|s| s.max(0.0) as u64);
        let timeout = a.timeout.max(0) as f64;
        let deadline = tokio::time::Instant::now() + Duration::from_secs_f64(timeout);
        loop {
            let (path, shared_cursor) = {
                let eng = self.engine.lock().await;
                match cursor {
                    Some(_) => (eng.logs.current_path().map(std::path::Path::to_owned), None),
                    None => (
                        eng.logs.live_read_target().map(|target| target.path),
                        Some(eng.log_poll.clone()),
                    ),
                }
            };
            let shared_cursor = match shared_cursor {
                Some(cursor) => Some(cursor.lock_owned().await),
                None => None,
            };
            let result = tokio::task::spawn_blocking(move || match shared_cursor {
                Some(mut position) => position.read_page(path.as_deref()),
                None => crate::serial_engine::LogPollCursor::at(cursor.unwrap_or_default())
                    .read_page(path.as_deref()),
            })
            .await
            .map_err(|error| {
                ErrorData::internal_error(format!("serial log reader failed: {error}"), None)
            })?;
            let has_new = result["lines"]
                .as_array()
                .is_some_and(|lines| !lines.is_empty());
            if has_new || result.get("error").is_some() || tokio::time::Instant::now() >= deadline {
                return Ok(text(result));
            }
            tokio::time::sleep_until(
                deadline.min(tokio::time::Instant::now() + Duration::from_millis(200)),
            )
            .await;
        }
    }

    #[tool(
        name = "serial_claim",
        description = "Claim serial ownership for this session and reconnect. Never steals the lock from another live MCP session.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn claim(&self, _p: Parameters<NoArgs>) -> Result<CallToolResult, ErrorData> {
        let result = self.engine.lock().await.claim_serial().await;
        if result["success"] == true {
            crate::serial_engine::ensure_background_tasks(self.engine.clone()).await;
        }
        // Gap #7: a refused claim (lock held by another live session) is a
        // cause-visible failure → isError:true.
        Ok(tool_result(result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::serial_engine::new_shared_engine;
    use crate::task_manager::TaskManager;
    use std::io::Write;
    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn handler_config(tmp: &TempDir, port: u16) -> Config {
        crate::config::test_config(
            tmp.path(),
            &port.to_string(),
            "0",
            None,
            ".dut-serial/test-dut",
            &tmp.path().join("locks").to_string_lossy(),
        )
    }

    fn write_large_archive(log_dir: &std::path::Path, name: &str) {
        std::fs::create_dir_all(log_dir).unwrap();
        let mut file = std::fs::File::create(log_dir.join(name)).unwrap();
        for index in 0..12_000 {
            writeln!(file, "boot log line {index:05} {}", "x".repeat(32)).unwrap();
        }
        assert!(file.metadata().unwrap().len() >= 600 * 1024);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn claim_raw_then_large_rolled_log_returns_bounded_with_state_and_metrics() {
        let tmp = TempDir::new().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = [0_u8; 64];
            let n = socket.read(&mut bytes).await.unwrap();
            assert!(n > 0, "claim probe must write a newline");
            socket.write_all(b"root@test:~# ").await.unwrap();
            tokio::time::sleep(Duration::from_secs(1)).await;
        });

        let engine = new_shared_engine(handler_config(&tmp, port));
        let handler = McpHandler::new(engine.clone(), TaskManager::new());
        let claim = tokio::time::timeout(Duration::from_secs(2), async {
            engine.lock().await.claim_serial().await
        })
        .await
        .expect("claim must be bounded");
        assert_eq!(claim["success"], true);
        let raw = engine.lock().await.send_raw("\n");
        assert_eq!(raw["success"], true);

        let log_dir = engine.lock().await.logs.log_dir().to_path_buf();
        // Older archive plus a large newest archive models a completed boot
        // rollover without mutating the selected path during the read.
        std::fs::create_dir_all(&log_dir).unwrap();
        std::fs::write(log_dir.join("boot-001_20260821_000000.log"), b"old boot\n").unwrap();
        write_large_archive(&log_dir, "boot-002_20260821_000100.log");

        let logs = handler.clone();
        let logs_task = tokio::spawn(async move {
            logs.get_logs(Parameters(GetLogsArgs {
                lines: 120,
                pattern: None,
                archive: 1,
                highlighted: false,
            }))
            .await
        });
        let state = handler.get_state(Parameters(NoArgs::default()));
        let metrics = handler.get_metrics(Parameters(NoArgs::default()));
        let (state, metrics) = tokio::time::timeout(Duration::from_millis(250), async {
            tokio::join!(state, metrics)
        })
        .await
        .expect("state and metrics must not starve behind a large log scan");
        assert!(state.is_ok());
        assert!(metrics.is_ok());
        let result = tokio::time::timeout(Duration::from_secs(2), logs_task)
            .await
            .expect("get_logs must be bounded")
            .unwrap()
            .unwrap();
        let response = serde_json::to_value(result).unwrap();
        let log: serde_json::Value =
            serde_json::from_str(response["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(log["total_lines"], 12_000);
        let content = log["content"].as_str().unwrap();
        assert_eq!(content.lines().count(), 120);
        assert!(content.starts_with("boot log line 11880 "));
        assert!(!content.contains("old boot"));

        server.abort();
        let _ = server.await;
    }
}
