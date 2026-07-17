use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use libsql::Connection;
use sha2::{Digest, Sha256};

use crate::shared::constants::DEFAULT_INDEX_CONTEXT_ID;
use crate::shared::errors::{OneupError, StorageError};
use crate::shared::symbols::{normalize_symbolish, EDGE_IDENTITY_BARE_IDENTIFIER};
use crate::shared::types::{ParsedRelation, ReferenceKind, SegmentRole, WorktreeContext};
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
    /// Content-addressed key into `embedding_pool` for an embeddable segment,
    /// resolved by the lookup-before-embed pipeline. `None` for non-embeddable
    /// segments and for runs with no active embedder. When `Some`, the pool-aware
    /// write path writes a `segment_vectors(segment_id, content_key)` reference
    /// and reconciles the pool `ref_count`.
    // Transient: produced by the lookup-before-embed pipeline but not read
    // until the pool-aware write path consumes it. Remove this attribute
    // when the pool-aware write path wires `content_key` into the
    // `segment_vectors` write.
    #[allow(dead_code)]
    pub content_key: Option<String>,
    /// Serialized embedding vector for this segment, present only when the
    /// content is *new* under the current model (a pool miss this run) and must
    /// be inserted into `embedding_pool`. `None` for a pool hit (the shared
    /// vector already exists) or a non-embeddable segment. Pairs with
    /// `content_key`: a hit is `content_key: Some, embedding_vec: None`.
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

    write_segment_vector_reference(conn, seg).await?;

    replace_segment_symbols_for_context(conn, context_id, seg).await?;

    Ok(())
}

/// Reconcile one segment's `segment_vectors` reference and the pool `ref_count`
/// it holds. The single-segment counterpart to [`batch_upsert_vectors`]:
///
/// - An embeddable segment (`content_key` set) points at its pooled vector. A
///   pool miss (`embedding_vec` set) inserts the shared row idempotently first;
///   a hit reuses the existing row. The reference is then incremented.
/// - A non-embeddable segment (`content_key` `None`) drops any prior reference
///   and decrements the pool row it used to hold.
///
/// The increment/decrement here are explicit because this path performs no
/// preceding segment delete (unlike the replace path, where the
/// `segments_vector_ad` trigger does the decrement). It is idempotent for the
/// no-op case and safe to re-run.
async fn write_segment_vector_reference(
    conn: &Connection,
    seg: &SegmentInsert,
) -> Result<(), OneupError> {
    let Some(content_key) = &seg.content_key else {
        // Non-embeddable (or embedder-absent): give up any prior pool reference,
        // then remove the now-orphaned segment_vectors row. The decrement must
        // run first, while the row still names its content_key.
        conn.execute(
            queries::DECREMENT_EMBEDDING_POOL_REF_COUNT_FOR_SEGMENT,
            [seg.id.clone()],
        )
        .await
        .map_err(|e| StorageError::Query(format!("decrement pool ref_count failed: {e}")))?;
        conn.execute(queries::DELETE_SEGMENT_VECTOR, [seg.id.clone()])
            .await
            .map_err(|e| StorageError::Query(format!("delete segment vector failed: {e}")))?;
        return Ok(());
    };

    if let Some(embedding_vec) = &seg.embedding_vec {
        conn.execute(
            queries::UPSERT_EMBEDDING_POOL,
            libsql::params![content_key.clone(), embedding_vec.clone()],
        )
        .await
        .map_err(|e| StorageError::Query(format!("upsert embedding pool failed: {e}")))?;
    }

    conn.execute(
        queries::UPSERT_SEGMENT_VECTOR,
        libsql::params![seg.id.clone(), content_key.clone()],
    )
    .await
    .map_err(|e| StorageError::Query(format!("upsert segment vector failed: {e}")))?;

    conn.execute(
        queries::INCREMENT_EMBEDDING_POOL_REF_COUNT,
        libsql::params![content_key.clone(), 1_i64],
    )
    .await
    .map_err(|e| StorageError::Query(format!("increment pool ref_count failed: {e}")))?;

    Ok(())
}

/// Returns the subset of `content_keys` that already exist in `embedding_pool`.
///
/// The lookup-before-embed pipeline uses this to embed only genuinely new
/// content: any key already present resolves to its shared vector instead of
/// being re-embedded. The check is batched in `SQLITE_MAX_PARAMS`-sized
/// chunks so an arbitrarily large key set stays within the bound-parameter
/// limit. Read-only: it never mutates the pool or `ref_count`.
pub async fn existing_embedding_pool_keys(
    conn: &Connection,
    content_keys: &[&str],
) -> Result<HashSet<String>, OneupError> {
    let mut present = HashSet::new();
    if content_keys.is_empty() {
        return Ok(present);
    }

    for chunk in content_keys.chunks(queries::SQLITE_MAX_PARAMS) {
        let mut sql = String::from(queries::SELECT_EMBEDDING_POOL_KEYS_PREFIX);
        for i in 0..chunk.len() {
            if i > 0 {
                sql.push_str(", ");
            }
            write!(sql, "?{}", i + 1).expect("write to String cannot fail");
        }
        sql.push(')');

        let params: Vec<libsql::Value> =
            chunk.iter().map(|key| (*key).to_string().into()).collect();

        let mut rows = conn
            .query(&sql, params)
            .await
            .map_err(|e| StorageError::Query(format!("query embedding pool keys failed: {e}")))?;
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| StorageError::Query(format!("embedding pool key iteration failed: {e}")))?
        {
            let key: String = row
                .get(0)
                .map_err(|e| StorageError::Query(format!("decode content_key failed: {e}")))?;
            present.insert(key);
        }
    }

    Ok(present)
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
#[allow(dead_code)]
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

/// Batch-fetch segments by exact id within one context, returning a map from
/// `id -> StoredSegment` for the ids that matched a row. An id with no matching
/// row is simply absent from the map — the same outcome as an individual
/// [`get_segment_by_id_for_context`] returning `None` — and duplicate ids are
/// harmless (each row keys on its own id). Batched in `SQLITE_MAX_PARAMS`-sized
/// chunks (reserving one bound slot for the leading context id) so an
/// arbitrarily large handle set stays within the bound-parameter limit.
pub async fn get_segments_by_ids_for_context(
    conn: &Connection,
    context_id: &str,
    ids: &[String],
) -> Result<HashMap<String, StoredSegment>, OneupError> {
    let mut found = HashMap::new();
    if ids.is_empty() {
        return Ok(found);
    }
    validate_context_id(context_id)?;

    // Reserve one bound-parameter slot for the leading context id (?1); the id
    // list occupies the remaining ?2.. placeholders of each chunk.
    for chunk in ids.chunks(queries::SQLITE_MAX_PARAMS - 1) {
        let sql = queries::select_segments_by_ids_for_context_sql(chunk.len());
        let mut params: Vec<libsql::Value> = Vec::with_capacity(chunk.len() + 1);
        params.push(context_id.to_string().into());
        params.extend(chunk.iter().map(|id| id.clone().into()));

        let mut rows = conn
            .query(&sql, params)
            .await
            .map_err(|e| StorageError::Query(format!("batch query segments by id failed: {e}")))?;
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| StorageError::Query(format!("row iteration failed: {e}")))?
        {
            let segment = row_to_stored_segment(&row)?;
            found.insert(segment.id.clone(), segment);
        }
    }

    Ok(found)
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

