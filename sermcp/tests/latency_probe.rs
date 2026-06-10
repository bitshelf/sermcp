//! Live-hardware interactive latency probe.
//!
//! Measures keystroke→echo RTT on the serial console in two modes:
//! - `board_tcp_probe` — raw TCP straight to ser2net: isolates the
//!   board + dev-host + UART latency (the floor no sermcp change can beat).
//! - `ws_probe` — through the sermcp WebSocket relay: adds the engine read
//!   loop and the WS hops. Comparing the two attributes tail latency.
//!
//! Both probes type on an idle shell with local echo and clear the input line
//! with Ctrl-U (0x15). Run them explicitly:
//!
//! ```text
//! cargo test --test latency_probe -- --ignored --nocapture board_tcp_probe
//! cargo test --test latency_probe -- --ignored --nocapture ws_probe
//! ```
//!
//! Environment knobs (all optional): `PROBE_HOST` (192.168.1.105),
//! `PROBE_PORT` (2010), `PROBE_WS_PORT` (3038), `PROBE_DUT` (yr3588),
//! `PROBE_TOKEN` (read from `../.dut-serial/control-<port>.token` by default),
//! `PROBE_SINGLE_SAMPLES` (400), `PROBE_BURST_SAMPLES` (200),
//! `PROBE_BURST_BYTES` (10), `PROBE_GAP_MS` (60).
//!
//! `trace_dump` decodes a `.pftrace` captured via `SERMCP_TRACE` and prints
//! relay events in time order — the per-hop deltas attribute the tails.
//! Hop events exist only in `--features relay-trace` builds (default builds
//! compile the `relay_trace!` sites out entirely):
//!
//! ```text
//! cargo musl --features relay-trace
//! SERMCP_TRACE=/tmp/relay.pftrace sermcp --http 127.0.0.1:3038 &
//! TRACE_DUMP=/tmp/relay.pftrace \
//!   cargo test --test latency_probe trace_dump -- --ignored --nocapture
//! ```

use std::time::{Duration, Instant};

type SendFn =
    Box<dyn FnMut(&[u8]) -> futures_util::future::BoxFuture<'static, std::io::Result<()>> + Send>;
type RecvFn =
    Box<dyn FnMut() -> futures_util::future::BoxFuture<'static, std::io::Result<Vec<u8>>> + Send>;

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn env_num(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let idx = ((sorted.len() - 1) as f64 * q).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn report(label: &str, samples: &[f64]) {
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    println!(
        "{label:<24} samples={:>3} p50={:>8.2} p90={:>8.2} p99={:>8.2} max={:>9.2} (ms)",
        samples.len(),
        percentile(&sorted, 0.50),
        percentile(&sorted, 0.90),
        percentile(&sorted, 0.99),
        sorted.last().copied().unwrap_or(f64::NAN),
    );
}

async fn expect_echo(recv: &mut RecvFn, payload: &[u8]) -> std::io::Result<()> {
    let mut remaining = payload;
    while !remaining.is_empty() {
        let chunk = recv().await?;
        if chunk.is_empty() || !remaining.starts_with(&chunk) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unexpected echo {chunk:?}; expected prefix of {remaining:?}"),
            ));
        }
        remaining = &remaining[chunk.len()..];
    }
    Ok(())
}

async fn wait_for_setup_echo(recv: &mut RecvFn, prefix: &[u8]) -> std::io::Result<()> {
    let mut received = Vec::new();
    loop {
        received.extend(recv().await?);
        if let Some(offset) = received
            .windows(prefix.len())
            .position(|part| part == prefix)
        {
            if offset + prefix.len() == received.len() {
                return Ok(());
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "unexpected serial output after the setup echo",
            ));
        }
        // Retain only the bytes that could start a split setup echo.
        let discard = received.len().saturating_sub(prefix.len() - 1);
        received.drain(..discard);
    }
}

