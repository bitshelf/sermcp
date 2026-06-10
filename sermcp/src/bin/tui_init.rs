//! `dutabo init` TUI — the ratatui FORM front for the headless wizard core.
//!
//! Architecture: the core (init.rs) exposes a declarative FORM SCHEMA
//! (`FORM_GROUPS` / `FormSchema` / `FieldInstance`) plus a shared pipeline
//! (`prepare_init` → answers → `finish_init`). This module replaces the
//! middle stage for the TTY front: instead of one-question-per-screen, the
//! schema renders as ONE single-screen device-management panel:
//! a HOST TAB BAR on top (`dev host1  x │ dev host2  x │ +`), the
//! DEVELOPMENT HOST panel below it (editable ip/user/pass with brackets,
//! `[Other ▼]` expanding the 25 advanced GLOBAL fields INSIDE the panel),
//! the DUTs panel (its own tab
//! bar `dut1  x │ +` + a two-column content box for the ONE selected
//! DUT), and a fixed footer of key-hint chips. F2 saves DIRECTLY
//! (resolve_answers → finish_init auto-write; no Review screen).
//! Esc quits immediately; 'q' quits too unless a text field's non-empty
//! buffer owns the letter (mid-edit 'q' is data). Del deletes the current
//! host/DUT (when a tab/button is focused — on a field it keeps the
//! editor's delete-at-cursor). Enter activates/edits, never submits.
//!
//! Layered so every layer is headless-testable:
//! - PURE model + layout + key handler: `FormState`, `derive_fields`,
//!   `handle_form_key`, `form_layout`, `render_form` — no /dev/tty, no raw
//!   mode; tested against ratatui's `TestBackend`, synthetic `KeyEvent`s
//!   and `MouseEvent`s. `form_layout` is the SINGLE layout function shared
//!   by render and mouse hit-testing — click targets match pixels by
//!   construction.
//! - `FormTui<B: Backend, E: EventSource>` — the event loop (sync, no
//!   tokio) generic over Backend + EventSource so the FULL flow (form →
//!   F2 save) runs headless in tests. It implements `init::PromptIO`:
//!   `input`/`confirm` are unreachable (Err — the form transport asks no
//!   questions); `finish` is the auto-write no-op (Ok — the write happens
//!   in `finish_init` after the form returns its answers).
//! - `PlainPromptIO` — the byte-identical plain-line fallback (piped
//!   stdin, TERM=dumb, tiny terminal, TUI startup failure).
//!
//! NOTE: ratatui 0.30.x (the 0.30 crate split) does not ship the `TextArea`
//! widget, so the single-line editor is implemented in the pure model
//! (buffer + cursor + horizontal view offset) with the same emacs-style
//! editing keys as the previous prompt UI.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::path::Path;

use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, List, ListItem, ListState, Paragraph};
use ratatui::{Frame, Terminal};

use sermcp::config::NumberOrString;
use sermcp::init::PreparedInit;
use sermcp::init::{
    self, AdvancedGate, BusyDecision, EndpointConflict, FORM_GROUPS, FieldInstance, FieldKey,
    FieldKind, FormSchema, InitOptions, NamePolicy, NoteKind, PromptIO, Repeat, SchemaTarget,
    WizardAnswers, WizardOutcome, endpoint_conflict_line, finish_init, materialize_group_fields,
    next_free_dut_name, prepare_init, resolve_answers,
};

// Flash-box probes (the TEST button's device-list check) live in their own
// module — `flash::run_list_devices_probe` + the read-only row key.
mod download;
mod flash;
mod pane;
mod project;
use project::{SkillPicker, load_skill_state};

// ── Form state model (pure, headless-testable) ────────────────────────

/// The focusable widgets outside the field editors: the tab-bar `+`/`x`
/// affordances and the host panel's `[Other ▼]` expander. Add/Remove
/// mutate the structure directly; ToggleOther expands the advanced global
/// fields inside the DEVELOPMENT HOST panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonId {
    /// A `skills:` footer chip (path segment or the agent/next chip).
    Skill(usize),
    /// Host tab bar's trailing `+`: add another dev host (auto-selected).
    AddHost,
    /// DUT tab bar's trailing `+`: add a DUT to the given host
    /// (auto-selected).
    AddDut(usize),
    /// A host tab's `x`: remove that host (its DUTs go with it).
    RemoveHost(usize),
    /// A DUT tab's `x`: remove that DUT from its host.
    RemoveDut(usize, usize),
    /// `[Other ▼]` / `[Other ▲]` in the DEVELOPMENT HOST panel's top-right
    /// corner: expand/collapse the advanced global fields inside the panel.
    ToggleOther,
    /// `[Save]` at the footer: save WITHOUT exiting the wizard.
    Save,
    /// `[Save & Exit]` at the footer's right edge: save and exit (F2).
    SaveExit,
    /// `[Exit]` at the footer's right edge: quit WITHOUT saving — the
    /// same clean abort as Esc/q (nothing written, exit 0).
    Exit,
    /// `[Test]` — dry-run validation + bounded connectivity probes
    /// ; errors surface promptly, nothing is
    /// written.
    Test,
}

/// Where the keyboard focus sits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FocusSlot {
    /// A host tab card (the ←/→ group it belongs to: host tabs).
    HostTab(usize),
    /// A DUT tab card of the selected host (←/→ group: DUT tabs).
    DutTab(usize),
    Field(FieldKey),
    Button(ButtonId),
}

/// What a keypress / mouse click did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormAction {
    /// Whole-form save AND exit (F2 / [Save & Exit]).
    Submit,
    /// Save WITHOUT exiting — write the config, keep editing
    /// (deliberate: [Save] saves without exiting).
    SaveOnly,
    /// Run the TEST pass (validation + probes) — nothing written.
    Test,
    /// User abort (Ctrl-C, Esc/q) — nothing written.
    Abort(String),
    /// Nothing submitted; re-render.
    Continue,
}

/// Per-field probe result (TEST button).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ProbeMark {
    Pending,
    Ok,
    Warn,
    Err,
    Skip,
}

impl ProbeMark {
    pub fn symbol(&self) -> &'static str {
        match self {
            ProbeMark::Pending => "…",
            ProbeMark::Ok => "✓",
            ProbeMark::Warn => "⚠",
            ProbeMark::Err => "✗",
            // Design §31 status language: ✓ PASS / ✗ FAIL / » SKIP.
            ProbeMark::Skip => "»",
        }
    }
    pub fn color(&self) -> Color {
        match self {
            ProbeMark::Pending => Color::DarkGray,
            ProbeMark::Ok => Color::Rgb(163, 190, 140),
            ProbeMark::Warn => Color::Yellow,
            ProbeMark::Err => Color::Red,
            ProbeMark::Skip => Color::DarkGray,
        }
    }
}

/// A pending TEST run: the worker sends per-field marks; the deadline
/// bounds the UI wait.
#[derive(Debug)]
pub struct TestRun {
    pub rx: std::sync::mpsc::Receiver<(FieldKey, ProbeMark, Option<String>)>,
    pub deadline: std::time::Instant,
    /// true = the full TEST pass (aggregate summary, endpoint refresh,
    /// spawned-MCP teardown). false = a per-row quick action: the mark
    /// lands on its row only — no header summary, and the MCP STAYS
    /// (the pane may be attached to it).
    pub summary: bool,
}

/// All form state. `values` IS the buffer: UPDATE prefills every existing
/// index from the current config; indices beyond the existing counts start
/// EMPTY with dim default hints (CREATE-like). Per-field editor state
/// survives count-driven rebuilds.
#[derive(Debug)]
pub struct FormState {
    pub pane: pane::Pane,
    pub skill_catalog: sermcp::skill_catalog::Catalog,
    pub setup_error: Option<String>,
    /// The selected skill path (dynamic selector levels; arbitrary
    /// depth). The ONLY platform context in the form — the download
    /// provider resolves from it.
    pub skills_path: Vec<String>,
    /// The selected agents for the leaf; several entries = the All
    /// choice expanded.
    pub skills_agents: Vec<String>,
    pub skill_picker: Option<SkillPicker>,
    pub schema: FormSchema,
    pub fields: Vec<FieldInstance>,
    pub values: BTreeMap<FieldKey, String>,
    /// (cursor, view_offset) per field — cursor normalized in the renderer.
    pub editors: BTreeMap<FieldKey, (usize, usize)>,
    pub errors: BTreeMap<FieldKey, String>,
    pub host_count: usize,
    pub dut_counts: Vec<usize>,
    /// The CURRENT host: its tab highlights, the DEVELOPMENT HOST panel
    /// and the DUTs panel both render it. Updated by `set_focus` whenever
    /// keyboard/mouse focus lands on any of a host's slots, and clamped on
    /// every structural mutation.
    pub selected_host: usize,
    /// The CURRENT DUT of the selected host (`None` when it has 0 DUTs).
    /// Exactly ONE DUT's config is ever visible.
    pub selected_dut: Option<usize>,
    /// `[Other ▼]` expanded: the advanced global fields render inside the
    /// DEVELOPMENT HOST panel (default false; --advanced Forced starts
    /// expanded).
    pub other_expanded: bool,
    /// Footer key-hint chips are HIDDEN by default; '?' toggles them.
    pub footer_hints_visible: bool,
    /// True while the host/DUT divider is being dragged.
    pub divider_drag: bool,
    /// True while the serial-terminal pane's top border is dragged.
    pub pane_drag: bool,
    /// User-pinned terminal pane height (rows incl. borders); the DUT
    /// panel cedes the space (and scrolls) when the pin exceeds it.
    pub pane_h_pin: Option<u16>,
    /// User-pinned host panel height (pixels; the drag writes it).
    /// Cleared by toggle_other / add/remove host/DUT / resize.
    pub host_h_pin: Option<u16>,
    /// Per-field probe marks from the last TEST run; any
    /// edit clears them coarsely.
    /// Endpoint-conflict snapshot (see `refresh_endpoint_conflicts`):
    /// which of the form's serial endpoints are held by a live MCP right
    /// now. Surfaced INLINE in the DUT panel title so the operator sees the
    /// holder while EDITING — never for the first time at save.
    pub endpoint_conflicts: Vec<sermcp::init::EndpointConflict>,
    /// PIDs spawned or borrowed by this form's active TEST. Their endpoint locks are
    /// expected ownership, not a conflict warning.
    pub test_owned_pids: std::collections::BTreeSet<u32>,
    /// Lock dir the snapshot above was read from (the existing config's
    /// override wins; default `/tmp/sermcp/locks`).
    pub endpoint_lock_dir: String,
    pub probes: BTreeMap<FieldKey, ProbeMark>,
    /// Probe DETAIL text per field (the flash device-list probe shows the
    /// newly-appeared device lines here).
    pub probe_details: BTreeMap<FieldKey, String>,
    /// A pending TEST run: the worker result channel + the deadline.
    pub test_run: Option<TestRun>,
    pub relay_action_active: bool,
    /// The last TEST run's summary line (worst-severity colored).
    pub test_summary: Option<(String, ProbeMark)>,
    pub focus: FocusSlot,
    /// Scroll in cells — applies to the DUT content box and the expanded
    /// advanced block (the tab bars, panel headers and the footer never
    /// scroll).
    pub scroll: usize,
    pub gate: bool,
    /// Shared yank buffer for the editor's kill commands.
    pub kill: String,
    /// Header status line ("dutabo init — <path>").
    pub target_label: String,
    /// Amber append-only note (count shrink below the existing config).
    pub note: Option<(NoteKind, String)>,
    /// Open controller-picker modal (Enter/click on a Select row). While
    /// open it owns every key: ↑/↓ move, Enter pick, Esc cancel.
    pub picker: Option<ControllerPicker>,
    /// Occupancy-gate modal: open while `busy_endpoints` asks
    /// how to proceed with serial endpoints held by another live MCP. Owns
    /// every key like the picker; Enter confirms the cursor, Esc aborts.
    pub busy_gate: Option<BusyGate>,
    /// The exit-family button (Save & Exit / Exit) whose click is waiting
    /// for the mouse RELEASE so the complete SGR sequence is consumed
    /// before terminal teardown.
    pub pending_exit_click: Option<ButtonId>,
}

/// The occupancy-gate modal state: the conflicts to display plus a cursor
/// over [continue | abort]. Read-only diagnostics — the holder's session
/// is never signaled or stopped here.
#[derive(Debug, Clone)]
pub struct BusyGate {
    pub conflicts: Vec<EndpointConflict>,
    /// 0 = continue writing, 1 = abort. Defaults to ABORT (the safe
    /// choice: writing is skipped unless the user deliberately proceeds).
    pub cursor: usize,
}

/// The controller (Select field) picker modal state.
#[derive(Debug, Clone)]
pub struct ControllerPicker {
    /// The Select field the choice applies to (control.dev_ctrl).
    pub key: FieldKey,
    /// The field's options, verbatim.
    pub options: Vec<String>,
    /// Highlighted option index.
    pub cursor: usize,
}

impl FormState {
    /// Fresh form for a prepared schema. UPDATE prefills every existing
    /// index (values), CREATE starts with empty buffers and dim hints.
    /// The structure is BUTTON-DRIVEN: the `+`/`x` tabs mutate
    /// `host_count`/`dut_counts` directly.
    pub fn new(schema: FormSchema, target_label: String) -> Self {
        // CREATE defaults to 1 host / 1 DUT; UPDATE reproduces the exact
        // existing shape (0 hosts allowed — the DUT section disappears).
        let host_count = schema.host_count_default;
        let mut dut_counts: Vec<usize> = Vec::new();
        for hi in 0..host_count {
            let d = schema
                .host_defaults
                .get(hi)
                .map(|h| h.dut_count)
                .unwrap_or(1);
            dut_counts.push(d);
        }
        let root = Path::new(&target_label).parent().unwrap_or(Path::new("."));
        let (skill_catalog, setup_error, skills_path, skills_agents) = load_skill_state(root);
        let mut state = Self {
            skill_catalog,
            setup_error,
            skills_path,
            skills_agents,
            skill_picker: None,
            pane: pane::Pane::default(),
            schema,
            fields: Vec::new(),
            values: BTreeMap::new(),
            editors: BTreeMap::new(),
            errors: BTreeMap::new(),
            host_count,
            dut_counts,
            selected_host: 0,
            selected_dut: None,
            other_expanded: true,
            footer_hints_visible: false,
            divider_drag: false,
            pane_drag: false,
            pane_h_pin: None,
            host_h_pin: None,
            endpoint_conflicts: Vec::new(),
            test_owned_pids: std::collections::BTreeSet::new(),
            endpoint_lock_dir: sermcp::lock_manager::default_lock_dir(),
            probes: BTreeMap::new(),
            probe_details: BTreeMap::new(),
            test_run: None,
            relay_action_active: false,
            test_summary: None,
            focus: FocusSlot::Field(FieldKey::HostCount),
            scroll: 0,
            gate: false,
            kill: String::new(),
            target_label,
            note: None,
            picker: None,
            busy_gate: None,
            pending_exit_click: None,
        };
        state.gate = match state.schema.advanced_gate {
            AdvancedGate::Skip => false,
            AdvancedGate::Forced => true,
            AdvancedGate::Confirm { default } => default,
        };
        // The advanced ([Other]) section is expanded by DEFAULT
        // ; [Other ▲] collapses it.
        state.fields = derive_fields(&state.schema, state.host_count, &state.dut_counts);
        state.sync_count_defaults();
        // Prefill (UPDATE existing indices; CREATE → None → empty buffers).
        let keys: Vec<FieldKey> = state.fields.iter().map(|f| f.key.clone()).collect();
        for key in keys {
            if let Some(v) = state.prefill_for(&key) {
                state.values.insert(key, v);
            }
        }
        state.seed_relay_gates();
        state.recompute_name_hints();
        state.selected_dut = state
            .dut_counts
            .first()
            .copied()
            .filter(|c| *c > 0)
            .map(|_| 0);
        if state.host_count == 0 {
            state.set_focus(FocusSlot::Button(ButtonId::AddHost));
        } else {
            state.set_focus(FocusSlot::HostTab(0));
        }
        if let Some(error) = &state.setup_error {
            state.note = Some((NoteKind::Error, error.clone()));
        }
        state
    }

    /// Every derived DUT gets a relay gate buffer ("no" unless prefilled).
    /// Re-read which of the form's host×DUT serial endpoints are held by a
    /// live MCP (see `init::held_endpoint_conflict`). Called at form entry and
    /// after each TEST run; the save-time occupancy gate re-reads the locks as
    /// the final authority.
    pub fn refresh_endpoint_conflicts(&mut self) {
        let mut out = Vec::new();
        for hi in 0..self.host_count {
            let Some(ip) = self.values.get(&FieldKey::HostIp(hi)).cloned() else {
                continue;
            };
            if ip.trim().is_empty() {
                continue;
            }
            for di in 0..self.dut_counts.get(hi).copied().unwrap_or(0) {
                let Some(port) = self.values.get(&FieldKey::DutPort(hi, di)).cloned() else {
                    continue;
                };
                if port.trim().is_empty() {
                    continue;
                }
                if let Some(c) =
                    sermcp::init::held_endpoint_conflict(&ip, port.trim(), &self.endpoint_lock_dir)
                {
                    out.push(c);
                }
            }
        }
        self.endpoint_conflicts = out;
    }
    fn seed_relay_gates(&mut self) {
        let keys: Vec<FieldKey> = self
            .fields
            .iter()
            .filter(|f| matches!(f.key, FieldKey::DutRelayGate(..)))
            .map(|f| f.key.clone())
            .collect();
        for key in keys {
            self.values.entry(key).or_insert_with(|| "no".into());
        }
    }

    /// The UPDATE prefill map: Some(value) for every EXISTING index (the
    /// form starts from the current config — an all-default F2 save stays
    /// byte-identical); None for indices beyond the existing counts (they
    /// start EMPTY with dim hints, CREATE-like) and for CREATE.
    fn prefill_for(&self, key: &FieldKey) -> Option<String> {
        let SchemaTarget::Update(existing) = &self.schema.target else {
            return None;
        };
        match key {
            FieldKey::HostCount => Some(self.schema.host_count_default.to_string()),
            FieldKey::DutCount(hi) => existing
                .dev_hosts
                .get(*hi)
                .map(|h| h.duts.len().max(1).to_string()),
            FieldKey::HostIp(hi) => existing.dev_hosts.get(*hi).and_then(|h| h.ip.clone()),
            FieldKey::HostUser(hi) => existing.dev_hosts.get(*hi).and_then(|h| h.user.clone()),
            FieldKey::HostPass(hi) => existing.dev_hosts.get(*hi).and_then(|h| h.pass.clone()),
            FieldKey::HostName(hi) => existing
                .dev_hosts
                .get(*hi)
                .and_then(|h| h.host_name.clone()),
            FieldKey::DutName(hi, di) => existing
                .dev_hosts
                .get(*hi)
                .and_then(|h| h.duts.get(*di))
                .map(|d| d.dut_name.clone()),
            FieldKey::DutPort(hi, di) => existing
                .dev_hosts
                .get(*hi)
                .and_then(|h| h.duts.get(*di))
                .and_then(|d| {
                    d.serial
                        .as_ref()
                        .and_then(|s| s.port.as_ref())
                        .map(NumberOrString::to_value_string)
                }),
            FieldKey::DutLoginUser(hi, di) => existing
                .dev_hosts
                .get(*hi)
                .and_then(|h| h.duts.get(*di))
                .and_then(|d| d.target.as_ref().and_then(|t| t.login_user.clone())),
            FieldKey::DutRelayGate(hi, di) => existing
                .dev_hosts
                .get(*hi)
                .and_then(|h| h.duts.get(*di))
                .map(|d| {
                    if d.relay.is_some() {
                        "yes".to_string()
                    } else {
                        "no".to_string()
                    }
                }),
            FieldKey::DutRelayType(hi, di) => existing
                .dev_hosts
                .get(*hi)
                .and_then(|h| h.duts.get(*di))
                .and_then(|d| d.relay.as_ref().and_then(|r| r.relay_type.clone()))
                .map(|relay_type| normalize_relay_type(&relay_type)),
            FieldKey::DutRelayPort(hi, di) => existing
                .dev_hosts
                .get(*hi)
                .and_then(|h| h.duts.get(*di))
                .and_then(|d| d.relay.as_ref().and_then(|r| r.port.map(|p| p.to_string()))),
            FieldKey::DutRelayReset(hi, di) => existing
                .dev_hosts
                .get(*hi)
                .and_then(|h| h.duts.get(*di))
                .and_then(|d| {
                    d.relay
                        .as_ref()
                        .and_then(|r| r.reset_ch.map(|c| c.to_string()))
                }),
            FieldKey::DutRelayDownload(hi, di) => existing
                .dev_hosts
                .get(*hi)
                .and_then(|h| h.duts.get(*di))
                .and_then(|d| {
                    d.relay
                        .as_ref()
                        .and_then(|r| r.download_ch.map(|c| c.to_string()))
                }),
            FieldKey::DutRelayPower(hi, di) => existing
                .dev_hosts
                .get(*hi)
                .and_then(|h| h.duts.get(*di))
                .and_then(|d| {
                    d.relay
                        .as_ref()
                        .and_then(|r| r.power_ch.map(|c| c.to_string()))
                }),
            FieldKey::DutRelayResetTimeMs(hi, di) => existing
                .dev_hosts
                .get(*hi)
                .and_then(|h| h.duts.get(*di))
                .and_then(|d| {
                    d.relay
                        .as_ref()
                        .and_then(|r| r.reset_time_ms.map(|t| t.to_string()))
                }),
            FieldKey::DutRelayPowerOffTimeMs(hi, di) => existing
                .dev_hosts
                .get(*hi)
                .and_then(|h| h.duts.get(*di))
                .and_then(|d| {
                    d.relay
                        .as_ref()
                        .and_then(|r| r.power_off_time_ms.map(|t| t.to_string()))
                }),
            FieldKey::Advanced(k) => self.schema.advanced_current.get(*k).cloned(),
            // DUT-owned advanced: prefill ONLY from the DUT's own override
            // sections — never the global fallback (an untouched UPDATE
            // must not copy fallback values into per-DUT sections).
            FieldKey::DutAdvanced(hi, di, k) => existing
                .dev_hosts
                .get(*hi)
                .and_then(|h| h.duts.get(*di))
                .and_then(|d| init::dut_advanced_current(d).get(*k).cloned()),
        }
    }

    /// Existing shape for the shrink note: (hosts, per-host dut counts).
    fn existing_shape(&self) -> Option<(usize, Vec<usize>)> {
        match &self.schema.target {
            SchemaTarget::Create => None,
            SchemaTarget::Update(c) => Some((
                c.dev_hosts.len(),
                c.dev_hosts.iter().map(|h| h.duts.len()).collect(),
            )),
        }
    }

    fn gate_forced(&self) -> bool {
        self.schema.advanced_gate == AdvancedGate::Forced
    }

    /// The full field list in VISUAL order — the order the plain walk asks
    /// them: per host (in order) its host fields, then per-DUT the DUT
    /// content boxes (the dev_ctrl section — relay sub-fields — only while
    /// the backend is not none/disabled; the gate row is gone), then the
    /// advanced fields last (absent entirely under --quick). Validation
    /// (`resolve_answers`) and `focus_first_error` walk this order.
    pub fn field_order(&self) -> Vec<FieldKey> {
        let mut v = Vec::new();
        for hi in 0..self.host_count {
            v.push(FieldKey::HostIp(hi));
            v.push(FieldKey::HostUser(hi));
            v.push(FieldKey::HostPass(hi));
            v.push(FieldKey::HostName(hi));
            for di in 0..self.dut_counts.get(hi).copied().unwrap_or(0) {
                // Disabled relay placeholder rows must not stay in the
                // VALIDATION walk either, or an error inside the dimmed
                // section would focus an unfocusable rect (per-DUT,
                // the filter mirrors `dut_zone_fields`).
                v.extend(
                    dut_boxes(self, hi, di)
                        .into_iter()
                        .flat_map(|b| b.fields)
                        .filter(|k| !relay_row_disabled(self, k) && !flash::is_devices_key(k)),
                );
            }
        }
        if self.schema.advanced_gate != AdvancedGate::Skip {
            for f in &self.fields {
                if let FieldKey::Advanced(_) = f.key {
                    v.push(f.key.clone());
                }
            }
        }
        v
    }

    /// The host fields of the SELECTED host in panel order (advanced fields
    /// appended when `[Other ▼]` is expanded) — the Up/Down cycle of the
    /// Host Fields zone.
    pub fn host_zone_fields(&self) -> Vec<FieldKey> {
        if self.host_count == 0 {
            return Vec::new();
        }
        let hi = self.selected_host;
        let v = vec![
            FieldKey::HostIp(hi),
            FieldKey::HostUser(hi),
            FieldKey::HostPass(hi),
            FieldKey::HostName(hi),
        ];
        v
    }

    /// The SELECTED DUT's fields in the content box's CANONICAL order (the
    /// `dut_boxes` box-major order — identical at every column count; the
    /// two-column layout pins whole boxes to columns so this is the
    /// width-independent reading order) — the Up/Down cycle of the DUT
    /// Fields zone.
    pub fn dut_zone_fields(&self) -> Vec<FieldKey> {
        let Some(di) = self.selected_dut else {
            return Vec::new();
        };
        // Disabled relay placeholder rows occupy their layout slot but are
        // NEVER focusable — navigation walks the live fields only.
        dut_boxes(self, self.selected_host, di)
            .into_iter()
            .flat_map(|b| b.fields)
            .filter(|k| !relay_row_disabled(self, k) && !flash::is_devices_key(k))
            .collect()
    }

    /// Flat Tab/Shift-Tab order, ZONE-ordered with wrapping:
    /// [HostTab(selected) → AddHost] → the selected host's fields →
    /// [ToggleOther → advanced fields (when expanded)] → [DutTab(selected)
    /// → AddDut] → the selected DUT's fields. Footer buttons stay LAST.
    /// Click and keyboard both follow it.
    pub fn focus_order(&self) -> Vec<FocusSlot> {
        let mut slots: Vec<FocusSlot> = Vec::new();
        if self.host_count == 0 {
            slots.push(FocusSlot::Button(ButtonId::AddHost));
        } else {
            let hi = self.selected_host;
            slots.push(FocusSlot::HostTab(hi));
            slots.push(FocusSlot::Button(ButtonId::AddHost));
            for key in [
                FieldKey::HostIp(hi),
                FieldKey::HostUser(hi),
                FieldKey::HostPass(hi),
                FieldKey::HostName(hi),
            ] {
                slots.push(FocusSlot::Field(key));
            }
            match self.selected_dut {
                Some(di) => {
                    slots.push(FocusSlot::DutTab(di));
                    slots.push(FocusSlot::Button(ButtonId::AddDut(hi)));
                    for key in self.dut_zone_fields() {
                        slots.push(FocusSlot::Field(key));
                    }
                }
                None => slots.push(FocusSlot::Button(ButtonId::AddDut(hi))),
            }
        }
        for chip in 0..self.skill_chips().len() {
            slots.push(FocusSlot::Button(ButtonId::Skill(chip)));
        }
        slots.push(FocusSlot::Button(ButtonId::Save));
        slots.push(FocusSlot::Button(ButtonId::Test));
        slots.push(FocusSlot::Button(ButtonId::SaveExit));
        slots.push(FocusSlot::Button(ButtonId::Exit));
        slots
    }
    // ── Structure mutations (the +/x tabs, Del) ────────────────────────

    /// `+` in the host tab bar: add another dev host and AUTO-SELECT it
    /// (its tab becomes current). The new host starts with ONE default DUT
    /// , also auto-selected.
    pub fn add_host(&mut self) {
        self.host_h_pin = None;
        self.pane_h_pin = None;
        if self.host_count >= self.schema.host_count_max {
            self.note = Some((
                NoteKind::Error,
                format!("dev host limit {} reached", self.schema.host_count_max),
            ));
            return;
        }
        self.host_count += 1;
        self.dut_counts.push(1);
        self.rebuild(FieldKey::HostIp(self.host_count - 1));
        self.selected_host = self.host_count - 1;
        self.selected_dut = Some(0);
        self.set_focus(FocusSlot::HostTab(self.selected_host));
    }

    /// `x` on a host tab / Del: remove the host AND its DUTs. Indices above
    /// the removed tab shift down; typed values move with their hosts (keys
    /// renumbered) so nothing typed for surviving hosts is lost. The
    /// selected host follows the renumber, clamped by `rebuild`.
    pub fn remove_host(&mut self, hi: usize) {
        if hi >= self.host_count {
            return;
        }
        self.host_h_pin = None;
        self.pane_h_pin = None;
        self.renumber_hosts_above(hi);
        self.host_count -= 1;
        self.dut_counts.remove(hi);
        if hi < self.selected_host {
            self.selected_host -= 1;
        }
        self.rebuild(FieldKey::HostCount);
        if self.host_count == 0 {
            self.set_focus(FocusSlot::Button(ButtonId::AddHost));
        } else {
            self.set_focus(FocusSlot::HostTab(self.selected_host));
        }
    }

    /// `+` in the DUT tab bar: add a DUT to THAT host and AUTO-SELECT it
    /// (its tab becomes current).
    pub fn add_dut(&mut self, hi: usize) {
        if hi >= self.host_count {
            return;
        }
        self.host_h_pin = None;
        self.pane_h_pin = None;
        while self.dut_counts.len() <= hi {
            self.dut_counts.push(0);
        }
        self.dut_counts[hi] += 1;
        let di = self.dut_counts[hi] - 1;
        self.rebuild(FieldKey::DutName(hi, di));
        self.selected_host = hi;
        self.selected_dut = Some(di);
        self.set_focus(FocusSlot::DutTab(di));
    }

    /// `x` on a DUT tab / Del: remove this DUT from its host. Later DUTs of
    /// the same host shift down; their typed values move with them.
    /// `selected_dut` is clamped (falls back to the shifted-in DUT).
    pub fn remove_dut(&mut self, hi: usize, di: usize) {
        self.host_h_pin = None;
        self.pane_h_pin = None;
        if hi >= self.host_count || di >= self.dut_counts.get(hi).copied().unwrap_or(0) {
            return;
        }
        self.renumber_duts_above(hi, di);
        self.dut_counts[hi] -= 1;
        self.rebuild(FieldKey::DutCount(hi));
        if self.selected_host == hi {
            let count = self.dut_counts.get(hi).copied().unwrap_or(0);
            if count > 0 {
                let d = self.selected_dut.unwrap_or(0).min(count - 1);
                self.set_focus(FocusSlot::DutTab(d));
            } else {
                self.set_focus(FocusSlot::Button(ButtonId::AddDut(hi)));
            }
        }
    }

    /// `[Other ▼]` / `[Other ▲]`: expand/collapse the advanced global
    /// fields inside the DEVELOPMENT HOST panel. In Confirm mode the gate
    /// follows the expansion (expanding = "yes, configure advanced
    /// fallbacks"); Forced keeps the gate on even when collapsed.
    pub fn toggle_other(&mut self) {
        self.host_h_pin = None;
        self.pane_h_pin = None;
        if self.schema.advanced_gate == AdvancedGate::Skip {
            return;
        }
        self.other_expanded = !self.other_expanded;
        if !self.gate_forced() {
            self.gate = self.other_expanded;
        }
        // Focus relocation on collapse: only the HOST-LEVEL advanced rows
        // disappear with the block — a DUT-owned advanced key keeps its row
        // in the DUT content box and keeps its focus.
        if !self.other_expanded
            && matches!(&self.focus, FocusSlot::Field(FieldKey::Advanced(k)) if !dut_owned_advanced(k))
        {
            self.set_focus(FocusSlot::Button(ButtonId::ToggleOther));
        }
    }

    /// Shift host indices above `removed` down by one (values, editors,
    /// errors, focus) — typed data follows its host tab.
    fn renumber_hosts_above(&mut self, removed: usize) {
        let shift = |k: FieldKey| -> FieldKey {
            match k {
                FieldKey::HostIp(h)
                | FieldKey::HostUser(h)
                | FieldKey::HostPass(h)
                | FieldKey::DutCount(h)
                    if h > removed =>
                {
                    FieldKey::shift_host_count(k, h - 1)
                }
                FieldKey::DutName(h, d)
                | FieldKey::DutPort(h, d)
                | FieldKey::DutLoginUser(h, d)
                | FieldKey::DutRelayGate(h, d)
                | FieldKey::DutRelayType(h, d)
                | FieldKey::DutRelayPort(h, d)
                | FieldKey::DutRelayReset(h, d)
                | FieldKey::DutRelayDownload(h, d)
                | FieldKey::DutRelayPower(h, d)
                | FieldKey::DutRelayResetTimeMs(h, d)
                | FieldKey::DutRelayPowerOffTimeMs(h, d)
                | FieldKey::DutAdvanced(h, d, _)
                    if h > removed =>
                {
                    FieldKey::shift_host_dut(k, h - 1, d)
                }
                other => other,
            }
        };
        self.values = std::mem::take(&mut self.values)
            .into_iter()
            .map(|(k, v)| (shift(k), v))
            .collect();
        self.editors = std::mem::take(&mut self.editors)
            .into_iter()
            .map(|(k, v)| (shift(k), v))
            .collect();
        self.errors = std::mem::take(&mut self.errors)
            .into_iter()
            .map(|(k, v)| (shift(k), v))
            .collect();
    }

    /// Shift DUT indices above `removed_d` within host `hi` down by one.
    fn renumber_duts_above(&mut self, hi: usize, removed_d: usize) {
        let shift = |k: FieldKey| -> FieldKey {
            match k {
                FieldKey::DutName(h, d)
                | FieldKey::DutPort(h, d)
                | FieldKey::DutLoginUser(h, d)
                | FieldKey::DutRelayGate(h, d)
                | FieldKey::DutRelayType(h, d)
                | FieldKey::DutRelayPort(h, d)
                | FieldKey::DutRelayReset(h, d)
                | FieldKey::DutRelayDownload(h, d)
                | FieldKey::DutRelayPower(h, d)
                | FieldKey::DutRelayResetTimeMs(h, d)
                | FieldKey::DutRelayPowerOffTimeMs(h, d)
                | FieldKey::DutAdvanced(h, d, _)
                    if h == hi && d > removed_d =>
                {
                    FieldKey::shift_host_dut(k, h, d - 1)
                }
                other => other,
            }
        };
        self.values = std::mem::take(&mut self.values)
            .into_iter()
            .map(|(k, v)| (shift(k), v))
            .collect();
        self.editors = std::mem::take(&mut self.editors)
            .into_iter()
            .map(|(k, v)| (shift(k), v))
            .collect();
        self.errors = std::mem::take(&mut self.errors)
            .into_iter()
            .map(|(k, v)| (shift(k), v))
            .collect();
    }

    /// Set the keyboard focus AND keep the CURRENT host/DUT in sync: focus
    /// landing on any of a dev host's slots (its tab or any of its fields)
    /// makes that host the current one; landing on a DUT slot makes that
    /// DUT current. Every focus-mutating path goes through here.
    pub fn set_focus(&mut self, slot: FocusSlot) {
        self.pane.focused = false;
        match &slot {
            FocusSlot::HostTab(hi) => {
                self.selected_host = (*hi).min(self.host_count.saturating_sub(1));
                let count = self
                    .dut_counts
                    .get(self.selected_host)
                    .copied()
                    .unwrap_or(0);
                self.selected_dut = if count > 0 {
                    Some(self.selected_dut.unwrap_or(0).min(count - 1))
                } else {
                    None
                };
            }
            FocusSlot::DutTab(di) => {
                let count = self
                    .dut_counts
                    .get(self.selected_host)
                    .copied()
                    .unwrap_or(0);
                if count > 0 {
                    self.selected_dut = Some((*di).min(count - 1));
                }
            }
            FocusSlot::Field(k) => {
                if let Some(hi) = field_host_index(k) {
                    self.selected_host = hi.min(self.host_count.saturating_sub(1));
                }
                if let Some((_, di)) = field_dut_index(k) {
                    self.selected_dut = Some(di);
                }
            }
            FocusSlot::Button(_) => {}
        }
        self.focus = slot;
    }

    /// Tab / Shift-Tab: one slot forward/back in the zone-ordered cycle.
    fn cycle_slot(&mut self, dir: i32) {
        let order = self.focus_order();
        if order.is_empty() {
            return;
        }
        let cur = order.iter().position(|s| *s == self.focus).unwrap_or(0);
        let n = order.len() as i32;
        let next = ((cur as i32 + dir).rem_euclid(n)) as usize;
        self.set_focus(order[next].clone());
    }

    /// Up/Down field-only navigation WITHIN the focused zone (the host
    /// fields, or the selected DUT's fields) in visual order (printable
    /// chars are NEVER navigation — 'j'/'k' etc. always reach the editor).
    fn cycle_field(&mut self, from: FieldKey, dir: i32) -> FieldKey {
        // Zone ownership follows the row's OWNING container, not the key's
        // index: the DUT-owned advanced keys (control.dev_ctrl,
        // target.login_*, flash.*, …) live in the DUT content box and must
        // cycle the DUT zone.
        let list = if field_in_host(&from) {
            self.host_zone_fields()
        } else {
            self.dut_zone_fields()
        };
        if list.is_empty() {
            return from;
        }
        let cur = list.iter().position(|k| *k == from).unwrap_or(0);
        let n = list.len() as i32;
        let next = ((cur as i32 + dir).rem_euclid(n)) as usize;
        list[next].clone()
    }