/// Escape LIKE wildcards in a handle prefix so `%` and `_` match literally under the
/// `LIKE ?||'%' ESCAPE '\'` prefix clauses. Mirrors the SQL escape character `\`:
/// `\`→`\\`, `%`→`\%`, `_`→`\_`. A wildcard-free prefix is returned byte-identical.
fn escape_like_prefix(prefix: &str) -> String {
    let mut escaped = String::with_capacity(prefix.len());
    for ch in prefix.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
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

    let escaped_prefix = escape_like_prefix(prefix);
    let mut rows = conn
        .query(
            queries::SELECT_SEGMENTS_BY_PREFIX,
            [escaped_prefix.as_str()],
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

    let escaped_prefix = escape_like_prefix(prefix);
    let mut rows = conn
        .query(
            queries::SELECT_SEGMENTS_BY_PREFIX_FOR_CONTEXT,
            libsql::params![context_id, escaped_prefix.as_str()],
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

/// Fetch up to `limit` candidate segment ids sharing `prefix` within one
/// context, id-only, for the unique-prefix handle recovery gate. The
/// caller runs a pure recovery gate over the returned candidate set, so this
/// query deliberately returns bare ids (never full segment bodies) ordered
/// deterministically. An empty prefix returns no candidates rather than
/// matching every row.
pub async fn get_segment_ids_by_prefix_for_context(
    conn: &Connection,
    context_id: &str,
    prefix: &str,
    limit: usize,
) -> Result<Vec<String>, OneupError> {
    if prefix.is_empty() {
        return Ok(Vec::new());
    }
    validate_context_id(context_id)?;

    let escaped_prefix = escape_like_prefix(prefix);
    let mut rows = conn
        .query(
            queries::SELECT_SEGMENT_IDS_BY_PREFIX_FOR_CONTEXT,
            libsql::params![context_id, escaped_prefix.as_str(), limit as i64],
        )
        .await
        .map_err(|e| StorageError::Query(format!("query segment ids by prefix failed: {e}")))?;

    let mut ids = Vec::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|e| StorageError::Query(format!("row iteration failed: {e}")))?
    {
        let id: String = row
            .get(0)
            .map_err(|e| StorageError::Query(format!("decode segment id failed: {e}")))?;
        ids.push(id);
    }

    Ok(ids)
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

/// Record the worktree context row for an index run, including the repository
/// head commit OID the context was indexed at (`head_oid`). Replaces any
/// previously recorded row for the same `context_id`.
pub async fn upsert_worktree_context(
    conn: &Connection,
    context: &WorktreeContext,
    project_id: &str,
) -> Result<(), OneupError> {
    validate_context_id(&context.context_id)?;
    conn.execute(
        queries::UPSERT_WORKTREE_CONTEXT,
        libsql::params![
            context.context_id.clone(),
            project_id.to_string(),
            context.state_root.to_string_lossy().into_owned(),
            context.source_root.to_string_lossy().into_owned(),
            context.main_worktree_root.to_string_lossy().into_owned(),
            context.worktree_role.as_str(),
            context.branch_name.clone(),
            context.branch_ref.clone(),
            context.branch_status.as_str(),
            context.head_oid.clone(),
            context
                .git_dir
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
            context
                .common_git_dir
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
        ],
    )
    .await
    .map_err(|e| StorageError::Query(format!("upsert worktree context failed: {e}")))?;
    Ok(())
}

/// Read the head commit OID recorded for a context by its last successful
/// index run. Returns `None` when the context has never been recorded or was
/// indexed without a known repository HEAD.
pub async fn get_worktree_context_head_oid(
    conn: &Connection,
    context_id: &str,
) -> Result<Option<String>, OneupError> {
    validate_context_id(context_id)?;
    let mut rows = conn
        .query(queries::SELECT_WORKTREE_CONTEXT_HEAD_OID, [context_id])
        .await
        .map_err(|e| StorageError::Query(format!("query worktree context head failed: {e}")))?;

    match rows
        .next()
        .await
        .map_err(|e| StorageError::Query(format!("row iteration failed: {e}")))?
    {
        Some(row) => {
            let head_oid: Option<String> = row.get(0).map_err(|e| {
                StorageError::Query(format!("read worktree context head failed: {e}"))
            })?;
            Ok(head_oid)
        }
        None => Ok(None),
    }
}

/// One row from `worktree_contexts`: a context recorded in the shared index, with
/// the worktree paths and branch that minted its `context_id`. Consumed by `1up gc`
/// to classify stale branch snapshots and dead worktrees.
///
/// `updated_at` is the raw `datetime('now')` TEXT value (`YYYY-MM-DD HH:MM:SS`,
/// UTC), bumped on every successful index run for this context; it is the
/// keep-count/age signal for the `SupersededSameSource` retention policy.
#[derive(Debug, Clone)]
pub struct IndexedContextRow {
    pub context_id: String,
    pub state_root: PathBuf,
    pub source_root: PathBuf,
    pub branch_name: Option<String>,
    pub updated_at: String,
}

/// List every worktree context recorded in the shared index.
pub async fn list_worktree_contexts(
    conn: &Connection,
) -> Result<Vec<IndexedContextRow>, OneupError> {
    let mut rows = conn
        .query(queries::SELECT_ALL_WORKTREE_CONTEXTS, ())
        .await
        .map_err(|e| StorageError::Query(format!("list worktree contexts failed: {e}")))?;

    let mut contexts = Vec::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|e| StorageError::Query(format!("row iteration failed: {e}")))?
    {
        let context_id: String = row
            .get(0)
            .map_err(|e| StorageError::Query(format!("read context_id failed: {e}")))?;
        let state_root: String = row
            .get(1)
            .map_err(|e| StorageError::Query(format!("read state_root failed: {e}")))?;
        let source_root: String = row
            .get(2)
            .map_err(|e| StorageError::Query(format!("read source_root failed: {e}")))?;
        let branch_name: Option<String> = row
            .get(3)
            .map_err(|e| StorageError::Query(format!("read branch_name failed: {e}")))?;
        let updated_at: String = row
            .get(4)
            .map_err(|e| StorageError::Query(format!("read updated_at failed: {e}")))?;
        contexts.push(IndexedContextRow {
            context_id,
            state_root: PathBuf::from(state_root),
            source_root: PathBuf::from(source_root),
            branch_name,
            updated_at,
        });
    }
    Ok(contexts)
}

/// Row counts removed from the shared index when a context is pruned.
#[derive(Debug, Clone, Copy, Default)]
pub struct ContextDeletionCounts {
    pub segments: u64,
    pub relations: u64,
    pub indexed_files: u64,
}

/// Evict one worktree context from the shared index, removing its rows from every
/// context-scoped table. Deleting `segments` cascades to `segment_vectors`,
/// `segment_symbols`, and the FTS index via the schema's AFTER DELETE triggers, so
/// only `segment_relations`, `indexed_files`, `segments`, and the `worktree_contexts`
/// registry row are deleted explicitly.
///
/// Reference-aware: embeddings are content-addressed and shared
/// across contexts via `embedding_pool`. The `segments_vector_ad` trigger
/// decrements `ref_count` as this context's segments are deleted, then the
/// delete-at-zero sweep removes only the pool rows whose last referencer is now
/// gone. A vector still referenced by another context keeps `ref_count >= 1` and
/// survives, so deleting one context never disturbs another's embeddings.
///
/// This is the single convergence point for both removal paths — explicit
/// `cli::gc` removal and the daemon startup source-missing prune — so both are
/// reference-aware by construction.
///
/// Idempotent: re-running on an already-pruned context deletes nothing, sweeps
/// no orphans, and still succeeds, so a partial failure is safe to retry.
pub async fn delete_context(
    conn: &Connection,
    context_id: &str,
) -> Result<ContextDeletionCounts, OneupError> {
    validate_context_id(context_id)?;

    let relations = conn
        .execute(queries::DELETE_SEGMENT_RELATIONS_BY_CONTEXT, [context_id])
        .await
        .map_err(|e| StorageError::Query(format!("delete context relations failed: {e}")))?;
    let indexed_files = conn
        .execute(queries::DELETE_INDEXED_FILES_BY_CONTEXT, [context_id])
        .await
        .map_err(|e| StorageError::Query(format!("delete context indexed files failed: {e}")))?;
    let segments = conn
        .execute(queries::DELETE_SEGMENTS_BY_CONTEXT, [context_id])
        .await
        .map_err(|e| StorageError::Query(format!("delete context segments failed: {e}")))?;
    // The segment deletes above fired `segments_vector_ad`, decrementing the pool
    // ref_count for each removed reference; now reclaim the rows that dropped to
    // zero references. Runs after the deletes so the counts are settled.
    conn.execute(queries::DELETE_ORPHANED_EMBEDDING_POOL_ROWS, ())
        .await
        .map_err(|e| StorageError::Query(format!("prune orphaned pool vectors failed: {e}")))?;
    conn.execute(queries::DELETE_WORKTREE_CONTEXT, [context_id])
        .await
        .map_err(|e| StorageError::Query(format!("delete worktree context row failed: {e}")))?;

    Ok(ContextDeletionCounts {
        segments,
        relations,
        indexed_files,
    })
}

/// Reclaim disk space freed by deleted rows. SQLite/libSQL keep emptied pages
/// allocated after a `DELETE`, so `VACUUM` is required to actually shrink the
/// `index.db` file. Runs outside any transaction and needs exclusive database
/// access, so callers should hold the rebuild lock and surface lock contention
/// (a live daemon) as an actionable "stop the daemon and retry" error.
pub async fn vacuum_database(conn: &Connection) -> Result<(), OneupError> {
    conn.execute(queries::VACUUM_DATABASE, ())
        .await
        .map_err(|e| StorageError::Query(format!("vacuum failed: {e}")))?;
    Ok(())
}

/// Bytes already freed by prior deletes but not yet returned to the filesystem:
/// `PRAGMA freelist_count * PRAGMA page_size`. Exact, not an estimate — the
/// floor of `1up status`'s reclaimable-bytes reporting (see [`vacuum_database`]
/// for why a `VACUUM` is still needed to actually shrink `index.db`).
pub async fn freelist_reclaimable_bytes(conn: &Connection) -> Result<u64, OneupError> {
    let freelist_count =
        read_pragma_u64(conn, queries::PRAGMA_FREELIST_COUNT, "freelist_count").await?;
    let page_size = read_pragma_u64(conn, queries::PRAGMA_PAGE_SIZE, "page_size").await?;
    Ok(freelist_count.saturating_mul(page_size))
}

async fn read_pragma_u64(conn: &Connection, sql: &str, label: &str) -> Result<u64, OneupError> {
    let mut rows = conn
        .query(sql, ())
        .await
        .map_err(|e| StorageError::Query(format!("read {label} failed: {e}")))?;
    match rows
        .next()
        .await
        .map_err(|e| StorageError::Query(format!("row iteration failed: {e}")))?
    {
        Some(row) => {
            let value: i64 = row
                .get(0)
                .map_err(|e| StorageError::Query(format!("parse {label} failed: {e}")))?;
            Ok(value.max(0) as u64)
        }
        None => Ok(0),
    }
}

/// A conservative, self-contained proxy for segments belonging to recorded
/// contexts (other than `active_context_id`) whose `source_root` no longer
/// exists on disk. Deliberately lighter than `1up gc`'s full `prune_reason`
/// classification (a stale-branch-snapshot or nested-subdir context whose
/// source still exists is not counted here) — a proxy signal for `1up
/// status`'s at-a-glance reclaimable-bytes estimate, not a claim that `1up gc
/// --apply` would prune exactly this set.
pub async fn prunable_segments_proxy(
    conn: &Connection,
    active_context_id: &str,
) -> Result<u64, OneupError> {
    let contexts = list_worktree_contexts(conn).await?;
    let mut total = 0u64;
    for ctx in &contexts {
        if ctx.context_id != active_context_id && !ctx.source_root.exists() {
            total += count_segments_for_context(conn, &ctx.context_id).await?;
        }
    }
    Ok(total)
}

/// Pure predicate: is `candidate` a stale per-branch snapshot of the `active`
/// live worktree — recorded under the same `state_root` and `source_root` but a
/// *different* `context_id` (the `context_id` embeds the branch, so a
/// same-worktree context that is not the active one is a leftover per-branch
/// index that rebuilds on demand if the branch is revisited)?
///
/// This is the single source of truth for "stale-branch snapshot" shared by
/// `1up gc`'s classifier, the `1up status`/`1up list` disclosure stats, and the
/// daemon's conservative auto-prune, so the three can never disagree on which
/// contexts a `1up gc` would reclaim. It intentionally does not check source
/// existence: a stale-branch snapshot shares the *active* worktree's
/// `source_root`, which is live by construction (a dead worktree is handled by
/// the source-missing path instead).
pub fn is_stale_branch_snapshot(active: &WorktreeContext, candidate: &IndexedContextRow) -> bool {
    candidate.context_id != active.context_id
        && candidate.state_root == active.state_root
        && candidate.source_root == active.source_root
}

/// True when `updated_at` (a `datetime('now')`-formatted TEXT value,
/// `YYYY-MM-DD HH:MM:SS`, UTC — see `worktree_contexts.updated_at`) is at least
/// `min_age` old relative to `now`. Unparseable input degrades to `false` (not
/// old enough): a retention decision must never prune on ambiguous data. Shared
/// by `1up gc`'s retention policy and the daemon's stale-branch auto-prune age
/// gate.
pub fn context_age_at_least(updated_at: &str, now: DateTime<Utc>, min_age: Duration) -> bool {
    match chrono::NaiveDateTime::parse_from_str(updated_at, "%Y-%m-%d %H:%M:%S") {
        Ok(parsed) => now - parsed.and_utc() >= min_age,
        Err(_) => false,
    }
}

/// Cheap, bounded, best-effort disclosure stats for `1up status`/`1up list`:
/// the count of stale-branch snapshot contexts for the active worktree (what
/// `1up gc` would reclaim unconditionally) plus an estimate of reclaimable
/// bytes.
///
/// The byte estimate is the exact already-free `freelist` floor plus a
/// proportional proxy for the segments belonging to prunable contexts
/// (source-missing peers via [`prunable_segments_proxy`] and stale-branch
/// snapshots), sized against the current `index.db` file. It is an estimate —
/// exact bytes are only known after `1up gc --apply`'s VACUUM. Bounded: it lists
/// the (few) recorded contexts and counts segments per prunable one; no full
/// table walk and never a VACUUM.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DisclosureStats {
    /// Number of stale-branch snapshot contexts for the active worktree.
    pub stale_contexts: u64,
    /// Estimated bytes `1up gc --apply` could reclaim (freelist floor + prunable
    /// segment proxy).
    pub reclaimable_bytes: u64,
}

pub async fn disclosure_stats(
    conn: &Connection,
    db_path: &Path,
    active: &WorktreeContext,
) -> Result<DisclosureStats, OneupError> {
    let freelist_bytes = freelist_reclaimable_bytes(conn).await?;
    // Source-missing peers are reclaimable too; reuse the single-source proxy.
    // These are disjoint from stale-branch snapshots (whose source_root equals
    // the live active source_root, which exists), so summing never double-counts.
    let source_missing_segments = prunable_segments_proxy(conn, &active.context_id).await?;

    let contexts = list_worktree_contexts(conn).await?;
    let mut stale_contexts = 0u64;
    let mut stale_segments = 0u64;
    for ctx in &contexts {
        if is_stale_branch_snapshot(active, ctx) {
            stale_contexts += 1;
            stale_segments = stale_segments
                .saturating_add(count_segments_for_context(conn, &ctx.context_id).await?);
        }
    }

    let prunable_segments = source_missing_segments.saturating_add(stale_segments);
    let total_segments = count_segments(conn).await?;
    let proxy_bytes = if prunable_segments == 0 || total_segments == 0 {
        0
    } else {
        let file_size = std::fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);
        ((prunable_segments as f64 / total_segments as f64) * file_size as f64).round() as u64
    };

    Ok(DisclosureStats {
        stale_contexts,
        reclaimable_bytes: freelist_bytes.saturating_add(proxy_bytes),
    })
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

/// Count stored vector rows for segments in a worktree context.
pub async fn count_vector_rows_for_context(
    conn: &Connection,
    context_id: &str,
) -> Result<u64, OneupError> {
    validate_context_id(context_id)?;
    count_single_value(
        conn,
        queries::COUNT_VECTOR_ROWS_FOR_CONTEXT,
        context_id,
        "count context vector rows",
    )
    .await
}

/// Count segments the pipeline would embed in a worktree context.
pub async fn count_embeddable_segments_for_context(
    conn: &Connection,
    context_id: &str,
) -> Result<u64, OneupError> {
    validate_context_id(context_id)?;
    count_single_value(
        conn,
        queries::COUNT_EMBEDDABLE_SEGMENTS_FOR_CONTEXT.as_str(),
        context_id,
        "count context embeddable segments",
    )
    .await
}

async fn count_single_value(
    conn: &Connection,
    sql: &str,
    context_id: &str,
    label: &str,
) -> Result<u64, OneupError> {
    let mut rows = conn
        .query(sql, [context_id])
        .await
        .map_err(|e| StorageError::Query(format!("{label} failed: {e}")))?;

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

/// Per-language file and segment counts inside one index context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanguageStat {
    pub language: String,
    pub files: u64,
    pub segments: u64,
}

/// Per-module segment count aggregated from segment file paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleSegmentCount {
    pub module: String,
    pub segments: u64,
}

