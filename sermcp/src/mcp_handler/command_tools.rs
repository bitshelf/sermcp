use std::time::Duration;

use rmcp::{
    ErrorData, handler::server::wrapper::Parameters, model::CallToolResult, tool, tool_router,
};
use serde_json::{Value, json};

use super::{McpHandler, text, tool_result};
use crate::state_manager::TargetState;
use crate::tools::params::{ExecArgs, ExecMode, SendCommandArgs, SendRawArgs, UbootCommandArgs};

#[tool_router(router = command_router, vis = "pub(super)")]
impl McpHandler {
    #[tool(
        name = "serial_exec",
        description = "Send a console command to the target and return the output. mode=auto (default) picks the protocol from the detected target state: at a U-Boot prompt it waits for the prompt or an output silence gap (no POSIX shell assumed), anywhere else it uses the Linux shell marker protocol and returns exit_code. Pass mode=shell or mode=uboot explicitly when the detected state is stale.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            open_world_hint = false
        )
    )]
    pub(super) async fn exec(
        &self,
        Parameters(a): Parameters<ExecArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let use_uboot = match a.mode {
            ExecMode::Uboot => true,
            ExecMode::Shell => false,
            ExecMode::Auto => {
                let eng = self.engine.lock().await;
                eng.state.current() == TargetState::UBoot
            }
        };
        let mut value = if use_uboot {
            self.wait_uboot_output(&UbootCommandArgs {
                command: a.command,
                timeout: a.timeout,
            })
            .await
        } else {
            self.exec_shell(a.command, a.timeout).await
        };
        value["mode"] = json!(if use_uboot { "uboot" } else { "shell" });
        Ok(tool_result(value))
    }

    #[tool(
        name = "serial_send_command",
        description = "Send a shell command to the target and return the output.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            open_world_hint = false
        )
    )]
    async fn send_command(
        &self,
        Parameters(a): Parameters<SendCommandArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(text(self.exec_shell(a.command, a.timeout).await))
    }

    /// Drive `serial_send_command` / `serial_exec(mode=shell)`: POSIX shell
    /// marker-echo protocol via the command queue, with the reboot-class
    /// fast path (no marker collection — the target is about to disappear).
    async fn exec_shell(&self, command: String, timeout: i64) -> Value {
        {
            let eng = self.engine.lock().await;
            if eng.state.current() == TargetState::DutOff {
                tracing::info!("shell command on DUT-off target: attempting command anyway");
            }
        }
        if matches!(command.trim(), "reboot" | "poweroff" | "shutdown") {
            let mut eng = self.engine.lock().await;
            eng.console.sendline(&command);
            eng.console.drain_writes().await;
            return json!({
                "output": format!("{} sent", command.trim()),
                "exit_code": 0,
                "timed_out": false,
            });
        }

        let rx = self
            .engine
            .lock()
            .await
            .queue_command(&command, timeout as f64);
        match tokio::time::timeout(Duration::from_secs_f64(timeout as f64), rx).await {
            Ok(Ok(result)) => {
                let mut value = json!({
                    "output": result.output,
                    "exit_code": result.exit_code,
                    "timed_out": result.timed_out,
                    "truncated": result.truncated,
                });
                if result.timed_out {
                    self.add_timeout_hint(&mut value).await;
                }
                value
            }
            Ok(Err(_)) => json!({"error": "Command cancelled"}),
            Err(_) => {
                let mut value = json!({
                    "output": "(timeout — engine did not respond)",
                    "exit_code": Value::Null,
                    "timed_out": true,
                });
                self.add_timeout_hint(&mut value).await;
                value
            }
        }
    }

    async fn add_timeout_hint(&self, result: &mut Value) {
        let probe = self.probe_after_command_timeout().await;
        result["hint"] = probe
            .get("hint")
            .cloned()
            .unwrap_or_else(|| json!("Command timed out. Check serial_get_state."));
        if probe["state_after"].as_str() == Some("DUT-off") {
            result["state_after"] = json!("DUT-off");
        }
    }

    #[tool(
        name = "serial_send_raw",
        description = "Send raw bytes to the serial port without markers or command wrapping.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            open_world_hint = false
        )
    )]
    async fn send_raw(
        &self,
        Parameters(a): Parameters<SendRawArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(text(self.engine.lock().await.send_raw(&a.data)))
    }

    #[tool(
        name = "serial_uboot_command",
        description = "Send a raw command at the U-Boot prompt and return output. Waits until the U-Boot prompt reappears, the output goes quiet, or the timeout expires — whichever comes first. Requires no POSIX shell, so use this (not serial_send_command) while the target sits in U-Boot.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            open_world_hint = false
        )
    )]
    async fn uboot_command(
        &self,
        Parameters(a): Parameters<UbootCommandArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(tool_result(self.wait_uboot_output(&a).await))
    }

    /// Drive `serial_uboot_command`: send the command, then collect output
    /// until the U-Boot prompt reappears, the line goes quiet, or the
    /// timeout expires.
    ///
    /// U-Boot cannot run the POSIX marker-echo wrapper `queue_command`
    /// relies on (`{ cmd; }` grouping), so collection observes the shared
    /// ring buffer while the read loop — the sole socket reader — feeds it.
    /// The engine lock is therefore only held per poll tick, never across
    /// the wait.
    async fn wait_uboot_output(&self, a: &UbootCommandArgs) -> Value {
        const POLL: Duration = Duration::from_millis(100);
        // A silence gap this long after at least the echoed command means
        // the command is done — covers vendor U-Boots whose prompt string
        // the built-in prompt pattern never matches.
        const IDLE_GAP: Duration = Duration::from_secs(1);
        // Inline output cap; anything longer stays in serial.current.log
        // (serial_get_logs / serial_poll_logs can fetch it).
        const MAX_OUTPUT: usize = 16 * 1024;

        let deadline =
            tokio::time::Instant::now() + Duration::from_secs_f64((a.timeout as f64).max(1.0));
        // Commit the old prompt BEFORE the snapshot. A quiet `=> ` has no
        // newline and is otherwise held outside the ring buffer, where a
        // post-snapshot observation would mistake it for the new prompt.
        let (snapshot, delivered) = {
            let mut eng = self.engine.lock().await;
            eng.commit_ser2net_partial();
            (
                eng.shared_stream_snapshot(),
                eng.console.sendline(&a.command),
            )
        };
        if !delivered {
            return json!({
                "success": false,
                "sent": a.command,
                "output": "",
                "code": "serial_not_connected",
                "error": "serial write channel closed — target not connected",
            });
        }

        let mut tail: Vec<u8> = Vec::new();
        let mut got_bytes = false;
        let mut prompt_matched = false;
        let mut timed_out = false;
        let mut truncated = false;
        let mut last_growth = tokio::time::Instant::now();
        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                timed_out = true;
                break;
            }
            tokio::time::sleep(POLL.min(deadline - now)).await;
            let (fresh, was_truncated) = {
                let eng = self.engine.lock().await;
                eng.serial_output_since(snapshot)
            };
            truncated |= was_truncated;
            if fresh != tail {
                tail = fresh;
                got_bytes = true;
                last_growth = tokio::time::Instant::now();
                if output_ends_with_uboot_prompt(&tail) {
                    prompt_matched = true;
                    break;
                }
            } else if got_bytes && last_growth.elapsed() >= IDLE_GAP {
                break;
            }
        }

        // Final observation includes the incomplete line without draining it
        // out of the engine's detector pipeline.
        let (tail, final_truncated) = {
            let eng = self.engine.lock().await;
            eng.serial_output_since(snapshot)
        };
        truncated |= final_truncated;
        if output_ends_with_uboot_prompt(&tail) {
            prompt_matched = true;
            timed_out = false;
        }

        let mut output = crate::ansi_strip::strip_str(&String::from_utf8_lossy(&tail))
            .trim()
            .to_string();
        let inline_truncated = output.len() > MAX_OUTPUT;
        if inline_truncated {
            // Keep the tail: the command verdict sits next to the prompt.
            let mut cut = output.len() - MAX_OUTPUT;
            while !output.is_char_boundary(cut) {
                cut += 1;
            }
            output = output[cut..].to_string();
        }

        let mut result = json!({
            "success": !timed_out,
            "sent": a.command,
            "output": output,
        });
        if prompt_matched {
            result["prompt_matched"] = json!(true);
        }
        if truncated || inline_truncated {
            result["truncated"] = json!(true);
        }
        if timed_out {
            result["timed_out"] = json!(true);
            result["code"] = json!("command_timeout");
            result["error"] = json!(format!(
                "Timed out waiting for U-Boot command output after {} seconds",
                a.timeout.max(1)
            ));
        }
        result
    }
}