    /// Editor state for a field (cursor lands at the buffer end on first
    /// focus).
    fn editor_entry(&mut self, key: &FieldKey) -> (usize, usize) {
        let len = self.values.get(key).map(|s| s.chars().count()).unwrap_or(0);
        *self.editors.entry(key.clone()).or_insert((len, 0))
    }

    /// Set a field's buffer (clears its error — per-field inline errors
    /// vanish the moment the field is edited).
    fn set_value(&mut self, key: FieldKey, value: String) {
        self.errors.remove(&key);
        // Any edit invalidates the TEST marks (coarse clear).
        if !self.probes.is_empty() {
            self.probes.clear();
            self.test_summary = None;
        }
        let endpoint_key_changed = matches!(key, FieldKey::HostIp(_) | FieldKey::DutPort(..));
        self.values.insert(key.clone(), value);
        // Editing an endpoint re-targets the occupancy snapshot immediately:
        // the ⚠ holder hint in the DUT title must track what is being TYPED,
        // not lag until the next TEST run or the save gate.
        if endpoint_key_changed {
            self.refresh_endpoint_conflicts();
        }
        // When the relay reset channel becomes configured, surface the
        // reference-log path the TEST cycle will save to — only fills the
        // EMPTY field, never overwrites a typed path. Direct insert: no
        // recursion, no probe invalidation.
        if matches!(
            key,
            FieldKey::DutAdvanced(_, _, "control.dev_ctrl")
                | FieldKey::DutRelayReset(..)
                | FieldKey::DutRelayPort(..)
                | FieldKey::DutName(..)
        ) {
            self.autofill_reference_log(&key);
        }
    }

    /// Fill the (empty) monitor.reference_log row with the per-DUT default
    /// path whenever the relay reset channel is configured — the value
    /// lands immediately, but the row renders it GRAY until the TEST's
    /// capture probe passed (requested), then normally.
    fn autofill_reference_log(&mut self, trigger: &FieldKey) {
        let (hi, di) = match trigger {
            FieldKey::DutAdvanced(h, d, _) => (*h, *d),
            FieldKey::DutRelayReset(h, d)
            | FieldKey::DutRelayPort(h, d)
            | FieldKey::DutName(h, d) => (*h, *d),
            _ => return,
        };
        let ref_key = FieldKey::DutAdvanced(hi, di, "monitor.reference_log");
        if self
            .values
            .get(&ref_key)
            .is_some_and(|v| !v.trim().is_empty())
        {
            return; // user-typed path wins
        }
        // EMPTY dev_ctrl counts as ENABLED (a filled relay reset channel
        // IS the enable — mirrors the TEST gate).
        let relay_enabled = !self
            .values
            .get(&FieldKey::DutAdvanced(hi, di, "control.dev_ctrl"))
            .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "none" | "disabled"))
            .unwrap_or(false);
        // Gate: the relay ENDPOINT is configured (requested:
        // the reference path shows as real text as soon as the relay port
        // is filled — the reset channel may still be defaulted).
        let relay_port = self
            .values
            .get(&FieldKey::DutRelayPort(hi, di))
            .and_then(|v| v.trim().parse::<u16>().ok())
            .unwrap_or(0);
        if !relay_enabled || relay_port == 0 {
            return;
        }
        let name = self
            .values
            .get(&FieldKey::DutName(hi, di))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("dut{}", di + 1));
        let default = init::reference_log_default(&name);
        // Only when the row is materialized (visible) — otherwise the
        // value would appear without its editor.
        if self.fields.iter().any(|f| f.key == ref_key) {
            self.values.insert(ref_key, default);
        }
    }

    /// Recompute the dim auto-name hints (dutN) live from the taken
    /// set ∪ every typed DUT name value — new blocks continue after names
    /// typed in ANY order. Hints are assigned SEQUENTIALLY in field order
    /// and feed the taken set, so DUT 0 hints dut1, DUT 1 dut2, …
    fn recompute_name_hints(&mut self) {
        let mut dut_taken = self.schema.taken_duts.clone();
        for (k, v) in &self.values {
            let t = v.trim();
            if t.is_empty() {
                continue;
            }
            if let FieldKey::DutName(..) = k {
                dut_taken.push(t.to_string());
            }
        }
        for f in &mut self.fields {
            let empty = !self
                .values
                .get(&f.key)
                .is_some_and(|v| !v.trim().is_empty());
            if !empty {
                continue;
            }
            match f.key {
                FieldKey::DutName(..) => {
                    let name = next_free_dut_name("dut", &dut_taken);
                    dut_taken.push(name.clone());
                    f.default = Some(name);
                }
                FieldKey::DutAdvanced(h, d, "monitor.reference_log")
                    if !self
                        .schema
                        .advanced_current
                        .contains_key("monitor.reference_log") =>
                {
                    // Per-DUT composed hint: THAT DUT's live name.
                    let name = self
                        .values
                        .get(&FieldKey::DutName(h, d))
                        .map(|s| s.trim())
                        .filter(|s| !s.is_empty())
                        .unwrap_or("dut1");
                    f.default = Some(init::reference_log_default(name));
                }
                _ => {}
            }
        }
    }

    /// Live structural rebuild after a +/x mutation: the counts were
    /// already updated by the caller — re-derive fields, keep surviving
    /// values by FieldKey, reseed prefills + relay gate buffers for newly
    /// materialized indices, drop removed blocks' buffers (they were never
    /// written), clamp `selected_host`/`selected_dut`, and fall back to the
    /// selected host's tab when the focused field vanished.
    fn rebuild(&mut self, trigger: FieldKey) {
        self.fields = derive_fields(&self.schema, self.host_count, &self.dut_counts);
        self.sync_count_defaults();
        let new_keys: BTreeSet<FieldKey> = self.fields.iter().map(|f| f.key.clone()).collect();
        self.values.retain(|k, _| new_keys.contains(k));
        self.editors.retain(|k, _| new_keys.contains(k));
        self.errors.retain(|k, _| new_keys.contains(k));
        let keys: Vec<FieldKey> = self.fields.iter().map(|f| f.key.clone()).collect();
        for key in keys {
            if let Some(v) = self.prefill_for(&key)
                && !self.values.contains_key(&key)
            {
                self.values.insert(key, v);
            }
        }
        self.seed_relay_gates();
        self.recompute_name_hints();
        // The selected host can never point past the surviving tabs.
        self.selected_host = self.selected_host.min(self.host_count.saturating_sub(1));
        let count = self
            .dut_counts
            .get(self.selected_host)
            .copied()
            .unwrap_or(0);
        self.selected_dut = self
            .selected_dut
            .and_then(|d| (count > 0).then(|| d.min(count - 1)));
        if let FocusSlot::Field(k) = &self.focus
            && !new_keys.contains(k)
        {
            if self.host_count == 0 {
                self.set_focus(FocusSlot::Button(ButtonId::AddHost));
            } else {
                self.set_focus(FocusSlot::HostTab(self.selected_host));
            }
        }
        let _ = trigger;
        // Amber append-only note when a count drops below the existing
        // config (tail entries are kept by compute_update_ops).
        self.note = match self.existing_shape() {
            Some((eh, ed)) => {
                if self.host_count < eh {
                    Some((
                        NoteKind::Info,
                        format!(
                            "host count reduced below existing {eh} — entries are kept (append-only update)"
                        ),
                    ))
                } else if let Some((hi, _)) = self
                    .dut_counts
                    .iter()
                    .enumerate()
                    .find(|(hi, c)| **c < ed.get(*hi).copied().unwrap_or(0))
                {
                    // The EXISTING count names the note (matching the
                    // host-count wording) — the matched pair carries the
                    // current count.
                    Some((
                        NoteKind::Info,
                        format!(
                            "DUT count reduced below existing {} — entries are kept (append-only update)",
                            ed.get(hi).copied().unwrap_or(0)
                        ),
                    ))
                } else {
                    None
                }
            }
            None => None,
        };
    }

    /// The count fields' dim hint is the COMMITTED visible count — an
    /// empty count buffer at submit means "keep the shown structure" — so
    /// re-sync the materialized defaults with the committed counts after
    /// every structural rebuild.
    fn sync_count_defaults(&mut self) {
        for f in &mut self.fields {
            match f.key {
                FieldKey::HostCount => f.default = Some(self.host_count.to_string()),
                FieldKey::DutCount(hi) => {
                    f.default = Some(self.dut_counts.get(hi).copied().unwrap_or(1).to_string());
                }
                _ => {}
            }
        }
    }

    /// The values map for a whole-form save. The count buffers are ALWAYS
    /// overwritten with the COMMITTED visible counts — the +/x tabs own the
    /// structure, so the buffers are never user-editable. An UPDATE prefill
    /// left non-empty count buffers ("1"/"1" = the existing structure) that
    /// would otherwise go stale the moment a tab mutates the structure and
    /// silently drop every added host/DUT (and their typed values) at
    /// save. The resolved answers can never diverge from the form the
    /// user was looking at.
    pub fn submit_values(&self) -> BTreeMap<FieldKey, String> {
        let mut values = self.values.clone();
        values.insert(FieldKey::HostCount, self.host_count.to_string());
        for hi in 0..self.host_count {
            values.insert(
                FieldKey::DutCount(hi),
                self.dut_counts.get(hi).copied().unwrap_or(0).to_string(),
            );
            // The dev_ctrl toggle is gone: a DUT whose OWN
            // control.dev_ctrl is a CHOSEN real backend IS relay-enabled.
            // The stamp is PER DUT — the global fallback must not leak
            // across DUTs — and an UNCHOSEN (empty) backend stamps
            // nothing (a no-op F2 must not fabricate relay blocks).
            // The gate ALSO opens when ANY relay sub-field buffer holds a
            // real value (typed port/reset_ch were
            // silently dropped when dev_ctrl was empty — the relay rows
            // are focusable regardless, so typed values must survive).
            for di in 0..self.dut_counts.get(hi).copied().unwrap_or(0) {
                let chosen = self
                    .values
                    .get(&FieldKey::DutAdvanced(hi, di, "control.dev_ctrl"))
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty());
                let dev_ctrl_on = chosen.is_some_and(|s| {
                    !matches!(s.to_ascii_lowercase().as_str(), "none" | "disabled")
                });
                let relay_typed = [
                    FieldKey::DutRelayPort(hi, di),
                    FieldKey::DutRelayReset(hi, di),
                    FieldKey::DutRelayDownload(hi, di),
                    FieldKey::DutRelayPower(hi, di),
                    FieldKey::DutRelayResetTimeMs(hi, di),
                    FieldKey::DutRelayPowerOffTimeMs(hi, di),
                ]
                .into_iter()
                .any(|k| {
                    self.values.get(&k).is_some_and(|v| {
                        let t = v.trim();
                        !t.is_empty() && t != "0"
                    })
                });
                if dev_ctrl_on || relay_typed {
                    values.insert(FieldKey::DutRelayGate(hi, di), "yes".into());
                }
            }
        }
        values
    }

    /// Open the controller picker modal for a Select field (no-op for
    /// text fields). The cursor starts on the current value when it is
    /// one of the options, else on the first option.
    pub fn open_picker(&mut self, key: &FieldKey) {
        if self.picker.is_some() {
            return;
        }
        let Some(field) = self.fields.iter().find(|f| &f.key == key) else {
            return;
        };
        if field.kind != FieldKind::Select || field.options.is_empty() {
            return;
        }
        let cur = self.values.get(key).cloned().unwrap_or_default();
        let cursor = field.options.iter().position(|o| *o == cur).unwrap_or(0);
        self.picker = Some(ControllerPicker {
            key: key.clone(),
            options: field.options.clone(),
            cursor,
        });
    }

    /// Select-field cycle: Up/Down walk the options (a value absent from
    /// the picker starts from options[0] / options[n-1]).
    fn select_cycle(&mut self, key: FieldKey, dir: i32) {
        let Some(field) = self.fields.iter().find(|f| f.key == key) else {
            return;
        };
        if field.kind != FieldKind::Select || field.options.is_empty() {
            return;
        }
        let cur = self.values.get(&key).cloned().unwrap_or_default();
        let idx = field.options.iter().position(|o| *o == cur);
        let next = if dir < 0 {
            match idx {
                // (i + len - 1) % len wraps 0 → last (Up must
                // cycle too, not clamp at the first option).
                Some(i) => {
                    field.options[(i + field.options.len() - 1) % field.options.len()].clone()
                }
                None => field.options[field.options.len() - 1].clone(),
            }
        } else {
            match idx {
                Some(i) => field.options[(i + 1) % field.options.len()].clone(),
                None => field.options[0].clone(),
            }
        };
        let len = next.chars().count();
        self.editors.insert(key.clone(), (len, 0));
        self.set_value(key, next);
    }

    /// A worker-backed TEST run is pending — the form loop busy-polls
    /// events while one is.
    pub fn busy(&self) -> bool {
        self.test_run.is_some()
    }
}
/// Materialize the schema into concrete field instances for the given
/// structure (the core's `materialize_group_fields` — the SAME
/// materializer the plain walk consumes, so ordering can never drift).
pub fn derive_fields(
    schema: &FormSchema,
    host_count: usize,
    dut_counts: &[usize],
) -> Vec<FieldInstance> {
    let mut fields = Vec::new();
    for group in FORM_GROUPS {
        match group.repeat {
            Repeat::Once => fields.extend(materialize_group_fields(schema, group, 0, 0)),
            Repeat::PerHost => {
                for hi in 0..host_count {
                    fields.extend(materialize_group_fields(schema, group, hi, 0));
                }
            }
            Repeat::PerHostDut => {
                for hi in 0..host_count {
                    // 0 DUTs is a valid shape (the "+" tab adds them;
                    // a reduced host must not fabricate a DUT block).
                    let dc = dut_counts.get(hi).copied().unwrap_or(0);
                    for di in 0..dc {
                        fields.extend(materialize_group_fields(schema, group, hi, di));
                    }
                }
            }
        }
    }
    fields
}

/// The dev host a field belongs to (None for the global count/advanced
/// fields). Focus landing on any of a host's fields selects that host.
fn field_host_index(key: &FieldKey) -> Option<usize> {
    match key {
        FieldKey::HostIp(h)
        | FieldKey::HostUser(h)
        | FieldKey::HostPass(h)
        | FieldKey::HostName(h)
        | FieldKey::DutName(h, _)
        | FieldKey::DutPort(h, _)
        | FieldKey::DutLoginUser(h, _)
        | FieldKey::DutRelayGate(h, _)
        | FieldKey::DutRelayType(h, _)
        | FieldKey::DutRelayPort(h, _)
        | FieldKey::DutRelayReset(h, _)
        | FieldKey::DutRelayDownload(h, _)
        | FieldKey::DutRelayPower(h, _)
        | FieldKey::DutRelayResetTimeMs(h, _)
        | FieldKey::DutRelayPowerOffTimeMs(h, _)
        | FieldKey::DutAdvanced(h, _, _) => Some(*h),
        FieldKey::HostCount | FieldKey::DutCount(_) | FieldKey::Advanced(_) => None,
    }
}

/// The (host, DUT) pair a DUT field belongs to (None for every other
/// field). Drives the selected-DUT sync in `set_focus`.
fn field_dut_index(key: &FieldKey) -> Option<(usize, usize)> {
    match key {
        FieldKey::DutName(h, d)
        | FieldKey::DutPort(h, d)
        | FieldKey::DutLoginUser(h, d)
        | FieldKey::DutRelayGate(h, d)
        | FieldKey::DutRelayType(h, d)
        | FieldKey::DutRelayPort(h, d)
        | FieldKey::DutRelayReset(h, d)
        | FieldKey::DutRelayDownload(h, d)
        | FieldKey::DutRelayPower(h, d)
        | FieldKey::DutRelayResetTimeMs(h, d)
        | FieldKey::DutRelayPowerOffTimeMs(h, d)
        | FieldKey::DutAdvanced(h, d, _) => Some((*h, *d)),
        _ => None,
    }
}

/// Does a field's row OWN the host panel (scroll/hit-test container)?
/// Indexed DUT fields belong to the DUT content box, and so do the
/// DUT-owned advanced keys (control.dev_ctrl, target.login_*,
/// uboot.interrupt_char, flash.*, monitor.reference_log moved there) —
/// every other field (host fields, counts, host-level advanced) is a host
/// row even when its absolute Y overflows below the host panel.
fn field_in_host(key: &FieldKey) -> bool {
    field_dut_index(key).is_none() && !matches!(key, FieldKey::Advanced(k) if dut_owned_advanced(k))
}

/// Is THIS DUT's control.dev_ctrl backend none/disabled? The dev_ctrl
/// section (relay sub-fields) hides entirely while it is — per-DUT
/// (relays may be limited to one DUT).
fn dut_ctrl_disabled(state: &FormState, hi: usize, di: usize) -> bool {
    state
        .values
        .get(&FieldKey::DutAdvanced(hi, di, "control.dev_ctrl"))
        .map(|s| matches!(s.trim().to_ascii_lowercase().as_str(), "none" | "disabled"))
        .unwrap_or(false)
}

/// Is this relay SUB-FIELD row currently disabled? While the backend is
/// none/disabled the 7 sub-rows render as BLANK reserved space (constant
/// box height) and are skipped by navigation/validation via this
/// predicate. dev_ctrl itself is never disabled.
fn relay_row_disabled(state: &FormState, key: &FieldKey) -> bool {
    let (h, d) = match key {
        FieldKey::DutRelayPort(h, d)
        | FieldKey::DutRelayReset(h, d)
        | FieldKey::DutRelayDownload(h, d)
        | FieldKey::DutRelayPower(h, d)
        | FieldKey::DutRelayResetTimeMs(h, d)
        | FieldKey::DutRelayPowerOffTimeMs(h, d) => (*h, *d),
        _ => return false,
    };
    dut_ctrl_disabled(state, h, d)
}

/// Does any field of host `hi`'s DUT `di` carry an inline error? Paints
/// the DUT content box's border red.
fn dut_has_errors(state: &FormState, hi: usize, di: usize) -> bool {
    state.errors.keys().any(|k| {
        matches!(
            k,
            FieldKey::DutName(h, d)
                | FieldKey::DutPort(h, d)
                | FieldKey::DutLoginUser(h, d)
                | FieldKey::DutRelayGate(h, d)
                | FieldKey::DutRelayType(h, d)
                | FieldKey::DutRelayPort(h, d)
                | FieldKey::DutRelayReset(h, d)
                | FieldKey::DutRelayDownload(h, d)
                | FieldKey::DutRelayPower(h, d)
                | FieldKey::DutRelayResetTimeMs(h, d)
                | FieldKey::DutRelayPowerOffTimeMs(h, d)
                | FieldKey::DutAdvanced(h, d, _)
                if *h == hi && *d == di
        )
    })
}

// ── Key handling (pure) ───────────────────────────────────────────────

/// Handle one key event. Pure: no terminal IO. Release events are ignored
/// (crossterm 0.29 key repeat delivers Press|Repeat). Ctrl-C aborts
/// immediately everywhere.
pub fn handle_form_key(state: &mut FormState, key: KeyEvent) -> FormAction {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return FormAction::Abort("aborted by user".into());
    }
    if key.kind == KeyEventKind::Release {
        return FormAction::Continue;
    }
    if state.handle_skill_key(key) {
        return FormAction::Continue;
    }
    // The open controller picker owns EVERY key: ↑/↓ move the cursor,
    // Enter confirms, Esc/'q'/anything-not-navigation cancels. No other
    // form handling may run while the modal is up.
    if let Some(picker) = state.picker.as_mut() {
        match key.code {
            KeyCode::Up => {
                picker.cursor =
                    (picker.cursor + picker.options.len() - 1) % picker.options.len().max(1);
            }
            KeyCode::Down => {
                picker.cursor = (picker.cursor + 1) % picker.options.len().max(1);
            }
            KeyCode::Enter => {
                let chosen = picker.options.get(picker.cursor).cloned();
                let key_field = picker.key.clone();
                if let Some(value) = chosen {
                    state.set_value(key_field, value);
                }
                state.picker = None;
            }
            _ => {
                // Esc / 'q' / any other key dismisses WITHOUT a choice.
                state.picker = None;
            }
        }
        return FormAction::Continue;
    }
    handle_form_mode_key(state, key)
}

fn handle_form_mode_key(state: &mut FormState, key: KeyEvent) -> FormAction {
    // Esc quits immediately everywhere (nothing written if not saved).
    if key.code == KeyCode::Esc {
        return FormAction::Abort("aborted by user".into());
    }
    // 'q' quits too — EXCEPT when a text field's non-empty buffer owns
    // the letter (mid-edit 'q' is data: alias "qemu", passwords etc.).
    // An empty buffer still quits — a stray 'q' in a fresh field is
    // unambiguous — and so do tabs/buttons and toggles (no editor).
    if key.code == KeyCode::Char('q') && !field_owns_q(state) {
        return FormAction::Abort("aborted by user".into());
    }
    // '?' toggles the footer key hints (hidden by default).
    if key.code == KeyCode::Char('?') && !field_owns_q(state) {
        state.footer_hints_visible = !state.footer_hints_visible;
        return FormAction::Continue;
    }
    // F2 saves DIRECTLY — no Review screen.
    if key.code == KeyCode::F(2) {
        return FormAction::Submit;
    }
    // PageUp/PageDown scroll the DUT content box / expanded advanced block
    // from any focus (the renderer clamps the offset to the overflow).
    if key.code == KeyCode::PageUp {
        state.scroll = state.scroll.saturating_sub(10);
        return FormAction::Continue;
    }
    if key.code == KeyCode::PageDown {
        state.scroll = state.scroll.saturating_add(10);
        return FormAction::Continue;
    }
    // Tab/Shift-Tab cycle the focus ZONES (Host Tabs → Host Fields → DUT
    // Tabs → DUT Fields, wrapping; the footer hints are non-focusable).
    match key.code {
        KeyCode::Tab => {
            state.cycle_slot(1);
            return FormAction::Continue;
        }
        KeyCode::BackTab => {
            state.cycle_slot(-1);
            return FormAction::Continue;
        }
        _ => {}
    }

    // ←/→ switch the CURRENT ZONE's tab group from ANY focus (user
    // decision the footer's '←/→ switch tab' must hold
    // everywhere). Cursor movement stays on the emacs keys (Ctrl-B/F,
    // Home/End) — fields never see plain arrow keys anymore.
    if matches!(key.code, KeyCode::Left | KeyCode::Right) {
        let delta: isize = if key.code == KeyCode::Left { -1 } else { 1 };
        // Zone ownership follows the row's OWNING container (field_in_host):
        // the DUT-owned advanced keys (control.dev_ctrl, target.login_*,
        // flash.*, …) sit in the DUT content box, so ←/→ switches the DUT
        // tab group from them — the pre-move classification treated every
        // Advanced key as a host-panel row.
        match &state.focus {
            FocusSlot::HostTab(_)
            | FocusSlot::Button(ButtonId::AddHost | ButtonId::ToggleOther) => {
                if state.host_count > 0 {
                    let n = (state.selected_host as isize + delta)
                        .rem_euclid(state.host_count as isize) as usize;
                    state.set_focus(FocusSlot::HostTab(n));
                }
            }
            FocusSlot::Field(k) if field_in_host(k) => {
                if state.host_count > 0 {
                    let n = (state.selected_host as isize + delta)
                        .rem_euclid(state.host_count as isize) as usize;
                    state.set_focus(FocusSlot::HostTab(n));
                }
            }
            FocusSlot::DutTab(_) | FocusSlot::Button(ButtonId::AddDut(_)) => {
                let count = state
                    .dut_counts
                    .get(state.selected_host)
                    .copied()
                    .unwrap_or(0);
                if count > 0 {
                    let di = state.selected_dut.unwrap_or(0);
                    let n = (di as isize + delta).rem_euclid(count as isize) as usize;
                    state.set_focus(FocusSlot::DutTab(n));
                }
            }
            // Every remaining field row owns the DUT content box (the
            // guarded host arm above already matched the host rows).
            FocusSlot::Field(_) => {
                let count = state
                    .dut_counts
                    .get(state.selected_host)
                    .copied()
                    .unwrap_or(0);
                if count > 0 {
                    let di = state.selected_dut.unwrap_or(0);
                    let n = (di as isize + delta).rem_euclid(count as isize) as usize;
                    state.set_focus(FocusSlot::DutTab(n));
                }
            }
            FocusSlot::Button(_) => {}
        }
        return FormAction::Continue;
    }
    let focus = state.focus.clone();
    match focus {
        FocusSlot::HostTab(_) => {
            if state.host_count == 0 {
                return FormAction::Continue;
            }
            // Del deletes the CURRENT host.
            if key.code == KeyCode::Delete {
                let hi = state.selected_host;
                state.remove_host(hi);
            }
            FormAction::Continue
        }
        FocusSlot::DutTab(di) => {
            let count = state
                .dut_counts
                .get(state.selected_host)
                .copied()
                .unwrap_or(0);
            if count == 0 {
                return FormAction::Continue;
            }
            match key.code {
                KeyCode::Right => {
                    let n = (di + 1) % count;
                    state.set_focus(FocusSlot::DutTab(n));
                }
                // Del deletes the CURRENT DUT.
                KeyCode::Delete => {
                    let hi = state.selected_host;
                    state.remove_dut(hi, di);
                }
                _ => {}
            }
            FormAction::Continue
        }
        FocusSlot::Button(button) => {
            let activate = matches!(key.code, KeyCode::Enter | KeyCode::Char(' '));
            match button {
                ButtonId::Skill(chip) => {
                    if activate {
                        state.open_skill_picker(chip);
                    }
                }
                ButtonId::AddHost => {
                    if activate {
                        state.add_host();
                    }
                }
                ButtonId::AddDut(hi) => {
                    if activate {
                        state.add_dut(hi);
                    }
                }
                ButtonId::RemoveHost(hi) => {
                    if activate {
                        state.remove_host(hi);
                    }
                }
                ButtonId::RemoveDut(hi, di) => {
                    if activate {
                        state.remove_dut(hi, di);
                    }
                }
                ButtonId::ToggleOther => {
                    if activate {
                        state.toggle_other();
                    }
                }
                ButtonId::Save => {
                    if activate {
                        return FormAction::SaveOnly;
                    }
                }
                ButtonId::SaveExit => {
                    if activate {
                        return FormAction::Submit;
                    }
                }
                ButtonId::Test => {
                    if activate {
                        return FormAction::Test;
                    }
                }
                ButtonId::Exit => {
                    if activate {
                        return FormAction::Abort("aborted by user".into());
                    }
                }
            }
            // Del on a host-zone button removes the current host; on a
            // DUT-zone button the current DUT.
            if key.code == KeyCode::Delete {
                match button {
                    ButtonId::AddHost | ButtonId::RemoveHost(_) | ButtonId::ToggleOther => {
                        let hi = state.selected_host;
                        if state.host_count > 0 {
                            state.remove_host(hi);
                        }
                    }
                    ButtonId::AddDut(_) | ButtonId::RemoveDut(..) => {
                        let hi = state.selected_host;
                        if let Some(di) = state.selected_dut {
                            state.remove_dut(hi, di);
                        }
                    }
                    ButtonId::Save
                    | ButtonId::SaveExit
                    | ButtonId::Test
                    | ButtonId::Skill(_)
                    | ButtonId::Exit => {}
                }
            }
            FormAction::Continue
        }
        FocusSlot::Field(fk) => handle_field_key(state, key, fk),
    }
}

/// Does the FOCUSED slot own a `'q'` keypress as typed data? Only a
/// text/select field with a NON-EMPTY buffer is mid-edit (a fresh empty
/// buffer still quits); tabs/buttons never own it.
fn field_owns_q(state: &FormState) -> bool {
    let FocusSlot::Field(fk) = &state.focus else {
        return false;
    };
    state.values.get(fk).map(|s| !s.is_empty()).unwrap_or(false)
}

fn handle_field_key(state: &mut FormState, key: KeyEvent, fk: FieldKey) -> FormAction {
    let field = state.fields.iter().find(|f| f.key == fk).cloned();
    let kind = field
        .as_ref()
        .map(|f| f.kind.clone())
        .unwrap_or(FieldKind::Text);
    let options: Vec<String> = field.map(|f| f.options).unwrap_or_default();
    let is_select = kind == FieldKind::Select;
    match key.code {
        // Enter on a Select field opens the controller picker modal; on a
        // text field it stays a no-op (F2 saves).
        KeyCode::Enter => {
            if is_select {
                state.open_picker(&fk);
            }
            return FormAction::Continue;
        }
        KeyCode::Tab => {
            state.cycle_slot(1);
            return FormAction::Continue;
        }
        KeyCode::BackTab => {
            state.cycle_slot(-1);
            return FormAction::Continue;
        }
        // Up/Down are the ONLY field-navigation keys — every printable
        // char (including 'j'/'k') must reach the editor buffer below.
        KeyCode::Up => {
            if is_select {
                state.select_cycle(fk, -1);
            } else {
                let next = state.cycle_field(fk, -1);
                state.set_focus(FocusSlot::Field(next));
            }
            return FormAction::Continue;
        }
        KeyCode::Down => {
            if is_select {
                state.select_cycle(fk, 1);
            } else {
                let next = state.cycle_field(fk, 1);
                state.set_focus(FocusSlot::Field(next));
            }
            return FormAction::Continue;
        }
        _ => {}
    }
    // Select-field picker semantics on top of the unified buffer.
    // (the per-DUT relay gate toggle is gone — the gate row
    // no longer exists; the dev_ctrl Select field drives the section.)
    if is_select {
        match key.code {
            // Space opens the controller picker (— cycling was
            // easy to overshoot; the modal shows every option at once).
            KeyCode::Char(' ') => {
                state.open_picker(&fk);
                return FormAction::Continue;
            }
            KeyCode::Char(c) if c.is_ascii_digit() && ('1'..='9').contains(&c) => {
                let d = c.to_digit(10).unwrap_or(0) as usize;
                // Digits 1-9 are direct shortcuts for the options. An
                // out-of-range digit is silently ignored (the choice set
                // is closed — no seeding, no buffer).
                if let Some(opt) = options.get(d - 1) {
                    let len = opt.chars().count();
                    state.editors.insert(fk.clone(), (len, 0));
                    state.set_value(fk, opt.clone());
                }
                return FormAction::Continue;
            }
            _ => {
                // STRICT select semantics: the value can ONLY
                // come from the option set — picker (Space/Enter/click),
                // digit shortcuts, or Up/Down cycling. Free-typed text
                // (e.g. a capitalized "None") must NEVER enter the buffer.
                return FormAction::Continue;
            }
        }
    }
    // The emacs single-line editor (buffer + cursor in `values`/`editors`).
    edit_field(state, &fk, false, key);
    FormAction::Continue
}

/// Insert one char at the cursor (count fields: ASCII digits only — the
/// caller pre-checks). `errors` clears via `set_value` paths.
fn insert_char(state: &mut FormState, fk: &FieldKey, c: char) {
    let len = state.values.get(fk).map(|s| s.chars().count()).unwrap_or(0);
    let (cursor, _) = state.editor_entry(fk);
    let cursor = cursor.min(len);
    let mut buf = state.values.get(fk).cloned().unwrap_or_default();
    buf.insert(cursor, c);
    state.editors.insert(fk.clone(), (cursor + 1, 0));
    state.set_value(fk.clone(), buf);
}

/// Emacs-style editing keys for one field's buffer (Left/Right/Home/End/
/// Backspace/Delete, Ctrl-A/E/B/F/H/D/U/K/W/Y, Alt-B/F). Kill buffer is
/// shared across the whole form (FormState.kill).
fn edit_field(state: &mut FormState, fk: &FieldKey, count_only: bool, key: KeyEvent) {
    let len = state.values.get(fk).map(|s| s.chars().count()).unwrap_or(0);
    let (mut cursor, _) = state.editor_entry(fk);
    cursor = cursor.min(len);
    match key.code {
        KeyCode::Home => cursor = 0,
        KeyCode::End => cursor = len,
        KeyCode::Backspace => {
            let mut buf = state.values.get(fk).cloned().unwrap_or_default();
            backspace(&mut buf, &mut cursor);
            state.set_value(fk.clone(), buf);
        }
        KeyCode::Delete => {
            let mut buf = state.values.get(fk).cloned().unwrap_or_default();
            delete_at(&mut buf, cursor);
            state.set_value(fk.clone(), buf);
        }
        KeyCode::Char(c) => {
            let control = key.modifiers.contains(KeyModifiers::CONTROL);
            let alt = key.modifiers.contains(KeyModifiers::ALT);
            if control {
                let mut buf = state.values.get(fk).cloned().unwrap_or_default();
                match c {
                    'a' => cursor = 0,
                    'e' => cursor = len,
                    'b' => cursor = cursor.saturating_sub(1),
                    'f' => cursor = (cursor + 1).min(len),
                    'h' => backspace(&mut buf, &mut cursor),
                    'd' => delete_at(&mut buf, cursor),
                    'u' => {
                        state.kill = buf.chars().take(cursor).collect();
                        buf = buf.chars().skip(cursor).collect();
                        cursor = 0;
                    }
                    'k' => {
                        state.kill = buf.chars().skip(cursor).collect();
                        buf = buf.chars().take(cursor).collect();
                    }
                    'w' => {
                        let start = word_back(&buf, cursor);
                        state.kill = buf.chars().skip(start).take(cursor - start).collect();
                        buf = buf
                            .chars()
                            .take(start)
                            .chain(buf.chars().skip(cursor))
                            .collect();
                        cursor = start;
                    }
                    'y' => {
                        let kill = state.kill.clone();
                        buf.insert_str(cursor, &kill);
                        cursor += kill.chars().count();
                    }
                    _ => {}
                }
                state.set_value(fk.clone(), buf);
            } else if alt {
                let buf = state.values.get(fk).cloned().unwrap_or_default();
                match c {
                    'b' => cursor = word_back(&buf, cursor),
                    'f' => cursor = word_forward(&buf, cursor),
                    _ => {}
                }
            } else if !count_only || c.is_ascii_digit() {
                insert_char(state, fk, c);
                return;
            }
        }
        _ => return,
    }
    state.editors.insert(fk.clone(), (cursor, 0));
}

/// Delete the char before `cursor`.
fn backspace(buffer: &mut String, cursor: &mut usize) {
    if *cursor > 0 {
        let idx = buffer
            .char_indices()
            .nth(*cursor - 1)
            .map(|(i, _)| i)
            .unwrap_or(0);
        buffer.remove(idx);
        *cursor -= 1;
    }
}

/// Delete the char at `cursor`.
fn delete_at(buffer: &mut String, cursor: usize) {
    if cursor < buffer.chars().count() {
        let idx = buffer
            .char_indices()
            .nth(cursor)
            .map(|(i, _)| i)
            .unwrap_or(buffer.len());
        buffer.remove(idx);
    }
}

/// Start index of the previous word (whitespace-delimited).
fn word_back(s: &str, cursor: usize) -> usize {
    let chars: Vec<char> = s.chars().collect();
    let mut i = cursor.saturating_sub(1);
    while i > 0 && chars[i - 1].is_whitespace() {
        i -= 1;
    }
    while i > 0 && !chars[i - 1].is_whitespace() {
        i -= 1;
    }
    i
}

/// End index of the next word (whitespace-delimited).
fn word_forward(s: &str, cursor: usize) -> usize {
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    let mut i = cursor.min(len);
    while i < len && chars[i].is_whitespace() {
        i += 1;
    }
    while i < len && !chars[i].is_whitespace() {
        i += 1;
    }
    i
}
// ── Layout (the single pure fn shared by render and hit-testing) ──────

/// One field row: label + value editor (+ optional error sub-line when the
/// message does not fit next to the value). Host-panel rows render their
/// values WITH brackets (the renderer paints them around `value_rect`);
/// DUT rows render plain.
#[derive(Debug, Clone)]
pub struct RowSpec {
    pub key: Option<FieldKey>,
    pub label: String,
    /// Absolute (unscrolled) coordinates.
    pub label_rect: Rect,
    pub value_rect: Rect,
    pub sub_rect: Option<Rect>,
    pub dim: bool,
    /// Required field (HostIp / DutPort): the label carries an amber `*`
    /// marker (deliberate — required values visible at a glance).
    pub required: bool,
}

