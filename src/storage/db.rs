use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use libsql::{Builder, Connection, Database};

use crate::shared::constants::{DB_LOCK_RETRY_ATTEMPTS, DB_LOCK_RETRY_DELAY_MS};
use crate::shared::errors::{OneupError, StorageError};
use crate::shared::fs::{ensure_secure_project_root, validate_regular_file_path};

/// A wrapper around a libsql database that manages connections.
pub struct Db {
    database: Database,
}

impl Db {
    /// Open a local database at the given path in read-write mode,
    /// creating the file and parent directories if they do not exist.
    pub async fn open_rw(path: &Path) -> Result<Self, OneupError> {
        let path = validate_project_db_path_for_write(path)?;
        Self::open_local_rw(&path).await
    }

    /// Open a build-aside staging database in read-write mode, creating the file
    /// and the secure `.1up` directory if they do not exist.
    ///
    /// `path` MUST be a `<project>/.1up/index.db.rebuild-<uuid>` staging sibling
    /// (see [`config::project_staging_db_path`]). A non-destructive rebuild builds
    /// the refreshed index into this staging file and atomically switches it over
    /// the served `index.db` once finalized. [`Db::open_rw`] deliberately accepts
    /// only the served `index.db`, so the rebuild owner opens the staging file
    /// through this dedicated constructor; the staging leaf is clamped to the
    /// `.1up` state root and a symlink leaf is rejected, mirroring `open_rw`'s gate.
    pub async fn open_staging_rw(path: &Path) -> Result<Self, OneupError> {
        let path = validate_staging_db_path_for_write(path)?;
        Self::open_local_rw(&path).await
    }

    /// Open a validated local database path in read-write mode. Shared by
    /// [`Db::open_rw`] and [`Db::open_staging_rw`]; the caller has already clamped
    /// `path` to its project's `.1up` state root.
    async fn open_local_rw(path: &Path) -> Result<Self, OneupError> {
        let path_str = path.to_str().ok_or_else(|| {
            StorageError::Connection(format!(
                "database path is not valid UTF-8: {}",
                path.display()
            ))
        })?;

        let database = build_local_with_retry(path_str).await?;

        Ok(Self { database })
    }

    /// Open a local database at the given path in read-only mode.
    /// The database file must already exist.
    pub async fn open_ro(path: &Path) -> Result<Self, OneupError> {
        let path = validate_existing_project_db_path(path)?;
        if !path.exists() {
            return Err(StorageError::Connection(format!(
                "database file not found: {}",
                path.display()
            ))
            .into());
        }

        let path_str = path.to_str().ok_or_else(|| {
            StorageError::Connection(format!(
                "database path is not valid UTF-8: {}",
                path.display()
            ))
        })?;

        let database = build_local_with_retry(path_str).await?;

        Ok(Self { database })
    }

    /// Open an in-memory database (useful for tests).
    #[allow(dead_code)]
    pub async fn open_memory() -> Result<Self, OneupError> {
        let database = Builder::new_local(":memory:")
            .build()
            .await
            .map_err(|e| StorageError::Connection(e.to_string()))?;

        Ok(Self { database })
    }

    /// Create a new connection from this database handle.
    pub fn connect(&self) -> Result<Connection, OneupError> {
        self.database
            .connect()
            .map_err(|e| StorageError::Connection(e.to_string()).into())
    }

    /// Create a new connection and apply project-local performance PRAGMAs.
    pub async fn connect_tuned(&self) -> Result<Connection, OneupError> {
        let conn = self.connect()?;
        apply_project_pragmas(&conn).await?;
        Ok(conn)
    }
}

