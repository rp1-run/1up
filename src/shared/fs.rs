
#[cfg(any(unix, test))]
use std::fs::File;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Component, Path, PathBuf};

use uuid::Uuid;

use crate::shared::config;
use crate::shared::constants::{PROJECT_STATE_DIR_MODE, XDG_STATE_DIR_MODE};
use crate::shared::errors::{FilesystemError, OneupError};

/// Test-only: serializes process-wide environment mutation (HOME/XDG_*) across every
/// module's tests. `dirs::*` reads these env vars at call time, so two tests mutating
/// them concurrently — even in different modules — corrupt each other's resolved paths.
/// Every test that mutates the process environment must hold this single lock; it is the
/// one process-wide env-serialization mutex (the former `config`/`update` locks were
/// consolidated onto it so no two env-mutating tests in this binary can run concurrently).
///
/// Lock order where a test also holds a model lock: `MODEL_MUTEX` -> `ENV_MUTEX`
/// (acquire the model lock first, then this one). Acquire poison-tolerantly via
/// `unwrap_or_else(PoisonError::into_inner)` so a panicking test cannot cascade.
#[cfg(test)]
pub(crate) static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Three-state outcome of probing whether a filesystem path is present.
///
/// A boolean `Path::exists()` collapses "definitely absent" and "could not tell"
/// into `false`, which lets a transient failure on a flaky or unmounted network
/// mount masquerade as deletion. Any decision that destructively prunes index
/// state on absence must distinguish the two so it never false-prunes a live
/// source that is merely unreachable this cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourcePresence {
    /// The path resolved to an existing filesystem object.
    Present,
    /// The path is definitively absent: `NotFound`, or an ancestor component is
    /// not a directory (`NotADirectory`), so the path cannot exist.
    Absent,
    /// The probe could not decide — a non-`NotFound` error such as permission
    /// denied, `EIO`, `ENOTCONN`, or `ESTALE` (typical of an unreachable network
    /// mount). Callers gating a destructive prune must retain the source this
    /// cycle.
    Indeterminate,
}

/// Probe whether `path` exists, distinguishing a definitively-absent path from an
/// indeterminate/transient failure.
///
/// Follows symlinks (matching `Path::exists` / `Path::try_exists` semantics) via
/// `fs::metadata`. `NotFound` and `NotADirectory` map to [`SourcePresence::Absent`]
/// (a missing path, or a path whose ancestor is not a directory, cannot exist);
/// every other error maps to [`SourcePresence::Indeterminate`] so a permission or
/// I/O fault on a flaky mount is never mistaken for deletion.
pub fn probe_source_presence(path: &Path) -> SourcePresence {
    match fs::metadata(path) {
        Ok(_) => SourcePresence::Present,
        Err(err) => presence_from_probe_error(err.kind()),
    }
}

/// Pure `ErrorKind` -> [`SourcePresence`] mapping, split out so the transient vs
/// definitely-absent classification is unit-testable without provoking a real
/// permission or I/O fault.
fn presence_from_probe_error(kind: ErrorKind) -> SourcePresence {
    match kind {
        ErrorKind::NotFound | ErrorKind::NotADirectory => SourcePresence::Absent,
        _ => SourcePresence::Indeterminate,
    }
}

pub fn ensure_secure_xdg_root() -> Result<PathBuf, OneupError> {
    ensure_secure_dir(&config::data_dir()?, XDG_STATE_DIR_MODE)
}

pub fn ensure_secure_project_root(project_root: &Path) -> Result<PathBuf, OneupError> {
    let project_root = validate_existing_directory(project_root)?;
    ensure_secure_dir(
        &config::project_dot_dir(&project_root),
        PROJECT_STATE_DIR_MODE,
    )
}

pub fn ensure_secure_dir(path: &Path, mode: u32) -> Result<PathBuf, OneupError> {
    let absolute = normalize_absolute(path)?;
    let mut current = PathBuf::new();

    for component in absolute.components() {
        current.push(component.as_os_str());
        if is_volume_root_component(&component) {
            continue;
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) => validate_directory_metadata(&current, &metadata)?,
            Err(err) if err.kind() == ErrorKind::NotFound => {
                create_secure_dir_component(&current, mode)?;
            }
            Err(err) => return Err(io_error(&current, err)),
        }
    }

    set_path_mode(&absolute, mode)?;

    Ok(absolute)
}

