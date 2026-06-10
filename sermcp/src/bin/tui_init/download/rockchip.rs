//! Rockchip download provider — rockusb via `upgrade_tool`.
//!
//! Defaults (never shown in the UI; `.target.jsonc` `download.*`
//! overlays them):
//!   loader_cmd       = `reboot loader`
//!   list_devices_cmd = `upgrade_tool ld`
//!
//! Device records: lines carrying `DevNo=`; identity prefers `SerialNo`
//! (`DevNo` is an enumeration index — it shifts when a neighbour board
//! unplugs, so it never identifies a device alone). Headers like
//! `List of rockusb connected(N)` are dropped before the device set.
use super::diff::DeviceIdentity;
use super::provider::{
    DownloadContext, DownloadDevice, DownloadError, DownloadProvider, SoftwareStep,
};

pub struct RockchipDownloadProvider;

impl DownloadProvider for RockchipDownloadProvider {
    fn id(&self) -> &'static str {
        "rockchip"
    }

    fn default_loader_cmd(&self) -> Option<&'static str> {
        Some("reboot loader")
    }

    fn default_list_devices_cmd(&self) -> &'static str {
        "upgrade_tool ld"
    }

    fn parse_devices(&self, stdout: &str) -> Vec<DownloadDevice> {
        parse_rockchip_devices(stdout)
    }

    /// Software entry plan for `reboot loader` — a SHELL command that
    /// no-ops at the U-Boot prompt ("Unknown command"). AT the prompt the
    /// configured loader_cmd is tried RAW first (a U-Boot-style command
    /// like `db {loader}` switches the DUT within seconds), then `rbrom`
    /// (the Rockchip reset-to-download command) so the path stays usable
    /// when loader_cmd only works in Linux AND auto-login is broken;
    /// finally the boot is resumed and the shell-style command sent.
    fn software_entry_plan<'a>(
        &self,
        loader_cmd: &'a str,
        state: Option<&str>,
    ) -> Vec<SoftwareStep<'a>> {
        match state {
            Some("uboot") => vec![
                SoftwareStep::UbootRaw(loader_cmd),
                SoftwareStep::UbootRaw("rbrom"),
                SoftwareStep::ShellAfterBoot(loader_cmd),
            ],
            // Mid-boot (an earlier probe reset the DUT): the shell must
            // be awaited so the command lands in a live console.
            Some("booting") | Some("connecting") => vec![SoftwareStep::ShellAfterBoot(loader_cmd)],
            _ => vec![SoftwareStep::Shell(loader_cmd)],
        }
    }

    /// Hardware entry: hold the download (download/FEL) key, pulse reset,
    /// release — the DUT re-enters boot with the key held and parks in
    /// download/loader mode. The release is safety-critical (a stranded
    /// press traps the board in download on the next power-on), so a
    /// failure is reported, never swallowed.
    fn enter_hardware(&self, ctx: &DownloadContext) -> Result<(), DownloadError> {
        ctx.press_download();
        ctx.pulse_reset();
        if !ctx.release_download() {
            return Err(DownloadError::HostCommand(
                "download-key release FAILED after retries — the key may still be pressed; \
                 release it before the next power-on"
                    .into(),
            ));
        }
        Ok(())
    }

    fn summarize(&self, dev: &DownloadDevice) -> String {
        format!("Loader / {}", dev.summary)
    }
}

/// Parse `upgrade_tool ld` output into device records. Every
/// non-`DevNo=` line — headers, counters, blank lines — is dropped: a
/// `connected(0)` → `connected(1)` change must never masquerade as a
/// device.
pub fn parse_rockchip_devices(stdout: &str) -> Vec<DownloadDevice> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let line = line.trim_end_matches('\r').trim();
        if !line.contains("DevNo=") {
            continue;
        }
        let field = |name: &str| -> Option<String> {
            line.split_whitespace()
                .find_map(|token| token.strip_prefix(name).map(|v| v.trim().to_string()))
                .filter(|v| !v.is_empty())
        };
        let dev_no = field("DevNo=");
        let serial = field("SerialNo=");
        let (identity, summary) = match &serial {
            Some(serial) => (DeviceIdentity::Stable(serial.clone()), serial.clone()),
            None => {
                let composite = vec![
                    dev_no.clone().unwrap_or_default(),
                    field("Chip=").unwrap_or_default(),
                    field("Version=").unwrap_or_default(),
                ];
                let summary = format!("DevNo {}", dev_no.unwrap_or_default());
                (DeviceIdentity::Composite(composite), summary)
            }
        };
        out.push(DownloadDevice {
            raw: line.to_string(),
            identity,
            summary,
        });
    }
    out
}

/// Test seam: the concrete provider (the bin crate has no re-export of
/// the type outside this module tree).
#[cfg(test)]
pub fn rockchip_for_tests() -> RockchipDownloadProvider {
    RockchipDownloadProvider
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_devno_records_and_ignores_headers() {
        let before = "List of rockusb connected(0)\n\n";
        assert!(parse_rockchip_devices(before).is_empty());
        let after = "\
List of rockusb connected(1)
DevNo=0 Vid=0x2207 Pid=0x350a LocationID=0a SerialNo=720812345678904 Luckychip
";
        let devices = parse_rockchip_devices(after);
        assert_eq!(devices.len(), 1);
        assert_eq!(
            devices[0].identity,
            DeviceIdentity::Stable("720812345678904".into())
        );
        assert_eq!(devices[0].summary, "720812345678904");
        assert_eq!(devices[0].raw, after.lines().nth(1).unwrap().trim());
        // The header count growth cancels: same record set plus one.
        let two = format!("{after}DevNo=1 Vid=0x2207 Pid=0x350a SerialNo=AAA Luckychip\n");
        let before_records = parse_rockchip_devices(after);
        let after_records = parse_rockchip_devices(&two);
        assert!(matches!(
            super::super::diff_devices(&before_records, &after_records),
            super::super::DeviceDiff::Unique(_)
        ));
    }

    #[test]
    fn serial_missing_falls_back_to_composite_identity() {
        let stdout = "DevNo=0 Chip=RK3576 Version=0.1\n";
        let devices = parse_rockchip_devices(stdout);
        assert_eq!(
            devices[0].identity,
            DeviceIdentity::Composite(vec!["0".into(), "RK3576".into(), "0.1".into()])
        );
        assert_eq!(devices[0].summary, "DevNo 0");
    }
}
