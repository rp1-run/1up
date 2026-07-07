use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use tracing::{debug, info, warn};

use crate::shared::config;
use crate::shared::constants::{
    DAEMON_DRAIN_POLL_INTERVAL_MS, DAEMON_DRAIN_TIMEOUT_MS, REBUILD_LOCK_CONTENTION_TIMEOUT_MS,
    REBUILD_LOCK_RETRY_INTERVAL_MS, SECURE_STATE_FILE_MODE, XDG_STATE_DIR_MODE,
};
use crate::shared::errors::{DaemonError, OneupError};
use crate::shared::fs::{
    atomic_replace, ensure_secure_project_root, ensure_secure_xdg_root, remove_regular_file,
    validate_regular_file_path,
};

const CONTENTION_RETRY_INTERVAL_MS: u64 = 200;
const CONTENTION_TIMEOUT_MS: u64 = 5000;

pub const fn supports_daemon() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonProbeState {
    NotRunning,
    Running(u32),
    Starting,
}

pub struct DaemonLock {
    _lock: Flock<File>,
    pid_path: PathBuf,
}

impl Drop for DaemonLock {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.pid_path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                warn!("failed to remove pid file on drop: {e}");
            }
        }
        debug!("daemon lock released: {}", self.pid_path.display());
    }
}

pub fn acquire_daemon_lock() -> Result<DaemonLock, OneupError> {
    let xdg_root = ensure_secure_xdg_root()
        .map_err(|err| DaemonError::PidFileError(format!("failed to prepare pid root: {err}")))?;
    let pid_path = config::pid_file_path()?;
    let validated_path = validate_regular_file_path(&pid_path, &xdg_root)
        .map_err(|err| DaemonError::PidFileError(format!("failed to validate pid file: {err}")))?;

    let file = open_pid_file(&validated_path)?;
    match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        Ok(lock) => write_pid_and_wrap(lock, validated_path),
        Err((_, Errno::EWOULDBLOCK)) => handle_lock_contention(&validated_path, &xdg_root),
        Err((_, errno)) => {
            Err(DaemonError::PidFileError(format!("failed to lock pid file: {errno}")).into())
        }
    }
}

fn open_pid_file(path: &Path) -> Result<File, OneupError> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(SECURE_STATE_FILE_MODE)
        .open(path)
        .map_err(|e| DaemonError::PidFileError(format!("failed to open pid file: {e}")).into())
}

fn write_pid_and_wrap(mut lock: Flock<File>, pid_path: PathBuf) -> Result<DaemonLock, OneupError> {
    let pid = std::process::id();
    lock.set_len(0)
        .map_err(|e| DaemonError::PidFileError(format!("failed to truncate pid file: {e}")))?;
    lock.seek(SeekFrom::Start(0))
        .map_err(|e| DaemonError::PidFileError(format!("failed to seek pid file: {e}")))?;
    write!(lock, "{pid}")
        .map_err(|e| DaemonError::PidFileError(format!("failed to write pid: {e}")))?;
    lock.sync_data()
        .map_err(|e| DaemonError::PidFileError(format!("failed to sync pid file: {e}")))?;
    debug!("acquired daemon lock: {} (pid={pid})", pid_path.display());
    Ok(DaemonLock {
        _lock: lock,
        pid_path,
    })
}

fn handle_lock_contention(pid_path: &Path, _xdg_root: &Path) -> Result<DaemonLock, OneupError> {
    observe_lock_contention(
        pid_path,
        Duration::from_millis(CONTENTION_TIMEOUT_MS),
        Duration::from_millis(CONTENTION_RETRY_INTERVAL_MS),
    )
}

