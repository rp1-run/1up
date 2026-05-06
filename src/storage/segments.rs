use std::collections::{HashMap, HashSet};
use std::fmt::Write;

use libsql::Connection;
use sha2::{Digest, Sha256};

use crate::shared::constants::DEFAULT_INDEX_CONTEXT_ID;
use crate::shared::errors::{OneupError, StorageError};
use crate::shared::symbols::{normalize_symbolish, EDGE_IDENTITY_BARE_IDENTIFIER};
use crate::shared::types::{ParsedRelation, ReferenceKind, SegmentRole};
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
    pub referenced_relations: String,
    pub called_symbols: String,
    pub called_relations: String,
    pub file_hash: String,
}

pub(crate) fn generate_segment_id(
    context_id: &str,
    file_path: &str,
    line_start: usize,
    line_end: usize,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(context_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(file_path.as_bytes());
    hasher.update(b"\0");
    hasher.update(line_start.to_string().as_bytes());
    hasher.update(b"\0");
    hasher.update(line_end.to_string().as_bytes());
    let hash = hasher.finalize();
    hash.iter()
        .map(|b| format!("{:02x}", b))
        .collect::<String>()[..32]
        .to_string()
}

/// Metadata for updating the indexed-files manifest alongside segment writes.
#[derive(Debug, Clone)]
pub struct IndexedFileMeta {
    pub extension: String,
    pub file_hash: String,
    pub file_size: i64,
    pub modified_ns: i64,
}

/// Parameters for replacing one file's indexed contents inside a batch transaction.
#[derive(Clone, Copy)]
#[allow(dead_code)]
pub struct FileSegmentBatch<'a> {
    pub file_path: &'a str,
    pub segments: &'a [SegmentInsert],
    pub manifest_meta: Option<&'a IndexedFileMeta>,
}

struct SegmentSymbolInsert {
    symbol: String,
    canonical_symbol: String,
    reference_kind: ReferenceKind,
}

/// Insert or replace a segment in the database.
#[allow(dead_code)]
pub async fn upsert_segment(conn: &Connection, seg: &SegmentInsert) -> Result<(), OneupError> {
    upsert_segment_for_context(conn, DEFAULT_INDEX_CONTEXT_ID, seg).await
}

/// Insert or replace a segment inside one index context.
#[allow(dead_code)]
pub async fn upsert_segment_for_context(
    conn: &Connection,
    context_id: &str,
    seg: &SegmentInsert,
) -> Result<(), OneupError> {
    validate_context_id(context_id)?;
    upsert_segment_record_for_context(conn, context_id, seg).await?;
    replace_segment_relations_for_context(
        conn,
        context_id,
        &seg.id,
        &build_segment_relation_rows(seg),
    )
    .await?;

    Ok(())
}

