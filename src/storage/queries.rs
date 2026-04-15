pub const CREATE_SEGMENTS_TABLE: &str = "
CREATE TABLE IF NOT EXISTS segments (
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
)";

pub const CREATE_INDEX_FILE_PATH: &str =
    "CREATE INDEX IF NOT EXISTS idx_segments_file_path ON segments(file_path)";

pub const CREATE_INDEX_LANGUAGE: &str =
    "CREATE INDEX IF NOT EXISTS idx_segments_language ON segments(language)";

pub const CREATE_INDEX_FILE_HASH: &str =
    "CREATE INDEX IF NOT EXISTS idx_segments_file_hash ON segments(file_hash)";

pub const CREATE_SEGMENT_VECTORS_TABLE: &str = "
CREATE TABLE IF NOT EXISTS segment_vectors (
    segment_id TEXT PRIMARY KEY,
    embedding_vec FLOAT32(384) NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
)";

pub const CREATE_SEGMENT_SYMBOLS_TABLE: &str = "
CREATE TABLE IF NOT EXISTS segment_symbols (
    segment_id TEXT NOT NULL,
    symbol TEXT NOT NULL,
    canonical_symbol TEXT NOT NULL,
    reference_kind TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (segment_id, canonical_symbol, reference_kind)
)";

pub const CREATE_SEGMENT_RELATIONS_TABLE: &str = "
CREATE TABLE IF NOT EXISTS segment_relations (
    source_segment_id TEXT NOT NULL,
    relation_kind TEXT NOT NULL,
    raw_target_symbol TEXT NOT NULL,
    canonical_target_symbol TEXT NOT NULL,
    lookup_canonical_symbol TEXT NOT NULL,
    qualifier_fingerprint TEXT NOT NULL,
    edge_identity_kind TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (
        source_segment_id,
        relation_kind,
        canonical_target_symbol,
        raw_target_symbol,
        edge_identity_kind
    )
)";

pub const CREATE_INDEX_SEGMENT_VECTORS_EMBEDDING: &str =
    "CREATE INDEX IF NOT EXISTS idx_segment_vectors_embedding ON segment_vectors (libsql_vector_idx(embedding_vec))";

pub const CREATE_INDEX_SEGMENT_SYMBOLS_EXACT: &str =
    "CREATE INDEX IF NOT EXISTS idx_segment_symbols_exact ON segment_symbols(canonical_symbol, reference_kind)";

pub const CREATE_INDEX_SEGMENT_SYMBOLS_PREFIX: &str =
    "CREATE INDEX IF NOT EXISTS idx_segment_symbols_prefix ON segment_symbols(canonical_symbol)";

pub const CREATE_INDEX_SEGMENT_RELATIONS_SOURCE: &str =
    "CREATE INDEX IF NOT EXISTS idx_segment_relations_source ON segment_relations(source_segment_id)";

pub const CREATE_INDEX_SEGMENT_RELATIONS_TARGET: &str =
    "CREATE INDEX IF NOT EXISTS idx_segment_relations_target ON segment_relations(canonical_target_symbol, relation_kind)";

pub const CREATE_INDEX_SEGMENT_RELATIONS_LOOKUP_TARGET: &str =
    "CREATE INDEX IF NOT EXISTS idx_segment_relations_lookup_target ON segment_relations(lookup_canonical_symbol, relation_kind)";

pub const CREATE_FTS_TABLE: &str = "
CREATE VIRTUAL TABLE IF NOT EXISTS segments_fts USING fts5(
    content,
    content='segments',
    content_rowid='rowid'
)";

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

pub const DROP_SEARCH_SCHEMA: &str = "
DROP TRIGGER IF EXISTS segments_ai;
DROP TRIGGER IF EXISTS segments_ad;
DROP TRIGGER IF EXISTS segments_au;
DROP TRIGGER IF EXISTS segments_vector_ad;
DROP TRIGGER IF EXISTS segments_symbol_ad;
DROP TABLE IF EXISTS segments_fts;
DROP INDEX IF EXISTS idx_segment_vectors_embedding;
DROP INDEX IF EXISTS idx_segment_symbols_exact;
DROP INDEX IF EXISTS idx_segment_symbols_prefix;
DROP INDEX IF EXISTS idx_segment_relations_source;
DROP INDEX IF EXISTS idx_segment_relations_target;
DROP INDEX IF EXISTS idx_segment_relations_lookup_target;
DROP TABLE IF EXISTS segment_vectors;
DROP TABLE IF EXISTS segment_symbols;
DROP TABLE IF EXISTS segment_relations;
DROP INDEX IF EXISTS idx_segments_file_path;
DROP INDEX IF EXISTS idx_segments_language;
DROP INDEX IF EXISTS idx_segments_file_hash;
DROP TABLE IF EXISTS segments;
DROP TABLE IF EXISTS meta";

pub const SELECT_SCHEMA_OBJECT: &str =
    "SELECT 1 FROM sqlite_master WHERE type = ?1 AND name = ?2 LIMIT 1";

pub const SELECT_HAS_USER_TABLES: &str =
    "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' LIMIT 1";

pub const SELECT_HAS_INDEXED_EMBEDDINGS: &str = "SELECT 1 FROM segment_vectors LIMIT 1";

pub const SELECT_VECTOR_CANDIDATES: &str = "
SELECT s.id, s.file_path, s.language, s.block_type,
       s.line_start, s.line_end, s.breadcrumb, s.complexity,
       s.role, s.defined_symbols, s.referenced_symbols, s.called_symbols
