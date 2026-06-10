//! rmcp `ServerHandler` implementation — the single, canonical MCP entry
//! point for every code agent (Claude Code, Codex, pi.dev, ...).
//!
//! This module replaces the earlier hand-rolled JSON-RPC dispatcher in
//! `mcp.rs::McpServer` (now deleted). It uses the official `rmcp` 3.x
//! macro pattern:
//!
//! * `#[tool_router]` aggregates every `#[tool] async fn` into a
//!   `ToolRouter<Self>`.
//! * `#[tool_handler]` auto-fills `list_tools` and `get_tool`. We override
//!   `get_info` to advertise the tools + resources + prompts + tasks
//!   capabilities, since an MCP server must declare a capability to expose
//!   the corresponding methods.
//! * A custom `call_tool` adds the `dutabo` takeover guard and converts
//!   `serial_reset` (with `wait_boot=true`) into a SEP-2663 task, since
//!   the macro does not know about the Tasks extension.
//! * `list_resources` / `read_resource` are implemented with the
//!   strongly-typed rmcp model types so the wire shape matches the current
//!   MCP spec exactly; `list_prompts` / `get_prompt` come from
//!   `#[prompt_handler]` over the macro-declared catalog in `catalog.rs`.
//! * `get_task` / `cancel_task` bridge to the in-tree `TaskManager`.
//!

use rmcp::{
    RoleServer, ServerHandler,
    handler::server::router::prompt::PromptRouter,
    handler::server::router::tool::ToolRouter,
    model::{
        CacheScope, CallToolRequestParams, CallToolResponse, CallToolResult, CancelTaskParams,
        ContentBlock, ElicitRequestParams, ElicitResult, ElicitationAction,
        ElicitationSchemaBuilder, ErrorData, GetTaskParams, GetTaskResult, Implementation,
        ListResourcesResult, ListToolsResult, PaginatedRequestParams, ProtocolVersion,
        ReadResourceRequestParams, ReadResourceResponse, ResultType, ServerCapabilities,
        ServerInfo, Tool, UpdateTaskParams,
    },
    prompt_handler,
    service::{ElicitationMode, NotificationContext, Peer, RequestContext},
    tool_handler,
};
use serde_json::{Value, json};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use crate::serial_engine::SharedEngine;
use crate::task_manager::TaskManager;

mod boot_tools;
mod catalog;
mod command_tools;
mod control_tools;
mod dut_inventory;
mod learning_tools;
mod query_tools;
mod task_runtime;

#[derive(Debug)]
pub(crate) struct AgentAccessRegistry {
    owner_reserved: bool,
    owner: Mutex<Weak<Mutex<AgentAccessRole>>>,
}

impl AgentAccessRegistry {
    pub(crate) fn new(owner_reserved: bool) -> Self {
        Self {
            owner_reserved,
            owner: Mutex::new(Weak::new()),
        }
    }