/// Everything the renderer and the mouse hit-tester need — produced by ONE
/// function, so click targets always match pixels by construction.
#[derive(Debug, Clone)]
pub struct LayoutMap {
    pub header: Rect,
    pub host_tab_bar: Rect,
    /// Host tab cards (hi, rect) — clicking a card selects that host.
    pub host_tabs: Vec<(usize, Rect)>,
    pub host_panel: Rect,
    /// The DEVELOPMENT HOST panel's interior.
    pub host_panel_inner: Rect,
    /// `[Other ▼]` / `[Other ▲]` on the panel's top border (None under
    /// --quick).
    pub other_btn: Option<Rect>,
    pub host_rows: Vec<RowSpec>,
    /// Advanced-block rows height inside the host panel (scroll clamp).
    pub host_content_height: usize,
    /// The amber host-count shrink note row (laid out, content-hugging).
    pub host_note: Option<Rect>,
    pub dut_panel: Rect,
    /// The ONE divider line between host and DUT (drag to resize).
    pub divider: Rect,
    /// Per-container scrollbar thumbs (None when no overflow).
    pub host_scrollbar: Option<Rect>,
    pub dut_scrollbar: Option<Rect>,
    pub dut_tab_bar: Rect,
    /// DUT tab cards (di, rect) of the SELECTED host.
    pub dut_tabs: Vec<(usize, Rect)>,
    pub dut_content: Rect,
    /// The DUT content box's interior.
    pub dut_content_inner: Rect,
    /// The N-1 column splits (│) of the multi-column DUT content box.
    pub separators: Vec<Rect>,
    pub dut_rows: Vec<RowSpec>,
    /// DUT content rows height (scroll clamp).
    pub dut_content_height: usize,
    /// The amber DUT-count shrink note row (laid out, content-hugging).
    pub dut_note: Option<Rect>,
    /// Every focusable field's editor rect (host panel + DUT content).
    pub fields: Vec<(FieldKey, Rect)>,
    pub footer: Rect,
    /// Footer key chips: (text, rect).
    pub footer_chips: Vec<(String, Rect)>,
    /// The tab-bar +/x affordances and [Other ▼] — the mouse hit-tester
    /// reaches every one of them.
    pub buttons: Vec<(ButtonId, Rect)>,
}

fn inset(r: Rect, n: u16) -> Rect {
    Rect {
        x: r.x.saturating_add(n),
        y: r.y.saturating_add(n),
        width: r.width.saturating_sub(2 * n),
        height: r.height.saturating_sub(2 * n),
    }
}

/// One line closes BOTH the DUT panel and the Serial Terminal directly
/// below it: a DOCKED pane draws the line with its own top border (title +
/// [ Full ]/[ Hide ] included), so the panel and its content box draw no
/// bottom border and the two freed rows go to the content area and to the
/// terminal. (`Hidden` keeps its own borders — the collapsed pane sits at
/// the FOOTER with a blank row between, not against the panel.)
fn pane_shares_bottom_line(state: &FormState) -> bool {
    state.pane.view == pane::View::Docked
}

/// A bordered box's INTERIOR: `inset(box, 1)`, except when the box has no
/// bottom border (`pane_shares_bottom_line`) — its last row is interior
/// then.
fn box_inner(box_rect: Rect, bottom_open: bool) -> Rect {
    let inner = inset(box_rect, 1);
    if bottom_open {
        Rect {
            height: inner.height.saturating_add(1),
            ..inner
        }
    } else {
        inner
    }
}

/// Compact display label for a HOST panel field row.
fn host_field_label(key: &FieldKey) -> &'static str {
    match key {
        FieldKey::HostIp(_) => "ip",
        FieldKey::HostUser(_) => "user",
        FieldKey::HostPass(_) => "passwd",
        FieldKey::HostName(_) => "host name",
        _ => "?",
    }
}

/// The host TAB's display name — the typed host_name when non-empty, else
/// a positional fallback (hosts without a name stay identifiable).
fn host_display_name(state: &FormState, hi: usize) -> String {
    match state.values.get(&FieldKey::HostName(hi)) {
        Some(v) if !v.trim().is_empty() => v.trim().chars().take(24).collect(),
        _ => format!("dev host {}", hi + 1),
    }
}

/// The DUT TAB's display name (and the 'Current DUT:' line): the typed
/// name when non-empty, else the default auto name.
fn dut_display_name(state: &FormState, hi: usize, di: usize) -> String {
    match state.values.get(&FieldKey::DutName(hi, di)) {
        Some(v) if !v.trim().is_empty() => v.trim().chars().take(24).collect(),
        _ => format!("dut{}", di + 1),
    }
}

/// Compact display label for a DUT content field row (§1.3 names).
fn dut_field_label(key: &FieldKey) -> &'static str {
    match key {
        FieldKey::DutName(..) => "name",
        FieldKey::DutPort(..) => "debug uart",
        FieldKey::DutLoginUser(..) => "login user",
        FieldKey::Advanced(k) | FieldKey::DutAdvanced(_, _, k) if *k == "target.login_pass" => {
            "login password"
        }
        FieldKey::Advanced(k) | FieldKey::DutAdvanced(_, _, k) if *k == "target.login_prompt" => {
            "login_prompt"
        }
        FieldKey::Advanced(k) | FieldKey::DutAdvanced(_, _, k) if *k == "control.dev_ctrl" => {
            "dev_ctrl"
        }
        FieldKey::DutRelayPort(..) => "relay.port",
        // the "ch" suffix was dropped — the channel-ness is
        // obvious from the relay section context.
        FieldKey::DutRelayReset(..) => "reset",
        FieldKey::DutRelayDownload(..) => "download",
        FieldKey::DutRelayPower(..) => "power",
        FieldKey::DutRelayResetTimeMs(..) => "reset hold",
        FieldKey::DutRelayPowerOffTimeMs(..) => "power hold",
        FieldKey::DutAdvanced(_, _, "flash.devices") => flash::devices_label(),
        FieldKey::Advanced(k) | FieldKey::DutAdvanced(_, _, k) => k.rsplit('.').next().unwrap_or(k),
        _ => "?",
    }
}

/// Fit a label into `width` cells: truncate from the LEFT (the distinctive
/// tail survives) with a leading ellipsis.
fn fit_label(label: &str, width: usize) -> String {
    let chars: Vec<char> = label.chars().collect();
    if chars.len() <= width {
        return label.to_string();
    }
    if width <= 1 {
        return "…".to_string();
    }
    let tail: String = chars[chars.len() - (width - 1)..].iter().collect();
    format!("…{tail}")
}

fn visible_auth_probe_detail<'a>(state: &'a FormState, key: &FieldKey) -> Option<&'a str> {
    let visible = match key {
        FieldKey::HostPass(_) => true,
        FieldKey::HostUser(hi) => state
            .values
            .get(&FieldKey::HostPass(*hi))
            .is_none_or(String::is_empty),
        _ => false,
    };
    visible
        .then(|| state.probe_details.get(key).map(String::as_str))
        .flatten()
        .filter(|detail| !detail.is_empty())
}

/// One field row: label + value, plus an error or authentication-result
/// sub-line when needed. Returns the row and its height in cells.
fn field_row(
    state: &FormState,
    field: &FieldInstance,
    label: &str,
    y: u16,
    label_rect: Rect,
    value_rect: Rect,
) -> (RowSpec, u16) {
    let buffer = state.values.get(&field.key).cloned().unwrap_or_default();
    let err = state.errors.get(&field.key);
    let fits = match err {
        Some(e) => {
            (value_rect.width as usize)
                >= buffer.chars().count().saturating_add(2 + e.chars().count())
        }
        None => true,
    };
    let (sub_rect, height) = if err.is_some() && !fits {
        (
            Some(Rect {
                x: value_rect.x,
                y: y + 1,
                width: value_rect.width,
                height: 1,
            }),
            2,
        )
    } else if visible_auth_probe_detail(state, &field.key).is_some()
        || loader_stub_note(&field.key, &buffer).is_some()
    {
        // A stub loader choice (loader=uefi) reserves a sub-line BELOW
        // the value for the "not implemented" note.
        (
            Some(Rect {
                x: value_rect.x,
                y: y + 1,
                width: value_rect.width,
                height: 1,
            }),
            2,
        )
    } else {
        (None, 1)
    };
    (
        RowSpec {
            key: Some(field.key.clone()),
            // Two reserved cells at the label-rect tail: the probe ✓
            // (right-1) plus one blank separator — a full-length label
            // must never touch the mark. Inline sub-labels (baud/hold,
            // a handful of cells) fit at FULL width — shrinking them
            // truncates to "…ud"/"…d".
            label: fit_label(
                label,
                if label_rect.width >= 8 {
                    label_rect.width.saturating_sub(2) as usize
                } else {
                    label_rect.width as usize
                },
            ),
            label_rect,
            value_rect,
            sub_rect,
            dim: false,
            required: field.required,
        },
        height,
    )
}

/// The HOST TAB BAR: one card per host (`dev host N  x ` — the `x` cell is
/// the remove button, the trailing space pads the card before the `│`
/// One tab card: the host/DUT index it represents and its absolute rect.
type TabCard = (usize, Rect);
/// Tab-bar geometry: the tab cards plus the actionable buttons (remove `x`,
/// add `+`) with their rects.
type TabBarLayout = (Vec<TabCard>, Vec<(ButtonId, Rect)>);
#[allow(clippy::type_complexity)]
fn host_tab_bar_layout(state: &FormState, bar: Rect) -> TabBarLayout {
    let mut tabs = Vec::new();
    let mut buttons = Vec::new();
    let mut x = bar.x;
    for hi in 0..state.host_count {
        let label = host_display_name(state, hi);
        let w = (label.chars().count() + 4) as u16; // label + "  x " (pad)
        let rect = Rect {
            x,
            y: bar.y,
            width: w,
            height: 1,
        };
        tabs.push((hi, rect));
        buttons.push((
            ButtonId::RemoveHost(hi),
            Rect {
                x: rect.right().saturating_sub(2),
                y: bar.y,
                width: 1,
                height: 1,
            },
        ));
        x = rect.right().saturating_add(1); // the separator cell
    }
    buttons.push((
        ButtonId::AddHost,
        Rect {
            x,
            y: bar.y,
            width: 3,
            height: 1,
        },
    ));
    (tabs, buttons)
}

/// The DUT TAB BAR of the SELECTED host: same card math (`dut N  x `,
/// RemoveDut on the `x`, AddDut on the trailing `+`).
fn dut_tab_bar_layout(state: &FormState, hi: usize, bar: Rect) -> TabBarLayout {
    let mut tabs = Vec::new();
    let mut buttons = Vec::new();
    let mut x = bar.x;
    let count = state.dut_counts.get(hi).copied().unwrap_or(0);
    for di in 0..count {
        let label = dut_display_name(state, hi, di);
        let w = (label.chars().count() + 4) as u16;
        let rect = Rect {
            x,
            y: bar.y,
            width: w,
            height: 1,
        };
        tabs.push((di, rect));
        buttons.push((
            ButtonId::RemoveDut(hi, di),
            Rect {
                x: rect.right().saturating_sub(2),
                y: bar.y,
                width: 1,
                height: 1,
            },
        ));
        x = rect.right().saturating_add(1);
    }
    buttons.push((
        ButtonId::AddDut(hi),
        Rect {
            x,
            y: bar.y,
            width: 3,
            height: 1,
        },
    ));
    (tabs, buttons)
}

/// The DEVELOPMENT HOST panel: ip/user/pass/name (editable, WITH brackets)
/// at one aligned label/colon/value column; the expanded `[Other ▼]` block
/// appends the advanced GLOBAL fields below as two-up pairs (values WITH
/// brackets) — all inside the panel. Returns the rows, the field rects,
/// the Other button and the content height.
type HostPanelLayout = (
    Vec<RowSpec>,
    Vec<(FieldKey, Rect)>,
    Option<Rect>,
    usize,
    Option<Rect>,
);

fn host_panel_layout(state: &FormState, panel: Rect) -> HostPanelLayout {
    let inner = inset(panel, 1);
    // Ends one cell before the panel's top-right corner so the border `┐`
    // survives (mockup: `... [Other ▼] ┐`).
    let other_btn = None;
    let mut rows = Vec::new();
    let mut fields = Vec::new();
    if state.host_count == 0 {
        // Zero hosts: only a dim hint; no DUT section content.
        let hint = Rect {
            x: inner.x.saturating_add(2),
            y: inner.y.saturating_add(1),
            width: inner.width.saturating_sub(4),
            height: 1,
        };
        rows.push(RowSpec {
            key: None,
            required: false,
            label: "No dev hosts — add one".into(),
            label_rect: hint,
            value_rect: hint,
            sub_rect: None,
            dim: true,
        });
        let note = state
            .note
            .as_ref()
            .filter(|(_, m)| m.contains("host count"))
            .map(|_| Rect {
                x: inner.x.saturating_add(2),
                y: inner.y.saturating_add(2),
                width: inner.width.saturating_sub(4),
                height: 1,
            });
        return (
            rows,
            fields,
            other_btn,
            1 + usize::from(note.is_some()),
            note,
        );
    }
    let lx = inner.x.saturating_add(2);
    // Shared label column: max over the host labels (incl. login_prompt)
    // + the HOST-LEVEL advanced keys only — the DUT-owned keys never
    // render in the host panel (reclassification).
    let mut maxl = 0;
    for key in [
        FieldKey::HostIp(0),
        FieldKey::HostUser(0),
        FieldKey::HostPass(0),
        FieldKey::HostName(0),
    ] {
        maxl = maxl.max(host_field_label(&key).len());
    }

    if state.schema.advanced_gate != AdvancedGate::Skip {
        for f in &state.fields {
            if let FieldKey::Advanced(k) = f.key
                && !dut_owned_advanced(k)
            {
                maxl = maxl.max(k.chars().count());
            }
        }
    }
    // Spec §4: label_w = max + 1 — one pad cell so the colon never touches
    // the longest label.
    let llw = maxl
        .saturating_add(3) // +1 pad + 2 for the required '* ' marker
        .min(((inner.width as usize).saturating_sub(24)).max(5));
    let hi = state.selected_host;
    // Two-column pairs: ip | user on the first
    // row, pass | host name below, then the advanced keys full-width-paired.
    // NARROW panels fall back to one field per row (two-up needs
    // 2*(llw+6)+2 cells of inner width).
    let y0 = inner.y;
    let two_up =
        inner.width.saturating_sub(4) >= 2u16.saturating_mul(llw as u16 + 6).saturating_add(2);
    // ONE unified two-column grid: the host
    // fields (ip|user, pass), login_prompt and the advanced global keys
    // all share the same pair geometry — no separate advanced block, no
    // separator row. Narrow panels fall back to one field/row.
    let keys: Vec<FieldKey> = vec![
        FieldKey::HostIp(hi),
        FieldKey::HostUser(hi),
        FieldKey::HostPass(hi),
        FieldKey::HostName(hi),
    ];
    let panel_label = |k: &FieldKey| -> &'static str {
        let l = host_field_label(k);
        if l == "?" { k_label(k) } else { l }
    };
    let mut y = y0;
    if two_up {
        let pair_w = inner
            .width
            .saturating_sub(4)
            .saturating_sub(2)
            .saturating_div(2);
        let px2 = lx.saturating_add(pair_w.saturating_add(2));
        for (k0, k1) in pack_pairs(&keys) {
            let row_y = y;
            let mut row_h = 1u16;
            for (px, k) in [(lx, k0.as_ref()), (px2, k1.as_ref())] {
                let Some(k) = k else { continue };
                let Some(field) = state.fields.iter().find(|f| &f.key == k) else {
                    continue;
                };
                let label_rect = Rect {
                    x: px,
                    y: row_y,
                    width: llw as u16,
                    height: 1,
                };
                // Value col: colon + space + '[' + editor + ']'.
                let vx = px.saturating_add(llw as u16 + 4);
                let value_rect = Rect {
                    x: vx,
                    y: row_y,
                    width: pair_w.saturating_sub(llw as u16 + 5),
                    height: 1,
                };
                let (row, h) =
                    field_row(state, field, panel_label(k), row_y, label_rect, value_rect);
                fields.push((k.clone(), value_rect));
                row_h = row_h.max(h);
                rows.push(row);
            }
            y = row_y.saturating_add(row_h);
        }
    } else {
        for key in &keys {
            let Some(field) = state.fields.iter().find(|f| f.key == *key) else {
                continue;
            };
            let label_rect = Rect {
                x: lx,
                y,
                width: llw as u16,
                height: 1,
            };
            let vx = lx.saturating_add(llw as u16 + 4);
            let value_rect = Rect {
                x: vx,
                y,
                width: inner
                    .right()
                    .saturating_sub(1)
                    .saturating_sub(vx)
                    .saturating_sub(1),
                height: 1,
            };
            let (row, h) = field_row(state, field, panel_label(key), y, label_rect, value_rect);
            fields.push((key.clone(), value_rect));
            rows.push(row);
            y = y.saturating_add(h);
        }
    }
    // The amber host-count note is a LAID-OUT row: the
    // content height includes it so the hugging panel never overlaps the
    // last field row with the transient note.
    let note = state
        .note
        .as_ref()
        .filter(|(_, m)| m.contains("host count"))
        .map(|_| {
            let rect = Rect {
                x: lx,
                y,
                width: inner.right().saturating_sub(1).saturating_sub(lx),
                height: 1,
            };
            y = y.saturating_add(1);
            rect
        });
    // Content height = the REAL field/note rows only — the panel's top
    // pad row (inner row 0) is chrome, not content, so the panel ends AT
    // its last content row (no trailing blank row).
    let content = (y.saturating_sub(inner.y).saturating_sub(1) as usize).max(1);
    (rows, fields, other_btn, content, note)
}

/// The advanced pair label: the full key path (e.g. "serial.baudrate").
fn k_label(k: &FieldKey) -> &'static str {
    match k {
        FieldKey::Advanced(k) | FieldKey::DutAdvanced(_, _, k) => k.rsplit('.').next().unwrap_or(k),
        _ => "?",
    }
}

/// The advanced keys the reclassification moved INTO the DUT
/// content box (one dev host may serve MULTIPLE SoCs; relays may be
/// limited to ONE DUT) — they must never appear in the host panel, its
/// expanded advanced block, or the host fields zone. Single source of
/// truth: `init::DUT_ADVANCED_KEYS` (the full 16-key set).
fn dut_owned_advanced(k: &str) -> bool {
    init::dut_owned_advanced(k)
}

/// Pair a slice into two-up rows — [(a, b), (c, d), (e, None)] — the
/// advanced block's two-per-row geometry (spec §6 helper).
fn pack_pairs<T: Clone>(items: &[T]) -> Vec<(Option<T>, Option<T>)> {
    items
        .chunks(2)
        .map(|chunk| (chunk.first().cloned(), chunk.get(1).cloned()))
        .collect()
}

/// One visual BOX of the DUT content panel: a dim dashed title row
/// followed by its member fields in fixed order (— the field
/// display groups into boxes: identity, relay, flash, console, monitor).
struct DutBox {
    name: &'static str,
    fields: Vec<FieldKey>,
}

/// The DUT content box's visual BOXES in CANONICAL order — the same order
/// `dut_zone_fields` walks (box-major) and the top-to-bottom / left-to-
/// right reading order of the two-column layout: identity → relay → flash
/// → console → monitor. The relay box always lays out all 8 rows
/// (constant height, no jump); under none/disabled the 7 sub-rows render
/// as blank reserved space — nothing shows below dev_ctrl (deliberate).
/// Only fields actually materialized are included; an empty box is dropped.
fn dut_boxes(state: &FormState, hi: usize, di: usize) -> Vec<DutBox> {
    let adv = |k: &'static str| FieldKey::DutAdvanced(hi, di, k);
    let mut left = vec![
        FieldKey::DutName(hi, di),
        FieldKey::DutPort(hi, di),
        adv("serial.baudrate"),
        FieldKey::DutLoginUser(hi, di),
        adv("target.login_pass"),
        adv("control.dev_ctrl"),
    ];
    let mut right = vec![
        adv("flash.loader_cmd"),
        adv("flash.list_devices_cmd"),
        adv("loader"),
        adv("uboot.interrupt_char"),
    ];
    if !dut_ctrl_disabled(state, hi, di) {
        left.extend([FieldKey::DutRelayPort(hi, di)]);
        if state
            .values
            .get(&FieldKey::DutRelayReset(hi, di))
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(0)
            > 0
        {
            right.push(adv("monitor.reference_log"));
        }
        // Physical keys in pairs: reset + power (both carry a hold time
        // on the same row) sit ADJACENT, then the download key. The hold
        // fields ride their channel rows (place_rows) — they are listed
        // only so `present` keeps them materialized. `recovery` has NO row
        // (it is the plain-prompt-only / config-file key: its value still
        // participates in the mode-button and download-entry logic, but
        // the box would sit alone in a row of its own).
        right.extend([
            FieldKey::DutRelayReset(hi, di),
            FieldKey::DutRelayResetTimeMs(hi, di),
            FieldKey::DutRelayPower(hi, di),
            FieldKey::DutRelayPowerOffTimeMs(hi, di),
            FieldKey::DutRelayDownload(hi, di),
        ]);
    }
    let present = |k: &FieldKey| state.fields.iter().any(|f| &f.key == k);
    left.retain(present);
    right.retain(present);
    vec![
        DutBox {
            name: "identity",
            fields: left,
        },
        DutBox {
            name: "console",
            fields: right,
        },
    ]
}

const TWO_COLUMN_MIN_WIDTH: u16 = 100;

/// How many columns the DUT content box lays out at a terminal `w` cells
/// wide: the plain LEFT/RIGHT split (deliberate — no 3/4
/// column compression) — 2 columns at ≥60, 1 below, where all boxes
/// stack in canonical order.
fn dut_column_count(w: u16) -> usize {
    if w >= TWO_COLUMN_MIN_WIDTH { 2 } else { 1 }
}

/// Which column a box is pinned to (2 columns): identity + relay LEFT,
/// flash + console + monitor RIGHT — the top-to-bottom / left-to-right
/// reading order . At 1 column all boxes stack in canonical
/// order. No box ever splits across the separator.
fn dut_box_column(name: &str, n: usize) -> usize {
    if n == 1 {
        return 0;
    }
    match name {
        "identity" | "relay" => 0,
        _ => 1,
    }
}

/// A column's horizontal span inside `inner`: label x .. last value cell.
/// Separator `i` sits at `inner.x + i * inner.width / n`; column `c` spans
/// `sep_{c-1} + 3 .. sep_c - 1` (the last one ends at the border).
fn dut_column_bounds(inner: Rect, lx: u16, n: usize, c: usize) -> (u16, u16) {
    let sep_x = |i: usize| -> u16 {
        inner.x.saturating_add(
            ((inner.width as u32).saturating_mul(i as u32) / n.max(1) as u32) as u16,
        )
    };
    let cstart = if c == 0 {
        lx
    } else {
        sep_x(c).saturating_add(3)
    };
    let cend = if c + 1 >= n {
        inner.right().saturating_sub(1)
    } else {
        sep_x(c + 1).saturating_sub(1)
    };
    (cstart, cend)
}

/// The DUT CONTENT box: line 1 `Current DUT: dutN`, then the field BOXES
/// (identity + relay LEFT; flash + console + monitor RIGHT — one dim
/// dashed TITLE rule per box), a 1-cell │ separator between the two
/// columns, values WITHOUT brackets, and the trailing `supported:` hint
/// while the dev_ctrl section is visible (the box hugs its
/// multi-column content).
type DutContentLayout = (
    Vec<RowSpec>,
    Vec<(FieldKey, Rect)>,
    Vec<Rect>,
    usize,
    Option<Rect>,
);

fn dut_content_layout(state: &FormState, content: Rect, shared_line: bool) -> DutContentLayout {
    let inner = box_inner(content, shared_line);
    let mut rows = Vec::new();
    let mut fields = Vec::new();
    let hi = state.selected_host;
    let lx = inner.x.saturating_add(2);
    let di = state.selected_dut;
    let Some(di) = di else {
        // Zero DUTs: dim hint, no detail rows, no separators.
        let hint = Rect {
            x: lx,
            y: inner.y.saturating_add(1),
            width: inner.width.saturating_sub(4),
            height: 1,
        };
        rows.push(RowSpec {
            key: None,
            required: false,
            label: "No DUTs — add one".into(),
            label_rect: hint,
            value_rect: hint,
            sub_rect: None,
            dim: true,
        });
        // The amber DUT-count note still gets its laid-out row below the
        // hint (no pinned overlays).
        let note = state
            .note
            .as_ref()
            .filter(|(_, m)| m.contains("DUT count"))
            .map(|_| Rect {
                x: lx,
                y: inner.y.saturating_add(2),
                width: inner.width.saturating_sub(4),
                height: 1,
            });
        return (
            rows,
            fields,
            Vec::new(),
            2 + usize::from(note.is_some()),
            note,
        );
    };
    let n = dut_column_count(content.width.saturating_add(2));
    let boxes = dut_boxes(state, hi, di);
    // Fixed box→column pinning: whole boxes per column, canonical order
    // top-to-bottom inside each (— no balancing, no splitting).
    let mut cols: Vec<Vec<&DutBox>> = vec![Vec::new(); n];
    for b in &boxes {
        cols[dut_box_column(b.name, n)].push(b);
    }

    // Per-column label widths (the SHOWN field labels only — the box
    // titles are excluded): longest label + 1 pad cell so the colon never
    // touches it + 2 for the required '* ' marker — capped so every
    // column keeps a usable value cell.
    let mut lws: Vec<usize> = Vec::new();
    for (c, cboxes) in cols.iter().enumerate() {
        let mut lw = 4usize;
        for b in cboxes {
            for k in &b.fields {
                lw = lw.max(dut_field_label(k).chars().count());
            }
        }
        let (cstart, cend) = dut_column_bounds(inner, lx, n, c);
        let span = cend.saturating_sub(cstart).saturating_add(1) as usize;
        lws.push((lw + 3).min(span.saturating_sub(12)).max(4));
    }

    // Place the columns top-to-bottom. Each box opens with a dim dashed
    // TITLE rule (`── name ─…`) spanning its column, then its field rows
    // (columns start at the same row, so short columns just leave the
    // rest of their span empty).
    let y0 = inner.y;
    let mut max_y = y0;
    for c in 0..n {
        let (cstart, cend) = dut_column_bounds(inner, lx, n, c);
        let col_w = cend.saturating_sub(cstart).saturating_add(1);
        let mut y = y0;
        for b in &cols[c] {
            y = place_rows(
                state,
                &b.fields,
                cstart,
                lws[c],
                col_w,
                y,
                &mut rows,
                &mut fields,
            );
        }
        max_y = max_y.max(y);
    }
    // (The conditional "supported:" hint row was removed: it
    // appeared/disappeared with the backend choice — a second layout-jump
    // source. The full option list now lives in the controller picker
    // popup instead.)
    // The amber DUT-count note is a LAID-OUT row: the
    // content height includes it, so the hugging box never overlaps the
    // last field row with the transient note.
    let note = state
        .note
        .as_ref()
        .filter(|(_, m)| m.contains("DUT count"))
        .map(|_| {
            let rect = Rect {
                x: lx,
                y: max_y,
                width: inner.right().saturating_sub(1).saturating_sub(lx),
                height: 1,
            };
            max_y = max_y.saturating_add(1);
            rect
        });
    let content_h = (max_y.saturating_sub(inner.y) as usize).max(1);
    // The separators span the whole content height (short columns just
    // keep the line).
    let mut separators = Vec::new();
    for i in 1..n {
        separators.push(Rect {
            x: inner
                .x
                .saturating_add(((inner.width as u32).saturating_mul(i as u32) / n as u32) as u16),
            y: inner.y.saturating_add(1),
            width: 1,
            height: content_h.saturating_sub(1) as u16,
        });
    }
    (rows, fields, separators, content_h, note)
}

/// Place a column's field rows top-to-bottom at (x, y) with the given label
/// width; returns the column's new y (error sub-lines widen rows).
#[allow(clippy::too_many_arguments)]
fn place_rows(
    state: &FormState,
    keys: &[FieldKey],
    x: u16,
    lw: usize,
    col_w: u16,
    y: u16,
    rows: &mut Vec<RowSpec>,
    fields: &mut Vec<(FieldKey, Rect)>,
) -> u16 {
    let mut y = y;
    for k in keys {
        if matches!(
            k,
            FieldKey::DutAdvanced(_, _, "serial.baudrate")
                | FieldKey::DutRelayResetTimeMs(..)
                | FieldKey::DutRelayPowerOffTimeMs(..)
        ) {
            continue;
        }
        // The relay channel rows carry their hold field on the SAME row
        // (channel value | [>] | "hold" label | hold value, even split)
        // — one row per physical key: `reset : 1 [>] hold [3000] ms`.
        if let (FieldKey::DutRelayReset(..) | FieldKey::DutRelayPower(..), true) = (&k, true)
            && let Some(hold_key) = match k {
                FieldKey::DutRelayReset(h, d) => Some(FieldKey::DutRelayResetTimeMs(*h, *d)),
                FieldKey::DutRelayPower(h, d) => Some(FieldKey::DutRelayPowerOffTimeMs(*h, *d)),
                _ => None,
            }
            && let (Some(channel_field), Some(hold_field)) = (
                state.fields.iter().find(|f| &f.key == k),
                state.fields.iter().find(|f| f.key == hold_key),
            )
        {
            let label = match k {
                FieldKey::DutRelayReset(..) => "reset",
                _ => "power",
            };
            let vx = x.saturating_add(lw as u16 + 3);
            let available = col_w.saturating_sub(lw as u16 + 3);
            // 6-cell "hold" label (4 text + the probe-mark cell + a pad)
            // behind the [>] button.
            let label_width = 6.min(available.saturating_sub(1));
            let remaining = available.saturating_sub(label_width + 1);
            let channel_width = (remaining / 2).max(4).min(remaining);
            let hold_width = remaining.saturating_sub(channel_width);
            let channel_rect = Rect::new(vx, y, channel_width, 1);
            let hold_label = Rect::new(channel_rect.right() + 1, y, label_width, 1);
            let hold_rect = Rect::new(hold_label.right(), y, hold_width, 1);
            let (row, h) = field_row(
                state,
                channel_field,
                label,
                y,
                Rect::new(x, y, lw as u16, 1),
                channel_rect,
            );
            let (hold_row, hh) = field_row(state, hold_field, "hold", y, hold_label, hold_rect);
            rows.extend([row, hold_row]);
            fields.extend([(k.clone(), channel_rect), (hold_key, hold_rect)]);
            y = y.saturating_add(h.max(hh));
            continue;
        }
        if let FieldKey::DutPort(hi, di) = k {
            let baud = FieldKey::DutAdvanced(*hi, *di, "serial.baudrate");
            if let (Some(port_field), Some(baud_field)) = (
                state.fields.iter().find(|f| &f.key == k),
                state.fields.iter().find(|f| f.key == baud),
            ) {
                let vx = x.saturating_add(lw as u16 + 3);
                let available = col_w.saturating_sub(lw as u16 + 3);
                // Even split: after the 6-cell "baud" label (4 text + the
                // probe-mark cell + a pad), the port and baud fields
                // halve the remaining width (the port no longer swallows
                // the row).
                let label_width = 6.min(available);
                let remaining = available - label_width;
                let port_width = (remaining / 2).max(4).min(remaining);
                let baud_width = remaining.saturating_sub(port_width);
                let port_rect = Rect::new(vx, y, port_width, 1);
                let baud_label = Rect::new(vx + port_width, y, label_width, 1);
                let baud_rect = Rect::new(baud_label.right(), y, baud_width, 1);
                let (row, h) = field_row(
                    state,
                    port_field,
                    "debug uart",
                    y,
                    Rect::new(x, y, lw as u16, 1),
                    port_rect,
                );
                let (baud_row, bh) = field_row(state, baud_field, "baud", y, baud_label, baud_rect);
                rows.extend([row, baud_row]);
                fields.extend([(k.clone(), port_rect), (baud, baud_rect)]);
                y = y.saturating_add(h.max(bh));
                continue;
            }
        }
        if flash::is_devices_key(k) {
            // Read-only flash device-list row: laid out like a field but
            // never an editor — the probe mark + detail fill its value
            // cell (and it is filtered out of focus/navigation).
            let label_rect = Rect {
                x,
                y,
                width: lw as u16,
                height: 1,
            };
            let vx = x.saturating_add(lw as u16 + 3);
            let value_rect = Rect {
                x: vx,
                y,
                width: col_w.saturating_sub(lw as u16 + 3),
                height: 1,
            };
            rows.push(RowSpec {
                key: Some(k.clone()),
                label: fit_label(flash::devices_label(), lw.saturating_sub(2)),
                label_rect,
                value_rect,
                sub_rect: None,
                dim: false,
                required: false,
            });
            fields.push((k.clone(), value_rect));
            y = y.saturating_add(1);
            continue;
        }
        let Some(field) = state.fields.iter().find(|f| &f.key == k) else {
            continue;
        };
        let label_rect = Rect {
            x,
            y,
            width: lw as u16,
            height: 1,
        };
        let vx = x.saturating_add(lw as u16 + 3);
        let value_rect = Rect {
            x: vx,
            y,
            width: col_w.saturating_sub(lw as u16 + 3),
            height: 1,
        };
        let (row, h) = field_row(state, field, dut_field_label(k), y, label_rect, value_rect);
        fields.push((k.clone(), value_rect));
        rows.push(row);
        y = y.saturating_add(h);
    }
    y
}

/// The footer key-hint chips (text + rect), groups separated by " │ ".
fn footer_hints_layout(footer: Rect) -> Vec<(String, Rect)> {
    let groups = [
        "q: quit",
        "Tab/Shift-Tab focus",
        "←/→ switch tab",
        "Enter edit",
        "F2 save",
        "Del delete",
        "Esc quit",
    ];
    let mut chips = Vec::new();
    let mut x = footer.x;
    for g in groups {
        let w = g.chars().count() as u16 + 2; // thin left/right border cells
        chips.push((
            g.to_string(),
            Rect {
                x,
                y: footer.y,
                width: w,
                height: 1,
            },
        ));
        x = x.saturating_add(w).saturating_add(1); // chip borders act as separators
    }
    chips
}

/// The whole screen layout. Pure — unit-testable without a terminal.
pub fn form_layout(area: Rect, state: &FormState) -> LayoutMap {
    form_mode_layout(area, state)
}

/// The cramped-floor reserve for the DUT panel (absolute rows, borders
/// included): host_h is never pinned taller than `body - 1 - DUT_MIN_ABS`.
/// At 5 the panel keeps 1 tab bar row + the content box's borders + its
/// own bottom border where it has one (the docked pane's shared line
/// replaces the two bottom borders, so the same 5 rows then hold 2
/// interior rows) — the tab bar sits on the panel's top row (no blank row
/// above it).
const DUT_MIN_ABS: u16 = 5;

/// The DOCKED Serial Terminal pane's minimum height (rows, its top border
/// included): the split line above it never squeezes the pane below this,
/// and a pinned height from a LARGER terminal is capped to it on a shrink.
const MIN_PANE_HEIGHT: u16 = 5;

