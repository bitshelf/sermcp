//! Bind a server to a session process, checking its start time against PID reuse.

use std::io;

#[derive(Clone, Debug)]
pub struct SessionOwner {
    pid: u32,
    start_ticks: u64,
    project: Option<std::path::PathBuf>,
}

fn process_start_ticks(pid: u32) -> io::Result<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let fields: Vec<_> = stat
        .rsplit_once(") ")
        .ok_or_else(|| io::Error::other("invalid process stat"))?
        .1
        .split_whitespace()
        .collect();
    if fields.first() == Some(&"Z") || fields.first() == Some(&"X") {
        return Err(io::Error::other("session process exited"));
    }
    fields
        .get(19)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| io::Error::other("missing process start time"))
}

impl SessionOwner {
    pub fn capture(pid: u32, expected_start_ticks: Option<u64>) -> io::Result<Self> {
        if pid <= 1 || pid == std::process::id() {
            return Err(io::Error::other(
                "session owner must be a live external process",
            ));
        }
        let start_ticks = process_start_ticks(pid)?;
        if expected_start_ticks.is_some_and(|expected| expected != start_ticks) {
            return Err(io::Error::other("session owner PID was reused"));
        }
        Ok(Self {
            pid,
            start_ticks,
            project: None,
        })
    }

    pub fn from_environment() -> io::Result<Option<Self>> {
        let Some(pid) = std::env::var_os("SERMCP_SESSION_PID") else {
            return Ok(None);
        };
        let pid = pid
            .to_str()
            .and_then(|p| p.parse().ok())
            .ok_or_else(|| io::Error::other("invalid SERMCP_SESSION_PID"))?;
        let ticks = std::env::var("SERMCP_SESSION_START_TICKS")
            .ok()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| {
                io::Error::other("SERMCP_SESSION_START_TICKS is required with SERMCP_SESSION_PID")
            })?;
        Self::capture(pid, Some(ticks)).map(Some)
    }

    pub fn with_project(mut self, project: &std::path::Path) -> Self {
        self.project = Some(project.to_path_buf());
        self
    }

    pub async fn exited(&self) {
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
        let mut empty_since = None;
        loop {
            tick.tick().await;
            if process_start_ticks(self.pid).ok() != Some(self.start_ticks)
                && !self
                    .project
                    .as_deref()
                    .is_some_and(project_has_live_sessions)
            {
                let since = empty_since.get_or_insert_with(tokio::time::Instant::now);
                if self.project.is_none() || since.elapsed() >= std::time::Duration::from_secs(3) {
                    return;
                }
            } else {
                empty_since = None;
            }
        }
    }
}

/// Only process-verified markers keep a project alive; legacy markers are not leases.
pub fn project_has_live_sessions(project: &std::path::Path) -> bool {
    let Ok(entries) = std::fs::read_dir(project.join(".dut-serial/sessions")) else {
        return false;
    };
    entries.flatten().any(|entry| {
        if entry.file_name().to_string_lossy().starts_with('.') {
            return false;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            return false;
        };
        let Some(line) = text.lines().nth(2) else {
            return false;
        };
        let mut fields = line.split_whitespace();
        let (Some(pid), Some(ticks)) = (fields.next(), fields.next()) else {
            return false;
        };
        let (Ok(pid), Ok(ticks)) = (pid.parse::<u32>(), ticks.parse::<u64>()) else {
            return false;
        };
        fields.next().is_none() && SessionOwner::capture(pid, Some(ticks)).is_ok()
    })
}

pub async fn shutdown_requested(owner: Option<SessionOwner>) -> &'static str {
    let session_exit = async {
        if let Some(owner) = owner {
            owner.exited().await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        tokio::select! {
            _ = session_exit => "session process exited",
            _ = term.recv() => "SIGTERM",
            _ = tokio::signal::ctrl_c() => "SIGINT",
        }
    }
    #[cfg(not(unix))]
    tokio::select! {
        _ = session_exit => "session process exited",
        _ = tokio::signal::ctrl_c() => "interrupt",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn another_verified_session_survives_original_owner_exit() {
        let project = tempfile::tempdir().unwrap();
        let sessions = project.path().join(".dut-serial/sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let mut first = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let mut second = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let owner = SessionOwner::capture(first.id(), None)
            .unwrap()
            .with_project(project.path());
        let ticks = process_start_ticks(second.id()).unwrap();
        let marker = sessions.join("second");
        std::fs::write(
            &marker,
            format!("timestamp\nguest\n{} {}\n", second.id(), ticks),
        )
        .unwrap();
        first.kill().unwrap();
        first.wait().unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(300), owner.exited())
                .await
                .is_err()
        );
        std::fs::write(
            &marker,
            format!("timestamp\nguest\n{} {}\n", second.id(), ticks + 1),
        )
        .unwrap();
        assert!(!project_has_live_sessions(project.path()));
        std::fs::write(&marker, "timestamp\nguest\n").unwrap();
        assert!(!project_has_live_sessions(project.path()));
        std::fs::write(
            &marker,
            format!("timestamp\nguest\n{} {}\n", second.id(), ticks),
        )
        .unwrap();
        second.kill().unwrap();
        second.wait().unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), owner.exited())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn session_exit_is_detected_even_before_child_is_reaped() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let owner = SessionOwner::capture(child.id(), None).unwrap();
        assert!(SessionOwner::capture(child.id(), Some(owner.start_ticks + 1)).is_err());
        child.kill().unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), owner.exited())
            .await
            .unwrap();
        child.wait().unwrap();
    }
}
