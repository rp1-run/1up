use libsql::Connection;

use crate::shared::constants::SCHEMA_VERSION;
use crate::shared::errors::{OneupError, StorageError};
use crate::storage::queries;

const META_KEY_SCHEMA_VERSION: &str = "schema_version";
const REQUIRED_SCHEMA_OBJECTS: &[(&str, &str)] = &[
    ("table", "segments"),
    ("table", "segments_fts"),
    ("table", "meta"),
    ("index", "idx_segments_file_path"),
    ("index", "idx_segments_language"),
    ("index", "idx_segments_file_hash"),
    ("index", "idx_segments_embedding"),
    ("trigger", "segments_ai"),
    ("trigger", "segments_ad"),
    ("trigger", "segments_au"),
];

/// Run all DDL statements to initialize the database schema.
/// This only creates the current schema version for fresh or explicitly rebuilt indexes.
pub async fn initialize(conn: &Connection) -> Result<(), OneupError> {
    conn.execute_batch(&format!(
        "{};{};{};{};{}",
        queries::CREATE_SEGMENTS_TABLE,
        queries::CREATE_INDEX_FILE_PATH,
        queries::CREATE_INDEX_LANGUAGE,
        queries::CREATE_INDEX_FILE_HASH,
        queries::CREATE_INDEX_EMBEDDING_VEC,
    ))
    .await
    .map_err(|e| StorageError::Migration(format!("failed to create segments schema: {e}")))?;

    // FTS5 virtual table and sync triggers
    conn.execute_batch(queries::CREATE_FTS_TABLE)
        .await
        .map_err(|e| StorageError::Migration(format!("failed to create FTS table: {e}")))?;

    conn.execute_batch(queries::CREATE_FTS_TRIGGERS)
        .await
        .map_err(|e| StorageError::Migration(format!("failed to create FTS triggers: {e}")))?;

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
    let mut rows = conn
        .query(queries::SELECT_SCHEMA_OBJECT, [object_type, name])
        .await
        .map_err(|e| {
            StorageError::Query(format!(
                "failed to inspect schema object {object_type} `{name}`: {e}"
            ))
        })?;

    match rows.next().await {
        Ok(Some(_)) => Ok(true),
        Ok(None) => Ok(false),
        Err(e) => Err(StorageError::Query(format!(
            "schema object inspection failed for {object_type} `{name}`: {e}"
        ))
        .into()),
    }
}

async fn segments_has_embedding_vec(conn: &Connection) -> Result<bool, OneupError> {
    let mut rows = conn
        .query(queries::SELECT_SEGMENTS_EMBEDDING_VEC_COLUMN, ())
        .await
        .map_err(|e| StorageError::Query(format!("failed to inspect segments columns: {e}")))?;

    match rows.next().await {
        Ok(Some(_)) => Ok(true),
        Ok(None) => Ok(false),
        Err(e) => {
            Err(StorageError::Query(format!("segments column inspection failed: {e}")).into())
        }
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

    if !segments_has_embedding_vec(conn).await? {
        return Err(reindex_required(format!(
            "index schema v{SCHEMA_VERSION} is incomplete (missing required column `segments.embedding_vec`)"
        )));
    }

    Ok(())
}

fn reindex_required(message: String) -> OneupError {
    StorageError::Migration(format!("{message}; run `1up reindex`")).into()
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
    async fn prepare_for_write_initializes_empty_database() {
        let (_db, conn) = setup().await;

        prepare_for_write(&conn).await.unwrap();

        assert_eq!(
            get_schema_version(&conn).await.unwrap(),
            Some(SCHEMA_VERSION)
        );
        assert!(
            schema_object_exists(&conn, "index", "idx_segments_embedding")
                .await
                .unwrap()
        );
        assert!(segments_has_embedding_vec(&conn).await.unwrap());
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
    async fn ensure_current_rejects_partial_v5_schema() {
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
        conn.execute(queries::UPSERT_META, [META_KEY_SCHEMA_VERSION, "5"])
            .await
            .unwrap();

        let err = ensure_current(&conn).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("incomplete"));
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