fn output_ends_with_uboot_prompt(data: &[u8]) -> bool {
    let clean = crate::ansi_strip::strip_str(&String::from_utf8_lossy(data));
    let Some(last) = clean.lines().next_back() else {
        return false;
    };
    matches!(last.trim(), "=>" | "U-Boot>" | "U-Boot#")
}

#[cfg(test)]
mod uboot_command_tests {
    use super::*;
    use crate::config::test_config;
    use crate::serial_engine::{SharedEngine, new_shared_engine};
    use crate::task_manager::TaskManager;
    use rmcp::model::CallToolResult;
    use serde_json::Value;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn uboot_fixture(port: u16) -> (McpHandler, tempfile::TempDir) {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = test_config(
            tmp.path(),
            &port.to_string(),
            "0",
            Some("test-dut"),
            ".dut-serial/test-dut",
            "/tmp/sermcp-test-locks",
        );
        let handler = McpHandler::new(new_shared_engine(config), TaskManager::new());
        (handler, tmp)
    }

    /// Mirror production's `ensure_background_tasks` cadence so writes drain
    /// and read bytes land in the shared ring buffer.
    fn spawn_read_driver(engine: SharedEngine) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                engine.lock().await.read_loop_iter().await;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
    }

    fn payload_of(result: CallToolResult) -> Value {
        let wire = serde_json::to_value(result).unwrap();
        serde_json::from_str(wire["content"][0]["text"].as_str().unwrap()).unwrap()
    }

    /// An old prompt already exists before the command, and the response body
    /// itself contains `=>`. Neither may finish the new command; only the final
    /// standalone prompt does.
    #[tokio::test]
    async fn collects_output_that_arrives_after_the_legacy_500ms_window() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            sock.write_all(b"=> ").await.unwrap();
            // Consume lines (startup probes included) until the command.
            let mut line = Vec::new();
            loop {
                let mut byte = [0u8; 1];
                sock.read_exact(&mut byte).await.unwrap();
                if byte[0] == b'\n' {
                    if String::from_utf8_lossy(&line).contains("version") {
                        break;
                    }
                    line.clear();
                } else {
                    line.push(byte[0]);
                }
            }
            tokio::time::sleep(Duration::from_millis(1200)).await;
            sock.write_all(b"\r\nvalue=a=>b\r\nU-Boot 2024.07\r\n")
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(600)).await;
            sock.write_all(b"=> ").await.unwrap();
        });

        let (handler, _tmp) = uboot_fixture(port);
        handler.engine.lock().await.start().await.unwrap();
        let driver = spawn_read_driver(handler.engine.clone());

        let started = tokio::time::Instant::now();
        let result = handler
            .uboot_command(Parameters(UbootCommandArgs {
                command: "version".into(),
                timeout: 8,
            }))
            .await
            .unwrap();
        let elapsed = started.elapsed();

        driver.abort();
        server.await.unwrap();
        handler.engine.lock().await.stop().await;

        assert_eq!(result.is_error, Some(false));
        let payload = payload_of(result);
        assert!(
            payload["output"]
                .as_str()
                .unwrap()
                .contains("U-Boot 2024.07"),
            "output must survive the legacy window: {payload}"
        );
        assert_eq!(payload["prompt_matched"], json!(true));
        assert!(
            elapsed >= Duration::from_millis(1800),
            "old/mid-output prompts must not finish the command, took {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(8),
            "prompt match must end the wait before the deadline, took {elapsed:?}"
        );
    }

    /// A target that never answers must hold the tool until the requested
    /// deadline (the old code returned after 500ms regardless of `timeout`).
    #[tokio::test]
    async fn silent_target_reports_timed_out_at_the_requested_deadline() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (_sock, _) = listener.accept().await.unwrap();
            // Hold the connection open without ever answering.
            tokio::time::sleep(Duration::from_secs(30)).await;
        });

        let (handler, _tmp) = uboot_fixture(port);
        handler.engine.lock().await.start().await.unwrap();
        let driver = spawn_read_driver(handler.engine.clone());

        let started = tokio::time::Instant::now();
        let result = handler
            .uboot_command(Parameters(UbootCommandArgs {
                command: "version".into(),
                timeout: 2,
            }))
            .await
            .unwrap();
        let elapsed = started.elapsed();

        driver.abort();
        server.abort();
        handler.engine.lock().await.stop().await;

        assert_eq!(result.is_error, Some(true));
        let payload = payload_of(result);
        assert_eq!(payload["success"], json!(false));
        assert_eq!(payload["timed_out"], json!(true));
        assert_eq!(payload["output"], json!(""));
        assert!(
            elapsed >= Duration::from_secs(2),
            "deadline must be respected, took {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(6),
            "must not wait far past the deadline, took {elapsed:?}"
        );
    }

    #[test]
    fn prompt_match_requires_a_standalone_final_line() {
        assert!(!output_ends_with_uboot_prompt(b"value=a=>b\r\n"));
        assert!(!output_ends_with_uboot_prompt(
            b"the log mentions => mid-line"
        ));
        assert!(output_ends_with_uboot_prompt(b"value=a=>b\r\n=> "));
        assert!(output_ends_with_uboot_prompt(b"U-Boot# "));
    }
}
