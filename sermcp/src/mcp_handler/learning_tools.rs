use rmcp::{
    ErrorData, handler::server::wrapper::Parameters, model::CallToolResult, tool, tool_router,
};
use serde_json::json;

use super::{McpHandler, tool_result};
use crate::serial_engine::SerialEngine;
use crate::tools::params::*;

fn fingerprint_summary(eng: &SerialEngine) -> (usize, std::collections::BTreeMap<String, usize>) {
    let mut stages = std::collections::BTreeMap::new();
    let fingerprints = eng
        .detector
        .learner
        .as_ref()
        .map(|learner| learner.export_fingerprints())
        .unwrap_or_default();
    for (stage, _) in &fingerprints {
        *stages.entry(stage.clone()).or_insert(0) += 1;
    }
    (fingerprints.len(), stages)
}

#[tool_router(router = learning_router, vis = "pub(super)")]
impl McpHandler {
    // ── reference learning ─────────────────────────────────────────────

    #[tool(
        name = "serial_load_reference",
        description = "Load a reference boot log to enable adaptive stage detection for a new/unknown SOC. The reference log should be a complete boot log (DDR→SPL→U-Boot→Kernel→Shell). After loading, the stage detector uses text similarity to identify stages instead of hardcoded regex patterns.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    pub(super) async fn load_reference(
        &self,
        Parameters(a): Parameters<LoadReferenceArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let mut eng = self.engine.lock().await;
        let path = std::path::PathBuf::from(&a.reference_log_path);
        let st = eng.config.learner_stage_threshold();
        let ct = eng.config.learner_crash_threshold();
        let v = match eng.detector.load_reference(&path, st, ct) {
            Ok(()) => {
                let (fingerprint_count, stages) = fingerprint_summary(&eng);
                json!({
                    "success": true,
                    "message": format!("Reference loaded from {}", a.reference_log_path),
                    "fingerprints": fingerprint_count,
                    "stages": stages,
                })
            }
            Err(e) => json!({ "success": false, "error": e }),
        };
        // Gap #7: failed reference load is a cause-visible failure → isError.
        Ok(tool_result(v))
    }

    #[tool(
        name = "serial_append_reference",
        description = "Append key anchor lines to the reference boot log and hot-reload StageLearner. Read log://serial/unclassified to identify new boot patterns first. The lines become new fingerprints on future boot cycles without restarting the server. Choose distinctive stage-boundary lines and avoid timestamps, memory addresses, or random numbers.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    pub(super) async fn append_reference(
        &self,
        Parameters(a): Parameters<AppendReferenceArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let mut eng = self.engine.lock().await;
        // Gap #7: no reference_log configured / file open failure → isError.
        let mut result = eng.append_reference(&a.lines);
        if result["success"].as_bool().unwrap_or(false) {
            let (_, stages) = fingerprint_summary(&eng);
            result["stages"] = json!(stages);
        }
        Ok(tool_result(result))
    }

    #[tool(
        name = "serial_learn_connection",
        description = "Run connection learning to verify serial connectivity. Performs hardware reset (if relay configured) or software reboot cycles (default 3, clamped to 2..=10) and compares boot log similarity. Learning stops as soon as the similarity reaches the threshold (>= 93%): the cycle count is only the sampling budget, the similarity is the pass criterion. If similarity >= 93%, generates reference boot log for stage detection. If relay reset similarity < 10%, marks relay as broken and suggests software reboot fallback.",
        annotations(
            read_only_hint = false,
            // Most-dangerous-path rule: the default method physically
            // resets/power-cycles the target — a Tool-level annotation must
            // describe that path even though method=software only reboots.
            destructive_hint = true,
            open_world_hint = false
        )
    )]
    async fn learn_connection(
        &self,
        Parameters(a): Parameters<LearnConnectionArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let method = a.method.as_deref().unwrap_or("hardware");
        let use_power = a.hardware_action.as_deref() == Some("power");
        let reference_path = a.reference_log_path.as_deref().map(std::path::Path::new);
        let mut eng = self.engine.lock().await;
        let v = match method {
            "software" | "reboot" => {
                eng.learn_connection_software(reference_path, a.cycles_bounded())
                    .await
            }
            "auto" => {
                let hw = eng
                    .learn_connection_hardware(
                        reference_path,
                        a.cycles_bounded(),
                        a.quick,
                        use_power,
                    )
                    .await;
                if hw["success"].as_bool().unwrap_or(false) {
                    hw
                } else {
                    let sw = eng
                        .learn_connection_software(reference_path, a.cycles_bounded())
                        .await;
                    json!({
                        "hardware_result": hw,
                        "software_result": sw,
                        "success": sw["success"],
                        "method_used": "software_reboot",
                    })
                }
            }
            _ => {
                eng.learn_connection_hardware(
                    reference_path,
                    a.cycles_bounded(),
                    a.quick,
                    use_power,
                )
                .await
            }
        };
        // Gap #7: learning failure (relay broken, similarity too low) →
        // isError:true so agents surface it instead of parsing it as success.
        Ok(tool_result(v))
    }

    #[tool(
        name = "serial_verify_relay",
        description = "Strictly verify the configured reset control: confirm protocol read-back, hold reset while requiring serial silence, release it, then require fresh serial boot output.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            open_world_hint = false
        )
    )]
    async fn verify_relay(&self, _p: Parameters<NoArgs>) -> Result<CallToolResult, ErrorData> {
        let mut eng = self.engine.lock().await;
        // Gap #7: unconfigured / failed relay verification → isError:true.
        Ok(tool_result(eng.verify_relay().await))
    }
}
