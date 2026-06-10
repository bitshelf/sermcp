//! Event-driven DUT state watcher.
//!
//! Uses the cross-platform [`notify`] crate (the inotify backend on Linux)
//! to watch only `<dut_dir>/target-state`. Configuration remains read-only
//! to agents and is validated during MCP startup; serial logs use their own
//! streaming paths and are deliberately not watched here.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use notify::{EventKind, RecursiveMode, Watcher};
use tokio::sync::mpsc;

/// Domain event emitted when the DUT state file changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchKind {
    StateChanged,
}

/// A filtered DUT state event.
#[derive(Debug)]
pub struct WatchEvent {
    pub kind: WatchKind,
    pub path: PathBuf,
}

/// Watches one DUT directory for `target-state` changes.
pub struct InotifyWatcher {
    /// The active watcher must stay alive for the receiver's lifetime.
    _watcher: notify::RecommendedWatcher,
    rx: mpsc::Receiver<WatchEvent>,
    /// Coalesce overflow instead of losing the latest state transition.
    overflow: Arc<Mutex<Option<WatchEvent>>>,
}

impl InotifyWatcher {
    /// Create a watcher for `<project_dir>/<dut_dir>/target-state`.
    ///
    /// The DUT directory is watched rather than the file itself so initial
    /// creation, deletion, and atomic replacement of `target-state` are all
    /// observable. No config or serial-log path is registered.
    pub fn new(project_dir: &Path, dut_dir: &str) -> Result<Self, String> {
        const EVENT_QUEUE_CAPACITY: usize = 16;

        if !project_dir.is_dir() {
            return Err(format!(
                "project directory does not exist: {}",
                project_dir.display()
            ));
        }

        let dut_path = project_dir.join(dut_dir);
        std::fs::create_dir_all(&dut_path)
            .map_err(|error| format!("create {}: {error}", dut_path.display()))?;

        let (tx, rx) = mpsc::channel::<WatchEvent>(EVENT_QUEUE_CAPACITY);
        let overflow = Arc::new(Mutex::new(None));
        let callback_overflow = Arc::clone(&overflow);

        let mut watcher = notify::recommended_watcher(
            move |result: notify::Result<notify::Event>| match result {
                Ok(event) => {
                    if let Some(event) = classify_event(&event)
                        && let Err(mpsc::error::TrySendError::Full(event)) = tx.try_send(event)
                    {
                        let mut pending = callback_overflow
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        *pending = Some(event);
                    }
                }
                Err(error) => tracing::warn!(%error, "DUT state watcher backend error"),
            },
        )
        .map_err(|error| format!("notify watcher init: {error}"))?;

        watcher
            .watch(&dut_path, RecursiveMode::NonRecursive)
            .map_err(|error| format!("watch DUT state directory: {error}"))?;

        Ok(Self {
            _watcher: watcher,
            rx,
            overflow,
        })
    }

    /// Await the next `target-state` change.
    pub async fn recv(&mut self) -> Option<WatchEvent> {
        match self.rx.try_recv() {
            Ok(event) => return Some(event),
            Err(mpsc::error::TryRecvError::Empty) => {}
            Err(mpsc::error::TryRecvError::Disconnected) => return self.take_overflow(),
        }

        if let Some(event) = self.take_overflow() {
            return Some(event);
        }

        self.rx.recv().await
    }

    /// Block for the next state change when called from a dedicated OS thread.
    pub fn wait(&mut self) -> Result<WatchEvent, String> {
        if tokio::runtime::Handle::try_current().is_ok() {
            return Err("wait() cannot block a Tokio runtime thread; use recv().await".to_string());
        }

        self.take_overflow()
            .or_else(|| self.rx.blocking_recv())
            .ok_or_else(|| "watcher closed".to_string())
    }

    fn take_overflow(&self) -> Option<WatchEvent> {
        self.overflow
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }
}

fn classify_event(event: &notify::Event) -> Option<WatchEvent> {
    let is_change = matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) | EventKind::Any
    );
    if !is_change {
        return None;
    }

    event.paths.iter().find_map(|path| {
        (path.file_name().and_then(|name| name.to_str()) == Some("target-state")).then(|| {
            WatchEvent {
                kind: WatchKind::StateChanged,
                path: path.clone(),
            }
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    async fn recv_state(watcher: &mut InotifyWatcher) -> WatchEvent {
        tokio::time::timeout(Duration::from_secs(2), watcher.recv())
            .await
            .expect("timed out waiting for target-state event")
            .expect("watcher closed unexpectedly")
    }

    #[test]
    fn test_watcher_creation_creates_dut_directory() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dut_dir = ".dut-serial/test-dut";

        let watcher = InotifyWatcher::new(tmp.path(), dut_dir);

        assert!(
            watcher.is_ok(),
            "watcher creation failed: {:?}",
            watcher.err()
        );
        assert!(tmp.path().join(dut_dir).is_dir());
    }

    #[tokio::test]
    async fn test_state_change_detection() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dut_dir = ".dut-serial/test-dut";
        let dut_path = tmp.path().join(dut_dir);
        let mut watcher = InotifyWatcher::new(tmp.path(), dut_dir).unwrap();
        let state_path = dut_path.join("target-state");

        std::fs::write(&state_path, "active").unwrap();

        let event = recv_state(&mut watcher).await;
        assert_eq!(event.kind, WatchKind::StateChanged);
        assert_eq!(event.path, state_path);
    }

    #[tokio::test]
    async fn test_config_and_log_changes_are_not_watched() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dut_dir = ".dut-serial/test-dut";
        let dut_path = tmp.path().join(dut_dir);
        let mut watcher = InotifyWatcher::new(tmp.path(), dut_dir).unwrap();

        std::fs::write(tmp.path().join(".target.jsonc"), "{}\n").unwrap();
        std::fs::create_dir_all(dut_path.join("logs")).unwrap();
        std::fs::write(dut_path.join("logs/current.serial.log"), "boot\n").unwrap();
        std::fs::write(dut_path.join("target-state"), "booting").unwrap();

        let event = recv_state(&mut watcher).await;
        assert_eq!(event.path, dut_path.join("target-state"));
    }

    #[test]
    fn test_classifies_target_state_in_any_event_path() {
        let event = notify::Event::new(EventKind::Any)
            .add_path(PathBuf::from("unrelated.tmp"))
            .add_path(PathBuf::from("target-state"));

        let event = classify_event(&event).expect("target-state should be classified");
        assert_eq!(event.kind, WatchKind::StateChanged);
        assert_eq!(event.path, PathBuf::from("target-state"));
    }

    #[test]
    fn test_ignores_config_and_log_paths() {
        for path in [".target.jsonc", "current.serial.log"] {
            let event = notify::Event::new(EventKind::Any).add_path(PathBuf::from(path));
            assert!(
                classify_event(&event).is_none(),
                "unexpected event for {path}"
            );
        }
    }

    #[tokio::test]
    async fn test_blocking_wait_rejects_runtime_thread() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut watcher = InotifyWatcher::new(tmp.path(), ".dut-serial/test-dut").unwrap();

        let error = watcher.wait().unwrap_err();
        assert!(error.contains("recv().await"));
    }
}
