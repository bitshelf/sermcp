//! Host:port mutual-exclusion lock — kernel-held `flock`
//! (`File::try_lock`, stabilized in Rust 1.89).
//!
//! The lock is held by an open file descriptor registered process-wide:
//! a holder that crashes or is killed releases automatically (the kernel
//! closes the fd), so the O_EXCL zombie-cleanup and retry machinery is
//! gone. The lock FILE stays on disk carrying the holder's pid for
//! diagnostics (the interactive CLI takeover negotiation reads it); a
//! leftover file from a dead holder is an empty shell — the next
//! acquirer locks it and rewrites the content.

use std::collections::HashMap;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Open lock descriptors this process holds, keyed by lock-file path.
/// Dropping a descriptor releases the kernel lock; process exit closes
/// every fd (statics do not run Drop, the kernel does).
static HELD_LOCKS: OnceLock<Mutex<HashMap<PathBuf, std::fs::File>>> = OnceLock::new();

fn held_locks() -> &'static Mutex<HashMap<PathBuf, std::fs::File>> {
    HELD_LOCKS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Lifetime guard for the single MCP service allowed in one project path.
///
/// The kernel lock is held on the canonical project directory itself. The
/// PID file is diagnostic state only: deleting or corrupting it cannot create
/// a second owner while the first process still holds the directory lock.
pub struct ProjectLock {
    pid_path: std::path::PathBuf,
    _project_dir: std::fs::File,
}

impl Drop for ProjectLock {
    fn drop(&mut self) {
        if read_pid_file(&self.pid_path) == Some(std::process::id()) {
            let _ = std::fs::remove_file(&self.pid_path);
        }
    }
}

/// Whole-file PID recorded in a singleton PID file, if parseable.
/// (Project-level PID files hold a bare PID, unlike endpoint locks whose
/// first LINE is the PID.)
fn read_pid_file(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Atomically claim the project-level MCP singleton.
pub fn acquire_project_lock(project_dir: &Path) -> Result<ProjectLock, u32> {
    use std::os::unix::fs::OpenOptionsExt;

    let canonical = project_dir
        .canonicalize()
        .unwrap_or_else(|_| project_dir.to_path_buf());
    let dir = project_dir.join(".dut-serial");
    let path = dir.join("mcp.pid");
    if std::fs::create_dir_all(&dir).is_err() {
        return Err(std::process::id());
    }

    // Lock the project directory inode, not the PID file. PID files are
    // routinely repaired and cleaned by hooks; using one as the lock inode
    // would let an unlink create two independent flock owners.
    let Ok(project_file) = std::fs::File::open(&canonical) else {
        return Err(std::process::id());
    };
    if project_file.try_lock().is_err() {
        return Err(read_pid_file(&path)
            .filter(|pid| is_live_server_holder(*pid))
            .or_else(|| check_project_singleton(project_dir, ".dut-serial"))
            .or_else(|| find_project_process(project_dir))
            .unwrap_or_default());
    }

    // Compatibility with older binaries that did not flock the project
    // directory. They did write either the root or a per-DUT PID file, so a
    // current binary must honor those live holders before publishing itself.
    if let Some(owner) = find_project_pid_file_holder(project_dir) {
        return Err(owner);
    }

    let write_result = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .and_then(|mut file| write!(file, "{}", std::process::id()));
    if write_result.is_err() {
        return Err(std::process::id());
    }
    Ok(ProjectLock {
        pid_path: path,
        _project_dir: project_file,
    })
}

/// Backward-compatible entry point for callers that select one DUT. The lock
/// scope remains the whole project path; a DUT name never creates a second
/// MCP ownership domain.
pub fn acquire_dut_lock(project_dir: &Path, _name: &str) -> Result<ProjectLock, u32> {
    acquire_project_lock(project_dir)
}

/// Find a live older server named by the root or an immediate per-DUT PID
/// file. Current servers use the project-directory flock; these files are
/// checked only to avoid overlapping with a pre-flock binary during upgrade.
fn find_project_pid_file_holder(project_dir: &Path) -> Option<u32> {
    let dut_root = project_dir.join(".dut-serial");
    let mut candidates = vec![dut_root.join("mcp.pid")];
    if let Ok(entries) = std::fs::read_dir(&dut_root) {
        candidates.extend(
            entries
                .flatten()
                .map(|entry| entry.path().join("mcp.pid"))
                .filter(|path| path.is_file()),
        );
    }
    candidates.into_iter().find_map(|path| {
        let pid = read_pid_file(&path)?;
        (pid != std::process::id()
            && is_live_server_holder(pid)
            && process_belongs_to_project(pid, project_dir))
        .then_some(pid)
    })
}

/// Fallback when an older process did not write (or later lost) its PID file.
/// MCP children are launched with the project as cwd, so `/proc/<pid>/cwd`
/// gives us an authoritative project identity without parsing command lines.
pub fn find_project_process(project_dir: &Path) -> Option<u32> {
    let current = std::process::id();
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        if pid == current {
            continue;
        }
        let Ok(comm) = std::fs::read_to_string(entry.path().join("comm")) else {
            continue;
        };
        if !comm.contains("sermcp") {
            continue;
        }
        if !process_alive(pid) || !process_belongs_to_project(pid, project_dir) {
            continue;
        }
        return Some(pid);
    }
    None
}

