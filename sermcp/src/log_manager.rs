//! Log manager — per-power-cycle log rotation, stored in `.dut-serial/logs/`.
//!
//! One power-on → power-off cycle = one `boot-NNN.log` archive file.
//! `current.serial.log` is always a **regular file** holding the current cycle's
//! data; `boot-NNN.log` files are standalone archives created on boot boundaries.
//! No symlinks are used (symlinks caused a truncation bug when `start_new_cycle`
//! opened `current.serial.log` with `O_TRUNC`, which followed the symlink and
//! erased the just-flushed archive).

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

/// Message sent to the background writer thread.
enum WriterMsg {
    /// Data to append to both log files.
    Data(Vec<u8>),
    /// Rotate: close current files and reopen (follows the LogManager's
    /// current_path changes from `start_new_cycle`).
    Rotate(PathBuf, PathBuf),
    /// Flush + respond via the oneshot (used by `flush_sync()` and shutdown).
    Flush(std::sync::mpsc::Sender<()>),
}

pub struct LogManager {
    log_dir: PathBuf,
    dut_dir: PathBuf,
    max_logs: usize,
    current_file: Option<fs::File>,
    current_path: Option<PathBuf>,
    /// `full.serial.log`: continuous full log (never truncated).
    full_file: Option<fs::File>,
    boot_number: u32,
    /// In-memory ring buffer — board data is buffered here while running;
    /// flushed to a numbered boot file when a new boot cycle is detected.
    pub ring_buffer: Vec<u8>,
    /// Monotonic count of bytes appended to `ring_buffer`. Unlike the current
    /// vector length, this remains a stable observation cursor when old bytes
    /// are drained or the buffer is cleared at a boot boundary.
    stream_bytes_written: u64,
    /// Start offset of the current boot sequence within `ring_buffer`.
    boot_start_pos: usize,
    /// Ring buffer max capacity (default 2 MB).
    max_buffer_size: usize,
    /// Send end of the writer channel. `write()` pushes data here; a single
    /// background thread drains it and does all file I/O sequentially.
    writer_tx: Option<std::sync::mpsc::Sender<WriterMsg>>,
    /// Handle to the writer thread (joined on close / rotation).
    writer_thread: Option<std::thread::JoinHandle<()>>,
}

impl LogManager {
    pub fn new(project_dir: &Path, max_logs: usize, _max_file_size_mb: u64, dut_dir: &str) -> Self {
        let dut_dir_path = project_dir.join(dut_dir);
        let log_dir = dut_dir_path.join("logs");
        let live_current = log_dir.join("current.serial.log");

        Self {
            log_dir,
            dut_dir: dut_dir_path,
            max_logs,
            current_file: None,
            // Live file from construction (not only after the first boot
            // rotation): serial_get_logs(archive=0) and poll cursors read
            // through current_path, and a freshly started server must see
            // the data the writer is already appending.
            current_path: Some(live_current),
            full_file: None,
            boot_number: 0,
            ring_buffer: Vec::new(),
            stream_bytes_written: 0,
            boot_start_pos: 0,
            max_buffer_size: 2 * 1024 * 1024, // 2 MB
            writer_tx: None,
            writer_thread: None,
        }
    }

    pub fn current_path(&self) -> Option<&Path> {
        self.current_path.as_deref()
    }

    /// Absolute path of the `.dut-serial/logs/` directory hosting every serial
    /// log file (`current.serial.log`, `full.serial.log`, `boot-NNN_*.log`).
    /// Exposed so the MCP `resources` capability can list/serve them as
    /// `Resource`s without the caller having to know the on-disk layout.
    pub fn log_dir(&self) -> &Path {
        &self.log_dir
    }

    /// Absolute path of the parent `.dut-serial/` directory (state files,
    /// `unclassified.log`, pid files, ...).
    pub fn dut_dir(&self) -> &Path {
        &self.dut_dir
    }

    pub fn boot_number(&self) -> u32 {
        self.boot_number
    }

    fn next_boot_number(&mut self) -> u32 {
        let count_file = self.dut_dir.join(".boot_count");
        fs::create_dir_all(&self.dut_dir).ok();
        let n = fs::read_to_string(&count_file)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(0)
            + 1;
        fs::write(&count_file, n.to_string()).ok();
        self.boot_number = n;
        n
    }