async fn measure(label_prefix: &str, mut send: SendFn, mut recv: RecvFn) {
    let deadline =
        tokio::time::Instant::now() + Duration::from_secs(env_num("PROBE_DEADLINE_SECS", 240));
    let gap = Duration::from_millis(env_num("PROBE_GAP_MS", 60));
    let burst_bytes = env_num("PROBE_BURST_BYTES", 10) as usize;
    assert!(
        (1..=40).contains(&burst_bytes),
        "PROBE_BURST_BYTES must fit on the input line"
    );
    let phases = [
        (
            "single-char".to_owned(),
            env_num("PROBE_SINGLE_SAMPLES", 400),
            1,
        ),
        (
            format!("burst-{burst_bytes}"),
            env_num("PROBE_BURST_SAMPLES", 200),
            burst_bytes,
        ),
    ];
    for (phase, count, width) in phases {
        let mut rtts = Vec::new();
        for i in 0..count {
            let sample = async {
                send(&[0x15]).await?;
                let prefix = format!("# {width:02x}-{i:08x} ");
                send(prefix.as_bytes()).await?;
                // Ctrl-U redraws may arrive late. Synchronize on the full
                // setup echo before starting the measured interval.
                wait_for_setup_echo(&mut recv, prefix.as_bytes()).await?;
                let payload: Vec<u8> = (0..width)
                    .map(|j| b'a' + ((i % 26) as usize * 7 + j) as u8 % 26)
                    .collect();
                let begin = Instant::now();
                send(&payload).await?;
                expect_echo(&mut recv, &payload).await?;
                Ok::<_, std::io::Error>(begin.elapsed().as_secs_f64() * 1000.0)
            };
            let sample_deadline =
                deadline.min(tokio::time::Instant::now() + Duration::from_secs(5));
            let outcome = tokio::time::timeout_at(sample_deadline, sample).await;
            match outcome {
                Ok(Ok(rtt)) => rtts.push(rtt),
                failed => {
                    let _ = tokio::time::timeout(Duration::from_secs(1), send(&[0x15])).await;
                    panic!(
                        "{label_prefix} {phase}: attempted={}, complete={}, requested={count}; {failed:?}",
                        i + 1,
                        rtts.len()
                    );
                }
            }
            tokio::time::sleep(gap).await;
        }
        send(&[0x15]).await.expect("clear probe input");
        report(&format!("{label_prefix} {phase}"), &rtts);
    }
}

#[tokio::test]
#[ignore = "live-hardware probe: types on the real console"]
async fn board_tcp_probe() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let host = env_or("PROBE_HOST", "192.168.1.105");
    let port: u16 = env_or("PROBE_PORT", "2010").parse().expect("PROBE_PORT");
    let stream = tokio::net::TcpStream::connect((host.as_str(), port))
        .await
        .expect("connect ser2net failed (another client may hold the endpoint)");
    stream.set_nodelay(true).unwrap();
    let (read_half, write_half) = stream.into_split();
    let read = std::sync::Arc::new(tokio::sync::Mutex::new(read_half));
    let write = std::sync::Arc::new(tokio::sync::Mutex::new(write_half));

    // ser2net banner + prompt echo settle: read until 600ms of quiet.
    {
        let mut read = read.lock().await;
        let mut buf = [0u8; 4096];
        loop {
            match tokio::time::timeout(Duration::from_millis(600), read.read(&mut buf)).await {
                Ok(Ok(n)) if n > 0 => continue,
                _ => break,
            }
        }
    }

    let send: SendFn = Box::new({
        let write = write.clone();
        move |payload: &[u8]| {
            let write = write.clone();
            let payload = payload.to_vec();
            Box::pin(async move { write.lock().await.write_all(&payload).await.map(|_| ()) })
        }
    });
    let recv: RecvFn = Box::new({
        let read = read.clone();
        move || {
            let read = read.clone();
            Box::pin(async move {
                let mut buf = [0u8; 4096];
                let size = read.lock().await.read(&mut buf).await?;
                if size == 0 {
                    return Err(std::io::ErrorKind::UnexpectedEof.into());
                }
                Ok(buf[..size].to_vec())
            })
        }
    });

    measure("board", send, recv).await;
}