fn create_secure_dir_component(path: &Path, mode: u32) -> Result<(), OneupError> {
    match fs::create_dir(path) {
        Ok(()) => set_path_mode(path, mode),
        Err(err) if err.kind() == ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(path).map_err(|source| io_error(path, source))?;
            validate_directory_metadata(path, &metadata)?;
            set_path_mode(path, mode)
        }
        Err(err) => Err(io_error(path, err)),
    }
}

fn validate_directory_metadata(path: &Path, metadata: &fs::Metadata) -> Result<(), OneupError> {
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        return Err(FilesystemError::SymlinkComponent(path.display().to_string()).into());
    }
    if !file_type.is_dir() {
        return Err(unexpected_type(path, "directory", &file_type));
    }
    Ok(())
}

pub fn ensure_secure_dir_within_root(
    path: &Path,
    approved_root: &Path,
    mode: u32,
) -> Result<PathBuf, OneupError> {
    let root = ensure_secure_dir(approved_root, mode)?;
    let absolute = normalize_absolute(path)?;
    if !absolute.starts_with(&root) {
        return Err(outside_root(&absolute, &root));
    }

    ensure_secure_dir(&absolute, mode)
}

pub fn validate_regular_file_path(
    path: &Path,
    approved_root: &Path,
) -> Result<PathBuf, OneupError> {
    validate_leaf_path(path, approved_root, Some(ExpectedLeaf::RegularFile))
}

pub fn clamp_canonical_path_to_root(
    approved_root: &Path,
    candidate: &Path,
) -> Result<PathBuf, OneupError> {
    let root = canonicalize_existing(approved_root)?;
    let canonical_candidate = canonicalize_existing(candidate)?;
    if !canonical_candidate.starts_with(&root) {
        return Err(outside_root(&canonical_candidate, &root));
    }

    Ok(canonical_candidate)
}

pub fn atomic_replace(
    path: &Path,
    contents: &[u8],
    approved_root: &Path,
    parent_mode: u32,
    file_mode: u32,
) -> Result<PathBuf, OneupError> {
    let absolute = normalize_absolute(path)?;
    let parent = absolute.parent().ok_or_else(|| {
        FilesystemError::InvalidPath(format!(
            "path must have a parent directory: {}",
            absolute.display()
        ))
    })?;
    let secure_parent = ensure_secure_dir_within_root(parent, approved_root, parent_mode)?;
    let validated_path = validate_regular_file_path(&absolute, approved_root)?;
    let temp_path = secure_parent.join(format!(".1up-tmp-{}", Uuid::new_v4()));

    let write_result = (|| -> Result<(), OneupError> {
        let mut temp_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .map_err(|source| io_error(&temp_path, source))?;
        set_path_mode(&temp_path, file_mode)?;
        temp_file
            .write_all(contents)
            .map_err(|source| io_error(&temp_path, source))?;
        temp_file
            .sync_all()
            .map_err(|source| io_error(&temp_path, source))?;

        fs::rename(&temp_path, &validated_path)
            .map_err(|source| io_error(&validated_path, source))?;
        set_path_mode(&validated_path, file_mode)?;
        sync_directory(&secure_parent)?;

        Ok(())
    })();

    if write_result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }

    write_result?;
    Ok(validated_path)
}

/// Atomically replaces the contents of an existing regular file that lives
/// inside `project_root`, preserving the file's existing permission mode.
///
/// Unlike [`atomic_replace`], this clamps to the user's project root rather than
/// the `.1up` secure state directory and does not impose 1up's restrictive
/// state-file mode: whatever mode the user's file already has is preserved. The
/// target's parent must already exist within the project root, a symlink leaf is
/// rejected, and any target whose parent canonicalizes outside `project_root` is
/// rejected before any write occurs. The replacement is written to a sibling
/// temp file, fsync'd, and atomically renamed over the target.
pub fn atomic_replace_within_project_root(
    path: &Path,
    contents: &[u8],
    project_root: &Path,
) -> Result<PathBuf, OneupError> {
    let validated_path = validate_regular_file_path(path, project_root)?;
    let parent = validated_path.parent().ok_or_else(|| {
        FilesystemError::InvalidPath(format!(
            "path must have a parent directory: {}",
            validated_path.display()
        ))
    })?;
    let existing_mode = existing_file_mode(&validated_path)?;
    let temp_path = parent.join(format!(".1up-tmp-{}", Uuid::new_v4()));

    let write_result = (|| -> Result<(), OneupError> {
        let mut temp_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .map_err(|source| io_error(&temp_path, source))?;
        if let Some(mode) = existing_mode {
            set_path_mode(&temp_path, mode)?;
        }
        temp_file
            .write_all(contents)
            .map_err(|source| io_error(&temp_path, source))?;
        temp_file
            .sync_all()
            .map_err(|source| io_error(&temp_path, source))?;

        fs::rename(&temp_path, &validated_path)
            .map_err(|source| io_error(&validated_path, source))?;
        if let Some(mode) = existing_mode {
            set_path_mode(&validated_path, mode)?;
        }
        sync_directory(parent)?;

        Ok(())
    })();

    if write_result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }

    write_result?;
    Ok(validated_path)
}

