use std::fmt::Write as _;
use std::sync::LazyLock;

pub const CREATE_WORKTREE_CONTEXTS_TABLE: &str = "
CREATE TABLE IF NOT EXISTS worktree_contexts (
    context_id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    state_root TEXT NOT NULL,
    source_root TEXT NOT NULL,
    main_worktree_root TEXT NOT NULL,
    worktree_role TEXT NOT NULL,
    branch_name TEXT,
    branch_ref TEXT,
    branch_status TEXT NOT NULL,
    head_oid TEXT,
    git_dir TEXT,
    common_git_dir TEXT,
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
)";

pub const CREATE_SEGMENTS_TABLE: &str = "
CREATE TABLE IF NOT EXISTS segments (
    id TEXT PRIMARY KEY,
    context_id TEXT NOT NULL DEFAULT 'default',
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
)";

pub const CREATE_INDEX_FILE_PATH: &str =
    "CREATE INDEX IF NOT EXISTS idx_segments_file_path ON segments(file_path)";

pub const CREATE_INDEX_SEGMENTS_CONTEXT_FILE_PATH: &str =
    "CREATE INDEX IF NOT EXISTS idx_segments_context_file_path ON segments(context_id, file_path)";

pub const CREATE_INDEX_LANGUAGE: &str =
    "CREATE INDEX IF NOT EXISTS idx_segments_language ON segments(language)";

pub const CREATE_INDEX_FILE_HASH: &str =
    "CREATE INDEX IF NOT EXISTS idx_segments_file_hash ON segments(file_hash)";

/// Content-addressed embedding store. One row per distinct `(model_id,
/// embedding_dim, embed_input)` content key, holding the shared vector bytes and
/// the DiskANN index. `ref_count` tracks how many `segment_vectors` rows
/// reference the row across every context; a row is physically deleted only when
/// its last referencing segment is gone (centralized in `delete_context`).
pub const CREATE_EMBEDDING_POOL_TABLE: &str = "
CREATE TABLE IF NOT EXISTS embedding_pool (
    content_key TEXT PRIMARY KEY,
    embedding_vec FLOAT8(384) NOT NULL,
    ref_count INTEGER NOT NULL DEFAULT 0
)";

/// Per-segment reference into [`CREATE_EMBEDDING_POOL_TABLE`]. The vector bytes
/// no longer live inline here; `content_key` points at the shared
/// `embedding_pool` row so byte-identical content across contexts shares a
/// single stored embedding.
pub const CREATE_SEGMENT_VECTORS_TABLE: &str = "
CREATE TABLE IF NOT EXISTS segment_vectors (
    segment_id TEXT PRIMARY KEY,
    content_key TEXT NOT NULL
)";

pub const CREATE_SEGMENT_SYMBOLS_TABLE: &str = "
CREATE TABLE IF NOT EXISTS segment_symbols (
    context_id TEXT NOT NULL DEFAULT 'default',
    segment_id TEXT NOT NULL,
    symbol TEXT NOT NULL,
    canonical_symbol TEXT NOT NULL,
    reference_kind TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (context_id, segment_id, canonical_symbol, reference_kind)
)";

pub const CREATE_SEGMENT_RELATIONS_TABLE: &str = "
CREATE TABLE IF NOT EXISTS segment_relations (
    context_id TEXT NOT NULL DEFAULT 'default',
    source_segment_id TEXT NOT NULL,
    relation_kind TEXT NOT NULL,
    raw_target_symbol TEXT NOT NULL,
    canonical_target_symbol TEXT NOT NULL,
    lookup_canonical_symbol TEXT NOT NULL,
    qualifier_fingerprint TEXT NOT NULL,
    edge_identity_kind TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (
        context_id,
        source_segment_id,
        relation_kind,
        canonical_target_symbol,
        raw_target_symbol,
        edge_identity_kind
    )
)";

pub const CREATE_INDEX_EMBEDDING_POOL_EMBEDDING: &str =
    "/* KEEP: max_neighbors=32 caps DiskANN fanout for REQ-001 (<=80 MiB); default (~62 for 384d) pushes the node block to a larger page tier (~95 MiB) with no measurable recall gain on the hand-curated corpus. */ CREATE INDEX IF NOT EXISTS idx_embedding_pool_embedding ON embedding_pool (libsql_vector_idx(embedding_vec, 'metric=cosine', 'compress_neighbors=float8', 'max_neighbors=32'))";

pub const CREATE_INDEX_SEGMENT_SYMBOLS_EXACT: &str =
    "CREATE INDEX IF NOT EXISTS idx_segment_symbols_exact ON segment_symbols(context_id, canonical_symbol, reference_kind)";

pub const CREATE_INDEX_SEGMENT_SYMBOLS_PREFIX: &str =
    "CREATE INDEX IF NOT EXISTS idx_segment_symbols_prefix ON segment_symbols(context_id, canonical_symbol)";

pub const CREATE_INDEX_SEGMENT_RELATIONS_SOURCE: &str =
    "CREATE INDEX IF NOT EXISTS idx_segment_relations_source ON segment_relations(context_id, source_segment_id)";

pub const CREATE_INDEX_SEGMENT_RELATIONS_TARGET: &str =
    "CREATE INDEX IF NOT EXISTS idx_segment_relations_target ON segment_relations(context_id, canonical_target_symbol, relation_kind)";

pub const CREATE_INDEX_SEGMENT_RELATIONS_LOOKUP_TARGET: &str =
    "CREATE INDEX IF NOT EXISTS idx_segment_relations_lookup_target ON segment_relations(context_id, lookup_canonical_symbol, relation_kind)";

pub const CREATE_INDEXED_FILES_TABLE: &str = "
CREATE TABLE IF NOT EXISTS indexed_files (
    context_id TEXT NOT NULL DEFAULT 'default',
    file_path TEXT NOT NULL,
    extension TEXT NOT NULL,
    file_hash TEXT NOT NULL,
    file_size INTEGER NOT NULL,
    modified_ns INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (context_id, file_path)
)";

pub const CREATE_FTS_TABLE: &str = "
CREATE VIRTUAL TABLE IF NOT EXISTS segments_fts USING fts5(
    content,
    content='segments',
    content_rowid='rowid'
)";

/// FTS sync triggers plus `segments_vector_ad`, the single decrement point for
/// `embedding_pool.ref_count`. Because `recursive_triggers` defaults OFF, this
/// trigger's own `DELETE FROM segment_vectors` fires no further trigger, so the
/// decrement is done here (before the row is deleted, while its `content_key` is
/// still readable) rather than via a `segment_vectors` AFTER DELETE trigger.
/// Every segment removal — file re-index replace and whole-context delete alike
/// — flows through here, keeping `ref_count` equal to the live referencing-row
/// count without any application bookkeeping. The trigger only decrements; it
/// never deletes a pool row, so a key dropping to zero references survives as a
/// harmless orphan until the reference-aware `delete_context` sweep removes it.
pub const CREATE_FTS_TRIGGERS: &str = "
CREATE TRIGGER IF NOT EXISTS segments_ai AFTER INSERT ON segments BEGIN
    INSERT INTO segments_fts(rowid, content) VALUES (new.rowid, new.content);
END;
CREATE TRIGGER IF NOT EXISTS segments_ad AFTER DELETE ON segments BEGIN
    INSERT INTO segments_fts(segments_fts, rowid, content) VALUES ('delete', old.rowid, old.content);
END;
CREATE TRIGGER IF NOT EXISTS segments_au AFTER UPDATE ON segments BEGIN
    INSERT INTO segments_fts(segments_fts, rowid, content) VALUES ('delete', old.rowid, old.content);
    INSERT INTO segments_fts(rowid, content) VALUES (new.rowid, new.content);
END;
CREATE TRIGGER IF NOT EXISTS segments_vector_ad AFTER DELETE ON segments BEGIN
    UPDATE embedding_pool
       SET ref_count = ref_count - 1
     WHERE content_key IN (
        SELECT content_key FROM segment_vectors WHERE segment_id = old.id
     );
    DELETE FROM segment_vectors WHERE segment_id = old.id;
END";

pub const CREATE_SEGMENT_SYMBOLS_TRIGGER: &str = "
CREATE TRIGGER IF NOT EXISTS segments_symbol_ad AFTER DELETE ON segments BEGIN
    DELETE FROM segment_symbols WHERE segment_id = old.id;
END";

pub const CREATE_META_TABLE: &str = "
CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
)";

