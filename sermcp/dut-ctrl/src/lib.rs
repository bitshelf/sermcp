//! Power control abstraction — trait + implementations.
//!
//! Provides a unified interface for controlling DUT power/reset/download across
//! different hardware backends (CH340 relay, SNMP PDU, software reboot, GPIO
//! button simulation).
//!
//! # Design
//!
//! The `PowerControl` trait abstracts physical button operations (press/release)
//! so the rest of the system never needs to know whether a "reset" is done via
//! a CH340 relay, an SNMP PDU, or a `reboot` command over serial.
//!
//! # Implementations
//!
//! - `Ch340RelayControl`: 4-byte TCP protocol via ser2net, with read-back verify.
//! - `SnmpPduControl`: future — SNMP v3 SET/GET on OID.
//! - `ButtonSimControl`: future — GPIO line control via /dev/gpiochipN.
//!
//! The backend is PLUGGABLE (check): every consumer talks to the
//! `PowerControl` trait through the `PowerControlBackend` enum, and the enum's
//! `from_config` maps the `.target.jsonc` `relay.type` string to a concrete
//! implementation. Adding a future backend (SNMP PDU, GPIO, ...) is one new
//! enum variant + one `from_config` arm + one `supports` arm — nothing in the
//! engine or MCP layer changes. The only remaining selectable backends are
//! `ch340-relay` and `none` (the usb-relay alias and the external dev-ctl
//! program backend were removed).

use std::time::Duration;

#[cfg(feature = "ch340")]
mod ch340;
#[cfg(feature = "ch340")]
pub use ch340::Ch340RelayControl;
// Raw 4-byte protocol primitives (re-exported for direct-cycle probes in
// the `dutabo init` TEST button, which drives the relay WITHOUT the async
// control stack).
pub use ch340::{
    HEADER as RELAY_HEADER, OP_OFF as RELAY_OP_OFF, OP_ON as RELAY_OP_ON,
    OP_STATUS as RELAY_OP_STATUS, packet as relay_packet,
};

/// Physical buttons that can be pressed/released.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Button {
    Reset,
    Download,
    Power,
}

impl Button {
    #[allow(dead_code)]
    pub fn as_str(&self) -> &'static str {
        match self {
            Button::Reset => "reset",
            Button::Download => "download",
            Button::Power => "power",
        }
    }
}

/// Errors from power control operations.
#[derive(Debug)]
pub enum PowerError {
    NotConfigured,
    NotConnected,
    VerifyFailed {
        sent: u8,
        read_back: u8,
    },
    /// ser2net rejected the relay connection with "Port already in use" —
    /// ANOTHER MCP/session currently holds the shared relay. Transient:
    /// retry with backoff until the holder releases.
    RelayBusy,
    /// Release after a pulse failed even after retries; the relay channel
    /// state is unknown (the button may still be asserted). Run
    /// `serial_verify_relay` to check.
    ReleaseFailed {
        button: Button,
        source: Box<PowerError>,
    },
    Io(std::io::Error),
    Timeout,
    UnsupportedBackend(String),
    /// The device answered, but with bytes that do not match the request —
    /// distinct from `VerifyFailed` (wrong relay STATE read back) and from
    /// `Io` (no answer at all): a live peer speaking the wrong protocol.
    Protocol(String),
}

impl std::fmt::Display for PowerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PowerError::NotConfigured => write!(f, "power control not configured"),
            PowerError::NotConnected => write!(f, "power control not connected"),
            PowerError::VerifyFailed { sent, read_back } => {
                write!(
                    f,
                    "relay verify failed: sent {sent:#04x}, read back {read_back:#04x}"
                )
            }
            PowerError::ReleaseFailed { button, source } => write!(
                f,
                "relay release for button '{}' failed after retries — relay state unknown, run serial_verify_relay: {source}",
                button.as_str()
            ),
            PowerError::Io(e) => write!(f, "IO error: {e}"),
            PowerError::Timeout => write!(f, "operation timed out"),
            PowerError::UnsupportedBackend(name) => {
                write!(f, "unsupported power-control backend: {name}")
            }
            PowerError::RelayBusy => write!(f, "relay endpoint busy (held by another session)"),
            PowerError::Protocol(msg) => write!(f, "protocol error: {msg}"),
        }
    }
}

impl std::error::Error for PowerError {}

impl From<std::io::Error> for PowerError {
    fn from(e: std::io::Error) -> Self {
        PowerError::Io(e)
    }
}