fn process_belongs_to_project(pid: u32, project_dir: &Path) -> bool {
    let expected = project_dir
        .canonicalize()
        .unwrap_or_else(|_| project_dir.to_path_buf());
    if std::fs::read_link(format!("/proc/{pid}/cwd"))
        .ok()
        .and_then(|path| path.canonicalize().ok().or(Some(path)))
        .is_some_and(|cwd| cwd == expected)
    {
        return true;
    }
    let Ok(environ) = std::fs::read(format!("/proc/{pid}/environ")) else {
        return false;
    };
    environ.split(|byte| *byte == 0).any(|entry| {
        let Some(value) = entry.strip_prefix(b"TARGET_CONF=") else {
            return false;
        };
        let path = PathBuf::from(String::from_utf8_lossy(value).into_owned());
        let config_project = if path.is_dir() {
            path
        } else {
            path.parent().unwrap_or(Path::new("")).to_path_buf()
        };
        config_project.canonicalize().unwrap_or(config_project) == expected
    })
}

/// Is `pid` a live sermcp server process?
///
/// The process comm carries the server identity. A recycled pid belonging to
/// any other executable must be reaped rather than treated as a holder. Keep
/// this shared predicate as the single identity check for project locks and
/// CLI takeover logic.
pub fn is_live_server_holder(pid: u32) -> bool {
    process_alive(pid)
        && std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .map(|comm| comm.contains("sermcp"))
            .unwrap_or(false)
}

/// Check one project-level singleton PID file.
pub fn check_project_singleton(project_dir: &Path, dut_dir: &str) -> Option<u32> {
    let pid_file = project_dir.join(dut_dir).join("mcp.pid");
    let pid: u32 = read_pid_file(&pid_file)?;
    if is_live_server_holder(pid) && process_belongs_to_project(pid, project_dir) {
        return Some(pid);
    }
    // Stale PID file → clean up.
    let _ = std::fs::remove_file(&pid_file);
    None
}

