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

use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use libsql::Connection;

use crate::shared::config;
use crate::shared::constants::{DB_LOCK_RETRY_ATTEMPTS, DB_LOCK_RETRY_DELAY_MS};
use crate::shared::errors::{OneupError, StorageError};
use crate::storage::db::{is_lock_error, Db};
use crate::storage::{queries, schema};

/// RAII guard owning a build-aside staging database for a non-destructive rebuild.
///
/// The refreshed index is built through [`Self::connection`] into a uuid-suffixed
/// staging file under `.1up/`, then [`Self::finalize_and_swap`] folds it into a
/// single self-contained file and atomically switches it over the served
/// `index.db`. The served index is never torn down before that switch, so a reader
/// keeps seeing the full prior index throughout the rebuild and a single rename
/// flips it to the full new index — and a rebuild that fails, is cancelled, or
/// panics before the switch leaves the prior `index.db` intact and served
/// (REQ-001 AC3).
///
/// On drop the staging file is best-effort removed so an aborted rebuild leaves no
/// orphan behind; after a successful switch the file was already renamed away, so
/// the removal is a harmless no-op. [`Self::finalize_and_swap`] must run while the
/// single-writer `RebuildLock` is held.
pub struct StagingRebuild {
    state_root: PathBuf,
    staging_path: PathBuf,
    // Held in `Option`s so `finalize_and_swap` can release the build connection
    // and move the `Db` into the finalize step (which consumes it) while `Drop`
    // still runs the staging-file cleanup on every exit path.
    db: Option<Db>,
    conn: Option<Connection>,
}

impl StagingRebuild {
    /// Open and initialize a fresh staging database for `state_root`.
    ///
    /// Sites the staging file at a uuid-suffixed path under `.1up/` and creates the
    /// current schema in it. The file is brand-new, so this uses
    /// [`schema::initialize`] rather than a destructive in-place drop-and-recreate
    /// — nothing is ever dropped from the served index.
    pub async fn open(state_root: &Path) -> Result<Self, OneupError> {
        let staging_path = config::project_staging_db_path(state_root);
        let db = Db::open_staging_rw(&staging_path).await?;
        // Cold full rebuilds write the entire refreshed index through this one
        // connection, so it takes the write/staging PRAGMA profile (raised
        // cache_size + wal_autocheckpoint) to cut mid-rebuild checkpoint churn.
        let conn = db.connect_tuned_staging().await?;
        // Defer the `idx_embedding_pool_embedding` DiskANN build: on a cold full
        // rebuild every pool row is known up front, so building the graph once after
        // the pool is loaded (in `finalize_and_swap`) is far cheaper than the
        // incremental per-insert maintenance the index does when it already exists
        // (R-006). The staging schema is therefore intentionally incomplete until the
        // deferred build runs; the served `index.db` only ever appears via the swap,
        // which happens strictly after that build.
        schema::initialize_with_vector_index(&conn, schema::VectorIndexBuild::Deferred).await?;
        Ok(Self {
            state_root: state_root.to_path_buf(),
            staging_path,
            db: Some(db),
            conn: Some(conn),
        })
    }

    /// The tuned connection to the staging database; the rebuild pipeline writes
    /// the refreshed index through this.
    pub fn connection(&self) -> &Connection {
        self.conn
            .as_ref()
            .expect("staging connection is live until finalize_and_swap consumes the guard")
    }

