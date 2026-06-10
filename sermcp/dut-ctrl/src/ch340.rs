//! CH340 four-byte relay backend.

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex, OnceLock, Weak};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;

use super::{Button, ButtonChannels, PowerControl, PowerError};

pub const HEADER: u8 = 0xA0;
pub const OP_OFF: u8 = 0x00;
pub const OP_ON: u8 = 0x01;
pub const OP_STATUS: u8 = 0x05;

const RELAY_BUSY_BANNER: &[u8] = b"already in use";

type SharedRelayStream = Arc<Mutex<Option<TcpStream>>>;
type WeakRelayStream = Weak<Mutex<Option<TcpStream>>>;

/// Process-wide connection registry for single-client ser2net endpoints.
/// Every control instance targeting the same host and port shares one TCP
/// stream, including detached safety releases and independently built plugin
/// backends.
fn shared_relay_stream(host: &str, port: u16) -> SharedRelayStream {
    static STREAMS: OnceLock<StdMutex<HashMap<(String, u16), WeakRelayStream>>> = OnceLock::new();
    let streams = STREAMS.get_or_init(|| StdMutex::new(HashMap::new()));
    let key = (host.to_string(), port);
    let mut streams = streams
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(stream) = streams.get(&key).and_then(Weak::upgrade) {
        return stream;
    }
    streams.retain(|_, stream| stream.strong_count() > 0);
    let stream = Arc::new(Mutex::new(None));
    streams.insert(key, Arc::downgrade(&stream));
    stream
}

pub fn packet(channel: u8, opcode: u8) -> [u8; 4] {
    let checksum = HEADER.wrapping_add(channel).wrapping_add(opcode);
    [HEADER, channel, opcode, checksum]
}

fn response_is_relay_busy(response: &[u8]) -> bool {
    response
        .windows(RELAY_BUSY_BANNER.len())
        .any(|window| window == RELAY_BUSY_BANNER)
}