/// Four vertical chunks below the header: host tab bar (7% of the content
/// area), DEVELOPMENT HOST panel (33%; 56% when `[Other ▼]` is expanded —
/// the DUT panel takes the rest), DUTs panel, footer (7%). All rects
/// absolute; the scroll shifts the DUT content box and the expanded
/// advanced block only.
fn form_mode_layout(area: Rect, state: &FormState) -> LayoutMap {
    let header = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: 1,
    };
    let ch = area.height.saturating_sub(1); // header
    let pct = |p: u32| ((ch as u32).saturating_mul(p) / 100).clamp(1, u32::MAX) as u16;
    let tab_h = 1;
    let footer_h = pct(7).max(if area.width < 90 { 2 } else { 1 });
    let body = ch.saturating_sub(tab_h).saturating_sub(footer_h);
    let body_y = header.bottom().saturating_add(tab_h);
    let host_tab_bar = Rect {
        x: area.x,
        y: header.bottom(),
        width: area.width,
        height: tab_h,
    };

    // PASS 1 — measure content heights with provisional full-body rects
    // (both layout fns are height-independent: rows lay out top-down).
    let probe = Rect {
        x: area.x,
        y: body_y,
        width: area.width,
        height: body,
    };
    let (_, _, _, host_content_height, _) = host_panel_layout(state, probe);
    let shared_line = pane_shares_bottom_line(state);
    let dut_content_height: usize = if state.host_count > 0 {
        let dut_inner_probe = inset(probe, 1);
        let content_probe = Rect {
            x: dut_inner_probe.x,
            y: dut_inner_probe.y.saturating_add(1),
            width: dut_inner_probe.width,
            height: dut_inner_probe.height.saturating_sub(1),
        };
        dut_content_layout(state, content_probe, shared_line).3
    } else {
        1
    };

    // PASS 2 — place. host_h is CONTENT-DRIVEN (deliberate: no blank
    // space below the host info); the DUT panel takes the
    // remainder and its full visibility WINS on conflict (host scrolls
    // first). The pin (drag) overrides, clamped to keep a minimal DUT.
    // +4 = 1 tab row + 2 box borders + the content rows + the panel
    // bottom border (+2 when the docked pane shares the bottom line:
    // tab row + the box's top border only) — the whole multi-column
    // content fits at the natural height (the tab bar sits on the panel's
    // top row, so there is no blank row above it).
    let dut_natural = (dut_content_height as u16).saturating_add(if shared_line { 2 } else { 4 });
    let host_auto = (host_content_height as u16)
        .saturating_add(2)
        .min(body.saturating_sub(1).saturating_sub(dut_natural));
    let host_h = if let Some(pin) = state.host_h_pin {
        pin.clamp(2, body.saturating_sub(1).saturating_sub(DUT_MIN_ABS))
    } else if host_auto < 2 {
        // Cramped floor (the DUT's natural height cannot fit at all): the
        // host hugs its content too — capped so the DUT keeps its minimal
        // panel — and the DUT takes the rest and scrolls (no
        // blank rows inside the host panel at any size).
        (host_content_height as u16)
            .saturating_add(2)
            .min(body.saturating_sub(1).saturating_sub(DUT_MIN_ABS))
    } else {
        host_auto.max(2)
    };
    let host_panel = Rect {
        x: area.x,
        y: body_y,
        width: area.width,
        height: host_h,
    };
    let divider = Rect {
        x: area.x,
        y: host_panel.bottom(),
        width: area.width,
        height: 1,
    };
    // The DUT panel hugs its multi-column content (deliberate
    // host content → divider → DUT content, no trailing blank
    // inside the panel). Any leftover space sits BELOW the DUT panel —
    // the footer stays pinned at the bottom. Stretching the panel to the
    // footer would just re-create the blank rows the content-hugging
    // removes (inside the border box this time), so the leftover stays
    // plain background.
    //
    // A DRAGGED pane (pane_h_pin, its top border's height handle) overrides
    // the hugging: the pane's height is then fixed, so the DUT panel takes
    // every remaining row down to the pane's top border — the split line
    // trades rows one for one (a row off the pane is a row on the panel)
    // instead of leaving dead background between them. DUT_MIN_ABS stays
    // reserved either way.
    let dut_space = body.saturating_sub(host_h).saturating_sub(1);
    let pane_reserve = match state.pane.view {
        // A HIDDEN pane is one collapsed row — the pin does not apply.
        pane::View::Hidden => None,
        _ => state.pane_h_pin,
    }
    .map(|pin| {
        pin.min(dut_space.saturating_sub(DUT_MIN_ABS))
            .max(MIN_PANE_HEIGHT)
    });
    let dut_height = match pane_reserve {
        Some(reserve) => dut_space.saturating_sub(reserve),
        None => dut_space.min(dut_natural).min(dut_space.saturating_sub(
            if state.pane.view == pane::View::Hidden {
                2
            } else {
                MIN_PANE_HEIGHT
            },
        )),
    };
    let dut_panel = Rect {
        x: area.x,
        y: divider.bottom(),
        width: area.width,
        height: dut_height,
    };
    let footer = Rect {
        x: area.x,
        y: body_y.saturating_add(body),
        width: area.width,
        height: footer_h,
    };

    // RE-LAY at the final rects (same widths → same heights; the pass-1
    // probe only measured).
    let (host_tabs, mut buttons) = host_tab_bar_layout(state, host_tab_bar);
    let (host_rows, mut fields, other_btn, host_content_height, host_note) =
        host_panel_layout(state, host_panel);
    let dut_inner = inset(dut_panel, 1);
    // The tab bar sits on the panel's TOP row: no blank row
    // between the divider and the tabs.
    let dut_tab_bar = Rect {
        x: dut_inner.x,
        y: dut_panel.y,
        width: dut_inner.width,
        height: 1,
    };
    let dut_content = Rect {
        x: dut_inner.x,
        y: dut_panel.y.saturating_add(1),
        width: dut_inner.width,
        // The box's bottom border row is the panel's last row — which the
        // docked pane's top border takes over (shared line).
        height: dut_panel
            .height
            .saturating_sub(if shared_line { 1 } else { 2 }),
    };
    let (dut_tabs, dut_rows, separators, dut_content_height, dut_fields, dut_note);
    if state.host_count > 0 {
        let (tabs, dut_buttons) = dut_tab_bar_layout(state, state.selected_host, dut_tab_bar);
        dut_tabs = tabs;
        buttons.extend(dut_buttons);
        let (rows, f, sep, chh, note) = dut_content_layout(state, dut_content, shared_line);
        dut_rows = rows;
        dut_fields = f;
        separators = sep;
        dut_content_height = chh;
        dut_note = note;
    } else {
        dut_tabs = Vec::new();
        dut_rows = Vec::new();
        separators = Vec::new();
        dut_content_height = 1;
        dut_fields = Vec::new();
        dut_note = None;
    }
    fields.extend(dut_fields);
    if let Some(r) = other_btn {
        buttons.push((ButtonId::ToggleOther, r));
    }
    // The host panel has NO bottom border, so its interior is bottom-open
    // (top + left + right inset only): the last content row sits on the
    // panel's last row and must stay inside the scroll clip.
    let host_inner = Rect {
        x: host_panel.x.saturating_add(1),
        y: host_panel.y.saturating_add(1),
        width: host_panel.width.saturating_sub(2),
        height: host_panel.height.saturating_sub(1),
    };
    let dut_content_inner = box_inner(dut_content, shared_line);
    // Per-container scrollbar thumbs — the single global
    // scroll offset is clamped PER container; a thumb renders only when
    // that container overflows.
    let host_scrollbar = scrollbar_thumb(host_inner, host_content_height, state.scroll);
    let dut_scrollbar = scrollbar_thumb(dut_content_inner, dut_content_height, state.scroll);
    let mut map = LayoutMap {
        header,
        host_tab_bar,
        host_tabs,
        host_panel,
        host_panel_inner: host_inner,
        other_btn,
        host_rows,
        host_content_height,
        host_note,
        dut_panel,
        divider,
        host_scrollbar,
        dut_scrollbar,
        dut_tab_bar,
        dut_tabs,
        dut_content,
        dut_content_inner,
        separators,
        dut_rows,
        dut_content_height,
        dut_note,
        fields,
        footer,
        footer_chips: footer_hints_layout(footer),
        buttons,
    };
    // [Save] (save-only) and [Save & Exit] at the footer's right edge
    // (save no longer exits).
    map.buttons
        .extend(project::selector_buttons(footer, state.skill_chips().len()));
    map.buttons.extend(footer_button_rects(footer));
    map
}

/// The `.target.jsonc` download overlay for one DUT, from the RENDERED
/// test config (existing text + form edits): DUT-level `download.*`
/// over global `download.*`, with the legacy `flash.loader_cmd` /
/// `flash.list_devices_cmd` spellings as migration fallbacks. The
/// provider defaults apply where this returns None — there is
/// deliberately no CLI layer.
fn download_overlay_for(test_text: &str, hi: usize, di: usize) -> (Option<String>, Option<String>) {
    let Ok(config) = sermcp::config::parse_jsonc_text(test_text) else {
        return (None, None);
    };
    let dut = config.dev_hosts.get(hi).and_then(|h| h.duts.get(di));
    fn overlay(
        sections: &sermcp::config::JsoncSections,
        key: fn(&sermcp::config::JsoncDownload) -> &Option<String>,
        legacy: fn(&sermcp::config::JsoncFlash) -> &Option<String>,
    ) -> Option<String> {
        sections
            .download
            .as_ref()
            .and_then(|d| key(d).clone())
            .or_else(|| sections.flash.as_ref().and_then(|f| legacy(f).clone()))
    }
    let dut_sections = dut.map(sermcp::config::JsoncSections::from);
    let global_sections = sermcp::config::JsoncSections::from(&config);
    let resolve = |field: fn(&sermcp::config::JsoncDownload) -> &Option<String>,
                   legacy: fn(&sermcp::config::JsoncFlash) -> &Option<String>| {
        // DUT section wins over the global fallback.
        dut_sections
            .as_ref()
            .and_then(|d| overlay(d, field, legacy))
            .or_else(|| overlay(&global_sections, field, legacy))
    };
    let loader = resolve(|d| &d.loader_cmd, |f| &f.loader_cmd);
    let list = resolve(|d| &d.list_devices_cmd, |f| &f.list_devices_cmd);
    (loader, list)
}

/// [Test] [Save] [Save & Exit] [Exit] at the footer's right edge — shared by
/// both top tabs (footer semantics are tab-independent). The strip stops
/// `RIGHT_MARGIN` cells short of the footer's edge so [Exit] keeps a gap to
/// the terminal's right border.
fn footer_button_rects(footer: Rect) -> Vec<(ButtonId, Rect)> {
    let right = footer.right().saturating_sub(RIGHT_MARGIN);
    vec![
        (
            ButtonId::Exit,
            Rect {
                x: right.saturating_sub(6),
                y: footer.y,
                width: 6,
                height: 1,
            },
        ),
        (
            ButtonId::SaveExit,
            Rect {
                x: right.saturating_sub(20),
                y: footer.y,
                width: 13,
                height: 1,
            },
        ),
        (
            ButtonId::Save,
            Rect {
                x: right.saturating_sub(27),
                y: footer.y,
                width: 6,
                height: 1,
            },
        ),
        (
            ButtonId::Test,
            Rect {
                x: right.saturating_sub(34),
                y: footer.y,
                width: 6,
                height: 1,
            },
        ),
    ]
}

/// Direct fallback parameters for the U-Boot interrupt probe — DEBUG-ONLY
/// (compiled with `direct-dut-probe`; production never direct-connects to
/// the DUT from `init`).
#[cfg(feature = "direct-dut-probe")]
#[derive(Clone)]
struct DirectUbootProbe {
    relay_host: String,
    relay_port: u16,
    channel: u8,
    serial_host: String,
    serial_port: u16,
    interrupt_char: u8,
    reset_hold_ms: u64,
}

/// Direct fallback parameters for the relay reset-cycle probe — DEBUG-ONLY.
#[cfg(feature = "direct-dut-probe")]
#[derive(Clone)]
struct DirectResetCycleProbe {
    relay_host: String,
    relay_port: u16,
    channel: u8,
    serial_host: String,
    serial_port: u16,
    save_path: std::path::PathBuf,
    reset_hold_ms: u64,
}

/// One connectivity probe (TEST button, real-hardware
/// extensions). Quick probes stay bounded (connect 700 ms,
/// per-probe ≤1 s); the relay cycle and SSH login are the two SLOW
/// probes (seconds) — the worker runs on its own thread either way, so
/// the UI never hangs.
#[derive(Clone)]
enum ProbeTarget {
    /// TCP connect + a short 'SSH-' banner read (host reachability).
    SshBanner { host: String, port: u16 },
    /// Plain TCP connect (ser2net / relay port).
    TcpConnect { host: String, port: u16 },
    /// A local character device path (no open() — avoids DTR pulses).
    CharDevice { path: String },
    /// Public-key SSH login for a dev host without a configured password.
    SshLogin {
        host: String,
        user: String,
        pass: String,
    },
    /// Test public-key authentication for visibility, then strictly test the
    /// configured username/password pair. The final mark follows the password
    /// result and is mirrored onto both fields because SSH cannot identify
    /// which member of a rejected credential pair is wrong.
    SshPasswordLogin {
        host: String,
        user: String,
        pass: String,
        linked_user: FieldKey,
    },
    /// RFC 2217 baudrate negotiation on the serial endpoint: connect,
    /// offer COM-PORT, set the baudrate — a ser2net rfc2217 port answers
    /// with telnet IAC traffic. DEBUG-ONLY direct probe (it would evict the
    /// MCP server's serial connection in production).
    #[cfg(feature = "direct-dut-probe")]
    SerialBaud { host: String, port: u16, baud: u32 },
    /// A MANDATORY probe whose prerequisite is missing (dev_ctrl chose a
    /// real relay backend but the relay port is not filled): the row must
    /// fail loudly (✗), not silently skip (user rule: with
    /// ch340-relay chosen, the port and reset MUST be tested).
    AlwaysFail,
    /// Fresh raw-byte baud test through the MCP-owned SerialEngine. The init
    /// process never opens ser2net directly in production. `use_reset=false`
    /// when the form's dev_ctrl is none: the server must not press the
    /// reset relay the user disabled.
    #[cfg(not(feature = "direct-dut-probe"))]
    McpBaudTest {
        ports: Vec<u16>,
        baud: u32,
        use_reset: bool,
        normalization_key: Option<FieldKey>,
        /// One MCP baud test also proves the mandatory serial.port link.
        /// Mirror its result instead of opening the endpoint twice.
        linked_key: FieldKey,
    },
    /// Put an unknown DUT state onto a deterministic normal-boot path before
    /// serial-content probes. Power is preferred over reset when configured.
    /// A configured download button is held across one cycle, then
    /// released and followed by one normal cycle so TEST never inherits the
    /// resulting flash mode.
    McpNormalizeState {
        ports: Vec<u16>,
        use_power: bool,
        mode_buttons: Vec<String>,
        linked_keys: Vec<FieldKey>,
    },
    /// U-Boot autoboot-interrupt test. Primary: the MCP server's dev-ctrl
    /// backend (`serial_enter_uboot`) — when dev_ctrl is enabled this proves
    /// the configured interrupt char stops autoboot at `=>`. The direct
    /// relay reset + interrupt flood fallback is compiled only with
    /// `direct-dut-probe`. The mark lands on uboot.interrupt_char.
    UbootInterrupt {
        ports: Vec<u16>,
        interrupt_char: String,
        soft_reboot: bool,
        normalization_key: Option<FieldKey>,
        #[cfg(feature = "direct-dut-probe")]
        direct: DirectUbootProbe,
    },
    /// Full relay reset cycle. Primary: the MCP server's connection
    /// learning (`serial_learn_connection`) — reset cycles whose pairwise
    /// similarity reaches ≥93% (≥2 cycles); on success the server writes
    /// the reference log. The direct press/release fallback is compiled
    /// only with `direct-dut-probe`. The mark lands on monitor.reference_log.
    RelayResetCycle {
        ports: Vec<u16>,
        reference_log_path: String,
        use_power: bool,
        #[cfg(feature = "direct-dut-probe")]
        direct: DirectResetCycleProbe,
    },
    /// Deliberate non-test: reference-log learning needs a HARDWARE reset to
    /// align the boot boundary (press → rotate the log in the silence →
    /// release → capture). Without the dev_ctrl relay there is no reliable
    /// boundary — a soft `reboot` trails a tail of shutdown prints, and no
    /// plain algorithm can tell "first line of the new boot" from that tail.
    /// Rendered as · skip with the reason, never ✗.
    Skip { reason: String },
    /// Per-channel relay liveness: send the ch340 STATUS packet for one
    /// FILLED channel and expect a reply — a non-disruptive check of
    /// every configured channel (reset/download/recovery/power). DEBUG-ONLY
    /// direct probe: compiled only with `direct-dut-probe`.
    #[cfg(feature = "direct-dut-probe")]
    RelayChannelStatus {
        host: String,
        port: u16,
        channel: u8,
    },
    /// Flash device-list probe: verify the DUT entered
    /// flashing mode. Runs `flash.list_devices_cmd` on the dev host BEFORE
    /// (baseline), triggers flashing mode, then polls it AGAIN for ~15 s —
    /// the newly-appeared device lines (the XOR of the two outputs) are the
    /// probe detail. The trigger is CONTEXT-AWARE (flash.rs): at the
    /// U-Boot prompt (left by the earlier U-Boot interrupt test) the
    /// loader_cmd is tried RAW first (U-Boot-style commands), then `boot`
    /// resumes the boot and the probe waits for the Linux shell before the
    /// shell-style loader_cmd; the recovery/download relay channels are the
    /// fallbacks when no serial trigger produced a device. The rule is
    /// tool-format agnostic (the operator's own list command, whatever
    /// vendor's device list it prints) and multi-DUT safe: other boards already in
    /// flashing mode cancel out of the XOR. A non-empty baseline that stays
    /// unchanged fails with an "already visible" detail instead of a bare
    /// "no device" claim. Skipped when neither loader_cmd nor a
    /// download/recovery channel is configured.
    ListDevices {
        host: String,
        user: String,
        pass: String,
        /// Resolved DownloadProvider id ("" = no provider: SKIP, never
        /// guess a platform).
        provider_id: String,
        /// Effective commands: `.target.jsonc` download overlay over the
        /// provider defaults. NEVER UI-editable, NEVER CLI-overridable.
        loader_cmd: Option<String>,
        list_cmd: String,
        reset_ch: Option<u8>,
        /// The effective hardware download channel + engine button name
        /// (recovery preferred, download fallback).
        download_ch: Option<(u8, String)>,
        dev_ctrl: bool,
        mcp_ports: Vec<u16>,
    },
    /// Fast parallel validation of the resolved device-list command. This
    /// runs before serial state transitions so a typo cannot consume the
    /// whole TEST budget and surface as an unrelated timeout.
    FlashListPreflight {
        host: String,
        user: String,
        pass: String,
        list_cmd: String,
    },
}

/// How many boot-log lines the relay test saves as the similarity
/// reference (matches the connection learner's compare window — the
/// default `compare_lines` is 50; keeping them identical means the saved
/// file feeds `serial_load_reference` unchanged).
#[cfg(feature = "direct-dut-probe")]
const REFERENCE_LINES: usize = 50;

/// First N meaningful lines of captured serial output: split on newline,
/// strip CR and trailing blanks; empty lines skipped. PURE (unit-tested).
#[cfg(feature = "direct-dut-probe")]
fn first_reference_lines(data: &[u8], n: usize) -> Vec<String> {
    let text = String::from_utf8_lossy(data);
    text.split('\n')
        .map(|l| l.trim_end_matches('\r').trim())
        .filter(|l| !l.is_empty())
        .take(n)
        .map(str::to_string)
        .collect()
}

/// Board-output check for the press window: bytes that survive telnet
/// IAC stripping and banner-line removal count as "the board talked while
/// held in reset" (a wiring/channel error). PURE (unit-tested).
#[cfg(feature = "direct-dut-probe")]
fn meaningful_serial_noise(data: &[u8]) -> bool {
    // Drop telnet IAC sequences (0xff + cmd [+ option] / SB...SE) — the
    // same shapes TerminalSanitizer handles; a tiny local copy keeps the
    // probe worker self-contained.
    let mut stripped = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if data[i] == 0xff && i + 1 < data.len() {
            let cmd = data[i + 1];
            if cmd == 0xff {
                stripped.push(0xff); // escaped literal
                i += 2;
                continue;
            }
            if (0xfb..=0xfe).contains(&cmd) {
                i += 3; // WILL/WONT/DO/DONT + option
                continue;
            }
            if cmd == 0xfa {
                // subnegotiation: skip to IAC SE
                i += 2;
                while i + 1 < data.len() && !(data[i] == 0xff && data[i + 1] == 0xf0) {
                    i += 1;
                }
                i += 2;
                continue;
            }
            i += 2; // two-byte command
            continue;
        }
        stripped.push(data[i]);
        i += 1;
    }
    let text = String::from_utf8_lossy(&stripped);
    text.lines().any(|l| {
        let t = l.trim_end_matches('\r').trim();
        !t.is_empty() && !t.contains("ser2net port")
    })
}

/// Try public-key authentication and retain the narrow stale-host-key recovery
/// used after a dev host is re-flashed at the same address.
fn probe_ssh_key_login(host: &str, user: &str) -> ProbeMark {
    let dest = format!("{user}@{host}");
    let base = [
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=5",
        "-o",
        "StrictHostKeyChecking=accept-new",
    ];
    // Keep stderr for one narrowly-scoped recovery: a re-flashed dev host
    // commonly keeps its IP but gets a new host key.  `accept-new` correctly
    // rejects that stale known_hosts entry, so remove only that literal IP
    // and retry with normal host-key verification.
    let mut key_attempt = ssh_key_login_attempt(&dest, &base);
    if key_attempt.0 {
        return ProbeMark::Ok;
    }
    if ssh_host_key_changed(&key_attempt.1) && remove_stale_ip_host_key(host) {
        key_attempt = ssh_key_login_attempt(&dest, &base);
        if key_attempt.0 {
            return ProbeMark::Ok;
        }
    }
    ProbeMark::Err
}

fn probe_ssh_login(host: &str, user: &str, _pass: &str) -> ProbeMark {
    probe_ssh_key_login(host, user)
}

/// Verify the configured password itself. This deliberately disables public
/// key authentication; otherwise a valid local key makes any password appear
/// correct even though password-only deployment commands would fail.
fn probe_ssh_password_login(host: &str, user: &str, pass: &str) -> ProbeMark {
    use std::process::Command;
    if pass.is_empty() {
        return ProbeMark::Skip;
    }
    let dest = format!("{user}@{host}");
    let base = [
        "-o",
        "BatchMode=no",
        "-o",
        "ConnectTimeout=5",
        "-o",
        "StrictHostKeyChecking=accept-new",
        "-o",
        "PubkeyAuthentication=no",
        "-o",
        "KbdInteractiveAuthentication=no",
        "-o",
        "NumberOfPasswordPrompts=1",
        "-o",
        "PreferredAuthentications=password",
    ];
    if let Ok(status) = Command::new("setsid")
        .arg("sshpass")
        .arg("-p")
        .arg(pass)
        .arg("ssh")
        .args(base)
        .arg(&dest)
        .arg("true")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        && status.success()
    {
        return ProbeMark::Ok;
    }
    if ssh_askpass_login(&dest, pass, &base) {
        ProbeMark::Ok
    } else {
        ProbeMark::Err
    }
}

/// Run one non-interactive key-auth attempt and retain SSH diagnostics.
fn ssh_key_login_attempt(dest: &str, base: &[&str]) -> (bool, Vec<u8>) {
    use std::process::Command;
    match Command::new("setsid")
        .arg("ssh")
        .args(base)
        .arg(dest)
        .arg("true")
        .stdout(std::process::Stdio::null())
        .output()
    {
        Ok(output) => (output.status.success(), output.stderr),
        Err(_) => (false, Vec::new()),
    }
}

/// Match only OpenSSH's explicit stale-key diagnostics. Authentication and
/// reachability failures must never trigger known_hosts mutation.
fn ssh_host_key_changed(stderr: &[u8]) -> bool {
    let text = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    text.contains("remote host identification has changed")
        || (text.contains("offending") && text.contains("known_hosts"))
}

/// Remove a stale key only for a configured literal IP. `ssh-keygen -R`
/// understands plain and hashed known_hosts entries and preserves a `.old`
/// backup. Hostnames and option-shaped input are deliberately rejected.
fn remove_stale_ip_host_key(host: &str) -> bool {
    use std::process::Command;
    if host.parse::<std::net::IpAddr>().is_err() {
        return false;
    }
    Command::new("ssh-keygen")
        .arg("-R")
        .arg(host)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Password login via OpenSSH's SSH_ASKPASS mechanism: a 0700 temp
/// script echoes the password; ssh reads it because stdin is /dev/null
/// (no tty) and DISPLAY is set. Removed immediately after the attempt.
fn ssh_askpass_login(dest: &str, pass: &str, base: &[&str]) -> bool {
    use std::process::Command;
    // Unique per attempt: concurrent probes must never share (or delete)
    // each other's script.
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let mut script = std::env::temp_dir();
    script.push(format!(
        ".dutabo-askpass-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let escaped = pass.replace('\'', "'\\''");
    let body = format!("#!/bin/sh\nprintf '%s\\n' '{escaped}'\n");
    let ok = std::fs::write(&script, body)
        .and_then(|_| {
            std::fs::set_permissions(&script, {
                use std::os::unix::fs::PermissionsExt;
                std::fs::Permissions::from_mode(0o700)
            })
        })
        .is_ok()
        // `setsid`: run ssh WITHOUT a controlling terminal — otherwise
        // ssh opens /dev/tty and prompts on the TUI's terminal (it
        // blocked the whole TEST run, field-observed). No tty ⇒
        // ssh must use SSH_ASKPASS.
        && Command::new("setsid")
            .env("DISPLAY", ":0")
            .env("SSH_ASKPASS", &script)
            .arg("ssh")
            .args(base)
            .arg("-o")
            .arg("PreferredAuthentications=password,keyboard-interactive")
            .arg(dest)
            .arg("true")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|st| st.success());
    let _ = std::fs::remove_file(&script);
    ok
}

/// RFC 2217 baudrate probe: connect, negotiate COM-PORT, set the rate —
/// an rfc2217 ser2net endpoint answers with IAC traffic.
#[cfg(feature = "direct-dut-probe")]
fn probe_serial_baud(host: &str, port: u16, baud: u32) -> ProbeMark {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::net::ToSocketAddrs;
    let Some(addr) = (host, port)
        .to_socket_addrs()
        .ok()
        .and_then(|mut a| a.next())
    else {
        return ProbeMark::Err;
    };
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(700))
    else {
        return ProbeMark::Err;
    };
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(700)));
    // Good-citizen prelude: accept the server's DO (IAC WILL), offer our
    // own DO, then the rate. (Live-verified against ser2net:
    // the endpoint may still send IAC WONT for the option itself — a
    // quirk — while applying AND acknowledging the requested rate.)
    let mut msg = vec![0xff, 0xfb, 44, 0xff, 0xfd, 44];
    msg.extend(sermcp::telnet_filter::rfc2217_set_baudrate(baud));
    if stream.write_all(&msg).is_err() {
        return ProbeMark::Err;
    }
    let _ = stream.flush();
    // Collect the answer until the SERVER-SET-BAUDRATE acknowledgement
    // (cmd 101) arrives — the authoritative "rate applied" reply.
    let mut buf = [0u8; 512];
    let mut resp = Vec::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1500);
    while std::time::Instant::now() < deadline {
        match stream.read(&mut buf) {
            Ok(n) if n > 0 => resp.extend_from_slice(&buf[..n]),
            _ => {}
        }
        if rfc2217_baud_ack(&resp).is_some() {
            break;
        }
    }
    let Some(actual) = rfc2217_baud_ack(&resp) else {
        // No acknowledgement: the endpoint does not speak rfc2217 (or
        // never applied the rate) — not a pass.
        return ProbeMark::Err;
    };
    if actual != baud {
        // Acknowledged at a DIFFERENT rate: the endpoint clamped it.
        return ProbeMark::Err;
    }
    // The ack alone cannot catch a WRONG-but-applied rate (ser2net acks
    // anything, field-observed: "9600 passed"). Data-quality
    // check: poke the console and classify the output — a mismatched
    // UART renders garbage; a matched one answers clean text.
    if stream.write_all(b"\r").is_err() {
        return ProbeMark::Warn;
    }
    let _ = stream.flush();
    let mut data = Vec::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1500);
    while std::time::Instant::now() < deadline {
        match stream.read(&mut buf) {
            Ok(n) if n > 0 => data.extend_from_slice(&buf[..n]),
            _ => {}
        }
    }
    // Strip telnet IAC traffic before judging text quality.
    let text = strip_iac(&data);
    if text.is_empty() {
        // Quiet console: the rate was applied but nothing answers —
        // cannot confirm nor deny.
        return ProbeMark::Warn;
    }
    let garbage = text
        .iter()
        .filter(|&&b| b >= 0x80 || (b < 0x20 && b != b'\r' && b != b'\n' && b != b'\t'))
        .count();
    let ratio = garbage as f64 / text.len() as f64;
    if ratio > 0.25 {
        ProbeMark::Err // mojibake: the configured baud is wrong
    } else {
        ProbeMark::Ok
    }
}

/// Drop telnet IAC sequences (negotiation/subnegotiation); keep escaped
/// literal 0xFF data bytes. PURE.
#[cfg(feature = "direct-dut-probe")]
fn strip_iac(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if data[i] == 0xff && i + 1 < data.len() {
            let cmd = data[i + 1];
            if cmd == 0xff {
                out.push(0xff);
                i += 2;
                continue;
            }
            if (0xfb..=0xfe).contains(&cmd) {
                i += 3;
                continue;
            }
            if cmd == 0xfa {
                i += 2;
                while i + 1 < data.len() && !(data[i] == 0xff && data[i + 1] == 0xf0) {
                    i += 1;
                }
                i += 2;
                continue;
            }
            i += 2;
            continue;
        }
        out.push(data[i]);
        i += 1;
    }
    out
}

/// Parse the RFC 2217 SERVER-SET-BAUDRATE acknowledgement
/// (`IAC SB COM-PORT SERVER-SET-BAUDRATE(101) <4-byte rate> IAC SE`) —
/// the authoritative "rate applied" reply. PURE.
#[cfg(feature = "direct-dut-probe")]
fn rfc2217_baud_ack(resp: &[u8]) -> Option<u32> {
    let needle = [0xff, 0xfa, 44, 101];
    let pos = resp.windows(4).position(|w| w == needle)?;
    let rate = resp.get(pos + 4..pos + 8)?;
    if resp.get(pos + 8) != Some(&0xff) {
        return None;
    }
    Some(u32::from_be_bytes([rate[0], rate[1], rate[2], rate[3]]))
}

/// One relay channel liveness check: connect to the relay endpoint and
/// send the ch340 STATUS packet — a responding relay is wired and the
/// channel number is in range (non-disruptive: STATUS never switches).
#[cfg(feature = "direct-dut-probe")]
fn probe_relay_channel_status(host: &str, port: u16, channel: u8) -> ProbeMark {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::net::ToSocketAddrs;
    let Some(addr) = (host, port)
        .to_socket_addrs()
        .ok()
        .and_then(|mut a| a.next())
    else {
        return ProbeMark::Err;
    };
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(700))
    else {
        return ProbeMark::Err;
    };
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(500)));
    let pkt = sermcp::power_control::relay_packet(channel, sermcp::power_control::RELAY_OP_STATUS);
    if stream.write_all(&pkt).is_err() {
        return ProbeMark::Err;
    }
    let mut buf = [0u8; 16];
    matches!(stream.read(&mut buf), Ok(n) if n > 0)
        .then_some(())
        .map_or(ProbeMark::Err, |_| ProbeMark::Ok)
}

/// U-Boot interrupt char from the already validated form value. PURE.
#[cfg(feature = "direct-dut-probe")]
fn parse_interrupt_char(v: &str) -> u8 {
    sermcp::config::parse_interrupt_char(v)
}

/// Reset via the relay and flood the interrupt char through the boot
/// window: Ok when the U-Boot prompt (`=>`) appears — the board stopped
/// at the bootloader command line (requested).
#[cfg(feature = "direct-dut-probe")]
fn probe_uboot_interrupt(
    relay_host: &str,
    relay_port: u16,
    channel: u8,
    serial_host: &str,
    serial_port: u16,
    interrupt_char: u8,
    reset_hold_ms: u64,
) -> ProbeMark {
    use std::io::{Read, Write};
    use std::net::{TcpStream, ToSocketAddrs};
    let connect = |host: &str, port: u16| -> Option<TcpStream> {
        let addr = (host, port)
            .to_socket_addrs()
            .ok()
            .and_then(|mut a| a.next())?;
        TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(700)).ok()
    };
    let Some(mut serial) = connect(serial_host, serial_port) else {
        return ProbeMark::Err;
    };
    let _ = serial.set_read_timeout(Some(std::time::Duration::from_millis(100)));
    let Some(mut relay) = connect(relay_host, relay_port) else {
        return ProbeMark::Err;
    };
    let pkt = |op: u8| sermcp::power_control::relay_packet(channel, op);

    // Press: hold the board in reset (drain the connect chatter).
    if relay
        .write_all(&pkt(sermcp::power_control::RELAY_OP_ON))
        .is_err()
    {
        return ProbeMark::Err;
    }
    let _ = relay.flush();
    let mut scratch = [0u8; 4096];
    if reset_hold_ms == 0 {
        return ProbeMark::Err;
    }
    let press_end = std::time::Instant::now() + std::time::Duration::from_millis(reset_hold_ms);
    while std::time::Instant::now() < press_end {
        let _ = serial.read(&mut scratch);
    }
    // Release + FLOOD the interrupt char through the boot window —
    // whenever the autoboot countdown appears, a char is already in
    // flight (the engine's "flood" strategy).
    if relay
        .write_all(&pkt(sermcp::power_control::RELAY_OP_OFF))
        .is_err()
    {
        return ProbeMark::Err;
    }
    let _ = relay.flush();
    let mut seen = Vec::new();
    let mut next_send = std::time::Instant::now();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while std::time::Instant::now() < deadline {
        if std::time::Instant::now() >= next_send {
            let _ = serial.write_all(&[interrupt_char]);
            next_send += std::time::Duration::from_millis(150);
        }
        if let Ok(n) = serial.read(&mut scratch)
            && n > 0
        {
            seen.extend_from_slice(&scratch[..n]);
            if ends_at_uboot_prompt(&seen) {
                // INTERACTIVE PROOF (user demand: actually
                // REACH the U-Boot command line, no fake pass): run a
                // command and require a genuine U-Boot reply — boot-log
                // debris containing "=>" can never satisfy this.
                let _ = serial.write_all(b"version\r");
                let mut reply = Vec::new();
                let proof_end = std::time::Instant::now() + std::time::Duration::from_secs(3);
                while std::time::Instant::now() < proof_end {
                    if let Ok(n) = serial.read(&mut scratch)
                        && n > 0
                    {
                        reply.extend_from_slice(&scratch[..n]);
                    }
                }
                let text = String::from_utf8_lossy(&reply).to_lowercase();
                return if text.contains("u-boot") {
                    ProbeMark::Ok
                } else {
                    ProbeMark::Err
                };
            }
        }
    }
    ProbeMark::Err
}

/// Strict U-Boot prompt check: the LAST non-empty clean line consists of
/// "=>" alone — boot-log debris that merely CONTAINS "=>" never matches
/// (that loose check was a false pass, field-observed). PURE.
#[cfg(feature = "direct-dut-probe")]
fn ends_at_uboot_prompt(data: &[u8]) -> bool {
    let text = String::from_utf8_lossy(data);
    text.lines()
        .map(|l| l.trim_end_matches('\r').trim())
        .rfind(|l| !l.is_empty())
        .is_some_and(|last| last == "=>")
}

/// The full relay reset cycle (real hardware): press → serial must go
/// silent → release → serial must produce boot output → save the first
/// REFERENCE_LINES lines as the similarity reference. PURE line-splitting
/// is factored into `first_reference_lines` (unit-tested); the capture
/// windows are generous because real boards boot slowly.
#[cfg(feature = "direct-dut-probe")]
fn probe_relay_reset_cycle(
    relay_host: &str,
    relay_port: u16,
    channel: u8,
    serial_host: &str,
    serial_port: u16,
    save_path: &std::path::Path,
    reset_hold_ms: u64,
) -> ProbeMark {
    use std::io::{Read, Write};
    use std::net::{TcpStream, ToSocketAddrs};

    let connect = |host: &str, port: u16| -> Option<TcpStream> {
        let addr = (host, port)
            .to_socket_addrs()
            .ok()
            .and_then(|mut a| a.next())?;
        TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(700)).ok()
    };

    // Serial first: the reader must be attached BEFORE the press so the
    // silence window is meaningful.
    let Some(mut serial) = connect(serial_host, serial_port) else {
        return ProbeMark::Err;
    };
    let _ = serial.set_read_timeout(Some(std::time::Duration::from_millis(200)));
    let Some(mut relay) = connect(relay_host, relay_port) else {
        return ProbeMark::Err;
    };
    let pkt = |op: u8| sermcp::power_control::relay_packet(channel, op);

    // PRESS the reset channel, then verify the serial goes quiet (the
    // board is held in reset — no output while pressed). The connect
    // banner + telnet IAC handshake (always sent by ser2net right after
    // connect) is NOT board output — strip it before judging silence.
    if relay
        .write_all(&pkt(sermcp::power_control::RELAY_OP_ON))
        .is_err()
    {
        return ProbeMark::Err;
    }
    let _ = relay.flush();
    let mut scratch = [0u8; 4096];
    let mut pressed_bytes = Vec::new();
    if reset_hold_ms == 0 {
        return ProbeMark::Err;
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(reset_hold_ms);
    while std::time::Instant::now() < deadline {
        if let Ok(n) = serial.read(&mut scratch)
            && n > 0
        {
            pressed_bytes.extend_from_slice(&scratch[..n]);
        }
    }
    let noisy_while_pressed = meaningful_serial_noise(&pressed_bytes);

    // THREE capture cycles (deliberate): release → capture
    // the boot window, repeat. The reference is written ONLY when the
    // three normalized windows are pairwise >= 0.93 similar — a stable
    // boot signature worth segmenting on (same threshold as the
    // connection learner's similarity_threshold).
    let pkt_on = pkt(sermcp::power_control::RELAY_OP_ON);
    let pkt_off = pkt(sermcp::power_control::RELAY_OP_OFF);
    let mut windows: Vec<String> = Vec::new();
    for cycle in 0..3 {
        if cycle > 0 {
            // Re-press for the next capture.
            if relay.write_all(&pkt_on).is_err() {
                return ProbeMark::Err;
            }
            let _ = relay.flush();
            let press_end = std::time::Instant::now() + std::time::Duration::from_millis(1500);
            let mut pressed_bytes = Vec::new();
            while std::time::Instant::now() < press_end {
                if let Ok(n) = serial.read(&mut scratch)
                    && n > 0
                {
                    pressed_bytes.extend_from_slice(&scratch[..n]);
                }
            }
            if meaningful_serial_noise(&pressed_bytes) {
                return ProbeMark::Warn; // board talked while held
            }
        }
        if relay.write_all(&pkt_off).is_err() {
            return ProbeMark::Err;
        }
        let _ = relay.flush();
        let mut captured = Vec::new();
        // ALIGNMENT: drop bytes before the first newline — the release
        // burst can begin mid-line (a leftover tail), and a misaligned
        // first line poisons every pairwise comparison after it.
        let mut aligned = false;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
        while std::time::Instant::now() < deadline {
            if let Ok(n) = serial.read(&mut scratch)
                && n > 0
            {
                if !aligned {
                    if let Some(nl) = scratch[..n].iter().position(|&b| b == b'\n') {
                        captured.extend_from_slice(&scratch[nl + 1..n]);
                        aligned = true;
                    }
                } else {
                    captured.extend_from_slice(&scratch[..n]);
                }
                if first_reference_lines(&captured, usize::MAX).len() >= REFERENCE_LINES {
                    break;
                }
            }
        }
        let lines = first_reference_lines(&captured, REFERENCE_LINES);
        if lines.is_empty() {
            return ProbeMark::Err; // released but silent
        }
        windows.push(lines.join("\n"));
    }
    if !reference_is_stable(&windows) {
        // The relay and serial work, but the boot output is not
        // deterministic enough to seed similarity segmentation — nothing
        // is written (rerun TEST on a quieter boot).
        return ProbeMark::Warn;
    }
    // Persist the FIRST capture's window as the similarity reference
    // (best effort — a read-only tree must not fail the whole cycle).
    if let Some(parent) = save_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(save_path, format!("{}\n", windows[0]));
    if noisy_while_pressed {
        ProbeMark::Warn
    } else {
        ProbeMark::Ok
    }
}

