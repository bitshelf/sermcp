//! Console driver — TCP connection to ser2net, modeled on labgrid SerialDriver.
//!
//! Talks to ser2net directly over tokio TCP (socket:// protocol), no socat
//! middle layer. All writes are sent through the `write_tx` channel; the read
//! loop performs the actual I/O.
//!
//! Channel is **bounded** (depth 64). When full, oldest messages are dropped
//! to prevent unbounded memory growth during extended disconnects.

use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use crate::error::SerialError;
use crate::telnet_filter::{self, TelnetDecoder, TelnetEvent, iac};

/// Max pending writes before dropping (prevents unbounded memory leak on disconnect).
const WRITE_CHANNEL_DEPTH: usize = 64;
/// Maximum wire burst for marker-wrapped shell commands.
///
/// Eight-byte chunks with a short cadence keep command bursts below common
/// UART line rates and give the ser2net/USB-UART/TTY receive path time to
/// drain. This transport pacing is independent of the baud rate selected in
/// `.target.jsonc`.
const COMMAND_WRITE_CHUNK_BYTES: usize = 8;
const COMMAND_WRITE_GAP: Duration = Duration::from_millis(2);
/// Window for ser2net to answer our IAC DO/WILL COM-PORT-OPTION probes.
const NEGOTIATE_TIMEOUT: Duration = Duration::from_millis(500);
/// Window for the NOTIFY-BAUDRATE ack after sending SET-BAUDRATE.
const SET_BAUD_ACK_TIMEOUT: Duration = Duration::from_millis(1000);

/// Console transport mode. Raw = byte passthrough (default, today's behavior).
/// Rfc2217 = telnet COM-PORT-OPTION negotiation + IAC filtering + dynamic baud.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConsoleMode {
    #[default]
    Raw,
    Rfc2217,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WritePacing {
    Immediate,
    Command,
}

#[derive(Debug)]
struct WriteRequest {
    data: Vec<u8>,
    pacing: WritePacing,
}

/// Cloneable producer for the console's single ordered write queue.
///
/// Raw input, control bytes and reset/boot helpers remain immediate.  Only
/// marker-wrapped `serial_send_command` payloads opt into command pacing.
///
/// Every successful enqueue pokes the write [`tokio::sync::Notify`] so the
/// read loop can flush the queue to the wire immediately instead of at its
/// polling cadence — this is what keeps relay keystrokes (dutabo serial)
/// from queueing behind the readability timeout.
#[derive(Clone)]
pub struct ConsoleWriteSender {
    tx: mpsc::Sender<WriteRequest>,
    notify: std::sync::Arc<tokio::sync::Notify>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConsoleWriteError;

impl ConsoleWriteSender {
    pub fn try_send(&self, data: Vec<u8>) -> Result<(), ConsoleWriteError> {
        self.try_send_with_pacing(data, WritePacing::Immediate)
    }

    pub async fn send(&self, data: Vec<u8>) -> Result<(), ConsoleWriteError> {
        self.tx
            .send(WriteRequest {
                data,
                pacing: WritePacing::Immediate,
            })
            .await
            .map_err(|_| ConsoleWriteError)?;
        self.notify.notify_one();
        Ok(())
    }

    pub(crate) fn try_send_command(&self, data: Vec<u8>) -> Result<(), ConsoleWriteError> {
        self.try_send_with_pacing(data, WritePacing::Command)
    }

    fn try_send_with_pacing(
        &self,
        data: Vec<u8>,
        pacing: WritePacing,
    ) -> Result<(), ConsoleWriteError> {
        self.tx
            .try_send(WriteRequest { data, pacing })
            .map_err(|_| ConsoleWriteError)?;
        self.notify.notify_one();
        Ok(())
    }
}

pub struct SerialConsoleDriver {
    host: String,
    /// TCP port number as string (from .target.jsonc), always ser2net
    target: String,
    stream: Option<TcpStream>,
    connected: bool,
    /// Write-request channel — CommandQueue sends data through this; the read loop performs the actual write
    write_tx: ConsoleWriteSender,
    write_rx: mpsc::Receiver<WriteRequest>,
    /// Signals that a write was enqueued; see [`SerialConsoleDriver::wait_writable`].
    write_notify: std::sync::Arc<tokio::sync::Notify>,
    mode: ConsoleMode,
    /// Number of UART copies emitted for each logical payload byte. Some
    /// hardware bridges only forward one byte from every repeated pair.
    tx_repeat: usize,
    /// Delay between repeated logical-byte groups.
    tx_gap: Duration,
    telnet: TelnetDecoder,
    /// true once COM-PORT-OPTION (44) is active on THIS connection.
    negotiated: bool,
    /// Pending NOTIFY-BAUDRATE ack — resolved when the server confirms a
    /// `set_baud` request. None when no request is in flight.
    baud_ack_tx: Option<tokio::sync::oneshot::Sender<u32>>,
}

impl SerialConsoleDriver {
    pub fn new(host: String, target: String) -> Self {
        Self::new_with_mode(host, target, ConsoleMode::Raw)
    }

    pub fn new_with_mode(host: String, target: String, mode: ConsoleMode) -> Self {
        Self::new_with_transport(host, target, mode, 1, Duration::ZERO)
    }

    pub fn new_with_transport(
        host: String,
        target: String,
        mode: ConsoleMode,
        tx_repeat: usize,
        tx_gap: Duration,
    ) -> Self {
        let (tx, rx) = mpsc::channel(WRITE_CHANNEL_DEPTH);
        let write_notify = std::sync::Arc::new(tokio::sync::Notify::new());
        Self {
            host,
            target,
            stream: None,
            connected: false,
            write_tx: ConsoleWriteSender {
                tx,
                notify: write_notify.clone(),
            },
            write_rx: rx,
            write_notify,
            mode,
            tx_repeat: tx_repeat.max(1),
            tx_gap,
            telnet: TelnetDecoder::new(),
            negotiated: false,
            baud_ack_tx: None,
        }
    }

    pub fn is_open(&self) -> bool {
        self.connected
    }