/// Atomically renames an existing finalized file `source` over `dest`, where both
/// paths resolve inside `approved_root`.
///
/// The index swap uses this to switch a finalized staging database over the
/// served `index.db` in a single `rename(2)` so any reader observes either the
/// full prior file or the full new file, never a partial or absent one. Both
/// paths are clamped to `approved_root` and a symlink leaf at either end is
/// rejected before the rename (mirroring [`atomic_replace`]'s validation); the
/// `source` must be an existing regular file. After the rename the destination's
/// parent directory is fsync'd so the switched directory entry is durable.
///
/// Unlike [`atomic_replace`], the source is an already-built file rather than
/// in-memory bytes, and the existing file mode is preserved (the rename carries
/// the source's mode), so this imposes no state-file mode of its own.
pub fn atomic_rename_file_within_root(
    source: &Path,
    dest: &Path,
    approved_root: &Path,
) -> Result<PathBuf, OneupError> {
    let validated_source = validate_regular_file_path(source, approved_root)?;
    let source_metadata =
        fs::symlink_metadata(&validated_source).map_err(|err| io_error(&validated_source, err))?;
    if !source_metadata.file_type().is_file() {
        return Err(unexpected_type(
            &validated_source,
            "regular file",
            &source_metadata.file_type(),
        ));
    }

    let validated_dest = validate_regular_file_path(dest, approved_root)?;
    let parent = validated_dest.parent().ok_or_else(|| {
        FilesystemError::InvalidPath(format!(
            "path must have a parent directory: {}",
            validated_dest.display()
        ))
    })?;

    fs::rename(&validated_source, &validated_dest).map_err(|err| io_error(&validated_dest, err))?;
    sync_directory(parent)?;

    Ok(validated_dest)
}

pub fn remove_regular_file(path: &Path, approved_root: &Path) -> Result<bool, OneupError> {
    remove_expected_leaf(path, approved_root, ExpectedLeaf::RegularFile)
}

pub fn remove_socket_file(path: &Path, approved_root: &Path) -> Result<bool, OneupError> {
    remove_expected_leaf(path, approved_root, ExpectedLeaf::Socket)
}

#[derive(Clone, Copy, Debug)]
enum ExpectedLeaf {
    RegularFile,
    Socket,
}

impl ExpectedLeaf {
    fn expected_name(self) -> &'static str {
        match self {
            Self::RegularFile => "regular file",
            Self::Socket => "socket",
        }
    }

    fn matches(self, file_type: &fs::FileType) -> bool {
        match self {
            Self::RegularFile => file_type.is_file(),
            Self::Socket => is_socket_type(file_type),
        }
    }
}

fn validate_leaf_path(
    path: &Path,
    approved_root: &Path,
    expected_existing: Option<ExpectedLeaf>,
) -> Result<PathBuf, OneupError> {
    let root = canonicalize_existing(approved_root)?;
    let absolute = normalize_absolute(path)?;
    let file_name = absolute.file_name().ok_or_else(|| {
        FilesystemError::InvalidPath(format!(
            "path must include a file name: {}",
            absolute.display()
        ))
    })?;
    let parent = absolute.parent().ok_or_else(|| {
        FilesystemError::InvalidPath(format!(
            "path must have a parent directory: {}",
            absolute.display()
        ))
    })?;
    let canonical_parent = canonicalize_existing(parent)?;
    if !canonical_parent.starts_with(&root) {
        return Err(outside_root(&absolute, &root));
    }

    let validated_path = canonical_parent.join(file_name);
    match fs::symlink_metadata(&validated_path) {
        Ok(metadata) => {
            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                return Err(FilesystemError::SymlinkComponent(
                    validated_path.display().to_string(),
                )
                .into());
            }
            if let Some(expected_leaf) = expected_existing {
                if !expected_leaf.matches(&file_type) {
                    return Err(unexpected_type(
                        &validated_path,
                        expected_leaf.expected_name(),
                        &file_type,
                    ));
                }
            }
        }
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => return Err(io_error(&validated_path, err)),
    }

    Ok(validated_path)
}