/// Acquire the lock for `host:target` (`target` may be a TCP port or device path).
/// Returns `None` on success, `Some(pid)` if a conflict exists.
///
/// Uses a bounded retry loop (max 8 attempts) instead of unbounded recursion
/// to prevent stack overflow under persistent races.
pub fn acquire_lock(host: &str, target: &str, lock_dir: &str) -> Option<u32> {
    let lock_path = endpoint_lock_path(host, target, lock_dir);
    std::fs::create_dir_all(lock_dir).ok();
    let mut held = held_locks()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // Idempotent re-acquire by the same process.
    if held.contains_key(&lock_path) {
        return None;
    }
    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock_path)
    };
    #[cfg(not(unix))]
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path);
    let Ok(file) = file else {
        // Unreadable lock dir — report an opaque conflict rather than
        // silently proceeding unlocked.
        return Some(std::process::id());
    };
    match file.try_lock() {
        Ok(()) => {
            // We hold the kernel lock: publish pid diagnostics for the
            // interactive takeover negotiation.
            let mut file = file;
            let _ = file.set_len(0);
            let _ = file.seek(SeekFrom::Start(0));
            let _ = file.write_all(
                format!(
                    "{}\n{}:{}\n{}\n{}",
                    std::process::id(),
                    host,
                    target,
                    hostname(),
                    chrono::Local::now().to_rfc3339()
                )
                .as_bytes(),
            );
            held.insert(lock_path.clone(), file);
            None
        }
        Err(_) => {
            // A live holder keeps the flock (the kernel guarantees the
            // holder exists). Report its recorded pid; a pid-less window
            // (winner between lock and write) surfaces as an opaque
            // conflict with our own pid.
            drop(file);
            Some(
                read_lock_pid(&lock_path)
                    .filter(|pid| process_alive(*pid))
                    .unwrap_or_else(std::process::id),
            )
        }
    }
}

/// Path of the `host:target` endpoint lock file — exported for `dutabo
/// serial`'s direct takeover: the CLI reads the lock to find the live owner
/// process it must ask to yield (SIGUSR1) before connecting directly.
pub fn endpoint_lock_path(host: &str, target: &str, lock_dir: &str) -> std::path::PathBuf {
    let lock_key = format!("{:x}", fnv1a_hash(&format!("{host}:{target}")))[..8].to_string();
    Path::new(lock_dir).join(format!("{lock_key}.lock"))
}

/// First-line PID recorded in a lock file, if parseable (no liveness check).
fn read_lock_pid(lock_path: &Path) -> Option<u32> {
    std::fs::read_to_string(lock_path)
        .ok()?
        .lines()
        .next()?
        .trim()
        .parse()
        .ok()
}

/// Live owner PID recorded in an endpoint lock file, if any.
pub fn endpoint_lock_owner(lock_path: &std::path::Path) -> Option<u32> {
    read_lock_pid(lock_path).filter(|pid| process_alive(*pid))
}

/// Force-remove the endpoint lock, but only if it still names `expected_owner`.
///
/// Sole caller is `dutabo serial`'s escalation step: human preemption is
/// absolute, so a live MCP owner that never processed SIGUSR1 (engine mutex
/// held by a long command, or an older binary without the handler) must not
/// keep the human out. The lock file stays the authority — verify the pid
/// right before unlinking so an unrelated new owner is never evicted. The
/// verify-then-remove window is not atomic; the caller immediately re-runs
/// `acquire_lock`, so losing that race only means losing the takeover to a
/// legitimate new owner, never blind clobbering.
pub fn steal_lock(host: &str, target: &str, expected_owner: u32, lock_dir: &str) -> bool {
    let lock_path = endpoint_lock_path(host, target, lock_dir);
    if read_lock_pid(&lock_path) != Some(expected_owner) {
        return false;
    }
    // flock authority: a LIVE holder owns the kernel lock, and unlinking
    // its file would let a third process lock a fresh inode — two
    // holders at once. The file is only removable when the holder is
    // dead (its flock already released; this is residue cleanup).
    if process_alive(expected_owner) && expected_owner != std::process::id() {
        return false;
    }
    std::fs::remove_file(&lock_path).is_ok()
}

/// Default lock dir (mirrors `Config::lock_dir`'s built-in default).
pub fn default_lock_dir() -> String {
    "/tmp/sermcp/locks".to_string()
}