    /// Open the current cycle log file (test helper).
    ///
    /// Creates a numbered `boot-NNN.log` and points `current_path` at it.
    /// Production code uses `ensure_current_file` + `mark_boot_start` instead.
    /// Kept as a public test helper because several test modules across the
    /// crate rely on it to set up a known boot-cycle state quickly.
    #[cfg(test)]
    pub fn open_current(&mut self) {
        let n = self.next_boot_number();
        fs::create_dir_all(&self.log_dir).ok();

        let ts = chrono::Local::now().format("%Y%m%d_%H%M%S");
        let fname = format!("boot-{:03}_{ts}.log", n);
        let path = self.log_dir.join(&fname);

        match fs::OpenOptions::new().append(true).create(true).open(&path) {
            Ok(mut file) => {
                let header = format!(
                    "=== Boot #{} — {} ===\n",
                    n,
                    chrono::Local::now().to_rfc3339()
                );
                file.write_all(header.as_bytes()).ok();
                self.current_file = Some(file);
                self.current_path = Some(path);
            }
            Err(e) => {
                tracing::error!("Failed to open log file {path:?}: {e}");
            }
        }
    }

    /// Append raw serial data: writes to `current.serial.log` +
    /// `full.serial.log` + the in-memory ring buffer.
    ///
    /// Ring buffer is updated synchronously (instant, needed by boot detection).
    /// File I/O is pushed to a **single background writer thread** via channel
    /// — no per-write thread spawn, no `open()` per chunk. The writer thread
    /// keeps file handles open and does sequential `write_all`.
    pub fn write(&mut self, data: &[u8]) {
        if self.current_file.is_none() {
            self.ensure_current_file();
        }
        // ── Ring buffer (synchronous, instant) ──────────────────────────
        self.ring_buffer.extend_from_slice(data);
        self.stream_bytes_written = self.stream_bytes_written.saturating_add(data.len() as u64);
        if self.ring_buffer.len() > self.max_buffer_size {
            let drain = self.ring_buffer.len() - self.max_buffer_size / 2;
            self.ring_buffer.drain(..drain);
            self.boot_start_pos = self.boot_start_pos.saturating_sub(drain);
        }

        // ── File I/O → background writer thread ─────────────────────────
        if !data.is_empty() {
            self.ensure_writer();
            let message = WriterMsg::Data(data.to_vec());
            if let Some(ref tx) = self.writer_tx
                && let Err(err) = tx.send(message)
            {
                // The worker exited unexpectedly. Clear its stale sender,
                // join it, start a replacement, and retry this same chunk.
                self.writer_tx.take();
                if let Some(handle) = self.writer_thread.take() {
                    let _ = handle.join();
                }
                self.ensure_writer();
                if let Some(ref retry_tx) = self.writer_tx {
                    let _ = retry_tx.send(err.0);
                }
            }
        }
    }

    /// Absolute cursor at the end of the shared serial stream.
    pub fn stream_cursor(&self) -> u64 {
        self.stream_bytes_written
    }

    /// Whether bytes were appended after an absolute stream cursor.
    pub fn stream_has_new(&self, cursor: u64) -> bool {
        self.stream_bytes_written != cursor
    }

    /// Retained bytes after an absolute stream cursor and whether older bytes
    /// needed by the caller have already fallen out of the ring buffer.
    pub fn stream_tail_since(&self, cursor: u64) -> (Vec<u8>, bool) {
        let base = self
            .stream_bytes_written
            .saturating_sub(self.ring_buffer.len() as u64);
        let truncated = cursor < base;
        let retained_cursor = cursor.max(base).min(self.stream_bytes_written);
        let start = (retained_cursor - base) as usize;
        (self.ring_buffer[start..].to_vec(), truncated)
    }

    /// Start the background writer thread (lazy, on first `write()`).
    fn ensure_writer(&mut self) {
        if self.writer_tx.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel::<WriterMsg>();
        let mut current_path = self
            .current_path
            .clone()
            .unwrap_or_else(|| self.log_dir.join("current.serial.log"));
        let mut full_path = self.log_dir.join("full.serial.log");

        let handle = match std::thread::Builder::new()
            .name("serial-log-writer".to_string())
            .spawn(move || {
                // Keep file handles open for the lifetime of the writer thread.
                let mut current_file: Option<std::fs::File> = std::fs::OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open(&current_path)
                    .ok();
                let mut full_file: Option<std::fs::File> = std::fs::OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open(&full_path)
                    .ok();

                for msg in rx {
                    match msg {
                        WriterMsg::Data(data) => {
                            write_with_reopen(&mut current_file, &current_path, &data);
                            write_with_reopen(&mut full_file, &full_path, &data);
                        }
                        WriterMsg::Rotate(new_current, new_full) => {
                            // Close old handles, open new ones.
                            current_path = new_current;
                            full_path = new_full;
                            current_file = std::fs::OpenOptions::new()
                                .append(true)
                                .create(true)
                                .open(&current_path)
                                .ok();
                            full_file = std::fs::OpenOptions::new()
                                .append(true)
                                .create(true)
                                .open(&full_path)
                                .ok();
                        }
                        WriterMsg::Flush(reply) => {
                            // Flush file handles to disk.
                            if let Some(ref mut f) = current_file {
                                let _ = f.flush();
                            }
                            if let Some(ref mut f) = full_file {
                                let _ = f.flush();
                            }
                            let _ = reply.send(());
                        }
                    }
                }
                // Channel closed — final flush.
                if let Some(ref mut f) = current_file {
                    let _ = f.flush();
                }
                if let Some(ref mut f) = full_file {
                    let _ = f.flush();
                }
            }) {
            Ok(handle) => handle,
            Err(e) => {
                tracing::error!("Failed to start serial log writer: {e}");
                // Leave writer_tx unset. The next write() calls ensure_writer()
                // again instead of permanently dropping all future log data.
                return;
            }
        };

        self.writer_tx = Some(tx);
        self.writer_thread = Some(handle);
    }