// Retained as a test-only schema-reset utility (e.g. exercising
// `ensure_current`'s rejection of a deliberately broken schema). The
// non-destructive rebuild builds a fresh staging index and atomically switches it
// over the served index, so production code never drops the live search schema.
#[cfg(test)]
pub const DROP_SEARCH_SCHEMA: &str = "
DROP TRIGGER IF EXISTS segments_ai;
DROP TRIGGER IF EXISTS segments_ad;
DROP TRIGGER IF EXISTS segments_au;
DROP TRIGGER IF EXISTS segments_vector_ad;
DROP TRIGGER IF EXISTS segments_symbol_ad;
DROP TABLE IF EXISTS segments_fts;
DROP INDEX IF EXISTS idx_embedding_pool_embedding;
DROP INDEX IF EXISTS idx_segment_symbols_exact;
DROP INDEX IF EXISTS idx_segment_symbols_prefix;
DROP INDEX IF EXISTS idx_segment_relations_source;
DROP INDEX IF EXISTS idx_segment_relations_target;
DROP INDEX IF EXISTS idx_segment_relations_lookup_target;
DROP INDEX IF EXISTS idx_indexed_files_context_path;
DROP TABLE IF EXISTS segment_vectors;
DROP TABLE IF EXISTS embedding_pool;
DROP TABLE IF EXISTS segment_symbols;
DROP TABLE IF EXISTS segment_relations;
DROP INDEX IF EXISTS idx_segments_context_file_path;
DROP INDEX IF EXISTS idx_segments_file_path;
DROP INDEX IF EXISTS idx_segments_language;
DROP INDEX IF EXISTS idx_segments_file_hash;
DROP TABLE IF EXISTS segments;
DROP TABLE IF EXISTS indexed_files;
DROP TABLE IF EXISTS worktree_contexts;
DROP TABLE IF EXISTS meta";

/// Fold every write-ahead-log frame into the main database file and truncate the
/// WAL to zero bytes. Returns one row `(busy, log, checkpointed)`: a non-zero
/// `busy` means a concurrent reader/writer blocked the checkpoint and the WAL was
/// *not* truncated, so the database is not yet self-contained. Used by
/// [`crate::storage::swap::finalize_staged_db`] to turn a freshly-built staging
/// database into a single self-contained file before it is renamed over the
/// served index.
#[allow(dead_code)]
pub const WAL_CHECKPOINT_TRUNCATE: &str = "PRAGMA wal_checkpoint(TRUNCATE)";

/// Fold as many write-ahead-log frames as possible into the main database file
/// *without waiting on concurrent readers* (unlike `TRUNCATE`, which blocks until
/// every reader releases the WAL). Returns one row `(busy, log, checkpointed)`; a
/// non-zero `busy` simply means a reader held part of the WAL and is not an error.
/// Used by [`crate::storage::swap`] to retire the prior index's WAL via the
/// open-then-immediately-close idiom before the atomic switch-over, where a live
/// CLI/MCP reader may hold the index and a blocking checkpoint would stall or fail
/// the swap.
#[allow(dead_code)]
pub const WAL_CHECKPOINT_PASSIVE: &str = "PRAGMA wal_checkpoint(PASSIVE)";

pub const SELECT_SCHEMA_OBJECT: &str =
    "SELECT 1 FROM sqlite_master WHERE type = ?1 AND name = ?2 LIMIT 1";

pub const SELECT_HAS_USER_TABLES: &str =
    "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' LIMIT 1";

pub const SELECT_HAS_INDEXED_EMBEDDINGS: &str = "SELECT 1 FROM segment_vectors LIMIT 1";

pub const SELECT_HAS_INDEXED_EMBEDDINGS_FOR_CONTEXT: &str = "
SELECT 1
FROM segment_vectors AS sv
JOIN segments AS s ON s.id = sv.segment_id
WHERE s.context_id = ?1
LIMIT 1";

pub const COUNT_VECTOR_CONTEXTS: &str = "
SELECT COUNT(DISTINCT s.context_id)
FROM segment_vectors AS sv
JOIN segments AS s ON s.id = sv.segment_id";

/// Context-agnostic sibling of [`SELECT_VECTOR_CANDIDATES_FOR_CONTEXT`] (no
/// `context_id` filter). Kept in lockstep with the pooled schema so it joins the
/// relocated pool index and fans out through `content_key`; the `s.id` secondary
/// sort keeps fan-out ties deterministic.
#[allow(dead_code)]
pub const SELECT_VECTOR_CANDIDATES: &str = "
WITH vector_matches AS (
    SELECT row_number() OVER () AS rank, id
    FROM vector_top_k('idx_embedding_pool_embedding', vector8(?1), ?2)
)
SELECT s.id, s.file_path, s.language, s.block_type,
       s.line_start, s.line_end, s.breadcrumb, s.complexity,
       s.role, s.defined_symbols, s.referenced_symbols, s.called_symbols, s.content
FROM vector_matches AS v
JOIN embedding_pool AS p ON p.rowid = v.id
JOIN segment_vectors AS sv ON sv.content_key = p.content_key
JOIN segments AS s ON s.id = sv.segment_id
ORDER BY v.rank, s.id";

/// Approximate (ANN) vector candidates for a context, used only above
/// `VECTOR_EXHAUSTIVE_SCAN_MAX_VECTORS`. `vector_top_k` runs over the relocated
/// pool index and returns one rowid per distinct pool vector (`?2` = over-fetch
/// budget, scaled by indexed-context count at the call site so enough top
/// vectors survive the per-context filter). Each pool row then fans out across
/// every `segment_vectors` reference sharing its `content_key`; the result is
/// filtered to the context and truncated to `?4` (the base candidate budget K).
/// The secondary sort `s.id` is load-bearing: one pool row now maps to multiple
/// segments, so `v.rank` alone leaves ties unordered — `ORDER BY v.rank, s.id`
/// makes the truncation deterministic.
pub const SELECT_VECTOR_CANDIDATES_FOR_CONTEXT: &str = "
WITH vector_matches AS (
    SELECT row_number() OVER () AS rank, id
    FROM vector_top_k('idx_embedding_pool_embedding', vector8(?1), ?2)
)
SELECT s.id, s.file_path, s.language, s.block_type,
       s.line_start, s.line_end, s.breadcrumb, s.complexity,
       s.role, s.defined_symbols, s.referenced_symbols, s.called_symbols, s.content
FROM vector_matches AS v
JOIN embedding_pool AS p ON p.rowid = v.id
JOIN segment_vectors AS sv ON sv.content_key = p.content_key
JOIN segments AS s ON s.id = sv.segment_id
WHERE s.context_id = ?3
ORDER BY v.rank, s.id
LIMIT ?4";

/* KEEP: the exhaustive path must not touch idx_embedding_pool_embedding.
Below VECTOR_EXHAUSTIVE_SCAN_MAX_VECTORS a full ordered scan over the
context's vectors is both exact and orders of magnitude faster than beam
traversal over the disk-based approximate index, which the profiling for the
small-corpus latency fix showed spending seconds in read-heavy graph walks.
The `segment_vectors -> embedding_pool` join is 1:1 (each reference names exactly
one pool row), so the candidate row set, the `vector_distance_cos` values, and the
`ORDER BY ..., s.id` ordering are byte-for-byte identical to the pre-pooling inline
column (HYP-001 CONFIRMED) — the vector bytes simply moved into the shared pool. */
pub const SELECT_VECTOR_CANDIDATES_EXHAUSTIVE_FOR_CONTEXT: &str = "
SELECT s.id, s.file_path, s.language, s.block_type,
       s.line_start, s.line_end, s.breadcrumb, s.complexity,
       s.role, s.defined_symbols, s.referenced_symbols, s.called_symbols, s.content
FROM segment_vectors AS sv
JOIN embedding_pool AS p ON p.content_key = sv.content_key
JOIN segments AS s ON s.id = sv.segment_id
WHERE s.context_id = ?2
ORDER BY vector_distance_cos(p.embedding_vec, vector8(?1)), s.id
LIMIT ?3";

#[allow(dead_code)]
pub const SELECT_FTS_CANDIDATES: &str = "
SELECT s.id, s.file_path, s.language, s.block_type,
       s.line_start, s.line_end, s.breadcrumb, s.complexity,
       s.role, s.defined_symbols, s.referenced_symbols, s.called_symbols, s.content
FROM segments_fts AS f
JOIN segments AS s ON s.rowid = f.rowid
WHERE segments_fts MATCH ?1
ORDER BY f.rank, s.rowid
LIMIT ?2";

pub const SELECT_FTS_CANDIDATES_FOR_CONTEXT: &str = "
SELECT s.id, s.file_path, s.language, s.block_type,
       s.line_start, s.line_end, s.breadcrumb, s.complexity,
       s.role, s.defined_symbols, s.referenced_symbols, s.called_symbols, s.content
