use rmcp::{
    ErrorData, handler::server::wrapper::Parameters, model::CallToolResult, tool, tool_router,
};
use serde_json::json;

use super::{McpHandler, text, text_error, tool_result};
use crate::flash::build_flash_plan_json;
use crate::tools::params::{ButtonArgs, FlashPlanArgs, SetBaudArgs, TestBaudArgs};

#[tool_router(router = control_router, vis = "pub(super)")]
impl McpHandler {
    // ── buttons / flash ────────────────────────────────────────────────

    #[tool(
        name = "serial_button",
        description = "Control a DUT button (reset/download) via the power control abstraction. Supports press, release, and pulse actions. Buttons must be configured in the .target.jsonc [relay] section.",
        annotations(
            read_only_hint = false,
            // Gap #9: pressing reset/download physically disturbs the
            // target's running state — destructiveHint must be true so
            // safety-gating clients treat it as a destructive operation.
            destructive_hint = true,
            open_world_hint = false
        )
    )]
    async fn button(
        &self,
        Parameters(a): Parameters<ButtonArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let mut eng = self.engine.lock().await;
        // Gap #7: unknown button/action or backend failure → isError:true.
        Ok(tool_result(
            eng.control_button(&a.button, &a.action, a.delay_ms.map(|v| v as u64))
                .await,
        ))
    }

    #[tool(
        name = "serial_set_baud",
        description = "Dynamically change the DUT serial console baud rate via RFC 2217 (telnet COM-PORT-OPTION) on the dev-host serial port, WITHOUT touching the dev host. Sends SET-BAUDRATE, waits for the NOTIFY-BAUDRATE ack, and returns the baud rate actually applied. Requires rfc2217 = true in .target.jsonc [dut.serial] AND the ser2net port configured as telnet(rfc2217),tcp,<port>. The DUT-side console (U-Boot: 'setenv baudrate N; saveenv'; kernel: 'console=ttyS0,N') must be switched to the same rate first, or the console will go silent. Typical rates: 9600, 115200, 921600, 1500000. Returns a clear error on raw (non-RFC 2217) ports.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    pub(super) async fn set_baud(
        &self,
        Parameters(a): Parameters<SetBaudArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let mut eng = self.engine.lock().await;
        let result = eng.set_baud(a.baud).await;
        if result["success"].as_bool().unwrap_or(false) {
            Ok(text(result))
        } else {
            Ok(text_error(result))
        }
    }

    #[tool(
        name = "serial_flash_plan",
        description = "Generate a flash plan from the .target.jsonc [flash] config. Resolves symlinks, computes upload path, and shows commands without executing them.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    pub(super) async fn flash_plan(
        &self,
        Parameters(a): Parameters<FlashPlanArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let eng = self.engine.lock().await;
        let image_type = a.image_type.as_deref().unwrap_or("full");
        if a.image_path.is_empty() {
            Ok(text_error(
                json!({"success": false, "error": "image_path required"}),
            ))
        } else {
            let result = build_flash_plan_json(&eng.config, &a.image_path, image_type);
            Ok(if result["success"].as_bool().unwrap_or(false) {
                text(result)
            } else {
                text_error(result)
            })
        }
    }

    #[tool(
        name = "serial_test_baud",
        description = "Verify a candidate serial baud rate from fresh raw console bytes. When reset control is configured (and use_reset=true, the default), presses reset, creates a new capture/log cycle, then releases reset and analyzes the resulting boot bytes. use_reset=false (dutabo init TEST with dev_ctrl=none) never touches the relay — it samples the current console output. RFC 2217 endpoints are switched temporarily; raw ser2net endpoints validate the configured rate (and advertised metadata when available) plus readable boot text.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            open_world_hint = false
        )
    )]
    pub(super) async fn test_baud(
        &self,
        Parameters(a): Parameters<TestBaudArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let mut eng = self.engine.lock().await;
        Ok(tool_result(
            eng.test_baud(a.baud, a.capture_secs, a.use_reset).await,
        ))
    }
}