/// Mask boot-to-boot volatile tokens (hex addresses/checksums, multi-digit
/// counters, training values) before comparing — the deterministic
/// STRUCTURE of the boot is what the reference segments on. PURE.
#[cfg(feature = "direct-dut-probe")]
fn stabilize_for_similarity(text: &str) -> String {
    use std::sync::LazyLock;
    static RE_HEX: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"[0-9a-fA-F]{4,}").unwrap());
    // ANY 2+ digit run — also inside words (ttot10, 09:48, ch16) — a
    // deterministic boot's structure lives in its words, not its counters
    // (live-measured: 2-digit residuals alone dropped the
    // blend to 0.83).
    static RE_NUM: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"\d{2,}").unwrap());
    let masked = RE_HEX.replace_all(text, "#H");
    RE_NUM.replace_all(&masked, "#N").to_string()
}

/// The 3-window stability gate for the reference write: every pair of
/// normalized windows must score >= 0.93 (the connection learner's
/// blended metric: jaro-winkler 0.6 + 3-gram jaccard 0.4). Volatile
/// tokens are masked first — DDR training values and checksums differ
/// every boot while the log structure stays identical (real boots never
/// reached 0.93 unmasked). PURE.
#[cfg(feature = "direct-dut-probe")]
fn reference_is_stable(windows: &[String]) -> bool {
    if windows.len() < 2 {
        return false;
    }
    let normalized: Vec<String> = windows
        .iter()
        .map(|w| {
            sermcp::connection_learner::ConnectionLearner::normalize(&stabilize_for_similarity(w))
        })
        .collect();
    (0..normalized.len()).all(|i| {
        (i + 1..normalized.len()).all(|j| {
            sermcp::connection_learner::compute_similarity(&normalized[i], &normalized[j]) >= 0.93
        })
    })
}

/// Candidate ports for the one project MCP, followed by explicit overrides.
fn mcp_ports_for_dut(cwd: &std::path::Path, _name: &str) -> Vec<u16> {
    let mut ports = vec![sermcp::ports::project_mcp_port(cwd)];
    if let Some(p) = super::detect_mcp_port_from_config() {
        ports.push(p);
    }
    if let Some(p) = std::env::var("MCP_PORT")
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
    {
        ports.push(p);
    }
    ports.sort_unstable();
    ports.dedup();
    ports
}

/// Shared probe runtime, built once per process. Probing used to build a
/// fresh tokio runtime per call (driver + timer setup on every mark); the
/// probes fire dozens of times per TEST run, so one lazily-initialized
/// runtime removes that churn. `block_on` on the shared handle is
/// thread-safe; concurrent probe lanes may briefly queue on the single
/// driver — their calls are short and the lanes poll at ≥1s cadence, so
/// results are unchanged.
pub(super) fn probe_runtime() -> &'static tokio::runtime::Runtime {
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("probe tokio runtime")
    })
}

/// Call one MCP tool synchronously from the probe worker thread and map it
/// to a probe mark. The TUI loop is sync (crossterm), so the async reqwest
/// client in `super` runs on the shared probe runtime here. A reachable
/// server whose tool answered `success: false` is a LOUD failure (✗) — the
/// DUT is wired but the test genuinely failed; an unreachable server is a
/// skip (·).
fn mcp_probe_mark(ports: &[u16], tool: &str, arguments: serde_json::Value) -> ProbeMark {
    let outcome = probe_runtime().block_on(super::mcp_call(
        "tools/call",
        serde_json::json!({"name": tool, "arguments": arguments}),
        ports,
    ));
    match outcome {
        super::McpOutcome::Result(result) => match super::tool_payload(&result) {
            Ok(_) => ProbeMark::Ok,
            Err(_) => ProbeMark::Err, // tool answered success: false
        },
        super::McpOutcome::RpcError { .. } => ProbeMark::Err,
        super::McpOutcome::Unreachable => ProbeMark::Skip,
    }
}

fn normalize_dut_state(ports: &[u16], use_power: bool, mode_buttons: &[String]) -> ProbeMark {
    let cycle = |ports: &[u16], verify_reset: bool| {
        if use_power {
            mcp_probe_mark(
                ports,
                "serial_power_cycle",
                serde_json::json!({"wait_boot": false}),
            )
        } else if verify_reset {
            mcp_probe_mark(ports, "serial_verify_relay", serde_json::json!({}))
        } else {
            mcp_probe_mark(
                ports,
                "serial_reset",
                serde_json::json!({"wait_boot": false, "failure_retry": 1}),
            )
        }
    };

    for button in mode_buttons {
        if mcp_probe_mark(
            ports,
            "serial_button",
            serde_json::json!({"button": button, "action": "press"}),
        ) != ProbeMark::Ok
        {
            return ProbeMark::Err;
        }
    }
    if !mode_buttons.is_empty() && cycle(ports, false) != ProbeMark::Ok {
        for button in mode_buttons.iter().rev() {
            let _ = mcp_probe_mark(
                ports,
                "serial_button",
                serde_json::json!({"button": button, "action": "release"}),
            );
        }
        return ProbeMark::Err;
    }
    for button in mode_buttons.iter().rev() {
        if mcp_probe_mark(
            ports,
            "serial_button",
            serde_json::json!({"button": button, "action": "release"}),
        ) != ProbeMark::Ok
        {
            return ProbeMark::Err;
        }
    }
    // The held-button cycle intentionally enters MASKROM. A second
    // cycle after release guarantees subsequent serial probes start normally.
    cycle(ports, true)
}

/// Try the MCP server tool first; when it does not PASS (unreachable or the
/// tool answered failure), fall back to the original direct-hardware probe
/// (MCP is primary, the direct code is the fallback).
///
/// The fallback closure only runs with `direct-dut-probe` — in production a
/// failed MCP call surfaces as an error instead of the init process poking
/// the DUT directly.
fn mcp_first_or_direct(
    ports: &[u16],
    tool: &str,
    arguments: serde_json::Value,
    direct: impl FnOnce() -> ProbeMark,
) -> ProbeMark {
    match mcp_probe_mark(ports, tool, arguments) {
        ProbeMark::Ok => ProbeMark::Ok,
        _ => {
            #[cfg(feature = "direct-dut-probe")]
            {
                direct()
            }
            #[cfg(not(feature = "direct-dut-probe"))]
            {
                let _ = direct;
                ProbeMark::Err
            }
        }
    }
}

/// Run the probes — ASYNC, one thread per probe (field-observed:
/// a slow/blocked probe must never stall the others; every mark streams
/// to the UI independently). SSH logins are memoized per credential tuple.
fn run_probes(
    probes: Vec<(FieldKey, ProbeTarget)>,
    tx: std::sync::mpsc::Sender<(FieldKey, ProbeMark, Option<String>)>,
) {
    use std::collections::{HashMap, HashSet};
    use std::sync::{Arc, Condvar, Mutex};
    /// (host, user, pass) → one shared login attempt's mark.
    type SshMemo = Arc<Mutex<HashMap<(String, String, String), ProbeMark>>>;
    let ssh_memo: SshMemo = Arc::new(Mutex::new(HashMap::new()));
    type ProbeResults = Arc<(Mutex<HashMap<FieldKey, ProbeMark>>, Condvar)>;
    let results: ProbeResults = Arc::new((Mutex::new(HashMap::new()), Condvar::new()));
    let mut available: HashSet<FieldKey> = probes.iter().map(|(key, _)| key.clone()).collect();
    #[cfg(not(feature = "direct-dut-probe"))]
    for (_, target) in &probes {
        if let ProbeTarget::McpBaudTest { linked_key, .. } = target {
            available.insert(linked_key.clone());
        }
    }
    for (_, target) in &probes {
        for linked_key in linked_probe_keys(target) {
            available.insert(linked_key);
        }
    }
    let mut handles = Vec::new();
    // SERIALIZATION of serial-endpoint probes (field-observed):
    // the baud check, the relay reset cycle, and the U-Boot interrupt
    // all open the SAME ser2net port — with `kickolduser` they would evict
    // each other mid-flight when run concurrently. Serial-touching probes
    // run IN LIST ORDER on ONE thread; everything else stays parallel.
    let (mut serial_bound, parallel): (Vec<_>, Vec<_>) = probes
        .into_iter()
        .partition(|(_, t)| touches_serial_endpoint(t));
    serial_bound.sort_by_key(|(_, target)| match target {
        ProbeTarget::McpNormalizeState { .. } => 0,
        #[cfg(not(feature = "direct-dut-probe"))]
        ProbeTarget::McpBaudTest { .. } => 1,
        ProbeTarget::RelayResetCycle { .. } => 2,
        ProbeTarget::UbootInterrupt { .. } => 3,
        ProbeTarget::ListDevices { .. } => 4,
        _ => 1,
    });
    {
        let tx = tx.clone();
        let memo = ssh_memo.clone();
        let results = results.clone();
        let available = available.clone();
        handles.push(std::thread::spawn(move || {
            for (key, target) in serial_bound {
                let _probe = tracing::info_span!("probe", key = ?key).entered();
                // Immediate UI feedback: the field shows "testing…" while
                // its probe runs (marks stay Pending for a long time on
                // the serial lane — one slow probe used to look like a
                // frozen TUI).
                tx.send((key.clone(), ProbeMark::Pending, Some("testing…".into())))
                    .ok();
                let linked_keys = linked_probe_keys(&target);
                let dependencies = probe_dependencies(&key, &target, &available);
                let blocked = wait_for_failed_dependency(&dependencies, &results);
                let outcome = if let Some(dependency) = blocked {
                    ProbeExecution {
                        mark: ProbeMark::Skip,
                        detail: Some(format!("blocked: prerequisite {dependency:?} did not pass")),
                        linked_mark: None,
                    }
                } else {
                    run_probe_with_detail(target, &memo)
                };
                let ProbeExecution {
                    mark,
                    detail,
                    linked_mark,
                } = outcome;
                publish_probe_result(&results, key.clone(), mark);
                tx.send((key, mark, detail.clone())).ok();
                for linked_key in linked_keys {
                    let linked_mark = linked_mark.unwrap_or(mark);
                    publish_probe_result(&results, linked_key.clone(), linked_mark);
                    tx.send((linked_key, linked_mark, detail.clone())).ok();
                }
            }
        }));
    }
    for (key, target) in parallel {
        let tx = tx.clone();
        let memo = ssh_memo.clone();
        let results = results.clone();
        let available = available.clone();
        handles.push(std::thread::spawn(move || {
            tx.send((key.clone(), ProbeMark::Pending, Some("testing…".into())))
                .ok();
            let _probe = tracing::info_span!("probe", key = ?key).entered();
            let linked_keys = linked_probe_keys(&target);
            let dependencies = probe_dependencies(&key, &target, &available);
            let blocked = wait_for_failed_dependency(&dependencies, &results);
            let outcome = if let Some(dependency) = blocked {
                ProbeExecution {
                    mark: ProbeMark::Skip,
                    detail: Some(format!("blocked: prerequisite {dependency:?} did not pass")),
                    linked_mark: None,
                }
            } else {
                run_probe_with_detail(target, &memo)
            };
            let ProbeExecution {
                mark,
                detail,
                linked_mark,
            } = outcome;
            publish_probe_result(&results, key.clone(), mark);
            tx.send((key, mark, detail.clone())).ok();
            for linked_key in linked_keys {
                let linked_mark = linked_mark.unwrap_or(mark);
                publish_probe_result(&results, linked_key.clone(), linked_mark);
                tx.send((linked_key, linked_mark, detail.clone())).ok();
            }
        }));
    }
    for h in handles {
        let _ = h.join();
    }
}

fn linked_probe_keys(target: &ProbeTarget) -> Vec<FieldKey> {
    match target {
        #[cfg(not(feature = "direct-dut-probe"))]
        ProbeTarget::McpBaudTest { linked_key, .. } => vec![linked_key.clone()],
        ProbeTarget::McpNormalizeState { linked_keys, .. } => linked_keys.clone(),
        ProbeTarget::SshPasswordLogin { linked_user, .. } => vec![linked_user.clone()],
        _ => Vec::new(),
    }
}

fn field_host_dut(key: &FieldKey) -> Option<(usize, usize)> {
    match key {
        FieldKey::DutName(hi, di)
        | FieldKey::DutPort(hi, di)
        | FieldKey::DutLoginUser(hi, di)
        | FieldKey::DutRelayGate(hi, di)
        | FieldKey::DutRelayType(hi, di)
        | FieldKey::DutRelayPort(hi, di)
        | FieldKey::DutRelayReset(hi, di)
        | FieldKey::DutRelayDownload(hi, di)
        | FieldKey::DutRelayPower(hi, di)
        | FieldKey::DutRelayResetTimeMs(hi, di)
        | FieldKey::DutRelayPowerOffTimeMs(hi, di)
        | FieldKey::DutAdvanced(hi, di, _) => Some((*hi, *di)),
        _ => None,
    }
}

fn probe_dependencies(
    key: &FieldKey,
    target: &ProbeTarget,
    available: &std::collections::HashSet<FieldKey>,
) -> Vec<FieldKey> {
    let mut dependencies = Vec::new();
    match key {
        FieldKey::HostUser(hi) | FieldKey::HostPass(hi) => {
            dependencies.push(FieldKey::HostIp(*hi));
        }
        _ => {
            if let Some((hi, di)) = field_host_dut(key) {
                dependencies.push(FieldKey::HostIp(hi));
                match target {
                    // The MCP-routed variant only exists without the
                    // direct-probe feature; the direct variant needs no
                    // extra dependency here.
                    #[cfg(not(feature = "direct-dut-probe"))]
                    ProbeTarget::McpBaudTest {
                        normalization_key: Some(key),
                        ..
                    } => dependencies.push(key.clone()),
                    ProbeTarget::UbootInterrupt {
                        normalization_key, ..
                    } => {
                        dependencies.push(FieldKey::DutAdvanced(hi, di, "serial.baudrate"));
                        if let Some(key) = normalization_key {
                            dependencies.push(key.clone());
                        }
                    }
                    ProbeTarget::RelayResetCycle { use_power, .. } => {
                        dependencies.push(if *use_power {
                            FieldKey::DutRelayPower(hi, di)
                        } else {
                            FieldKey::DutRelayReset(hi, di)
                        });
                        dependencies.push(FieldKey::DutAdvanced(hi, di, "serial.baudrate"));
                    }
                    ProbeTarget::ListDevices { .. } => {
                        dependencies.push(FieldKey::HostUser(hi));
                        dependencies.push(FieldKey::HostPass(hi));
                        dependencies.push(FieldKey::DutPort(hi, di));
                        dependencies.push(flash::devices_key(hi, di));
                    }
                    ProbeTarget::FlashListPreflight { .. } => {
                        dependencies.push(FieldKey::HostUser(hi));
                        dependencies.push(FieldKey::HostPass(hi));
                    }
                    _ => {}
                }
            }
        }
    }
    dependencies.retain(|dependency| available.contains(dependency));
    dependencies.sort();
    dependencies.dedup();
    dependencies
}

fn wait_for_failed_dependency(
    dependencies: &[FieldKey],
    results: &std::sync::Arc<(
        std::sync::Mutex<std::collections::HashMap<FieldKey, ProbeMark>>,
        std::sync::Condvar,
    )>,
) -> Option<FieldKey> {
    let (lock, changed) = &**results;
    let mut marks = lock.lock().unwrap_or_else(|error| error.into_inner());
    while dependencies
        .iter()
        .any(|dependency| !marks.contains_key(dependency))
    {
        marks = changed
            .wait(marks)
            .unwrap_or_else(|error| error.into_inner());
    }
    dependencies
        .iter()
        .find(|dependency| marks.get(*dependency) != Some(&ProbeMark::Ok))
        .cloned()
}

fn publish_probe_result(
    results: &std::sync::Arc<(
        std::sync::Mutex<std::collections::HashMap<FieldKey, ProbeMark>>,
        std::sync::Condvar,
    )>,
    key: FieldKey,
    mark: ProbeMark,
) {
    let (lock, changed) = &**results;
    lock.lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(key, mark);
    changed.notify_all();
}

/// Does this probe CONNECT to a ser2net endpoint? (They must be
/// serialized — kickolduser evicts the previous connection; a parallel
/// TcpConnect to the serial port killed the baud probe's read, user
/// report. SSH and local-device checks stay parallel.)
fn touches_serial_endpoint(t: &ProbeTarget) -> bool {
    #[cfg(feature = "direct-dut-probe")]
    {
        matches!(
            t,
            ProbeTarget::SerialBaud { .. }
                | ProbeTarget::RelayChannelStatus { .. }
                | ProbeTarget::TcpConnect { .. }
                | ProbeTarget::McpNormalizeState { .. }
                | ProbeTarget::UbootInterrupt { .. }
                | ProbeTarget::RelayResetCycle { .. }
                | ProbeTarget::ListDevices { .. }
        )
    }
    #[cfg(not(feature = "direct-dut-probe"))]
    {
        matches!(
            t,
            ProbeTarget::McpBaudTest { .. }
                | ProbeTarget::McpNormalizeState { .. }
                | ProbeTarget::TcpConnect { .. }
                | ProbeTarget::UbootInterrupt { .. }
                | ProbeTarget::RelayResetCycle { .. }
                | ProbeTarget::ListDevices { .. }
        )
    }
}

fn probe_mcp_ports(target: &ProbeTarget) -> Option<&[u16]> {
    match target {
        ProbeTarget::McpNormalizeState { ports, .. }
        | ProbeTarget::UbootInterrupt { ports, .. }
        | ProbeTarget::RelayResetCycle { ports, .. } => Some(ports),
        #[cfg(not(feature = "direct-dut-probe"))]
        ProbeTarget::McpBaudTest { ports, .. } => Some(ports),
        ProbeTarget::ListDevices { mcp_ports, .. } => Some(mcp_ports),
        _ => None,
    }
}

struct ProbeExecution {
    mark: ProbeMark,
    detail: Option<String>,
    /// A linked field normally mirrors `mark`. SSH credentials override this
    /// because successful key authentication proves the username even when
    /// the separately configured password is wrong.
    linked_mark: Option<ProbeMark>,
}

fn ssh_credential_execution(key_mark: ProbeMark, password_mark: ProbeMark) -> ProbeExecution {
    let user_mark = if key_mark == ProbeMark::Ok || password_mark == ProbeMark::Ok {
        ProbeMark::Ok
    } else {
        ProbeMark::Err
    };
    let detail = match (key_mark, password_mark) {
        (ProbeMark::Ok, ProbeMark::Ok) => "SSH key: ✓; password: ✓".to_string(),
        (ProbeMark::Ok, ProbeMark::Err) => "SSH key: ✓; password: ✗".to_string(),
        (_, ProbeMark::Ok) => "SSH key: ✗; username/password: ✓".to_string(),
        _ => "SSH key: ✗; username/password: ✗ (SSH cannot distinguish a wrong username from a wrong password)".to_string(),
    };
    ProbeExecution {
        mark: password_mark,
        detail: Some(detail),
        linked_mark: Some(user_mark),
    }
}

/// Arguments used by the init TEST for entering U-Boot. Always pass the
/// form's interrupt character explicitly: the MCP process may have loaded an
/// older on-disk config while the operator is testing an unsaved form value.
fn uboot_probe_arguments(interrupt_char: &str, soft_reboot: bool) -> serde_json::Value {
    if soft_reboot {
        serde_json::json!({
            "restart": "software",
            "interrupt_char": interrupt_char,
            "failure_retry": 1,
            "flood_interval_ms": 100,
        })
    } else {
        serde_json::json!({
            "restart": "hardware",
            "interrupt_char": interrupt_char,
            // The SPL to U-Boot window is typically 3-8s; a 10s flood
            // covers it while fitting the TEST budget (default 15s).
            "flood_duration_secs": 10.0,
        })
    }
}

/// Run one probe and retain an optional UI detail plus a distinct linked-field
/// mark when one physical check proves two fields differently.
fn run_probe_with_detail(target: ProbeTarget, ssh_memo: &SshMemoInner) -> ProbeExecution {
    #[cfg(not(feature = "direct-dut-probe"))]
    if let ProbeTarget::McpBaudTest {
        ports,
        baud,
        use_reset,
        ..
    } = &target
    {
        let outcome = probe_runtime().block_on(super::mcp_call(
            "tools/call",
            serde_json::json!({"name": "serial_test_baud", "arguments": {
                "baud": baud, "capture_secs": 3.0, "use_reset": use_reset
            }}),
            ports,
        ));
        let (mark, detail) = match outcome {
            super::McpOutcome::Result(result) => match super::tool_payload(&result) {
                Ok(payload) => (
                    ProbeMark::Ok,
                    format!(
                        "baud verified: {}",
                        payload
                            .get("requested_baud")
                            .unwrap_or(&serde_json::Value::from(*baud))
                    ),
                ),
                Err(error) => {
                    let detail = serde_json::from_str::<serde_json::Value>(&error)
                        .ok()
                        .and_then(|v| v.get("code").and_then(|c| c.as_str()).map(str::to_owned))
                        .unwrap_or(error);
                    (ProbeMark::Err, detail)
                }
            },
            super::McpOutcome::RpcError { message, .. } => (ProbeMark::Err, message),
            super::McpOutcome::Unreachable => (ProbeMark::Skip, "MCP server unreachable".into()),
        };
        return ProbeExecution {
            mark,
            detail: Some(detail),
            linked_mark: None,
        };
    }

    // The reset-cycle learn is the probe whose failure is LEAST
    // self-evident (relay channel unset, a `dutabo serial` takeover, a
    // reset that produced no boot output) — surface the server's own
    // error text on its row instead of a bare ✗.
    #[cfg(not(feature = "direct-dut-probe"))]
    if let ProbeTarget::RelayResetCycle {
        ports,
        reference_log_path,
        use_power,
        ..
    } = &target
    {
        let outcome = probe_runtime().block_on(super::mcp_call(
            "tools/call",
            serde_json::json!({"name": "serial_learn_connection", "arguments": {
                "method": "hardware",
                "hardware_action": if *use_power { "power" } else { "reset" },
                "reference_log_path": reference_log_path,
                // 2 cycles bound the failure path; a successful learn stops
                // EARLY anyway — the moment similarity ≥93% is met (the
                // cycle count is never the pass criterion).
                "cycles": 2,
                // Validate one fresh bounded prefix against the existing
                // reference. Explicit MCP learning remains exhaustive.
                "quick": true,
            }}),
            ports,
        ));
        let (mark, detail) = match outcome {
            super::McpOutcome::Result(result) => match super::tool_payload(&result) {
                Ok(payload) => (
                    ProbeMark::Ok,
                    format!(
                        "reset cycle learned: {} similar to the reference",
                        payload
                            .get("best_similarity")
                            .and_then(|value| value.as_str())
                            .unwrap_or(">= 93%")
                    ),
                ),
                Err(error) => (ProbeMark::Err, error),
            },
            super::McpOutcome::RpcError { message, .. } => (ProbeMark::Err, message),
            super::McpOutcome::Unreachable => (ProbeMark::Skip, "MCP server unreachable".into()),
        };
        return ProbeExecution {
            mark,
            detail: Some(detail),
            linked_mark: None,
        };
    }

    if let ProbeTarget::SshPasswordLogin {
        host, user, pass, ..
    } = &target
    {
        let key_mark = probe_ssh_key_login(host, user);
        let password_mark = probe_ssh_password_login(host, user, pass);
        return ssh_credential_execution(key_mark, password_mark);
    }
    if let ProbeTarget::SshLogin {
        host, user, pass, ..
    } = &target
    {
        let mark = probe_ssh_login(host, user, pass);
        return ProbeExecution {
            mark,
            detail: Some(format!("SSH key: {}", mark.symbol())),
            linked_mark: None,
        };
    }
    if let ProbeTarget::FlashListPreflight {
        host,
        user,
        pass,
        list_cmd,
    } = &target
    {
        let (mark, detail) = flash::preflight_list_devices(host, user, pass, list_cmd);
        return ProbeExecution {
            mark,
            detail,
            linked_mark: None,
        };
    }
    if let ProbeTarget::ListDevices { mcp_ports, .. } = &target {
        let ports = mcp_ports.clone();
        let result = flash::run_list_devices_probe_quick(target);
        if result.0 == ProbeMark::Ok {
            // A successful flash probe leaves the DUT in loader/MASKROM.
            // Return it to a normal boot immediately so the next TEST starts
            // from a neutral state and its baseline does not mistake this
            // same DUT for an unrelated, already-visible USB device.
            let _ = mcp_probe_mark(
                &ports,
                "serial_reset",
                serde_json::json!({"wait_boot": false, "failure_retry": 1}),
            );
        }
        return ProbeExecution {
            mark: result.0,
            detail: result.1,
            linked_mark: None,
        };
    }
    if let ProbeTarget::Skip { reason } = &target {
        return ProbeExecution {
            mark: ProbeMark::Skip,
            detail: Some(reason.clone()),
            linked_mark: None,
        };
    }
    ProbeExecution {
        mark: run_one_probe(target, ssh_memo),
        detail: None,
        linked_mark: None,
    }
}

/// One probe's mark (the per-arm body formerly inline in run_probes).
fn run_one_probe(target: ProbeTarget, ssh_memo: &SshMemoInner) -> ProbeMark {
    match target {
        ProbeTarget::SshBanner { host, port } => {
            use std::net::ToSocketAddrs;
            let Some(addr) = (host.as_str(), port)
                .to_socket_addrs()
                .ok()
                .and_then(|mut a| a.next())
            else {
                return ProbeMark::Err;
            };
            let Ok(stream) =
                std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(700))
            else {
                return ProbeMark::Err;
            };
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(300)));
            let mut buf = [0u8; 32];
            use std::io::Read;
            let ssh = (&stream)
                .read(&mut buf)
                .map(|n| String::from_utf8_lossy(&buf[..n]).contains("SSH-"))
                .unwrap_or(false);
            if ssh { ProbeMark::Ok } else { ProbeMark::Warn }
        }
        ProbeTarget::TcpConnect { host, port } => {
            use std::net::ToSocketAddrs;
            let Some(addr) = (host.as_str(), port)
                .to_socket_addrs()
                .ok()
                .and_then(|mut a| a.next())
            else {
                return ProbeMark::Err;
            };
            if std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(700))
                .is_ok()
            {
                ProbeMark::Ok
            } else {
                ProbeMark::Err
            }
        }
        ProbeTarget::CharDevice { path } => {
            if std::path::Path::new(&path).exists() {
                ProbeMark::Ok
            } else {
                ProbeMark::Err
            }
        }
        ProbeTarget::SshLogin { host, user, pass } => {
            // Memoized: the user and passwd fields share ONE real
            // login attempt per (host,user,pass).
            let mut memo = ssh_memo.lock().unwrap_or_else(|e| e.into_inner());
            let memo_key = (host.clone(), user.clone(), pass.clone());
            *memo
                .entry(memo_key)
                .or_insert_with(|| probe_ssh_login(&host, &user, &pass))
        }
        ProbeTarget::SshPasswordLogin {
            host,
            user,
            pass,
            linked_user: _,
        } => probe_ssh_password_login(&host, &user, &pass),
        #[cfg(feature = "direct-dut-probe")]
        ProbeTarget::SerialBaud { host, port, baud } => probe_serial_baud(&host, port, baud),
        ProbeTarget::AlwaysFail => ProbeMark::Err,
        #[cfg(not(feature = "direct-dut-probe"))]
        ProbeTarget::McpBaudTest {
            ports,
            baud,
            use_reset,
            normalization_key: _,
            linked_key: _,
        } => mcp_probe_mark(
            &ports,
            "serial_test_baud",
            serde_json::json!({
                "baud": baud,
                // Boot text starts immediately after reset. Three seconds
                // captures enough structure for the text classifier while
                // leaving the longer default available to CLI diagnostics.
                "capture_secs": 3.0,
                "use_reset": use_reset,
            }),
        ),
        ProbeTarget::McpNormalizeState {
            ports,
            use_power,
            mode_buttons,
            linked_keys: _,
        } => normalize_dut_state(&ports, use_power, &mode_buttons),
        #[cfg(feature = "direct-dut-probe")]
        ProbeTarget::RelayChannelStatus {
            host,
            port,
            channel,
        } => probe_relay_channel_status(&host, port, channel),
        ProbeTarget::UbootInterrupt {
            ports,
            interrupt_char,
            soft_reboot,
            normalization_key: _,
            #[cfg(feature = "direct-dut-probe")]
            direct,
        } => mcp_first_or_direct(
            &ports,
            "serial_enter_uboot",
            uboot_probe_arguments(&interrupt_char, soft_reboot),
            || {
                #[cfg(feature = "direct-dut-probe")]
                {
                    probe_uboot_interrupt(
                        &direct.relay_host,
                        direct.relay_port,
                        direct.channel,
                        &direct.serial_host,
                        direct.serial_port,
                        direct.interrupt_char,
                        direct.reset_hold_ms,
                    )
                }
                #[cfg(not(feature = "direct-dut-probe"))]
                {
                    ProbeMark::Err
                }
            },
        ),
        ProbeTarget::Skip { reason } => {
            // run_one_probe has no detail lane; the reason was preset into
            // `probe_details` when the target was built.
            let _ = reason;
            ProbeMark::Skip
        }
        ProbeTarget::RelayResetCycle {
            ports,
            reference_log_path,
            use_power,
            #[cfg(feature = "direct-dut-probe")]
            direct,
        } => mcp_first_or_direct(
            &ports,
            "serial_learn_connection",
            serde_json::json!({
                "method": "hardware",
                "hardware_action": if use_power { "power" } else { "reset" },
                "reference_log_path": reference_log_path,
                // 2 cycles bound the failure path; a successful learn stops
                // EARLY anyway — the moment similarity ≥93% is met (the
                // cycle count is never the pass criterion).
                "cycles": 2,
                // Validate one fresh bounded prefix against the existing
                // reference. Explicit MCP learning remains exhaustive.
                "quick": true,
            }),
            || {
                #[cfg(feature = "direct-dut-probe")]
                {
                    probe_relay_reset_cycle(
                        &direct.relay_host,
                        direct.relay_port,
                        direct.channel,
                        &direct.serial_host,
                        direct.serial_port,
                        &direct.save_path,
                        direct.reset_hold_ms,
                    )
                }
                #[cfg(not(feature = "direct-dut-probe"))]
                {
                    ProbeMark::Err
                }
            },
        ),
        // The flash device-list probe runs through
        // `run_probe_with_detail` — unreachable here.
        ProbeTarget::ListDevices { .. } => ProbeMark::Skip,
        ProbeTarget::FlashListPreflight {
            host,
            user,
            pass,
            list_cmd,
        } => flash::preflight_list_devices(&host, &user, &pass, &list_cmd).0,
    }
}

/// The unsynchronized inner type of [`SSH_MEMO`] (kept simple for the
/// helper's signature).
type SshMemoInner =
    std::sync::Mutex<std::collections::HashMap<(String, String, String), ProbeMark>>;

/// The scrollbar thumb for one container: track = the container's inner
/// height, content = its content height. None when nothing overflows.
/// Thumb height = max(1, track²/content); y = eff_scroll·(track-thumb)/overflow.
fn scrollbar_thumb(inner: Rect, content: usize, scroll: usize) -> Option<Rect> {
    let track = inner.height.saturating_sub(2);
    if track == 0 {
        return None;
    }
    let overflow = content.saturating_sub(inner.height as usize);
    if overflow == 0 {
        return None;
    }
    let eff = scroll.min(overflow);
    let thumb_h = (((track as usize).saturating_mul(track as usize)) / content).max(1) as u16;
    let y = inner.y.saturating_add(1).saturating_add(
        ((eff as u16).saturating_mul(track.saturating_sub(thumb_h))) / overflow.max(1) as u16,
    );
    Some(Rect {
        x: inner.right(),
        y,
        width: 1,
        height: thumb_h,
    })
}

/// Shift a rect up by the scroll offset, clipped to the visible container —
/// None when nothing remains visible. The offset stays negative-capable
/// through the clip intersection: a multi-row element scrolled past the
/// top edge keeps only its BOTTOM slice visible, and one entirely above
/// the clip returns None.
fn shift_rect(rect: Rect, scroll: usize, clip: Rect) -> Option<Rect> {
    let top = (rect.y as i32) - (scroll as i32);
    let bottom = top.saturating_add(rect.height as i32);
    let clip_top = clip.y as i32;
    let clip_bottom = clip_top.saturating_add(clip.height as i32);
    let y0 = top.max(clip_top);
    let y1 = bottom.min(clip_bottom);
    if y1 <= y0 {
        return None;
    }
    let x0 = rect.x.max(clip.x);
    let x1 = rect
        .x
        .saturating_add(rect.width)
        .min(clip.x.saturating_add(clip.width));
    if x1 <= x0 {
        return None;
    }
    Some(Rect {
        x: x0,
        y: y0 as u16,
        width: x1 - x0,
        height: (y1 - y0) as u16,
    })
}
// ── Render (pure; draws via the generic Backend) ───────────────────────

/// The deep-navy palette per the design spec: background #061424, panel bg
/// #07182A, gray-blue borders, CURRENT tab = medium deep blue, titles =
/// bright blue (ACCENT), text = light gray/cream, dividers = low-contrast
/// gray-blue. `x`/`+` render in normal light gray (NEVER red).
const ACCENT: Color = Color::Rgb(137, 180, 250);
const PANEL_BG: Color = Color::Rgb(7, 24, 42);
const SURFACE0: Color = Color::Rgb(14, 42, 71);
const OVERLAY0: Color = Color::Rgb(47, 72, 102);
const OVERLAY1: Color = Color::Rgb(96, 120, 150);
const TEXT: Color = Color::Rgb(214, 222, 235);
/// The whole-screen background (#061424).
const BACKGROUND: Color = Color::Rgb(6, 20, 36);
/// Amber marker for required fields — distinct
/// from error red and warning yellow.
const REQUIRED_MARK: Color = Color::Rgb(250, 179, 135);
/// The CURRENT tab's medium deep blue (ACCENT-darkened).
const TAB_ACTIVE: Color = Color::Rgb(36, 86, 140);
/// Unfocused input fields (quiet surface0); FOCUSED brightens + bolds.
const FIELD_BG: Color = SURFACE0;
const FOCUSED_FIELD_BG: Color = Color::Rgb(29, 78, 137);
/// Cells kept clear at the screen's right edge: the right-aligned header
/// status and the footer button strip stop here instead of touching the
/// terminal's right border.
const RIGHT_MARGIN: u16 = 2;

/// Render one frame. The layout rects come from `form_layout` — the same
/// function the mouse hit-tester uses.
pub fn render_form(f: &mut Frame<'_>, state: &mut FormState) {
    let area = f.area();
    f.buffer_mut().set_style(area, Style::new().bg(BACKGROUND));
    if state.pane.view == pane::View::Full {
        pane::render(f, state);
        return;
    }
    let map = form_layout(area, state);
    let host_max = map
        .host_content_height
        .saturating_sub(map.host_panel_inner.height as usize);
    let dut_max = map
        .dut_content_height
        .saturating_sub(map.dut_content_inner.height as usize);
    state.scroll = state.scroll.min(host_max.max(dut_max));
    keep_focus_visible(state, &map);
    render_header(f, state, &map);
    render_host_tabs(f, state, &map);
    render_host_panel(f, state, &map);
    render_dut_panel(f, state, &map);
    render_divider(f, state, &map);
    render_scrollbars(f, &map);
    render_footer_hints(f, state, &map);
    pane::render_actions(f, state, &map);
    pane::render(f, state);
    render_picker(f, state, &map);
    project::render_selector(f, state, &map);
    if state.footer_hints_visible {
        let area = f.area();
        let popup = Rect::new(
            area.x + area.width.saturating_sub(64) / 2,
            area.y + area.height.saturating_sub(6) / 2,
            area.width.min(64),
            area.height.min(6),
        );
        f.render_widget(ratatui::widgets::Clear, popup);
        let text = map
            .footer_chips
            .iter()
            .map(|(s, _)| s.as_str())
            .collect::<Vec<_>>()
            .join("  ");
        f.render_widget(
            Paragraph::new(text)
                .wrap(ratatui::widgets::Wrap { trim: true })
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("Keys — ? to close"),
                ),
            popup,
        );
    }
    render_busy_gate(f, state);
}