/// A shallow orchestration/definition segment that may serve as an overview
/// entry point. Test/low-signal path exclusion happens in the overview engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryPointCandidate {
    pub segment_id: String,
    pub file_path: String,
    pub line_start: i64,
    pub line_end: i64,
    pub role: String,
    pub breadcrumb: Option<String>,
    pub defined_symbols: String,
}

/// A qualifying type definition row resolved for an overview symbol key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QualifyingTypeDefinition {
    pub symbol_key: String,
    pub symbol: String,
    pub segment_id: String,
    pub file_path: String,
    pub line_start: i64,
    pub line_end: i64,
    pub block_type: String,
}

/// Per-language file and segment counts for one index context, ordered by
/// segment count descending then language ascending.
pub async fn get_language_stats_for_context(
    conn: &Connection,
    context_id: &str,
    limit: usize,
) -> Result<Vec<LanguageStat>, OneupError> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    validate_context_id(context_id)?;

    let mut rows = conn
        .query(
            queries::SELECT_LANGUAGE_STATS_FOR_CONTEXT,
            libsql::params![context_id, limit as i64],
        )
        .await
        .map_err(|e| StorageError::Query(format!("query language stats failed: {e}")))?;

    let mut results = Vec::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|e| StorageError::Query(format!("row iteration failed: {e}")))?
    {
        let files: i64 = row
            .get(1)
            .map_err(|e| StorageError::Query(format!("read file_count failed: {e}")))?;
        let segments: i64 = row
            .get(2)
            .map_err(|e| StorageError::Query(format!("read segment_count failed: {e}")))?;
        results.push(LanguageStat {
            language: row
                .get(0)
                .map_err(|e| StorageError::Query(format!("read language failed: {e}")))?,
            files: files as u64,
            segments: segments as u64,
        });
    }

    Ok(results)
}

async fn fetch_module_segment_counts(
    conn: &Connection,
    sql: &str,
    params: impl libsql::params::IntoParams,
) -> Result<Vec<ModuleSegmentCount>, OneupError> {
    let mut rows = conn
        .query(sql, params)
        .await
        .map_err(|e| StorageError::Query(format!("query module segment counts failed: {e}")))?;

    let mut results = Vec::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|e| StorageError::Query(format!("row iteration failed: {e}")))?
    {
        let segments: i64 = row
            .get(1)
            .map_err(|e| StorageError::Query(format!("read segment_count failed: {e}")))?;
        results.push(ModuleSegmentCount {
            module: row
                .get(0)
                .map_err(|e| StorageError::Query(format!("read module failed: {e}")))?,
            segments: segments as u64,
        });
    }

    Ok(results)
}

/// Depth-1 module segment counts for one index context. The module key is the
/// first path component; top-level files map to `(root)`.
pub async fn get_module_segment_counts_for_context(
    conn: &Connection,
    context_id: &str,
    limit: usize,
) -> Result<Vec<ModuleSegmentCount>, OneupError> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    validate_context_id(context_id)?;

    fetch_module_segment_counts(
        conn,
        queries::SELECT_MODULE_SEGMENT_COUNTS_FOR_CONTEXT.as_str(),
        libsql::params![context_id, limit as i64],
    )
    .await
}

/// Depth-2 segment counts under one depth-1 module for the dominant-module
/// expansion. Files directly inside the parent stay attributed to the parent
/// module name.
pub async fn get_module_child_segment_counts_for_context(
    conn: &Connection,
    context_id: &str,
    parent_module: &str,
    limit: usize,
) -> Result<Vec<ModuleSegmentCount>, OneupError> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    validate_context_id(context_id)?;

    fetch_module_segment_counts(
        conn,
        queries::SELECT_MODULE_CHILD_SEGMENT_COUNTS_FOR_CONTEXT.as_str(),
        libsql::params![context_id, parent_module, limit as i64],
    )
    .await
}

/// Shallow orchestration/definition entry-point candidates for one index
/// context, ordered by path depth, role rank (orchestration first), path,
/// then line start.
pub async fn get_entry_point_candidates_for_context(
    conn: &Connection,
    context_id: &str,
    limit: usize,
) -> Result<Vec<EntryPointCandidate>, OneupError> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    validate_context_id(context_id)?;

    let mut rows = conn
        .query(
            queries::SELECT_ENTRY_POINT_CANDIDATES_FOR_CONTEXT,
            libsql::params![context_id, limit as i64],
        )
        .await
        .map_err(|e| StorageError::Query(format!("query entry point candidates failed: {e}")))?;

    let mut results = Vec::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|e| StorageError::Query(format!("row iteration failed: {e}")))?
    {
        results.push(EntryPointCandidate {
            segment_id: row
                .get(0)
                .map_err(|e| StorageError::Query(format!("read id failed: {e}")))?,
            file_path: row
                .get(1)
                .map_err(|e| StorageError::Query(format!("read file_path failed: {e}")))?,
            line_start: row
                .get(2)
                .map_err(|e| StorageError::Query(format!("read line_start failed: {e}")))?,
            line_end: row
                .get(3)
                .map_err(|e| StorageError::Query(format!("read line_end failed: {e}")))?,
            role: row
                .get(4)
                .map_err(|e| StorageError::Query(format!("read role failed: {e}")))?,
            breadcrumb: row
                .get(5)
                .map_err(|e| StorageError::Query(format!("read breadcrumb failed: {e}")))?,
            defined_symbols: row
                .get(6)
                .map_err(|e| StorageError::Query(format!("read defined_symbols failed: {e}")))?,
        });
    }

    Ok(results)
}

