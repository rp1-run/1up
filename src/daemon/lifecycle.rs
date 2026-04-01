use std::path::Path;

use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use tracing::{debug, warn};

use crate::shared::config;
use crate::shared::errors::{DaemonError, OneupError};

pub fn write_pid_file() -> Result<(), OneupError> {
    let path = config::pid_file_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| DaemonError::PidFileError(format!("failed to create pid dir: {e}")))?;
    }

    let pid = std::process::id();
    std::fs::write(&path, pid.to_string())
        .map_err(|e| DaemonError::PidFileError(format!("failed to write pid file: {e}")))?;

    debug!("wrote pid file: {} (pid={})", path.display(), pid);
    Ok(())
}

pub fn read_pid_file() -> Result<Option<u32>, OneupError> {
    let path = config::pid_file_path()?;
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
    let path = config::pid_file_path()?;
    if path.exists() {
        std::fs::remove_file(&path)
            .map_err(|e| DaemonError::PidFileError(format!("failed to remove pid file: {e}")))?;
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
                nix::unistd::setsid().map_err(|e| {
                    std::io::Error::new(std::io::ErrorKind::Other, format!("setsid: {e}"))
                })?;
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
            registry.register(project_id, project_root)?;
            send_sighup(pid)?;
            debug!("auto-registered project and sent SIGHUP to daemon (pid={pid})");
        }

        return Ok(pid);
    }

    let mut registry = Registry::load()?;
    registry.register(project_id, project_root)?;

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
    fn pid_file_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let pid_path = tmp.path().join("test.pid");
        let pid = 12345u32;

        std::fs::write(&pid_path, pid.to_string()).unwrap();
        let content = std::fs::read_to_string(&pid_path).unwrap();
        let read_pid: u32 = content.trim().parse().unwrap();
        assert_eq!(read_pid, pid);
    }
}