fn remove_expected_leaf(
    path: &Path,
    approved_root: &Path,
    expected_leaf: ExpectedLeaf,
) -> Result<bool, OneupError> {
    let root = canonicalize_existing(approved_root)?;
    let absolute = normalize_absolute(path)?;
    let file_name = absolute.file_name().ok_or_else(|| {
        FilesystemError::InvalidPath(format!(
            "path must include a file name: {}",
            absolute.display()
        ))
    })?;
    let parent = absolute.parent().ok_or_else(|| {
        FilesystemError::InvalidPath(format!(
            "path must have a parent directory: {}",
            absolute.display()
        ))
    })?;

    let canonical_parent = match canonicalize_existing_if_present(parent)? {
        Some(path) => path,
        None => return Ok(false),
    };
    if !canonical_parent.starts_with(&root) {
        return Err(outside_root(&absolute, &root));
    }

    let validated_path = canonical_parent.join(file_name);
    let metadata = match fs::symlink_metadata(&validated_path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(io_error(&validated_path, err)),
    };
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        return Err(FilesystemError::SymlinkComponent(validated_path.display().to_string()).into());
    }
    if !expected_leaf.matches(&file_type) {
        return Err(unexpected_type(
            &validated_path,
            expected_leaf.expected_name(),
            &file_type,
        ));
    }

    fs::remove_file(&validated_path).map_err(|source| io_error(&validated_path, source))?;
    sync_directory(&canonical_parent)?;
    Ok(true)
}

fn canonicalize_existing(path: &Path) -> Result<PathBuf, OneupError> {
    let absolute = match validate_path_components(path, MissingComponentBehavior::Error)? {
        Some(path) => path,
        None => {
            return Err(FilesystemError::InvalidPath(format!(
                "path must exist: {}",
                path.display()
            ))
            .into())
        }
    };
    fs::canonicalize(&absolute).map_err(|source| io_error(&absolute, source))
}

fn validate_existing_directory(path: &Path) -> Result<PathBuf, OneupError> {
    let absolute = match validate_path_components(path, MissingComponentBehavior::Error)? {
        Some(path) => path,
        None => {
            return Err(FilesystemError::InvalidPath(format!(
                "path must exist: {}",
                path.display()
            ))
            .into())
        }
    };
    let metadata = fs::symlink_metadata(&absolute).map_err(|source| io_error(&absolute, source))?;
    let file_type = metadata.file_type();
    if !file_type.is_dir() {
        return Err(unexpected_type(&absolute, "directory", &file_type));
    }

    Ok(absolute)
}

fn canonicalize_existing_if_present(path: &Path) -> Result<Option<PathBuf>, OneupError> {
    let absolute = match validate_path_components(path, MissingComponentBehavior::ReturnNone)? {
        Some(path) => path,
        None => return Ok(None),
    };
    match fs::canonicalize(&absolute) {
        Ok(path) => Ok(Some(path)),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(err) => Err(io_error(&absolute, err)),
    }
}

fn validate_path_components(
    path: &Path,
    missing_behavior: MissingComponentBehavior,
) -> Result<Option<PathBuf>, OneupError> {
    let absolute = normalize_absolute(path)?;
    let component_count = absolute.components().count();
    let mut current = PathBuf::new();

    for (index, component) in absolute.components().enumerate() {
        let is_leaf = index + 1 == component_count;
        current.push(component.as_os_str());
        if is_volume_root_component(&component) {
            continue;
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                let file_type = metadata.file_type();
                if file_type.is_symlink() {
                    return Err(
                        FilesystemError::SymlinkComponent(current.display().to_string()).into(),
                    );
                }
                if !is_leaf && !file_type.is_dir() {
                    return Err(unexpected_type(&current, "directory", &file_type));
                }
            }
            Err(err) if err.kind() == ErrorKind::NotFound => match missing_behavior {
                MissingComponentBehavior::Error => return Err(io_error(&current, err)),
                MissingComponentBehavior::ReturnNone => return Ok(None),
            },
            Err(err) => return Err(io_error(&current, err)),
        }
    }

    Ok(Some(absolute))
}

#[derive(Clone, Copy, Debug)]
enum MissingComponentBehavior {
    Error,
    ReturnNone,
}

