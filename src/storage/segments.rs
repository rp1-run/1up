use turso::Connection;

use crate::shared::errors::{OneupError, StorageError};
use crate::shared::types::SegmentRole;
use crate::storage::queries;

/// A stored segment row read from the database.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct StoredSegment {
    pub id: String,
    pub file_path: String,
    pub language: String,
    pub block_type: String,
    pub content: String,
    pub line_start: i64,
    pub line_end: i64,
    pub breadcrumb: Option<String>,
    pub complexity: i64,
    pub role: String,
    pub defined_symbols: String,
    pub referenced_symbols: String,
    pub called_symbols: String,
    pub file_hash: String,
    pub created_at: String,
    pub updated_at: String,
}

impl StoredSegment {
    /// Parse the role string back into a SegmentRole enum.
    #[allow(dead_code)]
    pub fn parsed_role(&self) -> SegmentRole {
        match self.role.as_str() {
            "DEFINITION" => SegmentRole::Definition,
            "IMPLEMENTATION" => SegmentRole::Implementation,
            "ORCHESTRATION" => SegmentRole::Orchestration,
            "IMPORT" => SegmentRole::Import,
            "DOCS" => SegmentRole::Docs,
            _ => SegmentRole::Definition,
        }
    }

    /// Parse defined_symbols JSON string into a Vec<String>.
    pub fn parsed_defined_symbols(&self) -> Vec<String> {
        serde_json::from_str(&self.defined_symbols).unwrap_or_default()
    }

    /// Parse referenced_symbols JSON string into a Vec<String>.
    pub fn parsed_referenced_symbols(&self) -> Vec<String> {
        serde_json::from_str(&self.referenced_symbols).unwrap_or_default()
    }

    /// Parse called_symbols JSON string into a Vec<String>.
    pub fn parsed_called_symbols(&self) -> Vec<String> {
        serde_json::from_str(&self.called_symbols).unwrap_or_default()
    }
}

/// Parameters for inserting or upserting a segment.
pub struct SegmentInsert {
    pub id: String,
    pub file_path: String,
    pub language: String,
    pub block_type: String,
    pub content: String,
    pub line_start: i64,
    pub line_end: i64,
    pub embedding: Option<String>,
    pub embedding_q8: Option<String>,
    pub breadcrumb: Option<String>,
    pub complexity: i64,
    pub role: String,
    pub defined_symbols: String,
    pub referenced_symbols: String,
    pub called_symbols: String,
    pub file_hash: String,
}

/// Insert or replace a segment in the database.
pub async fn upsert_segment(conn: &Connection, seg: &SegmentInsert) -> Result<(), OneupError> {
    let embedding = seg
        .embedding
        .as_ref()
        .map(|v| turso::Value::Text(v.clone()))
        .unwrap_or(turso::Value::Null);
    let embedding_q8 = seg
        .embedding_q8
        .as_ref()
        .map(|v| turso::Value::Text(v.clone()))
        .unwrap_or(turso::Value::Null);

    conn.execute(
        queries::UPSERT_SEGMENT,
        turso::params![
            seg.id.clone(),
            seg.file_path.clone(),
            seg.language.clone(),
            seg.block_type.clone(),
            seg.content.clone(),
            seg.line_start,
            seg.line_end,
            embedding,
            embedding_q8,
            seg.breadcrumb.clone(),
            seg.complexity,
            seg.role.clone(),
            seg.defined_symbols.clone(),
            seg.referenced_symbols.clone(),
            seg.called_symbols.clone(),
            seg.file_hash.clone(),
        ],
    )
    .await
    .map_err(|e| StorageError::Query(format!("upsert segment failed: {e}")))?;

    Ok(())
}