/// Release the lock for `host:target` if it belongs to this process.
///
/// A second stdio MCP may call its shutdown path after failing to acquire the
/// endpoint. It must not remove the first MCP's live lock. When the lock
/// directory becomes empty after the release, it is rmdir'd too (R13b/A5) so
/// shutdown leaves nothing behind; a non-empty dir (other endpoints) is kept.
pub fn release_lock(host: &str, target: &str, lock_dir: &str) {
    let lock_path = endpoint_lock_path(host, target, lock_dir);
    let mut held = held_locks()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if held.remove(&lock_path).is_some() {
        // Descriptor dropped here → kernel lock released.
        std::fs::remove_file(&lock_path).ok();
        // R13b: only removes when truly empty (remove_dir fails on a
        // non-empty dir), so other endpoints' locks are never at risk.
        let _ = std::fs::remove_dir(lock_path.parent().unwrap_or(Path::new(".")));
    }
}

/// Remove endpoint-lock residue belonging to one process after that process
/// has been reaped. Used by short-lived `dutabo init` MCP children: it scans
/// only the configured lock directory, removes only files whose recorded PID
/// exactly matches `pid`, and refuses while that PID is still alive.
pub fn cleanup_dead_process_locks(pid: u32, lock_dir: &str) {
    if pid == 0 || process_alive(pid) {
        return;
    }
    let dir = Path::new(lock_dir);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("lock")
            && read_lock_pid(&path) == Some(pid)
        {
            let _ = std::fs::remove_file(path);
        }
    }
    let _ = std::fs::remove_dir(dir);
}

// Minimal libc bindings — avoid a `nix`/`libc` dependency for two syscalls.
// `C-unwind` is used so an unwind across the FFI boundary is safe under
// Rust 2024 edition (per Cargo.toml).
unsafe extern "C-unwind" {
    fn kill(pid: i32, sig: i32) -> i32;
    fn __errno_location() -> *mut i32;
}

/// Check whether a process is alive.
///
/// `kill(pid, 0)` returns 0 if the process exists AND the caller has
/// permission to signal it. If the process exists but the caller lacks
/// permission, `errno` is set to `EPERM` (= 1 on Linux) and `kill` returns
/// -1 — the process is still alive. We treat EPERM as alive to avoid
/// deleting another user's live lock (which would defeat mutual exclusion).
///
/// A **zombie** (exited but not yet reaped) also answers `kill(pid, 0)` with
/// 0, yet it holds no resources and will never run again — a lock it recorded
/// is stale by definition. MCP clients that respawn servers without reaping
/// leave exactly such zombies behind, and treating them as alive wedged the
/// endpoint lock until the client process itself exited. So after the
/// existence check passes, `/proc/<pid>/stat` is consulted and state `Z`
/// counts as dead. Readers without a readable stat (foreign uid, non-Linux
/// procfs) keep the conservative alive verdict.
pub fn process_alive(pid: u32) -> bool {
    // SAFETY: kill(2) with sig=0 is a standard existence check on Unix.
    let ret = unsafe { kill(pid as i32, 0) };
    if ret != 0 {
        // Check errno for EPERM (process exists, permission denied).
        // SAFETY: __errno_location returns a pointer to a thread-local int;
        // reading it is safe in a single-threaded context here.
        let errno = unsafe { *__errno_location() };
        return errno == 1; // EPERM on Linux
    }
    !is_zombie(pid)
}

/// True when `/proc/<pid>/stat` reports the kernel task state `Z` (exited,
/// awaiting reaping). Unreadable stat → false (not provably a zombie).
fn is_zombie(pid: u32) -> bool {
    // The state char sits after the comm field, which is parenthesised and
    // may itself contain spaces or ')' — split from the last ')' instead of
    // whitespace-splitting.
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    process_state_from_stat(&stat).is_some_and(|state| state == 'Z')
}

