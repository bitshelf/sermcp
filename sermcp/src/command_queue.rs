//! Command queue — serialized execution + marker response routing.
//!
//! Mirrors labgrid's `UBootDriver._run()` marker-echo pattern:
//!   echo '{marker[:4]}''{marker[4:]}'; {cmd}; echo "EXIT:$?"; echo '{marker[:4]}''{marker[4:]}'

use std::collections::VecDeque;
use std::io::{Seek, Write};
use std::path::PathBuf;
use std::time::Instant;

use tokio::sync::oneshot;

use crate::marker::gen_marker;

/// Max bytes buffered between begin/end markers before trimming oldest data.
/// Commands producing more than this will have their output truncated (the
/// oldest portion is discarded to keep memory bounded).
///
/// Sized for typical embedded debugging: `logcat -d` (2-5 MB), `journalctl -b`
/// (1-3 MB), `dmesg` (up to 1 MB), `cat /proc/kallsyms` (200 KB-1 MB).
const BUFFER_CAP: usize = 8 * 1024 * 1024; // 8 MB
/// When trimming, keep the most recent TRIM_KEEP bytes.
const TRIM_KEEP: usize = 4 * 1024 * 1024; // 4 MB

/// Max pending commands before rejecting new ones.
/// Prevents memory exhaustion from rapid-fire command submission.
const MAX_PENDING: usize = 100;

/// Strip ANSI escape codes (CSI/ESC sequences) from raw bytes — lossy
/// UTF-8, all non-escape characters (including `\r`) are kept so marker
/// and echo matching stay byte-faithful.
fn strip_ansi(data: &[u8]) -> Vec<u8> {
    crate::ansi_strip::strip_bytes_lossy(data)
}

struct PendingCommand {
    command: String,
    marker: String,
    timeout_secs: f64,
    sender: Option<oneshot::Sender<CommandResult>>,
    begin_sent: bool,
    found_begin: bool,
    /// The exact command line sent to the serial port (for echo stripping).
    sent_line: Vec<u8>,
    /// Bytes of `sent_line` already matched in the serial stream. The
    /// terminal echoes the command line byte-for-byte; this counter lets the
    /// echo be stripped across chunk boundaries.
    echo_matched: usize,
    /// In-memory buffer — holds the tail of the output for end-marker scanning.
    /// When it exceeds `buffer_cap`, older data is spilled to `spill_file`.
    buffer: Vec<u8>,
    /// Cross-chunk search buffer (cf. labgrid's `before` buffer).
    search_buf: Vec<u8>,
    sent_at: Instant,
    /// Total bytes written to spill file (0 if no spill).
    spill_bytes: u64,
    /// Temp file for large output spill. Created when buffer exceeds cap.
    spill_file: Option<(PathBuf, std::fs::File)>,
}

impl PendingCommand {
    fn marker_bytes(&self) -> Vec<u8> {
        self.marker.as_bytes().to_vec()
    }

    fn resolve(mut self, output: String, timed_out: bool, exit_code: Option<i32>) {
        let truncated = self.spill_bytes > 0;
        let final_output = if truncated {
            self.reconstruct_full_output(&output)
        } else {
            output
        };
        if let Some(tx) = self.sender.take() {
            let _ = tx.send(CommandResult {
                output: final_output,
                exit_code,
                timed_out,
                truncated,
                latency_ms: self.sent_at.elapsed().as_millis() as u64,
            });
        }
    }

    /// Reconstruct full output: spill file content + the buffer tail portion.
    fn reconstruct_full_output(&self, buffer_tail: &str) -> String {
        if let Some((ref path, _)) = self.spill_file {
            match std::fs::read(path) {
                Ok(file_data) => {
                    let file_str = String::from_utf8_lossy(&file_data).replace('\r', "");
                    format!("{}{}", file_str.trim_end(), buffer_tail)
                }
                Err(_) => buffer_tail.to_string(),
            }
        } else {
            buffer_tail.to_string()
        }
    }
}

impl Drop for PendingCommand {
    fn drop(&mut self) {
        // Clean up temp spill file if it exists.
        if let Some((ref path, _)) = self.spill_file {
            std::fs::remove_file(path).ok();
        }
    }
}

#[derive(Debug)]
pub struct CommandResult {
    pub output: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    /// True when output was large enough to spill to a temp file.
    pub truncated: bool,
    /// Wall-clock latency from enqueue to result, in milliseconds.
    pub latency_ms: u64,
}

type SerialWriteFn = Box<dyn Fn(&[u8]) + Send>;

pub struct CommandQueue {
    pending: VecDeque<PendingCommand>,
    current: Option<PendingCommand>,
    write_fn: Option<SerialWriteFn>,
    /// Max bytes to buffer before trimming (default: BUFFER_CAP).
    /// Lowered in tests to verify truncation without allocating 8 MB.
    buffer_cap: usize,
    /// Bytes to keep when trimming (default: TRIM_KEEP).
    trim_keep: usize,
    /// Counter: successfully completed commands (timed_out=false).
    pub completed_count: u64,
    /// Counter: failed commands (timed_out=true).
    pub error_count: u64,
    /// Recently completed command latencies in ms (drained each read_loop_iter).
    completed_latencies: Vec<u64>,
}

