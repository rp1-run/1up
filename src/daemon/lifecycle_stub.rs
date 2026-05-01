use std::path::{Path, PathBuf};

use crate::shared::errors::{DaemonError, OneupError};

pub const fn supports_daemon() -> bool {
    false
}

pub fn unsupported_message() -> &'static str {
    "background daemon workflows are not supported on this platform; use `1up index` or `1up reindex` and the local-mode search commands instead"
}

pub fn write_pid_file() -> Result<(), OneupError> {
    Err(unsupported_daemon_error())
}

pub fn read_pid_file() -> Result<Option<u32>, OneupError> {
    Ok(None)
}

pub fn remove_pid_file() -> Result<(), OneupError> {
    Ok(())
}

pub fn is_process_alive(_pid: u32) -> bool {
    false
}

pub fn is_daemon_running() -> Result<Option<u32>, OneupError> {
    Ok(None)
}

pub fn send_sighup(_pid: u32) -> Result<(), OneupError> {
    Err(unsupported_daemon_error())
}

pub fn send_sigterm(_pid: u32) -> Result<(), OneupError> {
    Err(unsupported_daemon_error())
}

pub fn spawn_daemon(_binary_path: &Path) -> Result<u32, OneupError> {
    Err(unsupported_daemon_error())
}

pub fn current_binary_path() -> Result<PathBuf, OneupError> {
    std::env::current_exe().map_err(|err| {
        DaemonError::PidFileError(format!("failed to determine binary path: {err}")).into()
    })
}

pub fn ensure_daemon(
    _project_id: &str,
    _project_root: &Path,
    _source_root: &Path,
) -> Result<u32, OneupError> {
    Err(unsupported_daemon_error())
}

fn unsupported_daemon_error() -> OneupError {
    DaemonError::RequestError(unsupported_message().to_string()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_support_is_disabled() {
        assert!(!supports_daemon());
        assert!(is_daemon_running().unwrap().is_none());
    }

    #[test]
    fn ensure_daemon_returns_local_mode_guidance() {
        let err = ensure_daemon("project-id", Path::new(".")).unwrap_err();
        assert!(err.to_string().contains("local-mode search commands"));
    }
}
