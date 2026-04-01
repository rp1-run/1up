use std::path::PathBuf;

use crate::shared::errors::{ConfigError, OneupError};

const APP_NAME: &str = "1up";

/// Returns the XDG config directory for 1up (~/.config/1up/).
pub fn config_dir() -> Result<PathBuf, OneupError> {
    let base = dirs::config_dir()
        .ok_or_else(|| ConfigError::XdgDirNotFound("XDG config directory not found".to_string()))?;
    Ok(base.join(APP_NAME))
}

/// Returns the XDG data directory for 1up (~/.local/share/1up/).
pub fn data_dir() -> Result<PathBuf, OneupError> {
    let base = dirs::data_dir()
        .ok_or_else(|| ConfigError::XdgDirNotFound("XDG data directory not found".to_string()))?;
    Ok(base.join(APP_NAME))
}

/// Returns the path to the embedding model directory.
pub fn model_dir() -> Result<PathBuf, OneupError> {
    Ok(data_dir()?.join("models").join("all-MiniLM-L6-v2"))
}

/// Returns the path to the daemon PID file.
pub fn pid_file_path() -> Result<PathBuf, OneupError> {
    Ok(data_dir()?.join("daemon.pid"))
}

/// Returns the path to the global project registry.
pub fn projects_registry_path() -> Result<PathBuf, OneupError> {
    Ok(data_dir()?.join("projects.json"))
}

/// Returns the path to the project-local .1up directory for a given project root.
pub fn project_dot_dir(project_root: &std::path::Path) -> PathBuf {
    project_root.join(".1up")
}

/// Returns the path to the project-local database file.
pub fn project_db_path(project_root: &std::path::Path) -> PathBuf {
    project_dot_dir(project_root).join("index.db")
}

/// Returns the path to the project_id file within the .1up directory.
pub fn project_id_path(project_root: &std::path::Path) -> PathBuf {
    project_dot_dir(project_root).join("project_id")
}
