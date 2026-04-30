use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Write};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
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
#[allow(dead_code)]
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

#[allow(dead_code)]
pub fn ensure_project_id(project_root: &Path) -> Result<(String, bool), OneupError> {
    match read_project_id(project_root) {
        Ok(project_id) => return Ok((project_id, false)),
        Err(err) if !is_not_initialized(&err) => return Err(err),
        Err(_) => {}
    }

    let dot_dir = ensure_secure_project_root(project_root)
        .map_err(|err| ProjectError::WriteFailed(err.to_string()))?;

    match create_project_id_if_absent(&dot_dir) {
        Ok(project_id) => Ok((project_id, true)),
        Err(err) if is_already_initialized(&err) => Ok((read_project_id(project_root)?, false)),
        Err(err) => Err(err),
    }
}

/// Checks whether a project has been initialized at the given root.
pub fn is_initialized(project_root: &Path) -> bool {
    read_project_id(project_root).is_ok()
}

#[allow(dead_code)]
fn create_project_id_if_absent(dot_dir: &Path) -> Result<String, OneupError> {
    let id = Uuid::new_v4().to_string();
    let path = validate_regular_file_path(&dot_dir.join("project_id"), dot_dir)
        .map_err(|err| ProjectError::WriteFailed(err.to_string()))?;
    let temp_path = validate_regular_file_path(
        &dot_dir.join(format!(".project-id-tmp-{}", Uuid::new_v4())),
        dot_dir,
    )
    .map_err(|err| ProjectError::WriteFailed(err.to_string()))?;

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(SECURE_STATE_FILE_MODE);

    let mut file = match options.open(&temp_path) {
        Ok(file) => file,
        Err(err) => return Err(project_write_io_error(&path, err)),
    };

    let write_result = (|| -> Result<(), OneupError> {
        set_project_id_mode(&temp_path)?;
        file.write_all(id.as_bytes())
            .map_err(|source| project_write_io_error(&temp_path, source))?;
        file.sync_all()
            .map_err(|source| project_write_io_error(&temp_path, source))
    })();
    drop(file);

    let publish_result = write_result.and_then(|()| {
        std::fs::hard_link(&temp_path, &path).map_err(|source| {
            if source.kind() == ErrorKind::AlreadyExists {
                ProjectError::AlreadyInitialized(path.display().to_string()).into()
            } else {
                project_write_io_error(&path, source)
            }
        })?;
        set_project_id_mode(&path)?;
        Ok(())
    });

    let _ = std::fs::remove_file(&temp_path);
    publish_result?;
    sync_project_state_dir(dot_dir)?;

    Ok(id)
}

#[allow(dead_code)]
fn is_not_initialized(err: &OneupError) -> bool {
    matches!(err, OneupError::Project(ProjectError::NotInitialized))
}

#[allow(dead_code)]
fn is_already_initialized(err: &OneupError) -> bool {
    matches!(
        err,
        OneupError::Project(ProjectError::AlreadyInitialized(_))
    )
}

#[allow(dead_code)]
fn project_write_io_error(path: &Path, source: std::io::Error) -> OneupError {
    ProjectError::WriteFailed(format!("{}: {source}", path.display())).into()
}