/// Outcome of scanning a read-back buffer for the expected echo frame.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum EchoCheck<'a> {
    /// Full frame with matching channel, opcode and checksum.
    Verified(&'a [u8]),
    /// Matching channel/opcode but a bad checksum byte — the frame is
    /// addressed to us yet its content cannot be trusted.
    ChecksumBad(&'a [u8]),
    /// Full 4-byte frame addressed to a different channel or opcode — a
    /// live peer speaking the wrong protocol.
    Mismatch(&'a [u8]),
    /// HEADER found without a full 4-byte frame behind it — likely a
    /// fragmented read, NOT a protocol violation.
    Truncated(&'a [u8]),
    /// No HEADER byte anywhere in the buffer (silent relay, banner only).
    Absent,
}

/// Wire checksum of a 4-byte frame: wrapping sum of HEADER + channel +
/// opcode, matching [`packet`].
fn frame_checksum(frame: &[u8]) -> u8 {
    HEADER.wrapping_add(frame[1]).wrapping_add(frame[2])
}

fn frame_hex(frame: &[u8]) -> String {
    frame
        .iter()
        .take(4)
        .map(|b| format!("{b:#04x}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Scan a read-back buffer for the expected echo frame.
///
/// ser2net may prepend banner bytes (which can even contain a stray HEADER
/// lookalike), so the buffer is walked at every HEADER position: a fully
/// valid frame wins immediately wherever it sits, otherwise the first
/// definite answer (checksum-bad beats mismatch beats truncated) is the
/// diagnosis.
pub(crate) fn classify_echo(buf: &[u8], channel: u8, opcode: u8) -> EchoCheck<'_> {
    let mut fallback = EchoCheck::Absent;
    for start in 0..buf.len() {
        if buf[start] != HEADER {
            continue;
        }
        let frame = &buf[start..];
        if frame.len() < 4 {
            if matches!(fallback, EchoCheck::Absent) {
                fallback = EchoCheck::Truncated(frame);
            }
            continue;
        }
        let candidate = &buf[start..start + 4];
        if candidate[1] == channel && candidate[2] == opcode {
            if candidate[3] == frame_checksum(candidate) {
                return EchoCheck::Verified(candidate);
            }
            if matches!(fallback, EchoCheck::Absent | EchoCheck::Truncated(_)) {
                fallback = EchoCheck::ChecksumBad(candidate);
            }
        } else if matches!(fallback, EchoCheck::Absent | EchoCheck::Truncated(_)) {
            fallback = EchoCheck::Mismatch(candidate);
        }
    }
    fallback
}
/// Compile-time default for minimum USB relay reset pulse duration (ms).
/// Set by build.rs from `[package.metadata.learn]` in Cargo.toml.
fn default_min_reset_ms() -> u64 {
    option_env!("DUT_CTRL_MIN_RESET_MS")
        .and_then(|s| s.parse().ok())
        .unwrap_or(3000)
}
/// CH340 relay control via TCP (ser2net). 4-byte protocol:
/// `[0xA0, channel, opcode, checksum]` where checksum = `(0xA0 + ch + op) & 0xFF`.
///
/// Channels are 1-indexed (user-facing channel 1 = protocol channel 1).
/// Read-back verification: after sending ON/OFF, send STATUS and compare.
pub struct Ch340RelayControl {
    host: String,
    port: u16,
    channels: ButtonChannels,
    stream: SharedRelayStream,
    /// Minimum reset pulse duration in ms. Pulse calls for Reset button
    /// are clamped to at least this value. Default: 3000 (3s) from Cargo.toml.
    min_reset_ms: u64,
    /// Minimum duration between power-off and power-on. This is a backend
    /// safety invariant: a cold reboot must not immediately re-apply power.
    min_power_off_ms: u64,
}
impl Ch340RelayControl {
    const OP_ON: u8 = OP_ON;
    const OP_OFF: u8 = OP_OFF;
    const OP_STATUS: u8 = OP_STATUS;

    pub fn new(host: String, port: u16, channels: ButtonChannels) -> Self {
        let stream = shared_relay_stream(&host, port);
        Self {
            host,
            port,
            channels,
            stream,
            min_reset_ms: default_min_reset_ms(),
            min_power_off_ms: 3000,
        }
    }

    /// Override the minimum reset pulse duration (ms).
    /// Use the value from `[dut.relay]` `reset_time_ms` in `.target.jsonc`
    /// if specified, otherwise the Cargo.toml default (3000).
    pub fn with_min_reset_ms(mut self, ms: u64) -> Self {
        if ms > 0 {
            self.min_reset_ms = ms;
        }
        self
    }

    /// Override the minimum power-off duration. Values below three seconds
    /// are clamped so the DUT power rails have time to discharge.
    pub fn with_min_power_off_ms(mut self, ms: u64) -> Self {
        self.min_power_off_ms = ms.max(3000);
        self
    }

    pub fn is_configured(&self) -> bool {
        self.port > 0 && self.channels.has_any()
    }

    async fn ensure_open(&self, shared_stream: &mut Option<TcpStream>) -> Result<(), PowerError> {
        if shared_stream.is_none() {
            let addr = format!("{}:{}", self.host, self.port);
            let stream = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(&addr))
                .await
                .map_err(|_| PowerError::Timeout)?
                .map_err(PowerError::Io)?;
            stream.set_nodelay(true).ok();
            *shared_stream = Some(stream);
        }
        Ok(())
    }

    async fn force_reconnect(
        &self,
        shared_stream: &mut Option<TcpStream>,
    ) -> Result<(), PowerError> {
        shared_stream.take();
        self.ensure_open(shared_stream).await
    }

    async fn clear_stream(&self) {
        self.stream.lock().await.take();
    }

    /// Send a 4-byte command and drain any response.
    async fn send_command(&mut self, channel: u8, opcode: u8) -> Result<Vec<u8>, PowerError> {
        let stream = Arc::clone(&self.stream);
        let mut shared_stream = stream.lock().await;
        let result = self
            .send_command_inner(&mut shared_stream, channel, opcode, true)
            .await;
        Self::reject_busy_response(&mut shared_stream, result)
    }

    /// Strict variant used only by the release-retry path: a connection-level
    /// error on the read-back (e.g. RST when ser2net restarts mid-exchange)
    /// or a read-back timeout (the relay stayed silent — hung firmware or a
    /// network partition without an RST) fails the command so the attempt
    /// counts as failed and the next attempt reconnects. A graceful EOF and
    /// short reads stay lenient: a clean close usually means the relay
    /// received the packet before the link dropped, and a short read can be
    /// a ser2net banner rather than evidence of failure.
    async fn send_command_strict(
        &mut self,
        channel: u8,
        opcode: u8,
    ) -> Result<Vec<u8>, PowerError> {
        let stream = Arc::clone(&self.stream);
        let mut shared_stream = stream.lock().await;
        let result = self
            .send_command_inner(&mut shared_stream, channel, opcode, false)
            .await;
        Self::reject_busy_response(&mut shared_stream, result)
    }

    /// A ser2net endpoint can accept TCP and then reject the session in-band.
    /// Never treat that banner as a successful relay response, and discard the
    /// rejected stream so the caller's next retry establishes a new session.
    fn reject_busy_response(
        shared_stream: &mut Option<TcpStream>,
        result: Result<Vec<u8>, PowerError>,
    ) -> Result<Vec<u8>, PowerError> {
        if result
            .as_ref()
            .is_ok_and(|response| response_is_relay_busy(response))
        {
            shared_stream.take();
            return Err(PowerError::RelayBusy);
        }
        result
    }

    async fn send_command_inner(
        &self,
        shared_stream: &mut Option<TcpStream>,
        channel: u8,
        opcode: u8,
        tolerate_readback_errors: bool,
    ) -> Result<Vec<u8>, PowerError> {
        let packet = packet(channel, opcode);

        for attempt in 0..2 {
            self.ensure_open(shared_stream).await.map_err(|e| {
                if attempt == 0 {
                    PowerError::NotConnected
                } else {
                    e
                }
            })?;

            let stream = shared_stream.as_mut().unwrap();
            match stream.write_all(&packet).await {
                Ok(_) => {}
                Err(_) if attempt == 0 => {
                    self.force_reconnect(shared_stream).await?;
                    continue;
                }
                Err(e) => return Err(PowerError::Io(e)),
            }
            stream.flush().await?;

            tokio::time::sleep(Duration::from_millis(150)).await;

            if opcode == Self::OP_STATUS {
                let mut buf = [0u8; 256];
                match tokio::time::timeout(Duration::from_millis(500), stream.read(&mut buf)).await
                {
                    Ok(Ok(n)) => return Ok(buf[..n].to_vec()),
                    Ok(Err(e)) => return Err(PowerError::Io(e)),
                    Err(_) => return Ok(Vec::new()),
                }
            }

            // Read back and verify the relay echo for ON/OFF commands.
            // The relay echoes: [0xA0, channel, opcode, checksum].
            // ser2net may prepend a banner — scan for the header byte.
            //
            // The strict path (release retries) gets a more generous read
            // budget: its job is to verify the relay actually received the
            // packet, so a slow-but-healthy relay must not be failed
            // spuriously, while a hung relay must still be caught.
            let read_ms = if tolerate_readback_errors { 500 } else { 1000 };
            let mut buf = [0u8; 256];
            match tokio::time::timeout(Duration::from_millis(read_ms), stream.read(&mut buf)).await
            {
                Ok(Ok(n)) if n >= 4 => match classify_echo(&buf[..n], channel, opcode) {
                    EchoCheck::Verified(echo) => return Ok(echo.to_vec()),
                    // The relay ANSWERED, but not with a trustworthy echo of
                    // our frame. Strict mode exists to verify the packet was
                    // applied, so every definite answer fails the attempt and
                    // lets the retry/restore path engage; lenient callers
                    // keep the historical tolerance.
                    EchoCheck::ChecksumBad(echo) if !tolerate_readback_errors => {
                        return Err(PowerError::Protocol(format!(
                            "relay echo checksum mismatch: sent [channel={channel}, opcode={opcode:#04x}], echo [{}]",
                            frame_hex(echo)
                        )));
                    }
                    EchoCheck::Mismatch(echo) if !tolerate_readback_errors => {
                        return Err(PowerError::Protocol(format!(
                            "relay echo mismatch: sent [channel={channel}, opcode={opcode:#04x}], echo [{}]",
                            frame_hex(echo)
                        )));
                    }
                    // A header without a full frame is a fragmented read, not
                    // a protocol violation — but strict mode still cannot
                    // verify the packet was applied, so the attempt fails
                    // (and is retried) rather than passing silently.
                    EchoCheck::Truncated(echo) if !tolerate_readback_errors => {
                        return Err(PowerError::Protocol(format!(
                            "relay echo truncated (fragmented read): [{}]",
                            frame_hex(echo)
                        )));
                    }
                    // No echo at all — still in spec (relay may not echo).
                    // Return empty to avoid breaking existing flow.
                    _ => return Ok(Vec::new()),
                },
                // Connection died mid-exchange (RST/reset): for the release
                // retry path this is a failed attempt worth retrying on a
                // fresh connection; every other caller stays tolerant.
                Ok(Err(e)) if !tolerate_readback_errors => return Err(PowerError::Io(e)),
                // Read timed out while the connection stayed open: the strict
                // path cannot verify the packet was applied, and a silent
                // relay (hung firmware, network partition) is exactly the
                // ambiguity the release retries exist for — count the
                // attempt as failed so the restore path engages. Graceful
                // EOF and short reads keep their historical leniency (a
                // clean FIN usually means the relay got the packet; a 1-3
                // byte read can be a ser2net banner and a single read
                // cannot distinguish it from a partial echo).
                Err(_) if !tolerate_readback_errors => return Err(PowerError::Timeout),
                _ => return Ok(Vec::new()), // timeout, EOF, or short read — lenient
            }
        }
        Err(PowerError::NotConnected)
    }

    /// Channel must be configured (1-indexed); 0 = not wired up.
    fn confirm_channel(&self, button: Button) -> Result<u8, PowerError> {
        let ch = self.channels.get(button);
        if ch == 0 {
            return Err(PowerError::NotConfigured);
        }
        Ok(ch)
    }

    /// Issue `opcode`, then PROVE it took effect by polling the channel
    /// status; re-issue up to `attempts` times when the relay still reports
    /// the old state. The echo verify checks the FRAME, not the RESULT —
    /// only the STATUS read-back survives rapid consecutive switching
    /// (where a frame can be lost or ignored mid-settle).
    async fn set_channel_verified(
        &mut self,
        channel: u8,
        opcode: u8,
        expected: u8,
    ) -> Result<(), PowerError> {
        let mut last = PowerError::VerifyFailed {
            sent: opcode,
            read_back: 0,
        };
        for _ in 0..3 {
            self.send_command(channel, opcode).await.map(|_| ())?;
            for _ in 0..3 {
                match self.read_channel_state(channel).await {
                    Ok(state) if state == expected => return Ok(()),
                    Ok(state) => {
                        last = PowerError::VerifyFailed {
                            sent: opcode,
                            read_back: state,
                        };
                    }
                    Err(e) => {
                        last = e;
                    }
                }
                // 9600 baud shared with boot text: the answer trails the
                // text bytes, so give it a slightly longer settle.
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            tokio::time::sleep(Duration::from_millis(120)).await;
        }
        Err(last)
    }

    /// Read back the status of a channel.
    async fn read_channel_state(&mut self, channel: u8) -> Result<u8, PowerError> {
        // TCP segmentation can split the ser2net banner and the response
        // frame across reads ("ser2net port…" first, `[A0, ch, status, ck]`
        // in a later segment), so a single read both misparses banner text
        // as state ('r' = 0x72) and misses frames that arrived late. Keep
        // RE-ISSUING the STATUS query and SCANNING each answer until the
        // well-formed frame appears or the retries run out.
        let mut last = PowerError::VerifyFailed {
            sent: OP_STATUS,
            read_back: 0,
        };
        for _ in 0..6 {
            let resp = self.send_command(channel, Self::OP_STATUS).await?;
            if resp.len() >= 4 {
                for pos in 0..=resp.len() - 4 {
                    // Response frame: [A0, channel, STATUS, checksum] where
                    // the checksum covers A0+channel+status (the status byte
                    // sits in the opcode position). The STATUS byte is only
                    // ever 0/1 — text bytes from ser2net banners (0x50 'P',
                    // 0x72 'r', …) that happen to satisfy the frame shape
                    // MUST be ignored, not reported as relay state.
                    if resp[pos] == HEADER
                        && resp[pos + 1] == channel
                        && resp[pos + 2] <= 0x01
                        && resp[pos + 3] == HEADER.wrapping_add(channel).wrapping_add(resp[pos + 2])
                    {
                        return Ok(resp[pos + 2]);
                    }
                }
                last = PowerError::VerifyFailed {
                    sent: OP_STATUS,
                    read_back: resp[0],
                };
            }
            tokio::time::sleep(Duration::from_millis(120)).await;
        }
        Err(last)
    }

    /// Send an opcode with `attempts` attempts, dropping the dead connection
    /// between retries so each attempt re-enters `ensure_open`. Uses the
    /// strict read-back: a connection that dies mid-exchange (RST) counts as
    /// a failed attempt, so ser2net restarts/cable pulls are actually retried.
    async fn send_opcode_with_retries(
        &mut self,
        channel: u8,
        opcode: u8,
        attempts: usize,
    ) -> Result<(), PowerError> {
        let mut last_err: Option<PowerError> = None;
        for attempt in 0..attempts {
            match self.send_command_strict(channel, opcode).await {
                Ok(_) => return Ok(()),
                Err(e) => {
                    last_err = Some(e);
                    if attempt + 1 < attempts {
                        self.clear_stream().await;
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
        }
        Err(last_err.unwrap_or(PowerError::NotConnected))
    }

    /// Release a button with retries. If every attempt fails (ser2net
    /// restart, cable pulled), best-effort restore the channel to the state
    /// it had before the pulse, then return an error flagging that the relay
    /// state is unknown.
    async fn release_with_retries(
        &mut self,
        button: Button,
        initial_state: Option<u8>,
    ) -> Result<(), PowerError> {
        let ch = self.channels.get(button);
        if ch == 0 {
            return Err(PowerError::NotConfigured);
        }
        const RELEASE_ATTEMPTS: usize = 3;
        let release_err = match self
            .send_opcode_with_retries(ch, Self::OP_OFF, RELEASE_ATTEMPTS)
            .await
        {
            Ok(()) => return Ok(()),
            Err(e) => e,
        };
        // Let a transient ser2net restart settle before the restore attempt.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let restore_opcode = match initial_state {
            Some(Self::OP_ON) => Self::OP_ON,
            _ => Self::OP_OFF,
        };
        self.clear_stream().await;
        let _ = self.send_command(ch, restore_opcode).await;
        Err(PowerError::ReleaseFailed {
            button,
            source: Box::new(release_err),
        })
    }
}

/// Detached best-effort release: reuses the endpoint's shared connection and
/// restores the channel to its pre-pulse state (or OFF if unknown).
async fn best_effort_release(
    host: String,
    port: u16,
    channels: ButtonChannels,
    button: Button,
    initial_state: Option<u8>,
) {
    let mut ctrl = Ch340RelayControl::new(host, port, channels);
    let _ = ctrl.release_with_retries(button, initial_state).await;
}

/// Cancellation safety for `pulse`: when dropped while armed, spawns a
/// detached task that releases the button over the shared endpoint connection.
/// An async release cannot run from `Drop`, so the task owns cloned connection
/// parameters instead of borrowing the relay.
struct PulseReleaseGuard {
    armed: bool,
    host: String,
    port: u16,
    channels: ButtonChannels,
    button: Button,
    initial_state: Option<u8>,
}

impl PulseReleaseGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PulseReleaseGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Best effort only — no runtime context means nothing can be done.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(best_effort_release(
                self.host.clone(),
                self.port,
                self.channels.clone(),
                self.button,
                self.initial_state,
            ));
        }
    }
}

#[async_trait::async_trait]
impl PowerControl for Ch340RelayControl {
    async fn press(&mut self, button: Button) -> Result<(), PowerError> {
        let ch = self.confirm_channel(button)?;
        self.send_command(ch, Self::OP_ON).await.map(|_| ())
    }

    async fn release(&mut self, button: Button) -> Result<(), PowerError> {
        let ch = self.confirm_channel(button)?;
        self.send_command(ch, Self::OP_OFF).await.map(|_| ())
    }

    async fn press_verified(&mut self, button: Button) -> Result<(), PowerError> {
        let ch = self.confirm_channel(button)?;
        self.set_channel_verified(ch, Self::OP_ON, 0x01).await
    }

    async fn release_verified(&mut self, button: Button) -> Result<(), PowerError> {
        let ch = self.confirm_channel(button)?;
        self.set_channel_verified(ch, Self::OP_OFF, 0x00).await
    }

    /// Pulse with minimum reset time enforcement for Reset button.
    /// USB relays require a minimum hold time (default 3s) to physically
    /// toggle; shorter pulses are silently extended to the minimum.
    ///
    /// Failure safety: the release is retried and, if it still fails, the
    /// channel is best-effort restored to its pre-pulse state. Cancellation
    /// safety: if this future is dropped while the relay is asserted (client
    /// disconnect, MCP call cancelled), `PulseReleaseGuard` releases the
    /// button from a detached task.
    async fn pulse(&mut self, button: Button, delay_ms: u64) -> Result<(), PowerError> {
        let actual_delay = match button {
            Button::Reset => delay_ms.max(self.min_reset_ms),
            Button::Power => delay_ms.max(self.min_power_off_ms),
            Button::Download => delay_ms,
        };
        let ch = self.channels.get(button);
        if ch == 0 {
            return Err(PowerError::NotConfigured);
        }

        // Remember the channel state before pressing so a failed release can
        // restore it (a Power channel left asserted keeps the DUT dark).
        let initial_state = self.read_channel_state(ch).await.ok();

        self.press(button).await?;

        let mut guard = PulseReleaseGuard {
            armed: true,
            host: self.host.clone(),
            port: self.port,
            channels: self.channels.clone(),
            button,
            initial_state,
        };

        tokio::time::sleep(Duration::from_millis(actual_delay)).await;
        let result = self.release_with_retries(button, initial_state).await;
        guard.disarm();
        result
    }

    async fn verify(&mut self) -> Result<bool, PowerError> {
        if !self.is_configured() {
            return Err(PowerError::NotConfigured);
        }
        // Verify by toggling the reset channel and reading back.
        // Verify with the reset channel, else the first available one.
        let ch = if self.channels.reset > 0 {
            self.channels.reset
        } else {
            self.channels.download
        };
        if ch == 0 {
            return Ok(false); // No channel to verify with
        }

        // Turn ON and read back.
        self.send_command(ch, Self::OP_ON).await?;
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Defer guarantee: once ON has been sent, every error path below must
        // re-send OP_OFF so verify never leaves a channel asserted (e.g. a
        // Reset channel held asserted would keep the DUT clamped in reset).
        let outcome: Result<(u8, u8), PowerError> = async {
            let state_on = self.read_channel_state(ch).await?;

            // Turn OFF and read back.
            self.send_command(ch, Self::OP_OFF).await?;
            tokio::time::sleep(Duration::from_millis(100)).await;
            let state_off = self.read_channel_state(ch).await?;
            Ok((state_on, state_off))
        }
        .await;

        match outcome {
            Ok((state_on, state_off)) => {
                let verified = state_on == Self::OP_ON && state_off == Self::OP_OFF;
                if !verified {
                    return Err(PowerError::VerifyFailed {
                        sent: Self::OP_ON,
                        read_back: state_on,
                    });
                }
                Ok(true)
            }
            Err(e) => {
                // Best-effort release — the original error is what matters.
                let _ = self.send_command(ch, Self::OP_OFF).await;
                Err(e)
            }
        }
    }

    fn name(&self) -> &'static str {
        "ch340-relay"
    }

    fn has_button(&self, button: Button) -> bool {
        self.channels.get(button) > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_port() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        port
    }

    #[test]
    fn test_ch340_configured() {
        let relay = Ch340RelayControl::new(
            "host.invalid".to_string(),
            test_port(),
            ButtonChannels {
                reset: 1,
                download: 2,
                power: 0,
            },
        );
        assert!(relay.is_configured());
    }

    #[test]
    fn test_ch340_not_configured_zero_port() {
        let relay = Ch340RelayControl::new(
            "host.invalid".to_string(),
            0,
            ButtonChannels {
                reset: 1,
                download: 0,
                power: 0,
            },
        );
        assert!(!relay.is_configured());
    }

    #[test]
    fn test_ch340_not_configured_no_channels() {
        let relay = Ch340RelayControl::new(
            "host.invalid".to_string(),
            test_port(),
            ButtonChannels::default(),
        );
        assert!(!relay.is_configured());
    }

    #[test]
    fn test_ch340_has_button() {
        let relay = Ch340RelayControl::new(
            "host.invalid".to_string(),
            test_port(),
            ButtonChannels {
                reset: 1,
                download: 2,
                power: 0,
            },
        );
        assert!(relay.has_button(Button::Reset));
        assert!(relay.has_button(Button::Download));
    }

    #[test]
    fn power_off_safety_interval_is_never_below_three_seconds() {
        let relay = Ch340RelayControl::new(
            "127.0.0.1".into(),
            test_port(),
            ButtonChannels {
                power: 1,
                ..Default::default()
            },
        )
        .with_min_power_off_ms(1);

        assert_eq!(relay.min_power_off_ms, 3000);
    }
    #[tokio::test]
    async fn test_ch340_verify_not_configured() {
        let mut relay =
            Ch340RelayControl::new("host.invalid".to_string(), 0, ButtonChannels::default());
        let result = relay.verify().await;
        assert!(result.is_err());
    }
    #[tokio::test]
    async fn test_ch340_relay_with_mock_server() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 16];

            // Read ON packet
            let n = socket.read(&mut buf).await.unwrap();
            assert_eq!(n, 4);
            let _ = n;
            assert_eq!(buf[0], 0xA0);
            assert_eq!(buf[2], Ch340RelayControl::OP_ON);

            // Respond with status ON
            tokio::time::sleep(Duration::from_millis(10)).await;
            let status_resp = [0xA0, buf[1], 0x01, (0xA0 + buf[1] + 0x01) as u8];
            socket.write_all(&status_resp).await.unwrap();

            // Read STATUS packet
            tokio::time::sleep(Duration::from_millis(50)).await;
            let _ = socket.read(&mut buf).await;

            // Respond with status ON
            let status_resp = [0xA0, 1, 0x01, (0xA0 + 1 + 0x01) as u8];
            socket.write_all(&status_resp).await.unwrap();

            // Read OFF packet
            tokio::time::sleep(Duration::from_millis(50)).await;
            let _n = socket.read(&mut buf).await.unwrap();
            assert_eq!(buf[2], Ch340RelayControl::OP_OFF);

            // Respond with status OFF
            tokio::time::sleep(Duration::from_millis(10)).await;
            let status_resp = [0xA0, buf[1], 0x00, (0xA0 + buf[1] + 0x00) as u8];
            socket.write_all(&status_resp).await.unwrap();

            // Read STATUS packet
            tokio::time::sleep(Duration::from_millis(50)).await;
            let _ = socket.read(&mut buf).await;

            // Respond with status OFF
            let status_resp = [0xA0, 1, 0x00, (0xA0 + 1 + 0x00) as u8];
            socket.write_all(&status_resp).await.unwrap();
        });

        let mut relay = Ch340RelayControl::new(
            "127.0.0.1".to_string(),
            port,
            ButtonChannels {
                reset: 1,
                download: 0,
                power: 0,
            },
        );

        let result = relay.verify().await;
        assert!(result.is_ok());
        assert!(result.unwrap());

        server.await.unwrap();
    }

    /// Release retry: the first attempt's connection dies after the write
    /// (RST), so the client's read-back fails and the release is retried on
    /// a fresh connection, where the echo makes the second attempt succeed.
    /// The assertions depend only on the packet/connection sequence, never
    /// on timing.
    #[tokio::test]
    async fn test_pulse_release_retries_until_success() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            // Connection 1: first attempt — read OP_OFF, then abort the
            // connection (RST) so the client's read-back fails.
            {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 16];
                // Read only header/channel/opcode: leaving the 4th byte
                // unread means closing the socket resets the connection
                // (RST), so the client's read-back fails with ECONNRESET
                // instead of seeing a graceful EOF.
                socket.read_exact(&mut buf[..3]).await.unwrap();
                assert_eq!(buf[0], 0xA0);
                assert_eq!(buf[2], Ch340RelayControl::OP_OFF);
            }

            // Connection 2: retried attempt — read OP_OFF and echo it so
            // send_command's read-back succeeds.
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 16];
            let n = socket.read(&mut buf).await.unwrap();
            assert_eq!(n, 4);
            assert_eq!(buf[0], 0xA0);
            assert_eq!(buf[2], Ch340RelayControl::OP_OFF);
            socket
                .write_all(&packet(buf[1], Ch340RelayControl::OP_OFF))
                .await
                .unwrap();
        });

        let mut relay = Ch340RelayControl::new(
            "127.0.0.1".to_string(),
            port,
            ButtonChannels {
                reset: 1,
                ..Default::default()
            },
        );

        let result = relay
            .release_with_retries(Button::Reset, Some(Ch340RelayControl::OP_OFF))
            .await;
        assert!(result.is_ok());

        let server_result = tokio::time::timeout(Duration::from_secs(10), server).await;
        assert!(server_result.is_ok(), "mock server never saw the release");
        server_result.unwrap().unwrap();
    }

    /// Release total failure: after all retries fail, the pre-pulse state
    /// (channel was ON) must be restored over a fresh connection, and the
    /// error must flag that the relay state is unknown.
    ///
    /// Deterministic failure injection: the mock server stays bound the
    /// whole time and fails each of the three release attempts by reading
    /// the OP_OFF packet and then resetting the connection (RST), so the
    /// client's read-back gets a connection error and the attempt fails.
    /// The fourth connection must carry the OP_ON restore. The assertions
    /// depend only on the packet/connection sequence, never on timing.
    #[tokio::test]
    async fn test_pulse_release_all_failed_restores_original_state() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            // Connections 1..=3: the three failed release attempts. Read the
            // OP_OFF packet, then abort the connection (RST) so the client's
            // read-back fails with a connection error.
            for _ in 0..3 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 16];
                // Read only header/channel/opcode: leaving the 4th byte
                // unread means closing the socket resets the connection
                // (RST), so the client's read-back fails with ECONNRESET
                // instead of seeing a graceful EOF.
                socket.read_exact(&mut buf[..3]).await.unwrap();
                assert_eq!(buf[0], 0xA0);
                assert_eq!(buf[2], Ch340RelayControl::OP_OFF);
            }

            // Connection 4 is the restore over a fresh connection: it must
            // re-assert the pre-pulse state (channel was ON).
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 16];
            let n = socket.read(&mut buf).await.unwrap();
            assert_eq!(n, 4);
            assert_eq!(buf[0], 0xA0);
            assert_eq!(buf[2], Ch340RelayControl::OP_ON);
            socket
                .write_all(&packet(buf[1], Ch340RelayControl::OP_ON))
                .await
                .unwrap();
        });

        let mut relay = Ch340RelayControl::new(
            "127.0.0.1".to_string(),
            port,
            ButtonChannels {
                power: 1,
                ..Default::default()
            },
        );

        let result = relay
            .release_with_retries(Button::Power, Some(Ch340RelayControl::OP_ON))
            .await;
        assert!(
            matches!(
                &result,
                Err(PowerError::ReleaseFailed {
                    button: Button::Power,
                    ..
                })
            ),
            "expected ReleaseFailed for Power, got {result:?}"
        );

        let server_result = tokio::time::timeout(Duration::from_secs(10), server).await;
        assert!(
            server_result.is_ok(),
            "mock server never saw the restore packet"
        );
        server_result.unwrap().unwrap();
    }

    /// Strict-mode safety: a relay that stays silent after the write (no
    /// echo, no close — hung relay firmware or a network partition without
    /// an RST) must NOT be reported as a successful release. Each strict
    /// attempt times out, so after 3 failed attempts the pre-pulse state
    /// (channel was ON) is restored over a fresh connection and
    /// ReleaseFailed is returned.
    ///
    /// Deterministic: the mock server stays bound the whole time and holds
    /// every release connection open without answering, so the client's
    /// read-back times out on every attempt regardless of wall-clock timing.
    #[tokio::test]
    async fn test_pulse_release_silent_relay_fails_and_restores() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            // Connections 1..=3: the three release attempts. Read the full
            // OP_OFF packet, then hold the connection open without
            // responding. Dropping the socket would send a graceful FIN,
            // which the client treats as a plausible "relay got it" — so
            // the sockets must stay open until the client gives up.
            let mut held = Vec::new();
            for _ in 0..3 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 16];
                let n = socket.read(&mut buf).await.unwrap();
                assert_eq!(n, 4);
                assert_eq!(buf[0], 0xA0);
                assert_eq!(buf[2], Ch340RelayControl::OP_OFF);
                held.push(socket);
            }

            // Connection 4 is the restore over a fresh connection: it must
            // re-assert the pre-pulse state (channel was ON).
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 16];
            let n = socket.read(&mut buf).await.unwrap();
            assert_eq!(n, 4);
            assert_eq!(buf[0], 0xA0);
            assert_eq!(buf[2], Ch340RelayControl::OP_ON);
            socket
                .write_all(&packet(buf[1], Ch340RelayControl::OP_ON))
                .await
                .unwrap();
            drop(held);
        });

        let mut relay = Ch340RelayControl::new(
            "127.0.0.1".to_string(),
            port,
            ButtonChannels {
                power: 1,
                ..Default::default()
            },
        );

        let result = relay
            .release_with_retries(Button::Power, Some(Ch340RelayControl::OP_ON))
            .await;
        assert!(
            matches!(
                &result,
                Err(PowerError::ReleaseFailed {
                    button: Button::Power,
                    ..
                })
            ),
            "expected ReleaseFailed for Power, got {result:?}"
        );

        let server_result = tokio::time::timeout(Duration::from_secs(10), server).await;
        assert!(
            server_result.is_ok(),
            "mock server never saw the restore packet"
        );
        server_result.unwrap().unwrap();
    }

    /// Strict-mode leniency: a graceful EOF after the write still counts as
    /// success — a clean close usually means the relay received OP_OFF
    /// before ser2net dropped the connection, which is not a hang. Pins the
    /// intended EOF behavior so a future tightening cannot regress it.
    #[tokio::test]
    async fn test_pulse_release_eof_counts_as_success() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 16];
            // Read the full OP_OFF packet, then close gracefully: all bytes
            // were consumed, so the client's read-back sees EOF, not RST.
            let n = socket.read(&mut buf).await.unwrap();
            assert_eq!(n, 4);
            assert_eq!(buf[0], 0xA0);
            assert_eq!(buf[2], Ch340RelayControl::OP_OFF);
        });

        let mut relay = Ch340RelayControl::new(
            "127.0.0.1".to_string(),
            port,
            ButtonChannels {
                power: 1,
                ..Default::default()
            },
        );

        let result = relay
            .release_with_retries(Button::Power, Some(Ch340RelayControl::OP_ON))
            .await;
        assert!(
            result.is_ok(),
            "graceful EOF must count as success, got {result:?}"
        );

        let server_result = tokio::time::timeout(Duration::from_secs(10), server).await;
        assert!(server_result.is_ok(), "mock server never saw the release");
        server_result.unwrap().unwrap();
    }

    /// verify() must turn the channel OFF again when the STATUS read-back
    /// fails after ON was asserted (regression: relay left ON).
    #[tokio::test]
    async fn test_verify_sends_off_when_status_read_fails() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 16];

            // Read ON packet, echo it back.
            let n = socket.read(&mut buf).await.unwrap();
            assert_eq!(n, 4);
            assert_eq!(buf[2], Ch340RelayControl::OP_ON);
            let echo = [
                0xA0,
                buf[1],
                Ch340RelayControl::OP_ON,
                (0xA0 + buf[1] + Ch340RelayControl::OP_ON) as u8,
            ];
            socket.write_all(&echo).await.unwrap();

            // Read STATUS packet, respond with a truncated (<3 byte) payload
            // so read_channel_state fails with VerifyFailed. The status read
            // now RE-ISSUES the query (frame-rescan loop) — answer every
            // STATUS query with the same garbage.
            let _ = socket.read(&mut buf).await.unwrap();
            assert_eq!(buf[2], Ch340RelayControl::OP_STATUS);
            socket.write_all(&[0x00, 0xFF]).await.unwrap();

            // verify() must send OP_OFF before returning the error: skip the
            // STATUS re-queries, then the cleanup packet must be first.
            loop {
                let n = socket.read(&mut buf).await.unwrap();
                if n == 4 && buf[2] == Ch340RelayControl::OP_OFF {
                    break;
                }
            }
        });

        let mut relay = Ch340RelayControl::new(
            "127.0.0.1".to_string(),
            port,
            ButtonChannels {
                reset: 1,
                ..Default::default()
            },
        );

        let result = relay.verify().await;
        assert!(result.is_err());

        let server_result = tokio::time::timeout(Duration::from_secs(10), server).await;
        assert!(
            server_result.is_ok(),
            "verify() never sent OP_OFF after the failed read-back"
        );
        server_result.unwrap().unwrap();
    }

    /// Cancellation safety: dropping a pulse future while the relay is
    /// asserted must still release the button (detached task on a fresh
    /// connection).
    #[tokio::test]
    async fn test_pulse_cancellation_releases_button() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (pressed, wait_pressed) = tokio::sync::oneshot::channel::<()>();

        let server = tokio::spawn(async move {
            // First connection: initial STATUS read + ON press.
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 16];

            let n = socket.read(&mut buf).await.unwrap();
            assert_eq!(n, 4);
            assert_eq!(buf[2], Ch340RelayControl::OP_STATUS);
            let status_resp = [0xA0, buf[1], 0x00, (0xA0 + buf[1]) as u8];
            socket.write_all(&status_resp).await.unwrap();

            let n = socket.read(&mut buf).await.unwrap();
            assert_eq!(n, 4);
            assert_eq!(buf[2], Ch340RelayControl::OP_ON);
            let echo = [
                0xA0,
                buf[1],
                Ch340RelayControl::OP_ON,
                (0xA0 + buf[1] + Ch340RelayControl::OP_ON) as u8,
            ];
            socket.write_all(&echo).await.unwrap();

            // Give the client time to finish press() (echo read-back) so the
            // pulse is in its sleep when the test cancels it.
            tokio::time::sleep(Duration::from_millis(300)).await;
            let _ = pressed.send(());

            // The cancellation guard opens a fresh connection and sends
            // OP_OFF (restore of the pre-pulse OFF state).
            let (mut socket2, _) = listener.accept().await.unwrap();
            let n = socket2.read(&mut buf).await.unwrap();
            assert_eq!(n, 4);
            assert_eq!(buf[2], Ch340RelayControl::OP_OFF);
            let echo = [
                0xA0,
                buf[1],
                Ch340RelayControl::OP_OFF,
                (0xA0 + buf[1]) as u8,
            ];
            socket2.write_all(&echo).await.unwrap();
        });

        let mut relay = Ch340RelayControl::new(
            "127.0.0.1".to_string(),
            port,
            ButtonChannels {
                reset: 1,
                ..Default::default()
            },
        )
        .with_min_reset_ms(10);

        // Start a long pulse and cancel it while the relay is asserted.
        let pulse = tokio::spawn(async move { relay.pulse(Button::Reset, 60_000).await });
        wait_pressed.await.unwrap();
        pulse.abort();

        let server_result = tokio::time::timeout(Duration::from_secs(10), server).await;
        assert!(
            server_result.is_ok(),
            "cancelled pulse never released the button"
        );
        server_result.unwrap().unwrap();
    }

    // ── classify_echo: pure-frame classification (no I/O) ────────────────

    fn frame(channel: u8, opcode: u8) -> [u8; 4] {
        packet(channel, opcode)
    }

    #[test]
    fn classify_echo_accepts_exact_frame() {
        let buf = frame(1, OP_ON);
        assert_eq!(classify_echo(&buf, 1, OP_ON), EchoCheck::Verified(&buf[..]));
    }

    #[test]
    fn classify_echo_finds_frame_after_ser2net_banner() {
        let mut buf = b"ser2net port 2002 device /dev/ttyUSB0\r\n".to_vec();
        buf.extend_from_slice(&frame(2, OP_OFF));
        assert!(matches!(
            classify_echo(&buf, 2, OP_OFF),
            EchoCheck::Verified(_)
        ));
    }

    #[test]
    fn classify_echo_scans_past_spurious_header_bytes() {
        // Banner garbage that happens to contain a HEADER lookalike must not
        // shadow the real frame further into the buffer.
        let mut buf = vec![0x21, HEADER, 0x07, b'\r', b'\n'];
        buf.extend_from_slice(&frame(1, OP_ON));
        assert!(matches!(
            classify_echo(&buf, 1, OP_ON),
            EchoCheck::Verified(_)
        ));
    }

    #[test]
    fn classify_echo_rejects_bad_checksum() {
        let buf = [HEADER, 1, OP_ON, 0xFF];
        assert!(matches!(
            classify_echo(&buf, 1, OP_ON),
            EchoCheck::ChecksumBad(_)
        ));
    }

    #[test]
    fn classify_echo_reports_mismatched_channel() {
        let buf = frame(3, OP_ON);
        assert!(matches!(
            classify_echo(&buf, 1, OP_ON),
            EchoCheck::Mismatch(_)
        ));
    }

    #[test]
    fn classify_echo_labels_truncated_tail_as_fragmented() {
        // HEADER + channel + opcode but the 4-byte frame cut short — a
        // fragmented read, not a protocol violation.
        let buf = [0x10, HEADER, 1, OP_ON];
        assert!(matches!(
            classify_echo(&buf, 1, OP_ON),
            EchoCheck::Truncated(_)
        ));
    }

    #[test]
    fn classify_echo_absent_without_header() {
        assert_eq!(classify_echo(b"banner\r\n", 1, OP_ON), EchoCheck::Absent);
    }
}

