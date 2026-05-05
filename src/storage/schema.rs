use std::thread;
use std::time::Duration;

use libsql::Connection;

use crate::shared::constants::{DB_LOCK_RETRY_ATTEMPTS, DB_LOCK_RETRY_DELAY_MS, SCHEMA_VERSION};
use crate::shared::errors::{OneupError, StorageError};
use crate::storage::db::is_lock_error;
use crate::storage::queries;

const META_KEY_SCHEMA_VERSION: &str = "schema_version";
const META_KEY_EMBEDDING_MODEL: &str = "embedding_model";
const META_KEY_EMBEDDING_DIM: &str = "embedding_dim";
const REQUIRED_SCHEMA_OBJECTS: &[(&str, &str)] = &[
    ("table", "worktree_contexts"),
    ("table", "segments"),
    ("table", "segment_vectors"),
    ("table", "segment_symbols"),
    ("table", "segment_relations"),
    ("table", "indexed_files"),
    ("table", "segments_fts"),
    ("table", "meta"),
    ("index", "idx_segments_file_path"),
    ("index", "idx_segments_context_file_path"),
    ("index", "idx_segments_language"),
    ("index", "idx_segments_file_hash"),
    ("index", "idx_segment_vectors_embedding"),
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
        queries::CREATE_INDEX_FILE_HASH,
        queries::CREATE_SEGMENT_VECTORS_TABLE,
        queries::CREATE_SEGMENT_SYMBOLS_TABLE,
        queries::CREATE_SEGMENT_RELATIONS_TABLE,
        queries::CREATE_INDEXED_FILES_TABLE,
    ))
    .await
    .map_err(|e| StorageError::Migration(format!("failed to create segments schema: {e}")))?;

    conn.execute(queries::CREATE_INDEX_SEGMENT_VECTORS_EMBEDDING, ())
        .await
        .map_err(|e| StorageError::Migration(format!("failed to create vector index: {e}")))?;

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

/// Create the current schema for an empty database or require explicit rebuild guidance.
pub async fn prepare_for_write(conn: &Connection) -> Result<(), OneupError> {
    if database_has_user_tables(conn).await? {
        ensure_current(conn).await
    } else {
        initialize(conn).await
    }
}

/// Drop all search objects and recreate the current schema from scratch.
pub async fn rebuild(conn: &Connection) -> Result<(), OneupError> {
    conn.execute_batch(queries::DROP_SEARCH_SCHEMA)
        .await
        .map_err(|e| StorageError::Migration(format!("failed to reset search schema: {e}")))?;
    initialize(conn).await
}

/// Verify that an existing database matches the current schema without mutating it.
pub async fn ensure_current(conn: &Connection) -> Result<(), OneupError> {
    let current = get_schema_version(conn).await?;

    match current {
        Some(v) if v == SCHEMA_VERSION => validate_required_objects(conn).await,
        Some(v) if v < SCHEMA_VERSION => Err(reindex_required(format!(
            "index schema is out of date (found v{v}, expected v{SCHEMA_VERSION})"
        ))),
        Some(v) => Err(StorageError::Migration(format!(
            "index schema v{v} is newer than this binary supports (expected v{SCHEMA_VERSION}); rebuild with a compatible binary or upgrade `1up`"
        ))
        .into()),
        None => {
            if database_has_user_tables(conn).await? {
                Err(reindex_required(
                    "index schema is missing or unreadable".to_string(),
                ))
            } else {
                Err(reindex_required("index is missing".to_string()))
            }
        }
    }
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
    let query = format!("SELECT 1 FROM pragma_table_info('{table_name}') WHERE name = ?1 LIMIT 1");
    let mut rows = conn.query(&query, [column_name]).await.map_err(|e| {
        StorageError::Query(format!(
            "failed to inspect table column {table_name}.{column_name}: {e}"
        ))
    })?;

    match rows.next().await {
        Ok(Some(_)) => Ok(true),
        Ok(None) => Ok(false),
        Err(e) => Err(StorageError::Query(format!(
            "table column inspection failed for {table_name}.{column_name}: {e}"
        ))
        .into()),
    }
}

async fn segment_vectors_has_embedding_vec(conn: &Connection) -> Result<bool, OneupError> {
    table_has_column(conn, "segment_vectors", "embedding_vec").await
}