#[allow(dead_code)]
fn set_project_id_mode(path: &Path) -> Result<(), OneupError> {
    #[cfg(unix)]
    {
        std::fs::set_permissions(
            path,
            std::fs::Permissions::from_mode(SECURE_STATE_FILE_MODE),
        )
        .map_err(|source| project_write_io_error(path, source))
    }

    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

#[allow(dead_code)]
fn sync_project_state_dir(path: &Path) -> Result<(), OneupError> {
    #[cfg(unix)]
    {
        File::open(path)
            .map_err(|source| project_write_io_error(path, source))?
            .sync_all()
            .map_err(|source| project_write_io_error(path, source))
    }

    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// A resolved project with separate state and source roots.
///
/// When running from a git worktree, `state_root` points to the main
/// repository (where `.1up/` lives) while `source_root` points to the
/// worktree (where the user's files are). Outside a worktree, both are
/// identical.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProject {
    /// Root where `.1up/` state directory lives — use for DB path, project ID,
    /// daemon communication, and registry operations.
    pub state_root: PathBuf,
    /// Root where source files should be read — use for scanning, indexing,
    /// file resolution, and fence installation.
    pub source_root: PathBuf,
}

/// Resolves the project roots for a given path by searching for an existing
/// `.1up/` directory. Checks the canonicalized path and its ancestors first,
/// then falls back to git worktree detection. Returns separate state and
/// source roots to avoid conflating where `.1up/` lives with where the
/// user's files are.
pub fn resolve_project_root(path: &Path) -> std::io::Result<ResolvedProject> {
    let canonical = path.canonicalize()?;

    if let Some(existing_root) = find_existing_project_root(&canonical) {
        return Ok(ResolvedProject {
            state_root: existing_root,
            source_root: canonical,
        });
    }

    if let Some(main_root) = resolve_worktree_main_root(&canonical) {
        if main_root.join(".1up").is_dir() {
            return Ok(ResolvedProject {
                state_root: main_root,
                source_root: canonical,
            });
        }
    }

    if let Some(git_root) = resolve_git_root(&canonical) {
        return Ok(ResolvedProject {
            state_root: git_root.clone(),
            source_root: git_root,
        });
    }

    Ok(ResolvedProject {
        state_root: canonical.clone(),
        source_root: canonical,
    })
}

/// Resolves a project root for commands that may create `.1up/` state.
///
/// Existing 1up projects are reused from the nearest ancestor. When no project
/// exists yet, automatic creation is only allowed at an actual git root. This
/// prevents daemon/MCP auto-start flows from creating large accidental projects
/// in broad parent directories such as a home or workspace folder.
pub fn resolve_project_root_for_creation(path: &Path) -> std::io::Result<ResolvedProject> {
    let resolved = resolve_project_root(path)?;
    if resolved.state_root.join(".1up").is_dir() || is_git_root(&resolved.state_root) {
        return Ok(resolved);
    }

    Err(std::io::Error::new(
        ErrorKind::NotFound,
        format!(
            "no existing 1up project found and {} is not a git root",
            resolved.state_root.display()
        ),
    ))
}

pub fn ensure_project_id_for_auto_init(project_root: &Path) -> Result<(String, bool), OneupError> {
    if is_initialized(project_root) || is_git_root(project_root) {
        return ensure_project_id(project_root);
    }

    Err(ProjectError::WriteFailed(format!(
        "refusing to create .1up at {}; automatic project creation requires an existing 1up project or a git root",
        project_root.display()
    ))
    .into())
}

fn find_existing_project_root(canonical: &Path) -> Option<PathBuf> {
    let mut current = Some(canonical);
    while let Some(dir) = current {
        if dir.join(".1up").is_dir() {
            return Some(dir.to_path_buf());
        }
        current = dir.parent();
    }
    None
}

fn resolve_git_root(start: &Path) -> Option<PathBuf> {
    let mut current = Some(start);
    while let Some(dir) = current {
        if is_git_root(dir) {
            return Some(dir.to_path_buf());
        }
        current = dir.parent();
    }
    None
}

fn is_git_root(path: &Path) -> bool {
    let dot_git = path.join(".git");
    dot_git.is_dir() || dot_git.is_file()
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

    use std::collections::HashSet;
    use std::fs;
    use std::sync::{Arc, Barrier};
    use std::thread;

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

    #[test]
    fn ensure_project_id_reuses_existing_project_id() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().canonicalize().unwrap().join("project");
        fs::create_dir_all(&project_root).unwrap();

        let first = write_project_id(&project_root).unwrap();
        let (second, created_now) = ensure_project_id(&project_root).unwrap();

        assert_eq!(second, first);
        assert!(!created_now);
    }

    #[test]
    fn ensure_project_id_concurrent_callers_converge_on_one_id() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = Arc::new(tmp.path().canonicalize().unwrap().join("project"));
        fs::create_dir_all(project_root.as_ref()).unwrap();

        let callers = 16;
        let barrier = Arc::new(Barrier::new(callers));
        let handles = (0..callers)
            .map(|_| {
                let project_root = Arc::clone(&project_root);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    ensure_project_id(project_root.as_ref()).unwrap()
                })
            })
            .collect::<Vec<_>>();

        let outcomes = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();
        let ids = outcomes
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<HashSet<_>>();
        let created_count = outcomes
            .iter()
            .filter(|(_, created_now)| *created_now)
            .count();

        assert_eq!(ids.len(), 1);
        assert_eq!(created_count, 1);
        assert_eq!(
            read_project_id(project_root.as_ref()).unwrap(),
            outcomes[0].0
        );
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
        assert_eq!(resolved.state_root, root);
        assert_eq!(resolved.source_root, root);
    }

    #[test]
    fn resolve_project_root_finds_dot_1up_in_ancestor() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        fs::create_dir_all(root.join(".1up")).unwrap();
        let subdir = root.join("deep").join("nested").join("dir");
        fs::create_dir_all(&subdir).unwrap();

        let resolved = resolve_project_root(&subdir).unwrap();
        assert_eq!(resolved.state_root, root);
        assert_eq!(resolved.source_root, subdir);
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
        assert_eq!(resolved.state_root, main_repo);
        assert_eq!(resolved.source_root, worktree);
    }

    #[test]
    fn resolve_project_root_uses_git_root_when_no_project_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().canonicalize().unwrap().join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        let subdir = repo.join("src").join("nested");
        fs::create_dir_all(&subdir).unwrap();

        let resolved = resolve_project_root(&subdir).unwrap();
        assert_eq!(resolved.state_root, repo);
        assert_eq!(resolved.source_root, repo);
    }

    #[test]
    fn resolve_project_root_uses_worktree_root_when_no_project_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp_root = tmp.path().canonicalize().unwrap();

        let main_repo = tmp_root.join("main");
        fs::create_dir_all(main_repo.join(".git").join("worktrees").join("feature")).unwrap();
        let wt_gitdir = main_repo.join(".git").join("worktrees").join("feature");
        fs::write(wt_gitdir.join("commondir"), "../..").unwrap();

        let worktree = tmp_root.join("worktree");
        fs::create_dir_all(worktree.join("src")).unwrap();
        fs::write(
            worktree.join(".git"),
            format!("gitdir: {}", wt_gitdir.display()),
        )
        .unwrap();

        let resolved = resolve_project_root(&worktree.join("src")).unwrap();
        assert_eq!(resolved.state_root, worktree);
        assert_eq!(resolved.source_root, worktree);
    }

    #[test]
    fn resolve_project_root_returns_canonical_when_no_project() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let subdir = root.join("empty");
        fs::create_dir_all(&subdir).unwrap();

        let resolved = resolve_project_root(&subdir).unwrap();
        assert_eq!(resolved.state_root, subdir);
        assert_eq!(resolved.source_root, subdir);
    }

    #[test]
    fn resolve_project_root_for_creation_rejects_non_git_without_project() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let subdir = root.join("empty");
        fs::create_dir_all(&subdir).unwrap();

        let err = resolve_project_root_for_creation(&subdir).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
    }

    #[test]
    fn ensure_project_id_for_auto_init_rejects_non_git_root() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().canonicalize().unwrap().join("project");
        fs::create_dir_all(&project_root).unwrap();

        let err = ensure_project_id_for_auto_init(&project_root).unwrap_err();
        assert!(err.to_string().contains("automatic project creation"));
        assert!(!project_root.join(".1up").exists());
    }

    #[test]
    fn ensure_project_id_for_auto_init_allows_git_root() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().canonicalize().unwrap().join("project");
        fs::create_dir_all(project_root.join(".git")).unwrap();

        let (project_id, created_now) = ensure_project_id_for_auto_init(&project_root).unwrap();

        assert!(created_now);
        assert_eq!(read_project_id(&project_root).unwrap(), project_id);
    }
}