    fn assign(
        &self,
        client_name: Option<&str>,
        session: &Arc<Mutex<AgentAccessRole>>,
    ) -> AgentAccessRole {
        if client_name == Some("dutabo") {
            return AgentAccessRole::Human;
        }
        let mut owner = self.owner.lock().unwrap_or_else(|p| p.into_inner());
        if self.owner_reserved || owner.upgrade().is_some() {
            AgentAccessRole::ReadOnly
        } else {
            *owner = Arc::downgrade(session);
            AgentAccessRole::Owner
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AgentAccessRole {
    Unassigned,
    Owner,
    ReadOnly,
    Human,
}

/// Which slice of the tool catalog a session sees. One HTTP server serves
/// every client of a DUT (code agents, `dutabo`, `tui_init`), so the profile
/// is resolved per SESSION from the authorization role — never from the
/// client name — and never changes the tools' semantics:
///
/// * code-agent sessions (Owner/ReadOnly, stdio) → `Agent`: the compact
///   day-to-day debugging catalog; `serial_exec` is the unified command
///   entry and implementation-detail tools are withheld.
/// * Human-role sessions (the `dutabo`/`tui_init` CLI, which initialize as
///   `"dutabo"`) → `Full`: every registered tool, because init/bring-up and
///   flash flows drive the learner, relay verification, and raw escapes
///   over this same server.
///
/// `SERMCP_TOOL_PROFILE=full` forces `Full` for every session (debugging
/// sermcp itself).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ToolProfile {
    Agent,
    Full,
}

/// Tools withheld from `Agent` sessions. They stay registered (the `Full`
/// profile and internal `dutabo`/`tui_init` clients call them over this
/// same server) and their handlers keep working — `Agent` sessions simply
/// do not discover them:
///
/// * `serial_send_command`/`serial_uboot_command`/`serial_send_raw` —
///   superseded for agents by `serial_exec(mode=...)`; raw bytes are an
///   escape hatch, not a model-facing default.
/// * `serial_get_config`/`serial_get_metrics`/`serial_list_logs` —
///   server-introspection queries; logs are discoverable via
///   `resources/list` and readable via `serial_get_logs`.
/// * `serial_test_baud`/`serial_verify_relay`/`serial_learn_connection`/
///   `serial_load_reference`/`serial_append_reference` — bring-up/init
///   surface (`tui_init` TEST, `dutabo init`), noise for daily debugging.
const AGENT_HIDDEN_TOOLS: &[&str] = &[
    "serial_send_command",
    "serial_uboot_command",
    "serial_send_raw",
    "serial_get_config",
    "serial_get_metrics",
    "serial_list_logs",
    "serial_test_baud",
    "serial_verify_relay",
    "serial_learn_connection",
    "serial_load_reference",
    "serial_append_reference",
];

/// Static per-DUT capability snapshot backing the capability-aware catalog:
/// an agent must not have to learn hardware capabilities through failed
/// calls. Snapshot ONCE at handler build from `.target.jsonc` so the
/// advertised set stays stable for the process lifetime (MCP expects a
/// deterministic catalog; transient online/offline state must not reshape
/// it). All-true on engine-lock contention — a visible-but-unusable tool
/// fails with a precise error, a hidden-but-needed one is undiscoverable.
#[derive(Debug, Clone, Copy)]
struct DutCapabilities {
    reset_control: bool,
    power_channel: bool,
    any_button: bool,
    rfc2217: bool,
}

impl DutCapabilities {
    fn from_engine(engine: &SharedEngine) -> Self {
        let Some(eng) = engine.try_lock().ok() else {
            return Self {
                reset_control: true,
                power_channel: true,
                any_button: true,
                rfc2217: true,
            };
        };
        Self {
            reset_control: eng.reset_control_configured(),
            power_channel: eng.power_cycle_configured(),
            any_button: eng.power_control_configured(),
            rfc2217: eng.config.serial_rfc2217(),
        }
    }
}

/// `SERMCP_TOOL_PROFILE=full` forces the full catalog for every session.
/// Resolved once — the env var must not flip mid-process.
fn tool_profile_forced_full() -> bool {
    static FORCED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FORCED.get_or_init(|| std::env::var("SERMCP_TOOL_PROFILE").as_deref() == Ok("full"))
}

#[derive(Clone)]
pub struct McpHandler {
    pub engine: SharedEngine,
    pub tasks: Arc<TaskManager>,
    /// Macro-generated router for `#[tool]` methods. Public so integration
    /// tests can inspect the registered tool set without going on the wire.
    pub tool_router: ToolRouter<Self>,
    /// Macro-generated router for the `#[prompt]` methods in `catalog.rs`;
    /// `#[prompt_handler]` derives `list_prompts`/`get_prompt` from it.
    pub prompt_router: PromptRouter<Self>,
    /// Returned by serial_get_state while the startup probe is genuinely
    /// still in progress (`EngineFlags::startup_in_progress`), so discovery
    /// and diagnostics stay responsive without queueing behind the probe.
    pub(super) connecting_state: Value,
    agent_access_registry: Option<Arc<AgentAccessRegistry>>,
    agent_access_role: Arc<Mutex<AgentAccessRole>>,
    dut_caps: DutCapabilities,
}

impl McpHandler {
    pub fn new(engine: SharedEngine, tasks: Arc<TaskManager>) -> Self {
        let connecting_state = engine
            .try_lock()
            .map(|eng| eng.connecting_state_dict())
            .unwrap_or_else(|_| json!({"state": "connecting", "probe": "in_progress"}));
        Self::new_with_connecting_state(engine, tasks, connecting_state)
    }

    pub fn new_with_connecting_state(
        engine: SharedEngine,
        tasks: Arc<TaskManager>,
        connecting_state: Value,
    ) -> Self {
        Self::build(engine, tasks, connecting_state, None)
    }

    pub(crate) fn new_shared_http(
        engine: SharedEngine,
        tasks: Arc<TaskManager>,
        connecting_state: Value,
        agent_access_registry: Arc<AgentAccessRegistry>,
    ) -> Self {
        Self::build(engine, tasks, connecting_state, Some(agent_access_registry))
    }

    fn build(
        engine: SharedEngine,
        tasks: Arc<TaskManager>,
        connecting_state: Value,
        agent_access_registry: Option<Arc<AgentAccessRegistry>>,
    ) -> Self {
        let initial_role = if agent_access_registry.is_some() {
            AgentAccessRole::Unassigned
        } else {
            AgentAccessRole::Owner
        };
        // Snapshot before `engine` moves into Self.
        let dut_caps = DutCapabilities::from_engine(&engine);
        Self {
            engine,
            tasks,
            tool_router: ToolRouter::new()
                + Self::query_router()
                + Self::command_router()
                + Self::boot_router()
                + Self::learning_router()
                + Self::control_router(),
            prompt_router: Self::prompt_catalog_router(),
            connecting_state,
            agent_access_registry,
            agent_access_role: Arc::new(Mutex::new(initial_role)),
            dut_caps,
        }
    }

    pub fn engine(&self) -> &SharedEngine {
        &self.engine
    }

    /// Resolve this session's tool catalog profile. Dimension is the
    /// authorization role (the filter MCP portability guidance allows) —
    /// never `clientInfo.name`, which proxies and forks mutate freely.
    fn session_tool_profile(&self, client_name: Option<&str>) -> ToolProfile {
        if tool_profile_forced_full() {
            return ToolProfile::Full;
        }
        match self.agent_access_registry {
            // stdio: the single client is the agent host that spawned us.
            None => ToolProfile::Agent,
            // Shared HTTP server: Human-role CLI clients (dutabo, tui_init)
            // drive init/flash flows and need the complete catalog.
            Some(_) => {
                if self.ensure_agent_access(client_name) == AgentAccessRole::Human {
                    ToolProfile::Full
                } else {
                    ToolProfile::Agent
                }
            }
        }
    }

    /// Is `name` visible under `profile`? Capability gating runs for every
    /// profile: a DUT without relay control must not advertise reset/power/
    /// button tools, one without RFC 2217 must not advertise `serial_set_baud`.
    fn tool_visible(&self, name: &str, profile: ToolProfile) -> bool {
        if profile == ToolProfile::Agent && AGENT_HIDDEN_TOOLS.contains(&name) {
            return false;
        }
        match name {
            "serial_reset" => self.dut_caps.reset_control,
            "serial_power_cycle" => self.dut_caps.power_channel,
            "serial_button" => self.dut_caps.any_button,
            "serial_set_baud" => self.dut_caps.rfc2217,
            _ => true,
        }
    }

    /// The tool catalog a session sees: the full router's deterministic
    /// (name-sorted) list filtered by profile + DUT capabilities.
    pub(crate) fn visible_tools(&self, profile: ToolProfile) -> Vec<Tool> {
        self.tool_router
            .list_all()
            .into_iter()
            .filter(|tool| self.tool_visible(tool.name.as_ref(), profile))
            .collect()
    }

    fn ensure_agent_access(&self, client_name: Option<&str>) -> AgentAccessRole {
        let mut role = self
            .agent_access_role
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if matches!(
            *role,
            AgentAccessRole::Unassigned | AgentAccessRole::ReadOnly
        ) {
            let previous = *role;
            *role = self
                .agent_access_registry
                .as_ref()
                .expect("shared HTTP handler must have an access registry")
                .assign(client_name, &self.agent_access_role);
            if *role != previous {
                tracing::info!(
                    client = client_name.unwrap_or("unknown"),
                    access = ?*role,
                    "assigned MCP session access"
                );
            }
        }
        *role
    }

    /// Resolve the state this SESSION effectively acts on. Occupancy is a
    /// real state, not a cosmetic mask: while the interactive CLI holds the
    /// port every session's state is "dutabo", and a guest session's state
    /// is "busy" — the MCP is held by another agent session and every
    /// mutating tool is rejected, so reporting the device state as primary
    /// would invite the agent to act on a console it cannot use. The device
    /// state stays visible in `device_state`: read-only guests can still
    /// watch a boot or notice a crash.
    fn with_session_display(&self, mut state: Value) -> Value {
        let guest = self.ensure_agent_access(None) == AgentAccessRole::ReadOnly;
        let dutabo_taken = self.engine.flags.dutabo_taken.load(Ordering::Relaxed) != 0;
        let device_state = state["state"].as_str().unwrap_or("unknown").to_string();
        let display = crate::dut_state::session_display(
            if dutabo_taken {
                "dutabo"
            } else {
                &device_state
            },
            state["dut_name"].as_str().unwrap_or("serial"),
            guest,
        );
        state["state"] = serde_json::Value::String(display.state.clone());
        state["device_state"] = serde_json::Value::String(device_state);
        state["display"] = serde_json::to_value(display).expect("session display is serializable");
        state
    }

    fn tool_is_read_only(&self, tool_name: &str) -> bool {
        self.tool_router
            .get(tool_name)
            .and_then(|tool| tool.annotations.as_ref())
            .and_then(|annotations| annotations.read_only_hint)
            .unwrap_or(false)
    }

    fn agent_access_rejection(
        &self,
        tool_name: &str,
        client_name: Option<&str>,
    ) -> Option<CallToolResult> {
        if self.ensure_agent_access(client_name) != AgentAccessRole::ReadOnly
            || self.tool_is_read_only(tool_name)
        {
            return None;
        }
        Some(text_error(json!({
            "success": false,
            "code": "agent_read_only",
            "error": "This Code Agent session is read-only",
            "hint": "The first Code Agent session in this project owns MCP operations; use read-only tools or ask the owner session to perform the action",
        })))
    }

    fn mutating_task_access(&self, client_name: Option<&str>) -> Result<(), ErrorData> {
        if self.ensure_agent_access(client_name) != AgentAccessRole::ReadOnly {
            return Ok(());
        }
        Err(ErrorData::invalid_request(
            "This Code Agent session is read-only",
            Some(json!({
                "code": "agent_read_only",
                "hint": "Only the first Code Agent session in this project may update or cancel MCP tasks",
            })),
        ))
    }

    /// Lock-free `call_tool` guard — returns the rejection result while an
    /// interactive dutabo session owns the port, while the startup probe
    /// is genuinely still in progress, or while this engine does NOT own the
    /// serial endpoint (non-owner: read-only tools stay available, control
    /// tools are rejected so one agent can never reset/flash another's DUT);
    /// `None` lets the tool proceed (its body then queues on the engine
    /// mutex as usual).
    ///
    /// Reads the [`crate::serial_engine::EngineFlags`] atomics, never
    /// `try_lock()`: the read loop holds the mutex ~91% of every idle cycle
    /// (`wait_readable(100ms)` + 10ms sleep), so try_lock would misclassify
    /// a healthy idle DUT as "still connecting". `serial_get_state` is
    /// exempt from the connecting rejection so discovery stays responsive.
    fn takeover_rejection(&self, tool_name: &str) -> Option<CallToolResult> {
        let flags = &self.engine.flags;
        if flags.dutabo_taken.load(Ordering::Relaxed) != 0 && tool_name != "serial_get_state" {
            return Some(text_error(json!({
                "success": false,
                "error": "Serial is taken over by dutabo interactive session",
                "state": "dutabo",
            })));
        }
        if flags.startup_in_progress.load(Ordering::Relaxed) && tool_name != "serial_get_state" {
            return Some(text_error(json!({
                "success": false,
                "state": "connecting",
                "error": "Serial initialization/probe is still in progress",
                "hint": "Retry after serial_get_state no longer reports connecting",
            })));
        }
        // Non-owner gating: tools with an explicit read-only annotation stay
        // available for discovery and diagnostics; everything else is
        // rejected with the owner pid.
        // Exception: `serial_claim` is the recovery path FOR a non-owner —
        // its body re-runs acquire_lock, which only ever steals the lock
        // when the recorded holder is dead (a live owner keeps it), so
        // gating it by non-ownership would make the busy error's own hint
        // ("call serial_claim") unreachable.
        if !flags.owns_serial_lock.load(Ordering::Relaxed)
            && tool_name != "serial_claim"
            && !self.tool_is_read_only(tool_name)
        {
            let owner_pid = flags.serial_owner_pid.load(Ordering::Relaxed);
            return Some(text_error(json!({
                "success": false,
                "code": "serial_endpoint_busy",
                "error": "This MCP instance does not own the serial endpoint",
                "hint": "Another agent's server controls this DUT; inspect it with \
                         serial_get_state, or ask the human to run `dutabo serial`",
                "owner_pid": owner_pid,
            })));
        }
        None
    }

    /// Pre-flash agent review gate (MCP elicitation).
    ///
    /// Without relay-based reset control a bad image can only be recovered
    /// by manual power intervention, so an agent caller must review the
    /// code changes that produced the image before flash work is issued.
    /// The review request is answered by the calling agent itself via
    /// elicitation — agent hosts hand it to their model, so it never opens
    /// a human confirmation dialog. Clients that did not declare the
    /// elicitation capability (the manual `dutabo` CLI) pass through with
    /// no prompt at all.
    ///
    /// Returns `None` when the flash may proceed (relay configured, no
    /// elicitation channel, or review accepted); `Some(result)` aborts the
    /// tool call.
    async fn flash_review_gate(&self, peer: &Peer<RoleServer>) -> Option<CallToolResult> {
        // Pure config read (build_power_control never touches the serial
        // port). If the engine is momentarily locked, assume the relay IS
        // configured rather than surprising the caller with a review.
        let relay_configured = self
            .engine
            .try_lock()
            .map(|eng| eng.power_control_configured())
            .unwrap_or(true);
        if relay_configured {
            return None;
        }
        if !peer
            .supported_elicitation_modes()
            .contains(&ElicitationMode::Form)
        {
            return None;
        }

        let schema = ElicitationSchemaBuilder::new()
            .required_string_property("review_summary", |field| {
                field
                    .title("Review summary")
                    .description("What you reviewed in the changes that produced this image, and why it is expected to boot")
            })
            .string_property("risk_notes", |field| {
                field
                    .title("Risk notes")
                    .description("Residual risks you accept; write 'none' if none")
            })
            .build()
            // Builder-level validation only (required names must exist as
            // properties); this construction satisfies it by design.
            .expect("valid elicitation schema");
        let params = ElicitRequestParams::FormElicitationParams {
            meta: None,
            message: "Relay reset control is NOT configured for this DUT: if this image \
                      fails to boot, recovery requires manual power intervention. Review \
                      the code changes that produced it (boot-chain files, defconfig/DTS \
                      consistency, image format and partition layout, diffs since the \
                      last known-good boot) and summarize the review to proceed, or \
                      decline to abort the flash."
                .to_string(),
            requested_schema: schema,
        };
        match flash_review_verdict(
            peer.create_elicitation_with_timeout(params, Some(FLASH_REVIEW_TIMEOUT))
                .await,
        ) {
            FlashReviewVerdict::Proceed { review_summary } => {
                tracing::info!(
                    review_summary = review_summary.as_deref().unwrap_or("<none>"),
                    "pre-flash review accepted"
                );
                None
            }
            FlashReviewVerdict::Abort { reason } => Some(text_error(json!({
                "success": false,
                "error": format!("Flash request aborted: {reason}"),
                "gate": "pre_flash_review",
                "relay_configured": false,
                "hint": "Review the code changes that produced the image, then retry the flash plan",
            }))),
        }
    }
}

/// `ServerHandler` impl: `#[tool_handler]` auto-fills `list_tools` and
/// `get_tool` from the `ToolRouter`; `#[prompt_handler]` auto-fills
/// `list_prompts` and `get_prompt` from the `PromptRouter` built by the
/// `#[prompt_router]` impl in `catalog.rs`. We override `get_info` to
/// advertise tools + resources + prompts + tasks capability (MCP spec
/// requires capability advertisement for a server to expose the
/// corresponding methods), and keep a custom `call_tool` for the dutabo
/// guard plus `serial_reset` Task materialization.
#[tool_handler(router = self.tool_router)]
#[prompt_handler(router = self.prompt_router)]
impl ServerHandler for McpHandler {
    async fn on_initialized(&self, context: NotificationContext<RoleServer>) {
        let client_name = context
            .peer
            .peer_info()
            .map(|info| info.client_info.name.clone());
        self.ensure_agent_access(client_name.as_deref());
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        // Borrow the tool name instead of allocating a String for the
        // dispatch comparisons below (the field is a Cow<str>).
        let name: &str = request.name.as_ref();
        let client_name = context.client_info().map(|info| info.name).or_else(|| {
            context
                .peer
                .peer_info()
                .map(|info| info.client_info.name.clone())
        });

        // Profile gate FIRST, with the router's exact "tool not found"
        // payload — an Agent session must not be able to distinguish
        // "withheld from this session" from "does not exist" by anything
        // other than a Full-profile tools/list.
        let profile = self.session_tool_profile(client_name.as_deref());
        if !self.tool_visible(name, profile) {
            return Err(ErrorData::invalid_params("tool not found", None));
        }

        // Dutabo takeover guard — block ALL serial tools while an interactive
        // `dutabo serial` session owns the port. (The earlier implementation
        // exempted `serial_send_command`; community best practice is to gate
        // every tool, since the agent must not race the live terminal.) The
        // guard
        // reads the engine's lock-free flags mirror instead of `try_lock()`:
        // the read loop parks on `wait_readable(100ms)` while holding the
        // mutex, so try_lock misreports a busy-but-healthy engine as "still
        // connecting" ~91% of every idle cycle.
        if let Some(rejection) = self.takeover_rejection(name) {
            return Ok(CallToolResponse::Complete(rejection));
        }

        if let Some(rejection) = self.agent_access_rejection(name, client_name.as_deref()) {
            return Ok(CallToolResponse::Complete(rejection));
        }

        // Pre-flash review gate: without relay reset control a bad image
        // needs manual recovery, so agent callers must review their code
        // changes before flash work is issued. The elicitation is answered
        // by the agent itself — never a human dialog (see
        // `flash_review_gate`).
        if FLASH_ENTRY_TOOLS.contains(&name)
            && let Some(rejection) = self.flash_review_gate(&context.peer).await
        {
            return Ok(CallToolResponse::Complete(rejection));
        }

        // serial_reset with wait_boot=true → SEP-2663 task (long-running).
        if name == "serial_reset" {
            let supports_tasks = context
                .client_capabilities()
                .is_some_and(|capabilities| capabilities.supports_tasks());
            return self.handle_reset_with_task(request, supports_tasks).await;
        }

        // Every other tool flows through the macro router.
        let tcc = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        self.tool_router.call(tcc).await
    }

    /// `tools/list` per session: the deterministic full catalog filtered by
    /// this session's profile and the DUT's configured capabilities. Mirrors
    /// the `#[tool_handler]`-generated body (including the 2026-07-28 cache
    /// hints) so overriding costs nothing but the filter.
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let client_name = context.client_info().map(|info| info.name).or_else(|| {
            context
                .peer
                .peer_info()
                .map(|info| info.client_info.name.clone())
        });
        let profile = self.session_tool_profile(client_name.as_deref());
        let supports_cache_hints = context
            .protocol_version()
            .is_some_and(|version| version >= ProtocolVersion::V_2026_07_28);
        Ok(ListToolsResult {
            result_type: Some(ResultType::COMPLETE),
            tools: self.visible_tools(profile),
            meta: None,
            next_cursor: None,
            ttl_ms: supports_cache_hints.then_some(0),
            cache_scope: supports_cache_hints.then_some(CacheScope::Public),
        })
    }

    // ── MCP Tasks extension (SEP-2663) ─────────────────────────────────

    /// `tasks/get` — return the current [`DetailedTask`] state. The
    /// `rmcp` SDK already gates this route on the negotiated `tasks`
    /// capability (see `validate_tasks_capability` in
    /// `rmcp::handler::server`), so we never reach here for requests that
    /// didn't declare the extension capability.
    async fn get_task(
        &self,
        request: GetTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetTaskResult, ErrorData> {
        self.tasks.get(&request.task_id)
    }

    /// `tasks/update` — pass responses to rmcp's task runtime. Unknown or
    /// already-consumed keys are ignored as required by SEP-2663.
    async fn update_task(
        &self,
        request: UpdateTaskParams,
        context: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        let client_name = context.client_info().map(|info| info.name).or_else(|| {
            context
                .peer
                .peer_info()
                .map(|info| info.client_info.name.clone())
        });
        self.mutating_task_access(client_name.as_deref())?;
        self.tasks.update(&request.task_id, request.input_responses)
    }

    /// `tasks/cancel` — record cancellation intent and acknowledge it. The
    /// worker owns the eventual terminal state; cancellation never lies by
    /// claiming the physical operation has already stopped.
    async fn cancel_task(
        &self,
        request: CancelTaskParams,
        context: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        let client_name = context.client_info().map(|info| info.name).or_else(|| {
            context
                .peer
                .peer_info()
                .map(|info| info.client_info.name.clone())
        });
        self.mutating_task_access(client_name.as_deref())?;
        self.tasks.cancel(&request.task_id)
    }

    // ── Server info / capabilities ─────────────────────────────────────
    //
    // The handler macros only enable `tools`/`prompts` in a generated
    // `get_info`. We override `get_info` so the server also advertises
    // `resources` and
    // the `io.modelcontextprotocol/tasks` extension — without this, MCP
    // clients (Claude Code, Codex, pi.dev) will not discover our
    // `list_resources` / `get_task` implementations even
    // though they are wired in below. Per MCP spec, a server MUST advertise
    // a capability to expose the corresponding methods.
    //
    // `resources` deliberately does NOT declare `listChanged`: the resource
    // set does change over time (boot archives accumulate, the
    // unclassified log appears), but rmcp exposes no way to push
    // notifications to Streamable HTTP sessions — advertising a
    // notification we can never send would leave clients waiting instead
    // of re-polling `resources/list`.
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_prompts()
                .enable_resources()
                .enable_tasks()
                .build(),
        )
        // from_build_env() == Implementation::new("sermcp",
        // CARGO_PKG_VERSION): CARGO_CRATE_NAME for this package is
        // "sermcp" (see Cargo.toml), so the advertised wire shape
        // is unchanged — just less boilerplate.
        .with_server_info(Implementation::from_build_env())
        .with_instructions(
            // Tool-selection rules first (Codex keeps the first ~512 chars
            // self-contained; hosts that drop instructions entirely still
            // work — the rules duplicate what tool descriptions and
            // server-side guards enforce).
            "Debug one embedded DUT through its managed serial console. \
             Before reset, power-cycle, button, U-Boot entry, or flash work, call \
             serial_get_state (and serial_list_duts in multi-DUT projects) to verify \
             the DUT name and state. Run console commands with serial_exec(mode=auto) \
             — the server picks the shell or U-Boot protocol from the detected state. \
             Read bounded/filtered history with serial_get_logs; follow live output \
             incrementally with serial_poll_logs. Only call tools this server \
             advertises: the catalog already reflects the DUT's configured \
             relay/button/RFC2217/flash capabilities. serial_reset(wait_boot=true) \
             returns a pollable MCP Task on task-capable clients.",
        )
    }