    /// Get a clone of the write channel (used for the CommandQueue write_fn)
    pub fn write_sender(&self) -> ConsoleWriteSender {
        self.write_tx.clone()
    }

    /// Current per-byte TX repeat factor (1 = plain send).
    pub fn tx_repeat(&self) -> usize {
        self.tx_repeat
    }

    /// Set the per-byte TX repeat factor. The login auto-probe sets this
    /// to 2 once a link that drops alternating TX bytes is confirmed.
    pub fn set_tx_repeat(&mut self, repeat: usize) {
        self.tx_repeat = repeat.max(1);
    }

    /// Test hook: mark the console open/closed without a live socket so
    /// watchdog-probe tests can observe writes through the channel.
    #[cfg(test)]
    pub fn set_open_for_test(&mut self, open: bool) {
        self.connected = open;
    }

    /// Test hook: drain one pending write from the write channel (None when
    /// nothing was enqueued since the last poll).
    #[cfg(test)]
    pub fn poll_pending_write(&mut self) -> Option<Vec<u8>> {
        self.write_rx.try_recv().ok().map(|request| request.data)
    }

    /// Connect (or reconnect) to ser2net.
    ///
    /// `timeout_secs` defaults to 5s for normal use. Fast-retry paths (e.g.
    /// handle_read_error) pass 2s to avoid holding the engine lock too long.
    pub async fn connect(&mut self) -> Result<(), std::io::Error> {
        self.connect_with_timeout(5).await
    }

