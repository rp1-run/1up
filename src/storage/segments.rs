use std::collections::{HashMap, HashSet};

use libsql::Connection;

use crate::shared::errors::{OneupError, StorageError};
use crate::shared::symbols::normalize_symbolish;
use crate::shared::types::{ReferenceKind, SegmentRole};
use crate::storage::queries;
use crate::storage::relations::{self, RelationInsert};

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
    pub embedding_vec: Option<String>,
    pub breadcrumb: Option<String>,
    pub complexity: i64,
    pub role: String,
    pub defined_symbols: String,
    pub referenced_symbols: String,
    pub called_symbols: String,
    pub file_hash: String,
}

/// Parameters for replacing one file's indexed contents inside a batch transaction.
#[derive(Clone, Copy)]
#[allow(dead_code)]
pub struct FileSegmentBatch<'a> {
    pub file_path: &'a str,
    pub segments: &'a [SegmentInsert],
}

struct SegmentSymbolInsert {
    symbol: String,
    canonical_symbol: String,
    reference_kind: ReferenceKind,
}

/// Insert or replace a segment in the database.
#[allow(dead_code)]
pub async fn upsert_segment(conn: &Connection, seg: &SegmentInsert) -> Result<(), OneupError> {
    upsert_segment_record(conn, seg).await?;
    relations::replace_segment_relations(conn, &seg.id, &build_segment_relation_rows(seg)).await?;

    Ok(())
}

