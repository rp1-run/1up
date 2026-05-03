#![allow(dead_code)]

#[cfg(any(unix, test))]
use std::fs::File;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Component, Path, PathBuf};

use uuid::Uuid;

use crate::shared::config;
use crate::shared::constants::{PROJECT_STATE_DIR_MODE, XDG_STATE_DIR_MODE};
use crate::shared::errors::{FilesystemError, OneupError};

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

#[cfg(unix)]
fn mode_bits(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;

    fs::metadata(path).unwrap().permissions().mode() & 0o777
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
    use std::sync::{Arc, Barrier, Mutex};
    use std::thread;

    use crate::shared::constants::SECURE_STATE_FILE_MODE;

    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    #[cfg(unix)]
    use std::os::unix::net::UnixListener;

    static ENV_MUTEX: Mutex<()> = Mutex::new(());

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
}