FROM segments_fts AS f
JOIN segments AS s ON s.rowid = f.rowid
WHERE segments_fts MATCH ?1
  AND s.context_id = ?2
ORDER BY f.rank, s.rowid
LIMIT ?3";

/* KEEP: segments writes must stay ON CONFLICT DO UPDATE, never INSERT OR
REPLACE. REPLACE resolves conflicts by deleting the existing row, which fires
`segments_vector_ad` under `PRAGMA recursive_triggers=ON` (cascade-deleting
the segment's vector row) and skips the FTS delete trigger under the default
OFF (leaving stale external-content FTS entries). DO UPDATE keeps the rowid
stable and routes through `segments_au`, which is correct in both modes. */
pub const UPSERT_SEGMENT: &str = "
INSERT INTO segments (
    id, context_id, file_path, language, block_type, content,
    line_start, line_end, breadcrumb, complexity, role, defined_symbols, referenced_symbols, called_symbols,
    file_hash, created_at, updated_at
) VALUES (
    ?1, ?2, ?3, ?4, ?5, ?6,
    ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
    ?15, datetime('now'), datetime('now')
)
ON CONFLICT(id) DO UPDATE SET
    context_id = excluded.context_id,
    file_path = excluded.file_path,
    language = excluded.language,
    block_type = excluded.block_type,
    content = excluded.content,
    line_start = excluded.line_start,
    line_end = excluded.line_end,
    breadcrumb = excluded.breadcrumb,
    complexity = excluded.complexity,
    role = excluded.role,
    defined_symbols = excluded.defined_symbols,
    referenced_symbols = excluded.referenced_symbols,
    called_symbols = excluded.called_symbols,
    file_hash = excluded.file_hash,
    updated_at = datetime('now')";

/// Prefix for a batched existence check against `embedding_pool`. The caller
/// appends a comma-separated `?n` placeholder list (one per content key) and a
/// closing `)`. The lookup-before-embed pipeline uses this to find which keys
/// are already stored so it can embed only the misses (REQ-002).
pub const SELECT_EMBEDDING_POOL_KEYS_PREFIX: &str =
    "SELECT content_key FROM embedding_pool WHERE content_key IN (";

/// Idempotent insert of a shared pool vector. The vector bytes for a given
/// `content_key` are a deterministic function of (model, content), so a key
/// already present is left untouched (`DO NOTHING`) rather than rewritten —
/// avoiding needless churn in the DiskANN index shadow tables. Concurrency-safe
/// (REQ-001): two writers inserting the same new content collapse to one row.
/// `ref_count` is reconciled separately (incremented on the `segment_vectors`
/// write, decremented by the `segments_vector_ad` AFTER DELETE trigger).
pub const UPSERT_EMBEDDING_POOL: &str = "
INSERT INTO embedding_pool (content_key, embedding_vec, ref_count)
VALUES (?1, vector8(?2), 0)
ON CONFLICT(content_key) DO NOTHING";

/// Add `?2` references to a pool row. Called once per distinct `content_key`
/// written into `segment_vectors`, with `?2` set to the number of new
/// referencing rows so `ref_count` stays equal to the referencing-row count.
pub const INCREMENT_EMBEDDING_POOL_REF_COUNT: &str =
    "UPDATE embedding_pool SET ref_count = ref_count + ?2 WHERE content_key = ?1";

/// Bulk form of [`INCREMENT_EMBEDDING_POOL_REF_COUNT`] (R-011): apply every
/// distinct key's increment from one write batch in a single statement instead
/// of one `UPDATE` per key. `?1` is a JSON object mapping each `content_key` to
/// the number of new referencing rows (e.g. `{"<key>": 2}`); `json_each` expands
/// it to `(key, value)` rows and the `UPDATE ... FROM` join adds each delta to
/// the matching pool row. Counts are identical to the per-key loop: the keys are
/// distinct so each matches exactly one (primary-key) pool row, keys absent from
/// `embedding_pool` match nothing and are silently skipped (as the per-key
/// `WHERE content_key = ?1` did), and `embedding_pool` carries no UPDATE trigger
/// so the bulk write fires nothing the loop did not.
pub const BATCH_INCREMENT_EMBEDDING_POOL_REF_COUNTS: &str = "
UPDATE embedding_pool
   SET ref_count = ref_count + CAST(j.value AS INTEGER)
  FROM json_each(?1) AS j
 WHERE embedding_pool.content_key = j.key";

/// Decrement one reference from the pool row a single segment referenced. Used
/// by the test-only single-segment write path when it removes a segment's
/// vector directly (the batch/replace path decrements via the
/// `segments_vector_ad` AFTER DELETE trigger instead).
pub const DECREMENT_EMBEDDING_POOL_REF_COUNT_FOR_SEGMENT: &str = "
UPDATE embedding_pool
   SET ref_count = ref_count - 1
 WHERE content_key = (SELECT content_key FROM segment_vectors WHERE segment_id = ?1)";

/// Reference-aware garbage collection of the shared pool (REQ-004). The
/// `segments_vector_ad` trigger decrements `ref_count` as segments are deleted
/// but never removes a pool row, so a vector whose last referencer is gone
/// lingers as a zero-ref orphan. `delete_context` runs this sweep after its
/// context-scoped segment deletes to drop exactly those orphans, freeing the
/// shared bytes and the DiskANN index entry. The `<= 0` floor is defensive: a
/// correctly maintained count never goes negative, but any row at or below zero
/// is unreferenced and safe to delete. Pool rows still referenced by another
/// context keep `ref_count >= 1`, so this never touches a live vector — the
/// per-context isolation guarantee (REQ-005).
pub const DELETE_ORPHANED_EMBEDDING_POOL_ROWS: &str =
    "DELETE FROM embedding_pool WHERE ref_count <= 0";

pub const UPSERT_SEGMENT_VECTOR: &str = "
INSERT INTO segment_vectors (
    segment_id, content_key
) VALUES (
    ?1, ?2
)
ON CONFLICT(segment_id) DO UPDATE SET
    content_key = excluded.content_key";

pub const DELETE_SEGMENT_VECTOR: &str = "DELETE FROM segment_vectors WHERE segment_id = ?1";

pub const INSERT_SEGMENT_SYMBOL: &str = "
INSERT OR REPLACE INTO segment_symbols (
    context_id, segment_id, symbol, canonical_symbol, reference_kind, created_at
) VALUES (
    ?1, ?2, ?3, ?4, ?5, datetime('now')
)";

#[allow(dead_code)]
pub const DELETE_SEGMENT_SYMBOLS_BY_SEGMENT_ID: &str =
    "DELETE FROM segment_symbols WHERE segment_id = ?1";

pub const DELETE_SEGMENT_SYMBOLS_BY_CONTEXT_AND_SEGMENT_ID: &str =
    "DELETE FROM segment_symbols WHERE context_id = ?1 AND segment_id = ?2";

#[allow(dead_code)]
pub const INSERT_SEGMENT_RELATION: &str = "
INSERT OR REPLACE INTO segment_relations (
    context_id,
    source_segment_id,
    relation_kind,
    raw_target_symbol,
    canonical_target_symbol,
    lookup_canonical_symbol,
    qualifier_fingerprint,
    edge_identity_kind,
    created_at
) VALUES (
    ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, datetime('now')
)";

#[allow(dead_code)]
pub const DELETE_SEGMENT_RELATIONS_BY_SOURCE_SEGMENT_ID: &str =
    "DELETE FROM segment_relations WHERE source_segment_id = ?1";

pub const DELETE_SEGMENT_RELATIONS_BY_CONTEXT_AND_SOURCE_SEGMENT_ID: &str =
    "DELETE FROM segment_relations WHERE context_id = ?1 AND source_segment_id = ?2";

#[allow(dead_code)]
pub const DELETE_SEGMENT_RELATIONS_BY_FILE: &str = "
DELETE FROM segment_relations
WHERE source_segment_id IN (
    SELECT id
    FROM segments
    WHERE file_path = ?1
)";

pub const DELETE_SEGMENT_RELATIONS_BY_CONTEXT_AND_FILE: &str = "
DELETE FROM segment_relations
WHERE context_id = ?1
  AND source_segment_id IN (
    SELECT id
    FROM segments
    WHERE context_id = ?1
      AND file_path = ?2
)";

#[allow(dead_code)]
pub const SELECT_OUTBOUND_RELATIONS: &str = "
SELECT
    source_segment_id,
    relation_kind,
    raw_target_symbol,
    canonical_target_symbol,
    lookup_canonical_symbol,
    qualifier_fingerprint,
    edge_identity_kind
FROM segment_relations
WHERE source_segment_id = ?1
ORDER BY
  CASE WHEN relation_kind = 'call' THEN 0 ELSE 1 END,
  canonical_target_symbol,
  edge_identity_kind,
  raw_target_symbol