    /// Finalize the staged build and atomically switch it over the served
    /// `index.db`.
    ///
    /// Releases the build connection first — a `wal_checkpoint(TRUNCATE)` cannot
    /// truncate the WAL while another connection is open (see
    /// [`finalize_staged_db`]) — then folds the staging WAL into a single file and
    /// performs the atomic switch-over ([`swap_index_into_place`]). MUST run while
    /// the single-writer `RebuildLock` is held. On a finalize or swap failure the
    /// prior `index.db` is left intact and the staging file is cleaned up (by the
    /// swap on a swap failure, otherwise by this guard's `Drop`).
    pub async fn finalize_and_swap(mut self) -> Result<(), OneupError> {
        // Build the deferred DiskANN vector index now that the pool is fully loaded,
        // before releasing the connection (R-006). This is the after-pool-load,
        // before-finalize hook: every `embedding_pool` row inserted by the rebuild
        // pipeline is present, so the graph is built once over the full pool instead
        // of being maintained per insert. It completes the deferred schema so the
        // finalized file passes `ensure_current`.
        {
            let conn = self
                .conn
                .as_ref()
                .expect("staging connection is live until finalize_and_swap consumes the guard");
            schema::build_embedding_pool_vector_index(conn).await?;
        }

        // Release the build connection so the finalize checkpoint can truncate the
        // staging WAL into a single self-contained file.
        self.conn = None;
        let db = self
            .db
            .take()
            .expect("staging db is live until finalize_and_swap consumes the guard");
        finalize_staged_db(db, &self.staging_path).await?;
        swap_index_into_place(&self.state_root, &self.staging_path).await
    }
}

impl Drop for StagingRebuild {
    fn drop(&mut self) {
        // Release any live handles, then best-effort remove the staging file so an
        // aborted rebuild (build error, cancellation, or panic before the switch)
        // leaves no orphan. After a successful switch the file was renamed away, so
        // this is a no-op. The staging path is never the served `index.db`, so this
        // can never disturb the prior served index.
        self.conn = None;
        self.db = None;
        let _ = crate::shared::fs::remove_regular_file(
            &self.staging_path,
            &config::project_dot_dir(&self.state_root),
        );
    }
}

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

/// Atomically switch a finalized staging database over the served `index.db`.
///
/// This is the storage half of the build-aside rebuild: the caller has already
/// built the refreshed index into `staging_path` and finalized it (see
/// [`finalize_staged_db`]) so it is a single self-contained file. This function
/// then retires any sidecars belonging to the prior `index.db` and atomically
/// renames the staging file over `index.db`, so an observer inspecting the served
/// index at any instant sees either the full prior index or the full new index —
/// never an absent, empty, or partially-built one.
///
/// # Sidecar retirement is a correctness invariant, not cleanup
///
/// The prior `index.db` may carry a header-compatible `-wal`/`-shm` pair. If such
/// an orphan WAL were left next to the freshly-renamed `index.db`, SQLite would
/// silently replay it and serve stale data that even `PRAGMA integrity_check`
/// reports as "ok" (HYP-001). The prior sidecars are therefore retired *before*
/// the rename — folding the prior WAL into the prior `index.db` and dropping every
/// handle so SQLite removes the now-empty sidecars (SQLite's sanctioned
/// open-then-immediately-close idiom, reusing [`finalize_staged_db`]), never a raw
/// `unlink`. Retiring before the rename is what guarantees the retirement strictly
/// precedes any reader opening the new inode: when the new inode appears there is
/// no orphan WAL left to replay. The prior `index.db` stays a complete, queryable
/// index between the retire and the rename, so a concurrent reader keeps seeing the
/// full prior index until the single atomic rename flips it to the full new index.
///
/// # Preconditions and failure behavior
///
/// MUST be called while the single-writer `RebuildLock` is held (and, for the
/// daemon, while its long-lived handle is quiesced — HYP-002) so no other writer
/// races the switch; this is debug-asserted on platforms with a real advisory
/// lock. On any failure before the rename the prior `index.db` is left intact and
/// queryable and the staging file is best-effort removed (mirroring
/// `atomic_replace`'s temp-file cleanup). On a cold start (no prior `index.db`)
/// the rename simply creates it.
pub async fn swap_index_into_place(
    state_root: &Path,
    staging_path: &Path,
) -> Result<(), OneupError> {
    let approved_root = config::project_dot_dir(state_root);
    let index_path = config::project_db_path(state_root);

    #[cfg(all(debug_assertions, unix))]
    {
        // Defense-in-depth precondition check: a non-blocking re-acquire of the
        // rebuild lock is denied while this process already holds it, so `Ok(None)`
        // confirms the lock for THIS state root is held. Catches a caller that
        // swaps without the lock, or holds the lock for a different state root.
        // Debug + unix only: the lock is a real advisory `flock` there, whereas the
        // non-unix stub cannot observe contention.
        debug_assert!(
            matches!(
                crate::daemon::lifecycle::try_acquire_rebuild_lock(state_root),
                Ok(None)
            ),
            "swap_index_into_place must run under the RebuildLock for {}",
            state_root.display()
        );
    }

    let swap_result = async {
        retire_prior_index_sidecars(&index_path).await?;
        crate::shared::fs::atomic_rename_file_within_root(
            staging_path,
            &index_path,
            &approved_root,
        )?;
        Ok(())
    }
    .await;

    if swap_result.is_err() {
        // Best-effort cleanup of the staging file, clamped to the `.1up` root so a
        // hostile path can never delete outside it. The cleanup error is discarded:
        // the swap error is the one worth surfacing.
        let _ = crate::shared::fs::remove_regular_file(staging_path, &approved_root);
    }

    swap_result
}