/// Power control trait — abstracts physical button operations.
///
/// All methods are async because backends may involve network I/O (TCP relay,
/// SNMP) or serial I/O (software reboot). `#[async_trait]` boxes the futures
/// so the trait is DYNAMICALLY DISPATCHABLE — the plugin registry stores
/// `Box<dyn PowerControl>` factories, so adding a backend never changes
/// existing code .
#[async_trait::async_trait]
pub trait PowerControl: Send + Sync {
    /// Press a button (set pin low / turn relay ON / PDU on).
    async fn press(&mut self, button: Button) -> Result<(), PowerError>;

    /// Release a button (set pin high / turn relay OFF / PDU off).
    async fn release(&mut self, button: Button) -> Result<(), PowerError>;

    /// Pulse: press → delay → release.
    ///
    /// The release is retried a few times: a failed release after a
    /// successful press would leave the button asserted (DUT held in reset
    /// or unpowered). Backends that hold physical state should override
    /// `pulse` for stronger guarantees (state restore and cancellation
    /// safety), as `Ch340RelayControl` does.
    async fn pulse(&mut self, button: Button, delay_ms: u64) -> Result<(), PowerError> {
        self.press(button).await?;
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        let mut last_err: Option<PowerError> = None;
        for _ in 0..3 {
            match self.release(button).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    last_err = Some(e);
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
        Err(last_err.unwrap_or(PowerError::NotConnected))
    }

    /// Press WITH FEEDBACK: the command alone proves nothing — a frame can
    /// get lost in transit (rapid consecutive switching, flaky ser2net) and
    /// lenient echo handling would silently accept it. Backends that can
    /// read the channel state back override this to RETRY until the
    /// HARDWARE reports the new state; the default trusts the command.
    async fn press_verified(&mut self, button: Button) -> Result<(), PowerError> {
        self.press(button).await
    }

    /// Release WITH FEEDBACK — see [`press_verified`].
    async fn release_verified(&mut self, button: Button) -> Result<(), PowerError> {
        self.release(button).await
    }

    /// Verify the control is actually working (read back state).
    /// Returns `Ok(true)` if verified, `Ok(false)` if not verifiable, `Err` on error.
    async fn verify(&mut self) -> Result<bool, PowerError>;

    /// Human-readable name of the backend (e.g. "ch340-relay").
    fn name(&self) -> &'static str;

    /// Whether this backend controls the given button.
    fn has_button(&self, button: Button) -> bool;
}

/// Mapping of buttons to relay channels (1-4). 0 = not configured.
#[derive(Debug, Clone, Default)]
pub struct ButtonChannels {
    pub reset: u8,
    pub download: u8,
    pub power: u8,
}

impl ButtonChannels {
    pub fn get(&self, button: Button) -> u8 {
        match button {
            Button::Reset => self.reset,
            Button::Download => self.download,
            Button::Power => self.power,
        }
    }

    pub fn has_any(&self) -> bool {
        self.reset > 0 || self.download > 0 || self.power > 0
    }
}

// ── Plugin registry ──────────────────────────────────────────
// Backends are PLUGINS: a `PowerControl` implementation + one registration
// call. The engine, MCP layer and `dutabo init` TEST all go through
// `build_backend` / `supports` — adding a backend (SNMP PDU, GPIO, ...)
// NEVER touches existing code, exactly the plugin contract (user rule:
// never modify existing code).
//
// ```rust
// // in the plugin's own crate:
// dut_ctrl::register_backend("snmp-pdu", |cfg| Box::new(SnmpPduControl::new(...)));
// ```
//
// The built-in ch340 backend registers itself the same way (feature-gated);
// `none`/`disabled` are the semantic "no power control" builtin, not plugins.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

/// One plugin's factory: build a `PowerControl` from the runtime config.
pub type BackendFactory = fn(&PowerControlConfig) -> Box<dyn PowerControl>;

static REGISTRY: LazyLock<Mutex<HashMap<String, BackendFactory>>> = LazyLock::new(|| {
    let mut map: HashMap<String, BackendFactory> = HashMap::new();
    #[cfg(feature = "ch340")]
    {
        map.insert("ch340".to_string(), ch340_factory);
        map.insert("ch340-relay".to_string(), ch340_factory);
    }
    Mutex::new(map)
});

/// Plugin entry point: register a backend factory under one or more names.
/// Call once at plugin startup; a later registration for the same name
/// replaces the earlier factory.
pub fn register_backend(name: &str, factory: BackendFactory) {
    REGISTRY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(name.to_string(), factory);
}

#[cfg(feature = "ch340")]
fn ch340_factory(config: &PowerControlConfig) -> Box<dyn PowerControl> {
    Box::new(
        Ch340RelayControl::new(config.host.clone(), config.port, config.channels.clone())
            .with_min_reset_ms(config.reset_time_ms)
            .with_min_power_off_ms(config.power_off_time_ms),
    )
}

/// Build the backend selected by `config.backend` ("" / absent → the ch340
/// plugin, mirroring the legacy default). Unknown names and unregistered
/// plugins fall back to the disabled backend with an explanatory reason —
/// never a panic, so a stale `.target.jsonc` cannot crash the server.
pub fn build_backend(config: &PowerControlConfig) -> Box<dyn PowerControl> {
    let backend = config.backend.trim().to_ascii_lowercase();
    match backend.as_str() {
        "none" | "disabled" => Box::new(DisabledControl::new(
            "power control disabled by configuration",
        )),
        "" => match REGISTRY
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get("ch340")
        {
            Some(factory) => factory(config),
            None => Box::new(DisabledControl::new(
                "CH340 backend was not compiled (enable dut-ctrl feature `ch340`)",
            )),
        },
        name => match REGISTRY.lock().unwrap_or_else(|e| e.into_inner()).get(name) {
            Some(factory) => factory(config),
            None => Box::new(DisabledControl::new(format!(
                "unknown backend '{name}' — no plugin registered under this name"
            ))),
        },
    }
}

/// Is a backend name available (a registered plugin, or the built-in
/// `none`/`disabled` semantics)?
pub fn supports(name: &str) -> bool {
    match name.trim().to_ascii_lowercase().as_str() {
        "" | "none" | "disabled" => true,
        n => REGISTRY
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(n),
    }
}

/// Runtime backend selection populated from `.target.jsonc`.
pub struct PowerControlConfig {
    pub backend: String,
    pub host: String,
    pub port: u16,
    pub channels: ButtonChannels,
    pub reset_time_ms: u64,
    pub power_off_time_ms: u64,
    pub dut_name: String,
}

pub struct DisabledControl {
    reason: String,
}

impl DisabledControl {
    fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

#[async_trait::async_trait]
impl PowerControl for DisabledControl {
    async fn press(&mut self, _button: Button) -> Result<(), PowerError> {
        Err(PowerError::UnsupportedBackend(self.reason.clone()))
    }

