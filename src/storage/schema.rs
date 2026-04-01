use turso::Connection;

use crate::shared::constants::SCHEMA_VERSION;
use crate::shared::errors::{OneupError, StorageError};
use crate::storage::queries;

const META_KEY_SCHEMA_VERSION: &str = "schema_version";

/// Run all DDL statements to initialize the database schema.
/// This is idempotent -- safe to call on an already-initialized database.
pub async fn initialize(conn: &Connection) -> Result<(), OneupError> {
    conn.execute_batch(&format!(
        "{};{};{};{};{};",
        queries::CREATE_SEGMENTS_TABLE,
        queries::CREATE_INDEX_FILE_PATH,
        queries::CREATE_INDEX_LANGUAGE,
        queries::CREATE_INDEX_FILE_HASH,
        queries::CREATE_FTS_INDEX,
    ))
    .await
    .map_err(|e| StorageError::Migration(format!("failed to create segments schema: {e}")))?;

    conn.execute(queries::CREATE_META_TABLE, ())
        .await
        .map_err(|e| StorageError::Migration(format!("failed to create meta table: {e}")))?;

    set_schema_version(conn, SCHEMA_VERSION).await?;

    Ok(())
}

/// Read the current schema version from the meta table.
/// Returns None if no version is stored yet.
pub async fn get_schema_version(conn: &Connection) -> Result<Option<u32>, OneupError> {
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

/// Run any pending migrations to bring the database up to the current schema version.
/// Currently only supports creating the initial schema (version 1).
pub async fn migrate(conn: &Connection) -> Result<(), OneupError> {
    let current = get_schema_version(conn).await.unwrap_or(None);

    match current {
        None => {
            initialize(conn).await?;
        }
        Some(v) if v < SCHEMA_VERSION => {
            // Drop and recreate to handle schema changes (e.g. libsql -> turso FTS migration)
            conn.execute("DROP TABLE IF EXISTS segments", ())
                .await
                .map_err(|e| StorageError::Migration(format!("drop segments: {e}")))?;
            conn.execute("DROP TABLE IF EXISTS meta", ())
                .await
                .map_err(|e| StorageError::Migration(format!("drop meta: {e}")))?;
            initialize(conn).await?;
        }
        Some(_) => {}
    }

    Ok(())
}