fn observe_lock_contention(
    pid_path: &Path,
    timeout: Duration,
    retry_interval: Duration,
) -> Result<DaemonLock, OneupError> {
    let deadline = Instant::now() + timeout;

    loop {
        if let Some(pid) = read_pid_from_path(pid_path) {
            if is_process_alive(pid) {
                info!("daemon lock contention: pid={pid} already holds the daemon lock");
                return Err(DaemonError::AlreadyRunning(pid).into());
            }
            debug!(
                "daemon lock contention: pid file contains inactive pid={pid}; observing holder"
            );
        } else {
            debug!("daemon lock contention: pid file is not ready; observing holder");
        }

        if let Some(lock) = try_acquire_pid_lock(pid_path)? {
            return write_pid_and_wrap(lock, pid_path.to_path_buf());
        }

        let now = Instant::now();
        if now >= deadline {
            break;
        }
        std::thread::sleep(retry_interval.min(deadline.saturating_duration_since(now)));
    }

    if let Some(pid) = read_pid_from_path(pid_path) {
        if is_process_alive(pid) {
            info!("daemon lock contention resolved to running pid={pid}");
            return Err(DaemonError::AlreadyRunning(pid).into());
        }
    }

    warn!("daemon lock contention: another startup still holds the daemon lock");
    Err(DaemonError::StartupInProgress.into())
}

fn try_acquire_pid_lock(pid_path: &Path) -> Result<Option<Flock<File>>, OneupError> {
    let file = open_pid_file(pid_path)?;
    match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        Ok(lock) => Ok(Some(lock)),
        Err((_, Errno::EWOULDBLOCK)) => Ok(None),
        Err((_, errno)) => Err(DaemonError::PidFileError(format!(
            "failed to probe daemon lock during contention: {errno}"
        ))
        .into()),
    }
}

fn read_pid_from_path(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

#[allow(dead_code)]
pub fn write_pid_file() -> Result<(), OneupError> {
    let xdg_root = ensure_secure_xdg_root()
        .map_err(|err| DaemonError::PidFileError(format!("failed to prepare pid root: {err}")))?;
    let pid = std::process::id();
    write_pid_file_at(&config::pid_file_path()?, &xdg_root, pid)
}

#[allow(dead_code)]
fn write_pid_file_at(path: &Path, approved_root: &Path, pid: u32) -> Result<(), OneupError> {
    let pid_text = pid.to_string();
    atomic_replace(
        path,
        pid_text.as_bytes(),
        approved_root,
        XDG_STATE_DIR_MODE,
        SECURE_STATE_FILE_MODE,
    )
    .map_err(|err| DaemonError::PidFileError(format!("failed to write pid file: {err}")))?;

    debug!("wrote pid file: {} (pid={})", path.display(), pid);
    Ok(())
}

#[allow(dead_code)]
pub fn read_pid_file() -> Result<Option<u32>, OneupError> {
    let xdg_root = ensure_secure_xdg_root()
        .map_err(|err| DaemonError::PidFileError(format!("failed to prepare pid root: {err}")))?;
    read_pid_file_at(&config::pid_file_path()?, &xdg_root)
}

#[allow(dead_code)]
fn read_pid_file_at(path: &Path, approved_root: &Path) -> Result<Option<u32>, OneupError> {
    let path = validate_regular_file_path(path, approved_root)
        .map_err(|err| DaemonError::PidFileError(format!("failed to validate pid file: {err}")))?;
    if !path.exists() {
        return Ok(None);
    }

    let content = std::fs::read_to_string(&path)
        .map_err(|e| DaemonError::PidFileError(format!("failed to read pid file: {e}")))?;

    let pid: u32 = content
        .trim()
        .parse()
        .map_err(|e| DaemonError::PidFileError(format!("invalid pid in file: {e}")))?;

    Ok(Some(pid))
}

#[allow(dead_code)]
pub fn remove_pid_file() -> Result<(), OneupError> {
    let xdg_root = ensure_secure_xdg_root()
        .map_err(|err| DaemonError::PidFileError(format!("failed to prepare pid root: {err}")))?;
    remove_pid_file_at(&config::pid_file_path()?, &xdg_root)
}

fn remove_pid_file_at(path: &Path, approved_root: &Path) -> Result<(), OneupError> {
    let removed = remove_regular_file(path, approved_root)
        .map_err(|err| DaemonError::PidFileError(format!("failed to remove pid file: {err}")))?;
    if removed {
        debug!("removed pid file: {}", path.display());
    }
    Ok(())
}

pub fn is_process_alive(pid: u32) -> bool {
    match signal::kill(Pid::from_raw(pid as i32), None) {
        Ok(_) => true,
        Err(nix::errno::Errno::ESRCH) => false,
        Err(nix::errno::Errno::EPERM) => true,
        Err(_) => false,
    }
}

pub fn probe_daemon() -> Result<DaemonProbeState, OneupError> {
    let xdg_root = ensure_secure_xdg_root()
        .map_err(|err| DaemonError::PidFileError(format!("failed to prepare pid root: {err}")))?;
    let pid_path = config::pid_file_path()?;
    probe_daemon_at(&pid_path, &xdg_root)
}

fn probe_daemon_at(pid_path: &Path, xdg_root: &Path) -> Result<DaemonProbeState, OneupError> {
    let file = match File::open(pid_path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(DaemonProbeState::NotRunning);
        }
        Err(e) => {
            return Err(DaemonError::PidFileError(format!("failed to open pid file: {e}")).into())
        }
    };

    match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        Ok(lock) => {
            drop(lock);
            warn!("stale pid file detected, cleaning up");
            let _ = remove_pid_file_at(pid_path, xdg_root);
            Ok(DaemonProbeState::NotRunning)
        }
        Err((mut file, Errno::EWOULDBLOCK)) => {
            let mut content = String::new();
            if file.read_to_string(&mut content).is_err() {
                return Ok(DaemonProbeState::Starting);
            }
            let Ok(pid) = content.trim().parse::<u32>() else {
                return Ok(DaemonProbeState::Starting);
            };
            debug!(
                "flock held by pid={pid}, is_process_alive={}",
                is_process_alive(pid)
            );
            if is_process_alive(pid) {
                Ok(DaemonProbeState::Running(pid))
            } else {
                Ok(DaemonProbeState::Starting)
            }
        }
        Err((_, errno)) => {
            Err(DaemonError::PidFileError(format!("failed to probe pid file lock: {errno}")).into())
        }
    }
}