/// Apply performance-tuned PRAGMAs to a project-local libSQL connection.
///
/// These settings optimize the local write-heavy indexing workload without
/// changing user-visible behavior or introducing new flags.
///
/// Uses `execute_batch` because `PRAGMA journal_mode=WAL` returns a result
/// row and libSQL's `execute()` rejects statements that produce rows.
pub async fn apply_project_pragmas(conn: &Connection) -> Result<(), OneupError> {
    let retry_delay = Duration::from_millis(DB_LOCK_RETRY_DELAY_MS);
    let mut last_error = None;

    for attempt in 0..DB_LOCK_RETRY_ATTEMPTS {
        match conn
            .execute_batch(
                "PRAGMA busy_timeout=5000;
                 PRAGMA journal_mode=WAL;
                 PRAGMA synchronous=NORMAL;
                 PRAGMA cache_size=-32768;
                 PRAGMA mmap_size=268435456;
                 PRAGMA temp_store=MEMORY;",
            )
            .await
        {
            Ok(_) => return Ok(()),
            Err(err) => {
                let err_text = err.to_string();
                if !is_lock_error(&err_text) || attempt + 1 == DB_LOCK_RETRY_ATTEMPTS {
                    return Err(StorageError::Connection(format!(
                        "failed to apply project PRAGMAs: {err_text}"
                    ))
                    .into());
                }
                last_error = Some(err_text);
                thread::sleep(retry_delay);
            }
        }
    }

    Err(StorageError::Connection(format!(
        "failed to apply project PRAGMAs: {}",
        last_error.unwrap_or_else(|| "database lock retry exhausted".to_string())
    ))
    .into())
}

/// The fixed served-index filename within a project's `.1up` directory.
const SERVED_INDEX_FILENAME: &str = "index.db";
/// Prefix shared by build-aside staging filenames (`index.db.rebuild-<uuid>`).
const STAGING_INDEX_PREFIX: &str = "index.db.rebuild-";

fn validate_project_db_path_for_write(path: &Path) -> Result<PathBuf, OneupError> {
    validate_db_path_for_write(path, project_root_from_db_path(path)?)
}

fn validate_staging_db_path_for_write(path: &Path) -> Result<PathBuf, OneupError> {
    validate_db_path_for_write(path, project_root_from_staging_path(path)?)
}

/// Prepare the secure `.1up` state directory and clamp a writable database leaf
/// to it. Shared by the served-index and staging-file write gates; the leaf-name
/// validation that produced `project_root` already distinguishes the two.
fn validate_db_path_for_write(path: &Path, project_root: &Path) -> Result<PathBuf, OneupError> {
    let secure_root = ensure_secure_project_root(project_root).map_err(|err| {
        StorageError::Connection(format!(
            "failed to prepare project state directory for {}: {err}",
            path.display()
        ))
    })?;
    validate_regular_file_path(path, &secure_root).map_err(|err| {
        StorageError::Connection(format!(
            "failed to validate database path {}: {err}",
            path.display()
        ))
        .into()
    })
}

fn validate_existing_project_db_path(path: &Path) -> Result<PathBuf, OneupError> {
    let project_root = project_root_from_db_path(path)?;
    validate_regular_file_path(path, project_root).map_err(|err| {
        StorageError::Connection(format!(
            "failed to validate database path {}: {err}",
            path.display()
        ))
        .into()
    })
}

fn project_root_from_db_path(path: &Path) -> Result<&Path, OneupError> {
    project_root_for_dot_dir_child(
        path,
        |leaf| leaf == SERVED_INDEX_FILENAME,
        "<project>/.1up/index.db",
    )
}

fn project_root_from_staging_path(path: &Path) -> Result<&Path, OneupError> {
    project_root_for_dot_dir_child(
        path,
        |leaf| leaf.starts_with(STAGING_INDEX_PREFIX),
        "<project>/.1up/index.db.rebuild-<uuid>",
    )
}

/// Derive the project root from a `<project>/.1up/<leaf>` database path, requiring
/// the leaf filename to satisfy `leaf_ok`. `target_desc` names the accepted layout
/// in the rejection error so a misrouted path reports what it should have been.
fn project_root_for_dot_dir_child<'a>(
    path: &'a Path,
    leaf_ok: impl Fn(&str) -> bool,
    target_desc: &str,
) -> Result<&'a Path, OneupError> {
    let leaf_valid = path
        .file_name()
        .and_then(OsStr::to_str)
        .is_some_and(leaf_ok);
    if !leaf_valid {
        return Err(StorageError::Connection(format!(
            "database path must target {target_desc}: {}",
            path.display()
        ))
        .into());
    }

    let dot_dir = path.parent().ok_or_else(|| {
        StorageError::Connection(format!(
            "database path is missing its .1up parent directory: {}",
            path.display()
        ))
    })?;
    if dot_dir.file_name() != Some(OsStr::new(".1up")) {
        return Err(StorageError::Connection(format!(
            "database path must target {target_desc}: {}",
            path.display()
        ))
        .into());
    }

    dot_dir.parent().ok_or_else(|| {
        StorageError::Connection(format!(
            "database path is missing its project root: {}",
            path.display()
        ))
        .into()
    })
}