/// Retire any sidecars belonging to the prior `index.db` using SQLite's sanctioned
/// open-then-immediately-close idiom, never a raw `unlink`.
///
/// A no-op when `index_path` is absent (cold start) or already carries no
/// `-wal`/`-shm` sidecars — there is nothing to retire and the prior index is left
/// byte-for-byte untouched, which is the common case once writers have closed
/// cleanly. Otherwise the prior index is opened (recovering and `PASSIVE`-folding
/// its WAL frames into the main file) and every handle is immediately dropped so
/// SQLite removes the now-retired sidecars when this is the last connection.
///
/// A `PASSIVE` checkpoint (not `TRUNCATE`) is deliberate: a live CLI/MCP reader may
/// hold the prior index open across the swap, and `TRUNCATE` would block on it or
/// fail. `PASSIVE` never waits on readers. This stays correct because the only
/// orphan WAL that replays *silently* against the new inode is a header-compatible
/// one carrying committed frames, which only a writer produces — and writers are
/// excluded here by the held `RebuildLock` (and the daemon's quiesced handle,
/// HYP-002). A reader's WAL carries no committed frames and is not replayed
/// (HYP-001), so leaving a reader's transient sidecar behind is harmless; what
/// matters is that no writer-produced WAL survives, which holds because none can be
/// produced during the swap.
async fn retire_prior_index_sidecars(index_path: &Path) -> Result<(), OneupError> {
    if !index_path.exists() || !index_has_sidecars(index_path) {
        return Ok(());
    }

    let db = Db::open_rw(index_path).await?;
    let conn = db.connect()?;
    // Force the file open + WAL recovery and best-effort fold; a non-zero `busy`
    // (a reader held part of the WAL) is expected and fine, only a hard query
    // failure is surfaced. A transient `database is locked` — a sibling handle
    // mid-close briefly holding the lock, made likelier now that the index carries
    // the `embedding_pool` DiskANN sidecar tables — is retried within the shared
    // lock budget rather than failing the swap, mirroring `checkpoint_truncate`.
    let retry_delay = Duration::from_millis(DB_LOCK_RETRY_DELAY_MS);
    for attempt in 0..DB_LOCK_RETRY_ATTEMPTS {
        match conn.query(queries::WAL_CHECKPOINT_PASSIVE, ()).await {
            Ok(_) => break,
            Err(err) => {
                let err_text = err.to_string();
                if !is_lock_error(&err_text) || attempt + 1 == DB_LOCK_RETRY_ATTEMPTS {
                    return Err(StorageError::Query(format!(
                        "failed to retire prior index WAL for {}: {err_text}",
                        index_path.display()
                    ))
                    .into());
                }
                thread::sleep(retry_delay);
            }
        }
    }
    drop(conn);
    drop(db);

    Ok(())
}

/// Whether `index_path` has a `-wal` or `-shm` sidecar on disk. A cleanly-closed
/// SQLite database in WAL mode has neither once its last connection is dropped.
fn index_has_sidecars(index_path: &Path) -> bool {
    wal_sidecar_path(index_path).exists() || shm_sidecar_path(index_path).exists()
}