LIMIT ?2";

pub const SELECT_OUTBOUND_RELATIONS_FOR_CONTEXT: &str = "
SELECT
    source_segment_id,
    relation_kind,
    raw_target_symbol,
    canonical_target_symbol,
    lookup_canonical_symbol,
    qualifier_fingerprint,
    edge_identity_kind
FROM segment_relations
WHERE context_id = ?1
  AND source_segment_id = ?2
  AND edge_identity_kind != 'doc_mention'
ORDER BY
  CASE WHEN relation_kind = 'call' THEN 0 ELSE 1 END,
  canonical_target_symbol,
  edge_identity_kind,
  raw_target_symbol
LIMIT ?3";

#[allow(dead_code)]
pub const SELECT_OUTBOUND_RELATIONS_BY_KIND: &str = "
SELECT
    source_segment_id,
    relation_kind,
    raw_target_symbol,
    canonical_target_symbol,
    lookup_canonical_symbol,
    qualifier_fingerprint,
    edge_identity_kind
FROM segment_relations
WHERE source_segment_id = ?1
  AND relation_kind = ?2
ORDER BY canonical_target_symbol, edge_identity_kind, raw_target_symbol
LIMIT ?3";

pub const SELECT_OUTBOUND_RELATIONS_BY_KIND_FOR_CONTEXT: &str = "
SELECT
    source_segment_id,
    relation_kind,
    raw_target_symbol,
    canonical_target_symbol,
    lookup_canonical_symbol,
    qualifier_fingerprint,
    edge_identity_kind
FROM segment_relations
WHERE context_id = ?1
  AND source_segment_id = ?2
  AND relation_kind = ?3
  AND edge_identity_kind != 'doc_mention'
ORDER BY canonical_target_symbol, edge_identity_kind, raw_target_symbol
LIMIT ?4";

#[allow(dead_code)]
pub const SELECT_INBOUND_RELATIONS: &str = "
SELECT
    source_segment_id,
    relation_kind,
    raw_target_symbol,
    canonical_target_symbol,
    lookup_canonical_symbol,
    qualifier_fingerprint,
    edge_identity_kind
FROM segment_relations
WHERE canonical_target_symbol = ?1
ORDER BY
  CASE WHEN relation_kind = 'call' THEN 0 ELSE 1 END,
  source_segment_id,
  edge_identity_kind,
  raw_target_symbol
LIMIT ?2";

pub const SELECT_INBOUND_RELATIONS_FOR_CONTEXT: &str = "
SELECT
    source_segment_id,
    relation_kind,
    raw_target_symbol,
    canonical_target_symbol,
    lookup_canonical_symbol,
    qualifier_fingerprint,
    edge_identity_kind
FROM segment_relations
WHERE context_id = ?1
  AND canonical_target_symbol = ?2
ORDER BY
  CASE WHEN relation_kind = 'call' THEN 0 ELSE 1 END,
  source_segment_id,
  edge_identity_kind,
  raw_target_symbol
LIMIT ?3";

#[allow(dead_code)]
pub const SELECT_INBOUND_RELATIONS_BY_KIND: &str = "
SELECT
    source_segment_id,
    relation_kind,
    raw_target_symbol,
    canonical_target_symbol,
    lookup_canonical_symbol,
    qualifier_fingerprint,
    edge_identity_kind
FROM segment_relations
WHERE canonical_target_symbol = ?1
  AND relation_kind = ?2
ORDER BY source_segment_id, edge_identity_kind, raw_target_symbol
LIMIT ?3";

pub const SELECT_INBOUND_RELATIONS_BY_KIND_FOR_CONTEXT: &str = "
SELECT
    source_segment_id,
    relation_kind,
    raw_target_symbol,
    canonical_target_symbol,
    lookup_canonical_symbol,
    qualifier_fingerprint,
    edge_identity_kind
FROM segment_relations
WHERE context_id = ?1
  AND canonical_target_symbol = ?2
  AND relation_kind = ?3
ORDER BY source_segment_id, edge_identity_kind, raw_target_symbol
LIMIT ?4";

#[allow(dead_code)]
pub const SELECT_INBOUND_RELATIONS_BY_LOOKUP_SYMBOL: &str = "
SELECT
    source_segment_id,
    relation_kind,
    raw_target_symbol,
    canonical_target_symbol,
    lookup_canonical_symbol,
    qualifier_fingerprint,
    edge_identity_kind
FROM segment_relations
WHERE lookup_canonical_symbol = ?1
ORDER BY
  CASE WHEN relation_kind = 'call' THEN 0 ELSE 1 END,
  source_segment_id,
  edge_identity_kind,
  raw_target_symbol
LIMIT ?2";

pub const SELECT_INBOUND_RELATIONS_BY_LOOKUP_SYMBOL_FOR_CONTEXT: &str = "
SELECT
    source_segment_id,
    relation_kind,
    raw_target_symbol,
    canonical_target_symbol,
    lookup_canonical_symbol,
    qualifier_fingerprint,
    edge_identity_kind
FROM segment_relations
WHERE context_id = ?1
  AND lookup_canonical_symbol = ?2
  AND edge_identity_kind != 'doc_mention'
ORDER BY
  CASE WHEN relation_kind = 'call' THEN 0 ELSE 1 END,
  source_segment_id,
  edge_identity_kind,
  raw_target_symbol
LIMIT ?3";

#[allow(dead_code)]
pub const SELECT_INBOUND_RELATIONS_BY_LOOKUP_SYMBOL_AND_KIND: &str = "
SELECT
    source_segment_id,
    relation_kind,
    raw_target_symbol,
    canonical_target_symbol,
    lookup_canonical_symbol,
    qualifier_fingerprint,
    edge_identity_kind
FROM segment_relations
WHERE lookup_canonical_symbol = ?1
  AND relation_kind = ?2
ORDER BY source_segment_id, edge_identity_kind, raw_target_symbol
LIMIT ?3";

pub const SELECT_INBOUND_RELATIONS_BY_LOOKUP_SYMBOL_AND_KIND_FOR_CONTEXT: &str = "
SELECT
    source_segment_id,
    relation_kind,
    raw_target_symbol,
    canonical_target_symbol,
    lookup_canonical_symbol,
    qualifier_fingerprint,
    edge_identity_kind
FROM segment_relations
WHERE context_id = ?1
  AND lookup_canonical_symbol = ?2
  AND relation_kind = ?3
  AND edge_identity_kind != 'doc_mention'
ORDER BY source_segment_id, edge_identity_kind, raw_target_symbol
LIMIT ?4";

#[allow(dead_code)]
pub const SELECT_SEGMENTS_BY_FILE: &str = "
SELECT id, file_path, language, block_type, content,
       line_start, line_end, breadcrumb, complexity, role,
       defined_symbols, referenced_symbols, called_symbols, file_hash,
       created_at, updated_at
FROM segments
WHERE file_path = ?1
ORDER BY line_start";

pub const SELECT_SEGMENTS_BY_FILE_FOR_CONTEXT: &str = "
SELECT id, file_path, language, block_type, content,
       line_start, line_end, breadcrumb, complexity, role,
       defined_symbols, referenced_symbols, called_symbols, file_hash,
       created_at, updated_at
FROM segments
WHERE context_id = ?1
  AND file_path = ?2
ORDER BY line_start";

#[allow(dead_code)]
pub const DELETE_SEGMENTS_BY_FILE: &str = "DELETE FROM segments WHERE file_path = ?1";

pub const DELETE_SEGMENTS_BY_CONTEXT_AND_FILE: &str =
    "DELETE FROM segments WHERE context_id = ?1 AND file_path = ?2";

#[allow(dead_code)]
pub const SELECT_FILE_HASH: &str = "
SELECT DISTINCT file_hash
FROM segments
WHERE file_path = ?1
LIMIT 1";

pub const SELECT_FILE_HASH_FOR_CONTEXT: &str = "
SELECT DISTINCT file_hash
FROM segments
WHERE context_id = ?1
  AND file_path = ?2
LIMIT 1";

#[allow(dead_code)]
pub const SELECT_ALL_FILE_PATHS: &str = "
SELECT DISTINCT file_path FROM segments ORDER BY file_path";

pub const SELECT_ALL_FILE_PATHS_FOR_CONTEXT: &str = "
SELECT DISTINCT file_path
FROM segments
WHERE context_id = ?1
ORDER BY file_path";