/// The controller-picker DROPDOWN: anchored directly BELOW the Select
/// row it belongs to (the "\u{25be}" spot — deliberate: no
/// centered modal). \u{2191}/\u{2193} move, Enter picks, Esc cancels (key handling
/// lives in `handle_form_key` — while open, the picker consumes every
/// key). Falls back to centering only when the row is scrolled away.
/// The "not implemented" note for a stub loader choice (loader=uefi): the
/// field keeps showing the raw value like dev_ctrl, and the note renders
/// on a sub-line BELOW it (requested).
fn loader_stub_note(key: &FieldKey, value: &str) -> Option<&'static str> {
    if matches!(key, FieldKey::DutAdvanced(_, _, "loader")) {
        sermcp::loader::not_implemented_note(value)
    } else {
        None
    }
}

/// The dim "not implemented" note under a stub-loader field (loader=uefi).
/// The error sub-line wins when both would occupy the row.
fn render_loader_stub_note(
    f: &mut Frame<'_>,
    state: &FormState,
    map: &LayoutMap,
    row: &RowSpec,
    in_host: bool,
) {
    let Some(sr) = &row.sub_rect else { return };
    let Some(key) = row.key.as_ref() else { return };
    if state.errors.contains_key(key) {
        return; // the error sub-line owns this row
    }
    let value = state.values.get(key).cloned().unwrap_or_default();
    let Some(note) = loader_stub_note(key, &value) else {
        return;
    };
    let Some(srect) = row_shift(state, map, *sr, in_host) else {
        return;
    };
    f.render_widget(
        Paragraph::new(Span::styled(
            note,
            Style::new().fg(OVERLAY0).add_modifier(Modifier::ITALIC),
        )),
        srect,
    );
}

/// Point-in-rect for absolute terminal coordinates (mouse.column/row).
fn rect_contains(r: Rect, column: u16, row: u16) -> bool {
    column >= r.x && column < r.x + r.width && row >= r.y && row < r.y + r.height
}

/// The dropdown popup rect for the open picker. ONE computation shared by
/// the renderer and the mouse hit-tester, so the visual and event
/// z-orders can never drift. ComboBox semantics: popup.x and
/// popup.width MATCH the field's VALUE cell, anchored directly BELOW it
/// (a stub-loader field's "not implemented" sub-line is included in the
/// field's height).
fn picker_popup_rect(state: &FormState, map: &LayoutMap, area: Rect) -> Option<Rect> {
    let picker = state.picker.as_ref()?;
    let anchor = map
        .fields
        .iter()
        .find(|(k, _)| *k == picker.key)
        .and_then(|(_, r)| row_shift(state, map, *r, field_in_host(&picker.key)));
    let note_row = map
        .dut_rows
        .iter()
        .chain(map.host_rows.iter())
        .find(|r| r.key.as_ref() == Some(&picker.key))
        .is_some_and(|r| r.sub_rect.is_some());
    let w = anchor
        .map(|r| r.width)
        .unwrap_or(32)
        .min(area.width.saturating_sub(2));
    let h = (picker.options.len() as u16 + 2).min(area.height.saturating_sub(2));
    Some(match anchor {
        Some(r) => Rect {
            x: r.x.min(area.right().saturating_sub(w)),
            y: (r.y + if note_row { 2 } else { 1 }).min(area.bottom().saturating_sub(h)),
            width: w,
            height: h,
        },
        None => Rect {
            x: area.x + (area.width.saturating_sub(w)) / 2,
            y: area.y + (area.height.saturating_sub(h)) / 2,
            width: w,
            height: h,
        },
    })
}

fn render_picker(f: &mut Frame<'_>, state: &mut FormState, map: &LayoutMap) {
    let Some(picker) = &state.picker else { return };
    let area = f.area();
    let Some(rect) = picker_popup_rect(state, map, area) else {
        return;
    };
    // The popup is drawn LAST (after the form) so it sits on top; the
    // mouse hit-tester uses the SAME rect (visual z-order == event
    // z-order).
    f.render_widget(ratatui::widgets::Clear, rect);
    let current = state.values.get(&picker.key).cloned().unwrap_or_default();
    let items: Vec<ListItem> = picker
        .options
        .iter()
        .map(|opt| {
            // Options render verbatim (like dev_ctrl) — the loader's
            // "uefi" stub note lives BELOW the field, not in the list.
            let prefix = if *opt == current { "✓ " } else { "  " };
            ListItem::new(format!("{prefix}{opt}"))
        })
        .collect();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(Style::new().fg(OVERLAY1).add_modifier(Modifier::BOLD));
    let list = List::new(items).block(block).highlight_style(
        Style::new()
            .fg(Color::White)
            .bg(ACCENT)
            .add_modifier(Modifier::BOLD),
    );
    let mut list_state = ListState::default();
    list_state.select(Some(picker.cursor));
    f.render_stateful_widget(list, rect, &mut list_state);
}

/// The occupancy-gate modal: a centered box listing every
/// serial endpoint held by ANOTHER live MCP (read-only diagnostics — the
/// holder's session is never signaled or stopped) with a
/// [continue writing | abort] choice. Drawn LAST so it sits above the
/// form; its own event loop lives in `FormTui::busy_endpoints`. Conflict
/// lines wrap (long project paths must stay readable), so the modal grows
/// a little headroom; worst case the bottom clips — still safe, because
/// Enter/Esc both default to abort.
fn render_busy_gate(f: &mut Frame<'_>, state: &FormState) {
    let Some(gate) = &state.busy_gate else { return };
    let area = f.area();
    let mut body: Vec<Line<'_>> = Vec::new();
    for c in &gate.conflicts {
        body.push(Line::from(format!(
            "• {}:{} — PID {}",
            c.host, c.port, c.pid
        )));
        let mut detail = String::new();
        if let Some(p) = &c.owner_project {
            detail.push_str(&format!("  project {p}"));
        }
        if !c.since.is_empty() {
            detail.push_str(&format!(", since {}", c.since));
        }
        if !detail.is_empty() {
            body.push(Line::from(detail));
        }
    }
    body.push(Line::from(""));
    body.push(Line::from(
        "The other MCP's session is NOT touched — it keeps running.",
    ));
    body.push(Line::from("Continue writing the config, or abort now?"));
    body.push(Line::from(""));
    let active_style = Style::new()
        .fg(Color::Black)
        .bg(ACCENT)
        .add_modifier(Modifier::BOLD);
    let idle_style = Style::new().fg(OVERLAY1);
    let mut spans = Vec::new();
    for (i, label) in ["continue writing", "abort"].iter().enumerate() {
        let style = if i == gate.cursor {
            active_style
        } else {
            idle_style
        };
        spans.push(Span::styled(format!("[ {label} ]"), style));
        spans.push(Span::raw("   "));
    }
    body.push(Line::from(spans));
    let content_w = body.iter().map(|l| l.width()).max().unwrap_or(0) as u16;
    let w = (content_w + 4).clamp(24, area.width.saturating_sub(2));
    let h = (body.len() as u16 + 4).min(area.height.saturating_sub(2));
    let rect = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    f.render_widget(ratatui::widgets::Clear, rect);
    let block = Block::default()
        .title(" serial endpoint held by another MCP ")
        .borders(Borders::ALL)
        .border_style(Style::new().fg(OVERLAY1).add_modifier(Modifier::BOLD));
    let paragraph = Paragraph::new(body)
        .block(block)
        .wrap(ratatui::widgets::Wrap { trim: true });
    f.render_widget(paragraph, rect);
}

/// Shift a scrollable rect with the SAME shift the renderer paints (per
/// container clamp; the host panel and the DUT content box each clip to
/// their own interior). Shared by render + mouse hit-testing.
///
/// `in_host` names the OWNING container — it must NOT be derived from the
/// rect's absolute Y: the expanded advanced block can overflow BELOW the
/// host panel, and its rows would otherwise be misclassified as DUT rows
/// and painted inside the DUT content box.
fn row_shift(state: &FormState, map: &LayoutMap, rect: Rect, in_host: bool) -> Option<Rect> {
    let (max, clip) = if in_host {
        (
            map.host_content_height
                .saturating_sub(map.host_panel_inner.height as usize),
            map.host_panel_inner,
        )
    } else {
        (
            map.dut_content_height
                .saturating_sub(map.dut_content_inner.height as usize),
            map.dut_content_inner,
        )
    };
    shift_rect(rect, state.scroll.min(max), clip)
}

/// Keep the focused field visible (scroll adjusts here, after layout).
fn keep_focus_visible(state: &mut FormState, map: &LayoutMap) {
    let rect = match &state.focus {
        FocusSlot::Field(focused) => map
            .fields
            .iter()
            .find(|(k, _)| k == focused)
            .map(|(_, r)| *r),
        _ => return,
    };
    let Some(rect) = rect else {
        return;
    };
    // The owning container follows the FIELD's key (never its Y — an
    // overflowed HOST-LEVEL advanced field sits below the host panel yet
    // belongs to it; the DUT-owned advanced keys belong to the DUT box).
    let in_host = match &state.focus {
        FocusSlot::Field(k) => field_in_host(k),
        _ => true,
    };
    let (max, clip) = if in_host {
        (
            map.host_content_height
                .saturating_sub(map.host_panel_inner.height as usize),
            map.host_panel_inner,
        )
    } else {
        (
            map.dut_content_height
                .saturating_sub(map.dut_content_inner.height as usize),
            map.dut_content_inner,
        )
    };
    if (rect.y as usize) < state.scroll.saturating_add(clip.y as usize) {
        state.scroll = (rect.y as usize).saturating_sub(clip.y as usize);
    }
    if (rect.bottom() as usize) > state.scroll.saturating_add(clip.bottom() as usize) {
        state.scroll = state
            .scroll
            .saturating_add((rect.bottom() as usize).saturating_sub(clip.bottom() as usize));
    }
    state.scroll = state.scroll.min(max);
}

/// The header status line: `dutabo init — <path>` on the left, the error /
/// TEST summary right-aligned with a `RIGHT_MARGIN`-cell gap before the
/// terminal's right border. The CREATE/UPDATE mode is NOT repeated here
/// (the written diff already shows it) — dropping it leaves the path label
/// its room. The two halves get disjoint rects: a summary long enough to
/// reach the label clips the LABEL, never overlaps it.
fn render_header(f: &mut Frame<'_>, state: &FormState, map: &LayoutMap) {
    let left = format!("dutabo init — {}", state.target_label);
    let n = state.errors.len();
    let mut right_spans = Vec::new();
    if n > 0 {
        right_spans.push(Span::styled(
            format!("{n} error{}", if n == 1 { "" } else { "s" }),
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
    }
    if let Some((NoteKind::Error, msg)) = &state.note {
        right_spans.push(Span::styled(msg.clone(), Style::new().fg(Color::Red)));
    }
    if let Some((text, worst)) = &state.test_summary {
        right_spans.push(Span::styled(text.clone(), Style::new().fg(worst.color())));
    }
    // " · " between the status parts, none for a single one.
    let right_len: u16 = right_spans
        .iter()
        .map(|s| s.content.chars().count() as u16)
        .sum::<u16>()
        .saturating_add(3 * right_spans.len().saturating_sub(1) as u16);
    let width = right_len.min(map.header.width.saturating_sub(RIGHT_MARGIN));
    if width > 0 {
        let mut line = Line::default();
        for (i, span) in right_spans.into_iter().enumerate() {
            if i > 0 {
                line.push_span(Span::styled(" · ", span.style));
            }
            line.push_span(span);
        }
        f.render_widget(
            Paragraph::new(line).alignment(Alignment::Right),
            Rect {
                x: map
                    .header
                    .right()
                    .saturating_sub(RIGHT_MARGIN)
                    .saturating_sub(width),
                y: map.header.y,
                width,
                height: 1,
            },
        );
    }
    // One blank cell separates the label from the summary.
    let label_width = map
        .header
        .width
        .saturating_sub(RIGHT_MARGIN)
        .saturating_sub(width)
        .saturating_sub(u16::from(width > 0));
    f.render_widget(
        Paragraph::new(left),
        Rect {
            x: map.header.x,
            y: map.header.y,
            width: label_width,
            height: 1,
        },
    );
}

/// The HOST TAB BAR: `dev host1  x │ dev host2  x │ +` — the CURRENT host
/// tab fills TAB_ACTIVE with bold TEXT, the others sit quiet on SURFACE0;
/// `x`/`+` are plain light-gray (never red); the focusable `+` reverses
/// while focused, `x` acts on click and never takes focus.
fn render_host_tabs(f: &mut Frame<'_>, state: &FormState, map: &LayoutMap) {
    // The bar's quiet panel-bg surface behind the cards.
    f.buffer_mut()
        .set_style(map.host_tab_bar, Style::new().bg(PANEL_BG));
    for (hi, rect) in &map.host_tabs {
        let label = host_display_name(state, *hi);
        let active = *hi == state.selected_host;
        let style = if active {
            Style::new()
                .bg(TAB_ACTIVE)
                .fg(TEXT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new().bg(SURFACE0).fg(OVERLAY1)
        };
        // `x` never takes focus (clicking it acts immediately) — it
        // renders plain light-gray, never red.
        let x_style = Style::new().fg(TEXT);
        let line = Line::from(vec![
            Span::styled(format!("{label}  "), style),
            Span::styled("x ", x_style),
        ]);
        f.render_widget(Paragraph::new(line), *rect);
        // The │ separator cell between cards.
        if let Some(cell) = f.buffer_mut().cell_mut((rect.right(), rect.y)) {
            cell.set_symbol("│").set_style(Style::new().fg(OVERLAY0));
        }
    }
    // The trailing + card.
    if let Some((_, plus)) = map.buttons.iter().find(|(b, _)| *b == ButtonId::AddHost) {
        let focused = state.focus == FocusSlot::Button(ButtonId::AddHost);
        let style = if focused {
            Style::new()
                .fg(TEXT)
                .bg(SURFACE0)
                .add_modifier(Modifier::REVERSED)
        } else {
            Style::new().fg(TEXT).bg(SURFACE0)
        };
        f.render_widget(Paragraph::new(Span::styled(" + ", style)), *plus);
    }
}

/// The DEVELOPMENT HOST panel: bordered frame with the bright-blue title,
/// `[Other ▼]` at the panel's top-right and the ip/user/pass editors WITH
/// brackets (advanced pairs inside when expanded). Key-less rows are the
/// dim hint/note rows ("No dev hosts", host-count notes).
fn render_host_panel(f: &mut Frame<'_>, state: &mut FormState, map: &LayoutMap) {
    let block = Block::default()
        .borders(Borders::TOP | Borders::LEFT | Borders::RIGHT)
        .border_style(Style::new().fg(OVERLAY1))
        .title(Span::styled(
            "DEVELOPMENT HOST",
            Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
    f.render_widget(block, map.host_panel);
    // [Other ▼] / [Other ▲] on the top border row.
    if let Some(r) = map.other_btn {
        let label = if state.other_expanded {
            "[Other ▲]"
        } else {
            "[Other ▼]"
        };
        let focused = state.focus == FocusSlot::Button(ButtonId::ToggleOther);
        let style = if focused {
            Style::new()
                .fg(ACCENT)
                .add_modifier(Modifier::REVERSED)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(OVERLAY1)
        };
        f.render_widget(
            Paragraph::new(Span::styled(label, style)).alignment(Alignment::Right),
            r,
        );
    }
    for row in &map.host_rows {
        let Some(vrect) = row_shift(state, map, row.value_rect, true) else {
            continue;
        };
        let lrect = row_shift(state, map, row.label_rect, true);
        match &row.key {
            None => {
                if row.dim {
                    f.render_widget(
                        Paragraph::new(Span::styled(&row.label, Style::new().fg(OVERLAY0))),
                        vrect,
                    );
                }
                continue;
            }
            Some(key) => {
                // Disabled relay rows keep their layout slot as BLANK
                // reserved space (constant box height — no jump) — the
                // row paints NOTHING: no label, no value, no markers.
                if relay_row_disabled(state, key) {
                    continue;
                }
                let err: Option<String> = state.errors.get(key).cloned();
                let label_style = if err.is_some() {
                    Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)
                } else {
                    Style::new().fg(TEXT)
                };
                if let Some(lrect) = lrect {
                    f.render_widget(
                        Paragraph::new(Span::styled(&row.label, label_style))
                            .alignment(Alignment::Left),
                        lrect,
                    );
                    if row.required
                        && let Some(cell) = f.buffer_mut().cell_mut((
                            lrect.x.saturating_add(row.label.chars().count() as u16 + 1),
                            lrect.y,
                        ))
                    {
                        cell.set_symbol("*").set_style(if err.is_some() {
                            label_style
                        } else {
                            Style::new().fg(REQUIRED_MARK)
                        });
                    }
                    if let Some(mark) = state.probes.get(key)
                        && let Some(cell) = f
                            .buffer_mut()
                            .cell_mut((lrect.right().saturating_sub(1), lrect.y))
                    {
                        cell.set_symbol(mark.symbol())
                            .set_style(Style::new().fg(mark.color()));
                    }
                    if let Some(cell) = f.buffer_mut().cell_mut((lrect.right(), lrect.y)) {
                        cell.set_symbol(":").set_style(label_style);
                    }
                }
                // Brackets around the editor.
                if let Some(cell) = f
                    .buffer_mut()
                    .cell_mut((vrect.x.saturating_sub(1), vrect.y))
                {
                    cell.set_symbol("[").set_style(Style::new().fg(OVERLAY1));
                }
                if let Some(cell) = f.buffer_mut().cell_mut((vrect.right(), vrect.y)) {
                    cell.set_symbol("]").set_style(Style::new().fg(OVERLAY1));
                }
                render_field_value(f, state, key, vrect, err.as_ref(), false);
                render_error_subline(f, state, map, row, true);
                render_auth_probe_subline(f, state, map, row, true);
                render_loader_stub_note(f, state, map, row, true);
            }
        }
    }
    // Amber append-only note (host-count shrink) at its LAID-OUT row —
    // the layout extended the content height for it (the
    // hugging panel never overlaps the last field row).
    if let (Some((NoteKind::Info, msg)), Some(rect)) = (&state.note, map.host_note)
        && msg.contains("host count")
    {
        let shifted = row_shift(state, map, rect, true);
        if let Some(rect) = shifted {
            f.render_widget(
                Paragraph::new(Span::styled(msg.clone(), Style::new().fg(Color::Yellow))),
                rect,
            );
        }
    }
}

/// The DUTs panel: outer frame (title `DUTs`), the DUT TAB BAR inside, and
/// the multi-column content box for the ONE selected DUT.
fn render_dut_panel(f: &mut Frame<'_>, state: &mut FormState, map: &LayoutMap) {
    // A docked pane draws the shared bottom line itself (its top border) —
    // the panel then closes with its LEFT/RIGHT borders only.
    let borders = if pane_shares_bottom_line(state) {
        Borders::LEFT | Borders::RIGHT
    } else {
        Borders::LEFT | Borders::RIGHT | Borders::BOTTOM
    };
    let block = Block::default()
        .borders(borders)
        .border_style(Style::new().fg(OVERLAY1));
    f.render_widget(block, map.dut_panel);
    if state.host_count == 0 {
        return; // NO DUTs panel content
    }
    render_dut_tabs(f, state, map);
    render_dut_content(f, state, map);
}

/// The DUT TAB BAR inside the DUTs panel: `dut1  x │ dut2  x │ +` — the
/// CURRENT DUT tab fills TAB_ACTIVE, the others sit quiet on SURFACE0;
/// `x`/`+` are plain light-gray (never red); the focusable `+` reverses
/// while focused, `x` acts on click and never takes focus.
fn render_dut_tabs(f: &mut Frame<'_>, state: &FormState, map: &LayoutMap) {
    // DUT tab bar on the quiet panel-bg surface.
    f.buffer_mut()
        .set_style(map.dut_tab_bar, Style::new().bg(PANEL_BG));
    for (di, rect) in &map.dut_tabs {
        let label = dut_display_name(state, state.selected_host, *di);
        let active = Some(*di) == state.selected_dut;
        let style = if active {
            Style::new()
                .bg(TAB_ACTIVE)
                .fg(TEXT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new().bg(SURFACE0).fg(OVERLAY1)
        };
        // `x` never takes focus (clicking it acts immediately) — it
        // renders plain light-gray, never red.
        let x_style = Style::new().fg(TEXT);
        let line = Line::from(vec![
            Span::styled(format!("{label}  "), style),
            Span::styled("x ", x_style),
        ]);
        f.render_widget(Paragraph::new(line), *rect);
        if let Some(cell) = f.buffer_mut().cell_mut((rect.right(), rect.y)) {
            cell.set_symbol("│").set_style(Style::new().fg(OVERLAY0));
        }
    }
    if let Some((_, plus)) = map
        .buttons
        .iter()
        .find(|(b, _)| *b == ButtonId::AddDut(state.selected_host))
    {
        let focused = state.focus == FocusSlot::Button(ButtonId::AddDut(state.selected_host));
        let style = if focused {
            Style::new()
                .fg(TEXT)
                .bg(SURFACE0)
                .add_modifier(Modifier::REVERSED)
        } else {
            Style::new().fg(TEXT).bg(SURFACE0)
        };
        f.render_widget(Paragraph::new(Span::styled(" + ", style)), *plus);
    }
}

/// The DUT CONTENT box: `Current DUT: dutN` line 1, then the two columns
/// (left/right by width) split by a 1-cell │ separator, one dim dashed
/// TITLE rule per field box, values WITHOUT brackets. The frame goes red
/// when the shown DUT carries an inline error; the amber append-only note
/// renders as a transient line at the box's bottom edge.
fn render_dut_content(f: &mut Frame<'_>, state: &mut FormState, map: &LayoutMap) {
    // Content box.
    let red = state
        .selected_dut
        .is_some_and(|di| dut_has_errors(state, state.selected_host, di));
    let cblock = Block::default()
        .borders(if pane_shares_bottom_line(state) {
            // The bottom row is the shared line (the pane's top border).
            Borders::TOP | Borders::LEFT | Borders::RIGHT
        } else {
            Borders::ALL
        })
        .border_style(Style::new().fg(if red { Color::Red } else { OVERLAY0 }));
    f.render_widget(cblock, map.dut_content);
    // Each │ separator column shifts + clips WITH the content (the SAME
    // row_shift math as the rows) — it must never leave the content box
    // interior. (the box titles ARE the horizontal rules, so
    // there are no separate rule rows and no ┼ crossings to paint.)
    for separator in &map.separators {
        let Some(srect) = row_shift(state, map, *separator, false) else {
            continue;
        };
        for y in srect.y..srect.bottom() {
            if let Some(cell) = f.buffer_mut().cell_mut((srect.x, y)) {
                cell.set_symbol("│").set_style(Style::new().fg(OVERLAY0));
            }
        }
    }
    // Rows.
    for row in &map.dut_rows {
        let Some(vrect) = row_shift(state, map, row.value_rect, false) else {
            continue;
        };
        let lrect = row_shift(state, map, row.label_rect, false);
        match &row.key {
            None => {
                if row.label.starts_with("Current DUT") {
                    let style = if row.dim {
                        Style::new().fg(OVERLAY0)
                    } else {
                        Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)
                    };
                    f.render_widget(Paragraph::new(Span::styled(&row.label, style)), vrect);
                } else if row.dim {
                    f.render_widget(
                        Paragraph::new(Span::styled(&row.label, Style::new().fg(OVERLAY0))),
                        vrect,
                    );
                }
                continue;
            }
            Some(key) => {
                // Disabled relay rows keep their layout slot as BLANK
                // reserved space (constant box height — no jump) — the
                // row paints NOTHING: no label, no value, no markers.
                if relay_row_disabled(state, key) {
                    continue;
                }
                let err: Option<String> = state.errors.get(key).cloned();
                let label_style = if err.is_some() {
                    Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)
                } else {
                    Style::new().fg(TEXT)
                };
                if let Some(lrect) = lrect {
                    f.render_widget(
                        Paragraph::new(Span::styled(&row.label, label_style))
                            .alignment(Alignment::Left),
                        lrect,
                    );
                    if row.required
                        && let Some(cell) = f.buffer_mut().cell_mut((
                            lrect.x.saturating_add(row.label.chars().count() as u16 + 1),
                            lrect.y,
                        ))
                    {
                        cell.set_symbol("*").set_style(if err.is_some() {
                            label_style
                        } else {
                            Style::new().fg(REQUIRED_MARK)
                        });
                    }
                    if let Some(mark) = state.probes.get(key)
                        && let Some(cell) = f
                            .buffer_mut()
                            .cell_mut((lrect.right().saturating_sub(1), lrect.y))
                    {
                        cell.set_symbol(mark.symbol())
                            .set_style(Style::new().fg(mark.color()));
                    }
                    if let Some(cell) = f.buffer_mut().cell_mut((lrect.right(), lrect.y)) {
                        cell.set_symbol(":").set_style(label_style);
                    }
                }
                // Values WITHOUT brackets (plain editors).
                if flash::is_devices_key(key) {
                    // Read-only device-list row: the probe detail (or a
                    // short result) renders through the SAME editor path
                    // as the other field values — identical background,
                    // underline and text color, so the style is uniform.
                    let inner = state
                        .probe_details
                        .get(key)
                        .cloned()
                        .filter(|d| !d.is_empty())
                        .unwrap_or_else(|| match state.probes.get(key).copied() {
                            Some(ProbeMark::Ok) => "device detected".into(),
                            Some(ProbeMark::Warn) => "no new device".into(),
                            Some(ProbeMark::Err) => "list failed".into(),
                            Some(ProbeMark::Pending) => "…".into(),
                            _ => String::new(),
                        });
                    let mut view_offset = 0usize;
                    render_editor(
                        f,
                        vrect,
                        &inner,
                        0,
                        &mut view_offset,
                        false,
                        Vec::new(),
                        Style::new(),
                    );
                } else {
                    render_field_value(f, state, key, vrect, err.as_ref(), true);
                }
                render_error_subline(f, state, map, row, false);
                render_auth_probe_subline(f, state, map, row, false);
                render_loader_stub_note(f, state, map, row, false);
            }
        }
    }
    // Amber append-only note (DUT-count shrink) at its LAID-OUT row —
    // the layout extended the content height for it (the
    // hugging box never overlaps the last field row).
    if let (Some((NoteKind::Info, msg)), Some(rect)) = (&state.note, map.dut_note)
        && msg.contains("DUT count")
    {
        let shifted = row_shift(state, map, rect, false);
        if let Some(rect) = shifted {
            f.render_widget(
                Paragraph::new(Span::styled(msg.clone(), Style::new().fg(Color::Yellow))),
                rect,
            );
        }
    }
}

/// One field's value: the single-line editor (text/select — the per-DUT
/// relay gate toggle row is gone). `with_hint` = show the dim
/// default hint (DUT content rows only — the host panel's painted brackets
/// make the bracketed hint redundant).
fn render_field_value(
    f: &mut Frame<'_>,
    state: &mut FormState,
    key: &FieldKey,
    vrect: Rect,
    err: Option<&String>,
    with_hint: bool,
) {
    let buffer = state.values.get(key).cloned().unwrap_or_default();
    let focused = state.focus == FocusSlot::Field(key.clone());
    // SELECT fields render as a static ComboBox cell (pick-only — the
    // controller picker owns the choice): the value LEFT-aligned with the
    // ▼/▲ arrow pinned to the RIGHT edge. The loader shows its raw value
    // ("uefi", exactly like dev_ctrl shows its backend); the "not
    // implemented" note renders on a sub-line BELOW the field instead
    //.
    if let Some(field) = state.fields.iter().find(|f| &f.key == key)
        && field.kind == FieldKind::Select
    {
        let display: String = if buffer.trim().is_empty() {
            let hint = field
                .options
                .iter()
                .map(|o| o.as_str())
                .collect::<Vec<_>>()
                .join(" | ");
            let room = vrect.width.saturating_sub(3) as usize;
            format!("[{}]", fit_label(&hint, room))
        } else {
            buffer.trim().to_string()
        };
        let opened = state.picker.as_ref().is_some_and(|p| &p.key == key);
        let value_style = if err.is_some() {
            Style::new().fg(Color::Red)
        } else {
            Style::new().fg(TEXT)
        };
        let arrow_w = 2u16;
        let text_area = Rect {
            x: vrect.x,
            y: vrect.y,
            width: vrect.width.saturating_sub(arrow_w),
            height: 1,
        };
        let arrow_area = Rect {
            x: vrect.right().saturating_sub(arrow_w),
            y: vrect.y,
            width: arrow_w,
            height: 1,
        };
        f.render_widget(
            Paragraph::new(Span::styled(display, value_style)).alignment(Alignment::Left),
            text_area,
        );
        f.render_widget(
            Paragraph::new(Span::styled(
                if opened { "▲" } else { "▼" },
                Style::new().fg(if focused { ACCENT } else { OVERLAY1 }),
            )),
            arrow_area,
        );
        return;
    }
    // The reference-log row renders its path GRAY until the TEST's capture
    // probe PASSED for this row (requested): the path is a plan until
    // the ✓ lands, then it renders in the normal style. The flip follows
    // the probe mark — any edit clears the marks, so the row re-dims
    // until TEST passes again.
    let pending_reference = matches!(key, FieldKey::DutAdvanced(_, _, "monitor.reference_log"))
        && !buffer.trim().is_empty()
        && state.probes.get(key).copied() != Some(ProbeMark::Ok);
    let (cursor, mut view_offset) = state.editor_entry(key);
    let mut trailing: Vec<Span<'static>> = Vec::new();
    if let Some(e) = err {
        if (vrect.width as usize) >= buffer.chars().count() + 2 + e.chars().count() {
            trailing.push(Span::styled(
                format!("  {e}"),
                Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
            ));
        }
    } else if buffer.is_empty() && with_hint {
        let field = state.fields.iter().find(|f| &f.key == key);
        if let Some(default) = field.and_then(|f| f.default.clone())
            && !default.is_empty()
        {
            trailing.push(Span::styled(
                format!(" [{}]", fit_label(&default, 24)),
                Style::new().fg(Color::DarkGray),
            ));
        }
    }
    let value_style = if err.is_some() {
        Style::new().fg(Color::Red)
    } else if pending_reference {
        Style::new().fg(OVERLAY0)
    } else {
        Style::new()
    };
    render_editor(
        f,
        vrect,
        &buffer,
        cursor,
        &mut view_offset,
        focused,
        trailing,
        value_style,
    );
}

/// The error sub-line below a row whose message does not fit beside the
/// value (`in_host`: the row's owning container).
fn render_error_subline(
    f: &mut Frame<'_>,
    state: &FormState,
    map: &LayoutMap,
    row: &RowSpec,
    in_host: bool,
) {
    let Some(sr) = &row.sub_rect else {
        return;
    };
    let Some(e) = row.key.as_ref().and_then(|k| state.errors.get(k)) else {
        return;
    };
    let Some(srect) = row_shift(state, map, *sr, in_host) else {
        return;
    };
    f.render_widget(
        Paragraph::new(Span::styled(
            e.clone(),
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        )),
        srect,
    );
}

fn render_auth_probe_subline(
    f: &mut Frame<'_>,
    state: &FormState,
    map: &LayoutMap,
    row: &RowSpec,
    in_host: bool,
) {
    let Some(sr) = &row.sub_rect else {
        return;
    };
    let Some((key, detail)) = row
        .key
        .as_ref()
        .and_then(|key| visible_auth_probe_detail(state, key).map(|detail| (key, detail)))
    else {
        return;
    };
    if state.errors.contains_key(key) {
        return;
    }
    let Some(srect) = row_shift(state, map, *sr, in_host) else {
        return;
    };
    let style = state
        .probes
        .get(key)
        .map(|mark| Style::new().fg(mark.color()))
        .unwrap_or_else(|| Style::new().fg(OVERLAY1));
    f.render_widget(Paragraph::new(Span::styled(detail, style)), srect);
}

/// The footer key hints: one line of chips, groups separated by │.
/// The ONE divider line between host and DUT:
/// a full-width "─" rule carrying the "DUTs" title; ACCENT-bold while the
/// drag is active. Draggable — handle_mouse hit-tests it FIRST.
fn render_divider(f: &mut Frame<'_>, state: &FormState, map: &LayoutMap) {
    let style = if state.divider_drag {
        Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(OVERLAY0)
    };
    let line: String = "─".repeat(map.divider.width as usize);
    f.render_widget(Paragraph::new(Span::styled(&line, style)), map.divider);
    f.render_widget(
        Paragraph::new(Span::styled(
            "DUTs",
            Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
        ))
        .alignment(Alignment::Left),
        map.divider,
    );
}

/// The per-container scrollbar thumbs: a 1-cell OVERLAY1 bar
/// on each container's right border column, only when it overflows.
fn render_scrollbars(f: &mut Frame<'_>, map: &LayoutMap) {
    for thumb in [map.host_scrollbar, map.dut_scrollbar]
        .into_iter()
        .flatten()
    {
        f.buffer_mut().set_style(thumb, Style::new().bg(OVERLAY1));
    }
}

fn render_footer_hints(f: &mut Frame<'_>, state: &FormState, map: &LayoutMap) {
    // [Test] [Save] [Save & Exit] [Exit] sit at the footer's right edge
    // (save no longer exits; Exit quits WITHOUT saving).
    for (btn, label) in [
        (ButtonId::Test, "[Test]"),
        (ButtonId::Save, "[Save]"),
        (ButtonId::SaveExit, "[Save & Exit]"),
        (ButtonId::Exit, "[Exit]"),
    ] {
        if let Some((_, r)) = map.buttons.iter().find(|(b, _)| *b == btn) {
            let focused = state.focus == FocusSlot::Button(btn);
            let style = if focused {
                Style::new()
                    .fg(TEXT)
                    .bg(SURFACE0)
                    .add_modifier(Modifier::REVERSED)
            } else {
                Style::new().fg(TEXT).bg(SURFACE0)
            };
            f.render_widget(Paragraph::new(Span::styled(label, style)), *r);
        }
    }
}

/// Single-line editor: the visible window of the buffer with the cursor
/// cell reversed (only when focused), plus trailing spans (dim default
/// hint, select marker, inline error). The window scrolls so the cursor is
/// always visible — normalization lives HERE (the render knows the width).
///
/// INPUT AFFORDANCE: the WHOLE rendered rect is painted with the field
/// background first (the painted rect IS the mouse hit-target — both come
/// from the one `form_layout`), the value text sits on an underline
/// baseline, and the FOCUSED field brightens the background, bolds the
/// text and shows the reversed cursor cell — the active input is
/// unmistakable; unfocused fields keep the quiet input-box look.
#[allow(clippy::too_many_arguments)]
fn render_editor(
    f: &mut Frame<'_>,
    rect: Rect,
    buffer: &str,
    cursor: usize,
    view_offset: &mut usize,
    focused: bool,
    trailing: Vec<Span<'static>>,
    style: Style,
) {
    let bg = if focused { FOCUSED_FIELD_BG } else { FIELD_BG };
    f.buffer_mut()
        .set_style(rect, Style::new().bg(bg).add_modifier(Modifier::UNDERLINED));
    let width = (rect.width as usize).max(1);
    let chars: Vec<char> = buffer.chars().collect();
    let len = chars.len();
    let cur = cursor.min(len);
    // Normalize the window so the cursor stays visible.
    if cur < *view_offset {
        *view_offset = cur;
    }
    if cur >= *view_offset + width {
        *view_offset = cur + 1 - width;
    }
    if *view_offset > len.saturating_sub(1) {
        *view_offset = len.saturating_sub(1);
    }
    let window: String = chars[*view_offset..].iter().take(width).collect();
    let cur_in_window = (cur - *view_offset).min(window.chars().count());
    let mut text_style = style;
    if focused {
        text_style = text_style.add_modifier(Modifier::BOLD);
    }
    let mut line = Line::default();
    line.push_span(Span::styled(
        window.chars().take(cur_in_window).collect::<String>(),
        text_style,
    ));
    if focused {
        line.push_span(Span::styled(
            window.chars().nth(cur_in_window).unwrap_or(' ').to_string(),
            text_style.add_modifier(Modifier::REVERSED),
        ));
    } else if cur_in_window < window.chars().count() {
        line.push_span(Span::styled(
            window.chars().nth(cur_in_window).unwrap_or(' ').to_string(),
            text_style,
        ));
    }
    line.push_span(Span::styled(
        window.chars().skip(cur_in_window + 1).collect::<String>(),
        text_style,
    ));
    for span in trailing {
        line.push_span(span);
    }
    f.render_widget(Paragraph::new(line), rect);
}
// ── Terminal lifecycle ─────────────────────────────────────────────────

/// The minimum terminal size for the TUI (guards the known ratatui
/// `Terminal::new` 0x0 panic; smaller terminals fall back to plain
/// prompts). 40x20 is the real floor: below 20 rows the four-zone chunk
/// layout leaves the host panel and/or the DUT content box with 0
/// interior rows — an empty form the user can type into but never see —
/// so those terminals get the fully-usable plain prompt flow instead.
pub fn terminal_usable(size: (u16, u16)) -> bool {
    size.0 >= 40 && size.1 >= 20
}

/// Pure dispatch decision: TUI needs BOTH stdin and stdout to be
/// terminals, TERM not "dumb" (case-insensitive; TERM unset or any other
/// value is fine), and a usable size.
pub fn tui_allowed(stdin_tty: bool, stdout_tty: bool, term: &str, size: (u16, u16)) -> bool {
    stdin_tty && stdout_tty && !term.eq_ignore_ascii_case("dumb") && terminal_usable(size)
}

/// Terminal size probe for the dispatch gate ((0, 0) on error — rejected
/// by `terminal_usable`).
pub fn probe_size() -> (u16, u16) {
    crossterm::terminal::size().unwrap_or((0, 0))
}

/// Panic safety: restore raw mode + alt screen + mouse capture, then chain
/// the previous hook (installed once per process).
fn install_panic_hook() {
    use std::sync::OnceLock;
    static HOOK: OnceLock<()> = OnceLock::new();
    HOOK.get_or_init(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = execute!(
                io::stdout(),
                DisableMouseCapture,
                crossterm::event::DisableBracketedPaste,
                crossterm::event::DisableFocusChange
            );
            let _ = execute!(io::stdout(), LeaveAlternateScreen);
            let _ = disable_raw_mode();
            discard_pending_input();
            prev(info);
        }));
    });
}