/// Query all segments for a given file path, ordered by line_start.
#[allow(dead_code)]
pub async fn get_segments_by_file(
    conn: &Connection,
    file_path: &str,
) -> Result<Vec<StoredSegment>, OneupError> {
    let mut rows = conn
        .query(queries::SELECT_SEGMENTS_BY_FILE, [file_path])
        .await
        .map_err(|e| StorageError::Query(format!("query segments by file failed: {e}")))?;

    let mut results = Vec::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|e| StorageError::Query(format!("row iteration failed: {e}")))?
    {
        results.push(row_to_stored_segment(&row)?);
    }
    Ok(results)
}

/// Get a single segment by its ID.
#[allow(dead_code)]
pub async fn get_segment_by_id(
    conn: &Connection,
    id: &str,
) -> Result<Option<StoredSegment>, OneupError> {
    let mut rows = conn
        .query(queries::SELECT_SEGMENT_BY_ID, [id])
        .await
        .map_err(|e| StorageError::Query(format!("query segment by id failed: {e}")))?;

    match rows
        .next()
        .await
        .map_err(|e| StorageError::Query(format!("row iteration failed: {e}")))?
    {
        Some(row) => Ok(Some(row_to_stored_segment(&row)?)),
        None => Ok(None),
    }
}

/// Delete all segments for a given file path.
pub async fn delete_segments_by_file(
    conn: &Connection,
    file_path: &str,
) -> Result<u64, OneupError> {
    let count = conn
        .execute(queries::DELETE_SEGMENTS_BY_FILE, [file_path])
        .await
        .map_err(|e| StorageError::Query(format!("delete segments by file failed: {e}")))?;
    Ok(count)
}

/// Get the stored file hash for a given file path (from the first segment found).
/// Returns None if no segments exist for this file.
pub async fn get_file_hash(
    conn: &Connection,
    file_path: &str,
) -> Result<Option<String>, OneupError> {
    let mut rows = conn
        .query(queries::SELECT_FILE_HASH, [file_path])
        .await
        .map_err(|e| StorageError::Query(format!("query file hash failed: {e}")))?;

    match rows
        .next()
        .await
        .map_err(|e| StorageError::Query(format!("row iteration failed: {e}")))?
    {
        Some(row) => {
            let hash: String = row
                .get(0)
                .map_err(|e| StorageError::Query(format!("read file_hash failed: {e}")))?;
            Ok(Some(hash))
        }
        None => Ok(None),
    }
}

/// Get all distinct file paths stored in the segments table.
pub async fn get_all_file_paths(conn: &Connection) -> Result<Vec<String>, OneupError> {
    let mut rows = conn
        .query(queries::SELECT_ALL_FILE_PATHS, ())
        .await
        .map_err(|e| StorageError::Query(format!("query all file paths failed: {e}")))?;

    let mut paths = Vec::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|e| StorageError::Query(format!("row iteration failed: {e}")))?
    {
        let path: String = row
            .get(0)
            .map_err(|e| StorageError::Query(format!("read file_path failed: {e}")))?;
        paths.push(path);
    }
    Ok(paths)
}

/// Get all distinct file paths for a given language.
pub async fn get_file_paths_by_language(
    conn: &Connection,
    language: &str,
) -> Result<Vec<String>, OneupError> {
    let mut rows = conn
        .query(queries::SELECT_FILE_PATHS_BY_LANGUAGE, [language])
        .await
        .map_err(|e| StorageError::Query(format!("query file paths by language failed: {e}")))?;

    let mut paths = Vec::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|e| StorageError::Query(format!("row iteration failed: {e}")))?
    {
        let path: String = row
            .get(0)
            .map_err(|e| StorageError::Query(format!("read file_path failed: {e}")))?;
        paths.push(path);
    }
    Ok(paths)
}

/// Set a key-value pair in the meta table.
#[allow(dead_code)]
pub async fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<(), OneupError> {
    conn.execute(queries::UPSERT_META, [key, value])
        .await
        .map_err(|e| StorageError::Query(format!("upsert meta failed: {e}")))?;
    Ok(())
}