#[allow(dead_code)]
pub const SELECT_TEST_FILE_PATHS_LIMITED: &str = "
SELECT DISTINCT file_path
FROM segments
WHERE lower(file_path) LIKE 'tests/%'
   OR lower(file_path) LIKE '%/tests/%'
   OR lower(file_path) LIKE '%/test/%'
   OR lower(file_path) LIKE '%/spec/%'
   OR lower(file_path) LIKE '%/__tests__/%'
   OR lower(file_path) LIKE '%_test.rs'
   OR lower(file_path) LIKE '%_spec.rs'
   OR lower(file_path) LIKE '%.test.ts'
   OR lower(file_path) LIKE '%.spec.ts'
   OR lower(file_path) LIKE '%.test.js'
   OR lower(file_path) LIKE '%.spec.js'
ORDER BY file_path
LIMIT ?1";

pub const SELECT_TEST_FILE_PATHS_LIMITED_FOR_CONTEXT: &str = "
SELECT DISTINCT file_path
FROM segments
WHERE context_id = ?1
  AND (
       lower(file_path) LIKE 'tests/%'
    OR lower(file_path) LIKE '%/tests/%'
    OR lower(file_path) LIKE '%/test/%'
    OR lower(file_path) LIKE '%/spec/%'
    OR lower(file_path) LIKE '%/__tests__/%'
    OR lower(file_path) LIKE '%_test.rs'
    OR lower(file_path) LIKE '%_spec.rs'
    OR lower(file_path) LIKE '%.test.ts'
    OR lower(file_path) LIKE '%.spec.ts'
    OR lower(file_path) LIKE '%.test.js'
    OR lower(file_path) LIKE '%.spec.js'
  )
ORDER BY file_path
LIMIT ?2";

#[allow(dead_code)]
pub const SELECT_SCOPED_TEST_FILE_PATHS_LIMITED: &str = "
SELECT DISTINCT file_path
FROM segments
WHERE (file_path = ?1 OR file_path LIKE ?2)
  AND (
       lower(file_path) LIKE 'tests/%'
    OR lower(file_path) LIKE '%/tests/%'
    OR lower(file_path) LIKE '%/test/%'
    OR lower(file_path) LIKE '%/spec/%'
    OR lower(file_path) LIKE '%/__tests__/%'
    OR lower(file_path) LIKE '%_test.rs'
    OR lower(file_path) LIKE '%_spec.rs'
    OR lower(file_path) LIKE '%.test.ts'
    OR lower(file_path) LIKE '%.spec.ts'
    OR lower(file_path) LIKE '%.test.js'
    OR lower(file_path) LIKE '%.spec.js'
  )
ORDER BY file_path
LIMIT ?3";

pub const SELECT_SCOPED_TEST_FILE_PATHS_LIMITED_FOR_CONTEXT: &str = "
SELECT DISTINCT file_path
FROM segments
WHERE context_id = ?1
  AND (file_path = ?2 OR file_path LIKE ?3)
  AND (
       lower(file_path) LIKE 'tests/%'
    OR lower(file_path) LIKE '%/tests/%'
    OR lower(file_path) LIKE '%/test/%'
    OR lower(file_path) LIKE '%/spec/%'
    OR lower(file_path) LIKE '%/__tests__/%'
    OR lower(file_path) LIKE '%_test.rs'
    OR lower(file_path) LIKE '%_spec.rs'
    OR lower(file_path) LIKE '%.test.ts'
    OR lower(file_path) LIKE '%.spec.ts'
    OR lower(file_path) LIKE '%.test.js'
    OR lower(file_path) LIKE '%.spec.js'
  )
ORDER BY file_path
LIMIT ?4";

#[allow(dead_code)]
pub const SELECT_ALL_FILE_HASHES: &str = "
SELECT file_path, MAX(file_hash) AS file_hash
FROM segments
GROUP BY file_path
ORDER BY file_path";

#[allow(dead_code)]
pub const SELECT_SEGMENT_BY_ID: &str = "
SELECT id, file_path, language, block_type, content,
       line_start, line_end, breadcrumb, complexity, role,
       defined_symbols, referenced_symbols, called_symbols, file_hash,
       created_at, updated_at
FROM segments
WHERE id = ?1";

pub const SELECT_SEGMENT_BY_ID_FOR_CONTEXT: &str = "
SELECT id, file_path, language, block_type, content,
       line_start, line_end, breadcrumb, complexity, role,
       defined_symbols, referenced_symbols, called_symbols, file_hash,
       created_at, updated_at
FROM segments
WHERE context_id = ?1
  AND id = ?2";

/// Resolve a segment handle by prefix. LIMIT 5 keeps disambiguation hints bounded
/// while still detecting collisions beyond the first two matches.
pub const SELECT_SEGMENTS_BY_PREFIX: &str = "
SELECT id, file_path, language, block_type, content,
       line_start, line_end, breadcrumb, complexity, role,
       defined_symbols, referenced_symbols, called_symbols, file_hash,
       created_at, updated_at
FROM segments
WHERE id LIKE ?1 || '%'
ORDER BY id
LIMIT 5";

pub const SELECT_SEGMENTS_BY_PREFIX_FOR_CONTEXT: &str = "
SELECT id, file_path, language, block_type, content,
       line_start, line_end, breadcrumb, complexity, role,
       defined_symbols, referenced_symbols, called_symbols, file_hash,
       created_at, updated_at
FROM segments
WHERE context_id = ?1
  AND id LIKE ?2 || '%'
ORDER BY id
LIMIT 5";

/// Build the exact-id batch-fetch statement for `id_count` segment ids within
/// one context. Selects the same columns in the same order as
/// [`SELECT_SEGMENT_BY_ID_FOR_CONTEXT`], so a batched fetch returns rows
/// byte-identical to issuing `id_count` individual id lookups (R-013).
/// Params: `?1` context id, `?2..?(id_count + 1)` segment ids.
pub fn select_segments_by_ids_for_context_sql(id_count: usize) -> String {
    let mut id_placeholders = String::new();
    for index in 0..id_count {
        if index > 0 {
            id_placeholders.push_str(", ");
        }
        write!(id_placeholders, "?{}", index + 2).expect("write to String cannot fail");
    }

    format!(
        "SELECT id, file_path, language, block_type, content,
       line_start, line_end, breadcrumb, complexity, role,
       defined_symbols, referenced_symbols, called_symbols, file_hash,
       created_at, updated_at
FROM segments
WHERE context_id = ?1
  AND id IN ({ids})",
        ids = id_placeholders,
    )
}

pub const UPSERT_META: &str = "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)";

pub const SELECT_META: &str = "SELECT value FROM meta WHERE key = ?1";

#[allow(dead_code)]
pub const DELETE_META: &str = "DELETE FROM meta WHERE key = ?1";

pub const COUNT_SEGMENTS: &str = "SELECT COUNT(*) FROM segments";

pub const COUNT_SEGMENTS_FOR_CONTEXT: &str = "SELECT COUNT(*) FROM segments WHERE context_id = ?1";

pub const COUNT_FILES: &str = "SELECT COUNT(DISTINCT file_path) FROM segments";

pub const COUNT_FILES_FOR_CONTEXT: &str =
    "SELECT COUNT(DISTINCT file_path) FROM segments WHERE context_id = ?1";

pub const COUNT_VECTOR_ROWS_FOR_CONTEXT: &str = "
SELECT COUNT(*)
FROM segment_vectors AS sv
JOIN segments AS s ON s.id = sv.segment_id
WHERE s.context_id = ?1";