    /// Wait for all pending file writes to land on disk (test determinism +
    /// `flush_boot_log`/`rotate` synchronization).
    pub fn flush_sync(&mut self) {
        if let Some(ref tx) = self.writer_tx {
            let (reply, rx) = std::sync::mpsc::channel();
            let _ = tx.send(WriterMsg::Flush(reply));
            let _ = rx.recv();
        }
    }

    /// Tell the writer thread to rotate files (used by `start_new_cycle`).
    fn writer_rotate(&mut self, current_path: PathBuf, full_path: PathBuf) {
        if let Some(ref tx) = self.writer_tx {
            let _ = tx.send(WriterMsg::Rotate(current_path, full_path));
        }
    }

    /// Ensure `current.serial.log` + `full.serial.log` are open.
    ///
    /// If `current.serial.log` exists as a **symlink** (leftover from a
    /// previous version that used symlinks), remove it and create a regular
    /// file instead — this prevents append-mode writes from following the
    /// symlink into an archive file.
    pub fn ensure_current_file(&mut self) {
        fs::create_dir_all(&self.log_dir).ok();
        // current.serial.log (current boot cycle, always a regular file)
        if self.current_file.is_none() {
            let path = self.log_dir.join("current.serial.log");
            // If a symlink lingered from an older version, remove it so we
            // don't append into an archive file via the symlink.
            if path.is_symlink() {
                fs::remove_file(&path).ok();
            }
            match fs::OpenOptions::new().create(true).append(true).open(&path) {
                Ok(file) => {
                    self.current_file = Some(file);
                    self.current_path = Some(path);
                }
                Err(e) => tracing::error!("Failed to open current.serial.log: {e}"),
            }
        }
        // full.serial.log (continuous, never truncated)
        if self.full_file.is_none() {
            let full = self.log_dir.join("full.serial.log");
            match fs::OpenOptions::new().create(true).append(true).open(&full) {
                Ok(file) => {
                    self.full_file = Some(file);
                }
                Err(e) => tracing::error!("Failed to open full.serial.log: {e}"),
            }
        }
    }

    /// Mark a new boot cycle: save snapshot → truncate
    /// `current.serial.log` → reset ring buffer position.
    pub fn mark_boot_start(&mut self) {
        self.flush_boot_log();
        self.start_new_cycle();
    }

    /// End the current cycle: close the old file handle, remove any existing
    /// `current.serial.log` (symlink or regular), and create a fresh empty
    /// regular file for the new cycle.
    fn start_new_cycle(&mut self) {
        self.flush_sync();
        self.current_file.take();
        let path = self.log_dir.join("current.serial.log");
        // Remove whatever is at `current.serial.log` (regular file or stale
        // symlink) so that `OpenOptions::truncate` acts on a NEW regular file,
        // NOT on a symlink target (which would erase a just-flushed archive).
        fs::remove_file(&path).ok();
        fs::create_dir_all(&self.log_dir).ok();
        self.current_file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .ok();
        self.current_path = Some(path.clone());
        self.cleanup_old_logs();
        self.boot_start_pos = self.ring_buffer.len();
        // Notify writer thread: close old handles, reopen on the new log file.
        let full_path = self.log_dir.join("full.serial.log");
        self.writer_rotate(path, full_path);
    }

    /// Flush the ring buffer contents to a new numbered boot log file.
    pub fn flush_boot_log(&mut self) {
        if self.boot_start_pos >= self.ring_buffer.len() {
            return; // No new data
        }
        let boot_data = self.ring_buffer[self.boot_start_pos..].to_vec();
        if boot_data.is_empty() {
            return;
        }
        // Create a new archive file
        fs::create_dir_all(&self.log_dir).ok();
        let n = self.next_boot_number();
        let ts = chrono::Local::now().format("%Y%m%d_%H%M%S");
        let fname = format!("boot-{:03}_{ts}.log", n);
        let path = self.log_dir.join(&fname);

        if let Ok(mut file) = fs::OpenOptions::new().append(true).create(true).open(&path) {
            file.write_all(&boot_data).ok();
            tracing::info!("Boot log saved: {} ({} bytes)", fname, boot_data.len());
        }
        // Clean up old logs
        self.cleanup_old_logs();
        // Reset the start position
        self.boot_start_pos = self.ring_buffer.len();
    }

