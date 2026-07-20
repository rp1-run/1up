use std::path::Path;
use std::thread;
use std::time::Duration;

use libsql::Connection;

use crate::shared::constants::{
    DB_LOCK_RETRY_ATTEMPTS, DB_LOCK_RETRY_DELAY_MS, SCHEMA_INIT_WAIT_ATTEMPTS,
    SCHEMA_INIT_WAIT_DELAY_MS, SCHEMA_VERSION,
};
use crate::shared::errors::{OneupError, StorageError};
use crate::storage::db::is_lock_error;
use crate::storage::queries;

const META_KEY_SCHEMA_VERSION: &str = "schema_version";
const META_KEY_EMBEDDING_MODEL: &str = "embedding_model";
const META_KEY_EMBEDDING_DIM: &str = "embedding_dim";
const META_KEY_SCOPE_ROOTS: &str = "scope_roots_v1";

/// Stable message fragment emitted by [`ensure_current`] when the database has
/// user tables but no readable `schema_version` row.
///
/// This is the exact shape produced while a fresh index is mid-initialization:
/// [`initialize`] creates every table first and writes the version row last (it
/// is not one atomic transaction), so a concurrent reader on a separate
/// connection can momentarily observe "tables exist, version absent". Callers
/// that can tolerate a transient state (e.g. the MCP readiness path during
/// daemon auto-start) match this fragment via [`is_initializing_schema_error`]
/// to avoid misreporting a freshly-initializing index as permanently stale.
pub const SCHEMA_MISSING_OR_UNREADABLE_FRAGMENT: &str = "index schema is missing or unreadable";
const REQUIRED_SCHEMA_OBJECTS: &[(&str, &str)] = &[
    ("table", "worktree_contexts"),
    ("table", "segments"),
    ("table", "embedding_pool"),
    ("table", "segment_vectors"),
    ("table", "segment_symbols"),
    ("table", "segment_relations"),
    ("table", "indexed_files"),
    ("table", "segments_fts"),
    ("table", "meta"),
    ("index", "idx_segments_file_path"),
    ("index", "idx_segments_context_file_path"),
    ("index", "idx_segments_language"),
    ("index", "idx_segment_symbols_exact"),
    ("index", "idx_segment_symbols_prefix"),
    ("index", "idx_segment_relations_source"),
    ("index", "idx_segment_relations_target"),
    ("index", "idx_segment_relations_lookup_target"),
    ("trigger", "segments_ai"),
    ("trigger", "segments_ad"),
    ("trigger", "segments_au"),
    ("trigger", "segments_vector_ad"),
    ("trigger", "segments_symbol_ad"),
];

/// Run all DDL statements to initialize the database schema.
/// This only creates the current schema version for fresh or explicitly rebuilt indexes.
pub async fn initialize(conn: &Connection) -> Result<(), OneupError> {
    conn.execute_batch(&format!(
        "{};{};{};{};{};{};{};{};{};{}",
        queries::CREATE_WORKTREE_CONTEXTS_TABLE,
        queries::CREATE_SEGMENTS_TABLE,
        queries::CREATE_INDEX_FILE_PATH,
        queries::CREATE_INDEX_SEGMENTS_CONTEXT_FILE_PATH,
        queries::CREATE_INDEX_LANGUAGE,
        queries::CREATE_EMBEDDING_POOL_TABLE,
        queries::CREATE_SEGMENT_VECTORS_TABLE,
        queries::CREATE_SEGMENT_SYMBOLS_TABLE,
        queries::CREATE_SEGMENT_RELATIONS_TABLE,
        queries::CREATE_INDEXED_FILES_TABLE,
    ))
    .await
    .map_err(|e| StorageError::Migration(format!("failed to create segments schema: {e}")))?;

    conn.execute_batch(&format!(
        "{};{};{};{};{}",
        queries::CREATE_INDEX_SEGMENT_SYMBOLS_EXACT,
        queries::CREATE_INDEX_SEGMENT_SYMBOLS_PREFIX,
        queries::CREATE_INDEX_SEGMENT_RELATIONS_SOURCE,
        queries::CREATE_INDEX_SEGMENT_RELATIONS_TARGET,
        queries::CREATE_INDEX_SEGMENT_RELATIONS_LOOKUP_TARGET,
    ))
    .await
    .map_err(|e| {
        StorageError::Migration(format!("failed to create symbol and relation indexes: {e}"))
    })?;

    conn.execute_batch(queries::CREATE_FTS_TABLE)
        .await
        .map_err(|e| StorageError::Migration(format!("failed to create FTS table: {e}")))?;

    conn.execute_batch(queries::CREATE_FTS_TRIGGERS)
        .await
        .map_err(|e| StorageError::Migration(format!("failed to create FTS triggers: {e}")))?;

    conn.execute_batch(queries::CREATE_SEGMENT_SYMBOLS_TRIGGER)
        .await
        .map_err(|e| StorageError::Migration(format!("failed to create symbol triggers: {e}")))?;

    conn.execute(queries::CREATE_META_TABLE, ())
        .await
        .map_err(|e| StorageError::Migration(format!("failed to create meta table: {e}")))?;

    validate_required_objects(conn).await?;
    set_schema_version(conn, SCHEMA_VERSION).await?;

    Ok(())
}

/// Read the current schema version from the meta table.
/// Returns None if no version is stored yet.
pub async fn get_schema_version(conn: &Connection) -> Result<Option<u32>, OneupError> {
    if !schema_object_exists(conn, "table", "meta").await? {
        return Ok(None);
    }

    let mut rows = conn
        .query(queries::SELECT_META, [META_KEY_SCHEMA_VERSION])
        .await
        .map_err(|e| StorageError::Query(format!("failed to read schema version: {e}")))?;

    match rows.next().await {
        Ok(Some(row)) => {
            let val: String = row
                .get(0)
                .map_err(|e| StorageError::Query(format!("failed to read version value: {e}")))?;
            let version: u32 = val
                .parse()
                .map_err(|e| StorageError::Query(format!("invalid schema version '{val}': {e}")))?;
            Ok(Some(version))
        }
        Ok(None) => Ok(None),
        Err(e) => Err(StorageError::Query(format!("schema version query failed: {e}")).into()),
    }
}

/// Write the schema version to the meta table.
async fn set_schema_version(conn: &Connection, version: u32) -> Result<(), OneupError> {
    conn.execute(
        queries::UPSERT_META,
        [META_KEY_SCHEMA_VERSION, &version.to_string()],
    )
    .await
    .map_err(|e| StorageError::Migration(format!("failed to set schema version: {e}")))?;
    Ok(())
}