impl Default for CommandQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandQueue {
    pub fn new() -> Self {
        Self {
            pending: VecDeque::new(),
            current: None,
            write_fn: None,
            buffer_cap: BUFFER_CAP,
            trim_keep: TRIM_KEEP,
            completed_count: 0,
            error_count: 0,
            completed_latencies: Vec::new(),
        }
    }

    /// Drain and return recently completed command latencies (ms).
    pub fn drain_latencies(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.completed_latencies)
    }

    pub fn set_write_fn(&mut self, f: SerialWriteFn) {
        self.write_fn = Some(f);
    }

    /// Number of commands waiting in the pending queue.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Submit a command and return a receiver to await the result.
    #[tracing::instrument(skip(self), fields(cmd = %command, timeout = timeout_secs))]
    pub fn execute(
        &mut self,
        command: String,
        timeout_secs: f64,
    ) -> oneshot::Receiver<CommandResult> {
        // Reject new commands when the pending queue is full, preventing
        // memory exhaustion from rapid-fire command submission.
        if self.pending.len() >= MAX_PENDING {
            let (tx, rx) = oneshot::channel();
            let _ = tx.send(CommandResult {
                output: "(rejected — too many pending commands)".to_string(),
                exit_code: None,
                timed_out: false,
                truncated: false,
                latency_ms: 0,
            });
            return rx;
        }

        let (tx, rx) = oneshot::channel();
        let marker = gen_marker();
        let pc = PendingCommand {
            command,
            marker,
            timeout_secs,
            sender: Some(tx),
            begin_sent: false,
            found_begin: false,
            sent_line: Vec::new(),
            echo_matched: 0,
            buffer: Vec::new(),
            search_buf: Vec::new(),
            sent_at: Instant::now(),
            spill_bytes: 0,
            spill_file: None,
        };

        if self.current.is_none() {
            self.send_command(pc);
        } else {
            self.pending.push_back(pc);
        }
        rx
    }

    /// Scan the serial data stream for begin/end markers and extract command output.
    pub fn feed_serial_data(&mut self, data: &[u8]) {
        let data = strip_ansi(data);
        let mut data: &[u8] = &data;

        // Check timeout first — MUST dequeue_next() to avoid queue deadlock.
        if let Some(ref pc) = self.current
            && pc.begin_sent
            && pc.sent_at.elapsed().as_secs_f64() > pc.timeout_secs
        {
            if let Some(pc) = self.current.take() {
                let partial = String::from_utf8_lossy(&pc.buffer).to_string();
                let output = if partial.trim().is_empty() {
                    "(timeout — no output)".to_string()
                } else {
                    format!("(timeout — partial output)\n{}", partial.trim())
                };
                pc.resolve(output, true, None);
                self.error_count += 1;
            }
            self.dequeue_next();
        }

        let Some(ref mut pc) = self.current else {
            return;
        };
        if !pc.begin_sent {
            return;
        }

        let marker = pc.marker_bytes();

        // Step 1: find real begin marker. Terminal echo is left on (like PuTTY
        // serial), so the command line is echoed. The echoed line splits the
        // marker across two quoted strings (`echo 'ABCD''EFGHIJ'`), so it can
        // never contain a contiguous marker. Strip the echo byte-for-byte —
        // `echo_matched` carries the progress across chunk boundaries — and
        // then the first contiguous marker in the stream is the real begin
        // marker.
        let mut begin_found_in_chunk = false;
        if !pc.found_begin {
            if pc.echo_matched < pc.sent_line.len() {
                let mut pos = 0;
                let mut echo_completed = false;
                while pos < data.len() && pc.echo_matched < pc.sent_line.len() {
                    if data[pos] == pc.sent_line[pc.echo_matched] {
                        pc.echo_matched += 1;
                        pos += 1;
                        echo_completed = pc.echo_matched == pc.sent_line.len();
                    } else {
                        // Not a byte-for-byte echo (stray prompt bytes before
                        // the echo, terminal line-wrap, or ANSI filtering of
                        // the sent line): give up on stripping and fall back
                        // to marker scanning. The echoed line cannot contain
                        // a contiguous marker, so the first match is still
                        // the real begin marker.
                        pc.echo_matched = pc.sent_line.len();
                        break;
                    }
                }
                data = &data[pos..];
                if echo_completed {
                    // Drop the echo's line ending ("\r\n" or "\n") so it
                    // cannot linger in the search buffer. If it straddles a
                    // chunk boundary it is discarded with the pre-marker
                    // bytes when the begin marker is found — harmless.
                    let mut skip = 0;
                    if data.starts_with(b"\r") {
                        skip += 1;
                    }
                    if data[skip..].starts_with(b"\n") {
                        skip += 1;
                    }
                    data = &data[skip..];
                }
            }

            pc.search_buf.extend_from_slice(data);
            if let Some(idx) = find_subsequence(&pc.search_buf, &marker) {
                pc.found_begin = true;
                pc.buffer = pc.search_buf[idx + marker.len()..].to_vec();
                pc.search_buf.clear();
                begin_found_in_chunk = true;
            } else if pc.search_buf.len() > self.buffer_cap {
                // Trim only when the marker was NOT found: a marker near the
                // start of a large single chunk must not be destroyed before
                // the scan (large-output truncation test feeds one big chunk).
                let drain = pc.search_buf.len() - self.trim_keep;
                pc.search_buf.drain(..drain);
            }
            if !pc.found_begin {
                return;
            }
        }

        // Step 2: begin_marker found — append data to buffer, spill to file
        // if output exceeds the in-memory cap, and scan for end_marker.
        // When the begin marker was found inside this chunk, the bytes after
        // it were already copied into `buffer` above; appending the whole
        // chunk again would replay the begin marker and resolve prematurely.
        if !begin_found_in_chunk {
            pc.buffer.extend_from_slice(data);
        }

        // Spill to temp file when buffer exceeds capacity. The file stores
        // the full output; buffer keeps only the tail for marker scanning.
        if pc.buffer.len() > self.buffer_cap {
            let drain = pc.buffer.len() - self.trim_keep;
            let spill_data = pc.buffer[..drain].to_vec();

            let spilled = if pc.spill_file.is_none() {
                // First spill: create temp file.
                let tmp_dir = std::env::temp_dir();
                let path = tmp_dir.join(format!("mcp-cmd-{}.tmp", pc.sent_at.elapsed().as_nanos()));
                match std::fs::File::create(&path) {
                    Ok(mut f) => {
                        if write_spill_chunk(&mut f, &spill_data) {
                            pc.spill_bytes = spill_data.len() as u64;
                            pc.spill_file = Some((path, f));
                            true
                        } else {
                            std::fs::remove_file(path).ok();
                            false
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Failed to create spill file: {e}");
                        false
                    }
                }
            } else if let Some((_, ref mut f)) = pc.spill_file {
                if write_spill_chunk(f, &spill_data) {
                    pc.spill_bytes += spill_data.len() as u64;
                    true
                } else {
                    false
                }
            } else {
                false
            };

            // Never discard the in-memory prefix unless the complete chunk is
            // durable.  The old code drained unconditionally, so a full or
            // unwritable temp filesystem silently lost the oldest output.
            if spilled {
                pc.buffer.drain(..drain);
            }
        }

        if let Some(idx) = find_subsequence(&pc.buffer, &marker) {
            let output_bytes = pc.buffer[..idx].to_vec();
            let output = String::from_utf8_lossy(&output_bytes)
                .replace('\r', "")
                .trim()
                .to_string();

            // Extract exit code: the LAST line before the end marker should be
            // `echo "EXIT:$?"` output (prefixed with EXIT: for unambiguous parsing).
            let (clean_output, exit_code) = extract_exit_code(&output);

            let rest = pc.buffer[idx + marker.len()..].to_vec();
            if let Some(pc) = self.current.take() {
                let latency = pc.sent_at.elapsed().as_millis() as u64;
                pc.resolve(clean_output, false, exit_code);
                self.completed_count += 1;
                self.completed_latencies.push(latency);
            }
            self.dequeue_next();
            if !rest.is_empty() {
                self.feed_serial_data(&rest);
            }
        }
    }

    /// Check whether the current command has timed out (called periodically by
    /// the external read loop).
    pub fn check_timeouts(&mut self) {
        if let Some(ref pc) = self.current
            && pc.begin_sent
            && pc.sent_at.elapsed().as_secs_f64() > pc.timeout_secs
            && let Some(pc) = self.current.take()
        {
            let partial = String::from_utf8_lossy(&pc.buffer).to_string();
            let output = if partial.trim().is_empty() {
                "(timeout — no output)".to_string()
            } else {
                format!("(timeout — partial output)\n{}", partial.trim())
            };
            pc.resolve(output, true, None);
            self.error_count += 1;
            self.dequeue_next();
        }
    }

    fn send_command(&mut self, mut pc: PendingCommand) {
        let m = &pc.marker;
        // Echo is left on (PuTTY-like); the echo-skipping logic handles the
        // echoed command text.  The { cmd; } wrapper ensures pipe output is
        // fully flushed before the exit-code line runs — fixes BusyBox ash
        // pipe buffering issues.
        let line = format!(
            "echo '{}''{}'; {{ {}; }}; echo \"EXIT:$?\"; echo '{}''{}'\n",
            &m[..4],
            &m[4..],
            pc.command,
            &m[..4],
            &m[4..],
        );

        if let Some(ref write_fn) = self.write_fn {
            pc.sent_line = line.as_bytes().to_vec(); // save for echo stripping
            pc.echo_matched = 0; // fresh send: no echo bytes matched yet
            pc.search_buf.clear(); // clear stale buffer to avoid matching a previous command's marker
            write_fn(&pc.sent_line);
            pc.sent_at = Instant::now();
            pc.begin_sent = true;
            self.current = Some(pc);
        }
    }

    fn dequeue_next(&mut self) {
        if self.current.is_some() {
            return;
        }
        if let Some(pc) = self.pending.pop_front() {
            self.send_command(pc);
        }
    }
}