/// Prefix and root components form the volume root: they always exist, cannot
/// be symlinks, and are never created or permissioned by these walks. Skipping
/// them matters on Windows, where `std::fs::canonicalize` returns
/// extended-length paths (`\\?\C:\...`) and metadata calls on the bare
/// `\\?\C:` prefix fail with `ERROR_INVALID_FUNCTION` (os error 1).
fn is_volume_root_component(component: &Component) -> bool {
    matches!(component, Component::Prefix(_) | Component::RootDir)
}

fn normalize_absolute(path: &Path) -> Result<PathBuf, OneupError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|source| io_error(Path::new("."), source))?
            .join(path)
    };

    Ok(normalize_path(&absolute))
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();

    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if normalized != Path::new("/") {
                    normalized.pop();
                }
            }
            Component::Normal(part) => normalized.push(part),
            Component::RootDir | Component::Prefix(_) => normalized.push(component.as_os_str()),
        }
    }

    normalized
}

fn sync_directory(path: &Path) -> Result<(), OneupError> {
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }

    #[cfg(unix)]
    {
        File::open(path)
            .map_err(|source| io_error(path, source))?
            .sync_all()
            .map_err(|source| io_error(path, source))
    }
}

/// Returns the permission bits (`& 0o7777`) of an existing file so they can be
/// re-applied after an atomic replace. On non-unix platforms permission modes
/// are not modeled, so this is a no-op returning `None`.
fn existing_file_mode(path: &Path) -> Result<Option<u32>, OneupError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let metadata = fs::metadata(path).map_err(|source| io_error(path, source))?;
        Ok(Some(metadata.permissions().mode() & 0o7777))
    }

    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(None)
    }
}

fn set_path_mode(path: &Path, mode: u32) -> Result<(), OneupError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .map_err(|source| io_error(path, source))
    }

    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Ok(())
    }
}

fn is_socket_type(file_type: &fs::FileType) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;

        file_type.is_socket()
    }

    #[cfg(not(unix))]
    {
        let _ = file_type;
        false
    }
}

fn io_error(path: &Path, source: std::io::Error) -> OneupError {
    FilesystemError::Io {
        path: path.display().to_string(),
        source,
    }
    .into()
}

fn outside_root(path: &Path, root: &Path) -> OneupError {
    FilesystemError::OutsideApprovedRoot {
        path: path.display().to_string(),
        root: root.display().to_string(),
    }
    .into()
}

fn unexpected_type(path: &Path, expected: &str, file_type: &fs::FileType) -> OneupError {
    FilesystemError::UnexpectedType {
        path: path.display().to_string(),
        expected: expected.to_string(),
        found: file_type_name(file_type).to_string(),
    }
    .into()
}

