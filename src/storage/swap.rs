//! Build-aside index swap primitives.
//!
//! A required index rebuild must never tear down the live index in place. The
//! rebuild is instead built into a sibling staging database under the same
//! `.1up/` directory and switched over only once it is complete and valid. This
//! module holds the storage-side steps of that discipline.
//!
//! [`finalize_staged_db`] is the first step: it turns a freshly-built staging
//! database into a *single self-contained file* by folding the write-ahead log
//! into the main database and dropping every open handle. This is a correctness
//! precondition for the later atomic rename — a staged database that still
//! carried a live, header-compatible WAL would, once renamed over the served
//! index, leave an orphan WAL that SQLite replays silently and that even
//! `PRAGMA integrity_check` reports as "ok" (HYP-001).

use std::path::Path;
use std::thread;
use std::time::Duration;

use crate::shared::constants::{DB_LOCK_RETRY_ATTEMPTS, DB_LOCK_RETRY_DELAY_MS};
use crate::shared::errors::{OneupError, StorageError};
use crate::storage::db::{is_lock_error, Db};
use crate::storage::queries;

/// Fold a freshly-built staging database into a single self-contained file.
///
/// Runs `PRAGMA wal_checkpoint(TRUNCATE)` so every committed WAL frame is folded
/// into the main staging file and the write-ahead log is truncated to zero, then
/// consumes and drops the [`Db`] handle so SQLite removes the now-empty
/// `-wal`/`-shm` sidecars on the final connection close. After this returns the
/// staging file is a single self-contained artifact safe to atomically rename
/// over the served `index.db`.
///
/// The caller MUST have dropped every other connection to the staging database
/// before calling this: a `wal_checkpoint(TRUNCATE)` cannot truncate the WAL
/// while another connection holds a read lock, so a lingering handle surfaces as
/// a non-zero `busy` result and is reported as an error rather than silently
/// leaving a live WAL behind.
///
/// `staging_path` is used only to frame error messages and to verify the
/// post-checkpoint on-disk state; the database is operated through `db`.
// Consumed by the atomic switch-over primitive (T2) and the rebuild owners
// (T4/T5); reserved ahead of those callers per the build-aside DAG.
#[allow(dead_code)]
pub async fn finalize_staged_db(db: Db, staging_path: &Path) -> Result<(), OneupError> {
    let conn = db.connect()?;
    checkpoint_truncate(&conn, staging_path).await?;

    // Drop every handle so SQLite's final-close path removes the (now empty)
    // WAL/SHM sidecars. Explicit drops make the "all handles released before
    // return" guarantee literal rather than relying on end-of-scope order.
    drop(conn);
    drop(db);

    ensure_no_live_wal(staging_path)?;

    Ok(())
}

/// Run `wal_checkpoint(TRUNCATE)` until it completes (`busy == 0`), bounded by the
/// shared DB-lock retry budget. A non-zero `busy` means the WAL could not be
/// truncated because another connection held a lock; that is treated the same as
/// a transient `database is locked` and retried, then failed loud if it persists.
#[allow(dead_code)]
async fn checkpoint_truncate(
    conn: &libsql::Connection,
    staging_path: &Path,
) -> Result<(), OneupError> {
    let retry_delay = Duration::from_millis(DB_LOCK_RETRY_DELAY_MS);
    let mut last_blocker: Option<String> = None;

    for attempt in 0..DB_LOCK_RETRY_ATTEMPTS {
        match checkpoint_truncate_once(conn).await {
            Ok(true) => return Ok(()),
            Ok(false) => {
                last_blocker = Some(
                    "checkpoint reported busy (a connection still holds the staging database)"
                        .to_string(),
                );
            }
            Err(err) => {
                let err_text = err.to_string();
                if !is_lock_error(&err_text) {
                    return Err(StorageError::Query(format!(
                        "failed to checkpoint staging database {}: {err_text}",
                        staging_path.display()
                    ))
                    .into());
                }
                last_blocker = Some(err_text);
            }
        }

        if attempt + 1 < DB_LOCK_RETRY_ATTEMPTS {
            thread::sleep(retry_delay);
        }
    }

    Err(StorageError::Query(format!(
        "failed to finalize staging database {}: {}",
        staging_path.display(),
        last_blocker.unwrap_or_else(|| "checkpoint retry exhausted".to_string())
    ))
    .into())
}

