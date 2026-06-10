//! Flash-box probes for the `dutabo init` TUI (the TEST button's
//! device-list check): SSH `flash.list_devices_cmd` on the dev host BEFORE
//! and AFTER triggering flashing mode, and XOR the two outputs — the
//! newly-appeared device lines are the probe detail shown on the read-only
//! "flash.devices" row.
//!
//! The tool is NEVER hardcoded: whatever command the operator puts in
//! `flash.list_devices_cmd` (any vendor's device-list command) is run
//! verbatim, and the before/after comparison is line-based with digit-run
//! normalization — count-bearing headers and re-enumeration number shifts
//! cancel, only genuinely new device lines survive. Other boards already
//! sitting in flashing mode appear in BOTH captures and cancel out, so the
//! probe is multi-DUT safe.
//!
//! After the trigger the probe polls the list for ~15 s: a board takes
//! several seconds to reboot into its flashing mode (ROM entry plus USB
//! re-enumeration), so a short poll window silently misses the device and
//! false-fails.
//!
//! The trigger is CONTEXT-AWARE. `flash.loader_cmd` is a command sent on
//! the DUT's serial console, but the marker-echo wrapper only works in a
//! Linux shell, while U-Boot commands (the provider's dumping command)
//! only work AT the U-Boot prompt. The probe checks the DUT state first
//! (serial_get_state):
//!
//!  - state `uboot` — the U-Boot interrupt probe that ran earlier in the
//!    same TEST leaves the DUT AT the prompt. The loader_cmd is tried RAW
//!    there first (U-Boot commands need no shell wrapper; a shell command
//!    no-ops harmlessly as "Unknown command" and falls through). If no
//!    device appears, `boot` resumes the interrupted boot and the probe
//!    waits for the Linux shell before sending the loader_cmd shell-style.
//!  - state `booting`/`connecting` — the baud/relay probes reset the DUT
//!    earlier; the probe waits for the shell before triggering.
//!  - otherwise the loader_cmd is sent immediately (marker-wrapped).
//!
//! A serial trigger whose poll shows no new device falls THROUGH to the
//! recovery/download relay attempts instead of failing immediately: the
//! marker echo completes even when the command no-ops (a shell command
//! rejected at the U-Boot prompt), so a "successful" serial call is not
//! proof the DUT entered flashing mode.

use super::{FieldKey, ProbeMark, ProbeTarget};

/// The read-only "flash.devices" row key (synthetic — never a config
/// field): the device-list probe's mark and detail land here.
pub fn devices_key(hi: usize, di: usize) -> FieldKey {
    FieldKey::DutAdvanced(hi, di, "flash.devices")
}

/// Is this the read-only flash device-list row?
pub fn is_devices_key(key: &FieldKey) -> bool {
    matches!(key, FieldKey::DutAdvanced(_, _, "flash.devices"))
}

/// The row label.
pub fn devices_label() -> &'static str {
    "device"
}

/// First line of a command's stderr (the actionable failure reason).
fn first_stderr_line(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr)
        .lines()
        .next()
        .unwrap_or("unknown ssh error")
        .trim()
        .to_string()
}

/// Did the key-auth attempt fail on AUTHENTICATION (as opposed to the
/// remote command itself exiting non-zero)? ssh prints "Permission
/// denied"/"publickey" on ITS stderr for auth failures; a remote
/// command's own non-zero exit (zero devices is not an error)
/// leaves ssh's stderr empty.
fn ssh_auth_denied(out: &std::process::Output) -> bool {
    let err = String::from_utf8_lossy(&out.stderr).to_ascii_lowercase();
    err.contains("permission denied") || err.contains("publickey")
}

/// Upper bound for one ssh invocation. `ConnectTimeout` only bounds the TCP
/// connect — a remote command wedged on a hung USB stack would otherwise
/// park the probe thread forever and starve every later probe in its lane.
const SSH_COMMAND_TIMEOUT_SECS: u64 = 30;

/// Wait for a spawned child with a hard wall-clock cap: the child is killed
/// and reaped when it outlives `secs`. `None` means "capped".
fn wait_with_timeout(
    child: &mut std::process::Child,
    secs: u64,
) -> Option<std::process::ExitStatus> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            Err(_) => return None,
        }
    }
}

/// Drain a child pipe on a helper thread so a chatty child cannot fill the
/// 64K pipe buffer and deadlock against [`wait_with_timeout`]'s wait loop.
fn drain_pipe<R: std::io::Read + Send + 'static>(
    pipe: Option<R>,
) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut pipe) = pipe
            && let Err(e) = pipe.read_to_end(&mut buf)
        {
            // EOF after the child dies arrives as Ok(0); anything else means
            // the pipe broke — the partial capture is still the best we have.
            let _ = e;
        }
        buf
    })
}

/// [`Command::output`](std::process::Command::output) with a hard cap:
/// stdout/stderr are drained on helper threads, the child is killed at the
/// deadline, and a capped run surfaces as `Err` (the caller's actionable
/// reason), never as a partial success.
fn run_capped(mut cmd: std::process::Command, secs: u64) -> Result<std::process::Output, String> {
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let stdout = drain_pipe(child.stdout.take());
    let stderr = drain_pipe(child.stderr.take());
    let status = wait_with_timeout(&mut child, secs);
    let out = std::process::Output {
        status: status
            .ok_or_else(|| format!("remote command did not finish within {secs}s (killed)"))?,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    };
    Ok(out)
}

/// One ssh invocation through the login chain (key auth first, then
/// sshpass with the password — the same chain as `probe_ssh_login`).
///
/// Returns `Ok(output)` whenever the remote command RAN (any exit code —
/// the caller interprets the exit status). `Err(reason)` only when ssh
/// itself could not start, the login chain was exhausted, or the command
/// outlived [`SSH_COMMAND_TIMEOUT_SECS`]; the reason is an ACTIONABLE hint
/// (missing key auth / sshpass, connection failure) — the classic cause is
/// "no ssh-copy-id yet", which has nothing to do with the DUT or the flash
/// tool.
fn ssh_output(
    host: &str,
    user: &str,
    pass: &str,
    cmd: &str,
) -> Result<std::process::Output, String> {
    use std::process::Command;
    let dest = format!("{user}@{host}");
    let base = [
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=5",
        "-o",
        "StrictHostKeyChecking=accept-new",
    ];
    // Key auth first. A non-zero exit is an auth failure only when ssh
    // itself says so on stderr — a remote command's own non-zero exit
    // (a device-list command's zero-device convention) must NOT trigger the
    // password fallback.
    let first = run_capped(
        {
            let mut c = Command::new("setsid");
            c.arg("ssh").args(base).arg(&dest).arg(cmd);
            c
        },
        SSH_COMMAND_TIMEOUT_SECS,
    );
    let auth_denied = match &first {
        Ok(out) => !out.status.success() && ssh_auth_denied(out),
        Err(_) => true,
    };
    if !auth_denied {
        return first;
    }
    let key_err = match &first {
        Ok(out) => first_stderr_line(out),
        Err(e) => e.clone(),
    };
    if pass.is_empty() {
        return Err(format!(
            "{key_err} — run once on this machine: ssh-copy-id {dest}"
        ));
    }
    let sshpass_ok = Command::new("sshpass")
        .arg("-V")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if sshpass_ok {
        let pwd_base = [
            "-o",
            "BatchMode=no",
            "-o",
            "ConnectTimeout=5",
            "-o",
            "StrictHostKeyChecking=accept-new",
        ];
        return run_capped(
            {
                let mut c = Command::new("setsid");
                c.arg("sshpass")
                    .arg("-p")
                    .arg(pass)
                    .arg("ssh")
                    .args(pwd_base)
                    .arg("-o")
                    .arg("PreferredAuthentications=password")
                    .arg(&dest)
                    .arg(cmd);
                c
            },
            SSH_COMMAND_TIMEOUT_SECS,
        );
    }
    Err(format!(
        "{key_err} — and sshpass is not installed; run once on this machine: \
         ssh-copy-id {dest}"
    ))
}