fn file_type_name(file_type: &fs::FileType) -> &'static str {
    if file_type.is_dir() {
        "directory"
    } else if file_type.is_file() {
        "regular file"
    } else if file_type.is_symlink() {
        "symlink"
    } else if is_socket_type(file_type) {
        "socket"
    } else {
        "special file"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::ffi::OsString;
    use std::io::Read;
    use std::path::PathBuf;
    use std::sync::{Arc, Barrier};
    use std::thread;

    use crate::shared::constants::SECURE_STATE_FILE_MODE;

    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    #[cfg(unix)]
    use std::os::unix::net::UnixListener;

    #[cfg(unix)]
    fn mode_bits(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;

        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn probe_source_presence_reports_present_for_an_existing_path() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(probe_source_presence(tmp.path()), SourcePresence::Present);
    }

    #[test]
    fn probe_source_presence_reports_absent_for_a_missing_path() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        assert_eq!(probe_source_presence(&missing), SourcePresence::Absent);
    }

    #[test]
    fn probe_source_presence_reports_absent_when_an_ancestor_is_not_a_directory() {
        // A path whose parent component is a regular file yields `NotADirectory`,
        // which is still a definitive "cannot exist" — never indeterminate.
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a-file");
        fs::write(&file, b"x").unwrap();
        let under_file = file.join("child");
        assert_eq!(probe_source_presence(&under_file), SourcePresence::Absent);
    }

    #[test]
    fn probe_error_mapping_treats_transient_errors_as_indeterminate() {
        // Permission/IO faults on a flaky or unmounted network mount must never be
        // read as deletion.
        for kind in [
            ErrorKind::PermissionDenied,
            ErrorKind::ConnectionReset,
            ErrorKind::TimedOut,
            ErrorKind::Other,
        ] {
            assert_eq!(
                presence_from_probe_error(kind),
                SourcePresence::Indeterminate,
                "{kind:?} must be indeterminate, not treated as deletion"
            );
        }
    }

    struct EnvGuard {
        saved: Vec<(&'static str, Option<OsString>)>,
    }

    impl EnvGuard {
        fn new(keys: &[&'static str]) -> Self {
            Self {
                saved: keys
                    .iter()
                    .map(|key| (*key, std::env::var_os(key)))
                    .collect(),
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, value) in &self.saved {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    #[test]
    fn ensure_secure_xdg_root_uses_owner_only_permissions() {
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|err| err.into_inner());
        let _guard = EnvGuard::new(&["XDG_DATA_HOME"]);
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var(
            "XDG_DATA_HOME",
            canonical_tmp_root(tmp.path()).join("xdg-data"),
        );

        let root = ensure_secure_xdg_root().unwrap();
        assert!(root.ends_with("1up"));
        #[cfg(unix)]
        assert_eq!(mode_bits(&root), XDG_STATE_DIR_MODE);
    }

    #[cfg(unix)]
    #[test]
    fn ensure_secure_project_root_rejects_symlink_component() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp_root = canonical_tmp_root(tmp.path());
        let real_project = tmp_root.join("real-project");
        fs::create_dir_all(&real_project).unwrap();
        let symlinked_project = tmp_root.join("linked-project");
        symlink(&real_project, &symlinked_project).unwrap();

        let err = ensure_secure_project_root(&symlinked_project).unwrap_err();
        assert!(err.to_string().contains("symlink"));
    }

    #[cfg(unix)]
    #[test]
    fn clamp_canonical_path_to_root_rejects_symlink_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp_root = canonical_tmp_root(tmp.path());
        let project_root = tmp_root.join("project");
        let outside_root = tmp_root.join("outside");
        fs::create_dir_all(&project_root).unwrap();
        fs::create_dir_all(&outside_root).unwrap();
        let outside_file = outside_root.join("secret.txt");
        fs::write(&outside_file, "secret").unwrap();
        symlink(&outside_file, project_root.join("escape.txt")).unwrap();

        let err = clamp_canonical_path_to_root(&project_root, &project_root.join("escape.txt"))
            .unwrap_err();
        assert!(err.to_string().contains("symlink"));
    }

    #[cfg(unix)]
    #[test]
    fn clamp_canonical_path_to_root_rejects_in_root_symlinked_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = canonical_tmp_root(tmp.path()).join("project");
        let real_dir = project_root.join("real");
        fs::create_dir_all(&real_dir).unwrap();
        fs::write(real_dir.join("state.json"), "{}").unwrap();
        symlink(&real_dir, project_root.join("linked")).unwrap();

        let err =
            clamp_canonical_path_to_root(&project_root, &project_root.join("linked/state.json"))
                .unwrap_err();
        assert!(err.to_string().contains("symlink"));
    }

    #[cfg(unix)]
    #[test]
    fn validate_regular_file_path_rejects_in_root_symlinked_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = canonical_tmp_root(tmp.path()).join("project");
        let real_dir = project_root.join("real");
        fs::create_dir_all(&real_dir).unwrap();
        fs::write(real_dir.join("state.json"), "{}").unwrap();
        symlink(&real_dir, project_root.join("linked")).unwrap();

        let err =
            validate_regular_file_path(&project_root.join("linked/state.json"), &project_root)
                .unwrap_err();
        assert!(err.to_string().contains("symlink"));
    }

    #[cfg(unix)]
    #[test]
    fn remove_regular_file_rejects_in_root_symlinked_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = canonical_tmp_root(tmp.path()).join("project");
        let real_dir = project_root.join("real");
        fs::create_dir_all(&real_dir).unwrap();
        let real_file = real_dir.join("state.json");
        fs::write(&real_file, "{}").unwrap();
        symlink(&real_dir, project_root.join("linked")).unwrap();

        let err = remove_regular_file(&project_root.join("linked/state.json"), &project_root)
            .unwrap_err();
        assert!(err.to_string().contains("symlink"));
        assert!(real_file.exists());
    }

    #[test]
    fn atomic_replace_sets_restrictive_permissions_and_replaces_content() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = canonical_tmp_root(tmp.path()).join("project");
        fs::create_dir_all(&project_root).unwrap();
        let secure_root = ensure_secure_project_root(&project_root).unwrap();
        let target = secure_root.join("state.json");

        let first = atomic_replace(
            &target,
            br#"{"version":1}"#,
            &secure_root,
            PROJECT_STATE_DIR_MODE,
            SECURE_STATE_FILE_MODE,
        )
        .unwrap();
        let second = atomic_replace(
            &target,
            br#"{"version":2}"#,
            &secure_root,
            PROJECT_STATE_DIR_MODE,
            SECURE_STATE_FILE_MODE,
        )
        .unwrap();

        let mut content = String::new();
        File::open(&second)
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();

        assert_eq!(first, second);
        assert_eq!(content, r#"{"version":2}"#);
        #[cfg(unix)]
        assert_eq!(mode_bits(&second), SECURE_STATE_FILE_MODE);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_replace_within_project_root_rejects_symlink_leaf() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = canonical_tmp_root(tmp.path()).join("project");
        fs::create_dir_all(&project_root).unwrap();
        let real_target = project_root.join("real.md");
        fs::write(&real_target, "original real contents\n").unwrap();
        symlink(&real_target, project_root.join("CLAUDE.md")).unwrap();

        let err = atomic_replace_within_project_root(
            &project_root.join("CLAUDE.md"),
            b"new contents\n",
            &project_root,
        )
        .unwrap_err();

        assert!(err.to_string().contains("symlink"));
        // The error must be raised without writing anything.
        assert_eq!(fs::read(&real_target).unwrap(), b"original real contents\n");
        assert_no_temp_files(&project_root);
    }

    #[test]
    fn atomic_replace_within_project_root_rejects_target_outside_root() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp_root = canonical_tmp_root(tmp.path());
        let project_root = tmp_root.join("project");
        let outside_dir = tmp_root.join("outside");
        fs::create_dir_all(&project_root).unwrap();
        fs::create_dir_all(&outside_dir).unwrap();
        let outside_file = outside_dir.join("CLAUDE.md");
        fs::write(&outside_file, "outside contents\n").unwrap();

        let err =
            atomic_replace_within_project_root(&outside_file, b"new contents\n", &project_root)
                .unwrap_err();

        assert!(err.to_string().contains("outside approved root"));
        assert_eq!(fs::read(&outside_file).unwrap(), b"outside contents\n");
        assert_no_temp_files(&project_root);
    }

    #[test]
    fn atomic_replace_within_project_root_preserves_mode_and_writes_exact_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = canonical_tmp_root(tmp.path()).join("project");
        fs::create_dir_all(&project_root).unwrap();
        let target = project_root.join("CLAUDE.md");
        let original =
            "line A\n<!-- 1up:hint:begin v=1 -->\nstale\n<!-- 1up:hint:end -->\nline B\n";
        fs::write(&target, original).unwrap();
        #[cfg(unix)]
        set_path_mode(&target, 0o640).unwrap();

        let cleaned = "line A\nline B\n";
        let written =
            atomic_replace_within_project_root(&target, cleaned.as_bytes(), &project_root).unwrap();

        // Bytes written are exactly what was handed in: surrounding lines are
        // byte-identical and only the removed span is gone.
        assert_eq!(fs::read(&written).unwrap(), cleaned.as_bytes());
        #[cfg(unix)]
        assert_eq!(mode_bits(&written), 0o640);
        assert_no_temp_files(&project_root);
    }

    #[test]
    fn atomic_rename_file_within_root_replaces_dest_and_preserves_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let root = ensure_secure_dir(
            &canonical_tmp_root(tmp.path()).join("secure"),
            PROJECT_STATE_DIR_MODE,
        )
        .unwrap();
        let source = root.join("index.db.rebuild-1234");
        let dest = root.join("index.db");
        fs::write(&source, b"new").unwrap();
        fs::write(&dest, b"old").unwrap();
        #[cfg(unix)]
        set_path_mode(&source, 0o640).unwrap();

        let renamed = atomic_rename_file_within_root(&source, &dest, &root).unwrap();

        assert_eq!(renamed, dest);
        assert_eq!(fs::read(&dest).unwrap(), b"new");
        // The source name is consumed by the rename.
        assert!(!source.exists());
        // The destination carries the source's mode (no state-file mode imposed).
        #[cfg(unix)]
        assert_eq!(mode_bits(&dest), 0o640);
    }

    #[test]
    fn atomic_rename_file_within_root_rejects_missing_source() {
        let tmp = tempfile::tempdir().unwrap();
        let root = ensure_secure_dir(
            &canonical_tmp_root(tmp.path()).join("secure"),
            PROJECT_STATE_DIR_MODE,
        )
        .unwrap();
        let dest = root.join("index.db");
        fs::write(&dest, b"old").unwrap();

        let err =
            atomic_rename_file_within_root(&root.join("absent.rebuild"), &dest, &root).unwrap_err();

        // Platform-agnostic "not found": Unix reports "No such file or directory";
        // Windows reports "The system cannot find the file specified".
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("no such file") || msg.contains("cannot find"),
            "expected a not-found error, got: {err}"
        );
        // A missing source leaves the destination byte-for-byte intact.
        assert_eq!(fs::read(&dest).unwrap(), b"old");
    }

    #[cfg(unix)]
    #[test]
    fn atomic_rename_file_within_root_rejects_symlink_dest() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp_root = canonical_tmp_root(tmp.path());
        let root = ensure_secure_dir(&tmp_root.join("secure"), PROJECT_STATE_DIR_MODE).unwrap();
        let source = root.join("index.db.rebuild-9999");
        fs::write(&source, b"new").unwrap();
        let real_target = tmp_root.join("outside.db");
        fs::write(&real_target, b"outside").unwrap();
        symlink(&real_target, root.join("index.db")).unwrap();

        let err =
            atomic_rename_file_within_root(&source, &root.join("index.db"), &root).unwrap_err();

        assert!(err.to_string().contains("symlink"));
        // The symlink target outside the root is never written through.
        assert_eq!(fs::read(&real_target).unwrap(), b"outside");
        assert!(source.exists());
    }

    #[test]
    fn remove_helpers_only_remove_regular_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = ensure_secure_dir(
            &canonical_tmp_root(tmp.path()).join("secure"),
            PROJECT_STATE_DIR_MODE,
        )
        .unwrap();

        let regular_file = root.join("file.txt");
        fs::write(&regular_file, "hello").unwrap();
        assert!(remove_regular_file(&regular_file, &root).unwrap());
        assert!(!regular_file.exists());

        let directory = root.join("directory");
        fs::create_dir_all(&directory).unwrap();
        let err = remove_regular_file(&directory, &root).unwrap_err();
        assert!(err.to_string().contains("regular file"));
    }

    #[cfg(windows)]
    #[test]
    fn ensure_secure_dir_accepts_verbatim_disk_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let verbatim_root = canonical_tmp_root(tmp.path());
        assert!(
            verbatim_root.to_string_lossy().starts_with(r"\\?\"),
            "canonicalize should produce an extended-length path: {}",
            verbatim_root.display()
        );

        let target = verbatim_root.join("secure").join("nested");
        let created = ensure_secure_dir(&target, PROJECT_STATE_DIR_MODE).unwrap();

        assert_eq!(created, target);
        assert!(target.is_dir());
    }

    #[cfg(windows)]
    #[test]
    fn validate_regular_file_path_accepts_verbatim_disk_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let root = canonical_tmp_root(tmp.path());
        let file = root.join("state.json");
        fs::write(&file, "{}").unwrap();

        let validated = validate_regular_file_path(&file, &root).unwrap();

        assert_eq!(validated, file);
    }

    #[test]
    fn ensure_secure_dir_tolerates_concurrent_creation() {
        let tmp = tempfile::tempdir().unwrap();
        let target = Arc::new(canonical_tmp_root(tmp.path()).join("secure").join("nested"));
        let callers = 16;
        let barrier = Arc::new(Barrier::new(callers));
        let handles = (0..callers)
            .map(|_| {
                let target = Arc::clone(&target);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    ensure_secure_dir(target.as_ref(), PROJECT_STATE_DIR_MODE).unwrap()
                })
            })
            .collect::<Vec<_>>();

        for handle in handles {
            assert_eq!(handle.join().unwrap(), *target);
        }
        assert!(target.is_dir());
        #[cfg(unix)]
        assert_eq!(mode_bits(target.as_ref()), PROJECT_STATE_DIR_MODE);
    }

    #[cfg(unix)]
    #[test]
    fn remove_helpers_only_remove_socket_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = ensure_secure_dir(
            &canonical_tmp_root(tmp.path()).join("secure"),
            PROJECT_STATE_DIR_MODE,
        )
        .unwrap();

        let socket_path = root.join("daemon.sock");
        let _listener = UnixListener::bind(&socket_path).unwrap();
        assert!(remove_socket_file(&socket_path, &root).unwrap());
        assert!(!socket_path.exists());
    }

    fn canonical_tmp_root(path: &Path) -> PathBuf {
        path.canonicalize().unwrap()
    }

    fn assert_no_temp_files(dir: &Path) {
        for entry in fs::read_dir(dir).unwrap() {
            let name = entry.unwrap().file_name();
            assert!(
                !name.to_string_lossy().starts_with(".1up-tmp-"),
                "leftover temp file: {}",
                name.to_string_lossy()
            );
        }
    }
}