/// Append one spill chunk transactionally. On a partial/failed write, roll the
/// file back to its previous length so a later retry cannot duplicate bytes.
fn write_spill_chunk(file: &mut std::fs::File, data: &[u8]) -> bool {
    let start = match file.seek(std::io::SeekFrom::End(0)) {
        Ok(pos) => pos,
        Err(e) => {
            tracing::warn!("Failed to seek spill file: {e}");
            return false;
        }
    };
    if let Err(e) = file.write_all(data) {
        tracing::warn!("Failed to write spill file: {e}");
        if let Err(rollback) = file.set_len(start) {
            tracing::warn!("Failed to roll back partial spill write: {rollback}");
        }
        let _ = file.seek(std::io::SeekFrom::Start(start));
        return false;
    }
    true
}

/// Find the first occurrence of `needle` in `haystack`.
fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0); // empty needle matches at position 0 (defensive)
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Extract the exit code from the last line of command output.
///
/// The shell command template ends with `echo "EXIT:$?"`, so the very last
/// line should be `EXIT:<0-255>`. Using an unambiguous prefix prevents false
/// matches on data lines that happen to be integers (e.g. `wc -l` → `42`).
///
/// Returns `(output_without_exit_code_line, Some(code))` or `(original_output, None)`.
fn extract_exit_code(output: &str) -> (String, Option<i32>) {
    let trimmed = output.trim_end();
    if let Some(last_line) = trimmed.lines().next_back() {
        let stripped = last_line.trim();
        // Parse EXIT:<digits> prefix for unambiguous extraction.
        if let Some(num_str) = stripped.strip_prefix("EXIT:")
            && let Ok(code) = num_str.parse::<u32>()
            && code <= 255
        {
            let exit_code = code as i32;
            // Remove the exit-code line from the output.
            let end = trimmed.rfind(stripped).unwrap_or(trimmed.len());
            let clean = trimmed[..end].trim_end().to_string();
            return (clean, Some(exit_code));
        }
    }
    (output.to_string(), None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_subsequence() {
        assert_eq!(find_subsequence(b"hello world", b"world"), Some(6));
        assert_eq!(find_subsequence(b"hello world", b"xyz"), None);
        assert_eq!(find_subsequence(b"ABCABC", b"ABC"), Some(0));
    }

    #[test]
    fn test_strip_ansi() {
        let input = b"\x1b[31mERROR\x1b[0m";
        let result = strip_ansi(input);
        assert_eq!(result, b"ERROR");
    }

    #[test]
    fn test_strip_ansi_multiple() {
        let input = b"\x1b[1m\x1b[31mBold Red\x1b[0m Normal";
        let result = strip_ansi(input);
        assert_eq!(result, b"Bold Red Normal");
    }

    #[test]
    fn test_strip_ansi_cursor() {
        let input = b"\x1b[2J\x1b[H\x1b[?25h";
        let result = strip_ansi(input);
        assert_eq!(result, b"");
    }

    #[test]
    fn test_find_subsequence_not_found() {
        assert_eq!(find_subsequence(b"hello", b"world"), None);
        assert_eq!(find_subsequence(b"", b"test"), None);
        assert_eq!(find_subsequence(b"short", b"longer"), None);
    }

    #[test]
    fn test_find_subsequence_empty() {
        // Empty needle matches at position 0 (defensive).
        assert_eq!(find_subsequence(b"hello", b""), Some(0));
        assert_eq!(find_subsequence(b"", b""), Some(0));
    }

    #[cfg(unix)]
    #[test]
    fn test_failed_spill_write_reports_failure() {
        let mut full = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/full")
            .unwrap();
        assert!(!write_spill_chunk(&mut full, b"must-not-be-discarded"));
    }

    #[test]
    fn test_extract_exit_code_valid() {
        let (out, code) = extract_exit_code("line1\nline2\nEXIT:0");
        assert_eq!(code, Some(0));
        assert_eq!(out, "line1\nline2");
    }

    #[test]
    fn test_extract_exit_code_nonzero() {
        let (out, code) = extract_exit_code("error msg\nEXIT:127");
        assert_eq!(code, Some(127));
        assert_eq!(out, "error msg");
    }

    #[test]
    fn test_extract_exit_code_no_code() {
        let (out, code) = extract_exit_code("just output\nno number");
        assert_eq!(code, None);
        assert_eq!(out, "just output\nno number");
    }

    #[test]
    fn test_extract_exit_code_out_of_range() {
        // Values > 255 are not valid shell exit codes → ignored.
        let (out, code) = extract_exit_code("data\nEXIT:300");
        assert_eq!(code, None);
        assert_eq!(out, "data\nEXIT:300");
    }

    #[test]
    fn test_extract_exit_code_negative() {
        // Negative values are not valid shell exit codes → ignored.
        let (_out, code) = extract_exit_code("data\nEXIT:-1");
        assert_eq!(code, None);
    }

    #[test]
    fn test_extract_exit_code_data_line_is_number() {
        // A data line that is a number should NOT be misread as exit code
        // because only the LAST line is examined and it must have EXIT: prefix.
        let (out, code) = extract_exit_code("42\nactual output");
        assert_eq!(code, None);
        assert_eq!(out, "42\nactual output");
    }

    #[test]
    fn test_extract_exit_code_wc_l_style_output() {
        // Regression: output from commands like `wc -l` that end with numbers
        // must not be misread as exit codes. EXIT: prefix prevents this.
        let (out, code) = extract_exit_code("total lines processed\n242");
        assert_eq!(code, None);
        assert_eq!(out, "total lines processed\n242");
    }

    #[test]
    fn test_extract_exit_code_no_exit_prefix() {
        // Bare 0 without EXIT: prefix → not an exit code.
        let (out, code) = extract_exit_code("command output\n0");
        assert_eq!(code, None);
        assert_eq!(out, "command output\n0");
    }

    #[test]
    fn test_command_format() {
        let mut cq = CommandQueue::new();
        use std::sync::{Arc, Mutex};
        let written = Arc::new(Mutex::new(Vec::new()));
        let written_clone = written.clone();
        let write_fn = Box::new(move |data: &[u8]| {
            written_clone.lock().unwrap().extend_from_slice(data);
        });
        cq.set_write_fn(write_fn);

        let _rx = cq.execute("uname -a".to_string(), 90.0);

        // Should have written the command with markers
        let written_data = written.lock().unwrap();
        let cmd_str = String::from_utf8_lossy(&written_data);
        assert!(cmd_str.starts_with("echo '"));
        assert!(cmd_str.contains("{ uname -a; }")); // cmd wrapped in { ; }
        assert!(cmd_str.contains("echo \"EXIT:$?\""));
        assert!(!cmd_str.contains("stty -echo"));
        assert!(!cmd_str.contains("stty echo"));
    }

    #[test]
    fn test_marker_extraction() {
        // This test verifies that feed_serial_data can extract output between markers
        // The actual shell echo behavior is complex, so we test the core logic directly
        let marker = gen_marker();
        let marker_bytes = marker.as_bytes();

        // Simulate serial data with marker appearing twice
        let serial_data = format!(
            "{}\noutput line 1\noutput line 2\nEXIT:0\n{}\n",
            marker, marker
        );

        // Verify the marker appears twice in the data
        let first_pos = serial_data.find(&marker);
        assert!(first_pos.is_some());
        let second_pos = serial_data[first_pos.unwrap() + marker.len()..].find(&marker);
        assert!(second_pos.is_some());

        // Verify find_subsequence works correctly
        assert_eq!(
            find_subsequence(serial_data.as_bytes(), marker_bytes),
            first_pos
        );
    }

    #[test]
    fn test_command_serialization() {
        let mut cq = CommandQueue::new();
        let write_fn = Box::new(|_data: &[u8]| {});
        cq.set_write_fn(write_fn);

        // Submit two commands
        let mut rx1 = cq.execute("cmd1".to_string(), 90.0);
        let mut rx2 = cq.execute("cmd2".to_string(), 90.0);

        // First command should be pending, second queued
        assert!(rx1.try_recv().is_err());
        assert!(rx2.try_recv().is_err());
    }

    #[test]
    fn test_marker_split() {
        let marker = gen_marker();
        assert_eq!(marker.len(), 10);

        let first_half = &marker[..4];
        let second_half = &marker[4..];

        // Both halves should be non-empty
        assert!(!first_half.is_empty());
        assert!(!second_half.is_empty());

        // Reconstruct should equal original
        let reconstructed = format!("{}{}", first_half, second_half);
        assert_eq!(reconstructed, marker);
    }

    #[test]
    fn test_strip_ansi_no_escapes() {
        let input = b"plain text without escapes";
        let result = strip_ansi(input);
        assert_eq!(result, b"plain text without escapes");
    }

    /// Regression: shell echo of the sent command contains the marker text
    /// (split across two quoted strings), which used to cause CommandQueue to
    /// match the wrong marker as begin. The fix strips the echoed command
    /// line byte-for-byte via `sent_line`, so the first contiguous marker is
    /// the real begin marker.  Test data simulates the post-echo-strip stream.
    #[test]
    fn test_skip_echoed_marker() {
        let mut cq = CommandQueue::new();

        // Set up a write_fn that captures the sent command
        let sm = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let sm_clone = sm.clone();
        let write_fn = Box::new(move |data: &[u8]| {
            *sm_clone.lock().unwrap() = String::from_utf8_lossy(data).to_string();
        });
        cq.set_write_fn(write_fn);

        let mut rx = cq.execute("dmesg | grep -iE 'opp|cpu' | head -5".to_string(), 90.0);

        let sent = sm.lock().unwrap();
        let marker: String = sent
            .split('\'')
            .nth(1)
            .unwrap()
            .chars()
            .chain(sent.split('\'').nth(3).unwrap().chars())
            .collect();
        assert_eq!(marker.len(), 10);

        let marker_bytes = marker.as_bytes().to_vec();

        // Simulate serial data with shell echo (same as 2>&1, pipe, redirect):
        // 1. Echoed command line (contains marker)
        // 2. Newline
        // 3. Actual marker output (from echo command)
        // 4. Command output
        // 5. Exit code
        // 6. End marker
        // Echo is left on; the echoed command line (if present) is stripped
        // byte-for-byte via sent_line before marker scanning.  Test data
        // simulates the post-echo-strip stream.
        let mut serial_stream = Vec::new();
        // Actual marker output (from echo 'MARKER')
        serial_stream.extend_from_slice(&marker_bytes);
        serial_stream.push(b'\n');
        // Command output
        serial_stream.extend_from_slice(b"CPU0: 408000\nOPP table v2\nCPU1: 600000\n");
        // Exit code
        serial_stream.extend_from_slice(b"EXIT:0\n");
        // End marker
        serial_stream.extend_from_slice(&marker_bytes);
        serial_stream.push(b'\n');

        cq.feed_serial_data(&serial_stream);

        let result = rx.try_recv().unwrap();
        assert!(!result.timed_out);
        assert_eq!(result.exit_code, Some(0));
        assert!(result.output.contains("CPU0: 408000"));
        assert!(result.output.contains("OPP table v2"));
        // Should NOT contain the echoed command text
        assert!(!result.output.contains("echo '"));
        assert!(!result.output.contains("dmesg"));
    }

    /// Verifies that piping with 2>&1 works correctly (regression test
    /// for the pipe/special-char marker interference issue).
    #[test]
    fn test_pipe_and_redirect_not_garbled() {
        let mut cq = CommandQueue::new();

        let sm = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let sm_clone = sm.clone();
        let write_fn = Box::new(move |data: &[u8]| {
            *sm_clone.lock().unwrap() = String::from_utf8_lossy(data).to_string();
        });
        cq.set_write_fn(write_fn);

        let mut rx = cq.execute("ls /sys/devices/system/cpu/cpufreq/ 2>&1".to_string(), 90.0);

        let sent = sm.lock().unwrap();
        let parts: Vec<&str> = sent.split('\'').collect();
        let marker: String = format!("{}{}", parts[1], parts[3]);
        let mb = marker.as_bytes().to_vec();

        // Echo is left on; echoed command text (if present) is stripped
        // byte-for-byte via sent_line before marker scanning.  Test data
        // simulates the post-echo-strip stream.
        let mut data = Vec::new();
        data.extend_from_slice(&mb); // real marker
        data.push(b'\n');
        data.extend_from_slice(b"policy0\npolicy4\n");
        data.extend_from_slice(b"EXIT:0\n");
        data.extend_from_slice(&mb);
        data.push(b'\n');

        cq.feed_serial_data(&data);
        let result = rx.try_recv().unwrap();
        assert!(!result.timed_out);
        assert!(result.output.contains("policy0"));
        assert!(!result.output.contains("echo '"));
        assert!(!result.output.contains("ls /sys"));
    }

    /// Large command output (> BUFFER_CAP) should set truncated=true and
    /// preserve the trailing portion where the end marker lives.
    #[test]
    fn test_large_output_truncation_tracking() {
        let mut cq = CommandQueue::new();
        // Use a tiny cap so we don't need 8 MB of test data.
        cq.buffer_cap = 1024;
        cq.trim_keep = 512;

        let sm = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let sm_clone = sm.clone();
        let write_fn = Box::new(move |data: &[u8]| {
            *sm_clone.lock().unwrap() = String::from_utf8_lossy(data).to_string();
        });
        cq.set_write_fn(write_fn);

        let mut rx = cq.execute("cat /proc/kallsyms".to_string(), 90.0);

        let sent = sm.lock().unwrap();
        let parts: Vec<&str> = sent.split('\'').collect();
        let marker: String = format!("{}{}", parts[1], parts[3]);
        let mb = marker.as_bytes().to_vec();

        // Generate > 1024 bytes of output (small cap set above).
        let mut data = Vec::new();
        data.extend_from_slice(&mb); // begin marker
        data.push(b'\n');
        for _ in 0..30 {
            data.extend_from_slice(
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789!!\n",
            );
        }
        data.push(b'\n');
        data.extend_from_slice(b"EXIT:0\n");
        data.extend_from_slice(&mb); // end marker
        data.push(b'\n');

        cq.feed_serial_data(&data);

        let result = rx.try_recv().unwrap();
        assert!(!result.timed_out);
        assert_eq!(result.exit_code, Some(0));
        assert!(
            result.truncated,
            "Output > buffer_cap must set truncated=true"
        );
    }

    /// Output within BUFFER_CAP must NOT set truncated.
    #[test]
    fn test_small_output_not_truncated() {
        let mut cq = CommandQueue::new();

        let sm = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let sm_clone = sm.clone();
        let write_fn = Box::new(move |data: &[u8]| {
            *sm_clone.lock().unwrap() = String::from_utf8_lossy(data).to_string();
        });
        cq.set_write_fn(write_fn);

        let mut rx = cq.execute("uname -a".to_string(), 90.0);

        let sent = sm.lock().unwrap();
        let parts: Vec<&str> = sent.split('\'').collect();
        let marker: String = format!("{}{}", parts[1], parts[3]);
        let mb = marker.as_bytes().to_vec();

        let mut data = Vec::new();
        data.extend_from_slice(&mb);
        data.push(b'\n');
        data.extend_from_slice(b"Linux 5.15.0\n");
        data.extend_from_slice(b"EXIT:0\n");
        data.extend_from_slice(&mb);
        data.push(b'\n');

        cq.feed_serial_data(&data);
        let result = rx.try_recv().unwrap();
        assert!(!result.timed_out, "should not time out");
        assert!(!result.truncated, "small output must not set truncated");
    }

    #[test]
    fn test_sequential_commands_do_not_interfere() {
        let mut cq = CommandQueue::new();
        let sm = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let sm2 = sm.clone();
        let write_fn = Box::new(move |data: &[u8]| {
            *sm2.lock().unwrap() = String::from_utf8_lossy(data).to_string();
        });
        cq.set_write_fn(write_fn);

        let mut rx1 = cq.execute("cmd1".to_string(), 90.0);
        let mut rx2 = cq.execute("cmd2".to_string(), 90.0);

        // First command should be sent, second queued
        let sent1 = sm.lock().unwrap();
        assert!(sent1.contains("cmd1"));
        assert!(rx1.try_recv().is_err()); // not resolved yet
        assert!(rx2.try_recv().is_err()); // not even started
    }

    #[test]
    fn test_queue_drains_on_timeout() {
        let mut cq = CommandQueue::new();
        let write_fn = Box::new(|_data: &[u8]| {});
        cq.set_write_fn(write_fn);

        let mut rx1 = cq.execute("cmd1".to_string(), 0.001); // 1ms timeout
        std::thread::sleep(std::time::Duration::from_millis(10));
        cq.feed_serial_data(b""); // triggers timeout check
        let result = rx1.try_recv().unwrap();
        assert!(result.timed_out);
        assert!(result.output.contains("timeout"));
    }

    #[test]
    fn test_multiple_commands_sequential_resolve() {
        let mut cq = CommandQueue::new();
        let sm = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sm2 = sm.clone();
        let write_fn = Box::new(move |data: &[u8]| {
            sm2.lock()
                .unwrap()
                .push(String::from_utf8_lossy(data).to_string());
        });
        cq.set_write_fn(write_fn);

        let _rx1 = cq.execute("cmd_a".to_string(), 90.0);
        let _rx2 = cq.execute("cmd_b".to_string(), 90.0);
        let _rx3 = cq.execute("cmd_c".to_string(), 90.0);

        // All three should be written (first sent, others queued)
        let writes = sm.lock().unwrap();
        assert_eq!(writes.len(), 1); // only first sent immediately
    }

    #[test]
    fn test_rapid_fire_commands() {
        let mut cq = CommandQueue::new();
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c2 = count.clone();
        let write_fn = Box::new(move |_data: &[u8]| {
            c2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });
        cq.set_write_fn(write_fn);

        let mut receivers = Vec::new();
        for i in 0..50 {
            receivers.push(cq.execute(format!("cmd{}", i), 90.0));
        }
        // All 50 should be queued (only 1 sent immediately to write_fn)
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    // ------------------------------------------------------------------
    // CR-1 regression tests: the marker parser must handle the real
    // echo-on serial stream — echoed command line + "\r\n" + begin marker
    // + output + "EXIT:0" + end marker — regardless of chunk boundaries.
    // ------------------------------------------------------------------

    /// Fixture for echo-on stream tests: captures the exact line written to
    /// the serial port and extracts the marker embedded in it.
    struct EchoFixture {
        cq: CommandQueue,
        rx: oneshot::Receiver<CommandResult>,
        sent_line: Vec<u8>,
        marker: Vec<u8>,
    }

    fn spawn_echo_fixture(cmd: &str) -> EchoFixture {
        let mut cq = CommandQueue::new();
        let sm = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let smc = sm.clone();
        cq.set_write_fn(Box::new(move |data: &[u8]| {
            smc.lock().unwrap().extend_from_slice(data);
        }));
        let rx = cq.execute(cmd.to_string(), 90.0);
        let sent_line = sm.lock().unwrap().clone();
        let sent_str = String::from_utf8_lossy(&sent_line);
        let parts: Vec<&str> = sent_str.split('\'').collect();
        // The sent line wraps the command with TWO marker templates (begin
        // and end), each with two quoted halves → 9 quote-split parts.
        assert_eq!(parts.len(), 9, "sent line must split into 9 quote parts");
        let marker = format!("{}{}", parts[1], parts[3]).into_bytes();
        assert_eq!(marker.len(), 10);
        EchoFixture {
            cq,
            rx,
            sent_line,
            marker,
        }
    }

    /// Realistic echo-on stream: terminal echo of the sent command line,
    /// real begin marker, command output, exit-code line, end marker.
    fn echo_on_stream(f: &EchoFixture, output: &[u8]) -> Vec<u8> {
        let mut s = Vec::new();
        s.extend_from_slice(&f.sent_line); // echoed command line
        s.extend_from_slice(b"\r\n");
        s.extend_from_slice(&f.marker); // real begin marker
        s.extend_from_slice(b"\r\n");
        s.extend_from_slice(output);
        s.extend_from_slice(b"EXIT:0\r\n");
        s.extend_from_slice(&f.marker); // end marker
        s.extend_from_slice(b"\r\n");
        s
    }

    /// Feed `data` in chunks of at most `width` bytes.
    fn feed_chunked(cq: &mut CommandQueue, data: &[u8], width: usize) {
        for chunk in data.chunks(width) {
            cq.feed_serial_data(chunk);
        }
    }

    /// The command must resolve without timeout and with the exact output.
    fn assert_clean_output(rx: &mut oneshot::Receiver<CommandResult>, expected: &str) {
        let result = rx.try_recv().expect("command should resolve");
        assert!(!result.timed_out, "command should not time out");
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.output, expected);
    }

    /// CR-1 (a)+(b): the full echo-on stream in a single chunk must resolve
    /// with the real command output. The old "; echo" heuristic mistook the
    /// real begin marker for the echoed command line, and the unconditional
    /// chunk replay then resolved on the replayed begin marker.
    #[test]
    fn test_echo_on_stream_single_chunk() {
        let mut f = spawn_echo_fixture("sleep 2; echo hi");
        let stream = echo_on_stream(&f, b"hi\r\n");
        f.cq.feed_serial_data(&stream);
        assert_clean_output(&mut f.rx, "hi");
    }

    /// CR-1 (a)+(c): byte-wise chunking exercises every boundary — the echo
    /// split across chunks, both markers split across chunks.
    #[test]
    fn test_echo_on_stream_bytewise_chunks() {
        let mut f = spawn_echo_fixture("uname -r");
        let stream = echo_on_stream(&f, b"5.15.0-139-generic\r\n");
        feed_chunked(&mut f.cq, &stream, 1);
        assert_clean_output(&mut f.rx, "5.15.0-139-generic");
    }

    /// CR-1 (a)+(c): both markers explicitly split across chunk boundaries
    /// (half in one chunk, half in the next) must still be found.
    #[test]
    fn test_markers_split_across_chunks() {
        let mut f = spawn_echo_fixture("echo hello");
        let stream = echo_on_stream(&f, b"hello\r\n");

        // Split inside the begin marker AND inside the end marker.
        let echo_len = f.sent_line.len() + 2; // echoed line + CRLF
        let begin_split = echo_len + 5; // halfway through the begin marker
        let end_start = stream.len() - f.marker.len() - 2; // end marker + CRLF
        let end_split = end_start + 3; // inside the end marker
        let chunks: [&[u8]; 3] = [
            &stream[..begin_split],
            &stream[begin_split..end_split],
            &stream[end_split..],
        ];
        for chunk in chunks {
            f.cq.feed_serial_data(chunk);
        }
        assert_clean_output(&mut f.rx, "hello");
    }

    /// CR-1 (b): begin marker completes one chunk; output arrives in later
    /// chunks. The tail after the begin marker must be carried over and the
    /// later chunks must not be dropped.
    #[test]
    fn test_begin_marker_then_output_in_later_chunks() {
        let mut f = spawn_echo_fixture("cat /etc/hostname");
        let stream = echo_on_stream(&f, b"rk3588-evb\r\n");

        // Chunk 1 ends right after the begin marker.
        let split = f.sent_line.len() + 2 + f.marker.len();
        f.cq.feed_serial_data(&stream[..split]);
        f.cq.feed_serial_data(&stream[split..]);
        assert_clean_output(&mut f.rx, "rk3588-evb");
    }

    /// CR-1 (b): begin marker and the first output line share a chunk — the
    /// old unconditional chunk replay appended the begin marker a second time
    /// and resolved on it, losing the first output line.
    #[test]
    fn test_begin_and_partial_output_same_chunk() {
        let mut f = spawn_echo_fixture("dmesg | head -2");
        let stream = echo_on_stream(&f, b"line one\r\nline two\r\n");

        // Chunk 1 = echo + begin marker + first output line.
        let split = f.sent_line.len() + 2 + f.marker.len() + 2 + b"line one\r\n".len();
        f.cq.feed_serial_data(&stream[..split]);
        f.cq.feed_serial_data(&stream[split..]);
        assert_clean_output(&mut f.rx, "line one\nline two");
    }

    /// CR-1 (d): slow command whose output trickles in over many chunks after
    /// the begin marker must not resolve early and must keep all output.
    #[test]
    fn test_slow_command_output_across_many_chunks() {
        let mut f = spawn_echo_fixture("dmesg | tail -3");
        let output = b"[ 12.34] line one\r\n[ 12.45] line two\r\n[ 12.56] line three\r\n";
        let stream = echo_on_stream(&f, output);

        // Echo + begin marker arrive first; output dribbles in later.
        let head = f.sent_line.len() + 2 + f.marker.len() + 2;
        f.cq.feed_serial_data(&stream[..head]);
        feed_chunked(&mut f.cq, &stream[head..], 9);
        assert_clean_output(
            &mut f.rx,
            "[ 12.34] line one\n[ 12.45] line two\n[ 12.56] line three",
        );
    }

    /// The stream often starts with leftover prompt bytes from the previous
    /// command (re-fed through the post-resolve recursion). Echo stripping
    /// must fall back to marker scanning when the stream does not start with
    /// the echoed line; the echoed line cannot contain a contiguous marker.
    #[test]
    fn test_echo_on_stream_with_prompt_prefix() {
        let mut f = spawn_echo_fixture("hostname");
        let mut stream = Vec::new();
        stream.extend_from_slice(b"\r\n# "); // leftover prompt bytes
        stream.extend_from_slice(&echo_on_stream(&f, b"rk3588-evb\r\n"));
        f.cq.feed_serial_data(&stream);
        assert_clean_output(&mut f.rx, "rk3588-evb");
    }

    #[test]
    fn test_busybox_pipeline_completes_without_fallback_suffix() {
        let available = std::process::Command::new("busybox")
            .args(["sh", "-c", ":"])
            .status()
            .is_ok_and(|status| status.success());
        if !available {
            return;
        }

        let mut f = spawn_echo_fixture("echo pipe-ready | grep pipe-ready");
        let shell_line = String::from_utf8(f.sent_line.clone()).unwrap();
        assert!(!shell_line.contains("; true"));
        let output = std::process::Command::new("busybox")
            .args(["sh", "-c", shell_line.trim_end()])
            .output()
            .unwrap();
        assert!(output.status.success());
        f.cq.feed_serial_data(&output.stdout);
        assert_clean_output(&mut f.rx, "pipe-ready");
    }
}