/// Execute a single `wal_checkpoint(TRUNCATE)` and report whether it fully
/// completed. Returns `Ok(true)` when the checkpoint truncated the WAL
/// (`busy == 0`), `Ok(false)` when it was blocked (`busy != 0`), and `Err` for a
/// query failure (e.g. a transient `database is locked`).
#[allow(dead_code)]
async fn checkpoint_truncate_once(conn: &libsql::Connection) -> Result<bool, libsql::Error> {
    let mut rows = conn.query(queries::WAL_CHECKPOINT_TRUNCATE, ()).await?;
    match rows.next().await? {
        // Column 0 is `busy`: 0 means the WAL was checkpointed and truncated.
        // A database that is not in WAL mode reports `(0, -1, -1)`, which is
        // also self-contained, so `busy == 0` is the correct success signal.
        Some(row) => Ok(row.get::<i64>(0)? == 0),
        // No result row is unexpected for this PRAGMA; treat as not-completed so
        // the caller surfaces it as a finalize failure rather than a false pass.
        None => Ok(false),
    }
}

/// Verify the staging database carries no live write-ahead log after finalize.
///
/// A `wal_checkpoint(TRUNCATE)` followed by dropping all handles should leave the
/// `-wal` sidecar either absent or zero-length. A non-empty `-wal` is the exact
/// HYP-001 hazard (a header-compatible orphan WAL the rename would carry over and
/// SQLite would silently replay), so it is rejected here rather than swapped.
#[allow(dead_code)]
fn ensure_no_live_wal(staging_path: &Path) -> Result<(), OneupError> {
    let wal_path = wal_sidecar_path(staging_path);
    match std::fs::metadata(&wal_path) {
        Ok(meta) if meta.len() > 0 => Err(StorageError::Query(format!(
            "staging database {} still has a live write-ahead log ({} bytes) after finalize",
            staging_path.display(),
            meta.len()
        ))
        .into()),
        // Absent or empty WAL: self-contained as required.
        Ok(_) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(StorageError::Query(format!(
            "failed to inspect write-ahead log for staging database {}: {err}",
            staging_path.display()
        ))
        .into()),
    }
}

/// Sidecar `-wal` path for a database file (`<name>` -> `<name>-wal`).
#[allow(dead_code)]
fn wal_sidecar_path(db_path: &Path) -> std::path::PathBuf {
    let mut name = db_path.as_os_str().to_os_string();
    name.push("-wal");
    std::path::PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::shared::config;
    use crate::storage::schema;

    /// Insert a minimal segment row, mirroring the NOT-NULL columns used by the
    /// schema tests, so the write lands in the WAL ahead of finalize.
    async fn insert_segment(conn: &libsql::Connection, id: &str) {
        conn.execute(
            "INSERT INTO segments (id, file_path, language, block_type, content, line_start, line_end, complexity, file_hash) \
             VALUES (?1, 'f.rs', 'rust', 'function', 'fn f(){}', 1, 1, 0, 'abc')",
            [id],
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn finalize_staged_db_produces_self_contained_file() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().canonicalize().unwrap().join("project");
        std::fs::create_dir_all(&project_root).unwrap();
        let staging_path = config::project_db_path(&project_root);

        // Build a staging database with committed rows held in the WAL.
        let db = Db::open_rw(&staging_path).await.unwrap();
        let conn = db.connect_tuned().await.unwrap();
        schema::initialize(&conn).await.unwrap();
        insert_segment(&conn, "s1").await;
        insert_segment(&conn, "s2").await;
        // The caller must release its own connections before finalize; finalize
        // owns and drops the `Db`.
        drop(conn);

        finalize_staged_db(db, &staging_path).await.unwrap();

        // AC: no live `-wal` sidecar remains (absent or zero-length).
        let wal_path = wal_sidecar_path(&staging_path);
        match std::fs::metadata(&wal_path) {
            Ok(meta) => assert_eq!(
                meta.len(),
                0,
                "finalize must leave no live WAL; found {} bytes",
                meta.len()
            ),
            Err(err) => assert_eq!(err.kind(), std::io::ErrorKind::NotFound),
        }

        // AC: the file opens read-only, passes integrity_check, and the committed
        // rows survived the WAL fold (proving the checkpoint folded, not lost).
        let ro = Db::open_ro(&staging_path).await.unwrap();
        let conn = ro.connect().unwrap();

        let mut rows = conn.query("PRAGMA integrity_check", ()).await.unwrap();
        let status: String = rows.next().await.unwrap().unwrap().get(0).unwrap();
        assert_eq!(
            status, "ok",
            "finalized staging DB must pass integrity_check"
        );

        let mut rows = conn.query(queries::COUNT_SEGMENTS, ()).await.unwrap();
        let count: i64 = rows.next().await.unwrap().unwrap().get(0).unwrap();
        assert_eq!(count, 2, "committed rows must survive finalize");
    }
}