/// Caller-supplied location for a schema-gate failure.
///
/// Threaded into [`ensure_current`] so a version mismatch can name the worktree
/// the caller resolved and the shared `.1up/index.db` it opened. Because every
/// git worktree of a repository shares one physical index (scoped only logically
/// by `context_id`), naming both turns the otherwise-generic cross-worktree drift
/// into an actionable error. Both paths are optional so internal write-path callers
/// (`prepare_for_write`) that hold no resolved paths can still gate without
/// fabricating one — the message then names the offending version and remediation
/// without a location clause.
#[derive(Debug, Clone, Copy, Default)]
pub struct SchemaContext<'a> {
    pub db_path: Option<&'a Path>,
    pub worktree_path: Option<&'a Path>,
}

impl<'a> SchemaContext<'a> {
    /// Context naming the shared index and the worktree that opened it.
    pub fn new(db_path: &'a Path, worktree_path: &'a Path) -> Self {
        Self {
            db_path: Some(db_path),
            worktree_path: Some(worktree_path),
        }
    }

    /// Context for internal callers that hold no resolved paths; the error still
    /// names the offending version and remediation, just not a location.
    pub fn unspecified() -> Self {
        Self::default()
    }
}

/// Render the caller-supplied location as a trailing clause for a version-mismatch
/// message (e.g. ` for worktree '/a/wt' sharing index '/repo/.1up/index.db'`).
/// Returns an empty string when no paths were supplied so the mandated substring
/// contract that `src/cli/start.rs` parses is never perturbed. The clause is always
/// appended *after* the `(found v.., expected v..)` / `index schema v..` tokens, so
/// the first-match version parsers in `start.rs` keep reading the real versions.
fn schema_location_clause(ctx: &SchemaContext) -> String {
    match (ctx.worktree_path, ctx.db_path) {
        (Some(worktree), Some(db)) => format!(
            " for worktree '{}' sharing index '{}'",
            worktree.display(),
            db.display()
        ),
        (Some(worktree), None) => format!(" for worktree '{}'", worktree.display()),
        (None, Some(db)) => format!(" for index '{}'", db.display()),
        (None, None) => String::new(),
    }
}

/// Create the current schema for an empty database or require explicit rebuild guidance.
pub async fn prepare_for_write(conn: &Connection) -> Result<(), OneupError> {
    if database_has_user_tables(conn).await? {
        ensure_current(conn, &SchemaContext::unspecified()).await
    } else {
        initialize(conn).await
    }
}

/// Verify that an existing database matches the current schema without mutating it.
///
/// On a version mismatch the error names the offending version, the caller-supplied
/// worktree/index location (via `ctx`), and the exact remediation. The mandated
/// substrings (`out of date`, `found v{N}`, `expected v{M}`, `newer than this binary
/// supports`, and the `; run `1up reindex`` remediation) are preserved verbatim so the
/// `src/cli/start.rs` parser (`classify_schema_error`/`parse_schema_versions`) keeps
/// classifying the enriched message.
pub async fn ensure_current(conn: &Connection, ctx: &SchemaContext<'_>) -> Result<(), OneupError> {
    let current = get_schema_version(conn).await?;

    match current {
        Some(v) if v == SCHEMA_VERSION => validate_required_objects(conn).await,
        Some(v) if v < SCHEMA_VERSION => Err(reindex_required(format!(
            "index schema is out of date (found v{v}, expected v{SCHEMA_VERSION}){location}",
            location = schema_location_clause(ctx)
        ))),
        Some(v) => Err(StorageError::Migration(format!(
            "index schema v{v} is newer than this binary supports (expected v{SCHEMA_VERSION}){location}; rebuild with a compatible binary or upgrade `1up`",
            location = schema_location_clause(ctx)
        ))
        .into()),
        None => {
            if database_has_user_tables(conn).await? {
                Err(reindex_required(
                    SCHEMA_MISSING_OR_UNREADABLE_FRAGMENT.to_string(),
                ))
            } else {
                Err(reindex_required("index is missing".to_string()))
            }
        }
    }
}

/// [`ensure_current`], but tolerant of the window in which a freshly created
/// index has its tables but not yet its `schema_version` row.
///
/// [`initialize`] creates every table first and writes the `schema_version`
/// row last, and is not a single transaction. A reader that races the daemon's
/// first index — or the atomic swap at the end of a rebuild — can observe
/// "tables exist, version absent", which [`ensure_current`] reports as the
/// transient [`is_initializing_schema_error`] shape. We retry on exactly that
/// shape to let initialization settle. A genuine version mismatch (`out of
/// date` / `newer than this binary supports`) is a distinct shape and still
/// fails fast on the first attempt.
///
/// The wait budget is [`SCHEMA_INIT_WAIT_ATTEMPTS`] × [`SCHEMA_INIT_WAIT_DELAY_MS`]
/// (roughly 5 s), sized for a concurrent *full schema initialization/upgrade* —
/// which on a large index can take several seconds — rather than the
/// millisecond-scale DB-lock retry budget it previously borrowed (GitHub issue
/// #93). See those constants for the rationale.
///
/// Read commands should call this instead of [`ensure_current`] so that
/// `search`/`status` right after a `reindex` (or during a daemon rebuild) ride
/// out the window rather than surfacing a spurious "reindex required" — the
/// same hardening the MCP readiness path already applies.
pub async fn ensure_current_tolerating_init(
    conn: &Connection,
    ctx: &SchemaContext<'_>,
) -> Result<(), OneupError> {
    ensure_current_tolerating_init_with_budget(
        conn,
        ctx,
        SCHEMA_INIT_WAIT_ATTEMPTS,
        Duration::from_millis(SCHEMA_INIT_WAIT_DELAY_MS),
    )
    .await
}

/// [`ensure_current_tolerating_init`] with an explicit retry budget.
///
/// Factored out so the public entry point pins the production
/// [`SCHEMA_INIT_WAIT_ATTEMPTS`]/[`SCHEMA_INIT_WAIT_DELAY_MS`] budget while tests
/// can exercise budget exhaustion with a tiny, fast budget. `attempts` is the
/// total number of [`ensure_current`] validations; between failures on the
/// transient initializing shape it sleeps `retry_delay`, so the total wait is
/// `(attempts - 1) × retry_delay`.
async fn ensure_current_tolerating_init_with_budget(
    conn: &Connection,
    ctx: &SchemaContext<'_>,
    attempts: usize,
    retry_delay: Duration,
) -> Result<(), OneupError> {
    let mut attempt = 0;
    loop {
        match ensure_current(conn, ctx).await {
            Ok(()) => return Ok(()),
            Err(err) => {
                attempt += 1;
                if attempt >= attempts || !is_initializing_schema_error(&err) {
                    return Err(err);
                }
                // Yield rather than block: this is an async fn and a caller (the
                // MCP warm-index path) may await it while holding a shared lock,
                // so a blocking `thread::sleep` here would stall unrelated tasks
                // on the same runtime during the init window. Mirrors the prior
                // MCP wrapper's `tokio::time::sleep`.
                tokio::time::sleep(retry_delay).await;
            }
        }
    }
}

