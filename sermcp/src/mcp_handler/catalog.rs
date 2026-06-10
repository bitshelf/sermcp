use chrono::{DateTime, Utc};
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{
        Annotations, ErrorData, GetPromptResult, PromptMessage, ReadResourceResponse,
        ReadResourceResult, Resource, ResourceContents, Role,
    },
    prompt, prompt_router,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use super::McpHandler;

/// File metadata → optional byte size + `lastModified` annotation for a
/// file-backed resource. Priority is a fixed per-kind hint (spec: 0.0 =
/// entirely optional, 1.0 = most important).
fn file_annotations(priority: f32, path: &std::path::Path) -> (Option<u64>, Option<Annotations>) {
    let Ok(meta) = std::fs::metadata(path) else {
        return (None, None);
    };
    let annotations = meta
        .modified()
        .ok()
        .map(|mtime| Annotations::for_resource(priority, DateTime::<Utc>::from(mtime)));
    (Some(meta.len()), annotations)
}

/// Build a text file-backed resource, adding `size` and `annotations`
/// when the backing file is stat-able.
fn file_resource(
    uri: impl Into<String>,
    name: impl Into<String>,
    description: String,
    priority: f32,
    path: &std::path::Path,
) -> Resource {
    let (size, annotations) = file_annotations(priority, path);
    let mut resource = Resource::new(uri, name)
        .with_description(description)
        .with_mime_type("text/plain");
    if let Some(size) = size {
        resource = resource.with_size(size);
    }
    if let Some(annotations) = annotations {
        resource = resource.with_annotations(annotations);
    }
    resource
}

impl McpHandler {
    // ── Resources (internal impl, SoC/DUT-agnostic) ──────────────────────
    //
    // The MCP server binds to exactly one DUT at process start (see
    // `SerialEngine::config.dut_dir()` — derived from a `duts[].dut_name =
    // "<name>"` entry in `.target.jsonc` → `.dut-serial/<name>/`). Each DUT
    // has its
    // own MCP process and its own resource namespace; URIs therefore do NOT
    // include the name.
    //
    // Resource URIs (stable, predictable — agents can read without listing):
    //
    // | URI                          | Backing file                          | MIME             |
    // |------------------------------|---------------------------------------|------------------|
    // | `state://target`             | (dynamic)                             | application/json |
    // | `log://serial/current`       | `<dut_dir>/logs/current.serial.log`   | text/plain       |
    // | `log://serial/full`          | `<dut_dir>/logs/full.serial.log`      | text/plain       |
    // | `log://serial/unclassified`  | `<dut_dir>/unclassified.log`          | text/plain       |
    // | `log://boot/{index}`         | `<dut_dir>/logs/boot-NNN_<ts>.log`    | text/plain       |

    pub async fn list_resources_impl(&self) -> Vec<Resource> {
        let eng = self.engine.lock().await;
        let mut resources: Vec<Resource> = Vec::new();

        // Live DUT state
        resources.push(
            Resource::new("state://target", "Target State")
                .with_description("Current DUT state and metadata (live)")
                .with_mime_type("application/json"),
        );

        // Current boot cycle (real-time log)
        resources.push(file_resource(
            "log://serial/current",
            "Serial — current cycle",
            "current.serial.log — the active boot cycle, written in real time".into(),
            0.8,
            &eng.logs.log_dir().join("current.serial.log"),
        ));

        // Full continuous log (never truncated)
        resources.push(file_resource(
            "log://serial/full",
            "Serial — full history",
            "full.serial.log — continuous serial output across all boot cycles, never truncated"
                .into(),
            0.3,
            &eng.logs.log_dir().join("full.serial.log"),
        ));

        // StageLearner unclassified lines (helps agents learning a new SoC)
        let unclassified_path = eng.logs.dut_dir().join("unclassified.log");
        let pending_unclassified = eng.detector.unclassified_lines.len();
        if unclassified_path.exists() || pending_unclassified > 0 {
            let (size, annotations) = file_annotations(0.6, &unclassified_path);
            let mut resource = Resource::new("log://serial/unclassified", "Serial — unclassified lines")
                .with_description(format!(
                    "unclassified.log — {} bytes on disk and {} pending lines of serial output StageLearner could not classify into any known boot stage; \
                     use these anchors to expand the reference log (serial_append_reference).",
                    size.unwrap_or(0),
                    pending_unclassified,
                ))
                .with_mime_type("text/plain");
            resource = resource.with_size(size.unwrap_or(0));
            if let Some(annotations) = annotations {
                resource = resource.with_annotations(annotations);
            }
            resources.push(resource);
        }

        // Boot-cycle archives (newest first, like `serial_list_logs`)
        for a in eng.logs.list_archives() {
            resources.push(file_resource(
                format!("log://boot/{}", a.index),
                format!("Boot {} — {}", a.index, a.filename),
                format!("Boot log archive #{} ({} bytes)", a.index, a.size_bytes),
                0.5,
                &eng.logs.log_dir().join(&a.filename),
            ));
        }

        resources
    }

