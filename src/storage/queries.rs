pub const CREATE_SEGMENTS_TABLE: &str = "
CREATE TABLE IF NOT EXISTS segments (
    id TEXT PRIMARY KEY,
    file_path TEXT NOT NULL,
    language TEXT NOT NULL,
    block_type TEXT NOT NULL,
    content TEXT NOT NULL,
    line_start INTEGER NOT NULL,
    line_end INTEGER NOT NULL,
    embedding F32_BLOB(384),
    embedding_q8 VECTOR8(384),
    complexity INTEGER NOT NULL DEFAULT 0,
    role TEXT NOT NULL DEFAULT 'DEFINITION',
    defined_symbols TEXT NOT NULL DEFAULT '[]',
    referenced_symbols TEXT NOT NULL DEFAULT '[]',
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

pub const CREATE_FTS_INDEX: &str = "
CREATE INDEX IF NOT EXISTS idx_segments_fts ON segments USING fts(content)";

pub const CREATE_META_TABLE: &str = "
CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
)";

pub const UPSERT_SEGMENT: &str = "
INSERT OR REPLACE INTO segments (
    id, file_path, language, block_type, content,
    line_start, line_end, embedding, embedding_q8,
    complexity, role, defined_symbols, referenced_symbols,
    file_hash, created_at, updated_at
) VALUES (
    ?1, ?2, ?3, ?4, ?5,
    ?6, ?7, ?8, ?9,
    ?10, ?11, ?12, ?13,
    ?14, datetime('now'), datetime('now')
)";

#[allow(dead_code)]
pub const SELECT_SEGMENTS_BY_FILE: &str = "
SELECT id, file_path, language, block_type, content,
       line_start, line_end, complexity, role,
       defined_symbols, referenced_symbols, file_hash,
       created_at, updated_at
FROM segments
WHERE file_path = ?1
ORDER BY line_start";

pub const DELETE_SEGMENTS_BY_FILE: &str = "DELETE FROM segments WHERE file_path = ?1";

pub const SELECT_FILE_HASH: &str = "
SELECT DISTINCT file_hash
FROM segments
WHERE file_path = ?1
LIMIT 1";

pub const SELECT_ALL_FILE_PATHS: &str = "
SELECT DISTINCT file_path FROM segments ORDER BY file_path";

#[allow(dead_code)]
pub const SELECT_SEGMENT_BY_ID: &str = "
SELECT id, file_path, language, block_type, content,
       line_start, line_end, complexity, role,
       defined_symbols, referenced_symbols, file_hash,
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

pub const SELECT_SYMBOLS_BY_DEFINED: &str = "
SELECT id, file_path, language, block_type, content,
       line_start, line_end, complexity, role,
       defined_symbols, referenced_symbols, file_hash,
       created_at, updated_at
FROM segments
WHERE defined_symbols LIKE '%' || ?1 || '%'
ORDER BY
  CASE WHEN block_type IN ('function', 'struct', 'trait', 'class', 'interface', 'type', 'enum') THEN 0 ELSE 1 END,
  file_path";

pub const SELECT_SYMBOLS_BY_REFERENCED: &str = "
SELECT id, file_path, language, block_type, content,
       line_start, line_end, complexity, role,
       defined_symbols, referenced_symbols, file_hash,
       created_at, updated_at
FROM segments
WHERE referenced_symbols LIKE '%' || ?1 || '%'
  AND defined_symbols NOT LIKE '%\"' || ?1 || '\"%'
ORDER BY file_path, line_start";