async fn upsert_segment_record(conn: &Connection, seg: &SegmentInsert) -> Result<(), OneupError> {
    conn.execute(
        queries::UPSERT_SEGMENT,
        libsql::params![
            seg.id.clone(),
            seg.file_path.clone(),
            seg.language.clone(),
            seg.block_type.clone(),
            seg.content.clone(),
            seg.line_start,
            seg.line_end,
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

    if let Some(embedding_vec) = &seg.embedding_vec {
        conn.execute(
            queries::UPSERT_SEGMENT_VECTOR,
            libsql::params![seg.id.clone(), embedding_vec.clone()],
        )
        .await
        .map_err(|e| StorageError::Query(format!("upsert segment vector failed: {e}")))?;
    } else {
        conn.execute(queries::DELETE_SEGMENT_VECTOR, [seg.id.clone()])
            .await
            .map_err(|e| StorageError::Query(format!("delete segment vector failed: {e}")))?;
    }

    replace_segment_symbols(conn, seg).await?;

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

/// Get the stored file hash for every indexed file path.
#[allow(dead_code)]
pub async fn get_all_file_hashes(conn: &Connection) -> Result<HashMap<String, String>, OneupError> {
    let mut rows = conn
        .query(queries::SELECT_ALL_FILE_HASHES, ())
        .await
        .map_err(|e| StorageError::Query(format!("query all file hashes failed: {e}")))?;

    let mut hashes = HashMap::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|e| StorageError::Query(format!("row iteration failed: {e}")))?
    {
        let file_path: String = row
            .get(0)
            .map_err(|e| StorageError::Query(format!("read file_path failed: {e}")))?;
        let file_hash: String = row
            .get(1)
            .map_err(|e| StorageError::Query(format!("read file_hash failed: {e}")))?;
        hashes.insert(file_path, file_hash);
    }

    Ok(hashes)
}

/// Delete all segments for a given file path.
pub async fn delete_segments_by_file(
    conn: &Connection,
    file_path: &str,
) -> Result<u64, OneupError> {
    relations::delete_relations_by_file(conn, file_path).await?;

    let count = conn
        .execute(queries::DELETE_SEGMENTS_BY_FILE, [file_path])
        .await
        .map_err(|e| StorageError::Query(format!("delete segments by file failed: {e}")))?;
    Ok(count)
}

/// Get the stored file hash for a given file path (from the first segment found).
/// Returns None if no segments exist for this file.
#[allow(dead_code)]
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

/// Replace all stored segments for a single file in one transaction.
#[allow(dead_code)]
pub async fn replace_file_segments_tx(
    conn: &Connection,
    file_path: &str,
    segments: &[SegmentInsert],
) -> Result<(), OneupError> {
    validate_replace_segments(file_path, segments)?;

    let tx = conn.transaction().await.map_err(|e| {
        StorageError::Transaction(format!("begin file replace transaction failed: {e}"))
    })?;

    replace_file_segments_in_transaction(&tx, file_path, segments).await?;

    tx.commit().await.map_err(|e| {
        StorageError::Transaction(format!("commit file replace transaction failed: {e}"))
    })?;

    Ok(())
}

/// Replace stored segments for multiple files in one transaction.
#[allow(dead_code)]
pub async fn replace_file_batch_tx(
    conn: &Connection,
    batches: &[FileSegmentBatch<'_>],
) -> Result<(), OneupError> {
    validate_replace_batches(batches)?;

    let tx = conn.transaction().await.map_err(|e| {
        StorageError::Transaction(format!("begin file batch replace transaction failed: {e}"))
    })?;

    for batch in batches {
        replace_file_segments_in_transaction(&tx, batch.file_path, batch.segments).await?;
    }

    tx.commit().await.map_err(|e| {
        StorageError::Transaction(format!("commit file batch replace transaction failed: {e}"))
    })?;

    Ok(())
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

#[allow(dead_code)]
fn validate_replace_segments(
    file_path: &str,
    segments: &[SegmentInsert],
) -> Result<(), OneupError> {
    for segment in segments {
        if segment.file_path != file_path {
            return Err(StorageError::Transaction(format!(
                "replace transaction for '{file_path}' received segment '{}' for '{}'",
                segment.id, segment.file_path
            ))
            .into());
        }
    }

    Ok(())
}

fn parse_symbols(value: &str) -> Vec<String> {
    serde_json::from_str(value).unwrap_or_default()
}

fn reference_kind_label(reference_kind: ReferenceKind) -> &'static str {
    match reference_kind {
        ReferenceKind::Definition => "definition",
        ReferenceKind::Usage => "usage",
    }
}

fn build_segment_symbol_rows(seg: &SegmentInsert) -> Vec<SegmentSymbolInsert> {
    let mut rows = Vec::new();
    let mut seen = HashSet::new();

    for (symbols, reference_kind) in [
        (
            parse_symbols(&seg.defined_symbols),
            ReferenceKind::Definition,
        ),
        (parse_symbols(&seg.referenced_symbols), ReferenceKind::Usage),
    ] {
        for symbol in symbols {
            let canonical_symbol = normalize_symbolish(&symbol);
            if canonical_symbol.is_empty() {
                continue;
            }

            let dedupe_key = (
                reference_kind_label(reference_kind).to_string(),
                canonical_symbol.clone(),
            );
            if seen.insert(dedupe_key) {
                rows.push(SegmentSymbolInsert {
                    symbol,
                    canonical_symbol,
                    reference_kind,
                });
            }
        }
    }

    rows
}

fn build_segment_relation_rows(seg: &SegmentInsert) -> Vec<RelationInsert> {
    relations::build_relation_inserts(
        &seg.id,
        &parse_symbols(&seg.called_symbols),
        &parse_symbols(&seg.referenced_symbols),
    )
}

async fn replace_segment_symbols(conn: &Connection, seg: &SegmentInsert) -> Result<(), OneupError> {
    conn.execute(
        queries::DELETE_SEGMENT_SYMBOLS_BY_SEGMENT_ID,
        [seg.id.clone()],
    )
    .await
    .map_err(|e| StorageError::Query(format!("delete segment symbols failed: {e}")))?;

    for symbol in build_segment_symbol_rows(seg) {
        conn.execute(
            queries::INSERT_SEGMENT_SYMBOL,
            libsql::params![
                seg.id.clone(),
                symbol.symbol,
                symbol.canonical_symbol,
                reference_kind_label(symbol.reference_kind),
            ],
        )
        .await
        .map_err(|e| StorageError::Query(format!("insert segment symbol failed: {e}")))?;
    }

    Ok(())
}

#[allow(dead_code)]
fn validate_replace_batches(batches: &[FileSegmentBatch<'_>]) -> Result<(), OneupError> {
    let mut seen_paths = HashSet::new();

    for batch in batches {
        if !seen_paths.insert(batch.file_path) {
            return Err(StorageError::Transaction(format!(
                "batch replace received duplicate file path '{}'",
                batch.file_path
            ))
            .into());
        }

        validate_replace_segments(batch.file_path, batch.segments)?;
    }

    Ok(())
}

#[allow(dead_code)]
async fn replace_file_segments_in_transaction(
    conn: &Connection,
    file_path: &str,
    segments: &[SegmentInsert],
) -> Result<(), OneupError> {
    delete_segments_by_file(conn, file_path).await?;

    let mut relation_rows = Vec::new();
    for segment in segments {
        upsert_segment_record(conn, segment).await?;
        relation_rows.extend(build_segment_relation_rows(segment));
    }

    relations::insert_relations(conn, &relation_rows).await?;

    Ok(())
}

pub fn row_to_stored_segment(row: &libsql::Row) -> Result<StoredSegment, OneupError> {
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
            embedding_vec: None,
            breadcrumb: None,
            complexity: 1,
            role: "DEFINITION".to_string(),
            defined_symbols: format!("[\"{id}\"]"),
            referenced_symbols: "[]".to_string(),
            called_symbols: "[]".to_string(),
            file_hash: file_hash.to_string(),
        }
    }

    async fn symbol_rows(conn: &Connection, segment_id: &str) -> Vec<(String, String, String)> {
        let mut rows = conn
            .query(
                "SELECT symbol, canonical_symbol, reference_kind
                 FROM segment_symbols
                 WHERE segment_id = ?1
                 ORDER BY reference_kind, canonical_symbol",
                [segment_id],
            )
            .await
            .unwrap();

        let mut results = Vec::new();
        while let Some(row) = rows.next().await.unwrap() {
            results.push((
                row.get(0).unwrap(),
                row.get(1).unwrap(),
                row.get(2).unwrap(),
            ));
        }

        results
    }

    async fn relation_rows(conn: &Connection, segment_id: &str) -> Vec<(String, String, String)> {
        let mut rows = conn
            .query(
                "SELECT relation_kind, raw_target_symbol, canonical_target_symbol
                 FROM segment_relations
                 WHERE source_segment_id = ?1
                 ORDER BY relation_kind, canonical_target_symbol",
                [segment_id],
            )
            .await
            .unwrap();

        let mut results = Vec::new();
        while let Some(row) = rows.next().await.unwrap() {
            results.push((
                row.get(0).unwrap(),
                row.get(1).unwrap(),
                row.get(2).unwrap(),
            ));
        }

        results
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

        let mut segment_a1 = test_segment("s1", "src/a.rs", "h1");
        segment_a1.called_symbols = r#"["load_config"]"#.to_string();
        upsert_segment(&conn, &segment_a1).await.unwrap();
        upsert_segment(&conn, &test_segment("s2", "src/a.rs", "h1"))
            .await
            .unwrap();
        let mut segment_b = test_segment("s3", "src/b.rs", "h2");
        segment_b.referenced_symbols = r#"["ConfigLoader"]"#.to_string();
        upsert_segment(&conn, &segment_b).await.unwrap();

        let deleted = delete_segments_by_file(&conn, "src/a.rs").await.unwrap();
        assert_eq!(deleted, 2);

        let remaining = get_segments_by_file(&conn, "src/a.rs").await.unwrap();
        assert!(remaining.is_empty());

        let other = get_segments_by_file(&conn, "src/b.rs").await.unwrap();
        assert_eq!(other.len(), 1);
        assert!(relation_rows(&conn, "s1").await.is_empty());
        assert_eq!(
            relation_rows(&conn, "s3").await,
            vec![(
                "reference".to_string(),
                "ConfigLoader".to_string(),
                "configloader".to_string(),
            )]
        );
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
    async fn all_file_hashes_are_preloaded_once_per_file() {
        let (_db, conn) = setup().await;

        upsert_segment(&conn, &test_segment("s1", "src/a.rs", "hash-a"))
            .await
            .unwrap();
        upsert_segment(&conn, &test_segment("s2", "src/a.rs", "hash-a"))
            .await
            .unwrap();
        upsert_segment(&conn, &test_segment("s3", "src/b.rs", "hash-b"))
            .await
            .unwrap();

        let hashes = get_all_file_hashes(&conn).await.unwrap();

        assert_eq!(hashes.len(), 2);
        assert_eq!(hashes.get("src/a.rs"), Some(&"hash-a".to_string()));
        assert_eq!(hashes.get("src/b.rs"), Some(&"hash-b".to_string()));
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
    async fn prepare_for_write_is_idempotent() {
        let (_db, conn) = setup().await;

        schema::prepare_for_write(&conn).await.unwrap();
        schema::prepare_for_write(&conn).await.unwrap();

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

    #[tokio::test]
    async fn upsert_stores_native_vector_embeddings() {
        let (_db, conn) = setup().await;

        let mut seg = test_segment("seg1", "src/main.rs", "abc123");
        seg.embedding_vec = Some(serde_json::to_string(&vec![0.5f32; 384]).unwrap());
        upsert_segment(&conn, &seg).await.unwrap();

        let mut rows = conn
            .query(
                "SELECT COUNT(*) FROM segment_vectors WHERE segment_id = ?1",
                ["seg1"],
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        let has_embedding: i64 = row.get(0).unwrap();
        assert_eq!(has_embedding, 1);
    }

    #[tokio::test]
    async fn upsert_stores_normalized_symbol_rows() {
        let (_db, conn) = setup().await;

        let mut seg = test_segment("seg1", "src/main.rs", "abc123");
        seg.defined_symbols = r#"["ConfigLoader","config_loader"]"#.to_string();
        seg.referenced_symbols = r#"["load_config"]"#.to_string();
        upsert_segment(&conn, &seg).await.unwrap();

        let rows = symbol_rows(&conn, "seg1").await;
        assert_eq!(
            rows,
            vec![
                (
                    "ConfigLoader".to_string(),
                    "configloader".to_string(),
                    "definition".to_string(),
                ),
                (
                    "load_config".to_string(),
                    "loadconfig".to_string(),
                    "usage".to_string(),
                ),
            ]
        );
    }

    #[tokio::test]
    async fn schema_excludes_legacy_embedding_columns() {
        let (_db, conn) = setup().await;

        let mut rows = conn
            .query(
                "SELECT COUNT(*) FROM pragma_table_info('segments') WHERE name IN ('embedding', 'embedding_q8')",
                (),
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        let legacy_column_count: i64 = row.get(0).unwrap();
        assert_eq!(legacy_column_count, 0);
    }

    #[tokio::test]
    async fn upsert_without_embedding_removes_existing_vector() {
        let (_db, conn) = setup().await;

        let mut seg = test_segment("seg1", "src/main.rs", "abc123");
        seg.embedding_vec = Some(serde_json::to_string(&vec![0.5f32; 384]).unwrap());
        upsert_segment(&conn, &seg).await.unwrap();

        seg.embedding_vec = None;
        upsert_segment(&conn, &seg).await.unwrap();

        let mut rows = conn
            .query(
                "SELECT COUNT(*) FROM segment_vectors WHERE segment_id = ?1",
                ["seg1"],
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        let vector_count: i64 = row.get(0).unwrap();
        assert_eq!(vector_count, 0);
    }

    #[tokio::test]
    async fn replace_file_segments_tx_replaces_one_file_without_touching_others() {
        let (_db, conn) = setup().await;

        let mut old_a_1 = test_segment("old_a_1", "src/a.rs", "old-a");
        old_a_1.called_symbols = r#"["legacy_call"]"#.to_string();
        old_a_1.referenced_symbols = r#"["LegacyType"]"#.to_string();
        upsert_segment(&conn, &old_a_1).await.unwrap();
        upsert_segment(&conn, &test_segment("old_a_2", "src/a.rs", "old-a"))
            .await
            .unwrap();
        let mut old_b_1 = test_segment("old_b_1", "src/b.rs", "old-b");
        old_b_1.called_symbols = r#"["keep_b"]"#.to_string();
        upsert_segment(&conn, &old_b_1).await.unwrap();

        let mut new_a_1 = test_segment("new_a_1", "src/a.rs", "new-a");
        new_a_1.called_symbols = r#"["new_call"]"#.to_string();
        new_a_1.referenced_symbols = r#"["NewType"]"#.to_string();
        let replacement = [new_a_1, test_segment("new_a_2", "src/a.rs", "new-a")];

        replace_file_segments_tx(&conn, "src/a.rs", &replacement)
            .await
            .unwrap();

        let file_a = get_segments_by_file(&conn, "src/a.rs").await.unwrap();
        let file_b = get_segments_by_file(&conn, "src/b.rs").await.unwrap();

        let file_a_ids: Vec<&str> = file_a.iter().map(|segment| segment.id.as_str()).collect();
        assert_eq!(file_a_ids, vec!["new_a_1", "new_a_2"]);
        assert!(file_a.iter().all(|segment| segment.file_hash == "new-a"));
        assert_eq!(file_b.len(), 1);
        assert_eq!(file_b[0].id, "old_b_1");
        assert_eq!(file_b[0].file_hash, "old-b");

        let new_symbol_rows = symbol_rows(&conn, "new_a_1").await;
        assert_eq!(
            new_symbol_rows,
            vec![
                (
                    "new_a_1".to_string(),
                    "newa1".to_string(),
                    "definition".to_string(),
                ),
                (
                    "NewType".to_string(),
                    "newtype".to_string(),
                    "usage".to_string(),
                ),
            ]
        );

        let mut rows = conn
            .query(
                "SELECT COUNT(*) FROM segment_symbols WHERE segment_id = 'old_a_1'",
                (),
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        let stale_symbol_count: i64 = row.get(0).unwrap();
        assert_eq!(stale_symbol_count, 0);
        assert_eq!(
            relation_rows(&conn, "new_a_1").await,
            vec![
                (
                    "call".to_string(),
                    "new_call".to_string(),
                    "newcall".to_string(),
                ),
                (
                    "reference".to_string(),
                    "NewType".to_string(),
                    "newtype".to_string(),
                ),
            ]
        );
        assert!(relation_rows(&conn, "old_a_1").await.is_empty());
        assert_eq!(
            relation_rows(&conn, "old_b_1").await,
            vec![(
                "call".to_string(),
                "keep_b".to_string(),
                "keepb".to_string(),
            )]
        );
    }

    #[tokio::test]
    async fn replace_file_batch_tx_rolls_back_all_files_on_failure() {
        let (_db, conn) = setup().await;

        let mut old_a_1 = test_segment("old_a_1", "src/a.rs", "old-a");
        old_a_1.called_symbols = r#"["legacy_a"]"#.to_string();
        upsert_segment(&conn, &old_a_1).await.unwrap();
        let mut old_b_1 = test_segment("old_b_1", "src/b.rs", "old-b");
        old_b_1.referenced_symbols = r#"["LegacyB"]"#.to_string();
        upsert_segment(&conn, &old_b_1).await.unwrap();

        let mut replacement_a_segment = test_segment("new_a_1", "src/a.rs", "new-a");
        replacement_a_segment.called_symbols = r#"["replacement_a"]"#.to_string();
        let replacement_a = [replacement_a_segment];
        let mut replacement_b = test_segment("new_b_1", "src/b.rs", "new-b");
        replacement_b.embedding_vec = Some("not-a-vector".to_string());
        replacement_b.called_symbols = r#"["replacement_b"]"#.to_string();
        let replacement_b = [replacement_b];

        let result = replace_file_batch_tx(
            &conn,
            &[
                FileSegmentBatch {
                    file_path: "src/a.rs",
                    segments: &replacement_a,
                },
                FileSegmentBatch {
                    file_path: "src/b.rs",
                    segments: &replacement_b,
                },
            ],
        )
        .await;

        assert!(result.is_err());

        let file_a = get_segments_by_file(&conn, "src/a.rs").await.unwrap();
        let file_b = get_segments_by_file(&conn, "src/b.rs").await.unwrap();

        assert_eq!(file_a.len(), 1);
        assert_eq!(file_a[0].id, "old_a_1");
        assert_eq!(file_a[0].file_hash, "old-a");
        assert_eq!(file_b.len(), 1);
        assert_eq!(file_b[0].id, "old_b_1");
        assert_eq!(file_b[0].file_hash, "old-b");
        assert_eq!(
            symbol_rows(&conn, "old_a_1").await,
            vec![(
                "old_a_1".to_string(),
                "olda1".to_string(),
                "definition".to_string(),
            )]
        );
        assert_eq!(
            symbol_rows(&conn, "old_b_1").await,
            vec![
                (
                    "old_b_1".to_string(),
                    "oldb1".to_string(),
                    "definition".to_string(),
                ),
                (
                    "LegacyB".to_string(),
                    "legacyb".to_string(),
                    "usage".to_string(),
                ),
            ]
        );
        assert_eq!(
            relation_rows(&conn, "old_a_1").await,
            vec![(
                "call".to_string(),
                "legacy_a".to_string(),
                "legacya".to_string(),
            )]
        );
        assert_eq!(
            relation_rows(&conn, "old_b_1").await,
            vec![(
                "reference".to_string(),
                "LegacyB".to_string(),
                "legacyb".to_string(),
            )]
        );
        assert!(relation_rows(&conn, "new_a_1").await.is_empty());
    }

    #[tokio::test]
    async fn replace_file_segments_tx_with_empty_segments_removes_relation_rows() {
        let (_db, conn) = setup().await;

        let mut old_a_1 = test_segment("old_a_1", "src/a.rs", "old-a");
        old_a_1.called_symbols = r#"["delete_me"]"#.to_string();
        upsert_segment(&conn, &old_a_1).await.unwrap();

        replace_file_segments_tx(&conn, "src/a.rs", &[])
            .await
            .unwrap();

        assert!(get_segments_by_file(&conn, "src/a.rs")
            .await
            .unwrap()
            .is_empty());
        assert!(relation_rows(&conn, "old_a_1").await.is_empty());
    }
}
