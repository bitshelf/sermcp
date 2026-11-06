//! Idle-only serial telemetry and conservative, replayable protocol filtering.
//!
//! Only exact command echoes and complete, validated records are removable.
//! Prompts and damaged/interleaved records are deliberately retained.

use std::io::{Read, Write};

const COMMAND_PREFIX: &[u8] = b": '__SMCMD1_";
const FRAME_PREFIX: &[u8] = b"@@SMHB1:";
const PROBE_COMMAND_PREFIX: &[u8] = b": '__SMCMD2_";
const PROBE_FRAME_PREFIX: &[u8] = b"@@SMHB2:";
const END: &str = " @@END@@";
const MAX_FRAME: usize = 8400;
pub const MAX_PROBE_BYTES: usize = 384;

/// Self-describing command echo: offline filtering needs no project config.
fn probe_command(id: &str, script: &str) -> String {
    let hex: String = script.bytes().map(|b| format!("{b:02x}")).collect();
    let octal: String = script.bytes().map(|b| format!("\\{b:03o}")).collect();
    format!(
        ": '__SMCMD2_{id}_{hex}__'; (export LC_ALL=C; o=$(sh -c \"$(printf '{octal}')\" 2>&1); r=$?; n=${{#o}}; h=$(printf '%s' \"$o\" | head -c 4096 | od -An -v -tx1 | tr -d ' \\n'); printf '@@%s:{id}@@ rc=%s size=%s out=%s @@END@@\\n' SMHB2 \"$r\" \"$n\" \"$h\")"
    )
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect()
}

/// Build a bounded POSIX-shell sample without files, services or external tools
/// except optional `date`. Missing Linux interfaces produce `NA` fields.
pub fn command(id: &str) -> String {
    // Keep the complete response prefix out of the echo. Counters are sent raw;
    // the host computes deltas without making the DUT sleep between samples.
    format!(
        ": '__SMCMD1_{id}__'; (t=$(date +%s 2>/dev/null); [ -n \"$t\" ] || t=NA; u=NA; c=NA; m=NA; l=NA; b=NA; d=NA; r=NA; w=NA; \
         [ ! -r /proc/uptime ] || read -r u rest </proc/uptime; \
         if [ -r /proc/stat ]; then while read -r k a z x v e f g h rest; do case $k in cpu) c=$a,$z,$x,$v,$e,$f,$g,$h;; procs_blocked) b=$a;; esac; done </proc/stat; fi; \
         if [ -r /proc/meminfo ]; then mt=NA; ma=NA; while read -r k v rest; do case $k in MemTotal:) mt=$v;; MemAvailable:) ma=$v;; esac; done </proc/meminfo; m=$ma,$mt; fi; \
         [ ! -r /proc/loadavg ] || read -r l rest </proc/loadavg; \
         for p in /sys/block/*; do n=${{p##*/}}; case $n in loop*|ram*|zram*|dm-*) continue;; esac; [ -r \"$p/stat\" ] || continue; read -r a z r x v e w rest <\"$p/stat\"; d=$n; break; done; \
         printf '@@%s:{id}@@ t=%s u=%s c=%s m=%s l=%s b=%s d=%s r=%s w=%s @@END@@\\n' SMHB1 \"$t\" \"$u\" \"$c\" \"$m\" \"$l\" \"$b\" \"$d\" \"$r\" \"$w\") 2>/dev/null"
    )
}