/// Resolve qualifying type definitions for the requested overview symbol
/// keys inside one index context, ordered by symbol key, file path, line
/// start, then segment id.
pub async fn get_qualifying_type_definitions_for_context(
    conn: &Connection,
    context_id: &str,
    symbol_keys: &[String],
    limit: usize,
) -> Result<Vec<QualifyingTypeDefinition>, OneupError> {
    if symbol_keys.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    validate_context_id(context_id)?;
    if symbol_keys.len() + 2 > queries::SQLITE_MAX_PARAMS {
        return Err(StorageError::Query(format!(
            "qualifying definition lookup for {} keys exceeds the {} parameter budget",
            symbol_keys.len(),
            queries::SQLITE_MAX_PARAMS
        ))
        .into());
    }

    let sql = queries::select_qualifying_type_definitions_for_context_sql(symbol_keys.len());
    let mut params: Vec<libsql::Value> = Vec::with_capacity(symbol_keys.len() + 2);
    params.push(context_id.to_string().into());
    for symbol_key in symbol_keys {
        params.push(symbol_key.clone().into());
    }
    params.push((limit as i64).into());

    let mut rows = conn
        .query(&sql, params)
        .await
        .map_err(|e| StorageError::Query(format!("query qualifying definitions failed: {e}")))?;

    let mut results = Vec::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|e| StorageError::Query(format!("row iteration failed: {e}")))?
    {
        results.push(QualifyingTypeDefinition {
            symbol_key: row
                .get(0)
                .map_err(|e| StorageError::Query(format!("read symbol_key failed: {e}")))?,
            symbol: row
                .get(1)
                .map_err(|e| StorageError::Query(format!("read symbol failed: {e}")))?,
            segment_id: row
                .get(2)
                .map_err(|e| StorageError::Query(format!("read id failed: {e}")))?,
            file_path: row
                .get(3)
                .map_err(|e| StorageError::Query(format!("read file_path failed: {e}")))?,
            line_start: row
                .get(4)
                .map_err(|e| StorageError::Query(format!("read line_start failed: {e}")))?,
            line_end: row
                .get(5)
                .map_err(|e| StorageError::Query(format!("read line_end failed: {e}")))?,
            block_type: row
                .get(6)
                .map_err(|e| StorageError::Query(format!("read block_type failed: {e}")))?,
        });
    }

    Ok(results)
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
            "INSERT INTO segments (\
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

        sql.push_str(queries::SEGMENT_UPSERT_CONFLICT_CLAUSE);

        conn.execute(&sql, params)
            .await
            .map_err(|e| StorageError::Query(format!("batch upsert segments failed: {e}")))?;
    }

    Ok(())
}

/// Persist the pooled embedding references for one file's segments.
///
/// Each embeddable segment carries a `content_key`; a pool *miss* additionally
/// carries the freshly embedded `embedding_vec` to be shared. The write proceeds
/// in three ordered phases so the pool row always exists before anything counts
/// against it:
///
/// 1. Insert pool rows for misses (`INSERT ... ON CONFLICT(content_key) DO
///    NOTHING`) — idempotent across contexts and concurrent writers, storing
///    each distinct `(model, content)` vector exactly once.
/// 2. Insert the thin `segment_vectors(segment_id, content_key)` references.
/// 3. Increment each pool row's `ref_count` by the number of references just
///    written for its key, keeping `ref_count` equal to the referencing-row
///    count.
///
/// Callers invoke this inside the per-file replace transaction *after*
/// `delete_segments_by_file_only_for_context`, whose `segments_vector_ad` trigger
/// has already decremented and removed the prior references, so the inserts here
/// are fresh and the `+1`-per-row increment is exact.
async fn batch_upsert_vectors(
    conn: &Connection,
    segments: &[SegmentInsert],
) -> Result<(), OneupError> {
    let ref_segments: Vec<&SegmentInsert> = segments
        .iter()
        .filter(|seg| seg.content_key.is_some())
        .collect();

    if ref_segments.is_empty() {
        return Ok(());
    }

    batch_upsert_embedding_pool(conn, &ref_segments).await?;
    batch_insert_segment_vector_refs(conn, &ref_segments).await?;
    batch_increment_pool_ref_counts(conn, &ref_segments).await?;

    Ok(())
}

/// Phase 1: idempotently insert pool rows for the pool *misses* (segments
/// carrying a freshly embedded vector). Hits already have a pool row and are
/// skipped here.
async fn batch_upsert_embedding_pool(
    conn: &Connection,
    ref_segments: &[&SegmentInsert],
) -> Result<(), OneupError> {
    let miss_segments: Vec<&&SegmentInsert> = ref_segments
        .iter()
        .filter(|seg| seg.embedding_vec.is_some())
        .collect();

    if miss_segments.is_empty() {
        return Ok(());
    }

    for chunk in miss_segments.chunks(queries::POOL_CHUNK_SIZE) {
        let mut sql = String::from(
            "INSERT INTO embedding_pool (\
             content_key, embedding_vec, ref_count\
             ) VALUES ",
        );
        let mut params: Vec<libsql::Value> =
            Vec::with_capacity(chunk.len() * queries::POOL_INSERT_COLS);

        for (i, seg) in chunk.iter().enumerate() {
            if i > 0 {
                sql.push_str(", ");
            }
            let b = i * queries::POOL_INSERT_COLS;
            write!(sql, "(?{}, vector8(?{}), 0)", b + 1, b + 2)
                .expect("write to String cannot fail");

            params.push(seg.content_key.clone().unwrap().into());
            params.push(seg.embedding_vec.clone().unwrap().into());
        }

        sql.push_str(queries::EMBEDDING_POOL_UPSERT_CONFLICT_CLAUSE);

        conn.execute(&sql, params)
            .await
            .map_err(|e| StorageError::Query(format!("batch upsert embedding pool failed: {e}")))?;
    }

    Ok(())
}

/// Phase 2: insert the per-segment `segment_vectors` references into the pool.
async fn batch_insert_segment_vector_refs(
    conn: &Connection,
    ref_segments: &[&SegmentInsert],
) -> Result<(), OneupError> {
    for chunk in ref_segments.chunks(queries::VECTOR_CHUNK_SIZE) {
        let mut sql = String::from(
            "INSERT INTO segment_vectors (\
             segment_id, content_key\
             ) VALUES ",
        );
        let mut params: Vec<libsql::Value> =
            Vec::with_capacity(chunk.len() * queries::VECTOR_INSERT_COLS);

        for (i, seg) in chunk.iter().enumerate() {
            if i > 0 {
                sql.push_str(", ");
            }
            let b = i * queries::VECTOR_INSERT_COLS;
            write!(sql, "(?{}, ?{})", b + 1, b + 2).expect("write to String cannot fail");

            params.push(seg.id.clone().into());
            params.push(seg.content_key.clone().unwrap().into());
        }

        sql.push_str(queries::VECTOR_UPSERT_CONFLICT_CLAUSE);

        conn.execute(&sql, params)
            .await
            .map_err(|e| StorageError::Query(format!("batch upsert vectors failed: {e}")))?;
    }

    Ok(())
}