#[cfg(test)]
async fn segment_vectors_embedding_vec_type(
    conn: &Connection,
) -> Result<Option<String>, OneupError> {
    let mut rows = conn
        .query(
            "SELECT type FROM pragma_table_info('segment_vectors') WHERE name = ?1 LIMIT 1",
            ["embedding_vec"],
        )
        .await
        .map_err(|e| {
            StorageError::Query(format!(
                "failed to read segment_vectors.embedding_vec type: {e}"
            ))
        })?;

    match rows.next().await {
        Ok(Some(row)) => {
            let ty: String = row.get(0).map_err(|e| {
                StorageError::Query(format!(
                    "failed to read segment_vectors.embedding_vec type value: {e}"
                ))
            })?;
            Ok(Some(ty))
        }
        Ok(None) => Ok(None),
        Err(e) => Err(StorageError::Query(format!(
            "segment_vectors.embedding_vec type inspection failed: {e}"
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

    if !segment_vectors_has_embedding_vec(conn).await? {
        return Err(reindex_required(format!(
            "index schema v{SCHEMA_VERSION} is incomplete (missing required column `segment_vectors.embedding_vec`)"
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::db::Db;

    async fn setup() -> (Db, Connection) {
        let db = Db::open_memory().await.unwrap();
        let conn = db.connect().unwrap();
        (db, conn)
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
        conn.execute(
            "INSERT INTO segment_vectors (segment_id, embedding_vec) VALUES ('s1', vector8(?1))",
            [zero_vector_json(384)],
        )
        .await
        .unwrap();

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
        conn.execute(
            "INSERT INTO segment_vectors (segment_id, embedding_vec) VALUES ('s1', vector8(?1))",
            [zero_vector_json(384)],
        )
        .await
        .unwrap();

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
    async fn prepare_for_write_initializes_v13() {
        let (_db, conn) = setup().await;

        prepare_for_write(&conn).await.unwrap();

        assert_eq!(
            get_schema_version(&conn).await.unwrap(),
            Some(SCHEMA_VERSION)
        );
        assert_eq!(SCHEMA_VERSION, 13);
        assert!(schema_object_exists(&conn, "table", "worktree_contexts")
            .await
            .unwrap());
        assert!(
            schema_object_exists(&conn, "index", "idx_segment_vectors_embedding")
                .await
                .unwrap()
        );
        assert!(
            schema_object_exists(&conn, "index", "idx_segments_context_file_path")
                .await
                .unwrap()
        );
        let declared_type = segment_vectors_embedding_vec_type(&conn)
            .await
            .unwrap()
            .expect("embedding_vec column should be present");
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
        assert!(segment_vectors_has_embedding_vec(&conn).await.unwrap());
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
        ensure_current(&conn).await.unwrap();
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
    async fn prepare_for_write_rejects_pre_v13_schema() {
        let (_db, conn) = setup().await;

        conn.execute(queries::CREATE_META_TABLE, ()).await.unwrap();
        conn.execute(queries::UPSERT_META, [META_KEY_SCHEMA_VERSION, "12"])
            .await
            .unwrap();

        let err = prepare_for_write(&conn).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("found v12, expected v13"));
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

        let err = ensure_current(&conn).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("incomplete"));
        assert!(msg.contains("run `1up reindex`"));
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
                queries::CREATE_INDEX_FILE_HASH,
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
        conn.execute(queries::CREATE_INDEX_SEGMENT_VECTORS_EMBEDDING, ())
            .await
            .unwrap();
        conn.execute(
            queries::UPSERT_META,
            [META_KEY_SCHEMA_VERSION, &SCHEMA_VERSION.to_string()],
        )
        .await
        .unwrap();

        let err = ensure_current(&conn).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("segment_relations.edge_identity_kind"));
        assert!(msg.contains("run `1up reindex`"));
    }

    #[tokio::test]
    async fn rebuild_recreates_current_schema_from_stale_database() {
        let (_db, conn) = setup().await;

        conn.execute(queries::CREATE_META_TABLE, ()).await.unwrap();
        conn.execute(queries::UPSERT_META, [META_KEY_SCHEMA_VERSION, "4"])
            .await
            .unwrap();

        rebuild(&conn).await.unwrap();

        assert_eq!(
            get_schema_version(&conn).await.unwrap(),
            Some(SCHEMA_VERSION)
        );
        ensure_current(&conn).await.unwrap();
    }
}