/// The device-list capture used by the flash probe: non-zero remote exit
/// is an error only when stderr says something — the first stderr line
/// becomes the reason (ssh auth denial, tool errors such as a USB claim
/// failure). A non-zero exit with EMPTY stderr is an empty device list, not
/// a failure: device-list commands differ on this — some print nothing and
/// exit non-zero with zero devices, others print a "connected(0)" style
/// header and exit 0. Login-chain failures carry an ssh-copy-id hint (see
/// [`ssh_output`]).
pub(crate) fn ssh_list_devices_stdout(
    host: &str,
    user: &str,
    pass: &str,
    cmd: &str,
) -> Result<String, String> {
    let out = ssh_output(host, user, pass, cmd)?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if out.status.success() {
        // A help/usage dump is NOT a device list: a tool invoked without
        // its subcommand prints usage lines, which the XOR would happily
        // count as "N devices were ALREADY visible". Fail loudly with the
        // actionable diagnosis instead.
        if looks_like_usage(&stdout) {
            return Err(format!(
                "`{cmd}` printed a HELP/USAGE text ({} lines), not a device list — \
                 the command must LIST devices in flashing mode",
                stdout.lines().count()
            ));
        }
        Ok(stdout)
    } else if !String::from_utf8_lossy(&out.stderr).trim().is_empty() {
        Err(first_stderr_line(&out))
    } else if stdout.trim().is_empty() {
        Ok(String::new())
    } else {
        Err(stdout
            .lines()
            .next()
            .unwrap_or("remote command failed")
            .trim()
            .to_string())
    }
}

pub fn preflight_list_devices(
    host: &str,
    user: &str,
    pass: &str,
    list_cmd: &str,
) -> (ProbeMark, Option<String>) {
    let list = list_cmd.trim().to_string();
    if list.is_empty() {
        return (ProbeMark::Skip, Some("list_devices_cmd is empty".into()));
    }
    match ssh_list_devices_stdout(host, user, pass, &list) {
        Ok(_) => (ProbeMark::Ok, None),
        Err(reason) => (
            ProbeMark::Err,
            Some(format!("list_devices_cmd over SSH failed: {reason}")),
        ),
    }
}

/// Heuristic for help/usage dumps: tool help screens open with a
/// usage/banner line ("Tool Usage", "usage: …", "Help:") — real device
/// listings never contain these.
fn looks_like_usage(stdout: &str) -> bool {
    stdout.contains("Usage") || stdout.contains("usage") || stdout.contains("Help:")
}

/// Cap for waiting on the DUT's serial state after resuming a boot from
/// the U-Boot prompt (or a mid-boot reset): a board takes roughly 20–40 s
/// from power-on to the Linux shell, so 45 s covers slow boards without
/// stretching the TEST spinner into the next probe.
const BOOT_TO_SHELL_WAIT_SECS: u64 = 45;

/// Sleep between device-list polls (and state polls). The
/// `DUTABO_TEST_FAST_FLASH_POLL` env var shrinks it to ~0 so the
/// end-to-end probe tests do not wait out real poll windows — a test-only
/// seam following the house precedent (TARGET_CONF / DUTABO_CONFIG
/// env overrides).
fn poll_interval() -> std::time::Duration {
    if std::env::var_os("DUTABO_TEST_FAST_FLASH_POLL").is_some() {
        std::time::Duration::from_millis(10)
    } else {
        std::time::Duration::from_secs(LIST_POLL_INTERVAL_SECS)
    }
}

/// Current DUT state via the MCP server (`serial_get_state`), or None
/// when no server answered or the payload was unusable.
/// [`mcp_dut_state`] on a caller-supplied runtime: the poll loops below call
/// this up to several times a second against the same server, so they keep
/// one runtime for the whole loop instead of rebuilding per poll.
pub(crate) fn mcp_dut_state_on(rt: &tokio::runtime::Runtime, ports: &[u16]) -> Option<String> {
    let outcome = rt.block_on(super::super::mcp_call(
        "tools/call",
        serde_json::json!({"name": "serial_get_state", "arguments": {}}),
        ports,
    ));
    match outcome {
        super::super::McpOutcome::Result(result) => super::super::tool_payload(&result)
            .ok()
            .and_then(|payload| payload["state"].as_str().map(str::to_string)),
        _ => None,
    }
}

/// Poll `serial_get_state` until the DUT reaches the wanted state, or the
/// window runs out. Used to land the shell-style loader command in a LIVE
/// Linux console — the marker-echo wrapper only works in a shell.
pub(crate) fn wait_for_dut_state(ports: &[u16], wanted: &str, timeout_secs: u64) -> bool {
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return false;
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    while std::time::Instant::now() < deadline {
        if mcp_dut_state_on(&rt, ports).as_deref() == Some(wanted) {
            return true;
        }
        std::thread::sleep(poll_interval());
    }
    mcp_dut_state_on(&rt, ports).as_deref() == Some(wanted)
}

/// Release a held relay button, retrying because a stranded press boots the
/// DUT into download on the next power-on. Returns true when any
/// attempt reached the server and succeeded.
const LIST_POLL_INTERVAL_SECS: u64 = 1;

/// Poll cadence after triggering flashing mode. A board takes several
/// seconds to reboot into its flashing mode (ROM entry plus USB
/// re-enumeration) — a short window silently misses the device and
/// false-fails. A 1 s cadence detects the device sooner and still fits the
/// TEST budget: 15 polls x 1 s = a 15 s window, covering the slowest
/// entry with margin.
const LIST_POLLS: u64 = 15;
const LIST_POLLS_QUICK: u64 = 8;

use crate::tui_init::download::{
    self, DeviceDiff, DownloadContext, DownloadDevice, DownloadProvider, SoftwareStep,
};

/// Outcome of the after-trigger polling loop (provider device diff).
enum PollOutcome {
    /// Exactly one unmatched new device — this DUT.
    Found(DownloadDevice),
    /// Every capture ran cleanly but no new device identity appeared.
    NoChange,
    /// The list command itself failed, or the diff was ambiguous.
    Failed(String),
}