pub fn is_daemon_running() -> Result<Option<u32>, OneupError> {
    match probe_daemon()? {
        DaemonProbeState::Running(pid) => Ok(Some(pid)),
        DaemonProbeState::NotRunning | DaemonProbeState::Starting => Ok(None),
    }
}

pub fn send_sighup(pid: u32) -> Result<(), OneupError> {
    signal::kill(Pid::from_raw(pid as i32), Signal::SIGHUP)
        .map_err(|e| DaemonError::SignalError(format!("failed to send SIGHUP to {pid}: {e}")))?;
    debug!("sent SIGHUP to pid={pid}");
    Ok(())
}

pub fn send_sigterm(pid: u32) -> Result<(), OneupError> {
    signal::kill(Pid::from_raw(pid as i32), Signal::SIGTERM)
        .map_err(|e| DaemonError::SignalError(format!("failed to send SIGTERM to {pid}: {e}")))?;
    debug!("sent SIGTERM to pid={pid}");
    Ok(())
}

pub fn spawn_daemon(binary_path: &Path) -> Result<u32, OneupError> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let child = unsafe {
        Command::new(binary_path)
            .arg("__worker")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .pre_exec(|| {
                nix::unistd::setsid().map_err(|e| std::io::Error::other(format!("setsid: {e}")))?;
                Ok(())
            })
            .spawn()
            .map_err(|e| DaemonError::PidFileError(format!("failed to spawn daemon: {e}")))?
    };

    let pid = child.id();
    debug!("spawned daemon worker (pid={pid})");
    Ok(pid)
}

pub fn current_binary_path() -> Result<std::path::PathBuf, OneupError> {
    Ok(std::env::current_exe()
        .map_err(|e| DaemonError::PidFileError(format!("failed to determine binary path: {e}")))?)
}

