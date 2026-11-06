//! Buffered diagnostic output with a dedicated I/O owner.
//!
//! UART evidence uses LogManager. Diagnostic formatting must not synchronously
//! open or write files on the serial task, even when verbose tracing is enabled.

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[derive(Default)]
struct Accounting {
    pending: AtomicU64,
    rejected: AtomicU64,
    cancelled: AtomicBool,
}

struct Queue {
    sender: Mutex<Option<mpsc::SyncSender<Vec<u8>>>>,
    accounting: Arc<Accounting>,
}

#[derive(Clone)]
pub struct DiagnosticWriter(Arc<Queue>);

pub struct RecordWriter {
    queue: Arc<Queue>,
    bytes: Vec<u8>,
}

impl Write for RecordWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for RecordWriter {
    fn drop(&mut self) {
        if self.bytes.is_empty() {
            return;
        }
        let size = self.bytes.len() as u64;
        let accounting = &self.queue.accounting;
        let sender = self.queue.sender.lock().unwrap();
        if let Some(sender) = sender.as_ref() {
            accounting.pending.fetch_add(size, Relaxed);
            if sender.try_send(std::mem::take(&mut self.bytes)).is_ok() {
                return;
            }
            accounting.pending.fetch_sub(size, Relaxed);
        }
        // Only diagnostic events can be rejected on overload. UART data uses
        // its own persistence queue and durability accounting.
        accounting.rejected.fetch_add(size, Relaxed);
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for DiagnosticWriter {
    type Writer = RecordWriter;

    fn make_writer(&'a self) -> Self::Writer {
        RecordWriter {
            queue: self.0.clone(),
            bytes: Vec::new(),
        }
    }
}

struct Worker {
    queue: Arc<Queue>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        // Closing the channel lets the writer drain without enqueueing a
        // shutdown message into a potentially full queue.
        self.queue.sender.lock().unwrap().take();
        let accounting = &self.queue.accounting;
        if let Some(thread) = self.thread.take() {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !thread.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if thread.is_finished() {
                if thread.join().is_err() {
                    let _ = writeln!(io::stderr(), "Diagnostic log worker panicked");
                }
            } else {
                // A blocked syscall cannot be cancelled by joining the thread.
                accounting.cancelled.store(true, Relaxed);
            }
        }
        let pending = accounting.pending.load(Relaxed);
        let rejected = accounting.rejected.load(Relaxed);
        if pending != 0 || rejected != 0 {
            // The sink may itself be stderr and may still hold its lock.
            // Bound the wait for reporting as well as for draining.
            let (done, reported) = mpsc::channel();
            if std::thread::Builder::new()
                .name("diagnostic-log-report".into())
                .spawn(move || {
                    let _ = writeln!(
                        io::stderr(),
                        "Diagnostic log incomplete: {pending} bytes pending, {rejected} bytes rejected"
                    );
                    let _ = done.send(());
                })
                .is_ok()
            {
                let _ = reported.recv_timeout(Duration::from_millis(100));
            }
        }
    }
}

fn retry_io<T>(accounting: &Accounting, mut operation: impl FnMut() -> io::Result<T>) -> Option<T> {
    let mut reported = false;
    while !accounting.cancelled.load(Relaxed) {
        match operation() {
            Ok(value) => return Some(value),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                if !reported {
                    let _ = writeln!(io::stderr(), "Diagnostic log I/O failed; retrying: {error}");
                    reported = true;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
    None
}

fn write_record(writer: &mut impl Write, accounting: &Accounting, bytes: &[u8]) -> Option<()> {
    let mut offset = 0;
    while offset < bytes.len() {
        offset += retry_io(accounting, || match writer.write(&bytes[offset..]) {
            Ok(0) => Err(io::ErrorKind::WriteZero.into()),
            result => result,
        })?;
    }
    retry_io(accounting, || writer.flush())
}

fn spawn(mut writer: impl Write + Send + 'static) -> io::Result<(DiagnosticWriter, Worker)> {
    let (sender, receiver) = mpsc::sync_channel::<Vec<u8>>(4096);
    let accounting = Arc::new(Accounting::default());
    let queue = Arc::new(Queue {
        sender: Mutex::new(Some(sender)),
        accounting: accounting.clone(),
    });
    let thread = std::thread::Builder::new()
        .name("diagnostic-log-writer".into())
        .spawn(move || {
            while let Ok(bytes) = receiver.recv() {
                if write_record(&mut writer, &accounting, &bytes).is_none() {
                    break;
                }
                accounting.pending.fetch_sub(bytes.len() as u64, Relaxed);
            }
        })?;
    Ok((
        DiagnosticWriter(queue.clone()),
        Worker {
            queue,
            thread: Some(thread),
        },
    ))
}

static WORKER: Mutex<Option<Worker>> = Mutex::new(None);

/// Install the process diagnostic sink before initializing tracing.
pub fn start(writer: impl Write + Send + 'static) -> io::Result<DiagnosticWriter> {
    let mut slot = WORKER.lock().expect("diagnostic worker registry poisoned");
    if slot.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "diagnostic writer already installed",
        ));
    }
    let (writer, worker) = spawn(writer)?;
    *slot = Some(worker);
    Ok(writer)
}

/// Drain accepted events before process::exit, which does not run destructors.
pub fn finalize() {
    let worker = WORKER
        .lock()
        .expect("diagnostic worker registry poisoned")
        .take();
    drop(worker);
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::fmt::MakeWriter;

    #[test]
    fn partial_write_then_storage_failure_retries_without_duplication() {
        struct FailingSink {
            output: Vec<u8>,
            calls: usize,
        }
        impl Write for FailingSink {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.calls += 1;
                if self.calls == 2 {
                    return Err(io::Error::from_raw_os_error(28));
                }
                let n = bytes.len().min(3);
                self.output.extend_from_slice(&bytes[..n]);
                Ok(n)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut sink = FailingSink {
            output: Vec::new(),
            calls: 0,
        };
        write_record(&mut sink, &Accounting::default(), b"abcdefgh").unwrap();
        assert_eq!(sink.output, b"abcdefgh");
    }

    #[test]
    fn slow_sink_does_not_block_producer_and_shutdown_drains_in_order() {
        struct SlowSink {
            entered: mpsc::Sender<()>,
            release: mpsc::Receiver<()>,
            output: Arc<Mutex<Vec<u8>>>,
        }
        impl Write for SlowSink {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.entered.send(()).unwrap();
                self.release.recv().unwrap();
                self.output.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (queued_tx, queued_rx) = mpsc::channel();
        let output = Arc::new(Mutex::new(Vec::new()));
        let (writer, worker) = spawn(SlowSink {
            entered: entered_tx,
            release: release_rx,
            output: output.clone(),
        })
        .unwrap();
        let producer = std::thread::spawn(move || {
            writer.make_writer().write_all(b"first\n").unwrap();
            entered_rx.recv().unwrap();
            writer.make_writer().write_all(b"second\n").unwrap();
            queued_tx.send(()).unwrap();
            drop(worker);
        });
        let queued = queued_rx.recv_timeout(Duration::from_secs(2));
        release_tx.send(()).unwrap();
        release_tx.send(()).unwrap();
        producer.join().unwrap();
        queued.expect("producer blocked on diagnostic I/O");
        assert_eq!(*output.lock().unwrap(), b"first\nsecond\n");
    }
}