    pub async fn read_resource_impl(&self, uri: &str) -> Result<ReadResourceResponse, ErrorData> {
        // Live DUT state
        if uri == "state://target" {
            let eng = self.engine.lock().await;
            let state = eng.get_state_dict();
            let body = serde_json::to_string_pretty(&state).unwrap_or_default();
            return Ok(ReadResourceResponse::Complete(ReadResourceResult::new(
                vec![ResourceContents::text(body, uri).with_mime_type("application/json")],
            )));
        }

        // Named serial logs under the DUT's `.dut-serial/<name>/` directory.
        if uri == "log://serial/unclassified" {
            let eng = self.engine.lock().await;
            let path = eng.logs.dut_dir().join("unclassified.log");
            let mut body = match std::fs::read_to_string(&path) {
                Ok(body) => body,
                Err(e)
                    if e.kind() == std::io::ErrorKind::NotFound
                        && !eng.detector.unclassified_lines.is_empty() =>
                {
                    String::new()
                }
                Err(e) => {
                    tracing::warn!("read_resource {uri}: read {} failed: {e}", path.display());
                    return Err(ErrorData::resource_not_found(
                        format!("Resource not found: {uri} ({e})"),
                        Some(json!({"uri": uri, "path": path})),
                    ));
                }
            };
            for line in &eng.detector.unclassified_lines {
                if !body.is_empty() && !body.ends_with('\n') {
                    body.push('\n');
                }
                body.push_str(line);
                body.push('\n');
            }
            return Ok(ReadResourceResponse::Complete(ReadResourceResult::new(
                vec![ResourceContents::text(body, uri).with_mime_type("text/plain")],
            )));
        }

        let known: &[(&str, &str)] = &[
            ("log://serial/current", "current.serial.log"),
            ("log://serial/full", "full.serial.log"),
        ];
        if let Some((_, rel)) = known.iter().find(|(u, _)| *u == uri) {
            let eng = self.engine.lock().await;
            let path = eng.logs.log_dir().join(rel);
            // Gap #10: a listed resource that cannot be read is a structured
            // failure (RESOURCE_NOT_FOUND), not a 200-OK text placeholder —
            // agents can branch on the error code instead of scraping text.
            let body = std::fs::read_to_string(&path).map_err(|e| {
                tracing::warn!("read_resource {uri}: read {} failed: {e}", path.display());
                ErrorData::resource_not_found(
                    format!("Resource {uri} is unreadable: {e}"),
                    Some(json!({"uri": uri, "path": path.to_string_lossy()})),
                )
            })?;
            return Ok(ReadResourceResponse::Complete(ReadResourceResult::new(
                vec![ResourceContents::text(body, uri)],
            )));
        }

        // Boot-cycle archives: `log://boot/{index}`
        if let Some(index_str) = uri.strip_prefix("log://boot/")
            && let Ok(index) = index_str.parse::<usize>()
        {
            let eng = self.engine.lock().await;
            let result = eng.logs.read_log(index, 500, None);
            return Ok(ReadResourceResponse::Complete(ReadResourceResult::new(
                vec![ResourceContents::text(result.content, uri.to_string())],
            )));
        }

        // Gap #6: unknown URI → RESOURCE_NOT_FOUND, not INVALID_PARAMS.
        // rmcp's SEP-2164 version-rewrite layer (handler/server.rs) only
        // converts errors carrying the RESOURCE_NOT_FOUND code, so the
        // previous INVALID_PARAMS bypassed it — older peers saw -32602
        // instead of the spec-mandated -32002.
        Err(ErrorData::resource_not_found(
            format!("Unknown resource URI: {uri}"),
            Some(json!({"uri": uri})),
        ))
    }
}

