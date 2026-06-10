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
        let power_result = {
            let mut eng = self.engine.lock().await;
            eng.power_cycle_target(false).await
        };
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

            // Software restart waits for SPL before arming the U-Boot prompt
            // watcher so bootdelay=0 targets are still interruptible.
            if restart == UbootRestart::Software {
                let (mut spl_rx, delivered) = {
                    let mut engine = self.engine.lock().await;
                    engine.state.transition(TargetState::Booting);
                    let delivered = engine.console.sendline("reboot");
                    engine.console.drain_writes().await;
                    engine.logs.flush_boot_log();
                    engine.logs.mark_boot_start();
                    engine.detector.reset_cycle();
                    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                    engine
                        .detector
                        .add_watcher(crate::loader::default().spl_pattern(), tx);
                    (rx, delivered)
                };
                if !delivered {
                    let mut engine = self.engine.lock().await;
                    engine.interrupt_char = original_interrupt_char;
                    engine
                        .detector
                        .remove_watcher_by_pattern(crate::loader::default().spl_pattern());
                    return Ok(text_error(json!({
                        "success": false,
                        "code": "serial_not_connected",
                        "error": "serial write channel closed — target not connected",
                        "restart": restart.as_str(),
                    })));
                }
                let _ = tokio::time::timeout(Duration::from_secs(30), spl_rx.recv()).await;
                self.engine
                    .lock()
                    .await
                    .detector
                    .remove_watcher_by_pattern(crate::loader::default().spl_pattern());
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
        // `send_ctrl_c` is kept for backward compatibility with existing agent
        // prompts; new prompts should prefer the SoC-agnostic name.
        let send_interrupt = a
            .action
            .as_deref()
            .is_some_and(|s| s == "send_interrupt" || s == "send_ctrl_c");
        let probe_on_timeout = a.pattern.contains("login")
            || a.pattern.contains("prompt")
            || a.pattern.contains("=>")
            || a.pattern.contains(r"\$")
            || a.pattern.contains('#');

        let console_tx = {
            let engine = self.engine.lock().await;
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

    /// Nonce for the vendor-neutral `echo <nonce>` prompt probe. Regex-inert
    /// characters only (the nonce doubles as a watcher pattern), unique
    /// enough per attempt that kernel boot spam can never collide with it.
    fn echo_probe_nonce() -> String {
        format!(
            "uboot_probe_{:08x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
                ^ (std::process::id() << 7)
        )
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
                .is_ok();
        }

        // Vendor-neutral interactive confirmation (fallback when the prompt
        // pattern never matched): the pattern is coupled to the prompt
        // STRING (`=>` / `U-Boot>` / `U-Boot#` is mainline only — vendor
        // forks ship their own), so it can false-negative forever. At ANY
        // interactive U-Boot command line `echo <nonce>` brings the nonce
        // back (as the echoed input line or as the command's output, the
        // nonce hex is regex-inert either way); during kernel boot spam it
        // never arrives on its own. One 2s probe per attempt.
        if !matched {
            let nonce = Self::echo_probe_nonce();
            let mut rx = {
                let mut engine = self.engine.lock().await;
                let rx = engine.queue_wait_pattern(&nonce);
                engine.send_raw(&format!("echo {nonce}\n"));
                rx
            };
            matched = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .is_ok();
            self.engine
                .lock()
                .await
                .detector
                .remove_watcher_by_pattern(&nonce);
        }

        let mut engine = self.engine.lock().await;
        engine
            .detector
            .remove_watcher_by_pattern(crate::loader::default().prompt_watch_pattern());
        if let Some(watch) = pattern_interrupt {
            engine.detector.remove_watcher_by_pattern(watch.pattern);
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
mod echo_probe_tests {
    use super::*;

    /// The nonce doubles as a watcher REGEX: it must be regex-inert
    /// (alphanumerics + underscore only) so the echoed input line AND the
    /// echo command's output both match it, and unique per call so kernel
    /// boot spam cannot collide with it.
    #[test]
    fn echo_probe_nonce_is_regex_inert_and_unique() {
        let a = McpHandler::echo_probe_nonce();
        let b = McpHandler::echo_probe_nonce();
        assert!(a.starts_with("uboot_probe_"), "{a}");
        assert!(
            a.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
            "regex-inert characters only: {a}"
        );
        assert_ne!(a, b, "nonce must advance between calls");
    }

    /// The probe command must be a bare echo (vendor/version independent —
    /// `echo` ships in essentially every U-Boot build), newline-terminated
    /// so the prompt executes it immediately.
    #[test]
    fn echo_probe_command_is_bare_echo_with_newline() {
        let nonce = McpHandler::echo_probe_nonce();
        let cmd = format!("echo {nonce}\n");
        assert!(cmd.starts_with("echo uboot_probe_"));
        assert!(cmd.ends_with('\n'));
        assert!(!cmd.contains('$'), "no shell expansion surface: {cmd}");
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