    async fn release(&mut self, _button: Button) -> Result<(), PowerError> {
        Err(PowerError::UnsupportedBackend(self.reason.clone()))
    }

    async fn verify(&mut self) -> Result<bool, PowerError> {
        Err(PowerError::UnsupportedBackend(self.reason.clone()))
    }

    fn name(&self) -> &'static str {
        "disabled"
    }

    fn has_button(&self, _button: Button) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend_config(backend: &str) -> PowerControlConfig {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        PowerControlConfig {
            backend: backend.into(),
            host: "127.0.0.1".into(),
            port,
            channels: ButtonChannels {
                reset: 1,
                ..Default::default()
            },
            reset_time_ms: 500,
            power_off_time_ms: 3000,
            dut_name: "test-dut".into(),
        }
    }

    #[test]
    fn test_button_channels() {
        let ch = ButtonChannels {
            reset: 1,
            download: 2,
            power: 0,
        };
        assert_eq!(ch.get(Button::Reset), 1);
        assert_eq!(ch.get(Button::Download), 2);
        assert!(ch.has_any());
    }

    #[test]
    fn test_button_channels_empty() {
        let ch = ButtonChannels::default();
        assert!(!ch.has_any());
        assert_eq!(ch.get(Button::Reset), 0);
    }

    #[test]
    fn test_button_as_str() {
        assert_eq!(Button::Reset.as_str(), "reset");
        assert_eq!(Button::Download.as_str(), "download");
    }

    #[cfg(feature = "ch340")]
    #[test]
    fn target_type_selects_ch340_extension() {
        let backend = build_backend(&backend_config("ch340"));
        assert_eq!(backend.name(), "ch340-relay");
        assert!(backend.has_button(Button::Reset));
        // The `usb-relay` alias was removed — it now resolves
        // to the disabled backend, never to ch340.
        let alias = build_backend(&backend_config("usb-relay"));
        assert_eq!(alias.name(), "disabled");
        assert!(!alias.has_button(Button::Reset));
    }

    #[test]
    fn target_type_can_disable_power_control() {
        let backend = build_backend(&backend_config("disabled"));
        assert_eq!(backend.name(), "disabled");
        assert!(!backend.has_button(Button::Reset));
        assert!(!supports("unknown-backend"));
        // The external dev-ctl program backend was removed.
        assert!(!supports("external-dev-ctl"));
        assert!(!supports("dev-ctl"));
        // An empty backend name resolves to the ch340 plugin (legacy
        // default), or the disabled backend when ch340 is not compiled.
        let empty = build_backend(&backend_config(""));
        assert!(!empty.has_button(Button::Download));
        #[cfg(feature = "ch340")]
        assert!(empty.has_button(Button::Reset));
    }
}
