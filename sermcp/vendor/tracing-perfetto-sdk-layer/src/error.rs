//! Common error definitions

use std::io;

/// Generic error from the Perfetto SDK
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A generic C++ exception occurred in the optional SDK code.
    #[cfg(feature = "sdk")]
    #[error("C++ code threw an exception: {0}")]
    Cxx(#[from] cxx::Exception),
    /// A generic OS-level IO error occurred.
    #[error("IO error: {0}")]
    IO(#[from] io::Error),
    /// An internal `Mutex` is poisoned due to some other thread panicking while
    /// holding the mutex.
    #[error("Encountered a poisoned mutex")]
    PoisonedMutex,
    /// An operation can't complete because someone already called
    /// `layer.stop()` on the layer.
    #[error("The layer has been stopped")]
    LayerStopped,
    /// An operation timed out according to one of the timeouts set in the layer
    /// builder.
    #[error("Failed to complete operation before timeout was reached")]
    TimedOut,
    /// A flush failed due to some unknown reason.
    ///
    /// This is not necessarily separate from the `TimedOut` case; we simply
    /// don't know the reason why the flush failed.
    #[error("Failed to flush data due to some unknown reason (might also be due to timeout)")]
    FlushFailed,
}

/// Convenience alias for [`std::result::Result`] using our own `Error` type.
pub type Result<A> = std::result::Result<A, Error>;
