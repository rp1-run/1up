use std::path::Path;

use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use tracing::{debug, warn};

use crate::shared::config;
use crate::shared::constants::{SECURE_STATE_FILE_MODE, XDG_STATE_DIR_MODE};
use crate::shared::errors::{DaemonError, OneupError};
use crate::shared::fs::{
    atomic_replace, ensure_secure_xdg_root, remove_regular_file, validate_regular_file_path,
};

pub const fn supports_daemon() -> bool {
    true
}

pub fn write_pid_file() -> Result<(), OneupError> {
    let xdg_root = ensure_secure_xdg_root()
        .map_err(|err| DaemonError::PidFileError(format!("failed to prepare pid root: {err}")))?;
    let pid = std::process::id();
    write_pid_file_at(&config::pid_file_path()?, &xdg_root, pid)
}

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

pub fn read_pid_file() -> Result<Option<u32>, OneupError> {
    let xdg_root = ensure_secure_xdg_root()
        .map_err(|err| DaemonError::PidFileError(format!("failed to prepare pid root: {err}")))?;
    read_pid_file_at(&config::pid_file_path()?, &xdg_root)
}

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

pub fn is_daemon_running() -> Result<Option<u32>, OneupError> {
    match read_pid_file()? {
        Some(pid) => {
            if is_process_alive(pid) {
                Ok(Some(pid))
            } else {
                warn!("stale pid file detected (pid={pid}), cleaning up");
                remove_pid_file()?;
                Ok(None)
            }
        }
        None => Ok(None),
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
pub fn ensure_daemon(project_id: &str, project_root: &Path) -> Result<u32, OneupError> {
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
            registry.register(project_id, project_root, None)?;
            send_sighup(pid)?;
            debug!("auto-registered project and sent SIGHUP to daemon (pid={pid})");
        }

        return Ok(pid);
    }

    let mut registry = Registry::load()?;
    registry.register(project_id, project_root, None)?;

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
}
