//! Reliable local stdio transport for rmcp.
//!
//! Codex uses a nonblocking pipe for the server's stdout. Tokio's standard
//! stdout adapter accepts a buffer before its blocking worker has performed
//! the write, so a later `EAGAIN` is reported during flush after rmcp already
//! considered the JSON-RPC frame consumed. Use readiness-driven pipe writes on
//! Linux so backpressure yields `Pending`, never a lost response.

use std::io;

#[cfg(target_os = "linux")]
mod linux {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::pin::Pin;
    use std::task::{Context, Poll, ready};

    use tokio::io::AsyncWrite;
    use tokio::io::unix::AsyncFd;

    use super::io;

    pub struct ReliableStdout {
        inner: Inner,
    }

    enum Inner {
        /// Readiness-driven pipe writes — EAGAIN becomes `Pending`, never a
        /// lost response.
        Ready(AsyncFd<OwnedFd>),
        /// Fallback for descriptors epoll refuses (e.g. /dev/null or a
        /// regular file under some sandboxes): blocking writes. Such
        /// sinks never return EAGAIN, so backpressure cannot be lost.
        Blocking(OwnedFd),
    }

    impl ReliableStdout {
        pub fn new() -> io::Result<Self> {
            // Keep an owned descriptor for the lifetime of the transport. A
            // duplicate refers to the same pipe and remains valid if the
            // process-level stdout handle is otherwise replaced or closed.
            // SAFETY: F_DUPFD_CLOEXEC duplicates a valid process descriptor.
            let fd = unsafe { libc::fcntl(libc::STDOUT_FILENO, libc::F_DUPFD_CLOEXEC, 3) };
            if fd == -1 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: ownership of the newly duplicated descriptor transfers
            // exactly once into OwnedFd.
            Self::from_owned_fd(unsafe { OwnedFd::from_raw_fd(fd) })
        }

        fn from_owned_fd(fd: OwnedFd) -> io::Result<Self> {
            set_nonblocking(fd.as_raw_fd())?;
            match AsyncFd::new(fd) {
                Ok(inner) => Ok(Self {
                    inner: Inner::Ready(inner),
                }),
                // epoll_ctl ADD fails EPERM on file-backed stdout (/dev/null
                // under some sandboxes, redirected files). Such sinks cannot
                // produce EAGAIN, so a plain blocking write is safe.
                Err(_) => Ok(Self {
                    inner: Inner::Blocking(dup_stdout()?),
                }),
            }
        }
    }

    fn dup_stdout() -> io::Result<OwnedFd> {
        // SAFETY: F_DUPFD_CLOEXEC duplicates a valid process descriptor.
        let fd = unsafe { libc::fcntl(libc::STDOUT_FILENO, libc::F_DUPFD_CLOEXEC, 3) };
        if fd == -1 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: ownership of the newly duplicated descriptor transfers
        // exactly once into OwnedFd.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    impl AsyncWrite for ReliableStdout {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            match &self.inner {
                Inner::Blocking(fd) => {
                    // SAFETY: fd is owned by `inner`; buf remains valid for
                    // the duration of this synchronous write call.
                    let written = unsafe {
                        libc::write(
                            fd.as_raw_fd(),
                            buf.as_ptr().cast::<libc::c_void>(),
                            buf.len(),
                        )
                    };
                    if written == -1 {
                        Poll::Ready(Err(io::Error::last_os_error()))
                    } else {
                        Poll::Ready(Ok(written as usize))
                    }
                }
                Inner::Ready(_) => {
                    let Inner::Ready(inner) = &self.inner else {
                        unreachable!()
                    };
                    loop {
                        let mut guard = ready!(inner.poll_write_ready(cx))?;
                        match guard.try_io(|inner| {
                            let fd = inner.get_ref().as_raw_fd();
                            // SAFETY: fd is owned by `inner`; buf remains valid
                            // for the duration of this synchronous write call.
                            let written = unsafe {
                                libc::write(fd, buf.as_ptr().cast::<libc::c_void>(), buf.len())
                            };
                            if written == -1 {
                                Err(io::Error::last_os_error())
                            } else {
                                Ok(written as usize)
                            }
                        }) {
                            Ok(result) => return Poll::Ready(result),
                            Err(_would_block) => continue,
                        }
                    }
                }
            }
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            // Writes go directly to the pipe; there is no userspace buffer.
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    fn set_nonblocking(fd: RawFd) -> io::Result<()> {
        // SAFETY: F_GETFL only reads status flags from a valid descriptor.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags == -1 {
            return Err(io::Error::last_os_error());
        }
        if flags & libc::O_NONBLOCK == 0 {
            // SAFETY: F_SETFL updates status flags on the same descriptor.
            if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    pub fn reliable_stdout() -> io::Result<ReliableStdout> {
        ReliableStdout::new()
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::io::Read;
        use std::thread;
        use std::time::Duration;
        use tokio::io::AsyncWriteExt;

        #[tokio::test]
        async fn large_response_waits_for_nonblocking_pipe_backpressure() {
            let mut fds = [-1; 2];
            // SAFETY: fds points to storage for exactly two descriptors.
            assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
            set_nonblocking(fds[1]).unwrap();

            let fill = [0x5a_u8; 4096];
            let mut prefilled = 0_usize;
            loop {
                // SAFETY: fds[1] and the source buffer are valid.
                let n = unsafe {
                    libc::write(fds[1], fill.as_ptr().cast::<libc::c_void>(), fill.len())
                };
                if n == -1 {
                    assert_eq!(
                        io::Error::last_os_error().raw_os_error(),
                        Some(libc::EAGAIN)
                    );
                    break;
                }
                prefilled += n as usize;
            }

            // SAFETY: each pipe descriptor transfers ownership exactly once.
            let reader_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
            // SAFETY: each pipe descriptor transfers ownership exactly once.
            let writer_fd = unsafe { OwnedFd::from_raw_fd(fds[1]) };
            let mut writer = ReliableStdout::from_owned_fd(writer_fd).unwrap();
            let response = vec![0x41_u8; 600 * 1024];
            let response_len = response.len();

            let reader = thread::spawn(move || {
                thread::sleep(Duration::from_millis(20));
                let mut file = std::fs::File::from(reader_fd);
                let mut received = Vec::new();
                file.read_to_end(&mut received).unwrap();
                received
            });

            tokio::time::timeout(Duration::from_secs(2), writer.write_all(&response))
                .await
                .expect("large response must remain bounded under backpressure")
                .expect("large response must not surface EAGAIN");
            writer.flush().await.unwrap();
            drop(writer);

            let received = reader.join().unwrap();
            assert_eq!(received.len(), prefilled + response_len);
            assert!(received[prefilled..].iter().all(|byte| *byte == 0x41));
        }
    }
}

#[cfg(target_os = "linux")]
pub use linux::reliable_stdout;

#[cfg(not(target_os = "linux"))]
pub fn reliable_stdout() -> io::Result<tokio::io::Stdout> {
    Ok(tokio::io::stdout())
}
