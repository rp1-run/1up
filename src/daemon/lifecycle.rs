use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use tracing::{debug, info, warn};

use crate::shared::config;
use crate::shared::constants::{SECURE_STATE_FILE_MODE, XDG_STATE_DIR_MODE};
use crate::shared::errors::{DaemonError, OneupError};
use crate::shared::fs::{
    atomic_replace, ensure_secure_xdg_root, remove_regular_file, validate_regular_file_path,
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
    use crate::daemon::registry::Registry;

    if let Some(pid) = is_daemon_running()? {
        let mut registry = Registry::load()?;
        let already_registered = registry.projects.iter().any(|p| {
            p.project_root
                == project_root
                    .canonicalize()
                    .unwrap_or(project_root.to_path_buf())
        });

        if !already_registered {
            registry.register_with_source(project_id, project_root, source_root, None)?;
            send_sighup(pid)?;
            debug!("auto-registered project and sent SIGHUP to daemon (pid={pid})");
        }

        return Ok(pid);
    }

    let mut registry = Registry::load()?;
    registry.register_with_source(project_id, project_root, source_root, None)?;

    let binary = current_binary_path()?;
    let pid = spawn_daemon(&binary)?;
    debug!(
        "auto-started daemon (pid={pid}) for project at {}",
        project_root.display()
    );
    Ok(pid)
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
}