// ── prompt catalog ───────────────────────────────────────────────────────────
//
// Declared with the rmcp `#[prompt]` macro: the catalog entry
// (`<fn>_prompt_attr()`) and the content handler come from the same
// function, so `prompts/list` can never advertise a name that
// `prompts/get` fails to resolve — the two hand-written lists this
// replaces could drift apart.

/// Arguments for the `debug_kernel` prompt. MCP prompt arguments are
/// string-valued on the wire; `lines` flows verbatim into the prompt body
/// (the body instructs an agent, nothing parses it as a number).
#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct DebugKernelArgs {
    /// Number of recent log lines to analyze
    pub lines: Option<String>,
}

/// Arguments for the `send_and_check` prompt.
#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct SendAndCheckArgs {
    /// Shell command to execute on target
    pub command: String,
}

/// Shared prompt body wrapper — same wire shape the hand-written catalog
/// produced: a single user message plus a description naming the prompt.
fn prompt_result(name: &str, body: String) -> Result<GetPromptResult, ErrorData> {
    Ok(
        GetPromptResult::new(vec![PromptMessage::new_text(Role::User, body)])
            .with_description(format!("prompt `{name}`")),
    )
}

#[prompt_router(router = "prompt_catalog_router", vis = "pub(super)")]
impl McpHandler {
    #[prompt(description = "Debug target boot process — monitor SPL, U-Boot, kernel, login")]
    async fn debug_boot(&self) -> Result<GetPromptResult, ErrorData> {
        prompt_result(
            "debug_boot",
            "Monitor the target boot process. Use serial_reset to reset the target, wait for each boot stage using serial_wait_pattern, and report the boot progress. Check for any errors or warnings in the boot log.".into(),
        )
    }

    #[prompt(description = "Diagnose kernel crashes from serial logs")]
    async fn debug_kernel(
        &self,
        Parameters(args): Parameters<DebugKernelArgs>,
    ) -> Result<GetPromptResult, ErrorData> {
        let lines = args
            .lines
            .as_deref()
            .map(|lines| format!("lines={lines}, "))
            .unwrap_or_default();
        prompt_result(
            "debug_kernel",
            format!(
                "Diagnose kernel crashes from the serial logs. Use serial_get_logs with {lines}pattern='panic|BUG|Oops|Call trace' to find crash information. Analyze the crash dump and suggest the likely root cause."
            ),
        )
    }

    #[prompt(description = "Quick target health check — state + recent output")]
    async fn check_status(&self) -> Result<GetPromptResult, ErrorData> {
        prompt_result(
            "check_status",
            "Check the target status. Use serial_get_state to get the current status, serial_get_logs with lines=20 to get recent output, and report the target's health.".into(),
        )
    }

    #[prompt(description = "Send a command to the target and verify the output")]
    async fn send_and_check(
        &self,
        Parameters(args): Parameters<SendAndCheckArgs>,
    ) -> Result<GetPromptResult, ErrorData> {
        prompt_result(
            "send_and_check",
            format!(
                "Send the command '{}' to the target via serial_send_command and verify the output. Report the command result.",
                args.command
            ),
        )
    }

    #[prompt(
        description = "Capture a clean boot log for StageLearner. Resets the target and waits for login."
    )]
    async fn boot_capture(&self) -> Result<GetPromptResult, ErrorData> {
        prompt_result(
            "boot_capture",
            "Run serial_reset(wait_boot=true) to capture a clean boot cycle. Then run serial_get_logs(lines=200) to review the boot log. Check serial_get_state to confirm the target reached active state.".into(),
        )
    }

    #[prompt(description = "Check if target has crashed and retrieve the crash log.")]
    async fn crash_diagnose(&self) -> Result<GetPromptResult, ErrorData> {
        prompt_result(
            "crash_diagnose",
            "Run serial_get_state — if state is 'crashed', run serial_get_logs(lines=200, pattern='panic|BUG|Oops|Call trace') to see the crash details. Then consider rebooting or entering U-Boot for recovery.".into(),
        )
    }

    #[prompt(
        description = "Enter U-Boot and recover from a bad kernel by flashing a new boot image."
    )]
    async fn uboot_recovery(&self) -> Result<GetPromptResult, ErrorData> {
        prompt_result(
            "uboot_recovery",
            "Run serial_enter_uboot to get to the U-Boot prompt. From there, use serial_uboot_command('setenv bootdelay 3') and serial_uboot_command('saveenv') to make U-Boot interactive. Then use serial_uboot_command('boot') to continue booting.".into(),
        )
    }
}