/// RAII terminal guard: `new` enables raw mode + the
/// alternate screen + mouse capture and hides the cursor; `Drop` restores
/// ALL of it on every exit path (Esc / q / `?` / error / panic), so the
/// shell can never be left in a TUI state. Hold the guard for the whole
/// `run_form` call.
pub struct TerminalGuard;

impl TerminalGuard {
    /// Enable the TUI terminal state. Returns None (restoring whatever was
    /// enabled) when the terminal is unusable or any step fails — the
    /// binary falls back to plain prompts.
    pub fn new() -> Option<Self> {
        let size = crossterm::terminal::size().unwrap_or((0, 0));
        if !terminal_usable(size) {
            return None;
        }
        if enable_raw_mode().is_err() {
            return None;
        }
        if execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableMouseCapture,
            crossterm::event::EnableBracketedPaste,
            crossterm::event::EnableFocusChange,
            crossterm::cursor::Hide,
        )
        .is_err()
        {
            teardown_terminal();
            return None;
        }
        install_panic_hook();
        Some(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Restore on EVERY exit path — a forgotten cleanup can never leak
        // raw mode / the alternate screen / mouse capture into the shell.
        teardown_terminal();
    }
}

/// Map retired relay backend names onto their surviving equivalents when
/// seeding the wizard from an existing config. The wizard exists to repair
/// configs — round-tripping a value that config validation rejects at
/// server start (`usb-relay` is no longer a backend) would make `dutabo
/// init` unable to fix the very projects it runs on. `usb-relay` spoke the
/// exact same CH340 wire protocol; unknown names pass through untouched and
/// keep failing loudly in validation.
fn normalize_relay_type(relay_type: &str) -> String {
    if relay_type.trim().eq_ignore_ascii_case("usb-relay") {
        "ch340-relay".to_string()
    } else {
        relay_type.to_string()
    }
}

/// Panic-only cleanup for unread input. Normal exits must not flush stdin:
/// after raw mode is disabled the shell owns new bytes, and flushing at
/// that boundary can consume only the ESC byte of a following arrow-key
/// sequence, leaving a literal `[A` at the prompt.
fn discard_pending_input() {
    unsafe {
        libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH);
    }
}

/// Best-effort terminal restore (symmetric with `TerminalGuard::new`) —
/// shared by the guard's `Drop`, the panic hook and the tests. Every step
/// runs even if an earlier one fails. Input is deliberately not flushed:
/// the Esc event was already consumed by crossterm, and newly arriving
/// bytes belong to the restored shell.
pub fn teardown_terminal() {
    let _ = execute!(
        io::stdout(),
        DisableMouseCapture,
        crossterm::event::DisableBracketedPaste,
        crossterm::event::DisableFocusChange
    );
    let _ = execute!(io::stdout(), LeaveAlternateScreen);
    let _ = execute!(io::stdout(), crossterm::cursor::Show);
    let _ = disable_raw_mode();
}

/// Event source abstraction so the full form flow is headless-testable.
pub trait EventSource {
    fn next_event(&mut self) -> io::Result<Event>;
    /// Non-blocking poll used while a TEST run is pending ;
    /// the default falls back to the blocking read.
    fn poll_event(&mut self, _timeout: std::time::Duration) -> io::Result<Option<Event>> {
        Ok(Some(self.next_event()?))
    }
}

/// Real event source: crossterm's blocking event read.
pub struct CrosstermEventSource;

impl EventSource for CrosstermEventSource {
    fn next_event(&mut self) -> io::Result<Event> {
        crossterm::event::read()
    }
    fn poll_event(&mut self, timeout: std::time::Duration) -> io::Result<Option<Event>> {
        if crossterm::event::poll(timeout)? {
            Ok(Some(crossterm::event::read()?))
        } else {
            Ok(None)
        }
    }
}

// ── MCP lifecycle for the TEST button ─────────────────────
// The DUT tests run through the MCP server: init starts one project HTTP MCP
// before probing and closes it as soon as that TEST run finishes
// (a server that was already running — the session hook's — is never touched).

/// A sermcp HTTP server THIS init run spawned for the TEST
/// button. Killed + reaped when the run finishes (and by Drop on early exit);
/// pre-existing servers are never touched.
struct SpawnedMcp {
    child: std::process::Child,
    project_dir: std::path::PathBuf,
    dut_name: String,
    /// The TEMP config this child was spawned with (stale-detection on
    /// reuse: a warm child must match the CURRENT form, or TEST would
    /// probe an edited-away configuration).
    config: std::path::PathBuf,
}

struct InventorySnapshot {
    project_dir: std::path::PathBuf,
    content: Option<Vec<u8>>,
}

pub struct McpHandle {
    children: Vec<SpawnedMcp>,
    inventory_snapshots: Vec<InventorySnapshot>,
    test_configs: Vec<std::path::PathBuf>,
    blocked_ports: std::collections::BTreeSet<u16>,
    borrowed_pids: std::collections::BTreeSet<u32>,
}

impl McpHandle {
    fn new() -> Self {
        Self {
            children: Vec::new(),
            inventory_snapshots: Vec::new(),
            test_configs: Vec::new(),
            blocked_ports: std::collections::BTreeSet::new(),
            borrowed_pids: std::collections::BTreeSet::new(),
        }
    }

    fn owned_pids(&self) -> std::collections::BTreeSet<u32> {
        self.children
            .iter()
            .map(|spawned| spawned.child.id())
            .chain(self.borrowed_pids.iter().copied())
            .collect()
    }

    /// Write the per-run TEST config. It sits in the PROJECT directory
    /// (beside `.target.jsonc`), never under `.dut-serial/`: the spawned
    /// server derives `project_dir` from the config's PARENT, so a config
    /// under `.dut-serial/` makes it treat that directory as the project —
    /// it then keeps a nested `.dut-serial/.dut-serial/` runtime tree with
    /// its OWN pid singleton (letting a second server run beside the real
    /// one), logs and audit trail under the wrong root. Removed by
    /// `stop_spawned` (and on drop).
    fn write_test_config(
        &mut self,
        project_dir: &Path,
        text: &str,
    ) -> Result<std::path::PathBuf, String> {
        use std::io::Write;
        std::fs::create_dir_all(project_dir)
            .map_err(|e| format!("cannot create {}: {e}", project_dir.display()))?;
        for nonce in 0..1000u16 {
            let path = project_dir.join(format!(
                ".target.init-test-{}-{nonce}.jsonc",
                std::process::id()
            ));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => {
                    file.write_all(text.as_bytes())
                        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
                    self.test_configs.push(path.clone());
                    return Ok(path);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(format!("cannot create {}: {e}", path.display())),
            }
        }
        Err("cannot allocate a temporary TEST config".into())
    }

    /// Spawn the project HTTP MCP server when none is already running.
    /// Best-effort: init keeps working when the binary or config is
    /// missing — the MCP probes simply fail/skip. Returns true when THIS
    /// call spawned a server (callers wait for it to come up).
    fn ensure(&mut self, project_dir: &Path, dut_name: &str, port: u16, config: &Path) -> bool {
        // Relay and flash fields can reach this path before the child writes
        // its PID file. The project path is the singleton key regardless of
        // which DUT triggered the probe. A warm OWN child is reused ONLY
        // while it still serves the CURRENT configuration — a kept-alive
        // quick-action child whose form has since been edited is stale and
        // must be reaped before respawning (TEST independence from edits).
        if let Some(index) = self.children.iter_mut().position(|spawned| {
            spawned.project_dir == project_dir && !matches!(spawned.child.try_wait(), Ok(Some(_)))
        }) {
            let fresh = self.children[index].dut_name == dut_name
                && effective_config_matches(&self.children[index].config, config, dut_name);
            if fresh {
                return false;
            }
            self.stop_child(index);
        }
        if mcp_alive_for_project(project_dir) {
            if let Some(pid) = reusable_project_mcp(project_dir, dut_name, port, config) {
                self.borrowed_pids.insert(pid);
            } else {
                self.blocked_ports.insert(port);
            }
            return false;
        }
        let Some(binary) = mcp_binary() else {
            return false;
        };
        if !config.is_file() {
            return false;
        }
        if !self
            .inventory_snapshots
            .iter()
            .any(|snapshot| snapshot.project_dir == project_dir)
        {
            self.inventory_snapshots.push(InventorySnapshot {
                project_dir: project_dir.to_path_buf(),
                content: std::fs::read(project_dir.join(".dut-serial/inventory.json")).ok(),
            });
        }
        let mut cmd = std::process::Command::new(binary);
        cmd.arg("--http")
            .arg(format!("127.0.0.1:{port}"))
            .current_dir(project_dir)
            .env("TARGET_CONF", config)
            .env("TARGET_DUT_NAME", dut_name)
            // A TEST child must become controllable quickly from any initial
            // DUT state. Loader/MASKROM cannot answer the startup prompt; the
            // following normalization reset is the operation that recovers it.
            .env("SERMCP_STARTUP_PROBE_TIMEOUT_MS", "1500");
        // Forward the Perfetto trace env under a derived sibling path so the
        // parent (client) and child (server) traces never clobber each other.
        if let Some(trace) = std::env::var_os("SERMCP_TRACE").filter(|v| !v.is_empty()) {
            cmd.env(
                "SERMCP_TRACE",
                sermcp::trace_perfetto::child_trace_path(&trace),
            );
        }
        let Ok(child) = cmd
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        else {
            return false;
        };
        self.children.push(SpawnedMcp {
            child,
            project_dir: project_dir.to_path_buf(),
            dut_name: dut_name.to_string(),
            config: config.to_path_buf(),
        });
        true
    }

    /// Kill ONE owned child (pid residue cleanup included). Returns the
    /// reaped child.
    fn stop_child(&mut self, index: usize) -> SpawnedMcp {
        let mut spawned = self.children.remove(index);
        let pid = spawned.child.id();
        let _ = spawned.child.kill();
        let _ = spawned.child.wait();
        let dut_root = spawned.project_dir.join(".dut-serial");
        for pid_file in [
            dut_root.join("mcp.pid"),
            dut_root.join(&spawned.dut_name).join("mcp.pid"),
        ] {
            if std::fs::read_to_string(&pid_file)
                .ok()
                .and_then(|text| text.trim().parse::<u32>().ok())
                == Some(pid)
            {
                let _ = std::fs::remove_file(pid_file);
            }
        }
        sermcp::lock_manager::cleanup_dead_process_locks(
            pid,
            &sermcp::lock_manager::default_lock_dir(),
        );
        spawned
    }