/// Whether an [`ensure_current`] error is the transient "tables present but no
/// readable schema version" shape (see [`SCHEMA_MISSING_OR_UNREADABLE_FRAGMENT`]).
///
/// This shape — and *only* this shape — can be produced by a fresh index that is
/// still mid-initialization (the daemon's [`initialize`] writes the version row
/// last). A genuine version mismatch reports the distinct `out of date (found
/// v{N}...)` / `newer than this binary supports` shapes instead, so matching this
/// fragment never masks a real incompatible-schema condition. Callers can use this
/// to ride out the initialization window before deciding an index is stale.
pub fn is_initializing_schema_error(err: &OneupError) -> bool {
    err.to_string()
        .contains(SCHEMA_MISSING_OR_UNREADABLE_FRAGMENT)
}

async fn database_has_user_tables(conn: &Connection) -> Result<bool, OneupError> {
    let mut rows = conn
        .query(queries::SELECT_HAS_USER_TABLES, ())
        .await
        .map_err(|e| StorageError::Query(format!("failed to inspect database contents: {e}")))?;

    match rows.next().await {
        Ok(Some(_)) => Ok(true),
        Ok(None) => Ok(false),
        Err(e) => Err(StorageError::Query(format!("database inspection failed: {e}")).into()),
    }
}

async fn schema_object_exists(
    conn: &Connection,
    object_type: &str,
    name: &str,
) -> Result<bool, OneupError> {
    let retry_delay = Duration::from_millis(DB_LOCK_RETRY_DELAY_MS);
    let mut last_error = None;

    for attempt in 0..DB_LOCK_RETRY_ATTEMPTS {
        match schema_object_exists_once(conn, object_type, name).await {
            Ok(exists) => return Ok(exists),
            Err(e) => {
                let err_text = e.to_string();
                if !is_lock_error(&err_text) || attempt + 1 == DB_LOCK_RETRY_ATTEMPTS {
                    return Err(StorageError::Query(format!(
                        "failed to inspect schema object {object_type} `{name}`: {err_text}"
                    ))
                    .into());
                }
                last_error = Some(err_text);
                thread::sleep(retry_delay);
            }
        }
    }

    Err(StorageError::Query(format!(
        "failed to inspect schema object {object_type} `{name}`: {}",
        last_error.unwrap_or_else(|| "database inspection failed".to_string())
    ))
    .into())
}

async fn schema_object_exists_once(
    conn: &Connection,
    object_type: &str,
    name: &str,
) -> Result<bool, libsql::Error> {
    let mut rows = conn
        .query(queries::SELECT_SCHEMA_OBJECT, [object_type, name])
        .await?;

    match rows.next().await {
        Ok(Some(_)) => Ok(true),
        Ok(None) => Ok(false),
        Err(e) => Err(e),
    }
}

async fn table_has_column(
    conn: &Connection,
    table_name: &str,
    column_name: &str,
) -> Result<bool, OneupError> {
    // Retry on transient `database is locked` exactly like `schema_object_exists`:
    // this PRAGMA-backed inspection runs on the MCP status / readiness path while
    // an auto-started daemon may hold a write lock, and a single-shot failure
    // would surface as a misleading `stale` schema state instead of retrying.
    let retry_delay = Duration::from_millis(DB_LOCK_RETRY_DELAY_MS);
    let mut last_error = None;

    for attempt in 0..DB_LOCK_RETRY_ATTEMPTS {
        match table_has_column_once(conn, table_name, column_name).await {
            Ok(exists) => return Ok(exists),
            Err(e) => {
                let err_text = e.to_string();
                if !is_lock_error(&err_text) || attempt + 1 == DB_LOCK_RETRY_ATTEMPTS {
                    return Err(StorageError::Query(format!(
                        "table column inspection failed for {table_name}.{column_name}: {err_text}"
                    ))
                    .into());
                }
                last_error = Some(err_text);
                thread::sleep(retry_delay);
            }
        }
    }

    Err(StorageError::Query(format!(
        "table column inspection failed for {table_name}.{column_name}: {}",
        last_error.unwrap_or_else(|| "database inspection failed".to_string())
    ))
    .into())
}

async fn table_has_column_once(
    conn: &Connection,
    table_name: &str,
    column_name: &str,
) -> Result<bool, libsql::Error> {
    let query = format!("SELECT 1 FROM pragma_table_info('{table_name}') WHERE name = ?1 LIMIT 1");
    let mut rows = conn.query(&query, [column_name]).await?;

    match rows.next().await {
        Ok(Some(_)) => Ok(true),
        Ok(None) => Ok(false),
        Err(e) => Err(e),
    }
}

async fn segment_vectors_has_content_key(conn: &Connection) -> Result<bool, OneupError> {
    table_has_column(conn, "segment_vectors", "content_key").await
}

#[cfg(test)]
async fn embedding_pool_embedding_vec_type(
    conn: &Connection,
) -> Result<Option<String>, OneupError> {
    let mut rows = conn
        .query(
            "SELECT type FROM pragma_table_info('embedding_pool') WHERE name = ?1 LIMIT 1",
            ["embedding_vec"],
        )
        .await
        .map_err(|e| {
            StorageError::Query(format!(
                "failed to read embedding_pool.embedding_vec type: {e}"
            ))
        })?;

    match rows.next().await {
        Ok(Some(row)) => {
            let ty: String = row.get(0).map_err(|e| {
                StorageError::Query(format!(
                    "failed to read embedding_pool.embedding_vec type value: {e}"
                ))
            })?;
            Ok(Some(ty))
        }
        Ok(None) => Ok(None),
        Err(e) => Err(StorageError::Query(format!(
            "embedding_pool.embedding_vec type inspection failed: {e}"
        ))
        .into()),
    }
}

async fn validate_required_objects(conn: &Connection) -> Result<(), OneupError> {
    for (object_type, name) in REQUIRED_SCHEMA_OBJECTS {
        if !schema_object_exists(conn, object_type, name).await? {
            return Err(reindex_required(format!(
                "index schema v{SCHEMA_VERSION} is incomplete (missing required {object_type} `{name}`)"
            )));
        }
    }

    if !segment_vectors_has_content_key(conn).await? {
        return Err(reindex_required(format!(
            "index schema v{SCHEMA_VERSION} is incomplete (missing required column `segment_vectors.content_key`)"
        )));
    }

    for (table_name, column_name) in [
        ("segments", "context_id"),
        ("indexed_files", "context_id"),
        ("segment_symbols", "context_id"),
        ("segment_relations", "context_id"),
    ] {
        if !table_has_column(conn, table_name, column_name).await? {
            return Err(reindex_required(format!(
                "index schema v{SCHEMA_VERSION} is incomplete (missing required column `{table_name}.{column_name}`)"
            )));
        }
    }

    if !table_has_column(conn, "segment_relations", "lookup_canonical_symbol").await? {
        return Err(reindex_required(format!(
            "index schema v{SCHEMA_VERSION} is incomplete (missing required column `segment_relations.lookup_canonical_symbol`)"
        )));
    }

    if !table_has_column(conn, "segment_relations", "qualifier_fingerprint").await? {
        return Err(reindex_required(format!(
            "index schema v{SCHEMA_VERSION} is incomplete (missing required column `segment_relations.qualifier_fingerprint`)"
        )));
    }

    if !table_has_column(conn, "segment_relations", "edge_identity_kind").await? {
        return Err(reindex_required(format!(
            "index schema v{SCHEMA_VERSION} is incomplete (missing required column `segment_relations.edge_identity_kind`)"
        )));
    }

    Ok(())
}

