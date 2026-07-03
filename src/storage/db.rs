use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use libsql::{Builder, Connection, Database};

use crate::shared::constants::{
    DB_LOCK_RETRY_ATTEMPTS, DB_LOCK_RETRY_DELAY_MS, STAGING_DB_CACHE_SIZE_KIB,
    STAGING_WAL_AUTOCHECKPOINT_PAGES,
};
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

    /// Create a new connection and apply the read/base performance PRAGMA
    /// profile (reads and incremental writes).
    pub async fn connect_tuned(&self) -> Result<Connection, OneupError> {
        let conn = self.connect()?;
        apply_project_pragmas(&conn).await?;
        Ok(conn)
    }

    /// Create a new connection and apply the write/staging PRAGMA profile.
    ///
    /// Used by the cold full-rebuild staging connection ([`StagingRebuild`]):
    /// it raises `cache_size` and `wal_autocheckpoint` over the read/base
    /// profile to cut mid-rebuild checkpoint churn and keep more index pages
    /// hot during a large rebuild. Every load-bearing base setting (including
    /// the implicit `recursive_triggers=OFF`) is preserved.
    ///
    /// [`StagingRebuild`]: crate::storage::swap::StagingRebuild
    pub async fn connect_tuned_staging(&self) -> Result<Connection, OneupError> {
        let conn = self.connect()?;
        apply_pragma_profile(&conn, PragmaProfile::WriteStaging).await?;
        Ok(conn)
    }
}

/// Connection PRAGMA profile.
///
/// Both profiles share the load-bearing tuned base; the write/staging profile
/// additionally raises `cache_size` and `wal_autocheckpoint` for large cold
/// rebuilds. `recursive_triggers` is intentionally absent from both so SQLite's
/// default (OFF) holds — the content-addressed `segments_vector_ad` AFTER DELETE
/// trigger must never recursively re-fire (it would double-decrement
/// `embedding_pool.ref_count`). DO NOT add `recursive_triggers` to either profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PragmaProfile {
    /// Tuned baseline for reads and incremental writes (the historical single
    /// profile, unchanged).
    ReadBase,
    /// Cold full-rebuild staging connection: the base plus raised
    /// checkpoint/cache headroom.
    WriteStaging,
}

/// The tuned baseline PRAGMA batch shared by both connection profiles.
///
/// `recursive_triggers` is deliberately omitted so SQLite's default (OFF) holds;
/// see [`PragmaProfile`].
const READ_BASE_PRAGMAS: &str = "PRAGMA busy_timeout=5000;\
     PRAGMA journal_mode=WAL;\
     PRAGMA synchronous=NORMAL;\
     PRAGMA cache_size=-32768;\
     PRAGMA mmap_size=268435456;\
     PRAGMA temp_store=MEMORY;";

/// Build the PRAGMA batch for `profile`.
///
/// The write/staging profile appends its overrides *after* the shared base, so
/// every base setting is still applied and only `cache_size` is overridden (the
/// last write in a batch wins) while `wal_autocheckpoint` is added. This keeps
/// the staging profile a strict superset of the base — no base setting can be
/// silently dropped — and leaves the read/base profile byte-identical.
fn pragma_batch(profile: PragmaProfile) -> String {
    match profile {
        PragmaProfile::ReadBase => READ_BASE_PRAGMAS.to_string(),
        PragmaProfile::WriteStaging => format!(
            "{READ_BASE_PRAGMAS}\
             PRAGMA cache_size={cache_size};\
             PRAGMA wal_autocheckpoint={wal_autocheckpoint};",
            cache_size = STAGING_DB_CACHE_SIZE_KIB,
            wal_autocheckpoint = STAGING_WAL_AUTOCHECKPOINT_PAGES,
        ),
    }
}

/// Apply the read/base performance PRAGMA profile to a project-local libSQL
/// connection. Retained as the entry point for the read/incremental-write tuned
/// callers; the write/staging profile is applied via [`Db::connect_tuned_staging`].
pub async fn apply_project_pragmas(conn: &Connection) -> Result<(), OneupError> {
    apply_pragma_profile(conn, PragmaProfile::ReadBase).await
}