#[cfg(test)]
mod verified_ops_tests {
    use super::*;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn frame(channel: u8, opcode: u8) -> [u8; 4] {
        packet(channel, opcode)
    }

    /// Mock relay: answers ON/OFF commands with an echo frame and STATUS
    /// commands with the next value from a scripted list. Records the
    /// opcodes it saw so the test can assert re-issues.
    async fn spawn_scripted_relay(
        status_script: Vec<u8>,
    ) -> (u16, std::sync::Arc<std::sync::Mutex<Vec<u8>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let script: std::sync::Arc<std::sync::Mutex<Vec<u8>>> =
            Arc::new(std::sync::Mutex::new(status_script));
        let seen: std::sync::Arc<std::sync::Mutex<Vec<u8>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_loop = seen.clone();
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = match listener.accept().await {
                    Ok(x) => x,
                    Err(_) => break,
                };
                let script = script.clone();
                let seen = seen_loop.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 16];
                    loop {
                        let n = tokio::time::timeout(Duration::from_secs(10), sock.read(&mut buf))
                            .await
                            .ok()
                            .and_then(Result::ok)
                            .unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        let opcode = buf[2];
                        seen.lock().unwrap().push(opcode);
                        if opcode == OP_STATUS {
                            let status = {
                                let mut q = script.lock().unwrap();
                                if q.len() > 1 {
                                    q.remove(0)
                                } else {
                                    q.first().copied().unwrap_or(0)
                                }
                            };
                            let mut resp = frame(buf[1], 0x05).to_vec();
                            resp[2] = status;
                            // Response checksum covers A0+channel+STATUS (the
                            // status byte sits in the opcode slot).
                            resp[3] = HEADER.wrapping_add(buf[1]).wrapping_add(status);
                            sock.write_all(&resp).await.unwrap();
                        } else {
                            sock.write_all(&frame(buf[1], opcode)).await.unwrap();
                        }
                    }
                });
            }
        });
        (port, seen)
    }

    /// Happy path: the relay echoes the ON and its STATUS immediately
    /// reports ON — press_verified returns without re-issues.
    #[tokio::test]
    async fn press_verified_confirms_on_first_read() {
        let (port, seen) = spawn_scripted_relay(vec![0x01]).await;
        let mut relay = Ch340RelayControl::new(
            "127.0.0.1".into(),
            port,
            ButtonChannels {
                reset: 1,
                download: 0,
                power: 0,
            },
        );
        relay.press_verified(Button::Reset).await.unwrap();
        assert_eq!(*seen.lock().unwrap(), vec![OP_ON, OP_STATUS]);
    }

    /// The relay reports OFF after the first ON (command lost mid-settle) —
    /// the verification loop RE-ISSUES the command until the STATUS
    /// confirms; it never reports success on an unconfirmed write.
    #[tokio::test]
    async fn press_verified_reissues_until_status_confirms() {
        let (port, seen) = spawn_scripted_relay(vec![0x00, 0x00, 0x00, 0x01]).await;
        let mut relay = Ch340RelayControl::new(
            "127.0.0.1".into(),
            port,
            ButtonChannels {
                reset: 1,
                download: 0,
                power: 0,
            },
        );
        relay.press_verified(Button::Reset).await.unwrap();
        let ops = seen.lock().unwrap().clone();
        assert_eq!(ops[0], OP_ON);
        assert_eq!(ops[1], OP_STATUS);
        assert_eq!(*ops.last().unwrap(), OP_STATUS);
        assert_eq!(
            ops.iter().filter(|&&op| op == OP_ON).count(),
            2,
            "re-issued"
        );
    }
}