    /// Rotate: flush buffer → open a new log → clean up old logs.
    /// An explicit `rotate` always creates a new boot file (even if the ring
    /// buffer is empty).
    pub fn rotate(&mut self) {
        let had_data = self.boot_start_pos < self.ring_buffer.len();
        self.flush_boot_log();
        if !had_data {
            // flush_boot_log skipped (no ring buffer data) → manually create
            // an empty boot file so the cycle boundary is recorded.
            fs::create_dir_all(&self.log_dir).ok();
            let n = self.next_boot_number();
            let ts = chrono::Local::now().format("%Y%m%d_%H%M%S");
            let fname = format!("boot-{:03}_{ts}.log", n);
            let path = self.log_dir.join(&fname);
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .ok();
        }
        self.start_new_cycle();
    }

    pub fn close(&mut self) {
        // Flush + join writer thread before dropping file handles.
        self.flush_sync();
        self.writer_tx.take(); // close channel → writer thread exits loop
        if let Some(handle) = self.writer_thread.take() {
            let _ = handle.join();
        }
        self.current_file.take();
        self.full_file.take();
    }

    /// Append unclassified lines to `.dut-serial/unclassified.log`
    /// (for Agent auto-learning analysis).
    pub fn append_unclassified(&mut self, lines: &[String]) -> Result<(), String> {
        let path = self.dut_dir.join("unclassified.log");
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("Cannot open unclassified.log: {e}"))?;
        for line in lines {
            writeln!(file, "{line}").map_err(|e| format!("Write error: {e}"))?;
        }
        Ok(())
    }

    /// Read all unclassified lines from `.dut-serial/unclassified.log`.
    pub fn read_unclassified(&self) -> Vec<String> {
        let path = self.dut_dir.join("unclassified.log");
        match fs::read_to_string(&path) {
            Ok(content) => content.lines().map(|s| s.to_string()).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Truncate the unclassified log (called on new boot cycle).
    pub fn truncate_unclassified(&self) {
        let path = self.dut_dir.join("unclassified.log");
        if path.exists() {
            fs::remove_file(&path).ok();
        }
    }

    /// List all archived boot logs (newest first).
    pub fn list_archives(&self) -> Vec<ArchiveInfo> {
        let mut logs = self.list_log_files();
        logs.sort_by(|a, b| b.path.cmp(&a.path));
        logs.into_iter()
            .enumerate()
            .map(|(i, p)| ArchiveInfo {
                index: i,
                filename: p.name,
                size_bytes: p.size,
            })
            .collect()
    }

    /// Read a specified archive log. Streams via `BufReader` to avoid loading
    /// the entire file into memory at once (prevents OOM on large logs).
    pub fn read_log(
        &self,
        archive_index: usize,
        lines: usize,
        pattern: Option<&str>,
    ) -> LogContent {
        let Some(target) = self.read_target(archive_index) else {
            return LogContent {
                content: String::new(),
                filename: String::new(),
                total_lines: 0,
                filtered_lines: 0,
            };
        };
        Self::read_target_content(target, lines, pattern)
    }

    /// Snapshot the selected archive under the engine lock. The returned
    /// target owns its path, so callers can release the lock before doing disk
    /// I/O; a subsequent boot rollover cannot retarget an in-flight read.
    /// Read target for the LIVE current-cycle file (`current.serial.log`).
    /// Valid from construction — a freshly started server must be able to
    /// serve its in-progress log before the first boot rotation.
    pub fn live_read_target(&self) -> Option<LogReadTarget> {
        self.current_path.as_ref().map(|path| LogReadTarget {
            filename: "current.serial.log".to_string(),
            path: path.clone(),
        })
    }

    pub fn read_target(&self, archive_index: usize) -> Option<LogReadTarget> {
        let logs = self.list_log_files_sorted();
        logs.get(archive_index).map(|target| LogReadTarget {
            filename: target.name.clone(),
            path: target.path.clone(),
        })
    }

    /// Read a previously snapshotted archive without accessing LogManager
    /// state. Intended for `spawn_blocking` callers.
    pub fn read_target_content(
        target: LogReadTarget,
        lines: usize,
        pattern: Option<&str>,
    ) -> LogContent {
        let filename = target.filename;

        let re = pattern.and_then(|pat| {
            regex::RegexBuilder::new(pat)
                .case_insensitive(true)
                .build()
                .ok()
        });

        // Stream the file line by line to avoid OOM on large files.
        let file = match fs::File::open(&target.path) {
            Ok(f) => f,
            Err(_) => {
                return LogContent {
                    content: String::new(),
                    filename,
                    total_lines: 0,
                    filtered_lines: 0,
                };
            }
        };
        let reader = BufReader::new(file);

        let mut total_lines: usize = 0;
        let mut filtered: Vec<String> = Vec::new();

        for line in reader.lines().map_while(Result::ok) {
            total_lines += 1;
            let matches = match &re {
                Some(rx) => rx.is_match(&line),
                None => true,
            };
            if matches {
                filtered.push(line);
            }
        }

        let filtered_lines = filtered.len();
        // Keep only the last `lines` entries if a limit is set.
        let result: Vec<String> = if lines > 0 && filtered.len() > lines {
            filtered.split_off(filtered.len() - lines)
        } else {
            filtered
        };

        LogContent {
            content: result.join("\n"),
            filename,
            total_lines,
            filtered_lines,
        }
    }

    // ── internal ──

    fn cleanup_old_logs(&self) {
        let mut logs = self.list_log_files();
        // Sort by filename (includes timestamp) for deterministic ordering.
        logs.sort_by(|a, b| a.path.cmp(&b.path));
        if logs.len() > self.max_logs {
            let to_remove = logs.len() - self.max_logs;
            for old in &logs[..to_remove] {
                fs::remove_file(&old.path).ok();
                tracing::debug!("Cleaned old log: {}", old.name);
            }
        }
    }

    fn list_log_files(&self) -> Vec<LogFileInfo> {
        let mut result = Vec::new();
        if let Ok(entries) = fs::read_dir(&self.log_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("log")
                    && !path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n == "current.serial.log" || n == "full.serial.log")
                {
                    let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                    result.push(LogFileInfo {
                        name: path.file_name().unwrap().to_string_lossy().to_string(),
                        size,
                        path,
                    });
                }
            }
        }
        result
    }

    fn list_log_files_sorted(&self) -> Vec<LogFileInfo> {
        let mut logs = self.list_log_files();
        logs.sort_by(|a, b| b.path.cmp(&a.path));
        logs
    }
}