fn valid_id(id: &str) -> bool {
    let Some((session, seq)) = id.split_once('_') else {
        return false;
    };
    session.len() == 32
        && session.bytes().all(|b| b.is_ascii_hexdigit())
        && !seq.is_empty()
        && seq.len() <= 20
        && seq.bytes().all(|b| b.is_ascii_digit())
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Sample {
    pub id: String,
    pub received_at: String,
    pub system_time: Option<u64>,
    pub uptime_secs: Option<f64>,
    pub cpu_counters: Option<Vec<u64>>,
    pub memory_available_kib: Option<u64>,
    pub memory_total_kib: Option<u64>,
    pub load_1m: Option<f64>,
    pub blocked_tasks: Option<u64>,
    pub io_device: String,
    pub read_sectors: Option<u64>,
    pub written_sectors: Option<u64>,
    pub exit_code: Option<u8>,
    pub output: Option<String>,
    pub output_truncated: bool,
}

fn numbers(s: &str, count: usize) -> bool {
    s == "NA"
        || (s.split(',').count() == count
            && s.split(',').all(|v| v == "NA" || v.parse::<u64>().is_ok()))
}

fn parse_frame(bytes: &[u8]) -> Option<Sample> {
    let text = std::str::from_utf8(bytes).ok()?;
    if let Some(body) = text.strip_prefix("@@SMHB2:") {
        let (id, body) = body.strip_suffix(END)?.split_once("@@ rc=")?;
        let (rc, body) = body.split_once(" size=")?;
        let (size, output) = body.split_once(" out=")?;
        let size: usize = size.parse().ok()?;
        let output = unhex(output)?;
        if !valid_id(id) || output.len() != size.min(4096) {
            return None;
        }
        return Some(Sample {
            id: id.into(),
            received_at: chrono::Utc::now().to_rfc3339(),
            system_time: None,
            uptime_secs: None,
            cpu_counters: None,
            memory_available_kib: None,
            memory_total_kib: None,
            load_1m: None,
            blocked_tasks: None,
            io_device: String::new(),
            read_sectors: None,
            written_sectors: None,
            exit_code: Some(rc.parse().ok()?),
            output: Some(String::from_utf8_lossy(&output).into_owned()),
            output_truncated: size > output.len(),
        });
    }
    let body = text.strip_prefix("@@SMHB1:")?.strip_suffix(END)?;
    let (id, values) = body.split_once("@@ ")?;
    if !valid_id(id) {
        return None;
    }
    let keys = ["t", "u", "c", "m", "l", "b", "d", "r", "w"];
    let mut fields = Vec::new();
    let words: Vec<_> = values.split(' ').collect();
    if words.len() != keys.len() {
        return None;
    }
    for (word, key) in words.iter().zip(keys) {
        let (name, value) = word.split_once('=')?;
        if name != key || value.is_empty() {
            return None;
        }
        let valid = match key {
            "u" | "l" => {
                value == "NA"
                    || (value.bytes().all(|b| b.is_ascii_digit() || b == b'.')
                        && value
                            .parse::<f64>()
                            .is_ok_and(|v| v.is_finite() && v >= 0.0))
            }
            "c" => numbers(value, 8),
            "m" => numbers(value, 2),
            "d" => {
                value.len() <= 64
                    && value
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
            }
            _ => numbers(value, 1),
        };
        if !valid {
            return None;
        }
        fields.push(value);
    }
    let (available, total) = fields[3].split_once(',').unwrap_or(("NA", "NA"));
    Some(Sample {
        id: id.into(),
        received_at: chrono::Utc::now().to_rfc3339(),
        system_time: fields[0].parse().ok(),
        uptime_secs: fields[1].parse().ok(),
        cpu_counters: fields[2]
            .split(',')
            .map(str::parse)
            .collect::<Result<Vec<_>, _>>()
            .ok(),
        memory_available_kib: available.parse().ok(),
        memory_total_kib: total.parse().ok(),
        load_1m: fields[4].parse().ok(),
        blocked_tasks: fields[5].parse().ok(),
        io_device: fields[6].into(),
        read_sectors: fields[7].parse().ok(),
        written_sectors: fields[8].parse().ok(),
        exit_code: None,
        output: None,
        output_truncated: false,
    })
}

/// Streaming filter with bounded lookahead. Ordinary bytes pass immediately;
/// an incomplete protocol prefix is retained until more data or `finish`.
#[derive(Default)]
pub struct Filter {
    pending: Vec<u8>,
    expected_echo: Option<Vec<u8>>,
}

impl Filter {
    pub fn feed(&mut self, bytes: &[u8], out: &mut Vec<u8>) -> Vec<Sample> {
        let mut samples = Vec::new();
        // Per-byte input bounds memory even for an arbitrarily large caller chunk.
        for &byte in bytes {
            self.pending.push(byte);
            if let Some(expected) = &self.expected_echo {
                if expected.get(self.pending.len() - 1) == Some(&byte) {
                    if expected.len() == self.pending.len() {
                        self.pending.clear();
                        self.expected_echo = None;
                    }
                    continue;
                }
                self.expected_echo = None;
                out.push(self.pending.remove(0));
            }
            loop {
                if self.pending.is_empty() {
                    break;
                }
                if COMMAND_PREFIX.starts_with(&self.pending)
                    || FRAME_PREFIX.starts_with(&self.pending)
                    || PROBE_COMMAND_PREFIX.starts_with(&self.pending)
                    || PROBE_FRAME_PREFIX.starts_with(&self.pending)
                {
                    break;
                }
                let probe = self.pending.starts_with(PROBE_COMMAND_PREFIX);
                if self.pending.starts_with(COMMAND_PREFIX) || probe {
                    let prefix = if probe {
                        PROBE_COMMAND_PREFIX
                    } else {
                        COMMAND_PREFIX
                    };
                    let tail = &self.pending[prefix.len()..];
                    if let Some(end) = tail.windows(3).position(|s| s == b"__'") {
                        let expected = std::str::from_utf8(&tail[..end]).ok().and_then(|tag| {
                            if probe {
                                let (id, hex) = tag.rsplit_once('_')?;
                                if !valid_id(id) || hex.len() > MAX_PROBE_BYTES * 2 {
                                    return None;
                                }
                                let script = String::from_utf8(unhex(hex)?).ok()?;
                                Some(probe_command(id, &script))
                            } else {
                                valid_id(tag).then(|| command(tag))
                            }
                        });
                        if let Some(expected) = expected
                            && expected.as_bytes().starts_with(&self.pending)
                        {
                            if expected.len() == self.pending.len() {
                                self.pending.clear();
                            } else {
                                self.expected_echo = Some(expected.into_bytes());
                            }
                            break;
                        }
                    } else if tail.len() < if probe { MAX_PROBE_BYTES * 2 + 58 } else { 58 }
                        && tail
                            .iter()
                            .all(|b| b.is_ascii_hexdigit() || *b == b'_' || *b == b'\'')
                    {
                        break;
                    }
                } else if (self.pending.starts_with(FRAME_PREFIX)
                    || self.pending.starts_with(PROBE_FRAME_PREFIX))
                    && self.pending.len() <= MAX_FRAME
                {
                    if self.pending.ends_with(END.as_bytes()) {
                        if let Some(sample) = parse_frame(&self.pending) {
                            samples.push(sample);
                            self.pending.clear();
                            break;
                        }
                    } else if !self.pending.contains(&b'\n') && !self.pending.contains(&b'\r') {
                        break;
                    }
                }
                out.push(self.pending.remove(0));
            }
        }
        samples
    }

    /// Release ambiguous bytes unchanged at disconnect, shutdown or EOF.
    pub fn finish(&mut self, out: &mut Vec<u8>) {
        out.append(&mut self.pending);
        self.expected_echo = None;
    }
}

/// Replay the same conservative filtering for offline batch processing.
pub fn filter_log(mut input: impl Read, mut output: impl Write) -> std::io::Result<()> {
    let mut filter = Filter::default();
    let mut buf = [0; 8192];
    loop {
        let n = input.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let mut clean = Vec::new();
        filter.feed(&buf[..n], &mut clean);
        output.write_all(&clean)?;
    }
    let mut tail = Vec::new();
    filter.finish(&mut tail);
    output.write_all(&tail)
}

/// Per-DUT receive clock and challenge/response bookkeeping, independent of
/// state inference: silence alone never proves that power was removed.
pub struct Monitor {
    pub last_rx: std::time::Instant,
    last_attempt: std::time::Instant,
    session: String,
    sequence: u64,
    pending: Option<(String, std::time::Instant)>,
    filter: Filter,
    pub shell_ready: bool,
    pub missed: u32,
    pub last_sample: Option<Sample>,
    pub last_success: Option<String>,
    pub rtt_ms: Option<u128>,
    pub cpu_percent: Option<f64>,
    pub read_kib_s: Option<f64>,
    pub write_kib_s: Option<f64>,
}

impl Default for Monitor {
    fn default() -> Self {
        Self {
            last_rx: std::time::Instant::now(),
            last_attempt: std::time::Instant::now(),
            session: format!("{:032x}", rand::random::<u128>()),
            sequence: 0,
            pending: None,
            filter: Filter::default(),
            shell_ready: false,
            missed: 0,
            last_sample: None,
            last_success: None,
            rtt_ms: None,
            cpu_percent: None,
            read_kib_s: None,
            write_kib_s: None,
        }
    }
}

impl Monitor {
    #[cfg(test)]
    pub fn age_for_test(&mut self, age: std::time::Duration) {
        self.last_rx -= age;
        self.last_attempt -= age;
    }
    pub fn receive(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.last_rx = std::time::Instant::now();
        let mut ordinary = Vec::new();
        for sample in self.filter.feed(bytes, &mut ordinary) {
            if let Some((id, sent)) = &self.pending
                && *id == sample.id
            {
                self.rtt_ms = Some(sent.elapsed().as_millis());
                self.cpu_percent = None;
                self.read_kib_s = None;
                self.write_kib_s = None;
                self.pending = None;
                self.missed = 0;
                if sample.exit_code.is_some_and(|rc| rc != 0) {
                    self.last_sample = Some(sample);
                    continue;
                }
                self.last_success = Some(sample.received_at.clone());
                if let Some(previous) = &self.last_sample {
                    let elapsed = sample
                        .uptime_secs
                        .zip(previous.uptime_secs)
                        .map(|(a, b)| a - b)
                        .filter(|v| *v > 0.0);
                    if let Some(elapsed) = elapsed {
                        if let Some((new, old)) = sample
                            .cpu_counters
                            .as_ref()
                            .zip(previous.cpu_counters.as_ref())
                        {
                            let delta: Option<Vec<u64>> = new
                                .iter()
                                .zip(old)
                                .map(|(a, b)| a.checked_sub(*b))
                                .collect();
                            if let Some(delta) = delta
                                && delta.len() == 8
                            {
                                let total: f64 = delta.iter().map(|v| *v as f64).sum();
                                if total > 0.0 {
                                    self.cpu_percent = Some(
                                        100.0 * (1.0 - (delta[3] as f64 + delta[4] as f64) / total),
                                    );
                                }
                            }
                        }
                        if sample.io_device == previous.io_device {
                            self.read_kib_s = sample
                                .read_sectors
                                .zip(previous.read_sectors)
                                .and_then(|(a, b)| a.checked_sub(b))
                                .map(|v| v as f64 / 2.0 / elapsed);
                            self.write_kib_s = sample
                                .written_sectors
                                .zip(previous.written_sectors)
                                .and_then(|(a, b)| a.checked_sub(b))
                                .map(|v| v as f64 / 2.0 / elapsed);
                        }
                    }
                }
                self.last_sample = Some(sample);
            }
        }
    }

    pub fn expire(&mut self) -> bool {
        if self
            .pending
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() >= std::time::Duration::from_secs(5))
        {
            self.pending = None;
            self.missed = self.missed.saturating_add(1);
            return true;
        }
        false
    }

    pub fn due(&self, silence: std::time::Duration) -> bool {
        self.shell_ready
            && self.pending.is_none()
            && self.last_rx.elapsed() >= silence
            && self.last_attempt.elapsed() >= silence
    }

    pub fn prepare(&mut self, script: Option<&str>) -> (String, String) {
        self.sequence += 1;
        let id = format!("{}_{}", self.session, self.sequence);
        let command = script
            .filter(|script| !script.is_empty())
            .map_or_else(|| command(&id), |script| probe_command(&id, script));
        (id, command)
    }

    pub fn sent(&mut self, id: String) {
        let now = std::time::Instant::now();
        self.last_attempt = now;
        self.pending = Some((id, now));
    }

    /// Start the response window after paced writes have reached the socket.
    pub fn dispatched(&mut self) {
        if let Some((_, sent)) = self.pending.as_mut() {
            *sent = std::time::Instant::now();
            self.last_attempt = *sent;
        }
    }

    pub fn state(&self) -> serde_json::Value {
        let status = match (self.pending.is_some(), self.missed, &self.last_sample) {
            (true, _, _) => "pending",
            (_, 3.., _) => "unresponsive",
            (_, 1.., _) => "timeout",
            (_, _, Some(sample)) if sample.exit_code.is_some_and(|code| code != 0) => {
                "command_failed"
            }
            (_, _, Some(_)) => "responded",
            _ => "not_probed",
        };
        serde_json::json!({
            "status": status,
            "shell_ready": self.shell_ready, "last_rx_seconds": self.last_rx.elapsed().as_secs_f64(),
            "missed": self.missed, "last_success": self.last_success, "rtt_ms": self.rtt_ms,
            "sample": self.last_sample, "cpu_percent": self.cpu_percent,
            "read_kib_s": self.read_kib_s, "write_kib_s": self.write_kib_s
        })
    }
}