#[cfg(test)]
mod status_scan_tests {
    use super::*;

    #[test]
    fn relay_busy_banner_uses_its_actual_length() {
        assert!(response_is_relay_busy(b"Port already in use\r\n"));
        assert!(!response_is_relay_busy(b"ser2net port ready\r\n"));
    }

    #[tokio::test]
    async fn controls_for_one_endpoint_share_exactly_one_tcp_connection() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut command = [0u8; 4];
            for expected in [OP_ON, OP_OFF] {
                socket.read_exact(&mut command).await.unwrap();
                assert_eq!(command[2], expected);
                socket
                    .write_all(&packet(command[1], expected))
                    .await
                    .unwrap();
            }
            assert!(
                tokio::time::timeout(Duration::from_millis(250), listener.accept())
                    .await
                    .is_err(),
                "a second control opened another TCP connection"
            );
        });

        let channels = ButtonChannels {
            reset: 1,
            download: 0,
            power: 0,
        };
        let mut first = Ch340RelayControl::new("127.0.0.1".into(), port, channels.clone());
        let mut second = Ch340RelayControl::new("127.0.0.1".into(), port, channels);
        first.press(Button::Reset).await.unwrap();
        second.release(Button::Reset).await.unwrap();
        server.await.unwrap();
    }

    /// A rejected ser2net session is delivered as text after TCP connect.
    /// The verified operation must discard that stream and recover through a
    /// fresh connection instead of polling the rejected session repeatedly.
    #[tokio::test]
    async fn press_verified_reconnects_after_busy_banner() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut rejected, _) = listener.accept().await.unwrap();
            let mut frame = [0u8; 4];
            rejected.read_exact(&mut frame).await.unwrap();
            assert_eq!(frame[2], OP_ON);
            rejected.write_all(&packet(frame[1], OP_ON)).await.unwrap();
            rejected.read_exact(&mut frame).await.unwrap();
            assert_eq!(frame[2], OP_STATUS);
            rejected
                .write_all(b"Port already in use\r\n")
                .await
                .unwrap();
            drop(rejected);

            let (mut accepted, _) = listener.accept().await.unwrap();
            accepted.read_exact(&mut frame).await.unwrap();
            assert_eq!(frame[2], OP_STATUS);
            accepted
                .write_all(&[
                    HEADER,
                    frame[1],
                    OP_ON,
                    HEADER.wrapping_add(frame[1]).wrapping_add(OP_ON),
                ])
                .await
                .unwrap();
        });

        let mut relay = Ch340RelayControl::new(
            "127.0.0.1".into(),
            port,
            ButtonChannels {
                reset: 1,
                download: 0,
                power: 0,
            },
        );
        relay.press_verified(Button::Reset).await.unwrap();
        server.await.unwrap();
    }

    /// ser2net prepends a TEXT banner on a fresh relay connection: the
    /// status frame must be found by scanning, and a banner byte ('r')
    /// must never be mistaken for the channel state.
    #[tokio::test]
    async fn read_channel_state_scans_past_text_banner() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 64];
            // STATUS query → answer banner text + a well-formed frame.
            let _ = sock.read(&mut buf).await;
            let mut resp = b"ser2net port 2000 device /dev/ttyUSB0\r\n".to_vec();
            let st = [HEADER, 1, 0x01, HEADER.wrapping_add(1).wrapping_add(0x01)];
            resp.extend_from_slice(&st);
            sock.write_all(&resp).await.unwrap();
        });
        let mut relay = Ch340RelayControl::new(
            "127.0.0.1".into(),
            port,
            ButtonChannels {
                reset: 1,
                download: 0,
                power: 0,
            },
        );
        let state = relay.read_channel_state(1).await.unwrap();
        assert_eq!(state, 0x01);
        server.await.unwrap();
    }
}
