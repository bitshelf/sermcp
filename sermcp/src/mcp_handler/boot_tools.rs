use std::time::Duration;

use rmcp::{
    ErrorData, handler::server::wrapper::Parameters, model::CallToolResult, tool, tool_router,
};
use serde_json::json;

use super::{McpHandler, text, text_error, tool_result};
use crate::serial_engine::InterruptStrategy;
use crate::state_manager::TargetState;
use crate::tools::params::*;

struct PatternInterrupt<'a> {
    receiver: &'a mut tokio::sync::mpsc::UnboundedReceiver<crate::boot_detector::WatcherMatch>,
    pattern: &'a str,
}

fn software_restart_pattern() -> String {
    let loader = crate::loader::default();
    format!(
        "(?:{})|(?:{})|(?:{})|(?im:^(?:.*reboot: Restarting system|INFO:[ \\t]+system reset))",
        loader.spl_pattern(),
        loader.banner_or_prompt_pattern(),
        loader.autoboot_pattern(),
    )
}

const VERSION_RESPONSE_PATTERN: &str = r"(?m)^U-Boot[ \t]+(?:[0-9]|v[0-9])[^\r\n]*";

fn resolve_interrupt_pattern(
    engine: &crate::serial_engine::SerialEngine,
    strategy: InterruptStrategy,
    override_pattern: Option<&str>,
) -> Result<Option<String>, String> {
    if strategy != InterruptStrategy::Pattern {
        return Ok(None);
    }
    let pattern = override_pattern
        .unwrap_or_else(|| engine.config.get("UBOOT_PATTERN"))
        .trim();
    if pattern.is_empty() {
        return Err("pattern strategy requires uboot.pattern or interrupt_pattern".into());
    }
    regex::Regex::new(pattern)
        .map_err(|error| format!("invalid interrupt pattern regex {pattern:?}: {error}"))?;
    Ok(Some(pattern.to_string()))
}

fn resolve_uboot_restart(requested: UbootRestart, reset_configured: bool) -> UbootRestart {
    match requested {
        UbootRestart::Auto if reset_configured => UbootRestart::Hardware,
        UbootRestart::Auto => UbootRestart::Software,
        restart => restart,
    }
}

#[tool_router(router = boot_router, vis = "pub(super)")]
impl McpHandler {
    // ── power / reset / boot entry ────────────────────────────────────