/// Get a value from the meta table by key.
#[allow(dead_code)]
pub async fn get_meta(conn: &Connection, key: &str) -> Result<Option<String>, OneupError> {
    let mut rows = conn
        .query(queries::SELECT_META, [key])
        .await
        .map_err(|e| StorageError::Query(format!("query meta failed: {e}")))?;

    match rows
        .next()
        .await
        .map_err(|e| StorageError::Query(format!("row iteration failed: {e}")))?
    {
        Some(row) => {
            let val: String = row
                .get(0)
                .map_err(|e| StorageError::Query(format!("read meta value failed: {e}")))?;
            Ok(Some(val))
        }
        None => Ok(None),
    }
}

/// Delete a key from the meta table.
#[allow(dead_code)]
pub async fn delete_meta(conn: &Connection, key: &str) -> Result<(), OneupError> {
    conn.execute(queries::DELETE_META, [key])
        .await
        .map_err(|e| StorageError::Query(format!("delete meta failed: {e}")))?;
    Ok(())
}

/// Count total number of segments in the database.
pub async fn count_segments(conn: &Connection) -> Result<u64, OneupError> {
    let mut rows = conn
        .query(queries::COUNT_SEGMENTS, ())
        .await
        .map_err(|e| StorageError::Query(format!("count segments failed: {e}")))?;

    match rows
        .next()
        .await
        .map_err(|e| StorageError::Query(format!("row iteration failed: {e}")))?
    {
        Some(row) => {
            let count: i64 = row
                .get(0)
                .map_err(|e| StorageError::Query(format!("read count failed: {e}")))?;
            Ok(count as u64)
        }
        None => Ok(0),
    }
}

/// Count distinct file paths in the segments table.
pub async fn count_files(conn: &Connection) -> Result<u64, OneupError> {
    let mut rows = conn
        .query(queries::COUNT_FILES, ())
        .await
        .map_err(|e| StorageError::Query(format!("count files failed: {e}")))?;

    match rows
        .next()
        .await
        .map_err(|e| StorageError::Query(format!("row iteration failed: {e}")))?
    {
        Some(row) => {
            let count: i64 = row
                .get(0)
                .map_err(|e| StorageError::Query(format!("read count failed: {e}")))?;
            Ok(count as u64)
        }
        None => Ok(0),
    }
}