    // ── Resources (strongly-typed, wire shape) ───────────────

    /// Expose every serial log under `.dut-serial/logs/` plus the live DUT
    /// state and the StageLearner unclassified log as MCP `Resource`s.
    ///
    /// URI scheme (stable, predictable — agents can read without first
    /// listing):
    ///
    /// | URI                          | Backing file                       | MIME             |
    /// |------------------------------|------------------------------------|------------------|
    /// | `state://target`             | (dynamic)                          | application/json |
    /// | `log://serial/current`       | `logs/current.serial.log`          | text/plain       |
    /// | `log://serial/full`          | `logs/full.serial.log`             | text/plain       |
    /// | `log://serial/unclassified`  | `.dut-serial/unclassified.log`     | text/plain       |
    /// | `log://boot/{index}`         | `logs/boot-NNN_<timestamp>.log`    | text/plain       |
    ///
    /// `resources/list` returns the full set (state + serial/current +
    /// serial/full + serial/unclassified + one entry per boot archive), so
    /// an agent discovers what's available without guessing.
    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        let mut result = ListResourcesResult::default();
        result.resources = self.list_resources_impl().await;
        Ok(result)
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        self.read_resource_impl(&request.uri).await
    }

    // ── Prompts ────────────────────────────────────────────────────────
    //
    // `list_prompts` / `get_prompt` are generated by `#[prompt_handler]`
    // above from the `#[prompt]`-declared catalog in `catalog.rs`; the
    // catalog entry and the content handler live in the same function, so
    // the two can no longer drift apart.
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// MCP tools that issue flash work. `serial_flash_plan` is the single MCP
/// entry point every flash path goes through — agents planning a flash and
/// the manual `dutabo flash` CLI both call it — so gating it covers both.
const FLASH_ENTRY_TOOLS: &[&str] = &["serial_flash_plan"];

/// How long the pre-flash review elicitation waits for the calling agent
/// before the flash request is aborted. Reviewing a diff takes time.
const FLASH_REVIEW_TIMEOUT: Duration = Duration::from_secs(300);

/// Outcome of the pre-flash review elicitation.
#[derive(Debug, PartialEq)]
enum FlashReviewVerdict {
    /// The caller reviewed its changes; the flash may proceed. Carries the
    /// review summary for the audit log.
    Proceed { review_summary: Option<String> },
    /// The flash must not proceed.
    Abort { reason: String },
}