/// Poll the provider-parsed device list until EXACTLY ONE unmatched new
/// identity appears (then re-confirm it once — a flapping enumeration
/// must not fake a pass), the window runs out, or the diff turns
/// ambiguous (>1 new device: never guess which one is this DUT).
fn poll_for_new_device(
    provider: &dyn DownloadProvider,
    ctx: &DownloadContext,
    list_cmd: &str,
    baseline: &[DownloadDevice],
    quick: bool,
) -> PollOutcome {
    let polls = if quick { LIST_POLLS_QUICK } else { LIST_POLLS };
    for _ in 0..polls {
        std::thread::sleep(poll_interval());
        let after = match provider.enumerate(ctx, list_cmd) {
            Ok(after) => after,
            Err(reason) => return PollOutcome::Failed(reason.to_string()),
        };
        match download::diff_devices(baseline, &after) {
            DeviceDiff::Unique(found) => {
                // Confirm the candidate still exists on one more sample
                // before declaring the DUT in download mode.
                std::thread::sleep(poll_interval());
                match provider.enumerate(ctx, list_cmd) {
                    Ok(confirm) => {
                        if matches!(
                            download::diff_devices(baseline, &confirm),
                            DeviceDiff::Unique(_)
                        ) {
                            return PollOutcome::Found(found);
                        }
                    }
                    Err(reason) => return PollOutcome::Failed(reason.to_string()),
                }
            }
            DeviceDiff::Ambiguous(devices) => {
                return PollOutcome::Failed(
                    download::DownloadError::Ambiguous(devices).to_string(),
                );
            }
            DeviceDiff::NoNew => {}
        }
    }
    PollOutcome::NoChange
}

/// The download-mode probe — DownloadProvider driven, platform-agnostic:
///
///   1. resolve the provider + effective commands (the CALLER does this:
///      skill path -> provider; `.target.jsonc` download overlay >
///      provider defaults; no UI fields, no CLI overrides);
///   2. enumerate BEFORE — two short-interval samples build a stable
///      baseline (a flapping list merges into the superset);
///   3. enter download mode: the provider's SOFTWARE plan first (its own
///      command sequence for the current DUT state), then the HARDWARE
///      sandwich when reset+download relay channels exist;
///   4. poll enumerate AFTER and diff PARSED device identities: exactly
///      one unmatched new record is this DUT (PASS with the provider's
///      summary); >1 is AMBIGUOUS (refuse — flashing the wrong board is
///      the failure mode this exists to prevent); 0 until the window
///      ends is FAIL.
#[cfg(test)]
pub fn run_list_devices_probe(target: ProbeTarget) -> (ProbeMark, Option<String>) {
    run_list_devices_probe_profile(target, false)
}

/// TEST-button profile: keep the flash proof inside the shared 60s budget.
pub fn run_list_devices_probe_quick(target: ProbeTarget) -> (ProbeMark, Option<String>) {
    run_list_devices_probe_profile(target, true)
}

fn run_list_devices_probe_profile(target: ProbeTarget, quick: bool) -> (ProbeMark, Option<String>) {
    // The failure detail reports the ACTUAL elapsed time: the boot-resume
    // path stretches the probe well past a single poll window.
    let probe_started = std::time::Instant::now();
    let ProbeTarget::ListDevices {
        host,
        user,
        pass,
        provider_id,
        loader_cmd,
        list_cmd,
        reset_ch,
        download_ch,
        dev_ctrl,
        mcp_ports,
    } = target
    else {
        return (ProbeMark::Skip, None);
    };
    let Some(provider) = download::registry::Registry::get(&provider_id) else {
        return (
            ProbeMark::Skip,
            Some("no download provider for the selected skill path".into()),
        );
    };
    if list_cmd.trim().is_empty() {
        return (
            ProbeMark::Skip,
            Some("no device-list command resolved".into()),
        );
    }
    let ctx = DownloadContext {
        host,
        user,
        pass,
        mcp_ports,
        reset_channel: reset_ch,
        download_channel: download_ch.as_ref().map(|&(channel, _)| channel),
        download_button: download_ch.map(|(_, button)| button).unwrap_or_default(),
        dev_ctrl_enabled: dev_ctrl,
    };
    // ① Baseline: two short-interval samples. Equal sets = stable; a
    // difference merges into the superset (a device that flaps between
    // the samples must not read as "new" after the trigger).
    let first = match provider.enumerate(&ctx, &list_cmd) {
        Ok(set) => set,
        Err(reason) => return (ProbeMark::Err, Some(reason.to_string())),
    };
    let baseline_note;
    let baseline = match provider.enumerate(&ctx, &list_cmd) {
        Ok(second) if second == first => {
            baseline_note = String::new();
            first
        }
        Ok(second) => {
            baseline_note = "baseline was unstable between the two BEFORE samples (merged)".into();
            let mut merged = first;
            for device in second {
                if !merged.iter().any(|d| d.identity == device.identity) {
                    merged.push(device);
                }
            }
            merged
        }
        Err(reason) => return (ProbeMark::Err, Some(reason.to_string())),
    };
    // Why each attempt was skipped — folded into the final failure detail
    // so "no new device" explains itself.
    let mut skipped: Vec<String> = Vec::new();
    // ② Software entry — the provider's own command plan for the current
    // DUT state (e.g. raw at the U-Boot prompt, then the provider's ROM
    // entry + boot-resume + shell; providers may define none).
    let state = ctx.dut_state();
    match loader_cmd
        .as_deref()
        .map(str::trim)
        .filter(|l| !l.is_empty())
    {
        Some(loader_cmd) => {
            for step in provider.software_entry_plan(loader_cmd, state.as_deref()) {
                match step {
                    SoftwareStep::UbootRaw(cmd) => {
                        // A successful trigger reboots the DUT before the
                        // marker returns — send, then poll: the poll is
                        // the truth.
                        ctx.uboot_command(cmd);
                    }
                    SoftwareStep::ShellAfterBoot(cmd) => {
                        // Resume the interrupted boot and wait for the
                        // shell so a shell-style command lands in a live
                        // console.
                        if !ctx.uboot_command("boot")
                            || !ctx.wait_for_state("active", BOOT_TO_SHELL_WAIT_SECS)
                        {
                            skipped.push(
                                "uboot resume: DUT never reached an active shell (login broken?)"
                                    .into(),
                            );
                            continue;
                        }
                        ctx.serial_command(cmd);
                    }
                    SoftwareStep::Shell(cmd) => {
                        // Mid-boot protection: a command sent while the
                        // kernel is still booting is lost.
                        if matches!(state.as_deref(), Some("booting") | Some("connecting"))
                            && !ctx.wait_for_state("active", BOOT_TO_SHELL_WAIT_SECS)
                        {
                            skipped
                                .push("DUT was mid-boot and never reached an active shell".into());
                            continue;
                        }
                        ctx.serial_command(cmd);
                    }
                }
                match poll_for_new_device(provider, &ctx, &list_cmd, &baseline, quick) {
                    PollOutcome::Found(device) => {
                        return (
                            ProbeMark::Ok,
                            Some(found_detail(provider, &device, &baseline_note)),
                        );
                    }
                    PollOutcome::Failed(reason) => return (ProbeMark::Err, Some(reason)),
                    PollOutcome::NoChange => {
                        skipped.push("software entry sent but no new device appeared".into())
                    }
                }
            }
        }
        None => skipped.push(
            "no software entry: the provider has no loader_cmd default and no download overlay"
                .into(),
        ),
    }
    // ③ Hardware entry — only when the physical sandwich (reset key +
    // download key on an enabled relay) exists; the timing is the
    // provider's.
    if ctx.hardware_entry_available() {
        match provider.enter_hardware(&ctx) {
            Err(reason) => skipped.push(format!("hardware entry failed: {reason}")),
            Ok(()) => match poll_for_new_device(provider, &ctx, &list_cmd, &baseline, quick) {
                PollOutcome::Found(device) => {
                    return (
                        ProbeMark::Ok,
                        Some(found_detail(provider, &device, &baseline_note)),
                    );
                }
                PollOutcome::Failed(reason) => return (ProbeMark::Err, Some(reason)),
                PollOutcome::NoChange => {
                    skipped.push("relay sandwich executed but no new device appeared".into())
                }
            },
        }
    } else {
        skipped.push(
            "no hardware entry: needs reset + download relay channels on an enabled dev_ctrl"
                .into(),
        );
    }
    // ④ No unmatched new device on every attempt → FAIL.
    let elapsed = probe_started.elapsed().as_secs();
    let mut detail = format!(
        "no new device identity appeared in `{list_cmd}` after {elapsed}s of trigger attempts — \
         the DUT did not enter download mode"
    );
    if !baseline.is_empty() {
        detail.push_str(&format!(
            " ({} device(s) were already visible: {})",
            baseline.len(),
            baseline
                .iter()
                .map(|d| d.summary.as_str())
                .collect::<Vec<_>>()
                .join(" | ")
        ));
    }
    if !baseline_note.is_empty() {
        detail.push_str(&format!("; {baseline_note}"));
    }
    if !skipped.is_empty() {
        detail.push_str(&format!(" Trigger attempts: {}.", skipped.join("; ")));
    }
    (ProbeMark::Err, Some(detail))
}