pub fn row_to_stored_segment(row: &turso::Row) -> Result<StoredSegment, OneupError> {
    Ok(StoredSegment {
        id: row
            .get(0)
            .map_err(|e| StorageError::Query(format!("read id failed: {e}")))?,
        file_path: row
            .get(1)
            .map_err(|e| StorageError::Query(format!("read file_path failed: {e}")))?,
        language: row
            .get(2)
            .map_err(|e| StorageError::Query(format!("read language failed: {e}")))?,
        block_type: row
            .get(3)
            .map_err(|e| StorageError::Query(format!("read block_type failed: {e}")))?,
        content: row
            .get(4)
            .map_err(|e| StorageError::Query(format!("read content failed: {e}")))?,
        line_start: row
            .get(5)
            .map_err(|e| StorageError::Query(format!("read line_start failed: {e}")))?,
        line_end: row
            .get(6)
            .map_err(|e| StorageError::Query(format!("read line_end failed: {e}")))?,
        breadcrumb: row
            .get(7)
            .map_err(|e| StorageError::Query(format!("read breadcrumb failed: {e}")))?,
        complexity: row
            .get(8)
            .map_err(|e| StorageError::Query(format!("read complexity failed: {e}")))?,
        role: row
            .get(9)
            .map_err(|e| StorageError::Query(format!("read role failed: {e}")))?,
        defined_symbols: row
            .get(10)
            .map_err(|e| StorageError::Query(format!("read defined_symbols failed: {e}")))?,
        referenced_symbols: row
            .get(11)
            .map_err(|e| StorageError::Query(format!("read referenced_symbols failed: {e}")))?,
        called_symbols: row
            .get(12)
            .map_err(|e| StorageError::Query(format!("read called_symbols failed: {e}")))?,
        file_hash: row
            .get(13)
            .map_err(|e| StorageError::Query(format!("read file_hash failed: {e}")))?,
        created_at: row
            .get(14)
            .map_err(|e| StorageError::Query(format!("read created_at failed: {e}")))?,
        updated_at: row
            .get(15)
            .map_err(|e| StorageError::Query(format!("read updated_at failed: {e}")))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{db::Db, schema};

    async fn setup() -> (Db, Connection) {
        let db = Db::open_memory().await.unwrap();
        let conn = db.connect().unwrap();
        schema::initialize(&conn).await.unwrap();
        (db, conn)
    }

    fn test_segment(id: &str, file_path: &str, file_hash: &str) -> SegmentInsert {
        SegmentInsert {
            id: id.to_string(),
            file_path: file_path.to_string(),
            language: "rust".to_string(),
            block_type: "function".to_string(),
            content: format!("fn {id}() {{ }}"),
            line_start: 1,
            line_end: 3,
            embedding: None,
            embedding_q8: None,
            breadcrumb: None,
            complexity: 1,
            role: "DEFINITION".to_string(),
            defined_symbols: format!("[\"{id}\"]"),
            referenced_symbols: "[]".to_string(),
            called_symbols: "[]".to_string(),
            file_hash: file_hash.to_string(),
        }
    }

    #[tokio::test]
    async fn upsert_and_query_by_file() {
        let (_db, conn) = setup().await;

        let seg = test_segment("seg1", "src/main.rs", "abc123");
        upsert_segment(&conn, &seg).await.unwrap();

        let results = get_segments_by_file(&conn, "src/main.rs").await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "seg1");
        assert_eq!(results[0].file_path, "src/main.rs");
        assert_eq!(results[0].language, "rust");
        assert_eq!(results[0].block_type, "function");
        assert_eq!(results[0].file_hash, "abc123");
    }

    #[tokio::test]
    async fn upsert_replaces_existing() {
        let (_db, conn) = setup().await;

        let seg1 = test_segment("seg1", "src/main.rs", "hash_v1");
        upsert_segment(&conn, &seg1).await.unwrap();

        let mut seg2 = test_segment("seg1", "src/main.rs", "hash_v2");
        seg2.content = "fn seg1_updated() { }".to_string();
        upsert_segment(&conn, &seg2).await.unwrap();

        let results = get_segments_by_file(&conn, "src/main.rs").await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].file_hash, "hash_v2");
        assert_eq!(results[0].content, "fn seg1_updated() { }");
    }

    #[tokio::test]
    async fn get_by_id() {
        let (_db, conn) = setup().await;

        let seg = test_segment("unique_id", "src/lib.rs", "hash1");
        upsert_segment(&conn, &seg).await.unwrap();

        let found = get_segment_by_id(&conn, "unique_id").await.unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().file_path, "src/lib.rs");

        let missing = get_segment_by_id(&conn, "nonexistent").await.unwrap();
        assert!(missing.is_none());
    }

    #[tokio::test]
    async fn delete_by_file() {
        let (_db, conn) = setup().await;

        upsert_segment(&conn, &test_segment("s1", "src/a.rs", "h1"))
            .await
            .unwrap();
        upsert_segment(&conn, &test_segment("s2", "src/a.rs", "h1"))
            .await
            .unwrap();
        upsert_segment(&conn, &test_segment("s3", "src/b.rs", "h2"))
            .await
            .unwrap();

        let deleted = delete_segments_by_file(&conn, "src/a.rs").await.unwrap();
        assert_eq!(deleted, 2);

        let remaining = get_segments_by_file(&conn, "src/a.rs").await.unwrap();
        assert!(remaining.is_empty());

        let other = get_segments_by_file(&conn, "src/b.rs").await.unwrap();
        assert_eq!(other.len(), 1);
    }

    #[tokio::test]
    async fn file_hash_lookup() {
        let (_db, conn) = setup().await;

        let hash = get_file_hash(&conn, "src/main.rs").await.unwrap();
        assert!(hash.is_none());

        upsert_segment(&conn, &test_segment("s1", "src/main.rs", "abc"))
            .await
            .unwrap();

        let hash = get_file_hash(&conn, "src/main.rs").await.unwrap();
        assert_eq!(hash, Some("abc".to_string()));
    }

    #[tokio::test]
    async fn all_file_paths() {
        let (_db, conn) = setup().await;

        upsert_segment(&conn, &test_segment("s1", "src/a.rs", "h"))
            .await
            .unwrap();
        upsert_segment(&conn, &test_segment("s2", "src/b.rs", "h"))
            .await
            .unwrap();
        upsert_segment(&conn, &test_segment("s3", "src/a.rs", "h"))
            .await
            .unwrap();

        let paths = get_all_file_paths(&conn).await.unwrap();
        assert_eq!(paths, vec!["src/a.rs", "src/b.rs"]);
    }

    #[tokio::test]
    async fn meta_crud() {
        let (_db, conn) = setup().await;

        assert!(get_meta(&conn, "test_key").await.unwrap().is_none());

        set_meta(&conn, "test_key", "test_value").await.unwrap();
        assert_eq!(
            get_meta(&conn, "test_key").await.unwrap(),
            Some("test_value".to_string())
        );

        set_meta(&conn, "test_key", "updated_value").await.unwrap();
        assert_eq!(
            get_meta(&conn, "test_key").await.unwrap(),
            Some("updated_value".to_string())
        );

        delete_meta(&conn, "test_key").await.unwrap();
        assert!(get_meta(&conn, "test_key").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn count_operations() {
        let (_db, conn) = setup().await;

        assert_eq!(count_segments(&conn).await.unwrap(), 0);
        assert_eq!(count_files(&conn).await.unwrap(), 0);

        upsert_segment(&conn, &test_segment("s1", "src/a.rs", "h"))
            .await
            .unwrap();
        upsert_segment(&conn, &test_segment("s2", "src/a.rs", "h"))
            .await
            .unwrap();
        upsert_segment(&conn, &test_segment("s3", "src/b.rs", "h"))
            .await
            .unwrap();

        assert_eq!(count_segments(&conn).await.unwrap(), 3);
        assert_eq!(count_files(&conn).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn schema_versioning() {
        let (_db, conn) = setup().await;

        let version = schema::get_schema_version(&conn).await.unwrap();
        assert_eq!(version, Some(crate::shared::constants::SCHEMA_VERSION));
    }

    #[tokio::test]
    async fn migrate_idempotent() {
        let (_db, conn) = setup().await;

        schema::migrate(&conn).await.unwrap();
        schema::migrate(&conn).await.unwrap();

        let version = schema::get_schema_version(&conn).await.unwrap();
        assert_eq!(version, Some(crate::shared::constants::SCHEMA_VERSION));
    }

    #[tokio::test]
    async fn stored_segment_helpers() {
        let (_db, conn) = setup().await;

        let mut seg = test_segment("s1", "src/main.rs", "h");
        seg.defined_symbols = r#"["foo","bar"]"#.to_string();
        seg.referenced_symbols = r#"["baz"]"#.to_string();
        seg.called_symbols = r#"["qux"]"#.to_string();
        seg.role = "IMPLEMENTATION".to_string();
        upsert_segment(&conn, &seg).await.unwrap();

        let results = get_segments_by_file(&conn, "src/main.rs").await.unwrap();
        let stored = &results[0];

        assert_eq!(stored.parsed_role(), SegmentRole::Implementation);
        assert_eq!(stored.parsed_defined_symbols(), vec!["foo", "bar"]);
        assert_eq!(stored.parsed_referenced_symbols(), vec!["baz"]);
        assert_eq!(stored.parsed_called_symbols(), vec!["qux"]);
    }
}
