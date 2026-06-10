use std::{sync, time};

use tracing_perfetto_sdk_sys as sys;
use tracing_perfetto_sdk_sys::ffi;

use crate::error;

pub fn with_session_lock<F, A>(
    ffi_session: &sync::Mutex<Option<cxx::UniquePtr<ffi::PerfettoTracingSession>>>,
    callback: F,
) -> error::Result<A>
where
    F: FnOnce(&mut cxx::UniquePtr<ffi::PerfettoTracingSession>) -> error::Result<A>,
{
    let mut mutex_guard = ffi_session
        .lock()
        .map_err(|_| error::Error::PoisonedMutex)?;

    if let Some(session) = mutex_guard.as_mut() {
        callback(session)
    } else {
        Err(error::Error::LayerStopped)
    }
}

pub fn do_poll_traces(
    session: &mut cxx::UniquePtr<ffi::PerfettoTracingSession>,
    timeout: time::Duration,
) -> error::Result<sys::PolledTraces> {
    let (ctx, rx) = sys::PollTracesCtx::new_sync();
    session
        .pin_mut()
        .poll_traces(Box::new(ctx), sys::PollTracesCtx::callback);
    rx.recv_timeout(timeout).map_err(|_| error::Error::TimedOut)
}

pub fn do_flush(
    session: &mut cxx::UniquePtr<ffi::PerfettoTracingSession>,
    timeout: time::Duration,
) -> error::Result<()> {
    let (ctx, rx) = sys::FlushCtx::new_sync();

    session.pin_mut().flush(
        timeout.as_millis() as u32,
        Box::new(ctx),
        sys::FlushCtx::callback,
    );

    let success = rx
        .recv_timeout(timeout)
        .map_err(|_| error::Error::TimedOut)?;
    if success {
        Ok(())
    } else {
        Err(error::Error::FlushFailed)
    }
}