fn reindex_required(message: String) -> OneupError {
    StorageError::Migration(format!("{message}; run `1up reindex`")).into()
}

/// Reads the embedding model name recorded in the meta table.
///
/// Returns `None` if no model metadata has been stored yet (i.e. the index
/// was created before model tracking was introduced, or is brand new).
pub async fn get_embedding_model(conn: &Connection) -> Result<Option<String>, OneupError> {
    if !schema_object_exists(conn, "table", "meta").await? {
        return Ok(None);
    }

    let mut rows = conn
        .query(queries::SELECT_META, [META_KEY_EMBEDDING_MODEL])
        .await
        .map_err(|e| StorageError::Query(format!("failed to read embedding model: {e}")))?;

    match rows.next().await {
        Ok(Some(row)) => {
            let val: String = row.get(0).map_err(|e| {
                StorageError::Query(format!("failed to read embedding model value: {e}"))
            })?;
            Ok(Some(val))
        }
        Ok(None) => Ok(None),
        Err(e) => Err(StorageError::Query(format!("embedding model query failed: {e}")).into()),
    }
}

/// Persists embedding model metadata (name and output dimension) to the meta table.
async fn set_embedding_model_meta(
    conn: &Connection,
    model_name: &str,
    dim: usize,
) -> Result<(), OneupError> {
    conn.execute(queries::UPSERT_META, [META_KEY_EMBEDDING_MODEL, model_name])
        .await
        .map_err(|e| StorageError::Migration(format!("failed to write embedding model: {e}")))?;
    conn.execute(
        queries::UPSERT_META,
        [META_KEY_EMBEDDING_DIM, &dim.to_string()],
    )
    .await
    .map_err(|e| StorageError::Migration(format!("failed to write embedding dim: {e}")))?;
    Ok(())
}

/// Returns true if `segment_vectors` contains at least one row.
async fn has_indexed_embeddings(conn: &Connection) -> Result<bool, OneupError> {
    let mut rows = conn
        .query(queries::SELECT_HAS_INDEXED_EMBEDDINGS, ())
        .await
        .map_err(|e| StorageError::Query(format!("failed to check for indexed embeddings: {e}")))?;

    match rows.next().await {
        Ok(Some(_)) => Ok(true),
        Ok(None) => Ok(false),
        Err(e) => Err(StorageError::Query(format!("indexed embeddings check failed: {e}")).into()),
    }
}

/// Verifies that the index was built with the current embedding model.
///
/// When no model metadata is recorded:
/// - If embeddings already exist (legacy index), requires a reindex so
///   vectors of unknown provenance are not mixed with new ones.
/// - If no embeddings exist yet, stamps the current model metadata.
///
/// When metadata exists but no embeddings have been written, the model
/// metadata is treated as unbound and can be updated freely — this avoids
/// forcing unnecessary reindexes when the model changes before any vectors
/// are stored.
///
/// If a different model is recorded *and* embeddings exist, returns an error
/// directing the user to run `1up reindex`.
pub async fn check_embedding_model_compatible(
    conn: &Connection,
    model_name: &str,
    dim: usize,
) -> Result<(), OneupError> {
    let stored = get_embedding_model(conn).await?;
    let has_vectors = has_indexed_embeddings(conn).await?;

    match stored {
        None if has_vectors => Err(reindex_required(
            "index contains embeddings from an unknown model".to_string(),
        )),
        None => set_embedding_model_meta(conn, model_name, dim).await,
        Some(ref s) if s == model_name => Ok(()),
        Some(_) if !has_vectors => set_embedding_model_meta(conn, model_name, dim).await,
        Some(stored) => Err(StorageError::Migration(format!(
            "index was built with embedding model '{stored}' but the current model is \
             '{model_name}'; run `1up reindex` to rebuild the index with the new model"
        ))
        .into()),
    }
}

/// Reads scope roots from the meta table for monorepo-scoped indexing.
///
/// Returns `None` if no scope has been set yet (unscoped index), or a JSON-serialized
/// vector of repo-relative directory roots if scope metadata is present. Fresh indexes
/// initialize scope as `None` (unscoped); existing unscoped indexes are migrated on
/// first write (no in-place migration).
pub async fn read_scope_from_meta(conn: &Connection) -> Result<Option<Vec<String>>, OneupError> {
    if !schema_object_exists(conn, "table", "meta").await? {
        return Ok(None);
    }

    let mut rows = conn
        .query(queries::SELECT_META, [META_KEY_SCOPE_ROOTS])
        .await
        .map_err(|e| StorageError::Query(format!("failed to read scope metadata: {e}")))?;

    match rows.next().await {
        Ok(Some(row)) => {
            let json_str: String = row.get(0).map_err(|e| {
                StorageError::Query(format!("failed to read scope metadata value: {e}"))
            })?;
            let roots: Vec<String> = serde_json::from_str(&json_str)
                .map_err(|e| StorageError::Query(format!("invalid scope metadata JSON: {e}")))?;
            Ok(Some(roots))
        }
        Ok(None) => Ok(None),
        Err(e) => Err(StorageError::Query(format!("scope metadata query failed: {e}")).into()),
    }
}