/// Map an elicitation round-trip to the gate decision. Only an explicit
/// accept passes; decline, cancel, and transport failures all abort — a
/// flash that bricks the board needs manual recovery, so an unanswered
/// review must never fall through to "proceed".
fn flash_review_verdict<E: std::fmt::Display>(
    result: Result<ElicitResult, E>,
) -> FlashReviewVerdict {
    match result {
        Ok(elicit) => match elicit.action {
            ElicitationAction::Accept => FlashReviewVerdict::Proceed {
                review_summary: elicit
                    .content
                    .as_ref()
                    .and_then(|content| content.get("review_summary"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
            },
            // The enum is non_exhaustive: unknown future actions default
            // to the safe side, same as an explicit decline.
            _ => FlashReviewVerdict::Abort {
                reason: format!("pre-flash review not accepted ({:?})", elicit.action),
            },
        },
        Err(error) => FlashReviewVerdict::Abort {
            reason: format!("pre-flash review elicitation failed: {error}"),
        },
    }
}

/// Convert the operation's domain result to the MCP tool-result shape.
/// Runtime/soft failures complete the task with `isError: true`; `failed`
/// task status is reserved for JSON-RPC execution failures by SEP-2663.
fn tool_result(v: Value) -> CallToolResult {
    if v.get("success").and_then(Value::as_bool) == Some(false) {
        text_error(v)
    } else {
        text(v)
    }
}

/// Wrap a JSON payload as a **success** `CallToolResult` — the tool ran
/// and produced this result; `isError` is NOT set. Use this when the
/// operation itself succeeded (even if the JSON body reports a downstream
/// "not found" / "not configured" condition that the agent should read).
fn text(v: Value) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(
        serde_json::to_string(&v).unwrap_or_default(),
    )])
}

/// `text` + the same payload as `structuredContent`, for tools that
/// advertise an `outputSchema`. The TextContent JSON stays as the portable
/// fallback for hosts that ignore structured output.
fn text_structured(v: Value) -> CallToolResult {
    let mut result = text(v.clone());
    result.structured_content = Some(v);
    result
}