/// Phase 3: bump `ref_count` by the number of references written per distinct
/// content key, so it equals the live referencing-row count.
///
/// Dedup-heavy write batches reference many keys, so a single bulk `UPDATE`
/// replaces the prior per-key `UPDATE` loop: the per-distinct-key deltas
/// are serialized to a JSON `content_key -> delta` object and applied in one
/// statement via [`queries::BATCH_INCREMENT_EMBEDDING_POOL_REF_COUNTS`]. Counts
/// are identical to the loop because the keys are distinct (one pool row each)
/// and `embedding_pool` has no UPDATE trigger.
async fn batch_increment_pool_ref_counts(
    conn: &Connection,
    ref_segments: &[&SegmentInsert],
) -> Result<(), OneupError> {
    let mut counts: HashMap<&str, i64> = HashMap::new();
    for seg in ref_segments {
        let key = seg
            .content_key
            .as_deref()
            .expect("ref_segments are filtered to content_key-bearing segments");
        *counts.entry(key).or_insert(0) += 1;
    }

    if counts.is_empty() {
        return Ok(());
    }

    let deltas = serde_json::to_string(&counts)
        .map_err(|e| StorageError::Query(format!("serialize ref_count deltas failed: {e}")))?;

    conn.execute(
        queries::BATCH_INCREMENT_EMBEDDING_POOL_REF_COUNTS,
        libsql::params![deltas],
    )
    .await
    .map_err(|e| StorageError::Query(format!("increment pool ref_count failed: {e}")))?;

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
    use crate::shared::types::{BranchStatus, WorktreeRole};
    use crate::storage::{db::Db, schema};
    use std::path::PathBuf;

    async fn setup() -> (Db, Connection) {
        let db = Db::open_memory().await.unwrap();
        let conn = db.connect().unwrap();
        schema::initialize(&conn).await.unwrap();
        (db, conn)
    }

    fn test_worktree_context_row(context_id: &str, head_oid: Option<&str>) -> WorktreeContext {
        WorktreeContext {
            context_id: context_id.to_string(),
            state_root: PathBuf::from("/tmp/state"),
            source_root: PathBuf::from("/tmp/source"),
            main_worktree_root: PathBuf::from("/tmp/state"),
            worktree_role: WorktreeRole::Main,
            git_dir: None,
            common_git_dir: None,
            branch_name: Some("main".to_string()),
            branch_ref: Some("refs/heads/main".to_string()),
            head_oid: head_oid.map(str::to_string),
            branch_status: BranchStatus::Named,
        }
    }

    #[tokio::test]
    async fn existing_embedding_pool_keys_returns_only_present_keys() {
        let (_db, conn) = setup().await;

        let vector = serde_json::to_string(&vec![0.1f32; 384]).unwrap();
        for key in ["key-present-a", "key-present-b"] {
            conn.execute(
                "INSERT INTO embedding_pool (content_key, embedding_vec, ref_count) \
                 VALUES (?1, vector8(?2), 0)",
                libsql::params![key, vector.clone()],
            )
            .await
            .unwrap();
        }

        // The lookup reports exactly the keys already stored, filtering out
        // absent ones, so the pipeline embeds only genuinely new content.
        let present =
            existing_embedding_pool_keys(&conn, &["key-present-a", "key-absent", "key-present-b"])
                .await
                .unwrap();
        assert_eq!(
            present,
            ["key-present-a".to_string(), "key-present-b".to_string()]
                .into_iter()
                .collect::<HashSet<_>>()
        );

        // An empty query set short-circuits without touching the database.
        assert!(existing_embedding_pool_keys(&conn, &[])
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn worktree_context_head_oid_write_read_roundtrip() {
        let (_db, conn) = setup().await;

        assert_eq!(
            get_worktree_context_head_oid(&conn, "ctx-a").await.unwrap(),
            None,
            "unrecorded context must read back as None"
        );

        upsert_worktree_context(
            &conn,
            &test_worktree_context_row("ctx-a", Some("aaa111")),
            "proj-1",
        )
        .await
        .unwrap();
        assert_eq!(
            get_worktree_context_head_oid(&conn, "ctx-a").await.unwrap(),
            Some("aaa111".to_string())
        );

        upsert_worktree_context(
            &conn,
            &test_worktree_context_row("ctx-a", Some("bbb222")),
            "proj-1",
        )
        .await
        .unwrap();
        assert_eq!(
            get_worktree_context_head_oid(&conn, "ctx-a").await.unwrap(),
            Some("bbb222".to_string()),
            "re-recording the same context must replace the head OID"
        );

        upsert_worktree_context(&conn, &test_worktree_context_row("ctx-b", None), "proj-1")
            .await
            .unwrap();
        assert_eq!(
            get_worktree_context_head_oid(&conn, "ctx-b").await.unwrap(),
            None,
            "a context indexed without a known HEAD must read back as None"
        );
        assert_eq!(
            get_worktree_context_head_oid(&conn, "ctx-a").await.unwrap(),
            Some("bbb222".to_string()),
            "contexts must stay isolated by context_id"
        );
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
            content_key: None,
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
    async fn prefix_lookup_escapes_like_wildcards() {
        let (_db, conn) = setup().await;

        // Hex ids carry no literal `_` or `%`, so an escaped wildcard prefix must
        // match nothing rather than treating the wildcard as a pattern.
        upsert_segment(
            &conn,
            &test_segment("a0f1e2c3d4b5f6a7", "src/lib.rs", "hash1"),
        )
        .await
        .unwrap();

        // `_` would wildcard-match the 'e' at that position without escaping.
        match get_segment_by_prefix(&conn, "a0f1_2c3").await.unwrap() {
            SegmentPrefixLookup::NotFound => {}
            other => panic!("expected NotFound for underscore prefix, got {other:?}"),
        }

        // `%` would match the whole id without escaping.
        match get_segment_by_prefix(&conn, "a0f1%").await.unwrap() {
            SegmentPrefixLookup::NotFound => {}
            other => panic!("expected NotFound for percent prefix, got {other:?}"),
        }

        // The literal (unescaped) prefix still resolves through the same path.
        match get_segment_by_prefix(&conn, "a0f1e2c3").await.unwrap() {
            SegmentPrefixLookup::Found(seg) => assert_eq!(seg.id, "a0f1e2c3d4b5f6a7"),
            other => panic!("expected Found for literal prefix, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ids_by_prefix_for_context_returns_bounded_ordered_candidates() {
        let (_db, conn) = setup().await;

        for id in ["0b25aaaa11112222", "0b25bbbb33334444", "0b25cccc55556666"] {
            upsert_segment(&conn, &test_segment(id, "src/x.rs", "hx"))
                .await
                .unwrap();
        }
        // A distinct-prefix row must not surface for the "0b25" floor prefix.
        upsert_segment(&conn, &test_segment("ffff000011112222", "src/y.rs", "hy"))
            .await
            .unwrap();

        // All three "0b25"-prefixed ids come back, ordered ascending by id.
        let ids =
            get_segment_ids_by_prefix_for_context(&conn, DEFAULT_INDEX_CONTEXT_ID, "0b25", 32)
                .await
                .unwrap();
        assert_eq!(
            ids,
            vec![
                "0b25aaaa11112222".to_string(),
                "0b25bbbb33334444".to_string(),
                "0b25cccc55556666".to_string(),
            ]
        );

        // The row limit caps the candidate set (saturation signal for the caller).
        let capped =
            get_segment_ids_by_prefix_for_context(&conn, DEFAULT_INDEX_CONTEXT_ID, "0b25", 2)
                .await
                .unwrap();
        assert_eq!(capped.len(), 2);

        // An empty prefix never matches every row.
        let empty = get_segment_ids_by_prefix_for_context(&conn, DEFAULT_INDEX_CONTEXT_ID, "", 32)
            .await
            .unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn escape_like_prefix_escapes_only_wildcards() {
        assert_eq!(escape_like_prefix("a0f1e2c3"), "a0f1e2c3");
        assert_eq!(escape_like_prefix("a_b"), "a\\_b");
        assert_eq!(escape_like_prefix("a%b"), "a\\%b");
        assert_eq!(escape_like_prefix("a\\b"), "a\\\\b");
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

        let seg = embedded_segment("seg1", "src/main.rs", "abc123", 0.5);
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

        let mut seg = embedded_segment("seg1", "src/main.rs", "abc123", 0.5);
        upsert_segment(&conn, &seg).await.unwrap();

        seg.content_key = None;
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
            let vector_json = serde_json::to_string(&embedding).unwrap();
            seg.content_key = Some(test_content_key(&vector_json));
            seg.embedding_vec = Some(vector_json);
            segments.push(seg);
        }

        batch_upsert_segments(&conn, &segments).await.unwrap();
        batch_upsert_vectors(&conn, &segments).await.unwrap();

        let stored = count_rows(&conn, "SELECT COUNT(*) FROM segment_vectors").await;
        assert_eq!(stored, 100, "one reference row per embedded segment");

        // The 100 orthogonal one-hot vectors are all distinct, so each maps to
        // its own pool row referenced exactly once.
        let pool_rows = count_rows(&conn, "SELECT COUNT(*) FROM embedding_pool").await;
        assert_eq!(pool_rows, 100, "distinct vectors store one pool row each");
        let total_refs = count_rows(
            &conn,
            "SELECT COALESCE(SUM(ref_count), 0) FROM embedding_pool",
        )
        .await;
        assert_eq!(
            total_refs, 100,
            "ref_count must equal the referencing-row count"
        );
    }

    async fn count_rows(conn: &Connection, sql: &str) -> i64 {
        let mut rows = conn.query(sql, ()).await.unwrap();
        let row = rows.next().await.unwrap().unwrap();
        row.get(0).unwrap()
    }

    /// Seed one `embedding_pool` row at a given starting `ref_count`.
    async fn seed_pool_row(conn: &Connection, content_key: &str, ref_count: i64) {
        let vector = serde_json::to_string(&vec![0.1f32; 384]).unwrap();
        conn.execute(
            "INSERT INTO embedding_pool (content_key, embedding_vec, ref_count) \
             VALUES (?1, vector8(?2), ?3)",
            libsql::params![content_key, vector, ref_count],
        )
        .await
        .unwrap();
    }

    /// Read every `(content_key, ref_count)` row, ordered for stable comparison.
    async fn all_pool_ref_counts(conn: &Connection) -> Vec<(String, i64)> {
        let mut rows = conn
            .query(
                "SELECT content_key, ref_count FROM embedding_pool ORDER BY content_key",
                (),
            )
            .await
            .unwrap();
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.unwrap() {
            out.push((row.get(0).unwrap(), row.get(1).unwrap()));
        }
        out
    }

    /// Byte-for-byte replica of the earlier per-key `UPDATE` loop, retained as
    /// the equivalence baseline the bulk statement must match (mirrors the
    /// established pattern of keeping the prior implementation in the test
    /// module).
    async fn loop_increment_pool_ref_counts_baseline(
        conn: &Connection,
        ref_segments: &[&SegmentInsert],
    ) {
        let mut counts: HashMap<&str, i64> = HashMap::new();
        for seg in ref_segments {
            let key = seg.content_key.as_deref().unwrap();
            *counts.entry(key).or_insert(0) += 1;
        }
        for (content_key, delta) in counts {
            conn.execute(
                queries::INCREMENT_EMBEDDING_POOL_REF_COUNT,
                libsql::params![content_key.to_string(), delta],
            )
            .await
            .unwrap();
        }
    }

    #[tokio::test]
    async fn batch_ref_count_bulk_statement_matches_per_key_loop() {
        // The single bulk `UPDATE` must leave `ref_count` rows
        // identical to the prior per-key loop for a multi-key batch with varied
        // per-key reference counts. Run both paths against separately-seeded
        // in-memory pools and assert the full row set matches.
        let mk = |id: &str, key: &str| -> SegmentInsert {
            let mut seg = test_segment(id, "src/x.rs", "h");
            seg.content_key = Some(key.to_string());
            seg
        };
        // keyA x3, keyB x1, keyC x2; keyE x2 is absent from the pool (exercises
        // the no-match path); keyD is seeded but never incremented.
        let owned = [
            mk("s1", "keyA"),
            mk("s2", "keyA"),
            mk("s3", "keyA"),
            mk("s4", "keyB"),
            mk("s5", "keyC"),
            mk("s6", "keyC"),
            mk("s7", "keyE"),
            mk("s8", "keyE"),
        ];
        let ref_segments: Vec<&SegmentInsert> = owned.iter().collect();

        // Non-zero starting counts prove this increments rather than sets.
        async fn seed(conn: &Connection) {
            seed_pool_row(conn, "keyA", 5).await;
            seed_pool_row(conn, "keyB", 0).await;
            seed_pool_row(conn, "keyC", 2).await;
            seed_pool_row(conn, "keyD", 10).await;
        }

        let (_db_bulk, conn_bulk) = setup().await;
        seed(&conn_bulk).await;
        batch_increment_pool_ref_counts(&conn_bulk, &ref_segments)
            .await
            .unwrap();

        let (_db_loop, conn_loop) = setup().await;
        seed(&conn_loop).await;
        loop_increment_pool_ref_counts_baseline(&conn_loop, &ref_segments).await;

        let bulk_rows = all_pool_ref_counts(&conn_bulk).await;
        let loop_rows = all_pool_ref_counts(&conn_loop).await;
        assert_eq!(
            bulk_rows, loop_rows,
            "bulk ref_count statement must yield identical rows to the per-key loop"
        );

        // Correctness guard so the test catches a wrong-delta or set-instead-of-add
        // regression, not merely two equal implementations of the same bug.
        assert_eq!(
            bulk_rows,
            vec![
                ("keyA".to_string(), 8),
                ("keyB".to_string(), 1),
                ("keyC".to_string(), 4),
                ("keyD".to_string(), 10),
            ],
            "each key gains exactly its reference count; an absent key inserts nothing"
        );
    }

    fn embedded_segment(id: &str, file_path: &str, file_hash: &str, fill: f32) -> SegmentInsert {
        let mut seg = test_segment(id, file_path, file_hash);
        let vector_json = serde_json::to_string(&vec![fill; 384]).unwrap();
        // Mirror the pipeline contract: an embeddable segment carries both a
        // content key and (for a pool miss) the vector. Keying on the vector
        // bytes means identical embeddings share a pool row, distinct ones do
        // not — the same dedup the production content key produces.
        seg.content_key = Some(test_content_key(&vector_json));
        seg.embedding_vec = Some(vector_json);
        seg
    }

    /// Deterministic stand-in for the pipeline's `embedding_content_key` in
    /// storage-layer tests: a short hash of the embedding bytes, so equal
    /// vectors collapse to one pool row and unequal vectors stay separate.
    fn test_content_key(vector_json: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(vector_json.as_bytes());
        let hash = hasher.finalize();
        hash.iter().map(|b| format!("{b:02x}")).collect::<String>()[..32].to_string()
    }

    async fn pool_ref_count(conn: &Connection, content_key: &str) -> Option<i64> {
        let mut rows = conn
            .query(
                "SELECT ref_count FROM embedding_pool WHERE content_key = ?1",
                [content_key],
            )
            .await
            .unwrap();
        rows.next().await.unwrap().map(|row| row.get(0).unwrap())
    }

    #[tokio::test]
    async fn pooled_write_shares_one_vector_across_contexts_with_matched_ref_count() {
        // Byte-identical content indexed in two contexts must
        // store its embedding exactly once and count both references. Identical
        // content => identical content_key, so the pool holds one row; the two
        // distinct segment_vectors rows (one per context) both reference it.
        let (_db, conn) = setup().await;
        let file_path = "src/shared.rs";
        let vector_json = serde_json::to_string(&vec![0.42f32; 384]).unwrap();
        let shared_key = test_content_key(&vector_json);

        let into_context = |context: &str| -> SegmentInsert {
            let id = generate_segment_id(context, file_path, 1, 3);
            let mut seg = test_segment(&id, file_path, "shared-hash");
            seg.content_key = Some(shared_key.clone());
            // Both contexts present the vector as a "miss" payload; the second
            // pool insert is a no-op (ON CONFLICT DO NOTHING), proving the bytes
            // are stored once regardless of how many contexts supply them.
            seg.embedding_vec = Some(vector_json.clone());
            seg
        };

        let ctx_a = into_context("ctx-a");
        let ctx_b = into_context("ctx-b");
        let a_segment_id = ctx_a.id.clone();
        let b_segment_id = ctx_b.id.clone();
        assert_ne!(a_segment_id, b_segment_id, "segment ids fold in context_id");

        replace_file_segments_for_context_tx(&conn, "ctx-a", file_path, &[ctx_a])
            .await
            .unwrap();
        replace_file_segments_for_context_tx(&conn, "ctx-b", file_path, &[ctx_b])
            .await
            .unwrap();

        assert_eq!(
            count_rows(&conn, "SELECT COUNT(*) FROM embedding_pool").await,
            1,
            "shared content must store exactly one pooled embedding"
        );
        assert_eq!(
            count_rows(&conn, "SELECT COUNT(*) FROM segment_vectors").await,
            2,
            "each context keeps its own reference into the shared pool row"
        );
        assert_eq!(
            pool_ref_count(&conn, &shared_key).await,
            Some(2),
            "ref_count equals the number of referencing segment_vectors rows"
        );

        // Re-indexing one context (delete-then-insert) must leave ref_count
        // unchanged: the trigger decrements on the segment delete, the write
        // re-increments. The invariant ref_count == referencing-row count holds.
        let mut ctx_a_again = test_segment(&a_segment_id, file_path, "shared-hash");
        ctx_a_again.content_key = Some(shared_key.clone());
        ctx_a_again.embedding_vec = Some(vector_json.clone());
        replace_file_segments_for_context_tx(&conn, "ctx-a", file_path, &[ctx_a_again])
            .await
            .unwrap();

        assert_eq!(
            pool_ref_count(&conn, &shared_key).await,
            Some(2),
            "re-indexing a context must not drift ref_count"
        );
        assert_eq!(
            count_rows(&conn, "SELECT COUNT(*) FROM embedding_pool").await,
            1,
            "re-indexing must not duplicate the shared pool row"
        );

        // Post-write invariant (mirrors the build-aside rebuild assertion): every
        // pool row's ref_count equals its live referencing-row count.
        let drifted = count_rows(
            &conn,
            "SELECT COUNT(*) FROM embedding_pool AS p \
             WHERE p.ref_count != (\
                SELECT COUNT(*) FROM segment_vectors AS sv WHERE sv.content_key = p.content_key\
             )",
        )
        .await;
        assert_eq!(
            drifted, 0,
            "no pool row's ref_count may drift from its references"
        );
    }

    #[tokio::test]
    async fn delete_context_refcounts_shared_pool_row_and_frees_last_referencer() {
        // A vector shared by two contexts is reference-counted
        // on deletion. Removing one context must leave the shared pool row with
        // ref_count == 1 (still resolvable by the survivor); removing the last
        // referencer must drop it to zero and physically delete the pool row.
        let (_db, conn) = setup().await;
        let file_path = "src/shared.rs";
        let vector_json = serde_json::to_string(&vec![0.17f32; 384]).unwrap();
        let shared_key = test_content_key(&vector_json);

        let into_context = |context: &str| -> SegmentInsert {
            let id = generate_segment_id(context, file_path, 1, 3);
            let mut seg = test_segment(&id, file_path, "shared-hash");
            seg.content_key = Some(shared_key.clone());
            seg.embedding_vec = Some(vector_json.clone());
            seg
        };

        let ctx_a = into_context("ctx-a");
        let ctx_b = into_context("ctx-b");
        let b_segment_id = ctx_b.id.clone();

        replace_file_segments_for_context_tx(&conn, "ctx-a", file_path, &[ctx_a])
            .await
            .unwrap();
        replace_file_segments_for_context_tx(&conn, "ctx-b", file_path, &[ctx_b])
            .await
            .unwrap();
        assert_eq!(
            pool_ref_count(&conn, &shared_key).await,
            Some(2),
            "both contexts reference the one shared pool row"
        );

        // Delete the first context: the shared vector must survive because the
        // second context still references it, with ref_count decremented to 1.
        delete_context(&conn, "ctx-a").await.unwrap();
        assert_eq!(
            count_rows(&conn, "SELECT COUNT(*) FROM embedding_pool").await,
            1,
            "a still-referenced shared vector must survive a context delete"
        );
        assert_eq!(
            pool_ref_count(&conn, &shared_key).await,
            Some(1),
            "the surviving context leaves ref_count == 1"
        );
        let mut rows = conn
            .query(
                "SELECT COUNT(*) FROM segment_vectors AS sv \
                 JOIN embedding_pool AS p ON p.content_key = sv.content_key \
                 WHERE sv.segment_id = ?1",
                [b_segment_id.as_str()],
            )
            .await
            .unwrap();
        let b_resolves: i64 = rows.next().await.unwrap().unwrap().get(0).unwrap();
        assert_eq!(
            b_resolves, 1,
            "the surviving context still resolves the shared embedding through the pool"
        );

        // Delete the last referencer: ref_count reaches zero, so the pool row is
        // physically removed by the delete-at-zero sweep this task adds.
        delete_context(&conn, "ctx-b").await.unwrap();
        assert_eq!(
            count_rows(&conn, "SELECT COUNT(*) FROM embedding_pool").await,
            0,
            "removing the last referencer frees the shared pool row"
        );
        assert_eq!(
            count_rows(&conn, "SELECT COUNT(*) FROM segment_vectors").await,
            0,
            "no dangling references remain once the last context is gone"
        );

        // Idempotency (AC3): re-deleting an already-pruned context is a safe no-op
        // and the sweep finds nothing left to remove.
        delete_context(&conn, "ctx-b").await.unwrap();
        assert_eq!(
            count_rows(&conn, "SELECT COUNT(*) FROM embedding_pool").await,
            0,
            "re-deleting a pruned context must remain a safe no-op"
        );
    }

    #[tokio::test]
    async fn vector_rows_survive_replace_reruns_and_conflicting_segment_rewrites() {
        // Defect A regression. `segments_vector_ad` fires AFTER DELETE on
        // segments, and REPLACE-style conflict resolution deletes conflicting
        // rows whenever `recursive_triggers` is ON, cascade-deleting freshly
        // written vector rows. The segment writes must therefore use
        // ON CONFLICT DO UPDATE, which keeps vectors intact under both
        // recursive-trigger modes. This replays the real CLI write path
        // (delete -> batched segment upsert -> batched vector upsert), then a
        // re-run, then a conflicting rewrite of the same segment rows.
        let (_db, conn) = setup().await;
        conn.execute("PRAGMA recursive_triggers = ON", ())
            .await
            .unwrap();

        let file_a = [
            embedded_segment("vec_a_1", "src/a.rs", "hash-a", 0.25),
            embedded_segment("vec_a_2", "src/a.rs", "hash-a", 0.5),
        ];
        let file_b = [embedded_segment("vec_b_1", "src/b.rs", "hash-b", 0.75)];
        let batches = [
            FileSegmentBatch {
                file_path: "src/a.rs",
                segments: &file_a,
                manifest_meta: None,
            },
            FileSegmentBatch {
                file_path: "src/b.rs",
                segments: &file_b,
                manifest_meta: None,
            },
        ];

        replace_file_batch_tx(&conn, &batches).await.unwrap();
        assert_eq!(
            count_rows(&conn, "SELECT COUNT(*) FROM segment_vectors").await,
            3,
            "fresh replace must store one vector row per embedded segment"
        );

        replace_file_batch_tx(&conn, &batches).await.unwrap();
        assert_eq!(
            count_rows(&conn, "SELECT COUNT(*) FROM segment_vectors").await,
            3,
            "a re-run over the same files must keep vector rows intact"
        );

        // A subsequent statement that rewrites the same segment rows without
        // a preceding delete (the conflict path) must not cascade-delete the
        // already-stored vectors.
        let rewrite = [
            test_segment("vec_a_1", "src/a.rs", "hash-a2"),
            test_segment("vec_a_2", "src/a.rs", "hash-a2"),
        ];
        batch_upsert_segments(&conn, &rewrite).await.unwrap();
        assert_eq!(
            count_rows(&conn, "SELECT COUNT(*) FROM segment_vectors").await,
            3,
            "conflicting segment rewrites must never cascade-delete vector rows"
        );

        let mut rows = conn
            .query(queries::SELECT_HAS_INDEXED_EMBEDDINGS, ())
            .await
            .unwrap();
        assert!(
            rows.next().await.unwrap().is_some(),
            "embeddings probe must still see vector rows"
        );
    }

    #[tokio::test]
    async fn single_segment_upsert_conflict_keeps_vector_row() {
        // Defect A regression for the single-row upsert statement: a repeat
        // upsert of an existing segment id (conflict path) with an embedding
        // must keep exactly one live vector row under recursive triggers.
        let (_db, conn) = setup().await;
        conn.execute("PRAGMA recursive_triggers = ON", ())
            .await
            .unwrap();

        let seg = embedded_segment("conflict_seg", "src/a.rs", "hash-1", 0.5);
        upsert_segment(&conn, &seg).await.unwrap();
        assert!(vector_exists(&conn, "conflict_seg").await);

        let updated = embedded_segment("conflict_seg", "src/a.rs", "hash-2", 0.75);
        upsert_segment(&conn, &updated).await.unwrap();
        assert!(
            vector_exists(&conn, "conflict_seg").await,
            "vector row must survive a conflicting single-segment upsert"
        );
    }

    #[tokio::test]
    async fn coverage_counters_report_vector_rows_against_embeddable_segments() {
        // Defect C support: status surfaces report stored-vector coverage,
        // so the counters must mirror the pipeline's embed decision
        // (structural segments and embeddable chunks count; excluded chunk
        // languages do not).
        let (_db, conn) = setup().await;
        let context = "ctx-coverage";

        let embedded = {
            let mut seg = embedded_segment("cov_fn", "src/a.rs", "h", 0.5);
            seg.block_type = "function".to_string();
            seg
        };
        let unembedded_function = {
            let mut seg = test_segment("cov_fn_plain", "src/a.rs", "h");
            seg.block_type = "function".to_string();
            seg
        };
        let json_chunk = {
            let mut seg = test_segment("cov_json", "config/data.json", "h");
            seg.block_type = "chunk".to_string();
            seg.language = "json".to_string();
            seg
        };

        replace_file_segments_for_context_tx(
            &conn,
            context,
            "src/a.rs",
            &[embedded, unembedded_function],
        )
        .await
        .unwrap();
        replace_file_segments_for_context_tx(&conn, context, "config/data.json", &[json_chunk])
            .await
            .unwrap();

        assert_eq!(
            count_vector_rows_for_context(&conn, context).await.unwrap(),
            1,
            "only the segment written with an embedding has a vector row"
        );
        assert_eq!(
            count_embeddable_segments_for_context(&conn, context)
                .await
                .unwrap(),
            2,
            "structural segments count as embeddable; excluded chunk languages do not"
        );
        assert_eq!(
            count_vector_rows_for_context(&conn, "ctx-other")
                .await
                .unwrap(),
            0,
            "coverage stays scoped to the requested context"
        );
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
        let main_old_vec = serde_json::to_string(&vec![0.5f32; 384]).unwrap();
        main_old.content_key = Some(test_content_key(&main_old_vec));
        main_old.embedding_vec = Some(main_old_vec);
        let mut linked_old = generated_test_segment(linked_context, file_path, "linked-old");
        linked_old.called_symbols = r#"["keep_linked_relation"]"#.to_string();
        let linked_old_vec = serde_json::to_string(&vec![0.25f32; 384]).unwrap();
        linked_old.content_key = Some(test_content_key(&linked_old_vec));
        linked_old.embedding_vec = Some(linked_old_vec);

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
        let main_new_vec = serde_json::to_string(&vec![0.75f32; 384]).unwrap();
        main_new.content_key = Some(test_content_key(&main_new_vec));
        main_new.embedding_vec = Some(main_new_vec);
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
        replacement_b.content_key = Some(test_content_key("not-a-vector"));
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
        bad_b.content_key = Some(test_content_key("not-a-vector"));
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

    fn overview_segment(
        id: &str,
        file_path: &str,
        language: &str,
        block_type: &str,
        role: &str,
        line_start: i64,
    ) -> SegmentInsert {
        SegmentInsert {
            id: id.to_string(),
            file_path: file_path.to_string(),
            language: language.to_string(),
            block_type: block_type.to_string(),
            content: format!("segment {id}"),
            line_start,
            line_end: line_start + 2,
            content_key: None,
            embedding_vec: None,
            breadcrumb: None,
            complexity: 1,
            role: role.to_string(),
            defined_symbols: "[]".to_string(),
            referenced_symbols: "[]".to_string(),
            referenced_relations: "[]".to_string(),
            called_symbols: "[]".to_string(),
            called_relations: "[]".to_string(),
            file_hash: format!("hash-{id}"),
        }
    }

    #[tokio::test]
    async fn language_stats_grouped_by_language() {
        let (_db, conn) = setup().await;

        for (id, path, language, line) in [
            ("lang_rs_a1", "src/a.rs", "rust", 1),
            ("lang_rs_a2", "src/a.rs", "rust", 10),
            ("lang_rs_b", "src/b.rs", "rust", 1),
            ("lang_go_1", "pkg/g.go", "go", 1),
            ("lang_go_2", "pkg/g.go", "go", 10),
            ("lang_go_3", "pkg/g.go", "go", 20),
            ("lang_ts_1", "web/c.ts", "typescript", 1),
            ("lang_ts_2", "web/c.ts", "typescript", 10),
        ] {
            upsert_segment_for_context(
                &conn,
                "ctx-a",
                &overview_segment(id, path, language, "function", "IMPLEMENTATION", line),
            )
            .await
            .unwrap();
        }
        upsert_segment_for_context(
            &conn,
            "ctx-b",
            &overview_segment(
                "lang_py",
                "tools/x.py",
                "python",
                "function",
                "IMPLEMENTATION",
                1,
            ),
        )
        .await
        .unwrap();

        let stats = get_language_stats_for_context(&conn, "ctx-a", 10)
            .await
            .unwrap();
        assert_eq!(
            stats,
            vec![
                LanguageStat {
                    language: "go".to_string(),
                    files: 1,
                    segments: 3,
                },
                LanguageStat {
                    language: "rust".to_string(),
                    files: 2,
                    segments: 3,
                },
                LanguageStat {
                    language: "typescript".to_string(),
                    files: 1,
                    segments: 2,
                },
            ]
        );

        let capped = get_language_stats_for_context(&conn, "ctx-a", 2)
            .await
            .unwrap();
        assert_eq!(capped.len(), 2);
        assert_eq!(capped[0].language, "go");
        assert!(get_language_stats_for_context(&conn, "ctx-a", 0)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn module_segment_counts_aggregate_by_depth() {
        let (_db, conn) = setup().await;

        for (id, path, line) in [
            ("mod_cli_1", "src/cli/a.rs", 1),
            ("mod_cli_2", "src/cli/a.rs", 10),
            ("mod_storage", "src/storage/b.rs", 1),
            ("mod_main", "src/main.rs", 1),
            ("mod_srcx", "srcx/z.rs", 1),
            ("mod_tests", "tests/t.rs", 1),
            ("mod_root", "README.md", 1),
        ] {
            upsert_segment_for_context(
                &conn,
                "ctx-a",
                &overview_segment(id, path, "rust", "function", "IMPLEMENTATION", line),
            )
            .await
            .unwrap();
        }
        upsert_segment_for_context(
            &conn,
            "ctx-b",
            &overview_segment(
                "mod_other",
                "src/cli/q.rs",
                "rust",
                "function",
                "IMPLEMENTATION",
                1,
            ),
        )
        .await
        .unwrap();

        let depth1 = get_module_segment_counts_for_context(&conn, "ctx-a", 10)
            .await
            .unwrap();
        assert_eq!(
            depth1,
            vec![
                ModuleSegmentCount {
                    module: "src".to_string(),
                    segments: 4,
                },
                ModuleSegmentCount {
                    module: "(root)".to_string(),
                    segments: 1,
                },
                ModuleSegmentCount {
                    module: "srcx".to_string(),
                    segments: 1,
                },
                ModuleSegmentCount {
                    module: "tests".to_string(),
                    segments: 1,
                },
            ]
        );

        let children = get_module_child_segment_counts_for_context(&conn, "ctx-a", "src", 10)
            .await
            .unwrap();
        assert_eq!(
            children,
            vec![
                ModuleSegmentCount {
                    module: "src/cli".to_string(),
                    segments: 2,
                },
                ModuleSegmentCount {
                    module: "src".to_string(),
                    segments: 1,
                },
                ModuleSegmentCount {
                    module: "src/storage".to_string(),
                    segments: 1,
                },
            ]
        );

        let capped = get_module_segment_counts_for_context(&conn, "ctx-a", 1)
            .await
            .unwrap();
        assert_eq!(capped.len(), 1);
        assert!(
            get_module_child_segment_counts_for_context(&conn, "ctx-a", "src", 0)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn entry_point_candidates_role_and_depth_ordered() {
        let (_db, conn) = setup().await;

        for (id, path, block_type, role, line) in [
            ("entry_main", "main.rs", "function", "ORCHESTRATION", 1),
            ("entry_lib_1", "src/lib.rs", "function", "ORCHESTRATION", 1),
            ("entry_lib_2", "src/lib.rs", "function", "ORCHESTRATION", 10),
            ("entry_tests", "tests/t.rs", "function", "ORCHESTRATION", 1),
            ("entry_def", "src/app.rs", "struct", "DEFINITION", 1),
            ("entry_impl", "src/deep.rs", "function", "IMPLEMENTATION", 1),
            ("entry_chunk", "notes.md", "chunk", "DEFINITION", 1),
        ] {
            upsert_segment_for_context(
                &conn,
                "ctx-a",
                &overview_segment(id, path, "rust", block_type, role, line),
            )
            .await
            .unwrap();
        }

        let candidates = get_entry_point_candidates_for_context(&conn, "ctx-a", 10)
            .await
            .unwrap();
        let ordered: Vec<(&str, &str, i64)> = candidates
            .iter()
            .map(|candidate| {
                (
                    candidate.file_path.as_str(),
                    candidate.role.as_str(),
                    candidate.line_start,
                )
            })
            .collect();
        assert_eq!(
            ordered,
            vec![
                ("main.rs", "ORCHESTRATION", 1),
                ("src/lib.rs", "ORCHESTRATION", 1),
                ("src/lib.rs", "ORCHESTRATION", 10),
                ("tests/t.rs", "ORCHESTRATION", 1),
                ("src/app.rs", "DEFINITION", 1),
            ]
        );
        assert_eq!(candidates[0].segment_id, "entry_main");
        assert_eq!(candidates[0].line_end, 3);
        assert_eq!(candidates[0].breadcrumb, None);

        let capped = get_entry_point_candidates_for_context(&conn, "ctx-a", 2)
            .await
            .unwrap();
        assert_eq!(capped.len(), 2);
    }

    #[tokio::test]
    async fn qualifying_type_definitions_resolve_requested_keys() {
        let (_db, conn) = setup().await;

        let mut def_db = overview_segment(
            "def_db",
            "src/storage/db.rs",
            "rust",
            "struct",
            "DEFINITION",
            5,
        );
        def_db.defined_symbols = r#"["Db"]"#.to_string();
        let mut def_db_test = overview_segment(
            "def_db_test",
            "tests/fixtures.rs",
            "rust",
            "struct",
            "DEFINITION",
            1,
        );
        def_db_test.defined_symbols = r#"["Db"]"#.to_string();
        let mut def_status_enum = overview_segment(
            "def_status_enum",
            "src/shared/types.rs",
            "rust",
            "enum",
            "DEFINITION",
            1,
        );
        def_status_enum.defined_symbols = r#"["BranchStatus"]"#.to_string();
        let mut def_status_fn = overview_segment(
            "def_status_fn",
            "src/daemon/registry.rs",
            "rust",
            "function",
            "DEFINITION",
            1,
        );
        def_status_fn.defined_symbols = r#"["branch_status"]"#.to_string();
        for seg in [&def_db, &def_db_test, &def_status_enum, &def_status_fn] {
            upsert_segment_for_context(&conn, "ctx-a", seg)
                .await
                .unwrap();
        }
        let mut def_db_other = overview_segment(
            "def_db_other",
            "src/other.rs",
            "rust",
            "struct",
            "DEFINITION",
            1,
        );
        def_db_other.defined_symbols = r#"["Db"]"#.to_string();
        upsert_segment_for_context(&conn, "ctx-b", &def_db_other)
            .await
            .unwrap();

        let keys = vec![
            "db".to_string(),
            "branchstatus".to_string(),
            "missing".to_string(),
        ];
        let definitions = get_qualifying_type_definitions_for_context(&conn, "ctx-a", &keys, 50)
            .await
            .unwrap();
        assert_eq!(
            definitions,
            vec![
                QualifyingTypeDefinition {
                    symbol_key: "branchstatus".to_string(),
                    symbol: "BranchStatus".to_string(),
                    segment_id: "def_status_enum".to_string(),
                    file_path: "src/shared/types.rs".to_string(),
                    line_start: 1,
                    line_end: 3,
                    block_type: "enum".to_string(),
                },
                QualifyingTypeDefinition {
                    symbol_key: "db".to_string(),
                    symbol: "Db".to_string(),
                    segment_id: "def_db".to_string(),
                    file_path: "src/storage/db.rs".to_string(),
                    line_start: 5,
                    line_end: 7,
                    block_type: "struct".to_string(),
                },
                QualifyingTypeDefinition {
                    symbol_key: "db".to_string(),
                    symbol: "Db".to_string(),
                    segment_id: "def_db_test".to_string(),
                    file_path: "tests/fixtures.rs".to_string(),
                    line_start: 1,
                    line_end: 3,
                    block_type: "struct".to_string(),
                },
            ]
        );

        assert!(
            get_qualifying_type_definitions_for_context(&conn, "ctx-a", &[], 50)
                .await
                .unwrap()
                .is_empty()
        );
        let capped = get_qualifying_type_definitions_for_context(&conn, "ctx-a", &keys, 2)
            .await
            .unwrap();
        assert_eq!(capped.len(), 2);
    }

    #[tokio::test]
    async fn freelist_reclaimable_bytes_is_zero_for_a_fresh_database() {
        let (_db, conn) = setup().await;

        assert_eq!(freelist_reclaimable_bytes(&conn).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn freelist_reclaimable_bytes_reflects_pages_freed_by_delete_without_vacuum() {
        // A file-backed connection, not in-memory: freelist accounting is a
        // property of the pager over the actual database file.
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().canonicalize().unwrap();
        let db_path = crate::shared::config::project_db_path(&project_root);
        let db = Db::open_rw(&db_path).await.unwrap();
        let conn = db.connect_tuned().await.unwrap();
        schema::initialize(&conn).await.unwrap();

        let segments: Vec<SegmentInsert> = (0..500)
            .map(|i| {
                test_segment(
                    &format!("seg-{i:04}"),
                    &format!("file-{i:04}.rs"),
                    &format!("hash-{i:04}"),
                )
            })
            .collect();
        batch_upsert_segments(&conn, &segments).await.unwrap();
        conn.execute("DELETE FROM segments", ()).await.unwrap();

        // auto_vacuum stays off (the project default), so pages freed by the
        // DELETE above sit on the freelist rather than shrinking the file.
        let bytes = freelist_reclaimable_bytes(&conn).await.unwrap();
        assert!(
            bytes > 0,
            "deleting 500 segments without VACUUM must leave freed pages on the freelist"
        );
    }

    #[tokio::test]
    async fn prunable_segments_proxy_counts_only_non_active_contexts_with_missing_source() {
        let (_db, conn) = setup().await;

        let missing_dir = std::env::temp_dir().join("oneup-gc-proxy-test-missing-source-dir");
        let _ = std::fs::remove_dir_all(&missing_dir);

        let active = test_worktree_context_row_with_source(
            "active000000",
            "/tmp/oneup-gc-proxy-test-active",
        );
        let gone =
            test_worktree_context_row_with_source("gone00000001", missing_dir.to_str().unwrap());
        let live = test_worktree_context_row_with_source("live0000001", env!("CARGO_MANIFEST_DIR"));

        for (ctx, seg_prefix) in [(&active, "a"), (&gone, "g"), (&live, "l")] {
            upsert_worktree_context(&conn, ctx, "proj-1").await.unwrap();
            let segment =
                generated_test_segment(&ctx.context_id, &format!("{seg_prefix}.rs"), "h1");
            batch_upsert_segments_for_context(&conn, &ctx.context_id, &[segment])
                .await
                .unwrap();
        }

        let prunable = prunable_segments_proxy(&conn, &active.context_id)
            .await
            .unwrap();

        // Only `gone` qualifies: its source_root does not exist on disk. `active`
        // is excluded regardless (it is the caller's own context), and `live`'s
        // source_root is the crate's own manifest directory, which exists.
        assert_eq!(prunable, 1);
    }

    fn disclosure_row(context_id: &str, state_root: &str, source_root: &str) -> IndexedContextRow {
        IndexedContextRow {
            context_id: context_id.to_string(),
            state_root: PathBuf::from(state_root),
            source_root: PathBuf::from(source_root),
            branch_name: None,
            updated_at: "2026-01-01 00:00:00".to_string(),
        }
    }

    #[test]
    fn is_stale_branch_snapshot_matches_same_roots_different_context() {
        let active = test_worktree_context_row_with_source("active000000", "/repo");
        // Same state_root + source_root as active but a different context_id: a
        // leftover per-branch snapshot.
        let stale = disclosure_row("oldbranch001", "/tmp/state", "/repo");
        assert!(is_stale_branch_snapshot(&active, &stale));
    }

    #[test]
    fn is_stale_branch_snapshot_excludes_active_and_other_worktrees() {
        let active = test_worktree_context_row_with_source("active000000", "/repo");
        // The active context itself is never a stale snapshot.
        let same_id = disclosure_row("active000000", "/tmp/state", "/repo");
        assert!(!is_stale_branch_snapshot(&active, &same_id));
        // A different worktree (different source_root) sharing the index is not a
        // snapshot of the active worktree.
        let other = disclosure_row("otherwt00001", "/tmp/state", "/repo-feature");
        assert!(!is_stale_branch_snapshot(&active, &other));
    }

    #[test]
    fn context_age_at_least_boundaries() {
        let now = DateTime::parse_from_rfc3339("2026-07-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert!(context_age_at_least(
            "2026-01-01 00:00:00",
            now,
            Duration::days(30)
        ));
        assert!(!context_age_at_least(
            "2026-06-20 00:00:00",
            now,
            Duration::days(30)
        ));
        // Unparseable input never counts as old enough.
        assert!(!context_age_at_least("not-a-date", now, Duration::days(0)));
    }

    fn test_worktree_context_row_with_source(
        context_id: &str,
        source_root: &str,
    ) -> WorktreeContext {
        WorktreeContext {
            context_id: context_id.to_string(),
            state_root: PathBuf::from("/tmp/state"),
            source_root: PathBuf::from(source_root),
            main_worktree_root: PathBuf::from("/tmp/state"),
            worktree_role: WorktreeRole::Main,
            git_dir: None,
            common_git_dir: None,
            branch_name: Some("main".to_string()),
            branch_ref: Some("refs/heads/main".to_string()),
            head_oid: None,
            branch_status: BranchStatus::Named,
        }
    }
}