/// Ensures the daemon is running for a given project. If no daemon is running,
/// registers the project and spawns a new daemon. If a daemon is already running
/// but the project is not registered, registers it and sends SIGHUP to reload.
/// Returns the daemon PID.
pub fn ensure_daemon(
    project_id: &str,
    project_root: &Path,
    source_root: &Path,
) -> Result<u32, OneupError> {
    use crate::daemon::registry::{registration_context, Registry};

    if let Some(pid) = is_daemon_running()? {
        let mut registry = Registry::load()?;
        let context = registration_context(project_root, source_root);
        let already_registered = registry.contains_context(&context);

        if !already_registered {
            registry.register_with_context(project_id, &context, None)?;
            send_sighup(pid)?;
            debug!("auto-registered project and sent SIGHUP to daemon (pid={pid})");
        }

        return Ok(pid);
    }

    let mut registry = Registry::load()?;
    let context = registration_context(project_root, source_root);
    registry.register_with_context(project_id, &context, None)?;

    let binary = current_binary_path()?;
    let pid = spawn_daemon(&binary)?;
    debug!(
        "auto-started daemon (pid={pid}) for project at {}",
        project_root.display()
    );
    Ok(pid)
}

/// Gracefully drains a running daemon: sends SIGTERM, then polls
/// [`is_process_alive`] at [`DAEMON_DRAIN_POLL_INTERVAL_MS`] until the process
/// exits or `timeout` elapses.
///
/// Returns `Ok(())` once the daemon has exited (releasing its DB write lock and
/// any held rebuild lock as their guards drop). On timeout it returns an
/// actionable error instructing the user to run `1up stop` then retry, rather
/// than forcing a kill or proceeding against a still-live daemon.
///
/// This is the shared SIGTERM+poll primitive reused by `1up update`'s
/// pre-update stop and by the post-upgrade version-handshake drain/restart.
pub fn drain_daemon(pid: u32, timeout: Duration) -> Result<(), OneupError> {
    debug!("draining daemon (pid={pid}) with SIGTERM; bound={timeout:?}");
    send_sigterm(pid)?;

    let poll_interval = Duration::from_millis(DAEMON_DRAIN_POLL_INTERVAL_MS);
    let deadline = Instant::now() + timeout;

    loop {
        if !is_process_alive(pid) {
            debug!("daemon (pid={pid}) exited within drain bound");
            return Ok(());
        }

        let now = Instant::now();
        if now >= deadline {
            break;
        }
        std::thread::sleep(poll_interval.min(deadline.saturating_duration_since(now)));
    }

    warn!("daemon (pid={pid}) did not exit within {timeout:?} drain bound");
    Err(DaemonError::DrainTimeout {
        pid,
        timeout_ms: timeout.as_millis(),
    }
    .into())
}

/// Drains a stale daemon and restarts a fresh one under the current binary.
///
/// Drains via [`drain_daemon`] using the standard [`DAEMON_DRAIN_TIMEOUT_MS`]
/// bound; if the drain exceeds the bound the actionable error is returned and
/// no restart is attempted (the caller falls back rather than proceeding). On a
/// clean drain the stale daemon has released its locks, so [`ensure_daemon`]
/// spawns a fresh daemon under the current executable and returns its pid.
pub fn drain_and_restart_daemon(
    pid: u32,
    project_id: &str,
    project_root: &Path,
    source_root: &Path,
) -> Result<u32, OneupError> {
    drain_daemon(pid, Duration::from_millis(DAEMON_DRAIN_TIMEOUT_MS))?;
    info!("stale daemon (pid={pid}) drained; restarting under current binary");
    ensure_daemon(project_id, project_root, source_root)
}

/// RAII guard for the single-writer rebuild lock.
///
/// Holds an exclusive `flock` on `<state_root>/.1up/rebuild.lock` for as long as
/// the guard is alive, so exactly one process owns a destructive rebuild /
/// format change of the shared index. The lock auto-releases when the guard
/// drops — on normal scope exit, on a `?` early return, or when an in-flight
/// indexing pass is cancelled and the holding frame unwinds — so a daemon
/// drained mid-rebuild frees the lock for the restarted binary with no
/// stale-lock reconciliation (HYP-002).
///
/// Unlike [`DaemonLock`], the lockfile is intentionally NOT removed on drop:
/// unlinking a held `flock` target races a concurrent waiter onto a different
/// inode, so the file is left in place and only the advisory lock is released.
#[must_use = "the rebuild lock releases as soon as the guard is dropped"]
pub struct RebuildLock {
    _lock: Flock<File>,
}