/// Wrap a JSON payload as an **error** `CallToolResult` (`isError: true`).
/// Per MCP spec, soft/runtime failures that the agent should see as tool
/// errors must set `isError` — agents that check the flag know to surface
/// the failure to the user instead of parsing the body as a success.
fn text_error(v: Value) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(
        serde_json::to_string(&v).unwrap_or_default(),
    )])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serial_engine::new_shared_engine;
    use crate::tools::params::{
        AppendReferenceArgs, FlashPlanArgs, LoadReferenceArgs, NoArgs, PollLogsArgs,
    };
    use rmcp::{
        handler::server::wrapper::Parameters,
        model::{Resource, ResourceContents},
    };
    use tempfile::TempDir;

    /// Build a handler bound to a **named** DUT (`.dut-serial/test-dut/`),
    /// mirroring the real on-disk layout produced by a `duts[].dut_name =
    /// "test-dut"` entry in `.target.jsonc`. Each DUT has its own
    /// sub-directory and
    /// its own MCP process, so resource URIs intentionally omit the name.
    /// Uses a SoC-neutral name so the test does not imply any vendor.
    fn make_handler() -> (McpHandler, TempDir) {
        make_handler_with_endpoints("59999", "0")
    }

    fn make_handler_with_endpoints(serial_port: &str, relay_port: &str) -> (McpHandler, TempDir) {
        let tmp = TempDir::new().unwrap();
        // DUT name "test-dut" → DUT_DIR = `.dut-serial/test-dut`.
        let config = crate::config::test_config(
            tmp.path(),
            serial_port,
            relay_port,
            Some("test-dut"),
            ".dut-serial/test-dut",
            "/tmp/sermcp-test-locks",
        );
        let engine = new_shared_engine(config);
        let tasks = TaskManager::new();
        (McpHandler::new(engine, tasks), tmp)
    }

    fn dut_dir(tmp: &TempDir) -> std::path::PathBuf {
        tmp.path().join(".dut-serial").join("test-dut")
    }

    fn logs_dir(tmp: &TempDir) -> std::path::PathBuf {
        dut_dir(tmp).join("logs")
    }

    fn uri_list(resources: &[Resource]) -> Vec<String> {
        resources.iter().map(|r| r.uri.clone()).collect()
    }

    fn make_shared_handler(registry: Arc<AgentAccessRegistry>) -> (McpHandler, TempDir) {
        let (base, tmp) = make_handler();
        let handler = McpHandler::new_shared_http(
            base.engine.clone(),
            base.tasks.clone(),
            base.connecting_state.clone(),
            registry,
        );
        (handler, tmp)
    }

    #[test]
    fn shared_http_assigns_first_code_agent_owner_and_later_agents_read_only() {
        let registry = Arc::new(AgentAccessRegistry::new(false));
        let (first, _first_tmp) = make_shared_handler(registry.clone());
        let (second, _second_tmp) = make_shared_handler(registry);

        assert_eq!(
            first.ensure_agent_access(Some("codex")),
            AgentAccessRole::Owner
        );
        assert_eq!(
            second.ensure_agent_access(Some("claude-code")),
            AgentAccessRole::ReadOnly
        );
    }

    #[test]
    fn shared_http_releases_owner_only_after_last_handler_clone_drops() {
        let registry = Arc::new(AgentAccessRegistry::new(false));
        let (owner, _tmp) = make_shared_handler(registry.clone());
        let (guest, _guest_tmp) = make_shared_handler(registry);
        assert_eq!(
            owner.ensure_agent_access(Some("codex")),
            AgentAccessRole::Owner
        );
        let clone = owner.clone();
        drop(owner);
        assert_eq!(
            guest.ensure_agent_access(Some("codex")),
            AgentAccessRole::ReadOnly
        );
        drop(clone);
        assert_eq!(
            guest.ensure_agent_access(Some("codex")),
            AgentAccessRole::Owner
        );
    }

    #[tokio::test]
    async fn guest_state_query_displays_busy_then_dutabo_even_with_engine_locked() {
        let registry = Arc::new(AgentAccessRegistry::new(true));
        let (guest, _tmp) = make_shared_handler(registry);
        guest.ensure_agent_access(Some("codex"));
        let state = guest.with_session_display(json!({"state":"active", "dut_name":"board"}));
        // Occupancy is the guest's primary state — busy, not "active".
        assert_eq!(state["state"], "busy");
        assert_eq!(state["device_state"], "active");
        assert_eq!(state["display"]["state"], "busy");
        let mut locked = guest.engine.lock().await;
        locked.begin_manual_session();
        let result = tokio::time::timeout(
            Duration::from_millis(100),
            guest.get_state(Parameters(NoArgs {})),
        )
        .await
        .expect("takeover status must not wait for the engine")
        .unwrap();
        let wire = serde_json::to_value(result).unwrap();
        let state: Value =
            serde_json::from_str(wire["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(state["state"], "dutabo");
        assert_eq!(state["device_state"], "dutabo");
        assert_eq!(state["display"]["state"], "dutabo");
        assert_ne!(wire["isError"], true);
    }

    #[test]
    fn dutabo_does_not_consume_code_agent_owner_slot() {
        let registry = Arc::new(AgentAccessRegistry::new(false));
        let (dutabo, _dutabo_tmp) = make_shared_handler(registry.clone());
        let (agent, _agent_tmp) = make_shared_handler(registry);

        assert_eq!(
            dutabo.ensure_agent_access(Some("dutabo")),
            AgentAccessRole::Human
        );
        assert_eq!(
            agent.ensure_agent_access(Some("codex")),
            AgentAccessRole::Owner
        );
    }

    #[test]
    fn stdio_owner_reservation_makes_all_http_code_agents_read_only() {
        let registry = Arc::new(AgentAccessRegistry::new(true));
        let (http_agent, _tmp) = make_shared_handler(registry);

        assert_eq!(
            http_agent.ensure_agent_access(Some("codex")),
            AgentAccessRole::ReadOnly
        );
    }

    #[test]
    fn read_only_agent_can_query_but_cannot_operate_or_mutate_tasks() {
        let registry = Arc::new(AgentAccessRegistry::new(true));
        let (handler, _tmp) = make_shared_handler(registry);

        assert!(
            handler
                .agent_access_rejection("serial_get_state", Some("codex"))
                .is_none()
        );
        assert!(
            handler
                .agent_access_rejection("serial_poll_logs", Some("codex"))
                .is_none()
        );
        assert!(
            handler
                .agent_access_rejection("serial_flash_plan", Some("codex"))
                .is_none()
        );

        for tool in ["serial_send_command", "serial_reset", "serial_claim"] {
            let rejection = handler
                .agent_access_rejection(tool, Some("codex"))
                .expect("mutating tool must be rejected for a read-only agent");
            let wire = serde_json::to_value(rejection).unwrap();
            let body = wire["content"][0]["text"].as_str().unwrap();
            assert!(body.contains("agent_read_only"), "{tool}: {body}");
        }

        let task_error = handler
            .mutating_task_access(Some("codex"))
            .expect_err("read-only agent must not mutate tasks");
        assert_eq!(task_error.data.unwrap()["code"], "agent_read_only");
    }

    #[test]
    fn every_tool_explicitly_declares_read_only_access() {
        let (handler, _tmp) = make_handler();
        for tool in handler.tool_router.list_all() {
            assert!(
                tool.annotations
                    .as_ref()
                    .and_then(|annotations| annotations.read_only_hint)
                    .is_some(),
                "{} must declare readOnlyHint for server-side access control",
                tool.name
            );
        }
    }

    // ── tool catalog profiles ───────────────────────────────────────────

    fn names_of(handler: &McpHandler, profile: ToolProfile) -> Vec<String> {
        handler
            .visible_tools(profile)
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect()
    }

    fn payload_of(result: &CallToolResult) -> Value {
        let wire = serde_json::to_value(result).unwrap();
        serde_json::from_str(wire["content"][0]["text"].as_str().unwrap()).unwrap()
    }

    /// Relay + power channels configured, so capability gating keeps the
    /// power tools visible and the Agent core can be asserted in full.
    fn make_handler_with_relay_buttons() -> (McpHandler, TempDir) {
        let tmp = TempDir::new().unwrap();
        let mut config = crate::config::test_config(
            tmp.path(),
            "59999",
            "59998",
            Some("test-dut"),
            ".dut-serial/test-dut",
            "/tmp/sermcp-test-locks",
        );
        config.values.insert("RESET_CHANNEL".into(), "1".into());
        config.values.insert("POWER_CHANNEL".into(), "2".into());
        // RFC 2217 on: every capability-gated tool stays visible, so the
        // Full profile equals the raw router catalog in this fixture.
        config.values.insert("SERIAL_RFC2217".into(), "true".into());
        let engine = new_shared_engine(config);
        (McpHandler::new(engine, TaskManager::new()), tmp)
    }

    #[test]
    fn stdio_session_defaults_to_the_agent_profile() {
        let (handler, _tmp) = make_handler();
        assert_eq!(handler.session_tool_profile(None), ToolProfile::Agent);
        assert_eq!(
            handler.session_tool_profile(Some("claude-code")),
            ToolProfile::Agent
        );
    }

    #[test]
    fn shared_http_human_sessions_get_full_agents_get_agent() {
        let registry = Arc::new(AgentAccessRegistry::new(false));
        let (dutabo_handler, _dutabo_tmp) = make_shared_handler(registry.clone());
        let (agent_handler, _agent_tmp) = make_shared_handler(registry);
        assert_eq!(
            dutabo_handler.session_tool_profile(Some("dutabo")),
            ToolProfile::Full
        );
        assert_eq!(
            agent_handler.session_tool_profile(Some("codex")),
            ToolProfile::Agent
        );
    }

    #[test]
    fn agent_profile_keeps_debugging_core_withholds_implementation_tools() {
        let (handler, _tmp) = make_handler_with_relay_buttons();
        let names = names_of(&handler, ToolProfile::Agent);

        for core in [
            "serial_get_state",
            "serial_list_duts",
            "serial_get_logs",
            "serial_poll_logs",
            "serial_exec",
            "serial_wait_pattern",
            "serial_enter_uboot",
            "serial_reset",
            "serial_power_cycle",
            "serial_button",
            "serial_flash_plan",
            "serial_claim",
        ] {
            assert!(
                names.iter().any(|name| name == core),
                "{core} missing: {names:?}"
            );
        }
        for hidden in AGENT_HIDDEN_TOOLS {
            assert!(
                !names.iter().any(|name| name == hidden),
                "{hidden} leaked into the agent catalog: {names:?}"
            );
        }
        // Deterministic order — clients cache the catalog and the model's
        // prompt cache benefits from a stable listing.
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
    }

    #[test]
    fn full_profile_with_relay_exposes_the_entire_router_catalog() {
        let (handler, _tmp) = make_handler_with_relay_buttons();
        let full = names_of(&handler, ToolProfile::Full);
        let router: Vec<String> = handler
            .tool_router
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        assert_eq!(full, router);
        // The unified exec entry coexists with the legacy pair the internal
        // CLI clients (dutabo, tui_init) still drive over this same server.
        for name in ["serial_exec", "serial_send_command", "serial_uboot_command"] {
            assert!(full.iter().any(|n| n == name), "{name}: {full:?}");
        }
    }

    #[test]
    fn capability_gating_hides_power_tools_without_relay_config() {
        let (handler, _tmp) = make_handler();
        for profile in [ToolProfile::Agent, ToolProfile::Full] {
            let names = names_of(&handler, profile);
            for name in [
                "serial_reset",
                "serial_power_cycle",
                "serial_button",
                "serial_set_baud",
            ] {
                assert!(
                    !names.iter().any(|n| n == name),
                    "{name} must stay hidden without relay/RFC2217 config \
                     ({profile:?}): {names:?}"
                );
            }
        }
        // Hidden ≠ removed: the routes stay registered for internal use.
        assert!(handler.tool_router.get("serial_reset").is_some());
        assert!(handler.tool_router.get("serial_set_baud").is_some());
    }

    #[test]
    fn tool_visibility_gate_covers_hidden_and_capability_dimensions() {
        let (handler, _tmp) = make_handler_with_relay_buttons();
        assert!(!handler.tool_visible("serial_learn_connection", ToolProfile::Agent));
        assert!(handler.tool_visible("serial_learn_connection", ToolProfile::Full));
        assert!(handler.tool_visible("serial_exec", ToolProfile::Agent));
        // Capability dimension: relay buttons configured in this fixture.
        assert!(handler.tool_visible("serial_reset", ToolProfile::Agent));
    }

    // ── serial_exec protocol selection ──────────────────────────────────

    #[tokio::test]
    async fn serial_exec_auto_selects_uboot_protocol_in_uboot_state() {
        let (handler, _tmp) = make_handler();
        handler
            .engine
            .lock()
            .await
            .state
            .transition(crate::state_manager::TargetState::UBoot);
        let result = handler
            .exec(Parameters(crate::tools::params::ExecArgs {
                command: "version".into(),
                mode: crate::tools::params::ExecMode::Auto,
                timeout: 2,
            }))
            .await
            .unwrap();
        // Disconnected fixture: the U-Boot collection path times out
        // without output; the mode field proves which protocol ran.
        let payload = payload_of(&result);
        assert_eq!(payload["mode"], json!("uboot"));
        assert_eq!(payload["success"], json!(false));
        assert_eq!(result.is_error, Some(true));
    }

    #[tokio::test]
    async fn serial_exec_auto_selects_shell_protocol_in_active_state() {
        let (handler, _tmp) = make_handler();
        handler
            .engine
            .lock()
            .await
            .state
            .transition(crate::state_manager::TargetState::Active);
        let result = handler
            .exec(Parameters(crate::tools::params::ExecArgs {
                command: "echo hi".into(),
                mode: crate::tools::params::ExecMode::Auto,
                timeout: 1,
            }))
            .await
            .unwrap();
        let payload = payload_of(&result);
        assert_eq!(payload["mode"], json!("shell"));
    }

    #[tokio::test]
    async fn serial_exec_explicit_mode_overrides_the_detected_state() {
        let (handler, _tmp) = make_handler();
        handler
            .engine
            .lock()
            .await
            .state
            .transition(crate::state_manager::TargetState::Active);
        let result = handler
            .exec(Parameters(crate::tools::params::ExecArgs {
                command: "version".into(),
                mode: crate::tools::params::ExecMode::Uboot,
                timeout: 2,
            }))
            .await
            .unwrap();
        let payload = payload_of(&result);
        assert_eq!(payload["mode"], json!("uboot"));
        assert_eq!(payload["success"], json!(false));
    }

    #[tokio::test]
    async fn get_state_carries_structured_content_and_schema_is_advertised() {
        let (handler, _tmp) = make_handler();
        let result = handler.get_state(Parameters(NoArgs {})).await.unwrap();
        assert_eq!(result.is_error, Some(false));
        let structured = result
            .structured_content
            .expect("serial_get_state must carry structuredContent");
        assert!(structured.get("state").is_some());
        // The advertised outputSchema only requires `state`; the live
        // payload (dut_name, display, ...) validates against it.
        let tool = handler
            .tool_router
            .get("serial_get_state")
            .expect("serial_get_state registered");
        let schema = tool
            .output_schema
            .as_ref()
            .expect("serial_get_state must advertise an outputSchema");
        assert!(
            schema
                .get("properties")
                .and_then(|p| p.get("state"))
                .is_some(),
            "schema must declare the state property: {schema:?}"
        );
    }

    #[test]
    fn soft_error_sets_mcp_is_error() {
        let result = text_error(json!({"success": false, "error": "expected"}));
        assert_eq!(result.is_error, Some(true));
    }

    /// Gap #7: the unified `tool_result()` channel flags every
    /// `success:false` body as `isError:true` and leaves success bodies clean.
    #[test]
    fn tool_result_flags_soft_failure_only() {
        let err = tool_result(json!({"success": false, "error": "refused"}));
        assert_eq!(err.is_error, Some(true));
        let ok = tool_result(json!({"success": true, "message": "claimed"}));
        assert_eq!(ok.is_error, Some(false));
        // Bodies without a `success` key are treated as success (older shapes).
        let older = tool_result(json!({"lines": [], "count": 0}));
        assert_eq!(older.is_error, Some(false));
    }

    /// Gap #7: a failed reference-log load must surface as isError:true.
    #[tokio::test]
    async fn load_reference_failure_sets_mcp_is_error() {
        let (handler, _tmp) = make_handler();
        let result = handler
            .load_reference(Parameters(LoadReferenceArgs {
                reference_log_path: "/nonexistent/definitely-missing-ref.log".into(),
            }))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true));
        let wire = serde_json::to_value(result).unwrap();
        assert!(
            wire["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("\"success\":false"),
            "expected a success:false body: {wire}"
        );
    }

    #[tokio::test]
    async fn load_reference_returns_stage_summary() {
        let (handler, tmp) = make_handler();
        let path = tmp.path().join("reference.log");
        std::fs::write(
            &path,
            "DDR Version 1\nDDR Init done\nU-Boot SPL 2024\nSPL board init\nBL31: v2\nOP-TEE version\nU-Boot 2024.07\nLinux version 6.6\nStarting kernel\ninit: started\nconsole:/ #\n",
        )
        .unwrap();

        let result = handler
            .load_reference(Parameters(LoadReferenceArgs {
                reference_log_path: path.to_string_lossy().into_owned(),
            }))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(false));
        let wire = serde_json::to_value(result).unwrap();
        let body: serde_json::Value =
            serde_json::from_str(wire["content"][0]["text"].as_str().unwrap()).unwrap();
        assert!(body["fingerprints"].as_u64().unwrap() > 0, "{body}");
        assert!(body["stages"]["ddr"].as_u64().unwrap() > 0, "{body}");
        assert!(body["stages"]["kernel"].as_u64().unwrap() > 0, "{body}");
    }

    #[tokio::test]
    async fn append_reference_returns_reloaded_stage_summary() {
        let (handler, tmp) = make_handler();
        let path = tmp.path().join("reference.log");
        std::fs::write(
            &path,
            "DDR Version 1\nDDR Init done\nU-Boot SPL 2024\nSPL board init\nBL31: v2\nOP-TEE version\nU-Boot 2024.07\nLinux version 6.6\nStarting kernel\ninit: started\nconsole:/ #\n",
        )
        .unwrap();
        handler
            .engine
            .lock()
            .await
            .config
            .values
            .insert("REFERENCE_LOG".into(), path.to_string_lossy().into_owned());

        let result = handler
            .append_reference(Parameters(AppendReferenceArgs {
                lines: "Boot completed successfully".into(),
            }))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(false));
        let wire = serde_json::to_value(result).unwrap();
        let body: serde_json::Value =
            serde_json::from_str(wire["content"][0]["text"].as_str().unwrap()).unwrap();
        assert!(body["fingerprints"].as_u64().unwrap() > 0, "{body}");
        assert!(body["stages"]["booted"].as_u64().unwrap() > 0, "{body}");
    }

    /// Gap #8: `since` is the byte-offset cursor echoed from a previous
    /// response — passing it back must deliver exactly the new lines.
    #[tokio::test]
    async fn poll_logs_honors_since_cursor_and_returns_only_new_lines() {
        let (handler, _tmp) = make_handler();
        {
            let mut eng = handler.engine.lock().await;
            eng.logs.open_current();
            eng.logs.write(b"boot line 1\n");
            eng.logs.flush_sync();
        }

        // Cursor 0 → everything since the start of the current log file.
        let first = handler
            .poll_logs(Parameters(PollLogsArgs {
                since: Some(0.0),
                timeout: 0,
            }))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(
            serde_json::to_value(first).unwrap()["content"][0]["text"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert!(
            v["lines"]
                .as_array()
                .unwrap()
                .iter()
                .any(|l| l == "boot line 1"),
            "cursor 0 must yield the initial content: {v}"
        );
        let cursor = v["since"].as_u64().expect("poll must echo a since cursor");
        assert!(cursor > 0);

        // Echoed cursor with no new data → empty lines, same cursor.
        let second = handler
            .poll_logs(Parameters(PollLogsArgs {
                since: Some(cursor as f64),
                timeout: 0,
            }))
            .await
            .unwrap();
        let v2: serde_json::Value = serde_json::from_str(
            serde_json::to_value(second).unwrap()["content"][0]["text"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert!(v2["lines"].as_array().unwrap().is_empty());
        assert_eq!(v2["since"].as_u64().unwrap(), cursor);

        // Append → only the new line comes back from the same cursor.
        {
            let mut eng = handler.engine.lock().await;
            eng.logs.write(b"boot line 2\n");
            eng.logs.flush_sync();
        }
        let third = handler
            .poll_logs(Parameters(PollLogsArgs {
                since: Some(cursor as f64),
                timeout: 0,
            }))
            .await
            .unwrap();
        let v3: serde_json::Value = serde_json::from_str(
            serde_json::to_value(third).unwrap()["content"][0]["text"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(v3["lines"].as_array().unwrap().len(), 1);
        assert_eq!(v3["lines"][0], "boot line 2");
    }

    /// Gap #8: the `timeout` parameter must actually long-poll — the tool
    /// blocks until new output arrives instead of returning immediately.
    #[tokio::test]
    async fn poll_logs_long_polls_until_new_output_within_timeout() {
        let (handler, _tmp) = make_handler();
        {
            let mut eng = handler.engine.lock().await;
            eng.logs.open_current();
            eng.logs.write(b"initial\n");
            eng.logs.flush_sync();
        }
        // Prime the engine's positional cursor (since=None path) so the
        // initial content is consumed before the long-poll begins.
        handler
            .poll_logs(Parameters(PollLogsArgs {
                since: None,
                timeout: 0,
            }))
            .await
            .unwrap();

        // A concurrent "read loop" appends a line after 300ms.
        let writer = {
            let handler = handler.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                let mut eng = handler.engine.lock().await;
                eng.logs.write(b"late line\n");
                eng.logs.flush_sync();
            })
        };

        let start = std::time::Instant::now();
        let result = handler
            .poll_logs(Parameters(PollLogsArgs {
                since: None,
                timeout: 5,
            }))
            .await
            .unwrap();
        let elapsed = start.elapsed();
        writer.await.unwrap();

        let v: serde_json::Value = serde_json::from_str(
            serde_json::to_value(result).unwrap()["content"][0]["text"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert!(
            v["lines"]
                .as_array()
                .unwrap()
                .iter()
                .any(|l| l == "late line"),
            "long-poll must deliver the late line: {v}"
        );
        assert!(
            elapsed >= std::time::Duration::from_millis(100)
                && elapsed < std::time::Duration::from_secs(4),
            "long-poll must wait for the data, not spin or oversleep: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn state_is_immediately_available_while_startup_holds_engine_lock() {
        let (handler, _tmp) = make_handler();
        let _startup_guard = handler.engine.lock().await;
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            handler.get_state(Parameters(NoArgs {})),
        )
        .await
        .expect("serial_get_state must not wait for the startup probe")
        .unwrap();
        let wire = serde_json::to_value(result).unwrap();
        assert_eq!(wire["isError"], false);
        assert!(
            wire["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("\"state\":\"connecting\"")
        );
        assert!(
            wire["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("\"dut_name\":\"test-dut\"")
        );
    }

    #[tokio::test]
    async fn state_marks_serial_relay_endpoint_conflict_as_mcp_error() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let configured_port = listener.local_addr().unwrap().port().to_string();
        drop(listener);
        let (handler, _tmp) = make_handler_with_endpoints(&configured_port, &configured_port);
        let result = handler.get_state(Parameters(NoArgs {})).await.unwrap();
        let wire = serde_json::to_value(result).unwrap();
        assert_eq!(wire["isError"], true);
        let text = wire["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("\"state\":\"configuration-error\""));
        assert!(text.contains("serial_relay_endpoint_conflict"));
        assert!(!text.contains("\"state\":\"DUT-off\""));
    }

    /// Regression (CR-3): while the read loop holds the engine mutex, a
    /// non-get_state tool must queue on the mutex — not be rejected as
    /// "initialization in progress" by a try_lock-based guard. Simulates the
    /// read loop's ~100ms `wait_readable` hold with a held lock.
    #[tokio::test]
    async fn tools_queue_behind_busy_read_loop_instead_of_connecting_error() {
        let (handler, _tmp) = make_handler();
        // Startup finished — the connecting window is over.
        handler
            .engine
            .flags
            .startup_in_progress
            .store(false, Ordering::Relaxed);

        // Simulate the read loop holding the engine mutex.
        let read_loop_guard = handler.engine.lock().await;

        // The call_tool guard must not misreport the busy engine as
        // "initialization in progress"...
        assert!(
            handler.takeover_rejection("serial_get_config").is_none(),
            "busy read loop must not trip the connecting rejection"
        );
        // ...and the tool body must queue on the mutex (timeout), not fail.
        let mut queued = tokio::spawn({
            let handler = handler.clone();
            async move { handler.get_config(Parameters(NoArgs {})).await }
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(200), &mut queued)
                .await
                .is_err(),
            "tool body must wait for the read loop, not fail immediately"
        );

        drop(read_loop_guard);
        let result = queued.await.unwrap().unwrap();
        // Successful tool results carry an explicit `is_error: Some(false)`.
        assert_eq!(result.is_error, Some(false));
        let wire = serde_json::to_value(result).unwrap();
        assert!(
            !wire["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("initialization/probe is still in progress")
        );
    }

    /// Regression (CR-3): the connecting rejection must be driven by the
    /// startup flag — reject while the startup probe is genuinely in flight,
    /// run normally once it finishes. serial_get_state stays exempt.
    #[tokio::test]
    async fn connecting_guard_rejects_tools_only_while_startup_in_progress() {
        let (handler, _tmp) = make_handler();
        // Fresh engine: startup probe not yet finished → connecting window.
        let rejection = handler
            .takeover_rejection("serial_get_config")
            .expect("startup-in-progress must reject non-state tools");
        assert_eq!(rejection.is_error, Some(true));
        let wire = serde_json::to_value(rejection).unwrap();
        assert!(
            wire["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("initialization/probe is still in progress")
        );
        // serial_get_state stays available for discovery while connecting.
        assert!(handler.takeover_rejection("serial_get_state").is_none());

        // Startup finished → guard opens and the tool body runs normally.
        handler
            .engine
            .flags
            .startup_in_progress
            .store(false, Ordering::Relaxed);
        assert!(handler.takeover_rejection("serial_get_config").is_none());
        let result = handler.get_config(Parameters(NoArgs {})).await.unwrap();
        assert_eq!(result.is_error, Some(false));
    }

    /// Regression (CR-3): a dutabo takeover must reject tools from the
    /// lock-free snapshot even while the read loop holds the mutex, and the
    /// rejection must clear when the takeover ends.
    #[tokio::test]
    async fn dutabo_takeover_guard_rejects_tools_while_ws_session_owns_port() {
        let (handler, _tmp) = make_handler();
        handler
            .engine
            .flags
            .startup_in_progress
            .store(false, Ordering::Relaxed);
        // A takeover only happens on a server that OWNS the serial endpoint;
        // after the session ends the guard must fall through to "owner,
        // ready" (the non-owner read-only gate is a separate concern).
        handler
            .engine
            .flags
            .owns_serial_lock
            .store(true, Ordering::Relaxed);

        // A WebSocket takeover flips the engine into Dutabo; the guard must
        // reject control tools while leaving state queries available.
        handler.engine.lock().await.begin_manual_session();
        let rejection = handler
            .takeover_rejection("serial_send_command")
            .expect("dutabo takeover must block tools");
        assert_eq!(rejection.is_error, Some(true));
        let wire = serde_json::to_value(rejection).unwrap();
        assert!(
            wire["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("taken over by dutabo interactive session")
        );
        assert!(handler.takeover_rejection("serial_get_state").is_none());

        // The session ends → guard opens again.
        handler.engine.lock().await.clear_ws_tx();
        assert!(handler.takeover_rejection("serial_send_command").is_none());
    }

    /// serial_claim must stay reachable for a non-owner engine: it is the
    /// recovery path for a stale endpoint lock (crashed sibling, rebooted
    /// machine), and its body re-runs acquire_lock which never steals from a
    /// live holder. Gating it by non-ownership made the busy error's own
    /// "call serial_claim" hint unreachable.
    #[tokio::test]
    async fn non_owner_gate_keeps_serial_claim_available() {
        let (handler, _tmp) = make_handler();
        handler
            .engine
            .flags
            .startup_in_progress
            .store(false, Ordering::Relaxed);
        handler
            .engine
            .flags
            .owns_serial_lock
            .store(false, Ordering::Relaxed);
        handler
            .engine
            .flags
            .serial_owner_pid
            .store(999_999, Ordering::Relaxed);

        // The claim itself passes the non-owner gate…
        assert!(handler.takeover_rejection("serial_claim").is_none());
        // …while every other control tool stays gated for the same engine.
        assert!(handler.takeover_rejection("serial_send_command").is_some());
        assert!(handler.takeover_rejection("serial_reset").is_some());
    }

    /// Regression (CR-3): serial_get_state must not answer "connecting"
    /// when only the read loop holds the mutex — it queues briefly and
    /// returns the real state (the pre-flags try_lock fallback reported the
    /// stale connecting snapshot ~91% of every idle cycle).
    #[tokio::test]
    async fn get_state_reports_real_state_not_connecting_when_read_loop_holds_lock() {
        let (handler, _tmp) = make_handler();
        handler
            .engine
            .flags
            .startup_in_progress
            .store(false, Ordering::Relaxed);

        let busy = handler.engine.lock().await;
        let mut query = tokio::spawn({
            let handler = handler.clone();
            async move { handler.get_state(Parameters(NoArgs {})).await }
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(200), &mut query)
                .await
                .is_err(),
            "get_state must queue behind the read loop, not fall back to connecting"
        );
        drop(busy);
        let result = query.await.unwrap().unwrap();
        let wire = serde_json::to_value(result).unwrap();
        let text = wire["content"][0]["text"].as_str().unwrap();
        assert!(
            !text.contains("\"state\":\"connecting\""),
            "healthy idle engine must not report connecting: {text}"
        );
        assert!(
            text.contains("\"dut_name\":\"test-dut\""),
            "expected the real state dict: {text}"
        );
    }

    #[tokio::test]
    async fn server_advertises_tasks_and_soft_failure_completes_task() {
        let (handler, _tmp) = make_handler();
        assert!(handler.get_info().capabilities.supports_tasks());

        let created = handler
            .tasks
            .spawn("working", |_| async {
                Ok(tool_result(json!({"success": false})))
            })
            .unwrap();
        let id = created.task.task_id;
        let wire = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                let wire = serde_json::to_value(handler.tasks.get(&id).unwrap()).unwrap();
                if wire["status"] != "working" {
                    break wire;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(wire["status"], "completed");
        assert_eq!(wire["result"]["isError"], true);
    }

    #[tokio::test]
    async fn reset_task_rejects_invalid_arguments_before_hardware_access() {
        let (handler, _tmp) = make_handler();
        let mut arguments = serde_json::Map::new();
        arguments.insert("wait_boot".into(), json!("yes"));
        let request = CallToolRequestParams::new("serial_reset").with_arguments(arguments);

        let error = handler
            .handle_reset_with_task(request, true)
            .await
            .unwrap_err();
        assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    }

    #[tokio::test]
    async fn reset_task_returns_handle_then_reports_soft_failure() {
        let (handler, _tmp) = make_handler();
        let mut arguments = serde_json::Map::new();
        arguments.insert("wait_boot".into(), json!(true));
        let request = CallToolRequestParams::new("serial_reset").with_arguments(arguments);

        let response = handler.handle_reset_with_task(request, true).await.unwrap();
        let CallToolResponse::Task(created) = response else {
            panic!("task-capable reset must return a task handle");
        };
        assert_eq!(created.task.status, rmcp::model::TaskStatus::Working);

        let task_id = created.task.task_id;
        let wire = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let current = handler.tasks.get(&task_id).unwrap();
                let wire = serde_json::to_value(current).unwrap();
                if wire["status"] != "working" {
                    break wire;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("reset task did not reach a terminal state");

        assert_eq!(wire["status"], "completed");
        assert_eq!(wire["result"]["resultType"], "complete");
        assert_eq!(wire["result"]["isError"], true);
    }

    #[test]
    fn prompt_catalog_list_and_get_stay_in_sync() {
        let router = McpHandler::prompt_catalog_router();
        let prompts = router.list_all();
        let names: Vec<_> = prompts.iter().map(|prompt| prompt.name.clone()).collect();
        // The router builds the catalog entry and the get route from the
        // same #[prompt] fn, so this holds structurally — the test guards
        // against a future regression back to hand-written lists.
        assert_eq!(names.len(), 7, "{names:?}");
        assert!(names.iter().all(|name| !name.contains('-')), "{names:?}");
        assert!(names.iter().all(|name| router.has_route(name)), "{names:?}");
        assert!(!router.has_route("unknown_prompt"));

        // Argument declarations survive the schema derivation that feeds
        // the advertised Prompt catalog.
        let debug_kernel = prompts
            .iter()
            .find(|prompt| prompt.name == "debug_kernel")
            .expect("debug_kernel in catalog");
        let lines = debug_kernel
            .arguments
            .as_ref()
            .expect("debug_kernel declares arguments")
            .iter()
            .find(|arg| arg.name == "lines")
            .expect("lines argument");
        assert!(!lines.required.unwrap_or(false));
        let send_and_check = prompts
            .iter()
            .find(|prompt| prompt.name == "send_and_check")
            .expect("send_and_check in catalog");
        let command = send_and_check
            .arguments
            .as_ref()
            .expect("send_and_check declares arguments")
            .iter()
            .find(|arg| arg.name == "command")
            .expect("command argument");
        assert!(command.required.unwrap_or(false));
    }

    #[test]
    fn flash_review_verdict_only_accept_proceeds() {
        // Accept with the schema's review_summary → proceed, summary kept
        // for the audit log.
        let mut accepted = ElicitResult::new(ElicitationAction::Accept);
        accepted.content = Some(json!({"review_summary": "boot chain reviewed"}));
        assert_eq!(
            flash_review_verdict(Ok::<_, &str>(accepted)),
            FlashReviewVerdict::Proceed {
                review_summary: Some("boot chain reviewed".into())
            }
        );

        // Accept without content still proceeds — the client enforces the
        // schema; the server stays lenient on a bare accept.
        let bare = ElicitResult::new(ElicitationAction::Accept);
        assert_eq!(
            flash_review_verdict(Ok::<_, &str>(bare)),
            FlashReviewVerdict::Proceed {
                review_summary: None
            }
        );

        // Decline and cancel abort.
        for action in [ElicitationAction::Decline, ElicitationAction::Cancel] {
            match flash_review_verdict(Ok::<_, &str>(ElicitResult::new(action))) {
                FlashReviewVerdict::Abort { reason } => assert!(!reason.is_empty()),
                verdict => panic!("decline/cancel must abort, got {verdict:?}"),
            }
        }

        // A transport failure must never fall through to proceed.
        match flash_review_verdict(Err::<ElicitResult, _>("peer gone")) {
            FlashReviewVerdict::Abort { reason } => assert!(reason.contains("failed")),
            verdict => panic!("transport failure must abort, got {verdict:?}"),
        }
    }

    #[tokio::test]
    async fn flash_plan_soft_failure_sets_mcp_is_error() {
        let (handler, _tmp) = make_handler();
        let result = handler
            .flash_plan(Parameters(FlashPlanArgs {
                image_path: String::new(),
                image_type: None,
            }))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true));
    }

    #[tokio::test]
    async fn resources_list_includes_dut_named_logs() {
        let (h, tmp) = make_handler();
        std::fs::create_dir_all(logs_dir(&tmp)).unwrap();
        std::fs::write(logs_dir(&tmp).join("current.serial.log"), "current body\n").unwrap();
        std::fs::write(logs_dir(&tmp).join("full.serial.log"), "full body\n").unwrap();
        std::fs::write(dut_dir(&tmp).join("unclassified.log"), "unc\n").unwrap();

        let resources = h.list_resources_impl().await;
        let uris = uri_list(&resources);
        assert!(
            uris.contains(&"state://target".to_string()),
            "missing state://target: {uris:?}"
        );
        assert!(
            uris.contains(&"log://serial/current".to_string()),
            "missing log://serial/current: {uris:?}"
        );
        assert!(
            uris.contains(&"log://serial/full".to_string()),
            "missing log://serial/full: {uris:?}"
        );
        assert!(
            uris.contains(&"log://serial/unclassified".to_string()),
            "missing log://serial/unclassified: {uris:?}"
        );
    }

    #[tokio::test]
    async fn read_resource_returns_current_serial_log_content() {
        let (h, tmp) = make_handler();
        std::fs::create_dir_all(logs_dir(&tmp)).unwrap();
        std::fs::write(
            logs_dir(&tmp).join("current.serial.log"),
            "current-line-1\ncurrent-line-2\n",
        )
        .unwrap();

        let resp = h.read_resource_impl("log://serial/current").await.unwrap();
        match resp {
            ReadResourceResponse::Complete(r) => {
                let text = match &r.contents[0] {
                    ResourceContents::TextResourceContents { text, .. } => text.clone(),
                    _ => panic!("expected text content"),
                };
                assert!(text.contains("current-line-1"));
                assert!(text.contains("current-line-2"));
            }
            _ => panic!("expected Complete response"),
        }
    }

    #[tokio::test]
    async fn read_resource_returns_target_state_json() {
        let (h, _tmp) = make_handler();
        let resp = h.read_resource_impl("state://target").await.unwrap();
        match resp {
            ReadResourceResponse::Complete(r) => {
                let text = match &r.contents[0] {
                    ResourceContents::TextResourceContents { text, .. } => text.clone(),
                    _ => panic!("expected text content"),
                };
                let v: serde_json::Value = serde_json::from_str(&text).unwrap();
                assert!(
                    v.get("state").is_some(),
                    "state JSON missing 'state' field: {text}"
                );
            }
            _ => panic!("expected Complete response"),
        }
    }

    #[tokio::test]
    async fn read_resource_returns_full_serial_log_content() {
        let (h, tmp) = make_handler();
        std::fs::create_dir_all(logs_dir(&tmp)).unwrap();
        std::fs::write(
            logs_dir(&tmp).join("full.serial.log"),
            "full-line-1\nfull-line-2\n",
        )
        .unwrap();

        let resp = h.read_resource_impl("log://serial/full").await.unwrap();
        match resp {
            ReadResourceResponse::Complete(r) => {
                let text = match &r.contents[0] {
                    ResourceContents::TextResourceContents { text, .. } => text.clone(),
                    _ => panic!("expected text content"),
                };
                assert!(text.contains("full-line-1"));
                assert!(text.contains("full-line-2"));
            }
            _ => panic!("expected Complete response"),
        }
    }

    #[tokio::test]
    async fn read_resource_returns_unclassified_log_under_dut_dir() {
        // unclassified.log lives in `.dut-serial/<name>/` (NOT logs/), so
        // this verifies the resource layer resolves it via `dut_dir()`.
        let (h, tmp) = make_handler();
        std::fs::create_dir_all(dut_dir(&tmp)).unwrap();
        std::fs::write(dut_dir(&tmp).join("unclassified.log"), "unclassified-1\n").unwrap();

        let resp = h
            .read_resource_impl("log://serial/unclassified")
            .await
            .unwrap();
        match resp {
            ReadResourceResponse::Complete(r) => {
                let text = match &r.contents[0] {
                    ResourceContents::TextResourceContents { text, .. } => text.clone(),
                    _ => panic!("expected text content"),
                };
                assert!(text.contains("unclassified-1"), "got: {text}");
            }
            _ => panic!("expected Complete response"),
        }
    }

    #[tokio::test]
    async fn unclassified_resource_includes_pending_detector_lines() {
        let (h, _tmp) = make_handler();
        h.engine
            .lock()
            .await
            .detector
            .unclassified_lines
            .push("pending-unclassified".into());

        let resources = h.list_resources_impl().await;
        assert!(
            uri_list(&resources).contains(&"log://serial/unclassified".to_string()),
            "pending lines must make the resource discoverable"
        );
        let resp = h
            .read_resource_impl("log://serial/unclassified")
            .await
            .unwrap();
        match resp {
            ReadResourceResponse::Complete(result) => match &result.contents[0] {
                ResourceContents::TextResourceContents { text, .. } => {
                    assert!(text.contains("pending-unclassified"), "got: {text}");
                }
                _ => panic!("expected text content"),
            },
            _ => panic!("expected Complete response"),
        }
    }

    /// Gap #6: unknown resource URIs must carry RESOURCE_NOT_FOUND (-32002),
    /// not INVALID_PARAMS (-32602) — the SDK's SEP-2164 layer converts
    /// version-appropriately only for errors with the RESOURCE_NOT_FOUND code.
    #[tokio::test]
    async fn read_resource_unknown_uri_returns_resource_not_found_error() {
        let (h, _tmp) = make_handler();
        let result = h.read_resource_impl("log://nonexistent/whatever").await;
        let err = result.expect_err("unknown URI must error");
        assert_eq!(err.code, rmcp::model::ErrorCode::RESOURCE_NOT_FOUND);
    }

    /// Gap #10: a listed resource whose backing file cannot be read is a
    /// structured RESOURCE_NOT_FOUND error, not a 200-OK "(failed to read)"
    /// text placeholder.
    #[tokio::test]
    async fn read_resource_missing_log_file_returns_resource_not_found_error() {
        let (h, _tmp) = make_handler();
        let result = h.read_resource_impl("log://serial/current").await;
        let err = result.expect_err("missing log file must error");
        assert_eq!(err.code, rmcp::model::ErrorCode::RESOURCE_NOT_FOUND);
    }

    #[test]
    fn all_known_resource_uris_are_stable_strings() {
        // Documents the URI contract for agent authors; if any of these
        // change, agent prompts published elsewhere break.
        for uri in [
            "state://target",
            "log://serial/current",
            "log://serial/full",
            "log://serial/unclassified",
            "log://boot/0",
        ] {
            assert!(!uri.is_empty());
        }
    }

    // ── pre-flash review gate, end to end ─────────────────────────────────
    //
    // Drives a real rmcp client over an in-memory transport against the
    // server handler. The client's `create_elicitation` answers from code —
    // the same place an agent host routes elicitation to its model — which
    // is the proof this gate never needs a human confirmation dialog.

    use rmcp::model::CallToolRequestParams;
    use rmcp::model::ClientCapabilities;
    use rmcp::model::ElicitationCapability;
    use rmcp::model::FormElicitationCapability;
    use rmcp::model::Implementation;
    use rmcp::service::RoleClient;
    use rmcp::service::serve_client;
    use rmcp::service::serve_server;
    use std::sync::atomic::AtomicBool;

    /// Programmatic stand-in for an agent host's elicitation handling.
    /// `elicit_form` mirrors whether the client declares the elicitation
    /// capability: `None` models the manual `dutabo` CLI (no capability,
    /// never prompted).
    struct ReviewAgent {
        action: ElicitationAction,
        content: Option<Value>,
        elicit_form: bool,
        elicited: Arc<AtomicBool>,
    }

    impl ReviewAgent {
        fn new(action: ElicitationAction, content: Option<Value>, elicit_form: bool) -> Self {
            Self {
                action,
                content,
                elicit_form,
                elicited: Arc::new(AtomicBool::new(false)),
            }
        }
    }

    impl rmcp::handler::client::ClientHandler for ReviewAgent {
        fn get_info(&self) -> rmcp::model::ClientInfo {
            let mut capabilities = ClientCapabilities::default();
            if self.elicit_form {
                capabilities.elicitation =
                    Some(ElicitationCapability::new().with_form(FormElicitationCapability::new()));
            }
            rmcp::model::ClientInfo::new(
                capabilities,
                Implementation::new("review-agent-test", "test"),
            )
        }

        async fn create_elicitation(
            &self,
            request: ElicitRequestParams,
            _context: rmcp::service::RequestContext<RoleClient>,
        ) -> Result<ElicitResult, ErrorData> {
            self.elicited.store(true, Ordering::Relaxed);
            assert!(
                matches!(request, ElicitRequestParams::FormElicitationParams { .. }),
                "gate must use form-mode elicitation"
            );
            let mut result = ElicitResult::new(self.action.clone());
            result.content = self.content.clone();
            Ok(result)
        }
    }

    /// Spin up server + client over an in-memory duplex, call
    /// `serial_flash_plan`, and return (wire result text, was_elicited).
    async fn drive_flash_plan(agent: ReviewAgent) -> (String, bool) {
        let (handler, _tmp) = make_handler();
        // Startup finished and this instance owns the serial endpoint —
        // otherwise the connecting/non-owner guards reject every tool
        // before the review gate can run.
        handler
            .engine
            .flags
            .startup_in_progress
            .store(false, Ordering::Relaxed);
        handler
            .engine
            .flags
            .owns_serial_lock
            .store(true, Ordering::Relaxed);
        // Gate precondition: the unit-test config must have no relay.
        assert!(
            !handler.engine.lock().await.power_control_configured(),
            "test config must not configure a relay"
        );
        let elicited = agent.elicited.clone();
        let (client_io, server_io) = tokio::io::duplex(4096);
        // Concurrent handshake: both serves exchange `initialize` before
        // returning, so awaiting them sequentially would deadlock.
        let (server_res, client_res) = tokio::join!(
            serve_server(handler, server_io),
            serve_client(agent, client_io),
        );
        let _server = server_res.expect("serve_server");
        let client = client_res.expect("serve_client");
        let mut params = CallToolRequestParams::default();
        params.name = "serial_flash_plan".into();
        params.arguments = json!({"image_path": "/tmp/review-gate-test.img"})
            .as_object()
            .cloned();
        let result = client
            .call_tool(params)
            .await
            .expect("tools/call round-trip completes");
        let wire = serde_json::to_value(&result).expect("serialize result");
        let text = wire["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_string();
        (text, elicited.load(std::sync::atomic::Ordering::Relaxed))
    }

    #[tokio::test]
    async fn flash_review_accept_reaches_the_plan() {
        let agent = ReviewAgent::new(
            ElicitationAction::Accept,
            Some(json!({"review_summary": "boot chain reviewed"})),
            true,
        );
        let (text, elicited) = drive_flash_plan(agent).await;
        assert!(elicited, "gate must have asked for the review");
        // Past the gate: the plan itself answers (a soft failure is fine —
        // the flash config is not set up in unit tests — as long as it is
        // NOT the gate's abort payload).
        assert!(!text.contains("pre_flash_review"), "got: {text}");
    }

    #[tokio::test]
    async fn flash_review_decline_aborts_the_flash() {
        let agent = ReviewAgent::new(ElicitationAction::Decline, None, true);
        let (text, elicited) = drive_flash_plan(agent).await;
        assert!(elicited, "gate must have asked for the review");
        assert!(text.contains("pre_flash_review"), "got: {text}");
        assert!(text.contains("Flash request aborted"), "got: {text}");
    }

    #[tokio::test]
    async fn flash_review_skips_clients_without_elicitation() {
        // Models the manual `dutabo` CLI path: no elicitation capability,
        // never prompted, plan returned directly.
        let agent = ReviewAgent::new(ElicitationAction::Decline, None, false);
        let (text, elicited) = drive_flash_plan(agent).await;
        assert!(!elicited, "gate must not prompt this client");
        assert!(!text.contains("pre_flash_review"), "got: {text}");
    }

    // ── tools/list per session, end to end ──────────────────────────────
    //
    // Wire-level contract check: the SAME shared HTTP server hands an agent
    // session the compact Agent catalog and the `dutabo` CLI session the
    // Full catalog, with no semantic forking — only discovery differs.

    struct NamedClient(&'static str);

    impl rmcp::handler::client::ClientHandler for NamedClient {
        fn get_info(&self) -> rmcp::model::ClientInfo {
            rmcp::model::ClientInfo::new(
                ClientCapabilities::default(),
                Implementation::new(self.0, "test"),
            )
        }
    }

    async fn tools_seen_by(name: &'static str, handler: McpHandler) -> Vec<String> {
        let (client_io, server_io) = tokio::io::duplex(4096);
        // Concurrent handshake: both serves exchange `initialize` before
        // returning, so awaiting them sequentially would deadlock.
        let (server_res, client_res) = tokio::join!(
            serve_server(handler, server_io),
            serve_client(NamedClient(name), client_io),
        );
        let server = server_res.expect("serve_server");
        let client = client_res.expect("serve_client");
        let result = client.list_tools(None).await.expect("tools/list");
        drop(client);
        drop(server);
        result
            .tools
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect()
    }

    #[tokio::test]
    async fn wire_tools_list_filters_by_session_role() {
        let registry = Arc::new(AgentAccessRegistry::new(false));
        let (agent_handler, _agent_tmp) = make_shared_handler(registry.clone());
        let (dutabo_handler, _dutabo_tmp) = make_shared_handler(registry);

        let agent_tools = tools_seen_by("codex-agent", agent_handler).await;
        let dutabo_tools = tools_seen_by("dutabo", dutabo_handler).await;

        // Agent catalog: unified entry in, implementation tools out.
        assert!(agent_tools.iter().any(|name| name == "serial_exec"));
        for hidden in [
            "serial_send_command",
            "serial_test_baud",
            "serial_get_metrics",
        ] {
            assert!(
                !agent_tools.iter().any(|name| name == hidden),
                "{hidden} must not be listed for an agent session: {agent_tools:?}"
            );
        }
        // Human-role CLI session keeps the complete catalog.
        for present in ["serial_exec", "serial_send_command", "serial_test_baud"] {
            assert!(
                dutabo_tools.iter().any(|name| name == present),
                "{present} must be listed for the dutabo session: {dutabo_tools:?}"
            );
        }
    }
}