    /// Like `connect()` but with an explicit timeout in seconds.
    pub async fn connect_with_timeout(&mut self, timeout_secs: u64) -> Result<(), std::io::Error> {
        self.stream.take();
        self.connected = false;

        let port: u16 = self.target.parse().map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "invalid dut.serial.port {:?}; set it in .target.jsonc with dutabo init",
                    self.target
                ),
            )
        })?;
        let addr = format!("{}:{}", self.host, port);
        let stream = tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs),
            TcpStream::connect(&addr),
        )
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "connect timeout"))??;
        stream.set_nodelay(true).ok();
        #[cfg(target_os = "linux")]
        {
            let socket = socket2::SockRef::from(&stream);
            socket.set_keepalive(true)?;
            let keepalive = socket2::TcpKeepalive::new()
                .with_time(std::time::Duration::from_secs(30))
                .with_interval(std::time::Duration::from_secs(5))
                .with_retries(3);
            socket.set_tcp_keepalive(&keepalive)?;
        }
        self.stream = Some(stream);
        self.connected = true;
        self.telnet.reset();
        self.negotiated = false;
        self.baud_ack_tx = None;
        if self.mode == ConsoleMode::Rfc2217 {
            self.negotiate().await;
        }
        Ok(())
    }

    /// RFC 2217 negotiation: send IAC DO 44 + IAC WILL 44, then wait up to
    /// NEGOTIATE_TIMEOUT for ser2net's answering WILL/DO. On success sets
    /// `negotiated = true`. On timeout or I/O error falls back to raw
    /// passthrough — connect still returns Ok. Never blocks > ~600ms.
    async fn negotiate(&mut self) {
        let Some(stream) = self.stream.as_mut() else {
            return;
        };
        if let Err(e) =
            tokio::io::AsyncWriteExt::write_all(stream, &[0xFF, 0xFD, 0x2C, 0xFF, 0xFB, 0x2C]).await
        {
            tracing::warn!("RFC 2217 negotiation write failed: {e}");
            self.connected = false;
            return;
        }
        let deadline = tokio::time::Instant::now() + NEGOTIATE_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                tracing::warn!("RFC 2217 negotiation failed — falling back to raw passthrough");
                return;
            }
            let chunk = match self
                .read_available(remaining.min(Duration::from_millis(100)), 256)
                .await
            {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("RFC 2217 negotiation I/O error: {e}");
                    return;
                }
            };
            if chunk.is_empty() {
                continue;
            }
            // `negotiated` is still false here, so read_available returned raw
            // bytes. Payload events (ser2net banner / early boot output) are
            // discarded — the engine's probe_initial_state re-reads post-connect.
            for ev in self.telnet.decode(&chunk) {
                if matches!(ev, TelnetEvent::PortOptionActive) {
                    self.negotiated = true;
                    tracing::info!("RFC 2217 negotiated (COM-PORT-OPTION)");
                    return;
                }
            }
        }
    }

    /// Close the connection
    pub fn close(&mut self) {
        self.stream.take();
        self.connected = false;
        self.telnet.reset();
        self.negotiated = false;
        self.baud_ack_tx = None;
    }

    /// Change the console baud rate via RFC 2217 SET-BAUDRATE + NOTIFY-BAUDRATE
    /// ack. Bounded at ~1s by SET_BAUD_ACK_TIMEOUT. The engine read loop is
    /// parked while the caller holds the engine lock, so this drives socket
    /// reads itself until the ack arrives.
    pub async fn set_baud(&mut self, baud: u32) -> Result<u32, SerialError> {
        if self.mode != ConsoleMode::Rfc2217 {
            return Err(SerialError::InvalidConfig(
                "RFC 2217 not enabled for this DUT (set rfc2217 = true in [dut.serial])".into(),
            ));
        }
        if !self.connected || self.stream.is_none() {
            return Err(SerialError::ConnectionFailed(
                "serial console not connected".into(),
            ));
        }
        if !self.negotiated {
            return Err(SerialError::TelnetNegotiation(
                "COM-PORT-OPTION not negotiated (ser2net port is raw)".into(),
            ));
        }
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        self.baud_ack_tx = Some(tx);
        let wire = telnet_filter::rfc2217_set_baudrate(baud);
        let Some(stream) = self.stream.as_mut() else {
            self.baud_ack_tx = None;
            return Err(SerialError::ConnectionFailed(
                "serial console not connected".into(),
            ));
        };
        if let Err(e) = tokio::io::AsyncWriteExt::write_all(stream, &wire).await {
            self.baud_ack_tx = None;
            self.connected = false;
            return Err(SerialError::Io(e));
        }
        let deadline = tokio::time::Instant::now() + SET_BAUD_ACK_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                self.baud_ack_tx = None;
                return Err(SerialError::NegotiationTimeout);
            }
            match self
                .read_available(remaining.min(Duration::from_millis(100)), 256)
                .await
            {
                Ok(_) => {
                    // read_available already ran the decoder: a NOTIFY-BAUDRATE
                    // subneg resolves `baud_ack_tx` via the oneshot.
                    if let Ok(actual) = rx.try_recv() {
                        if actual != baud {
                            return Err(SerialError::BaudMismatch {
                                requested: baud,
                                actual,
                            });
                        }
                        return Ok(actual);
                    }
                }
                Err(e) => {
                    self.baud_ack_tx = None;
                    self.connected = false;
                    return Err(SerialError::Io(e));
                }
            }
        }
    }

    /// Asynchronously send a text line (via the channel). Returns whether it
    /// was successfully queued — returns false (and drops the data) when the
    /// channel is full, so backpressure never blocks the caller.
    /// The caller (heartbeat probe) must use the return value to decide
    /// whether to count the probe as "sent".
    pub fn sendline(&self, line: &str) -> bool {
        let data = format!("{line}\n");
        self.write_tx.try_send(data.into_bytes()).is_ok()
    }

    /// Asynchronously send a control character (Ctrl-C = 0x03, etc.)
    /// Silently dropped when the channel is full (a lost heartbeat/probe
    /// character never affects correctness).
    pub fn sendcontrol(&self, ch: char) {
        let ch = ch.to_ascii_lowercase();
        if let Some(idx) = (b'a'..=b'z').position(|c| c == ch as u8) {
            let ctrl_byte = (idx as u8) + 1;
            self.write_tx.try_send(vec![ctrl_byte]).ok();
        }
    }

    /// Write directly to the TCP stream (bypasses the channel, for urgent Ctrl-C flood)
    pub async fn write_raw(&mut self, data: &[u8]) {
        if let Some(ref mut stream) = self.stream
            && let Err(e) = write_payload(
                stream,
                data,
                self.mode == ConsoleMode::Rfc2217 && self.negotiated,
                WritePacing::Immediate,
                self.tx_repeat,
                self.tx_gap,
            )
            .await
        {
            tracing::warn!("write_raw failed: {e}");
            self.connected = false;
        }
    }

    /// Drain all queued write requests (called by the read loop)
    /// Mirrors labgrid SerialDriver._write() — blocking writes via write_all.
    /// On write failure the data is dropped (not re-queued, preventing
    /// unbounded buildup while disconnected).
    pub async fn drain_writes(&mut self) {
        let stream = match self.stream.as_mut() {
            Some(s) => s,
            None => {
                while self.write_rx.try_recv().is_ok() {}
                return;
            }
        };

        while let Ok(request) = self.write_rx.try_recv() {
            let pacing = request.pacing;
            let logical_bytes = request.data.len();
            let rfc2217 = self.mode == ConsoleMode::Rfc2217 && self.negotiated;
            let wire_bytes = encoded_payload_len(&request.data, rfc2217, self.tx_repeat);
            if let Err(e) = write_payload(
                stream,
                &request.data,
                rfc2217,
                pacing,
                self.tx_repeat,
                self.tx_gap,
            )
            .await
            {
                tracing::warn!("drain_writes: write_all failed: {e}");
                self.connected = false;
                // Drop failed data — do not re-queue (prevents unbounded growth).
                return;
            }
            tracing::trace!(
                pacing = ?pacing,
                logical_bytes,
                wire_bytes,
                tx_repeat = self.tx_repeat,
                "drain_writes: write completed"
            );
        }
    }

    /// Event-driven wait for readability (tokio readable / epoll, no polling)
    pub async fn wait_readable(&self, timeout: Duration) -> bool {
        match &self.stream {
            Some(s) => matches!(
                tokio::time::timeout(timeout, s.readable()).await,
                Ok(Ok(()))
            ),
            None => {
                // No connection: sleep to avoid busy-waiting
                tokio::time::sleep(timeout.min(Duration::from_secs(5))).await;
                false
            }
        }
    }

    /// Wait until a producer enqueues a write.
    ///
    /// The read loop selects on this alongside [`Self::wait_readable`] so a
    /// queued keystroke (WebSocket relay input) is flushed to the wire within
    /// scheduler latency instead of waiting out the readability timeout. The
    /// mpsc queue is the source of truth — a missed notification only falls
    /// back to the loop's entry `drain_writes`, never loses data.
    pub async fn wait_writable(&self) {
        self.write_notify.notified().await;
    }

    /// Read available data (blocking until data arrives or the timeout expires)
    ///
    /// **Concurrency contract**: this reads the raw TCP socket, so the caller
    /// must be the SOLE reader — hold the engine lock for the entire read
    /// while the read loop is parked. The startup probe, `negotiate`,
    /// `set_baud`, `send_and_expect`/`probe_initial_state` and
    /// `capture_boot_output` all satisfy this (their callers hold the
    /// engine lock, or run before the read loop exists). Never use it for a
    /// liveness probe that waits with the lock released: the read loop
    /// consumes the response first and the probe false-negatives (A2 — a live
    /// DUT was downgraded to DUT-off). Such probes must observe the shared
    /// ring buffer instead (`SerialEngine::shared_stream_snapshot` /
    /// `shared_stream_has_new`, see `probe_after_command_timeout`).
    ///
    /// Returns:
    /// - `Ok(data)` with non-empty data: bytes were read
    /// - `Ok(data)` with empty data: timeout, no data
    /// - `Err`: connection error
    pub async fn read_available(
        &mut self,
        timeout: Duration,
        max_size: usize,
    ) -> Result<Vec<u8>, std::io::Error> {
        let stream = self.stream.as_mut().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotConnected, "not connected")
        })?;

        let mut buf = vec![0u8; max_size];
        match tokio::time::timeout(timeout, stream.read(&mut buf)).await {
            Ok(Ok(0)) => {
                // TCP connection closed by peer
                self.connected = false;
                Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "Connection closed by ser2net",
                ))
            }
            Ok(Ok(n)) => {
                buf.truncate(n);
                if self.mode == ConsoleMode::Rfc2217 && self.negotiated {
                    Ok(self.decode_events(&buf))
                } else {
                    Ok(buf)
                }
            }
            Ok(Err(e)) => {
                self.connected = false;
                Err(e)
            }
            Err(_) => {
                // Timeout, no data
                Ok(Vec::new())
            }
        }
    }

    /// Run raw bytes through the telnet decoder; concatenate all Payload
    /// events (order preserved). Handles RFC 2217 subnegotiations:
    /// - NOTIFY-BAUDRATE (101) resolves the pending `baud_ack_tx` oneshot.
    /// - FLOWCONTROL-SUSPEND/RESUME (108/109) are informational only.
    ///   Everything else is ignored.
    fn decode_events(&mut self, buf: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(buf.len());
        for ev in self.telnet.decode(buf) {
            match ev {
                TelnetEvent::Payload(p) => out.extend_from_slice(&p),
                TelnetEvent::Subnegotiation { option, data } => {
                    if option != iac::COM_PORT_OPTION {
                        continue;
                    }
                    match telnet_filter::parse_rfc2217_subneg(&data) {
                        Some((iac::CMD_NOTIFY_BAUDRATE, rest)) if rest.len() == 4 => {
                            let actual = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]);
                            if let Some(tx) = self.baud_ack_tx.take() {
                                let _ = tx.send(actual);
                            }
                        }
                        Some((iac::CMD_FLOWCONTROL_SUSPEND | iac::CMD_FLOWCONTROL_RESUME, _)) => {
                            tracing::debug!("RFC 2217 flow-control suspend/resume");
                        }
                        other => {
                            tracing::debug!("RFC 2217 COM-PORT-OPTION subneg ignored: {other:?}");
                        }
                    }
                }
                TelnetEvent::Will(_)
                | TelnetEvent::Wont(_)
                | TelnetEvent::Do(_)
                | TelnetEvent::Dont(_)
                | TelnetEvent::PortOptionActive => {}
            }
        }
        out
    }
}