#[cfg(test)]
mod monitor_tests {
    use super::*;
    use std::time::Duration;

    fn response(id: &str, uptime: u64, cpu: &str, sectors: u64) -> String {
        format!(
            "@@SMHB1:{id}@@ t=NA u={uptime} c={cpu} m=NA,2048 l=NA b=0 d=mmcblk0 r={sectors} w={sectors} @@END@@"
        )
    }

    #[test]
    fn echoes_and_old_responses_do_not_credit_a_challenge() {
        let mut monitor = Monitor::default();
        let (old, _) = monitor.prepare(None);
        let (id, echo) = monitor.prepare(None);
        monitor.sent(id.clone());
        monitor.receive(echo.as_bytes());
        monitor.receive(response(&old, 10, "1,2,3,4,5,6,7,8", 10).as_bytes());
        assert!(monitor.last_sample.is_none());
        assert_eq!(monitor.state()["status"], "pending");
        monitor.receive(response(&id, 10, "1,2,3,4,5,6,7,8", 10).as_bytes());
        assert_eq!(monitor.state()["status"], "responded");
        assert!(monitor.cpu_percent.is_none());
        let (id, _) = monitor.prepare(None);
        monitor.sent(id.clone());
        monitor.receive(response(&id, 20, "11,2,3,14,5,6,7,8", 50).as_bytes());
        assert_eq!(monitor.cpu_percent, Some(50.0));
        assert_eq!(monitor.read_kib_s, Some(2.0));
    }