    /// Stop only servers spawned by this TEST run and remove only residue
    /// carrying those exact, now-dead PIDs. Pre-existing servers, locks and
    /// inventory are never touched. Called as soon as the run completes, so
    /// keeping the init form open cannot affect another Agent.
    fn stop_spawned(&mut self) {
        let mut stopped_pids = std::collections::BTreeSet::new();
        for mut spawned in self.children.drain(..) {
            let pid = spawned.child.id();
            let _ = spawned.child.kill();
            let _ = spawned.child.wait();
            stopped_pids.insert(pid);

            let dut_root = spawned.project_dir.join(".dut-serial");
            for pid_file in [
                dut_root.join("mcp.pid"),
                dut_root.join(&spawned.dut_name).join("mcp.pid"),
            ] {
                if std::fs::read_to_string(&pid_file)
                    .ok()
                    .and_then(|text| text.trim().parse::<u32>().ok())
                    == Some(pid)
                {
                    let _ = std::fs::remove_file(pid_file);
                }
            }
            sermcp::lock_manager::cleanup_dead_process_locks(
                pid,
                &sermcp::lock_manager::default_lock_dir(),
            );
        }

        for snapshot in self.inventory_snapshots.drain(..) {
            let path = snapshot.project_dir.join(".dut-serial/inventory.json");
            let current_pid = std::fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                .and_then(|value| value.get("mcp_pid").and_then(|pid| pid.as_u64()))
                .and_then(|pid| u32::try_from(pid).ok());
            // A concurrently started project MCP is authoritative. Restore
            // only when the current inventory is absent or belongs to one of
            // the temporary children just reaped.
            if current_pid.is_some_and(|pid| !stopped_pids.contains(&pid)) {
                continue;
            }
            match snapshot.content {
                Some(content) => {
                    let tmp = path.with_file_name(format!(
                        ".inventory.json.init-restore-{}",
                        std::process::id()
                    ));
                    if std::fs::write(&tmp, content).is_ok() {
                        let _ = std::fs::rename(tmp, path);
                    }
                }
                None => {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
        for path in self.test_configs.drain(..) {
            let _ = std::fs::remove_file(path);
        }
        self.blocked_ports.clear();
        self.borrowed_pids.clear();
    }
}

impl Drop for McpHandle {
    fn drop(&mut self) {
        self.stop_spawned();
    }
}

/// Locate the sermcp binary: the sibling of the current
/// executable (dutabo and the MCP server install together), then PATH.
fn mcp_binary() -> Option<std::path::PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        let sibling = exe.parent().map(|p| p.join("sermcp"))?;
        if sibling.is_file() {
            return Some(sibling);
        }
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("sermcp");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Reuse only the project's advertised server and the same effective DUT config.
/// Borrowing never transfers process ownership to the TEST cleanup path.
fn reusable_project_mcp(
    project_dir: &Path,
    dut_name: &str,
    port: u16,
    config: &Path,
) -> Option<u32> {
    let inventory: serde_json::Value = serde_json::from_slice(
        &std::fs::read(project_dir.join(".dut-serial/inventory.json")).ok()?,
    )
    .ok()?;
    let pid: u32 = inventory.get("mcp_pid")?.as_u64()?.try_into().ok()?;
    let pid_text = std::fs::read_to_string(project_dir.join(".dut-serial/mcp.pid")).ok()?;
    if pid_text.trim().parse::<u32>().ok()? != pid
        || inventory.get("mcp_http_port")?.as_u64()? != u64::from(port)
        || Path::new(inventory.get("project_dir")?.as_str()?) != project_dir
    {
        return None;
    }
    // Probes currently use the server's default DUT. Do not send another
    // board's test to that default in a multi-DUT project.
    let duts = inventory.get("duts")?.as_array()?;
    if duts.len() != 1 || duts[0].get("dut_name")?.as_str()? != dut_name {
        return None;
    }
    let saved = Path::new(inventory.get("config_path")?.as_str()?);
    if !effective_config_matches(saved, config, dut_name) {
        return None;
    }
    Some(pid)
}

/// Do two config files produce the SAME effective flat map for one DUT?
/// Shared by the warm-child reuse check and the borrowed-server check —
/// both must never serve a configuration the form has edited away.
fn effective_config_matches(a: &Path, b: &Path, dut_name: &str) -> bool {
    let effective = |path: &Path| -> Option<std::collections::HashMap<String, String>> {
        let mut global = sermcp::config::defaults();
        global.extend(sermcp::config::parse_jsonc_config(path).ok()?);
        let dut = sermcp::config::parse_dut_configs(path)
            .ok()?
            .into_iter()
            .find(|dut| dut.dut_name == dut_name)?;
        Some(dut.to_config_map(&global))
    };
    match (effective(a), effective(b)) {
        (Some(map_a), Some(map_b)) => map_a == map_b,
        // An unparseable side means "cannot prove equality" — refuse reuse.
        _ => false,
    }
}

/// Is a live MCP server already serving this project?
fn mcp_alive_for_project(project_dir: &Path) -> bool {
    let pid_file = project_dir.join(".dut-serial/mcp.pid");
    let Ok(pid_text) = std::fs::read_to_string(&pid_file) else {
        return false;
    };
    let Ok(pid) = pid_text.trim().parse::<u32>() else {
        return false;
    };
    sermcp::lock_manager::is_live_server_holder(pid)
}

/// Wait (bounded) for the MCP HTTP endpoint to accept connections — the
/// server binds before its 35s engine-connect finishes in the background.
fn health_response_is_ready(response: &[u8]) -> bool {
    response
        .windows(br#""status":"ok""#.len())
        .any(|window| window == br#""status":"ok""#)
}

#[tracing::instrument(fields(port))]
fn wait_for_mcp_ready(port: u16, timeout: std::time::Duration) -> bool {
    use std::io::{Read, Write};
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if let Ok(mut stream) = std::net::TcpStream::connect(("127.0.0.1", port)) {
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(1)));
            let request = format!(
                "GET /health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
            );
            if stream.write_all(request.as_bytes()).is_ok() {
                let mut response = Vec::new();
                if stream.read_to_end(&mut response).is_ok() && health_response_is_ready(&response)
                {
                    return true;
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    false
}

// ── FormTui: the event loop + the PromptIO bridge ──────────────────────

/// The form transport. Generic over Backend + EventSource for headless
/// tests. Implements `init::PromptIO`: `input`/`confirm` are unreachable
/// (Err — the form transport asks no questions); `finish` is the
/// auto-write no-op (F2 already saved the form — `finish_init` writes
/// right after it returns; the Review screen is gone).
pub struct FormTui<B: Backend, E: EventSource> {
    pub terminal: Terminal<B>,
    pub events: E,
    pub state: FormState,
    /// (prepared, opts) — lets [Save] write WITHOUT exiting the loop.
    pub write_ctx: Option<(PreparedInit, InitOptions)>,
    /// MCP servers THIS init spawned for the TEST button — killed on exit.
    pub mcp: McpHandle,
}

impl<B: Backend, E: EventSource> FormTui<B, E> {
    pub fn new(terminal: Terminal<B>, events: E, state: FormState) -> Self {
        Self {
            write_ctx: None,
            mcp: McpHandle::new(),
            terminal,
            events,
            state,
        }
    }

    /// Draw one frame (draw errors are clean aborts, never unwraps).
    fn draw(&mut self) -> Result<(), String> {
        self.terminal
            .draw(|f| render_form(f, &mut self.state))
            .map(|_| ())
            .map_err(|e| format!("TUI draw error: {e}"))
    }

    fn next_event(&mut self) -> Result<Event, String> {
        self.events
            .next_event()
            .map_err(|e| format!("TUI event error: {e}"))
    }

    /// Resolve a form action: Continue → None (keep looping); Submit →
    /// authoritative `resolve_answers` (per-field errors land in
    /// `state.errors`, focus jumps to the first error); Abort → Err.
    fn apply_action(&mut self, action: FormAction) -> Result<Option<WizardAnswers>, String> {
        match action {
            FormAction::Continue => Ok(None),
            FormAction::Abort(msg) => Err(msg),
            // SaveOnly resolves exactly like Submit; the form_loop decides
            // whether to exit or write-and-continue. Test = dry-run of the
            // SAME resolution (errors surface, nothing written).
            FormAction::Submit | FormAction::SaveOnly | FormAction::Test => {
                match resolve_answers(
                    &self.state.schema,
                    &self.state.submit_values(),
                    self.state.gate,
                    NamePolicy::ErrorOnCollision,
                ) {
                    Ok(answers) => Ok(Some(answers)),
                    Err(errors) => {
                        let n = errors.len();
                        let errors: BTreeMap<FieldKey, String> = errors.into_iter().collect();
                        self.state.errors = errors;
                        self.state.note = Some((
                            NoteKind::Error,
                            format!(
                                "{n} error{} — fix highlighted fields",
                                if n == 1 { "" } else { "s" }
                            ),
                        ));
                        self.focus_first_error();
                        Ok(None)
                    }
                }
            }
        }
    }

    /// Focus the first erroring field in visual order (the renderer's
    /// keep-focus-visible scroll then brings it on screen; the erroring
    /// host/DUT becomes current). An erroring HOST-LEVEL advanced field
    /// must become VISIBLE first — its layout exists only while
    /// `[Other ▼]` is expanded (Forced mode can hold advanced errors while
    /// collapsed) — so the section expands before the focus lands. A
    /// DUT-owned advanced key's row lives in the DUT content box
    /// (scroll-reachable regardless of the block), so it must NOT expand
    /// the host panel.
    fn focus_first_error(&mut self) {
        for key in self.state.field_order() {
            if self.state.errors.contains_key(&key) {
                if matches!(key, FieldKey::Advanced(k) if !dut_owned_advanced(k))
                    && !self.state.other_expanded
                {
                    self.state.toggle_other();
                }
                self.state.set_focus(FocusSlot::Field(key));
                return;
            }
        }
    }

    /// Mouse events while the controller picker is open (ComboBox
    /// semantics): click the field cell toggles closed, a
    /// click on an option row picks it and closes, hover moves the browse
    /// cursor, any other click closes WITHOUT a choice. The popup rect
    /// comes from the SAME `picker_popup_rect` the renderer uses, so the
    /// hit-test can never drift from what is on screen.
    fn handle_picker_mouse(&mut self, m: MouseEvent) -> Result<Option<FormAction>, String> {
        let x = m.column;
        let y = m.row;
        let size = self
            .terminal
            .size()
            .map_err(|e| format!("TUI size error: {e}"))?;
        let area = Rect::new(0, 0, size.width, size.height);
        let map = form_layout(area, &self.state);
        let popup = picker_popup_rect(&self.state, &map, area);
        let picker_key = self.state.picker.as_ref().expect("picker open").key.clone();
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                // 1. Click the field cell → toggle closed (click-to-open
                //    is the normal field-row loop when the picker is
                //    closed).
                let field_rect =
                    map.fields
                        .iter()
                        .find(|(k, _)| *k == picker_key)
                        .and_then(|(_, r)| {
                            row_shift(&self.state, &map, *r, field_in_host(&picker_key))
                        });
                if field_rect.is_some_and(|r| rect_contains(r, x, y)) {
                    self.state.picker = None;
                    return Ok(None);
                }
                // 2. Click an option row (inside the popup's border) →
                //    pick it and close.
                if let Some(popup) = popup {
                    let inner = Block::default().borders(Borders::ALL).inner(popup);
                    if rect_contains(inner, x, y) {
                        let idx = y.saturating_sub(inner.y) as usize;
                        if let Some(p) = self.state.picker.as_ref()
                            && let Some(value) = p.options.get(idx).cloned()
                        {
                            let key_field = p.key.clone();
                            self.state.set_value(key_field, value);
                            self.state.picker = None;
                        }
                        return Ok(None);
                    }
                }
                // 3. Click outside the popup → close without a choice.
                self.state.picker = None;
                Ok(None)
            }
            MouseEventKind::Moved => {
                // Hover moves the BROWSE cursor (never the committed
                // value — selected stays until Enter/click confirms).
                if let Some(popup) = popup {
                    let inner = Block::default().borders(Borders::ALL).inner(popup);
                    if rect_contains(inner, x, y) {
                        let idx = y.saturating_sub(inner.y) as usize;
                        if let Some(p) = self.state.picker.as_mut()
                            && idx < p.options.len()
                        {
                            p.cursor = idx;
                        }
                    }
                }
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    /// Mouse event → focus/click action (pure layout hit-testing through
    /// `form_layout`). The tab bars, panel headers and the footer never
    /// scroll (absolute targets); field rows hit-test against the SAME
    /// scroll-shifted rect the renderer paints.
    fn handle_mouse(&mut self, m: MouseEvent) -> Result<Option<FormAction>, String> {
        if self.state.skill_picker.is_some() {
            self.skill_mouse(m)?;
            return Ok(None);
        }
        // The open controller picker owns mouse events FIRST: clicks
        // inside pick, hover moves the cursor, clicks outside close —
        // visual z-order == event z-order.
        if self.state.picker.is_some() {
            return self.handle_picker_mouse(m);
        }
        if self.relay_mouse(m)? {
            return Ok(None);
        }
        if self.pane_mouse(m)? {
            return Ok(None);
        }
        // The divider row is hit-tested FIRST: a Down starts the drag,
        // Drag updates the pin, Up ends it.
        match m.kind {
            MouseEventKind::Drag(MouseButton::Left) => {
                if self.state.pane_drag {
                    let area = self
                        .terminal
                        .size()
                        .map_err(|e| format!("TUI size error: {e}"))?;
                    let map = form_layout(Rect::new(0, 0, area.width, area.height), &self.state);
                    // The pane bottom stays pinned at the footer; the
                    // drag sets its height (borders included), clamped
                    // so the DUT panel keeps its minimum.
                    let max = map
                        .footer
                        .y
                        .saturating_sub(map.divider.bottom())
                        .saturating_sub(DUT_MIN_ABS)
                        .max(MIN_PANE_HEIGHT);
                    let pin = map
                        .footer
                        .y
                        .saturating_sub(m.row)
                        .clamp(MIN_PANE_HEIGHT, max);
                    self.state.pane_h_pin = Some(pin);
                    return Ok(None);
                }
                if self.state.divider_drag {
                    let area = self
                        .terminal
                        .size()
                        .map_err(|e| format!("TUI size error: {e}"))?;
                    let map = form_layout(Rect::new(0, 0, area.width, area.height), &self.state);
                    let body = map.dut_panel.bottom().saturating_sub(map.host_panel.y);
                    let pin = m
                        .row
                        .saturating_sub(map.host_panel.y)
                        .clamp(2, body.saturating_sub(1).saturating_sub(DUT_MIN_ABS));
                    self.state.host_h_pin = Some(pin);
                    return Ok(None);
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.state.divider_drag = false;
                self.state.pane_drag = false;
                if let Some(pending) = self.state.pending_exit_click.take() {
                    let size = self
                        .terminal
                        .size()
                        .map_err(|e| format!("TUI size error: {e}"))?;
                    let map = form_layout(Rect::new(0, 0, size.width, size.height), &self.state);
                    let released_on_button = map.buttons.iter().any(|(button, rect)| {
                        *button == pending && rect.contains((m.column, m.row).into())
                    });
                    return Ok(released_on_button.then(|| match pending {
                        ButtonId::SaveExit => FormAction::Submit,
                        ButtonId::Exit => FormAction::Abort("aborted by user".into()),
                        other => {
                            self.state.pending_exit_click = Some(other);
                            FormAction::Continue
                        }
                    }));
                }
                return Ok(None);
            }
            MouseEventKind::ScrollUp => {
                self.state.scroll = self.state.scroll.saturating_sub(3);
                return Ok(None);
            }
            MouseEventKind::ScrollDown => {
                self.state.scroll = self.state.scroll.saturating_add(3);
                return Ok(None);
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let pos = (m.column, m.row);
                let size = self
                    .terminal
                    .size()
                    .map_err(|e| format!("TUI size error: {e}"))?;
                let map = form_layout(Rect::new(0, 0, size.width, size.height), &self.state);
                if map.divider.contains(pos.into()) {
                    self.state.divider_drag = true;
                    return Ok(None);
                }
            }
            _ => return Ok(None),
        }
        let size = self
            .terminal
            .size()
            .map_err(|e| format!("TUI size error: {e}"))?;
        let area = Rect::new(0, 0, size.width, size.height);
        let map = form_layout(area, &self.state);
        let pos = (m.column, m.row);
        // Buttons first: a tab's `x`, the trailing `+` cards, `[Other ▼]`.
        for (btn, r) in &map.buttons {
            if r.contains(pos.into()) {
                if matches!(btn, ButtonId::SaveExit | ButtonId::Exit) {
                    self.state.pending_exit_click = Some(*btn);
                    self.state.set_focus(FocusSlot::Button(*btn));
                    return Ok(None);
                }
                return Ok(Some(match btn {
                    ButtonId::Skill(chip) => {
                        self.state.open_skill_picker(*chip);
                        FormAction::Continue
                    }
                    ButtonId::AddHost => {
                        self.state.add_host();
                        FormAction::Continue
                    }
                    ButtonId::AddDut(hi) => {
                        self.state.add_dut(*hi);
                        FormAction::Continue
                    }
                    ButtonId::RemoveHost(hi) => {
                        self.state.remove_host(*hi);
                        FormAction::Continue
                    }
                    ButtonId::RemoveDut(hi, di) => {
                        self.state.remove_dut(*hi, *di);
                        FormAction::Continue
                    }
                    ButtonId::Save => FormAction::SaveOnly,
                    ButtonId::SaveExit | ButtonId::Exit => FormAction::Continue,
                    ButtonId::Test => FormAction::Test,
                    ButtonId::ToggleOther => {
                        self.state.toggle_other();
                        FormAction::Continue
                    }
                }));
            }
        }
        // Tab cards: clicking a tab SELECTS it (both panels update).
        for (hi, r) in &map.host_tabs {
            if r.contains(pos.into()) {
                self.state.set_focus(FocusSlot::HostTab(*hi));
                return Ok(None);
            }
        }
        for (di, r) in &map.dut_tabs {
            if r.contains(pos.into()) {
                self.state.set_focus(FocusSlot::DutTab(*di));
                return Ok(None);
            }
        }
        // Field rows: focus + cursor (Select rows cycle the next option
        // instead of placing a cursor). The owning container follows the
        // field's key (an overflowed HOST-LEVEL advanced field belongs to
        // the HOST panel even though it sits below it; the DUT-owned
        // advanced keys belong to the DUT content box).
        for (key, r) in &map.fields {
            // The read-only flash device-list row is display-only: clicks
            // pass through (never focus an unfocusable rect).
            if flash::is_devices_key(key) {
                continue;
            }
            let in_host = field_in_host(key);
            if let Some(sr) = row_shift(&self.state, &map, *r, in_host)
                && sr.contains(pos.into())
            {
                self.state.set_focus(FocusSlot::Field(key.clone()));
                if self
                    .state
                    .fields
                    .iter()
                    .any(|f| &f.key == key && f.kind == FieldKind::Select)
                {
                    // Clicking a Select row opens the controller picker
                    // modal (— click-to-cycle overshot easily).
                    self.state.open_picker(key);
                } else {
                    self.click_cursor(key.clone(), m.column, *r);
                }
                return Ok(None);
            }
        }
        Ok(None)
    }

    /// Place the cursor nearest the click x within the value rect.
    fn click_cursor(&mut self, key: FieldKey, col: u16, rect: Rect) {
        let (_, view) = self.state.editor_entry(&key);
        let off = (col.saturating_sub(rect.x) as usize).saturating_add(view);
        let len = self
            .state
            .values
            .get(&key)
            .map(|s| s.chars().count())
            .unwrap_or(0);
        self.state.editors.insert(key, (off.min(len), view));
    }

    /// The form loop: draw + events until a clean F2 save (or abort).
    /// While a TEST worker is pending the same 50 ms busy-poll keeps the
    /// UI responsive (deadline 2.5 s).
    pub fn form_loop(&mut self) -> Result<WizardAnswers, String> {
        loop {
            if let Some((host, port)) = &self.state.pane.target {
                let hi = self.state.selected_host;
                let changed = self.state.selected_dut.is_none_or(|di| {
                    self.state.values.get(&FieldKey::HostIp(hi)) != Some(host)
                        || self.state.values.get(&FieldKey::DutPort(hi, di)) != Some(port)
                });
                if changed {
                    self.state.pane = pane::Pane::default();
                }
            }
            self.state.pane.poll();
            self.draw()?;
            let event = if self.state.busy() || self.state.pane.connected() {
                match self
                    .events
                    .poll_event(std::time::Duration::from_millis(50))
                    .map_err(|e| format!("TUI event error: {e}"))?
                {
                    Some(ev) => {
                        self.poll_test_run();
                        Some(ev)
                    }
                    None => {
                        self.poll_test_run();
                        continue;
                    }
                }
            } else {
                Some(self.next_event()?)
            };
            let Some(event) = event else {
                continue;
            };
            let action = match event {
                Event::Key(key) if self.state.pane.focused => {
                    self.state.pane.key(key);
                    FormAction::Continue
                }
                Event::Paste(text) if self.state.pane.focused => {
                    self.state
                        .pane
                        .send(self.state.pane.core.encode_paste(&text));
                    FormAction::Continue
                }
                Event::FocusLost => {
                    self.state.pane.outer_focus = false;
                    FormAction::Continue
                }
                Event::FocusGained => {
                    self.state.pane.outer_focus = true;
                    FormAction::Continue
                }
                Event::Key(key) => handle_form_key(&mut self.state, key),
                Event::Mouse(m) => match self.handle_mouse(m)? {
                    Some(action) => action,
                    None => FormAction::Continue,
                },
                Event::Resize(..) => {
                    self.state.host_h_pin = None;
                    FormAction::Continue
                }
                _ => FormAction::Continue,
            };
            match action {
                FormAction::Submit => {
                    if let Some(answers) = self.apply_action(FormAction::Submit)? {
                        return Ok(answers);
                    }
                }
                FormAction::SaveOnly => {
                    if let Some(answers) = self.apply_action(FormAction::Submit)? {
                        // Write WITHOUT exiting; the loop keeps running.
                        if let Some((prepared, opts)) = self.write_ctx.clone() {
                            match self.finish_project(&opts, &prepared, &answers) {
                                Ok(outcome) => {
                                    self.state.note = Some((
                                        NoteKind::Info,
                                        format!("saved — {}", outcome.config_path.display()),
                                    ));
                                }
                                Err(e) => {
                                    self.state.note = Some((NoteKind::Error, e));
                                }
                            }
                        } else {
                            self.state.note = Some((NoteKind::Info, "saved".into()));
                        }
                    }
                }
                FormAction::Test => {
                    // Phase A: the dry-run — errors land + focus jumps,
                    // nothing is written. Only a CLEAN resolve probes.
                    if self.apply_action(FormAction::Submit)?.is_some() {
                        self.start_test_run();
                    }
                }
                FormAction::Abort(msg) => {
                    // Esc during a TEST run cancels the run only; the
                    // next Esc quits.
                    if self.state.test_run.is_some() {
                        self.state.test_run = None;
                        self.state.probes.clear();
                        // A cancelled run may already have pulsed relays or
                        // reset the DUT — refresh the holder snapshot.
                        self.state.refresh_endpoint_conflicts();
                    } else {
                        return Err(msg);
                    }
                }
                FormAction::Continue => {}
            }
        }
    }

    /// Spawn the bounded probe worker (TEST button). The
    /// probe set mirrors the entered info: host ssh reachability, DUT
    /// serial/relay ports, device paths. Anything unsupported → Skip.
    fn start_test_run(&mut self) {
        // Clear the previous footer before arming a new run. Otherwise two
        // consecutive identical outcomes produce no terminal repaint, which
        // makes the second run look stuck to both users and TUI automation.
        self.state.test_summary = None;
        // Surface the reference path as REAL text before the run (user
        // report) — a config loaded with the relay already
        // configured never triggers the set_value autofill otherwise.
        for hi in 0..self.state.host_count {
            for di in 0..self.state.dut_counts.get(hi).copied().unwrap_or(0) {
                let trigger = if self
                    .state
                    .fields
                    .iter()
                    .any(|f| f.key == FieldKey::DutAdvanced(hi, di, "control.dev_ctrl"))
                {
                    FieldKey::DutAdvanced(hi, di, "control.dev_ctrl")
                } else {
                    FieldKey::DutRelayReset(hi, di)
                };
                self.state.autofill_reference_log(&trigger);
            }
        }
        let answers = match resolve_answers(
            &self.state.schema,
            &self.state.submit_values(),
            self.state.gate,
            NamePolicy::ErrorOnCollision,
        ) {
            Ok(answers) => answers,
            Err(errors) => {
                self.state.errors = errors.into_iter().collect();
                self.state.note = Some((
                    NoteKind::Error,
                    "TEST config became invalid after resolving defaults".into(),
                ));
                self.focus_first_error();
                return;
            }
        };
        let Some((prepared, _)) = self.write_ctx.as_ref() else {
            self.state.note = Some((
                NoteKind::Error,
                "TEST requires an initialized target context".into(),
            ));
            return;
        };
        let cwd = prepared
            .target_path
            .parent()
            .unwrap_or(Path::new("."))
            .to_path_buf();
        let test_text = match init::render_test_config(prepared, &answers) {
            Ok(text) => text,
            Err(error) => {
                self.state.note = Some((NoteKind::Error, error));
                return;
            }
        };
        let test_config = match self.mcp.write_test_config(&cwd, &test_text) {
            Ok(path) => path,
            Err(error) => {
                self.state.note = Some((NoteKind::Error, error));
                return;
            }
        };
        let state = &self.state;
        let mut probes: Vec<(FieldKey, ProbeTarget)> = Vec::new();
        let mut slow = false;
        // Ports of MCP servers THIS run spawned — wait for them below so the
        // first probe does not race a starting server.
        let mut mcp_wait_ports: Vec<u16> = Vec::new();
        for hi in 0..state.host_count {
            let ip = state
                .values
                .get(&FieldKey::HostIp(hi))
                .cloned()
                .unwrap_or_default();
            if ip.trim().is_empty() {
                continue;
            }
            let host = ip.trim().to_string();
            probes.push((
                FieldKey::HostIp(hi),
                ProbeTarget::SshBanner {
                    host: host.clone(),
                    port: 22,
                },
            ));
            // With a password, strictly authenticate the pair and mirror the
            // result onto both fields: an SSH rejection cannot identify which
            // member is wrong. Public-key auth is reported in the detail but
            // cannot hide a bad password. Without a password, test key auth.
            let user = state
                .values
                .get(&FieldKey::HostUser(hi))
                .cloned()
                .unwrap_or_default()
                .trim()
                .to_string();
            let pass = state
                .values
                .get(&FieldKey::HostPass(hi))
                .cloned()
                .unwrap_or_default();
            if !user.is_empty() {
                slow = true;
                if pass.is_empty() {
                    probes.push((
                        FieldKey::HostUser(hi),
                        ProbeTarget::SshLogin {
                            host: host.clone(),
                            user: user.clone(),
                            pass: pass.clone(),
                        },
                    ));
                } else {
                    probes.push((
                        FieldKey::HostPass(hi),
                        ProbeTarget::SshPasswordLogin {
                            host: host.clone(),
                            user: user.clone(),
                            pass: pass.clone(),
                            linked_user: FieldKey::HostUser(hi),
                        },
                    ));
                }
            }
            for di in 0..state.dut_counts.get(hi).copied().unwrap_or(0) {
                // EMPTY dev_ctrl counts as ENABLED (a filled relay port IS
                // the enable) — only the explicit none/disabled opt-out
                // disables. Computed up front: the baud probe must know
                // whether the server may press the reset relay.
                let dev_ctrl = state
                    .values
                    .get(&FieldKey::DutAdvanced(hi, di, "control.dev_ctrl"))
                    .cloned()
                    .unwrap_or_default();
                let relay_enabled = !matches!(
                    dev_ctrl.trim().to_ascii_lowercase().as_str(),
                    "none" | "disabled"
                );
                let port = state
                    .values
                    .get(&FieldKey::DutPort(hi, di))
                    .cloned()
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                if port.is_empty() {
                    continue;
                }
                if port.starts_with("/dev/") {
                    probes.push((
                        FieldKey::DutPort(hi, di),
                        ProbeTarget::CharDevice { path: port },
                    ));
                } else {
                    #[cfg_attr(not(feature = "direct-dut-probe"), allow(unused_variables))]
                    if let Ok(p) = port.parse::<u16>() {
                        // Direct serial-endpoint probes are DEBUG-ONLY
                        //: a direct TCP / RFC 2217 connection to
                        // the ser2net port EVICTS the MCP server's own
                        // connection (kickolduser) and breaks every
                        // MCP-based test that follows. Production validates
                        // the serial link through the MCP server instead.
                        #[cfg(feature = "direct-dut-probe")]
                        {
                            probes.push((
                                FieldKey::DutPort(hi, di),
                                ProbeTarget::TcpConnect {
                                    host: host.clone(),
                                    port: p,
                                },
                            ));
                            // Baudrate verification (rfc2217 endpoints only).
                            let baud = state
                                .values
                                .get(&FieldKey::DutAdvanced(hi, di, "serial.baudrate"))
                                .cloned()
                                .unwrap_or_default();
                            let baud = if baud.trim().is_empty() {
                                state
                                    .fields
                                    .iter()
                                    .find(|f| {
                                        f.key == FieldKey::DutAdvanced(hi, di, "serial.baudrate")
                                    })
                                    .and_then(|f| f.default.clone())
                                    .unwrap_or_else(|| "1500000".into())
                            } else {
                                baud
                            };
                            if let Ok(b) = baud.trim().parse::<u32>() {
                                let baud_key = if state.fields.iter().any(|f| {
                                    f.key == FieldKey::DutAdvanced(hi, di, "serial.baudrate")
                                }) {
                                    FieldKey::DutAdvanced(hi, di, "serial.baudrate")
                                } else {
                                    FieldKey::DutPort(hi, di)
                                };
                                probes.push((
                                    baud_key,
                                    ProbeTarget::SerialBaud {
                                        host: host.clone(),
                                        port: p,
                                        baud: b,
                                    },
                                ));
                            }
                        }
                        // Shared per-DUT context: needed by BOTH the
                        // MCP-routed probes (default build) and the direct
                        // probes (direct-dut-probe build).
                        let name = state
                            .values
                            .get(&FieldKey::DutName(hi, di))
                            .cloned()
                            .unwrap_or_default()
                            .trim()
                            .to_string();
                        let name = if name.is_empty() {
                            format!("dut{}", di + 1)
                        } else {
                            name
                        };
                        let ports = mcp_ports_for_dut(&cwd, &name);
                        if self.mcp.ensure(&cwd, &name, ports[0], &test_config) {
                            mcp_wait_ports.push(ports[0]);
                        }
                        let configured_channel = |key: FieldKey| {
                            state
                                .values
                                .get(&key)
                                .is_some_and(|value| !value.trim().is_empty())
                        };
                        let has_power = configured_channel(FieldKey::DutRelayPower(hi, di));
                        let has_reset = configured_channel(FieldKey::DutRelayReset(hi, di));
                        let normalization_key = if relay_enabled && has_power {
                            Some(FieldKey::DutRelayPower(hi, di))
                        } else if relay_enabled && has_reset {
                            Some(FieldKey::DutRelayReset(hi, di))
                        } else {
                            None
                        };

                        #[cfg(not(feature = "direct-dut-probe"))]
                        {
                            // Production: the MCP-owned SerialEngine performs a
                            // fresh reset/capture and classifies the RAW bytes.
                            // A merely connected TCP socket is not baud proof.
                            let baud_key =
                                if state.fields.iter().any(|f| {
                                    f.key == FieldKey::DutAdvanced(hi, di, "serial.baudrate")
                                }) {
                                    FieldKey::DutAdvanced(hi, di, "serial.baudrate")
                                } else {
                                    FieldKey::DutPort(hi, di)
                                };
                            let baud = state
                                .values
                                .get(&FieldKey::DutAdvanced(hi, di, "serial.baudrate"))
                                .cloned()
                                .filter(|v| !v.trim().is_empty())
                                .or_else(|| {
                                    state
                                        .fields
                                        .iter()
                                        .find(|f| {
                                            f.key
                                                == FieldKey::DutAdvanced(hi, di, "serial.baudrate")
                                        })
                                        .and_then(|f| f.default.clone())
                                });
                            if let Some(baud) = baud
                                && let Ok(baud) = baud.trim().parse::<u32>()
                            {
                                probes.push((
                                    baud_key,
                                    ProbeTarget::McpBaudTest {
                                        ports: ports.clone(),
                                        baud,
                                        // State normalization already cycled configured
                                        // hardware. With no dev_ctrl this samples the
                                        // current serial stream without touching a relay.
                                        use_reset: false,
                                        normalization_key: normalization_key.clone(),
                                        linked_key: FieldKey::DutPort(hi, di),
                                    },
                                ));
                            } else {
                                probes.push((baud_key, ProbeTarget::AlwaysFail));
                                probes.push((FieldKey::DutPort(hi, di), ProbeTarget::AlwaysFail));
                            }
                        }

                        // State normalization: cycles configured mode buttons
                        // (power > reset) through one full cycle so every later
                        // serial-content probe starts from a known boot state.
                        if let Some(key) = normalization_key.clone() {
                            let mut mode_buttons = Vec::new();
                            let mut linked_keys = Vec::new();
                            // ONE download-family key: the download row
                            // (download on the wire). `recovery_ch` is a
                            // legacy config key — the download layer maps it
                            // onto this same channel (see `download_ch`).
                            if configured_channel(FieldKey::DutRelayDownload(hi, di)) {
                                mode_buttons.push("download".to_string());
                                linked_keys.push(FieldKey::DutRelayDownload(hi, di));
                            }
                            probes.push((
                                key,
                                ProbeTarget::McpNormalizeState {
                                    ports: ports.clone(),
                                    use_power: has_power,
                                    mode_buttons,
                                    linked_keys,
                                },
                            ));
                            if has_power && has_reset {
                                probes.push((
                                    FieldKey::DutRelayReset(hi, di),
                                    ProbeTarget::Skip {
                                        reason: "» reset is not exercised because a power channel is configured and takes precedence"
                                            .into(),
                                    },
                                ));
                            }
                        }
                        // DUT tests run through the MCP server:
                        // the server owns the relay/serial endpoint and its
                        // engine serializes resets. The form only computes the
                        // per-DUT server ports; the hardware work is the
                        // server's serial_verify_relay / serial_enter_uboot /
                        // serial_learn_connection tools.
                        // A CHOSEN built-in relay backend (ch340-relay — the
                        // only relay choice left) makes
                        // the relay port and the reset test MANDATORY —
                        // a missing/invalid port fails BOTH rows loudly instead
                        // of skipping.
                        let backend_chosen = dev_ctrl.trim().eq_ignore_ascii_case("ch340-relay");
                        let relay_port = state
                            .values
                            .get(&FieldKey::DutRelayPort(hi, di))
                            .cloned()
                            .unwrap_or_default();
                        let reset_ch = state
                            .values
                            .get(&FieldKey::DutRelayReset(hi, di))
                            .cloned()
                            .unwrap_or_default();
                        let rp_ok = relay_port.trim().parse::<u16>().is_ok_and(|p| p > 0);
                        if relay_enabled && backend_chosen && !rp_ok {
                            probes.push((FieldKey::DutRelayPort(hi, di), ProbeTarget::AlwaysFail));
                            probes.push((FieldKey::DutRelayReset(hi, di), ProbeTarget::AlwaysFail));
                        }
                        if relay_enabled
                            && rp_ok
                            && let Ok(rp) = relay_port.trim().parse::<u16>()
                        {
                            // The DUT name drives the per-DUT MCP port.
                            let name = state
                                .values
                                .get(&FieldKey::DutName(hi, di))
                                .cloned()
                                .unwrap_or_default()
                                .trim()
                                .to_string();
                            let name = if name.is_empty() {
                                format!("dut{}", di + 1)
                            } else {
                                name
                            };
                            let ports = mcp_ports_for_dut(&cwd, &name);
                            // Start the MCP server before probing: spawn the
                            // per-DUT HTTP MCP server when none is serving this
                            // DUT yet; init closes it again on exit.
                            if self.mcp.ensure(&cwd, &name, ports[0], &test_config) {
                                mcp_wait_ports.push(ports[0]);
                            }

                            // The relay endpoint itself.
                            probes.push((
                                FieldKey::DutRelayPort(hi, di),
                                ProbeTarget::TcpConnect {
                                    host: host.clone(),
                                    port: rp,
                                },
                            ));
                            // Every other FILLED channel gets a non-disruptive
                            // direct liveness check (ch340 STATUS) on its own row
                            // — DEBUG-ONLY (production must not direct-connect
                            // to the DUT from init).
                            #[cfg(feature = "direct-dut-probe")]
                            {
                                let channels = [
                                    (
                                        FieldKey::DutRelayDownload(hi, di),
                                        state
                                            .values
                                            .get(&FieldKey::DutRelayDownload(hi, di))
                                            .cloned()
                                            .unwrap_or_default(),
                                    ),
                                    (
                                        FieldKey::DutRelayRecovery(hi, di),
                                        state
                                            .values
                                            .get(&FieldKey::DutRelayRecovery(hi, di))
                                            .cloned()
                                            .unwrap_or_default(),
                                    ),
                                    (
                                        FieldKey::DutRelayPower(hi, di),
                                        state
                                            .values
                                            .get(&FieldKey::DutRelayPower(hi, di))
                                            .cloned()
                                            .unwrap_or_default(),
                                    ),
                                ];
                                // Channel 0 IS a valid channel (this bench's
                                // ch340 wires reset to channel 0); only an EMPTY
                                // field means unset.
                                for (ch_key, ch_val) in channels {
                                    if !ch_val.trim().is_empty()
                                        && let Ok(ch) = ch_val.trim().parse::<u8>()
                                    {
                                        probes.push((
                                            ch_key,
                                            ProbeTarget::RelayChannelStatus {
                                                host: host.clone(),
                                                port: rp,
                                                channel: ch,
                                            },
                                        ));
                                    }
                                }
                            }
                            // DUT tests (reference capture + U-Boot entry) go
                            // through the MCP server FIRST; with
                            // direct-dut-probe the original direct relay probes
                            // are the fallback.
                            slow = true;
                            let ref_val = state
                                .values
                                .get(&FieldKey::DutAdvanced(hi, di, "monitor.reference_log"))
                                .cloned()
                                .unwrap_or_default();
                            let save_path = if ref_val.trim().is_empty() {
                                cwd.join(init::reference_log_default(&name))
                            } else {
                                let path = std::path::PathBuf::from(ref_val.trim());
                                if path.is_absolute() {
                                    path
                                } else {
                                    cwd.join(path)
                                }
                            };
                            let ic_val = state
                                .values
                                .get(&FieldKey::DutAdvanced(hi, di, "uboot.interrupt_char"))
                                .cloned()
                                .filter(|v| !v.trim().is_empty())
                                .unwrap_or_else(|| "ctrl_c".into());
                            let loader_is_uboot = state
                                .values
                                .get(&FieldKey::DutAdvanced(hi, di, "loader"))
                                .map(String::as_str)
                                .filter(|value| !value.trim().is_empty())
                                .or_else(|| {
                                    state
                                        .fields
                                        .iter()
                                        .find(|field| {
                                            field.key == FieldKey::DutAdvanced(hi, di, "loader")
                                        })
                                        .and_then(|field| field.default.as_deref())
                                })
                                .unwrap_or("uboot")
                                .trim()
                                == "uboot";
                            #[cfg(feature = "direct-dut-probe")]
                            let (reset_cycle_ch, interrupt_char, reset_hold_ms) = {
                                let reset_ok_for_cycle = if !reset_ch.trim().is_empty() {
                                    reset_ch.trim().parse::<u8>().ok()
                                } else {
                                    None
                                };
                                // The configured reset channel when filled, else
                                // channel 1 (the original direct-probe default).
                                let reset_cycle_ch = reset_ok_for_cycle.unwrap_or(1);
                                let reset_hold_ms = state
                                    .values
                                    .get(&FieldKey::DutRelayResetTimeMs(hi, di))
                                    .and_then(|value| value.trim().parse::<u64>().ok())
                                    .unwrap_or(0);
                                (reset_cycle_ch, parse_interrupt_char(&ic_val), reset_hold_ms)
                            };
                            // User rule: without the dev_ctrl relay the
                            // reference-log probe cannot align the boot
                            // boundary (soft `reboot` trails shutdown
                            // prints) — SKIP it, never run it to a false ✗.
                            if dut_ctrl_disabled(state, hi, di) {
                                probes.push((
                                    FieldKey::DutAdvanced(hi, di, "monitor.reference_log"),
                                    ProbeTarget::Skip {
                                        reason: "reference_log learning needs the reset relay (control.dev_ctrl is none/disabled) — a soft reboot has no reliable boot boundary, so the learn is skipped"
                                            .into(),
                                    },
                                ));
                            } else {
                                probes.push((
                                    FieldKey::DutAdvanced(hi, di, "monitor.reference_log"),
                                    ProbeTarget::RelayResetCycle {
                                        ports: ports.clone(),
                                        reference_log_path: save_path
                                            .to_string_lossy()
                                            .into_owned(),
                                        use_power: state
                                            .values
                                            .get(&FieldKey::DutRelayPower(hi, di))
                                            .is_some_and(|value| !value.trim().is_empty()),
                                        #[cfg(feature = "direct-dut-probe")]
                                        direct: DirectResetCycleProbe {
                                            relay_host: host.clone(),
                                            relay_port: rp,
                                            channel: reset_cycle_ch,
                                            serial_host: host.clone(),
                                            serial_port: p,
                                            save_path: save_path.clone(),
                                            reset_hold_ms,
                                        },
                                    },
                                ));
                            }
                            if loader_is_uboot {
                                probes.push((
                                    FieldKey::DutAdvanced(hi, di, "uboot.interrupt_char"),
                                    ProbeTarget::UbootInterrupt {
                                        ports,
                                        interrupt_char: ic_val,
                                        soft_reboot: false,
                                        normalization_key: if state
                                            .values
                                            .get(&FieldKey::DutRelayPower(hi, di))
                                            .is_some_and(|value| !value.trim().is_empty())
                                        {
                                            Some(FieldKey::DutRelayPower(hi, di))
                                        } else {
                                            Some(FieldKey::DutRelayReset(hi, di))
                                        },
                                        #[cfg(feature = "direct-dut-probe")]
                                        direct: DirectUbootProbe {
                                            relay_host: host.clone(),
                                            relay_port: rp,
                                            channel: reset_cycle_ch,
                                            serial_host: host.clone(),
                                            serial_port: p,
                                            interrupt_char,
                                            reset_hold_ms,
                                        },
                                    },
                                ));
                            }
                        }
                        if !relay_enabled {
                            probes.push((
                                FieldKey::DutAdvanced(hi, di, "monitor.reference_log"),
                                ProbeTarget::Skip {
                                    reason: "blocked: reference_log requires an enabled dev_ctrl reset relay"
                                    .into(),
                                },
                            ));
                            let loader_is_uboot = state
                                .values
                                .get(&FieldKey::DutAdvanced(hi, di, "loader"))
                                .map(String::as_str)
                                .filter(|value| !value.trim().is_empty())
                                .unwrap_or("uboot")
                                .trim()
                                == "uboot";
                            if loader_is_uboot {
                                let interrupt_char = state
                                    .values
                                    .get(&FieldKey::DutAdvanced(hi, di, "uboot.interrupt_char"))
                                    .cloned()
                                    .filter(|value| !value.trim().is_empty())
                                    .unwrap_or_else(|| "ctrl_c".into());
                                let name = state
                                    .values
                                    .get(&FieldKey::DutName(hi, di))
                                    .cloned()
                                    .unwrap_or_else(|| format!("dut{}", di + 1));
                                probes.push((
                                    FieldKey::DutAdvanced(hi, di, "uboot.interrupt_char"),
                                    ProbeTarget::UbootInterrupt {
                                        ports: mcp_ports_for_dut(&cwd, name.trim()),
                                        interrupt_char,
                                        soft_reboot: true,
                                        normalization_key: None,
                                        #[cfg(feature = "direct-dut-probe")]
                                        direct: DirectUbootProbe {
                                            relay_host: host.clone(),
                                            relay_port: 0,
                                            channel: 0,
                                            serial_host: host.clone(),
                                            serial_port: p,
                                            interrupt_char: 3,
                                            reset_hold_ms: 0,
                                        },
                                    },
                                ));
                            }
                        }

                        // Download probing is deliberately last because a
                        // successful trigger leaves the DUT in download
                        // mode. The provider resolves from the SELECTED
                        // SKILL PATH (the form's only platform context);
                        // the effective commands are the `.target.jsonc`
                        // `download.*` overlay over the provider defaults
                        // — never UI fields, never CLI overrides.
                        let provider = if state.skills_path.is_empty() {
                            None
                        } else {
                            let capability = state
                                .skills_agents
                                .first()
                                .and_then(|agent| {
                                    state.skill_catalog.resolve_leaf(&state.skills_path, agent)
                                })
                                .and_then(|leaf| leaf.download_provider);
                            download::resolve_provider(capability.as_deref(), &state.skills_path)
                        };
                        let (provider_id, list_cmd, loader_cmd) = match provider {
                            Some(provider) => {
                                let (overlay_loader, overlay_list) =
                                    download_overlay_for(test_text.as_str(), hi, di);
                                (
                                    provider.id().to_string(),
                                    overlay_list.unwrap_or_else(|| {
                                        provider.default_list_devices_cmd().to_string()
                                    }),
                                    overlay_loader.or_else(|| {
                                        provider.default_loader_cmd().map(str::to_string)
                                    }),
                                )
                            }
                            None => (String::new(), String::new(), None),
                        };
                        let reset_ch = relay_enabled
                            .then(|| {
                                state
                                    .values
                                    .get(&FieldKey::DutRelayReset(hi, di))?
                                    .trim()
                                    .parse::<u8>()
                                    .ok()
                            })
                            .flatten();
                        let download_ch = relay_enabled
                            .then(|| pane::download_channel(state, hi, di))
                            .flatten()
                            .map(|(ch, button)| (ch, button.to_string()));
                        let has_download_entry =
                            loader_cmd.as_deref().is_some_and(|l| !l.trim().is_empty())
                                || (relay_enabled && reset_ch.is_some() && download_ch.is_some());
                        if !provider_id.is_empty() && has_download_entry {
                            let name = state
                                .values
                                .get(&FieldKey::DutName(hi, di))
                                .cloned()
                                .unwrap_or_default()
                                .trim()
                                .to_string();
                            let name = if name.is_empty() {
                                format!("dut{}", di + 1)
                            } else {
                                name
                            };
                            let ports = mcp_ports_for_dut(&cwd, &name);
                            if self.mcp.ensure(&cwd, &name, ports[0], &test_config) {
                                mcp_wait_ports.push(ports[0]);
                            }
                            slow = true;
                            probes.push((
                                flash::devices_key(hi, di),
                                ProbeTarget::FlashListPreflight {
                                    host: host.clone(),
                                    user: user.clone(),
                                    pass: pass.clone(),
                                    list_cmd: list_cmd.clone(),
                                },
                            ));
                            probes.push((
                                flash::devices_key(hi, di),
                                ProbeTarget::ListDevices {
                                    host: host.clone(),
                                    user: user.clone(),
                                    pass: pass.clone(),
                                    provider_id,
                                    list_cmd,
                                    loader_cmd,
                                    reset_ch,
                                    download_ch,
                                    dev_ctrl: relay_enabled,
                                    mcp_ports: ports,
                                },
                            ));
                        }
                    } else {
                        probes.push((FieldKey::DutPort(hi, di), ProbeTarget::AlwaysFail));
                    }
                }
            }
        }
        // TEST has ensured (or found) an MCP owning the SELECTED DUT's
        // serial endpoint — the embedded terminal attaches to it
        // automatically in PASSIVE MONITOR mode so the DUT's output
        // during the probes (reset cycles, boot logs) streams into the
        // pane without claiming the manual session (an interactive
        // takeover would block every serial tool and fail the TEST).
        // An ALREADY-interactive pane (the user clicked the terminal)
        // holds that takeover: it must be released here, or every serial
        // probe — the reset cycle first — is rejected with "Serial is
        // taken over by dutabo interactive session" and the run fails no
        // matter how often TEST is clicked.
        if let Some(di) = self.state.selected_dut
            && self.state.host_count > 0
        {
            let hi = self.state.selected_host;
            let name = self
                .state
                .values
                .get(&FieldKey::DutName(hi, di))
                .cloned()
                .unwrap_or_default();
            let name = if name.trim().is_empty() {
                format!("dut{}", di + 1)
            } else {
                name.trim().to_string()
            };
            let ports = mcp_ports_for_dut(&cwd, &name);
            let endpoint_known =
                |ip: &str, port: &str| !ip.trim().is_empty() && !port.trim().is_empty();
            let (ip, port) = (
                self.state
                    .values
                    .get(&FieldKey::HostIp(hi))
                    .cloned()
                    .unwrap_or_default(),
                self.state
                    .values
                    .get(&FieldKey::DutPort(hi, di))
                    .cloned()
                    .unwrap_or_default(),
            );
            if (ports.iter().any(|p| mcp_wait_ports.contains(p)) || mcp_alive_for_project(&cwd))
                && endpoint_known(&ip, &port)
            {
                self.state.pane.watch(ports, ip, port);
            }
        }
        if probes.is_empty() {
            self.state.test_summary = Some(("test: nothing to probe".into(), ProbeMark::Skip));
            self.mcp.stop_spawned();
            return;
        }
        for (_, target) in &mut probes {
            if probe_mcp_ports(target).is_some_and(|ports| {
                ports
                    .iter()
                    .any(|port| self.mcp.blocked_ports.contains(port))
            }) {
                *target = ProbeTarget::Skip {
                    reason: "blocked: running MCP configuration does not match this DUT form; save and reload the MCP before testing"
                        .into(),
                };
            }
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let worker_probes = probes.clone();
        mcp_wait_ports.sort_unstable();
        mcp_wait_ports.dedup();
        std::thread::spawn(move || {
            // Keep the TUI responsive while a newly spawned MCP performs its
            // serial startup probe. A listening socket is not ready: control
            // tools are rejected as `connecting` until /health says `ok`.
            // Readiness is included in the hard 60-second TEST budget.
            for port in mcp_wait_ports {
                wait_for_mcp_ready(port, std::time::Duration::from_secs(10));
            }
            run_probes(worker_probes, tx)
        });
        self.state.probes.clear();
        self.state.test_owned_pids = self.mcp.owned_pids();
        self.state.refresh_endpoint_conflicts();
        for (key, _) in &probes {
            self.state.probes.insert(key.clone(), ProbeMark::Pending);
        }
        // Deliberate skips (reference learn without the reset relay) carry
        // their explanation so the · mark is self-explanatory on the form.
        for (key, target) in &probes {
            if let ProbeTarget::Skip { reason } = target {
                self.state.probe_details.insert(key.clone(), reason.clone());
            }
        }
        #[cfg(not(feature = "direct-dut-probe"))]
        for (_, target) in &probes {
            if let ProbeTarget::McpBaudTest { linked_key, .. } = target {
                self.state
                    .probes
                    .insert(linked_key.clone(), ProbeMark::Pending);
            }
        }
        for (_, target) in &probes {
            if let ProbeTarget::McpNormalizeState { linked_keys, .. } = target {
                for linked_key in linked_keys {
                    self.state
                        .probes
                        .insert(linked_key.clone(), ProbeMark::Pending);
                }
            }
        }
        // Hardware TEST is a bounded diagnostic, not exhaustive learning.
        // Readiness, baud, relay, quick reference validation, U-Boot and flash
        // enumeration must collectively finish inside one minute.
        let budget = if slow {
            std::time::Duration::from_secs(60)
        } else {
            std::time::Duration::from_millis(2500)
        };
        self.state.test_run = Some(TestRun {
            rx,
            deadline: std::time::Instant::now() + budget,
            summary: true,
        });
    }

    /// Drain the worker channel into `probes`; on the deadline (or when
    /// every mark is resolved) finalize with the summary line.
    fn poll_test_run(&mut self) {
        let Some(run) = &self.state.test_run else {
            return;
        };
        while let Ok((key, mark, detail)) = run.rx.try_recv() {
            self.state.probes.insert(key.clone(), mark);
            match detail {
                Some(d) => {
                    self.state.probe_details.insert(key, d);
                }
                // A finished probe without detail must CLEAR the leftover
                // "testing…" running note.
                None => {
                    self.state.probe_details.remove(&key);
                }
            }
        }
        let all_resolved = self.state.probes.values().all(|m| *m != ProbeMark::Pending);
        let deadline_hit = std::time::Instant::now() >= run.deadline;
        if all_resolved || deadline_hit {
            let summary_run = run.summary;
            self.state.test_run = None;
            // A per-row QUICK action stops here: its mark landed on its
            // row — no aggregate summary in the header, no endpoint
            // refresh, and the MCP stays up (the pane may be attached).
            if !summary_run {
                return;
            }
            // Hardware states changed during the run (resets, boots, relay
            // pulses) — refresh the endpoint-holder snapshot so the title
            // warning reflects post-TEST reality.
            self.state.refresh_endpoint_conflicts();
            // TEST owns the temporary MCP lifetime only while its probes are
            // in flight: stop the server as soon as every mark resolved. On
            // a deadline hit the slow probe is still mid-hardware-sequence
            // (relay cycle / U-Boot flood) — killing the server would cut it
            // off and guarantee the timeout, so leave it running: it
            // finishes naturally, McpHandle::drop cleans it up on form exit,
            // and a warm server makes a re-run skip the spawn + ready wait.
            // User rule: after EVERY TEST the spawned MCP must be GONE —
            // no warm keep-alive. A deadline-hit server is stopped too:
            // its slow probe is already past its budget, and a leftover
            // process outliving the form is exactly what this rule
            // forbids.
            self.mcp.stop_spawned();
            self.state.test_owned_pids.clear();
            let mut counts = std::collections::BTreeMap::new();
            for mark in self.state.probes.values() {
                *counts.entry(*mark).or_insert(0usize) += 1;
            }
            let mut parts = vec!["test:".to_string()];
            for (mark, label) in [
                (ProbeMark::Ok, "✓"),
                (ProbeMark::Warn, "⚠"),
                (ProbeMark::Err, "✗"),
                (ProbeMark::Skip, "» skipped"),
                (ProbeMark::Pending, "timeout"),
            ] {
                if let Some(n) = counts.get(&mark) {
                    parts.push(format!("{n} {label}"));
                }
            }
            let worst = [
                ProbeMark::Err,
                ProbeMark::Warn,
                ProbeMark::Ok,
                ProbeMark::Skip,
            ]
            .into_iter()
            .find(|m| counts.contains_key(m))
            .unwrap_or(ProbeMark::Skip);
            self.state.test_summary = Some((parts.join(" · "), worst));
        }
    }
}

impl<B: Backend, E: EventSource> PromptIO for FormTui<B, E> {
    fn input(&mut self, _prompt: &str, _default: Option<&str>) -> Result<String, String> {
        // Defensive — the form transport asks no questions (unreachable:
        // the form front runs `resolve_answers`, never `walk_schema`).
        Err("form transport asks no questions".into())
    }

    fn confirm(&mut self, _prompt: &str, _default: bool) -> Result<bool, String> {
        Err("form transport asks no questions".into())
    }

    fn finish(&mut self, _target_path: &Path, _changes: &[String]) -> Result<(), String> {
        // F2 saved DIRECTLY: `finish_init` writes immediately after this
        // returns — the Review screen is deleted.
        Ok(())
    }

    fn busy_endpoints(&mut self, conflicts: &[EndpointConflict]) -> Result<BusyDecision, String> {
        // The occupancy-gate modal owns its own event loop (it runs inside
        // `finish_init`, after the form loop has already returned): draw +
        // poll until Enter confirms the cursor or Esc/Ctrl-C/'q' aborts.
        // Default cursor = ABORT (the safe choice — writing is skipped
        // unless the user deliberately proceeds). The holder's session is
        // NEVER signaled or stopped.
        self.state.busy_gate = Some(BusyGate {
            conflicts: conflicts.to_vec(),
            cursor: 1,
        });
        let decision = loop {
            self.draw()?;
            let event = self.next_event()?;
            match event {
                Event::Key(key)
                    if key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    break BusyDecision::Abort;
                }
                Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
                    KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right => {
                        let gate = self.state.busy_gate.as_mut().expect("busy_gate is open");
                        gate.cursor = (gate.cursor + 1) % 2;
                    }
                    KeyCode::Enter => {
                        let cursor = self
                            .state
                            .busy_gate
                            .as_ref()
                            .expect("busy_gate is open")
                            .cursor;
                        break if cursor == 0 {
                            BusyDecision::Continue
                        } else {
                            BusyDecision::Abort
                        };
                    }
                    KeyCode::Esc | KeyCode::Char('q') => break BusyDecision::Abort,
                    _ => {}
                },
                _ => {}
            }
        };
        self.state.busy_gate = None;
        Ok(decision)
    }
}

// ── Entry points ───────────────────────────────────────────────────────

/// Headless-testable full pipeline: prepare → form loop → finish (the
/// auto-write runs through the same `PromptIO::finish`).
#[cfg(test)]
pub fn run_form_with<B: Backend, E: EventSource>(
    terminal: Terminal<B>,
    events: E,
    opts: &InitOptions,
    cwd: &Path,
) -> Result<WizardOutcome, String> {
    let prepared = prepare_init(cwd, opts)?;
    let mut tui = FormTui::new(
        terminal,
        events,
        FormState::new(
            prepared.schema.clone(),
            prepared.target_path.display().to_string(),
        ),
    );
    tui.write_ctx = Some((prepared.clone(), opts.clone()));
    let answers = tui.form_loop()?;
    tui.finish_project(opts, &prepared, &answers)
}

/// The real-terminal form front (the binary holds a `TerminalGuard` while
/// calling this): prepare → form loop → finish. Terminal restore is owned
/// solely by the guard's `Drop`, after this function returns.
pub fn run_form(opts: &InitOptions, cwd: &Path) -> Result<WizardOutcome, String> {
    run_form_inner(opts, cwd)
}

fn run_form_inner(opts: &InitOptions, cwd: &Path) -> Result<WizardOutcome, String> {
    let prepared = prepare_init(cwd, opts)?;
    let terminal = Terminal::new(CrosstermBackend::new(io::stdout()))
        .map_err(|e| format!("TUI start error: {e}"))?;
    let state = FormState::new(
        prepared.schema.clone(),
        prepared.target_path.display().to_string(),
    );
    // Surface endpoint holders INLINE while editing (the save-time gate
    // stays the final authority — it re-reads the locks).
    let mut state = state;
    state.endpoint_lock_dir = prepared
        .existing
        .as_ref()
        .and_then(|c| c.lock_dir.clone())
        .unwrap_or_else(sermcp::lock_manager::default_lock_dir);
    state.refresh_endpoint_conflicts();
    let mut tui = FormTui::new(terminal, CrosstermEventSource, state);
    tui.write_ctx = Some((prepared.clone(), opts.clone()));
    (|| {
        let answers = tui.form_loop()?;
        tui.finish_project(opts, &prepared, &answers)
    })()
}

// ── Plain-line fallback ────────────────────────────────────────────────

/// The non-TTY transport: byte-identical prompts to the piped-stdin
/// behavior the integration tests rely on ("{prompt} [{def}]: " on stderr,
/// stdin read_line, EOF → Err("aborted (EOF on stdin)"), y/n/empty=default
/// confirm loop). Notes print to stderr; the finish gate is a no-op so
/// piped scripts see ZERO new prompts.
pub struct PlainPromptIO;

impl PromptIO for PlainPromptIO {
    fn input(&mut self, prompt: &str, default: Option<&str>) -> Result<String, String> {
        let def = default.unwrap_or("");
        eprint!("{prompt} [{def}]: ");
        let _ = io::stderr().flush();
        let mut line = String::new();
        let n = io::stdin()
            .read_line(&mut line)
            .map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("aborted (EOF on stdin)".into());
        }
        Ok(line.trim_end_matches(['\r', '\n']).to_string())
    }

    fn confirm(&mut self, prompt: &str, default: bool) -> Result<bool, String> {
        let hint = if default { "Y/n" } else { "y/N" };
        loop {
            eprint!("{prompt} [{hint}]: ");
            let _ = io::stderr().flush();
            let mut line = String::new();
            let n = io::stdin()
                .read_line(&mut line)
                .map_err(|e| e.to_string())?;
            if n == 0 {
                return Err("aborted (EOF on stdin)".into());
            }
            match line.trim().to_ascii_lowercase().as_str() {
                "" => return Ok(default),
                "y" | "yes" => return Ok(true),
                "n" | "no" => return Ok(false),
                _ => {} // re-ask
            }
        }
    }

    fn note(&mut self, _kind: NoteKind, msg: &str) {
        eprintln!("note: {msg}");
    }

    fn busy_endpoints(&mut self, conflicts: &[EndpointConflict]) -> Result<BusyDecision, String> {
        // The holder's session is NEVER touched — the user decides. Default
        // (empty enter) = abort, the safe choice.
        for c in conflicts {
            eprintln!("{}", endpoint_conflict_line(c));
        }
        eprintln!(
            "The other MCP's session is NOT touched — it keeps running. Writing the config does not free the endpoint."
        );
        loop {
            eprint!("Continue writing the config anyway? [y/N]: ");
            let _ = io::stderr().flush();
            let mut line = String::new();
            let n = io::stdin()
                .read_line(&mut line)
                .map_err(|e| e.to_string())?;
            if n == 0 {
                return Err("aborted (EOF on stdin)".into());
            }
            match line.trim().to_ascii_lowercase().as_str() {
                "" | "n" | "no" => return Ok(BusyDecision::Abort),
                "y" | "yes" => return Ok(BusyDecision::Continue),
                _ => {} // re-ask
            }
        }
    }
}
// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