#[tokio::test]
#[ignore = "live-hardware probe: takes over the console via the MCP server"]
async fn ws_probe() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let port: u16 = env_or("PROBE_WS_PORT", "3038")
        .parse()
        .expect("PROBE_WS_PORT");
    let dut = env_or("PROBE_DUT", "yr3588");
    let token = match std::env::var("PROBE_TOKEN") {
        Ok(token) => token,
        Err(_) => {
            let path = std::path::PathBuf::from(env_or(
                "PROBE_TOKEN_PATH",
                &format!("../.dut-serial/control-{port}.token"),
            ));
            std::fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("read control token {path:?}: {error}"))
                .trim()
                .to_string()
        }
    };

    let url = format!("ws://127.0.0.1:{port}/serial/ws?dut={dut}");
    let mut request = url.into_client_request().expect("ws url");
    request.headers_mut().insert(
        sermcp::lifecycle::CONTROL_HEADER,
        token.parse().expect("token header value"),
    );
    let (ws, _) = connect_async(request).await.expect("ws connect");
    if let tokio_tungstenite::MaybeTlsStream::Plain(stream) = ws.get_ref() {
        let _ = stream.set_nodelay(true);
    }
    let (sink, stream) = ws.split();
    let sink = std::sync::Arc::new(tokio::sync::Mutex::new(sink));
    let stream = std::sync::Arc::new(tokio::sync::Mutex::new(stream));

    // The server sends a `\r` prompt nudge on attach; settle its echo.
    loop {
        let item =
            tokio::time::timeout(Duration::from_millis(600), stream.lock().await.next()).await;
        match item {
            Ok(Some(Ok(Message::Binary(_) | Message::Text(_)))) => continue,
            _ => break,
        }
    }

    let send: SendFn = Box::new({
        let sink = sink.clone();
        move |payload: &[u8]| {
            let sink = sink.clone();
            let message = Message::Binary(payload.to_vec().into());
            Box::pin(async move {
                sink.lock()
                    .await
                    .send(message)
                    .await
                    .map(|_| ())
                    .map_err(std::io::Error::other)
            })
        }
    });
    let recv: RecvFn = Box::new({
        let stream = stream.clone();
        move || {
            let stream = stream.clone();
            Box::pin(async move {
                let mut stream = stream.lock().await;
                loop {
                    match stream.next().await {
                        Some(Ok(Message::Binary(data))) if !data.is_empty() => {
                            return Ok(data.to_vec());
                        }
                        Some(Ok(Message::Text(text))) if !text.is_empty() => {
                            return Ok(text.as_bytes().to_vec());
                        }
                        Some(Err(error)) => return Err(std::io::Error::other(error)),
                        Some(Ok(Message::Close(_))) | None => {
                            return Err(std::io::ErrorKind::UnexpectedEof.into());
                        }
                        _ => {}
                    }
                }
            })
        }
    });

    measure("ws", send, recv).await;
}

/// Decode a `SERMCP_TRACE` capture and print every track event in time
/// order with the microsecond delta to its predecessor. Relay events carry
/// the `relay` category (see the instrumentation in mcp_http/console/
/// serial_engine/dutabo); the gaps between `ws_rx_keystroke` →
/// `write_wake` → `drain_writes` → `engine_rx` → `ws_tx_echo` are the
/// per-hop latencies the tail analysis needs.
#[test]
#[ignore = "set TRACE_DUMP=<path.pftrace> to decode a capture"]
fn trace_dump() {
    use prost::Message as _;
    use tracing_perfetto_sdk_schema::trace_packet::Data;
    use tracing_perfetto_sdk_schema::track_event::NameField;

    let path = env_or("TRACE_DUMP", "");
    assert!(!path.is_empty(), "TRACE_DUMP=<path.pftrace> is required");
    let bytes = std::fs::read(&path).unwrap_or_else(|error| panic!("read {path}: {error}"));
    let trace = tracing_perfetto_sdk_schema::Trace::decode(bytes.as_slice()).unwrap();

    // Events can carry a message as a debug annotation or an interned body.
    let mut interned_names = std::collections::HashMap::new();
    let mut interned_bodies = std::collections::HashMap::new();
    let mut rows: Vec<(u64, String)> = Vec::new();
    for packet in &trace.packet {
        if let Some(data) = packet.interned_data.as_ref() {
            for name in &data.event_names {
                if let (Some(iid), Some(name)) = (name.iid, name.name.as_ref()) {
                    interned_names.insert(iid, name.clone());
                }
            }
            for body in &data.log_message_body {
                if let (Some(iid), Some(body)) = (body.iid, body.body.as_ref()) {
                    interned_bodies.insert(iid, body.clone());
                }
            }
        }
        let Some(Data::TrackEvent(event)) = packet.data.as_ref() else {
            continue;
        };
        let name = match event.name_field.as_ref() {
            Some(NameField::Name(name)) => name.clone(),
            Some(NameField::NameIid(iid)) => interned_names.get(iid).cloned().unwrap_or_default(),
            None => String::new(),
        };
        // Prefer the message body (e.g. `ws_rx_keystroke bytes=1`) over the
        // synthetic `event <file>:<line>` name.
        let body = trace_message(event)
            .map(str::to_owned)
            .or_else(|| {
                event
                    .log_message
                    .as_ref()
                    .and_then(|log| log.body_iid)
                    .and_then(|iid| interned_bodies.get(&iid).cloned())
            })
            .unwrap_or_default();
        if name.is_empty() && body.is_empty() {
            continue;
        }
        let label = if body.is_empty() {
            name
        } else if name.starts_with("event ") {
            body
        } else {
            format!("{name}: {body}")
        };
        rows.push((packet.timestamp.unwrap_or(0), label));
    }
    rows.sort_by_key(|(ts, _)| *ts);

    println!("== {} events from {} ==", rows.len(), path);
    let mut previous = None;
    for (ts, label) in &rows {
        let delta_us = previous.map_or(0.0, |prev: u64| (ts - prev) as f64 / 1000.0);
        println!("{delta_us:>10.3} µs  {label}");
        previous = Some(*ts);
    }
}

