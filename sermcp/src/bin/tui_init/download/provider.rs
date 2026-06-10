//! The provider contract + the generic device model.
pub use super::diff::DeviceIdentity;

/// One enumerated download device: the RAW vendor record, a stable
/// identity for before/after matching and a SHORT UI summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadDevice {
    pub raw: String,
    pub identity: DeviceIdentity,
    pub summary: String,
}

#[derive(Debug)]
pub enum DownloadError {
    /// The list/entry command failed on the dev host (SSH or tool).
    HostCommand(String),
    /// More than one new device appeared — refusing to guess which one
    /// is this DUT (flashing the wrong board is the failure mode).
    Ambiguous(Vec<DownloadDevice>),
}

impl std::fmt::Display for DownloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HostCommand(reason) => write!(f, "device-list command failed: {reason}"),
            Self::Ambiguous(devices) => write!(
                f,
                "ambiguous: {} new devices appeared — {} (refusing to pick one)",
                devices.len(),
                devices
                    .iter()
                    .map(|d| d.summary.as_str())
                    .collect::<Vec<_>>()
                    .join(" | ")
            ),
        }
    }
}

/// Everything one download test needs about the environment. The
/// execution helpers route through the shared plumbing (SSH for host
/// commands, the project MCP for serial/relay) — providers never open
/// sockets themselves.
#[derive(Debug, Clone)]
pub struct DownloadContext {
    pub host: String,
    pub user: String,
    pub pass: String,
    pub mcp_ports: Vec<u16>,
    /// Relay reset channel (physical reset key).
    pub reset_channel: Option<u8>,
    /// Relay download channel (the download/FEL key) + the button NAME
    /// the engine's serial_button tool accepts for it.
    pub download_channel: Option<u8>,
    pub download_button: String,
    /// Whether dev_ctrl selects a real relay backend.
    pub dev_ctrl_enabled: bool,
}

impl DownloadContext {
    /// Hardware entry needs the physical sandwich: a download key AND a
    /// reset key, on an enabled relay.
    pub fn hardware_entry_available(&self) -> bool {
        self.dev_ctrl_enabled
            && self.reset_channel.is_some()
            && self.download_channel.is_some()
            && !self.download_button.is_empty()
    }

    /// Run a command on the dev host over SSH and return its stdout
    /// (non-zero + empty stderr = "no devices", not an error — the
    /// sunxi-fel convention).
    pub fn run_on_host(&self, cmd: &str) -> Result<String, DownloadError> {
        crate::tui_init::flash::ssh_list_devices_stdout(&self.host, &self.user, &self.pass, cmd)
            .map_err(DownloadError::HostCommand)
    }

    /// The engine's current DUT state (`uboot`, `booting`, `active`, …).
    pub fn dut_state(&self) -> Option<String> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()?;
        crate::tui_init::flash::mcp_dut_state_on(&rt, &self.mcp_ports)
    }

    /// Wait (bounded) for the engine to report the wanted state.
    pub fn wait_for_state(&self, wanted: &str, timeout_secs: u64) -> bool {
        crate::tui_init::flash::wait_for_dut_state(&self.mcp_ports, wanted, timeout_secs)
    }

    /// Marker-wrapped shell command on this DUT's console. A successful
    /// download entry reboots the DUT before the marker returns — the
    /// return value is informational only, the device poll is the truth.
    pub fn serial_command(&self, cmd: &str) -> bool {
        crate::tui_init::mcp_probe_mark(
            &self.mcp_ports,
            "serial_send_command",
            serde_json::json!({"command": cmd, "timeout": 5}),
        ) == crate::tui_init::ProbeMark::Ok
    }

    /// Raw command at a U-Boot prompt (no marker wrapping).
    pub fn uboot_command(&self, cmd: &str) -> bool {
        crate::tui_init::mcp_probe_mark(
            &self.mcp_ports,
            "serial_uboot_command",
            serde_json::json!({"command": cmd, "timeout": 5}),
        ) == crate::tui_init::ProbeMark::Ok
    }

    /// Press/release the physical download key.
    pub fn press_download(&self) -> bool {
        self.button(&self.download_button.clone(), "press")
    }
    pub fn release_download(&self) -> bool {
        self.button(&self.download_button.clone(), "release")
    }

    fn button(&self, button: &str, action: &str) -> bool {
        crate::tui_init::mcp_probe_mark(
            &self.mcp_ports,
            "serial_button",
            serde_json::json!({"button": button, "action": action}),
        ) == crate::tui_init::ProbeMark::Ok
    }

    /// One reset pulse via the engine (no boot wait, no retry loop — the
    /// device poll after it is the verification).
    pub fn pulse_reset(&self) -> bool {
        crate::tui_init::mcp_probe_mark(
            &self.mcp_ports,
            "serial_reset",
            serde_json::json!({"wait_boot": false, "failure_retry": 1}),
        ) == crate::tui_init::ProbeMark::Ok
    }
}

/// One software-entry step the main flow executes (then polls).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SoftwareStep<'a> {
    /// Raw command at a U-Boot prompt (no marker wrapping).
    UbootRaw(&'a str),
    /// Resume the interrupted boot, wait for the shell, then send the
    /// command marker-wrapped.
    ShellAfterBoot(&'a str),
    /// Marker-wrapped shell command in the current console.
    Shell(&'a str),
}

/// The provider contract. `enumerate` runs the list command on the dev
/// host and parses it into device records; `software_entry_plan` /
/// `enter_hardware` move THIS DUT into download mode (the exact commands
/// and timing live in the provider — never in the caller).
pub trait DownloadProvider: Send + Sync {
    fn id(&self) -> &'static str;

    /// The software-entry command (sent on the DUT's console); None when
    /// the platform has no reliable software entry.
    fn default_loader_cmd(&self) -> Option<&'static str>;

    /// The device-list command run on the dev host.
    fn default_list_devices_cmd(&self) -> &'static str;

    /// Parse vendor list output into device records. Headers/counters
    /// NEVER enter the set — only parsed device records do.
    fn parse_devices(&self, stdout: &str) -> Vec<DownloadDevice>;

    /// Run `list_cmd` on the dev host and parse it.
    fn enumerate(
        &self,
        ctx: &DownloadContext,
        list_cmd: &str,
    ) -> Result<Vec<DownloadDevice>, DownloadError> {
        let stdout = ctx.run_on_host(list_cmd)?;
        Ok(self.parse_devices(&stdout))
    }

    /// The software-entry plan for `loader_cmd` given the engine's
    /// current DUT state. The MAIN FLOW executes the steps and polls the
    /// device list between them — the PLAN (which commands, in which
    /// console context, including platform fallbacks like Rockchip's
    /// `rbrom`) is provider-owned. Empty = no software entry.
    fn software_entry_plan<'a>(
        &self,
        loader_cmd: &'a str,
        state: Option<&str>,
    ) -> Vec<SoftwareStep<'a>>;

    /// Hardware entry: the physical key sandwich (provider-owned timing).
    fn enter_hardware(&self, ctx: &DownloadContext) -> Result<(), DownloadError>;

    /// The short read-only summary for the UI's `device` row.
    fn summarize(&self, dev: &DownloadDevice) -> String {
        dev.summary.clone()
    }
}