impl std::fmt::Debug for RebuildLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RebuildLock").finish_non_exhaustive()
    }
}

/// Opens (creating if absent) the `state_root`-keyed rebuild lockfile under
/// `.1up/` with owner-only permissions, rejecting symlinked components.
fn open_rebuild_lock_file(state_root: &Path) -> Result<(File, PathBuf), OneupError> {
    let dot_dir = ensure_secure_project_root(state_root).map_err(|err| {
        DaemonError::RebuildLockError(format!("failed to prepare rebuild lock root: {err}"))
    })?;
    let lock_path = config::project_rebuild_lock_path(state_root);
    let validated_path = validate_regular_file_path(&lock_path, &dot_dir).map_err(|err| {
        DaemonError::RebuildLockError(format!("failed to validate rebuild lock file: {err}"))
    })?;

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(SECURE_STATE_FILE_MODE)
        .open(&validated_path)
        .map_err(|e| {
            DaemonError::RebuildLockError(format!("failed to open rebuild lock file: {e}"))
        })?;
    Ok((file, validated_path))
}

/// Attempts to acquire the rebuild lock without blocking.
///
/// Returns `Ok(Some(guard))` when the lock is acquired, `Ok(None)` when another
/// process currently holds it, or an error if the lockfile cannot be opened.
/// The daemon uses this to defer an indexing pass (leaving the project dirty for
/// a later retry) instead of blocking its event loop while a competing one-shot
/// rebuild runs.
pub fn try_acquire_rebuild_lock(state_root: &Path) -> Result<Option<RebuildLock>, OneupError> {
    let (file, lock_path) = open_rebuild_lock_file(state_root)?;
    match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        Ok(lock) => {
            debug!("acquired rebuild lock: {}", lock_path.display());
            Ok(Some(RebuildLock { _lock: lock }))
        }
        Err((_, Errno::EWOULDBLOCK)) => Ok(None),
        Err((_, errno)) => Err(DaemonError::RebuildLockError(format!(
            "failed to lock rebuild file: {errno}"
        ))
        .into()),
    }
}

/// Acquires the single-writer rebuild lock, waiting up to
/// [`REBUILD_LOCK_CONTENTION_TIMEOUT_MS`] for a competing holder to release it
/// before failing closed with an actionable, named reason.
///
/// Used by the synchronous one-shot rebuild paths (CLI `index`/`reindex`, MCP)
/// so two processes never race a destructive rebuild of the shared
/// `.1up/index.db`. The returned guard releases on drop.
pub fn acquire_rebuild_lock(state_root: &Path) -> Result<RebuildLock, OneupError> {
    acquire_rebuild_lock_with_bound(
        state_root,
        Duration::from_millis(REBUILD_LOCK_CONTENTION_TIMEOUT_MS),
        Duration::from_millis(REBUILD_LOCK_RETRY_INTERVAL_MS),
    )
}

fn acquire_rebuild_lock_with_bound(
    state_root: &Path,
    timeout: Duration,
    retry_interval: Duration,
) -> Result<RebuildLock, OneupError> {
    let deadline = Instant::now() + timeout;

    loop {
        if let Some(lock) = try_acquire_rebuild_lock(state_root)? {
            return Ok(lock);
        }

        let now = Instant::now();
        if now >= deadline {
            break;
        }
        std::thread::sleep(retry_interval.min(deadline.saturating_duration_since(now)));
    }

    warn!(
        "rebuild lock for {} held by another process within {timeout:?}; failing closed",
        state_root.display()
    );
    Err(DaemonError::RebuildLockContended {
        state_root: state_root.display().to_string(),
    }
    .into())
}