async fn build_local_with_retry(path_str: &str) -> Result<Database, OneupError> {
    let retry_delay = Duration::from_millis(DB_LOCK_RETRY_DELAY_MS);
    let mut last_error = None;

    for attempt in 0..DB_LOCK_RETRY_ATTEMPTS {
        match Builder::new_local(path_str).build().await {
            Ok(database) => return Ok(database),
            Err(err) => {
                let err_text = err.to_string();
                if !is_lock_error(&err_text) || attempt + 1 == DB_LOCK_RETRY_ATTEMPTS {
                    return Err(StorageError::Connection(err_text).into());
                }
                last_error = Some(err_text);
                thread::sleep(retry_delay);
            }
        }
    }

    Err(
        StorageError::Connection(last_error.unwrap_or_else(|| "database open failed".to_string()))
            .into(),
    )
}

pub(crate) fn is_lock_error(error: &str) -> bool {
    let lower = error.to_lowercase();
    lower.contains("database is locked")
        || lower.contains("locking error")
        || lower.contains("failed locking file")
        || lower.contains("locked by another process")
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

    use crate::shared::config;
    use crate::shared::constants::PROJECT_STATE_DIR_MODE;

    #[tokio::test]
    async fn open_rw_creates_secure_project_state_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().canonicalize().unwrap().join("project");
        fs::create_dir_all(&project_root).unwrap();
        let db_path = config::project_db_path(&project_root);

        let db = Db::open_rw(&db_path).await.unwrap();
        db.connect().unwrap();

        assert!(db_path.exists());
        #[cfg(unix)]
        {
            let dot_dir = config::project_dot_dir(&project_root);
            assert_eq!(mode_bits(&dot_dir), PROJECT_STATE_DIR_MODE);
        }
    }

    #[tokio::test]
    async fn open_rw_rejects_non_project_db_layouts() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().canonicalize().unwrap().join("project");
        fs::create_dir_all(&project_root).unwrap();
        let invalid_path = project_root.join("index.db");

        let err = Db::open_rw(&invalid_path).await.err().unwrap();
        assert!(err.to_string().contains("<project>/.1up/index.db"));
    }

    #[tokio::test]
    async fn open_staging_rw_creates_uuid_suffixed_staging_file() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().canonicalize().unwrap().join("project");
        fs::create_dir_all(&project_root).unwrap();
        let staging_path = config::project_staging_db_path(&project_root);

        let db = Db::open_staging_rw(&staging_path).await.unwrap();
        db.connect().unwrap();

        assert!(staging_path.exists());
        assert!(staging_path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("index.db.rebuild-"));
        #[cfg(unix)]
        {
            let dot_dir = config::project_dot_dir(&project_root);
            assert_eq!(mode_bits(&dot_dir), PROJECT_STATE_DIR_MODE);
        }
    }

    #[tokio::test]
    async fn open_staging_rw_rejects_non_staging_leaf() {
        // The staging gate must not become a back door for opening the served
        // index (or any other file) read-write: only `index.db.rebuild-*` siblings
        // inside `.1up` are accepted.
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().canonicalize().unwrap().join("project");
        fs::create_dir_all(config::project_dot_dir(&project_root)).unwrap();

        let served = config::project_db_path(&project_root);
        let err = Db::open_staging_rw(&served).await.err().unwrap();
        assert!(err
            .to_string()
            .contains("<project>/.1up/index.db.rebuild-<uuid>"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn open_ro_rejects_symlinked_database_leaf() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp_root = tmp.path().canonicalize().unwrap();
        let project_root = tmp_root.join("project");
        let outside_root = tmp_root.join("outside");
        fs::create_dir_all(config::project_dot_dir(&project_root)).unwrap();
        fs::create_dir_all(&outside_root).unwrap();
        fs::write(outside_root.join("index.db"), b"not-a-real-db").unwrap();
        symlink(
            outside_root.join("index.db"),
            config::project_db_path(&project_root),
        )
        .unwrap();

        let err = Db::open_ro(&config::project_db_path(&project_root))
            .await
            .err()
            .unwrap();
        assert!(err.to_string().contains("symlink"));
    }
}
