use std::io::ErrorKind;
use std::path::Path;

use uuid::Uuid;

use crate::shared::config;
use crate::shared::constants::{PROJECT_STATE_DIR_MODE, SECURE_STATE_FILE_MODE};
use crate::shared::errors::{FilesystemError, OneupError, ProjectError};
use crate::shared::fs::{atomic_replace, ensure_secure_project_root, validate_regular_file_path};

/// Reads the project ID from the .1up/project_id file at the given project root.
pub fn read_project_id(project_root: &Path) -> Result<String, OneupError> {
    let path = config::project_id_path(project_root);
    let path = match validate_regular_file_path(&path, project_root) {
        Ok(path) => path,
        Err(OneupError::Filesystem(FilesystemError::Io { source, .. }))
            if source.kind() == ErrorKind::NotFound =>
        {
            return Err(ProjectError::NotInitialized.into());
        }
        Err(err) => return Err(ProjectError::ReadFailed(err.to_string()).into()),
    };

    std::fs::read_to_string(&path)
        .map(|s| s.trim().to_string())
        .map_err(|err| {
            if err.kind() == ErrorKind::NotFound {
                ProjectError::NotInitialized.into()
            } else {
                ProjectError::ReadFailed(err.to_string()).into()
            }
        })
}

/// Writes a new project ID to the .1up/project_id file, creating the directory if needed.
/// Returns the generated project ID.
pub fn write_project_id(project_root: &Path) -> Result<String, OneupError> {
    let dot_dir = ensure_secure_project_root(project_root)
        .map_err(|err| ProjectError::WriteFailed(err.to_string()))?;
    let id = Uuid::new_v4().to_string();
    let path = dot_dir.join("project_id");
    atomic_replace(
        &path,
        id.as_bytes(),
        &dot_dir,
        PROJECT_STATE_DIR_MODE,
        SECURE_STATE_FILE_MODE,
    )
    .map_err(|err| ProjectError::WriteFailed(err.to_string()))?;

    Ok(id)
}

/// Checks whether a project has been initialized at the given root.
pub fn is_initialized(project_root: &Path) -> bool {
    read_project_id(project_root).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    use std::os::unix::fs::{symlink, PermissionsExt};

    #[test]
    fn write_project_id_secures_project_state_and_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().canonicalize().unwrap().join("project");
        fs::create_dir_all(&project_root).unwrap();

        let project_id = write_project_id(&project_root).unwrap();
        let dot_dir = config::project_dot_dir(&project_root);
        let project_id_path = config::project_id_path(&project_root);
        let dot_dir_mode = fs::metadata(&dot_dir).unwrap().permissions().mode() & 0o777;
        let file_mode = fs::metadata(&project_id_path).unwrap().permissions().mode() & 0o777;

        assert_eq!(read_project_id(&project_root).unwrap(), project_id);
        assert_eq!(dot_dir_mode, PROJECT_STATE_DIR_MODE);
        assert_eq!(file_mode, SECURE_STATE_FILE_MODE);
    }

    #[test]
    fn write_project_id_rejects_symlinked_project_state_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp_root = tmp.path().canonicalize().unwrap();
        let project_root = tmp_root.join("project");
        let outside_root = tmp_root.join("outside");
        fs::create_dir_all(&project_root).unwrap();
        fs::create_dir_all(&outside_root).unwrap();
        symlink(&outside_root, project_root.join(".1up")).unwrap();

        let err = write_project_id(&project_root).unwrap_err();
        assert!(err.to_string().contains("symlink"));
    }
}