/// Counts the segments the pipeline would embed for one context, mirroring
/// `should_embed_segment`: structural segments always count, text chunks
/// count unless their language is in `NON_EMBEDDABLE_CHUNK_LANGUAGES`.
pub static COUNT_EMBEDDABLE_SEGMENTS_FOR_CONTEXT: LazyLock<String> = LazyLock::new(|| {
    let excluded = crate::shared::constants::NON_EMBEDDABLE_CHUNK_LANGUAGES
        .iter()
        .map(|language| format!("'{language}'"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "SELECT COUNT(*) FROM segments \
         WHERE context_id = ?1 \
           AND NOT (block_type = 'chunk' AND language IN ({excluded}))"
    )
});

pub const SELECT_FILE_PATHS_BY_LANGUAGE: &str = "
SELECT DISTINCT file_path FROM segments
WHERE language = ?1
ORDER BY file_path";

pub const SELECT_FILE_PATHS_BY_LANGUAGE_FOR_CONTEXT: &str = "
SELECT DISTINCT file_path FROM segments
WHERE context_id = ?1
  AND language = ?2
ORDER BY file_path";

#[allow(dead_code)]
pub const SELECT_SYMBOL_MATCHES_BY_CANONICAL: &str = "
SELECT s.id, s.file_path, s.language, s.block_type, s.content,
       s.line_start, s.line_end, s.breadcrumb, s.complexity, s.role,
       s.defined_symbols, s.referenced_symbols, s.called_symbols, s.file_hash,
       s.created_at, s.updated_at, ss.symbol, ss.canonical_symbol
FROM segment_symbols AS ss
JOIN segments AS s ON s.id = ss.segment_id
WHERE ss.reference_kind = ?1
  AND ss.canonical_symbol = ?2
ORDER BY
  CASE WHEN s.block_type IN ('function', 'struct', 'trait', 'class', 'interface', 'type', 'enum') THEN 0 ELSE 1 END,
  s.file_path,
  s.line_start,
  ss.symbol";

pub const SELECT_SYMBOL_MATCHES_BY_CANONICAL_FOR_CONTEXT: &str = "
SELECT s.id, s.file_path, s.language, s.block_type, s.content,
       s.line_start, s.line_end, s.breadcrumb, s.complexity, s.role,
       s.defined_symbols, s.referenced_symbols, s.called_symbols, s.file_hash,
       s.created_at, s.updated_at, ss.symbol, ss.canonical_symbol
FROM segment_symbols AS ss
JOIN segments AS s ON s.id = ss.segment_id
WHERE ss.context_id = ?1
  AND s.context_id = ?1
  AND ss.reference_kind = ?2
  AND ss.canonical_symbol = ?3
ORDER BY
  CASE WHEN s.block_type IN ('function', 'struct', 'trait', 'class', 'interface', 'type', 'enum') THEN 0 ELSE 1 END,
  s.file_path,
  s.line_start,
  ss.symbol";

/// Build the symbol-match statement for `canonical_count` canonical symbols
/// within one context. Identical columns and `ORDER BY` to
/// [`SELECT_SYMBOL_MATCHES_BY_CANONICAL_FOR_CONTEXT`], so the rows for any one
/// canonical come back in the same relative order a single-canonical lookup
/// would return; the trailing `ss.canonical_symbol` column lets the caller
/// regroup rows per canonical and replay the per-canonical iteration order and
/// dedup of the per-item path (R-013).
/// Params: `?1` context id, `?2` reference kind,
/// `?3..?(canonical_count + 2)` canonical symbols.
pub fn select_symbol_matches_by_canonicals_for_context_sql(canonical_count: usize) -> String {
    let mut canonical_placeholders = String::new();
    for index in 0..canonical_count {
        if index > 0 {
            canonical_placeholders.push_str(", ");
        }
        write!(canonical_placeholders, "?{}", index + 3).expect("write to String cannot fail");
    }

    format!(
        "SELECT s.id, s.file_path, s.language, s.block_type, s.content,
       s.line_start, s.line_end, s.breadcrumb, s.complexity, s.role,
       s.defined_symbols, s.referenced_symbols, s.called_symbols, s.file_hash,
       s.created_at, s.updated_at, ss.symbol, ss.canonical_symbol
FROM segment_symbols AS ss
JOIN segments AS s ON s.id = ss.segment_id
WHERE ss.context_id = ?1
  AND s.context_id = ?1
  AND ss.reference_kind = ?2
  AND ss.canonical_symbol IN ({canonicals})
ORDER BY
  CASE WHEN s.block_type IN ('function', 'struct', 'trait', 'class', 'interface', 'type', 'enum') THEN 0 ELSE 1 END,
  s.file_path,
  s.line_start,
  ss.symbol",
        canonicals = canonical_placeholders,
    )
}

#[allow(dead_code)]
pub const SELECT_DISTINCT_SYMBOL_CANONICALS_BY_PREFIX: &str = "
SELECT DISTINCT canonical_symbol
FROM segment_symbols
WHERE reference_kind = ?1
  AND canonical_symbol LIKE ?2 || '%'
ORDER BY LENGTH(canonical_symbol), canonical_symbol
LIMIT ?3";

pub const SELECT_DISTINCT_SYMBOL_CANONICALS_BY_PREFIX_FOR_CONTEXT: &str = "
SELECT DISTINCT canonical_symbol
FROM segment_symbols
WHERE context_id = ?1
  AND reference_kind = ?2
  AND canonical_symbol LIKE ?3 || '%'
ORDER BY LENGTH(canonical_symbol), canonical_symbol
LIMIT ?4";

#[allow(dead_code)]
pub const SELECT_DISTINCT_SYMBOL_CANONICALS_BY_CONTAINS: &str = "
SELECT DISTINCT canonical_symbol
FROM segment_symbols
WHERE reference_kind = ?1
  AND canonical_symbol LIKE '%' || ?2 || '%'
ORDER BY
  CASE WHEN canonical_symbol LIKE ?2 || '%' THEN 0 ELSE 1 END,
  ABS(LENGTH(canonical_symbol) - LENGTH(?2)),
  LENGTH(canonical_symbol),
  canonical_symbol
LIMIT ?3";

pub const SELECT_DISTINCT_SYMBOL_CANONICALS_BY_CONTAINS_FOR_CONTEXT: &str = "
SELECT DISTINCT canonical_symbol
FROM segment_symbols
WHERE context_id = ?1
  AND reference_kind = ?2
  AND canonical_symbol LIKE '%' || ?3 || '%'
ORDER BY
  CASE WHEN canonical_symbol LIKE ?3 || '%' THEN 0 ELSE 1 END,
  ABS(LENGTH(canonical_symbol) - LENGTH(?3)),
  LENGTH(canonical_symbol),
  canonical_symbol
LIMIT ?4";

#[allow(dead_code)]
pub const SELECT_ALL_INDEXED_FILES: &str = "
SELECT file_path, extension, file_hash, file_size, modified_ns
FROM indexed_files
ORDER BY file_path";

pub const SELECT_ALL_INDEXED_FILES_FOR_CONTEXT: &str = "
SELECT file_path, extension, file_hash, file_size, modified_ns
FROM indexed_files
WHERE context_id = ?1
ORDER BY file_path";

#[allow(dead_code)]
pub const SELECT_INDEXED_FILE: &str = "
SELECT file_path, extension, file_hash, file_size, modified_ns
FROM indexed_files
WHERE file_path = ?1";

pub const SELECT_INDEXED_FILE_FOR_CONTEXT: &str = "
SELECT file_path, extension, file_hash, file_size, modified_ns
FROM indexed_files
WHERE context_id = ?1
  AND file_path = ?2";

pub const UPSERT_INDEXED_FILE: &str = "
INSERT OR REPLACE INTO indexed_files (
    context_id, file_path, extension, file_hash, file_size, modified_ns,
    created_at, updated_at
) VALUES (
    ?1, ?2, ?3, ?4, ?5, ?6, datetime('now'), datetime('now')
)";

pub const DELETE_INDEXED_FILE: &str =
    "DELETE FROM indexed_files WHERE context_id = ?1 AND file_path = ?2";

pub const UPSERT_WORKTREE_CONTEXT: &str = "
INSERT OR REPLACE INTO worktree_contexts (
    context_id, project_id, state_root, source_root, main_worktree_root,
    worktree_role, branch_name, branch_ref, branch_status, head_oid,
    git_dir, common_git_dir, updated_at
) VALUES (
    ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, datetime('now')
)";

pub const SELECT_WORKTREE_CONTEXT_HEAD_OID: &str =
    "SELECT head_oid FROM worktree_contexts WHERE context_id = ?1";

/// Every recorded worktree context, used by `1up gc` to decide which contexts are
/// stale branch snapshots or dead worktrees that can be pruned from the shared index.
pub const SELECT_ALL_WORKTREE_CONTEXTS: &str =
    "SELECT context_id, state_root, source_root, branch_name FROM worktree_contexts";

// Context-wide deletion, used by `1up gc --apply` to evict one worktree context from
// the shared index. `segments` is deleted last of the data tables so its AFTER DELETE
// triggers (`segments_vector_ad`, `segments_symbol_ad`, the FTS `segments_ad`) cascade
// the matching `segment_vectors`, `segment_symbols`, and FTS rows; `segment_relations`
// and `indexed_files` carry no such trigger and are deleted explicitly by context.
pub const DELETE_SEGMENT_RELATIONS_BY_CONTEXT: &str =
    "DELETE FROM segment_relations WHERE context_id = ?1";

pub const DELETE_INDEXED_FILES_BY_CONTEXT: &str = "DELETE FROM indexed_files WHERE context_id = ?1";

pub const DELETE_SEGMENTS_BY_CONTEXT: &str = "DELETE FROM segments WHERE context_id = ?1";

pub const DELETE_WORKTREE_CONTEXT: &str = "DELETE FROM worktree_contexts WHERE context_id = ?1";

/// Reclaim pages freed by context deletion so the `index.db` file actually shrinks.
/// Must run outside a transaction and acquires an exclusive lock.
pub const VACUUM_DATABASE: &str = "VACUUM";

/// Conflict clause appended to chunked multi-row segment inserts. Mirrors
/// `UPSERT_SEGMENT`: DO UPDATE (never REPLACE) so conflict resolution cannot
/// delete rows and fire `segments_vector_ad` or bypass the FTS triggers.
pub const SEGMENT_UPSERT_CONFLICT_CLAUSE: &str = "
ON CONFLICT(id) DO UPDATE SET
    context_id = excluded.context_id,
    file_path = excluded.file_path,
    language = excluded.language,
    block_type = excluded.block_type,
    content = excluded.content,
    line_start = excluded.line_start,
    line_end = excluded.line_end,
    breadcrumb = excluded.breadcrumb,
    complexity = excluded.complexity,
    role = excluded.role,
    defined_symbols = excluded.defined_symbols,
    referenced_symbols = excluded.referenced_symbols,
    called_symbols = excluded.called_symbols,
    file_hash = excluded.file_hash,
    updated_at = datetime('now')";

/// Conflict clause appended to chunked multi-row vector inserts. Mirrors
/// `UPSERT_SEGMENT_VECTOR`: a `segment_vectors` row now carries only the
/// `content_key` reference into `embedding_pool`, so a conflicting re-write of
/// the same `segment_id` repoints it at its (re-derived) content key.
pub const VECTOR_UPSERT_CONFLICT_CLAUSE: &str = "
ON CONFLICT(segment_id) DO UPDATE SET
    content_key = excluded.content_key";

/// Conflict clause appended to chunked multi-row `embedding_pool` inserts.
/// Mirrors [`UPSERT_EMBEDDING_POOL`]: a key already present keeps its existing
/// (deterministic) vector and `ref_count`, so re-seeing shared content across
/// contexts never rewrites the pooled vector.
pub const EMBEDDING_POOL_UPSERT_CONFLICT_CLAUSE: &str = "
ON CONFLICT(content_key) DO NOTHING";

/// Maximum number of SQL parameters per statement to stay below SQLite limits.
pub const SQLITE_MAX_PARAMS: usize = 999;

/// Number of columns in a segment INSERT (positional params only, excludes datetime('now') literals).
pub const SEGMENT_INSERT_COLS: usize = 15;

/// Number of columns in a segment_symbols INSERT (positional params only).
pub const SYMBOL_INSERT_COLS: usize = 5;

/// Number of columns in a segment_relations INSERT (positional params only).
pub const RELATION_INSERT_COLS: usize = 7;

/// Number of columns in a context-scoped segment_relations INSERT (positional params only).
pub const CONTEXT_RELATION_INSERT_COLS: usize = 8;

/// Number of columns in a segment_vectors INSERT (positional params only):
/// `(segment_id, content_key)`.
pub const VECTOR_INSERT_COLS: usize = 2;

/// Number of columns in an embedding_pool INSERT (positional params only):
/// `(content_key, embedding_vec)`; `ref_count` is seeded by the literal `0`.
pub const POOL_INSERT_COLS: usize = 2;

/// Maximum rows per chunk for each table, derived from `SQLITE_MAX_PARAMS`.
pub const SEGMENT_CHUNK_SIZE: usize = SQLITE_MAX_PARAMS / SEGMENT_INSERT_COLS;
pub const SYMBOL_CHUNK_SIZE: usize = SQLITE_MAX_PARAMS / SYMBOL_INSERT_COLS;
pub const RELATION_CHUNK_SIZE: usize = SQLITE_MAX_PARAMS / RELATION_INSERT_COLS;
pub const CONTEXT_RELATION_CHUNK_SIZE: usize = SQLITE_MAX_PARAMS / CONTEXT_RELATION_INSERT_COLS;
pub const VECTOR_CHUNK_SIZE: usize = SQLITE_MAX_PARAMS / VECTOR_INSERT_COLS;
pub const POOL_CHUNK_SIZE: usize = SQLITE_MAX_PARAMS / POOL_INSERT_COLS;

// --- Overview digest aggregates ---------------------------------------------
//
// Bounded aggregate queries backing the `oneup_overview` orientation digest.
// The symbol-ranking and module-dependency statements must apply an identical
// filter stack (identity-bearing edge kinds, qualifying definition kinds,
// qualifying roles, edge/block compatibility), so those fragments are defined
// once below and the full statements are composed from them.
//
// HARD CONSTRAINT (design D16): every per-key or per-pair predicate is
// pre-aggregated in a CTE grouped by the symbol key and equi-joined to the
// relation scan. Correlated per-row subqueries or EXISTS probes against
// `segment_symbols`/`segments` are prohibited: HYP-001 v2 measured identical
// predicates at 0.19-0.44s in this form versus 183.8s (~400x) correlated on
// an 81k-relation index.

/// Module key used for files that live directly in the repository root.
pub const OVERVIEW_ROOT_MODULE_KEY: &str = "(root)";

/// Relation rows that carry usable target identity for aggregate ranking.
/// `method_receiver`/`member_access` rows are excluded: their identity is only
/// derivable through per-pair owner alignment, which a bounded aggregate
/// cannot compute (design D13).
pub const OVERVIEW_IDENTITY_BEARING_EDGE_KINDS_SQL: &str =
    "('bare_identifier', 'qualified_path', 'constructor_like', 'macro_like')";

/// Qualifying definition kinds for overview ranking: type definitions only,
/// the shipped Branch B policy (HYP-001 v3 verdict, design D19 documented
/// REQ-003 downscope).
pub const OVERVIEW_QUALIFYING_TYPE_KINDS_SQL: &str =
    "('struct', 'enum', 'trait', 'class', 'interface')";

/// Roles a segment must carry for its symbol rows to qualify as definitions.
pub const OVERVIEW_QUALIFYING_ROLES_SQL: &str = "('DEFINITION', 'IMPLEMENTATION', 'ORCHESTRATION')";

/// Per-key edge-compatibility flag columns computed inside the qualifying
/// definitions CTE. Mirrors `relation_candidate_edge_compatible` in
/// `src/search/impact.rs`: `macro_like` edges pair only with `macro`
/// definitions and `constructor_like` edges only with constructor-like block
/// types; under the Branch B kind policy the macro flag is always 0, so
/// `macro_like` edges never count toward rank.
const OVERVIEW_EDGE_COMPATIBILITY_FLAGS_SQL: &str = "\
MAX(CASE WHEN s.block_type = 'macro' THEN 1 ELSE 0 END) AS has_macro_definition,
       MAX(CASE WHEN s.block_type IN ('constructor', 'class', 'struct', 'enum') THEN 1 ELSE 0 END) AS has_constructor_compatible_definition";

/// Edge/definition compatibility predicate applied to each relation row
/// against the pre-aggregated per-key flags (`bare_identifier` and
/// `qualified_path` pair with any qualifying definition).
const OVERVIEW_EDGE_COMPATIBILITY_PREDICATE_SQL: &str = "\
CASE r.edge_identity_kind
    WHEN 'macro_like' THEN qd.has_macro_definition
    WHEN 'constructor_like' THEN qd.has_constructor_compatible_definition
    ELSE 1
  END = 1";

/// Pre-aggregated qualifying definition CTE body: one row per symbol key with
/// its qualifying definition count and edge-compatibility flags (D16 shape).
fn overview_qualifying_definitions_cte_sql() -> String {
    format!(
        "SELECT ss.canonical_symbol AS symbol_key,
       COUNT(*) AS definition_count,
       {flags}
FROM segment_symbols AS ss
JOIN segments AS s ON s.id = ss.segment_id
WHERE ss.context_id = ?1
  AND s.context_id = ?1
  AND ss.reference_kind = 'definition'
  AND s.block_type IN {kinds}
  AND s.role IN {roles}
GROUP BY ss.canonical_symbol",
        flags = OVERVIEW_EDGE_COMPATIBILITY_FLAGS_SQL,
        kinds = OVERVIEW_QUALIFYING_TYPE_KINDS_SQL,
        roles = OVERVIEW_QUALIFYING_ROLES_SQL,
    )
}

/// Depth-1 module key for a path column: the first path component, or
/// `(root)` for top-level files.
fn overview_depth1_module_expr_sql(path_column: &str) -> String {
    format!(
        "CASE WHEN instr({col}, '/') = 0 THEN '{root}' \
         ELSE substr({col}, 1, instr({col}, '/') - 1) END",
        col = path_column,
        root = OVERVIEW_ROOT_MODULE_KEY,
    )
}

/// Depth-2 module key for a path column: the first two path components.
/// Files directly inside a depth-1 directory keep the depth-1 key, and
/// top-level files map to `(root)`.
fn overview_depth2_module_expr_sql(path_column: &str) -> String {
    format!(
        "CASE WHEN instr({col}, '/') = 0 THEN '{root}' \
         WHEN instr(substr({col}, instr({col}, '/') + 1), '/') = 0 \
             THEN substr({col}, 1, instr({col}, '/') - 1) \
         ELSE substr({col}, 1, instr({col}, '/') + instr(substr({col}, instr({col}, '/') + 1), '/') - 1) END",
        col = path_column,
        root = OVERVIEW_ROOT_MODULE_KEY,
    )
}

/// Per-language file and segment counts for one context.
/// Params: `?1` context id, `?2` row limit.
pub const SELECT_LANGUAGE_STATS_FOR_CONTEXT: &str = "
SELECT language,
       COUNT(DISTINCT file_path) AS file_count,
       COUNT(*) AS segment_count
FROM segments
WHERE context_id = ?1
GROUP BY language
ORDER BY segment_count DESC, language ASC
LIMIT ?2";

/// Depth-1 module segment counts for one context.
/// Params: `?1` context id, `?2` row limit.
pub static SELECT_MODULE_SEGMENT_COUNTS_FOR_CONTEXT: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT {module} AS module,
       COUNT(*) AS segment_count
FROM segments
WHERE context_id = ?1
GROUP BY module
ORDER BY segment_count DESC, module ASC
LIMIT ?2",
        module = overview_depth1_module_expr_sql("file_path"),
    )
});