/// Run `wal_checkpoint(TRUNCATE)` until it completes (`busy == 0`), bounded by the
/// shared DB-lock retry budget. A non-zero `busy` means the WAL could not be
/// truncated because another connection held a lock; that is treated the same as
/// a transient `database is locked` and retried, then failed loud if it persists.
async fn checkpoint_truncate(conn: &Connection, staging_path: &Path) -> Result<(), OneupError> {
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
async fn checkpoint_truncate_once(conn: &Connection) -> Result<bool, libsql::Error> {
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
fn wal_sidecar_path(db_path: &Path) -> PathBuf {
    sidecar_path(db_path, "-wal")
}

/// Sidecar `-shm` path for a database file (`<name>` -> `<name>-shm`).
fn shm_sidecar_path(db_path: &Path) -> PathBuf {
    sidecar_path(db_path, "-shm")
}

/// Append a sidecar suffix to a database path (`<name>` -> `<name><suffix>`).
fn sidecar_path(db_path: &Path, suffix: &str) -> PathBuf {
    let mut name = db_path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use crate::daemon::lifecycle;
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

    /// Seed one embeddable segment together with its pooled vector and reference,
    /// so the exact `vector_distance_cos` scan can rank it. `vec` is a 384-d unit-ish
    /// vector serialized to JSON for `vector8(...)`.
    async fn seed_embeddable_segment(
        conn: &libsql::Connection,
        id: &str,
        content_key: &str,
        vec: &[f32],
    ) {
        insert_segment(conn, id).await;
        let vector = serde_json::to_string(vec).unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO embedding_pool (content_key, embedding_vec, ref_count) \
             VALUES (?1, vector8(?2), 1)",
            libsql::params![content_key, vector],
        )
        .await
        .unwrap();
        conn.execute(
            "INSERT INTO segment_vectors (segment_id, content_key) VALUES (?1, ?2)",
            libsql::params![id, content_key],
        )
        .await
        .unwrap();
    }

    /// A distinct 384-d vector whose first component is `lead` (and the rest a small
    /// constant), so `vector_distance_cos` against a probe yields a deterministic,
    /// well-separated ordering independent of any approximate-index quirks.
    fn probe_vector(lead: f32) -> Vec<f32> {
        let mut v = vec![0.01f32; 384];
        v[0] = lead;
        v
    }

    /// Run the served exact `vector_distance_cos` scan and return the ranked
    /// `segment_id` order for the default context.
    async fn exact_scan_order(conn: &libsql::Connection, query_vec: &[f32]) -> Vec<String> {
        let query = serde_json::to_string(query_vec).unwrap();
        let mut rows = conn
            .query(
                queries::SELECT_VECTOR_CANDIDATES_EXHAUSTIVE_FOR_CONTEXT,
                libsql::params![query, "default", 100_i64],
            )
            .await
            .unwrap();
        let mut order = Vec::new();
        while let Some(row) = rows.next().await.unwrap() {
            order.push(row.get::<String>(0).unwrap());
        }
        order
    }

    /// Create and canonicalize a project root under `tmp` (canonicalization makes
    /// the secure-fs path checks and the `flock` probe take their real paths on
    /// macOS, where `/var` is a symlink to `/private/var`).
    fn project_root(tmp: &tempfile::TempDir, name: &str) -> std::path::PathBuf {
        let root = tmp.path().canonicalize().unwrap().join(name);
        fs::create_dir_all(&root).unwrap();
        root
    }

    /// Build a finalized, self-contained index at `index_path` holding one segment
    /// per id (the id set distinguishes the prior index generation from the new
    /// one). `index_path` must be a `<project>/.1up/index.db` path.
    async fn build_index(index_path: &std::path::Path, ids: &[&str]) {
        let db = Db::open_rw(index_path).await.unwrap();
        let conn = db.connect_tuned().await.unwrap();
        schema::initialize(&conn).await.unwrap();
        for id in ids {
            insert_segment(&conn, id).await;
        }
        drop(conn);
        finalize_staged_db(db, index_path).await.unwrap();
    }

    /// Build a finalized staging file at `state_root`'s uuid staging path by
    /// building the index in a scratch project and moving the single self-contained
    /// file into place (the staging filename is not `index.db`, which `Db::open_rw`
    /// requires, so the file is built elsewhere and moved — the swap only renames
    /// it). The scratch project is discarded once its finalized file has been moved
    /// out.
    async fn staged_index(state_root: &std::path::Path, ids: &[&str]) -> std::path::PathBuf {
        let scratch = tempfile::tempdir().unwrap();
        let scratch_index = config::project_db_path(&project_root(&scratch, "scratch"));
        build_index(&scratch_index, ids).await;

        let staging = config::project_staging_db_path(state_root);
        fs::create_dir_all(config::project_dot_dir(state_root)).unwrap();
        fs::rename(&scratch_index, &staging).unwrap();
        staging
    }

    /// Open `index.db` read-only, gate it through `ensure_current`, and return the
    /// segment count — the "is this a complete, valid index?" probe a real reader
    /// runs. `Err` means a partial/missing index was observed.
    async fn read_segment_count(index_path: &std::path::Path) -> Result<i64, String> {
        let ro = Db::open_ro(index_path).await.map_err(|e| e.to_string())?;
        let conn = ro.connect().map_err(|e| e.to_string())?;
        schema::ensure_current(&conn, &schema::SchemaContext::unspecified())
            .await
            .map_err(|e| e.to_string())?;
        let mut rows = conn
            .query(queries::COUNT_SEGMENTS, ())
            .await
            .map_err(|e| e.to_string())?;
        let row = rows.next().await.map_err(|e| e.to_string())?.unwrap();
        row.get::<i64>(0).map_err(|e| e.to_string())
    }

    fn assert_no_sidecars(index_path: &std::path::Path) {
        for sidecar in [wal_sidecar_path(index_path), shm_sidecar_path(index_path)] {
            assert!(
                !sidecar.exists(),
                "pre-swap sidecar must not remain after the swap: {}",
                sidecar.display()
            );
        }
    }

    #[tokio::test]
    async fn finalize_staged_db_produces_self_contained_file() {
        let tmp = tempfile::tempdir().unwrap();
        let staging_path = config::project_db_path(&project_root(&tmp, "project"));

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

    #[tokio::test]
    async fn swap_replaces_index_with_new_generation_and_leaves_no_sidecars() {
        let tmp = tempfile::tempdir().unwrap();
        let root = project_root(&tmp, "project");
        let index_path = config::project_db_path(&root);

        build_index(&index_path, &["old1", "old2"]).await;
        let staging = staged_index(&root, &["new1", "new2", "new3"]).await;

        let _lock = lifecycle::acquire_rebuild_lock(&root).unwrap();
        swap_index_into_place(&root, &staging).await.unwrap();

        // The switch left no pre-swap sidecars (HYP-001: a header-compatible orphan
        // WAL would replay silently), checked before any reader opens the new inode.
        assert_no_sidecars(&index_path);
        // Reads observe the full NEW generation, not the old one.
        assert_eq!(read_segment_count(&index_path).await.unwrap(), 3);
        // The staging file was consumed by the rename.
        assert!(!staging.exists());
    }

    #[tokio::test]
    async fn swap_retires_prior_sidecars_before_renaming() {
        let tmp = tempfile::tempdir().unwrap();
        let root = project_root(&tmp, "project");
        let index_path = config::project_db_path(&root);

        build_index(&index_path, &["old1"]).await;
        // Simulate a prior index that was not closed cleanly: a leftover (empty)
        // `-wal` sidecar sits next to it. The swap must retire it (open-then-close)
        // before the rename, not carry it over onto the new inode.
        fs::write(wal_sidecar_path(&index_path), b"").unwrap();
        assert!(index_has_sidecars(&index_path));

        let staging = staged_index(&root, &["new1", "new2"]).await;

        let _lock = lifecycle::acquire_rebuild_lock(&root).unwrap();
        swap_index_into_place(&root, &staging).await.unwrap();

        assert_no_sidecars(&index_path);
        assert_eq!(read_segment_count(&index_path).await.unwrap(), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn swap_is_all_or_nothing_under_concurrent_readers() {
        let tmp = tempfile::tempdir().unwrap();
        let root = project_root(&tmp, "project");
        let index_path = config::project_db_path(&root);

        // Prior generation = 2 rows; new generation = 5 rows. A reader sampling the
        // served index while the swap is in flight must only ever observe one of
        // these two complete states — never an absent, empty, or partial index.
        build_index(&index_path, &["old1", "old2"]).await;
        let staging = staged_index(&root, &["new1", "new2", "new3", "new4", "new5"]).await;
        let _lock = lifecycle::acquire_rebuild_lock(&root).unwrap();

        // Real concurrency (separate tasks on a multi-thread runtime): the readers
        // hammer `index.db` on worker threads while the swap renames underneath them.
        let done = Arc::new(AtomicBool::new(false));
        let readers: Vec<_> = (0..3)
            .map(|_| {
                let index_path = index_path.clone();
                let done = Arc::clone(&done);
                tokio::spawn(async move {
                    let mut observations = Vec::new();
                    while !done.load(Ordering::Acquire) {
                        observations.push(read_segment_count(&index_path).await);
                    }
                    // A few more samples after the swap so some land on the new inode.
                    for _ in 0..5 {
                        observations.push(read_segment_count(&index_path).await);
                    }
                    observations
                })
            })
            .collect();

        // Let the reader tasks start hammering before the swap so reads straddle it.
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        swap_index_into_place(&root, &staging).await.unwrap();
        done.store(true, Ordering::Release);

        let mut saw_new = false;
        for reader in readers {
            for observation in reader.await.unwrap() {
                let count = observation.expect("every read must see a complete, valid index");
                assert!(
                    count == 2 || count == 5,
                    "read observed a torn/partial index: {count} rows (expected 2 or 5)"
                );
                saw_new |= count == 5;
            }
        }
        assert!(saw_new, "post-swap reads must observe the new generation");
        // Final settled read is unambiguously the new index.
        assert_eq!(read_segment_count(&index_path).await.unwrap(), 5);
    }

    #[tokio::test]
    async fn swap_leaves_prior_index_unchanged_when_staging_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = project_root(&tmp, "project");
        let index_path = config::project_db_path(&root);

        build_index(&index_path, &["old1", "old2"]).await;
        let before = fs::read(&index_path).unwrap();

        // The staging file does not exist: the swap fails before the rename, with
        // no prior sidecars to retire, so `index.db` is byte-for-byte untouched.
        let missing = config::project_staging_db_path(&root);
        let _lock = lifecycle::acquire_rebuild_lock(&root).unwrap();
        let err = swap_index_into_place(&root, &missing).await.unwrap_err();
        // Platform-agnostic "not found": Unix reports "No such file or directory";
        // Windows reports "The system cannot find the file specified".
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("no such file") || msg.contains("cannot find"),
            "expected a not-found error, got: {err}"
        );

        assert_eq!(
            fs::read(&index_path).unwrap(),
            before,
            "a pre-rename failure must leave index.db byte-for-byte unchanged"
        );
        assert_eq!(read_segment_count(&index_path).await.unwrap(), 2);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn swap_cleans_up_staging_on_failure() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let root = project_root(&tmp, "project");

        // A hostile `index.db` symlink pointing outside the `.1up` root: the rename
        // step rejects the symlink leaf, so the swap fails. The staging file it was
        // handed must then be best-effort removed.
        fs::create_dir_all(config::project_dot_dir(&root)).unwrap();
        let outside = tmp.path().canonicalize().unwrap().join("outside.db");
        fs::write(&outside, b"outside").unwrap();
        symlink(&outside, config::project_db_path(&root)).unwrap();

        let staging = staged_index(&root, &["new1"]).await;
        assert!(staging.exists());

        let _lock = lifecycle::acquire_rebuild_lock(&root).unwrap();
        let err = swap_index_into_place(&root, &staging).await.unwrap_err();
        assert!(err.to_string().contains("symlink"));

        assert!(
            !staging.exists(),
            "the staging file must be cleaned up when the swap fails"
        );
        // The symlink target outside the root is never written through.
        assert_eq!(fs::read(&outside).unwrap(), b"outside");
    }

    #[tokio::test]
    async fn swap_creates_index_on_cold_start() {
        let tmp = tempfile::tempdir().unwrap();
        let root = project_root(&tmp, "project");
        let index_path = config::project_db_path(&root);

        // No prior index exists: the swap simply renames the staged index into
        // place (the cold-start path must not require a prior index).
        let staging = staged_index(&root, &["new1", "new2"]).await;
        assert!(!index_path.exists());

        let _lock = lifecycle::acquire_rebuild_lock(&root).unwrap();
        swap_index_into_place(&root, &staging).await.unwrap();

        assert_no_sidecars(&index_path);
        assert_eq!(read_segment_count(&index_path).await.unwrap(), 2);
        assert!(!staging.exists());
    }

    #[tokio::test]
    async fn staging_rebuild_switches_in_the_new_generation() {
        let tmp = tempfile::tempdir().unwrap();
        let root = project_root(&tmp, "project");
        let index_path = config::project_db_path(&root);

        // A prior served index of 2 rows.
        build_index(&index_path, &["old1", "old2"]).await;

        // Build the refreshed index (3 rows) into the staging file through the
        // guard, then finalize + switch it over the served index under the lock.
        let _lock = lifecycle::acquire_rebuild_lock(&root).unwrap();
        let staged = StagingRebuild::open(&root).await.unwrap();
        for id in ["new1", "new2", "new3"] {
            insert_segment(staged.connection(), id).await;
        }
        staged.finalize_and_swap().await.unwrap();

        // The served index is the full new generation, with no orphan sidecars.
        assert_no_sidecars(&index_path);
        assert_eq!(read_segment_count(&index_path).await.unwrap(), 3);
    }

    #[tokio::test]
    async fn aborted_staging_rebuild_leaves_prior_index_intact_and_no_orphan() {
        // REQ-001 AC3: a rebuild interrupted/cancelled before the switch-over must
        // leave the prior `index.db` intact and served, and leave no orphan staging
        // file. Dropping the guard without `finalize_and_swap` is the build-aside
        // equivalent of a failed/cancelled rebuild before the switch.
        let tmp = tempfile::tempdir().unwrap();
        let root = project_root(&tmp, "project");
        let index_path = config::project_db_path(&root);

        build_index(&index_path, &["old1", "old2"]).await;
        let before = fs::read(&index_path).unwrap();

        let staging_path = {
            let staged = StagingRebuild::open(&root).await.unwrap();
            let staging_path = staged.staging_path.clone();
            // Partially build the staged index, then drop the guard below without
            // ever finalizing or switching it over.
            insert_segment(staged.connection(), "partial").await;
            assert!(staging_path.exists(), "staging file exists mid-build");
            staging_path
        };

        // The prior served index is byte-for-byte intact and still queryable: the
        // build-aside rebuild never touched it before the (skipped) switch.
        assert_eq!(
            fs::read(&index_path).unwrap(),
            before,
            "an aborted rebuild must not touch the served index"
        );
        assert_eq!(read_segment_count(&index_path).await.unwrap(), 2);
        // The aborted build left no orphan staging file behind.
        assert!(
            !staging_path.exists(),
            "dropping the guard before the switch must remove the staging file"
        );
    }

    /// R-006 / HYP-002 acceptance: a cold rebuild that *defers* the DiskANN build
    /// (the `StagingRebuild` path) serves byte-identical ranked results to one that
    /// builds the index incrementally/immediately, and the served read path is the
    /// exact `vector_distance_cos` scan at this corpus size (well below
    /// `VECTOR_EXHAUSTIVE_SCAN_MAX_VECTORS`).
    ///
    /// Built test-first: pointing `StagingRebuild::open` back at the immediate
    /// `initialize` (so the index exists during the build) does not change the served
    /// ranking — the equivalence holds by construction because the exact scan never
    /// consults the DiskANN graph. The teeth here are on the *deferred build actually
    /// running*: removing the `build_embedding_pool_vector_index` call from
    /// `finalize_and_swap` makes the served index miss its required DiskANN index, so
    /// `read_through_ensure_current` below fails closed.
    #[tokio::test]
    async fn deferred_build_search_equivalent_to_immediate_and_uses_exact_scan() {
        // Three embeddable segments with well-separated vectors and a probe nearest
        // to `s-mid`, then `s-near`, then `s-far` — a deterministic expected order.
        let seeds: [(&str, &str, Vec<f32>); 3] = [
            ("s-near", "k-near", probe_vector(0.9)),
            ("s-mid", "k-mid", probe_vector(1.0)),
            ("s-far", "k-far", probe_vector(0.2)),
        ];
        let probe = probe_vector(1.0);

        // (A) Reference index built the IMMEDIATE way (DiskANN index created inline,
        // as the daemon/incremental path does).
        let tmp_imm = tempfile::tempdir().unwrap();
        let imm_index = config::project_db_path(&project_root(&tmp_imm, "immediate"));
        {
            let db = Db::open_rw(&imm_index).await.unwrap();
            let conn = db.connect_tuned().await.unwrap();
            schema::initialize(&conn).await.unwrap();
            for (id, key, vec) in &seeds {
                seed_embeddable_segment(&conn, id, key, vec).await;
            }
            drop(conn);
            finalize_staged_db(db, &imm_index).await.unwrap();
        }
        let imm_db = Db::open_ro(&imm_index).await.unwrap();
        let imm_conn = imm_db.connect().unwrap();
        let immediate_order = exact_scan_order(&imm_conn, &probe).await;

        // (B) Served index built the DEFERRED way through the real staging rebuild
        // guard: pool/vector rows inserted first, DiskANN index built once in
        // `finalize_and_swap`, then atomically swapped into place.
        let tmp_def = tempfile::tempdir().unwrap();
        let root = project_root(&tmp_def, "deferred");
        let def_index = config::project_db_path(&root);
        let _lock = lifecycle::acquire_rebuild_lock(&root).unwrap();
        let staged = StagingRebuild::open(&root).await.unwrap();
        // Mid-rebuild the staging schema is intentionally incomplete (no DiskANN
        // index yet); the served index only ever appears after the deferred build.
        assert!(
            !schema_object_exists_named(staged.connection(), "idx_embedding_pool_embedding").await,
            "deferred staging schema must omit the DiskANN index until finalize"
        );
        for (id, key, vec) in &seeds {
            seed_embeddable_segment(staged.connection(), id, key, vec).await;
        }
        staged.finalize_and_swap().await.unwrap();

        // The served index passes the reader schema gate (proving the deferred build
        // completed the schema) and carries the DiskANN index.
        let def_db = Db::open_ro(&def_index).await.unwrap();
        let def_conn = def_db.connect().unwrap();
        schema::ensure_current(&def_conn, &schema::SchemaContext::unspecified())
            .await
            .expect("deferred-built served index must pass ensure_current");
        assert!(
            schema_object_exists_named(&def_conn, "idx_embedding_pool_embedding").await,
            "deferred build must leave the DiskANN index present on the served index"
        );

        let deferred_order = exact_scan_order(&def_conn, &probe).await;

        // Search equivalence: identical ranked segment_id ordering.
        assert_eq!(
            deferred_order, immediate_order,
            "deferred-built index must return identical ranked order to the immediate build"
        );
        assert_eq!(
            deferred_order,
            vec![
                "s-mid".to_string(),
                "s-near".to_string(),
                "s-far".to_string()
            ],
            "ranked order must follow vector_distance_cos to the probe"
        );

        // The served path is the exact scan, not the DiskANN beam search: the exact
        // query plan reads `embedding_pool` via the 1:1 `segment_vectors` join and
        // never references `vector_top_k`/the DiskANN index.
        assert!(
            !queries::SELECT_VECTOR_CANDIDATES_EXHAUSTIVE_FOR_CONTEXT.contains("vector_top_k"),
            "the served exact scan must not consult the DiskANN index at this corpus size"
        );
    }

    /// Whether a named object exists in the database (test convenience for the
    /// vector-index presence assertions).
    async fn schema_object_exists_named(conn: &libsql::Connection, name: &str) -> bool {
        let mut rows = conn
            .query(
                "SELECT 1 FROM sqlite_master WHERE name = ?1 LIMIT 1",
                [name],
            )
            .await
            .unwrap();
        rows.next().await.unwrap().is_some()
    }
}