/// Apply `profile`'s PRAGMA batch to `conn`, retrying transient lock failures.
///
/// Uses `execute_batch` because `PRAGMA journal_mode=WAL` returns a result row
/// and libSQL's `execute()` rejects statements that produce rows.
async fn apply_pragma_profile(conn: &Connection, profile: PragmaProfile) -> Result<(), OneupError> {
    let batch = pragma_batch(profile);
    let retry_delay = Duration::from_millis(DB_LOCK_RETRY_DELAY_MS);
    let mut last_error = None;

    for attempt in 0..DB_LOCK_RETRY_ATTEMPTS {
        match conn.execute_batch(&batch).await {
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
/// Sourced from [`crate::shared::config::STAGING_INDEX_DB_PREFIX`] so the
/// path-building and validation sides cannot drift.
const STAGING_INDEX_PREFIX: &str = crate::shared::config::STAGING_INDEX_DB_PREFIX;

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

    /// Pins the load-bearing invariant that `recursive_triggers` stays OFF on
    /// project connections. The content-addressed `segments_vector_ad` trigger's
    /// inner `DELETE FROM segment_vectors` must NOT recursively re-fire, or it
    /// would double-decrement `embedding_pool.ref_count`. `apply_project_pragmas`
    /// must never enable it, and SQLite's default is OFF.
    #[tokio::test]
    async fn project_connections_keep_recursive_triggers_off() {
        async fn recursive_triggers(conn: &Connection) -> i64 {
            let mut rows = conn.query("PRAGMA recursive_triggers", ()).await.unwrap();
            rows.next().await.unwrap().unwrap().get(0).unwrap()
        }

        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().canonicalize().unwrap().join("project");
        fs::create_dir_all(&project_root).unwrap();
        let db = Db::open_rw(&config::project_db_path(&project_root))
            .await
            .unwrap();

        assert_eq!(
            recursive_triggers(&db.connect().unwrap()).await,
            0,
            "plain project connection must keep recursive_triggers OFF (segments_vector_ad must not re-fire)"
        );
        assert_eq!(
            recursive_triggers(&db.connect_tuned().await.unwrap()).await,
            0,
            "tuned project connection must keep recursive_triggers OFF"
        );

        // The write/staging profile (raised cache_size + wal_autocheckpoint) must
        // also leave recursive_triggers OFF — it only appends those two overrides
        // to the shared base and must not enable the recursive trigger re-fire.
        let staging = Db::open_staging_rw(&config::project_staging_db_path(&project_root))
            .await
            .unwrap();
        assert_eq!(
            recursive_triggers(&staging.connect_tuned_staging().await.unwrap()).await,
            0,
            "write/staging tuned connection must keep recursive_triggers OFF"
        );
    }

    /// Pins T9's read-vs-write/staging PRAGMA profile split: the read/base
    /// profile (`connect_tuned`) keeps the historical tuned `cache_size` and
    /// SQLite's default `wal_autocheckpoint`, while the write/staging profile
    /// (`connect_tuned_staging`) raises both. Fails if the read profile drifts
    /// from its load-bearing baseline or the staging profile fails to raise.
    #[tokio::test]
    async fn read_and_write_staging_pragma_profiles_differ_as_specified() {
        async fn pragma_i64(conn: &Connection, pragma: &str) -> i64 {
            let mut rows = conn.query(&format!("PRAGMA {pragma}"), ()).await.unwrap();
            rows.next().await.unwrap().unwrap().get(0).unwrap()
        }

        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().canonicalize().unwrap().join("project");
        fs::create_dir_all(&project_root).unwrap();

        // Read/base profile: the historical tuned baseline — DO NOT change.
        let read_db = Db::open_rw(&config::project_db_path(&project_root))
            .await
            .unwrap();
        let read = read_db.connect_tuned().await.unwrap();
        assert_eq!(
            pragma_i64(&read, "cache_size").await,
            -32768,
            "read/base cache_size must stay at the tuned 32 MiB baseline"
        );
        assert_eq!(
            pragma_i64(&read, "wal_autocheckpoint").await,
            1000,
            "read/base profile must keep SQLite's default wal_autocheckpoint"
        );

        // Write/staging profile: raised checkpoint + cache headroom for cold rebuilds.
        let staging_db = Db::open_staging_rw(&config::project_staging_db_path(&project_root))
            .await
            .unwrap();
        let staging = staging_db.connect_tuned_staging().await.unwrap();
        assert_eq!(
            pragma_i64(&staging, "cache_size").await,
            STAGING_DB_CACHE_SIZE_KIB as i64,
            "write/staging profile must raise cache_size to 128 MiB"
        );
        assert_eq!(
            pragma_i64(&staging, "wal_autocheckpoint").await,
            STAGING_WAL_AUTOCHECKPOINT_PAGES as i64,
            "write/staging profile must raise wal_autocheckpoint"
        );
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