    #[tool(
        name = "serial_power_cycle",
        description = "Power cycle the target via relay: OFF → wait (≥3s) → ON. Requires power_ch configured in .target.jsonc [dut.relay].",
        annotations(
            read_only_hint = false,
            // Gap #9: power-cycling interrupts whatever the target is doing.
            destructive_hint = true,
            open_world_hint = false
        )
    )]
    async fn power_cycle(
        &self,
        Parameters(a): Parameters<PowerCycleArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let power_result =
            crate::serial_engine::pulse_shared(&self.engine, dut_ctrl::Button::Power).await;
        if !power_result["success"].as_bool().unwrap_or(false) {
            // Gap #7: relay/backend failure is a cause-visible failure.
            return Ok(tool_result(power_result));
        }
        if !a.wait_boot {
            return Ok(text(power_result));
        }
        let wait = self.wait_for_login_without_engine_lock(120.0).await;
        let eng = self.engine.lock().await;
        if wait["matched"].as_bool().unwrap_or(false) {
            Ok(text(json!({
                "success": true,
                "new_boot_number": eng.logs.boot_number(),
                "log_path": eng.log_path_string(),
                "boot_complete": true,
            })))
        } else {
            Ok(text_error(json!({
                "success": false,
                "error": "Boot did not complete within timeout",
                "new_boot_number": eng.logs.boot_number(),
            })))
        }
    }

    // The router registration is required so `tools/list` advertises the
    // tool and its schema. `call_tool` intercepts normal wire calls before
    // this fallback to materialize a Task when the request supports it.
    #[tool(
        name = "serial_reset",
        description = "Reset the target via relay. With wait_boot=true, task-capable clients receive an asynchronous MCP Task that can be polled or cancelled.",
        annotations(
            read_only_hint = false,
            // Gap #9: a relay reset physically restarts the target.
            destructive_hint = true,
            open_world_hint = false
        )
    )]
    async fn reset(
        &self,
        Parameters(a): Parameters<ResetArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(tool_result(self.serial_reset_sync(&a).await))
    }

    #[tool(
        name = "serial_enter_uboot",
        description = "Enter the U-Boot prompt with restart=auto|hardware|software|none. auto uses reset control when configured and otherwise sends reboot through the serial shell. flood continuously sends UBOOT_INTERRUPT_CHAR; pattern waits for uboot.pattern/interrupt_pattern and sends that byte once. Retries on timeout.",
        annotations(
            read_only_hint = false,
            // Restarting or interrupting autoboot aborts the running OS.
            destructive_hint = true,
            open_world_hint = false
        )
    )]
    async fn enter_uboot(
        &self,
        Parameters(a): Parameters<EnterUbootArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let failure_retry = a.failure_retry.max(1) as usize;
        let interrupt_override = match a.interrupt_char.as_deref() {
            Some(value) => match crate::config::parse_interrupt_char_checked(value) {
                Ok(ch) => Some(ch),
                Err(error) => {
                    return Ok(text_error(json!({
                        "success": false,
                        "code": "invalid_interrupt_char",
                        "error": error,
                    })));
                }
            },
            None => None,
        };

        let (strategy, pattern, restart) = {
            let engine = self.engine.lock().await;
            // Software restart types `reboot` on the console and hardware
            // restart pulses the relay — neither may run under a human's
            // interactive session.
            if engine.in_manual_takeover() {
                return Ok(text_error(json!({
                    "success": false,
                    "code": "dutabo_session",
                    "error": "Serial console owned by interactive dutabo session; end it first",
                })));
            }
            let strategy = match engine.resolve_interrupt_strategy(a.interrupt_strategy.as_deref())
            {
                Ok(strategy) => strategy,
                Err(error) => {
                    return Ok(text_error(json!({
                        "success": false,
                        "code": "invalid_interrupt_strategy",
                        "error": error,
                    })));
                }
            };
            let pattern = match resolve_interrupt_pattern(
                &engine,
                strategy,
                a.interrupt_pattern.as_deref(),
            ) {
                Ok(pattern) => pattern,
                Err(error) => {
                    return Ok(text_error(json!({
                        "success": false,
                        "code": "invalid_interrupt_pattern",
                        "error": error,
                    })));
                }
            };
            let restart = resolve_uboot_restart(a.restart, engine.reset_control_configured());
            (strategy, pattern, restart)
        };

        for attempt in 1..=failure_retry {
            let original_interrupt_char = {
                let mut engine = self.engine.lock().await;
                let original = engine.interrupt_char;
                if let Some(ch) = interrupt_override {
                    engine.interrupt_char = ch;
                }
                original
            };

            // Warm reboots may omit SPL and the version banner. Start the
            // interrupt as soon as firmware reports the reset boundary.
            if restart == UbootRestart::Software {
                let restart_pattern = software_restart_pattern();
                let (mut boot_rx, delivered) = {
                    let mut engine = self.engine.lock().await;
                    if engine.in_manual_takeover() {
                        engine.interrupt_char = original_interrupt_char;
                        return Ok(text_error(
                            crate::serial_engine::SerialEngine::dutabo_session_error(),
                        ));
                    }
                    engine.state.transition(TargetState::Booting);
                    let delivered = engine.console.sendline("reboot");
                    engine.console.drain_writes().await;
                    engine.logs.expect_boot();
                    engine.detector.reset_cycle();
                    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                    engine.detector.add_watcher(&restart_pattern, tx);
                    (rx, delivered)
                };
                if !delivered {
                    let mut engine = self.engine.lock().await;
                    engine.interrupt_char = original_interrupt_char;
                    engine.detector.remove_watcher_by_pattern(&restart_pattern);
                    return Ok(text_error(json!({
                        "success": false,
                        "code": "serial_not_connected",
                        "error": "serial write channel closed — target not connected",
                        "restart": restart.as_str(),
                    })));
                }
                let boot_observed = tokio::time::timeout(Duration::from_secs(30), boot_rx.recv())
                    .await
                    .is_ok_and(|event| event.is_some());
                let mut engine = self.engine.lock().await;
                engine.detector.remove_watcher_by_pattern(&restart_pattern);
                if !boot_observed {
                    engine.interrupt_char = original_interrupt_char;
                    return Ok(text_error(json!({
                        "success": false,
                        "code": "restart_not_observed",
                        "error": "No firmware restart boundary observed after the software reboot",
                        "restart": restart.as_str(),
                    })));
                }
            }

            let (mut rx, mut pattern_rx, original_interrupt_char) = {
                let mut engine = self.engine.lock().await;
                let pattern_rx = pattern
                    .as_deref()
                    .map(|pattern| engine.queue_wait_pattern(pattern));
                let rx = engine.queue_enter_uboot();
                if restart == UbootRestart::Hardware
                    && let Err(e) = engine.do_relay_reset(strategy).await
                {
                    engine.interrupt_char = original_interrupt_char;
                    engine
                        .detector
                        .remove_watcher_by_pattern(crate::loader::default().prompt_watch_pattern());
                    if let Some(pattern) = pattern.as_deref() {
                        engine.detector.remove_watcher_by_pattern(pattern);
                    }
                    let err = json!({ "success": false, "error": e });
                    return Ok(text_error(err));
                }
                (rx, pattern_rx, original_interrupt_char)
            };

            let duration_ms = (a.flood_duration_secs * 1000.0) as u64;
            let pattern_interrupt = pattern_rx
                .as_mut()
                .zip(pattern.as_deref())
                .map(|(receiver, pattern)| PatternInterrupt { receiver, pattern });
            let mut result = self
                .finish_uboot_attempt(
                    &mut rx,
                    duration_ms,
                    a.flood_interval_ms as u64,
                    attempt,
                    strategy,
                    pattern_interrupt,
                )
                .await;
            self.engine.lock().await.interrupt_char = original_interrupt_char;
            if result["success"].as_bool().unwrap_or(false) {
                result["restart"] = json!(restart.as_str());
                result["relay_reset_attempted"] = json!(restart == UbootRestart::Hardware);
                result["strategy"] = json!(strategy.as_str());
                return Ok(text(result));
            }

            if attempt < failure_retry {
                tokio::time::sleep(Duration::from_secs_f64(a.failure_retry_interval)).await;
            }
        }
        let state_after = self
            .engine
            .lock()
            .await
            .state
            .current()
            .as_str()
            .to_string();
        Ok(text_error(json!({
            "success": false,
            "state_after": state_after,
            "error": format!("Timed out waiting for U-Boot prompt after {failure_retry} attempts"),
            "attempts": failure_retry,
            "restart": restart.as_str(),
        })))
    }

    #[tool(
        name = "serial_wait_pattern",
        description = "Wait until a regex pattern appears in serial output. It may send the configured interrupt byte when an action is requested.",
        annotations(
            read_only_hint = false,
            // Most-dangerous-path rule: action=send_interrupt transmits the
            // configured interrupt byte, which can abort autoboot/whatever is
            // running — the passive wait is the common path, not the worst.
            destructive_hint = true,
            open_world_hint = false
        )
    )]
    async fn wait_pattern(
        &self,
        Parameters(a): Parameters<WaitPatternArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        // `action="send_interrupt"` sends the configured UBOOT_INTERRUPT_CHAR
        // (default 0x03 / Ctrl-C) once the pattern matches. The alias
        let send_interrupt = a.action.as_deref().is_some_and(|s| s == "send_interrupt");
        let probe_on_timeout = a.pattern.contains("login")
            || a.pattern.contains("prompt")
            || a.pattern.contains("=>")
            || a.pattern.contains(r"\$")
            || a.pattern.contains('#');

        let console_tx = {
            let engine = self.engine.lock().await;
            // Watching is passive, but the timeout probe and the
            // send_interrupt action both transmit bytes; the detector feed
            // is skipped during a takeover anyway, so reject up front
            // instead of letting the caller wait out a guaranteed timeout.
            if engine.in_manual_takeover() {
                return Ok(text_error(json!({
                    "success": false,
                    "code": "dutabo_session",
                    "error": "Serial console owned by interactive dutabo session; end it first",
                })));
            }
            engine.console.write_sender()
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        {
            let mut engine = self.engine.lock().await;
            engine.detector.add_watcher(&a.pattern, tx);
        }
        let result = crate::serial_engine::wait_pattern_with_probe(
            &mut rx,
            a.timeout as f64,
            probe_on_timeout,
            &console_tx,
        )
        .await;
        {
            let mut engine = self.engine.lock().await;
            engine.detector.remove_watcher_by_pattern(&a.pattern);
        }
        let result = if send_interrupt && result["matched"].as_bool().unwrap_or(false) {
            let mut engine = self.engine.lock().await;
            if engine.in_manual_takeover() {
                return Ok(text_error(
                    crate::serial_engine::SerialEngine::dutabo_session_error(),
                ));
            }
            // Send the configured interrupt byte (UBOOT_INTERRUPT_CHAR), not a
            // hard-coded Ctrl-C — some boards use a different byte to abort.
            let byte = engine.interrupt_char;
            engine.console.write_raw(&[byte]).await;
            json!({"matched": true, "matched_line": null})
        } else {
            result
        };
        Ok(text(result))
    }

    /// One interrupt attempt: flood/pattern for `duration_ms`, then a short
    /// grace wait for the prompt. Its own span (attempt N) so the chrome
    /// trace shows each retry separately.
    #[tracing::instrument(
        skip(self, rx, pattern_interrupt),
        fields(attempt, duration_ms, interval_ms)
    )]
    async fn finish_uboot_attempt(
        &self,
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<crate::boot_detector::WatcherMatch>,
        duration_ms: u64,
        interval_ms: u64,
        attempt: usize,
        strategy: InterruptStrategy,
        mut pattern_interrupt: Option<PatternInterrupt<'_>>,
    ) -> serde_json::Value {
        // Declared instrument fields must be recorded too — an unrecorded
        // field renders empty in the chrome trace, defeating the
        // per-attempt separation this span exists for.
        let span = tracing::Span::current();
        span.record("attempt", attempt);
        span.record("duration_ms", duration_ms);
        span.record("interval_ms", interval_ms);
        let rounds = duration_ms / interval_ms.max(1);
        let mut matched = false;
        let mut interrupted = false;
        for _ in 0..rounds {
            if rx.try_recv().is_ok() {
                matched = true;
                break;
            }
            match strategy {
                InterruptStrategy::Flood => {
                    self.engine.lock().await.flood_one().await;
                }
                InterruptStrategy::Pattern => {
                    if !interrupted
                        && pattern_interrupt
                            .as_mut()
                            .is_some_and(|watch| watch.receiver.try_recv().is_ok())
                    {
                        let mut engine = self.engine.lock().await;
                        if engine.in_manual_takeover() {
                            break;
                        }
                        let ch = engine.interrupt_char;
                        engine.console.write_raw(&[ch]).await;
                        interrupted = true;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(interval_ms)).await;
        }
        if !matched {
            matched = tokio::time::timeout(Duration::from_secs(3), rx.recv())
                .await
                .is_ok_and(|event| event.is_some());
        }

        // A Linux shell also echoes arbitrary nonces. Require the version
        // command's U-Boot banner when the vendor uses a custom prompt.
        if !matched {
            let mut version_rx = {
                let mut engine = self.engine.lock().await;
                let rx = engine.queue_wait_pattern(VERSION_RESPONSE_PATTERN);
                engine.send_raw("version\n");
                rx
            };
            matched = tokio::time::timeout(Duration::from_secs(2), version_rx.recv())
                .await
                .is_ok_and(|event| event.is_some());
            self.engine
                .lock()
                .await
                .detector
                .remove_watcher_by_pattern(VERSION_RESPONSE_PATTERN);
        }

        let mut engine = self.engine.lock().await;
        engine
            .detector
            .remove_watcher_by_pattern(crate::loader::default().prompt_watch_pattern());
        if let Some(watch) = pattern_interrupt {
            engine.detector.remove_watcher_by_pattern(watch.pattern);
        }
        if engine.in_manual_takeover() {
            return crate::serial_engine::SerialEngine::dutabo_session_error();
        }
        if matched {
            engine.state.transition(TargetState::UBoot);
            json!({
                "success": true,
                "state_after": "uboot",
                "attempts": attempt,
                "strategy": strategy.as_str(),
                "pattern_matched": if strategy == InterruptStrategy::Pattern { Some(interrupted) } else { None },
            })
        } else {
            json!({
                "success": false,
                "state_after": engine.state.current().as_str(),
                "error": "Timed out waiting for U-Boot prompt",
                "attempts": attempt,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn software_restart_matches_warm_reset_without_spl() {
        let pattern = regex::Regex::new(&software_restart_pattern()).unwrap();
        for line in [
            "INFO:    system reset......",
            "reboot: Restarting system",
            "U-Boot SPL board init",
            "U-Boot v5-gd439f2ec09df",
            "Hit key to stop autoboot('CTRL+C'):  0",
        ] {
            assert!(pattern.is_match(line), "{line}");
        }
        assert!(!pattern.is_match("root@debian:/# reboot"));
        assert!(!pattern.is_match("Stopping systemd-reboot.service"));
    }

    #[test]
    fn version_probe_rejects_linux_shell_echo() {
        let pattern = regex::Regex::new(VERSION_RESPONSE_PATTERN).unwrap();
        for line in [
            "root@debian:/# echo uboot_probe_12345678",
            "uboot_probe_12345678",
            "root@debian:/# version",
            "bash: version: command not found",
            "U-Boot SPL 2024.01",
        ] {
            assert!(!pattern.is_match(line), "{line}");
        }
        assert!(pattern.is_match("U-Boot 2024.07 (test build)"));
        assert!(pattern.is_match("U-Boot v5-gd439f2ec09df (test build)"));
    }

    async fn simulated_entry(restart: &str, version_reply: &[u8]) -> serde_json::Value {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port().to_string();
        let reply = version_reply.to_vec();
        let simulator = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut command = Vec::new();
            let mut buf = [0; 256];
            loop {
                let count = stream.read(&mut buf).await.unwrap();
                if count == 0 {
                    break;
                }
                for byte in &buf[..count] {
                    if *byte == 3 {
                        continue;
                    }
                    if *byte == b'\n' {
                        match command.as_slice() {
                            b"reboot" => stream
                                .write_all(b"INFO:    system reset......\r\n")
                                .await
                                .unwrap(),
                            b"version" => stream.write_all(&reply).await.unwrap(),
                            _ => {}
                        }
                        command.clear();
                    } else {
                        command.push(*byte);
                    }
                }
            }
        });
        let tmp = tempfile::tempdir().unwrap();
        let config = crate::config::test_config(
            tmp.path(),
            &port,
            "0",
            Some("test"),
            ".dut-serial/test",
            "/tmp/sermcp-test-locks",
        );
        let engine = crate::serial_engine::new_shared_engine(config);
        engine.lock().await.console.connect().await.unwrap();
        let reader_engine = engine.clone();
        let reader = tokio::spawn(async move {
            loop {
                {
                    let mut engine = reader_engine.lock().await;
                    engine.console.drain_writes().await;
                    if let Ok(data) = engine
                        .console
                        .read_available(Duration::from_millis(10), 4096)
                        .await
                    {
                        engine.detector.feed(&data);
                    }
                }
                tokio::task::yield_now().await;
            }
        });
        let handler = McpHandler::new(engine.clone(), crate::task_manager::TaskManager::new());
        let args = serde_json::from_value(json!({
            "restart":restart, "failure_retry":1,
            "flood_duration_secs":0.1, "flood_interval_ms":10
        }))
        .unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(8),
            handler.enter_uboot(Parameters(args)),
        )
        .await;
        reader.abort();
        let _ = reader.await;
        engine.lock().await.console.close();
        simulator.abort();
        let _ = simulator.await;
        let result = result
            .expect("entry must handle a warm reset without waiting for SPL")
            .unwrap();
        let result = serde_json::to_value(result).unwrap();
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn warm_restart_without_spl_confirms_vendor_uboot_by_version() {
        let result = simulated_entry(
            "software",
            b"version\r\nU-Boot v5-test (test build)\r\nMYD-YR3588# ",
        )
        .await;
        assert_eq!(result["success"], true, "{result}");
        assert_eq!(result["state_after"], "uboot");
    }

    #[tokio::test]
    async fn linux_shell_cannot_confirm_uboot_entry() {
        let result = simulated_entry(
            "none",
            b"root@debian:/# version\r\nbash: version: command not found\r\nroot@debian:/# ",
        )
        .await;
        assert_eq!(result["success"], false, "{result}");
    }

    #[test]
    fn auto_restart_prefers_reset_and_falls_back_to_software() {
        assert_eq!(
            resolve_uboot_restart(UbootRestart::Auto, true),
            UbootRestart::Hardware
        );
        assert_eq!(
            resolve_uboot_restart(UbootRestart::Auto, false),
            UbootRestart::Software
        );
        assert_eq!(
            resolve_uboot_restart(UbootRestart::None, true),
            UbootRestart::None
        );
    }
}
