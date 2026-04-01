use std::path::Path;

use uuid::Uuid;

use crate::shared::config;
use crate::shared::errors::{OneupError, ProjectError};

/// Reads the project ID from the .1up/project_id file at the given project root.
pub fn read_project_id(project_root: &Path) -> Result<String, OneupError> {
    let path = config::project_id_path(project_root);
    std::fs::read_to_string(&path)
        .map(|s| s.trim().to_string())
        .map_err(|_| ProjectError::NotInitialized.into())
}

/// Writes a new project ID to the .1up/project_id file, creating the directory if needed.
/// Returns the generated project ID.
pub fn write_project_id(project_root: &Path) -> Result<String, OneupError> {
    let dot_dir = config::project_dot_dir(project_root);
    std::fs::create_dir_all(&dot_dir)
        .map_err(|e| ProjectError::WriteFailed(format!("failed to create .1up directory: {e}")))?;

    let id = Uuid::new_v4().to_string();
    let path = config::project_id_path(project_root);
    std::fs::write(&path, &id)
        .map_err(|e| ProjectError::WriteFailed(format!("failed to write project_id: {e}")))?;

    Ok(id)
}

/// Checks whether a project has been initialized at the given root.
pub fn is_initialized(project_root: &Path) -> bool {
    config::project_id_path(project_root).exists()
}