    #[test]
    fn whitespace_and_repeated_errors_postpone_probes() {
        let mut monitor = Monitor {
            shell_ready: true,
            ..Monitor::default()
        };
        for bytes in [b"\r\n ".as_slice(), b"error\n", b"error\n"] {
            monitor.age_for_test(Duration::from_secs(61));
            assert!(monitor.due(Duration::from_secs(60)));
            monitor.receive(bytes);
            assert!(!monitor.due(Duration::from_secs(60)));
        }
    }

    #[test]
    fn deadline_counts_only_sent_challenges_and_does_not_infer_power() {
        let mut monitor = Monitor::default();
        monitor.age_for_test(Duration::from_secs(61));
        assert!(!monitor.expire());
        for _ in 0..3 {
            let (id, _) = monitor.prepare(None);
            monitor.sent(id);
            monitor.pending.as_mut().unwrap().1 -= Duration::from_secs(6);
            assert!(monitor.expire());
            assert!(!monitor.expire());
        }
        assert_eq!(monitor.state()["status"], "unresponsive");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const ID: &str = "0123456789abcdef0123456789abcdef_128";

    #[test]
    fn failed_probes_are_responses_and_clear_previous_misses() {
        let mut monitor = Monitor {
            missed: 2,
            ..Monitor::default()
        };
        for _ in 0..3 {
            let (id, _) = monitor.prepare(Some("false"));
            monitor.sent(id.clone());
            monitor.receive(format!("@@SMHB2:{id}@@ rc=1 size=0 out= @@END@@\n").as_bytes());
            assert_eq!(monitor.state()["status"], "command_failed");
            assert_eq!(monitor.missed, 0);
            assert!(monitor.last_success.is_none());
        }
    }

    #[test]
    fn command_arrays_stop_on_failure_and_remain_filterable() {
        for (commands, expected, rc) in [
            (vec!["test -r /proc/uptime", "printf ready"], "ready", 0),
            (
                vec!["printf before; false", "printf should-not-run"],
                "before",
                1,
            ),
            (
                vec!["value=ready # comment", "printf '%s' \"$value\" >&2"],
                "ready",
                0,
            ),
        ] {
            let script = crate::config::HeartbeatCommand::Commands(
                commands.into_iter().map(str::to_owned).collect(),
            )
            .script();
            let command = probe_command(ID, &script);
            let output = std::process::Command::new("sh")
                .args(["-c", &command])
                .output()
                .unwrap();
            assert!(output.status.success());
            let mut wire = format!("root# {command}\r\n").into_bytes();
            wire.extend_from_slice(&output.stdout);
            wire.extend_from_slice(b"ordinary log\n");
            let mut filter = Filter::default();
            let mut clean = Vec::new();
            let mut samples = Vec::new();
            for byte in wire.chunks(1) {
                samples.extend(filter.feed(byte, &mut clean));
            }
            filter.finish(&mut clean);
            assert_eq!(samples.len(), 1);
            assert_eq!(samples[0].exit_code, Some(rc));
            assert_eq!(samples[0].output.as_deref(), Some(expected));
            assert_eq!(clean, b"root# \r\n\nordinary log\n");
        }
    }

    #[test]
    fn configured_probe_executes_and_filters_echo_and_output() {
        for (script, expected, rc) in [
            (
                "printf 'ready\\n'; printf 'warning' >&2",
                "ready\nwarning",
                0,
            ),
            (
                "printf 'quote: \" $HOME @@END@@'; exit 7",
                "quote: \" $HOME @@END@@",
                7,
            ),
            ("true", "", 0),
        ] {
            let mut monitor = Monitor::default();
            let (id, command) = monitor.prepare(Some(script));
            let output = std::process::Command::new("sh")
                .args(["-c", &command])
                .output()
                .unwrap();
            assert!(output.status.success());
            assert!(output.stderr.is_empty());
            let mut wire = format!("root# {command}\r\n").into_bytes();
            wire.extend_from_slice(&output.stdout);
            wire.extend_from_slice(b"ordinary boot message\n");
            for chunk_size in [1, 7, 4096] {
                let mut filter = Filter::default();
                let mut clean = Vec::new();
                let mut samples = Vec::new();
                for chunk in wire.chunks(chunk_size) {
                    samples.extend(filter.feed(chunk, &mut clean));
                }
                filter.finish(&mut clean);
                assert_eq!(samples.len(), 1);
                assert_eq!(samples[0].output.as_deref(), Some(expected));
                assert_eq!(samples[0].exit_code, Some(rc));
                assert_eq!(clean, b"root# \r\n\nordinary boot message\n");
            }
            monitor.sent(id);
            monitor.receive(&output.stdout);
            assert_eq!(
                monitor.state()["status"],
                if rc == 0 {
                    "responded"
                } else {
                    "command_failed"
                }
            );
            assert_eq!(monitor.last_success.is_some(), rc == 0);
        }
    }

    #[test]
    fn configured_probe_truncates_large_output_and_preserves_invalid_frames() {
        let command = probe_command(ID, "printf '%5000s' x");
        let output = std::process::Command::new("sh")
            .args(["-c", &command])
            .output()
            .unwrap();
        let mut filter = Filter::default();
        let mut clean = Vec::new();
        let samples = filter.feed(&output.stdout, &mut clean);
        assert_eq!(samples.len(), 1);
        assert!(samples[0].output_truncated);
        assert_eq!(samples[0].output.as_ref().unwrap().len(), 4096);
        let malformed = format!("@@SMHB2:{ID}@@ rc=0 size=8 out=61 @@END@@");
        clean.clear();
        assert!(filter.feed(malformed.as_bytes(), &mut clean).is_empty());
        filter.finish(&mut clean);
        assert_eq!(clean, malformed.as_bytes());
    }

    fn frame() -> String {
        format!(
            "@@SMHB1:{ID}@@ t=123 u=725.4 c=1,2,3,4,5,6,7,8 m=612,2048 l=0.42 b=0 d=mmcblk0 r=12 w=48 @@END@@"
        )
    }

    #[test]
    fn split_echo_and_response_preserve_other_bytes() {
        let wire = format!("root# {}\r\n{}\r\nkernel errorÿ", command(ID), frame());
        for chunk_size in [1, 2, 7, 4096] {
            let mut filter = Filter::default();
            let mut out = Vec::new();
            let mut samples = Vec::new();
            for chunk in wire.as_bytes().chunks(chunk_size) {
                samples.extend(filter.feed(chunk, &mut out));
            }
            filter.finish(&mut out);
            assert_eq!(
                String::from_utf8(out).unwrap(),
                "root# \r\n\r\nkernel errorÿ"
            );
            assert_eq!(samples.len(), 1);
        }
    }

    #[test]
    fn damaged_and_interleaved_data_is_retained() {
        let damaged = frame().replace("c=1,", "c=1,\r\n[panic] broken\r\n");
        let echo = command(ID).replace("u=NA", "u=\r\nOops\r\nNA");
        let wire = format!("{damaged}\n{echo}\n@@SMHB1:partial");
        let mut out = Vec::new();
        filter_log(wire.as_bytes(), &mut out).unwrap();
        assert_eq!(out, wire.as_bytes());
    }

    #[test]
    fn shell_sample_works_without_temp_files() {
        let result = std::process::Command::new("sh")
            .args(["-c", &command(ID)])
            .output()
            .unwrap();
        assert!(result.status.success());
        let text = String::from_utf8(result.stdout).unwrap();
        let sample = parse_frame(text.trim_end().as_bytes()).unwrap();
        assert!(sample.uptime_secs.is_some());
        assert!(sample.memory_total_kib.is_some());
    }
}