FROM vector_top_k('idx_segment_vectors_embedding', vector(?1), ?2) AS v
JOIN segment_vectors AS sv ON sv.rowid = v.id
JOIN segments AS s ON s.id = sv.segment_id";

pub const SELECT_FTS_CANDIDATES: &str = "
SELECT s.id, s.file_path, s.language, s.block_type,
       s.line_start, s.line_end, s.breadcrumb, s.complexity,
       s.role, s.defined_symbols, s.referenced_symbols, s.called_symbols
FROM segments_fts AS f
JOIN segments AS s ON s.rowid = f.rowid
WHERE segments_fts MATCH ?1
ORDER BY f.rank, s.rowid
LIMIT ?2";

pub const UPSERT_SEGMENT: &str = "
INSERT OR REPLACE INTO segments (
    id, file_path, language, block_type, content,
    line_start, line_end, breadcrumb, complexity, role, defined_symbols, referenced_symbols, called_symbols,
    file_hash, created_at, updated_at
) VALUES (
    ?1, ?2, ?3, ?4, ?5,
    ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
    ?14, datetime('now'), datetime('now')
)";

pub const UPSERT_SEGMENT_VECTOR: &str = "
INSERT OR REPLACE INTO segment_vectors (
    segment_id, embedding_vec, created_at, updated_at
) VALUES (
    ?1, vector(?2), datetime('now'), datetime('now')
)";

pub const DELETE_SEGMENT_VECTOR: &str = "DELETE FROM segment_vectors WHERE segment_id = ?1";

pub const INSERT_SEGMENT_SYMBOL: &str = "
INSERT OR REPLACE INTO segment_symbols (
    segment_id, symbol, canonical_symbol, reference_kind, created_at
) VALUES (
    ?1, ?2, ?3, ?4, datetime('now')
)";

pub const DELETE_SEGMENT_SYMBOLS_BY_SEGMENT_ID: &str =
    "DELETE FROM segment_symbols WHERE segment_id = ?1";

#[allow(dead_code)]
pub const INSERT_SEGMENT_RELATION: &str = "
INSERT OR REPLACE INTO segment_relations (
    source_segment_id,
    relation_kind,
    raw_target_symbol,
    canonical_target_symbol,
    lookup_canonical_symbol,
    qualifier_fingerprint,
    edge_identity_kind,
    created_at
) VALUES (
    ?1, ?2, ?3, ?4, ?5, ?6, ?7, datetime('now')
)";

#[allow(dead_code)]
pub const DELETE_SEGMENT_RELATIONS_BY_SOURCE_SEGMENT_ID: &str =
    "DELETE FROM segment_relations WHERE source_segment_id = ?1";

#[allow(dead_code)]
pub const DELETE_SEGMENT_RELATIONS_BY_FILE: &str = "
DELETE FROM segment_relations
WHERE source_segment_id IN (
    SELECT id
    FROM segments
    WHERE file_path = ?1
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

#[allow(dead_code)]
pub const SELECT_SEGMENTS_BY_FILE: &str = "
SELECT id, file_path, language, block_type, content,
       line_start, line_end, breadcrumb, complexity, role,
       defined_symbols, referenced_symbols, called_symbols, file_hash,
       created_at, updated_at
FROM segments
WHERE file_path = ?1
ORDER BY line_start";

pub const DELETE_SEGMENTS_BY_FILE: &str = "DELETE FROM segments WHERE file_path = ?1";

#[allow(dead_code)]
pub const SELECT_FILE_HASH: &str = "
SELECT DISTINCT file_hash
FROM segments
WHERE file_path = ?1
LIMIT 1";

pub const SELECT_ALL_FILE_PATHS: &str = "
SELECT DISTINCT file_path FROM segments ORDER BY file_path";

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

pub const UPSERT_META: &str = "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)";

pub const SELECT_META: &str = "SELECT value FROM meta WHERE key = ?1";

#[allow(dead_code)]
pub const DELETE_META: &str = "DELETE FROM meta WHERE key = ?1";

pub const COUNT_SEGMENTS: &str = "SELECT COUNT(*) FROM segments";

pub const COUNT_FILES: &str = "SELECT COUNT(DISTINCT file_path) FROM segments";

pub const SELECT_FILE_PATHS_BY_LANGUAGE: &str = "
SELECT DISTINCT file_path FROM segments
WHERE language = ?1
ORDER BY file_path";

pub const SELECT_SYMBOL_MATCHES_BY_CANONICAL: &str = "
SELECT s.id, s.file_path, s.language, s.block_type, s.content,
       s.line_start, s.line_end, s.breadcrumb, s.complexity, s.role,
       s.defined_symbols, s.referenced_symbols, s.called_symbols, s.file_hash,
       s.created_at, s.updated_at, ss.symbol
FROM segment_symbols AS ss
JOIN segments AS s ON s.id = ss.segment_id
WHERE ss.reference_kind = ?1
  AND ss.canonical_symbol = ?2
ORDER BY
  CASE WHEN s.block_type IN ('function', 'struct', 'trait', 'class', 'interface', 'type', 'enum') THEN 0 ELSE 1 END,
  s.file_path,
  s.line_start,
  ss.symbol";

pub const SELECT_DISTINCT_SYMBOL_CANONICALS_BY_PREFIX: &str = "
SELECT DISTINCT canonical_symbol
FROM segment_symbols
WHERE reference_kind = ?1
  AND canonical_symbol LIKE ?2 || '%'
ORDER BY LENGTH(canonical_symbol), canonical_symbol
LIMIT ?3";

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
