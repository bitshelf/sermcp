//! `dutabo init` — JSONC config wizard (headless core).
//!
//! The entire wizard is a pure function of (options, `PromptIO` answers, CWD):
//! the binary supplies a ratatui TUI `PromptIO` (plus a plain-line fallback
//! when stdin/stdout is not a terminal), tests supply a canned-answer one,
//! and nothing in this module ever touches a TTY. The lib (and therefore the
//! MCP server binary) has NO TUI dependency.
//!
//! Two flows share one entry point:
//! - CREATE — no `.target.jsonc` anywhere: ask the quick question set (host
//!   count → per host: ip/user/pass, DUT count → per DUT:
//!   dut_name/serial.port/login_user/relay.type) plus the optional advanced
//!   group (global fallback sections), render a canonical fully-commented
//!   document, write it atomically, and generate `.mcp.json` if absent.
//! - UPDATE — an existing `.target.jsonc`: parse it (strict) to seed the
//!   answer defaults, then apply changes as byte-level edit ops (see below).
//!   A file that does NOT parse is never edited (refuse, exit 1).
//!
//! EDIT STRATEGY (update) = TEXT-PATCH, never AST rewrite:
//! 1. parse the existing text with the strict `jsonc_options()`; on failure
//!    the wizard REFUSES to edit — hand-edited errors must never be silently
//!    normalized away;
//! 2. build a span map with a small quote/bracket-aware locator (the file is
//!    already validated, so the locator only LOCATES; a bad locate is caught
//!    by step 4);
//! 3. ops: SET scalar = replace the value span bytes with the new literal,
//!    preserving any same-line trailing comment and the trailing comma;
//!    APPEND host/dut = splice an indented rendered block before the closing
//!    bracket of the parent `dev_hosts`/`duts` array (comma-correct even
//!    when the last entry lacks a trailing comma); REMOVE key = drop the
//!    member's whole line range (used when a zero answer means "inherit");
//! 4. after EVERY op the whole file is re-parsed with the strict options;
//!    on failure the edit reverts to the pre-edit in-memory copy and the
//!    wizard returns Err — it can never corrupt a file it parsed.
//!
//! Untouched regions stay byte-identical BY CONSTRUCTION: comments are never
//! rewritten. Structural moves (relocating a DUT between hosts) are
//! delete+append, not rename; this wizard only appends — shrinking a config
//! is a hand edit (documented limitation).
//!
//! The wizard cannot UNSET string values (empty input keeps the current
//! value); numeric answers of 0 / 0.0 unset zero-guarded keys instead
//! (REMOVE op).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::config::{
    CONFIG_FILE_NAME, JsoncControl, JsoncDevHost, JsoncDut, JsoncFlash, JsoncMonitor, JsoncRelay,
    JsoncSerial, JsoncTarget, JsoncUboot, NumberOrString, TargetJsoncConfig, supported_relay_types,
};
use crate::loader;

/// `.mcp.json` `sermcp.description` (canonical template text,
/// pointing `TARGET_CONF` at the project's `.target.jsonc`).
const MCP_JSON_DESCRIPTION: &str = "Serial debug MCP for embedded Linux target boards";

/// Canonical document header — the comment block every wizard-written
/// `.target.jsonc` starts with (mirrors references/.target.jsonc.example).
const HEADER: &str = r#"// .target.jsonc — Embedded Debug Target Configuration (JSONC)
// Copy to project root. // comments and trailing commas are valid.
// Create/update via `dutabo init` — it preserves your comments.
//
// Structure: dev_hosts[] each NESTS its duts[]. A DUT inherits its parent
// host's ip/user/pass — names are display-only; the host ip is the linking key.
// Top-level sections (serial/target/uboot/relay/monitor/flash/control) are
// GLOBAL fallbacks: a DUT value overrides; otherwise the global value wins.
//   commented out = not set (built-in default)
//   port 0 / "0"  = unset sentinel (validation error, as before)
//   ""            = explicitly empty -> falls back to parent dev host ip
//                    (relay.ip only — serial has no ip key at all)
// Unknown keys are rejected at parse time (typos fail loudly).
// The agent must NOT modify this file — read only. Edit via `dutabo init`.
"#;

/// Commented-out global fallback block (CREATE only — teaches the shape).
const GLOBAL_HINTS: &str = r#"  // Global fallbacks (optional) — apply to every DUT that does not override:
  // "serial":  { "port": "<required>", "baudrate": "<required>", "rfc2217": false },
  // "target":  { "login_user": "root", "login_prompt": "" },
  // "uboot":   { "interrupt_char": "ctrl_c", "interrupt_strategy": "flood", "pattern": "Hit any key" },
  // "relay":   { "type": "ch340", "reset_ch": 1, "power_off_time_ms": 3000 },
  // "monitor": { "hang_timeout": 60, "max_archived_logs": 10, "crash_keywords": ["Kernel panic"] },
  // "flash":   { "tool": "<vendor flash tool>", "upload_dir": "/tmp" },
  // "control": { "dev_ctrl": "" },
  // "loader":  "uboot",                    // "uboot" | "uefi" (uefi: not implemented yet)
  // "reference_log": "",       // back-compat top-level key (overrides monitor.reference_log)
  // "dut_dir": ".dut-serial",  // back-compat top-level key
  // "lock_dir": "/tmp/sermcp/locks"   // top-level ONLY — never valid inside a dut
"#;

/// Key comments (canonical example text) — the single source for the
/// renderer and the append blocks.
const C_DUT_NAME: &str = "REQUIRED — unique across ALL DUTs";
const C_HOST_IP: &str = "REQUIRED — ssh / ser2net host";
const C_HOST_USER: &str = "ssh user (shown as user@ip in dutabo list)";
const C_HOST_PASS: &str = "ssh password (optional, plaintext as before)";
const C_HOST_NAME: &str = "optional display name — falls back to the host ip when absent";
const C_PORT: &str = "ser2net port OR device path \"/dev/ttyUSB0\"; number or string";
const C_BAUDRATE: &str = "number or string (default 460800)";
const C_RFC2217: &str = "true for telnet(rfc2217) ser2net ports (dynamic baud)";
const C_LOGIN_USER: &str = "empty = boot-to-shell detection disabled (warn)";
const C_LOGIN_PROMPT: &str = "regex; empty = built-in \"login:\" matcher";
const C_UBOOT_CHAR: &str = "ctrl_c | \"0x03\" | any 1 char | decimal";
const C_RELAY_TYPE: &str =
    "ch340 | ch340-relay | none | disabled (usb-relay and external dev-ctl removed)";
const C_RELAY_CH_ALIASES: &str =
    "aliases reset_channel/download_channel/power_channel also accepted";
const C_POWER_CH: &str = "0 = inherit global (zero-guarded insertion)";
const C_RESET_TIME: &str = "0 = inherit";
const C_POWER_OFF: &str = "clamped to >= 3000; 0 = inherit global";
const C_CRASH_KEYWORDS: &str = "substrings, not regex";
const C_DUT_DIR: &str = "default: \".dut-serial/<dut_name>\"";
const C_REFERENCE_LOG_DUT: &str = "back-compat per-DUT key, overrides monitor.reference_log";
const C_LOCK_DIR: &str = "top-level ONLY — never valid inside a dut";

/// Prompt abstraction. The binary implements this with a ratatui TUI (plus a
/// plain-line fallback when stdin/stdout is not a terminal); tests use a
/// canned-answer implementation. Every method returns the RAW line/choice —
/// policy (re-ask on empty, defaults, collision handling) lives in the
/// wizard, not in the transport.
pub trait PromptIO {
    /// Ask for a line of text. `default` is rendered in the prompt TEXT only
    /// (informational); an empty submit returns "" — the wizard decides what
    /// empty means (accept the default / skip / re-ask). Err = user abort
    /// (EOF / Ctrl-C).
    fn input(&mut self, prompt: &str, default: Option<&str>) -> Result<String, String>;
    /// Yes/no question.
    fn confirm(&mut self, prompt: &str, default: bool) -> Result<bool, String>;
    /// Non-fatal feedback for the current question (re-ask explanations,
    /// auto-naming announcements). Defaults to silent — headless transports
    /// stay byte-identical.
    fn note(&mut self, _kind: NoteKind, _msg: &str) {}
    /// Review gate: called with the authoritative pending change lines
    /// BEFORE anything is written. Err = user abort at the Review (zero
    /// bytes on disk). Defaults to Ok — headless transports stay
    /// byte-identical.
    fn finish(&mut self, _target_path: &Path, _changes: &[String]) -> Result<(), String> {
        Ok(())
    }
    /// Occupancy gate: called when the answers select serial
    /// endpoints currently held by ANOTHER live MCP process. The transport
    /// MUST surface every conflict (endpoint, PID, since, holder project)
    /// and let the user decide (Continue = write the config anyway, Abort =
    /// stop the wizard). The holder's session is NEVER signaled or stopped
    /// here — another path's MCP may be mid-task; only the user resolves it.
    /// Default: Continue (headless transports without a user rely on the
    /// non-interactive path, which warns and continues).
    fn busy_endpoints(&mut self, _conflicts: &[EndpointConflict]) -> Result<BusyDecision, String> {
        Ok(BusyDecision::Continue)
    }
}

/// Severity of a `PromptIO::note`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteKind {
    /// Informational (auto-naming announcements).
    Info,
    /// Error (invalid input — the question will be re-asked).
    Error,
}

/// `dutabo init` argument shape (also `DUTABO_NON_INTERACTIVE=1` env).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InitOptions {
    /// Headless: answers come from defaults only; missing required values
    /// produce a hard error listing EVERY one of them.
    pub non_interactive: bool,
    /// Minimal question set (skip the advanced global-fallback group).
    pub quick: bool,
    /// Force-ask the advanced group.
    pub advanced: bool,
    /// `--force` overwrites an existing `.target.jsonc` (fresh CREATE
    /// instead of UPDATE).
    pub force: bool,
    /// `--tui`: force the full-screen TUI transport (the binary's dispatch
    /// gate decides otherwise). Data only — this module stays TUI-free.
    pub tui: bool,
    /// `--plain`: force the plain line-prompt transport (no TUI even on a
    /// real terminal). Mutually exclusive with `--tui`.
    pub plain: bool,
}

impl InitOptions {
    /// Parse positional args after the `init` command. Returns Err for
    /// unknown flags (the binary exits 1 with the message).
    pub fn from_args(positional: &[String], non_interactive_env: bool) -> Result<Self, String> {
        let mut opts = InitOptions::default();
        for arg in positional {
            match arg.as_str() {
                "--non-interactive" => opts.non_interactive = true,
                "--quick" => opts.quick = true,
                "--advanced" => opts.advanced = true,
                "--force" => opts.force = true,
                "--tui" => opts.tui = true,
                "--plain" => opts.plain = true,
                _ => return Err(format!("unknown argument for `dutabo init`: {arg}")),
            }
        }
        if non_interactive_env {
            opts.non_interactive = true;
        }
        if opts.quick && opts.advanced {
            return Err("--quick and --advanced are mutually exclusive".into());
        }
        if opts.tui && opts.plain {
            return Err("--tui and --plain are mutually exclusive".into());
        }
        Ok(opts)
    }
}

// ── Declarative form schema (single source of truth) ──────────────────
//
// The static FORM_GROUPS table describes the WHOLE wizard — the plain
// sequential walk (walk_schema), the headless default filler
// (fill_defaults), the form TUI renderer (derive_fields) and the form
// submit resolver (resolve_answers) all consume it, so the question order,
// labels, kinds, required-ness, empty-submit semantics and defaults can
// never drift between the two fronts. Group order == the legacy
// depth-first question order exactly.

/// Editor/widget kind of a field.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum FieldKind {
    /// Free text.
    Text,
    /// Digits-only count editor (UI clamps 1..=host_count_max).
    Count,
    /// Option picker (relay.type); the plain walk asks it via `io.input`
    /// like every other field — this is TUI rendering metadata only.
    Select,
    /// Yes/no toggle row (the per-DUT relay gate). Rendered as `[x]`/`[ ]`
    /// with Space/y = on, n = off; the plain walk asks it via
    /// `io.confirm` BEFORE any gated sub-field.
    Toggle,
}

/// What a bare (empty) submit means for the field.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum EmptyMeans {
    /// Empty submit → Some(default). Required fields with an empty default
    /// still error ("value is required"); the others accept whatever the
    /// default is (possibly "").
    AcceptDefault,
    /// Empty submit → the key is ABSENT from the answers (skip/inherit).
    /// Used by the ""-default advanced keys and the zero-guarded relay
    /// family ("0" typed is recorded and later maps to RemoveKey).
    SkipInherit,
}

/// How a group template is instantiated.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum Repeat {
    Once,
    PerHost,
    PerHostDut,
}

/// A field's identity inside a template group.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum FieldKeyFrag {
    HostCount,
    HostIp,
    HostUser,
    HostPass,
    HostName,
    DutCount,
    DutName,
    DutPort,
    DutLoginUser,
    /// The per-DUT relay gate toggle — "use relay?"; the relay sub-fields
    /// below only render/ask when it is on.
    DutRelayGate,
    DutRelayType,
    DutRelayPort,
    DutRelayReset,
    DutRelayDownload,
    DutRelayPower,
    DutRelayResetTimeMs,
    DutRelayPowerOffTimeMs,
    /// Flat key path, e.g. "monitor.hang_timeout" — the HOST-LEVEL
    /// advanced keys only (the global fallback set; 4 keys).
    Advanced(&'static str),
    /// Flat key path of a DUT-OWNED advanced key (e.g.
    /// "monitor.crash_keywords") — materializes per (host, DUT) into
    /// `FieldKey::DutAdvanced` (one dev host may serve an
    /// RK3576 AND a T527; each DUT edits its own buffer).
    DutAdvanced(&'static str),
}

/// Where a field's default hint comes from.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum DefaultSrc {
    /// Fixed literal ("root" for login_user, "ch340" for relay.type,
    /// "" for the runtime-seeded ip/user/pass/port fields whose real
    /// defaults come from the existing config at materialization time).
    Static(&'static str),
    /// next_free_dut_name over taken ∪ current non-empty values (DUT auto-name).
    AutoDutName,
    /// advanced_default(key) after the current value.
    AdvancedNatural,
    /// ".dut-serial/{first_dut_name}/reference-boot.log" (fallback "dut1")
    /// when no current value exists — monitor.reference_log only.
    ReferenceLogComposed,
    /// Host/dut count default from the runtime schema.
    RuntimeCount,
    /// Per-DUT relay gate default from the runtime schema's DutDefaults
    /// (update: existing relay section present; create: NO).
    RuntimeRelayGate,
}

fn empty_slice<T>(s: &[T]) -> bool {
    s.is_empty()
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FieldDef {
    pub key: FieldKeyFrag,
    pub kind: FieldKind,
    /// EXACT current prompt literal.
    pub label: &'static str,
    /// "(required)" suffix + submit gate + missing_required membership.
    pub required: bool,
    pub empty: EmptyMeans,
    pub default: DefaultSrc,
    /// Select options (only when kind == Select). Empty lists serialize
    /// away (the pinned FORM_GROUPS snapshot stays byte-stable).
    #[serde(skip_serializing_if = "empty_slice")]
    pub options: &'static [&'static str],
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FormGroup {
    pub repeat: Repeat,
    pub title: &'static str,
    pub fields: &'static [FieldDef],
}

/// The advanced group gate: asked once (plain: confirm; form: tab toggle).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum AdvancedGate {
    /// --quick: the whole advanced group is skipped.
    Skip,
    /// Normal: gate defaults to `default` (form toggle / plain confirm).
    Confirm { default: bool },
    /// --advanced: gate is on and non-dismissable.
    Forced,
}

/// One advanced key's spec — REPLACES the old ADVANCED_KEYS +
/// ENTER_ACCEPTS_DEFAULT pair (same order/labels; `empty: AcceptDefault`
/// == the old ENTER_ACCEPTS_DEFAULT membership, asserted by a migration
/// test).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AdvFieldSpec {
    pub key: &'static str,
    pub label: &'static str,
    pub empty: EmptyMeans,
}

/// Single source for the ADVANCED_FIELDS spec table (the ordered key set
/// shared by the walk, the resolver and the migration pins). The two
/// FORM_GROUPS field lists (ADVANCED_GROUP_FIELDS = host-level, 4 keys;
/// DUT_ADVANCED_GROUP_FIELDS = DUT-owned, 16 keys —: one dev
/// host may serve an RK3576 AND a T527) are hand-written below the table
/// and a test pins the partition one-for-one against ADVANCED_FIELDS.
macro_rules! advanced_field_table {
    ($($key:literal, $label:literal, $empty:ident, $default:ident $(, select [$($opt:literal),*])? $(, dut)?;)* $(;)?) => {
        pub const ADVANCED_FIELDS: &[AdvFieldSpec] = &[
            $(AdvFieldSpec { key: $key, label: $label, empty: EmptyMeans::$empty }),*
        ];
    };
}

/// HOST-LEVEL advanced group (the global fallback set — 4 keys; the
/// DUT-owned set lives in DUT_ADVANCED_GROUP_FIELDS). The
/// migration pin test below asserts both groups partition ADVANCED_FIELDS
/// one-for-one.
const ADVANCED_GROUP_FIELDS: &[FieldDef] = &[
    FieldDef {
        key: FieldKeyFrag::Advanced("monitor.hang_timeout"),
        kind: FieldKind::Text,
        label: "Hang timeout seconds",
        required: false,
        empty: EmptyMeans::AcceptDefault,
        default: DefaultSrc::AdvancedNatural,
        options: &[],
    },
    FieldDef {
        key: FieldKeyFrag::Advanced("monitor.max_log_file_size"),
        kind: FieldKind::Text,
        label: "Max log file size MB",
        required: false,
        empty: EmptyMeans::AcceptDefault,
        default: DefaultSrc::AdvancedNatural,
        options: &[],
    },
    FieldDef {
        key: FieldKeyFrag::Advanced("monitor.learner_stage_threshold"),
        kind: FieldKind::Text,
        label: "Learner stage threshold (0.0-1.0)",
        required: false,
        empty: EmptyMeans::AcceptDefault,
        default: DefaultSrc::AdvancedNatural,
        options: &[],
    },
    FieldDef {
        key: FieldKeyFrag::Advanced("monitor.learner_crash_threshold"),
        kind: FieldKind::Text,
        label: "Learner crash threshold (0.0-1.0)",
        required: false,
        empty: EmptyMeans::AcceptDefault,
        default: DefaultSrc::AdvancedNatural,
        options: &[],
    },
];

/// DUT-OWNED advanced group (per-DUT storage): one dev host
/// may serve an RK3576 AND a T527 — these keys render and store inside
/// each DUT's own sections.
const DUT_ADVANCED_GROUP_FIELDS: &[FieldDef] = &[
    FieldDef {
        key: FieldKeyFrag::DutAdvanced("serial.baudrate"),
        kind: FieldKind::Text,
        label: "Serial baudrate (number)",
        required: false,
        empty: EmptyMeans::AcceptDefault,
        default: DefaultSrc::AdvancedNatural,
        options: &[],
    },
    FieldDef {
        key: FieldKeyFrag::DutAdvanced("target.login_pass"),
        kind: FieldKind::Text,
        label: "Target login password",
        required: false,
        empty: EmptyMeans::SkipInherit,
        default: DefaultSrc::AdvancedNatural,
        options: &[],
    },
    FieldDef {
        key: FieldKeyFrag::DutAdvanced("target.login_prompt"),
        kind: FieldKind::Text,
        label: "Target login prompt regex",
        required: false,
        empty: EmptyMeans::SkipInherit,
        default: DefaultSrc::AdvancedNatural,
        options: &[],
    },
    FieldDef {
        key: FieldKeyFrag::DutAdvanced("uboot.interrupt_char"),
        kind: FieldKind::Text,
        label: "U-Boot interrupt char",
        required: false,
        empty: EmptyMeans::AcceptDefault,
        default: DefaultSrc::AdvancedNatural,
        options: &[],
    },
    FieldDef {
        key: FieldKeyFrag::DutAdvanced("uboot.interrupt_strategy"),
        kind: FieldKind::Select,
        label: "U-Boot interrupt strategy",
        required: false,
        empty: EmptyMeans::AcceptDefault,
        default: DefaultSrc::AdvancedNatural,
        options: &["flood", "pattern"],
    },
    FieldDef {
        key: FieldKeyFrag::DutAdvanced("uboot.pattern"),
        kind: FieldKind::Text,
        label: "U-Boot interrupt pattern regex",
        required: false,
        empty: EmptyMeans::SkipInherit,
        default: DefaultSrc::AdvancedNatural,
        options: &[],
    },
    FieldDef {
        key: FieldKeyFrag::DutAdvanced("monitor.max_archived_logs"),
        kind: FieldKind::Text,
        label: "Max archived boot logs",
        required: false,
        empty: EmptyMeans::AcceptDefault,
        default: DefaultSrc::AdvancedNatural,
        options: &[],
    },
    FieldDef {
        key: FieldKeyFrag::DutAdvanced("monitor.reference_log"),
        kind: FieldKind::Text,
        label: "Reference boot log path",
        required: false,
        empty: EmptyMeans::AcceptDefault,
        default: DefaultSrc::ReferenceLogComposed,
        options: &[],
    },
    FieldDef {
        key: FieldKeyFrag::DutAdvanced("monitor.crash_keywords"),
        kind: FieldKind::Text,
        label: "Crash keywords (comma/newline separated)",
        required: false,
        empty: EmptyMeans::AcceptDefault,
        default: DefaultSrc::AdvancedNatural,
        options: &[],
    },
    FieldDef {
        key: FieldKeyFrag::DutAdvanced("flash.full_image_cmd"),
        kind: FieldKind::Text,
        label: "Flash full-image command",
        required: false,
        empty: EmptyMeans::AcceptDefault,
        default: DefaultSrc::AdvancedNatural,
        options: &[],
    },
    FieldDef {
        key: FieldKeyFrag::DutAdvanced("flash.kernel_image_cmd"),
        kind: FieldKind::Text,
        label: "Flash kernel-image command",
        required: false,
        empty: EmptyMeans::AcceptDefault,
        default: DefaultSrc::AdvancedNatural,
        options: &[],
    },
    FieldDef {
        key: FieldKeyFrag::DutAdvanced("flash.loader_bin"),
        kind: FieldKind::Text,
        label: "Flash loader binary path",
        required: false,
        empty: EmptyMeans::AcceptDefault,
        default: DefaultSrc::AdvancedNatural,
        options: &[],
    },
    FieldDef {
        key: FieldKeyFrag::DutAdvanced("control.dev_ctrl"),
        kind: FieldKind::Select,
        label: "Device control backend",
        required: false,
        empty: EmptyMeans::SkipInherit,
        default: DefaultSrc::AdvancedNatural,
        // Replaceable backend selector: the built-in ch340 relay or none
        // (only these two choices remain). `dutabo init` writes
        // the choice to `control.dev_ctrl`; the MCP server resolves it (see
        // `resolve_relay_type`), so swapping the power-control device only
        // changes this one field.
        options: &["ch340-relay", "none"],
    },
    FieldDef {
        key: FieldKeyFrag::DutAdvanced("loader"),
        kind: FieldKind::Select,
        label: "Boot loader",
        required: false,
        empty: EmptyMeans::AcceptDefault,
        default: DefaultSrc::AdvancedNatural,
        options: loader::OPTIONS,
    },
];

/// The DUT-owned advanced key set (reclassification): these
/// keys render and store PER DUT — editing dut2's crash_keywords must
/// never overwrite dut1's. A test pins this list to the group split.
pub const DUT_ADVANCED_KEYS: &[&str] = &[
    "serial.baudrate",
    "target.login_pass",
    "target.login_prompt",
    "uboot.interrupt_char",
    "uboot.interrupt_strategy",
    "uboot.pattern",
    "monitor.max_archived_logs",
    "monitor.reference_log",
    "monitor.crash_keywords",
    "flash.full_image_cmd",
    "flash.kernel_image_cmd",
    "flash.loader_bin",
    "control.dev_ctrl",
    "loader",
];

/// Is the advanced key DUT-owned (per-DUT storage) as opposed to a
/// HOST-level global fallback?
pub fn dut_owned_advanced(k: &str) -> bool {
    DUT_ADVANCED_KEYS.contains(&k)
}

advanced_field_table! {
    "serial.baudrate", "Serial baudrate (number)", AcceptDefault, AdvancedNatural;
    "target.login_pass", "Target login password", SkipInherit, AdvancedNatural;
    "target.login_prompt", "Target login prompt regex", SkipInherit, AdvancedNatural;
    "uboot.interrupt_char", "U-Boot interrupt char", AcceptDefault, AdvancedNatural;
    "uboot.interrupt_strategy", "U-Boot interrupt strategy", AcceptDefault, AdvancedNatural;
    "uboot.pattern", "U-Boot interrupt pattern regex", SkipInherit, AdvancedNatural;
    "monitor.hang_timeout", "Hang timeout seconds", AcceptDefault, AdvancedNatural;
    "monitor.max_archived_logs", "Max archived boot logs", AcceptDefault, AdvancedNatural;
    "monitor.max_log_file_size", "Max log file size MB", AcceptDefault, AdvancedNatural;
    "monitor.reference_log", "Reference boot log path", AcceptDefault, ReferenceLogComposed;
    "monitor.learner_stage_threshold", "Learner stage threshold (0.0-1.0)", AcceptDefault, AdvancedNatural;
    "monitor.learner_crash_threshold", "Learner crash threshold (0.0-1.0)", AcceptDefault, AdvancedNatural;
    "monitor.crash_keywords", "Crash keywords (comma/newline separated)", AcceptDefault, AdvancedNatural;
    "flash.full_image_cmd", "Flash full-image command", AcceptDefault, AdvancedNatural;
    "flash.kernel_image_cmd", "Flash kernel-image command", AcceptDefault, AdvancedNatural;
    "flash.loader_bin", "Flash loader binary path", AcceptDefault, AdvancedNatural;
    "control.dev_ctrl", "Device control backend", SkipInherit, AdvancedNatural, select ["ch340-relay", "none"];
    "loader", "Boot loader", AcceptDefault, AdvancedNatural, select ["uboot", "uefi"];
}

/// The static form table. ORDER IS LOAD-BEARING: group 0..2 == the EXACT
/// legacy quick question order (host count → per host:
/// ip/user/pass/dut-count → per DUT: dut_name/port/login_user/relay);
/// group 3 is the DUT-OWNED advanced set (PerHostDut — asked per DUT in
/// every mode); group 4 is the HOST-LEVEL advanced fallback set (Once —
/// behind the advanced gate).
pub static FORM_GROUPS: &[FormGroup] = &[
    FormGroup {
        repeat: Repeat::Once,
        title: "host-count",
        fields: &[FieldDef {
            key: FieldKeyFrag::HostCount,
            kind: FieldKind::Count,
            label: "Number of dev hosts",
            required: false,
            empty: EmptyMeans::AcceptDefault,
            default: DefaultSrc::RuntimeCount,
            options: &[],
        }],
    },
    FormGroup {
        repeat: Repeat::PerHost,
        title: "dev-host",
        fields: &[
            FieldDef {
                key: FieldKeyFrag::HostIp,
                kind: FieldKind::Text,
                label: "Dev host ip (ssh / ser2net host)",
                required: true,
                empty: EmptyMeans::AcceptDefault,
                default: DefaultSrc::Static(""),
                options: &[],
            },
            FieldDef {
                key: FieldKeyFrag::HostUser,
                kind: FieldKind::Text,
                label: "Dev host ssh user",
                required: false,
                empty: EmptyMeans::AcceptDefault,
                default: DefaultSrc::Static(""),
                options: &[],
            },
            FieldDef {
                key: FieldKeyFrag::HostPass,
                kind: FieldKind::Text,
                label: "Dev host ssh password (plaintext)",
                required: false,
                empty: EmptyMeans::AcceptDefault,
                default: DefaultSrc::Static(""),
                options: &[],
            },
            FieldDef {
                key: FieldKeyFrag::HostName,
                kind: FieldKind::Text,
                label: "Dev host display name (optional — falls back to ip)",
                required: false,
                empty: EmptyMeans::AcceptDefault,
                default: DefaultSrc::Static(""),
                options: &[],
            },
            FieldDef {
                key: FieldKeyFrag::DutCount,
                kind: FieldKind::Count,
                label: "Number of DUTs on this host",
                required: false,
                empty: EmptyMeans::AcceptDefault,
                default: DefaultSrc::RuntimeCount,
                options: &[],
            },
        ],
    },
    FormGroup {
        repeat: Repeat::PerHostDut,
        title: "dut",
        fields: &[
            FieldDef {
                key: FieldKeyFrag::DutName,
                kind: FieldKind::Text,
                label: "DUT name",
                required: false,
                empty: EmptyMeans::AcceptDefault,
                default: DefaultSrc::AutoDutName,
                options: &[],
            },
            FieldDef {
                key: FieldKeyFrag::DutPort,
                kind: FieldKind::Text,
                label: "Serial port (ser2net port or /dev/ttyUSB0)",
                required: true,
                empty: EmptyMeans::AcceptDefault,
                default: DefaultSrc::Static(""),
                options: &[],
            },
            FieldDef {
                key: FieldKeyFrag::DutLoginUser,
                kind: FieldKind::Text,
                label: "Target login user",
                required: false,
                empty: EmptyMeans::AcceptDefault,
                default: DefaultSrc::Static("root"),
                options: &[],
            },
            FieldDef {
                key: FieldKeyFrag::DutRelayGate,
                kind: FieldKind::Toggle,
                label: "Use relay? (reset/download/power control)",
                required: false,
                empty: EmptyMeans::AcceptDefault,
                default: DefaultSrc::RuntimeRelayGate,
                options: &[],
            },
            FieldDef {
                key: FieldKeyFrag::DutRelayType,
                kind: FieldKind::Select,
                label: "Relay type",
                required: false,
                empty: EmptyMeans::AcceptDefault,
                default: DefaultSrc::Static("ch340"),
                options: &[],
            },
            FieldDef {
                key: FieldKeyFrag::DutRelayPort,
                kind: FieldKind::Text,
                label: "Relay port (0 = inherit)",
                required: false,
                empty: EmptyMeans::SkipInherit,
                default: DefaultSrc::Static("0"),
                options: &[],
            },
            FieldDef {
                key: FieldKeyFrag::DutRelayReset,
                kind: FieldKind::Text,
                label: "Relay reset (0 = inherit)",
                required: false,
                empty: EmptyMeans::SkipInherit,
                default: DefaultSrc::Static("0"),
                options: &[],
            },
            FieldDef {
                key: FieldKeyFrag::DutRelayDownload,
                kind: FieldKind::Text,
                label: "Relay download (0 = inherit)",
                required: false,
                empty: EmptyMeans::SkipInherit,
                default: DefaultSrc::Static("0"),
                options: &[],
            },
            FieldDef {
                key: FieldKeyFrag::DutRelayPower,
                kind: FieldKind::Text,
                label: "Relay power (0 = inherit)",
                required: false,
                empty: EmptyMeans::SkipInherit,
                default: DefaultSrc::Static("0"),
                options: &[],
            },
            FieldDef {
                key: FieldKeyFrag::DutRelayResetTimeMs,
                kind: FieldKind::Text,
                label: "Reset hold ms — how long the reset key stays pressed (0 = inherit)",
                required: false,
                empty: EmptyMeans::SkipInherit,
                default: DefaultSrc::Static("0"),
                options: &[],
            },
            FieldDef {
                key: FieldKeyFrag::DutRelayPowerOffTimeMs,
                kind: FieldKind::Text,
                label: "Power hold ms — how long the power key stays pressed (0 = inherit global)",
                required: false,
                empty: EmptyMeans::AcceptDefault,
                default: DefaultSrc::Static("3000"),
                options: &[],
            },
        ],
    },
    FormGroup {
        repeat: Repeat::PerHostDut,
        title: "dut-advanced",
        fields: DUT_ADVANCED_GROUP_FIELDS,
    },
    FormGroup {
        repeat: Repeat::Once,
        title: "advanced",
        fields: ADVANCED_GROUP_FIELDS,
    },
];

// ── Runtime schema (built once per run from the existing config) ───────

#[derive(Debug, Clone)]
pub enum SchemaTarget {
    /// No `.target.jsonc` (or --force): fresh CREATE defaults.
    Create,
    /// Existing config — prefill source for every form field.
    Update(Box<TargetJsoncConfig>),
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct HostDefaults {
    pub ip: Option<String>,
    pub user: Option<String>,
    pub pass: Option<String>,
    pub host_name: Option<String>,
    pub dut_count: usize,
    pub duts: Vec<DutDefaults>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DutDefaults {
    pub dut_name: Option<String>,
    pub serial_port: Option<String>,
    pub login_user: Option<String>,
    /// Existing DUT relay section present => gate pre-fills YES.
    pub relay_gate: bool,
    pub relay_type: Option<String>,
    pub relay_port: Option<String>,
    pub relay_reset_ch: Option<String>,
    pub relay_download_ch: Option<String>,
    pub relay_power_ch: Option<String>,
    pub relay_reset_time_ms: Option<String>,
    pub relay_power_off_time_ms: Option<String>,
    /// This DUT's OWN advanced override values (from its own sections —
    /// never the global fallbacks).
    pub advanced: HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct FormSchema {
    pub target: SchemaTarget,
    pub host_count_default: usize,
    /// Count-field UI clamp (host AND dut counts).
    pub host_count_max: usize,
    /// Seeded with ALL existing DUT names (collision-free auto-naming).
    pub taken_duts: Vec<String>,
    pub host_defaults: Vec<HostDefaults>,
    pub advanced_gate: AdvancedGate,
    /// advanced_current(config) — the update defaults for advanced keys.
    pub advanced_current: HashMap<String, String>,
}

/// Build the runtime schema from the existing config (None = CREATE) and
/// the CLI options.
pub fn build_form_schema(existing: Option<&TargetJsoncConfig>, opts: &InitOptions) -> FormSchema {
    let host_count_default = existing.map(|c| c.dev_hosts.len()).unwrap_or(1);
    let taken_duts: Vec<String> = existing
        .map(|c| {
            c.dev_hosts
                .iter()
                .flat_map(|h| h.duts.iter().map(|d| d.dut_name.clone()))
                .collect()
        })
        .unwrap_or_default();
    let host_defaults: Vec<HostDefaults> = existing
        .map(|c| {
            c.dev_hosts
                .iter()
                .map(|h| HostDefaults {
                    ip: h.ip.clone(),
                    user: h.user.clone(),
                    pass: h.pass.clone(),
                    host_name: h.host_name.clone(),
                    dut_count: h.duts.len(),
                    duts: h
                        .duts
                        .iter()
                        .map(|d| DutDefaults {
                            dut_name: Some(d.dut_name.clone()),
                            serial_port: d
                                .serial
                                .as_ref()
                                .and_then(|s| s.port.as_ref())
                                .map(NumberOrString::to_value_string),
                            login_user: d.target.as_ref().and_then(|t| t.login_user.clone()),
                            relay_gate: d.relay.is_some(),
                            relay_type: d.relay.as_ref().and_then(|r| r.relay_type.clone()),
                            relay_port: d
                                .relay
                                .as_ref()
                                .and_then(|r| r.port.map(|p| p.to_string())),
                            relay_reset_ch: d
                                .relay
                                .as_ref()
                                .and_then(|r| r.reset_ch.map(|c| c.to_string())),
                            relay_download_ch: d
                                .relay
                                .as_ref()
                                .and_then(|r| r.download_ch.map(|c| c.to_string())),
                            relay_power_ch: d
                                .relay
                                .as_ref()
                                .and_then(|r| r.power_ch.map(|c| c.to_string())),
                            relay_reset_time_ms: d
                                .relay
                                .as_ref()
                                .and_then(|r| r.reset_time_ms.map(|t| t.to_string())),
                            relay_power_off_time_ms: d
                                .relay
                                .as_ref()
                                .and_then(|r| r.power_off_time_ms.map(|t| t.to_string())),
                            advanced: dut_advanced_current(d),
                        })
                        .collect(),
                })
                .collect()
        })
        .unwrap_or_default();
    let advanced_gate = if opts.quick {
        AdvancedGate::Skip
    } else if opts.advanced {
        AdvancedGate::Forced
    } else {
        AdvancedGate::Confirm { default: false }
    };
    FormSchema {
        target: existing
            .map(|c| SchemaTarget::Update(Box::new(c.clone())))
            .unwrap_or(SchemaTarget::Create),
        host_count_default,
        host_count_max: 99,
        taken_duts,
        host_defaults,
        advanced_gate,
        advanced_current: existing.map(advanced_current).unwrap_or_default(),
    }
}

// ── Materialized instances (concrete keys/labels/defaults/options) ─────

#[derive(Debug, Clone, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub enum FieldKey {
    HostCount,
    HostIp(usize),
    HostUser(usize),
    HostPass(usize),
    HostName(usize),
    DutCount(usize),
    DutName(usize, usize),
    DutPort(usize, usize),
    DutLoginUser(usize, usize),
    DutRelayGate(usize, usize),
    DutRelayType(usize, usize),
    DutRelayPort(usize, usize),
    DutRelayReset(usize, usize),
    DutRelayDownload(usize, usize),
    DutRelayPower(usize, usize),
    DutRelayResetTimeMs(usize, usize),
    DutRelayPowerOffTimeMs(usize, usize),
    /// HOST-level advanced fallback key (global storage).
    Advanced(&'static str),
    /// DUT-owned advanced key (per-DUT storage): one dev host
    /// may serve an RK3576 AND a T527 — each DUT edits its own buffer.
    DutAdvanced(usize, usize, &'static str),
}

impl FieldKey {
    /// Re-key a per-host variant with a new host index (grid row removal
    /// renumbering — typed values follow their host row).
    pub fn shift_host_count(k: FieldKey, hi: usize) -> FieldKey {
        match k {
            FieldKey::HostIp(_) => FieldKey::HostIp(hi),
            FieldKey::HostUser(_) => FieldKey::HostUser(hi),
            FieldKey::HostPass(_) => FieldKey::HostPass(hi),
            FieldKey::HostName(_) => FieldKey::HostName(hi),
            FieldKey::DutCount(_) => FieldKey::DutCount(hi),
            other => other,
        }
    }

    /// Re-key a per-DUT variant with new host/dut indices (grid row/DUT
    /// removal renumbering).
    pub fn shift_host_dut(k: FieldKey, hi: usize, di: usize) -> FieldKey {
        match k {
            FieldKey::DutName(..) => FieldKey::DutName(hi, di),
            FieldKey::DutPort(..) => FieldKey::DutPort(hi, di),
            FieldKey::DutLoginUser(..) => FieldKey::DutLoginUser(hi, di),
            FieldKey::DutRelayGate(..) => FieldKey::DutRelayGate(hi, di),
            FieldKey::DutRelayType(..) => FieldKey::DutRelayType(hi, di),
            FieldKey::DutRelayPort(..) => FieldKey::DutRelayPort(hi, di),
            FieldKey::DutRelayReset(..) => FieldKey::DutRelayReset(hi, di),
            FieldKey::DutRelayDownload(..) => FieldKey::DutRelayDownload(hi, di),
            FieldKey::DutRelayPower(..) => FieldKey::DutRelayPower(hi, di),
            FieldKey::DutRelayResetTimeMs(..) => FieldKey::DutRelayResetTimeMs(hi, di),
            FieldKey::DutRelayPowerOffTimeMs(..) => FieldKey::DutRelayPowerOffTimeMs(hi, di),
            FieldKey::DutAdvanced(_, _, k) => FieldKey::DutAdvanced(hi, di, k),
            other => other,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FieldInstance {
    pub key: FieldKey,
    /// EXACT prompt literal (no template slots).
    pub label: String,
    pub kind: FieldKind,
    pub required: bool,
    pub empty: EmptyMeans,
    /// Fully resolved default hint (existing value > auto-name / natural
    /// default / composed reference log / count default). Some("") = no
    /// hint.
    pub default: Option<String>,
    /// Select fields only: supported_relay_types() at schema build.
    pub options: Vec<String>,
}

/// THE one materializer — the plain walker and the TUI's derive_fields
/// both consume it, so their field order is identical by construction.
pub fn materialize_group_fields(
    schema: &FormSchema,
    group: &FormGroup,
    hi: usize,
    di: usize,
) -> Vec<FieldInstance> {
    group
        .fields
        .iter()
        .map(|def| {
            let key = match def.key {
                FieldKeyFrag::HostCount => FieldKey::HostCount,
                FieldKeyFrag::HostIp => FieldKey::HostIp(hi),
                FieldKeyFrag::HostUser => FieldKey::HostUser(hi),
                FieldKeyFrag::HostPass => FieldKey::HostPass(hi),
                FieldKeyFrag::HostName => FieldKey::HostName(hi),
                FieldKeyFrag::DutCount => FieldKey::DutCount(hi),
                FieldKeyFrag::DutName => FieldKey::DutName(hi, di),
                FieldKeyFrag::DutPort => FieldKey::DutPort(hi, di),
                FieldKeyFrag::DutLoginUser => FieldKey::DutLoginUser(hi, di),
                FieldKeyFrag::DutRelayGate => FieldKey::DutRelayGate(hi, di),
                FieldKeyFrag::DutRelayType => FieldKey::DutRelayType(hi, di),
                FieldKeyFrag::DutRelayPort => FieldKey::DutRelayPort(hi, di),
                FieldKeyFrag::DutRelayReset => FieldKey::DutRelayReset(hi, di),
                FieldKeyFrag::DutRelayDownload => FieldKey::DutRelayDownload(hi, di),
                FieldKeyFrag::DutRelayPower => FieldKey::DutRelayPower(hi, di),
                FieldKeyFrag::DutRelayResetTimeMs => FieldKey::DutRelayResetTimeMs(hi, di),
                FieldKeyFrag::DutRelayPowerOffTimeMs => FieldKey::DutRelayPowerOffTimeMs(hi, di),
                FieldKeyFrag::Advanced(k) => FieldKey::Advanced(k),
                FieldKeyFrag::DutAdvanced(k) => FieldKey::DutAdvanced(hi, di, k),
            };
            let host = schema.host_defaults.get(hi);
            let dut = host.and_then(|h| h.duts.get(di));
            // Static-spec fallback first, then the runtime existing value
            // wins for the per-host/per-DUT current-value fields.
            let mut default = match (def.default.clone(), &key) {
                (DefaultSrc::RuntimeCount, FieldKey::HostCount) => {
                    Some(schema.host_count_default.to_string())
                }
                (DefaultSrc::RuntimeCount, FieldKey::DutCount(_)) => {
                    Some(host.map(|h| h.dut_count).unwrap_or(1).to_string())
                }
                (DefaultSrc::AutoDutName, FieldKey::DutName(..)) => dut
                    .and_then(|d| d.dut_name.clone())
                    .filter(|n| !n.is_empty())
                    .or_else(|| Some(next_free_dut_name("dut", &schema.taken_duts))),
                (DefaultSrc::Static(s), _) => Some(s.to_string()),
                (DefaultSrc::RuntimeRelayGate, FieldKey::DutRelayGate(..)) => {
                    Some(if dut.map(|d| d.relay_gate).unwrap_or(false) {
                        "yes".into()
                    } else {
                        "no".into()
                    })
                }
                (DefaultSrc::AdvancedNatural, FieldKey::Advanced(k)) => Some(
                    schema
                        .advanced_current
                        .get(*k)
                        .cloned()
                        .unwrap_or_else(|| advanced_default(k)),
                ),
                (DefaultSrc::ReferenceLogComposed, FieldKey::Advanced(k)) => {
                    let kk = *k;
                    Some(schema.advanced_current.get(kk).cloned().unwrap_or_else(|| {
                        let name = schema
                            .host_defaults
                            .first()
                            .and_then(|h| h.duts.first())
                            .and_then(|d| d.dut_name.as_deref())
                            .unwrap_or("dut1");
                        reference_log_default(name)
                    }))
                }
                // DUT-owned advanced: per-DUT override > global current >
                // natural/composed (that DUT's name).
                (DefaultSrc::AdvancedNatural, FieldKey::DutAdvanced(h, d, k)) => {
                    Some(dut_advanced_default(schema, *h, *d, k, None))
                }
                (DefaultSrc::ReferenceLogComposed, FieldKey::DutAdvanced(h, d, k)) => {
                    Some(dut_advanced_default(schema, *h, *d, k, None))
                }
                _ => None,
            };
            match &key {
                FieldKey::HostIp(_) if let Some(ip) = host.and_then(|h| h.ip.clone()) => {
                    default = Some(ip);
                }
                FieldKey::HostUser(_) if let Some(u) = host.and_then(|h| h.user.clone()) => {
                    default = Some(u);
                }
                FieldKey::HostPass(_) if let Some(p) = host.and_then(|h| h.pass.clone()) => {
                    default = Some(p);
                }
                FieldKey::HostName(_) if let Some(n) = host.and_then(|h| h.host_name.clone()) => {
                    default = Some(n);
                }
                FieldKey::DutPort(..) if let Some(p) = dut.and_then(|d| d.serial_port.clone()) => {
                    default = Some(p);
                }
                FieldKey::DutLoginUser(..)
                    if let Some(l) = dut.and_then(|d| d.login_user.clone()) =>
                {
                    default = Some(l);
                }
                FieldKey::DutRelayType(..)
                    if let Some(r) = dut.and_then(|d| d.relay_type.clone()) =>
                {
                    default = Some(r);
                }
                FieldKey::DutRelayPort(..)
                    if let Some(p) = dut.and_then(|d| d.relay_port.clone()) =>
                {
                    default = Some(p);
                }
                FieldKey::DutRelayReset(..)
                    if let Some(c) = dut.and_then(|d| d.relay_reset_ch.clone()) =>
                {
                    default = Some(c);
                }
                FieldKey::DutRelayDownload(..)
                    if let Some(c) = dut.and_then(|d| d.relay_download_ch.clone()) =>
                {
                    default = Some(c);
                }
                FieldKey::DutRelayPower(..)
                    if let Some(c) = dut.and_then(|d| d.relay_power_ch.clone()) =>
                {
                    default = Some(c);
                }
                FieldKey::DutRelayResetTimeMs(..)
                    if let Some(t) = dut.and_then(|d| d.relay_reset_time_ms.clone()) =>
                {
                    default = Some(t);
                }
                FieldKey::DutRelayPowerOffTimeMs(..)
                    if let Some(t) = dut.and_then(|d| d.relay_power_off_time_ms.clone()) =>
                {
                    default = Some(t);
                }
                _ => {}
            }
            FieldInstance {
                key,
                label: def.label.to_string(),
                kind: def.kind.clone(),
                required: def.required,
                empty: def.empty.clone(),
                default,
                options: if def.kind == FieldKind::Select {
                    if def.options.is_empty() {
                        supported_relay_types()
                            .into_iter()
                            .map(|s| s.to_string())
                            .collect()
                    } else {
                        def.options.iter().map(|s| s.to_string()).collect()
                    }
                } else {
                    Vec::new()
                },
            }
        })
        .collect()
}

/// Full flat field list for the schema's DEFAULT counts — a test asserts
/// this equals the legacy question sequence exactly.
pub fn schema_fields(schema: &FormSchema) -> Vec<FieldInstance> {
    let host_count = schema.host_count_default.max(1);
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
                    let dut_count = schema
                        .host_defaults
                        .get(hi)
                        .map(|h| h.dut_count)
                        .unwrap_or(1)
                        .max(1);
                    for di in 0..dut_count {
                        fields.extend(materialize_group_fields(schema, group, hi, di));
                    }
                }
            }
        }
    }
    fields
}

/// What `run_init` did — the binary prints `changes` line by line.
#[derive(Debug)]
pub struct WizardOutcome {
    /// Human diff summary (created/updated lines, per-key changes,
    /// auto-naming announcements, .mcp.json notes).
    pub changes: Vec<String>,
    /// Absolute-ish path of the `.target.jsonc` written/updated.
    pub config_path: PathBuf,
    /// true for CREATE (or --force overwrite), false for UPDATE.
    pub config_created: bool,
}

// ── Wizard answers ────────────────────────────────────────────────────

#[derive(Debug, Default, PartialEq)]
pub struct WizardAnswers {
    pub hosts: Vec<HostAnswers>,
    /// HOST-LEVEL advanced group → flat key path (e.g.
    /// "monitor.hang_timeout"). The DUT-owned keys live per DUT in
    /// `DutAnswers::advanced`.
    pub advanced: HashMap<String, String>,
    /// Naming announcements (auto-named DUTs, collision auto-advance).
    pub announcements: Vec<String>,
}

#[derive(Debug, PartialEq)]
pub struct HostAnswers {
    pub ip: String,
    pub user: String,
    pub pass: String,
    pub host_name: String,
    pub duts: Vec<DutAnswers>,
}

#[derive(Debug, PartialEq)]
pub struct DutAnswers {
    pub dut_name: String,
    pub serial_port: String,
    pub login_user: String,
    /// The relay gate answer: true => the relay sub-fields below are
    /// materialized; false => relay section omitted (end-state identical
    /// to the old "no relay" outcome).
    pub relay_gate: bool,
    pub relay_type: String,
    pub relay_port: String,
    pub relay_reset_ch: String,
    pub relay_download_ch: String,
    pub relay_power_ch: String,
    pub relay_reset_time_ms: String,
    pub relay_power_off_time_ms: String,
    /// DUT-owned advanced answers (per-DUT storage). Records
    /// non-empty typed values, cleared-field re-records (override/global),
    /// and `""` unset markers for cleared SkipInherit fields — NEVER the
    /// natural defaults (untouched fields write nothing).
    pub advanced: HashMap<String, String>,
}

// ── Occupancy diagnostics ────────────────────────────────

/// A serial endpoint the wizard is about to write that is currently held
/// by ANOTHER live MCP process (the shared host:port lock under
/// `/tmp/sermcp/locks`). READ-ONLY diagnostics: the wizard never
/// signals, stops, or otherwise disturbs the holder — the other session
/// keeps running (user requirement: never break another path's MCP; the
/// holder may be mid-task or a forgotten leftover).
#[derive(Debug, Clone, PartialEq)]
pub struct EndpointConflict {
    pub host: String,
    pub port: String,
    /// Live PID recorded in the endpoint lock file.
    pub pid: u32,
    /// Lock acquisition timestamp (RFC3339, lock-file line 4); "" when
    /// unreadable.
    pub since: String,
    /// The holder's working directory (its project), when readable.
    pub owner_project: Option<String>,
}

/// User decision at the occupancy gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusyDecision {
    /// Write the config anyway — the holder keeps its session untouched.
    Continue,
    /// Stop the wizard: resolve the holder first, then re-run. Nothing is
    /// written.
    Abort,
}

/// Scan the endpoint locks for every (dev host, serial port) pair in the
/// answers. Only the SAME host:port counts as a conflict — a different DUT
/// on a different port has its own lock and never blocks (the host:port
/// mutex IS the conflict domain).
/// Core occupancy probe shared by the save-time gate and the TUI's
/// entry-time snapshot: is this serial endpoint's lock held by a LIVE
/// process, and who owns it?
pub fn held_endpoint_conflict(host: &str, port: &str, lock_dir: &str) -> Option<EndpointConflict> {
    let lock_path = crate::lock_manager::endpoint_lock_path(host, port, lock_dir);
    let pid = crate::lock_manager::endpoint_lock_owner(&lock_path)?;
    let content = std::fs::read_to_string(&lock_path).unwrap_or_default();
    let since = content.lines().nth(3).unwrap_or_default().to_string();
    let owner_project = std::fs::read_link(format!("/proc/{pid}/cwd"))
        .ok()
        .map(|p| p.to_string_lossy().into_owned());
    Some(EndpointConflict {
        host: host.to_string(),
        port: port.to_string(),
        pid,
        since,
        owner_project,
    })
}

pub fn endpoint_conflicts(answers: &WizardAnswers, lock_dir: &str) -> Vec<EndpointConflict> {
    let mut seen: std::collections::HashSet<(&str, &str)> = std::collections::HashSet::new();
    let mut out = Vec::new();
    for host in &answers.hosts {
        for dut in &host.duts {
            if !seen.insert((host.ip.as_str(), dut.serial_port.as_str())) {
                continue;
            }
            if let Some(c) = held_endpoint_conflict(&host.ip, &dut.serial_port, lock_dir) {
                out.push(c);
            }
        }
    }
    out
}

/// One human line describing a conflict — shared verbatim by the plain
/// prompts, the TUI modal, the non-interactive warnings and the abort
/// error, so the transports can never drift.
pub fn endpoint_conflict_line(c: &EndpointConflict) -> String {
    let mut line = format!(
        "serial endpoint {}:{} is held by another MCP (PID {}",
        c.host, c.port, c.pid
    );
    if let Some(p) = &c.owner_project {
        line.push_str(&format!(", project {p}"));
    }
    if !c.since.is_empty() {
        line.push_str(&format!(", since {}", c.since));
    }
    line.push(')');
    line
}

/// Occupancy gate: before ANY write, tell the user about serial endpoints
/// held by another live MCP and let them decide. NEVER signals or stops the
/// holder — the other session keeps running. Non-interactive runs cannot
/// ask: they emit a warning note and continue (writing the config does not
/// touch the holder's lock).
fn occupancy_gate(
    io: &mut dyn PromptIO,
    opts: &InitOptions,
    answers: &WizardAnswers,
    lock_dir: &str,
    project_dir: &Path,
) -> Result<(), String> {
    let conflicts = endpoint_conflicts(answers, lock_dir);
    if conflicts.is_empty() {
        return Ok(());
    }
    // THIS project's own MCP holding the endpoint is NOT a blocker: the
    // write never touches the holder's lock or session, and the TEST probe
    // is MEANT to reach that very server. Aborting on it used to trap the
    // operator behind an orphaned holder (agent gone, idle-exit pending)
    // with no way forward but waiting out the idle timeout.
    let (own, foreign): (Vec<EndpointConflict>, Vec<EndpointConflict>) =
        conflicts.into_iter().partition(|c| {
            c.owner_project.as_deref().is_some_and(|holder| {
                std::fs::canonicalize(holder).unwrap_or_else(|_| PathBuf::from(holder))
                    == std::fs::canonicalize(project_dir)
                        .unwrap_or_else(|_| project_dir.to_path_buf())
            })
        });
    for c in &own {
        io.note(
            NoteKind::Info,
            &format!(
                "{} — this project's own MCP holds the endpoint; the config is still \
                 written, but stop that server before TEST so the form snapshot can own the endpoint",
                endpoint_conflict_line(c)
            ),
        );
    }
    if foreign.is_empty() {
        return Ok(());
    }
    if opts.non_interactive {
        for c in &foreign {
            io.note(NoteKind::Error, &endpoint_conflict_line(c));
        }
        io.note(
            NoteKind::Error,
            "serial endpoint(s) held by another project's MCP — the config is written, but tools stay rejected until the holder releases them (it is NEVER stopped automatically)",
        );
        return Ok(());
    }
    match io.busy_endpoints(&foreign)? {
        BusyDecision::Continue => Ok(()),
        BusyDecision::Abort => Err(format!(
            "aborted: {} — resolve the holder first, then re-run `dutabo init` (nothing was written)",
            foreign
                .iter()
                .map(endpoint_conflict_line)
                .collect::<Vec<_>>()
                .join("; ")
        )),
    }
}

// ── Entry point ───────────────────────────────────────────────────────

/// Everything the wizard needs BEFORE any question is asked: the resolved
/// target path, the (strictly parsed) existing config when updating, and
/// the runtime form schema. An unparseable existing file REFUSES here —
/// before the form renders — with the exact legacy error text.
#[derive(Debug, Clone)]
pub struct PreparedInit {
    pub target_path: PathBuf,
    pub existing_text: Option<String>,
    /// None for CREATE (and --force: the existing text is NOT parsed —
    /// fresh CREATE defaults instead of the UPDATE diff).
    pub existing: Option<TargetJsoncConfig>,
    pub schema: FormSchema,
    /// true for CREATE (or --force overwrite), false for UPDATE.
    pub created: bool,
}

/// Stage 1 of the pipeline: resolve the target, strict-parse any existing
/// text, and build the form schema.
pub fn prepare_init(cwd: &Path, opts: &InitOptions) -> Result<PreparedInit, String> {
    let (target_path, existing_text) = resolve_target(cwd)?;
    let created = existing_text.is_none() || opts.force;
    let existing = match (existing_text.as_deref(), opts.force) {
        (Some(text), false) => {
            let parsed = crate::config::parse_jsonc_text(text).map_err(|e| {
                format!(
                    "{} does not parse ({e}) — refusing to edit; fix the file by hand first",
                    target_path.display()
                )
            })?;
            Some(parsed)
        }
        _ => None,
    };
    let schema = build_form_schema(existing.as_ref(), opts);
    Ok(PreparedInit {
        target_path,
        existing_text,
        existing,
        schema,
        created,
    })
}

/// Stage 3 of the pipeline — the UNCHANGED-in-spirit CREATE/UPDATE
/// pipeline (answers → config/diff → render/apply → Review gate → validated
/// atomic write → .mcp.json reconcile).
pub fn finish_init(
    io: &mut dyn PromptIO,
    opts: &InitOptions,
    prepared: &PreparedInit,
    answers: &WizardAnswers,
) -> Result<WizardOutcome, String> {
    // Occupancy gate: the answers select serial endpoints
    // currently held by ANOTHER live MCP process (the shared host:port
    // lock). The user must see WHO holds WHAT and decide whether to write
    // anyway — the wizard NEVER signals or stops the holder, so another
    // path's MCP session is never disturbed. Only the SAME host:port
    // conflicts; a different DUT (different port) has its own lock and is
    // never a blocker. Runs on every write-capable path (CREATE, UPDATE,
    // and no-op UPDATE — the holder must surface even when nothing would
    // change).
    let lock_dir = prepared
        .existing
        .as_ref()
        .and_then(|c| c.lock_dir.clone())
        .unwrap_or_else(crate::lock_manager::default_lock_dir);
    let mut changes: Vec<String> = Vec::new();
    if opts.force && prepared.existing_text.is_some() {
        // --force: overwrite an existing .target.jsonc with a fresh
        // CREATE instead of the UPDATE diff.
        changes.push(format!(
            "--force: overwriting {}",
            prepared.target_path.display()
        ));
    }
    match (
        prepared.existing.as_ref(),
        prepared.existing_text.as_deref(),
    ) {
        // CREATE (no existing text, or --force rerouting to fresh CREATE;
        // (Some, None) is unreachable by prepare_init's construction).
        (None, _) | (Some(_), None) => {
            let missing = missing_required(answers);
            if !missing.is_empty() {
                return Err(format!(
                    "non-interactive init requires: {}",
                    missing.join(", ")
                ));
            }
            let config = build_config_from_answers(answers)?;
            let text = render_jsonc(&config, true);
            // Defensive: the renderer's output must always parse (the created
            // file becomes the next update's input).
            crate::config::parse_jsonc_text(&text)
                .map_err(|e| format!("internal: generated config does not parse ({e})"))?;
            // Occupancy gate BEFORE the Review: an endpoint held by another
            // live MCP is surfaced and the user decides (continue/abort).
            occupancy_gate(
                io,
                opts,
                answers,
                &lock_dir,
                prepared.target_path.parent().unwrap_or(Path::new(".")),
            )?;
            // Review gate: all computation is done; nothing is on disk yet.
            // Err here = user abort with zero bytes written.
            let mut pending: Vec<String> = changes.clone();
            pending.push(format!("created {}", prepared.target_path.display()));
            pending.extend(answers.announcements.iter().cloned());
            io.finish(&prepared.target_path, &pending)?;
            write_validated(&prepared.target_path, &text)?;
            changes.push(format!("created {}", prepared.target_path.display()));
            changes.extend(answers.announcements.clone());
        }
        // UPDATE: diff ops → apply (FAIL FAST before the Review gate) →
        // Review → write.
        (Some(existing), Some(existing_text)) => {
            let missing = missing_required(answers);
            if !missing.is_empty() {
                return Err(format!(
                    "non-interactive init requires: {}",
                    missing.join(", ")
                ));
            }
            // Occupancy gate BEFORE the write decision — it also covers
            // no-op updates (nothing to write): the user must learn about
            // a holder whenever init runs against occupied endpoints, even
            // if the config would not change.
            occupancy_gate(
                io,
                opts,
                answers,
                &lock_dir,
                prepared.target_path.parent().unwrap_or(Path::new(".")),
            )?;
            let ops = compute_update_ops(existing_text, existing, answers)?;
            let mut pending: Vec<String> = changes.clone();
            if ops.is_empty() {
                pending.push(format!("{} unchanged", prepared.target_path.display()));
                pending.extend(answers.announcements.iter().cloned());
                io.finish(&prepared.target_path, &pending)?;
                changes.push(format!("{} unchanged", prepared.target_path.display()));
                changes.extend(answers.announcements.clone());
            } else {
                // Apply (pure compute + per-op re-parse) BEFORE the Review
                // gate so a revert-level failure can never surface after
                // the user approved.
                let new_text = apply_edits(
                    existing_text,
                    &ops.iter().map(|o| o.op.clone()).collect::<Vec<_>>(),
                )?;
                pending.push(format!("updated {}", prepared.target_path.display()));
                pending.extend(ops.iter().map(|o| o.desc.clone()));
                pending.extend(answers.announcements.iter().cloned());
                io.finish(&prepared.target_path, &pending)?;
                write_validated(&prepared.target_path, &new_text)?;
                changes.push(format!("updated {}", prepared.target_path.display()));
                changes.extend(ops.iter().map(|o| o.desc.clone()));
                changes.extend(answers.announcements.clone());
            }
        }
    }
    finish_mcp_json(&prepared.target_path, &mut changes);
    Ok(WizardOutcome {
        changes,
        config_path: prepared.target_path.clone(),
        config_created: prepared.created,
    })
}

/// Render the configuration represented by the current wizard answers without
/// writing it. The TEST button uses this snapshot so every probe observes the
/// edited form values, including unsaved host, serial and relay fields.
pub fn render_test_config(
    prepared: &PreparedInit,
    answers: &WizardAnswers,
) -> Result<String, String> {
    let missing = missing_required(answers);
    if !missing.is_empty() {
        return Err(format!("test requires: {}", missing.join(", ")));
    }
    let text = match (
        prepared.existing.as_ref(),
        prepared.existing_text.as_deref(),
    ) {
        (None, _) | (Some(_), None) => render_jsonc(&build_config_from_answers(answers)?, true),
        (Some(existing), Some(existing_text)) => {
            let ops = compute_update_ops(existing_text, existing, answers)?;
            if ops.is_empty() {
                existing_text.to_string()
            } else {
                apply_edits(
                    existing_text,
                    &ops.into_iter().map(|op| op.op).collect::<Vec<_>>(),
                )?
            }
        }
    };
    crate::config::parse_jsonc_text(&text)
        .map_err(|e| format!("internal: generated TEST config does not parse ({e})"))?;
    Ok(text)
}

/// Run the wizard on the PLAIN transport: prepare → (fill defaults when
/// headless | sequential schema walk) → finish. `cwd` is the base for
/// walk-up discovery (the binary passes `std::env::current_dir()`).
pub fn run_init(
    io: &mut dyn PromptIO,
    opts: &InitOptions,
    cwd: &Path,
) -> Result<WizardOutcome, String> {
    let mut prepared = prepare_init(cwd, opts)?;
    let answers = if opts.non_interactive {
        fill_defaults(&prepared.schema)
    } else {
        walk_schema(&mut prepared.schema, io)?
    };
    finish_init(io, opts, &prepared, &answers)
}

/// Resolve the target config path: TARGET_CONF env (explicit instruction —
/// create there if missing), else walk-up `.target.jsonc` (UPDATE), else
/// CREATE at `cwd`.
fn resolve_target(cwd: &Path) -> Result<(PathBuf, Option<String>), String> {
    if let Ok(env) = std::env::var("TARGET_CONF") {
        let p = PathBuf::from(&env);
        let existing = std::fs::read_to_string(&p).ok();
        return Ok((p, existing));
    }
    if let Some(p) = walk_up(cwd, CONFIG_FILE_NAME) {
        let existing =
            std::fs::read_to_string(&p).map_err(|e| format!("Cannot read {}: {e}", p.display()))?;
        return Ok((p, Some(existing)));
    }
    Ok((cwd.join(CONFIG_FILE_NAME), None))
}

/// Walk up from `dir` looking for `file_name`.
fn walk_up(dir: &Path, file_name: &str) -> Option<PathBuf> {
    let mut d = dir.to_path_buf();
    loop {
        let candidate = d.join(file_name);
        if candidate.exists() {
            return Some(candidate);
        }
        if !d.pop() {
            break;
        }
    }
    None
}

/// Reconcile `.mcp.json` after a wizard write (both CREATE and UPDATE paths
/// land here via `run_init`): create the canonical file when missing, MERGE
/// the `sermcp` entry into an existing file that lacks it (all other
/// servers/keys preserved verbatim), and leave a file that already has the
/// entry untouched.
fn finish_mcp_json(target_path: &Path, changes: &mut Vec<String>) {
    let project_dir = target_path.parent().unwrap_or(Path::new("."));
    match ensure_mcp_json(project_dir, target_path) {
        Ok(McpJsonAction::Created) => {
            changes.push("generated .mcp.json (TARGET_CONF -> .target.jsonc)".into())
        }
        Ok(McpJsonAction::Merged) => {
            changes.push("added sermcp entry to .mcp.json (TARGET_CONF -> .target.jsonc)".into())
        }
        Ok(McpJsonAction::Untouched) => {
            changes.push(".mcp.json already has a sermcp entry — left unchanged".into())
        }
        Err(e) => changes.push(format!("warning: {e}")),
    }
}

// ── Question flow ─────────────────────────────────────────────────────

/// The PLAIN transport's sequential walk: depth-first over FORM_GROUPS,
/// materializing each group instance with `materialize_group_fields` — the
/// SAME materializer the form TUI consumes — and dispatching every field
/// to the policy functions below by its FieldKey. Prompt literals,
/// auto-name prefixes, required-ness and shown defaults all come from the
/// materialized FieldInstance, so the plain prompts can never drift from
/// the form's schema (pinned by test_walk_prompts_are_form_group_labels).
/// Prompts and notes stay byte-identical to the legacy question flow (the
/// CannedIO fixtures stay green with mechanical-only edits).
/// `schema.taken_*` mutate in ask order — the legacy order-dependence is
/// preserved exactly.
pub fn walk_schema(
    schema: &mut FormSchema,
    io: &mut dyn PromptIO,
) -> Result<WizardAnswers, String> {
    let mut answers = WizardAnswers::default();
    // Group 0 ("host-count", Once): the host-count question.
    let host_count = {
        let fields = materialize_group_fields(schema, &FORM_GROUPS[0], 0, 0);
        ask_count(io, &fields[0].label, schema.host_count_default)?
    };
    for hi in 0..host_count {
        // Group 1 ("dev-host", PerHost): ip/user/pass, then the per-host
        // DUT count.
        let host_fields = materialize_group_fields(schema, &FORM_GROUPS[1], hi, 0);
        let mut ip = String::new();
        let mut user = String::new();
        let mut pass = String::new();
        let mut host_name = String::new();
        let mut dut_count = 1usize;
        for field in &host_fields {
            match &field.key {
                FieldKey::HostIp(_) => ip = ask_required(io, &field.label, field.default.clone())?,
                FieldKey::HostUser(_) => {
                    user = ask_optional(io, &field.label, field.default.clone())?
                }
                FieldKey::HostPass(_) => {
                    pass = ask_optional(io, &field.label, field.default.clone())?
                }
                FieldKey::HostName(_) => {
                    host_name = ask_optional(io, &field.label, field.default.clone())?
                }
                FieldKey::DutCount(_) => {
                    let default = field
                        .default
                        .as_deref()
                        .and_then(|s| s.parse::<usize>().ok())
                        .unwrap_or(1);
                    dut_count = ask_count(io, &field.label, default)?;
                }
                _ => {}
            }
        }
        let mut duts = Vec::new();
        for di in 0..dut_count {
            // Group 2 ("dut", PerHostDut), materialized per instance.
            let dut_fields = materialize_group_fields(schema, &FORM_GROUPS[2], hi, di);
            let mut dut_name = String::new();
            let mut port = String::new();
            let mut login_user = String::new();
            // The relay gate defaults from the existing config's DUT relay
            // section (update) or NO (create).
            let mut relay_gate = false;
            let mut relay_type = String::new();
            let mut relay_port = String::new();
            let mut relay_reset_ch = String::new();
            let mut relay_download_ch = String::new();
            let mut relay_power_ch = String::new();
            let mut relay_reset_time_ms = String::new();
            let mut relay_power_off_time_ms = String::new();
            for field in &dut_fields {
                match &field.key {
                    FieldKey::DutName(..) => {
                        let (a, note) = ask_dut_name(
                            io,
                            &field.label,
                            "dut",
                            field.default.clone(),
                            &mut schema.taken_duts,
                        )?;
                        if let Some(note) = note {
                            answers.announcements.push(note);
                        }
                        dut_name = a;
                    }
                    FieldKey::DutPort(..) => {
                        // Required; "0" is the validation sentinel and counts
                        // as missing (never auto-defaulted — see
                        // missing_required).
                        let mut p = ask_required(io, &field.label, field.default.clone())?;
                        while p.trim() == "0" {
                            io.note(NoteKind::Error, "serial.port cannot be 0");
                            p = ask_required(io, "Serial port must be non-zero", None)?;
                        }
                        port = p;
                    }
                    FieldKey::DutLoginUser(..) => {
                        login_user = ask_optional(io, &field.label, field.default.clone())?
                    }
                    FieldKey::DutRelayGate(..) => {
                        // The toggle is asked FIRST — before any relay
                        // question — and NO skips the relay sub-fields.
                        relay_gate = io.confirm(
                            &format!(
                                "Use relay for DUT '{dut_name}' (type/channels/reset control)?"
                            ),
                            field.default.as_deref().is_some_and(|d| d == "yes"),
                        )?;
                    }
                    FieldKey::DutRelayType(..) if relay_gate => {
                        relay_type = ask_optional(io, &field.label, field.default.clone())?;
                    }
                    FieldKey::DutRelayPort(..) if relay_gate => {
                        relay_port = ask_relay_field(io, field, "relay.port")?;
                    }
                    FieldKey::DutRelayReset(..) if relay_gate => {
                        relay_reset_ch = ask_relay_field(io, field, "relay.reset_ch")?;
                    }
                    FieldKey::DutRelayDownload(..) if relay_gate => {
                        relay_download_ch = ask_relay_field(io, field, "relay.download_ch")?;
                    }
                    FieldKey::DutRelayPower(..) if relay_gate => {
                        relay_power_ch = ask_relay_field(io, field, "relay.power_ch")?;
                    }
                    FieldKey::DutRelayResetTimeMs(..) if relay_gate => {
                        relay_reset_time_ms = ask_relay_field(io, field, "relay.reset_time_ms")?;
                    }
                    FieldKey::DutRelayPowerOffTimeMs(..) if relay_gate => {
                        relay_power_off_time_ms =
                            ask_relay_field(io, field, "relay.power_off_time_ms")?;
                    }
                    _ => {}
                }
            }
            // DUT-owned advanced: asked per DUT in EVERY mode (the advanced
            // gate only governs the HOST-level fallback set below; the
            // natural hints are informational — bare Enter on a field with
            // no per-DUT override / global value records NOTHING, so
            // untouched flows stay byte-stable.
            let mut dut_advanced = HashMap::new();
            let dut_adv_fields = materialize_group_fields(schema, &FORM_GROUPS[3], hi, di);
            for field in &dut_adv_fields {
                let FieldKey::DutAdvanced(_, _, k) = &field.key else {
                    continue;
                };
                if *k == "uboot.pattern" {
                    let strategy = dut_advanced
                        .get("uboot.interrupt_strategy")
                        .cloned()
                        .or_else(|| {
                            dut_advanced_current_value(schema, hi, di, "uboot.interrupt_strategy")
                        })
                        .unwrap_or_else(|| advanced_default("uboot.interrupt_strategy"));
                    if strategy != "pattern" {
                        continue;
                    }
                }
                // Shown hint: override > global > natural/composed. Bare
                // Enter ACCEPTS only the effective current value (override
                // > global) — the natural default is an informational hint
                // and is never recorded (byte-stability).
                let hint = dut_advanced_default(schema, hi, di, k, Some(&dut_name));
                let effective = dut_advanced_current_value(schema, hi, di, k);
                let value = loop {
                    let raw = io.input(&field.label, Some(&hint))?;
                    let trimmed = raw.trim();
                    if trimmed.is_empty() {
                        if *k == "serial.baudrate" {
                            break hint.clone();
                        }
                        if *k == "uboot.pattern" {
                            io.note(
                                NoteKind::Error,
                                "uboot.pattern is required for pattern strategy",
                            );
                            continue;
                        }
                        if field.empty == EmptyMeans::AcceptDefault
                            && let Some(eff) = effective.as_deref()
                            && !eff.is_empty()
                        {
                            break eff.to_string();
                        }
                        break String::new(); // skip/inherit
                    }
                    match advanced_literal(k, trimmed) {
                        Ok(_) => break trimmed.to_string(),
                        Err(e) => {
                            io.note(NoteKind::Error, &e);
                            // re-ask the SAME key
                        }
                    }
                };
                let value = value.trim().to_string();
                if !value.is_empty() {
                    dut_advanced.insert(k.to_string(), value);
                }
            }
            duts.push(DutAnswers {
                dut_name,
                serial_port: port,
                login_user,
                relay_gate,
                relay_type,
                relay_port,
                relay_reset_ch,
                relay_download_ch,
                relay_power_ch,
                relay_reset_time_ms,
                relay_power_off_time_ms,
                advanced: dut_advanced,
            });
        }
        answers.hosts.push(HostAnswers {
            ip,
            user,
            pass,
            host_name,
            duts,
        });
    }

    // HOST-LEVEL advanced group: global fallback sections (one answer set,
    // not per DUT). The DUT-owned keys were asked per DUT above.
    let ask_advanced = match schema.advanced_gate {
        AdvancedGate::Skip => false,
        AdvancedGate::Forced => true,
        AdvancedGate::Confirm { default } => {
            io.confirm("Configure advanced options (global fallbacks)?", default)?
        }
    };
    if ask_advanced {
        for spec in ADVANCED_FIELDS
            .iter()
            .filter(|s| !dut_owned_advanced(s.key))
        {
            let default = schema
                .advanced_current
                .get(spec.key)
                .cloned()
                .unwrap_or_else(|| advanced_default(spec.key));
            // Empty input on an AcceptDefault key ACCEPTS the shown default
            // (which is the CURRENT value on update → identical-value skip →
            // byte-identical file; the natural default on create /
            // absent-on-update → WRITTEN, reqs 9/10). SkipInherit keys keep
            // the skip/inherit semantics. Typed values are validated NOW
            // (key-named error note + re-ask) instead of at the write gate.
            let value = ask_advanced_value(io, spec.label, &spec.empty, spec.key, &default)?;
            let value = value.trim().to_string();
            if !value.is_empty() {
                answers.advanced.insert(spec.key.to_string(), value);
            }
        }
    }
    Ok(answers)
}

/// The headless answer producer: every field resolves to its default
/// (counts max(1), names default-or-auto-name with taken-set growth but
/// NO announcements, optional fields their defaults, required fields the
/// possibly-empty default — `missing_required` collects those), and the
/// advanced group records ONLY current values (a strict no-op on update).
pub fn fill_defaults(schema: &FormSchema) -> WizardAnswers {
    let mut answers = WizardAnswers::default();
    let mut taken_duts = schema.taken_duts.clone();
    let host_count = schema.host_count_default.max(1);
    for hi in 0..host_count {
        let host_default = schema.host_defaults.get(hi).cloned().unwrap_or_default();
        let dut_count = host_default.dut_count.max(1);
        let mut duts = Vec::new();
        for di in 0..dut_count {
            let dut_default = host_default.duts.get(di).cloned().unwrap_or_default();
            let dut_name = match dut_default.dut_name.clone().filter(|n| !n.is_empty()) {
                Some(n) => n,
                None => {
                    let name = next_free_dut_name("dut", &taken_duts);
                    if !taken_duts.iter().any(|t| t == &name) {
                        taken_duts.push(name.clone());
                    }
                    name
                }
            };
            if !taken_duts.iter().any(|t| t == &dut_name) {
                taken_duts.push(dut_name.clone());
            }
            // Headless relay: gate = the existing relay section (update) or
            // NO (create); gate on → current values (update) or defaults.
            let relay_gate = dut_default.relay_gate;
            duts.push(DutAnswers {
                dut_name,
                serial_port: dut_default.serial_port.unwrap_or_default(),
                login_user: dut_default.login_user.unwrap_or_else(|| "root".into()),
                relay_gate,
                relay_type: if relay_gate {
                    dut_default.relay_type.unwrap_or_else(|| "ch340".into())
                } else {
                    String::new()
                },
                relay_port: if relay_gate {
                    dut_default.relay_port.unwrap_or_default()
                } else {
                    String::new()
                },
                relay_reset_ch: if relay_gate {
                    dut_default.relay_reset_ch.unwrap_or_default()
                } else {
                    String::new()
                },
                relay_download_ch: if relay_gate {
                    dut_default.relay_download_ch.unwrap_or_default()
                } else {
                    String::new()
                },
                relay_power_ch: if relay_gate {
                    dut_default.relay_power_ch.unwrap_or_default()
                } else {
                    String::new()
                },
                relay_reset_time_ms: if relay_gate {
                    dut_default.relay_reset_time_ms.unwrap_or_default()
                } else {
                    String::new()
                },
                relay_power_off_time_ms: if relay_gate {
                    dut_default
                        .relay_power_off_time_ms
                        .unwrap_or_else(|| "3000".into())
                } else {
                    String::new()
                },
                // Headless per-DUT advanced: record ONLY this DUT's
                // existing overrides (a strict no-op on update — never the
                // natural defaults; gate-independent).
                advanced: {
                    let mut advanced = dut_default.advanced.clone();
                    advanced
                        .entry("serial.baudrate".into())
                        .or_insert_with(|| advanced_default("serial.baudrate"));
                    advanced
                },
            });
        }
        answers.hosts.push(HostAnswers {
            ip: host_default.ip.unwrap_or_default(),
            user: host_default.user.unwrap_or_default(),
            pass: host_default.pass.unwrap_or_default(),
            host_name: host_default.host_name.unwrap_or_default(),
            duts,
        });
    }
    // Headless update records only CURRENT values (which diff to no-ops).
    if schema.advanced_gate == AdvancedGate::Forced {
        for spec in ADVANCED_FIELDS {
            if let Some(cur) = schema.advanced_current.get(spec.key)
                && !cur.is_empty()
            {
                answers.advanced.insert(spec.key.to_string(), cur.clone());
            }
        }
    }
    answers
}

/// Collision policy for DUT-name resolution. Plain walk = AutoAdvance (ask
/// order, announcements); form = ErrorOnCollision (inline per-field
/// errors, order-independent). The divergence is deliberate and
/// documented: one resolver, two renderings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamePolicy {
    AutoAdvance,
    ErrorOnCollision,
}

/// Parse a count field's buffer (empty → default; invalid → the exact
/// ask_count error string). 0 is a VALID count: zero dev hosts means the
/// DUT section disappears entirely and the written config carries no DUTs.
fn count_value(raw: Option<&String>, default: usize) -> Result<usize, String> {
    match raw {
        None => Ok(default),
        Some(s) => {
            let t = s.trim();
            if t.is_empty() {
                return Ok(default);
            }
            match t.parse::<usize>() {
                Ok(n) => Ok(n),
                _ => Err(format!("'{t}' is not a non-negative integer")),
            }
        }
    }
}

/// AUTHORITATIVE form-submit validation: buffers → WizardAnswers (or a
/// per-field error list). All rules mirror the plain walk's policy
/// functions one-for-one — the TUI never reimplements them. Empty buffers
/// with a non-empty default silently accept that default (the fixed
/// ask_required contract); required fields with an empty default error.
pub fn resolve_answers(
    schema: &FormSchema,
    values: &BTreeMap<FieldKey, String>,
    gate_enabled: bool,
    policy: NamePolicy,
) -> Result<WizardAnswers, Vec<(FieldKey, String)>> {
    let mut errors: Vec<(FieldKey, String)> = Vec::new();
    let mut answers = WizardAnswers::default();

    let host_count = match count_value(values.get(&FieldKey::HostCount), schema.host_count_default)
    {
        Ok(n) => n,
        Err(e) => {
            errors.push((FieldKey::HostCount, e));
            schema.host_count_default
        }
    };

    // Collision universe: existing names + every typed name value (the
    // whole-values-map view — order-independent, per the form contract).
    let mut dut_taken: Vec<String> = schema.taken_duts.clone();
    for (k, v) in values {
        let t = v.trim();
        if t.is_empty() {
            continue;
        }
        if let FieldKey::DutName(..) = k {
            dut_taken.push(t.to_string());
        }
    }
    let dut_name_occurrences: HashMap<String, usize> = values
        .iter()
        .filter(|(k, _)| matches!(k, FieldKey::DutName(..)))
        .map(|(_, v)| v.trim().to_string())
        .fold(HashMap::new(), |mut m, v| {
            *m.entry(v).or_insert(0) += 1;
            m
        });

    for hi in 0..host_count {
        let host_default = schema.host_defaults.get(hi).cloned().unwrap_or_default();
        let ip = required_text(
            FieldKey::HostIp(hi),
            values.get(&FieldKey::HostIp(hi)),
            host_default.ip.clone(),
            &mut errors,
        );
        let user = optional_text(
            values.get(&FieldKey::HostUser(hi)),
            host_default.user.clone(),
        );
        let pass = optional_text(
            values.get(&FieldKey::HostPass(hi)),
            host_default.pass.clone(),
        );
        let host_name = optional_text(
            values.get(&FieldKey::HostName(hi)),
            host_default.host_name.clone(),
        );
        // Empty DUT-count buffer accepts the default (1 on CREATE, the
        // existing count on UPDATE); a typed 0 is honored (host with no
        // DUTs — the grid model).
        let dut_count = match count_value(
            values.get(&FieldKey::DutCount(hi)),
            host_default.dut_count.max(1),
        ) {
            Ok(n) => n,
            Err(e) => {
                errors.push((FieldKey::DutCount(hi), e));
                host_default.dut_count.max(1)
            }
        };
        let mut duts = Vec::new();
        for di in 0..dut_count {
            let dut_default = host_default.duts.get(di).cloned().unwrap_or_default();
            let dut_name = resolve_dut_name(
                FieldKey::DutName(hi, di),
                "dut",
                dut_default.dut_name.as_deref(),
                values,
                &dut_name_occurrences,
                &schema.taken_duts,
                &mut dut_taken,
                policy,
                &mut errors,
                &mut answers.announcements,
            );
            let port = match values.get(&FieldKey::DutPort(hi, di)).map(|s| s.trim()) {
                None | Some("") => match dut_default.serial_port.as_deref() {
                    Some(d) if !d.is_empty() => d.to_string(),
                    _ => {
                        errors.push((FieldKey::DutPort(hi, di), "value is required".into()));
                        String::new()
                    }
                },
                Some("0") => {
                    errors.push((FieldKey::DutPort(hi, di), "serial.port cannot be 0".into()));
                    "0".to_string()
                }
                Some(v) => v.to_string(),
            };
            let login_user = optional_text(
                values.get(&FieldKey::DutLoginUser(hi, di)),
                dut_default
                    .login_user
                    .clone()
                    .or_else(|| Some("root".into())),
            );
            // The relay gate: typed yes/no wins; empty → the existing
            // relay-section presence (update) or NO (create). The relay
            // sub-fields are ONLY resolved when the gate is on — off means
            // "relay omitted" in the end state.
            let relay_gate = match values
                .get(&FieldKey::DutRelayGate(hi, di))
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
            {
                Some(v) => match v.to_ascii_lowercase().as_str() {
                    "yes" | "true" | "1" | "on" => true,
                    "no" | "false" | "0" | "off" => false,
                    other => {
                        errors.push((
                            FieldKey::DutRelayGate(hi, di),
                            format!("'{other}' is not yes/no"),
                        ));
                        dut_default.relay_gate
                    }
                },
                None => dut_default.relay_gate,
            };
            let mut relay_type = String::new();
            let mut relay_port = String::new();
            let mut relay_reset_ch = String::new();
            let mut relay_download_ch = String::new();
            let mut relay_power_ch = String::new();
            let mut relay_reset_time_ms = String::new();
            let mut relay_power_off_time_ms = String::new();
            if relay_gate {
                relay_type = optional_text(
                    values.get(&FieldKey::DutRelayType(hi, di)),
                    dut_default
                        .relay_type
                        .clone()
                        .or_else(|| Some("ch340".into())),
                );
                // Zero-guarded numeric family: empty = inherit (SkipInherit)
                // or the shown default (AcceptDefault); typed values are
                // validated NOW with the same error strings as the walk.
                let mut resolve_relay_field =
                    |key: FieldKey, advanced_key: &str, empty_default: Option<String>| -> String {
                        match values.get(&key).map(|s| s.trim()) {
                            None | Some("") => empty_default.unwrap_or_default(),
                            Some(v) => match advanced_literal(advanced_key, v) {
                                Ok(_) => v.to_string(),
                                Err(e) => {
                                    errors.push((key, e));
                                    String::new()
                                }
                            },
                        }
                    };
                relay_port =
                    resolve_relay_field(FieldKey::DutRelayPort(hi, di), "relay.port", None);
                relay_reset_ch =
                    resolve_relay_field(FieldKey::DutRelayReset(hi, di), "relay.reset_ch", None);
                relay_download_ch = resolve_relay_field(
                    FieldKey::DutRelayDownload(hi, di),
                    "relay.download_ch",
                    None,
                );
                relay_power_ch =
                    resolve_relay_field(FieldKey::DutRelayPower(hi, di), "relay.power_ch", None);
                relay_reset_time_ms = resolve_relay_field(
                    FieldKey::DutRelayResetTimeMs(hi, di),
                    "relay.reset_time_ms",
                    dut_default
                        .relay_reset_time_ms
                        .clone()
                        .or_else(|| Some("3000".into())),
                );
                relay_power_off_time_ms = resolve_relay_field(
                    FieldKey::DutRelayPowerOffTimeMs(hi, di),
                    "relay.power_off_time_ms",
                    dut_default
                        .relay_power_off_time_ms
                        .clone()
                        .or_else(|| Some("3000".into())),
                );
            }
            // DUT-owned advanced: resolved UNCONDITIONALLY — the advanced
            // gate only governs the HOST-level set, and the DUT-owned
            // fields are always visible/editable in the DUT box (            // GATE BUG fix: typed values must survive an F2 while
            // collapsed). An ABSENT buffer records nothing (untouched —
            // byte-stability); a cleared buffer AcceptDefaults the
            // effective value (per-DUT override > global) or marks the key
            // unset (SkipInherit); the natural defaults are hints only.
            let mut dut_advanced = HashMap::new();
            for def in DUT_ADVANCED_GROUP_FIELDS {
                let FieldKeyFrag::DutAdvanced(k) = def.key else {
                    unreachable!("dut-advanced group carries only DutAdvanced keys");
                };
                let key = FieldKey::DutAdvanced(hi, di, k);
                match values.get(&key).map(|s| s.as_str()) {
                    None if k == "serial.baudrate" => {
                        dut_advanced.insert(k.into(), advanced_default(k));
                    }
                    None => {} // untouched buffer → nothing recorded
                    Some(raw) => {
                        let trimmed = raw.trim();
                        if trimmed.is_empty() {
                            if def.empty == EmptyMeans::AcceptDefault {
                                // Cleared + AcceptDefault → re-record the
                                // effective value (per-DUT override > global;
                                // the natural default is a hint only).
                                let default = dut_advanced_current_value(schema, hi, di, k);
                                if let Some(d) = default.filter(|d| !d.is_empty()) {
                                    dut_advanced.insert(k.to_string(), d);
                                } else if k == "serial.baudrate" {
                                    dut_advanced.insert(k.into(), advanced_default(k));
                                }
                            } else {
                                // Cleared SkipInherit → explicit unset
                                // (inherits the global fallback; an
                                // existing override is removed at write).
                                dut_advanced.insert(k.to_string(), String::new());
                            }
                        } else {
                            match advanced_literal(k, trimmed) {
                                Ok(_) => {
                                    dut_advanced.insert(k.to_string(), trimmed.to_string());
                                }
                                Err(e) => errors.push((key, e)),
                            }
                        }
                    }
                }
            }
            let interrupt_strategy = dut_advanced
                .get("uboot.interrupt_strategy")
                .filter(|value| !value.trim().is_empty())
                .cloned()
                .or_else(|| dut_advanced_current_value(schema, hi, di, "uboot.interrupt_strategy"))
                .unwrap_or_else(|| advanced_default("uboot.interrupt_strategy"));
            if interrupt_strategy == "pattern" {
                let pattern = dut_advanced
                    .get("uboot.pattern")
                    .filter(|value| !value.trim().is_empty())
                    .cloned()
                    .or_else(|| dut_advanced_current_value(schema, hi, di, "uboot.pattern"));
                if pattern.is_none() {
                    errors.push((
                        FieldKey::DutAdvanced(hi, di, "uboot.pattern"),
                        "value is required when interrupt strategy is pattern".into(),
                    ));
                }
            }
            duts.push(DutAnswers {
                dut_name,
                serial_port: port,
                login_user,
                relay_gate,
                relay_type,
                relay_port,
                relay_reset_ch,
                relay_download_ch,
                relay_power_ch,
                relay_reset_time_ms,
                relay_power_off_time_ms,
                advanced: dut_advanced,
            });
        }
        answers.hosts.push(HostAnswers {
            ip,
            user,
            pass,
            host_name,
            duts,
        });
    }

    if gate_enabled {
        // HOST-LEVEL advanced only — the DUT-owned keys were resolved per
        // DUT above (gate-independently).
        for spec in ADVANCED_FIELDS
            .iter()
            .filter(|s| !dut_owned_advanced(s.key))
        {
            let key = FieldKey::Advanced(spec.key);
            let raw = values.get(&key).map(|s| s.as_str()).unwrap_or("");
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                if spec.empty == EmptyMeans::AcceptDefault {
                    let default = schema
                        .advanced_current
                        .get(spec.key)
                        .cloned()
                        .unwrap_or_else(|| advanced_default(spec.key));
                    if !default.is_empty() {
                        answers.advanced.insert(spec.key.to_string(), default);
                    }
                }
                continue;
            }
            match advanced_literal(spec.key, trimmed) {
                Ok(_) => {
                    answers
                        .advanced
                        .insert(spec.key.to_string(), trimmed.to_string());
                }
                Err(e) => errors.push((key, e)),
            }
        }
    }

    if errors.is_empty() {
        Ok(answers)
    } else {
        Err(errors)
    }
}

/// One name field: empty → default (existing) or auto-name (announced);
/// colliding → policy (ErrorOnCollision errors BOTH fields of a collision
/// pair; AutoAdvance announces). `taken` grows as auto-names are assigned.
#[allow(clippy::too_many_arguments)]
fn resolve_dut_name(
    key: FieldKey,
    prefix: &str,
    default: Option<&str>,
    values: &BTreeMap<FieldKey, String>,
    occurrences: &HashMap<String, usize>,
    schema_taken: &[String],
    taken: &mut Vec<String>,
    policy: NamePolicy,
    errors: &mut Vec<(FieldKey, String)>,
    announcements: &mut Vec<String>,
) -> String {
    let trimmed = values.get(&key).map(|s| s.trim()).unwrap_or("");
    if trimmed.is_empty() {
        return match default.filter(|d| !d.is_empty()) {
            Some(def) => {
                if !taken.iter().any(|t| t == def) {
                    taken.push(def.to_string());
                }
                def.to_string()
            }
            None => {
                let name = next_free_dut_name(prefix, taken);
                taken.push(name.clone());
                let announcement = format!("dut name was empty — auto-named '{name}'");
                announcements.push(announcement);
                name
            }
        };
    }
    // The field's OWN existing name (the shown prefill) is exempt from the
    // taken-set check — legacy ask_dut_name keeps a default that is already in
    // `taken`; only a DIFFERENT existing name or a duplicated typed value
    // collides.
    let collides = occurrences.get(trimmed).copied().unwrap_or(0) > 1
        || (default != Some(trimmed) && schema_taken.iter().any(|t| t == trimmed));
    if collides {
        match policy {
            NamePolicy::AutoAdvance => {
                let name = next_free_dut_name(prefix, taken);
                taken.push(name.clone());
                let announcement = format!("dut name '{trimmed}' already taken — using '{name}'");
                announcements.push(announcement);
                name
            }
            NamePolicy::ErrorOnCollision => {
                let auto = next_free_dut_name(prefix, taken);
                errors.push((
                    key,
                    format!(
                        "dut name '{trimmed}' already taken — pick another or leave empty to auto-name '{auto}'"
                    ),
                ));
                trimmed.to_string()
            }
        }
    } else {
        taken.push(trimmed.to_string());
        trimmed.to_string()
    }
}

/// Required text: empty buffer with a non-empty default silently accepts
/// the default (the fixed ask_required); an empty default errors.
fn required_text(
    key: FieldKey,
    raw: Option<&String>,
    default: Option<String>,
    errors: &mut Vec<(FieldKey, String)>,
) -> String {
    match raw.map(|s| s.trim()).unwrap_or("") {
        "" => match default.as_deref() {
            Some(d) if !d.is_empty() => d.to_string(),
            _ => {
                errors.push((key, "value is required".into()));
                String::new()
            }
        },
        v => v.to_string(),
    }
}

/// Optional text: empty buffer → default (which may be "").
fn optional_text(raw: Option<&String>, default: Option<String>) -> String {
    match raw.map(|s| s.trim()).unwrap_or("") {
        "" => default.unwrap_or_default(),
        v => v.to_string(),
    }
}

/// Count question: 0..=N. Bare Enter accepts the shown default and
/// proceeds IMMEDIATELY (P0 — the old loop silently re-asked the same
/// screen when the transport returned "" for an empty submit); invalid
/// non-empty input gets an error note + re-ask. The headless default path
/// lives in `fill_defaults`.
fn ask_count(io: &mut dyn PromptIO, prompt: &str, default: usize) -> Result<usize, String> {
    let def = default.to_string();
    loop {
        let raw = io.input(prompt, Some(&def))?;
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Ok(default);
        }
        if let Ok(n) = trimmed.parse::<usize>() {
            return Ok(n);
        }
        io.note(
            NoteKind::Error,
            &format!("'{trimmed}' is not a non-negative integer"),
        );
        // invalid → re-ask (Ctrl-C/EOF aborts via io)
    }
}

/// DUT-name question: never empty, never colliding. Empty input →
/// auto-name; input colliding with an existing name → auto-advance to the
/// next free name. Both are announced.
fn ask_dut_name(
    io: &mut dyn PromptIO,
    prompt: &str,
    prefix: &str,
    default: Option<String>,
    taken: &mut Vec<String>,
) -> Result<(String, Option<String>), String> {
    let auto = next_free_dut_name(prefix, taken);
    let def = default.clone().unwrap_or_else(|| auto.clone());
    let raw = io.input(prompt, Some(&def))?;
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == def {
        // Empty input (or typing the shown default back): keep the default.
        // The default is either the CURRENT name (already in `taken` — not
        // a collision) or the auto-name. Only an EMPTY default auto-names.
        if def.is_empty() {
            let name = next_free_dut_name(prefix, taken);
            taken.push(name.clone());
            let announcement = format!("dut name was empty — auto-named '{name}'");
            io.note(NoteKind::Info, &announcement);
            return Ok((name, Some(announcement)));
        }
        if !taken.iter().any(|t| t == &def) {
            taken.push(def.clone());
        }
        return Ok((def, None));
    }
    if taken.iter().any(|t| t == trimmed) {
        let name = next_free_dut_name(prefix, taken);
        taken.push(name.clone());
        let announcement = format!("dut name '{trimmed}' already taken — using '{name}'");
        io.note(NoteKind::Info, &announcement);
        return Ok((name, Some(announcement)));
    }
    taken.push(trimmed.to_string());
    Ok((trimmed.to_string(), None))
}

/// Required question: re-asks until non-empty (Ctrl-C/EOF aborts), with an
/// explanatory note on each empty submit when there is no default to fall
/// back on; an empty submit with a NON-empty shown default returns that
/// default silently (no note, no re-ask — the default is a value, so
/// nothing is missing).
fn ask_required(
    io: &mut dyn PromptIO,
    prompt: &str,
    default: Option<String>,
) -> Result<String, String> {
    let def = default.clone().unwrap_or_default();
    loop {
        let raw = io.input(&format!("{prompt} (required)"), Some(&def))?;
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
        if !def.is_empty() {
            // Accept the shown default — it is non-empty, so the item is
            // not missing; return it without a note or a re-ask.
            return Ok(def.clone());
        }
        io.note(NoteKind::Error, "value is required");
    }
}

/// Optional question: empty input → default (which is "" when no default).
fn ask_optional(
    io: &mut dyn PromptIO,
    prompt: &str,
    default: Option<String>,
) -> Result<String, String> {
    let def = default.clone().unwrap_or_default();
    let raw = io.input(prompt, Some(&def))?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        Ok(def)
    } else {
        Ok(trimmed.to_string())
    }
}

/// One advanced question (plain walk): bare Enter accepts the shown
/// default for AcceptDefault fields only; SkipInherit keeps skip/inherit.
/// Typed values are validated NOW via `advanced_literal` (key-named error
/// note + re-ask) instead of at the write gate.
fn ask_advanced_value(
    io: &mut dyn PromptIO,
    label: &str,
    empty: &EmptyMeans,
    key: &str,
    default: &str,
) -> Result<String, String> {
    loop {
        let raw = io.input(label, Some(default))?;
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            if *empty == EmptyMeans::AcceptDefault && !default.is_empty() {
                return Ok(default.to_string());
            }
            return Ok(String::new()); // skip/inherit
        }
        match advanced_literal(key, trimmed) {
            Ok(_) => return Ok(trimmed.to_string()),
            Err(e) => {
                io.note(NoteKind::Error, &e);
                // re-ask the SAME key
            }
        }
    }
}

/// One zero-guarded per-DUT relay numeric question (plain walk). Empty
/// input follows the field's EmptyMeans: SkipInherit → "" (inherit),
/// AcceptDefault → the shown default. Typed values are validated NOW via
/// `advanced_literal` (key-named error note + re-ask); "0" is a valid
/// answer (zero-guarded at build time).
fn ask_relay_field(
    io: &mut dyn PromptIO,
    field: &FieldInstance,
    advanced_key: &str,
) -> Result<String, String> {
    loop {
        let raw = io.input(&field.label, field.default.as_deref())?;
        let trimmed = raw.trim().to_string();
        if trimmed.is_empty() {
            return Ok(match field.empty {
                EmptyMeans::AcceptDefault => field.default.clone().unwrap_or_default(),
                EmptyMeans::SkipInherit => String::new(),
            });
        }
        match advanced_literal(advanced_key, &trimmed) {
            Ok(_) => return Ok(trimmed),
            Err(e) => io.note(NoteKind::Error, &e),
        }
    }
}

/// Every missing REQUIRED item (dev host ip, serial port) — the spec's
/// non-interactive error names ALL of them, never just the first.
fn missing_required(answers: &WizardAnswers) -> Vec<String> {
    let mut missing = Vec::new();
    for (hi, host) in answers.hosts.iter().enumerate() {
        if host.ip.trim().is_empty() {
            missing.push(format!("dev_hosts[{hi}].ip"));
        }
        for (di, dut) in host.duts.iter().enumerate() {
            let port = dut.serial_port.trim();
            if port.is_empty() || port == "0" {
                missing.push(format!("dev_hosts[{hi}].duts[{di}].serial.port"));
            }
        }
    }
    missing
}

/// First free `<prefix><n>` name, continuing after every taken name.
pub fn next_free_dut_name(prefix: &str, taken: &[String]) -> String {
    let mut n = 1usize;
    loop {
        let candidate = format!("{prefix}{n}");
        if !taken.iter().any(|t| t == &candidate) {
            return candidate;
        }
        n += 1;
    }
}

// ── Advanced question table ───────────────────────────────────────────
//
// ADVANCED_FIELDS (and the FORM_GROUPS advanced group) are generated by
// the `advanced_field_table!` macro above from ONE literal list — the old
// ADVANCED_KEYS + ENTER_ACCEPTS_DEFAULT pair is gone; `empty: AcceptDefault`
// replaces the ENTER_ACCEPTS_DEFAULT membership (asserted by a migration
// test).

/// The composed monitor.reference_log default for a DUT name (req #10).
pub fn reference_log_default(name: &str) -> String {
    format!(".dut-serial/{name}/reference-boot.log")
}

/// Built-in default for an advanced key (shown when no current value).
pub fn advanced_default(key: &str) -> String {
    match key {
        "serial.baudrate" => "460800".into(),
        "target.login_pass"
        | "target.login_prompt"
        | "uboot.pattern"
        | "monitor.reference_log"
        | "control.dev_ctrl" => String::new(),
        "flash.loader_bin" => "rockdev/loader.bin".into(),
        "uboot.interrupt_char" => loader::default().default_interrupt_char().into(),
        "uboot.interrupt_strategy" => loader::default().default_interrupt_strategy().into(),
        "monitor.hang_timeout" => "60".into(),
        "monitor.max_archived_logs" => "10".into(),
        "monitor.max_log_file_size" => "100".into(),
        "monitor.learner_stage_threshold" => "0.45".into(),
        "monitor.learner_crash_threshold" => "0.50".into(),
        "monitor.crash_keywords" => {
            "Kernel panic, BUG:, Oops:, Unable to handle kernel, panic - not syncing".into()
        }
        "flash.full_image_cmd" => "uf {image}".into(),
        "flash.kernel_image_cmd" => "di -k {image}".into(),
        "loader" => loader::default().name().into(),
        _ => String::new(),
    }
}

/// Current stringified value of an advanced key from the config's GLOBAL
/// sections (the update defaults).
pub fn advanced_current(config: &TargetJsoncConfig) -> HashMap<String, String> {
    let mut m = HashMap::new();
    if let Some(s) = &config.serial
        && let Some(v) = &s.baudrate
    {
        m.insert("serial.baudrate".into(), v.to_value_string());
    }
    if let Some(t) = &config.target {
        if let Some(v) = &t.login_pass {
            m.insert("target.login_pass".into(), v.clone());
        }
        if let Some(v) = &t.login_prompt {
            m.insert("target.login_prompt".into(), v.clone());
        }
    }
    if let Some(u) = &config.uboot {
        if let Some(v) = &u.interrupt_char {
            m.insert("uboot.interrupt_char".into(), v.clone());
        }
        if let Some(v) = &u.interrupt_strategy {
            m.insert("uboot.interrupt_strategy".into(), v.clone());
        }
        if let Some(v) = &u.pattern {
            m.insert("uboot.pattern".into(), v.clone());
        }
    }
    if let Some(mo) = &config.monitor {
        if let Some(v) = mo.hang_timeout {
            m.insert("monitor.hang_timeout".into(), v.to_string());
        }
        if let Some(v) = mo.max_archived_logs {
            m.insert("monitor.max_archived_logs".into(), v.to_string());
        }
        if let Some(v) = mo.max_log_file_size {
            m.insert("monitor.max_log_file_size".into(), v.to_string());
        }
        if let Some(v) = &mo.reference_log {
            m.insert("monitor.reference_log".into(), v.clone());
        }
        if let Some(v) = mo.learner_stage_threshold {
            m.insert("monitor.learner_stage_threshold".into(), v.to_string());
        }
        if let Some(v) = mo.learner_crash_threshold {
            m.insert("monitor.learner_crash_threshold".into(), v.to_string());
        }
        if let Some(v) = &mo.crash_keywords {
            m.insert("monitor.crash_keywords".into(), v.join(", "));
        }
    }
    if let Some(f) = &config.flash {
        for (key, value) in [
            ("flash.full_image_cmd", &f.full_image_cmd),
            ("flash.kernel_image_cmd", &f.kernel_image_cmd),
            ("flash.loader_bin", &f.loader_bin),
        ] {
            if let Some(v) = value {
                m.insert(key.into(), v.clone());
            }
        }
    }
    if let Some(c) = &config.control
        && let Some(v) = &c.dev_ctrl
    {
        m.insert("control.dev_ctrl".into(), v.clone());
    }
    if let Some(v) = &config.loader {
        m.insert("loader".into(), v.clone());
    }
    m
}

/// Current stringified value of the DUT-owned advanced keys from ONE
/// DUT's OWN override sections (never the global fallbacks).
pub fn dut_advanced_current(dut: &JsoncDut) -> HashMap<String, String> {
    let mut m = HashMap::new();
    if let Some(s) = &dut.serial
        && let Some(v) = &s.baudrate
    {
        m.insert("serial.baudrate".into(), v.to_value_string());
    }
    if let Some(t) = &dut.target {
        if let Some(v) = &t.login_pass {
            m.insert("target.login_pass".into(), v.clone());
        }
        if let Some(v) = &t.login_prompt {
            m.insert("target.login_prompt".into(), v.clone());
        }
    }
    if let Some(u) = &dut.uboot {
        if let Some(v) = &u.interrupt_char {
            m.insert("uboot.interrupt_char".into(), v.clone());
        }
        if let Some(v) = &u.interrupt_strategy {
            m.insert("uboot.interrupt_strategy".into(), v.clone());
        }
        if let Some(v) = &u.pattern {
            m.insert("uboot.pattern".into(), v.clone());
        }
    }
    if let Some(mo) = &dut.monitor {
        if let Some(v) = mo.max_archived_logs {
            m.insert("monitor.max_archived_logs".into(), v.to_string());
        }
        if let Some(v) = &mo.reference_log {
            m.insert("monitor.reference_log".into(), v.clone());
        }
        if let Some(v) = &mo.crash_keywords {
            m.insert("monitor.crash_keywords".into(), v.join(", "));
        }
    }
    if let Some(f) = &dut.flash {
        for (key, value) in [
            ("flash.full_image_cmd", &f.full_image_cmd),
            ("flash.kernel_image_cmd", &f.kernel_image_cmd),
            ("flash.loader_bin", &f.loader_bin),
        ] {
            if let Some(v) = value {
                m.insert(key.into(), v.clone());
            }
        }
    }
    if let Some(c) = &dut.control
        && let Some(v) = &c.dev_ctrl
    {
        m.insert("control.dev_ctrl".into(), v.clone());
    }
    if let Some(v) = &dut.loader {
        m.insert("loader".into(), v.clone());
    }
    m
}

/// The effective CURRENT value for one DUT-owned advanced key — the DUT's
/// own override or the GLOBAL fallback (never the natural default).
pub fn dut_advanced_current_value(
    schema: &FormSchema,
    hi: usize,
    di: usize,
    k: &str,
) -> Option<String> {
    let dut = schema.host_defaults.get(hi).and_then(|h| h.duts.get(di));
    dut.and_then(|d| d.advanced.get(k).cloned())
        .or_else(|| schema.advanced_current.get(k).cloned())
}

/// The resolved DEFAULT HINT for one DUT-owned advanced key — per-DUT
/// override > GLOBAL current value > natural/composed. The composed
/// monitor.reference_log uses `live_name` (the just-answered name in the
/// plain walk) or that DUT's existing name. The hint is INFORMATIONAL:
/// the natural/composed defaults are never recorded as answers (an
/// untouched field writes nothing — byte-stability).
pub fn dut_advanced_default(
    schema: &FormSchema,
    hi: usize,
    di: usize,
    k: &str,
    live_name: Option<&str>,
) -> String {
    if let Some(v) = dut_advanced_current_value(schema, hi, di, k) {
        return v;
    }
    if k == "monitor.reference_log" {
        let dut = schema.host_defaults.get(hi).and_then(|h| h.duts.get(di));
        let name = live_name
            .or_else(|| dut.and_then(|d| d.dut_name.as_deref()))
            .or_else(|| {
                schema
                    .host_defaults
                    .first()
                    .and_then(|h| h.duts.first())
                    .and_then(|d| d.dut_name.as_deref())
            })
            .unwrap_or("dut1");
        reference_log_default(name)
    } else {
        advanced_default(k)
    }
}

/// Parse the flat advanced key into `(section, field)`; "lock_dir" has no
/// section (root member).
fn advanced_field(key: &str) -> (&str, &str) {
    match key.split_once('.') {
        Some((section, field)) => (section, field),
        None => ("", key),
    }
}

/// Comment for an advanced key's rendered line (canonical text only).
fn comment_for(key: &str) -> Option<&'static str> {
    let field = advanced_field(key).1;
    Some(match field {
        "port" => C_PORT,
        "baudrate" => C_BAUDRATE,
        "rfc2217" => C_RFC2217,
        "login_user" => C_LOGIN_USER,
        "login_prompt" => C_LOGIN_PROMPT,
        "interrupt_char" => C_UBOOT_CHAR,
        "type" => C_RELAY_TYPE,
        "reset_ch" => C_RELAY_CH_ALIASES,
        "power_ch" => C_POWER_CH,
        "reset_time_ms" => C_RESET_TIME,
        "power_off_time_ms" => C_POWER_OFF,
        "crash_keywords" => C_CRASH_KEYWORDS,
        "dut_dir" => C_DUT_DIR,
        "reference_log" => C_REFERENCE_LOG_DUT,
        "lock_dir" => C_LOCK_DIR,
        _ => return None,
    })
}

/// The JSON literal for an advanced answer, or None when the answer means
/// "unset/inherit" (empty, 0, 0.0, empty pattern list — the zero-guarded
/// rule). Invalid typed values Err naming the key.
pub fn advanced_literal(key: &str, value: &str) -> Result<Option<String>, String> {
    let v = value.trim();
    if v.is_empty() {
        return Ok(None);
    }
    let int = |kind: &str| -> Result<String, String> {
        v.parse::<u64>()
            .map(|n| n.to_string())
            .map_err(|_| format!("{key}: invalid {kind}: {v}"))
    };
    let bool_lit = || -> Result<String, String> {
        match v.to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Ok("true".into()),
            "false" | "0" | "no" | "off" => Ok("false".into()),
            _ => Err(format!("{key}: invalid bool: {v}")),
        }
    };
    let zero_guarded = |kind: &str| -> Result<Option<String>, String> {
        let n = v
            .parse::<u64>()
            .map_err(|_| format!("{key}: invalid {kind}: {v}"))?;
        if n == 0 {
            Ok(None)
        } else {
            Ok(Some(n.to_string()))
        }
    };
    let threshold = || -> Result<Option<String>, String> {
        let f = v
            .parse::<f64>()
            .map_err(|_| format!("{key}: invalid threshold: {v}"))?;
        if f <= 0.0 {
            Ok(None)
        } else {
            Ok(Some(f.to_string()))
        }
    };
    match key {
        "serial.baudrate" => Ok(Some(json_literal(&parse_number_or_string(v)?))),
        "serial.rfc2217" => Ok(Some(bool_lit()?)),
        "target.login_pass"
        | "target.login_prompt"
        | "monitor.reference_log"
        | "flash.full_image_cmd"
        | "flash.kernel_image_cmd"
        | "flash.loader_bin"
        | "control.dev_ctrl"
        | "loader"
        | "lock_dir" => Ok(Some(serde_json::to_string(v).map_err(|e| e.to_string())?)),
        "uboot.interrupt_strategy" => match v {
            "flood" | "pattern" => Ok(Some(serde_json::to_string(v).map_err(|e| e.to_string())?)),
            _ => Err(format!(
                "{key}: unsupported value {v:?}; valid values: flood, pattern"
            )),
        },
        "uboot.pattern" => {
            regex::Regex::new(v).map_err(|e| format!("{key}: invalid regex: {e}"))?;
            Ok(Some(serde_json::to_string(v).map_err(|e| e.to_string())?))
        }
        "uboot.interrupt_char" => {
            crate::config::parse_interrupt_char_checked(v).map_err(|e| format!("{key}: {e}"))?;
            Ok(Some(serde_json::to_string(v).map_err(|e| e.to_string())?))
        }
        "relay.port" => zero_guarded("port"),
        "relay.reset_ch" | "relay.download_ch" | "relay.power_ch" => zero_guarded("channel"),
        "relay.reset_time_ms" => zero_guarded("ms"),
        "relay.power_off_time_ms" => zero_guarded("ms")
            .map(|o| o.map(|n| n.parse::<u64>().unwrap_or(0).max(3000).to_string())),
        "monitor.hang_timeout" => Ok(Some(int("seconds")?)),
        "monitor.hang_hysteresis" | "monitor.max_archived_logs" | "monitor.max_log_file_size" => {
            Ok(Some(int("count")?))
        }
        "monitor.learner_stage_threshold" | "monitor.learner_crash_threshold" => threshold(),
        "monitor.crash_keywords" => {
            let keywords: Vec<String> = v
                .split([',', '\n'])
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            if keywords.is_empty() {
                return Ok(None);
            }
            Ok(Some(pretty_array_literal(&keywords, 0)))
        }
        _ => Err(format!("unknown advanced key: {key}")),
    }
}

/// Merge advanced answers into the config's GLOBAL sections (zero-guarded:
/// 0 / empty answers never insert — they mean "inherit").
pub fn merge_advanced(
    config: &mut TargetJsoncConfig,
    answers: &HashMap<String, String>,
) -> Result<(), String> {
    for (key, value) in answers {
        let (section, field) = advanced_field(key);
        let set = |slot: &mut Option<String>| {
            slot.replace(value.trim().to_string());
            Ok::<(), String>(())
        };
        match section {
            "serial" => {
                let s = config.serial.get_or_insert(JsoncSerial::default());
                match field {
                    "baudrate" => s.baudrate = Some(parse_number_or_string(value)?),
                    "rfc2217" => match value.trim().to_ascii_lowercase().as_str() {
                        "true" | "1" | "yes" | "on" => s.rfc2217 = Some(true),
                        "false" | "0" | "no" | "off" => s.rfc2217 = Some(false),
                        _ => return Err(format!("{key}: invalid bool: {value}")),
                    },
                    _ => return Err(format!("unknown advanced key: {key}")),
                }
            }
            "target" => {
                let t = config.target.get_or_insert(JsoncTarget::default());
                match field {
                    "login_pass" => set(&mut t.login_pass)?,
                    "login_prompt" => set(&mut t.login_prompt)?,
                    _ => return Err(format!("unknown advanced key: {key}")),
                }
            }
            "uboot" => {
                let u = config.uboot.get_or_insert(JsoncUboot::default());
                match field {
                    "interrupt_char" => set(&mut u.interrupt_char)?,
                    "interrupt_strategy" => set(&mut u.interrupt_strategy)?,
                    "pattern" => set(&mut u.pattern)?,
                    _ => return Err(format!("unknown advanced key: {key}")),
                }
            }
            "relay" => {
                let r = config.relay.get_or_insert(JsoncRelay::default());
                match field {
                    "port" => {
                        let n = value
                            .trim()
                            .parse::<u16>()
                            .map_err(|_| format!("{key}: invalid port: {value}"))?;
                        if n > 0 {
                            r.port = Some(n);
                        }
                    }
                    "reset_ch" | "download_ch" | "power_ch" => {
                        let n = value
                            .trim()
                            .parse::<u8>()
                            .map_err(|_| format!("{key}: invalid channel: {value}"))?;
                        if n > 0 {
                            let slot = match field {
                                "reset_ch" => &mut r.reset_ch,
                                "download_ch" => &mut r.download_ch,
                                _ => &mut r.power_ch,
                            };
                            *slot = Some(n);
                        }
                    }
                    "reset_time_ms" => {
                        let n = value
                            .trim()
                            .parse::<u64>()
                            .map_err(|_| format!("{key}: invalid ms: {value}"))?;
                        if n > 0 {
                            r.reset_time_ms = Some(n);
                        }
                    }
                    "power_off_time_ms" => {
                        let n = value
                            .trim()
                            .parse::<u64>()
                            .map_err(|_| format!("{key}: invalid ms: {value}"))?;
                        if n > 0 {
                            r.power_off_time_ms = Some(n.max(3000));
                        }
                    }
                    _ => return Err(format!("unknown advanced key: {key}")),
                }
            }
            "monitor" => {
                let m = config.monitor.get_or_insert(JsoncMonitor::default());
                match field {
                    "hang_timeout" => {
                        m.hang_timeout = Some(
                            value
                                .trim()
                                .parse()
                                .map_err(|_| format!("{key}: invalid seconds: {value}"))?,
                        );
                    }
                    "hang_hysteresis" => {
                        m.hang_hysteresis = Some(
                            value
                                .trim()
                                .parse()
                                .map_err(|_| format!("{key}: invalid count: {value}"))?,
                        );
                    }
                    "max_archived_logs" => {
                        m.max_archived_logs = Some(
                            value
                                .trim()
                                .parse()
                                .map_err(|_| format!("{key}: invalid count: {value}"))?,
                        );
                    }
                    "max_log_file_size" => {
                        m.max_log_file_size = Some(
                            value
                                .trim()
                                .parse()
                                .map_err(|_| format!("{key}: invalid MB: {value}"))?,
                        );
                    }
                    "reference_log" => set(&mut m.reference_log)?,
                    "learner_stage_threshold" => {
                        let f = value
                            .trim()
                            .parse::<f64>()
                            .map_err(|_| format!("{key}: invalid threshold: {value}"))?;
                        if f > 0.0 {
                            m.learner_stage_threshold = Some(f);
                        }
                    }
                    "learner_crash_threshold" => {
                        let f = value
                            .trim()
                            .parse::<f64>()
                            .map_err(|_| format!("{key}: invalid threshold: {value}"))?;
                        if f > 0.0 {
                            m.learner_crash_threshold = Some(f);
                        }
                    }
                    "crash_keywords" => {
                        let keywords: Vec<String> = value
                            .split([',', '\n'])
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(str::to_string)
                            .collect();
                        if !keywords.is_empty() {
                            m.crash_keywords = Some(keywords);
                        }
                    }
                    _ => return Err(format!("unknown advanced key: {key}")),
                }
            }
            "flash" => {
                let f = config.flash.get_or_insert(JsoncFlash::default());
                match field {
                    "full_image_cmd" => set(&mut f.full_image_cmd)?,
                    "kernel_image_cmd" => set(&mut f.kernel_image_cmd)?,
                    "loader_bin" => set(&mut f.loader_bin)?,
                    "loader_cmd" => set(&mut f.loader_cmd)?,
                    "list_devices_cmd" => set(&mut f.list_devices_cmd)?,
                    _ => return Err(format!("unknown advanced key: {key}")),
                }
            }
            "control" => {
                let c = config
                    .control
                    .get_or_insert(JsoncControl { dev_ctrl: None });
                if field == "dev_ctrl" {
                    set(&mut c.dev_ctrl)?;
                } else {
                    return Err(format!("unknown advanced key: {key}"));
                }
            }
            "" => match field {
                "lock_dir" => set(&mut config.lock_dir)?,
                "loader" => set(&mut config.loader)?,
                _ => return Err(format!("unknown advanced key: {key}")),
            },
            _ => return Err(format!("unknown advanced key: {key}")),
        }
    }
    Ok(())
}

/// Merge DUT-owned advanced answers into ONE DUT's override sections
/// (zero-guarded: empty answers never insert — they mean "inherit"; the
/// "" unset marker is skipped — CREATE has nothing to remove). Mirrors
/// `merge_advanced`'s per-field parse semantics for the JsoncDut section
/// types.
pub fn merge_dut_advanced(
    dut: &mut JsoncDut,
    answers: &HashMap<String, String>,
) -> Result<(), String> {
    for (key, value) in answers {
        let (section, field) = advanced_field(key);
        let t = value.trim();
        if t.is_empty() {
            continue; // "" = unset/inherit marker — nothing to write
        }
        let set = |slot: &mut Option<String>| {
            slot.replace(t.to_string());
            Ok::<(), String>(())
        };
        match section {
            "serial" => {
                let s = dut.serial.get_or_insert(JsoncSerial::default());
                if field == "baudrate" {
                    s.baudrate = Some(parse_number_or_string(t)?);
                } else {
                    return Err(format!("unknown advanced key: {key}"));
                }
            }
            "target" => {
                let tgt = dut.target.get_or_insert(JsoncTarget::default());
                match field {
                    "login_pass" => set(&mut tgt.login_pass)?,
                    "login_prompt" => set(&mut tgt.login_prompt)?,
                    _ => return Err(format!("unknown advanced key: {key}")),
                }
            }
            "uboot" => {
                let u = dut.uboot.get_or_insert(JsoncUboot::default());
                match field {
                    "interrupt_char" => set(&mut u.interrupt_char)?,
                    "interrupt_strategy" => set(&mut u.interrupt_strategy)?,
                    "pattern" => set(&mut u.pattern)?,
                    _ => return Err(format!("unknown advanced key: {key}")),
                }
            }
            "monitor" => {
                let m = dut.monitor.get_or_insert(JsoncMonitor::default());
                match field {
                    "max_archived_logs" => {
                        m.max_archived_logs = Some(
                            t.parse()
                                .map_err(|_| format!("{key}: invalid count: {t}"))?,
                        );
                    }
                    "reference_log" => set(&mut m.reference_log)?,
                    "crash_keywords" => {
                        let keywords: Vec<String> = t
                            .split([',', '\n'])
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(str::to_string)
                            .collect();
                        if !keywords.is_empty() {
                            m.crash_keywords = Some(keywords);
                        }
                    }
                    _ => return Err(format!("unknown advanced key: {key}")),
                }
            }
            "flash" => {
                let f = dut.flash.get_or_insert(JsoncFlash::default());
                match field {
                    "full_image_cmd" => set(&mut f.full_image_cmd)?,
                    "kernel_image_cmd" => set(&mut f.kernel_image_cmd)?,
                    "loader_bin" => set(&mut f.loader_bin)?,
                    "loader_cmd" => set(&mut f.loader_cmd)?,
                    "list_devices_cmd" => set(&mut f.list_devices_cmd)?,
                    _ => return Err(format!("unknown advanced key: {key}")),
                }
            }
            "control" => {
                let c = dut.control.get_or_insert(JsoncControl { dev_ctrl: None });
                if field == "dev_ctrl" {
                    set(&mut c.dev_ctrl)?;
                } else {
                    return Err(format!("unknown advanced key: {key}"));
                }
            }
            "" if field == "loader" => {
                // Top-level string key stored directly on the DUT.
                dut.loader = Some(t.to_string());
            }
            _ => return Err(format!("unknown advanced key: {key}")),
        }
    }
    Ok(())
}

// ── Answers → config ──────────────────────────────────────────────────

/// Parse a scalar that may be a number (int/float) or a string (device path).
pub fn parse_number_or_string(s: &str) -> Result<NumberOrString, String> {
    let t = s.trim();
    if let Ok(n) = t.parse::<u64>() {
        return Ok(NumberOrString::Int(n));
    }
    if let Ok(f) = t.parse::<f64>() {
        return Ok(NumberOrString::Float(f));
    }
    Ok(NumberOrString::String(t.to_string()))
}

/// JSON literal for a number-or-string value (quotes only for strings).
fn json_literal(v: &NumberOrString) -> String {
    match v {
        NumberOrString::Int(n) => n.to_string(),
        NumberOrString::Float(f) => f.to_string(),
        NumberOrString::String(s) => serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into()),
    }
}

/// JSON array literal, pretty-printed one item per line at `indent` (the
/// closing bracket column) — used for crash_keywords.
pub fn pretty_array_literal(items: &[String], indent: usize) -> String {
    let mut out = String::from("[");
    let inner = " ".repeat(indent + 2);
    for (i, item) in items.iter().enumerate() {
        out.push('\n');
        out.push_str(&inner);
        out.push_str(&serde_json::to_string(item).unwrap_or_else(|_| "\"\"".into()));
        if i + 1 < items.len() {
            out.push(',');
        }
    }
    out.push('\n');
    out.push_str(&" ".repeat(indent));
    out.push(']');
    out
}

/// Parse a zero-guarded per-DUT relay scalar: ""/"0" → None (inherit),
/// n → Some(n) with range validation.
fn parse_relay_u16(key: &str, v: &str) -> Result<Option<u16>, String> {
    let t = v.trim();
    if t.is_empty() {
        return Ok(None);
    }
    let n: u16 = t.parse().map_err(|_| format!("{key}: invalid port: {t}"))?;
    Ok((n > 0).then_some(n))
}

/// Zero-guarded relay channel scalar (""/"0" → inherit).
fn parse_relay_u8(key: &str, v: &str) -> Result<Option<u8>, String> {
    let t = v.trim();
    if t.is_empty() {
        return Ok(None);
    }
    let n: u8 = t
        .parse()
        .map_err(|_| format!("{key}: invalid channel: {t}"))?;
    Ok((n > 0).then_some(n))
}

/// Zero-guarded relay duration scalar (""/"0" → inherit).
fn parse_relay_u64(key: &str, v: &str) -> Result<Option<u64>, String> {
    let t = v.trim();
    if t.is_empty() {
        return Ok(None);
    }
    let n: u64 = t.parse().map_err(|_| format!("{key}: invalid ms: {t}"))?;
    Ok((n > 0).then_some(n))
}

/// DUT answers → the relay section. Gate OFF → `None` (relay omitted —
/// the same end state the old flow produced when no relay was chosen).
/// Gate ON → the relay fields materialize (zero-guarded: "0"/"" inherit;
/// power_off_time_ms clamped to >= 3000).
fn relay_from_answers(dut: &DutAnswers) -> Result<Option<JsoncRelay>, String> {
    if !dut.relay_gate {
        return Ok(None);
    }
    Ok(Some(JsoncRelay {
        relay_type: Some(dut.relay_type.clone()),
        port: parse_relay_u16("relay.port", &dut.relay_port)?,
        ip: None,
        reset_ch: parse_relay_u8("relay.reset_ch", &dut.relay_reset_ch)?,
        download_ch: parse_relay_u8("relay.download_ch", &dut.relay_download_ch)?,
        power_ch: parse_relay_u8("relay.power_ch", &dut.relay_power_ch)?,
        reset_time_ms: parse_relay_u64("relay.reset_time_ms", &dut.relay_reset_time_ms)?,
        power_off_time_ms: parse_relay_u64(
            "relay.power_off_time_ms",
            &dut.relay_power_off_time_ms,
        )?
        .map(|n| n.max(3000)),
    }))
}

/// Build the nested JSONC schema from the wizard's answers (quick fields
/// nested per host/DUT; advanced answers become global fallback sections).
/// Zero hosts => zero DUTs: `dev_hosts` is written empty and no DUT data
/// can survive.
fn build_config_from_answers(answers: &WizardAnswers) -> Result<TargetJsoncConfig, String> {
    let mut config = TargetJsoncConfig {
        dev_hosts: Vec::new(),
        serial: None,
        target: None,
        uboot: None,
        relay: None,
        monitor: None,
        flash: None,
        download: None,
        control: None,
        loader: None,
        reference_log: None,
        dut_dir: None,
        lock_dir: None,
    };
    for host in &answers.hosts {
        let mut duts = Vec::new();
        for dut in &host.duts {
            let port = parse_number_or_string(&dut.serial_port)
                .map_err(|e| format!("DUT '{}': {e}", dut.dut_name))?;
            let mut dutc = JsoncDut {
                dut_name: dut.dut_name.clone(),
                serial: Some(JsoncSerial {
                    port: Some(port),
                    baudrate: None,
                    rfc2217: None,
                    tx_repeat: None,
                    tx_gap_ms: None,
                    backspace: None,
                    enter: None,
                    rx_newline: None,
                    highlight: None,
                    mouse: None,
                }),
                target: Some(JsoncTarget {
                    login_user: Some(dut.login_user.clone()),
                    login_pass: Some(String::new()),
                    login_prompt: None,
                }),
                relay: relay_from_answers(dut)?,
                uboot: None,
                monitor: None,
                flash: None,
                download: None,
                control: None,
                loader: None,
                reference_log: None,
                dut_dir: None,
            };
            merge_dut_advanced(&mut dutc, &dut.advanced)?;
            duts.push(dutc);
        }
        config.dev_hosts.push(JsoncDevHost {
            host_name: Some(host.host_name.clone()).filter(|s| !s.is_empty()),
            ip: Some(host.ip.clone()),
            user: Some(host.user.clone()),
            pass: Some(host.pass.clone()),
            duts,
        });
    }
    merge_advanced(&mut config, &answers.advanced)?;
    Ok(config)
}

// ── Renderer (the canonical template) ─────────────────────────────────

/// One line inside a section block: real key or commented-out hint.
#[derive(Debug, Clone, PartialEq)]
enum Line {
    Key(&'static str, String, Option<&'static str>),
    Hint(String, Option<&'static str>),
}

/// `"name": { <lines> },` — a full section block starting at `indent`,
/// member lines at `indent + 2`.
fn render_section_block(name: &str, lines: &[Line], indent: usize) -> String {
    let i = " ".repeat(indent);
    let i2 = " ".repeat(indent + 2);
    let mut out = format!("{i}\"{name}\": {{\n");
    for line in lines {
        match line {
            Line::Key(key, lit, comment) => {
                out.push_str(&key_line(&i2, key, lit, *comment));
            }
            Line::Hint(text, comment) => {
                out.push_str(&hint_line(&i2, text, *comment));
            }
        }
        out.push('\n');
    }
    out.push_str(&format!("{i}}},"));
    out
}

/// Render one key line with the canonical trailing comment (padded so
/// comments line up within a block).
fn key_line(indent: &str, key: &str, literal: &str, comment: Option<&str>) -> String {
    // Every member line carries its trailing comma (valid JSONC) so blocks
    // splice cleanly into arrays/objects without comma surgery.
    let base = format!("{indent}\"{key}\": {literal},");
    match comment {
        Some(c) => {
            let width = 44usize.saturating_sub(indent.len());
            if base.len() >= width {
                format!("{base} // {c}")
            } else {
                format!("{base:<width$} // {c}", width = width)
            }
        }
        None => base,
    }
}

/// Render one commented-out hint line (`// "key": default` with an optional
/// trailing second comment, canonical style).
fn hint_line(indent: &str, text: &str, comment: Option<&str>) -> String {
    match comment {
        Some(c) => {
            let width = 42usize.saturating_sub(indent.len() + 3);
            if text.len() >= width {
                format!("{indent}// {text} // {c}")
            } else {
                format!("{indent}// {text:<width$} // {c}", width = width)
            }
        }
        None => format!("{indent}// {text}"),
    }
}

/// Lines for a `serial` section body (port always real in wizard-created
/// files; baudrate/rfc2217/ip render as canonical hints when unset).
fn serial_lines(serial: &JsoncSerial) -> Vec<Line> {
    let mut lines = Vec::new();
    if let Some(port) = &serial.port {
        lines.push(Line::Key("port", json_literal(port), Some(C_PORT)));
    }
    match &serial.baudrate {
        Some(b) => lines.push(Line::Key("baudrate", json_literal(b), Some(C_BAUDRATE))),
        None => lines.push(Line::Hint("\"baudrate\": 1500000".into(), Some(C_BAUDRATE))),
    }
    match serial.rfc2217 {
        Some(v) => lines.push(Line::Key("rfc2217", v.to_string(), Some(C_RFC2217))),
        None => lines.push(Line::Hint("\"rfc2217\": false".into(), Some(C_RFC2217))),
    }
    // tx_repeat / tx_gap_ms are deliberately NOT shown here: the engine
    // auto-probes alternating-byte TX loss at the login prompt and enables
    // double-send on shell confirmation. The .target.jsonc keys remain as
    // a manual escape hatch for exotic links.
    lines
}

/// Lines for a `target` section body (login_user/login_pass always real in
/// wizard-created files; login_prompt renders as a hint when unset).
fn target_lines(target: &JsoncTarget) -> Vec<Line> {
    let mut lines = vec![
        Line::Key(
            "login_user",
            serde_json::to_string(target.login_user.as_deref().unwrap_or(""))
                .unwrap_or_else(|_| "\"\"".into()),
            Some(C_LOGIN_USER),
        ),
        Line::Key(
            "login_pass",
            serde_json::to_string(target.login_pass.as_deref().unwrap_or(""))
                .unwrap_or_else(|_| "\"\"".into()),
            None,
        ),
    ];
    match &target.login_prompt {
        Some(p) => lines.push(Line::Key(
            "login_prompt",
            serde_json::to_string(p).unwrap_or_else(|_| "\"\"".into()),
            Some(C_LOGIN_PROMPT),
        )),
        None => lines.push(Line::Hint(
            "\"login_prompt\": \"\"".into(),
            Some(C_LOGIN_PROMPT),
        )),
    }
    lines
}

/// Lines for a `relay` section body — only set keys (0s are omitted: the
/// zero-guarded rule means an explicit 0 IS the omission).
fn relay_lines(relay: &JsoncRelay) -> Vec<Line> {
    let mut lines = Vec::new();
    if let Some(t) = &relay.relay_type {
        lines.push(Line::Key(
            "type",
            serde_json::to_string(t).unwrap_or_else(|_| "\"\"".into()),
            Some(C_RELAY_TYPE),
        ));
    }
    if let Some(p) = relay.port {
        lines.push(Line::Key("port", p.to_string(), None));
    }
    if let Some(ip) = &relay.ip {
        lines.push(Line::Key(
            "ip",
            serde_json::to_string(ip).unwrap_or_else(|_| "\"\"".into()),
            None,
        ));
    }
    for (key, value, comment) in [
        ("reset_ch", relay.reset_ch, Some(C_RELAY_CH_ALIASES)),
        ("download_ch", relay.download_ch, None),
        ("power_ch", relay.power_ch, Some(C_POWER_CH)),
    ] {
        if let Some(v) = value.filter(|&v| v > 0) {
            lines.push(Line::Key(key, v.to_string(), comment));
        }
    }
    if let Some(v) = relay.reset_time_ms.filter(|&v| v > 0) {
        lines.push(Line::Key(
            "reset_time_ms",
            v.to_string(),
            Some(C_RESET_TIME),
        ));
    }
    if let Some(v) = relay.power_off_time_ms.filter(|&v| v > 0) {
        lines.push(Line::Key(
            "power_off_time_ms",
            v.to_string(),
            Some(C_POWER_OFF),
        ));
    }
    lines
}

/// Lines for a `monitor` section body — only set keys.
fn monitor_lines(monitor: &JsoncMonitor) -> Vec<Line> {
    let mut lines = Vec::new();
    for (key, value) in [
        ("hang_timeout", monitor.hang_timeout.map(|v| v.to_string())),
        (
            "hang_hysteresis",
            monitor.hang_hysteresis.map(|v| v.to_string()),
        ),
        (
            "max_archived_logs",
            monitor.max_archived_logs.map(|v| v.to_string()),
        ),
        (
            "max_log_file_size",
            monitor.max_log_file_size.map(|v| v.to_string()),
        ),
    ] {
        if let Some(v) = value {
            lines.push(Line::Key(key, v, None));
        }
    }
    if let Some(r) = &monitor.reference_log {
        lines.push(Line::Key(
            "reference_log",
            serde_json::to_string(r).unwrap_or_else(|_| "\"\"".into()),
            None,
        ));
    }
    if let Some(v) = monitor.learner_stage_threshold.filter(|&v| v > 0.0) {
        lines.push(Line::Key("learner_stage_threshold", v.to_string(), None));
    }
    if let Some(v) = monitor.learner_crash_threshold.filter(|&v| v > 0.0) {
        lines.push(Line::Key("learner_crash_threshold", v.to_string(), None));
    }
    if let Some(keywords) = &monitor.crash_keywords {
        lines.push(Line::Key(
            "crash_keywords",
            pretty_array_literal(keywords, 0),
            Some(C_CRASH_KEYWORDS),
        ));
    }
    lines
}

/// Lines for a `flash` section body — only set keys.
fn flash_lines(flash: &JsoncFlash) -> Vec<Line> {
    let mut lines = Vec::new();
    for (key, value) in [
        ("full_image_cmd", &flash.full_image_cmd),
        ("kernel_image_cmd", &flash.kernel_image_cmd),
        ("loader_bin", &flash.loader_bin),
    ] {
        if let Some(v) = value {
            lines.push(Line::Key(
                key,
                serde_json::to_string(v).unwrap_or_else(|_| "\"\"".into()),
                None,
            ));
        }
    }
    lines
}

/// One DUT block at `indent` (closing brace + trailing comma on its own
/// line) — shared by CREATE, UPDATE appends and the canonical render.
pub fn render_dut_block(dut: &JsoncDut, indent: usize) -> String {
    let i = " ".repeat(indent);
    let i2 = " ".repeat(indent + 2);
    let mut lines: Vec<String> = vec![format!("{i}{{")];
    lines.push(key_line(
        &i2,
        "dut_name",
        &serde_json::to_string(&dut.dut_name).unwrap_or_else(|_| "\"\"".into()),
        Some(C_DUT_NAME),
    ));
    if let Some(serial) = &dut.serial {
        lines.push(render_section_block(
            "serial",
            &serial_lines(serial),
            indent + 2,
        ));
    }
    if let Some(target) = &dut.target {
        lines.push(render_section_block(
            "target",
            &target_lines(target),
            indent + 2,
        ));
    }
    if let Some(uboot) = &dut.uboot {
        let mut ls = Vec::new();
        if let Some(c) = &uboot.interrupt_char {
            ls.push(Line::Key(
                "interrupt_char",
                serde_json::to_string(c).unwrap_or_else(|_| "\"\"".into()),
                Some(C_UBOOT_CHAR),
            ));
        }
        if let Some(s) = &uboot.interrupt_strategy {
            ls.push(Line::Key(
                "interrupt_strategy",
                serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into()),
                None,
            ));
        }
        if let Some(pattern) = &uboot.pattern {
            ls.push(Line::Key(
                "pattern",
                serde_json::to_string(pattern).unwrap_or_else(|_| "\"\"".into()),
                None,
            ));
        }
        lines.push(render_section_block("uboot", &ls, indent + 2));
    }
    if let Some(relay) = &dut.relay {
        lines.push(render_section_block(
            "relay",
            &relay_lines(relay),
            indent + 2,
        ));
    }
    if let Some(monitor) = &dut.monitor {
        lines.push(render_section_block(
            "monitor",
            &monitor_lines(monitor),
            indent + 2,
        ));
    }
    if let Some(flash) = &dut.flash {
        lines.push(render_section_block(
            "flash",
            &flash_lines(flash),
            indent + 2,
        ));
    }
    if let Some(control) = &dut.control {
        let mut ls = Vec::new();
        if let Some(dev_ctrl) = &control.dev_ctrl {
            ls.push(Line::Key(
                "dev_ctrl",
                serde_json::to_string(dev_ctrl).unwrap_or_else(|_| "\"\"".into()),
                None,
            ));
        }
        lines.push(render_section_block("control", &ls, indent + 2));
    }
    if let Some(l) = &dut.loader {
        lines.push(key_line(
            &i2,
            "loader",
            &serde_json::to_string(l).unwrap_or_else(|_| "\"\"".into()),
            None,
        ));
    }
    if let Some(r) = &dut.reference_log {
        lines.push(key_line(
            &i2,
            "reference_log",
            &serde_json::to_string(r).unwrap_or_else(|_| "\"\"".into()),
            Some(C_REFERENCE_LOG_DUT),
        ));
    }
    if let Some(d) = &dut.dut_dir {
        lines.push(key_line(
            &i2,
            "dut_dir",
            &serde_json::to_string(d).unwrap_or_else(|_| "\"\"".into()),
            Some(C_DUT_DIR),
        ));
    }
    lines.push(format!("{i}}},"));
    lines.join("\n")
}

/// One dev host block at `indent` (ip/user/pass always real so SET
/// paths are stable; pass renders empty when unset. host_name renders as a
/// real key when set, else a canonical hint).
pub fn render_host_block(host: &JsoncDevHost, indent: usize) -> String {
    let i = " ".repeat(indent);
    let i2 = " ".repeat(indent + 2);
    let mut lines: Vec<String> = vec![format!("{i}{{")];
    lines.push(key_line(
        &i2,
        "ip",
        &serde_json::to_string(host.ip.as_deref().unwrap_or("")).unwrap_or_else(|_| "\"\"".into()),
        Some(C_HOST_IP),
    ));
    lines.push(key_line(
        &i2,
        "user",
        &serde_json::to_string(host.user.as_deref().unwrap_or(""))
            .unwrap_or_else(|_| "\"\"".into()),
        Some(C_HOST_USER),
    ));
    lines.push(key_line(
        &i2,
        "pass",
        &serde_json::to_string(host.pass.as_deref().unwrap_or(""))
            .unwrap_or_else(|_| "\"\"".into()),
        Some(C_HOST_PASS),
    ));
    match &host.host_name {
        Some(n) if !n.is_empty() => lines.push(key_line(
            &i2,
            "host_name",
            &serde_json::to_string(n).unwrap_or_else(|_| "\"\"".into()),
            Some(C_HOST_NAME),
        )),
        _ => lines.push(hint_line(&i2, "\"host_name\": \"\"", Some(C_HOST_NAME))),
    }
    lines.push(format!("{i2}\"duts\": ["));
    for dut in &host.duts {
        lines.push(render_dut_block(dut, indent + 4));
    }
    lines.push(format!("{i2}]"));
    lines.push(format!("{i}}},"));
    lines.join("\n")
}

/// Render the whole canonical document. `with_global_hints` appends the
/// commented-out global-fallback block when no global section is set
/// (CREATE passes true).
pub fn render_jsonc(config: &TargetJsoncConfig, with_global_hints: bool) -> String {
    let mut out = String::from(HEADER);
    out.push_str("{\n");
    out.push_str("  \"dev_hosts\": [\n");
    for host in &config.dev_hosts {
        out.push_str(&render_host_block(host, 4));
        out.push('\n');
    }
    out.push_str("  ],\n");

    let mut any_section = false;
    for section in render_global_section_blocks(config) {
        out.push_str(&section);
        any_section = true;
    }
    if let Some(lock) = &config.lock_dir {
        out.push_str(&key_line(
            "  ",
            "lock_dir",
            &serde_json::to_string(lock).unwrap_or_else(|_| "\"\"".into()),
            Some(C_LOCK_DIR),
        ));
        out.push('\n');
        any_section = true;
    }
    if with_global_hints && !any_section {
        out.push_str(GLOBAL_HINTS);
    }
    out.push_str("}\n");
    out
}

/// Global fallback section blocks (indent 2), plus top-level back-compat
/// reference_log/dut_dir keys. Empty list when nothing is set.
fn render_global_section_blocks(config: &TargetJsoncConfig) -> Vec<String> {
    let mut blocks = Vec::new();
    if let Some(serial) = &config.serial {
        blocks.push(render_section_block("serial", &serial_lines(serial), 2));
    }
    if let Some(target) = &config.target {
        blocks.push(render_section_block("target", &target_lines(target), 2));
    }
    if let Some(uboot) = &config.uboot {
        let mut ls = Vec::new();
        if let Some(c) = &uboot.interrupt_char {
            ls.push(Line::Key(
                "interrupt_char",
                serde_json::to_string(c).unwrap_or_else(|_| "\"\"".into()),
                Some(C_UBOOT_CHAR),
            ));
        }
        if let Some(s) = &uboot.interrupt_strategy {
            ls.push(Line::Key(
                "interrupt_strategy",
                serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into()),
                None,
            ));
        }
        if let Some(pattern) = &uboot.pattern {
            ls.push(Line::Key(
                "pattern",
                serde_json::to_string(pattern).unwrap_or_else(|_| "\"\"".into()),
                None,
            ));
        }
        blocks.push(render_section_block("uboot", &ls, 2));
    }
    if let Some(relay) = &config.relay {
        blocks.push(render_section_block("relay", &relay_lines(relay), 2));
    }
    if let Some(monitor) = &config.monitor {
        blocks.push(render_section_block("monitor", &monitor_lines(monitor), 2));
    }
    if let Some(flash) = &config.flash {
        blocks.push(render_section_block("flash", &flash_lines(flash), 2));
    }
    if let Some(control) = &config.control {
        let mut ls = Vec::new();
        if let Some(dev_ctrl) = &control.dev_ctrl {
            ls.push(Line::Key(
                "dev_ctrl",
                serde_json::to_string(dev_ctrl).unwrap_or_else(|_| "\"\"".into()),
                None,
            ));
        }
        blocks.push(render_section_block("control", &ls, 2));
    }
    if let Some(l) = &config.loader {
        blocks.push(key_line(
            "  ",
            "loader",
            &serde_json::to_string(l).unwrap_or_else(|_| "\"\"".into()),
            None,
        ));
    }
    if let Some(r) = &config.reference_log {
        blocks.push(key_line(
            "  ",
            "reference_log",
            &serde_json::to_string(r).unwrap_or_else(|_| "\"\"".into()),
            Some(C_REFERENCE_LOG_DUT),
        ));
    }
    if let Some(d) = &config.dut_dir {
        blocks.push(key_line(
            "  ",
            "dut_dir",
            &serde_json::to_string(d).unwrap_or_else(|_| "\"\"".into()),
            Some(C_DUT_DIR),
        ));
    }
    blocks
}

// ── Span locator (line-based; file already validated) ────────────────

/// One segment of a key path: root object keys are `Key("dev_hosts")`,
/// array elements are `Index(n)`. The root object itself is the empty path.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PathSeg {
    Key(String),
    Index(usize),
}

/// Everything the locator records in one pass over the text.
#[derive(Debug, Default)]
pub struct Spans {
    /// Value spans (start..end, strings include the quotes) by path.
    pub values: Vec<(Vec<PathSeg>, (usize, usize))>,
    /// Member key spans (name, start..end) by PARENT object path.
    pub keys: Vec<(Vec<PathSeg>, String, (usize, usize))>,
    /// Container open-bracket positions by path.
    pub opens: Vec<(Vec<PathSeg>, usize)>,
    /// Container close-bracket positions by path.
    pub closes: Vec<(Vec<PathSeg>, usize)>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum FrameKind {
    Obj,
    Arr,
}

struct Frame {
    path: Vec<PathSeg>,
    kind: FrameKind,
    /// Key being filled in the current object: (name, token span).
    pending_key: Option<(String, (usize, usize))>,
    /// Arr only: number of elements completed so far (drives Index paths).
    count: usize,
}

/// One pass over already-validated JSONC text recording every value/key/
/// container span. Comment- and string-aware; the file parsed successfully
/// so this only LOCATES (a mis-locate is caught by the re-parse contract).
pub fn scan_jsonc(text: &str) -> Spans {
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut spans = Spans::default();
    let mut stack: Vec<Frame> = Vec::new();
    let mut pos = 0usize;

    while pos < len {
        let b = bytes[pos];
        match b {
            b' ' | b'\t' | b'\r' | b'\n' => pos += 1,
            b'/' if pos + 1 < len && bytes[pos + 1] == b'/' => {
                while pos < len && bytes[pos] != b'\n' {
                    pos += 1;
                }
            }
            b'/' if pos + 1 < len && bytes[pos + 1] == b'*' => {
                pos += 2;
                while pos + 1 < len && !(bytes[pos] == b'*' && bytes[pos + 1] == b'/') {
                    pos += 1;
                }
                pos = (pos + 2).min(len);
            }
            b'{' | b'[' => {
                let kind = if b == b'{' {
                    FrameKind::Obj
                } else {
                    FrameKind::Arr
                };
                let path = match stack.last_mut() {
                    Some(top) => match top.kind {
                        FrameKind::Obj => {
                            // A container VALUE: the pending key names it and
                            // is consumed here (the value ends when the
                            // container closes).
                            let key = top
                                .pending_key
                                .take()
                                .map(|(name, _)| name)
                                .unwrap_or_default();
                            let mut p = top.path.clone();
                            p.push(PathSeg::Key(key));
                            p
                        }
                        FrameKind::Arr => {
                            let mut p = top.path.clone();
                            p.push(PathSeg::Index(top.count));
                            p
                        }
                    },
                    None => Vec::new(), // root container
                };
                spans.opens.push((path.clone(), pos));
                stack.push(Frame {
                    path,
                    kind,
                    pending_key: None,
                    count: 0,
                });
                pos += 1;
            }
            b'}' | b']' => {
                if let Some(frame) = stack.pop() {
                    spans.closes.push((frame.path.clone(), pos));
                    if let Some(top) = stack.last_mut()
                        && top.kind == FrameKind::Arr
                    {
                        // A container value just ended inside an array.
                        top.count += 1;
                    }
                }
                pos += 1;
            }
            b'"' => {
                let start = pos;
                pos += 1;
                while pos < len {
                    if bytes[pos] == b'\\' {
                        pos += 2;
                        continue;
                    }
                    if bytes[pos] == b'"' {
                        pos += 1;
                        break;
                    }
                    pos += 1;
                }
                let end = pos;
                let inner = unescape_json_string(&text[start + 1..end.saturating_sub(1)]);
                if let Some(top) = stack.last_mut() {
                    match top.kind {
                        FrameKind::Obj => {
                            if let Some((key, _)) = top.pending_key.take() {
                                // String VALUE for the pending key.
                                let mut p = top.path.clone();
                                p.push(PathSeg::Key(key));
                                spans.values.push((p, (start, end)));
                            } else {
                                // A KEY token.
                                let mut p = top.path.clone();
                                p.push(PathSeg::Key(inner.clone()));
                                spans.keys.push((p, inner.clone(), (start, end)));
                                top.pending_key = Some((inner, (start, end)));
                            }
                        }
                        FrameKind::Arr => {
                            let mut p = top.path.clone();
                            p.push(PathSeg::Index(top.count));
                            spans.values.push((p, (start, end)));
                            top.count += 1;
                        }
                    }
                }
            }
            b':' => pos += 1,
            b',' => pos += 1, // array counts already bumped when elements end
            b'-' | b'0'..=b'9' => {
                let start = pos;
                while pos < len && is_number_byte(bytes[pos]) {
                    pos += 1;
                }
                record_scalar(&mut spans, &mut stack, start, pos);
            }
            b't' | b'f' | b'n' => {
                let start = pos;
                for word in [&b"true"[..], &b"false"[..], &b"null"[..]] {
                    if bytes[pos..].starts_with(word) {
                        pos += word.len();
                        break;
                    }
                }
                record_scalar(&mut spans, &mut stack, start, pos);
            }
            _ => pos += 1, // validated file — tolerate anything else
        }
    }
    spans
}

fn is_number_byte(b: u8) -> bool {
    b.is_ascii_digit() || matches!(b, b'-' | b'+' | b'.' | b'e' | b'E')
}

/// Record a scalar (number/bool/null) value span at the current position.
fn record_scalar(spans: &mut Spans, stack: &mut [Frame], start: usize, end: usize) {
    let Some(top) = stack.last_mut() else {
        return;
    };
    match top.kind {
        FrameKind::Obj => {
            if let Some((key, _)) = top.pending_key.take() {
                let mut p = top.path.clone();
                p.push(PathSeg::Key(key));
                spans.values.push((p, (start, end)));
            }
        }
        FrameKind::Arr => {
            let mut p = top.path.clone();
            p.push(PathSeg::Index(top.count));
            spans.values.push((p, (start, end)));
            top.count += 1;
        }
    }
}

/// Minimal JSON string unescape for key names (\\\" \\\\ \\/ \\n \\r \\t \\uXXXX).
fn unescape_json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'"' => out.push('"'),
                b'\\' => out.push('\\'),
                b'/' => out.push('/'),
                b'n' => out.push('\n'),
                b'r' => out.push('\r'),
                b't' => out.push('\t'),
                b'u' => {
                    if i + 5 < bytes.len()
                        && let Ok(hex) = std::str::from_utf8(&bytes[i + 2..i + 6])
                        && let Ok(code) = u32::from_str_radix(hex, 16)
                    {
                        if let Some(c) = char::from_u32(code) {
                            out.push(c);
                        }
                        i += 6;
                        continue;
                    }
                    out.push('u');
                }
                _ => out.push(bytes[i + 1] as char),
            }
            i += 2;
        } else {
            // Assumes ASCII-compatible UTF-8 for non-escape bytes (keys are).
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

/// Value span for `path`, or None.
pub fn locate_value(text: &str, path: &[PathSeg]) -> Option<(usize, usize)> {
    scan_jsonc(text)
        .values
        .into_iter()
        .find(|(p, _)| p == path)
        .map(|(_, span)| span)
}

/// Container open/close bracket positions for `path` (root = empty path).
pub fn locate_container_open(text: &str, path: &[PathSeg]) -> Option<usize> {
    scan_jsonc(text)
        .opens
        .into_iter()
        .find(|(p, _)| p == path)
        .map(|(_, pos)| pos)
}

pub fn locate_container_close(text: &str, path: &[PathSeg]) -> Option<usize> {
    scan_jsonc(text)
        .closes
        .into_iter()
        .find(|(p, _)| p == path)
        .map(|(_, pos)| pos)
}

/// Leading whitespace of the line containing `pos`.
/// Leading spaces/tabs of the line containing `pos` (callers pass bracket
/// positions — the first non-whitespace char on their line).
fn leading_ws_of_line(text: &str, pos: usize) -> String {
    let line_start = text[..pos].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let bytes = text.as_bytes();
    let mut end = line_start;
    while end < pos && matches!(bytes[end], b' ' | b'\t') {
        end += 1;
    }
    text[line_start..end].to_string()
}

/// Indent for entries of the container at `path` (its open line indent + 2).
fn container_entry_indent(text: &str, path: &[PathSeg]) -> Result<usize, String> {
    let open = locate_container_open(text, path)
        .ok_or_else(|| format!("cannot locate container at {path:?}"))?;
    Ok(leading_ws_of_line(text, open).len() + 2)
}

/// Indent for members of the object at `path` (its open line indent + 2).
fn container_member_indent(text: &str, path: &[PathSeg]) -> Result<usize, String> {
    container_entry_indent(text, path)
}

/// Skip whitespace and comments BACKWARDS from just before `i` (which must
/// be >= `open`); returns the previous significant byte inside the
/// container, or None when the container is empty. Best-effort on block
/// comments (an "/*" inside a string value can fool it — the re-parse
/// contract catches any resulting mis-splice).
fn prev_significant(text: &str, open: usize, mut i: usize) -> Option<u8> {
    let bytes = text.as_bytes();
    loop {
        while i > open && matches!(bytes[i - 1], b' ' | b'\t' | b'\r' | b'\n') {
            i -= 1;
        }
        // Empty container: the only byte left is the open bracket itself.
        if i <= open || bytes[i - 1] == b'[' || bytes[i - 1] == b'{' {
            return None;
        }
        // Block-comment tail "*/"?
        if bytes[i - 1] == b'/' && i >= open + 2 && bytes[i - 2] == b'*' {
            if let Some(head) = text[..i].rfind("/*")
                && head >= open
            {
                i = head;
                continue;
            }
            return Some(b'/');
        }
        // Line-comment tail: walk to the previous newline.
        if bytes[i - 1] == b'/' && i >= open + 2 && bytes[i - 2] == b'/' {
            let mut j = i;
            while j > open && bytes[j - 1] != b'\n' {
                j -= 1;
            }
            if j > open {
                i = j;
                continue;
            }
            return None;
        }
        // The candidate byte may sit INSIDE a trailing line comment (the
        // last member line can end `,  // note`): jump to the comment's
        // `//` so the branch above skips the whole line.
        if let Some(comment_pos) = line_comment_start(text, i, open) {
            i = comment_pos;
            continue;
        }
        return Some(bytes[i - 1]);
    }
}

/// Position of the first `//` outside string literals on the line that
/// contains `pos` (bounded to `open`), or None. Backslash escapes inside
/// strings are best-effort (the re-parse contract catches mis-splices).
fn line_comment_start(text: &str, pos: usize, open: usize) -> Option<usize> {
    let line_start = text[..pos]
        .rfind('\n')
        .map(|p| p + 1)
        .unwrap_or(0)
        .max(open);
    let line = &text[line_start..pos];
    let bytes = line.as_bytes();
    let mut in_str = false;
    let mut j = 0;
    while j < bytes.len() {
        match bytes[j] {
            b'"' if j == 0 || bytes[j - 1] != b'\\' => in_str = !in_str,
            b'/' if !in_str && j + 1 < bytes.len() && bytes[j + 1] == b'/' => {
                return Some(line_start + j);
            }
            _ => {}
        }
        j += 1;
    }
    None
}

// ── Edit ops ──────────────────────────────────────────────────────────

/// One text-level edit. Every op is followed by a strict re-parse
/// (`apply_edits`); on failure the whole sequence reverts (the caller keeps
/// the original text and the wizard returns Err — never partial output).
#[derive(Debug, Clone, PartialEq)]
pub enum EditOp {
    /// Replace the value span at `path` with `literal` bytes (trailing
    /// comma and same-line comment untouched).
    SetScalar { path: Vec<PathSeg>, literal: String },
    /// Splice `block` (pre-indented, ending with a trailing comma) before
    /// the closing bracket of the container at `path`.
    AppendBlock { path: Vec<PathSeg>, block: String },
    /// Splice `block` (pre-indented, ending with a trailing comma) before
    /// the line of member `key` in the object at `path` — keeps a new host
    /// scalar (ip/user/pass/host_name) above the `duts` member in canonical
    /// order instead of appending after the array.
    InsertBefore {
        path: Vec<PathSeg>,
        key: String,
        block: String,
    },
    /// Remove member `key` (its whole line range) from the object at `path`
    /// — used when a zero answer means "inherit".
    RemoveKey { path: Vec<PathSeg>, key: String },
    /// Replace the WHOLE container (array/object) at `path` with
    /// `literal` bytes — the zero-host end state writes `[]` over the
    /// entire dev_hosts array (no host or DUT data survives).
    ReplaceContainer { path: Vec<PathSeg>, literal: String },
}

impl EditOp {
    fn path(&self) -> &[PathSeg] {
        match self {
            EditOp::SetScalar { path, .. }
            | EditOp::AppendBlock { path, .. }
            | EditOp::InsertBefore { path, .. }
            | EditOp::RemoveKey { path, .. }
            | EditOp::ReplaceContainer { path, .. } => path,
        }
    }
}

/// Apply one op to `text` (no re-parse — `apply_edits` does that).
pub fn apply_edit(text: &str, op: &EditOp) -> Result<String, String> {
    match op {
        EditOp::SetScalar { path, literal } => {
            let (start, end) = locate_value(text, path)
                .ok_or_else(|| format!("cannot locate value at {path:?}"))?;
            Ok(format!("{}{}{}", &text[..start], literal, &text[end..]))
        }
        EditOp::AppendBlock { path, block } => {
            let close = locate_container_close(text, path)
                .ok_or_else(|| format!("cannot locate container at {path:?}"))?;
            let open = locate_container_open(text, path)
                .ok_or_else(|| format!("cannot locate container open at {path:?}"))?;
            let (prefix, suffix) = match prev_significant(text, open, close) {
                None => {
                    // Empty container: bracket stays on its own line.
                    let indent = leading_ws_of_line(text, close);
                    (
                        format!("{}{}", &text[..close], ""),
                        format!("\n{block}\n{indent}{}", &text[close..]),
                    )
                }
                Some(b',') => (
                    text[..close].to_string(),
                    format!("{block}\n{}", &text[close..]),
                ),
                Some(_) => (
                    text[..close].to_string(),
                    format!(",\n{block}\n{}", &text[close..]),
                ),
            };
            Ok(format!("{prefix}{suffix}"))
        }
        EditOp::InsertBefore { path, key, block } => {
            let spans = scan_jsonc(text);
            let mut key_path = path.clone();
            key_path.push(PathSeg::Key(key.clone()));
            let start = spans
                .keys
                .iter()
                .find(|(p, name, _)| p == &key_path && name == key)
                .map(|(_, _, span)| span.0)
                .ok_or_else(|| format!("cannot locate member {key:?} in {path:?}"))?;
            let line_start = text[..start].rfind('\n').map(|i| i + 1).unwrap_or(0);
            let on_own_line = text[line_start..start]
                .bytes()
                .all(|b| matches!(b, b' ' | b'\t'));
            if !on_own_line {
                // Compact single-line file: a mid-line splice would put the
                // member's trailing comment inside the line and swallow what
                // follows — degrade to the plain append (valid, comment kept).
                return apply_edit(
                    text,
                    &EditOp::AppendBlock {
                        path: path.clone(),
                        block: block.clone(),
                    },
                );
            }
            // Hand-written files may omit the previous member's trailing
            // comma — restore it at the end of the previous line.
            let comma_fix = match locate_container_open(text, path) {
                Some(open) => {
                    matches!(prev_significant(text, open, line_start), Some(b) if b != b',')
                }
                None => false,
            };
            let prefix_end = if comma_fix {
                line_start - 1
            } else {
                line_start
            };
            Ok(format!(
                "{}{}{}\n{}",
                &text[..prefix_end],
                if comma_fix { "," } else { "" },
                block,
                &text[line_start..]
            ))
        }
        EditOp::ReplaceContainer { path, literal } => {
            let open = locate_container_open(text, path)
                .ok_or_else(|| format!("cannot locate container at {path:?}"))?;
            let close = locate_container_close(text, path)
                .ok_or_else(|| format!("cannot locate container at {path:?}"))?;
            Ok(format!(
                "{}{}{}",
                &text[..open],
                literal,
                &text[close + 1..]
            ))
        }
        EditOp::RemoveKey { path, key } => {
            let spans = scan_jsonc(text);
            let mut key_path = path.clone();
            key_path.push(PathSeg::Key(key.clone()));
            let key_span = spans
                .keys
                .iter()
                .find(|(p, name, _)| p == &key_path && name == key)
                .map(|(_, _, span)| *span)
                .ok_or_else(|| format!("cannot locate key {key:?} in {path:?}"))?;
            let value_span = spans
                .values
                .iter()
                .find(|(p, _)| p == &key_path)
                .map(|(_, span)| *span)
                .ok_or_else(|| format!("cannot locate value of {key:?} in {path:?}"))?;
            // Remove the member's whole line range (key line through the
            // newline after the value — the member's own trailing comment
            // goes with it).
            let line_start = text[..key_span.0].rfind('\n').map(|i| i + 1).unwrap_or(0);
            let line_end = text[value_span.1..]
                .find('\n')
                .map(|i| value_span.1 + i + 1)
                .unwrap_or(text.len());
            Ok(format!("{}{}", &text[..line_start], &text[line_end..]))
        }
    }
}

/// Apply a sequence of ops, re-parsing with the strict options after EVERY
/// op. Any parse failure reverts the whole sequence (returns Err; the input
/// `text` is untouched — the wizard never writes partial output).
pub fn apply_edits(text: &str, ops: &[EditOp]) -> Result<String, String> {
    let mut current = text.to_string();
    for op in ops {
        current = apply_edit(&current, op)?;
        if let Err(e) = crate::config::parse_jsonc_text(&current) {
            return Err(format!(
                "edit at {:?} produced invalid JSONC ({e}); all edits reverted",
                op.path()
            ));
        }
    }
    Ok(current)
}

// ── Update op computation ─────────────────────────────────────────────

/// An edit plus its human diff-summary line.
#[derive(Debug)]
struct OpWithDesc {
    op: EditOp,
    desc: String,
}

fn seg_key(s: &str) -> PathSeg {
    PathSeg::Key(s.to_string())
}

/// One scalar diff candidate — what to write, and where a NEW key lands.
struct ScalarEdit<'a> {
    key: &'a str,
    literal: &'a str,
    comment: Option<&'a str>,
    desc: String,
    /// Splice a NEW key above this anchor member instead of after the last
    /// one (host scalars land above `duts` in canonical order).
    before: Option<&'static str>,
}

/// Set the key's value when it exists, append the member when it doesn't
/// (both comment-preserving: a SET keeps the same-line comment, an APPEND
/// carries the canonical comment).
fn set_or_append_key(
    text: &str,
    ops: &mut Vec<OpWithDesc>,
    container: &[PathSeg],
    edit: ScalarEdit<'_>,
) -> Result<(), String> {
    let ScalarEdit {
        key,
        literal,
        comment,
        desc,
        before,
    } = edit;
    let mut key_path = container.to_vec();
    key_path.push(seg_key(key));
    if locate_value(text, &key_path).is_some() {
        ops.push(OpWithDesc {
            op: EditOp::SetScalar {
                path: key_path,
                literal: literal.to_string(),
            },
            desc,
        });
    } else if locate_container_open(text, &key_path).is_some() {
        // The existing value is an ARRAY/OBJECT (e.g. crash_keywords) —
        // scalars have value spans, containers do not. Replace the whole
        // container (same-line trailing comment preserved).
        ops.push(OpWithDesc {
            op: EditOp::ReplaceContainer {
                path: key_path,
                literal: literal.to_string(),
            },
            desc,
        });
    } else {
        let member_indent = container_member_indent(text, container)?;
        let member = key_line(&" ".repeat(member_indent), key, literal, comment);
        // Fall back to a plain append when the anchor member does not exist
        // (a host without a `duts` member still takes the new scalar).
        let anchored = match before {
            Some(anchor) => {
                let mut anchor_path = container.to_vec();
                anchor_path.push(seg_key(anchor));
                locate_value(text, &anchor_path).is_some()
                    || locate_container_open(text, &anchor_path).is_some()
            }
            None => false,
        };
        ops.push(OpWithDesc {
            op: match (anchored, before) {
                (true, Some(anchor)) => EditOp::InsertBefore {
                    path: container.to_vec(),
                    key: anchor.to_string(),
                    block: member,
                },
                _ => EditOp::AppendBlock {
                    path: container.to_vec(),
                    block: member,
                },
            },
            desc,
        });
    }
    Ok(())
}

/// Append a whole single-field section block to `container` (used when the
/// section object itself does not exist yet). `key` is in advanced-key form
/// ("section.field", e.g. "serial.port").
fn append_section_block(
    text: &str,
    ops: &mut Vec<OpWithDesc>,
    container: &[PathSeg],
    key: &str,
    literal: &str,
    comment: Option<&'static str>,
    desc: String,
) -> Result<(), String> {
    let (section, field) = advanced_field(key);
    let indent = container_member_indent(text, container)?;
    let body = vec![Line::Key(
        field_to_static(field)?,
        literal.to_string(),
        comment,
    )];
    let block = render_section_block(section, &body, indent);
    ops.push(OpWithDesc {
        op: EditOp::AppendBlock {
            path: container.to_vec(),
            block,
        },
        desc,
    });
    Ok(())
}

/// `Line::Key` needs a &'static key; the section field names all live
/// forever (they come from the ADVANCED_FIELDS table and fixed comparers).
fn field_to_static(field: &str) -> Result<&'static str, String> {
    match field {
        "port" => Ok("port"),
        "baudrate" => Ok("baudrate"),
        "rfc2217" => Ok("rfc2217"),
        "ip" => Ok("ip"),
        "login_user" => Ok("login_user"),
        "login_pass" => Ok("login_pass"),
        "login_prompt" => Ok("login_prompt"),
        "interrupt_char" => Ok("interrupt_char"),
        "interrupt_strategy" => Ok("interrupt_strategy"),
        "pattern" => Ok("pattern"),
        "type" => Ok("type"),
        "reset_ch" => Ok("reset_ch"),
        "download_ch" => Ok("download_ch"),
        "power_ch" => Ok("power_ch"),
        "reset_time_ms" => Ok("reset_time_ms"),
        "power_off_time_ms" => Ok("power_off_time_ms"),
        "hang_timeout" => Ok("hang_timeout"),
        "hang_hysteresis" => Ok("hang_hysteresis"),
        "max_archived_logs" => Ok("max_archived_logs"),
        "max_log_file_size" => Ok("max_log_file_size"),
        "reference_log" => Ok("reference_log"),
        "learner_stage_threshold" => Ok("learner_stage_threshold"),
        "learner_crash_threshold" => Ok("learner_crash_threshold"),
        "crash_keywords" => Ok("crash_keywords"),
        "tool" => Ok("tool"),
        "full_image_cmd" => Ok("full_image_cmd"),
        "kernel_image_cmd" => Ok("kernel_image_cmd"),
        "loader_bin" => Ok("loader_bin"),
        "loader_cmd" => Ok("loader_cmd"),
        "list_devices_cmd" => Ok("list_devices_cmd"),
        "upload_dir" => Ok("upload_dir"),
        "dev_ctrl" => Ok("dev_ctrl"),
        "loader" => Ok("loader"),
        "lock_dir" => Ok("lock_dir"),
        _ => Err(format!("unknown field: {field}")),
    }
}

/// Diff the wizard answers against the existing config and produce the edit
/// ops. Only DIFFERENCES become ops; existing entries beyond the answered
/// counts are left untouched (announced by the caller's summary).
fn compute_update_ops(
    text: &str,
    existing: &TargetJsoncConfig,
    answers: &WizardAnswers,
) -> Result<Vec<OpWithDesc>, String> {
    let mut ops: Vec<OpWithDesc> = Vec::new();
    let dev_hosts_path = [seg_key("dev_hosts")];
    if answers.hosts.is_empty() && !existing.dev_hosts.is_empty() {
        // Zero dev hosts => zero DUTs: the whole dev_hosts array is
        // replaced with []. No host or DUT data survives a reduction to 0.
        ops.push(OpWithDesc {
            op: EditOp::ReplaceContainer {
                path: dev_hosts_path.to_vec(),
                literal: "[]".into(),
            },
            desc: "zero dev hosts — dev_hosts cleared (all host/DUT data dropped)".into(),
        });
        return Ok(ops);
    }
    // The effective GLOBAL fallback values (per-DUT answers equal to them
    // write nothing — inheriting the global section is the absence of an
    // override).
    let current = advanced_current(existing);
    for (hi, host_answers) in answers.hosts.iter().enumerate() {
        let mut host_path = dev_hosts_path.to_vec();
        host_path.push(PathSeg::Index(hi));
        match existing.dev_hosts.get(hi) {
            None => {
                let host = host_jsonc(host_answers)?;
                let entry_indent = container_entry_indent(text, &dev_hosts_path)?;
                let block = render_host_block(&host, entry_indent);
                ops.push(OpWithDesc {
                    op: EditOp::AppendBlock {
                        path: dev_hosts_path.to_vec(),
                        block,
                    },
                    desc: format!("added host '{}' at dev_hosts[{hi}]", host_answers.ip),
                });
            }
            Some(ex_host) => {
                compare_host(text, &mut ops, &host_path, ex_host, host_answers)?;
                let mut duts_path = host_path.clone();
                duts_path.push(seg_key("duts"));
                for (di, dut_answers) in host_answers.duts.iter().enumerate() {
                    let mut dut_path = host_path.clone();
                    dut_path.push(seg_key("duts"));
                    dut_path.push(PathSeg::Index(di));
                    match ex_host.duts.get(di) {
                        None => {
                            let dut = dut_jsonc(dut_answers)?;
                            let entry_indent = container_entry_indent(text, &duts_path)?;
                            let block = render_dut_block(&dut, entry_indent);
                            ops.push(OpWithDesc {
                                op: EditOp::AppendBlock {
                                    path: duts_path.clone(),
                                    block,
                                },
                                desc: format!(
                                    "added DUT '{}' at dev_hosts[{hi}].duts[{di}]",
                                    dut.dut_name
                                ),
                            });
                        }
                        Some(ex_dut) => {
                            compare_dut(text, &mut ops, &dut_path, ex_dut, dut_answers, &current)?
                        }
                    }
                }
            }
        }
    }

    // HOST-LEVEL advanced group. Keys are grouped by section so a
    // previously-absent section yields ONE block carrying ALL its keys —
    // emitting a separate `"section": { ... }` object per key would
    // duplicate the field and the strict re-parse (after every op) would
    // abort the whole update.
    let mut by_section: Vec<(String, Vec<(&String, &String)>)> = Vec::new();
    for (key, value) in &answers.advanced {
        let section = advanced_field(key).0;
        match by_section.iter_mut().find(|(s, _)| s == section) {
            Some((_, keys)) => keys.push((key, value)),
            None => by_section.push((section.to_string(), vec![(key, value)])),
        }
    }
    for (section, keys) in &by_section {
        let container: Vec<PathSeg> = if section.is_empty() {
            Vec::new()
        } else {
            vec![seg_key(section)]
        };
        if section.is_empty() || locate_container_close(text, &container).is_some() {
            // Section exists (or the key lives at the root): per-key
            // set/remove/skip, exactly as before.
            for (key, value) in keys {
                let (_, field) = advanced_field(key);
                let literal = advanced_literal(key, value)?;
                let desc = format!("{key}: {}", value.trim());
                match literal {
                    None => {
                        // "unset/inherit" answer: drop the key when it exists.
                        let mut key_path = container.clone();
                        key_path.push(seg_key(field));
                        if locate_value(text, &key_path).is_some() {
                            ops.push(OpWithDesc {
                                op: EditOp::RemoveKey {
                                    path: container.clone(),
                                    key: field.to_string(),
                                },
                                desc: format!("unset {key} (inherit)"),
                            });
                        }
                    }
                    Some(lit) => {
                        if current.get(*key).is_some_and(|c| c == value.as_str()) {
                            continue; // unchanged
                        }
                        set_or_append_key(
                            text,
                            &mut ops,
                            &container,
                            ScalarEdit {
                                key: field,
                                literal: &lit,
                                comment: comment_for(key),
                                desc,
                                before: None,
                            },
                        )?;
                    }
                }
            }
        } else {
            // Section absent: ONE block with every SET key ("unset" answers
            // have nothing to remove from a section that does not exist).
            let indent = container_member_indent(text, &[])?;
            let mut lines: Vec<Line> = Vec::new();
            let mut descs: Vec<String> = Vec::new();
            for (key, value) in keys {
                let (_, field) = advanced_field(key);
                if let Some(lit) = advanced_literal(key, value)? {
                    if current.get(*key).is_some_and(|c| c == value.as_str()) {
                        continue; // unchanged
                    }
                    lines.push(Line::Key(field_to_static(field)?, lit, comment_for(key)));
                    descs.push(format!("{key}: {}", value.trim()));
                }
            }
            if !lines.is_empty() {
                ops.push(OpWithDesc {
                    op: EditOp::AppendBlock {
                        path: Vec::new(),
                        block: render_section_block(section, &lines, indent),
                    },
                    desc: descs.join("; "),
                });
            }
        }
    }
    Ok(ops)
}

/// Host answers → schema struct (for APPEND blocks).
fn host_jsonc(host: &HostAnswers) -> Result<JsoncDevHost, String> {
    Ok(JsoncDevHost {
        host_name: Some(host.host_name.clone()).filter(|s| !s.is_empty()),
        ip: Some(host.ip.clone()),
        user: Some(host.user.clone()),
        pass: Some(host.pass.clone()),
        duts: host.duts.iter().map(dut_jsonc).collect::<Result<_, _>>()?,
    })
}

/// DUT answers → schema struct (for APPEND blocks).
fn dut_jsonc(dut: &DutAnswers) -> Result<JsoncDut, String> {
    let mut dutc = JsoncDut {
        dut_name: dut.dut_name.clone(),
        serial: Some(JsoncSerial {
            port: Some(parse_number_or_string(&dut.serial_port)?),
            baudrate: None,
            rfc2217: None,
            tx_repeat: None,
            tx_gap_ms: None,
            backspace: None,
            enter: None,
            rx_newline: None,
            highlight: None,
            mouse: None,
        }),
        target: Some(JsoncTarget {
            login_user: Some(dut.login_user.clone()),
            login_pass: Some(String::new()),
            login_prompt: None,
        }),
        relay: relay_from_answers(dut)?,
        uboot: None,
        monitor: None,
        flash: None,
        download: None,
        control: None,
        loader: None,
        reference_log: None,
        dut_dir: None,
    };
    merge_dut_advanced(&mut dutc, &dut.advanced)?;
    Ok(dutc)
}

/// Scalar diffs for an EXISTING host (ip/user/pass/host_name).
fn compare_host(
    text: &str,
    ops: &mut Vec<OpWithDesc>,
    host_path: &[PathSeg],
    ex: &JsoncDevHost,
    answers: &HostAnswers,
) -> Result<(), String> {
    let json_str = |s: &str| serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into());
    for (key, old, new, comment) in [
        (
            "ip",
            ex.ip.clone().unwrap_or_default(),
            answers.ip.clone(),
            Some(C_HOST_IP),
        ),
        (
            "user",
            ex.user.clone().unwrap_or_default(),
            answers.user.clone(),
            Some(C_HOST_USER),
        ),
        (
            "pass",
            ex.pass.clone().unwrap_or_default(),
            answers.pass.clone(),
            Some(C_HOST_PASS),
        ),
        (
            "host_name",
            ex.host_name.clone().unwrap_or_default(),
            answers.host_name.clone(),
            Some(C_HOST_NAME),
        ),
    ] {
        if old == new {
            continue;
        }
        let desc = format!("dev_hosts[..].{key}: {old} -> {new}");
        set_or_append_key(
            text,
            ops,
            host_path,
            ScalarEdit {
                key,
                literal: &json_str(&new),
                comment,
                desc,
                // A missing host scalar must land ABOVE the `duts` member
                // (canonical order), not after the array.
                before: Some("duts"),
            },
        )?;
    }
    Ok(())
}

/// One relay sub-field diff candidate: (key, existing, answer, default,
/// comment).
struct RelayChange {
    key: &'static str,
    existing: Option<String>,
    answer: String,
    default: &'static str,
    comment: Option<&'static str>,
}

/// Scalar diffs for an EXISTING DUT (dut_name, serial.port, login_user,
/// relay.type — the quick set; sections missing on the existing DUT are
/// appended as whole single-field section blocks) plus the DUT-OWNED
/// advanced diff (per-DUT storage).
fn compare_dut(
    text: &str,
    ops: &mut Vec<OpWithDesc>,
    dut_path: &[PathSeg],
    ex: &JsoncDut,
    answers: &DutAnswers,
    global_current: &HashMap<String, String>,
) -> Result<(), String> {
    let json_str = |s: &str| serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into());
    if ex.dut_name != answers.dut_name {
        set_or_append_key(
            text,
            ops,
            dut_path,
            ScalarEdit {
                key: "dut_name",
                literal: &json_str(&answers.dut_name),
                comment: Some(C_DUT_NAME),
                desc: format!("DUT name: {} -> {}", ex.dut_name, answers.dut_name),
                before: None,
            },
        )?;
    }
    // serial.port
    let ex_port = ex
        .serial
        .as_ref()
        .and_then(|s| s.port.as_ref())
        .map(NumberOrString::to_value_string);
    if ex_port.as_deref() != Some(answers.serial_port.as_str()) {
        let literal = json_literal(&parse_number_or_string(&answers.serial_port)?);
        let mut serial_path = dut_path.to_vec();
        serial_path.push(seg_key("serial"));
        let desc = format!(
            "serial.port: {} -> {}",
            ex_port.unwrap_or_default(),
            answers.serial_port
        );
        if locate_container_close(text, &serial_path).is_some() {
            set_or_append_key(
                text,
                ops,
                &serial_path,
                ScalarEdit {
                    key: "port",
                    literal: &literal,
                    comment: Some(C_PORT),
                    desc,
                    before: None,
                },
            )?;
        } else {
            append_section_block(
                text,
                ops,
                dut_path,
                "serial.port",
                &literal,
                Some(C_PORT),
                desc,
            )?;
        }
    }
    // target.login_user — a missing section already MEANS the runtime
    // default ("root"), so an answer equal to the default is a no-op, not
    // an append (the wizard never inserts noise sections).
    let ex_user = ex.target.as_ref().and_then(|t| t.login_user.clone());
    let user_differs = match &ex_user {
        Some(u) => u != &answers.login_user,
        None => answers.login_user != "root",
    };
    if user_differs {
        let mut target_path = dut_path.to_vec();
        target_path.push(seg_key("target"));
        let desc = format!(
            "login_user: {} -> {}",
            ex_user.unwrap_or_default(),
            answers.login_user
        );
        if locate_container_close(text, &target_path).is_some() {
            set_or_append_key(
                text,
                ops,
                &target_path,
                ScalarEdit {
                    key: "login_user",
                    literal: &json_str(&answers.login_user),
                    comment: Some(C_LOGIN_USER),
                    desc,
                    before: None,
                },
            )?;
        } else {
            append_section_block(
                text,
                ops,
                dut_path,
                "target.login_user",
                &json_str(&answers.login_user),
                Some(C_LOGIN_USER),
                desc,
            )?;
        }
    }
    // relay — gate OFF leaves any existing relay section untouched
    // (append-only update: the written file keeps the section); gate ON
    // diffs the sub-fields with default-equivalence ("ch340" type /
    // zero-guarded numerics) — an unset answer ("0"/"") drops the key.
    if answers.relay_gate {
        let ex_relay = ex.relay.as_ref();
        let mut relay_path = dut_path.to_vec();
        relay_path.push(seg_key("relay"));
        let relay_exists = locate_container_close(text, &relay_path).is_some();
        // (key, existing stringified, answer, default, comment)
        let mut changes: Vec<RelayChange> = Vec::new();
        let num = |v: u8| v.to_string();
        let num16 = |v: u16| v.to_string();
        let num64 = |v: u64| v.to_string();
        changes.push(RelayChange {
            key: "type",
            existing: ex_relay.and_then(|r| r.relay_type.clone()),
            answer: answers.relay_type.clone(),
            default: "ch340",
            comment: Some(C_RELAY_TYPE),
        });
        changes.push(RelayChange {
            key: "port",
            existing: ex_relay.and_then(|r| r.port.map(num16)),
            answer: answers.relay_port.clone(),
            default: "0",
            comment: None,
        });
        changes.push(RelayChange {
            key: "reset_ch",
            existing: ex_relay.and_then(|r| r.reset_ch.map(num)),
            answer: answers.relay_reset_ch.clone(),
            default: "0",
            comment: Some(C_RELAY_CH_ALIASES),
        });
        changes.push(RelayChange {
            key: "download_ch",
            existing: ex_relay.and_then(|r| r.download_ch.map(num)),
            answer: answers.relay_download_ch.clone(),
            default: "0",
            comment: None,
        });
        changes.push(RelayChange {
            key: "power_ch",
            existing: ex_relay.and_then(|r| r.power_ch.map(num)),
            answer: answers.relay_power_ch.clone(),
            default: "0",
            comment: Some(C_POWER_CH),
        });
        changes.push(RelayChange {
            key: "reset_time_ms",
            existing: ex_relay.and_then(|r| r.reset_time_ms.map(num64)),
            answer: answers.relay_reset_time_ms.clone(),
            default: "0",
            comment: Some(C_RESET_TIME),
        });
        changes.push(RelayChange {
            key: "power_off_time_ms",
            existing: ex_relay.and_then(|r| r.power_off_time_ms.map(num64)),
            answer: answers.relay_power_off_time_ms.clone(),
            default: "3000",
            comment: Some(C_POWER_OFF),
        });
        // Compute the diff once: (key, Some(literal), desc) = set,
        // (key, desc) = unset/inherit (drop the key when it exists).
        let mut sets: Vec<(&'static str, String, Option<&'static str>, String)> = Vec::new();
        let mut unsets: Vec<(&'static str, String)> = Vec::new();
        for c in changes {
            let t = c.answer.trim();
            if c.existing.is_none() && (t == c.default || t.is_empty()) {
                continue; // absent + default-equivalent → nothing to write
            }
            if c.existing.as_deref() == Some(t) {
                continue; // unchanged
            }
            let desc = format!(
                "relay.{}: {} -> {}",
                c.key,
                c.existing.as_deref().unwrap_or_default(),
                if t.is_empty() { "(inherit)" } else { t }
            );
            let literal = if c.key == "type" {
                Some(json_str(t))
            } else {
                // Zero-guarded numeric: ""/"0" → unset/inherit.
                advanced_literal(&format!("relay.{}", c.key), t)?
            };
            match literal {
                None => {
                    if c.existing.is_some() {
                        unsets.push((c.key, desc));
                    }
                }
                Some(lit) => sets.push((c.key, lit, c.comment, desc)),
            }
        }
        if relay_exists {
            // Existing relay section: per-key set / remove.
            for (key, desc) in unsets {
                ops.push(OpWithDesc {
                    op: EditOp::RemoveKey {
                        path: relay_path.clone(),
                        key: key.to_string(),
                    },
                    desc,
                });
            }
            for (key, lit, comment, desc) in sets {
                set_or_append_key(
                    text,
                    ops,
                    &relay_path,
                    ScalarEdit {
                        key,
                        literal: &lit,
                        comment,
                        desc,
                        before: None,
                    },
                )?;
            }
        } else if !sets.is_empty() {
            // Section absent: ONE block with every set key (a separate
            // single-field relay object per key would duplicate the field
            // and the strict re-parse would abort the whole update).
            let indent = container_member_indent(text, dut_path)?;
            let mut lines: Vec<Line> = Vec::new();
            let mut block_descs: Vec<String> = Vec::new();
            for (key, lit, comment, desc) in sets {
                lines.push(Line::Key(field_to_static(key)?, lit, comment));
                block_descs.push(desc);
            }
            ops.push(OpWithDesc {
                op: EditOp::AppendBlock {
                    path: dut_path.to_vec(),
                    block: render_section_block("relay", &lines, indent),
                },
                desc: block_descs.join("; "),
            });
        }
    }
    // DUT-owned advanced (gate-independent): diff against the DUT's OWN
    // override — never the effective global. An answer equal to the
    // override skips; an answer equal to the GLOBAL fallback with no
    // override skips (no per-DUT noise); "" (cleared SkipInherit) removes
    // an existing override; absent sections are appended as ONE block per
    // section (the strict re-parse contract)..
    let ex_adv = dut_advanced_current(ex);
    // (section, field, literal, comment, desc) for keys whose section is
    // ABSENT — emitted as one grouped block per section at the end.
    let mut missing: Vec<(String, String, String, Option<&'static str>, String)> = Vec::new();
    for (key, value) in &answers.advanced {
        let (section, field) = advanced_field(key);
        let t = value.trim();
        // Top-level keys (empty section, e.g. "loader") live DIRECTLY
        // inside the DUT block; section keys live inside `dut.<section>`.
        let mut container = dut_path.to_vec();
        if !section.is_empty() {
            container.push(seg_key(section));
        }
        match advanced_literal(key, value)? {
            None => {
                // Unset/inherit: drop the key when the DUT has it.
                if ex_adv.contains_key(key) {
                    let mut key_path = container.clone();
                    key_path.push(seg_key(field));
                    if locate_value(text, &key_path).is_some() {
                        ops.push(OpWithDesc {
                            op: EditOp::RemoveKey {
                                path: container,
                                key: field.to_string(),
                            },
                            desc: format!("unset {key} (inherit global)"),
                        });
                    }
                }
            }
            Some(lit) => {
                if ex_adv.get(key).is_some_and(|c| c == t) {
                    continue; // unchanged (typed back the override)
                }
                if !ex_adv.contains_key(key) && global_current.get(key).is_some_and(|c| c == t) {
                    continue; // inherits the global fallback — no noise
                }
                let desc = format!(
                    "{key}: {} -> {}",
                    ex_adv.get(key).map(String::as_str).unwrap_or("(inherit)"),
                    t
                );
                if locate_container_close(text, &container).is_some() {
                    set_or_append_key(
                        text,
                        ops,
                        &container,
                        ScalarEdit {
                            key: field,
                            literal: &lit,
                            comment: comment_for(key),
                            desc,
                            before: None,
                        },
                    )?;
                } else {
                    missing.push((
                        section.to_string(),
                        field.to_string(),
                        lit,
                        comment_for(key),
                        desc,
                    ));
                }
            }
        }
    }
    // Absent sections: ONE block per section carrying every set key
    // (deterministic member order — the strict re-parse contract).
    missing.sort_by(|a, b| (a.0.as_str(), a.1.as_str()).cmp(&(b.0.as_str(), b.1.as_str())));
    let mut emitted: Vec<String> = Vec::new();
    for (section, _field, _lit, _comment, _desc) in &missing {
        if emitted.iter().any(|s| s == section) {
            continue;
        }
        emitted.push(section.clone());
        let mut lines: Vec<Line> = Vec::new();
        let mut descs: Vec<String> = Vec::new();
        for (_, f, l, c, d) in missing.iter().filter(|(s, ..)| s == section) {
            lines.push(Line::Key(field_to_static(f)?, l.clone(), *c));
            descs.push(d.clone());
        }
        let indent = container_member_indent(text, dut_path)?;
        ops.push(OpWithDesc {
            op: EditOp::AppendBlock {
                path: dut_path.to_vec(),
                block: render_section_block(section, &lines, indent),
            },
            desc: descs.join("; "),
        });
    }
    Ok(())
}

// ── Atomic write / .mcp.json / validation ─────────────────────────────

/// Atomic write via tmp + rename (a crashed wizard can never leave a corrupt
/// config; an existing file's permissions are preserved) where the CONTENT
/// is validated BEFORE the rename: a wizard-created/updated config that
/// fails `Config::validate()` never lands on disk (the tmp file is removed
/// and the error returned — exit 1).
fn write_validated(path: &Path, content: &str) -> Result<(), String> {
    atomic_commit(path, content, true)
}

fn atomic_commit(path: &Path, content: &str, validate: bool) -> Result<(), String> {
    let tmp = path.with_file_name(".target.jsonc.tmp");
    std::fs::write(&tmp, content).map_err(|e| format!("Cannot write {}: {e}", tmp.display()))?;
    if validate && let Err(e) = validate_written(&tmp) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Ok(meta) = std::fs::metadata(path) {
        // Best-effort: permission preservation must never block the write.
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    std::fs::rename(&tmp, path)
        .map_err(|e| format!("Cannot rename {} to {}: {e}", tmp.display(), path.display()))
}

/// What the `.mcp.json` reconciler did after a wizard write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpJsonAction {
    /// `.mcp.json` did not exist — created with the canonical entry.
    Created,
    /// `.mcp.json` existed without a `sermcp` key under
    /// `mcpServers` — the canonical entry was merged in.
    Merged,
    /// `.mcp.json` already had a `sermcp` entry — left untouched.
    Untouched,
}

/// The canonical `sermcp` entry written to `.mcp.json` (stdio spawn;
/// `TARGET_CONF` pinned to the project's `.target.jsonc`). Shared by the
/// CREATE and MERGE paths so both always produce the identical shape.
pub fn sermcp_entry(target_conf: &Path) -> serde_json::Value {
    serde_json::json!({
        "command": "sermcp",
        "env": {
            "TARGET_CONF": target_conf.to_string_lossy()
        },
        "description": MCP_JSON_DESCRIPTION
    })
}

/// PURE merge (no IO, no TTY): reconcile the canonical `sermcp`
/// entry with parsed `.mcp.json` contents (`None` = file does not exist).
///
/// - `None` → canonical `{"mcpServers": {"sermcp": <entry>}}`, `Created`.
/// - object without `mcpServers` or without `mcpServers.sermcp` →
///   the entry is merged in; every existing top-level key and every other
///   server is preserved verbatim (serde_json `preserve_order` keeps key
///   order), `Merged`.
/// - object already carrying `mcpServers.sermcp` → returned
///   unchanged, `Untouched` (its content is never replaced).
///
/// JSONC comments in the source file are NOT preserved by this Value
/// round-trip — acceptable for `.mcp.json` (generated config, not
/// user-authored target config; the user's comment-bearing config lives in
/// `.target.jsonc`, which the wizard edits as text).
pub fn merge_sermcp_entry(
    existing: Option<serde_json::Value>,
    entry: serde_json::Value,
) -> Result<(serde_json::Value, McpJsonAction), String> {
    let mut root = match existing {
        None => {
            return Ok((
                serde_json::json!({ "mcpServers": { "sermcp": entry } }),
                McpJsonAction::Created,
            ));
        }
        Some(v) => v,
    };
    if !root.is_object() {
        return Err(".mcp.json does not contain a JSON object at the top level".into());
    }
    let root_obj = root.as_object_mut().expect("checked above");
    match root_obj.get_mut("mcpServers") {
        None => {
            root_obj.insert("mcpServers".into(), serde_json::json!({ "sermcp": entry }));
            Ok((root, McpJsonAction::Merged))
        }
        Some(servers) => {
            if !servers.is_object() {
                return Err(".mcp.json `mcpServers` is not a JSON object".into());
            }
            if servers.get("sermcp").is_some() {
                return Ok((root, McpJsonAction::Untouched));
            }
            servers
                .as_object_mut()
                .expect("checked above")
                .insert("sermcp".into(), entry);
            Ok((root, McpJsonAction::Merged))
        }
    }
}

/// Reconcile `.mcp.json` with the canonical `sermcp` entry:
///
/// - missing → CREATE the canonical file (previous behavior);
/// - exists (JSONC — comments allowed) without a `sermcp` key under
///   `mcpServers` → MERGE the entry in, preserving every other key/server
///   verbatim (Value round-trip; JSONC comments are dropped — documented on
///   `merge_sermcp_entry`);
/// - exists with a `sermcp` key → untouched.
///
/// Parse/validation failures are loud errors — the file is never edited.
pub fn ensure_mcp_json(project_dir: &Path, target_conf: &Path) -> Result<McpJsonAction, String> {
    let mcp_json = project_dir.join(".mcp.json");
    let existing: Option<serde_json::Value> = match std::fs::read_to_string(&mcp_json) {
        Ok(text) => Some(crate::config::parse_jsonc_value(&text).map_err(|e| {
            format!(
                "{} does not parse ({e}) — refusing to edit; fix the file by hand first",
                mcp_json.display()
            )
        })?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("Cannot read {}: {e}", mcp_json.display())),
    };
    let (root, action) = merge_sermcp_entry(existing, sermcp_entry(target_conf))?;
    if action == McpJsonAction::Untouched {
        return Ok(action);
    }
    let mut content = serde_json::to_string_pretty(&root)
        .map_err(|e| format!("JSON serialization error: {e}"))?;
    content.push('\n');
    std::fs::write(&mcp_json, content).map_err(|e| format!("Cannot write .mcp.json: {e}"))?;
    Ok(action)
}

/// Run `Config::validate()` against the file just written (defaults +
/// file keys). Hard failure for CREATE/UPDATE (should be unreachable —
/// required answers are enforced).
fn validate_written(path: &Path) -> Result<(), String> {
    let mut values = crate::config::defaults();
    values.extend(crate::config::parse_jsonc_config(path)?);
    let cfg = crate::config::Config {
        values,
        config_path: Some(path.to_path_buf()),
        project_dir: path.parent().map(Path::to_path_buf),
        format: crate::config::ConfigFormat::Jsonc,
    };
    cfg.validate().map_err(|errors| errors.join("\n"))
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse_jsonc_text;
    use std::assert_matches;
    use std::collections::VecDeque;
    use std::sync::{Mutex, MutexGuard};
    use tempfile::TempDir;

    /// run_init reads TARGET_CONF — serialize the tests that touch env so a
    /// parallel sibling never leaks into them.
    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    fn lock_env() -> MutexGuard<'static, ()> {
        ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Canned-answer PromptIO — the entire wizard is unit-testable without
    /// a TTY. Every io call pops the next canned answer; running out is an
    /// Err (the test asserts the exact question count implicitly).
    struct CannedIO {
        inputs: VecDeque<String>,
        confirms: VecDeque<bool>,
        busy: VecDeque<BusyDecision>,
        busy_calls: Vec<Vec<EndpointConflict>>,
        notes: Vec<String>,
    }

    impl CannedIO {
        fn new(inputs: &[&str]) -> Self {
            Self {
                inputs: inputs.iter().map(|s| s.to_string()).collect(),
                confirms: VecDeque::new(),
                busy: VecDeque::new(),
                busy_calls: Vec::new(),
                notes: Vec::new(),
            }
        }

        /// Canned occupancy-gate decisions (unpushed → Continue, so
        /// pre-existing fixtures without a holder never change behavior).
        fn with_busy(mut self, decisions: &[BusyDecision]) -> Self {
            self.busy = decisions.iter().copied().collect();
            self
        }
    }

    impl PromptIO for CannedIO {
        fn input(&mut self, _prompt: &str, _default: Option<&str>) -> Result<String, String> {
            self.inputs
                .pop_front()
                .ok_or_else(|| "no more canned inputs".to_string())
        }
        fn confirm(&mut self, _prompt: &str, default: bool) -> Result<bool, String> {
            // Unpushed confirms default to the question's default (the
            // per-DUT relay gate defaults NO on create — quick-flow
            // fixtures need no extra canned answers).
            Ok(self.confirms.pop_front().unwrap_or(default))
        }
        fn note(&mut self, _kind: NoteKind, msg: &str) {
            self.notes.push(msg.to_string());
        }
        fn busy_endpoints(
            &mut self,
            conflicts: &[EndpointConflict],
        ) -> Result<BusyDecision, String> {
            self.busy_calls.push(conflicts.to_vec());
            Ok(self.busy.pop_front().unwrap_or(BusyDecision::Continue))
        }
    }

    fn quick_opts() -> InitOptions {
        InitOptions {
            quick: true,
            ..Default::default()
        }
    }

    fn non_interactive_opts() -> InitOptions {
        InitOptions {
            non_interactive: true,
            ..Default::default()
        }
    }

    /// One host / one DUT — the canned answers for the quick flow when the
    /// caller does not care about values (empty = defaults/auto-names).
    /// The relay gate is a CONFIRM (unpushed → the question default: NO on
    /// create), so no relay questions are asked and no inputs needed.
    const QUICK_ANSWERS: &[&str] = &[
        "1",        // host count
        "10.0.0.1", // host ip (required)
        "linaro",   // host user
        "",         // host pass
        "",         // host name (optional) -> ""
        "1",        // dut count
        "",         // dut name -> dut1
        "41002",    // serial port (required)
        "",         // login user -> root
    ];

    // ── Naming ────────────────────────────────────────────────────────

    #[test]
    fn test_next_free_dut_name() {
        assert_eq!(next_free_dut_name("dut", &["dut1".into()]), "dut2");
    }

    #[test]
    fn test_parse_number_or_string() {
        assert_eq!(
            parse_number_or_string("41002").unwrap().to_value_string(),
            "41002"
        );
        assert_eq!(
            parse_number_or_string("41002.5").unwrap().to_value_string(),
            "41002.5"
        );
        assert_matches!(
            parse_number_or_string("/dev/ttyUSB0").unwrap(),
            NumberOrString::String(ref s) if s == "/dev/ttyUSB0"
        );
    }

    // ── Advanced table (full option coverage) ─────────────────────────

    #[test]
    fn test_advanced_defaults_cover_every_key() {
        for spec in ADVANCED_FIELDS {
            // Every key has a known default AND a known literal mapping —
            // the table drives both, so coverage is structural.
            let _ = advanced_literal(spec.key, &advanced_default(spec.key));
        }
    }

    #[test]
    fn test_advanced_literal_zero_guarded() {
        // 0 / "" / 0.0 answers mean "inherit" — never inserted.
        for key in [
            "relay.port",
            "relay.reset_ch",
            "relay.download_ch",
            "relay.power_ch",
            "relay.reset_time_ms",
            "relay.power_off_time_ms",
        ] {
            assert_eq!(
                advanced_literal(key, "0").unwrap(),
                None,
                "{key} 0 must be zero-guarded"
            );
        }
        assert_eq!(
            advanced_literal("monitor.learner_stage_threshold", "0.0").unwrap(),
            None
        );
        assert_eq!(
            advanced_literal("monitor.crash_keywords", "").unwrap(),
            None
        );
        assert_eq!(
            advanced_literal("monitor.crash_keywords", "  , , ").unwrap(),
            None
        );
        // 41000 is legitimate (not a sentinel).
        assert_eq!(
            advanced_literal("relay.port", "41000").unwrap(),
            Some("41000".into())
        );
    }

    #[test]
    fn test_advanced_literal_typed_errors_name_key() {
        assert!(
            advanced_literal("relay.port", "abc")
                .unwrap_err()
                .contains("relay.port")
        );
        assert!(
            advanced_literal("serial.rfc2217", "maybe")
                .unwrap_err()
                .contains("serial.rfc2217")
        );
        assert!(
            advanced_literal("monitor.hang_timeout", "x")
                .unwrap_err()
                .contains("monitor.hang_timeout")
        );
        assert!(
            advanced_literal("no.such.key", "1")
                .unwrap_err()
                .contains("no.such.key")
        );
    }

    #[test]
    fn test_interrupt_char_is_validated_before_test_or_save() {
        for value in ["ctrl_c", "^C", "esc", "space", "F12", "0x03", "3", "x"] {
            assert!(
                advanced_literal("uboot.interrupt_char", value).is_ok(),
                "{value} should be accepted"
            );
        }
        for value in ["ctrl_shift_c", "0x100", "中文"] {
            let error = advanced_literal("uboot.interrupt_char", value).unwrap_err();
            assert!(error.contains("uboot.interrupt_char"), "{error}");
        }
    }

    #[test]
    fn test_advanced_literal_power_off_clamped() {
        assert_eq!(
            advanced_literal("relay.power_off_time_ms", "1000").unwrap(),
            Some("3000".into())
        );
        assert_eq!(
            advanced_literal("relay.power_off_time_ms", "5000").unwrap(),
            Some("5000".into())
        );
    }

    #[test]
    fn test_merge_advanced_roundtrip() {
        let mut config = TargetJsoncConfig {
            dev_hosts: Vec::new(),
            serial: None,
            target: None,
            uboot: None,
            relay: None,
            monitor: None,
            flash: None,
            download: None,
            control: None,
            loader: None,
            reference_log: None,
            dut_dir: None,
            lock_dir: None,
        };
        let answers = HashMap::from([
            ("serial.baudrate".into(), "921600".into()),
            ("serial.rfc2217".into(), "true".into()),
            ("monitor.crash_keywords".into(), "Kernel panic, BUG:".into()),
            ("control.dev_ctrl".into(), "ctl --reset 1".into()),
            ("loader".into(), "uefi".into()),
            ("lock_dir".into(), "/tmp/locks".into()),
        ]);
        merge_advanced(&mut config, &answers).unwrap();
        let serial = config.serial.as_ref().unwrap();
        assert_eq!(
            serial.baudrate.as_ref().unwrap().to_value_string(),
            "921600"
        );
        assert_eq!(serial.rfc2217, Some(true));
        // relay.* keys are NO LONGER advanced keys (they live per-DUT under
        // the relay gate) — merge_advanced must not touch the relay section.
        assert!(config.relay.is_none());
        assert_eq!(
            config
                .monitor
                .as_ref()
                .unwrap()
                .crash_keywords
                .as_ref()
                .unwrap(),
            &["Kernel panic".to_string(), "BUG:".to_string()]
        );
        assert_eq!(config.lock_dir.as_deref(), Some("/tmp/locks"));
        assert_eq!(config.loader.as_deref(), Some("uefi"));
        // advanced_current feeds the UPDATE defaults from the same fields.
        let current = advanced_current(&config);
        assert_eq!(
            current.get("serial.baudrate").map(String::as_str),
            Some("921600")
        );
        assert_eq!(
            current.get("monitor.crash_keywords").map(String::as_str),
            Some("Kernel panic, BUG:")
        );
    }

    // ── Locator ───────────────────────────────────────────────────────

    const LOCATE_FIXTURE: &str = r#"{
  // leading comment with braces {} and brackets []
  "dev_hosts": [
    {
      "ip": "10.0.0.1",   /* block comment { } */
      "user": "linaro",
      "pass": "",
      "duts": [
        {
          "dut_name": "board1",  // board
          "serial": {
            "port": 41000,  // keep this comment
            "baudrate": 1500000,
            "rfc2217": false
          },
          "target": { "login_user": "root", "login_pass": "" },
          "relay": { "type": "ch340" }
        },
        {
          "dut_name": "board2",
          "serial": { "port": "41001", "baudrate": 460800 }
        }
      ]
    }
  ]
}"#;

    fn key(s: &str) -> PathSeg {
        PathSeg::Key(s.to_string())
    }

    #[test]
    fn test_locate_values() {
        let text = LOCATE_FIXTURE;
        let ip_path = [key("dev_hosts"), PathSeg::Index(0), key("ip")];
        let (start, end) = locate_value(text, &ip_path).unwrap();
        assert_eq!(&text[start..end], "\"10.0.0.1\"");

        let port_path = [
            key("dev_hosts"),
            PathSeg::Index(0),
            key("duts"),
            PathSeg::Index(0),
            key("serial"),
            key("port"),
        ];
        let (start, end) = locate_value(text, &port_path).unwrap();
        assert_eq!(&text[start..end], "41000");

        // String port on the second DUT.
        let port2_path = [
            key("dev_hosts"),
            PathSeg::Index(0),
            key("duts"),
            PathSeg::Index(1),
            key("serial"),
            key("port"),
        ];
        let (start, end) = locate_value(text, &port2_path).unwrap();
        assert_eq!(&text[start..end], "\"41001\"");
    }

    #[test]
    fn test_locate_handles_strings_with_braces_and_escapes() {
        let text = r#"{
  "dev_hosts": [
    {
      "ip": "10.0.0.1",
      "pass": "}{not\"a\\\"key}",
      "duts": [
        { "dut_name": "board1", "serial": { "port": 41000 } }
      ]
    }
  ]
}"#;
        let ip_path = [key("dev_hosts"), PathSeg::Index(0), key("ip")];
        let (start, end) = locate_value(text, &ip_path).unwrap();
        assert_eq!(&text[start..end], "\"10.0.0.1\"");
        let pass_path = [key("dev_hosts"), PathSeg::Index(0), key("pass")];
        let (start, end) = locate_value(text, &pass_path).unwrap();
        assert_eq!(&text[start..end], r#""}{not\"a\\\"key}""#);
    }

    #[test]
    fn test_locate_missing_path_is_none() {
        assert_eq!(locate_value(LOCATE_FIXTURE, &[key("nope")]), None);
        assert_eq!(
            locate_value(
                LOCATE_FIXTURE,
                &[key("dev_hosts"), PathSeg::Index(3), key("ip")]
            ),
            None
        );
    }

    #[test]
    fn test_locate_container_close() {
        let text = LOCATE_FIXTURE;
        let duts_close =
            locate_container_close(text, &[key("dev_hosts"), PathSeg::Index(0), key("duts")])
                .unwrap();
        assert_eq!(&text[duts_close..duts_close + 1], "]");
        let root_close = locate_container_close(text, &[]).unwrap();
        assert_eq!(&text[root_close..root_close + 1], "}");
        let dev_hosts_close = locate_container_close(text, &[key("dev_hosts")]).unwrap();
        assert_eq!(&text[dev_hosts_close..dev_hosts_close + 1], "]");
    }

    // ── Edit ops ──────────────────────────────────────────────────────

    #[test]
    fn test_set_scalar_preserves_trailing_comment_and_comma() {
        let text = LOCATE_FIXTURE;
        let op = EditOp::SetScalar {
            path: vec![
                key("dev_hosts"),
                PathSeg::Index(0),
                key("duts"),
                PathSeg::Index(0),
                key("serial"),
                key("port"),
            ],
            literal: "41003".into(),
        };
        let result = apply_edits(text, &[op]).unwrap();
        assert!(result.contains("41003,  // keep this comment"), "{result}");
        assert!(result.contains("\"baudrate\": 1500000"), "{result}");
        // Untouched regions byte-identical: the prefix before the value is
        // unchanged.
        let original_prefix = &text[..text.find("41000").unwrap()];
        assert!(
            result.starts_with(original_prefix),
            "prefix must be byte-identical"
        );
    }

    #[test]
    fn test_append_host_into_existing_array() {
        let text = LOCATE_FIXTURE;
        let new_host = JsoncDevHost {
            ip: Some("10.0.0.2".into()),
            user: Some(String::new()),
            pass: Some(String::new()),
            host_name: None,
            duts: Vec::new(),
        };
        let op = EditOp::AppendBlock {
            path: vec![key("dev_hosts")],
            block: render_host_block(&new_host, 4),
        };
        let result = apply_edits(text, &[op]).unwrap();
        parse_jsonc_text(&result).unwrap_or_else(|e| panic!("{e}\n{result}"));
        assert!(result.contains("\"ip\": \"10.0.0.2\""), "{result}");
        // The original host block is byte-preserved (keyed on its ip line).
        let old_block = "\"ip\": \"10.0.0.1\",   /* block comment { } */";
        assert!(result.contains(old_block), "{result}");
    }

    #[test]
    fn test_append_dut_into_nested_duts_array() {
        let text = LOCATE_FIXTURE;
        let new_dut = JsoncDut {
            dut_name: "board3".into(),
            serial: Some(JsoncSerial {
                port: Some(NumberOrString::Int(41003)),
                baudrate: None,
                rfc2217: None,
                tx_repeat: None,
                tx_gap_ms: None,
                backspace: None,
                enter: None,
                rx_newline: None,
                highlight: None,
                mouse: None,
            }),
            target: Some(JsoncTarget {
                login_user: Some("root".into()),
                login_pass: Some(String::new()),
                login_prompt: None,
            }),
            relay: Some(JsoncRelay {
                relay_type: Some("ch340".into()),
                port: None,
                ip: None,
                reset_ch: None,
                download_ch: None,
                power_ch: None,
                reset_time_ms: None,
                power_off_time_ms: None,
            }),
            uboot: None,
            monitor: None,
            flash: None,
            download: None,
            control: None,
            loader: None,
            reference_log: None,
            dut_dir: None,
        };
        let op = EditOp::AppendBlock {
            path: vec![key("dev_hosts"), PathSeg::Index(0), key("duts")],
            block: render_dut_block(&new_dut, 8),
        };
        let result = apply_edits(text, &[op]).unwrap();
        parse_jsonc_text(&result).unwrap_or_else(|e| panic!("{e}\n{result}"));
        assert!(result.contains("\"dut_name\": \"board3\""), "{result}");
        assert!(result.contains("\"port\": 41003"), "{result}");
    }

    #[test]
    fn test_append_into_empty_array() {
        let text = "{\n  \"dev_hosts\": [],\n  \"serial\": { \"port\": 41000 }\n}\n";
        let host = JsoncDevHost {
            ip: Some("10.0.0.1".into()),
            user: Some(String::new()),
            pass: Some(String::new()),
            host_name: None,
            duts: Vec::new(),
        };
        let op = EditOp::AppendBlock {
            path: vec![key("dev_hosts")],
            block: render_host_block(&host, 4),
        };
        let result = apply_edits(text, &[op]).unwrap();
        parse_jsonc_text(&result).unwrap_or_else(|e| panic!("{e}\n{result}"));
        assert!(result.contains("\"ip\": \"10.0.0.1\""), "{result}");
    }

    #[test]
    fn test_append_when_last_entry_lacks_trailing_comma() {
        // Hand-edited file: last host has NO trailing comma.
        let text = "{\n  \"dev_hosts\": [\n    { \"ip\": \"10.0.0.1\" }\n  ]\n}\n";
        let host = JsoncDevHost {
            ip: Some("10.0.0.2".into()),
            user: Some(String::new()),
            pass: Some(String::new()),
            host_name: None,
            duts: Vec::new(),
        };
        let op = EditOp::AppendBlock {
            path: vec![key("dev_hosts")],
            block: render_host_block(&host, 4),
        };
        let result = apply_edits(text, &[op]).unwrap();
        parse_jsonc_text(&result).unwrap_or_else(|e| panic!("{e}\n{result}"));
        assert!(result.contains("\"ip\": \"10.0.0.2\""), "{result}");
    }

    #[test]
    fn test_apply_edits_reverts_on_parse_failure() {
        let text = LOCATE_FIXTURE;
        let bad = EditOp::SetScalar {
            path: vec![key("dev_hosts"), PathSeg::Index(0), key("ip")],
            literal: "not-json".into(),
        };
        let err = apply_edits(text, &[bad]).unwrap_err();
        assert!(err.contains("invalid JSONC"), "{err}");
        // Caller keeps the original text (never partial output).
        assert_eq!(text, LOCATE_FIXTURE);
    }

    #[test]
    fn test_remove_key_removes_line() {
        let text =
            "{\n  \"lock_dir\": \"/tmp/x\",  // comment\n  \"dut_dir\": \".dut-serial\"\n}\n";
        let op = EditOp::RemoveKey {
            path: vec![],
            key: "lock_dir".into(),
        };
        let result = apply_edits(text, &[op]).unwrap();
        parse_jsonc_text(&result).unwrap_or_else(|e| panic!("{e}\n{result}"));
        assert!(!result.contains("lock_dir"), "{result}");
        assert!(result.contains("dut_dir"), "{result}");
    }

    // ── Renderer ──────────────────────────────────────────────────────

    #[test]
    fn test_render_roundtrip_with_answers() {
        let answers = WizardAnswers {
            hosts: vec![HostAnswers {
                ip: "10.0.0.1".into(),
                user: "linaro".into(),
                pass: String::new(),
                host_name: String::new(),
                duts: vec![dut_answers("dut1", "41002")],
            }],
            advanced: HashMap::new(),
            announcements: Vec::new(),
        };
        let config = build_config_from_answers(&answers).unwrap();
        let text = render_jsonc(&config, true);
        let parsed = parse_jsonc_text(&text).unwrap_or_else(|e| panic!("{e}\n{text}"));
        assert_eq!(parsed.dev_hosts.len(), 1);
        assert_eq!(parsed.dev_hosts[0].ip.as_deref(), Some("10.0.0.1"));
        assert_eq!(parsed.dev_hosts[0].duts.len(), 1);
        assert_eq!(
            parsed.dev_hosts[0].duts[0]
                .serial
                .as_ref()
                .unwrap()
                .port
                .as_ref()
                .unwrap()
                .to_value_string(),
            "41002"
        );
        // Canonical comments present.
        assert!(text.contains("REQUIRED — unique across ALL DUTs"), "{text}");
        assert!(text.contains("ser2net port OR device path"), "{text}");
        // with_global_hints=true renders the commented fallback block.
        assert!(text.contains("Global fallbacks (optional)"), "{text}");
        // The quick flow sets no global section → hints only.
        assert!(!render_jsonc(&config, false).contains("Global fallbacks"));
        // Header present.
        assert!(text.starts_with("// .target.jsonc"), "{text}");
    }

    #[test]
    fn test_render_global_sections_when_set() {
        let config = build_config_from_answers(&WizardAnswers {
            hosts: vec![HostAnswers {
                ip: "10.0.0.1".into(),
                user: String::new(),
                pass: String::new(),
                host_name: String::new(),
                duts: vec![DutAnswers {
                    dut_name: "dut1".into(),
                    serial_port: "41002".into(),
                    login_user: "root".into(),
                    relay_gate: true,
                    relay_type: "ch340".into(),
                    relay_port: String::new(),
                    relay_reset_ch: "1".into(),
                    relay_download_ch: String::new(),
                    relay_power_ch: String::new(),
                    relay_reset_time_ms: String::new(),
                    relay_power_off_time_ms: "3000".into(),
                    advanced: HashMap::from([("serial.baudrate".into(), "921600".into())]),
                }],
            }],
            advanced: HashMap::from([("monitor.hang_timeout".into(), "30".into())]),
            announcements: Vec::new(),
        })
        .unwrap();
        let text = render_jsonc(&config, true);
        // serial.baudrate is DUT-owned: it renders inside the DUT's serial
        // section; the host-level answer renders in the global block.
        assert!(text.contains("\"baudrate\": 921600"), "{text}");
        assert!(text.contains("\"hang_timeout\": 30"), "{text}");
        // Real sections replace the commented hints.
        assert!(!text.contains("Global fallbacks"), "{text}");
        parse_jsonc_text(&text).unwrap();
    }

    /// `render_jsonc` emits the NEW `crash_keywords` key (config files
    /// upgrade on next wizard write) and the output round-trips.
    #[test]
    fn test_render_jsonc_emits_crash_keywords() {
        let config = parse_jsonc_text(
            r#"{
  "dev_hosts": [],
  "monitor": { "crash_keywords": ["Kernel panic", "BUG:"] }
}"#,
        )
        .unwrap();
        let text = render_jsonc(&config, false);
        assert!(text.contains("\"crash_keywords\""), "rendered: {text}");
        assert!(
            !text.contains("crash_patterns"),
            "old spelling must not be rendered: {text}"
        );
        let reparsed = parse_jsonc_text(&text).unwrap();
        assert_eq!(
            reparsed
                .monitor
                .as_ref()
                .and_then(|m| m.crash_keywords.as_ref())
                .unwrap(),
            &["Kernel panic".to_string(), "BUG:".to_string()]
        );
    }

    // ── Wizard flows ──────────────────────────────────────────────────

    #[test]
    fn test_wizard_create_canned_answers() {
        let _guard = lock_env();
        let tmp = TempDir::new().unwrap();
        unsafe {
            std::env::remove_var("TARGET_CONF");
        }
        let mut io = CannedIO::new(&quick_with_dut_advanced());
        let outcome = run_init(&mut io, &quick_opts(), tmp.path()).unwrap();
        assert!(outcome.config_created);
        let path = tmp.path().join(CONFIG_FILE_NAME);
        assert!(path.exists());
        let text = std::fs::read_to_string(&path).unwrap();
        let parsed = parse_jsonc_text(&text).unwrap();
        assert_eq!(parsed.dev_hosts[0].duts[0].dut_name, "dut1");
        // The CREATE renderer emits the real dut_name key and the optional
        // host_name hint (empty answer → hint, never a real empty key).
        assert!(text.contains("\"dut_name\": \"dut1\""), "{text}");
        assert!(text.contains("\"host_name\""), "{text}");
        // Accepting the shown defaults is silent — nothing was auto-named.
        assert!(
            !outcome.changes.iter().any(|c| c.contains("auto-named")),
            "{:?}",
            outcome.changes
        );
        // .mcp.json generated with TARGET_CONF -> .target.jsonc.
        let mcp = tmp.path().join(".mcp.json");
        assert!(mcp.exists());
        let mcp_text = std::fs::read_to_string(&mcp).unwrap();
        assert!(mcp_text.contains("TARGET_CONF"), "{mcp_text}");
        assert!(mcp_text.contains(".target.jsonc"), "{mcp_text}");
        // No tmp residue.
        assert!(!tmp.path().join(".target.jsonc.tmp").exists());
    }

    #[test]
    fn test_wizard_create_required_reasked_until_given() {
        let _guard = lock_env();
        let tmp = TempDir::new().unwrap();
        unsafe {
            std::env::remove_var("TARGET_CONF");
        }
        // Empty ip and empty port first → the wizard re-asks.
        let mut inputs: Vec<String> = [
            "1", "", "10.0.0.1", "", "", "", "1", "", "", "41002", "", "",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        inputs.extend(dut_advanced_empties());
        let mut io = CannedIO::new(&[]);
        io.inputs = inputs.into();
        let outcome = run_init(&mut io, &quick_opts(), tmp.path()).unwrap();
        let parsed =
            parse_jsonc_text(&std::fs::read_to_string(&outcome.config_path).unwrap()).unwrap();
        assert_eq!(parsed.dev_hosts[0].ip.as_deref(), Some("10.0.0.1"));
        assert_eq!(
            parsed.dev_hosts[0].duts[0]
                .serial
                .as_ref()
                .unwrap()
                .port
                .as_ref()
                .unwrap()
                .to_value_string(),
            "41002"
        );
    }

    #[test]
    fn test_non_interactive_create_lists_every_missing_required() {
        let _guard = lock_env();
        let tmp = TempDir::new().unwrap();
        unsafe {
            std::env::remove_var("TARGET_CONF");
        }
        let mut io = CannedIO::new(&[]); // no prompts at all in headless mode
        let err = run_init(&mut io, &non_interactive_opts(), tmp.path()).unwrap_err();
        assert!(err.contains("dev_hosts[0].ip"), "{err}");
        assert!(err.contains("dev_hosts[0].duts[0].serial.port"), "{err}");
        assert!(!tmp.path().join(CONFIG_FILE_NAME).exists());
    }

    #[test]
    fn test_non_interactive_update_is_noop() {
        let _guard = lock_env();
        let tmp = TempDir::new().unwrap();
        unsafe {
            std::env::remove_var("TARGET_CONF");
        }
        let path = tmp.path().join(CONFIG_FILE_NAME);
        std::fs::write(
            &path,
            "{\n  // untouched\n  \"dev_hosts\": [\n    {\n      \"ip\": \"10.0.0.1\",\n      \"user\": \"u\",\n      \"pass\": \"\",\n      \"duts\": [\n        {\n          \"dut_name\": \"board1\",\n          \"serial\": { \"port\": 41000, \"baudrate\": 460800 },\n          \"target\": { \"login_user\": \"root\" },\n          \"relay\": { \"type\": \"ch340\" }\n        }\n      ]\n    }\n  ]\n}\n",
        )
        .unwrap();
        let before = std::fs::read_to_string(&path).unwrap();
        let mut io = CannedIO::new(&[]);
        let outcome = run_init(&mut io, &non_interactive_opts(), tmp.path()).unwrap();
        assert!(!outcome.config_created);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        assert!(
            outcome.changes.iter().any(|c| c.contains("unchanged")),
            "{:?}",
            outcome.changes
        );
    }

    #[test]
    fn test_wizard_update_preserves_comments_golden() {
        let _guard = lock_env();
        let tmp = TempDir::new().unwrap();
        unsafe {
            std::env::remove_var("TARGET_CONF");
        }
        let path = tmp.path().join(CONFIG_FILE_NAME);
        std::fs::write(&path, LOCATE_FIXTURE).unwrap();

        // Host 0 keeps its identity; port + login_user change; a second
        // DUT and a second host are appended. Canned answers in ask order.
        let inputs = [
            "2",        // host count
            "10.0.0.1", // host 0 ip (same)
            "linaro",   // host 0 user (same)
            "",         // host 0 pass (same)
            "",         // host 0 name (same)
            "2",        // host 0 dut count (one appended)
            "board1",   // dut 0 name (same)
            "41003",    // dut 0 port (CHANGED)
            "ubuntu",   // dut 0 login_user (CHANGED)
            "ch340",    // dut 0 relay.type (same)
            "",         // dut 0 relay.port (inherit)
            "",         // dut 0 relay.reset_ch (inherit)
            "",         // dut 0 relay.download_ch (inherit)
            "",         // dut 0 relay.power_ch (inherit)
            "",         // dut 0 relay.reset_time_ms (inherit)
            "",         // dut 0 relay.power_off_time_ms (default 3000 -> no-op)
            "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "",
            "",       // dut 0 advanced (bare Enter x17)
            "board2", // dut 1 name (same)
            "41001",  // dut 1 port (same)
            "root",   // dut 1 login_user (same; section missing -> default no-op)
            "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "",
            "",         // dut 1 advanced (bare Enter x17)
            "10.0.0.2", // host 1 ip
            "",         // host 1 user
            "",         // host 1 pass
            "",         // host 1 name
            "1",        // host 1 dut count
            "",         // dut name -> dut2 (auto)
            "41004",    // port
            "",         // login user -> root
            "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "",
            "", // dut advanced (bare Enter x17)
        ];
        let mut io = CannedIO::new(&inputs);
        let outcome = run_init(&mut io, &quick_opts(), tmp.path()).unwrap();
        assert!(!outcome.config_created);

        let result = std::fs::read_to_string(&path).unwrap();
        // Golden: SET keeps the same-line comment, untouched comments are
        // byte-identical.
        assert!(result.contains("41003,  // keep this comment"), "{result}");
        assert!(
            result.contains("\"ip\": \"10.0.0.1\",   /* block comment { } */"),
            "{result}"
        );
        assert!(
            result.contains("// leading comment with braces {} and brackets []"),
            "{result}"
        );
        assert!(result.contains("\"login_user\": \"ubuntu\""), "{result}");
        // The appended host is identified by its ip; the appended DUT
        // auto-names dut1 (no existing dut names to avoid).
        assert!(result.contains("\"ip\": \"10.0.0.2\""), "{result}");
        assert!(result.contains("\"dut_name\": \"dut1\""), "{result}");
        // The appended host renders the optional host_name hint line.
        assert!(result.contains("\"host_name\""), "{result}");
        let parsed = parse_jsonc_text(&result).unwrap_or_else(|e| panic!("{e}\n{result}"));
        assert_eq!(parsed.dev_hosts.len(), 2);
        assert_eq!(parsed.dev_hosts[0].duts.len(), 2);
        assert_eq!(
            parsed.dev_hosts[0].duts[0]
                .serial
                .as_ref()
                .unwrap()
                .port
                .as_ref()
                .unwrap()
                .to_value_string(),
            "41003"
        );
        assert_eq!(
            parsed.dev_hosts[0].duts[0]
                .target
                .as_ref()
                .unwrap()
                .login_user
                .as_deref(),
            Some("ubuntu")
        );
        // Diff summary lines.
        assert!(
            outcome
                .changes
                .iter()
                .any(|c| c.contains("serial.port: 41000 -> 41003")),
            "{:?}",
            outcome.changes
        );
        assert!(
            outcome
                .changes
                .iter()
                .any(|c| c.contains("added host '10.0.0.2' at dev_hosts[1]")),
            "{:?}",
            outcome.changes
        );
        // The appended HOST block carries its DUTs — one "added host" line.
        let host_added = outcome
            .changes
            .iter()
            .any(|c| c.contains("added host '10.0.0.2' at dev_hosts[1]"));
        assert!(host_added, "{:?}", outcome.changes);
    }

    /// UPDATE with a newly typed host_name must insert the key ABOVE the
    /// `duts` member (canonical host order), never append it after the
    /// array with a dangling comma line.
    #[test]
    fn test_wizard_update_typed_host_name_inserts_before_duts() {
        let _guard = lock_env();
        let tmp = TempDir::new().unwrap();
        unsafe {
            std::env::remove_var("TARGET_CONF");
        }
        let path = tmp.path().join(CONFIG_FILE_NAME);
        std::fs::write(
            &path,
            "{\n  \"dev_hosts\": [\n    {\n      \"ip\": \"10.0.0.1\",\n      \"user\": \"u\",\n      \"pass\": \"\",\n      \"duts\": [\n        {\n          \"dut_name\": \"b1\",\n          \"serial\": { \"port\": 41000 }\n        }\n      ]\n    }\n  ]\n}\n",
        )
        .unwrap();

        // Same counts/values except the host display name (empty -> "lab-pc").
        let mut inputs: Vec<String> = vec![
            "1".into(),
            "10.0.0.1".into(),
            "u".into(),
            "".into(),
            "lab-pc".into(), // host name (typed for the first time)
            "1".into(),
            "b1".into(),
            "41000".into(),
            "root".into(),
        ];
        inputs.extend(dut_advanced_empties());
        let mut io = CannedIO::new(&[]);
        io.inputs = inputs.into();
        let outcome = run_init(&mut io, &quick_opts(), tmp.path()).unwrap();

        let result = std::fs::read_to_string(&path).unwrap();
        let parsed = parse_jsonc_text(&result).unwrap_or_else(|e| panic!("{e}\n{result}"));
        assert_eq!(
            parsed.dev_hosts[0].host_name.as_deref(),
            Some("lab-pc"),
            "{result}"
        );
        // Position: host_name sits ABOVE the duts member, never after it.
        let host_name_pos = result.find("\"host_name\"").expect("host_name key");
        let duts_pos = result.find("\"duts\"").expect("duts key");
        assert!(host_name_pos < duts_pos, "{result}");
        // No dangling comma line glued to the duts close bracket.
        assert!(!result.contains("]\n    ,"), "{result}");
        assert!(
            outcome
                .changes
                .iter()
                .any(|c| c.contains("host_name:  -> lab-pc")),
            "{:?}",
            outcome.changes
        );
    }

    #[test]
    fn test_wizard_update_adds_global_section_from_advanced() {
        let _guard = lock_env();
        let tmp = TempDir::new().unwrap();
        unsafe {
            std::env::remove_var("TARGET_CONF");
        }
        let path = tmp.path().join(CONFIG_FILE_NAME);
        std::fs::write(
            &path,
            "{\n  \"dev_hosts\": [\n    { \"ip\": \"10.0.0.1\", \"user\": \"u\", \"pass\": \"\", \"duts\": [ { \"dut_name\": \"b1\", \"serial\": { \"port\": 41000 } } ] }\n  ]\n}\n",
        )
        .unwrap();

        // Same counts/values (no host/dut ops), then the DUT-owned advanced
        // set with `--advanced`: serial.baudrate is DUT-owned now — it lands
        // INSIDE the DUT's serial section; a host-level key still lands in
        // the GLOBAL section.
        let mut inputs: Vec<String> = vec![
            "1".into(),
            "10.0.0.1".into(),
            "u".into(),
            "".into(),
            "".into(), // host name (optional) -> ""
            "1".into(),
            "b1".into(),
            "41000".into(),
            "root".into(),
        ];
        let mut dut_adv = dut_advanced_empties();
        dut_adv[0] = "921600".into(); // serial.baudrate (DUT-owned #0)
        inputs.extend(dut_adv);
        for spec in ADVANCED_FIELDS
            .iter()
            .filter(|s| !dut_owned_advanced(s.key))
        {
            inputs.push(match spec.key {
                "monitor.hang_timeout" => "60".into(),
                _ => String::new(), // empty → skip (default is informational)
            });
        }
        let mut io = CannedIO::new(&[]);
        io.inputs = inputs.into();
        let outcome = run_init(
            &mut io,
            &InitOptions {
                advanced: true,
                ..Default::default()
            },
            tmp.path(),
        )
        .unwrap();

        let result = std::fs::read_to_string(&path).unwrap();
        let parsed = parse_jsonc_text(&result).unwrap_or_else(|e| panic!("{e}\n{result}"));
        // Per-DUT override written into the DUT's own serial section.
        assert_eq!(
            parsed.dev_hosts[0].duts[0]
                .serial
                .as_ref()
                .unwrap()
                .baudrate
                .as_ref()
                .unwrap()
                .to_value_string(),
            "921600"
        );
        // The GLOBAL serial section is untouched (baudrate is DUT-owned).
        assert!(parsed.serial.is_none());
        // The host-level answer lands in the GLOBAL monitor section.
        assert_eq!(parsed.monitor.as_ref().unwrap().hang_timeout, Some(60));
        // The GLOBAL relay section is no longer wizard-managed (relay is
        // per-DUT under the gate) — it stays untouched on update.
        assert!(parsed.relay.is_none());
        assert!(
            outcome
                .changes
                .iter()
                .any(|c| c.contains("serial.baudrate")),
            "{:?}",
            outcome.changes
        );
    }

    /// Two advanced answers in the SAME previously-absent global section must
    /// produce ONE section block — a separate `"monitor": { ... }` object per
    /// key would duplicate the field and the strict re-parse (after every op)
    /// would abort the whole update.
    #[test]
    fn test_wizard_update_adds_key_in_new_section() {
        let _guard = lock_env();
        let tmp = TempDir::new().unwrap();
        unsafe {
            std::env::remove_var("TARGET_CONF");
        }
        let path = tmp.path().join(CONFIG_FILE_NAME);
        std::fs::write(
            &path,
            "{\n  \"dev_hosts\": [\n    { \"ip\": \"10.0.0.1\", \"user\": \"u\", \"pass\": \"\", \"duts\": [ { \"dut_name\": \"b1\", \"serial\": { \"port\": 41000 } } ] }\n  ]\n}\n",
        )
        .unwrap();

        // Same counts/values (no host/dut ops), then the advanced group with
        // `--advanced`: set BOTH monitor keys, skip the rest.
        let mut inputs: Vec<String> = vec![
            "1".into(),
            "10.0.0.1".into(),
            "u".into(),
            "".into(),
            "".into(), // host name (optional) -> ""
            "1".into(),
            "b1".into(),
            "41000".into(),
            "root".into(),
        ];
        inputs.extend(dut_advanced_empties());
        for spec in ADVANCED_FIELDS
            .iter()
            .filter(|s| !dut_owned_advanced(s.key))
        {
            inputs.push(match spec.key {
                "monitor.hang_timeout" => "60".into(),
                _ => String::new(), // empty → skip (default is informational)
            });
        }
        let mut io = CannedIO::new(&[]);
        io.inputs = inputs.into();
        let outcome = run_init(
            &mut io,
            &InitOptions {
                advanced: true,
                ..Default::default()
            },
            tmp.path(),
        )
        .unwrap();

        let result = std::fs::read_to_string(&path).unwrap();
        let parsed = parse_jsonc_text(&result).unwrap_or_else(|e| panic!("{e}\n{result}"));
        let monitor = parsed.monitor.as_ref().expect("monitor section written");
        assert_eq!(monitor.hang_timeout, Some(60));
        // Exactly one monitor section object in the text.
        assert_eq!(result.matches("\"monitor\"").count(), 1, "{result}");
        assert!(
            outcome
                .changes
                .iter()
                .any(|c| c.contains("monitor.hang_timeout")),
            "{:?}",
            outcome.changes
        );
    }

    #[test]
    fn test_wizard_create_invalid_values_never_land_on_disk() {
        let _guard = lock_env();
        let tmp = TempDir::new().unwrap();
        unsafe {
            std::env::remove_var("TARGET_CONF");
        }
        // Relay type must be a SUPPORTED backend — with the relay gate ON
        // the validated write must refuse and leave no file behind.
        let inputs = [
            "1",
            "10.0.0.1",
            "",
            "",
            "",
            "1",
            "",
            "41002",
            "",
            "bogus-relay", // relay type (gate ON below)
            "",            // relay.port
            "",            // relay.reset_ch
            "",            // relay.download_ch
            "",            // relay.power_ch
            "",            // relay.reset_time_ms
            "",            // relay.power_off_time_ms
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "", // dut advanced (bare Enter x13)
        ];
        let mut io = CannedIO::new(&inputs);
        io.confirms.push_back(true); // relay gate: YES
        let err = run_init(&mut io, &quick_opts(), tmp.path()).unwrap_err();
        assert!(err.contains("unsupported relay.type"), "{err}");
        assert!(
            !tmp.path().join(CONFIG_FILE_NAME).exists(),
            "invalid config must never land on disk"
        );
        assert!(!tmp.path().join(".target.jsonc.tmp").exists());
    }

    #[test]
    fn test_wizard_update_refuses_unparseable_file() {
        let _guard = lock_env();
        let tmp = TempDir::new().unwrap();
        unsafe {
            std::env::remove_var("TARGET_CONF");
        }
        let path = tmp.path().join(CONFIG_FILE_NAME);
        std::fs::write(&path, "this is not jsonc {{{").unwrap();
        let mut io = CannedIO::new(&[]);
        let err = run_init(&mut io, &quick_opts(), tmp.path()).unwrap_err();
        assert!(err.contains("does not parse"), "{err}");
        assert!(err.contains("refusing to edit"), "{err}");
    }

    #[test]
    fn test_wizard_update_no_changes() {
        let _guard = lock_env();
        let tmp = TempDir::new().unwrap();
        unsafe {
            std::env::remove_var("TARGET_CONF");
        }
        let path = tmp.path().join(CONFIG_FILE_NAME);
        std::fs::write(&path, LOCATE_FIXTURE).unwrap();
        // Same answers as the fixture (1 host, 2 duts).
        let mut inputs: Vec<String> = [
            "1", "10.0.0.1", "linaro", "", "", "2", "board1", "41000", "root", "ch340", "", "", "",
            "", "", "",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        inputs.extend(dut_advanced_empties());
        inputs.extend(["board2".into(), "41001".into(), "root".into()]);
        inputs.extend(dut_advanced_empties());
        let mut io = CannedIO::new(&[]);
        io.inputs = inputs.into();
        let outcome = run_init(&mut io, &quick_opts(), tmp.path()).unwrap();
        assert!(
            outcome.changes.iter().any(|c| c.contains("unchanged")),
            "{:?}",
            outcome.changes
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            LOCATE_FIXTURE,
            "unchanged file must be byte-identical"
        );
    }

    #[test]
    fn test_mcp_json_missing_is_created_via_run_init() {
        let _guard = lock_env();
        let tmp = TempDir::new().unwrap();
        unsafe {
            std::env::remove_var("TARGET_CONF");
        }
        let mut io = CannedIO::new(&quick_with_dut_advanced());
        let outcome = run_init(&mut io, &quick_opts(), tmp.path()).unwrap();
        let parsed = crate::config::parse_jsonc_value(
            &std::fs::read_to_string(tmp.path().join(".mcp.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(parsed["mcpServers"]["sermcp"]["command"], "sermcp");
        assert!(
            outcome
                .changes
                .iter()
                .any(|c| c.contains("generated .mcp.json"))
        );
    }

    #[test]
    fn test_mcp_json_existing_without_entry_is_merged_via_run_init() {
        let _guard = lock_env();
        let tmp = TempDir::new().unwrap();
        unsafe {
            std::env::remove_var("TARGET_CONF");
        }
        // JSONC with a comment + an unrelated top-level key + another
        // server: all three must survive the merge (the comment is dropped
        // by the Value round-trip — documented behavior).
        std::fs::write(
            tmp.path().join(".mcp.json"),
            "{\n  // project-level comment\n  \"permissions\": {\"allow\": [\"Bash\"]},\n  \"mcpServers\": {\"other-server\": {\"command\": \"other\"}}\n}\n",
        )
        .unwrap();
        let mut io = CannedIO::new(&quick_with_dut_advanced());
        let outcome = run_init(&mut io, &quick_opts(), tmp.path()).unwrap();
        let text = std::fs::read_to_string(tmp.path().join(".mcp.json")).unwrap();
        let parsed = crate::config::parse_jsonc_value(&text).unwrap();
        assert_eq!(parsed["permissions"]["allow"][0], "Bash");
        assert_eq!(parsed["mcpServers"]["other-server"]["command"], "other");
        assert_eq!(parsed["mcpServers"]["sermcp"]["command"], "sermcp");
        assert!(
            outcome
                .changes
                .iter()
                .any(|c| c.contains("added sermcp entry")),
            "{:?}",
            outcome.changes
        );
    }

    #[test]
    fn test_mcp_json_existing_entry_left_byte_identical() {
        let _guard = lock_env();
        let tmp = TempDir::new().unwrap();
        unsafe {
            std::env::remove_var("TARGET_CONF");
        }
        // Whatever the existing sermcp entry looks like (even a
        // non-canonical one) it is never replaced.
        let original =
            "{\n  // keep me\n  \"mcpServers\": {\"sermcp\": {\"command\": \"custom\"}}\n}\n";
        std::fs::write(tmp.path().join(".mcp.json"), original).unwrap();
        let mut io = CannedIO::new(&quick_with_dut_advanced());
        let outcome = run_init(&mut io, &quick_opts(), tmp.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(tmp.path().join(".mcp.json")).unwrap(),
            original
        );
        assert!(
            outcome
                .changes
                .iter()
                .any(|c| c.contains("already has a sermcp entry")),
            "{:?}",
            outcome.changes
        );
    }

    #[test]
    fn test_mcp_json_merge_runs_on_update_path_too() {
        // The UPDATE wizard path (existing .target.jsonc) must reconcile
        // .mcp.json exactly like CREATE does.
        let _guard = lock_env();
        let tmp = TempDir::new().unwrap();
        unsafe {
            std::env::remove_var("TARGET_CONF");
        }
        let path = tmp.path().join(CONFIG_FILE_NAME);
        std::fs::write(
            &path,
            "{\n  \"dev_hosts\": [\n    {\n      \"ip\": \"10.0.0.1\",\n      \"user\": \"u\",\n      \"pass\": \"\",\n      \"duts\": [\n        {\n          \"dut_name\": \"board1\",\n          \"serial\": { \"port\": 41000 },\n          \"target\": { \"login_user\": \"root\" },\n          \"relay\": { \"type\": \"ch340\" }\n        }\n      ]\n    }\n  ]\n}\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join(".mcp.json"),
            "{\n  \"mcpServers\": {\"other\": {\"command\": \"x\"}}\n}\n",
        )
        .unwrap();
        let mut io = CannedIO::new(&[]);
        let outcome = run_init(&mut io, &non_interactive_opts(), tmp.path()).unwrap();
        assert!(!outcome.config_created);
        let parsed = crate::config::parse_jsonc_value(
            &std::fs::read_to_string(tmp.path().join(".mcp.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(parsed["mcpServers"]["other"]["command"], "x");
        assert_eq!(parsed["mcpServers"]["sermcp"]["command"], "sermcp");
    }

    #[test]
    fn test_mcp_json_malformed_is_loud_error_not_clobbered() {
        let _guard = lock_env();
        let tmp = TempDir::new().unwrap();
        unsafe {
            std::env::remove_var("TARGET_CONF");
        }
        let original = "{ not jsonc at all";
        std::fs::write(tmp.path().join(".mcp.json"), original).unwrap();
        let mut io = CannedIO::new(&quick_with_dut_advanced());
        let outcome = run_init(&mut io, &quick_opts(), tmp.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(tmp.path().join(".mcp.json")).unwrap(),
            original,
            "unparseable .mcp.json must never be edited"
        );
        assert!(
            outcome.changes.iter().any(|c| c.contains("warning:")),
            "{:?}",
            outcome.changes
        );
    }

    // ── Pure merge helper (no TTY, no IO) ─────────────────────────────

    fn test_entry() -> serde_json::Value {
        sermcp_entry(Path::new("/proj/.target.jsonc"))
    }

    #[test]
    fn test_merge_missing_file_creates_canonical_shape() {
        let (root, action) = merge_sermcp_entry(None, test_entry()).unwrap();
        assert_eq!(action, McpJsonAction::Created);
        assert_eq!(root["mcpServers"]["sermcp"]["command"], "sermcp");
        assert!(root["mcpServers"]["sermcp"].get("args").is_none());
        assert_eq!(
            root["mcpServers"]["sermcp"]["env"]["TARGET_CONF"],
            "/proj/.target.jsonc"
        );
    }

    #[test]
    fn test_merge_preserves_every_other_key_verbatim_and_order() {
        let existing = serde_json::json!({
            "permissions": {"allow": ["Bash"]},
            "mcpServers": {"other-server": {"command": "other"}},
        });
        let (root, action) = merge_sermcp_entry(Some(existing), test_entry()).unwrap();
        assert_eq!(action, McpJsonAction::Merged);
        assert_eq!(root["permissions"]["allow"][0], "Bash");
        assert_eq!(root["mcpServers"]["other-server"]["command"], "other");
        assert_eq!(root["mcpServers"]["sermcp"]["command"], "sermcp");
        // Key order is preserved (serde_json preserve_order): insertion
        // order survives the round-trip and the merged entry lands last.
        let keys: Vec<&str> = root
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        assert_eq!(keys, ["permissions", "mcpServers"]);
        let server_keys: Vec<&str> = root["mcpServers"]
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        assert_eq!(server_keys, ["other-server", "sermcp"]);
    }

    #[test]
    fn test_merge_without_mcp_servers_key_adds_it() {
        let existing = serde_json::json!({"permissions": {"allow": []}});
        let (root, action) = merge_sermcp_entry(Some(existing), test_entry()).unwrap();
        assert_eq!(action, McpJsonAction::Merged);
        assert_eq!(root["permissions"]["allow"].as_array().unwrap().len(), 0);
        assert_eq!(root["mcpServers"]["sermcp"]["command"], "sermcp");
    }

    #[test]
    fn test_merge_existing_entry_is_untouched() {
        let existing = serde_json::json!({
            "mcpServers": {"sermcp": {"command": "custom-entry"}}
        });
        let (root, action) = merge_sermcp_entry(Some(existing.clone()), test_entry()).unwrap();
        assert_eq!(action, McpJsonAction::Untouched);
        assert_eq!(root, existing, "existing entry must never be replaced");
    }

    #[test]
    fn test_merge_non_object_root_is_error() {
        for bad in [
            serde_json::json!([]),
            serde_json::json!("str"),
            serde_json::json!(42),
        ] {
            let err = merge_sermcp_entry(Some(bad), test_entry()).unwrap_err();
            assert!(err.contains("top level"), "{err}");
        }
    }

    #[test]
    fn test_merge_non_object_mcp_servers_is_error() {
        let existing = serde_json::json!({"mcpServers": ["sermcp"]});
        let err = merge_sermcp_entry(Some(existing), test_entry()).unwrap_err();
        assert!(err.contains("mcpServers"), "{err}");
    }

    #[test]
    fn test_resolve_target_env_and_plain_create() {
        let _guard = lock_env();
        let tmp = TempDir::new().unwrap();
        // TARGET_CONF to a missing .jsonc → CREATE there.
        let p = tmp.path().join("custom.jsonc");
        unsafe {
            std::env::set_var("TARGET_CONF", p.to_str().unwrap());
        }
        let (path, existing) = resolve_target(tmp.path()).unwrap();
        assert_eq!(path, p);
        assert!(existing.is_none());
        // TARGET_CONF is honored for any extension (no format check); an
        // existing file is read and its parse errors surface later.
        let other = tmp.path().join("other-ext.toml");
        std::fs::write(&other, "[serial]\n").unwrap();
        unsafe {
            std::env::set_var("TARGET_CONF", other.to_str().unwrap());
        }
        let (path, existing) = resolve_target(tmp.path()).unwrap();
        assert_eq!(path, other);
        assert_eq!(existing.as_deref(), Some("[serial]\n"));
        // No TARGET_CONF and no walk-up .target.jsonc → CREATE at cwd
        // (stray files in the tree are never consulted).
        unsafe {
            std::env::remove_var("TARGET_CONF");
        }
        std::fs::write(tmp.path().join(".target.txt"), "[serial]\n").unwrap();
        let (path, existing) = resolve_target(tmp.path()).unwrap();
        assert_eq!(path, tmp.path().join(CONFIG_FILE_NAME));
        assert!(existing.is_none());
    }

    // ── Argument parsing ──────────────────────────────────────────────

    #[test]
    fn test_init_options_from_args() {
        let s = |v: &str| v.to_string();
        let opts =
            InitOptions::from_args(&[s("--non-interactive"), s("--quick"), s("--force")], false)
                .unwrap();
        assert!(opts.non_interactive && opts.quick && opts.force);
        assert!(!opts.advanced);

        // Env flag.
        let opts = InitOptions::from_args(&[], true).unwrap();
        assert!(opts.non_interactive);

        // Unknown flag is loud.
        let err = InitOptions::from_args(&[s("--bogus")], false).unwrap_err();
        assert!(err.contains("--bogus"), "{err}");

        // Mutually exclusive.
        let err = InitOptions::from_args(&[s("--quick"), s("--advanced")], false).unwrap_err();
        assert!(err.contains("mutually exclusive"), "{err}");
    }

    /// Transport selectors: --tui forces the full-screen TUI, --plain
    /// forces plain line prompts; the two are mutually exclusive and both
    /// default off (the binary's dispatch gate decides).
    #[test]
    fn test_init_options_tui_plain_flags() {
        let s = |v: &str| v.to_string();
        let opts = InitOptions::from_args(&[s("--tui")], false).unwrap();
        assert!(opts.tui && !opts.plain);
        let opts = InitOptions::from_args(&[s("--plain")], false).unwrap();
        assert!(!opts.tui && opts.plain);
        let opts = InitOptions::from_args(&[s("--tui"), s("--quick")], false).unwrap();
        assert!(opts.tui && opts.quick);
        // Defaults: neither selected.
        assert!(!InitOptions::default().tui && !InitOptions::default().plain);
        // Mutually exclusive with each other.
        let err = InitOptions::from_args(&[s("--tui"), s("--plain")], false).unwrap_err();
        assert!(err.contains("mutually exclusive"), "{err}");
    }

    // ── TUI-era wizard policy (count Enter=default, notes, review gate) ──

    /// Test-only PromptIO wrapper that records every `note()` call and
    /// every `input` (prompt, default) pair, delegating the rest to
    /// CannedIO.
    struct RecordingIO {
        inner: CannedIO,
        notes: Vec<(NoteKind, String)>,
        calls: Vec<(String, Option<String>)>,
    }

    impl RecordingIO {
        fn new(inputs: &[&str]) -> Self {
            Self {
                inner: CannedIO::new(inputs),
                notes: Vec::new(),
                calls: Vec::new(),
            }
        }
    }

    impl PromptIO for RecordingIO {
        fn input(&mut self, prompt: &str, default: Option<&str>) -> Result<String, String> {
            self.calls
                .push((prompt.to_string(), default.map(str::to_string)));
            self.inner.input(prompt, default)
        }
        fn confirm(&mut self, prompt: &str, default: bool) -> Result<bool, String> {
            self.inner.confirm(prompt, default)
        }
        fn note(&mut self, kind: NoteKind, msg: &str) {
            self.notes.push((kind, msg.to_string()));
        }
    }

    /// Test-only PromptIO wrapper whose `finish()` (the Review gate) can be
    /// forced to Ok or Err; records the pending change lines it was shown.
    struct FinishingIO {
        inner: CannedIO,
        outcome: Result<(), String>,
        seen: Vec<String>,
    }

    impl FinishingIO {
        fn new(inputs: &[&str], outcome: Result<(), String>) -> Self {
            Self {
                inner: CannedIO::new(inputs),
                outcome,
                seen: Vec::new(),
            }
        }
    }

    impl PromptIO for FinishingIO {
        fn input(&mut self, prompt: &str, default: Option<&str>) -> Result<String, String> {
            self.inner.input(prompt, default)
        }
        fn confirm(&mut self, prompt: &str, default: bool) -> Result<bool, String> {
            self.inner.confirm(prompt, default)
        }
        fn finish(&mut self, _target_path: &Path, changes: &[String]) -> Result<(), String> {
            self.seen = changes.to_vec();
            self.outcome.clone()
        }
    }

    /// P0: bare Enter on BOTH count questions accepts the shown default (1)
    /// and proceeds IMMEDIATELY to the per-host / per-DUT fields — no silent
    /// re-ask of the same screen (the reported TTY stall).
    #[test]
    fn test_wizard_count_enter_accepts_default_proceeds_to_host_fields() {
        let _guard = lock_env();
        let tmp = TempDir::new().unwrap();
        unsafe {
            std::env::remove_var("TARGET_CONF");
        }
        let mut inputs: Vec<String> = ["", "10.0.0.1", "", "", "", "", "", "41002", "", ""]
            .iter()
            .map(|s| s.to_string())
            .collect();
        inputs.extend(dut_advanced_empties());
        let mut io = CannedIO::new(&[]);
        io.inputs = inputs.into();
        let outcome = run_init(&mut io, &quick_opts(), tmp.path()).unwrap();
        assert!(outcome.config_created);
        let parsed =
            parse_jsonc_text(&std::fs::read_to_string(&outcome.config_path).unwrap()).unwrap();
        assert_eq!(parsed.dev_hosts.len(), 1);
        assert_eq!(parsed.dev_hosts[0].duts.len(), 1);
        assert_eq!(parsed.dev_hosts[0].duts[0].dut_name, "dut1");
        assert_eq!(
            parsed.dev_hosts[0].duts[0]
                .serial
                .as_ref()
                .unwrap()
                .port
                .as_ref()
                .unwrap()
                .to_value_string(),
            "41002"
        );
    }

    /// Explicit N proceeds to N host/DUT field sets in sequence (the count
    /// answer drives the nested loops; answers must not misalign).
    #[test]
    fn test_wizard_count_explicit_number_proceeds_in_sequence() {
        let _guard = lock_env();
        let tmp = TempDir::new().unwrap();
        unsafe {
            std::env::remove_var("TARGET_CONF");
        }
        let inputs = [
            "2",        // host count
            "10.0.0.1", // host 0 ip
            "",         // host 0 user
            "",         // host 0 pass
            "",         // host 0 name
            "2",        // host 0 DUT count
            "",         // dut 0 name -> dut1
            "41002",    // dut 0 port
            "",         // dut 0 login user
            "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "",
            "",      // dut 0 advanced (bare Enter x17)
            "",      // dut 1 name -> dut2
            "41003", // dut 1 port
            "",      // dut 1 login user
            "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "",
            "",         // dut 1 advanced (bare Enter x17)
            "10.0.0.2", // host 1 ip
            "",         // host 1 user
            "",         // host 1 pass
            "",         // host 1 name
            "1",        // host 1 DUT count
            "",         // dut name -> dut3
            "41004",    // port
            "",         // login user
            "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "",
            "", // dut advanced (bare Enter x17)
        ];
        let mut io = CannedIO::new(&inputs);
        let outcome = run_init(&mut io, &quick_opts(), tmp.path()).unwrap();
        let parsed =
            parse_jsonc_text(&std::fs::read_to_string(&outcome.config_path).unwrap()).unwrap();
        assert_eq!(parsed.dev_hosts.len(), 2);
        assert_eq!(parsed.dev_hosts[0].ip.as_deref(), Some("10.0.0.1"));
        assert_eq!(parsed.dev_hosts[1].ip.as_deref(), Some("10.0.0.2"));
        assert_eq!(parsed.dev_hosts[0].duts.len(), 2);
        assert_eq!(parsed.dev_hosts[1].duts.len(), 1);
        assert_eq!(parsed.dev_hosts[0].duts[0].dut_name, "dut1");
        assert_eq!(parsed.dev_hosts[0].duts[1].dut_name, "dut2");
        assert_eq!(parsed.dev_hosts[1].duts[0].dut_name, "dut3");
    }

    /// Invalid count ("abc") -> error note + re-ask the SAME question; "0"
    /// is a VALID answer (zero hosts -> zero DUTs in the written config).
    #[test]
    fn test_wizard_count_invalid_reasks_with_note() {
        let _guard = lock_env();
        let tmp = TempDir::new().unwrap();
        unsafe {
            std::env::remove_var("TARGET_CONF");
        }
        let mut inputs: Vec<String> = [
            "abc", "1",        // host count: invalid, valid
            "10.0.0.1", // ip
            "", "", "",      // user, pass, name
            "1",     // dut count
            "",      // dut name -> dut1
            "41002", // port
            "",      // login user
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        inputs.extend(dut_advanced_empties());
        let mut io = RecordingIO::new(&[]);
        io.inner.inputs = inputs.into();
        let outcome = run_init(&mut io, &quick_opts(), tmp.path()).unwrap();
        let parsed =
            parse_jsonc_text(&std::fs::read_to_string(&outcome.config_path).unwrap()).unwrap();
        assert_eq!(parsed.dev_hosts.len(), 1);
        let errors: Vec<&(NoteKind, String)> = io
            .notes
            .iter()
            .filter(|(k, _)| *k == NoteKind::Error)
            .collect();
        assert_eq!(errors.len(), 1, "{:?}", io.notes);
        assert!(
            errors[0].1.contains("not a non-negative integer"),
            "{:?}",
            errors
        );
    }

    /// Required fields re-ask with an explanatory note (empty ip, port "0").
    #[test]
    fn test_wizard_required_notes_before_reask() {
        let _guard = lock_env();
        let tmp = TempDir::new().unwrap();
        unsafe {
            std::env::remove_var("TARGET_CONF");
        }
        let mut io = RecordingIO::new(&[
            "1",        // host count
            "",         // ip EMPTY -> note + re-ask
            "10.0.0.1", // ip
            "", "", "",      // user, pass, name
            "1",     // dut count
            "",      // dut name
            "0",     // port "0" -> note + re-ask
            "41002", // port
            "",      // login user
            "",      // (stale relay-type slot; now the first dut-advanced question)
            "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "",
        ]);
        let outcome = run_init(&mut io, &quick_opts(), tmp.path()).unwrap();
        let parsed =
            parse_jsonc_text(&std::fs::read_to_string(&outcome.config_path).unwrap()).unwrap();
        assert_eq!(parsed.dev_hosts[0].ip.as_deref(), Some("10.0.0.1"));
        let errors: Vec<&String> = io
            .notes
            .iter()
            .filter(|(k, _)| *k == NoteKind::Error)
            .map(|(_, m)| m)
            .collect();
        assert!(
            errors.iter().any(|m| m.contains("value is required")),
            "{errors:?}"
        );
        assert!(
            errors.iter().any(|m| m.contains("serial.port cannot be 0")),
            "{errors:?}"
        );
    }

    /// REGRESSION: ask_required with a NON-empty shown default — an empty
    /// submit returns that default SILENTLY (no "value is required" note,
    /// no re-ask). The default IS a value, so nothing is missing.
    #[test]
    fn test_ask_required_empty_enter_accepts_shown_default_silently() {
        let mut io = RecordingIO::new(&[""]);
        let got = ask_required(
            &mut io,
            "Dev host ip (ssh / ser2net host)",
            Some("10.0.0.1".into()),
        )
        .unwrap();
        assert_eq!(got, "10.0.0.1");
        assert!(
            io.notes.iter().all(|(k, _)| *k != NoteKind::Error),
            "{:?}",
            io.notes
        );
    }

    /// ...while the EMPTY-default case keeps its enforcement: error note +
    /// re-ask until a non-empty answer arrives.
    #[test]
    fn test_ask_required_empty_default_still_notes_and_reasks() {
        let mut io = RecordingIO::new(&["", "10.0.0.1"]);
        let got = ask_required(&mut io, "Dev host ip (ssh / ser2net host)", None).unwrap();
        assert_eq!(got, "10.0.0.1");
        assert!(
            io.notes
                .iter()
                .any(|(k, m)| *k == NoteKind::Error && m.contains("value is required")),
            "{:?}",
            io.notes
        );
    }

    /// End-to-end: UPDATE with bare Enter on every required field (host ip,
    /// both serial ports) accepts the shown prefill silently — zero Error
    /// notes and a byte-identical file (identical-value skip).
    #[test]
    fn test_wizard_update_enter_accepts_required_defaults_silently() {
        let _guard = lock_env();
        let tmp = TempDir::new().unwrap();
        unsafe {
            std::env::remove_var("TARGET_CONF");
        }
        let path = tmp.path().join(CONFIG_FILE_NAME);
        std::fs::write(&path, LOCATE_FIXTURE).unwrap();
        let mut io = RecordingIO::new(&[
            "1",      // host count
            "",       // ip -> 10.0.0.1 (shown default)
            "linaro", // user
            "",       // pass
            "",       // host name
            "2",      // dut count
            "",       // dut 0 name -> board1
            "",       // dut 0 port -> 41000 (shown default)
            "root",   // login user
            "ch340",  // relay type (gate ON: board1 has a relay section)
            "", "", "", "", "", "", // port/channels/times (inherit)
            "", "", "", "", "", "", "", "", "", "", "", "",
            "",     // dut 0 advanced (bare Enter x13)
            "",     // dut 1 name -> board2
            "",     // dut 1 port -> "41001" (shown default)
            "root", // login user
            "", "", "", "", "", "", "", "", "", "", "", "",
            "", // dut 1 advanced (bare Enter x13)
        ]);
        let outcome = run_init(&mut io, &quick_opts(), tmp.path()).unwrap();
        assert!(!outcome.config_created);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            LOCATE_FIXTURE,
            "bare Enter on required fields must keep the file byte-identical"
        );
        assert!(
            io.notes.iter().all(|(k, _)| *k != NoteKind::Error),
            "{:?}",
            io.notes
        );
    }

    /// Name auto-name/auto-advance announcements also surface as notes.
    #[test]
    fn test_wizard_name_announcements_are_noted() {
        let _guard = lock_env();
        let tmp = TempDir::new().unwrap();
        unsafe {
            std::env::remove_var("TARGET_CONF");
        }
        // Second DUT name typed identically to the first -> collision ->
        // auto-advance (announcement + note). DUT count question is Enter.
        let mut io = RecordingIO::new(&[
            "1",        // host count
            "10.0.0.1", // ip
            "", "", "",      // user, pass, name
            "2",     // dut count
            "dutX",  // dut 0 name (typed)
            "41002", // dut 0 port
            "",      // login user
            "", "", "", "", "", "", "", "", "", "", "", "",
            "",      // dut 0 advanced (bare Enter x13)
            "dutX",  // dut 1 name COLLIDES -> auto-advance
            "41003", // dut 1 port
            "",      // login user
            "", "", "", "", "", "", "", "", "", "", "", "",
            "", // dut 1 advanced (bare Enter x13)
        ]);
        let outcome = run_init(&mut io, &quick_opts(), tmp.path()).unwrap();
        let parsed =
            parse_jsonc_text(&std::fs::read_to_string(&outcome.config_path).unwrap()).unwrap();
        assert_eq!(parsed.dev_hosts[0].duts[0].dut_name, "dutX");
        assert_eq!(parsed.dev_hosts[0].duts[1].dut_name, "dut1");
        assert!(
            outcome
                .changes
                .iter()
                .any(|c| c.contains("already taken — using")),
            "{:?}",
            outcome.changes
        );
        assert!(
            io.notes
                .iter()
                .any(|(k, m)| *k == NoteKind::Info && m.contains("already taken — using")),
            "{:?}",
            io.notes
        );
    }

    /// CREATE --advanced with bare Enter everywhere: ENTER_ACCEPTS_DEFAULT
    /// keys accept their natural default (written), excluded keys keep the
    /// skip/inherit semantics (absent).
    #[test]
    fn test_wizard_advanced_enter_accepts_natural_defaults() {
        let _guard = lock_env();
        let tmp = TempDir::new().unwrap();
        unsafe {
            std::env::remove_var("TARGET_CONF");
        }
        let mut inputs: Vec<String> = QUICK_ANSWERS.iter().map(|s| s.to_string()).collect();
        inputs.extend(dut_advanced_empties()); // per-DUT advanced: hints only
        for _ in ADVANCED_FIELDS
            .iter()
            .filter(|s| !dut_owned_advanced(s.key))
        {
            inputs.push(String::new()); // host-level advanced (4)
        }
        let mut io = CannedIO::new(&[]);
        io.inputs = inputs.into();
        let outcome = run_init(
            &mut io,
            &InitOptions {
                advanced: true,
                ..Default::default()
            },
            tmp.path(),
        )
        .unwrap();
        assert!(outcome.config_created);
        let parsed =
            parse_jsonc_text(&std::fs::read_to_string(&outcome.config_path).unwrap()).unwrap();
        // HOST-LEVEL AcceptDefault keys still accept their natural default
        // on bare Enter and are WRITTEN globally (reqs 9/10).
        let monitor = parsed.monitor.as_ref().unwrap();
        assert_eq!(monitor.hang_timeout, Some(60));
        assert_eq!(monitor.max_log_file_size, Some(100));
        assert_eq!(monitor.learner_stage_threshold, Some(0.45));
        assert_eq!(monitor.learner_crash_threshold, Some(0.50));
        assert!(
            monitor.crash_keywords.is_none(),
            "crash_keywords is DUT-owned now — never written globally"
        );
        // DUT-owned natural defaults are INFORMATIONAL hints only: an
        // untouched per-DUT field writes nothing.
        let dut = &parsed.dev_hosts[0].duts[0];
        assert_eq!(
            dut.serial
                .as_ref()
                .unwrap()
                .baudrate
                .as_ref()
                .map(NumberOrString::to_value_string)
                .as_deref(),
            Some("460800")
        );
        assert!(dut.monitor.is_none());
        assert!(dut.flash.is_none());
        assert!(dut.uboot.is_none());
        assert!(dut.control.is_none());
        // Excluded keys keep skip/inherit on Enter (""-defaults), and the
        // relay gate OFF on create => the relay section is OMITTED (no
        // per-DUT relay questions were asked).
        assert!(parsed.target.is_none());
        assert!(parsed.serial.is_none());
        assert!(parsed.relay.is_none(), "gate off -> relay omitted");
        assert!(parsed.dev_hosts[0].duts[0].relay.is_none());
    }

    /// monitor.reference_log's composed default uses the FIRST DUT name
    /// (req #10: the name is fixed before the question is asked).
    #[test]
    fn test_wizard_reference_log_default_composed_from_dut_name() {
        let _guard = lock_env();
        let tmp = TempDir::new().unwrap();
        unsafe {
            std::env::remove_var("TARGET_CONF");
        }
        let mut inputs: Vec<String> = vec![
            "1".into(),
            "10.0.0.1".into(),
            "".into(),
            "".into(),
            "".into(), // host name
            "1".into(),
            "boardX".into(), // DUT name (typed, not the default)
            "41002".into(),
            "".into(),
        ];
        // Per-DUT advanced: reference_log is DUT-owned (DUT_ADVANCED_KEYS
        // index 6) — TYPE a value for the DUT; the rest bare Enter.
        let mut dut_adv: Vec<String> = dut_advanced_empties();
        dut_adv[6] = "/tmp/ref.log".into();
        inputs.extend(dut_adv);
        for _ in ADVANCED_FIELDS
            .iter()
            .filter(|s| !dut_owned_advanced(s.key))
        {
            inputs.push(String::new());
        }
        let mut io = CannedIO::new(&[]);
        io.inputs = inputs.into();
        let outcome = run_init(
            &mut io,
            &InitOptions {
                advanced: true,
                ..Default::default()
            },
            tmp.path(),
        )
        .unwrap();
        let parsed =
            parse_jsonc_text(&std::fs::read_to_string(&outcome.config_path).unwrap()).unwrap();
        // The typed value lands in THAT DUT's override section — not the
        // global fallback (reference_log moved per-DUT).
        assert_eq!(
            parsed.dev_hosts[0].duts[0]
                .monitor
                .as_ref()
                .unwrap()
                .reference_log
                .as_deref(),
            Some("/tmp/ref.log")
        );
        assert!(
            parsed
                .monitor
                .as_ref()
                .and_then(|m| m.reference_log.clone())
                .is_none(),
            "the GLOBAL monitor carries no reference_log — it is DUT-owned"
        );
        assert!(outcome.config_created);
    }

    /// UPDATE --advanced with bare Enter everywhere: the shown default is
    /// the CURRENT value -> identical-value skip -> byte-identical file.
    #[test]
    fn test_wizard_advanced_enter_on_update_keeps_existing_values() {
        let _guard = lock_env();
        let tmp = TempDir::new().unwrap();
        unsafe {
            std::env::remove_var("TARGET_CONF");
        }
        // Step 1: CREATE a fully-populated config (typed values everywhere).
        let mut create_inputs: Vec<String> = vec![
            "1".into(),
            "10.0.0.1".into(),
            "linaro".into(),
            "".into(),
            "".into(), // host name
            "1".into(),
            "".into(),
            "41002".into(),
            "".into(),
        ];
        // DUT-owned advanced values (asked per DUT, gate-independent).
        // NOTE: `uboot.pattern` is skipped by the walk when the strategy is
        // not "pattern" (no input consumed) — the canned stream must mirror
        // the walk's actual consumption, so pattern gets NO slot here.
        for k in DUT_ADVANCED_KEYS.iter().filter(|k| **k != "uboot.pattern") {
            create_inputs.push(
                match *k {
                    "serial.baudrate" => "460800",
                    "target.login_pass" => "pw",
                    "target.login_prompt" => "login:",
                    "uboot.interrupt_char" => "0x03",
                    "uboot.interrupt_strategy" => "flood",
                    "monitor.max_archived_logs" => "20",
                    "monitor.reference_log" => "/tmp/ref.log",
                    "monitor.crash_keywords" => "Oops:",
                    "flash.full_image_cmd" => "cmd1",
                    "flash.kernel_image_cmd" => "cmd2",
                    "flash.loader_bin" => "x.bin",
                    "control.dev_ctrl" => "ch340-relay",
                    "loader" => "uboot",
                    _ => "",
                }
                .into(),
            );
        }
        // Host-level advanced values (the global fallback set).
        for spec in ADVANCED_FIELDS
            .iter()
            .filter(|s| !dut_owned_advanced(s.key))
        {
            create_inputs.push(
                match spec.key {
                    "monitor.hang_timeout" => "30",
                    "monitor.max_log_file_size" => "50",
                    "monitor.learner_stage_threshold" => "0.60",
                    "monitor.learner_crash_threshold" => "0.70",
                    _ => "",
                }
                .into(),
            );
        }
        let mut io = CannedIO::new(&[]);
        io.inputs = create_inputs.into();
        run_init(
            &mut io,
            &InitOptions {
                advanced: true,
                ..Default::default()
            },
            tmp.path(),
        )
        .unwrap();
        let before = std::fs::read_to_string(tmp.path().join(CONFIG_FILE_NAME)).unwrap();

        // Step 2: UPDATE — bare Enter everywhere (quick values back to the
        // defaults = current values; advanced all "").
        let mut update_inputs: Vec<String> = vec![
            "1".into(),
            "10.0.0.1".into(),
            "linaro".into(),
            "".into(),
            "".into(), // host name
            "1".into(),
            "".into(),
            "41002".into(),
            "".into(),
        ];
        update_inputs.extend(dut_advanced_empties());
        for _ in ADVANCED_FIELDS
            .iter()
            .filter(|s| !dut_owned_advanced(s.key))
        {
            update_inputs.push(String::new());
        }
        let mut io = CannedIO::new(&[]);
        io.inputs = update_inputs.into();
        let outcome = run_init(
            &mut io,
            &InitOptions {
                advanced: true,
                ..Default::default()
            },
            tmp.path(),
        )
        .unwrap();
        assert!(!outcome.config_created);
        let after = std::fs::read_to_string(tmp.path().join(CONFIG_FILE_NAME)).unwrap();
        assert_eq!(
            after, before,
            "Enter-on-update must keep the file byte-identical"
        );
        assert!(
            outcome.changes.iter().any(|c| c.contains("unchanged")),
            "{:?}",
            outcome.changes
        );
    }

    /// Typed advanced values are validated IMMEDIATELY: invalid input gets a
    /// key-named error note and the SAME question is re-asked.
    #[test]
    fn test_wizard_advanced_invalid_typed_value_reasked_with_note() {
        let _guard = lock_env();
        let tmp = TempDir::new().unwrap();
        unsafe {
            std::env::remove_var("TARGET_CONF");
        }
        let mut inputs: Vec<String> = QUICK_ANSWERS.iter().map(|s| s.to_string()).collect();
        inputs.extend(dut_advanced_empties());
        for spec in ADVANCED_FIELDS
            .iter()
            .filter(|s| !dut_owned_advanced(s.key))
        {
            match spec.key {
                "monitor.max_log_file_size" => {
                    inputs.push("abc".into()); // invalid -> note + re-ask
                    inputs.push("41000".into()); // valid
                }
                _ => inputs.push(String::new()),
            }
        }
        let mut io = RecordingIO::new(&[]);
        io.inner.inputs = inputs.into();
        let outcome = run_init(
            &mut io,
            &InitOptions {
                advanced: true,
                ..Default::default()
            },
            tmp.path(),
        )
        .unwrap();
        let parsed =
            parse_jsonc_text(&std::fs::read_to_string(&outcome.config_path).unwrap()).unwrap();
        assert_eq!(
            parsed.monitor.as_ref().unwrap().max_log_file_size,
            Some(41000)
        );
        assert!(
            io.notes.iter().any(|(k, m)| {
                *k == NoteKind::Error && m.contains("monitor.max_log_file_size: invalid count: abc")
            }),
            "{:?}",
            io.notes
        );
    }

    /// Esc at the Review gate = Err BEFORE any write: zero bytes on disk,
    /// no .target.jsonc.tmp residue.
    #[test]
    fn test_wizard_finish_abort_writes_nothing() {
        let _guard = lock_env();
        let tmp = TempDir::new().unwrap();
        unsafe {
            std::env::remove_var("TARGET_CONF");
        }
        let mut io = FinishingIO::new(&quick_with_dut_advanced(), Err("aborted by user".into()));
        let err = run_init(&mut io, &quick_opts(), tmp.path()).unwrap_err();
        assert!(err.contains("aborted by user"), "{err}");
        assert!(!tmp.path().join(CONFIG_FILE_NAME).exists());
        assert!(!tmp.path().join(".target.jsonc.tmp").exists());
        // The Review gate was shown the authoritative pending change lines.
        assert!(
            io.seen.iter().any(|c| c.contains(&format!(
                "created {}",
                tmp.path().join(CONFIG_FILE_NAME).display()
            ))),
            "{:?}",
            io.seen
        );
    }

    /// Enter at the Review gate writes and the pending lines land in the
    /// outcome in the established order.
    #[test]
    fn test_wizard_finish_ok_writes() {
        let _guard = lock_env();
        let tmp = TempDir::new().unwrap();
        unsafe {
            std::env::remove_var("TARGET_CONF");
        }
        let mut io = FinishingIO::new(&quick_with_dut_advanced(), Ok(()));
        let outcome = run_init(&mut io, &quick_opts(), tmp.path()).unwrap();
        assert!(outcome.config_created);
        assert!(outcome.config_path.exists());
        let created_line = format!("created {}", outcome.config_path.display());
        assert!(io.seen.contains(&created_line), "{:?}", io.seen);
        assert_eq!(
            outcome.changes.first().map(String::as_str),
            Some(created_line.as_str())
        );
    }

    // ── Declarative schema (single source of truth) ───────────────────

    /// schema_fields(schema) for 1 host x 1 DUT equals the EXACT legacy
    /// question sequence: [HostCount, HostIp(0), HostUser(0),
    /// HostPass(0), HostName(0), DutCount(0), DutName(0,0), DutPort(0,0),
    /// DutLoginUser(0,0), DutRelayType(0,0), then ADVANCED_FIELDS in order]
    /// — the CannedIO exhaustion boundary (QUICK_ANSWERS) stays green with
    /// zero fixture reordering.
    #[test]
    fn test_schema_fields_order_matches_legacy_sequence() {
        let schema = build_form_schema(None, &quick_opts());
        let keys: Vec<FieldKey> = schema_fields(&schema).into_iter().map(|f| f.key).collect();
        let mut expected: Vec<FieldKey> = vec![
            FieldKey::HostCount,
            FieldKey::HostIp(0),
            FieldKey::HostUser(0),
            FieldKey::HostPass(0),
            FieldKey::HostName(0),
            FieldKey::DutCount(0),
            FieldKey::DutName(0, 0),
            FieldKey::DutPort(0, 0),
            FieldKey::DutLoginUser(0, 0),
            FieldKey::DutRelayGate(0, 0),
            FieldKey::DutRelayType(0, 0),
            FieldKey::DutRelayPort(0, 0),
            FieldKey::DutRelayReset(0, 0),
            FieldKey::DutRelayDownload(0, 0),
            FieldKey::DutRelayPower(0, 0),
            FieldKey::DutRelayResetTimeMs(0, 0),
            FieldKey::DutRelayPowerOffTimeMs(0, 0),
        ];
        // the DUT-owned advanced set materializes per DUT
        // (group "dut-advanced", PerHostDut) BEFORE the host-level set
        // (group "advanced", Once).
        for k in DUT_ADVANCED_KEYS {
            expected.push(FieldKey::DutAdvanced(0, 0, k));
        }
        for spec in ADVANCED_FIELDS
            .iter()
            .filter(|s| !dut_owned_advanced(s.key))
        {
            expected.push(FieldKey::Advanced(spec.key));
        }
        assert_eq!(keys, expected);
        // The quick walk asks the first 9 in exactly this order — the
        // QUICK_ANSWERS fixture is the byte-level proof (run_init tests).
        assert_eq!(&keys[..9], &expected[..9]);
    }

    /// Cross-front pin: the plain walk's prompts ARE the FORM_GROUPS
    /// labels (the walk materializes the same schema the form renders) —
    /// interpolated and suffixed exactly like ask_required renders them.
    /// The expected prompts are DERIVED from the schema here (never
    /// re-typed), so a label edit in FORM_GROUPS must flow through the
    /// walk and can never silently diverge.
    #[test]
    fn test_walk_prompts_are_form_group_labels() {
        let mut schema = build_form_schema(None, &InitOptions::default());
        let mut inputs: Vec<String> = vec![
            "1".into(),        // host count
            "10.0.0.1".into(), // host ip
            "".into(),         // user
            "".into(),         // pass
            "".into(),         // host name
            "1".into(),        // dut count
            "".into(),         // dut name -> dut1
            "41002".into(),    // port
            "".into(),         // login user
        ];
        inputs.extend(dut_advanced_empties()); // per-DUT advanced (16)
        for _ in ADVANCED_FIELDS
            .iter()
            .filter(|s| !dut_owned_advanced(s.key))
        {
            inputs.push(String::new()); // host-level advanced (4)
        }
        let mut io = RecordingIO::new(&[]);
        io.inner.inputs = inputs.into();
        io.inner.confirms.push_back(false); // per-DUT relay gate: NO
        io.inner.confirms.push_back(true); // advanced gate: yes
        // Confirm order matters: the per-DUT relay gate is asked BEFORE the
        // advanced gate, and NO skips the relay questions entirely.
        walk_schema(&mut schema, &mut io).unwrap();
        let host_group = &FORM_GROUPS[1];
        let dut_group = &FORM_GROUPS[2];
        let dut_adv_group = &FORM_GROUPS[3];
        let label_of = |group: &FormGroup, frag: &FieldKeyFrag| {
            group
                .fields
                .iter()
                .find(|d| &d.key == frag)
                .map(|d| d.label)
                .unwrap()
        };
        let mut expected: Vec<String> = vec![
            FORM_GROUPS[0].fields[0].label.to_string(),
            format!("{} (required)", label_of(host_group, &FieldKeyFrag::HostIp)),
            label_of(host_group, &FieldKeyFrag::HostUser).to_string(),
            label_of(host_group, &FieldKeyFrag::HostPass).to_string(),
            label_of(host_group, &FieldKeyFrag::HostName).to_string(),
            label_of(host_group, &FieldKeyFrag::DutCount).to_string(),
            label_of(dut_group, &FieldKeyFrag::DutName).to_string(),
            format!("{} (required)", label_of(dut_group, &FieldKeyFrag::DutPort)),
            label_of(dut_group, &FieldKeyFrag::DutLoginUser).to_string(),
        ];
        // Per-DUT advanced labels (asked per DUT BEFORE the gate).
        for k in DUT_ADVANCED_KEYS {
            // The default strategy is flood, so its conditional pattern
            // editor is not asked in this walk.
            if *k == "uboot.pattern" {
                continue;
            }
            expected.push(label_of(dut_adv_group, &FieldKeyFrag::DutAdvanced(k)).to_string());
        }
        for spec in ADVANCED_FIELDS
            .iter()
            .filter(|s| !dut_owned_advanced(s.key))
        {
            expected.push(spec.label.to_string());
        }
        let got: Vec<String> = io.calls.iter().map(|(p, _)| p.clone()).collect();
        assert_eq!(got, expected);
    }

    /// The serialized FORM_GROUPS quick part is pinned (labels are the
    /// exact current prompt literals; kinds/required/empty/default fixed).
    /// The advanced group's pin lives in the migration test below.
    #[test]
    fn test_form_groups_quick_part_pinned() {
        let value = serde_json::to_value(FORM_GROUPS).unwrap();
        let groups = value.as_array().unwrap();
        assert_eq!(groups.len(), 5);
        let pinned_quick = serde_json::json!([
            {
                "repeat": "Once",
                "title": "host-count",
                "fields": [
                    {"key": "HostCount", "kind": "Count",
                     "label": "Number of dev hosts", "required": false,
                     "empty": "AcceptDefault",
                     "default": "RuntimeCount"}
                ]
            },
            {
                "repeat": "PerHost",
                "title": "dev-host",
                "fields": [
                    {"key": "HostIp", "kind": "Text",
                     "label": "Dev host ip (ssh / ser2net host)", "required": true,
                     "empty": "AcceptDefault",
                     "default": {"Static": ""}},
                    {"key": "HostUser", "kind": "Text", "label": "Dev host ssh user",
                     "required": false, "empty": "AcceptDefault",
                     "default": {"Static": ""}},
                    {"key": "HostPass", "kind": "Text",
                     "label": "Dev host ssh password (plaintext)", "required": false,
                     "empty": "AcceptDefault",
                     "default": {"Static": ""}},
                    {"key": "HostName", "kind": "Text",
                     "label": "Dev host display name (optional — falls back to ip)",
                     "required": false, "empty": "AcceptDefault",
                     "default": {"Static": ""}},
                    {"key": "DutCount", "kind": "Count",
                     "label": "Number of DUTs on this host", "required": false,
                     "empty": "AcceptDefault",
                     "default": "RuntimeCount"}
                ]
            },
            {
                "repeat": "PerHostDut",
                "title": "dut",
                "fields": [
                    {"key": "DutName", "kind": "Text", "label": "DUT name",
                     "required": false, "empty": "AcceptDefault",
                     "default": "AutoDutName"},
                    {"key": "DutPort", "kind": "Text",
                     "label": "Serial port (ser2net port or /dev/ttyUSB0)",
                     "required": true, "empty": "AcceptDefault",
                     "default": {"Static": ""}},
                    {"key": "DutLoginUser", "kind": "Text", "label": "Target login user",
                     "required": false, "empty": "AcceptDefault",
                     "default": {"Static": "root"}},
                    {"key": "DutRelayGate", "kind": "Toggle",
                     "label": "Use relay? (reset/download/power control)",
                     "required": false, "empty": "AcceptDefault",
                     "default": "RuntimeRelayGate"},
                    {"key": "DutRelayType", "kind": "Select", "label": "Relay type",
                     "required": false, "empty": "AcceptDefault",
                     "default": {"Static": "ch340"}},
                    {"key": "DutRelayPort", "kind": "Text",
                     "label": "Relay port (0 = inherit)",
                     "required": false, "empty": "SkipInherit",
                     "default": {"Static": "0"}},
                    {"key": "DutRelayReset", "kind": "Text",
                     "label": "Relay reset (0 = inherit)",
                     "required": false, "empty": "SkipInherit",
                     "default": {"Static": "0"}},
                    {"key": "DutRelayDownload", "kind": "Text",
                     "label": "Relay download (0 = inherit)",
                     "required": false, "empty": "SkipInherit",
                     "default": {"Static": "0"}},
                    {"key": "DutRelayPower", "kind": "Text",
                     "label": "Relay power (0 = inherit)",
                     "required": false, "empty": "SkipInherit",
                     "default": {"Static": "0"}},
                    {"key": "DutRelayResetTimeMs", "kind": "Text",
                     "label": "Reset hold ms — how long the reset key stays pressed (0 = inherit)",
                     "required": false, "empty": "SkipInherit",
                     "default": {"Static": "0"}},
                    {"key": "DutRelayPowerOffTimeMs", "kind": "Text",
                     "label": "Power hold ms — how long the power key stays pressed (0 = inherit global)",
                     "required": false, "empty": "AcceptDefault",
                     "default": {"Static": "3000"}}
                ]
            }
        ]);
        assert_eq!(&groups[..3], pinned_quick.as_array().unwrap());
        // the advanced group SPLIT — group 3 is the DUT-owned
        // set (PerHostDut), group 4 the host-level fallback set (Once).
        // Both serialize exactly to their source consts.
        assert_eq!(groups[3]["repeat"], "PerHostDut");
        assert_eq!(groups[3]["title"], "dut-advanced");
        assert_eq!(
            groups[3]["fields"],
            serde_json::to_value(DUT_ADVANCED_GROUP_FIELDS).unwrap()
        );
        assert_eq!(groups[4]["repeat"], "Once");
        assert_eq!(groups[4]["title"], "advanced");
        assert_eq!(
            groups[4]["fields"],
            serde_json::to_value(ADVANCED_GROUP_FIELDS).unwrap()
        );
    }

    /// Migration pin: ADVANCED_FIELDS order/labels == the OLD ADVANCED_KEYS
    /// index-for-index, and `empty: AcceptDefault` == the OLD
    /// ENTER_ACCEPTS_DEFAULT membership exactly. The FORM_GROUPS advanced
    /// group mirrors ADVANCED_FIELDS (same key/label/empty/default).
    #[test]
    fn test_advanced_fields_migration_pinned() {
        const OLD_ADVANCED_KEYS: &[(&str, &str)] = &[
            ("serial.baudrate", "Serial baudrate (number)"),
            ("target.login_pass", "Target login password"),
            ("target.login_prompt", "Target login prompt regex"),
            ("uboot.interrupt_char", "U-Boot interrupt char"),
            ("uboot.interrupt_strategy", "U-Boot interrupt strategy"),
            ("uboot.pattern", "U-Boot interrupt pattern regex"),
            ("monitor.hang_timeout", "Hang timeout seconds"),
            ("monitor.max_archived_logs", "Max archived boot logs"),
            ("monitor.max_log_file_size", "Max log file size MB"),
            ("monitor.reference_log", "Reference boot log path"),
            (
                "monitor.learner_stage_threshold",
                "Learner stage threshold (0.0-1.0)",
            ),
            (
                "monitor.learner_crash_threshold",
                "Learner crash threshold (0.0-1.0)",
            ),
            (
                "monitor.crash_keywords",
                "Crash keywords (comma/newline separated)",
            ),
            ("flash.full_image_cmd", "Flash full-image command"),
            ("flash.kernel_image_cmd", "Flash kernel-image command"),
            ("flash.loader_bin", "Flash loader binary path"),
            ("control.dev_ctrl", "Device control backend"),
            ("loader", "Boot loader"),
        ];
        const OLD_ENTER_ACCEPTS_DEFAULT: &[&str] = &[
            "serial.baudrate",
            "uboot.interrupt_char",
            "uboot.interrupt_strategy",
            "monitor.hang_timeout",
            "monitor.max_archived_logs",
            "monitor.max_log_file_size",
            "monitor.reference_log",
            "monitor.learner_stage_threshold",
            "monitor.learner_crash_threshold",
            "monitor.crash_keywords",
            "flash.full_image_cmd",
            "flash.kernel_image_cmd",
            "flash.loader_bin",
            "loader",
        ];
        assert_eq!(ADVANCED_FIELDS.len(), OLD_ADVANCED_KEYS.len());
        for (i, spec) in ADVANCED_FIELDS.iter().enumerate() {
            assert_eq!((spec.key, spec.label), OLD_ADVANCED_KEYS[i], "index {i}");
            let accepts = spec.empty == EmptyMeans::AcceptDefault;
            assert_eq!(
                accepts,
                OLD_ENTER_ACCEPTS_DEFAULT.contains(&spec.key),
                "ENTER-accepts membership drift for {}",
                spec.key
            );
        }
        // SPLIT pin: FORM_GROUPS group 3 (dut-advanced,
        // PerHostDut) + group 4 (advanced, Once) partition ADVANCED_FIELDS
        // one-for-one, and the DUT-owned partition matches the
        // hand-written DUT_ADVANCED_KEYS list exactly.
        let dut_group = &FORM_GROUPS[3];
        assert_eq!(dut_group.repeat, Repeat::PerHostDut);
        assert_eq!(dut_group.title, "dut-advanced");
        let host_group = &FORM_GROUPS[4];
        assert_eq!(host_group.repeat, Repeat::Once);
        assert_eq!(host_group.title, "advanced");
        assert_eq!(
            dut_group.fields.len() + host_group.fields.len(),
            ADVANCED_FIELDS.len()
        );
        for (def, key) in dut_group.fields.iter().zip(DUT_ADVANCED_KEYS.iter()) {
            assert_eq!(def.key, FieldKeyFrag::DutAdvanced(key));
            let spec = ADVANCED_FIELDS.iter().find(|s| s.key == *key).unwrap();
            assert_eq!(def.label, spec.label);
            assert_eq!(def.empty, spec.empty);
            assert_eq!(
                def.kind,
                if matches!(
                    *key,
                    "control.dev_ctrl" | "loader" | "uboot.interrupt_strategy"
                ) {
                    FieldKind::Select
                } else {
                    FieldKind::Text
                },
                "kind for {key}"
            );
            if *key == "control.dev_ctrl" {
                // The backend is a REPLACEABLE power-control choice: the
                // built-in ch340 relay or none.
                assert_eq!(def.options, &["ch340-relay", "none"]);
            } else if *key == "loader" {
                assert_eq!(def.options, loader::OPTIONS);
            } else if *key == "uboot.interrupt_strategy" {
                assert_eq!(def.options, &["flood", "pattern"]);
            }
            let expected_default = if *key == "monitor.reference_log" {
                DefaultSrc::ReferenceLogComposed
            } else {
                DefaultSrc::AdvancedNatural
            };
            assert_eq!(def.default, expected_default, "{key}");
        }
        let host_keys: Vec<&str> = ADVANCED_FIELDS
            .iter()
            .map(|s| s.key)
            .filter(|k| !dut_owned_advanced(k))
            .collect();
        assert_eq!(host_keys.len(), host_group.fields.len());
        for (def, key) in host_group.fields.iter().zip(host_keys.iter()) {
            assert_eq!(def.key, FieldKeyFrag::Advanced(key));
            let spec = ADVANCED_FIELDS.iter().find(|s| s.key == *key).unwrap();
            assert_eq!(def.label, spec.label);
            assert_eq!(def.empty, spec.empty);
            assert_eq!(def.kind, FieldKind::Text, "kind for {key}");
            assert_eq!(def.default, DefaultSrc::AdvancedNatural, "{key}");
        }
    }

    // ── Runtime schema build ───────────────────────────────────────────

    #[test]
    fn test_build_form_schema_create_defaults() {
        let schema = build_form_schema(None, &InitOptions::default());
        assert_matches!(schema.target, SchemaTarget::Create);
        assert_eq!(schema.host_count_default, 1);
        assert_eq!(schema.host_count_max, 99);
        assert!(schema.taken_duts.is_empty());
        assert!(schema.host_defaults.is_empty());
        assert_eq!(
            schema.advanced_gate,
            AdvancedGate::Confirm { default: false }
        );
        assert!(schema.advanced_current.is_empty());
        // The optional host_name field materializes with an empty default
        // (never required).
        let fields = schema_fields(&schema);
        let host_name = fields
            .iter()
            .find(|f| f.key == FieldKey::HostName(0))
            .expect("host_name field exists on create");
        assert_eq!(host_name.default.as_deref(), Some(""));
        assert!(!host_name.required, "host_name is optional");
    }

    /// A config with GLOBAL fallback sections (the quick questions never
    /// touch them; advanced_current seeds the advanced defaults).
    const UPDATE_GLOBAL_FIXTURE: &str = r#"{
  "dev_hosts": [
    {
      "ip": "10.0.0.1",
      "user": "linaro",
      "pass": "",
      "duts": [
        {
          "dut_name": "board1",
          "serial": { "port": 41000, "baudrate": 460800 },
          "target": { "login_user": "root" },
          "relay": { "type": "ch340" }
        }
      ]
    }
  ],
  "serial": { "baudrate": 1500000, "rfc2217": false },
  "relay": { "reset_ch": 2 },
  "flash": { "loader_bin": "rockdev/loader.bin" }
}"#;

    #[test]
    fn test_build_form_schema_update_prefills_and_gates() {
        let existing = parse_jsonc_text(LOCATE_FIXTURE).unwrap();
        let schema = build_form_schema(Some(&existing), &quick_opts());
        assert_matches!(schema.target, SchemaTarget::Update(_));
        assert_eq!(schema.host_count_default, 1);
        assert_eq!(schema.taken_duts, ["board1", "board2"]);
        let h0 = &schema.host_defaults[0];
        assert_eq!(h0.ip.as_deref(), Some("10.0.0.1"));
        assert_eq!(h0.user.as_deref(), Some("linaro"));
        assert_eq!(h0.pass.as_deref(), Some(""));
        assert!(
            h0.host_name.is_none(),
            "LOCATE_FIXTURE has no host_name key → None prefill"
        );
        assert_eq!(h0.dut_count, 2);
        assert_eq!(h0.duts[0].serial_port.as_deref(), Some("41000"));
        assert_eq!(h0.duts[1].serial_port.as_deref(), Some("41001"));
        assert_eq!(h0.duts[0].login_user.as_deref(), Some("root"));
        assert_eq!(h0.duts[0].relay_type.as_deref(), Some("ch340"));
        // The relay gate prefills from the existing relay section presence:
        // board1 has one (YES + fields), board2 does not (NO).
        assert!(h0.duts[0].relay_gate, "board1 has a relay section");
        assert!(!h0.duts[1].relay_gate, "board2 has no relay section");
        assert_eq!(schema.advanced_gate, AdvancedGate::Skip);
        // advanced_current seeded from the GLOBAL sections only (a DUT's
        // serial.baudrate never seeds the global fallback).
        let with_globals = parse_jsonc_text(UPDATE_GLOBAL_FIXTURE).unwrap();
        let gschema = build_form_schema(Some(&with_globals), &InitOptions::default());
        assert_eq!(
            gschema
                .advanced_current
                .get("serial.baudrate")
                .map(String::as_str),
            Some("1500000")
        );
        assert_eq!(
            gschema
                .advanced_current
                .get("serial.rfc2217")
                .map(String::as_str),
            None
        );
        // relay.* keys are no longer advanced keys (per-DUT relay lives
        // under the gate) — advanced_current must NOT carry them even when
        // the file has a global relay section.
        assert!(!gschema.advanced_current.contains_key("relay.reset_ch"));
        assert_eq!(
            gschema
                .advanced_current
                .get("flash.loader_bin")
                .map(String::as_str),
            Some("rockdev/loader.bin")
        );
    }

    // ── Relay gate (per-DUT "use relay?" toggle) ──────────────────────

    /// Gate OFF in the plain walk: the toggle is asked FIRST and the relay
    /// questions are skipped entirely (no relay inputs consumed).
    #[test]
    fn test_plain_walk_relay_gate_no_skips_relay_questions() {
        let mut schema = build_form_schema(None, &quick_opts());
        let mut inputs: Vec<String> = QUICK_ANSWERS[..9].iter().map(|s| s.to_string()).collect();
        inputs.extend(dut_advanced_empties());
        let mut io = RecordingIO::new(&[]);
        io.inner.inputs = inputs.into();
        io.inner.confirms.push_back(false); // relay gate: NO
        let answers = walk_schema(&mut schema, &mut io).unwrap();
        assert!(!answers.hosts[0].duts[0].relay_gate);
        assert_eq!(answers.hosts[0].duts[0].relay_type, "");
        assert_eq!(
            answers.hosts[0].duts[0]
                .advanced
                .get("serial.baudrate")
                .map(String::as_str),
            Some("460800")
        );
        let prompts: Vec<String> = io.calls.iter().map(|(p, _)| p.clone()).collect();
        assert!(
            !prompts.iter().any(|p| p.contains("Relay")),
            "gate NO must skip every relay question: {prompts:?}"
        );
    }

    /// Gate ON in the plain walk: the relay sub-fields ARE asked in order
    /// (type select, then the zero-guarded numeric family).
    #[test]
    fn test_plain_walk_relay_gate_yes_asks_relay_fields() {
        let mut schema = build_form_schema(None, &quick_opts());
        let mut inputs: Vec<String> = QUICK_ANSWERS[..9].iter().map(|s| s.to_string()).collect();
        inputs.extend([
            "ch340-relay".into(), // relay type
            "41000".into(),       // relay.port
            "1".into(),           // relay.reset_ch
            "".into(),            // relay.download_ch (inherit)
            "".into(),            // relay.power_ch
            "".into(),            // relay.reset_time_ms
            "5000".into(),        // relay.power_off_time_ms
        ]);
        inputs.extend(dut_advanced_empties());
        let mut io = RecordingIO::new(&[]);
        io.inner.inputs = inputs.into();
        io.inner.confirms.push_back(true); // relay gate: YES
        let answers = walk_schema(&mut schema, &mut io).unwrap();
        let dut = &answers.hosts[0].duts[0];
        assert!(dut.relay_gate);
        assert_eq!(dut.relay_type, "ch340-relay");
        assert_eq!(dut.relay_port, "41000");
        assert_eq!(dut.relay_reset_ch, "1");
        assert_eq!(dut.relay_power_off_time_ms, "5000");
        let config = build_config_from_answers(&answers).unwrap();
        let relay = config.dev_hosts[0].duts[0].relay.as_ref().unwrap();
        assert_eq!(relay.relay_type.as_deref(), Some("ch340-relay"));
        assert_eq!(relay.port, Some(41000));
        assert_eq!(relay.reset_ch, Some(1));
        assert_eq!(relay.download_ch, None, "empty = inherit");
        assert_eq!(relay.power_off_time_ms, Some(5000));
    }

    /// End-to-end CREATE: gate NO → the DUT's relay section is OMITTED;
    /// gate YES → the relay fields materialize (zero-guarded family).
    #[test]
    fn test_wizard_create_relay_gate_end_state() {
        let _guard = lock_env();
        let tmp = TempDir::new().unwrap();
        unsafe {
            std::env::remove_var("TARGET_CONF");
        }
        // Gate NO: relay omitted.
        let mut inputs: Vec<String> = QUICK_ANSWERS[..9].iter().map(|s| s.to_string()).collect();
        inputs.extend(dut_advanced_empties());
        let mut io = CannedIO::new(&[]);
        io.inputs = inputs.into();
        io.confirms.push_back(false);
        run_init(&mut io, &quick_opts(), tmp.path()).unwrap();
        let parsed =
            parse_jsonc_text(&std::fs::read_to_string(tmp.path().join(CONFIG_FILE_NAME)).unwrap())
                .unwrap();
        assert!(
            parsed.dev_hosts[0].duts[0].relay.is_none(),
            "gate NO must omit the relay section"
        );

        // Gate YES with typed fields: the section materializes.
        let mut inputs: Vec<String> = QUICK_ANSWERS[..9].iter().map(|s| s.to_string()).collect();
        inputs.extend([
            "ch340".into(), // type
            "".into(),      // port (inherit)
            "2".into(),     // reset_ch
            "".into(),      // download_ch
            "".into(),      // power_ch
            "100".into(),   // reset_time_ms
            "1000".into(),  // power_off_time_ms (clamped to 3000)
        ]);
        inputs.extend(dut_advanced_empties());
        let mut io = CannedIO::new(&[]);
        io.inputs = inputs.into();
        io.confirms.push_back(true);
        let outcome = run_init(&mut io, &quick_opts(), tmp.path()).unwrap();
        let parsed =
            parse_jsonc_text(&std::fs::read_to_string(&outcome.config_path).unwrap()).unwrap();
        let relay = parsed.dev_hosts[0].duts[0].relay.as_ref().unwrap();
        assert_eq!(relay.reset_ch, Some(2));
        assert_eq!(relay.port, None);
        assert_eq!(relay.reset_time_ms, Some(100));
        assert_eq!(
            relay.power_off_time_ms,
            Some(3000),
            "power_off_time_ms clamped to >= 3000"
        );
    }

    /// resolve_answers: gate OFF drops any relay buffers (relay omitted);
    /// gate ON records them with per-field validation.
    #[test]
    fn test_resolve_relay_gate_tri_state() {
        let schema = build_form_schema(None, &quick_opts());
        // Gate typed "no": relay fields ignored even if typed.
        let mut values = values_of(&[
            (FieldKey::HostIp(0), "10.0.0.1"),
            (FieldKey::DutPort(0, 0), "41002"),
            (FieldKey::DutRelayGate(0, 0), "no"),
            (FieldKey::DutRelayType(0, 0), "ch340-relay"),
        ]);
        let answers =
            resolve_answers(&schema, &values, false, NamePolicy::ErrorOnCollision).unwrap();
        let dut = &answers.hosts[0].duts[0];
        assert!(!dut.relay_gate);
        assert_eq!(dut.relay_type, "", "gate off drops relay buffers");
        // Gate ON: fields recorded; invalid channel errors per field.
        values.insert(FieldKey::DutRelayGate(0, 0), "yes".into());
        values.insert(FieldKey::DutRelayReset(0, 0), "2".into());
        let answers =
            resolve_answers(&schema, &values, false, NamePolicy::ErrorOnCollision).unwrap();
        let dut = &answers.hosts[0].duts[0];
        assert!(dut.relay_gate);
        assert_eq!(dut.relay_type, "ch340-relay");
        assert_eq!(dut.relay_reset_ch, "2");
        values.insert(FieldKey::DutRelayReset(0, 0), "abc".into());
        let errs =
            resolve_answers(&schema, &values, false, NamePolicy::ErrorOnCollision).unwrap_err();
        assert!(
            errs.iter().any(|(k, m)| {
                k == &FieldKey::DutRelayReset(0, 0)
                    && m.contains("relay.reset_ch: invalid channel: abc")
            }),
            "{errs:?}"
        );
    }

    // ── Zero dev hosts => zero DUTs ────────────────────────────────────

    /// CREATE with host count 0: the DUT section never renders (no DUT
    /// questions) and the written config carries an empty dev_hosts array.
    #[test]
    fn test_wizard_create_zero_hosts_writes_empty_dev_hosts() {
        let _guard = lock_env();
        let tmp = TempDir::new().unwrap();
        unsafe {
            std::env::remove_var("TARGET_CONF");
        }
        let mut io = CannedIO::new(&["0"]); // host count: zero
        let outcome = run_init(&mut io, &quick_opts(), tmp.path()).unwrap();
        assert!(outcome.config_created);
        let text = std::fs::read_to_string(&outcome.config_path).unwrap();
        let parsed = parse_jsonc_text(&text).unwrap();
        assert!(parsed.dev_hosts.is_empty(), "zero hosts => zero DUTs");
        assert!(text.contains("\"dev_hosts\""), "{text}");
    }

    /// UPDATE reducing the host count to 0: every host AND DUT is dropped
    /// from the written config (no DUT data survives).
    #[test]
    fn test_wizard_update_zero_hosts_clears_dev_hosts() {
        let _guard = lock_env();
        let tmp = TempDir::new().unwrap();
        unsafe {
            std::env::remove_var("TARGET_CONF");
        }
        let path = tmp.path().join(CONFIG_FILE_NAME);
        std::fs::write(&path, LOCATE_FIXTURE).unwrap();
        let mut io = CannedIO::new(&["0"]); // host count: zero
        let outcome = run_init(&mut io, &quick_opts(), tmp.path()).unwrap();
        assert!(!outcome.config_created);
        let text = std::fs::read_to_string(&path).unwrap();
        let parsed = parse_jsonc_text(&text).unwrap_or_else(|e| panic!("{e}\n{text}"));
        assert!(parsed.dev_hosts.is_empty(), "{text}");
        assert!(
            outcome.changes.iter().any(|c| c.contains("zero dev hosts")),
            "{:?}",
            outcome.changes
        );
        // The written config validates (the explicit empty dev_hosts is a
        // legitimate end state).
        assert!(
            outcome
                .changes
                .iter()
                .any(|c| c.contains("dev_hosts cleared")),
            "{:?}",
            outcome.changes
        );
    }

    #[test]
    fn test_build_form_schema_gate_matrix() {
        let existing = parse_jsonc_text(LOCATE_FIXTURE).unwrap();
        let advanced = build_form_schema(
            Some(&existing),
            &InitOptions {
                advanced: true,
                ..Default::default()
            },
        );
        assert_eq!(advanced.advanced_gate, AdvancedGate::Forced);
        let quick = build_form_schema(Some(&existing), &quick_opts());
        assert_eq!(quick.advanced_gate, AdvancedGate::Skip);
        let normal = build_form_schema(Some(&existing), &InitOptions::default());
        assert_eq!(
            normal.advanced_gate,
            AdvancedGate::Confirm { default: false }
        );
    }

    #[test]
    fn test_materialize_defaults_runtime_values_win() {
        let existing = parse_jsonc_text(LOCATE_FIXTURE).unwrap();
        let schema = build_form_schema(Some(&existing), &InitOptions::default());
        let fields = schema_fields(&schema);
        let by_key = |k: &FieldKey| fields.iter().find(|f| &f.key == k).unwrap().clone();
        assert_eq!(by_key(&FieldKey::HostCount).default.as_deref(), Some("1"));
        assert_eq!(
            by_key(&FieldKey::HostIp(0)).default.as_deref(),
            Some("10.0.0.1")
        );
        assert_eq!(by_key(&FieldKey::DutCount(0)).default.as_deref(), Some("2"));
        assert_eq!(
            by_key(&FieldKey::DutPort(0, 0)).default.as_deref(),
            Some("41000")
        );
        assert_eq!(
            by_key(&FieldKey::DutPort(0, 1)).default.as_deref(),
            Some("41001")
        );
        assert_eq!(
            by_key(&FieldKey::DutLoginUser(0, 0)).default.as_deref(),
            Some("root")
        );
        assert_eq!(
            by_key(&FieldKey::DutRelayType(0, 0)).default.as_deref(),
            Some("ch340")
        );
        // Relay picker options come from the compiled backends.
        let relay = by_key(&FieldKey::DutRelayType(0, 0));
        assert_eq!(relay.kind, FieldKind::Select);
        assert_eq!(
            relay.options,
            supported_relay_types()
                .into_iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        );
        // DUT-owned advanced defaults: the DUT's OWN override wins
        // (board1's serial.baudrate is 1500000 in the fixture), and
        // reference_log composes from THAT DUT's name.
        assert_eq!(
            by_key(&FieldKey::DutAdvanced(0, 0, "serial.baudrate"))
                .default
                .as_deref(),
            Some("1500000"),
            "board1's own override"
        );
        assert_eq!(
            by_key(&FieldKey::DutAdvanced(0, 1, "serial.baudrate"))
                .default
                .as_deref(),
            Some("460800"),
            "board2 has no override -> advanced_default (hint only)"
        );
        assert_eq!(
            by_key(&FieldKey::DutAdvanced(0, 0, "monitor.reference_log"))
                .default
                .as_deref(),
            Some(".dut-serial/board1/reference-boot.log")
        );
        assert_eq!(
            by_key(&FieldKey::DutAdvanced(0, 1, "monitor.reference_log"))
                .default
                .as_deref(),
            Some(".dut-serial/board2/reference-boot.log")
        );
        // The host-level set keeps the global current default.
        assert_eq!(
            by_key(&FieldKey::Advanced("monitor.hang_timeout"))
                .default
                .as_deref(),
            Some("60")
        );
        // host_name runtime override: an explicit host_name on the
        // existing host becomes the materialized default.
        let named = parse_jsonc_text(
            r#"{ "dev_hosts": [ { "ip": "10.0.0.1", "host_name": "lab-pc", "user": "u", "pass": "", "duts": [ { "dut_name": "board1", "serial": { "port": 41000 } } ] } ] }"#,
        )
        .unwrap();
        let nschema = build_form_schema(Some(&named), &InitOptions::default());
        let nfields = schema_fields(&nschema);
        let nname = nfields
            .iter()
            .find(|f| f.key == FieldKey::HostName(0))
            .expect("host_name field exists on update");
        assert_eq!(nname.default.as_deref(), Some("lab-pc"));
        // CREATE: composed reference_log falls back to dut1.
        let create = build_form_schema(None, &InitOptions::default());
        let create_fields = schema_fields(&create);
        let ref_log = create_fields
            .iter()
            .find(|f| f.key == FieldKey::DutAdvanced(0, 0, "monitor.reference_log"))
            .unwrap();
        assert_eq!(
            ref_log.default.as_deref(),
            Some(".dut-serial/dut1/reference-boot.log")
        );
    }

    // ── resolve_answers (form submit; the fixed ask_required contract) ─

    fn values_of(pairs: &[(FieldKey, &str)]) -> BTreeMap<FieldKey, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.clone(), v.to_string()))
            .collect()
    }

    #[test]
    fn test_resolve_required_empty_default_errors() {
        let schema = build_form_schema(None, &quick_opts());
        // CREATE: ip default is "" → an empty buffer errors, and an
        // ABSENT buffer errors identically (parity with ask_required: the
        // shown default is empty, so nothing is silently accepted).
        let errs = resolve_answers(
            &schema,
            &values_of(&[
                (FieldKey::HostCount, "1"),
                (FieldKey::DutCount(0), "1"),
                (FieldKey::DutPort(0, 0), "41002"),
            ]),
            false,
            NamePolicy::ErrorOnCollision,
        )
        .unwrap_err();
        assert!(
            errs.iter()
                .any(|(k, m)| k == &FieldKey::HostIp(0) && m == "value is required"),
            "{errs:?}"
        );
        let errs = resolve_answers(
            &schema,
            &values_of(&[
                (FieldKey::HostCount, "1"),
                (FieldKey::HostIp(0), ""), // empty buffer, empty default
                (FieldKey::DutCount(0), "1"),
                (FieldKey::DutPort(0, 0), "41002"),
            ]),
            false,
            NamePolicy::ErrorOnCollision,
        )
        .unwrap_err();
        assert!(
            errs.iter()
                .any(|(k, m)| k == &FieldKey::HostIp(0) && m == "value is required"),
            "{errs:?}"
        );
    }

    #[test]
    fn test_resolve_required_empty_buffer_accepts_shown_default_on_update() {
        // UPDATE: prefilled non-empty default + empty buffer → silent accept.
        let existing = parse_jsonc_text(LOCATE_FIXTURE).unwrap();
        let schema = build_form_schema(Some(&existing), &quick_opts());
        let mut values = values_of(&[
            (FieldKey::HostCount, "1"),
            (FieldKey::DutCount(0), "2"),
            (FieldKey::DutPort(0, 0), "41000"),
            (FieldKey::DutPort(0, 1), "41001"),
        ]);
        values.insert(FieldKey::HostIp(0), String::new()); // empty buffer
        let answers =
            resolve_answers(&schema, &values, false, NamePolicy::ErrorOnCollision).unwrap();
        assert_eq!(answers.hosts[0].ip, "10.0.0.1");
        assert_eq!(answers.hosts[0].duts.len(), 2);
    }

    #[test]
    fn test_resolve_port_zero_errors_and_device_path_passes() {
        let schema = build_form_schema(None, &quick_opts());
        let errs = resolve_answers(
            &schema,
            &values_of(&[
                (FieldKey::HostCount, "1"),
                (FieldKey::HostIp(0), "10.0.0.1"),
                (FieldKey::DutCount(0), "1"),
                (FieldKey::DutPort(0, 0), "0"),
            ]),
            false,
            NamePolicy::ErrorOnCollision,
        )
        .unwrap_err();
        assert!(
            errs.iter()
                .any(|(k, m)| k == &FieldKey::DutPort(0, 0) && m == "serial.port cannot be 0"),
            "{errs:?}"
        );
        let answers = resolve_answers(
            &schema,
            &values_of(&[
                (FieldKey::HostCount, "1"),
                (FieldKey::HostIp(0), "10.0.0.1"),
                (FieldKey::DutCount(0), "1"),
                (FieldKey::DutPort(0, 0), "/dev/ttyUSB0"),
            ]),
            false,
            NamePolicy::ErrorOnCollision,
        )
        .unwrap();
        assert_eq!(answers.hosts[0].duts[0].serial_port, "/dev/ttyUSB0");
    }

    #[test]
    fn test_resolve_count_empty_default_invalid_errors() {
        let schema = build_form_schema(None, &quick_opts());
        // Empty count buffers → defaults (1).
        let answers = resolve_answers(
            &schema,
            &values_of(&[
                (FieldKey::HostIp(0), "10.0.0.1"),
                (FieldKey::DutPort(0, 0), "41002"),
            ]),
            false,
            NamePolicy::ErrorOnCollision,
        )
        .unwrap();
        assert_eq!(answers.hosts.len(), 1);
        assert_eq!(answers.hosts[0].duts.len(), 1);
        // 0 is a VALID count now: zero dev hosts → zero DUTs (the DUT
        // section disappears; the host loop never runs).
        let answers = resolve_answers(
            &schema,
            &values_of(&[
                (FieldKey::HostCount, "0"),
                (FieldKey::HostIp(0), "10.0.0.1"),
                (FieldKey::DutCount(0), "1"),
                (FieldKey::DutPort(0, 0), "41002"),
            ]),
            false,
            NamePolicy::ErrorOnCollision,
        )
        .unwrap();
        assert!(answers.hosts.is_empty(), "{answers:?}");
        // Invalid count → the exact ask_count error string.
        let errs = resolve_answers(
            &schema,
            &values_of(&[
                (FieldKey::HostCount, "x"),
                (FieldKey::DutPort(0, 0), "41002"),
            ]),
            false,
            NamePolicy::ErrorOnCollision,
        )
        .unwrap_err();
        assert!(
            errs.iter().any(|(k, m)| {
                k == &FieldKey::HostCount && m == "'x' is not a non-negative integer"
            }),
            "{errs:?}"
        );
        // DUT count 0 → a host with NO DUTs (valid in the grid model).
        let answers = resolve_answers(
            &schema,
            &values_of(&[
                (FieldKey::HostIp(0), "10.0.0.1"),
                (FieldKey::DutCount(0), "0"),
            ]),
            false,
            NamePolicy::ErrorOnCollision,
        )
        .unwrap();
        assert_eq!(answers.hosts.len(), 1);
        assert!(answers.hosts[0].duts.is_empty());
    }

    #[test]
    fn test_resolve_dut_name_empty_autonames_and_collision_policies() {
        let schema = build_form_schema(None, &quick_opts());
        // Empty DUT name → auto-name + announcement.
        let answers = resolve_answers(
            &schema,
            &values_of(&[
                (FieldKey::HostIp(0), "10.0.0.1"),
                (FieldKey::DutPort(0, 0), "41002"),
            ]),
            false,
            NamePolicy::ErrorOnCollision,
        )
        .unwrap();
        assert_eq!(answers.hosts[0].duts[0].dut_name, "dut1");
        // Collision pair: ErrorOnCollision errors BOTH DUT fields.
        let errs = resolve_answers(
            &schema,
            &values_of(&[
                (FieldKey::HostCount, "1"),
                (FieldKey::HostIp(0), "10.0.0.1"),
                (FieldKey::DutCount(0), "2"),
                (FieldKey::DutName(0, 0), "dup"),
                (FieldKey::DutName(0, 1), "dup"),
                (FieldKey::DutPort(0, 0), "41002"),
                (FieldKey::DutPort(0, 1), "41003"),
            ]),
            false,
            NamePolicy::ErrorOnCollision,
        )
        .unwrap_err();
        let collisions: Vec<&(FieldKey, String)> = errs
            .iter()
            .filter(|(_, m)| m.contains("already taken"))
            .collect();
        assert_eq!(collisions.len(), 2, "{errs:?}");
        // AutoAdvance announces instead of erroring.
        let answers = resolve_answers(
            &schema,
            &values_of(&[
                (FieldKey::HostCount, "1"),
                (FieldKey::HostIp(0), "10.0.0.1"),
                (FieldKey::DutCount(0), "2"),
                (FieldKey::DutName(0, 0), "dup"),
                (FieldKey::DutName(0, 1), "dup"),
                (FieldKey::DutPort(0, 0), "41002"),
                (FieldKey::DutPort(0, 1), "41003"),
            ]),
            false,
            NamePolicy::AutoAdvance,
        )
        .unwrap();
        assert!(
            answers
                .announcements
                .iter()
                .any(|a| a.contains("already taken — using")),
            "{:?}",
            answers.announcements
        );
    }

    #[test]
    fn test_resolve_advanced_tri_state_and_gate() {
        let existing = parse_jsonc_text(LOCATE_FIXTURE).unwrap();
        let schema = build_form_schema(Some(&existing), &InitOptions::default());
        let base = |extra: Vec<(FieldKey, &str)>| {
            let mut v = values_of(&[
                (FieldKey::HostCount, "1"),
                (FieldKey::DutCount(0), "2"),
                (FieldKey::DutName(0, 0), "board1"),
                (FieldKey::DutName(0, 1), "board2"),
                (FieldKey::DutPort(0, 0), "41000"),
                (FieldKey::DutPort(0, 1), "41001"),
            ]);
            for (k, val) in extra {
                v.insert(k, val.to_string());
            }
            v
        };
        // GATE OFF: typed DUT-owned values are KEPT (the gate
        // fix — the gate only governs the HOST-level set).
        let mut values = base(vec![]);
        values.insert(
            FieldKey::DutAdvanced(0, 0, "flash.loader_bin"),
            "x.bin".into(),
        );
        let answers =
            resolve_answers(&schema, &values, false, NamePolicy::ErrorOnCollision).unwrap();
        assert!(
            answers.advanced.is_empty(),
            "gate off → no host-level answers"
        );
        assert_eq!(
            answers.hosts[0].duts[0]
                .advanced
                .get("flash.loader_bin")
                .map(String::as_str),
            Some("x.bin")
        );
        // GATE ON: host-level AcceptDefault empty buffer → natural default
        // (max_log_file_size is absent from the fixture); typed recorded.
        let mut values = base(vec![(FieldKey::Advanced("monitor.hang_timeout"), "30")]);
        values.insert(
            FieldKey::Advanced("monitor.max_log_file_size"),
            String::new(),
        );
        let answers =
            resolve_answers(&schema, &values, true, NamePolicy::ErrorOnCollision).unwrap();
        assert_eq!(
            answers
                .advanced
                .get("monitor.max_log_file_size")
                .map(String::as_str),
            Some("100")
        );
        assert_eq!(
            answers
                .advanced
                .get("monitor.hang_timeout")
                .map(String::as_str),
            Some("30")
        );
        // DUT-owned: typed recorded per DUT; cleared SkipInherit → the ""
        // unset marker; untouched buffers → absent (hints informational).
        let mut values = base(vec![]);
        values.insert(
            FieldKey::DutAdvanced(0, 0, "target.login_pass"),
            "pw".into(),
        );
        values.insert(
            FieldKey::DutAdvanced(0, 1, "target.login_pass"),
            String::new(),
        );
        let answers =
            resolve_answers(&schema, &values, true, NamePolicy::ErrorOnCollision).unwrap();
        assert_eq!(
            answers.hosts[0].duts[0]
                .advanced
                .get("target.login_pass")
                .map(String::as_str),
            Some("pw")
        );
        assert_eq!(
            answers.hosts[0].duts[1]
                .advanced
                .get("target.login_pass")
                .map(String::as_str),
            Some(""),
            "cleared SkipInherit → explicit unset marker"
        );
        // Invalid typed host-level value → the exact error string.
        let mut values = base(vec![]);
        values.insert(FieldKey::Advanced("monitor.hang_timeout"), "abc".into());
        let errs =
            resolve_answers(&schema, &values, true, NamePolicy::ErrorOnCollision).unwrap_err();
        assert!(
            errs.iter().any(|(k, m)| {
                k == &FieldKey::Advanced("monitor.hang_timeout")
                    && m.contains("monitor.hang_timeout: invalid seconds: abc")
            }),
            "{errs:?}"
        );
        // DUT-owned invalid value → per-DUT-keyed error.
        let mut values = base(vec![]);
        values.insert(
            FieldKey::DutAdvanced(0, 1, "monitor.max_archived_logs"),
            "abc".into(),
        );
        let errs =
            resolve_answers(&schema, &values, true, NamePolicy::ErrorOnCollision).unwrap_err();
        assert!(
            errs.iter().any(|(k, m)| {
                k == &FieldKey::DutAdvanced(0, 1, "monitor.max_archived_logs")
                    && m.contains("monitor.max_archived_logs: invalid count: abc")
            }),
            "{errs:?}"
        );
        // Typed per-DUT reference_log recorded for THAT DUT only.
        let mut values = base(vec![]);
        values.insert(
            FieldKey::DutAdvanced(0, 1, "monitor.reference_log"),
            "/tmp/typed.log".into(),
        );
        let answers =
            resolve_answers(&schema, &values, true, NamePolicy::ErrorOnCollision).unwrap();
        assert_eq!(
            answers.hosts[0].duts[1]
                .advanced
                .get("monitor.reference_log")
                .map(String::as_str),
            Some("/tmp/typed.log")
        );
        assert!(
            !answers.hosts[0].duts[0]
                .advanced
                .contains_key("monitor.reference_log")
        );
        // Untouched buffers + gate on → nothing per-DUT (the composed
        // natural hint is informational and never recorded).
        let answers =
            resolve_answers(&schema, &base(vec![]), true, NamePolicy::ErrorOnCollision).unwrap();
        for dut in &answers.hosts[0].duts {
            assert_eq!(
                dut.advanced.get("serial.baudrate").map(String::as_str),
                Some("460800")
            );
        }
    }

    #[test]
    fn test_resolve_reference_log_composed_from_live_dut_name() {
        // CREATE: the composed reference_log HINT uses the live DUT name;
        // it is informational — a cleared buffer records NOTHING (the
        // composed default is never written for an untouched field).
        let schema = build_form_schema(None, &InitOptions::default());
        assert_eq!(
            dut_advanced_default(&schema, 0, 0, "monitor.reference_log", Some("boardX")),
            ".dut-serial/boardX/reference-boot.log"
        );
        let mut values = values_of(&[
            (FieldKey::HostIp(0), "10.0.0.1"),
            (FieldKey::DutName(0, 0), "boardX"),
            (FieldKey::DutPort(0, 0), "41002"),
        ]);
        values.insert(
            FieldKey::DutAdvanced(0, 0, "monitor.reference_log"),
            String::new(),
        );
        let answers =
            resolve_answers(&schema, &values, true, NamePolicy::ErrorOnCollision).unwrap();
        assert!(
            !answers.hosts[0].duts[0]
                .advanced
                .contains_key("monitor.reference_log"),
            "composed natural default is a hint, never an answer"
        );
        // Typed value → recorded for THAT DUT.
        values.insert(
            FieldKey::DutAdvanced(0, 0, "monitor.reference_log"),
            "/tmp/x.log".into(),
        );
        let answers =
            resolve_answers(&schema, &values, true, NamePolicy::ErrorOnCollision).unwrap();
        assert_eq!(
            answers.hosts[0].duts[0]
                .advanced
                .get("monitor.reference_log")
                .map(String::as_str),
            Some("/tmp/x.log")
        );
    }

    /// Cross-front parity: walk_schema answers == resolve_answers answers
    /// for the same raw inputs (CREATE, quick gate).
    #[test]
    fn test_resolve_matches_walk_for_same_inputs() {
        let schema = build_form_schema(None, &quick_opts());
        let mut walked_schema = schema.clone();
        // The walk now asks the DUT-owned advanced set in every mode —
        // bare Enter everywhere records nothing (hints are informational).
        let mut inputs: Vec<String> = QUICK_ANSWERS.iter().map(|s| s.to_string()).collect();
        inputs.extend(dut_advanced_empties());
        let mut io = CannedIO::new(&[]);
        io.inputs = inputs.into();
        let walked = walk_schema(&mut walked_schema, &mut io).unwrap();
        let values = values_of(&[
            (FieldKey::HostCount, "1"),
            (FieldKey::HostIp(0), "10.0.0.1"),
            (FieldKey::HostUser(0), "linaro"),
            (FieldKey::HostPass(0), ""),
            (FieldKey::HostName(0), ""),
            (FieldKey::DutCount(0), "1"),
            (FieldKey::DutName(0, 0), ""),
            (FieldKey::DutPort(0, 0), "41002"),
            (FieldKey::DutLoginUser(0, 0), ""),
            (FieldKey::DutRelayType(0, 0), ""),
        ]);
        let resolved =
            resolve_answers(&schema, &values, false, NamePolicy::ErrorOnCollision).unwrap();
        // hosts + advanced are identical; announcements deliberately
        // differ: the plain walk keeps today's silent keep-the-shown-auto
        // semantics (the shown default IS the auto name), while the form
        // announces the dut auto-naming (its buffers carry no defaults).
        assert_eq!(walked.hosts, resolved.hosts);
        assert_eq!(walked.advanced, resolved.advanced);
        assert!(resolved.announcements.len() == 1); // form announces dut1
        assert!(walked.announcements.is_empty()); // plain stays silent
    }

    // ── Per-DUT advanced (migration) ────────────────────────

    /// One DUT answers struct with the defaults the tests use everywhere.
    fn dut_answers(name: &str, port: &str) -> DutAnswers {
        DutAnswers {
            dut_name: name.into(),
            serial_port: port.into(),
            login_user: "root".into(),
            relay_gate: false,
            relay_type: String::new(),
            relay_port: String::new(),
            relay_reset_ch: String::new(),
            relay_download_ch: String::new(),
            relay_power_ch: String::new(),
            relay_reset_time_ms: String::new(),
            relay_power_off_time_ms: String::new(),
            advanced: HashMap::new(),
        }
    }

    /// 17 bare Enters (one per DUT-owned advanced question).
    // One bare Enter per DUT-owned advanced question the plain flow asks
    // (DUT_ADVANCED_KEYS minus uboot.pattern — kept in sync with the
    // ask loop; was 17 before the flash download-overlay rework).
    const DUT_ADVANCED_EMPTIES: [&str; 13] = [""; 13];

    // ── Occupancy gate ────────────────────────────────────

    /// RAII live endpoint lock (our own PID, always alive) — removed on
    /// drop so a panicking test can never leak a lock that breaks later
    /// tests (the suite runs single-threaded against shared /tmp locks).
    struct EndpointHold {
        path: std::path::PathBuf,
    }

    impl Drop for EndpointHold {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    fn hold_endpoint(lock_dir: &str, host: &str, port: &str) -> EndpointHold {
        std::fs::create_dir_all(lock_dir).unwrap();
        let path = crate::lock_manager::endpoint_lock_path(host, port, lock_dir);
        let since = chrono::Utc::now().to_rfc3339();
        let content = format!("{}\n{host}:{port}\nhost\n{since}", std::process::id());
        std::fs::write(&path, content).unwrap();
        EndpointHold { path }
    }

    fn one_dut_answers(ip: &str, port: &str) -> WizardAnswers {
        let mut answers = WizardAnswers {
            hosts: vec![HostAnswers {
                ip: ip.into(),
                user: "linaro".into(),
                pass: String::new(),
                host_name: String::new(),
                duts: vec![dut_answers("dut1", port)],
            }],
            advanced: HashMap::new(),
            announcements: Vec::new(),
        };
        // fill_defaults records the serial.baudrate natural default for
        // every DUT (validation requires it) — mirror that here.
        for dut in &mut answers.hosts[0].duts {
            dut.advanced
                .entry("serial.baudrate".into())
                .or_insert_with(|| advanced_default("serial.baudrate"));
        }
        answers
    }

    /// Only the SAME host:port is a conflict — a different DUT on a
    /// different port has its own lock and never blocks.
    #[test]
    fn test_endpoint_conflicts_only_same_host_port() {
        let dir = std::env::temp_dir().join(format!(
            "init-busy-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let dir_str = dir.to_str().unwrap();
        let hold = hold_endpoint(dir_str, "10.9.9.9", "41001");

        let conflicts = endpoint_conflicts(&one_dut_answers("10.9.9.9", "41001"), dir_str);
        assert_eq!(conflicts.len(), 1, "{conflicts:?}");
        assert_eq!(conflicts[0].host, "10.9.9.9");
        assert_eq!(conflicts[0].port, "41001");
        assert_eq!(conflicts[0].pid, std::process::id());
        assert!(
            !conflicts[0].since.is_empty(),
            "the holder's lock 'since' line is surfaced"
        );
        assert!(
            conflicts[0].owner_project.is_some(),
            "the holder's /proc/<pid>/cwd is surfaced"
        );
        assert!(
            endpoint_conflict_line(&conflicts[0]).contains("PID"),
            "{}",
            endpoint_conflict_line(&conflicts[0])
        );

        // Same host, different port → no conflict.
        assert!(endpoint_conflicts(&one_dut_answers("10.9.9.9", "41002"), dir_str).is_empty());
        // Different host, same port → no conflict.
        assert!(endpoint_conflicts(&one_dut_answers("10.9.9.8", "41001"), dir_str).is_empty());
        // Same endpoint listed twice → deduped.
        let mut dup = one_dut_answers("10.9.9.9", "41001");
        dup.hosts[0].duts.push(dut_answers("dut2", "41001"));
        assert_eq!(endpoint_conflicts(&dup, dir_str).len(), 1);

        drop(hold);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The gate's project boundary: THIS project's own MCP holding the
    /// endpoint bypasses the interactive question entirely (info note, no
    /// abort), while a FOREIGN holder still asks — canned Abort aborts with
    /// the `aborted:` reason, canned Continue proceeds.
    #[test]
    fn occupancy_gate_bypasses_own_project_and_asks_for_foreign() {
        let dir = std::env::temp_dir().join(format!(
            "init-gate-own-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let dir_str = dir.to_str().unwrap();
        // ── THIS project's own MCP is not a blocker ────────────────────────
        // The lock's owner_project is the test process's cwd (hold_endpoint
        // stamps our own pid); passing the SAME dir as the init project must
        // bypass the interactive question entirely (no busy_endpoints call,
        // no abort even when the canned decision would be Abort) and emit an
        // informational note instead.
        let own_project = std::env::current_dir().unwrap();
        let mut io = CannedIO::new(&[]);
        io.busy.push_back(BusyDecision::Abort); // must never be consulted
        let hold_own = hold_endpoint(dir_str, "10.9.9.9", "41001");
        occupancy_gate(
            &mut io,
            &InitOptions::default(),
            &one_dut_answers("10.9.9.9", "41001"),
            dir_str,
            &own_project,
        )
        .expect("own-project holder must not abort init");
        assert!(
            io.busy_calls.is_empty(),
            "own-project holder never prompts: {:?}",
            io.busy_calls
        );
        assert!(
            io.notes
                .iter()
                .any(|n| n.contains("this project's own MCP holds the endpoint")),
            "an informational note names the own-project holder: {:?}",
            io.notes
        );
        drop(hold_own);

        // A FOREIGN project's holder still goes through the interactive
        // question: canned Abort aborts with the reason; canned Continue
        // proceeds silently.
        let foreign_project = dir.join("some-other-project");
        std::fs::create_dir_all(&foreign_project).unwrap();
        let hold_foreign = hold_endpoint(dir_str, "10.9.9.9", "41001");
        let mut io_abort = CannedIO::new(&[]);
        io_abort.busy.push_back(BusyDecision::Abort);
        let err = occupancy_gate(
            &mut io_abort,
            &InitOptions::default(),
            &one_dut_answers("10.9.9.9", "41001"),
            dir_str,
            &foreign_project,
        )
        .unwrap_err();
        assert!(err.starts_with("aborted: "), "{err}");
        assert_eq!(io_abort.busy_calls.len(), 1, "the user is asked once");

        let mut io_continue = CannedIO::new(&[]);
        io_continue.busy.push_back(BusyDecision::Continue);
        occupancy_gate(
            &mut io_continue,
            &InitOptions::default(),
            &one_dut_answers("10.9.9.9", "41001"),
            dir_str,
            &foreign_project,
        )
        .expect("Continue writes the config");
        drop(hold_foreign);
    }

    /// A held endpoint with the user choosing ABORT: finish_init errors,
    /// zero bytes are written, and the holder's lock is untouched.
    #[test]
    fn test_finish_init_aborts_on_busy_endpoint_writes_nothing() {
        let _guard = lock_env();
        let tmp = TempDir::new().unwrap();
        unsafe {
            std::env::set_var(
                "TARGET_CONF",
                tmp.path().join(".target.jsonc").to_str().unwrap(),
            );
        }
        let port = "47111";
        let hold = hold_endpoint(
            crate::lock_manager::default_lock_dir().as_str(),
            "10.0.0.1",
            port,
        );
        let opts = quick_opts();
        let prepared = prepare_init(tmp.path(), &opts).unwrap();
        let mut io = CannedIO::new(&[]).with_busy(&[BusyDecision::Abort]);
        let err = finish_init(
            &mut io,
            &opts,
            &prepared,
            &one_dut_answers("10.0.0.1", port),
        )
        .unwrap_err();
        assert!(err.contains("aborted"), "{err}");
        assert!(err.contains("10.0.0.1"), "{err}");
        assert!(err.contains(port), "{err}");
        assert!(
            !tmp.path().join(".target.jsonc").exists(),
            "zero bytes written"
        );
        assert_eq!(io.busy_calls.len(), 1, "the user was asked exactly once");
        assert_eq!(io.busy_calls[0][0].pid, std::process::id());
        assert!(hold.path.exists(), "the holder's lock is NEVER touched");
        // A second run with the lock released proceeds normally.
        drop(hold);
        let mut io2 = CannedIO::new(&[]);
        let outcome = finish_init(
            &mut io2,
            &opts,
            &prepared,
            &one_dut_answers("10.0.0.1", port),
        )
        .unwrap();
        assert!(outcome.config_created);
        assert!(io2.busy_calls.is_empty());
        unsafe {
            std::env::remove_var("TARGET_CONF");
        }
    }

    /// Continue writes the config and still never touches the holder.
    #[test]
    fn test_finish_init_continue_writes_keeps_holder_lock() {
        let _guard = lock_env();
        let tmp = TempDir::new().unwrap();
        unsafe {
            std::env::set_var(
                "TARGET_CONF",
                tmp.path().join(".target.jsonc").to_str().unwrap(),
            );
        }
        let port = "47112";
        let hold = hold_endpoint(
            crate::lock_manager::default_lock_dir().as_str(),
            "10.0.0.1",
            port,
        );
        let opts = quick_opts();
        let prepared = prepare_init(tmp.path(), &opts).unwrap();
        let mut io = CannedIO::new(&[]).with_busy(&[BusyDecision::Continue]);
        let outcome = finish_init(
            &mut io,
            &opts,
            &prepared,
            &one_dut_answers("10.0.0.1", port),
        )
        .unwrap();
        assert!(outcome.config_created);
        let text = std::fs::read_to_string(tmp.path().join(".target.jsonc")).unwrap();
        assert!(text.contains(port), "{text}");
        assert!(hold.path.exists(), "the holder's lock is NEVER touched");
        drop(hold);
        unsafe {
            std::env::remove_var("TARGET_CONF");
        }
    }

    /// Non-interactive cannot ask: it warns via notes and continues.
    #[test]
    fn test_finish_init_non_interactive_warns_and_continues() {
        let _guard = lock_env();
        let tmp = TempDir::new().unwrap();
        unsafe {
            std::env::set_var(
                "TARGET_CONF",
                tmp.path().join(".target.jsonc").to_str().unwrap(),
            );
        }
        let port = "47113";
        let hold = hold_endpoint(
            crate::lock_manager::default_lock_dir().as_str(),
            "10.0.0.1",
            port,
        );
        let opts = InitOptions {
            non_interactive: true,
            ..Default::default()
        };
        let prepared = prepare_init(tmp.path(), &opts).unwrap();
        let mut io = CannedIO::new(&[]);
        let outcome = finish_init(
            &mut io,
            &opts,
            &prepared,
            &one_dut_answers("10.0.0.1", port),
        )
        .unwrap();
        assert!(outcome.config_created);
        assert!(io.busy_calls.is_empty(), "headless never asks");
        assert!(
            io.notes.iter().any(|n| n.contains(port)),
            "the warning names the endpoint: {:?}",
            io.notes
        );
        assert!(io.notes.iter().any(|n| n.contains("held by another MCP")));
        assert!(hold.path.exists(), "the holder's lock is NEVER touched");
        drop(hold);
        unsafe {
            std::env::remove_var("TARGET_CONF");
        }
    }

    /// A no-op UPDATE (nothing to write) still runs the gate: the user must
    /// learn about the holder even when nothing would change. The default
    /// Continue proceeds and writes nothing.
    #[test]
    fn test_finish_init_noop_update_still_gates() {
        let _guard = lock_env();
        let tmp = TempDir::new().unwrap();
        unsafe {
            std::env::set_var(
                "TARGET_CONF",
                tmp.path().join(".target.jsonc").to_str().unwrap(),
            );
        }
        // Write a config that already matches the answers.
        let opts = quick_opts();
        let prepared = prepare_init(tmp.path(), &opts).unwrap();
        let answers = one_dut_answers("10.0.0.1", "47114");
        let config = build_config_from_answers(&answers).unwrap();
        write_validated(&prepared.target_path, &render_jsonc(&config, true)).unwrap();
        let port = "47114";
        let hold = hold_endpoint(
            crate::lock_manager::default_lock_dir().as_str(),
            "10.0.0.1",
            port,
        );
        let prepared2 = prepare_init(tmp.path(), &opts).unwrap();
        let mut io = CannedIO::new(&[]);
        let outcome = finish_init(&mut io, &opts, &prepared2, &answers).unwrap();
        assert!(
            outcome.changes.iter().any(|c| c.contains("unchanged")),
            "{:?}",
            outcome.changes
        );
        assert_eq!(io.busy_calls.len(), 1, "the gate still asks on a no-op");
        drop(hold);
        unsafe {
            std::env::remove_var("TARGET_CONF");
        }
    }

    /// The plain walk asks the DUT-owned advanced set in EVERY mode — one
    /// bare Enter per key (the natural defaults are informational hints
    /// only: they are never recorded, so untouched flows stay byte-stable).
    fn dut_advanced_empties() -> Vec<String> {
        DUT_ADVANCED_EMPTIES.iter().map(|s| s.to_string()).collect()
    }

    /// QUICK_ANSWERS + one bare Enter per DUT-owned advanced question.
    fn quick_with_dut_advanced() -> Vec<&'static str> {
        QUICK_ANSWERS
            .iter()
            .copied()
            .chain(DUT_ADVANCED_EMPTIES)
            .collect()
    }

    /// Per-DUT advanced identity: dut2's typed values live in dut2's
    /// answers ONLY (dut1's map stays empty), and the advanced GATE does
    /// not gate them — F2 with the gate OFF must persist typed DUT-owned
    /// values (GATE BUG fix).
    #[test]
    fn test_resolve_dut_advanced_isolated_and_gate_independent() {
        let existing = parse_jsonc_text(LOCATE_FIXTURE).unwrap();
        let schema = build_form_schema(Some(&existing), &InitOptions::default());
        let base = |extra: Vec<(FieldKey, &str)>| {
            let mut v = values_of(&[
                (FieldKey::HostCount, "1"),
                (FieldKey::DutCount(0), "2"),
                (FieldKey::DutName(0, 0), "board1"),
                (FieldKey::DutName(0, 1), "board2"),
                (FieldKey::DutPort(0, 0), "41000"),
                (FieldKey::DutPort(0, 1), "41001"),
            ]);
            for (k, val) in extra {
                v.insert(k, val.to_string());
            }
            v
        };
        // GATE OFF: typed dut2 values persist; dut1's buffers untouched.
        let values = base(vec![
            (
                FieldKey::DutAdvanced(0, 1, "monitor.crash_keywords"),
                "Oops2",
            ),
            (FieldKey::DutAdvanced(0, 1, "serial.baudrate"), "921600"),
        ]);
        let answers =
            resolve_answers(&schema, &values, false, NamePolicy::ErrorOnCollision).unwrap();
        let dut1 = &answers.hosts[0].duts[0];
        let dut2 = &answers.hosts[0].duts[1];
        assert_eq!(
            dut1.advanced.get("serial.baudrate").map(String::as_str),
            Some("460800"),
            "dut1 required baudrate is materialized: {dut1:?}"
        );
        assert_eq!(
            dut2.advanced
                .get("monitor.crash_keywords")
                .map(String::as_str),
            Some("Oops2")
        );
        assert_eq!(
            dut2.advanced.get("serial.baudrate").map(String::as_str),
            Some("921600")
        );
        // The gate only governs the HOST-LEVEL set.
        assert!(answers.advanced.is_empty());
        // Validation errors are keyed per DUT.
        let mut values = base(vec![]);
        values.insert(
            FieldKey::DutAdvanced(0, 1, "monitor.max_archived_logs"),
            "abc".into(),
        );
        let errs =
            resolve_answers(&schema, &values, false, NamePolicy::ErrorOnCollision).unwrap_err();
        assert!(
            errs.iter().any(|(k, m)| {
                k == &FieldKey::DutAdvanced(0, 1, "monitor.max_archived_logs")
                    && m.contains("monitor.max_archived_logs: invalid count: abc")
            }),
            "{errs:?}"
        );
    }

    /// CREATE: per-DUT advanced answers land ONLY in the typed DUT's
    /// override sections; the GLOBAL sections carry only the host-level
    /// answers — DUT-owned keys never leak into the fallbacks.
    #[test]
    fn test_create_renders_dut_advanced_overrides_global_untouched() {
        let mut d0 = dut_answers("dut1", "41002");
        d0.advanced = HashMap::from([
            ("serial.baudrate".into(), "921600".into()),
            ("monitor.crash_keywords".into(), "Oops:".into()),
            ("flash.loader_bin".into(), "/opt/custom-loader.bin".into()),
        ]);
        let mut d1 = dut_answers("dut2", "41003");
        d1.advanced = HashMap::from([("monitor.crash_keywords".into(), "BUG:".into())]);
        let answers = WizardAnswers {
            hosts: vec![HostAnswers {
                ip: "10.0.0.1".into(),
                user: String::new(),
                pass: String::new(),
                host_name: String::new(),
                duts: vec![d0, d1],
            }],
            advanced: HashMap::from([("monitor.hang_timeout".into(), "30".into())]),
            announcements: Vec::new(),
        };
        let config = build_config_from_answers(&answers).unwrap();
        // Global: ONLY the host-level answer landed.
        assert!(config.serial.is_none());
        assert!(config.target.is_none());
        assert!(config.flash.is_none());
        assert!(config.control.is_none());
        assert_eq!(config.monitor.as_ref().unwrap().hang_timeout, Some(30));
        assert!(
            config.monitor.as_ref().unwrap().crash_keywords.is_none(),
            "DUT-owned crash_keywords must not leak into the global monitor"
        );
        // dut1 carries exactly its typed overrides.
        let d0 = &config.dev_hosts[0].duts[0];
        assert_eq!(
            d0.serial
                .as_ref()
                .unwrap()
                .baudrate
                .as_ref()
                .unwrap()
                .to_value_string(),
            "921600"
        );
        assert_eq!(
            d0.monitor
                .as_ref()
                .unwrap()
                .crash_keywords
                .as_ref()
                .unwrap(),
            &["Oops:".to_string()]
        );
        assert_eq!(
            d0.flash.as_ref().unwrap().loader_bin.as_deref(),
            Some("/opt/custom-loader.bin")
        );
        // dut2: only crash_keywords; flash absent.
        let d1 = &config.dev_hosts[0].duts[1];
        assert!(
            d1.serial.as_ref().unwrap().baudrate.is_none(),
            "dut2 typed no baudrate — no per-DUT line"
        );
        assert_eq!(
            d1.monitor
                .as_ref()
                .unwrap()
                .crash_keywords
                .as_ref()
                .unwrap(),
            &["BUG:".to_string()]
        );
        assert!(d1.flash.is_none());
        let text = render_jsonc(&config, true);
        parse_jsonc_text(&text).unwrap_or_else(|e| panic!("{e}\n{text}"));
        assert!(text.contains("\"crash_keywords\""), "{text}");
    }

    /// UPDATE: a NEW DUT's typed overrides ride the text-patch append path
    /// into its own block; every pre-existing line stays byte-identical.
    #[test]
    fn test_update_appends_new_dut_overrides_only() {
        let existing_text = "{\n  // untouched\n  \"dev_hosts\": [\n    {\n      \"ip\": \"10.0.0.1\",\n      \"user\": \"u\",\n      \"pass\": \"\",\n      \"duts\": [\n        {\n          \"dut_name\": \"board1\",\n          \"serial\": { \"port\": 41000 },\n          \"target\": { \"login_user\": \"root\" }\n        }\n      ]\n    }\n  ],\n  \"monitor\": { \"hang_timeout\": 60 }\n}\n";
        let existing = parse_jsonc_text(existing_text).unwrap();
        let mut b2 = dut_answers("board2", "41001");
        b2.advanced = HashMap::from([
            ("monitor.crash_keywords".into(), "Oops:".into()),
            ("flash.full_image_cmd".into(), "uf-b {image}".into()),
        ]);
        let answers = WizardAnswers {
            hosts: vec![HostAnswers {
                ip: "10.0.0.1".into(),
                user: "u".into(),
                pass: String::new(),
                host_name: String::new(),
                duts: vec![dut_answers("board1", "41000"), b2],
            }],
            advanced: HashMap::new(),
            announcements: Vec::new(),
        };
        let ops = compute_update_ops(existing_text, &existing, &answers).unwrap();
        let new_text = apply_edits(
            existing_text,
            &ops.iter().map(|o| o.op.clone()).collect::<Vec<_>>(),
        )
        .unwrap();
        // The original DUT block is preserved verbatim (the splice appends
        // only before the duts array's closing bracket).
        let before = &existing_text[..existing_text.rfind("\n      ]").unwrap()];
        assert!(
            new_text.contains(before),
            "existing lines untouched: {new_text}"
        );
        // The new DUT block carries its own override sections.
        let parsed = parse_jsonc_text(&new_text).unwrap_or_else(|e| panic!("{e}\n{new_text}"));
        let b2p = &parsed.dev_hosts[0].duts[1];
        assert_eq!(b2p.dut_name, "board2");
        assert_eq!(
            b2p.monitor
                .as_ref()
                .unwrap()
                .crash_keywords
                .as_ref()
                .unwrap(),
            &["Oops:".to_string()]
        );
        assert_eq!(
            b2p.flash.as_ref().unwrap().full_image_cmd.as_deref(),
            Some("uf-b {image}")
        );
        // board1 got no per-DUT sections.
        assert!(parsed.dev_hosts[0].duts[0].monitor.is_none());
        assert!(parsed.dev_hosts[0].duts[0].flash.is_none());
        assert!(
            ops.iter().any(|o| o.desc.contains("added DUT 'board2'")),
            "{:?}",
            ops.iter().map(|o| &o.desc).collect::<Vec<_>>()
        );
    }

    /// UPDATE: typed per-DUT values SET keys inside the DUT's sections; a
    /// cleared SkipInherit field with an existing override removes it
    /// (inherit global); an answer equal to the global fallback writes
    /// nothing (no per-DUT noise).
    #[test]
    fn test_update_dut_advanced_set_unset_and_global_equal_skip() {
        // Wizard-written style: one member per line (the UPDATE text-patch
        // contract — RemoveKey removes whole member lines).
        let existing_text = "{\n  \"dev_hosts\": [\n    {\n      \"ip\": \"10.0.0.1\",\n      \"user\": \"u\",\n      \"pass\": \"\",\n      \"duts\": [\n        {\n          \"dut_name\": \"board1\",\n          \"serial\": {\n            \"port\": 41000\n          },\n          \"target\": {\n            \"login_user\": \"root\",\n            \"login_pass\": \"oldpw\"\n          },\n          \"monitor\": {\n            \"crash_keywords\": [\n              \"Old\"\n            ]\n          }\n        }\n      ]\n    }\n  ],\n  \"serial\": {\n    \"baudrate\": 460800\n  }\n}\n";
        let existing = parse_jsonc_text(existing_text).unwrap();
        let answers = WizardAnswers {
            hosts: vec![HostAnswers {
                ip: "10.0.0.1".into(),
                user: "u".into(),
                pass: String::new(),
                host_name: String::new(),
                duts: vec![DutAnswers {
                    advanced: HashMap::from([
                        ("monitor.crash_keywords".into(), "New".into()),
                        // cleared SkipInherit field → explicit unset
                        ("target.login_pass".into(), String::new()),
                        // equals the GLOBAL fallback → no per-DUT write
                        ("serial.baudrate".into(), "460800".into()),
                    ]),
                    ..dut_answers("board1", "41000")
                }],
            }],
            advanced: HashMap::new(),
            announcements: Vec::new(),
        };
        let ops = compute_update_ops(existing_text, &existing, &answers).unwrap();
        let new_text = apply_edits(
            existing_text,
            &ops.iter().map(|o| o.op.clone()).collect::<Vec<_>>(),
        )
        .unwrap();
        let parsed = parse_jsonc_text(&new_text).unwrap_or_else(|e| panic!("{e}\n{new_text}"));
        let dut = &parsed.dev_hosts[0].duts[0];
        assert_eq!(
            dut.monitor
                .as_ref()
                .unwrap()
                .crash_keywords
                .as_ref()
                .unwrap(),
            &["New".to_string()]
        );
        // login_pass removed from the DUT's target section (login_user kept).
        assert_eq!(dut.target.as_ref().unwrap().login_pass, None);
        assert_eq!(
            dut.target.as_ref().unwrap().login_user.as_deref(),
            Some("root")
        );
        // baudrate still inherits the global fallback — no per-DUT key.
        assert!(
            dut.serial.as_ref().unwrap().baudrate.is_none(),
            "equal-to-global answers write nothing per-DUT"
        );
        assert_eq!(
            parsed
                .serial
                .as_ref()
                .unwrap()
                .baudrate
                .as_ref()
                .unwrap()
                .to_value_string(),
            "460800",
            "the global section stays untouched"
        );
        assert!(!new_text.contains("oldpw"), "{new_text}");
        assert!(
            ops.iter()
                .any(|o| o.desc.contains("unset target.login_pass")),
            "{:?}",
            ops.iter().map(|o| &o.desc).collect::<Vec<_>>()
        );
    }

    /// The loader top-level (section-less) DUT key sets directly INSIDE
    /// the DUT block — never under a bogus `""` section — and the global
    /// fallback stays untouched (the "loader" key).
    #[test]
    fn test_update_dut_advanced_loader_top_level_key() {
        let existing_text = "{\n  \"dev_hosts\": [\n    {\n      \"ip\": \"10.0.0.1\",\n      \"user\": \"u\",\n      \"pass\": \"\",\n      \"duts\": [\n        {\n          \"dut_name\": \"board1\",\n          \"serial\": {\n            \"port\": 41000\n          }\n        }\n      ]\n    }\n  ],\n  \"loader\": \"uboot\"\n}\n";
        let existing = parse_jsonc_text(existing_text).unwrap();
        let answers = WizardAnswers {
            hosts: vec![HostAnswers {
                ip: "10.0.0.1".into(),
                user: "u".into(),
                pass: String::new(),
                host_name: String::new(),
                duts: vec![DutAnswers {
                    advanced: HashMap::from([("loader".into(), "uefi".into())]),
                    ..dut_answers("board1", "41000")
                }],
            }],
            advanced: HashMap::new(),
            announcements: Vec::new(),
        };
        let ops = compute_update_ops(existing_text, &existing, &answers).unwrap();
        let new_text = apply_edits(
            existing_text,
            &ops.iter().map(|o| o.op.clone()).collect::<Vec<_>>(),
        )
        .unwrap();
        let parsed = parse_jsonc_text(&new_text).unwrap_or_else(|e| panic!("{e}\n{new_text}"));
        // Per-DUT override landed directly inside the DUT block.
        assert_eq!(parsed.dev_hosts[0].duts[0].loader.as_deref(), Some("uefi"));
        // The global fallback is untouched.
        assert_eq!(parsed.loader.as_deref(), Some("uboot"));
        assert!(!new_text.contains("\"\": {"), "{new_text}");
    }

    /// Schema layer: DutDefaults carries each DUT's OWN advanced values;
    /// the materialized defaults prefer the DUT override, fall back to the
    /// GLOBAL current value, and compose reference_log from THAT DUT's
    /// dut_name.
    #[test]
    fn test_schema_dut_advanced_prefill_and_materialize() {
        const FIXTURE: &str = r#"{
  "dev_hosts": [
    {
      "ip": "10.0.0.1",
      "user": "u",
      "pass": "",
      "duts": [
        {
          "dut_name": "board1",
          "serial": { "port": 41000, "baudrate": 921600 },
          "monitor": { "crash_keywords": ["Kernel panic"] }
        },
        {
          "dut_name": "board2",
          "serial": { "port": 41001 }
        }
      ]
    }
  ],
  "serial": { "baudrate": 460800 }
}"#;
        let existing = parse_jsonc_text(FIXTURE).unwrap();
        let schema = build_form_schema(Some(&existing), &InitOptions::default());
        let d0 = &schema.host_defaults[0].duts[0];
        assert_eq!(
            d0.advanced.get("serial.baudrate").map(String::as_str),
            Some("921600")
        );
        assert_eq!(
            d0.advanced
                .get("monitor.crash_keywords")
                .map(String::as_str),
            Some("Kernel panic")
        );
        let d1 = &schema.host_defaults[0].duts[1];
        assert!(d1.advanced.is_empty(), "board2 typed nothing per-DUT");
        let fields = schema_fields(&schema);
        let by = |k: &FieldKey| fields.iter().find(|f| &f.key == k).unwrap().default.clone();
        // DUT override wins…
        assert_eq!(
            by(&FieldKey::DutAdvanced(0, 0, "serial.baudrate")).as_deref(),
            Some("921600")
        );
        // …absent override falls back to the GLOBAL fallback (hint only —
        // never written unless typed).
        assert_eq!(
            by(&FieldKey::DutAdvanced(0, 1, "serial.baudrate")).as_deref(),
            Some("460800")
        );
        // reference_log composes from THAT DUT's dut_name.
        assert_eq!(
            by(&FieldKey::DutAdvanced(0, 1, "monitor.reference_log")).as_deref(),
            Some(".dut-serial/board2/reference-boot.log")
        );
        // The host-level keys keep the global current default.
        assert_eq!(
            by(&FieldKey::Advanced("monitor.hang_timeout")).as_deref(),
            Some("60")
        );
    }

    /// The plain walk asks the DUT-owned advanced set in EVERY mode (the
    /// gate only governs the host-level fallback set): gate OFF still
    /// records typed per-DUT values.
    #[test]
    fn test_walk_dut_advanced_asked_regardless_of_gate() {
        let mut schema = build_form_schema(None, &InitOptions::default());
        let mut inputs: Vec<String> = QUICK_ANSWERS.iter().map(|s| s.to_string()).collect();
        inputs.extend(dut_advanced_empties());
        // serial.baudrate is the FIRST dut-owned key (input index 9).
        inputs[9] = "921600".into();
        let mut io = RecordingIO::new(&[]);
        io.inner.inputs = inputs.into();
        io.inner.confirms.push_back(false); // relay gate: NO
        io.inner.confirms.push_back(false); // advanced gate: NO
        let answers = walk_schema(&mut schema, &mut io).unwrap();
        assert_eq!(
            answers.hosts[0].duts[0]
                .advanced
                .get("serial.baudrate")
                .map(String::as_str),
            Some("921600")
        );
        assert!(
            answers.advanced.is_empty(),
            "gate NO → no host-level answers"
        );
        // Host-level questions were never asked.
        assert!(
            !io.calls.iter().any(|(p, _)| p.contains("Hang timeout")),
            "{:?}",
            io.calls
        );
    }

    // ── fill_defaults (headless) ───────────────────────────────────────

    #[test]
    fn test_fill_defaults_create_matches_legacy() {
        let schema = build_form_schema(None, &quick_opts());
        let answers = fill_defaults(&schema);
        assert_eq!(answers.hosts.len(), 1);
        assert_eq!(answers.hosts[0].ip, "");
        assert_eq!(answers.hosts[0].user, "");
        assert_eq!(answers.hosts[0].pass, "");
        assert_eq!(answers.hosts[0].host_name, "");
        assert_eq!(answers.hosts[0].duts.len(), 1);
        assert_eq!(answers.hosts[0].duts[0].dut_name, "dut1");
        assert_eq!(answers.hosts[0].duts[0].serial_port, "");
        assert_eq!(answers.hosts[0].duts[0].login_user, "root");
        // Relay gate defaults NO on create: no relay answers at all (the
        // relay section is omitted from the written config).
        assert!(!answers.hosts[0].duts[0].relay_gate);
        assert_eq!(answers.hosts[0].duts[0].relay_type, "");
        assert!(answers.advanced.is_empty());
        assert!(
            answers.announcements.is_empty(),
            "no announcements headless"
        );
    }

    #[test]
    fn test_fill_defaults_update_is_strict_noop_answers() {
        let existing = parse_jsonc_text(UPDATE_GLOBAL_FIXTURE).unwrap();
        // advanced: true → the advanced group records ONLY current values.
        let schema = build_form_schema(
            Some(&existing),
            &InitOptions {
                advanced: true,
                ..Default::default()
            },
        );
        let answers = fill_defaults(&schema);
        assert_eq!(answers.hosts.len(), 1);
        assert_eq!(answers.hosts[0].ip, "10.0.0.1");
        assert_eq!(answers.hosts[0].user, "linaro");
        assert_eq!(
            answers.hosts[0].host_name, "",
            "no host_name in the fixture → empty answer (strict no-op)"
        );
        assert_eq!(answers.hosts[0].duts.len(), 1);
        assert_eq!(answers.hosts[0].duts[0].serial_port, "41000");
        // Only current advanced values — keys absent from the config are
        // skipped entirely (no natural defaults), so a headless update
        // stays a strict no-op.
        assert_eq!(
            answers.advanced.get("serial.baudrate").map(String::as_str),
            Some("1500000")
        );
        assert_eq!(
            answers.advanced.get("serial.rfc2217").map(String::as_str),
            None,
            "rfc2217 is program-controlled, not wizard-driven"
        );
        assert_eq!(
            answers.advanced.get("flash.loader_bin").map(String::as_str),
            Some("rockdev/loader.bin")
        );
        // The global relay section is NOT wizard-managed anymore.
        assert_eq!(answers.advanced.get("relay.reset_ch"), None);
        assert_eq!(answers.advanced.get("monitor.hang_timeout"), None);
        // The DUT's OWN relay section pre-fills the gate: board1 has one
        // (YES + type), so a headless update keeps it.
        assert!(answers.hosts[0].duts[0].relay_gate);
        assert_eq!(answers.hosts[0].duts[0].relay_type, "ch340");
        assert!(answers.announcements.is_empty());
        // Non-advanced headless update: no advanced answers at all.
        let quick = build_form_schema(Some(&existing), &quick_opts());
        assert!(fill_defaults(&quick).advanced.is_empty());
    }

    // ── prepare_init ───────────────────────────────────────────────────

    #[test]
    fn test_prepare_init_refuses_unparseable_before_questions() {
        let _guard = lock_env();
        let tmp = TempDir::new().unwrap();
        unsafe {
            std::env::remove_var("TARGET_CONF");
        }
        let path = tmp.path().join(CONFIG_FILE_NAME);
        std::fs::write(&path, "this is not jsonc {{{").unwrap();
        let err = prepare_init(tmp.path(), &quick_opts()).unwrap_err();
        assert!(err.contains("does not parse"), "{err}");
        assert!(err.contains("refusing to edit"), "{err}");
    }

    #[test]
    fn test_init_refuses_config_with_removed_host_alias() {
        let _guard = lock_env();
        let tmp = TempDir::new().unwrap();
        unsafe {
            std::env::remove_var("TARGET_CONF");
        }
        let path = tmp.path().join(CONFIG_FILE_NAME);
        std::fs::write(
            &path,
            "{\n  \"dev_hosts\": [\n    { \"alias\": \"old\", \"ip\": \"10.0.0.1\" }\n  ]\n}\n",
        )
        .unwrap();
        let err = prepare_init(tmp.path(), &quick_opts()).unwrap_err();
        assert!(err.contains("renamed to host_name"), "{err}");
        assert!(err.contains("refusing to edit"), "{err}");
        assert!(err.contains("re-run 'dutabo init'"), "{err}");
    }

    #[test]
    fn test_prepare_init_force_reroutes_to_create_defaults() {
        let _guard = lock_env();
        let tmp = TempDir::new().unwrap();
        unsafe {
            std::env::remove_var("TARGET_CONF");
        }
        let path = tmp.path().join(CONFIG_FILE_NAME);
        std::fs::write(&path, LOCATE_FIXTURE).unwrap();
        let prepared = prepare_init(
            tmp.path(),
            &InitOptions {
                force: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(prepared.created);
        assert!(prepared.existing.is_none());
        assert!(prepared.existing_text.is_some());
        // CREATE defaults: empty host_defaults (the existing config is NOT
        // parsed/used as prefill).
        assert!(prepared.schema.host_defaults.is_empty());
        // A force run overwrites the file (verified end-to-end by
        // run_init's --force line).
        let mut io = CannedIO::new(&quick_with_dut_advanced());
        let outcome = run_init(
            &mut io,
            &InitOptions {
                force: true,
                quick: true,
                ..Default::default()
            },
            tmp.path(),
        )
        .unwrap();
        assert!(outcome.config_created);
        assert!(
            outcome
                .changes
                .first()
                .is_some_and(|c| c.contains("--force: overwriting")),
            "{:?}",
            outcome.changes
        );
    }
}