/// REQ-010: Check if a rebuild lock file is stale.
/// A lock is stale if the file age exceeds STALENESS_THRESHOLD_SECS (5 minutes) AND
/// no process currently holds the lock (non-blocking lock succeeds).
/// This allows auto-clearing of locks from dead processes so they don't block forever.
pub fn is_rebuild_lock_stale(state_root: &Path) -> Result<bool, OneupError> {
    use crate::shared::constants::STALENESS_THRESHOLD_SECS;
    use std::fs;

    let lock_path = config::project_rebuild_lock_path(state_root);

    // If the lock file doesn't exist, it's not stale (non-existent is fine)
    let metadata = match fs::metadata(&lock_path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => {
            return Err(
                DaemonError::RebuildLockError(format!("failed to stat lock file: {e}")).into(),
            )
        }
    };

    // Check file age
    let Ok(modified) = metadata.modified() else {
        return Ok(false);
    };

    let Ok(elapsed) = SystemTime::now().duration_since(modified) else {
        return Ok(false);
    };

    // If file is newer than threshold, it's not stale
    if elapsed.as_secs() <= STALENESS_THRESHOLD_SECS {
        return Ok(false);
    }

    // File is old. Try to acquire lock without blocking. If we can acquire it,
    // that means no one else holds it, so it's stale.
    match try_acquire_rebuild_lock(state_root) {
        Ok(Some(_lock)) => {
            // We got the lock, which means it was free. The old file is stale.
            // The guard will release on drop.
            Ok(true)
        }
        Ok(None) => {
            // Someone still holds it, so it's not stale
            Ok(false)
        }
        Err(_) => {
            // Error checking; assume not stale to be conservative
            Ok(false)
        }
    }
}