async fn upsert_segment_record_for_context(
    conn: &Connection,
    context_id: &str,
    seg: &SegmentInsert,
) -> Result<(), OneupError> {
    conn.execute(
        queries::UPSERT_SEGMENT,
        libsql::params![
            seg.id.clone(),
            context_id.to_string(),
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

    replace_segment_symbols_for_context(conn, context_id, seg).await?;

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

/// Query all segments for a given file path inside one index context, ordered by line_start.
#[allow(dead_code)]
pub async fn get_segments_by_file_for_context(
    conn: &Connection,
    context_id: &str,
    file_path: &str,
) -> Result<Vec<StoredSegment>, OneupError> {
    validate_context_id(context_id)?;
    let mut rows = conn
        .query(
            queries::SELECT_SEGMENTS_BY_FILE_FOR_CONTEXT,
            libsql::params![context_id, file_path],
        )
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

/// Get a single segment by its ID inside one index context.
pub async fn get_segment_by_id_for_context(
    conn: &Connection,
    context_id: &str,
    id: &str,
) -> Result<Option<StoredSegment>, OneupError> {
    validate_context_id(context_id)?;
    let mut rows = conn
        .query(
            queries::SELECT_SEGMENT_BY_ID_FOR_CONTEXT,
            libsql::params![context_id, id],
        )
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

/// Outcome of a prefix-based segment lookup.
///
/// `get` accepts both full segment ids and the 12-char display handle emitted
/// by the lean row grammar. Using `LIKE ?||'%'` handles both shapes uniformly; the
/// caller distinguishes unique matches from ambiguous prefixes via this enum.
#[derive(Debug, Clone)]
pub enum SegmentPrefixLookup {
    /// Exactly one segment matched the prefix. Boxed so the enum stays small; the
    /// inner `StoredSegment` carries the full content body.
    Found(Box<StoredSegment>),
    /// No segment matched the prefix.
    NotFound,
    /// More than one segment matched; the vector carries the matching ids (bounded
    /// to the query's LIMIT) so callers can surface a disambiguation hint.
    Ambiguous(Vec<String>),
}

/// Resolve a segment handle by prefix. A full-length id resolves to exactly one row
/// via the same `LIKE ?||'%'` path that also handles the 12-char display handle.
#[allow(dead_code)]
pub async fn get_segment_by_prefix(
    conn: &Connection,
    prefix: &str,
) -> Result<SegmentPrefixLookup, OneupError> {
    if prefix.is_empty() {
        return Ok(SegmentPrefixLookup::NotFound);
    }

    let mut rows = conn
        .query(queries::SELECT_SEGMENTS_BY_PREFIX, [prefix])
        .await
        .map_err(|e| StorageError::Query(format!("query segment by prefix failed: {e}")))?;

    let mut matches: Vec<StoredSegment> = Vec::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|e| StorageError::Query(format!("row iteration failed: {e}")))?
    {
        matches.push(row_to_stored_segment(&row)?);
    }

    match matches.len() {
        0 => Ok(SegmentPrefixLookup::NotFound),
        1 => Ok(SegmentPrefixLookup::Found(Box::new(
            matches.into_iter().next().unwrap(),
        ))),
        _ => Ok(SegmentPrefixLookup::Ambiguous(
            matches.into_iter().map(|seg| seg.id).collect(),
        )),
    }
}

/// Resolve a segment handle by prefix inside one index context.
#[allow(dead_code)]
pub async fn get_segment_by_prefix_for_context(
    conn: &Connection,
    context_id: &str,
    prefix: &str,
) -> Result<SegmentPrefixLookup, OneupError> {
    if prefix.is_empty() {
        return Ok(SegmentPrefixLookup::NotFound);
    }
    validate_context_id(context_id)?;

    let mut rows = conn
        .query(
            queries::SELECT_SEGMENTS_BY_PREFIX_FOR_CONTEXT,
            libsql::params![context_id, prefix],
        )
        .await
        .map_err(|e| StorageError::Query(format!("query segment by prefix failed: {e}")))?;

    let mut matches: Vec<StoredSegment> = Vec::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|e| StorageError::Query(format!("row iteration failed: {e}")))?
    {
        matches.push(row_to_stored_segment(&row)?);
    }

    match matches.len() {
        0 => Ok(SegmentPrefixLookup::NotFound),
        1 => Ok(SegmentPrefixLookup::Found(Box::new(
            matches.into_iter().next().unwrap(),
        ))),
        _ => Ok(SegmentPrefixLookup::Ambiguous(
            matches.into_iter().map(|seg| seg.id).collect(),
        )),
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
#[allow(dead_code)]
pub async fn delete_segments_by_file(
    conn: &Connection,
    file_path: &str,
) -> Result<u64, OneupError> {
    delete_segments_by_file_for_context(conn, DEFAULT_INDEX_CONTEXT_ID, file_path).await
}

/// Delete all segments for a given file path inside one index context.
#[allow(dead_code)]
pub async fn delete_segments_by_file_for_context(
    conn: &Connection,
    context_id: &str,
    file_path: &str,
) -> Result<u64, OneupError> {
    validate_context_id(context_id)?;
    let count = delete_segments_by_file_only_for_context(conn, context_id, file_path).await?;
    delete_indexed_file_for_context(conn, context_id, file_path).await?;
    Ok(count)
}

async fn delete_segments_by_file_only_for_context(
    conn: &Connection,
    context_id: &str,
    file_path: &str,
) -> Result<u64, OneupError> {
    conn.execute(
        queries::DELETE_SEGMENT_RELATIONS_BY_CONTEXT_AND_FILE,
        libsql::params![context_id, file_path],
    )
    .await
    .map_err(|e| StorageError::Query(format!("delete segment relations by file failed: {e}")))?;

    let count = conn
        .execute(
            queries::DELETE_SEGMENTS_BY_CONTEXT_AND_FILE,
            libsql::params![context_id, file_path],
        )
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
    get_file_hash_for_context(conn, DEFAULT_INDEX_CONTEXT_ID, file_path).await
}

/// Get the stored file hash for a given file path in one index context.
#[allow(dead_code)]
pub async fn get_file_hash_for_context(
    conn: &Connection,
    context_id: &str,
    file_path: &str,
) -> Result<Option<String>, OneupError> {
    validate_context_id(context_id)?;
    let mut rows = conn
        .query(
            queries::SELECT_FILE_HASH_FOR_CONTEXT,
            libsql::params![context_id, file_path],
        )
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
    replace_file_segments_for_context_tx(conn, DEFAULT_INDEX_CONTEXT_ID, file_path, segments).await
}

/// Replace all stored segments for a single file in one index context.
#[allow(dead_code)]
pub async fn replace_file_segments_for_context_tx(
    conn: &Connection,
    context_id: &str,
    file_path: &str,
    segments: &[SegmentInsert],
) -> Result<(), OneupError> {
    replace_file_segments_for_context_tx_with_meta(conn, context_id, file_path, segments, None)
        .await
}

/// Replace all stored segments for a single file in one transaction, updating the manifest.
#[allow(dead_code)]
pub async fn replace_file_segments_tx_with_meta(
    conn: &Connection,
    file_path: &str,
    segments: &[SegmentInsert],
    manifest_meta: Option<&IndexedFileMeta>,
) -> Result<(), OneupError> {
    replace_file_segments_for_context_tx_with_meta(
        conn,
        DEFAULT_INDEX_CONTEXT_ID,
        file_path,
        segments,
        manifest_meta,
    )
    .await
}

/// Replace all stored segments for a single file in one index context, updating the manifest.
pub async fn replace_file_segments_for_context_tx_with_meta(
    conn: &Connection,
    context_id: &str,
    file_path: &str,
    segments: &[SegmentInsert],
    manifest_meta: Option<&IndexedFileMeta>,
) -> Result<(), OneupError> {
    validate_context_id(context_id)?;
    validate_replace_segments(file_path, segments)?;

    let tx = conn.transaction().await.map_err(|e| {
        StorageError::Transaction(format!("begin file replace transaction failed: {e}"))
    })?;

    replace_file_segments_in_transaction_with_meta(
        &tx,
        context_id,
        file_path,
        segments,
        manifest_meta,
    )
    .await?;

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
    replace_file_batch_for_context_tx(conn, DEFAULT_INDEX_CONTEXT_ID, batches).await
}

/// Replace stored segments for multiple files in one index context and one transaction.
#[allow(dead_code)]
pub async fn replace_file_batch_for_context_tx(
    conn: &Connection,
    context_id: &str,
    batches: &[FileSegmentBatch<'_>],
) -> Result<(), OneupError> {
    validate_context_id(context_id)?;
    validate_replace_batches(batches)?;

    let tx = conn.transaction().await.map_err(|e| {
        StorageError::Transaction(format!("begin file batch replace transaction failed: {e}"))
    })?;

    for batch in batches {
        replace_file_segments_in_transaction_with_meta(
            &tx,
            context_id,
            batch.file_path,
            batch.segments,
            batch.manifest_meta,
        )
        .await?;
    }

    tx.commit().await.map_err(|e| {
        StorageError::Transaction(format!("commit file batch replace transaction failed: {e}"))
    })?;

    Ok(())
}

/// Get all distinct file paths stored in the segments table.
#[allow(dead_code)]
pub async fn get_all_file_paths(conn: &Connection) -> Result<Vec<String>, OneupError> {
    get_all_file_paths_for_context(conn, DEFAULT_INDEX_CONTEXT_ID).await
}

/// Get all distinct file paths stored in the segments table for one index context.
#[allow(dead_code)]
pub async fn get_all_file_paths_for_context(
    conn: &Connection,
    context_id: &str,
) -> Result<Vec<String>, OneupError> {
    validate_context_id(context_id)?;
    let mut rows = conn
        .query(queries::SELECT_ALL_FILE_PATHS_FOR_CONTEXT, [context_id])
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

/// Get distinct test-like file paths, optionally constrained to a scope prefix.
#[allow(dead_code)]
pub async fn get_test_file_paths(
    conn: &Connection,
    scope: Option<&str>,
    limit: usize,
) -> Result<Vec<String>, OneupError> {
    if limit == 0 {
        return Ok(Vec::new());
    }

    let mut rows = match scope {
        Some(scope) => {
            conn.query(
                queries::SELECT_SCOPED_TEST_FILE_PATHS_LIMITED,
                libsql::params![scope, format!("{scope}/%"), limit as i64],
            )
            .await
            .map_err(|e| StorageError::Query(format!("query scoped test file paths failed: {e}")))?
        }
        None => conn
            .query(queries::SELECT_TEST_FILE_PATHS_LIMITED, [limit as i64])
            .await
            .map_err(|e| StorageError::Query(format!("query test file paths failed: {e}")))?,
    };

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

/// Get distinct test-like file paths inside one index context, optionally constrained to a scope prefix.
pub async fn get_test_file_paths_for_context(
    conn: &Connection,
    context_id: &str,
    scope: Option<&str>,
    limit: usize,
) -> Result<Vec<String>, OneupError> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    validate_context_id(context_id)?;

    let mut rows = match scope {
        Some(scope) => {
            conn.query(
                queries::SELECT_SCOPED_TEST_FILE_PATHS_LIMITED_FOR_CONTEXT,
                libsql::params![context_id, scope, format!("{scope}/%"), limit as i64],
            )
            .await
            .map_err(|e| StorageError::Query(format!("query scoped test file paths failed: {e}")))?
        }
        None => conn
            .query(
                queries::SELECT_TEST_FILE_PATHS_LIMITED_FOR_CONTEXT,
                libsql::params![context_id, limit as i64],
            )
            .await
            .map_err(|e| StorageError::Query(format!("query test file paths failed: {e}")))?,
    };

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

/// Get all distinct file paths for a given language inside one index context.
pub async fn get_file_paths_by_language_for_context(
    conn: &Connection,
    context_id: &str,
    language: &str,
) -> Result<Vec<String>, OneupError> {
    validate_context_id(context_id)?;
    let mut rows = conn
        .query(
            queries::SELECT_FILE_PATHS_BY_LANGUAGE_FOR_CONTEXT,
            libsql::params![context_id, language],
        )
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

/// Count total number of segments in a worktree context.
pub async fn count_segments_for_context(
    conn: &Connection,
    context_id: &str,
) -> Result<u64, OneupError> {
    validate_context_id(context_id)?;
    let mut rows = conn
        .query(queries::COUNT_SEGMENTS_FOR_CONTEXT, [context_id])
        .await
        .map_err(|e| StorageError::Query(format!("count context segments failed: {e}")))?;

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

/// Count distinct file paths in a worktree context.
pub async fn count_files_for_context(
    conn: &Connection,
    context_id: &str,
) -> Result<u64, OneupError> {
    validate_context_id(context_id)?;
    let mut rows = conn
        .query(queries::COUNT_FILES_FOR_CONTEXT, [context_id])
        .await
        .map_err(|e| StorageError::Query(format!("count context files failed: {e}")))?;

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

fn parse_relations(value: &str) -> Vec<ParsedRelation> {
    serde_json::from_str(value).unwrap_or_default()
}

fn fallback_relations(symbols: &[String]) -> Vec<ParsedRelation> {
    symbols
        .iter()
        .map(|symbol| ParsedRelation {
            symbol: symbol.clone(),
            edge_identity_kind: EDGE_IDENTITY_BARE_IDENTIFIER.to_string(),
            kind: None,
        })
        .collect()
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
    let called_symbols = parse_symbols(&seg.called_symbols);
    let called_relations = {
        let parsed = parse_relations(&seg.called_relations);
        if parsed.is_empty() && !called_symbols.is_empty() {
            fallback_relations(&called_symbols)
        } else {
            parsed
        }
    };
    let referenced_symbols = parse_symbols(&seg.referenced_symbols);
    let referenced_relations = {
        let parsed = parse_relations(&seg.referenced_relations);
        if parsed.is_empty() && !referenced_symbols.is_empty() {
            fallback_relations(&referenced_symbols)
        } else {
            parsed
        }
    };

    relations::build_relation_inserts(&seg.id, &called_relations, &referenced_relations)
}

fn validate_context_id(context_id: &str) -> Result<(), OneupError> {
    if context_id.trim().is_empty() {
        return Err(
            StorageError::Transaction("index context id cannot be empty".to_string()).into(),
        );
    }
    if context_id.trim() != context_id {
        return Err(StorageError::Transaction(
            "index context id cannot contain surrounding whitespace".to_string(),
        )
        .into());
    }

    Ok(())
}

async fn replace_segment_symbols_for_context(
    conn: &Connection,
    context_id: &str,
    seg: &SegmentInsert,
) -> Result<(), OneupError> {
    conn.execute(
        queries::DELETE_SEGMENT_SYMBOLS_BY_CONTEXT_AND_SEGMENT_ID,
        libsql::params![context_id, seg.id.clone()],
    )
    .await
    .map_err(|e| StorageError::Query(format!("delete segment symbols failed: {e}")))?;

    for symbol in build_segment_symbol_rows(seg) {
        conn.execute(
            queries::INSERT_SEGMENT_SYMBOL,
            libsql::params![
                context_id.to_string(),
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

async fn replace_segment_relations_for_context(
    conn: &Connection,
    context_id: &str,
    source_segment_id: &str,
    relations: &[RelationInsert],
) -> Result<(), OneupError> {
    validate_relation_source_ids(source_segment_id, relations)?;
    delete_segment_relations_by_context_and_source_segment_id(conn, context_id, source_segment_id)
        .await?;
    batch_insert_relations_for_context(conn, context_id, relations).await
}

fn validate_relation_source_ids(
    source_segment_id: &str,
    relations: &[RelationInsert],
) -> Result<(), OneupError> {
    for relation in relations {
        if relation.source_segment_id != source_segment_id {
            return Err(StorageError::Transaction(format!(
                "relation replace for '{source_segment_id}' received row for '{}'",
                relation.source_segment_id
            ))
            .into());
        }
    }

    Ok(())
}

async fn delete_segment_relations_by_context_and_source_segment_id(
    conn: &Connection,
    context_id: &str,
    source_segment_id: &str,
) -> Result<u64, OneupError> {
    conn.execute(
        queries::DELETE_SEGMENT_RELATIONS_BY_CONTEXT_AND_SOURCE_SEGMENT_ID,
        libsql::params![context_id, source_segment_id],
    )
    .await
    .map_err(|e| StorageError::Query(format!("delete segment relations failed: {e}")))
    .map_err(Into::into)
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
    replace_file_segments_in_transaction_with_meta(
        conn,
        DEFAULT_INDEX_CONTEXT_ID,
        file_path,
        segments,
        None,
    )
    .await
}

async fn replace_file_segments_in_transaction_with_meta(
    conn: &Connection,
    context_id: &str,
    file_path: &str,
    segments: &[SegmentInsert],
    manifest_meta: Option<&IndexedFileMeta>,
) -> Result<(), OneupError> {
    delete_segments_by_file_only_for_context(conn, context_id, file_path).await?;

    batch_upsert_segments_for_context(conn, context_id, segments).await?;
    batch_upsert_vectors(conn, segments).await?;

    let symbol_rows: Vec<(String, SegmentSymbolInsert)> = segments
        .iter()
        .flat_map(|seg| {
            build_segment_symbol_rows(seg)
                .into_iter()
                .map(|sym| (seg.id.clone(), sym))
        })
        .collect();
    batch_insert_symbols_for_context(conn, context_id, &symbol_rows).await?;

    let relation_rows: Vec<RelationInsert> = segments
        .iter()
        .flat_map(build_segment_relation_rows)
        .collect();
    batch_insert_relations_for_context(conn, context_id, &relation_rows).await?;

    if let Some(meta) = manifest_meta {
        upsert_indexed_file_for_context(
            conn,
            context_id,
            file_path,
            &meta.extension,
            &meta.file_hash,
            meta.file_size,
            meta.modified_ns,
        )
        .await?;
    } else if segments.is_empty() {
        delete_indexed_file_for_context(conn, context_id, file_path).await?;
    }

    Ok(())
}

#[cfg(test)]
async fn batch_upsert_segments(
    conn: &Connection,
    segments: &[SegmentInsert],
) -> Result<(), OneupError> {
    batch_upsert_segments_for_context(conn, DEFAULT_INDEX_CONTEXT_ID, segments).await
}

async fn batch_upsert_segments_for_context(
    conn: &Connection,
    context_id: &str,
    segments: &[SegmentInsert],
) -> Result<(), OneupError> {
    if segments.is_empty() {
        return Ok(());
    }

    for chunk in segments.chunks(queries::SEGMENT_CHUNK_SIZE) {
        let mut sql = String::from(
            "INSERT OR REPLACE INTO segments (\
             id, context_id, file_path, language, block_type, content, \
             line_start, line_end, breadcrumb, complexity, role, \
             defined_symbols, referenced_symbols, called_symbols, \
             file_hash, created_at, updated_at\
             ) VALUES ",
        );
        let mut params: Vec<libsql::Value> =
            Vec::with_capacity(chunk.len() * queries::SEGMENT_INSERT_COLS);

        for (i, seg) in chunk.iter().enumerate() {
            if i > 0 {
                sql.push_str(", ");
            }
            let b = i * queries::SEGMENT_INSERT_COLS;
            write!(
                sql,
                "(?{}, ?{}, ?{}, ?{}, ?{}, ?{}, ?{}, ?{}, ?{}, ?{}, ?{}, ?{}, ?{}, ?{}, ?{}, datetime('now'), datetime('now'))",
                b+1, b+2, b+3, b+4, b+5, b+6, b+7, b+8, b+9, b+10, b+11, b+12, b+13, b+14, b+15,
            ).expect("write to String cannot fail");

            params.push(seg.id.clone().into());
            params.push(context_id.to_string().into());
            params.push(seg.file_path.clone().into());
            params.push(seg.language.clone().into());
            params.push(seg.block_type.clone().into());
            params.push(seg.content.clone().into());
            params.push(seg.line_start.into());
            params.push(seg.line_end.into());
            params.push(seg.breadcrumb.clone().into());
            params.push(seg.complexity.into());
            params.push(seg.role.clone().into());
            params.push(seg.defined_symbols.clone().into());
            params.push(seg.referenced_symbols.clone().into());
            params.push(seg.called_symbols.clone().into());
            params.push(seg.file_hash.clone().into());
        }

        conn.execute(&sql, params)
            .await
            .map_err(|e| StorageError::Query(format!("batch upsert segments failed: {e}")))?;
    }

    Ok(())
}

async fn batch_upsert_vectors(
    conn: &Connection,
    segments: &[SegmentInsert],
) -> Result<(), OneupError> {
    let vec_segments: Vec<&SegmentInsert> = segments
        .iter()
        .filter(|seg| seg.embedding_vec.is_some())
        .collect();

    if vec_segments.is_empty() {
        return Ok(());
    }

    for chunk in vec_segments.chunks(queries::VECTOR_CHUNK_SIZE) {
        let mut sql = String::from(
            "INSERT OR REPLACE INTO segment_vectors (\
             segment_id, embedding_vec, created_at, updated_at\
             ) VALUES ",
        );
        let mut params: Vec<libsql::Value> =
            Vec::with_capacity(chunk.len() * queries::VECTOR_INSERT_COLS);

        for (i, seg) in chunk.iter().enumerate() {
            if i > 0 {
                sql.push_str(", ");
            }
            let b = i * queries::VECTOR_INSERT_COLS;
            write!(
                sql,
                "(?{}, vector8(?{}), datetime('now'), datetime('now'))",
                b + 1,
                b + 2
            )
            .expect("write to String cannot fail");

            params.push(seg.id.clone().into());
            params.push(seg.embedding_vec.clone().unwrap().into());
        }

        conn.execute(&sql, params)
            .await
            .map_err(|e| StorageError::Query(format!("batch upsert vectors failed: {e}")))?;
    }

    Ok(())
}

#[allow(dead_code)]
async fn batch_insert_symbols(
    conn: &Connection,
    symbols: &[(String, SegmentSymbolInsert)],
) -> Result<(), OneupError> {
    batch_insert_symbols_for_context(conn, DEFAULT_INDEX_CONTEXT_ID, symbols).await
}

async fn batch_insert_symbols_for_context(
    conn: &Connection,
    context_id: &str,
    symbols: &[(String, SegmentSymbolInsert)],
) -> Result<(), OneupError> {
    if symbols.is_empty() {
        return Ok(());
    }

    for chunk in symbols.chunks(queries::SYMBOL_CHUNK_SIZE) {
        let mut sql = String::from(
            "INSERT OR REPLACE INTO segment_symbols (\
             context_id, segment_id, symbol, canonical_symbol, reference_kind, created_at\
             ) VALUES ",
        );
        let mut params: Vec<libsql::Value> =
            Vec::with_capacity(chunk.len() * queries::SYMBOL_INSERT_COLS);

        for (i, (segment_id, sym)) in chunk.iter().enumerate() {
            if i > 0 {
                sql.push_str(", ");
            }
            let b = i * queries::SYMBOL_INSERT_COLS;
            write!(
                sql,
                "(?{}, ?{}, ?{}, ?{}, ?{}, datetime('now'))",
                b + 1,
                b + 2,
                b + 3,
                b + 4,
                b + 5
            )
            .expect("write to String cannot fail");

            params.push(context_id.to_string().into());
            params.push(segment_id.clone().into());
            params.push(sym.symbol.clone().into());
            params.push(sym.canonical_symbol.clone().into());
            params.push(reference_kind_label(sym.reference_kind).to_string().into());
        }

        conn.execute(&sql, params)
            .await
            .map_err(|e| StorageError::Query(format!("batch insert symbols failed: {e}")))?;
    }

    Ok(())
}

async fn batch_insert_relations_for_context(
    conn: &Connection,
    context_id: &str,
    relations: &[RelationInsert],
) -> Result<(), OneupError> {
    if relations.is_empty() {
        return Ok(());
    }

    for chunk in relations.chunks(queries::CONTEXT_RELATION_CHUNK_SIZE) {
        let mut sql = String::from(
            "INSERT OR REPLACE INTO segment_relations (\
             context_id, source_segment_id, relation_kind, raw_target_symbol, \
             canonical_target_symbol, lookup_canonical_symbol, \
             qualifier_fingerprint, edge_identity_kind, created_at\
             ) VALUES ",
        );
        let mut params: Vec<libsql::Value> =
            Vec::with_capacity(chunk.len() * queries::CONTEXT_RELATION_INSERT_COLS);

        for (i, relation) in chunk.iter().enumerate() {
            if i > 0 {
                sql.push_str(", ");
            }
            let base = i * queries::CONTEXT_RELATION_INSERT_COLS;
            write!(
                sql,
                "(?{}, ?{}, ?{}, ?{}, ?{}, ?{}, ?{}, ?{}, datetime('now'))",
                base + 1,
                base + 2,
                base + 3,
                base + 4,
                base + 5,
                base + 6,
                base + 7,
                base + 8,
            )
            .expect("write to String cannot fail");

            params.push(context_id.to_string().into());
            params.push(relation.source_segment_id.clone().into());
            params.push(relation.relation_kind.as_str().to_string().into());
            params.push(relation.raw_target_symbol.clone().into());
            params.push(relation.canonical_target_symbol.clone().into());
            params.push(relation.lookup_canonical_symbol.clone().into());
            params.push(relation.qualifier_fingerprint.clone().into());
            params.push(relation.edge_identity_kind.clone().into());
        }

        conn.execute(&sql, params).await.map_err(|e| {
            StorageError::Query(format!("batch insert segment relations failed: {e}"))
        })?;
    }

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

/// A row from the `indexed_files` manifest table.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct IndexedFileEntry {
    pub file_path: String,
    pub extension: String,
    pub file_hash: String,
    pub file_size: i64,
    pub modified_ns: i64,
}

/// Load the full indexed-files manifest keyed by file path.
#[allow(dead_code)]
pub async fn get_all_indexed_files(
    conn: &Connection,
) -> Result<HashMap<String, IndexedFileEntry>, OneupError> {
    get_all_indexed_files_for_context(conn, DEFAULT_INDEX_CONTEXT_ID).await
}

/// Load one context's indexed-files manifest keyed by file path.
#[allow(dead_code)]
pub async fn get_all_indexed_files_for_context(
    conn: &Connection,
    context_id: &str,
) -> Result<HashMap<String, IndexedFileEntry>, OneupError> {
    validate_context_id(context_id)?;
    let mut rows = conn
        .query(queries::SELECT_ALL_INDEXED_FILES_FOR_CONTEXT, [context_id])
        .await
        .map_err(|e| StorageError::Query(format!("query all indexed files failed: {e}")))?;

    let mut entries = HashMap::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|e| StorageError::Query(format!("row iteration failed: {e}")))?
    {
        let file_path: String = row
            .get(0)
            .map_err(|e| StorageError::Query(format!("read file_path failed: {e}")))?;
        entries.insert(
            file_path.clone(),
            IndexedFileEntry {
                file_path,
                extension: row
                    .get(1)
                    .map_err(|e| StorageError::Query(format!("read extension failed: {e}")))?,
                file_hash: row
                    .get(2)
                    .map_err(|e| StorageError::Query(format!("read file_hash failed: {e}")))?,
                file_size: row
                    .get(3)
                    .map_err(|e| StorageError::Query(format!("read file_size failed: {e}")))?,
                modified_ns: row
                    .get(4)
                    .map_err(|e| StorageError::Query(format!("read modified_ns failed: {e}")))?,
            },
        );
    }

    Ok(entries)
}

/// Load a single indexed-file entry by path.
#[allow(dead_code)]
pub async fn get_indexed_file(
    conn: &Connection,
    file_path: &str,
) -> Result<Option<IndexedFileEntry>, OneupError> {
    get_indexed_file_for_context(conn, DEFAULT_INDEX_CONTEXT_ID, file_path).await
}

/// Load a single indexed-file entry by context and path.
#[allow(dead_code)]
pub async fn get_indexed_file_for_context(
    conn: &Connection,
    context_id: &str,
    file_path: &str,
) -> Result<Option<IndexedFileEntry>, OneupError> {
    validate_context_id(context_id)?;
    let mut rows = conn
        .query(
            queries::SELECT_INDEXED_FILE_FOR_CONTEXT,
            libsql::params![context_id, file_path],
        )
        .await
        .map_err(|e| StorageError::Query(format!("query indexed file failed: {e}")))?;

    match rows
        .next()
        .await
        .map_err(|e| StorageError::Query(format!("row iteration failed: {e}")))?
    {
        Some(row) => Ok(Some(IndexedFileEntry {
            file_path: row
                .get(0)
                .map_err(|e| StorageError::Query(format!("read file_path failed: {e}")))?,
            extension: row
                .get(1)
                .map_err(|e| StorageError::Query(format!("read extension failed: {e}")))?,
            file_hash: row
                .get(2)
                .map_err(|e| StorageError::Query(format!("read file_hash failed: {e}")))?,
            file_size: row
                .get(3)
                .map_err(|e| StorageError::Query(format!("read file_size failed: {e}")))?,
            modified_ns: row
                .get(4)
                .map_err(|e| StorageError::Query(format!("read modified_ns failed: {e}")))?,
        })),
        None => Ok(None),
    }
}

/// Write or update an indexed-file manifest entry.
#[allow(dead_code)]
pub async fn upsert_indexed_file(
    conn: &Connection,
    file_path: &str,
    extension: &str,
    file_hash: &str,
    file_size: i64,
    modified_ns: i64,
) -> Result<(), OneupError> {
    upsert_indexed_file_for_context(
        conn,
        DEFAULT_INDEX_CONTEXT_ID,
        file_path,
        extension,
        file_hash,
        file_size,
        modified_ns,
    )
    .await
}

/// Write or update an indexed-file manifest entry for one context.
#[allow(dead_code)]
pub async fn upsert_indexed_file_for_context(
    conn: &Connection,
    context_id: &str,
    file_path: &str,
    extension: &str,
    file_hash: &str,
    file_size: i64,
    modified_ns: i64,
) -> Result<(), OneupError> {
    validate_context_id(context_id)?;
    conn.execute(
        queries::UPSERT_INDEXED_FILE,
        libsql::params![
            context_id,
            file_path,
            extension,
            file_hash,
            file_size,
            modified_ns
        ],
    )
    .await
    .map_err(|e| StorageError::Query(format!("upsert indexed file failed: {e}")))?;
    Ok(())
}

/// Remove an indexed-file manifest entry.
#[allow(dead_code)]
pub async fn delete_indexed_file(conn: &Connection, file_path: &str) -> Result<(), OneupError> {
    delete_indexed_file_for_context(conn, DEFAULT_INDEX_CONTEXT_ID, file_path).await
}

/// Remove one context's indexed-file manifest entry.
#[allow(dead_code)]
pub async fn delete_indexed_file_for_context(
    conn: &Connection,
    context_id: &str,
    file_path: &str,
) -> Result<(), OneupError> {
    validate_context_id(context_id)?;
    conn.execute(
        queries::DELETE_INDEXED_FILE,
        libsql::params![context_id, file_path],
    )
    .await
    .map_err(|e| StorageError::Query(format!("delete indexed file failed: {e}")))?;
    Ok(())
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
            referenced_relations: "[]".to_string(),
            called_symbols: "[]".to_string(),
            called_relations: "[]".to_string(),
            file_hash: file_hash.to_string(),
        }
    }

    fn generated_test_segment(context_id: &str, file_path: &str, file_hash: &str) -> SegmentInsert {
        let id = generate_segment_id(context_id, file_path, 1, 3);
        test_segment(&id, file_path, file_hash)
    }

    #[test]
    fn segment_ids_use_extended_hash_prefix() {
        let id = generate_segment_id("ctx-main", "src/main.rs", 1, 3);
        assert_eq!(id.len(), 32);
    }

    #[test]
    fn context_ids_reject_surrounding_whitespace() {
        assert!(validate_context_id("ctx-main").is_ok());
        assert!(validate_context_id(" ctx-main").is_err());
        assert!(validate_context_id("ctx-main ").is_err());
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

    async fn relation_rows(
        conn: &Connection,
        segment_id: &str,
    ) -> Vec<(String, String, String, String, String, String)> {
        let mut rows = conn
            .query(
                "SELECT relation_kind, raw_target_symbol, canonical_target_symbol,
                        lookup_canonical_symbol, qualifier_fingerprint, edge_identity_kind
                 FROM segment_relations
                 WHERE source_segment_id = ?1
                 ORDER BY relation_kind, canonical_target_symbol, edge_identity_kind",
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
                row.get(3).unwrap(),
                row.get(4).unwrap(),
                row.get(5).unwrap(),
            ));
        }

        results
    }

    async fn segment_ids_for_context(
        conn: &Connection,
        context_id: &str,
        file_path: &str,
    ) -> Vec<String> {
        let mut rows = conn
            .query(
                "SELECT id
                 FROM segments
                 WHERE context_id = ?1
                   AND file_path = ?2
                 ORDER BY id",
                libsql::params![context_id, file_path],
            )
            .await
            .unwrap();

        let mut results = Vec::new();
        while let Some(row) = rows.next().await.unwrap() {
            results.push(row.get(0).unwrap());
        }

        results
    }

    async fn vector_exists(conn: &Connection, segment_id: &str) -> bool {
        let mut rows = conn
            .query(
                "SELECT COUNT(*) FROM segment_vectors WHERE segment_id = ?1",
                [segment_id],
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        let count: i64 = row.get(0).unwrap();
        count == 1
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
    async fn get_accepts_prefix_or_full_id() {
        let (_db, conn) = setup().await;

        upsert_segment(
            &conn,
            &test_segment("a0f1e2c3d4b5f6a7", "src/lib.rs", "hash1"),
        )
        .await
        .unwrap();
        upsert_segment(
            &conn,
            &test_segment("b7c2a4e5d6f812ab", "src/main.rs", "hash2"),
        )
        .await
        .unwrap();

        // 12-char display handle resolves unambiguously.
        match get_segment_by_prefix(&conn, "a0f1e2c3d4b5").await.unwrap() {
            SegmentPrefixLookup::Found(seg) => {
                assert_eq!(seg.id, "a0f1e2c3d4b5f6a7");
                assert_eq!(seg.file_path, "src/lib.rs");
            }
            other => panic!("expected Found, got {other:?}"),
        }

        // Full-length id also resolves through the same path.
        match get_segment_by_prefix(&conn, "b7c2a4e5d6f812ab")
            .await
            .unwrap()
        {
            SegmentPrefixLookup::Found(seg) => assert_eq!(seg.id, "b7c2a4e5d6f812ab"),
            other => panic!("expected Found, got {other:?}"),
        }

        // Unknown prefix surfaces NotFound.
        match get_segment_by_prefix(&conn, "deadbeef").await.unwrap() {
            SegmentPrefixLookup::NotFound => {}
            other => panic!("expected NotFound, got {other:?}"),
        }

        // Empty input is treated as NotFound instead of matching everything.
        match get_segment_by_prefix(&conn, "").await.unwrap() {
            SegmentPrefixLookup::NotFound => {}
            other => panic!("expected NotFound for empty prefix, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn get_disambiguates_on_prefix_collision() {
        let (_db, conn) = setup().await;

        upsert_segment(&conn, &test_segment("abc111000000aaaa", "src/a.rs", "h1"))
            .await
            .unwrap();
        upsert_segment(&conn, &test_segment("abc222000000bbbb", "src/b.rs", "h2"))
            .await
            .unwrap();

        match get_segment_by_prefix(&conn, "abc").await.unwrap() {
            SegmentPrefixLookup::Ambiguous(ids) => {
                assert_eq!(ids.len(), 2);
                assert!(ids.contains(&"abc111000000aaaa".to_string()));
                assert!(ids.contains(&"abc222000000bbbb".to_string()));
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }
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
                "configloader".to_string(),
                String::new(),
                "bare_identifier".to_string(),
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
    async fn file_paths_by_language_are_scoped_to_context() {
        let (_db, conn) = setup().await;
        let main_context = "ctx-main";
        let linked_context = "ctx-linked";

        let rust_main = test_segment("main-rust", "src/main.rs", "hash-main");
        let rust_linked = test_segment("linked-rust", "src/linked.rs", "hash-linked");
        let mut python_main = test_segment("main-python", "src/main.py", "hash-python");
        python_main.language = "python".to_string();

        replace_file_segments_for_context_tx(&conn, main_context, "src/main.rs", &[rust_main])
            .await
            .unwrap();
        replace_file_segments_for_context_tx(
            &conn,
            linked_context,
            "src/linked.rs",
            &[rust_linked],
        )
        .await
        .unwrap();
        replace_file_segments_for_context_tx(&conn, main_context, "src/main.py", &[python_main])
            .await
            .unwrap();

        assert_eq!(
            get_file_paths_by_language_for_context(&conn, main_context, "rust")
                .await
                .unwrap(),
            vec!["src/main.rs"]
        );
        assert_eq!(
            get_file_paths_by_language_for_context(&conn, linked_context, "rust")
                .await
                .unwrap(),
            vec!["src/linked.rs"]
        );
        assert_eq!(
            get_file_paths_by_language_for_context(&conn, linked_context, "python")
                .await
                .unwrap(),
            Vec::<String>::new()
        );
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
    async fn batch_upsert_vectors_at_new_element_type() {
        let (_db, conn) = setup().await;

        let mut segments: Vec<SegmentInsert> = Vec::with_capacity(100);
        for i in 0..100 {
            let id = format!("seg-{i:03}");
            let mut seg = test_segment(&id, &format!("src/file_{i:03}.rs"), &format!("hash-{i}"));
            let mut embedding = vec![0.0f32; 384];
            embedding[i % 384] = 1.0;
            seg.embedding_vec = Some(serde_json::to_string(&embedding).unwrap());
            segments.push(seg);
        }

        batch_upsert_segments(&conn, &segments).await.unwrap();
        batch_upsert_vectors(&conn, &segments).await.unwrap();

        let mut rows = conn
            .query("SELECT COUNT(*) FROM segment_vectors", ())
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        let stored: i64 = row.get(0).unwrap();
        assert_eq!(stored, 100);
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
        new_a_1.called_symbols = r#"["crate::new::new_call"]"#.to_string();
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
                    "crate::new::new_call".to_string(),
                    "cratenewnewcall".to_string(),
                    "newcall".to_string(),
                    "new".to_string(),
                    "bare_identifier".to_string(),
                ),
                (
                    "reference".to_string(),
                    "NewType".to_string(),
                    "newtype".to_string(),
                    "newtype".to_string(),
                    String::new(),
                    "bare_identifier".to_string(),
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
                "keepb".to_string(),
                String::new(),
                "bare_identifier".to_string(),
            )]
        );
    }

    #[tokio::test]
    async fn replace_file_segments_for_context_tx_scopes_rows_by_context_and_file() {
        let (_db, conn) = setup().await;
        let main_context = "ctx-main";
        let linked_context = "ctx-linked";
        let file_path = "src/a.rs";
        let main_segment_id = generate_segment_id(main_context, file_path, 1, 3);
        let linked_segment_id = generate_segment_id(linked_context, file_path, 1, 3);

        assert_ne!(main_segment_id, linked_segment_id);

        let mut main_old = generated_test_segment(main_context, file_path, "main-old");
        main_old.called_symbols = r#"["delete_main_relation"]"#.to_string();
        main_old.embedding_vec = Some(serde_json::to_string(&vec![0.5f32; 384]).unwrap());
        let mut linked_old = generated_test_segment(linked_context, file_path, "linked-old");
        linked_old.called_symbols = r#"["keep_linked_relation"]"#.to_string();
        linked_old.embedding_vec = Some(serde_json::to_string(&vec![0.25f32; 384]).unwrap());

        let main_meta = IndexedFileMeta {
            extension: "rs".to_string(),
            file_hash: "main-old".to_string(),
            file_size: 10,
            modified_ns: 100,
        };
        let linked_meta = IndexedFileMeta {
            extension: "rs".to_string(),
            file_hash: "linked-old".to_string(),
            file_size: 20,
            modified_ns: 200,
        };

        replace_file_segments_for_context_tx_with_meta(
            &conn,
            main_context,
            file_path,
            &[main_old],
            Some(&main_meta),
        )
        .await
        .unwrap();
        replace_file_segments_for_context_tx_with_meta(
            &conn,
            linked_context,
            file_path,
            &[linked_old],
            Some(&linked_meta),
        )
        .await
        .unwrap();

        let mut main_new = generated_test_segment(main_context, file_path, "main-new");
        main_new.embedding_vec = Some(serde_json::to_string(&vec![0.75f32; 384]).unwrap());
        let main_new_meta = IndexedFileMeta {
            extension: "rs".to_string(),
            file_hash: "main-new".to_string(),
            file_size: 30,
            modified_ns: 300,
        };
        replace_file_segments_for_context_tx_with_meta(
            &conn,
            main_context,
            file_path,
            &[main_new],
            Some(&main_new_meta),
        )
        .await
        .unwrap();

        assert_eq!(
            segment_ids_for_context(&conn, main_context, file_path).await,
            vec![main_segment_id.clone()]
        );
        assert_eq!(
            segment_ids_for_context(&conn, linked_context, file_path).await,
            vec![linked_segment_id.clone()]
        );
        assert!(relation_rows(&conn, &main_segment_id).await.is_empty());
        assert!(!relation_rows(&conn, &linked_segment_id).await.is_empty());
        assert!(vector_exists(&conn, &main_segment_id).await);
        assert!(vector_exists(&conn, &linked_segment_id).await);

        let main_manifest = get_all_indexed_files_for_context(&conn, main_context)
            .await
            .unwrap();
        let linked_manifest = get_all_indexed_files_for_context(&conn, linked_context)
            .await
            .unwrap();
        assert_eq!(main_manifest[file_path].file_hash, "main-new");
        assert_eq!(linked_manifest[file_path].file_hash, "linked-old");

        replace_file_segments_for_context_tx(&conn, main_context, file_path, &[])
            .await
            .unwrap();

        assert!(segment_ids_for_context(&conn, main_context, file_path)
            .await
            .is_empty());
        assert_eq!(
            segment_ids_for_context(&conn, linked_context, file_path).await,
            vec![linked_segment_id.clone()]
        );
        assert!(!vector_exists(&conn, &main_segment_id).await);
        assert!(vector_exists(&conn, &linked_segment_id).await);
        assert!(!get_all_indexed_files_for_context(&conn, main_context)
            .await
            .unwrap()
            .contains_key(file_path));
        assert!(get_all_indexed_files_for_context(&conn, linked_context)
            .await
            .unwrap()
            .contains_key(file_path));
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
                    manifest_meta: None,
                },
                FileSegmentBatch {
                    file_path: "src/b.rs",
                    segments: &replacement_b,
                    manifest_meta: None,
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
                "legacya".to_string(),
                String::new(),
                "bare_identifier".to_string(),
            )]
        );
        assert_eq!(
            relation_rows(&conn, "old_b_1").await,
            vec![(
                "reference".to_string(),
                "LegacyB".to_string(),
                "legacyb".to_string(),
                "legacyb".to_string(),
                String::new(),
                "bare_identifier".to_string(),
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

    #[tokio::test]
    async fn indexed_files_rows_stay_transactionally_aligned_with_segments() {
        let (_db, conn) = setup().await;

        let mut seg_a1 = test_segment("a1", "src/a.rs", "hash-a");
        seg_a1.called_symbols = r#"["call_a"]"#.to_string();
        seg_a1.defined_symbols = r#"["SymA"]"#.to_string();
        let seg_a2 = test_segment("a2", "src/a.rs", "hash-a");
        let seg_b1 = test_segment("b1", "src/b.rs", "hash-b");

        let meta_a = IndexedFileMeta {
            extension: "rs".to_string(),
            file_hash: "hash-a".to_string(),
            file_size: 100,
            modified_ns: 1_000_000,
        };
        let meta_b = IndexedFileMeta {
            extension: "rs".to_string(),
            file_hash: "hash-b".to_string(),
            file_size: 200,
            modified_ns: 2_000_000,
        };

        replace_file_batch_tx(
            &conn,
            &[
                FileSegmentBatch {
                    file_path: "src/a.rs",
                    segments: &[seg_a1, seg_a2],
                    manifest_meta: Some(&meta_a),
                },
                FileSegmentBatch {
                    file_path: "src/b.rs",
                    segments: &[seg_b1],
                    manifest_meta: Some(&meta_b),
                },
            ],
        )
        .await
        .unwrap();

        let manifest = get_all_indexed_files(&conn).await.unwrap();
        assert_eq!(manifest.len(), 2);
        assert_eq!(manifest["src/a.rs"].file_hash, "hash-a");
        assert_eq!(manifest["src/a.rs"].file_size, 100);
        assert_eq!(manifest["src/b.rs"].file_hash, "hash-b");
        assert_eq!(manifest["src/b.rs"].file_size, 200);

        let seg_a = get_segments_by_file(&conn, "src/a.rs").await.unwrap();
        assert_eq!(seg_a.len(), 2);
        assert!(!relation_rows(&conn, "a1").await.is_empty());
        assert!(!symbol_rows(&conn, "a1").await.is_empty());

        let new_a1 = test_segment("a1_v2", "src/a.rs", "hash-a-v2");
        let meta_a_v2 = IndexedFileMeta {
            extension: "rs".to_string(),
            file_hash: "hash-a-v2".to_string(),
            file_size: 150,
            modified_ns: 3_000_000,
        };

        replace_file_segments_tx_with_meta(&conn, "src/a.rs", &[new_a1], Some(&meta_a_v2))
            .await
            .unwrap();

        let manifest = get_all_indexed_files(&conn).await.unwrap();
        assert_eq!(manifest.len(), 2);
        assert_eq!(manifest["src/a.rs"].file_hash, "hash-a-v2");
        assert_eq!(manifest["src/a.rs"].file_size, 150);
        assert_eq!(manifest["src/b.rs"].file_hash, "hash-b");

        let seg_a = get_segments_by_file(&conn, "src/a.rs").await.unwrap();
        assert_eq!(seg_a.len(), 1);
        assert_eq!(seg_a[0].id, "a1_v2");
        assert!(relation_rows(&conn, "a1").await.is_empty());
        assert!(symbol_rows(&conn, "a1").await.is_empty());

        replace_file_segments_tx(&conn, "src/b.rs", &[])
            .await
            .unwrap();

        let manifest = get_all_indexed_files(&conn).await.unwrap();
        assert_eq!(manifest.len(), 1);
        assert!(manifest.contains_key("src/a.rs"));
        assert!(!manifest.contains_key("src/b.rs"));
        assert!(get_segments_by_file(&conn, "src/b.rs")
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn batch_rollback_keeps_indexed_files_aligned() {
        let (_db, conn) = setup().await;

        let seg_a = test_segment("old_a", "src/a.rs", "old-hash");
        let meta_a = IndexedFileMeta {
            extension: "rs".to_string(),
            file_hash: "old-hash".to_string(),
            file_size: 50,
            modified_ns: 1_000_000,
        };
        replace_file_segments_tx_with_meta(&conn, "src/a.rs", &[seg_a], Some(&meta_a))
            .await
            .unwrap();

        let new_a = test_segment("new_a", "src/a.rs", "new-hash");
        let mut bad_b = test_segment("bad_b", "src/b.rs", "b-hash");
        bad_b.embedding_vec = Some("not-a-vector".to_string());

        let result = replace_file_batch_tx(
            &conn,
            &[
                FileSegmentBatch {
                    file_path: "src/a.rs",
                    segments: &[new_a],
                    manifest_meta: Some(&IndexedFileMeta {
                        extension: "rs".to_string(),
                        file_hash: "new-hash".to_string(),
                        file_size: 100,
                        modified_ns: 2_000_000,
                    }),
                },
                FileSegmentBatch {
                    file_path: "src/b.rs",
                    segments: &[bad_b],
                    manifest_meta: Some(&IndexedFileMeta {
                        extension: "rs".to_string(),
                        file_hash: "b-hash".to_string(),
                        file_size: 200,
                        modified_ns: 3_000_000,
                    }),
                },
            ],
        )
        .await;

        assert!(result.is_err());

        let manifest = get_all_indexed_files(&conn).await.unwrap();
        assert_eq!(manifest.len(), 1);
        assert_eq!(manifest["src/a.rs"].file_hash, "old-hash");
        assert_eq!(manifest["src/a.rs"].file_size, 50);
        assert!(!manifest.contains_key("src/b.rs"));

        let seg_a = get_segments_by_file(&conn, "src/a.rs").await.unwrap();
        assert_eq!(seg_a.len(), 1);
        assert_eq!(seg_a[0].id, "old_a");
    }
}