fn trace_message(event: &tracing_perfetto_sdk_schema::TrackEvent) -> Option<&str> {
    use tracing_perfetto_sdk_schema::debug_annotation::{NameField, Value};

    event.debug_annotations.iter().find_map(|annotation| {
        match (&annotation.name_field, &annotation.value) {
            (Some(NameField::Name(name)), Some(Value::StringValue(value))) if name == "message" => {
                Some(value.as_str())
            }
            _ => None,
        }
    })
}

/// Empirical check: which tracing levels does the unfiltered perfetto layer
/// actually capture? Emits one event per level and decodes the result.
#[test]
#[ignore = "diagnostic"]
fn trace_level_capture() {
    use prost::Message as _;

    let path = std::env::temp_dir().join("level-capture.pftrace");
    let _ = std::fs::remove_file(&path);
    unsafe { std::env::set_var("SERMCP_TRACE", &path) };
    {
        use tracing_subscriber::util::SubscriberInitExt;
        let (registry, guard) =
            sermcp::trace_perfetto::attach_perfetto(tracing_subscriber::registry());
        registry.init();
        tracing::trace!("probe_trace_level");
        tracing::debug!("probe_debug_level");
        tracing::info!("probe_info_level");
        tracing::warn!("probe_warn_level");
        drop(guard);
    }
    sermcp::trace_perfetto::finalize();
    unsafe { std::env::remove_var("SERMCP_TRACE") };

    let bytes = std::fs::read(&path).unwrap();
    let trace = tracing_perfetto_sdk_schema::Trace::decode(bytes.as_slice()).unwrap();
    let messages: Vec<_> = trace
        .packet
        .iter()
        .filter_map(|packet| {
            let tracing_perfetto_sdk_schema::trace_packet::Data::TrackEvent(event) =
                packet.data.as_ref()?
            else {
                return None;
            };
            trace_message(event)
        })
        .collect();
    assert_eq!(
        messages,
        [
            "probe_trace_level",
            "probe_debug_level",
            "probe_info_level",
            "probe_warn_level",
        ]
    );
}

/// Diagnostic: emit two events with a known 50ms gap and print raw packet
/// timestamps + sequence_flags — reveals whether the layer writes absolute
/// timestamps or perfetto incremental (delta) encoding.
#[test]
#[ignore = "diagnostic"]
fn trace_timestamp_mode() {
    use prost::Message as _;

    let path = std::env::temp_dir().join("ts-mode.pftrace");
    let _ = std::fs::remove_file(&path);
    unsafe { std::env::set_var("SERMCP_TRACE", &path) };
    {
        use tracing_subscriber::util::SubscriberInitExt;
        let (registry, guard) =
            sermcp::trace_perfetto::attach_perfetto(tracing_subscriber::registry());
        registry.init();
        tracing::info!("first_marker");
        std::thread::sleep(std::time::Duration::from_millis(50));
        tracing::info!("second_marker");
        std::thread::sleep(std::time::Duration::from_millis(50));
        tracing::info!("third_marker");
        drop(guard);
    }
    sermcp::trace_perfetto::finalize();
    unsafe { std::env::remove_var("SERMCP_TRACE") };

    let bytes = std::fs::read(&path).unwrap();
    let trace = tracing_perfetto_sdk_schema::Trace::decode(bytes.as_slice()).unwrap();
    for (i, packet) in trace.packet.iter().enumerate() {
        if packet.data.is_some() {
            println!(
                "packet {i}: ts={:?} flags={:?} clock={:?}",
                packet.timestamp, packet.sequence_flags, packet.timestamp_clock_id
            );
        }
    }
}
