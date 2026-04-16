use std::io::ErrorKind;
use std::path::{Path, PathBuf};

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

/// Resolves the project root for a given path by searching for an existing
/// `.1up/` directory. Checks the canonicalized path and its ancestors first,
/// then falls back to git worktree detection. Returns the canonicalized path
/// if no existing project is found.
pub fn resolve_project_root(path: &Path) -> std::io::Result<PathBuf> {
    let canonical = path.canonicalize()?;

    let mut current = Some(canonical.as_path());
    while let Some(dir) = current {
        if dir.join(".1up").is_dir() {
            return Ok(dir.to_path_buf());
        }
        current = dir.parent();
    }

    if let Some(main_root) = resolve_worktree_main_root(&canonical) {
        if main_root.join(".1up").is_dir() {
            return Ok(main_root);
        }
    }

    Ok(canonical)
}

/// Detects if the given path is inside a git worktree and returns the main
/// worktree root. A git worktree has a `.git` file (not directory) containing
/// `gitdir: <path>`. The referenced gitdir contains a `commondir` file
/// pointing to the main repository's `.git` directory.
fn resolve_worktree_main_root(start: &Path) -> Option<PathBuf> {
    let mut current = Some(start);
    while let Some(dir) = current {
        let dot_git = dir.join(".git");
        if dot_git.is_file() {
            let content = std::fs::read_to_string(&dot_git).ok()?;
            let gitdir_path = content.trim().strip_prefix("gitdir: ")?;
            let gitdir = if Path::new(gitdir_path).is_absolute() {
                PathBuf::from(gitdir_path)
            } else {
                dir.join(gitdir_path)
            };

            let commondir_content = std::fs::read_to_string(gitdir.join("commondir")).ok()?;
            let commondir_ref = commondir_content.trim();
            let common_git_dir = if Path::new(commondir_ref).is_absolute() {
                PathBuf::from(commondir_ref)
            } else {
                gitdir.join(commondir_ref)
            };

            let main_root = common_git_dir.canonicalize().ok()?.parent()?.to_path_buf();
            return Some(main_root);
        }
        current = dir.parent();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;

    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    #[cfg(unix)]
    fn mode_bits(path: &std::path::Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;

        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn write_project_id_secures_project_state_and_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().canonicalize().unwrap().join("project");
        fs::create_dir_all(&project_root).unwrap();

        let project_id = write_project_id(&project_root).unwrap();
        let dot_dir = config::project_dot_dir(&project_root);
        let project_id_path = config::project_id_path(&project_root);

        assert_eq!(read_project_id(&project_root).unwrap(), project_id);
        #[cfg(unix)]
        {
            assert_eq!(mode_bits(&dot_dir), PROJECT_STATE_DIR_MODE);
            assert_eq!(mode_bits(&project_id_path), SECURE_STATE_FILE_MODE);
        }
    }

    #[cfg(unix)]
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

    #[test]
    fn resolve_project_root_finds_dot_1up_at_path() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        fs::create_dir_all(root.join(".1up")).unwrap();

        let resolved = resolve_project_root(&root).unwrap();
        assert_eq!(resolved, root);
    }

    #[test]
    fn resolve_project_root_finds_dot_1up_in_ancestor() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        fs::create_dir_all(root.join(".1up")).unwrap();
        let subdir = root.join("deep").join("nested").join("dir");
        fs::create_dir_all(&subdir).unwrap();

        let resolved = resolve_project_root(&subdir).unwrap();
        assert_eq!(resolved, root);
    }

    #[test]
    fn resolve_project_root_follows_worktree_git_file() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp_root = tmp.path().canonicalize().unwrap();

        let main_repo = tmp_root.join("main");
        fs::create_dir_all(main_repo.join(".git")).unwrap();
        fs::create_dir_all(main_repo.join(".1up")).unwrap();

        let wt_gitdir = main_repo.join(".git").join("worktrees").join("feature");
        fs::create_dir_all(&wt_gitdir).unwrap();
        fs::write(wt_gitdir.join("commondir"), "../..").unwrap();

        let worktree = tmp_root.join("worktree");
        fs::create_dir_all(&worktree).unwrap();
        fs::write(
            worktree.join(".git"),
            format!("gitdir: {}", wt_gitdir.display()),
        )
        .unwrap();

        let resolved = resolve_project_root(&worktree).unwrap();
        assert_eq!(resolved, main_repo);
    }

    #[test]
    fn resolve_project_root_returns_canonical_when_no_project() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let subdir = root.join("empty");
        fs::create_dir_all(&subdir).unwrap();

        let resolved = resolve_project_root(&subdir).unwrap();
        assert_eq!(resolved, subdir);
    }
}
