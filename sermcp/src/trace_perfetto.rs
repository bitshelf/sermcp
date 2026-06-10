//! Native Perfetto protobuf export for `tracing` spans.
//!
//! Set `SERMCP_TRACE=<path>.pftrace` before starting either binary.
//! [`tracing_perfetto_sdk_layer::NativeLayer`] encodes trace packets directly
//! in Rust; the crate's default `sdk` feature is disabled in Cargo.toml, so no
//! Perfetto C++ SDK or C++ compiler is involved. The resulting file opens
//! directly in [ui.perfetto.dev](https://ui.perfetto.dev).
//!
//! `dutabo init` passes a derived sibling path to every MCP server it spawns,
//! yielding separate client and server timelines for one TEST run.

use std::fs::File;
use std::sync::Arc;
use tracing_perfetto_sdk_layer::{Flavor, NativeLayer};
use tracing_subscriber::layer::SubscriberExt;

type PerfettoLayer = NativeLayer<Arc<File>>;

/// The registry subscriber and this slot share clones of one layer. Taking the
/// global clone gives explicit exit paths a synchronization point before
/// `process::exit`; packet writes themselves are synchronous and unbuffered.
static GLOBAL_LAYER: std::sync::Mutex<Option<PerfettoLayer>> = std::sync::Mutex::new(None);

/// Keeps the process-global trace layer registered for the caller's lifetime.
pub struct PerfettoTraceGuard {
    _private: (),
}

impl Drop for PerfettoTraceGuard {
    fn drop(&mut self) {
        finalize();
    }
}

/// Stop and release the process-global layer. This is idempotent and safe on
/// explicit `process::exit` paths, which skip ordinary Rust destructors.
pub fn finalize() {
    let mut slot = GLOBAL_LAYER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(layer) = slot.take() {
        let _ = layer.stop();
        drop(layer);
    }
}

/// Finalize tracing, then terminate the process.
pub fn exit_with_trace(code: i32) -> ! {
    finalize();
    std::process::exit(code)
}

fn register_layer(layer: PerfettoLayer) {
    let mut slot = GLOBAL_LAYER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if slot.is_none() {
        *slot = Some(layer);
    }
}

/// Attach an env-gated native Perfetto layer to an existing subscriber.
pub fn attach_perfetto<S>(
    subscriber: S,
) -> (
    tracing_subscriber::layer::Layered<Option<PerfettoLayer>, S>,
    Option<PerfettoTraceGuard>,
)
where
    S: tracing::Subscriber
        + for<'span> tracing_subscriber::registry::LookupSpan<'span>
        + Send
        + Sync,
{
    let (layer, guard) = match std::env::var_os("SERMCP_TRACE").filter(|p| !p.is_empty()) {
        None => (None, None),
        Some(path) => {
            let file = match File::create(&path) {
                Ok(file) => file,
                Err(error) => {
                    eprintln!("[trace] cannot create {}: {error}", path.to_string_lossy());
                    return (subscriber.with(None), None);
                }
            };
            // Empty config bytes encode a valid default TraceConfig. With the
            // C++ `sdk` feature disabled, NativeLayer writes Rust tracing
            // packets directly and does not start a Perfetto tracing session.
            let layer = match NativeLayer::from_config_bytes(&[], Arc::new(file))
                .with_name("sermcp")
                .with_force_flavor(Some(Flavor::Async))
                .build()
            {
                Ok(layer) => layer,
                Err(error) => {
                    eprintln!(
                        "[trace] cannot initialize {}: {error}",
                        path.to_string_lossy()
                    );
                    return (subscriber.with(None), None);
                }
            };
            eprintln!(
                "[trace] native Perfetto trace enabled -> {}",
                path.to_string_lossy()
            );
            register_layer(layer.clone());
            (Some(layer), Some(PerfettoTraceGuard { _private: () }))
        }
    };
    (subscriber.with(layer), guard)
}

/// Compatibility name for callers compiled against the previous helper API.
pub use attach_perfetto as attach_chrome;

/// Derive a sibling path for an MCP child without overwriting the client trace.
pub fn child_trace_path(base: &std::ffi::OsStr) -> std::path::PathBuf {
    child_trace_path_tagged(base, "")
}

/// Tagged child path for concurrent MCP children.
pub fn child_trace_path_tagged(base: &std::ffi::OsStr, tag: &str) -> std::path::PathBuf {
    let mut path = std::path::PathBuf::from(base);
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => (stem, extension),
        _ => (name.as_str(), ""),
    };
    let marker = if tag.is_empty() {
        "server".to_string()
    } else {
        format!("server.{tag}")
    };
    let child = if extension.is_empty() {
        format!("{stem}.{marker}")
    } else {
        format!("{stem}.{marker}.{extension}")
    };
    path.set_file_name(child);
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message as _;

    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn native_trace_is_valid_perfetto_protobuf() {
        let _lock = ENV_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("probe.pftrace");
        let previous = std::env::var_os("SERMCP_TRACE");
        unsafe { std::env::set_var("SERMCP_TRACE", &path) };

        let (registry, guard) = attach_perfetto(tracing_subscriber::registry());
        assert!(guard.is_some());
        tracing::subscriber::with_default(registry, || {
            let _span = tracing::info_span!("measured", step = "cycle").entered();
            tracing::info!(result = "ok", "inner event");
        });
        drop(guard);

        let bytes = std::fs::read(&path).unwrap();
        let trace = tracing_perfetto_sdk_schema::Trace::decode(bytes.as_slice()).unwrap();
        assert!(!trace.packet.is_empty(), "trace contains packets");
        assert!(
            bytes.windows("measured".len()).any(|w| w == b"measured"),
            "span name is present in the protobuf"
        );
        restore_env(previous);
    }

    #[test]
    fn finalize_without_attach_is_a_noop() {
        let _lock = ENV_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = std::env::var_os("SERMCP_TRACE");
        unsafe { std::env::remove_var("SERMCP_TRACE") };
        let (_registry, guard) = attach_perfetto(tracing_subscriber::registry());
        assert!(guard.is_none());
        finalize();
        restore_env(previous);
    }

    #[test]
    fn child_trace_paths_do_not_collide() {
        assert_eq!(
            child_trace_path(std::ffi::OsStr::new("/tmp/trace.pftrace")),
            std::path::PathBuf::from("/tmp/trace.server.pftrace")
        );
        assert_eq!(
            child_trace_path_tagged(std::ffi::OsStr::new("trace.pftrace"), "3017"),
            std::path::PathBuf::from("trace.server.3017.pftrace")
        );
    }

    fn restore_env(previous: Option<std::ffi::OsString>) {
        match previous {
            Some(value) => unsafe { std::env::set_var("SERMCP_TRACE", value) },
            None => unsafe { std::env::remove_var("SERMCP_TRACE") },
        }
    }
}
