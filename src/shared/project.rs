#[cfg(unix)]
use std::fs::File;
use std::fs::OpenOptions;
use std::io::{ErrorKind, Write};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::shared::config;
use crate::shared::constants::{PROJECT_STATE_DIR_MODE, SECURE_STATE_FILE_MODE};
use crate::shared::errors::{FilesystemError, OneupError, ProjectError};
use crate::shared::fs::{atomic_replace, ensure_secure_project_root, validate_regular_file_path};
use crate::shared::types::{BranchStatus, WorktreeContext, WorktreeRole};

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

pub fn canonical_project_root(project_root: &Path) -> PathBuf {
    project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf())
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
    /// Explicit git worktree and branch metadata for the active source root.
    pub worktree_context: WorktreeContext,
}

/// Resolves the project roots for a given path. Linked git worktrees anchor
/// state to the main worktree root while scanning files from the linked
/// worktree. Other paths reuse the nearest existing `.1up/` ancestor before
/// falling back to the enclosing git root.
pub fn resolve_project_root(path: &Path) -> std::io::Result<ResolvedProject> {
    let canonical = path.canonicalize()?;

    if let Some(worktree_info) = resolve_linked_worktree_info(&canonical) {
        return Ok(resolved_project(
            worktree_info.main_root.clone(),
            worktree_info.worktree_root.clone(),
            Some(worktree_info),
        ));
    }

    if let Some(existing_root) = find_existing_project_root(&canonical) {
        return Ok(resolved_project(existing_root, canonical, None));
    }

    if let Some(git_root) = resolve_git_root(&canonical) {
        return Ok(resolved_project(git_root.clone(), git_root, None));
    }

    Ok(resolved_project(canonical.clone(), canonical, None))
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

#[derive(Debug, Clone)]
struct GitWorktreeInfo {
    main_root: PathBuf,
    worktree_root: PathBuf,
    git_dir: PathBuf,
    common_git_dir: PathBuf,
    role: WorktreeRole,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BranchIdentity {
    branch_name: Option<String>,
    branch_ref: Option<String>,
    head_oid: Option<String>,
    branch_status: BranchStatus,
}

/// Detects if the given path is inside a linked git worktree. A linked
/// worktree has a `.git` file containing `gitdir: <path>`. The referenced
/// gitdir contains a `commondir` file pointing to the main repository's `.git`
/// directory.
fn resolve_linked_worktree_info(start: &Path) -> Option<GitWorktreeInfo> {
    let mut current = Some(start);
    while let Some(dir) = current {
        let dot_git = dir.join(".git");
        if dot_git.is_file() {
            let git_dir = read_gitdir_file(&dot_git, dir)?;
            let common_git_dir = read_commondir(&git_dir)?;
            let main_root = canonical_path(&common_git_dir).parent()?.to_path_buf();

            return Some(GitWorktreeInfo {
                main_root,
                worktree_root: dir.to_path_buf(),
                git_dir: canonical_path(&git_dir),
                common_git_dir: canonical_path(&common_git_dir),
                role: WorktreeRole::Linked,
            });
        }
        current = dir.parent();
    }
    None
}

fn resolved_project(
    state_root: PathBuf,
    source_root: PathBuf,
    linked_info: Option<GitWorktreeInfo>,
) -> ResolvedProject {
    let git_info = linked_info.or_else(|| resolve_main_worktree_info(&state_root));
    let worktree_context = build_worktree_context(&state_root, &source_root, git_info);

    ResolvedProject {
        state_root,
        source_root,
        worktree_context,
    }
}

fn resolve_main_worktree_info(state_root: &Path) -> Option<GitWorktreeInfo> {
    let dot_git = state_root.join(".git");
    if !dot_git.is_dir() {
        return None;
    }

    Some(GitWorktreeInfo {
        main_root: state_root.to_path_buf(),
        worktree_root: state_root.to_path_buf(),
        git_dir: canonical_path(&dot_git),
        common_git_dir: canonical_path(&dot_git),
        role: WorktreeRole::Main,
    })
}

fn build_worktree_context(
    state_root: &Path,
    source_root: &Path,
    git_info: Option<GitWorktreeInfo>,
) -> WorktreeContext {
    let branch = git_info
        .as_ref()
        .map(read_branch_identity)
        .unwrap_or_else(unknown_branch_identity);
    let main_worktree_root = git_info
        .as_ref()
        .map(|info| info.main_root.clone())
        .unwrap_or_else(|| state_root.to_path_buf());
    let worktree_role = git_info
        .as_ref()
        .map(|info| info.role)
        .unwrap_or(WorktreeRole::Unknown);
    let git_dir = git_info.as_ref().map(|info| info.git_dir.clone());
    let common_git_dir = git_info.as_ref().map(|info| info.common_git_dir.clone());
    let context_id = context_id_for(state_root, source_root, &branch);

    WorktreeContext {
        context_id,
        state_root: state_root.to_path_buf(),
        source_root: source_root.to_path_buf(),
        main_worktree_root,
        worktree_role,
        git_dir,
        common_git_dir,
        branch_name: branch.branch_name,
        branch_ref: branch.branch_ref,
        head_oid: branch.head_oid,
        branch_status: branch.branch_status,
    }
}

fn read_gitdir_file(dot_git: &Path, worktree_root: &Path) -> Option<PathBuf> {
    let content = std::fs::read_to_string(dot_git).ok()?;
    let gitdir_path = content.trim().strip_prefix("gitdir:")?.trim();
    if gitdir_path.is_empty() {
        return None;
    }

    Some(resolve_git_path(worktree_root, gitdir_path))
}

fn read_commondir(git_dir: &Path) -> Option<PathBuf> {
    let commondir_content = std::fs::read_to_string(git_dir.join("commondir")).ok()?;
    let commondir_ref = commondir_content.trim();
    if commondir_ref.is_empty() {
        return None;
    }

    Some(resolve_git_path(git_dir, commondir_ref))
}

fn resolve_git_path(base: &Path, raw: &str) -> PathBuf {
    let path = Path::new(raw);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

fn read_branch_identity(info: &GitWorktreeInfo) -> BranchIdentity {
    let head = match std::fs::read_to_string(info.git_dir.join("HEAD")) {
        Ok(content) => content.trim().to_string(),
        Err(_) => {
            return BranchIdentity {
                branch_status: BranchStatus::Unreadable,
                ..unknown_branch_identity()
            };
        }
    };

    if let Some(branch_ref) = head.strip_prefix("ref:").map(str::trim) {
        if branch_ref.is_empty() {
            return BranchIdentity {
                branch_status: BranchStatus::Unreadable,
                ..unknown_branch_identity()
            };
        }

        let branch_ref = branch_ref.to_string();
        let branch_name = branch_ref
            .strip_prefix("refs/heads/")
            .map(|name| name.to_string());
        let head_oid = read_ref_oid(&info.git_dir, &info.common_git_dir, &branch_ref);

        return BranchIdentity {
            branch_name,
            branch_ref: Some(branch_ref),
            head_oid,
            branch_status: BranchStatus::Named,
        };
    }

    if is_hex_oid(&head) {
        return BranchIdentity {
            branch_name: None,
            branch_ref: None,
            head_oid: Some(head),
            branch_status: BranchStatus::Detached,
        };
    }

    BranchIdentity {
        branch_status: BranchStatus::Unreadable,
        ..unknown_branch_identity()
    }
}

fn read_ref_oid(git_dir: &Path, common_git_dir: &Path, branch_ref: &str) -> Option<String> {
    [git_dir, common_git_dir]
        .into_iter()
        .find_map(|root| read_loose_ref_oid(root, branch_ref))
        .or_else(|| read_packed_ref_oid(common_git_dir, branch_ref))
        .or_else(|| read_packed_ref_oid(git_dir, branch_ref))
}

fn read_loose_ref_oid(root: &Path, branch_ref: &str) -> Option<String> {
    std::fs::read_to_string(root.join(branch_ref))
        .ok()
        .and_then(|content| first_oid_token(content.trim()))
}

fn read_packed_ref_oid(git_dir: &Path, branch_ref: &str) -> Option<String> {
    let packed_refs = std::fs::read_to_string(git_dir.join("packed-refs")).ok()?;
    packed_refs.lines().find_map(|line| {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('^') {
            return None;
        }

        let mut parts = line.split_whitespace();
        let oid = parts.next()?;
        let name = parts.next()?;
        (name == branch_ref).then(|| first_oid_token(oid)).flatten()
    })
}

fn first_oid_token(raw: &str) -> Option<String> {
    raw.split_whitespace()
        .next()
        .filter(|token| is_hex_oid(token))
        .map(|token| token.to_string())
}

fn is_hex_oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn unknown_branch_identity() -> BranchIdentity {
    BranchIdentity {
        branch_name: None,
        branch_ref: None,
        head_oid: None,
        branch_status: BranchStatus::Unknown,
    }
}

fn context_id_for(state_root: &Path, source_root: &Path, branch: &BranchIdentity) -> String {
    let branch_identity = branch
        .branch_ref
        .as_deref()
        .or(match branch.branch_status {
            BranchStatus::Detached => branch.head_oid.as_deref(),
            BranchStatus::Named | BranchStatus::Unreadable | BranchStatus::Unknown => None,
        })
        .unwrap_or_else(|| branch.branch_status.as_str());

    let mut hasher = Sha256::new();
    hasher.update(b"oneup-worktree-context-v1\0");
    hasher.update(state_root.to_string_lossy().as_bytes());
    hasher.update(b"\0");
    hasher.update(source_root.to_string_lossy().as_bytes());
    hasher.update(b"\0");
    hasher.update(branch.branch_status.as_str().as_bytes());
    hasher.update(b"\0");
    hasher.update(branch_identity.as_bytes());
    let digest = hasher.finalize();
    digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn canonical_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
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
    fn resolve_project_root_uses_worktree_main_root_when_no_project_exists() {
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
        assert_eq!(resolved.state_root, main_repo);
        assert_eq!(resolved.source_root, worktree);
    }

    #[test]
    fn resolve_project_root_for_creation_uses_worktree_main_root() {
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

        let resolved = resolve_project_root_for_creation(&worktree.join("src")).unwrap();
        assert_eq!(resolved.state_root, main_repo);
        assert_eq!(resolved.source_root, worktree);
    }

    #[test]
    fn resolve_project_root_ignores_accidental_worktree_local_state() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp_root = tmp.path().canonicalize().unwrap();

        let main_repo = tmp_root.join("main");
        fs::create_dir_all(main_repo.join(".git").join("worktrees").join("feature")).unwrap();
        let wt_gitdir = main_repo.join(".git").join("worktrees").join("feature");
        fs::write(wt_gitdir.join("commondir"), "../..").unwrap();

        let worktree = tmp_root.join("worktree");
        fs::create_dir_all(worktree.join(".1up")).unwrap();
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
    fn resolve_project_root_exposes_main_worktree_context() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().canonicalize().unwrap().join("repo");
        write_main_git_branch(&repo, "main", "1111111111111111111111111111111111111111");
        let subdir = repo.join("src");
        fs::create_dir_all(&subdir).unwrap();

        let resolved = resolve_project_root(&subdir).unwrap();
        let context = &resolved.worktree_context;

        assert_eq!(context.state_root, repo);
        assert_eq!(context.source_root, repo);
        assert_eq!(context.main_worktree_root, repo);
        assert_eq!(context.worktree_role, WorktreeRole::Main);
        assert_eq!(
            context.git_dir,
            Some(context.main_worktree_root.join(".git"))
        );
        assert_eq!(context.common_git_dir, context.git_dir);
        assert_eq!(context.branch_name.as_deref(), Some("main"));
        assert_eq!(context.branch_ref.as_deref(), Some("refs/heads/main"));
        assert_eq!(
            context.head_oid.as_deref(),
            Some("1111111111111111111111111111111111111111")
        );
        assert_eq!(context.branch_status, BranchStatus::Named);
        assert_eq!(context.context_id.len(), 32);
        assert_eq!(
            resolve_project_root(&subdir)
                .unwrap()
                .worktree_context
                .context_id,
            context.context_id
        );
    }

    #[test]
    fn resolve_project_root_exposes_linked_worktree_context() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp_root = tmp.path().canonicalize().unwrap();
        let main_repo = tmp_root.join("main");
        let worktree = tmp_root.join("worktree");
        write_main_git_branch(
            &main_repo,
            "main",
            "1111111111111111111111111111111111111111",
        );
        write_linked_worktree_branch(
            &main_repo,
            &worktree,
            "feature",
            "2222222222222222222222222222222222222222",
        );

        let resolved = resolve_project_root(&worktree).unwrap();
        let context = &resolved.worktree_context;

        assert_eq!(resolved.state_root, main_repo);
        assert_eq!(resolved.source_root, worktree);
        assert_eq!(context.state_root, main_repo);
        assert_eq!(context.source_root, worktree);
        assert_eq!(context.main_worktree_root, main_repo);
        assert_eq!(context.worktree_role, WorktreeRole::Linked);
        assert_eq!(
            context.git_dir,
            Some(main_repo.join(".git").join("worktrees").join("feature"))
        );
        assert_eq!(context.common_git_dir, Some(main_repo.join(".git")));
        assert_eq!(context.branch_name.as_deref(), Some("feature"));
        assert_eq!(context.branch_ref.as_deref(), Some("refs/heads/feature"));
        assert_eq!(
            context.head_oid.as_deref(),
            Some("2222222222222222222222222222222222222222")
        );
        assert_eq!(context.branch_status, BranchStatus::Named);
        assert_eq!(context.context_id.len(), 32);
    }

    #[test]
    fn resolve_project_root_exposes_detached_branch_context() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().canonicalize().unwrap().join("repo");
        let git_dir = repo.join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        fs::write(
            git_dir.join("HEAD"),
            "3333333333333333333333333333333333333333\n",
        )
        .unwrap();

        let resolved = resolve_project_root(&repo).unwrap();
        let context = &resolved.worktree_context;

        assert_eq!(context.worktree_role, WorktreeRole::Main);
        assert_eq!(context.branch_name, None);
        assert_eq!(context.branch_ref, None);
        assert_eq!(
            context.head_oid.as_deref(),
            Some("3333333333333333333333333333333333333333")
        );
        assert_eq!(context.branch_status, BranchStatus::Detached);
    }

    #[test]
    fn resolve_project_root_exposes_unreadable_branch_context() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().canonicalize().unwrap().join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();

        let resolved = resolve_project_root(&repo).unwrap();
        let context = &resolved.worktree_context;

        assert_eq!(context.worktree_role, WorktreeRole::Main);
        assert_eq!(context.branch_name, None);
        assert_eq!(context.branch_ref, None);
        assert_eq!(context.head_oid, None);
        assert_eq!(context.branch_status, BranchStatus::Unreadable);
    }

    #[test]
    fn resolve_project_root_exposes_unknown_worktree_context_without_git() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap().join("empty");
        fs::create_dir_all(&root).unwrap();

        let resolved = resolve_project_root(&root).unwrap();
        let context = &resolved.worktree_context;

        assert_eq!(context.state_root, root);
        assert_eq!(context.source_root, root);
        assert_eq!(context.main_worktree_root, root);
        assert_eq!(context.worktree_role, WorktreeRole::Unknown);
        assert_eq!(context.git_dir, None);
        assert_eq!(context.common_git_dir, None);
        assert_eq!(context.branch_name, None);
        assert_eq!(context.branch_ref, None);
        assert_eq!(context.head_oid, None);
        assert_eq!(context.branch_status, BranchStatus::Unknown);
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

    fn write_main_git_branch(repo: &std::path::Path, branch: &str, oid: &str) {
        let git_dir = repo.join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        fs::write(git_dir.join("HEAD"), format!("ref: refs/heads/{branch}\n")).unwrap();
        write_ref(&git_dir, &format!("refs/heads/{branch}"), oid);
    }

    fn write_linked_worktree_branch(
        main_repo: &std::path::Path,
        worktree: &std::path::Path,
        branch: &str,
        oid: &str,
    ) {
        let worktree_git_dir = main_repo.join(".git").join("worktrees").join(branch);
        fs::create_dir_all(&worktree_git_dir).unwrap();
        fs::create_dir_all(worktree).unwrap();
        fs::write(worktree_git_dir.join("commondir"), "../..").unwrap();
        fs::write(
            worktree_git_dir.join("HEAD"),
            format!("ref: refs/heads/{branch}\n"),
        )
        .unwrap();
        fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", worktree_git_dir.display()),
        )
        .unwrap();
        write_ref(
            &main_repo.join(".git"),
            &format!("refs/heads/{branch}"),
            oid,
        );
    }

    fn write_ref(git_dir: &std::path::Path, branch_ref: &str, oid: &str) {
        let ref_path = git_dir.join(branch_ref);
        fs::create_dir_all(ref_path.parent().unwrap()).unwrap();
        fs::write(ref_path, format!("{oid}\n")).unwrap();
    }
}