/// Append data and make a closed/failed handle recoverable on the next write.
fn write_with_reopen(file: &mut Option<std::fs::File>, path: &Path, data: &[u8]) {
    if file.is_none() {
        match std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
        {
            Ok(opened) => *file = Some(opened),
            Err(e) => {
                tracing::error!("Failed to reopen serial log {}: {e}", path.display());
                return;
            }
        }
    }
    if let Some(opened) = file
        && let Err(e) = opened.write_all(data)
    {
        tracing::error!("Failed to write serial log {}: {e}", path.display());
        *file = None;
    }
}

struct LogFileInfo {
    name: String,
    size: u64,
    path: PathBuf,
}

#[derive(Debug)]
pub struct ArchiveInfo {
    pub index: usize,
    pub filename: String,
    pub size_bytes: u64,
}

#[derive(Debug)]
pub struct LogContent {
    pub content: String,
    pub filename: String,
    pub total_lines: usize,
    pub filtered_lines: usize,
}

#[derive(Debug, Clone)]
pub struct LogReadTarget {
    filename: String,
    path: PathBuf,
}

#[cfg(test)]
mod tests {
    /// Regression (live acceptance): a FRESH LogManager must serve the
    /// live current-cycle log through live_read_target() BEFORE any boot
    /// rotation — the writer appends to current.serial.log from startup,
    /// and serial_get_logs(archive=0) reads through this path.
    #[test]
    fn fresh_log_manager_serves_live_current_log() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let mut lm = LogManager::new(tmp.path(), 10, 100, ".dut-serial");
        lm.write(b"hello live log\n");
        // give the writer thread a moment to flush
        std::thread::sleep(std::time::Duration::from_millis(200));
        let target = lm
            .live_read_target()
            .expect("live target must exist before any rotation");
        let content = LogManager::read_target_content(target, 50, None);
        assert!(
            content.content.contains("hello live log"),
            "fresh live log must be readable, got: {:?}",
            content.content
        );
        assert_eq!(content.filename, "current.serial.log");
    }

    use super::*;
    use tempfile::TempDir;

    fn create_test_log_manager(max_logs: usize, max_size_mb: u64) -> (LogManager, TempDir) {
        let tmp = TempDir::new().unwrap();
        let lm = LogManager::new(tmp.path(), max_logs, max_size_mb, ".dut-serial");
        (lm, tmp)
    }

    fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }

    #[test]
    fn test_open_current_creates_file() {
        let (mut lm, _tmp) = create_test_log_manager(10, 100);
        lm.open_current();

        assert_eq!(lm.boot_number(), 1);
        assert!(lm.current_path().is_some());
        assert!(lm.current_path().unwrap().exists());
        assert_eq!(lm.current_path().unwrap().extension().unwrap(), "log");
    }

    #[test]
    fn test_boot_number_increments() {
        let (mut lm, _tmp) = create_test_log_manager(10, 100);

        lm.open_current();
        assert_eq!(lm.boot_number(), 1);

        lm.rotate();
        assert_eq!(lm.boot_number(), 2);

        lm.rotate();
        assert_eq!(lm.boot_number(), 3);
    }

    #[test]
    fn test_write_data() {
        let (mut lm, _tmp) = create_test_log_manager(10, 100);
        lm.open_current();

        lm.write(b"line 1\n");
        lm.flush_sync();
        lm.write(b"line 2\n");
        lm.flush_sync();
        lm.write(b"line 3\n");
        lm.flush_sync();

        let content = fs::read_to_string(lm.current_path().unwrap()).unwrap();
        assert!(content.contains("line 1"));
        assert!(content.contains("line 2"));
        assert!(content.contains("line 3"));
    }

    #[test]
    fn test_writer_reopens_after_initial_open_failure() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("not-created-yet/serial.log");
        let mut file = None;

        write_with_reopen(&mut file, &path, b"lost-attempt");
        assert!(file.is_none());

        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        write_with_reopen(&mut file, &path, b"recovered\n");
        assert!(file.is_some());
        assert_eq!(std::fs::read(&path).unwrap(), b"recovered\n");
    }

    #[test]
    fn test_write_preserves_raw_dut_bytes_in_current_full_and_ring() {
        let (mut lm, tmp) = create_test_log_manager(10, 100);
        let raw = b"state \x1b[32mUP\x1b[0m\0\x01\r\n    inet \x1b[32m127.0.0.1\x1b[0m/8\n";

        lm.ensure_current_file();
        lm.write(raw);
        lm.flush_sync();

        let log_dir = tmp.path().join(".dut-serial/logs");
        let current = fs::read(log_dir.join("current.serial.log")).unwrap();
        let full = fs::read(log_dir.join("full.serial.log")).unwrap();

        assert!(contains_bytes(&current, raw));
        assert!(contains_bytes(&full, raw));
        assert!(contains_bytes(&lm.ring_buffer, raw));
    }

    #[test]
    fn test_write_preserves_split_ip_color_sequences_without_inserting_m() {
        let (mut lm, tmp) = create_test_log_manager(10, 100);
        let chunks: [&[u8]; 5] = [
            b"2: eth0: <BROADCAST> mtu 1500 state \x1b[32",
            b"mUP\x1b[0",
            b"m group default\n    inet \x1b[32",
            b"m127.0.0.1\x1b[0",
            b"m/8 scope host lo\n",
        ];

        lm.ensure_current_file();
        for chunk in chunks {
            lm.write(chunk);
        }
        lm.flush_sync();

        let log = fs::read(tmp.path().join(".dut-serial/logs/current.serial.log")).unwrap();
        let expected = b"2: eth0: <BROADCAST> mtu 1500 state \x1b[32mUP\x1b[0m group default\n    inet \x1b[32m127.0.0.1\x1b[0m/8 scope host lo\n";

        assert!(contains_bytes(&log, expected));
        assert!(!contains_bytes(&log, b"state mUP"));
        assert!(!contains_bytes(&log, b"inet m127"));
    }

    #[test]
    fn test_rotate_preserves_raw_dut_bytes_in_archive() {
        let (mut lm, tmp) = create_test_log_manager(10, 100);
        let raw = b"awlink0: <NOARP,ECHO> state \x1b[31mDOWN\x1b[0m\0\n";

        lm.ensure_current_file();
        lm.write(raw);
        lm.rotate();
        lm.flush_sync();

        let log_dir = tmp.path().join(".dut-serial/logs");
        let archive = std::fs::read_dir(&log_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .find(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with("boot-") && n.ends_with(".log"))
            })
            .expect("expected archived boot log");
        let archived = fs::read(archive.path()).unwrap();

        assert!(contains_bytes(&archived, raw));
    }

    #[test]
    fn test_rotate_creates_new_file() {
        let (mut lm, _tmp) = create_test_log_manager(10, 100);

        lm.open_current();
        let path1 = lm.current_path().unwrap().to_path_buf();
        lm.write(b"boot 1 data\n");

        lm.rotate();
        let path2 = lm.current_path().unwrap().to_path_buf();
        lm.write(b"boot 2 data\n");

        assert_ne!(path1, path2);
        assert!(path1.exists());
        assert!(path2.exists());

        lm.flush_sync();
        let content1 = fs::read_to_string(&path1).unwrap();
        let content2 = fs::read_to_string(&path2).unwrap();
        assert!(content1.contains("boot 1 data"));
        assert!(content2.contains("boot 2 data"));
    }

    #[test]
    fn test_cleanup_old_logs() {
        let (mut lm, _tmp) = create_test_log_manager(3, 100); // keep only 3

        // Create first log
        lm.open_current();
        lm.write(b"boot 0\n");

        // Create 4 more logs via rotate (total 5)
        for i in 1..5 {
            lm.rotate();
            lm.write(format!("boot {i}\n").as_bytes());
        }

        assert!(!lm.log_dir.join("highlighted").exists());

        // Should keep at most max_logs
        let archives = lm.list_archives();
        assert!(
            archives.len() <= 3,
            "Expected <= 3 logs, got {}",
            archives.len()
        );
    }

    #[test]
    fn test_list_archives_ordering() {
        let (mut lm, _tmp) = create_test_log_manager(10, 100);

        // Create 3 logs with different content
        for i in 1..=3 {
            lm.open_current();
            lm.write(format!("boot {i}\n").as_bytes());
            std::thread::sleep(std::time::Duration::from_millis(10)); // ensure different timestamps
        }

        let archives = lm.list_archives();
        assert_eq!(archives.len(), 3);

        // Should be ordered newest first (by filename, which includes timestamp)
        assert!(archives[0].filename > archives[1].filename);
        assert!(archives[1].filename > archives[2].filename);
    }

    #[test]
    fn test_read_log_basic() {
        let (mut lm, _tmp) = create_test_log_manager(10, 100);
        lm.open_current();

        // Note: open_current writes a header line
        for i in 1..=10 {
            lm.write(format!("line {i}\n").as_bytes());
        }
        lm.flush_sync();

        let result = lm.read_log(0, 5, None);
        // Total lines includes header
        assert!(result.total_lines >= 10);
        // filtered_lines counts all lines before the `lines` limit
        assert_eq!(result.filtered_lines, result.total_lines);
        // Content should have at most 5 lines (limited by `lines` parameter)
        let content_lines = result.content.lines().count();
        assert_eq!(content_lines, 5);
        assert!(result.content.contains("line 10"));
    }

    #[test]
    fn test_read_log_with_pattern() {
        let (mut lm, _tmp) = create_test_log_manager(10, 100);
        lm.open_current();

        lm.write(b"ERROR: something failed\n");
        lm.write(b"INFO: all good\n");
        lm.write(b"ERROR: another failure\n");
        lm.write(b"DEBUG: debugging\n");
        lm.flush_sync();

        let result = lm.read_log(0, 100, Some("ERROR"));
        assert_eq!(result.filtered_lines, 2);
        assert!(result.content.contains("ERROR"));
        assert!(!result.content.contains("INFO"));
        assert!(!result.content.contains("DEBUG"));
    }

    #[test]
    fn test_read_log_archive_index() {
        let (mut lm, _tmp) = create_test_log_manager(10, 100);

        // Use the production path (ensure_current_file + write + rotate) so the
        // archive semantics match real behavior: each rotate flushes the current
        // cycle's ring buffer into a numbered boot-NNN.log archive.
        lm.ensure_current_file();
        lm.write(b"boot 1 data\n");
        lm.rotate(); // archives "boot 1 data" into boot-001

        lm.write(b"boot 2 data\n");
        lm.rotate(); // archives "boot 2 data" into boot-002

        // Read boot 2 (index 0 = newest)
        let result = lm.read_log(0, 100, None);
        assert!(result.content.contains("boot 2 data"));

        // Read boot 1 (index 1)
        let result = lm.read_log(1, 100, None);
        assert!(result.content.contains("boot 1 data"));

        // Invalid index
        let result = lm.read_log(99, 100, None);
        assert_eq!(result.content, "");
    }

    #[test]
    fn test_boot_count_file() {
        let (mut lm, tmp) = create_test_log_manager(10, 100);
        let count_file = tmp.path().join(".dut-serial/.boot_count");

        lm.open_current();
        assert_eq!(fs::read_to_string(&count_file).unwrap(), "1");

        lm.rotate();
        assert_eq!(fs::read_to_string(&count_file).unwrap(), "2");
    }

    #[test]
    fn test_close() {
        let (mut lm, _tmp) = create_test_log_manager(10, 100);
        lm.open_current();
        assert!(lm.current_path().is_some());

        lm.close();
        // After close, current_path should still be Some (just file handle closed)
        assert!(lm.current_path().is_some());
    }

    #[test]
    fn test_log_file_header() {
        let (mut lm, _tmp) = create_test_log_manager(10, 100);
        lm.open_current();

        let content = fs::read_to_string(lm.current_path().unwrap()).unwrap();
        assert!(content.contains("=== Boot #1"));
        assert!(content.contains("==="));
    }

    // ── Smoke tests: boot log splitting ─────────────────────────────────

    /// `ensure_current_file()` opens `current.serial.log` WITHOUT creating a
    /// numbered `boot-NNN.log`.  Boot log files are only created on BootStart.
    #[test]
    fn test_ensure_current_file_no_numbered_log() {
        let (mut lm, tmp) = create_test_log_manager(10, 100);
        let log_dir = tmp.path().join(".dut-serial/logs");

        lm.ensure_current_file();

        // current.serial.log should exist as a regular file (not a symlink)
        assert!(log_dir.join("current.serial.log").exists());
        assert!(!log_dir.join("current.serial.log").is_symlink());
        // But no numbered boot-NNN.log should have been created
        let numbered: Vec<_> = std::fs::read_dir(&log_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with("boot-") && n.ends_with(".log"))
            })
            .collect();
        assert!(
            numbered.is_empty(),
            "ensure_current_file() should NOT create numbered boot logs, got {}",
            numbered.len()
        );
    }

    /// Only `mark_boot_start()` creates numbered boot logs.
    #[test]
    fn test_mark_boot_start_creates_numbered_log() {
        let (mut lm, tmp) = create_test_log_manager(10, 100);
        let log_dir = tmp.path().join(".dut-serial/logs");

        // Write some data first
        lm.ensure_current_file();
        lm.write(b"SPL: U-Boot 2024\n");
        lm.write(b"kernel booting...\n");

        // Simulate BootStart detection
        lm.mark_boot_start();

        // Should have created a numbered boot-NNN.log
        let numbered: Vec<_> = std::fs::read_dir(&log_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with("boot-") && n.ends_with(".log"))
            })
            .collect();
        assert_eq!(
            numbered.len(),
            1,
            "mark_boot_start should create exactly 1 boot log"
        );
        // current.serial.log is a regular file (not a symlink)
        assert!(!log_dir.join("current.serial.log").is_symlink());
    }

    /// After `mark_boot_start`, the archived boot log retains its data (the
    /// symlink-truncation regression test).
    #[test]
    fn test_mark_boot_start_preserves_archive_content() {
        let (mut lm, tmp) = create_test_log_manager(10, 100);
        let log_dir = tmp.path().join(".dut-serial/logs");

        lm.ensure_current_file();
        lm.write(b"boot 1 line A\n");
        lm.write(b"boot 1 line B\n");

        lm.mark_boot_start();

        // The archived boot-NNN.log must NOT be empty (regression: symlink
        // truncation used to erase it).
        let numbered: Vec<_> = std::fs::read_dir(&log_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with("boot-") && n.ends_with(".log"))
            })
            .collect();
        assert_eq!(numbered.len(), 1);
        let archive_content = fs::read_to_string(numbered[0].path()).unwrap();
        assert!(
            archive_content.contains("boot 1 line A"),
            "archived boot log must preserve its data, got: {archive_content:?}"
        );
        assert!(archive_content.contains("boot 1 line B"));
    }

    /// After `mark_boot_start`, new data goes to a fresh cycle.
    #[test]
    fn test_mark_boot_start_resets_cycle() {
        let (mut lm, tmp) = create_test_log_manager(10, 100);
        let log_dir = tmp.path().join(".dut-serial/logs");

        lm.ensure_current_file();
        lm.write(b"boot 1 data\n");
        let boot1 = lm.boot_number();

        lm.mark_boot_start();
        assert_eq!(lm.boot_number(), boot1 + 1); // incremented

        lm.write(b"boot 2 data\n");
        lm.flush_sync();

        // boot 1 data should be in the archived file, not current
        let current = std::fs::read_to_string(log_dir.join("current.serial.log")).unwrap();
        assert!(current.contains("boot 2 data"));
        assert!(!current.contains("boot 1 data")); // flushed to numbered log
    }

    #[test]
    fn test_empty_log_returns_nothing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let lm = LogManager::new(tmp.path(), 10, 100, ".dut-serial");
        let result = lm.read_log(0, 50, None);
        assert!(result.content.is_empty());
    }

    #[test]
    fn test_archive_index_bounds() {
        let tmp = tempfile::TempDir::new().unwrap();
        let lm = LogManager::new(tmp.path(), 10, 100, ".dut-serial");
        // Out-of-bounds archive should return empty
        let result = lm.read_log(999, 50, None);
        assert!(result.content.is_empty());
    }

    /// Double `mark_boot_start` with no data in between should not create
    /// empty logs.
    #[test]
    fn test_double_mark_boot_start_no_empty_logs() {
        let (mut lm, tmp) = create_test_log_manager(10, 100);
        let log_dir = tmp.path().join(".dut-serial/logs");

        lm.ensure_current_file();
        lm.write(b"real boot data\n");
        lm.mark_boot_start();

        // Second mark_boot_start with NO new data → flush_boot_log returns early
        lm.mark_boot_start();

        let numbered: Vec<_> = std::fs::read_dir(&log_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with("boot-") && n.ends_with(".log"))
            })
            .collect();
        assert_eq!(
            numbered.len(),
            1,
            "double mark with no data should NOT create empty logs"
        );
    }
}