/// Write one already-encoded wire request.  RFC 2217 stuffing is deliberately
/// completed before this function so chunk boundaries cannot change the
/// logical byte stream (`0xff` remains `0xff 0xff` on the wire).
async fn write_wire<W: AsyncWrite + Unpin>(
    writer: &mut W,
    wire: &[u8],
    pacing: WritePacing,
) -> std::io::Result<()> {
    if pacing == WritePacing::Immediate || wire.len() <= COMMAND_WRITE_CHUNK_BYTES {
        return writer.write_all(wire).await;
    }

    let mut chunks = wire.chunks(COMMAND_WRITE_CHUNK_BYTES).peekable();
    while let Some(chunk) = chunks.next() {
        writer.write_all(chunk).await?;
        if chunks.peek().is_some() {
            // Async sleep yields the executor; it does not block the runtime
            // thread.  SerialConsoleDriver's exclusive &mut borrow remains the
            // single writer and prevents another producer from interleaving
            // bytes inside this marker-wrapped command.
            tokio::time::sleep(COMMAND_WRITE_GAP).await;
        }
    }
    Ok(())
}

fn encoded_payload_len(data: &[u8], rfc2217: bool, tx_repeat: usize) -> usize {
    let repeated = data.len().saturating_mul(tx_repeat.max(1));
    if rfc2217 {
        repeated.saturating_add(
            data.iter()
                .filter(|byte| **byte == iac::IAC)
                .count()
                .saturating_mul(tx_repeat.max(1)),
        )
    } else {
        repeated
    }
}

