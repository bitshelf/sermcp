//! Allwinner download provider — FEL via `sunxi-fel`.
//!
//! Enumeration uses `sunxi-fel --list --verbose` (a LIST of devices);
//! `sunxi-fel info` addresses ONE device and is never used here.
//! Identity prefers the SID (a fuse-stable id); USB bus/device numbers
//! shift on re-enumeration and are only a composite fallback.
//!
//! Software entry: none (FEL has no in-shell entry) — hardware entry or
//! SKIP.
use super::diff::DeviceIdentity;
use super::provider::{
    DownloadContext, DownloadDevice, DownloadError, DownloadProvider, SoftwareStep,
};

pub struct AllwinnerDownloadProvider;

impl DownloadProvider for AllwinnerDownloadProvider {
    fn id(&self) -> &'static str {
        "allwinner"
    }

    fn default_loader_cmd(&self) -> Option<&'static str> {
        None
    }

    fn default_list_devices_cmd(&self) -> &'static str {
        "sunxi-fel --list --verbose"
    }

    fn parse_devices(&self, stdout: &str) -> Vec<DownloadDevice> {
        parse_sunxi_devices(stdout)
    }

    fn software_entry_plan<'a>(
        &self,
        _loader_cmd: &'a str,
        _state: Option<&str>,
    ) -> Vec<SoftwareStep<'a>> {
        // FEL is entered with the FEL key held during reset; there is no
        // reliable in-shell command.
        Vec::new()
    }

    /// Hardware entry: hold the FEL key, pulse reset, release.
    fn enter_hardware(&self, ctx: &DownloadContext) -> Result<(), DownloadError> {
        ctx.press_download();
        ctx.pulse_reset();
        if !ctx.release_download() {
            return Err(DownloadError::HostCommand(
                "FEL-key release FAILED after retries — the key may still be pressed; \
                 release it before the next power-on"
                    .into(),
            ));
        }
        Ok(())
    }

    fn summarize(&self, dev: &DownloadDevice) -> String {
        format!("FEL / {}", dev.summary)
    }
}

/// Parse `sunxi-fel --list --verbose` output. One record per
/// `USB device` line; the identity is the `SID`/`sid` token when
/// present, else a composite of the SoC name + bus/device (best effort —
/// the SID is the stable key).
pub fn parse_sunxi_devices(stdout: &str) -> Vec<DownloadDevice> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let line = line.trim_end_matches('\r').trim();
        if !line.starts_with("USB device") {
            continue;
        }
        // Token layout: `USB device <bus:dev> <SoC name…> [SID: <sid>]`.
        // The SID value is a SEPARATE token (or attached with `SID=`).
        let tokens: Vec<&str> = line.split_whitespace().collect();
        let sid_at = tokens
            .iter()
            .position(|t| *t == "SID:" || t.starts_with("SID="));
        let sid = sid_at
            .and_then(|at| {
                tokens[at]
                    .strip_prefix("SID=")
                    .filter(|v| !v.is_empty())
                    .map(str::to_string)
                    .or_else(|| tokens.get(at + 1).map(|v| v.trim_matches(',').to_string()))
            })
            .filter(|v| !v.is_empty());
        let soc = tokens
            .get(3..sid_at.unwrap_or(tokens.len()))
            .map(|slice| slice.join(" "))
            .unwrap_or_default();
        let location = tokens.get(2).copied().unwrap_or_default().to_string();
        let (identity, summary) = match sid {
            Some(sid) => (DeviceIdentity::Stable(sid.clone()), format!("{soc} {sid}")),
            None => (
                DeviceIdentity::Composite(vec![soc.clone(), location.clone()]),
                soc,
            ),
        };
        out.push(DownloadDevice {
            raw: line.to_string(),
            identity,
            summary,
        });
    }
    out
}

/// Test seam: the parse entry point used by flash.rs's diff tests.
#[cfg(test)]
pub fn parse_sunxi_devices_for_tests() -> fn(&str) -> Vec<DownloadDevice> {
    parse_sunxi_devices
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_usb_device_lines_with_sid_identity() {
        let stdout = "\
USB device 002:005   Allwinner T527   SID: 82c0000355120484
USB device 002:007   Allwinner T527   SID: 82c0000355120499
";
        let devices = parse_sunxi_devices(stdout);
        assert_eq!(devices.len(), 2);
        assert_eq!(
            devices[0].identity,
            DeviceIdentity::Stable("82c0000355120484".into())
        );
        assert_eq!(devices[0].summary, "Allwinner T527 82c0000355120484");
        let before = devices[..1].to_vec();
        assert!(matches!(
            super::super::diff_devices(&before, &devices),
            super::super::DeviceDiff::Unique(_)
        ));
    }

    #[test]
    fn non_usb_lines_are_never_devices() {
        assert!(parse_sunxi_devices("no devices found\n\n").is_empty());
        assert!(
            parse_sunxi_devices("Allwinner FEL utility\nUSB device 003:001 F1C100s\n").len() == 1
        );
    }
}