/// Writes scope roots to the meta table for monorepo-scoped indexing.
///
/// Persists a JSON-serialized vector of repo-relative directory roots. Called
/// after a successful incremental or full rebuild to ensure scope metadata survives
/// branch switches, daemon restarts, and context reloads.
pub async fn write_scope_to_meta(conn: &Connection, roots: &[String]) -> Result<(), OneupError> {
    let json_str = serde_json::to_string(roots)
        .map_err(|e| StorageError::Migration(format!("failed to serialize scope metadata: {e}")))?;
    conn.execute(queries::UPSERT_META, [META_KEY_SCOPE_ROOTS, &json_str])
        .await
        .map_err(|e| StorageError::Migration(format!("failed to write scope metadata: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::db::Db;

    async fn setup() -> (Db, Connection) {
        let db = Db::open_memory().await.unwrap();
        let conn = db.connect().unwrap();
        (db, conn)
    }

    #[tokio::test]
    async fn initialize_does_not_create_the_dead_file_hash_index() {
        // `idx_segments_file_hash` had no reader and was dropped. A fresh
        // index build must no longer create it, and it must not be a required object.
        let (_db, conn) = setup().await;
        initialize(&conn).await.unwrap();

        assert!(
            !schema_object_exists(&conn, "index", "idx_segments_file_hash")
                .await
                .unwrap(),
            "a fresh build must not create the dead idx_segments_file_hash index"
        );
        assert!(
            !REQUIRED_SCHEMA_OBJECTS
                .iter()
                .any(|(_, name)| *name == "idx_segments_file_hash"),
            "idx_segments_file_hash must not be a required schema object"
        );
        // The schema is still complete without it.
        ensure_current(&conn, &SchemaContext::unspecified())
            .await
            .expect("schema must be complete after dropping the dead index");
    }

    #[tokio::test]
    async fn initialize_omits_the_removed_content_key_index() {
        // `idx_segment_vectors_content_key` existed solely to back the removed
        // ANN fan-out join (`sv.content_key = p.content_key`); the exact scan
        // probes `embedding_pool` by its `content_key` primary key instead, so
        // a v20 build must neither create nor require it.
        let (_db, conn) = setup().await;
        initialize(&conn).await.unwrap();

        assert!(
            !schema_object_exists(&conn, "index", "idx_segment_vectors_content_key")
                .await
                .unwrap(),
            "a fresh build must not create the removed idx_segment_vectors_content_key index"
        );
        assert!(
            !REQUIRED_SCHEMA_OBJECTS
                .iter()
                .any(|(_, name)| *name == "idx_segment_vectors_content_key"),
            "idx_segment_vectors_content_key must not be a required schema object"
        );
        ensure_current(&conn, &SchemaContext::unspecified())
            .await
            .expect("schema must be complete without the removed index");
    }

    /// Build a 384-dimension zero-valued JSON vector literal for test fixtures.
    /// Format-agnostic: matches the production write path (`vector8(?)` takes JSON text)
    /// so the fixture does not encode a specific element byte size.
    fn zero_vector_json(dim: usize) -> String {
        let mut s = String::from("[");
        for i in 0..dim {
            if i > 0 {
                s.push(',');
            }
            s.push('0');
        }
        s.push(']');
        s
    }

    /// Seed one pooled embedding plus a `segment_vectors` reference to it,
    /// mirroring the content-addressed write path: the vector bytes live once in
    /// `embedding_pool` and `segment_vectors` carries only the `content_key`.
    /// Used to put the index into a "has embeddings" state for model-compat tests.
    async fn seed_pooled_vector(
        conn: &Connection,
        content_key: &str,
        segment_id: &str,
        vector_json: &str,
    ) {
        conn.execute(
            "INSERT INTO embedding_pool (content_key, embedding_vec, ref_count) VALUES (?1, vector8(?2), 1)",
            libsql::params![content_key, vector_json],
        )
        .await
        .unwrap();
        conn.execute(
            "INSERT INTO segment_vectors (segment_id, content_key) VALUES (?1, ?2)",
            libsql::params![segment_id, content_key],
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn check_embedding_model_compatible_records_on_first_run() {
        let (_db, conn) = setup().await;
        prepare_for_write(&conn).await.unwrap();

        check_embedding_model_compatible(&conn, "org/model-v1", 384)
            .await
            .unwrap();

        assert_eq!(
            get_embedding_model(&conn).await.unwrap(),
            Some("org/model-v1".to_string())
        );
    }

    #[tokio::test]
    async fn check_embedding_model_compatible_passes_for_same_model() {
        let (_db, conn) = setup().await;
        prepare_for_write(&conn).await.unwrap();

        check_embedding_model_compatible(&conn, "org/model-v1", 384)
            .await
            .unwrap();
        check_embedding_model_compatible(&conn, "org/model-v1", 384)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn check_embedding_model_compatible_allows_model_change_without_vectors() {
        let (_db, conn) = setup().await;
        prepare_for_write(&conn).await.unwrap();

        check_embedding_model_compatible(&conn, "org/model-v1", 384)
            .await
            .unwrap();

        check_embedding_model_compatible(&conn, "org/model-v2", 768)
            .await
            .unwrap();

        assert_eq!(
            get_embedding_model(&conn).await.unwrap(),
            Some("org/model-v2".to_string())
        );
    }

    #[tokio::test]
    async fn check_embedding_model_compatible_fails_for_different_model_with_vectors() {
        let (_db, conn) = setup().await;
        prepare_for_write(&conn).await.unwrap();

        check_embedding_model_compatible(&conn, "org/model-v1", 384)
            .await
            .unwrap();

        conn.execute(
            "INSERT INTO segments (id, file_path, language, block_type, content, line_start, line_end, complexity, file_hash) VALUES ('s1', 'f.rs', 'rust', 'function', 'fn f(){}', 1, 1, 0, 'abc')",
            (),
        )
        .await
        .unwrap();
        seed_pooled_vector(&conn, "k1", "s1", &zero_vector_json(384)).await;

        let err = check_embedding_model_compatible(&conn, "org/model-v2", 768)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("org/model-v1"), "should mention stored model");
        assert!(msg.contains("org/model-v2"), "should mention new model");
        assert!(msg.contains("run `1up reindex`"));
    }

    #[tokio::test]
    async fn check_embedding_model_compatible_rejects_legacy_index_with_vectors() {
        let (_db, conn) = setup().await;
        prepare_for_write(&conn).await.unwrap();

        conn.execute(
            "INSERT INTO segments (id, file_path, language, block_type, content, line_start, line_end, complexity, file_hash) VALUES ('s1', 'f.rs', 'rust', 'function', 'fn f(){}', 1, 1, 0, 'abc')",
            (),
        )
        .await
        .unwrap();
        seed_pooled_vector(&conn, "k1", "s1", &zero_vector_json(384)).await;

        let err = check_embedding_model_compatible(&conn, "org/model-v1", 384)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unknown model"),
            "should mention unknown model"
        );
        assert!(msg.contains("run `1up reindex`"));
    }

    #[tokio::test]
    async fn get_embedding_model_returns_none_before_any_indexing() {
        let (_db, conn) = setup().await;
        prepare_for_write(&conn).await.unwrap();

        assert_eq!(get_embedding_model(&conn).await.unwrap(), None);
    }

    #[tokio::test]
    async fn prepare_for_write_initializes_v20() {
        let (_db, conn) = setup().await;

        prepare_for_write(&conn).await.unwrap();

        assert_eq!(
            get_schema_version(&conn).await.unwrap(),
            Some(SCHEMA_VERSION)
        );
        assert_eq!(SCHEMA_VERSION, 20);
        assert!(schema_object_exists(&conn, "table", "worktree_contexts")
            .await
            .unwrap());
        assert!(schema_object_exists(&conn, "table", "embedding_pool")
            .await
            .unwrap());
        assert!(
            schema_object_exists(&conn, "index", "idx_segments_context_file_path")
                .await
                .unwrap()
        );
        // The shared vector bytes live on the pool now.
        let declared_type = embedding_pool_embedding_vec_type(&conn)
            .await
            .unwrap()
            .expect("embedding_pool.embedding_vec column should be present");
        assert!(
            declared_type.contains("FLOAT8") || declared_type.contains("F1BIT"),
            "expected embedding_vec declared type to contain FLOAT8 or F1BIT, got `{declared_type}`"
        );
        assert!(schema_object_exists(&conn, "table", "segment_symbols")
            .await
            .unwrap());
        assert!(schema_object_exists(&conn, "table", "segment_relations")
            .await
            .unwrap());
        assert!(
            schema_object_exists(&conn, "index", "idx_segment_symbols_exact")
                .await
                .unwrap()
        );
        assert!(
            schema_object_exists(&conn, "index", "idx_segment_symbols_prefix")
                .await
                .unwrap()
        );
        assert!(
            schema_object_exists(&conn, "index", "idx_segment_relations_source")
                .await
                .unwrap()
        );
        assert!(
            schema_object_exists(&conn, "index", "idx_segment_relations_target")
                .await
                .unwrap()
        );
        assert!(
            schema_object_exists(&conn, "index", "idx_segment_relations_lookup_target")
                .await
                .unwrap()
        );
        assert!(schema_object_exists(&conn, "trigger", "segments_symbol_ad")
            .await
            .unwrap());
        assert!(segment_vectors_has_content_key(&conn).await.unwrap());
        assert!(table_has_column(&conn, "embedding_pool", "ref_count")
            .await
            .unwrap());
        assert!(table_has_column(&conn, "segments", "context_id")
            .await
            .unwrap());
        assert!(table_has_column(&conn, "indexed_files", "context_id")
            .await
            .unwrap());
        assert!(table_has_column(&conn, "segment_symbols", "context_id")
            .await
            .unwrap());
        assert!(table_has_column(&conn, "segment_relations", "context_id")
            .await
            .unwrap());
        assert!(
            table_has_column(&conn, "segment_relations", "lookup_canonical_symbol")
                .await
                .unwrap()
        );
        assert!(
            table_has_column(&conn, "segment_relations", "qualifier_fingerprint")
                .await
                .unwrap()
        );
        assert!(
            table_has_column(&conn, "segment_relations", "edge_identity_kind")
                .await
                .unwrap()
        );
        ensure_current(&conn, &SchemaContext::unspecified())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn prepare_for_write_rejects_stale_schema_versions() {
        let (_db, conn) = setup().await;

        conn.execute(queries::CREATE_META_TABLE, ()).await.unwrap();
        conn.execute(queries::UPSERT_META, [META_KEY_SCHEMA_VERSION, "4"])
            .await
            .unwrap();

        let err = prepare_for_write(&conn).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("out of date"));
        assert!(msg.contains("run `1up reindex`"));
    }

    #[tokio::test]
    async fn prepare_for_write_rejects_prior_version_schema() {
        // Fail-closed at the previous -> current version boundary: an index one
        // version behind is refused with reindex guidance naming found vs
        // expected, forcing reindex with no in-place migration attempted.
        let (_db, conn) = setup().await;

        let prior = SCHEMA_VERSION - 1;
        conn.execute(queries::CREATE_META_TABLE, ()).await.unwrap();
        conn.execute(
            queries::UPSERT_META,
            [META_KEY_SCHEMA_VERSION, &prior.to_string()],
        )
        .await
        .unwrap();

        let err = prepare_for_write(&conn).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains(&format!("found v{prior}, expected v{SCHEMA_VERSION}")));
        assert!(msg.contains("run `1up reindex`"));
    }

    #[tokio::test]
    async fn ensure_current_rejects_partial_v10_schema() {
        let (_db, conn) = setup().await;

        conn.execute(
            "CREATE TABLE segments (
                id TEXT PRIMARY KEY,
                file_path TEXT NOT NULL,
                language TEXT NOT NULL,
                block_type TEXT NOT NULL,
                content TEXT NOT NULL,
                line_start INTEGER NOT NULL,
                line_end INTEGER NOT NULL,
                breadcrumb TEXT,
                complexity INTEGER NOT NULL DEFAULT 0,
                role TEXT NOT NULL DEFAULT 'DEFINITION',
                defined_symbols TEXT NOT NULL DEFAULT '[]',
                referenced_symbols TEXT NOT NULL DEFAULT '[]',
                called_symbols TEXT NOT NULL DEFAULT '[]',
                file_hash TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            )",
            (),
        )
        .await
        .unwrap();
        conn.execute(queries::CREATE_META_TABLE, ()).await.unwrap();
        conn.execute(
            queries::UPSERT_META,
            [META_KEY_SCHEMA_VERSION, &SCHEMA_VERSION.to_string()],
        )
        .await
        .unwrap();

        let err = ensure_current(&conn, &SchemaContext::unspecified())
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("incomplete"));
        assert!(msg.contains("run `1up reindex`"));
    }

    #[tokio::test]
    async fn ensure_current_reports_initializing_when_tables_present_but_version_absent() {
        // Reproduces the daemon-auto-start window: `initialize` creates the user
        // tables first and writes the `schema_version` row last (not one
        // transaction), so a concurrent reader can observe "tables exist, no
        // version". This must surface the transient initializing shape — *not* a
        // version-mismatch shape — so the MCP readiness path can ride it out /
        // degrade to `missing` instead of misreporting `stale`.
        let (_db, conn) = setup().await;
        conn.execute("CREATE TABLE segments (id TEXT PRIMARY KEY)", ())
            .await
            .unwrap();
        assert!(database_has_user_tables(&conn).await.unwrap());
        assert!(get_schema_version(&conn).await.unwrap().is_none());

        let err = ensure_current(&conn, &SchemaContext::unspecified())
            .await
            .unwrap_err();
        assert!(
            is_initializing_schema_error(&err),
            "tables-without-version must be classified as initializing, got: {err}"
        );
        assert!(err
            .to_string()
            .contains(SCHEMA_MISSING_OR_UNREADABLE_FRAGMENT));
    }

    #[tokio::test]
    async fn is_initializing_schema_error_ignores_genuine_version_mismatches() {
        // A real incompatible schema (out of date / newer than supported) must
        // never be mistaken for the transient initializing window, so the
        // readiness path keeps reporting those as `stale`.
        let (_db, older) = setup().await;
        seed_schema_version(&older, SCHEMA_VERSION - 1).await;
        let older_err = ensure_current(&older, &SchemaContext::unspecified())
            .await
            .unwrap_err();
        assert!(!is_initializing_schema_error(&older_err));

        let (_db2, newer) = setup().await;
        seed_schema_version(&newer, SCHEMA_VERSION + 1).await;
        let newer_err = ensure_current(&newer, &SchemaContext::unspecified())
            .await
            .unwrap_err();
        assert!(!is_initializing_schema_error(&newer_err));
    }

    #[tokio::test]
    async fn ensure_current_tolerating_init_passes_on_a_current_schema() {
        // The happy path: a fully-initialized index validates on the first
        // attempt, so read commands pay no retry cost once initialization has
        // settled. (The retry-until-settled behavior itself is exercised by the
        // integration suite, where reads race the daemon's first index.)
        let (_db, conn) = setup().await;
        initialize(&conn).await.unwrap();

        ensure_current_tolerating_init(&conn, &SchemaContext::unspecified())
            .await
            .expect("a current schema must validate without retrying");
    }

    #[tokio::test]
    async fn ensure_current_tolerating_init_does_not_wait_on_version_mismatch() {
        // Tolerating the initialization window must NOT mask a genuine
        // incompatible schema: a definitive version mismatch must fail on the
        // very first attempt with zero waits — surfaced with its reindex
        // guidance, never spending the (now several-second) schema-init budget
        // on a shape that can never resolve. We pass an absurd per-attempt
        // delay: if the loop erroneously slept even once (misclassifying the
        // mismatch as the transient init window) this test would hang for an
        // hour instead of returning immediately.
        let (_db, conn) = setup().await;
        seed_schema_version(&conn, SCHEMA_VERSION - 1).await;

        let err = ensure_current_tolerating_init_with_budget(
            &conn,
            &SchemaContext::unspecified(),
            SCHEMA_INIT_WAIT_ATTEMPTS,
            Duration::from_secs(3600),
        )
        .await
        .unwrap_err();
        assert!(!is_initializing_schema_error(&err));
        let msg = err.to_string();
        assert!(msg.contains("out of date"));
        assert!(msg.contains("run `1up reindex`"));
    }

    #[tokio::test]
    async fn ensure_current_tolerating_init_recovers_when_version_appears_within_budget() {
        // GitHub issue #93's happy resolution: a reader observes the "tables
        // present, version absent" init window while another process is running a
        // full schema initialization, then that initializer commits the
        // `schema_version` row within the wait budget, and the read succeeds
        // instead of surfacing a spurious "reindex required".
        //
        // A file-backed DB is required (unlike the other tests' `:memory:` DBs,
        // whose connections are isolated) so the reader and the concurrent
        // initializer share state; WAL (`connect_tuned`) lets the reader poll
        // without blocking on the writer's brief commit lock.
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().canonicalize().unwrap().join("project");
        std::fs::create_dir_all(&project_root).unwrap();
        let db = Db::open_rw(&crate::shared::config::project_db_path(&project_root))
            .await
            .unwrap();
        let reader = db.connect_tuned().await.unwrap();
        let writer = db.connect_tuned().await.unwrap();

        // Fully initialize, then remove only the version row to re-enter the
        // transient init window that `ensure_current` reports as `initializing`.
        initialize(&reader).await.unwrap();
        writer
            .execute("DELETE FROM meta WHERE key = ?1", [META_KEY_SCHEMA_VERSION])
            .await
            .unwrap();
        assert!(
            is_initializing_schema_error(
                &ensure_current(&reader, &SchemaContext::unspecified())
                    .await
                    .unwrap_err()
            ),
            "removing only the version row must reproduce the transient init shape"
        );

        // A concurrent initializer commits the version row shortly after the
        // reader starts waiting; the reader must ride out the window and succeed.
        let init_writer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            set_schema_version(&writer, SCHEMA_VERSION).await.unwrap();
        });

        ensure_current_tolerating_init_with_budget(
            &reader,
            &SchemaContext::unspecified(),
            SCHEMA_INIT_WAIT_ATTEMPTS,
            Duration::from_millis(20),
        )
        .await
        .expect("reader must recover once the version row is committed within budget");

        init_writer.await.unwrap();
    }

    #[tokio::test]
    async fn ensure_current_tolerating_init_exhausts_budget_with_clear_schema_error() {
        // Tables exist but the version row is never written (a stuck / crashed
        // initializer, or a wait budget too short for a very large index). After
        // exhausting its budget the tolerance loop must surface the same clear
        // initializing schema error `ensure_current` produces rather than hang
        // forever. A tiny zero-delay budget keeps the test fast while still
        // exercising the exhaustion path.
        let (_db, conn) = setup().await;
        conn.execute("CREATE TABLE segments (id TEXT PRIMARY KEY)", ())
            .await
            .unwrap();
        assert!(database_has_user_tables(&conn).await.unwrap());
        assert!(get_schema_version(&conn).await.unwrap().is_none());

        let err = ensure_current_tolerating_init_with_budget(
            &conn,
            &SchemaContext::unspecified(),
            3,
            Duration::ZERO,
        )
        .await
        .unwrap_err();
        assert!(
            is_initializing_schema_error(&err),
            "budget exhaustion must surface the transient initializing shape, got: {err}"
        );
        assert!(err
            .to_string()
            .contains(SCHEMA_MISSING_OR_UNREADABLE_FRAGMENT));
    }

    #[tokio::test]
    async fn ensure_current_rejects_schema_missing_edge_identity_kind() {
        let (_db, conn) = setup().await;

        conn.execute_batch(queries::DROP_SEARCH_SCHEMA)
            .await
            .unwrap();
        conn.execute_batch(
            &[
                queries::CREATE_WORKTREE_CONTEXTS_TABLE,
                queries::CREATE_SEGMENTS_TABLE,
                queries::CREATE_INDEX_FILE_PATH,
                queries::CREATE_INDEX_SEGMENTS_CONTEXT_FILE_PATH,
                queries::CREATE_INDEX_LANGUAGE,
                queries::CREATE_EMBEDDING_POOL_TABLE,
                queries::CREATE_SEGMENT_VECTORS_TABLE,
                queries::CREATE_SEGMENT_SYMBOLS_TABLE,
                "CREATE TABLE segment_relations (
            context_id TEXT NOT NULL DEFAULT 'default',
            source_segment_id TEXT NOT NULL,
            relation_kind TEXT NOT NULL,
            raw_target_symbol TEXT NOT NULL,
            canonical_target_symbol TEXT NOT NULL,
            lookup_canonical_symbol TEXT NOT NULL,
            qualifier_fingerprint TEXT NOT NULL,
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            PRIMARY KEY (
                context_id,
                source_segment_id,
                relation_kind,
                canonical_target_symbol,
                raw_target_symbol
            )
        )",
                queries::CREATE_INDEXED_FILES_TABLE,
                queries::CREATE_INDEX_SEGMENT_SYMBOLS_EXACT,
                queries::CREATE_INDEX_SEGMENT_SYMBOLS_PREFIX,
                queries::CREATE_INDEX_SEGMENT_RELATIONS_SOURCE,
                queries::CREATE_INDEX_SEGMENT_RELATIONS_TARGET,
                queries::CREATE_INDEX_SEGMENT_RELATIONS_LOOKUP_TARGET,
                queries::CREATE_FTS_TABLE,
                queries::CREATE_FTS_TRIGGERS,
                queries::CREATE_SEGMENT_SYMBOLS_TRIGGER,
                queries::CREATE_META_TABLE,
            ]
            .join(";"),
        )
        .await
        .unwrap();
        conn.execute(
            queries::UPSERT_META,
            [META_KEY_SCHEMA_VERSION, &SCHEMA_VERSION.to_string()],
        )
        .await
        .unwrap();

        let err = ensure_current(&conn, &SchemaContext::unspecified())
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("segment_relations.edge_identity_kind"));
        assert!(msg.contains("run `1up reindex`"));
    }

    /// Seed a meta-only DB at an arbitrary schema version so the version-mismatch
    /// branches of `ensure_current` can be exercised in isolation.
    async fn seed_schema_version(conn: &Connection, version: u32) {
        conn.execute(queries::CREATE_META_TABLE, ()).await.unwrap();
        conn.execute(
            queries::UPSERT_META,
            [META_KEY_SCHEMA_VERSION, &version.to_string()],
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn ensure_current_names_version_worktree_and_remediation_for_older_db() {
        let (_db, conn) = setup().await;
        seed_schema_version(&conn, SCHEMA_VERSION - 1).await;

        let db_path = Path::new("/repo/.1up/index.db");
        let worktree_path = Path::new("/repo/worktrees/feature-x");
        let err = ensure_current(&conn, &SchemaContext::new(db_path, worktree_path))
            .await
            .unwrap_err();
        let msg = err.to_string();

        // Mandated substring contract (consumed by start.rs::parse_schema_versions).
        assert!(
            msg.contains("out of date"),
            "older-DB error must say `out of date`; got: {msg}"
        );
        assert!(
            msg.contains(&format!("found v{}", SCHEMA_VERSION - 1)),
            "older-DB error must name the found version; got: {msg}"
        );
        assert!(
            msg.contains(&format!("expected v{SCHEMA_VERSION}")),
            "older-DB error must name the expected version; got: {msg}"
        );
        assert!(
            msg.contains("run `1up reindex`"),
            "older-DB error must state the reindex remediation; got: {msg}"
        );
        // Enrichment: the precise location-clause framing the cross-worktree
        // contract mandates — `for worktree '<wt>' sharing index '<db>'` — not
        // merely that the paths appear somewhere. A refactor that reworded the
        // clause (e.g. dropped `sharing index`) must fail here.
        assert!(
            msg.contains(
                "for worktree '/repo/worktrees/feature-x' sharing index '/repo/.1up/index.db'"
            ),
            "older-DB error must carry the exact worktree+shared-index location clause; got: {msg}"
        );
    }

    #[tokio::test]
    async fn ensure_current_names_version_worktree_and_remediation_for_newer_db() {
        let (_db, conn) = setup().await;
        seed_schema_version(&conn, SCHEMA_VERSION + 1).await;

        let db_path = Path::new("/repo/.1up/index.db");
        let worktree_path = Path::new("/repo/worktrees/feature-y");
        let err = ensure_current(&conn, &SchemaContext::new(db_path, worktree_path))
            .await
            .unwrap_err();
        let msg = err.to_string();

        // Mandated substring contract (consumed by start.rs::classify_schema_error
        // + parse_single_version, which reads the integer after "index schema v").
        assert!(
            msg.contains("newer than this binary supports"),
            "newer-DB error must say `newer than this binary supports`; got: {msg}"
        );
        assert!(
            msg.contains(&format!("index schema v{}", SCHEMA_VERSION + 1)),
            "newer-DB error must name the found version after `index schema v`; got: {msg}"
        );
        assert!(
            msg.contains(&format!("expected v{SCHEMA_VERSION}")),
            "newer-DB error must name the expected version; got: {msg}"
        );
        // Newer-than-supported direction: the remediation must direct the user
        // to upgrade the binary (NOT to reindex). This guards the second
        // recovery direction of the cross-worktree error contract.
        assert!(
            msg.contains("upgrade `1up`"),
            "newer-DB error must state the upgrade-`1up` remediation; got: {msg}"
        );
        assert!(
            !msg.contains("run `1up reindex`"),
            "newer-DB error must NOT offer the reindex remediation (wrong direction); got: {msg}"
        );
        // Enrichment: the precise location-clause framing the cross-worktree
        // contract mandates — `for worktree '<wt>' sharing index '<db>'`.
        assert!(
            msg.contains(
                "for worktree '/repo/worktrees/feature-y' sharing index '/repo/.1up/index.db'"
            ),
            "newer-DB error must carry the exact worktree+shared-index location clause; got: {msg}"
        );
    }

    #[tokio::test]
    async fn ensure_current_omits_location_clause_when_context_unspecified() {
        let (_db, conn) = setup().await;
        seed_schema_version(&conn, SCHEMA_VERSION - 1).await;

        let err = ensure_current(&conn, &SchemaContext::unspecified())
            .await
            .unwrap_err();
        let msg = err.to_string();

        // Substring contract still holds with no location supplied.
        assert!(msg.contains(&format!("found v{}", SCHEMA_VERSION - 1)));
        assert!(msg.contains(&format!("expected v{SCHEMA_VERSION}")));
        assert!(msg.contains("run `1up reindex`"));
        assert!(
            !msg.contains("for worktree"),
            "unspecified context must not render a location clause; got: {msg}"
        );
    }

    #[tokio::test]
    async fn read_scope_from_meta_returns_none_for_unscoped_index() {
        let (_db, conn) = setup().await;
        prepare_for_write(&conn).await.unwrap();

        let scope = read_scope_from_meta(&conn).await.unwrap();
        assert_eq!(scope, None, "fresh index should have no scope metadata");
    }

    #[tokio::test]
    async fn write_scope_to_meta_persists_roots() {
        let (_db, conn) = setup().await;
        prepare_for_write(&conn).await.unwrap();

        let roots = vec!["services/auth".to_string(), "libs/core".to_string()];
        write_scope_to_meta(&conn, &roots).await.unwrap();

        let stored = read_scope_from_meta(&conn).await.unwrap();
        assert_eq!(stored, Some(roots), "written scope should be readable");
    }

    #[tokio::test]
    async fn write_scope_to_meta_overwrites_existing_scope() {
        let (_db, conn) = setup().await;
        prepare_for_write(&conn).await.unwrap();

        let roots1 = vec!["services/auth".to_string()];
        write_scope_to_meta(&conn, &roots1).await.unwrap();

        let roots2 = vec![
            "services/auth".to_string(),
            "libs/core".to_string(),
            "tools".to_string(),
        ];
        write_scope_to_meta(&conn, &roots2).await.unwrap();

        let stored = read_scope_from_meta(&conn).await.unwrap();
        assert_eq!(
            stored,
            Some(roots2),
            "subsequent write should overwrite prior scope"
        );
    }

    #[tokio::test]
    async fn write_scope_to_meta_handles_empty_scope() {
        let (_db, conn) = setup().await;
        prepare_for_write(&conn).await.unwrap();

        let roots: Vec<String> = vec![];
        write_scope_to_meta(&conn, &roots).await.unwrap();

        let stored = read_scope_from_meta(&conn).await.unwrap();
        assert_eq!(
            stored,
            Some(roots),
            "empty scope vector should be persisted and readable"
        );
    }
}