async fn write_payload<W: AsyncWrite + Unpin>(
    writer: &mut W,
    logical: &[u8],
    rfc2217: bool,
    pacing: WritePacing,
    tx_repeat: usize,
    tx_gap: Duration,
) -> std::io::Result<()> {
    let tx_repeat = tx_repeat.max(1);
    if tx_repeat == 1 {
        let wire = if rfc2217 {
            telnet_filter::stuff(logical)
        } else {
            logical.to_vec()
        };
        return write_wire(writer, &wire, pacing).await;
    }

    for (index, byte) in logical.iter().enumerate() {
        let repeated = vec![*byte; tx_repeat];
        let wire = if rfc2217 {
            telnet_filter::stuff(&repeated)
        } else {
            repeated
        };
        writer.write_all(&wire).await?;
        if index + 1 < logical.len() {
            tokio::time::sleep(tx_gap).await;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::assert_matches;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[derive(Clone)]
    struct TraceBuffer(Arc<Mutex<Vec<u8>>>);

    struct TraceWriter(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for TraceWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for TraceBuffer {
        type Writer = TraceWriter;

        fn make_writer(&'a self) -> Self::Writer {
            TraceWriter(Arc::clone(&self.0))
        }
    }

    /// Models a bridge that accepts a TCP write but whose downstream UART
    /// FIFO preserves only the first `burst_capacity` bytes of each burst.
    /// Returning the full input length is intentional: it reproduces the
    /// field symptom where TCP `write_all` succeeds while target echo proves
    /// that downstream bytes were dropped.
    struct BurstLimitedSink {
        burst_capacity: usize,
        received: Vec<u8>,
    }

    impl BurstLimitedSink {
        fn new(burst_capacity: usize) -> Self {
            Self {
                burst_capacity,
                received: Vec::new(),
            }
        }
    }

    impl AsyncWrite for BurstLimitedSink {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            let preserved = buf.len().min(self.burst_capacity);
            self.received.extend_from_slice(&buf[..preserved]);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// Models the observed dual-serial adapter behavior: for each downstream
    /// write, only the first byte from each adjacent pair reaches the DUT.
    struct AlternateByteSink {
        received: Vec<u8>,
    }

    impl AsyncWrite for AlternateByteSink {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            self.received.extend(buf.iter().step_by(2));
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn test_new_console() {
        let console = SerialConsoleDriver::new("127.0.0.1".into(), "12345".into());
        assert!(!console.is_open());
    }

    /// Every enqueue must poke the write notify so the read loop can flush
    /// relay keystrokes immediately instead of at its polling cadence —
    /// this is the latency fix for `dutabo serial` typed-echo lag.
    #[tokio::test]
    async fn test_enqueue_wakes_wait_writable() {
        let console = SerialConsoleDriver::new("127.0.0.1".into(), "12345".into());
        let sender = console.write_sender();

        // notify_one stores a permit when nothing is waiting yet, so the
        // FIRST waiter after an enqueue completes without ever parking.
        sender.try_send(b"x".to_vec()).unwrap();
        console.wait_writable().await;

        // Without a new enqueue a second wait must stay pending.
        let pending =
            tokio::time::timeout(Duration::from_millis(20), console.wait_writable()).await;
        assert!(
            pending.is_err(),
            "wait_writable must not complete without an enqueue"
        );

        // A fresh enqueue wakes it again.
        sender.try_send(b"y".to_vec()).unwrap();
        console.wait_writable().await;
    }

    #[tokio::test]
    async fn test_connect_close() {
        // Start a mock TCP server
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server_handle = tokio::spawn(async move {
            let (_socket, _) = listener.accept().await.unwrap();
            // Keep connection alive briefly
            tokio::time::sleep(Duration::from_millis(100)).await;
        });

        let mut console = SerialConsoleDriver::new("127.0.0.1".into(), port.to_string());
        assert!(!console.is_open());

        console.connect().await.unwrap();
        assert!(console.is_open());

        console.close();
        assert!(!console.is_open());

        server_handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_connect_failure() {
        let mut console = SerialConsoleDriver::new("127.0.0.1".to_string(), "59999".into());
        let result = console.connect().await;
        assert!(result.is_err());
        assert!(!console.is_open());
    }

    #[tokio::test]
    async fn test_sendline() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server_handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 100];
            let n = socket.read(&mut buf).await.unwrap();
            String::from_utf8_lossy(&buf[..n]).to_string()
        });

        let mut console = SerialConsoleDriver::new("127.0.0.1".into(), port.to_string());
        console.connect().await.unwrap();

        console.sendline("test command");

        // Drain writes to actually send
        console.drain_writes().await;

        // Give time for data to arrive
        tokio::time::sleep(Duration::from_millis(50)).await;

        console.close();
        let received = server_handle.await.unwrap();
        assert_eq!(received, "test command\n");
    }

    #[tokio::test]
    async fn test_sendcontrol() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server_handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 10];
            let n = socket.read(&mut buf).await.unwrap();
            buf[..n].to_vec()
        });

        let mut console = SerialConsoleDriver::new("127.0.0.1".into(), port.to_string());
        console.connect().await.unwrap();

        console.sendcontrol('c'); // Ctrl-C = 0x03
        console.drain_writes().await;

        tokio::time::sleep(Duration::from_millis(50)).await;
        console.close();

        let received = server_handle.await.unwrap();
        assert_eq!(received, vec![0x03]);
    }

    #[tokio::test]
    async fn test_read_available_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server_handle = tokio::spawn(async move {
            let (_socket, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
            // Don't send anything
        });

        let mut console = SerialConsoleDriver::new("127.0.0.1".into(), port.to_string());
        console.connect().await.unwrap();

        let result = console
            .read_available(Duration::from_millis(50), 1024)
            .await
            .unwrap();

        assert!(result.is_empty()); // Should timeout with empty data

        server_handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_read_available_data() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server_handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
            socket.write_all(b"test data").await.unwrap();
            socket.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
        });

        let mut console = SerialConsoleDriver::new("127.0.0.1".into(), port.to_string());
        console.connect().await.unwrap();

        let result = console
            .read_available(Duration::from_millis(100), 1024)
            .await
            .unwrap();

        assert_eq!(result, b"test data");

        server_handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_write_sender_clone() {
        let console = SerialConsoleDriver::new("127.0.0.1".into(), "12345".into());
        let sender1 = console.write_sender();
        let sender2 = console.write_sender();

        // Both senders should be valid (try_send on bounded channel)
        assert!(sender1.try_send(b"test1".to_vec()).is_ok());
        assert!(sender2.try_send(b"test2".to_vec()).is_ok());
    }

    #[tokio::test]
    async fn test_drain_writes_no_connection() {
        let mut console = SerialConsoleDriver::new("127.0.0.1".into(), "12345".into());

        // Send data without connecting
        console.write_tx.try_send(b"test".to_vec()).ok();

        // Should not panic, just discard
        console.drain_writes().await;
    }

    #[tokio::test]
    async fn test_multiple_writes() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server_handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 1000];
            let mut total = 0;
            for _ in 0..3 {
                let n = socket.read(&mut buf[total..]).await.unwrap();
                total += n;
            }
            String::from_utf8_lossy(&buf[..total]).to_string()
        });

        let mut console = SerialConsoleDriver::new("127.0.0.1".into(), port.to_string());
        console.connect().await.unwrap();

        console.sendline("cmd1");
        console.sendline("cmd2");
        console.sendline("cmd3");

        console.drain_writes().await;

        tokio::time::sleep(Duration::from_millis(100)).await;
        console.close();

        let received = server_handle.await.unwrap();
        assert!(received.contains("cmd1"));
        assert!(received.contains("cmd2"));
        assert!(received.contains("cmd3"));
    }

    /// Regression for the RK3576 field trace where a complete marker command
    /// was accepted by TCP but echoed as `co'UZ'JJW;{eh ...`: an oversized
    /// downstream burst loses bytes even though write_all reports success.
    /// Command pacing must preserve the exact order, both marker halves, and
    /// RFC2217 IAC stuffing on that same simulated sink.
    #[tokio::test]
    async fn test_command_pacing_survives_burst_capacity_and_preserves_iac() {
        let marker = "SUZKJJWQAA";
        let marker_line = format!(
            "echo '{}''{}'; {{ systemctl start --no-block ubuntu-firstboot.service; }}; \
             echo \"EXIT:$?\"; echo '{}''{}'\n",
            &marker[..4],
            &marker[4..],
            &marker[..4],
            &marker[4..],
        );
        let mut logical = marker_line.as_bytes().to_vec();
        logical.push(0xff);
        let wire = telnet_filter::stuff(&logical);

        let mut old_whole_burst = BurstLimitedSink::new(COMMAND_WRITE_CHUNK_BYTES);
        write_wire(&mut old_whole_burst, &wire, WritePacing::Immediate)
            .await
            .unwrap();
        assert_eq!(old_whole_burst.received.len(), COMMAND_WRITE_CHUNK_BYTES);
        assert_ne!(old_whole_burst.received, wire);
        assert!(
            !old_whole_burst
                .received
                .windows(marker.as_bytes()[4..].len())
                .any(|window| window == &marker.as_bytes()[4..]),
            "the old whole-burst path must stably reproduce marker loss"
        );

        let mut paced = BurstLimitedSink::new(COMMAND_WRITE_CHUNK_BYTES);
        write_wire(&mut paced, &wire, WritePacing::Command)
            .await
            .unwrap();
        assert_eq!(
            paced.received, wire,
            "paced bytes must remain complete and ordered"
        );
        assert!(
            paced
                .received
                .windows(marker.as_bytes()[..4].len())
                .any(|w| w == &marker.as_bytes()[..4])
        );
        assert!(
            paced
                .received
                .windows(marker.as_bytes()[4..].len())
                .any(|w| w == &marker.as_bytes()[4..])
        );
        assert_eq!(
            paced.received.last_chunk::<2>(),
            Some(&[0xff, 0xff]),
            "RFC2217 must stuff literal 0xff before paced chunking"
        );
    }

    #[tokio::test]
    async fn test_repeated_byte_transport_survives_alternate_byte_loss() {
        let logical = b"echo SERIAL_DOUBLE_OK\r";

        let mut ordinary = AlternateByteSink {
            received: Vec::new(),
        };
        write_payload(
            &mut ordinary,
            logical,
            false,
            WritePacing::Command,
            1,
            Duration::ZERO,
        )
        .await
        .unwrap();
        assert_ne!(ordinary.received, logical);

        let mut repeated = AlternateByteSink {
            received: Vec::new(),
        };
        write_payload(
            &mut repeated,
            logical,
            false,
            WritePacing::Command,
            2,
            Duration::ZERO,
        )
        .await
        .unwrap();
        assert_eq!(repeated.received, logical);
    }

    #[tokio::test]
    async fn test_repeated_byte_transport_preserves_rfc2217_stuffing() {
        let mut wire = Vec::new();
        write_payload(
            &mut wire,
            &[b'x', iac::IAC],
            true,
            WritePacing::Immediate,
            2,
            Duration::ZERO,
        )
        .await
        .unwrap();
        assert_eq!(wire, b"xx\xff\xff\xff\xff");
        assert_eq!(encoded_payload_len(&[b'x', iac::IAC], true, 2), 6);
    }

    /// A successful drain emits enough metadata to distinguish immediate vs.
    /// paced and logical vs. RFC2217 wire bytes, without logging the payload.
    #[tokio::test(flavor = "current_thread")]
    async fn test_drain_writes_trace_counts_bytes_without_payload() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let logical = b"PRIVATE-PAYLOAD\xff".to_vec();
        let wire = telnet_filter::stuff(&logical);
        assert_ne!(logical.len(), wire.len());

        let expected_wire = wire.clone();
        let server_handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut received = vec![0; expected_wire.len()];
            socket.read_exact(&mut received).await.unwrap();
            received
        });

        let captured = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(TraceBuffer(Arc::clone(&captured)))
            .finish();
        tracing::subscriber::set_global_default(subscriber).unwrap();

        let mut console = SerialConsoleDriver::new("127.0.0.1".into(), port.to_string());
        console.connect().await.unwrap();
        console.mode = ConsoleMode::Rfc2217;
        console.negotiated = true;
        console
            .write_sender()
            .try_send_command(logical.clone())
            .unwrap();
        console.drain_writes().await;

        assert_eq!(server_handle.await.unwrap(), wire);
        let trace = String::from_utf8(captured.lock().unwrap().clone()).unwrap();
        assert!(trace.contains("drain_writes: write completed"));
        assert!(trace.contains("pacing=Command"));
        assert!(trace.contains(&format!("logical_bytes={}", logical.len())));
        assert!(trace.contains(&format!("wire_bytes={}", wire.len())));
        assert!(!trace.contains("PRIVATE-PAYLOAD"));
    }

    /// Routing guard: only CommandQueue may opt into paced writes. Raw bytes
    /// and control characters keep the immediate path used by interactive
    /// input, reset/boot helpers and interrupt floods.
    #[test]
    fn test_write_routing_paces_commands_but_not_raw_or_control() {
        let mut console = SerialConsoleDriver::new("127.0.0.1".into(), "12345".into());
        let sender = console.write_sender();

        assert!(
            sender
                .try_send_command(b"marker command\n".to_vec())
                .is_ok()
        );
        assert!(sender.try_send(b"raw input".to_vec()).is_ok());
        console.sendcontrol('c');

        let command = console.write_rx.try_recv().unwrap();
        let raw = console.write_rx.try_recv().unwrap();
        let control = console.write_rx.try_recv().unwrap();
        assert_eq!(command.pacing, WritePacing::Command);
        assert_eq!(raw.pacing, WritePacing::Immediate);
        assert_eq!(control.pacing, WritePacing::Immediate);
        assert_eq!(control.data, vec![0x03]);
    }

    /// Bounded channel silently drops writes when full — no unbounded memory growth.
    #[test]
    fn test_bounded_channel_drops_on_full() {
        let console = SerialConsoleDriver::new("127.0.0.1".into(), "12345".into());
        // Fill the channel to capacity (WRITE_CHANNEL_DEPTH = 64)
        for i in 0..64 {
            assert!(console.write_tx.try_send(vec![i as u8]).is_ok());
        }
        // 65th write should silently fail (channel full)
        assert!(console.write_tx.try_send(vec![65]).is_err());
    }

    /// Verify that failed writes are NOT re-queued: drain_writes sets
    /// connected=false and returns, discarding the failed data.
    #[tokio::test]
    async fn test_no_requeue_on_write_failure() {
        // No server — connect will fail, write_raw will mark disconnected
        let mut console = SerialConsoleDriver::new("127.0.0.1".to_string(), "59999".into());
        // Queue a write
        console
            .write_tx
            .try_send(b"should_be_discarded".to_vec())
            .ok();
        // drain_writes with no connection should drain and discard
        console.drain_writes().await;
        // The channel should now be empty (write was discarded, not re-queued)
        assert!(console.write_rx.try_recv().is_err());
    }

    /// Verify that sendline and sendcontrol use try_send (non-blocking).
    #[test]
    fn test_sendline_uses_try_send() {
        let mut console = SerialConsoleDriver::new("127.0.0.1".into(), "12345".into());
        // Should not block even with no receiver draining
        console.sendline("test");
        console.sendcontrol('c');
        // Both should have been enqueued
        assert!(console.write_rx.try_recv().is_ok());
        assert!(console.write_rx.try_recv().is_ok());
    }

    /// sendline must report whether the write was enqueued — the watchdog
    /// relies on the return value to only mark probes "sent" that actually
    /// reached the channel (a full channel silently drops the write).
    #[test]
    fn test_sendline_reports_drop_on_full_channel() {
        let mut console = SerialConsoleDriver::new("127.0.0.1".into(), "12345".into());

        // Fill the bounded channel to capacity: every line enqueues.
        while console.sendline("fill") {}
        // Channel is full: this probe must be reported as dropped.
        assert!(
            !console.sendline("probe"),
            "sendline must return false when the write channel drops the line"
        );
        // Free one slot; the next sendline must succeed again.
        assert!(console.write_rx.try_recv().is_ok());
        assert!(console.sendline("probe"));
    }

    #[test]
    fn test_sendcontrol_byte_calculation() {
        // Ctrl-A = 0x01, Ctrl-B = 0x02, ..., Ctrl-Z = 0x1A
        let test_cases = [
            ('a', 0x01),
            ('b', 0x02),
            ('c', 0x03),
            ('x', 0x18),
            ('y', 0x19),
            ('z', 0x1A),
        ];

        for (ch, expected) in test_cases {
            let idx = (ch as u8) - b'a';
            let ctrl_byte = idx + 1;
            assert_eq!(ctrl_byte, expected);
        }
    }

    #[tokio::test]
    async fn test_console_new_and_close() {
        let mut console = SerialConsoleDriver::new("127.0.0.1".to_string(), "9999".to_string());
        assert!(!console.is_open());
        console.close();
        assert!(!console.is_open());
    }

    #[test]
    fn test_console_default_state() {
        let console =
            SerialConsoleDriver::new("host.invalid".to_string(), "configured-port".to_string());
        assert!(!console.is_open());
    }

    // ── RFC 2217 ─────────────────────────────────────────────────────────────

    /// Mock ser2net: read the client's DO/WILL probes, answer with our own
    /// DO 44 + WILL 44 (either direction triggers port_option_active).
    async fn server_handshake(socket: &mut tokio::net::TcpStream) {
        let mut buf = [0u8; 6];
        socket.read_exact(&mut buf).await.unwrap();
        assert_eq!(
            buf.to_vec(),
            vec![0xFF, 0xFD, 0x2C, 0xFF, 0xFB, 0x2C],
            "client must send IAC DO 44 + IAC WILL 44"
        );
        socket
            .write_all(&[0xFF, 0xFD, 0x2C, 0xFF, 0xFB, 0x2C])
            .await
            .unwrap();
        socket.flush().await.unwrap();
    }

    #[tokio::test]
    async fn test_rfc2217_negotiate_success() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server_handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            server_handshake(&mut socket).await;
            // Give the client's negotiate() time to return before payload.
            tokio::time::sleep(Duration::from_millis(300)).await;
            socket.write_all(b"prompt").await.unwrap();
            socket.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(500)).await;
        });

        let mut console = SerialConsoleDriver::new_with_mode(
            "127.0.0.1".into(),
            port.to_string(),
            ConsoleMode::Rfc2217,
        );
        console.connect_with_timeout(2).await.unwrap();
        assert!(console.negotiated, "COM-PORT-OPTION must be negotiated");

        // read_available must return decoded payload only.
        let mut collected = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline && collected.len() < b"prompt".len() {
            match console
                .read_available(Duration::from_millis(100), 256)
                .await
            {
                Ok(d) if !d.is_empty() => collected.extend_from_slice(&d),
                Ok(_) => {}
                Err(e) => panic!("read error: {e}"),
            }
        }
        assert_eq!(collected, b"prompt");

        console.close();
        server_handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_rfc2217_negotiate_timeout_falls_back_to_raw() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server_handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            // Stay silent through the 500ms negotiation window, then send raw.
            tokio::time::sleep(Duration::from_millis(900)).await;
            socket.write_all(b"raw bytes").await.unwrap();
            socket.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(500)).await;
        });

        let mut console = SerialConsoleDriver::new_with_mode(
            "127.0.0.1".into(),
            port.to_string(),
            ConsoleMode::Rfc2217,
        );
        console.connect_with_timeout(2).await.unwrap(); // Ok — fallback, not error
        assert!(!console.negotiated);

        let mut collected = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline && collected.len() < b"raw bytes".len() {
            match console
                .read_available(Duration::from_millis(100), 256)
                .await
            {
                Ok(d) if !d.is_empty() => collected.extend_from_slice(&d),
                Ok(_) => {}
                Err(e) => panic!("read error: {e}"),
            }
        }
        assert_eq!(
            collected, b"raw bytes",
            "raw fallback must pass bytes unmodified"
        );

        console.close();
        server_handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_rfc2217_junk_server_treated_as_raw() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server_handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            // Non-telnet junk during the negotiation window.
            socket
                .write_all(b"some plain banner text\r\n")
                .await
                .unwrap();
            socket.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(900)).await;
            socket.write_all(b"post-junk").await.unwrap();
            socket.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(500)).await;
        });

        let mut console = SerialConsoleDriver::new_with_mode(
            "127.0.0.1".into(),
            port.to_string(),
            ConsoleMode::Rfc2217,
        );
        console.connect_with_timeout(2).await.unwrap();
        assert!(!console.negotiated);

        let mut collected = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline && collected.len() < b"post-junk".len() {
            match console
                .read_available(Duration::from_millis(100), 256)
                .await
            {
                Ok(d) if !d.is_empty() => collected.extend_from_slice(&d),
                Ok(_) => {}
                Err(e) => panic!("read error: {e}"),
            }
        }
        assert_eq!(collected, b"post-junk");

        console.close();
        server_handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_set_baud_encoding_and_ack() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server_handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            server_handshake(&mut socket).await;
            // Expect exactly the SET-BAUDRATE wire bytes.
            let wire = vec![0xFF, 0xFA, 0x2C, 0x01, 0x00, 0x16, 0xE3, 0x60, 0xFF, 0xF0];
            let mut buf = vec![0u8; wire.len()];
            socket.read_exact(&mut buf).await.unwrap();
            assert_eq!(buf, wire);
            // NOTIFY-BAUDRATE ack (cmd 101 = 0x65) with the applied rate.
            socket
                .write_all(&[0xFF, 0xFA, 0x2C, 0x65, 0x00, 0x16, 0xE3, 0x60, 0xFF, 0xF0])
                .await
                .unwrap();
            socket.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(300)).await;
        });

        let mut console = SerialConsoleDriver::new_with_mode(
            "127.0.0.1".into(),
            port.to_string(),
            ConsoleMode::Rfc2217,
        );
        console.connect_with_timeout(2).await.unwrap();
        assert!(console.negotiated);

        let result = console.set_baud(1500000).await;
        assert_eq!(result.unwrap(), 1500000);

        console.close();
        server_handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_set_baud_mismatch_reports_actual() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server_handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            server_handshake(&mut socket).await;
            let mut buf = vec![0u8; 10];
            socket.read_exact(&mut buf).await.unwrap();
            // Ack with 115200 (0x0001C200) instead of the requested rate.
            socket
                .write_all(&[0xFF, 0xFA, 0x2C, 0x65, 0x00, 0x01, 0xC2, 0x00, 0xFF, 0xF0])
                .await
                .unwrap();
            socket.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(300)).await;
        });

        let mut console = SerialConsoleDriver::new_with_mode(
            "127.0.0.1".into(),
            port.to_string(),
            ConsoleMode::Rfc2217,
        );
        console.connect_with_timeout(2).await.unwrap();
        assert!(console.negotiated);

        let result = console.set_baud(1500000).await;
        match result {
            Err(SerialError::BaudMismatch {
                requested: 1500000,
                actual: 115200,
            }) => {}
            other => panic!("expected BaudMismatch, got {other:?}"),
        }
        assert!(
            console.is_open(),
            "connection remains usable after mismatch"
        );

        console.close();
        server_handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_set_baud_no_ack_times_out() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server_handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            server_handshake(&mut socket).await;
            let mut buf = vec![0u8; 10];
            socket.read_exact(&mut buf).await.unwrap();
            // Never ack.
            tokio::time::sleep(Duration::from_secs(3)).await;
        });

        let mut console = SerialConsoleDriver::new_with_mode(
            "127.0.0.1".into(),
            port.to_string(),
            ConsoleMode::Rfc2217,
        );
        console.connect_with_timeout(2).await.unwrap();
        assert!(console.negotiated);

        let result = console.set_baud(1500000).await;
        assert_matches!(result, Err(SerialError::NegotiationTimeout));

        console.close();
        server_handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_set_baud_rejected_in_raw_mode() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server_handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            // Client closes after the rejection — read everything until EOF.
            let mut total = 0usize;
            let mut buf = [0u8; 64];
            loop {
                match socket.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => total += n,
                }
            }
            total
        });

        let mut console = SerialConsoleDriver::new("127.0.0.1".into(), port.to_string());
        console.connect_with_timeout(2).await.unwrap();

        let result = console.set_baud(1500000).await;
        assert_matches!(result, Err(SerialError::InvalidConfig(_)));

        console.close();
        let received = server_handle.await.unwrap();
        assert_eq!(
            received, 0,
            "raw-mode driver must never write SET-BAUDRATE bytes"
        );
    }

    #[tokio::test]
    async fn test_write_unstuffed_before_negotiation() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server_handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            // Silent server: let the client's negotiation time out.
            tokio::time::sleep(Duration::from_millis(900)).await;
            let mut buf = [0u8; 64];
            let mut total = Vec::new();
            loop {
                match socket.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => total.extend_from_slice(&buf[..n]),
                }
            }
            total
        });

        let mut console = SerialConsoleDriver::new_with_mode(
            "127.0.0.1".into(),
            port.to_string(),
            ConsoleMode::Rfc2217,
        );
        console.connect_with_timeout(2).await.unwrap();
        assert!(!console.negotiated);

        console
            .write_tx
            .try_send(vec![b'x', 0xFF, b'y', b'\n'])
            .ok();
        console.drain_writes().await;
        console.close();

        let received = server_handle.await.unwrap();
        let mut expected = vec![0xFF, 0xFD, 0x2C, 0xFF, 0xFB, 0x2C]; // negotiation probes
        expected.extend_from_slice(b"x\xFFy\n");
        assert_eq!(received, expected, "stuffing is gated on negotiated");
    }

    #[tokio::test]
    async fn test_write_stuffed_after_negotiation() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server_handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            server_handshake(&mut socket).await;
            let mut buf = [0u8; 64];
            let mut total = Vec::new();
            loop {
                match socket.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => total.extend_from_slice(&buf[..n]),
                }
            }
            total
        });

        let mut console = SerialConsoleDriver::new_with_mode(
            "127.0.0.1".into(),
            port.to_string(),
            ConsoleMode::Rfc2217,
        );
        console.connect_with_timeout(2).await.unwrap();
        assert!(console.negotiated);

        console
            .write_tx
            .try_send(vec![b'x', 0xFF, b'y', b'\n'])
            .ok();
        console.drain_writes().await;
        console.close();

        let received = server_handle.await.unwrap();
        assert_eq!(
            received,
            b"x\xFF\xFFy\n".to_vec(),
            "0xFF must be IAC-escaped after negotiation"
        );
    }
}