fn process_state_from_stat(stat: &str) -> Option<char> {
    let (_, after_comm) = stat.rsplit_once(')')?;
    after_comm.trim_start().chars().next()
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// FNV-1a 64-bit hash — fast non-cryptographic hash for lock key uniqueness.
/// The `host:port` space is small (hundreds), so collision probability is
/// negligible.
fn fnv1a_hash(input: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in input.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Test-only: spawn a REAL foreign lock holder — this test binary
/// re-executed on `child_mode_park`, which acquires the endpoint lock
/// and parks until killed. Under flock, same-process re-acquire is
/// idempotent, so cross-process semantics need a real second process.
#[cfg(test)]
pub fn spawn_endpoint_holder(host: &str, target: &str, lock_dir: &str) -> std::process::Child {
    std::process::Command::new(std::env::current_exe().unwrap())
        .arg("lock_manager::tests::child_mode_park")
        .arg("--exact")
        .env("LOCK_CHILD_MODE", format!("{host}|{target}|{lock_dir}"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn child holder")
}

/// Test-only: block until the lock file names a live FOREIGN pid
/// (the spawned holder has taken the lock). Returns that pid.
#[cfg(test)]
pub fn wait_for_foreign_holder(lock_path: &Path) -> Option<u32> {
    for _ in 0..100 {
        if let Some(pid) = read_lock_pid(lock_path)
            && pid != std::process::id()
            && process_alive(pid)
        {
            return Some(pid);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_lock_dir() -> PathBuf {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("embedded-debug-test-{}-{}", std::process::id(), id));
        // Clean up any old directory.
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Generate an endpoint identity for lock-key tests without borrowing a
    /// lab IP or a configured ser2net port. `acquire_lock` never connects to
    /// it; the OS-assigned port simply guarantees environment independence.
    fn test_endpoint() -> (String, String) {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port().to_string();
        drop(listener);
        (format!("lock-{id}.invalid"), port)
    }

    #[test]
    fn acquire_and_release() {
        let dir = temp_lock_dir();
        let dir_str = dir.to_str().unwrap();
        let (host, port) = test_endpoint();
        assert!(acquire_lock(&host, &port, dir_str).is_none());
        release_lock(&host, &port, dir_str);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn steal_lock_only_displaces_the_recorded_owner() {
        let dir = temp_lock_dir();
        let dir_str = dir.to_str().unwrap();
        let (host, port) = test_endpoint();
        let lock_path = endpoint_lock_path(&host, &port, dir_str);
        std::fs::write(&lock_path, format!("4123\n{host}:{port}\nother-project\n")).unwrap();

        // A different recorded owner is never evicted.
        assert!(!steal_lock(&host, &port, 9999, dir_str));
        assert!(lock_path.exists());
        // The recorded owner is displaced and the endpoint becomes claimable.
        assert!(steal_lock(&host, &port, 4123, dir_str));
        assert!(!lock_path.exists());
        assert!(acquire_lock(&host, &port, dir_str).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn project_lock_is_singleton_and_owner_drop_releases_it() {
        let project = temp_lock_dir();
        let guard = acquire_project_lock(&project).unwrap();
        // Guarded pattern + no-Debug payload: assert_matches! does not
        // support this shape, keep the plain matches! form.
        assert!(matches!(
            acquire_project_lock(&project),
            Err(pid) if pid == std::process::id()
        ));
        assert_eq!(
            std::fs::read_to_string(project.join(".dut-serial/mcp.pid")).unwrap(),
            std::process::id().to_string()
        );

        drop(guard);
        assert!(!project.join(".dut-serial/mcp.pid").exists());
        assert!(acquire_project_lock(&project).is_ok());
        std::fs::remove_dir_all(project).ok();
    }

    #[test]
    fn stale_unanchored_per_dut_pid_does_not_block_project_lock() {
        let project = temp_lock_dir();
        let dut_dir = project.join(".dut-serial/rk3576");
        std::fs::create_dir_all(&dut_dir).unwrap();
        std::fs::write(dut_dir.join("mcp.pid"), std::process::id().to_string()).unwrap();

        // The test process is not anchored to this temporary project, so the
        // copied PID is stale metadata rather than a legacy owner.
        let guard = acquire_project_lock(&project).unwrap();
        assert!(project.join(".dut-serial/mcp.pid").exists());
        assert!(dut_dir.join("mcp.pid").exists());
        drop(guard);
        std::fs::remove_dir_all(project).ok();
    }

    #[test]
    fn dut_lock_uses_the_same_project_singleton() {
        let project = temp_lock_dir();
        // A DUT selection changes engine configuration only. It never creates
        // a second ownership domain inside the same project path.
        let guard = acquire_project_lock(&project).unwrap();
        assert!(acquire_dut_lock(&project, "rk3576-2").is_err());
        assert!(acquire_dut_lock(&project, "rk3576").is_err());
        drop(guard);
        let dut_guard = acquire_dut_lock(&project, "rk3576").unwrap();
        assert!(acquire_project_lock(&project).is_err());
        drop(dut_guard);
        std::fs::remove_dir_all(project).ok();
    }

    #[test]
    fn different_ports_no_conflict() {
        let dir = temp_lock_dir();
        let dir_str = dir.to_str().unwrap();
        let (host, first_port) = test_endpoint();
        let (_, mut second_port) = test_endpoint();
        while second_port == first_port {
            (_, second_port) = test_endpoint();
        }
        assert!(acquire_lock(&host, &first_port, dir_str).is_none());
        assert!(acquire_lock(&host, &second_port, dir_str).is_none());
        release_lock(&host, &first_port, dir_str);
        release_lock(&host, &second_port, dir_str);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_lock_file_content() {
        let dir = temp_lock_dir();
        let dir_str = dir.to_str().unwrap();
        let (host, port) = test_endpoint();
        assert!(acquire_lock(&host, &port, dir_str).is_none());

        let endpoint = format!("{host}:{port}");
        let lock_key = format!("{:x}", fnv1a_hash(&endpoint))[..8].to_string();
        let lock_path = dir.join(format!("{lock_key}.lock"));
        assert!(lock_path.exists());

        let content = std::fs::read_to_string(&lock_path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0], std::process::id().to_string()); // PID
        assert_eq!(lines[1], endpoint); // host:port
        // lines[2] is hostname, lines[3] is timestamp

        release_lock(&host, &port, dir_str);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_zombie_lock_cleanup() {
        let dir = temp_lock_dir();
        let dir_str = dir.to_str().unwrap();
        let (host, port) = test_endpoint();
        let endpoint = format!("{host}:{port}");
        let lock_key = format!("{:x}", fnv1a_hash(&endpoint))[..8].to_string();
        let lock_path = dir.join(format!("{lock_key}.lock"));

        // Create zombie lock with invalid PID
        std::fs::write(&lock_path, format!("999999\n{endpoint}\nhost\nnow")).unwrap();
        assert!(lock_path.exists());

        // acquire_lock should clean up zombie and succeed
        assert!(acquire_lock(&host, &port, dir_str).is_none());

        release_lock(&host, &port, dir_str);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cleanup_dead_process_locks_is_pid_scoped() {
        let dir = temp_lock_dir();
        let dead = dir.join("dead.lock");
        let live = dir.join("live.lock");
        let (dead_host, dead_port) = test_endpoint();
        let (live_host, live_port) = test_endpoint();
        std::fs::write(
            &dead,
            format!("99999999\n{dead_host}:{dead_port}\nhost\nnow"),
        )
        .unwrap();
        std::fs::write(
            &live,
            format!("{}\n{live_host}:{live_port}\nhost\nnow", std::process::id()),
        )
        .unwrap();

        cleanup_dead_process_locks(99999999, dir.to_str().unwrap());
        assert!(!dead.exists(), "matching dead PID residue is removed");
        assert!(live.exists(), "another process's lock is untouched");
        cleanup_dead_process_locks(std::process::id(), dir.to_str().unwrap());
        assert!(live.exists(), "a live PID is never cleaned");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn test_same_host_port_conflict() {
        let dir = temp_lock_dir();
        let dir_str = dir.to_str().unwrap();

        // Same-process re-acquire is idempotent under flock: one process,
        // one holder — no self-conflict.
        let (host, port) = test_endpoint();
        assert!(acquire_lock(&host, &port, dir_str).is_none());
        assert_eq!(acquire_lock(&host, &port, dir_str), None);

        release_lock(&host, &port, dir_str);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// flock's signature property, end to end with a REAL foreign
    /// process: a live child holder blocks the parent (conflict carries
    /// its pid); SIGKILLing the child releases the kernel lock instantly
    /// — no zombie cleanup, no pid liveness heuristics.
    #[test]
    fn killed_holder_releases_flock_and_lock_is_reacquirable() {
        let dir = temp_lock_dir();
        let dir_str = dir.to_str().unwrap();
        let (host, port) = test_endpoint();

        // Child: acquire, print nothing, then stand by until killed.
        // Re-runs THIS test binary in child mode via an env marker.
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("lock_manager::tests::child_mode_park")
            .arg("--exact")
            .env("LOCK_CHILD_MODE", format!("{host}|{port}|{dir_str}"))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn child holder");

        // Wait until the child actually holds the lock (pid file appears).
        let lock_path = endpoint_lock_path(&host, &port, dir_str);
        let mut held = false;
        for _ in 0..100 {
            if lock_path.exists()
                && read_lock_pid(&lock_path).is_some_and(|pid| pid != std::process::id())
            {
                held = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(held, "child never acquired the lock");

        // Live foreign holder → conflict reports the child's pid.
        let child_pid = read_lock_pid(&lock_path).unwrap();
        assert_eq!(acquire_lock(&host, &port, dir_str), Some(child_pid));

        // Kill the holder: the kernel closes its fd and the flock dies
        // with it — the next acquire must succeed immediately.
        child.kill().expect("kill child holder");
        let _ = child.wait();
        assert!(
            acquire_lock(&host, &port, dir_str).is_none(),
            "flock must release with the killed holder's fd"
        );

        release_lock(&host, &port, dir_str);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Child-holder mode: the killed-holder test re-executes this test
    /// binary with `--exact` on this test plus the LOCK_CHILD_MODE env;
    /// here we acquire the endpoint lock and park until the parent kills
    /// us — proving the kernel (not cleanup code) owns the release.
    #[test]
    fn child_mode_park() {
        let Some(marker) = std::env::var("LOCK_CHILD_MODE")
            .ok()
            .filter(|m| !m.is_empty())
        else {
            return; // normal test run: nothing to do
        };
        let mut parts = marker.splitn(3, '|');
        let (host, port, dir) = (
            parts.next().unwrap_or_default(),
            parts.next().unwrap_or_default(),
            parts.next().unwrap_or_default(),
        );
        if acquire_lock(host, port, dir).is_some() {
            std::process::exit(2);
        }
        loop {
            std::thread::sleep(std::time::Duration::from_secs(60));
        }
    }

    #[test]
    fn test_release_and_reacquire() {
        let dir = temp_lock_dir();
        let dir_str = dir.to_str().unwrap();

        // Acquire
        let (host, port) = test_endpoint();
        assert!(acquire_lock(&host, &port, dir_str).is_none());

        // Release
        release_lock(&host, &port, dir_str);

        // Re-acquire should succeed
        assert!(acquire_lock(&host, &port, dir_str).is_none());

        release_lock(&host, &port, dir_str);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn release_does_not_remove_foreign_lock() {
        let dir = temp_lock_dir();
        let dir_str = dir.to_str().unwrap();
        let (host, port) = test_endpoint();
        let endpoint = format!("{host}:{port}");
        let lock_key = format!("{:x}", fnv1a_hash(&endpoint))[..8].to_string();
        let lock_path = dir.join(format!("{lock_key}.lock"));
        std::fs::write(&lock_path, format!("999999\n{endpoint}\nforeign\nnow")).unwrap();

        release_lock(&host, &port, dir_str);

        assert!(lock_path.exists(), "a non-owner must not remove the lock");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_different_hosts_no_conflict() {
        let dir = temp_lock_dir();
        let dir_str = dir.to_str().unwrap();

        let (first_host, port) = test_endpoint();
        let (second_host, _) = test_endpoint();
        assert!(acquire_lock(&first_host, &port, dir_str).is_none());
        assert!(acquire_lock(&second_host, &port, dir_str).is_none());

        release_lock(&first_host, &port, dir_str);
        release_lock(&second_host, &port, dir_str);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_fnv1a_hash_deterministic() {
        let h1 = fnv1a_hash("test:1234");
        let h2 = fnv1a_hash("test:1234");
        assert_eq!(h1, h2);

        let h3 = fnv1a_hash("test:1235");
        assert_ne!(h1, h3);
    }

    #[test]
    fn test_process_alive() {
        // Current process should be alive
        assert!(process_alive(std::process::id()));

        // Very high PID should not be alive
        assert!(!process_alive(999999999));
    }

    #[test]
    fn process_state_parser_handles_parentheses_and_spaces_in_comm() {
        assert_eq!(
            process_state_from_stat("42 (worker pool) S 1 2 3"),
            Some('S')
        );
        assert_eq!(
            process_state_from_stat("42 (worker) name) Z 1 2 3"),
            Some('Z')
        );
        assert_eq!(process_state_from_stat("42 malformed"), None);
        assert_eq!(process_state_from_stat("42 (worker)"), None);
    }

    #[test]
    fn test_acquire_lock_respects_custom_dir() {
        // Regression: the old recursive code changed lock_dir on retry. Verify
        // the lock is created in the specified directory.
        let dir = temp_lock_dir();
        let dir_str = dir.to_str().unwrap();
        let (host, port) = test_endpoint();
        assert!(acquire_lock(&host, &port, dir_str).is_none());

        // The lock file must be in `dir`, not in /tmp/sermcp/locks.
        let lock_key = format!("{:x}", fnv1a_hash(&format!("{host}:{port}")))[..8].to_string();
        let lock_path = dir.join(format!("{lock_key}.lock"));
        assert!(
            lock_path.exists(),
            "lock should be in custom dir, not default"
        );

        release_lock(&host, &port, dir_str);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// R10 (regression pin): a LIVE pid whose comm is NOT a sermcp
    /// server (a recycled pid in the pid file) must never block project
    /// acquisition — the stale file is reaped and the claim proceeds.
    /// PID 1 (init/systemd) is the canonical always-alive non-server pid;
    /// `std::process::id()` would NOT work here — the test binary's own
    /// comm is `sermcp-*` and DOES match the holder-prefix check,
    /// so it would be mistaken for a live holder.
    #[test]
    fn recycled_pid_never_blocks_project_acquisition() {
        let project = temp_lock_dir();
        std::fs::create_dir_all(project.join(".dut-serial")).unwrap();
        std::fs::write(project.join(".dut-serial/mcp.pid"), "1").unwrap();

        let guard = acquire_project_lock(&project).unwrap();
        assert_eq!(
            std::fs::read_to_string(project.join(".dut-serial/mcp.pid")).unwrap(),
            std::process::id().to_string()
        );
        drop(guard);
        std::fs::remove_dir_all(project).ok();
    }

    /// R13b/A5: releasing the endpoint lock also rmdir's the lock directory
    /// when it becomes empty — shutdown must leave nothing behind (R3).
    #[test]
    fn release_lock_rmdirs_empty_lock_dir() {
        let base = temp_lock_dir();
        let dir = base.join("locks");
        let dir_str = dir.to_str().unwrap();
        let (host, port) = test_endpoint();
        assert!(acquire_lock(&host, &port, dir_str).is_none());
        release_lock(&host, &port, dir_str);
        assert!(
            !dir.exists(),
            "lock dir must be removed when empty after release"
        );
        std::fs::remove_dir_all(base).ok();
    }
}