/// The PASS detail: the provider summary plus the RAW vendor record (the
/// UI keeps the raw for the device row's detail overlay).
fn found_detail(provider: &dyn DownloadProvider, device: &DownloadDevice, note: &str) -> String {
    let mut detail = format!("{}\n{}", provider.summarize(device), device.raw);
    if !note.is_empty() {
        detail.push_str(&format!("\n{note}"));
    }
    detail
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs the fake-`setsid` tests below mutate the PROCESS-WIDE PATH env var
    /// (each test prepends its own fake dir). The harness may run tests on
    /// parallel threads regardless of `--test-threads`, so every test that
    /// touches PATH takes this lock — otherwise one test's fake `setsid`
    /// leaks into another test's spawn.
    static FAKE_SSH_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    /// Uniquifies each probe test's temp dir — the pid-based dir is shared
    /// across tests in the process, and a parallel test's cleanup deleting
    /// it mid-setup is a real race.
    static PROBE_TEST_SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    // ── End-to-end probe tests (fake MCP server + fake `setsid` ssh) ──

    /// When the fake MCP server makes the DUT's device line appear in the
    /// dev-host list.
    #[derive(Clone, Copy, PartialEq)]
    enum TriggerOn {
        /// `serial_send_command` (the shell-style trigger) is the trigger.
        SendCommand,
        /// The RAW `serial_uboot_command` loader command is the trigger.
        UbootRaw,
        /// No call ever triggers the device — the probe must fall through
        /// every attempt and fail with an empty XOR.
        Never,
    }

    /// Locate the `\r\n\r\n` header/body split in a raw HTTP request.
    fn find_header_end(buf: &[u8]) -> Option<usize> {
        buf.windows(4).position(|w| w == b"\r\n\r\n")
    }

    /// Read one HTTP request body from the socket (headers + Content-Length).
    async fn read_http_body(sock: &mut tokio::net::TcpStream) -> Option<String> {
        use tokio::io::AsyncReadExt;
        let mut buf: Vec<u8> = Vec::new();
        let mut tmp = [0u8; 2048];
        loop {
            if let Some(pos) = find_header_end(&buf) {
                let header_text = String::from_utf8_lossy(&buf[..pos]);
                let content_length = header_text
                    .lines()
                    .find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        key.trim()
                            .eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())?
                    })
                    .unwrap_or(0);
                if buf.len() >= pos + 4 + content_length {
                    let body = String::from_utf8_lossy(&buf[pos + 4..pos + 4 + content_length])
                        .to_string();
                    return Some(body);
                }
            }
            let n = sock.read(&mut tmp).await.ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&tmp[..n]);
        }
    }

    /// Write one HTTP response and close the connection.
    async fn write_http_response(
        sock: &mut tokio::net::TcpStream,
        status: &str,
        extra_headers: &str,
        body: &str,
    ) {
        use tokio::io::AsyncWriteExt;
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n{extra_headers}\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = sock.write_all(response.as_bytes()).await;
        let _ = sock.shutdown().await;
    }

    /// The JSON-RPC `tools/call` result frame carrying `payload` as the
    /// tool's text.
    fn tool_result_frame(payload: serde_json::Value) -> String {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "content": [{"type": "text", "text": payload.to_string()}]
            }
        })
        .to_string()
    }

    /// Fake Streamable-HTTP MCP server: answers `initialize` (with a
    /// session header), `notifications/initialized`, and `tools/call` for
    /// serial_get_state / serial_uboot_command / serial_send_command.
    /// `state` starts at the given DUT state and flips to `active` when
    /// `serial_uboot_command boot` arrives; the trigger file appears when
    /// the configured trigger call arrives.
    struct FakeMcp {
        listener: tokio::net::TcpListener,
        state: std::sync::Arc<std::sync::Mutex<String>>,
        trigger_on: TriggerOn,
        /// serial_uboot_command `boot` answers `success: false` (the
        /// boot-resume path must fall through to the next attempt).
        boot_fails: bool,
        trigger_path: std::path::PathBuf,
        calls: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl FakeMcp {
        async fn bind(
            state: &str,
            trigger_on: TriggerOn,
            boot_fails: bool,
            trigger_path: std::path::PathBuf,
        ) -> FakeMcp {
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .unwrap();
            FakeMcp {
                listener,
                state: std::sync::Arc::new(std::sync::Mutex::new(state.to_string())),
                trigger_on,
                boot_fails,
                trigger_path,
                calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            }
        }

        fn port(&self) -> u16 {
            self.listener.local_addr().unwrap().port()
        }

        /// Serve until the runtime is shut down; returns the shared state
        /// and the captured call log ("tools/call:<tool>:<command>").
        fn spawn(
            self,
            rt: &tokio::runtime::Runtime,
        ) -> (
            std::sync::Arc<std::sync::Mutex<String>>,
            std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        ) {
            let state = self.state.clone();
            let calls = self.calls.clone();
            let state_ret = state.clone();
            let calls_ret = calls.clone();
            rt.spawn(async move {
                loop {
                    let Ok((mut sock, _)) = self.listener.accept().await else {
                        break;
                    };
                    let Some(body) = read_http_body(&mut sock).await else {
                        continue;
                    };
                    let Ok(req) = serde_json::from_str::<serde_json::Value>(&body) else {
                        continue;
                    };
                    let method = req["method"].as_str().unwrap_or("").to_string();
                    let tool = req["params"]["name"].as_str().unwrap_or("").to_string();
                    let command = req["params"]["arguments"]["command"]
                        .as_str()
                        .unwrap_or("")
                        .to_string();
                    let button = req["params"]["arguments"]["button"]
                        .as_str()
                        .unwrap_or("")
                        .to_string();
                    let detail = if tool == "serial_button" {
                        button
                    } else {
                        command.clone()
                    };
                    calls
                        .lock()
                        .unwrap()
                        .push(format!("{method}:{tool}:{detail}"));
                    match (method.as_str(), tool.as_str()) {
                        ("initialize", _) => {
                            write_http_response(
                                &mut sock,
                                "200 OK",
                                "Mcp-Session-Id: s1\r\n",
                                &serde_json::json!({
                                    "jsonrpc": "2.0",
                                    "id": 0,
                                    "result": {
                                        "protocolVersion": "2024-11-05",
                                        "capabilities": {},
                                        "serverInfo": {"name": "fake", "version": "0.0.0"}
                                    }
                                })
                                .to_string(),
                            )
                            .await;
                        }
                        ("notifications/initialized", _) => {
                            write_http_response(&mut sock, "202 Accepted", "", "").await;
                        }
                        ("tools/call", "serial_get_state") => {
                            let payload = serde_json::json!({"state": *state.lock().unwrap()});
                            write_http_response(
                                &mut sock,
                                "200 OK",
                                "",
                                &tool_result_frame(payload),
                            )
                            .await;
                        }
                        ("tools/call", "serial_uboot_command") => {
                            if command == "boot" {
                                if self.boot_fails {
                                    write_http_response(
                                        &mut sock,
                                        "200 OK",
                                        "",
                                        &tool_result_frame(serde_json::json!({
                                            "success": false,
                                            "error": "boot refused (fake)"
                                        })),
                                    )
                                    .await;
                                    continue;
                                }
                                *state.lock().unwrap() = "active".to_string();
                            }
                            if self.trigger_on == TriggerOn::UbootRaw && command != "boot" {
                                std::fs::write(&self.trigger_path, "1").ok();
                            }
                            write_http_response(
                                &mut sock,
                                "200 OK",
                                "",
                                &tool_result_frame(serde_json::json!({
                                    "sent": command,
                                    "output": ""
                                })),
                            )
                            .await;
                        }
                        ("tools/call", "serial_button") => {
                            write_http_response(
                                &mut sock,
                                "200 OK",
                                "",
                                &tool_result_frame(serde_json::json!({"success": true})),
                            )
                            .await;
                        }
                        ("tools/call", "serial_reset") => {
                            write_http_response(
                                &mut sock,
                                "200 OK",
                                "",
                                &tool_result_frame(serde_json::json!({
                                    "success": true,
                                    "wait_boot": false
                                })),
                            )
                            .await;
                        }
                        ("tools/call", "serial_send_command") => {
                            if self.trigger_on == TriggerOn::SendCommand {
                                std::fs::write(&self.trigger_path, "1").ok();
                            }
                            write_http_response(
                                &mut sock,
                                "200 OK",
                                "",
                                &tool_result_frame(serde_json::json!({
                                    "output": "ok",
                                    "exit_code": 0,
                                    "timed_out": false
                                })),
                            )
                            .await;
                        }
                        _ => {
                            write_http_response(
                                &mut sock,
                                "200 OK",
                                "",
                                &tool_result_frame(serde_json::json!({})),
                            )
                            .await;
                        }
                    }
                }
            });
            (state_ret, calls_ret)
        }
    }

    /// Run the real probe end-to-end against a fake MCP server and a fake
    /// `setsid` ssh (whose "remote" `list_devices_cmd` reports the DUT once
    /// the trigger file exists). Returns the probe outcome, the captured
    /// MCP call log, and the temp dir (still on disk until the caller
    /// cleans up).
    fn run_probe_with_fakes(
        state: &str,
        loader_cmd: &str,
        trigger_on: TriggerOn,
        rt: &tokio::runtime::Runtime,
    ) -> ((ProbeMark, Option<String>), Vec<String>, std::path::PathBuf) {
        run_probe_with_fakes_full(state, loader_cmd, trigger_on, false, None, rt)
    }

    /// [`run_probe_with_fakes`] with the full knobs: `boot_fails` makes
    /// the boot-resume answer failure; `download_ch` arms the relay
    /// fallback attempts.
    #[allow(clippy::too_many_arguments)]
    fn run_probe_with_fakes_full(
        state: &str,
        loader_cmd: &str,
        trigger_on: TriggerOn,
        boot_fails: bool,
        download_ch: Option<u8>,
        rt: &tokio::runtime::Runtime,
    ) -> ((ProbeMark, Option<String>), Vec<String>, std::path::PathBuf) {
        use std::os::unix::fs::PermissionsExt;

        let _guard = FAKE_SSH_LOCK.lock().unwrap();
        // Serialize against dutabo.rs's own tests that assert on the
        // process-global MCP session cache contents (SESSION_TEST_MUTEX is
        // a tokio mutex — blocking_lock is fine from a sync test).
        let _session_guard = crate::tests::SESSION_TEST_MUTEX.blocking_lock();
        let seq = PROBE_TEST_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp =
            std::env::temp_dir().join(format!("dutabo-flash-probe-{}-{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let trigger_path = tmp.join("trigger");
        let fake_setsid = tmp.join("setsid");
        std::fs::write(
            &fake_setsid,
            "#!/bin/sh\n\
             if [ -f \"$TRIGGER_FILE\" ]; then\n\
             \x20 printf 'List of rockusb connected(1)\\nDevNo=0\\tVid=0x2207,Pid=0x350b,\
             LocationID=104\\tMode=Loader\\tSerialNo=TESTDUT\\n'\n\
             else\n\
             \x20 printf 'List of rockusb connected(0)\\n'\n\
             fi\n\
             exit 0\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake_setsid, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mcp = rt.block_on(FakeMcp::bind(
            state,
            trigger_on,
            boot_fails,
            trigger_path.clone(),
        ));
        let port = mcp.port();
        let (_, calls) = mcp.spawn(rt);
        // The session cache is process-global: drop any entry from an
        // earlier test so this probe starts a fresh handshake.
        *crate::MCP_SESSION.lock().unwrap() = None;

        let old_path = std::env::var_os("PATH").unwrap_or_default();
        let new_path = std::env::join_paths(
            std::iter::once(tmp.clone()).chain(std::env::split_paths(&old_path)),
        )
        .unwrap();
        // SAFETY: single-threaded tests (--test-threads=1) and FAKE_SSH_LOCK;
        // restored before any assertion can panic.
        unsafe { std::env::set_var("PATH", new_path) };
        unsafe { std::env::set_var("TRIGGER_FILE", &trigger_path) };
        unsafe { std::env::set_var("DUTABO_TEST_FAST_FLASH_POLL", "1") };
        let target = ProbeTarget::ListDevices {
            host: "192.0.2.1".into(),
            user: "linaro".into(),
            pass: String::new(),
            provider_id: "rockchip".into(),
            list_cmd: "upgrade_tool ld".into(),
            loader_cmd: Some(loader_cmd.into()),
            reset_ch: download_ch.is_some().then_some(1),
            download_ch: download_ch.map(|ch| (ch, "download".to_string())),
            dev_ctrl: download_ch.is_some(),
            mcp_ports: vec![port],
        };
        let result = run_list_devices_probe(target);
        unsafe { std::env::set_var("PATH", old_path) };
        unsafe { std::env::remove_var("TRIGGER_FILE") };
        unsafe { std::env::remove_var("DUTABO_TEST_FAST_FLASH_POLL") };
        (result, calls.lock().unwrap().clone(), tmp)
    }

    /// THE reported regression: the U-Boot interrupt probe ran earlier in
    /// the same TEST and left the DUT AT the U-Boot prompt; the shell-style
    /// loader_cmd (`reboot loader`) is an "Unknown command" there. The
    /// probe must resume the boot (`boot`), wait for the Linux shell, THEN
    /// send the shell trigger.
    #[test]
    fn probe_recovers_from_uboot_prompt_before_shell_loader_cmd() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let ((mark, detail), calls, tmp) =
            run_probe_with_fakes("uboot", "reboot loader", TriggerOn::SendCommand, &rt);
        let _ = std::fs::remove_dir_all(&tmp);
        rt.shutdown_timeout(std::time::Duration::from_secs(1));

        assert_eq!(
            mark,
            ProbeMark::Ok,
            "shell loader_cmd after a U-Boot prompt must still pass; detail={detail:?} calls={calls:?}"
        );
        let detail = detail.unwrap_or_default();
        assert!(detail.contains("TESTDUT"), "device line missing: {detail}");
        let raw_idx = calls
            .iter()
            .position(|c| c == "tools/call:serial_uboot_command:reboot loader");
        let boot_idx = calls
            .iter()
            .position(|c| c == "tools/call:serial_uboot_command:boot");
        let send_idx = calls
            .iter()
            .position(|c| c == "tools/call:serial_send_command:reboot loader");
        assert!(
            raw_idx.is_some(),
            "the loader_cmd must be tried RAW at the prompt first; calls={calls:?}"
        );
        assert!(
            boot_idx.is_some(),
            "must resume the boot from the U-Boot prompt; calls={calls:?}"
        );
        assert!(
            send_idx.is_some(),
            "must send the shell loader_cmd after the boot; calls={calls:?}"
        );
        assert!(
            raw_idx.unwrap() < boot_idx.unwrap() && boot_idx.unwrap() < send_idx.unwrap(),
            "order must be raw → boot → shell trigger; calls={calls:?}"
        );
    }

    /// The normal case: the DUT is already in Linux (state `active`) — the
    /// shell loader_cmd is sent directly, no U-Boot dance.
    #[test]
    fn probe_sends_shell_loader_cmd_directly_from_active_state() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let ((mark, detail), calls, tmp) =
            run_probe_with_fakes("active", "reboot loader", TriggerOn::SendCommand, &rt);
        let _ = std::fs::remove_dir_all(&tmp);
        rt.shutdown_timeout(std::time::Duration::from_secs(1));

        assert_eq!(
            mark,
            ProbeMark::Ok,
            "shell loader_cmd from a live Linux shell must pass; detail={detail:?} calls={calls:?}"
        );
        assert!(
            detail
                .as_ref()
                .unwrap_or(&String::new())
                .contains("TESTDUT"),
            "device line missing: {detail:?}"
        );
        assert!(
            !calls
                .iter()
                .any(|c| c.starts_with("tools/call:serial_uboot_command")),
            "no U-Boot dance needed from active; calls={calls:?}"
        );
        assert!(
            calls.contains(&"tools/call:serial_send_command:reboot loader".to_string()),
            "shell trigger must be sent; calls={calls:?}"
        );
    }

    /// A U-Boot-style loader_cmd (`db loader.bin`) IS valid at the prompt:
    /// the probe tries it RAW first and passes without the boot-resume.
    #[test]
    fn probe_tries_uboot_style_loader_cmd_raw_at_the_prompt() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let ((mark, detail), calls, tmp) =
            run_probe_with_fakes("uboot", "db loader.bin", TriggerOn::UbootRaw, &rt);
        let _ = std::fs::remove_dir_all(&tmp);
        rt.shutdown_timeout(std::time::Duration::from_secs(1));

        assert_eq!(
            mark,
            ProbeMark::Ok,
            "U-Boot-style loader_cmd at the prompt must pass; detail={detail:?} calls={calls:?}"
        );
        assert!(
            detail
                .as_ref()
                .unwrap_or(&String::new())
                .contains("TESTDUT"),
            "device line missing: {detail:?}"
        );
        assert!(
            calls.contains(&"tools/call:serial_uboot_command:db loader.bin".to_string()),
            "the loader_cmd must be tried RAW at the prompt; calls={calls:?}"
        );
        assert!(
            !calls.contains(&"tools/call:serial_uboot_command:boot".to_string()),
            "raw trigger succeeded — no boot-resume needed; calls={calls:?}"
        );
        assert!(
            !calls
                .iter()
                .any(|c| c.starts_with("tools/call:serial_send_command")),
            "a U-Boot command must never be shell-wrapped; calls={calls:?}"
        );
    }

    /// A trigger that never fires must fall THROUGH the shell attempt into
    /// the relay sandwich (press the download key + reset + release)
    /// and only then fail — the marker echo completing is NOT proof the
    /// DUT entered flashing mode.
    #[test]
    fn probe_no_change_falls_through_to_relay_attempts() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let ((mark, detail), calls, tmp) = run_probe_with_fakes_full(
            "active",
            "reboot loader",
            TriggerOn::Never,
            false,
            Some(2),
            &rt,
        );
        let _ = std::fs::remove_dir_all(&tmp);
        rt.shutdown_timeout(std::time::Duration::from_secs(1));

        assert_eq!(
            mark,
            ProbeMark::Err,
            "a never-firing trigger must fail after ALL attempts; detail={detail:?} calls={calls:?}"
        );
        assert!(
            detail
                .as_ref()
                .unwrap_or(&String::new())
                .contains("no new device identity appeared"),
            "failure detail must explain the empty diff: {detail:?}"
        );
        assert!(
            calls.contains(&"tools/call:serial_button:download".to_string()),
            "the download sandwich must be attempted; calls={calls:?}"
        );
        assert!(
            calls.contains(&"tools/call:serial_reset:".to_string()),
            "the relay fallback pulses reset; calls={calls:?}"
        );
    }

    /// A failed boot-resume (`serial_uboot_command boot` answers
    /// failure) must fall through to the next attempt instead of sending
    /// the shell trigger into a dead console.
    #[test]
    fn probe_boot_failure_falls_through() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let ((mark, detail), calls, tmp) = run_probe_with_fakes_full(
            "uboot",
            "reboot loader",
            TriggerOn::SendCommand,
            true,
            None,
            &rt,
        );
        let _ = std::fs::remove_dir_all(&tmp);
        rt.shutdown_timeout(std::time::Duration::from_secs(1));

        assert_eq!(
            mark,
            ProbeMark::Err,
            "a failed boot-resume must end in a loud failure, not a shell trigger; \
             detail={detail:?} calls={calls:?}"
        );
        assert!(
            calls.contains(&"tools/call:serial_uboot_command:boot".to_string()),
            "boot must be attempted; calls={calls:?}"
        );
        assert!(
            !calls
                .iter()
                .any(|c| c.starts_with("tools/call:serial_send_command")),
            "no shell trigger after a failed boot-resume; calls={calls:?}"
        );
    }

    fn devices(stdout: &str) -> Vec<crate::tui_init::download::DownloadDevice> {
        let provider = crate::tui_init::download::rockchip_for_tests();
        provider.parse_devices(stdout)
    }

    /// The lab-wall scenario: dozens of SAME-MODEL devices whose lines
    /// differ only in numbers. Identity is the vendor SerialNo and the
    /// diff counts records (never set-collapses them): the 39th device
    /// is the addition.
    #[test]
    fn parsed_diff_detects_addition_among_identical_devices() {
        let device = |dev: usize, serial: &str| {
            format!(
                "DevNo={dev}\tVid=0x2207,Pid=0x350e,LocationID={}\tMode=Loader\tSerialNo={serial}",
                100 + dev
            )
        };
        let mut old_lines: Vec<String> = (1..=38)
            .map(|d| device(d, &format!("serial{d:04}")))
            .collect();
        old_lines.insert(0, "List of rockusb connected(38)".into());
        let mut new_lines = old_lines.clone();
        new_lines[0] = "List of rockusb connected(39)".into();
        let newcomer = device(39, "serial9999");
        new_lines.push(newcomer.clone());

        let before = devices(&old_lines.join("\n"));
        let after = devices(&new_lines.join("\n"));
        assert_eq!(before.len(), 38, "headers never enter the device set");
        match crate::tui_init::download::diff_devices(&before, &after) {
            crate::tui_init::download::DeviceDiff::Unique(d) => {
                assert_eq!(d.raw, newcomer, "exactly the new device is reported")
            }
            other => panic!("expected unique, got {other:?}"),
        }
    }

    /// Re-enumeration that only SHIFTS numbers (same device, new DevNo /
    /// LocationID) must not be reported as an addition: SerialNo is the
    /// identity, DevNo is not.
    #[test]
    fn parsed_diff_ignores_pure_renumbering() {
        let old = "DevNo=1\tSerialNo=aaa\nDevNo=2\tSerialNo=bbb\n";
        let new = "DevNo=5\tSerialNo=aaa\nDevNo=6\tSerialNo=bbb\n";
        assert_eq!(
            crate::tui_init::download::diff_devices(&devices(old), &devices(new)),
            crate::tui_init::download::DeviceDiff::NoNew
        );
    }

    /// Real `upgrade_tool ld` layouts: another board already in Loader
    /// mode appears in BOTH captures and cancels; only this DUT's new
    /// record remains. The count header growth never becomes a device.
    #[test]
    fn parsed_diff_rockusb_other_board_cancels() {
        let old = "List of rockusb connected(1)\n\
                   DevNo=1\tVid=0x2207,Pid=0x3505,LocationID=101\tMode=Loader\tSerialNo=other-board\n";
        let new = "List of rockusb connected(2)\n\
                   DevNo=1\tVid=0x2207,Pid=0x3505,LocationID=101\tMode=Loader\tSerialNo=other-board\n\
                   DevNo=2\tVid=0x2207,Pid=0x350e,LocationID=113\tMode=Loader\tSerialNo=7208c83dd51a8ec8\n";
        match crate::tui_init::download::diff_devices(&devices(old), &devices(new)) {
            crate::tui_init::download::DeviceDiff::Unique(d) => assert_eq!(
                d.identity,
                crate::tui_init::download::DeviceIdentity::Stable("7208c83dd51a8ec8".into())
            ),
            other => panic!("expected unique, got {other:?}"),
        }
    }

    /// Real `sunxi-fel --list --verbose` layout (Allwinner): a foreign
    /// board already in FEL mode cancels; this DUT's new record remains.
    #[test]
    fn parsed_diff_sunxi_fel_format() {
        let sunxi = |stdout: &str| {
            crate::tui_init::download::allwinner::parse_sunxi_devices_for_tests()(stdout)
        };
        let old = "USB device 001:004   Allwinner A64    \n";
        let new = "USB device 001:004   Allwinner A64    \n\
                   USB device 001:007   Allwinner D1     SID: f0d51ba:c11c0000:00000000:00000000\n";
        match crate::tui_init::download::diff_devices(&sunxi(old), &sunxi(new)) {
            crate::tui_init::download::DeviceDiff::Unique(d) => assert_eq!(
                d.identity,
                crate::tui_init::download::DeviceIdentity::Stable(
                    "f0d51ba:c11c0000:00000000:00000000".into()
                )
            ),
            other => panic!("expected unique, got {other:?}"),
        }
    }

    /// The classic re-image / fresh-workstation failure: ssh key auth
    /// denied, no sshpass. The probe must surface an ACTIONABLE hint
    /// (ssh-copy-id), not a bare "over SSH failed".
    #[test]
    fn ssh_auth_failure_hint_mentions_ssh_copy_id() {
        use std::os::unix::fs::PermissionsExt;

        let _guard = FAKE_SSH_LOCK.lock().unwrap();

        let tmp = std::env::temp_dir().join(format!("dutabo-fake-setsid-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let fake = tmp.join("setsid");
        std::fs::write(
            &fake,
            "#!/bin/sh\n\
             echo 'linaro@192.0.2.1: Permission denied (publickey,password).' >&2\n\
             exit 255\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

        let old_path = std::env::var_os("PATH").unwrap_or_default();
        let new_path = std::env::join_paths(
            std::iter::once(tmp.clone()).chain(std::env::split_paths(&old_path)),
        )
        .unwrap();
        // SAFETY: this crate's tests run single-threaded (--test-threads=1);
        // PATH is restored before any assertion can panic.
        unsafe { std::env::set_var("PATH", new_path) };
        let result = ssh_list_devices_stdout("192.0.2.1", "linaro", "", "upgrade_tool ld");
        unsafe { std::env::set_var("PATH", old_path) };
        let _ = std::fs::remove_dir_all(&tmp);

        match result {
            Err(msg) => {
                assert!(
                    msg.contains("ssh-copy-id linaro@192.0.2.1"),
                    "hint missing from: {msg}"
                );
                assert!(
                    msg.contains("Permission denied"),
                    "reason missing from: {msg}"
                );
            }
            Ok(_) => panic!("fake setsid should have failed key auth"),
        }
    }

    /// When a password IS configured but sshpass is absent, the hint must
    /// still point at the stable fix (key auth), not dead-end at sshpass.
    #[test]
    fn ssh_auth_failure_with_pass_but_no_sshpass_still_hints_copy_id() {
        use std::os::unix::fs::PermissionsExt;

        let _guard = FAKE_SSH_LOCK.lock().unwrap();

        let tmp = std::env::temp_dir().join(format!("dutabo-fake-ssh-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let fake = tmp.join("setsid");
        std::fs::write(
            &fake,
            "#!/bin/sh\n\
             echo 'linaro@192.0.2.1: Permission denied (publickey).' >&2\n\
             exit 255\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

        let old_path = std::env::var_os("PATH").unwrap_or_default();
        let new_path = std::env::join_paths(
            std::iter::once(tmp.clone()).chain(std::env::split_paths(&old_path)),
        )
        .unwrap();
        // SAFETY: single-threaded tests; restored before assertions.
        unsafe { std::env::set_var("PATH", new_path) };
        let result = ssh_list_devices_stdout("192.0.2.1", "linaro", "secret", "upgrade_tool ld");
        unsafe { std::env::set_var("PATH", old_path) };
        let _ = std::fs::remove_dir_all(&tmp);

        match result {
            Err(msg) => {
                assert!(msg.contains("ssh-copy-id"), "hint missing from: {msg}");
                assert!(msg.contains("sshpass"), "sshpass note missing from: {msg}");
            }
            Ok(_) => panic!("fake setsid should have failed key auth"),
        }
    }

    /// `sunxi-fel list` with zero FEL devices exits 1 and prints NOTHING
    /// (no stderr) — the probe must read that as an empty device list,
    /// not an ssh/tool failure.
    #[test]
    fn list_command_nonzero_exit_with_empty_stderr_is_no_devices() {
        use std::os::unix::fs::PermissionsExt;

        let _guard = FAKE_SSH_LOCK.lock().unwrap();

        let tmp = std::env::temp_dir().join(format!("dutabo-fake-sunxi-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let fake = tmp.join("setsid");
        std::fs::write(&fake, "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

        let old_path = std::env::var_os("PATH").unwrap_or_default();
        let new_path = std::env::join_paths(
            std::iter::once(tmp.clone()).chain(std::env::split_paths(&old_path)),
        )
        .unwrap();
        // SAFETY: single-threaded tests; restored before assertions.
        unsafe { std::env::set_var("PATH", new_path) };
        let result = ssh_list_devices_stdout("192.0.2.1", "linaro", "", "sunxi-fel list");
        unsafe { std::env::set_var("PATH", old_path) };
        let _ = std::fs::remove_dir_all(&tmp);

        assert_eq!(
            result,
            Ok(String::new()),
            "empty stderr + non-zero exit = no devices in flashing mode"
        );
    }

    #[test]
    fn list_command_nonzero_exit_with_stdout_surfaces_command_error() {
        use std::os::unix::fs::PermissionsExt;

        let _guard = FAKE_SSH_LOCK.lock().unwrap();
        let tmp =
            std::env::temp_dir().join(format!("dutabo-fake-invalid-list-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let fake = tmp.join("setsid");
        std::fs::write(
            &fake,
            "#!/bin/sh\necho 'command is invalid, check usage'\nexit 255\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

        let old_path = std::env::var_os("PATH").unwrap_or_default();
        let new_path = std::env::join_paths(
            std::iter::once(tmp.clone()).chain(std::env::split_paths(&old_path)),
        )
        .unwrap();
        unsafe { std::env::set_var("PATH", new_path) };
        let result = preflight_list_devices("192.0.2.1", "linaro", "", "upgrade_tool ld");
        unsafe { std::env::set_var("PATH", old_path) };
        let _ = std::fs::remove_dir_all(&tmp);

        assert_eq!(result.0, ProbeMark::Err);
        assert!(
            result
                .1
                .as_deref()
                .is_some_and(|detail| detail.contains("command is invalid"))
        );
    }

    /// A flash tool that fails on stderr (upgrade_tool's
    /// usb_claim_interface / permission errors) must still surface as an
    /// error, never as "no devices".
    #[test]
    fn list_command_real_error_on_stderr_surfaces() {
        use std::os::unix::fs::PermissionsExt;

        let _guard = FAKE_SSH_LOCK.lock().unwrap();

        let tmp = std::env::temp_dir().join(format!("dutabo-fake-tool-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let fake = tmp.join("setsid");
        std::fs::write(
            &fake,
            "#!/bin/sh\n\
             echo 'usb_claim_interface failed for 2207:350e: Operation not permitted' >&2\n\
             exit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

        let old_path = std::env::var_os("PATH").unwrap_or_default();
        let new_path = std::env::join_paths(
            std::iter::once(tmp.clone()).chain(std::env::split_paths(&old_path)),
        )
        .unwrap();
        // SAFETY: single-threaded tests; restored before assertions.
        unsafe { std::env::set_var("PATH", new_path) };
        let result = ssh_list_devices_stdout("192.0.2.1", "linaro", "", "upgrade_tool ld");
        unsafe { std::env::set_var("PATH", old_path) };
        let _ = std::fs::remove_dir_all(&tmp);

        match result {
            Err(msg) => {
                assert!(msg.contains("usb_claim_interface failed"), "got: {msg}");
            }
            Ok(_) => panic!("stderr error must surface, not read as no devices"),
        }
    }

    // ── wait_with_timeout: real-subprocess cap semantics ─────────────────

    /// A child that finishes well inside the cap returns its exit status.
    #[test]
    fn wait_with_timeout_returns_status_for_prompt_child() {
        use std::process::Command;
        let mut child = Command::new("sh").args(["-c", "exit 0"]).spawn().unwrap();
        let status = wait_with_timeout(&mut child, 10).expect("finishes in time");
        assert!(status.success());
    }

    /// A child that outlives the cap is killed and reaped: `None` comes
    /// back and the call returns promptly instead of parking the probe
    /// thread forever (the hung-remote-command leak).
    #[test]
    fn wait_with_timeout_kills_a_hung_child() {
        use std::process::Command;
        let mut child = Command::new("sh").args(["-c", "sleep 30"]).spawn().unwrap();
        let started = std::time::Instant::now();
        let status = wait_with_timeout(&mut child, 1);
        let elapsed = started.elapsed();
        assert!(status.is_none(), "capped child reports None");
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "cap must fire near 1s, took {elapsed:?}"
        );
        // The child was reaped by the kill path — a second wait must not
        // see a zombie.
        assert!(child.wait().is_ok());
    }
}