/// REQ-010: Clear a stale rebuild lock file.
/// Only clears if the lock is confirmed stale (old file, no holder).
pub fn clear_stale_rebuild_lock(state_root: &Path) -> Result<(), OneupError> {
    if is_rebuild_lock_stale(state_root)? {
        let lock_path = config::project_rebuild_lock_path(state_root);
        if let Err(e) = std::fs::remove_file(&lock_path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                warn!(
                    "failed to remove stale rebuild lock {}: {}",
                    lock_path.display(),
                    e
                );
            }
        } else {
            info!("cleared stale rebuild lock: {}", lock_path.display());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use nix::errno::Errno;
    use nix::fcntl::{Flock, FlockArg};

    #[test]
    fn current_process_is_alive() {
        let pid = std::process::id();
        assert!(is_process_alive(pid));
    }

    #[test]
    fn nonexistent_process_is_not_alive() {
        assert!(!is_process_alive(99999));
    }

    #[test]
    fn pid_file_roundtrip_uses_secure_state_files() {
        let tmp = tempfile::tempdir().unwrap();
        let xdg_root = tmp.path().canonicalize().unwrap().join("xdg-root");
        let pid_path = xdg_root.join("daemon.pid");

        fs::create_dir_all(&xdg_root).unwrap();
        fs::set_permissions(&xdg_root, fs::Permissions::from_mode(0o755)).unwrap();

        write_pid_file_at(&pid_path, &xdg_root, 12345).unwrap();

        let file_mode = fs::metadata(&pid_path).unwrap().permissions().mode() & 0o777;
        let root_mode = fs::metadata(&xdg_root).unwrap().permissions().mode() & 0o777;

        assert_eq!(read_pid_file_at(&pid_path, &xdg_root).unwrap(), Some(12345));
        assert_eq!(file_mode, SECURE_STATE_FILE_MODE);
        assert_eq!(root_mode, XDG_STATE_DIR_MODE);

        remove_pid_file_at(&pid_path, &xdg_root).unwrap();
        assert!(!pid_path.exists());
    }

    #[test]
    fn read_pid_file_returns_none_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let xdg_root = tmp.path().canonicalize().unwrap().join("xdg-root");
        let pid_path = xdg_root.join("daemon.pid");
        fs::create_dir_all(&xdg_root).unwrap();

        assert_eq!(read_pid_file_at(&pid_path, &xdg_root).unwrap(), None);
    }

    #[test]
    fn flock_probe_detects_stale_pid_file() {
        let tmp = tempfile::tempdir().unwrap();
        let pid_path = tmp.path().join("daemon.pid");

        fs::write(&pid_path, "99999").unwrap();

        let file = File::open(&pid_path).unwrap();
        match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(lock) => {
                drop(lock);
            }
            Err(_) => panic!("expected to acquire lock on stale pid file"),
        }
    }

    #[test]
    fn flock_probe_detects_held_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let pid_path = tmp.path().join("daemon.pid");

        let pid = std::process::id();
        fs::write(&pid_path, pid.to_string()).unwrap();

        let holder = File::open(&pid_path).unwrap();
        let _held = Flock::lock(holder, FlockArg::LockExclusiveNonblock)
            .expect("should acquire lock as holder");

        let probe = File::open(&pid_path).unwrap();
        match Flock::lock(probe, FlockArg::LockExclusiveNonblock) {
            Ok(_) => panic!("expected EWOULDBLOCK when lock is held"),
            Err((_, errno)) => {
                assert_eq!(errno, Errno::EWOULDBLOCK);
            }
        }
    }

    #[test]
    fn lock_contention_reports_running_pid_without_terminating_it() {
        struct ChildGuard {
            child: std::process::Child,
        }

        impl Drop for ChildGuard {
            fn drop(&mut self) {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let pid_path = tmp.path().join("daemon.pid");
        let child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let child_pid = child.id();
        let _child = ChildGuard { child };

        fs::write(&pid_path, child_pid.to_string()).unwrap();
        let holder = File::open(&pid_path).unwrap();
        let _held = Flock::lock(holder, FlockArg::LockExclusiveNonblock)
            .expect("should acquire lock as holder");

        let err = match observe_lock_contention(
            &pid_path,
            std::time::Duration::from_millis(20),
            std::time::Duration::from_millis(1),
        ) {
            Ok(_) => panic!("expected lock contention to lose to running daemon"),
            Err(err) => err,
        };

        match err {
            OneupError::Daemon(DaemonError::AlreadyRunning(pid)) => assert_eq!(pid, child_pid),
            other => panic!("expected already-running contention, got {other:?}"),
        }
        assert!(is_process_alive(child_pid));
    }

    #[test]
    fn lock_contention_without_readable_pid_reports_starting() {
        let tmp = tempfile::tempdir().unwrap();
        let pid_path = tmp.path().join("daemon.pid");

        fs::write(&pid_path, "").unwrap();
        let holder = File::open(&pid_path).unwrap();
        let _held = Flock::lock(holder, FlockArg::LockExclusiveNonblock)
            .expect("should acquire lock as holder");

        let err = match observe_lock_contention(
            &pid_path,
            std::time::Duration::from_millis(5),
            std::time::Duration::from_millis(1),
        ) {
            Ok(_) => panic!("expected unreadable lock holder to remain in progress"),
            Err(err) => err,
        };

        assert!(matches!(
            err,
            OneupError::Daemon(DaemonError::StartupInProgress)
        ));
    }

    #[test]
    fn drain_daemon_times_out_with_actionable_error_when_pid_ignores_sigterm() {
        struct ChildGuard {
            child: std::process::Child,
        }

        impl Drop for ChildGuard {
            fn drop(&mut self) {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }

        // A process that ignores SIGTERM, so the bounded drain must give up
        // rather than observe an exit. `kill()` (SIGKILL) cleans it up.
        let child = std::process::Command::new("sh")
            .arg("-c")
            .arg(r#"trap "" TERM; while true; do sleep 1; done"#)
            .spawn()
            .expect("spawn sigterm-ignoring child");
        let pid = child.id();
        let _guard = ChildGuard { child };

        assert!(
            is_process_alive(pid),
            "child should be running before drain"
        );

        let err = drain_daemon(pid, std::time::Duration::from_millis(50))
            .expect_err("drain must time out when the pid ignores SIGTERM");

        let msg = err.to_string();
        assert!(
            msg.contains("1up stop"),
            "timeout error must instruct `1up stop`: {msg}"
        );
        assert!(
            msg.contains("retry"),
            "timeout error must instruct a retry: {msg}"
        );
        assert!(
            is_process_alive(pid),
            "drain must not force-kill the daemon on timeout"
        );
    }

    fn state_root_dir() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let state_root = tmp.path().canonicalize().unwrap().join("project");
        fs::create_dir_all(&state_root).unwrap();
        (tmp, state_root)
    }

    #[test]
    fn rebuild_lock_creates_secure_lockfile_under_dot_1up() {
        let (_tmp, state_root) = state_root_dir();

        let _lock = acquire_rebuild_lock(&state_root).unwrap();

        let lock_path = crate::shared::config::project_rebuild_lock_path(&state_root);
        assert_eq!(lock_path, state_root.join(".1up").join("rebuild.lock"));
        assert!(lock_path.exists(), "lockfile must be created on acquire");

        let mode = fs::metadata(&lock_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, SECURE_STATE_FILE_MODE);
    }

    #[test]
    fn rebuild_lock_contention_fails_closed_with_named_reason() {
        let (_tmp, state_root) = state_root_dir();

        let _held = acquire_rebuild_lock(&state_root).unwrap();

        // A second acquirer must give up within the bound rather than start a
        // competing rebuild, and the reason must name the contended index.
        let err = acquire_rebuild_lock_with_bound(
            &state_root,
            Duration::from_millis(30),
            Duration::from_millis(1),
        )
        .expect_err("second acquisition must fail closed while the lock is held");

        assert!(matches!(
            err,
            OneupError::Daemon(DaemonError::RebuildLockContended { .. })
        ));
        let msg = err.to_string();
        assert!(
            msg.contains(&state_root.display().to_string()),
            "contention error must name the index path: {msg}"
        );
        assert!(
            msg.contains("rebuilding the index"),
            "contention error must explain the cause: {msg}"
        );
    }

    #[test]
    fn rebuild_lock_releases_on_drop_so_a_later_acquire_succeeds() {
        let (_tmp, state_root) = state_root_dir();

        {
            let _held = acquire_rebuild_lock(&state_root).unwrap();
            assert!(
                try_acquire_rebuild_lock(&state_root).unwrap().is_none(),
                "lock must be exclusive while held"
            );
        }

        // The flock auto-released when the guard dropped: a fresh acquire wins.
        let reacquired = try_acquire_rebuild_lock(&state_root).unwrap();
        assert!(
            reacquired.is_some(),
            "lock must be re-acquirable after the holder is dropped"
        );
    }

    #[test]
    fn rebuild_lock_is_keyed_per_state_root() {
        let (_tmp_a, state_root_a) = state_root_dir();
        let (_tmp_b, state_root_b) = state_root_dir();

        // Holding the lock for one state root blocks a second acquire for the
        // SAME state root (linked worktrees share `.1up/` so resolve here),...
        let _held = acquire_rebuild_lock(&state_root_a).unwrap();
        assert!(
            try_acquire_rebuild_lock(&state_root_a).unwrap().is_none(),
            "same state root must contend on the same lock"
        );

        // ...but leaves an independent state root free to acquire its own lock.
        assert!(
            try_acquire_rebuild_lock(&state_root_b).unwrap().is_some(),
            "a different state root must use an independent lock"
        );
    }

    #[test]
    fn recent_rebuild_lock_is_not_stale() {
        let (_tmp, state_root) = state_root_dir();

        // Acquire the lock (it's fresh)
        let _lock = acquire_rebuild_lock(&state_root).unwrap();

        // A recently created lock should not be stale
        assert!(
            !is_rebuild_lock_stale(&state_root).unwrap(),
            "recent lock should not be stale"
        );
    }

    #[test]
    fn rebuild_lock_missing_is_not_stale() {
        let (_tmp, state_root) = state_root_dir();

        // If lock file doesn't exist, it's not stale
        assert!(
            !is_rebuild_lock_stale(&state_root).unwrap(),
            "missing lock file should not be stale"
        );
    }
}
