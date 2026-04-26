use std::ffi::OsStr;
use std::path::Path;
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
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;
         PRAGMA cache_size=-32768;
         PRAGMA mmap_size=268435456;
         PRAGMA temp_store=MEMORY;",
    )
    .await
    .map_err(|e| StorageError::Connection(format!("failed to apply project PRAGMAs: {e}")))?;
    Ok(())
}

fn validate_project_db_path_for_write(path: &Path) -> Result<std::path::PathBuf, OneupError> {
    let project_root = project_root_from_db_path(path)?;
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

fn validate_existing_project_db_path(path: &Path) -> Result<std::path::PathBuf, OneupError> {
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
    if path.file_name() != Some(OsStr::new("index.db")) {
        return Err(StorageError::Connection(format!(
            "database path must target <project>/.1up/index.db: {}",
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
            "database path must target <project>/.1up/index.db: {}",
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

        let dot_dir = config::project_dot_dir(&project_root);
        assert!(db_path.exists());
        #[cfg(unix)]
        assert_eq!(mode_bits(&dot_dir), PROJECT_STATE_DIR_MODE);
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
