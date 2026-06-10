//! DownloadProvider — platform download-mode entry + device enumeration.
//!
//! Registry + Strategy + Adapter: the TEST flow stays platform-agnostic;
//! every platform difference (default commands, device-list parsing,
//! hardware/software entry sequences) lives inside a provider
//! (`rockchip`, `allwinner`, …). New platforms register a provider — the
//! main flow never grows a platform `if`.
pub mod allwinner;
pub mod diff;
mod provider;
pub mod registry;
pub mod resolver;
mod rockchip;

// The re-exports below are consumed by the TEST flow once it resolves a
// provider; until then `unused` lints would fire on a bare `pub use` in
// a bin crate. Allow that explicitly.
#[allow(unused_imports)]
pub use diff::{DeviceDiff, diff_devices};
#[allow(unused_imports)]
pub use provider::{
    DeviceIdentity, DownloadContext, DownloadDevice, DownloadError, DownloadProvider, SoftwareStep,
};
#[allow(unused_imports)]
pub use resolver::resolve_provider;

#[cfg(test)]
pub use rockchip::rockchip_for_tests;