/// Depth-2 segment counts under one depth-1 module (dominant-module
/// expansion). The prefix match is exact (`substr`), not LIKE, so module
/// names containing wildcard characters cannot over-match.
/// Params: `?1` context id, `?2` parent module key, `?3` row limit.
pub static SELECT_MODULE_CHILD_SEGMENT_COUNTS_FOR_CONTEXT: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT {module} AS module,
       COUNT(*) AS segment_count
FROM segments
WHERE context_id = ?1
  AND substr(file_path, 1, length(?2) + 1) = ?2 || '/'
GROUP BY module
ORDER BY segment_count DESC, module ASC
LIMIT ?3",
        module = overview_depth2_module_expr_sql("file_path"),
    )
});

/// Overview symbol ranking: distinct referencing source files per symbol key
/// over identity-bearing relation rows equi-joined to the pre-aggregated
/// qualifying definition CTE (D16). The per-key 1..=3 ambiguity rule is
/// applied by the overview engine after Rust-side path exclusions, so this
/// statement reports `definition_count` instead of capping on it.
/// Params: `?1` context id, `?2` row limit (oversample).
pub static SELECT_TOP_TYPE_SYMBOL_REFERENCES_FOR_CONTEXT: LazyLock<String> = LazyLock::new(|| {
    format!(
        "WITH qualifying_definitions AS (
{cte}
)
SELECT r.lookup_canonical_symbol AS symbol_key,
       COUNT(DISTINCT src.file_path) AS referencing_files,
       qd.definition_count AS definition_count
FROM segment_relations AS r
JOIN segments AS src ON src.id = r.source_segment_id
JOIN qualifying_definitions AS qd ON qd.symbol_key = r.lookup_canonical_symbol
WHERE r.context_id = ?1
  AND src.context_id = ?1
  AND r.edge_identity_kind IN {edge_kinds}
  AND {compatibility}
GROUP BY symbol_key, qd.definition_count
ORDER BY referencing_files DESC, symbol_key ASC
LIMIT ?2",
        cte = overview_qualifying_definitions_cte_sql(),
        edge_kinds = OVERVIEW_IDENTITY_BEARING_EDGE_KINDS_SQL,
        compatibility = OVERVIEW_EDGE_COMPATIBILITY_PREDICATE_SQL,
    )
});

/// Directed depth-2 module dependency pairs sharing the symbol-ranking filter
/// stack plus the SQL-side per-key qualifying-definition-count cap of 1..=3
/// (design D18; counted pre-exclusion, a documented divergence from the
/// top-symbols rule). Each pair counts distinct (referencing file, symbol
/// key) combinations; self-edge dropping and test/low-signal target exclusion
/// happen in the overview engine rollup.
/// Params: `?1` context id, `?2` row limit.
pub static SELECT_MODULE_DEPENDENCY_PAIRS_FOR_CONTEXT: LazyLock<String> = LazyLock::new(|| {
    format!(
        "WITH qualifying_definitions AS (
{cte}
),
definition_modules AS (
    SELECT ss.canonical_symbol AS symbol_key,
           {target_module} AS target_module
    FROM segment_symbols AS ss
    JOIN segments AS s ON s.id = ss.segment_id
    WHERE ss.context_id = ?1
      AND s.context_id = ?1
      AND ss.reference_kind = 'definition'
      AND s.block_type IN {kinds}
      AND s.role IN {roles}
    GROUP BY ss.canonical_symbol, target_module
)
SELECT {source_module} AS source_module,
       dm.target_module AS target_module,
       COUNT(DISTINCT src.file_path || char(31) || r.lookup_canonical_symbol) AS pair_count
FROM segment_relations AS r
JOIN segments AS src ON src.id = r.source_segment_id
JOIN qualifying_definitions AS qd
  ON qd.symbol_key = r.lookup_canonical_symbol
 AND qd.definition_count BETWEEN 1 AND 3
JOIN definition_modules AS dm ON dm.symbol_key = r.lookup_canonical_symbol
WHERE r.context_id = ?1
  AND src.context_id = ?1
  AND r.edge_identity_kind IN {edge_kinds}
  AND {compatibility}
GROUP BY source_module, dm.target_module
ORDER BY pair_count DESC, source_module ASC, target_module ASC
LIMIT ?2",
        cte = overview_qualifying_definitions_cte_sql(),
        target_module = overview_depth2_module_expr_sql("s.file_path"),
        source_module = overview_depth2_module_expr_sql("src.file_path"),
        kinds = OVERVIEW_QUALIFYING_TYPE_KINDS_SQL,
        roles = OVERVIEW_QUALIFYING_ROLES_SQL,
        edge_kinds = OVERVIEW_IDENTITY_BEARING_EDGE_KINDS_SQL,
        compatibility = OVERVIEW_EDGE_COMPATIBILITY_PREDICATE_SQL,
    )
});

/// Shallow orchestration/definition entry-point candidates for one context.
/// Test/low-signal path exclusion happens in the overview engine, which is
/// why callers oversample.
/// Params: `?1` context id, `?2` row limit.
pub const SELECT_ENTRY_POINT_CANDIDATES_FOR_CONTEXT: &str = "
SELECT id, file_path, line_start, line_end, role, breadcrumb, defined_symbols
FROM segments
WHERE context_id = ?1
  AND role IN ('ORCHESTRATION', 'DEFINITION')
  AND block_type != 'chunk'
ORDER BY
  length(file_path) - length(replace(file_path, '/', '')) ASC,
  CASE role WHEN 'ORCHESTRATION' THEN 0 ELSE 1 END ASC,
  file_path ASC,
  line_start ASC
LIMIT ?2";

/// Build the qualifying type definition resolution statement for `key_count`
/// symbol keys (the oversampled ranking keys). Returned rows are ordered by
/// symbol key, file path, line start, then segment id; kind-rank attribution
/// and path exclusion happen in the overview engine.
/// Params: `?1` context id, `?2..?(key_count + 1)` symbol keys,
/// `?(key_count + 2)` row limit.
pub fn select_qualifying_type_definitions_for_context_sql(key_count: usize) -> String {
    let mut key_placeholders = String::new();
    for index in 0..key_count {
        if index > 0 {
            key_placeholders.push_str(", ");
        }
        write!(key_placeholders, "?{}", index + 2).expect("write to String cannot fail");
    }

    format!(
        "SELECT ss.canonical_symbol AS symbol_key,
       ss.symbol,
       s.id,
       s.file_path,
       s.line_start,
       s.line_end,
       s.block_type
FROM segment_symbols AS ss
JOIN segments AS s ON s.id = ss.segment_id
WHERE ss.context_id = ?1
  AND s.context_id = ?1
  AND ss.reference_kind = 'definition'
  AND s.block_type IN {kinds}
  AND s.role IN {roles}
  AND ss.canonical_symbol IN ({keys})
ORDER BY symbol_key ASC, s.file_path ASC, s.line_start ASC, s.id ASC
LIMIT ?{limit_param}",
        kinds = OVERVIEW_QUALIFYING_TYPE_KINDS_SQL,
        roles = OVERVIEW_QUALIFYING_ROLES_SQL,
        keys = key_placeholders,
        limit_param = key_count + 2,
    )
}
