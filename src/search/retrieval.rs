use std::sync::atomic::{AtomicBool, Ordering};

use libsql::{Connection, Row};

use crate::search::ranking::tokenize_text;
use crate::search::scope::SearchScope;
use crate::shared::constants::{VECTOR_EXACT_SCAN_WARN_THRESHOLD, VECTOR_PREFILTER_K};
use crate::shared::errors::{OneupError, SearchError};
use crate::shared::types::SegmentRole;
use crate::storage::queries;

#[derive(Debug, Clone, PartialEq)]
pub struct CandidateRow {
    pub segment_id: String,
    pub file_path: String,
    pub language: String,
    pub block_type: String,
    pub line_number: usize,
    pub line_end: usize,
    pub breadcrumb: Option<String>,
    pub complexity: Option<u32>,
    pub role: Option<SegmentRole>,
    pub defined_symbols: Option<Vec<String>>,
    pub referenced_symbols: Option<Vec<String>>,
    pub called_symbols: Option<Vec<String>>,
    pub content: String,
}

impl CandidateRow {
    pub fn line_count(&self) -> usize {
        self.line_end
            .saturating_sub(self.line_number)
            .saturating_add(1)
    }

    pub fn is_definition_like(&self) -> bool {
        if matches!(self.role, Some(SegmentRole::Definition)) {
            return true;
        }

        let has_symbols = self
            .defined_symbols
            .as_ref()
            .map(|symbols| !symbols.is_empty())
            .unwrap_or(false);

        has_symbols
            && matches!(
                self.block_type.as_str(),
                "function"
                    | "method"
                    | "impl"
                    | "struct"
                    | "enum"
                    | "trait"
                    | "type"
                    | "class"
                    | "interface"
                    | "module"
                    | "macro"
                    | "constructor"
            )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(
    dead_code,
    reason = "consumed via the lib target by benches/search_bench.rs"
)]
pub enum RetrievalMode {
    SqlVectorV2,
    FtsOnly,
}

#[allow(
    dead_code,
    reason = "consumed via the lib target by benches/search_bench.rs"
)]
pub struct RetrievedCandidates {
    pub vector_results: Vec<CandidateRow>,
    pub fts_results: Vec<CandidateRow>,
}

#[allow(
    dead_code,
    reason = "consumed via the lib target by benches/search_bench.rs"
)]
pub enum RetrievalBackend<'a> {
    SqlVectorV2(SqlVectorV2<'a>),
    FtsOnly(FtsOnly<'a>),
}

#[allow(
    dead_code,
    reason = "constructed on the bench-kept RetrievalBackend::select_scoped path (benches/search_bench.rs)"
)]
pub struct SqlVectorV2<'a> {
    conn: &'a Connection,
    scope: SearchScope,
}

#[allow(
    dead_code,
    reason = "constructed on the bench-kept RetrievalBackend::select_scoped path (benches/search_bench.rs)"
)]
pub struct FtsOnly<'a> {
    conn: &'a Connection,
    scope: SearchScope,
}

impl<'a> RetrievalBackend<'a> {
    #[allow(
        dead_code,
        reason = "consumed via the lib target by benches/search_bench.rs"
    )]
    pub async fn select(
        conn: &'a Connection,
        query_embedding: Option<&[f32]>,
    ) -> Result<Self, OneupError> {
        Self::select_scoped(conn, query_embedding, SearchScope::default_context()).await
    }

    #[allow(
        dead_code,
        reason = "sole caller is the bench-kept RetrievalBackend::select (benches/search_bench.rs)"
    )]
    pub async fn select_scoped(
        conn: &'a Connection,
        query_embedding: Option<&[f32]>,
        scope: SearchScope,
    ) -> Result<Self, OneupError> {
        if query_embedding.is_some() && has_indexed_embeddings(conn, &scope).await? {
            Ok(Self::SqlVectorV2(SqlVectorV2 { conn, scope }))
        } else {
            Ok(Self::FtsOnly(FtsOnly { conn, scope }))
        }
    }

    #[allow(
        dead_code,
        reason = "consumed via the lib target by benches/search_bench.rs"
    )]
    pub fn mode(&self) -> RetrievalMode {
        match self {
            Self::SqlVectorV2(_) => RetrievalMode::SqlVectorV2,
            Self::FtsOnly(_) => RetrievalMode::FtsOnly,
        }
    }

    #[allow(
        dead_code,
        reason = "consumed via the lib target by benches/search_bench.rs"
    )]
    pub async fn search(
        &self,
        query: &str,
        query_embedding: Option<&[f32]>,
    ) -> Result<RetrievedCandidates, OneupError> {
        match self {
            Self::SqlVectorV2(backend) => {
                backend
                    .search(
                        query,
                        query_embedding.ok_or_else(|| {
                            SearchError::QueryFailed(
                                "vector backend selected without a query embedding".to_string(),
                            )
                        })?,
                    )
                    .await
            }
            Self::FtsOnly(backend) => backend.search(query).await,
        }
    }
}

impl<'a> SqlVectorV2<'a> {
    #[allow(
        dead_code,
        reason = "dispatched from the bench-kept RetrievalBackend::search (benches/search_bench.rs)"
    )]
    async fn search(
        &self,
        query: &str,
        query_embedding: &[f32],
    ) -> Result<RetrievedCandidates, OneupError> {
        let (vector_results, fts_results) = tokio::try_join!(
            fetch_vector_candidates(self.conn, &self.scope, query_embedding),
            fetch_fts_candidates(self.conn, &self.scope, query),
        )?;

        Ok(RetrievedCandidates {
            vector_results,
            fts_results,
        })
    }
}

impl<'a> FtsOnly<'a> {
    #[allow(
        dead_code,
        reason = "dispatched from the bench-kept RetrievalBackend::search (benches/search_bench.rs)"
    )]
    async fn search(&self, query: &str) -> Result<RetrievedCandidates, OneupError> {
        Ok(RetrievedCandidates {
            vector_results: Vec::new(),
            fts_results: fetch_fts_candidates(self.conn, &self.scope, query).await?,
        })
    }
}

pub(crate) async fn has_indexed_embeddings(
    conn: &Connection,
    scope: &SearchScope,
) -> Result<bool, OneupError> {
    let mut rows = conn
        .query(
            queries::SELECT_HAS_INDEXED_EMBEDDINGS_FOR_CONTEXT,
            [scope.context_id()],
        )
        .await
        .map_err(|e| {
            SearchError::QueryFailed(format!("failed to inspect indexed embeddings: {e}"))
        })?;

    match rows.next().await {
        Ok(Some(_)) => Ok(true),
        Ok(None) => Ok(false),
        Err(e) => Err(SearchError::QueryFailed(format!(
            "indexed-embedding inspection failed: {e}"
        ))
        .into()),
    }
}

/// Emits a single process-wide `tracing::warn!` the first time a context's
/// vector count exceeds [`VECTOR_EXACT_SCAN_WARN_THRESHOLD`]. Above this
/// (deliberately high) bound the exact scan — the only vector path — stays
/// correct, but its latency grows linearly with corpus size, so the operator
/// gets one heads-up rather than a per-query log flood.
fn warn_once_above_exact_scan_threshold(vector_count: usize) {
    static WARNED: AtomicBool = AtomicBool::new(false);

    if vector_count > VECTOR_EXACT_SCAN_WARN_THRESHOLD
        && WARNED
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    {
        tracing::warn!(
            "vector corpus has {vector_count} vectors, above the {VECTOR_EXACT_SCAN_WARN_THRESHOLD} warn threshold; \
             the exact vector scan stays correct but its latency grows linearly with corpus size"
        );
    }
}

#[allow(
    dead_code,
    reason = "called by the bench-kept SqlVectorV2::search (benches/search_bench.rs)"
)]
pub(crate) async fn fetch_vector_candidates(
    conn: &Connection,
    scope: &SearchScope,
    query_embedding: &[f32],
) -> Result<Vec<CandidateRow>, OneupError> {
    fetch_vector_candidates_with_count(conn, scope, query_embedding, None).await
}

/// Like [`fetch_vector_candidates`], but accepts a pre-computed per-context
/// vector count so a caller that already knows it (the daemon caches it on
/// `ProjectState`, invalidated on index swap) can skip the per-query
/// `COUNT(*)`. The count feeds only the one-time large-corpus latency warning
/// and the debug line below — never the served results — and the cached value
/// MUST equal the live `COUNT(*)` for the open index; passing `None` falls
/// back to the live count.
pub(crate) async fn fetch_vector_candidates_with_count(
    conn: &Connection,
    scope: &SearchScope,
    query_embedding: &[f32],
    cached_count: Option<usize>,
) -> Result<Vec<CandidateRow>, OneupError> {
    let started = std::time::Instant::now();
    let query_embedding = serialize_query_embedding(query_embedding)?;

    // A `path_prefix` scope skips the count: the scoped scan is already
    // bounded by the prefix filter, and the warning is about whole-context
    // corpus growth.
    let vector_count = if scope.path_prefix().is_some() {
        None
    } else {
        let count = match cached_count {
            Some(count) => count,
            None => count_vector_rows_for_context(conn, scope).await?,
        };
        warn_once_above_exact_scan_threshold(count);
        Some(count)
    };

    let results = fetch_vector_candidates_exhaustive(conn, scope, &query_embedding).await?;

    tracing::debug!(
        "vector stage: exact scan over {vector_count:?} context vectors (path_prefix={:?}) returned {} candidates in {:?}",
        scope.path_prefix(),
        results.len(),
        started.elapsed()
    );

    Ok(results)
}

async fn fetch_vector_candidates_exhaustive(
    conn: &Connection,
    scope: &SearchScope,
    serialized_embedding: &str,
) -> Result<Vec<CandidateRow>, OneupError> {
    let mut rows = match scope.path_prefix_like_pattern() {
        Some(pattern) => {
            conn.query(
                queries::SELECT_VECTOR_CANDIDATES_EXHAUSTIVE_FOR_CONTEXT_SCOPED,
                libsql::params![
                    serialized_embedding,
                    scope.context_id(),
                    VECTOR_PREFILTER_K as i64,
                    pattern
                ],
            )
            .await
        }
        None => {
            conn.query(
                queries::SELECT_VECTOR_CANDIDATES_EXHAUSTIVE_FOR_CONTEXT,
                libsql::params![
                    serialized_embedding,
                    scope.context_id(),
                    VECTOR_PREFILTER_K as i64
                ],
            )
            .await
        }
    }
    .map_err(|e| SearchError::QueryFailed(format!("vector exhaustive scan: {e}")))?;

    collect_candidate_rows(&mut rows, "vector exhaustive scan row iteration").await
}

async fn collect_candidate_rows(
    rows: &mut libsql::Rows,
    iteration_context: &str,
) -> Result<Vec<CandidateRow>, OneupError> {
    let mut results = Vec::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|e| SearchError::QueryFailed(format!("{iteration_context}: {e}")))?
    {
        results.push(row_to_candidate_row(&row)?);
    }

    Ok(results)
}

pub(crate) async fn count_vector_rows_for_context(
    conn: &Connection,
    scope: &SearchScope,
) -> Result<usize, OneupError> {
    let mut rows = conn
        .query(queries::COUNT_VECTOR_ROWS_FOR_CONTEXT, [scope.context_id()])
        .await
        .map_err(|e| {
            SearchError::QueryFailed(format!("failed to count context vector rows: {e}"))
        })?;

    match rows.next().await {
        Ok(Some(row)) => {
            let count: i64 = row.get(0).map_err(|e| {
                SearchError::QueryFailed(format!("read context vector count failed: {e}"))
            })?;
            Ok(usize::try_from(count.max(0)).unwrap_or(usize::MAX))
        }
        Ok(None) => Ok(0),
        Err(e) => Err(SearchError::QueryFailed(format!(
            "context vector count iteration failed: {e}"
        ))
        .into()),
    }
}

pub(crate) async fn fetch_fts_candidates(
    conn: &Connection,
    scope: &SearchScope,
    query: &str,
) -> Result<Vec<CandidateRow>, OneupError> {
    let fts_query = build_fts_query(query);
    if fts_query.is_empty() {
        return Ok(Vec::new());
    }

    let mut rows = match scope.path_prefix_like_pattern() {
        Some(pattern) => {
            conn.query(
                queries::SELECT_FTS_CANDIDATES_FOR_CONTEXT_SCOPED,
                libsql::params![
                    fts_query,
                    scope.context_id(),
                    VECTOR_PREFILTER_K as i64,
                    pattern
                ],
            )
            .await
        }
        None => {
            conn.query(
                queries::SELECT_FTS_CANDIDATES_FOR_CONTEXT,
                libsql::params![fts_query, scope.context_id(), VECTOR_PREFILTER_K as i64],
            )
            .await
        }
    }
    .map_err(|e| SearchError::QueryFailed(format!("FTS search: {e}")))?;

    collect_candidate_rows(&mut rows, "FTS row iteration").await
}

fn serialize_query_embedding(query_embedding: &[f32]) -> Result<String, OneupError> {
    serde_json::to_string(query_embedding)
        .map_err(|e| SearchError::QueryFailed(format!("serialize query embedding: {e}")).into())
}

fn row_to_candidate_row(row: &Row) -> Result<CandidateRow, OneupError> {
    let segment_id: String = row
        .get(0)
        .map_err(|e| SearchError::QueryFailed(e.to_string()))?;
    let file_path: String = row
        .get(1)
        .map_err(|e| SearchError::QueryFailed(e.to_string()))?;
    let language: String = row
        .get(2)
        .map_err(|e| SearchError::QueryFailed(e.to_string()))?;
    let block_type: String = row
        .get(3)
        .map_err(|e| SearchError::QueryFailed(e.to_string()))?;
    let line_start: i64 = row
        .get(4)
        .map_err(|e| SearchError::QueryFailed(e.to_string()))?;
    let line_end: i64 = row
        .get(5)
        .map_err(|e| SearchError::QueryFailed(e.to_string()))?;
    let breadcrumb: Option<String> = row
        .get(6)
        .map_err(|e| SearchError::QueryFailed(e.to_string()))?;
    let complexity: i64 = row
        .get(7)
        .map_err(|e| SearchError::QueryFailed(e.to_string()))?;
    let role_str: String = row
        .get(8)
        .map_err(|e| SearchError::QueryFailed(e.to_string()))?;
    let defined_symbols: String = row
        .get(9)
        .map_err(|e| SearchError::QueryFailed(e.to_string()))?;
    let referenced_symbols: String = row
        .get(10)
        .map_err(|e| SearchError::QueryFailed(e.to_string()))?;
    let called_symbols: String = row
        .get(11)
        .map_err(|e| SearchError::QueryFailed(e.to_string()))?;
    let content: String = row
        .get(12)
        .map_err(|e| SearchError::QueryFailed(e.to_string()))?;

    let role = parse_role(&role_str);
    let def_syms: Vec<String> = serde_json::from_str(&defined_symbols).unwrap_or_default();
    let ref_syms: Vec<String> = serde_json::from_str(&referenced_symbols).unwrap_or_default();
    let call_syms: Vec<String> = serde_json::from_str(&called_symbols).unwrap_or_default();

    Ok(CandidateRow {
        segment_id,
        file_path,
        language,
        block_type,
        line_number: line_start as usize,
        line_end: line_end as usize,
        breadcrumb,
        complexity: Some(complexity as u32),
        role,
        defined_symbols: some_if_not_empty(def_syms),
        referenced_symbols: some_if_not_empty(ref_syms),
        called_symbols: some_if_not_empty(call_syms),
        content,
    })
}

fn parse_role(role_str: &str) -> Option<SegmentRole> {
    match role_str {
        "DEFINITION" => Some(SegmentRole::Definition),
        "IMPLEMENTATION" => Some(SegmentRole::Implementation),
        "ORCHESTRATION" => Some(SegmentRole::Orchestration),
        "IMPORT" => Some(SegmentRole::Import),
        "DOCS" => Some(SegmentRole::Docs),
        _ => None,
    }
}

fn some_if_not_empty(values: Vec<String>) -> Option<Vec<String>> {
    if values.is_empty() {
        None
    } else {
        Some(values)
    }
}

/// Maximum identifier-split and prefix variants appended to the base FTS
/// terms. Bounds query fan-out so identifier-aware matching cannot regress
/// FTS latency on long natural-language queries.
const MAX_FTS_VARIANT_TERMS: usize = 16;

/// Minimum cleaned-term length for emitting a prefix variant. Prefix terms
/// let a plain query word reach the front of a concatenated identifier token
/// (`impact` -> `impacthorizonengine`); shorter prefixes pull in too much
/// noise for the recall they add.
const MIN_FTS_PREFIX_TERM_CHARS: usize = 4;

/// Minimum plain-word length for emitting a stem-prefix variant. Shorter
/// words rarely carry the inflectional suffixes this targets, and their
/// stems are too short to stay selective as prefixes.
const MIN_FTS_STEM_TERM_CHARS: usize = 5;

/// Common English inflection suffixes stripped to form stem-prefix variants,
/// checked in order with first match winning when the remaining stem is long
/// enough.
const FTS_STEM_SUFFIXES: &[&str] = &["ed", "ing", "es", "s"];

/// Returns the stem-prefix variant for a plain query word: `composed` yields
/// `compos` (reaching the indexed token `compose`) and `embeddings` yields
/// `embedding`. Only alphabetic words of at least [`MIN_FTS_STEM_TERM_CHARS`]
/// stem, and the stem must keep [`MIN_FTS_PREFIX_TERM_CHARS`] so it stays a
/// selective prefix.
fn stem_prefix(term: &str) -> Option<String> {
    if term.chars().count() < MIN_FTS_STEM_TERM_CHARS
        || !term.chars().all(|c| c.is_ascii_alphabetic())
    {
        return None;
    }

    let lower = term.to_lowercase();
    FTS_STEM_SUFFIXES.iter().find_map(|suffix| {
        let stem = lower.strip_suffix(suffix)?;
        (stem.len() >= MIN_FTS_PREFIX_TERM_CHARS).then(|| stem.to_string())
    })
}

/// Builds an FTS5 match expression with identifier-aware variants.
///
/// The unicode61 tokenizer indexes `ImpactHorizonEngine` as the single token
/// `impacthorizonengine`, which plain quoted query terms never match. On top
/// of the base quoted terms this adds: a split-part phrase variant for
/// CamelCase/snake_case query terms (matching identifiers mentioned as
/// prose), bounded prefix variants for plain words so they can match the
/// head of concatenated identifiers, and stem-prefix variants for inflected
/// plain words (`composed` -> `compos`*) so natural-language inflections
/// reach stem-named identifiers. Split parts stay phrase-bound rather than
/// OR'd individually so exact-identifier queries keep their precision.
fn build_fts_query(query: &str) -> String {
    let cleaned_terms: Vec<String> = query
        .split_whitespace()
        .filter(|term| term.len() >= 2)
        .map(|term| {
            term.chars()
                .filter(|c| c.is_alphanumeric() || *c == '_')
                .collect::<String>()
        })
        .filter(|term| !term.is_empty())
        .collect();

    if cleaned_terms.is_empty() {
        return String::new();
    }

    let mut units: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for term in &cleaned_terms {
        if seen.insert(term.to_lowercase()) {
            units.push(format!("\"{term}\""));
        }
    }

    let mut variant_count = 0;
    for term in &cleaned_terms {
        if variant_count >= MAX_FTS_VARIANT_TERMS {
            break;
        }

        let parts = tokenize_text(term);
        if parts.len() > 1 {
            let phrase = parts.join(" ");
            if seen.insert(phrase.clone()) {
                units.push(format!("\"{phrase}\""));
                variant_count += 1;
            }
            continue;
        }

        if term.len() >= MIN_FTS_PREFIX_TERM_CHARS
            && seen.insert(format!("{}*", term.to_lowercase()))
        {
            units.push(format!("\"{term}\" *"));
            variant_count += 1;
        }

        if variant_count >= MAX_FTS_VARIANT_TERMS {
            break;
        }

        if let Some(stem) = stem_prefix(term) {
            if seen.insert(format!("{stem}*")) {
                units.push(format!("\"{stem}\" *"));
                variant_count += 1;
            }
        }
    }

    units.join(" OR ")
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::storage::db::Db;
    use crate::storage::schema;
    use sha2::{Digest, Sha256};

    fn embedding_with(values: &[(usize, f32)]) -> Vec<f32> {
        let mut embedding = vec![0.0; 384];
        for (idx, value) in values {
            embedding[*idx] = *value;
        }
        embedding
    }

    /// Content-addressed key for a serialized embedding, mirroring the production
    /// pool write: identical vectors hash to one key and therefore collapse to a
    /// single `embedding_pool` row, so seeding the same embedding for two segments
    /// faithfully reproduces cross-segment sharing.
    fn test_content_key(serialized_embedding: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(serialized_embedding.as_bytes());
        let hash = hasher.finalize();
        hash.iter().map(|b| format!("{b:02x}")).collect()
    }

    async fn insert_segment(
        conn: &Connection,
        id: &str,
        file_path: &str,
        content: &str,
        embedding: Option<&[f32]>,
    ) {
        match embedding {
            Some(embedding) => {
                let embedding = serialize_query_embedding(embedding).unwrap();
                conn.execute(
                    "INSERT INTO segments (
                        id, file_path, language, block_type, content,
                        line_start, line_end, breadcrumb, complexity,
                        role, defined_symbols, referenced_symbols, called_symbols,
                        file_hash, created_at, updated_at
                    ) VALUES (
                        ?1, ?2, 'rust', 'function', ?3,
                        1, 3, NULL, 1,
                        'DEFINITION', '[]', '[]', '[]',
                        ?4, datetime('now'), datetime('now')
                    )",
                    libsql::params![id, file_path, content, format!("hash-{id}")],
                )
                .await
                .unwrap();
                let content_key = test_content_key(&embedding);
                conn.execute(
                    "INSERT INTO embedding_pool (content_key, embedding_vec, ref_count)
                     VALUES (?1, vector8(?2), 1)
                     ON CONFLICT(content_key) DO UPDATE SET ref_count = ref_count + 1",
                    libsql::params![content_key.clone(), embedding],
                )
                .await
                .unwrap();
                conn.execute(
                    "INSERT INTO segment_vectors (segment_id, content_key) VALUES (?1, ?2)",
                    libsql::params![id, content_key],
                )
                .await
                .unwrap();
            }
            None => {
                conn.execute(
                    "INSERT INTO segments (
                        id, file_path, language, block_type, content,
                        line_start, line_end, breadcrumb, complexity,
                        role, defined_symbols, referenced_symbols, called_symbols,
                        file_hash, created_at, updated_at
                    ) VALUES (
                        ?1, ?2, 'rust', 'function', ?3,
                        1, 3, NULL, 1,
                        'DEFINITION', '[]', '[]', '[]',
                        ?4, datetime('now'), datetime('now')
                    )",
                    libsql::params![id, file_path, content, format!("hash-{id}")],
                )
                .await
                .unwrap();
            }
        }
    }

    async fn setup() -> Connection {
        let db = Db::open_memory().await.unwrap();
        let conn = db.connect().unwrap();
        schema::initialize(&conn).await.unwrap();
        conn
    }

    #[test]
    fn fts_query_building() {
        let query = build_fts_query("error handling network");
        assert!(query.contains("\"error\""));
        assert!(query.contains("\"handling\""));
        assert!(query.contains("\"network\""));
        assert!(query.contains(" OR "));
    }

    #[test]
    fn fts_query_skips_short_terms() {
        let query = build_fts_query("a is the error");
        assert!(query.contains("\"is\""));
        assert!(query.contains("\"the\""));
        assert!(query.contains("\"error\""));
        assert!(!query.contains("\"a\""));
    }

    #[test]
    fn fts_query_adds_prefix_variants_for_plain_words() {
        let query = build_fts_query("impact horizon");

        assert!(query.contains("\"impact\""));
        assert!(query.contains("\"impact\" *"));
        assert!(query.contains("\"horizon\" *"));
    }

    #[test]
    fn fts_query_splits_identifier_terms_into_phrase_variants() {
        let query = build_fts_query("ImpactHorizonEngine summary_json");

        assert!(query.contains("\"ImpactHorizonEngine\""));
        assert!(query.contains("\"impact horizon engine\""));
        assert!(query.contains("\"summary_json\""));
        assert!(query.contains("\"summary json\""));
    }

    #[test]
    fn fts_query_adds_stem_prefix_variants_for_inflected_words() {
        let query = build_fts_query("embeddings composed indexing");

        assert!(query.contains("\"embedding\" *"), "missing stem in {query}");
        assert!(query.contains("\"compos\" *"), "missing stem in {query}");
        assert!(query.contains("\"index\" *"), "missing stem in {query}");
        assert!(
            query.contains("\"embeddings\" *"),
            "base prefix variant should stay alongside the stem: {query}"
        );
    }

    #[test]
    fn fts_query_skips_stems_for_short_or_identifier_words() {
        let query = build_fts_query("goes summary_json table");

        assert!(
            !query.contains("\"go\" *"),
            "short words must not stem: {query}"
        );
        assert!(
            !query.contains("\"goe\" *"),
            "short words must not stem: {query}"
        );
        assert!(query.contains("\"summary json\""));
        assert!(
            !query.contains("\"tabl\" *"),
            "suffix-free words must not stem: {query}"
        );
    }

    #[tokio::test]
    async fn fts_matches_snake_case_identifier_from_inflected_query() {
        let conn = setup().await;
        insert_segment(
            &conn,
            "seg-compose-embedding",
            "src/indexer/pipeline.rs",
            "fn compose_embedding_text(relative_path: &str) -> String {\n    relative_path.to_string()\n}",
            None,
        )
        .await;

        let candidates = fetch_fts_candidates(
            &conn,
            &SearchScope::default_context(),
            "where are embeddings composed before indexing",
        )
        .await
        .unwrap();

        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.segment_id == "seg-compose-embedding"),
            "inflected query words should reach snake_case tokens via stem-prefix variants"
        );
    }

    #[test]
    fn fts_query_caps_added_variants() {
        let long_query = (0..40)
            .map(|i| format!("verylongword{i:02}"))
            .collect::<Vec<_>>()
            .join(" ");

        let query = build_fts_query(&long_query);

        assert_eq!(query.matches('*').count(), MAX_FTS_VARIANT_TERMS);
    }

    #[tokio::test]
    async fn fts_matches_camelcase_identifier_from_conceptual_query() {
        let conn = setup().await;
        insert_segment(
            &conn,
            "seg-impact-engine",
            "src/search/impact.rs",
            "pub struct ImpactHorizonEngine<'a> {\n    conn: &'a Connection,\n}",
            None,
        )
        .await;

        let candidates = fetch_fts_candidates(
            &conn,
            &SearchScope::default_context(),
            "impact horizon expansion with owner aware corroboration",
        )
        .await
        .unwrap();

        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.segment_id == "seg-impact-engine"),
            "conceptual terms should reach the CamelCase token via prefix variants"
        );
    }

    #[tokio::test]
    async fn backend_selection_uses_fts_only_without_indexed_embeddings() {
        let conn = setup().await;
        let query_embedding = embedding_with(&[(0, 1.0)]);

        let backend = RetrievalBackend::select(&conn, Some(&query_embedding))
            .await
            .unwrap();

        assert_eq!(backend.mode(), RetrievalMode::FtsOnly);
    }

    #[tokio::test]
    async fn backend_selection_uses_sql_vector_v2_when_embeddings_exist() {
        let conn = setup().await;
        let query_embedding = embedding_with(&[(0, 1.0)]);
        insert_segment(
            &conn,
            "seg-1",
            "src/main.rs",
            "fn config_loader() -> String { \"config\".to_string() }",
            Some(&query_embedding),
        )
        .await;

        let backend = RetrievalBackend::select(&conn, Some(&query_embedding))
            .await
            .unwrap();

        assert_eq!(backend.mode(), RetrievalMode::SqlVectorV2);
    }

    #[tokio::test]
    async fn sql_vector_backend_preserves_candidate_order() {
        let conn = setup().await;
        let query_embedding = embedding_with(&[(0, 1.0)]);
        let near_embedding = embedding_with(&[(0, 0.95), (1, 0.05)]);
        let far_embedding = embedding_with(&[(1, 1.0)]);

        insert_segment(
            &conn,
            "seg-near",
            "src/config.rs",
            "fn config_loader() -> String { \"config\".to_string() }",
            Some(&near_embedding),
        )
        .await;
        insert_segment(
            &conn,
            "seg-far",
            "src/network.rs",
            "fn network_loader() -> String { \"network\".to_string() }",
            Some(&far_embedding),
        )
        .await;

        let backend = RetrievalBackend::select(&conn, Some(&query_embedding))
            .await
            .unwrap();
        let candidates = backend
            .search("config loader", Some(&query_embedding))
            .await
            .unwrap();

        assert_eq!(backend.mode(), RetrievalMode::SqlVectorV2);
        assert_eq!(candidates.vector_results.len(), 2);
        assert_eq!(candidates.vector_results[0].file_path, "src/config.rs");
        assert_eq!(candidates.vector_results[1].file_path, "src/network.rs");
        assert_eq!(candidates.vector_results[0].line_count(), 3);
        assert!(!candidates.fts_results.is_empty());
    }

    #[tokio::test]
    async fn fts_only_backend_returns_fts_candidates() {
        let conn = setup().await;
        insert_segment(
            &conn,
            "seg-fts",
            "src/lib.rs",
            "fn handle_error() { eprintln!(\"error occurred\"); }",
            None,
        )
        .await;

        let backend = RetrievalBackend::select(&conn, None).await.unwrap();
        let candidates = backend.search("error", None).await.unwrap();

        assert_eq!(backend.mode(), RetrievalMode::FtsOnly);
        assert!(candidates.vector_results.is_empty());
        assert_eq!(candidates.fts_results[0].file_path, "src/lib.rs");
    }

    #[tokio::test]
    async fn vector_backend_ignores_fts_only_segments() {
        let conn = setup().await;
        let query_embedding = embedding_with(&[(0, 1.0)]);

        insert_segment(
            &conn,
            "seg-vector",
            "src/semantic.rs",
            "fn semantic_match() -> &'static str { \"config\" }",
            Some(&query_embedding),
        )
        .await;
        insert_segment(
            &conn,
            "seg-fts-only",
            "config/settings.ini",
            "config = enabled\nmode = strict",
            None,
        )
        .await;

        let backend = RetrievalBackend::select(&conn, Some(&query_embedding))
            .await
            .unwrap();
        let candidates = backend
            .search("config", Some(&query_embedding))
            .await
            .unwrap();

        assert_eq!(backend.mode(), RetrievalMode::SqlVectorV2);
        assert_eq!(candidates.vector_results.len(), 1);
        assert_eq!(candidates.vector_results[0].file_path, "src/semantic.rs");
        assert!(candidates
            .fts_results
            .iter()
            .any(|result| result.file_path == "config/settings.ini"));
    }

    #[tokio::test]
    async fn exhaustive_scan_ranks_nearest_vector_first() {
        let conn = setup().await;

        // Ten one-hot segments in distinct dimensions so cosine similarity can separate them.
        for i in 0..10 {
            let embedding = embedding_with(&[(i, 1.0)]);
            insert_segment(
                &conn,
                &format!("seg-{i}"),
                &format!("src/file_{i}.rs"),
                &format!("fn item_{i}() {{ }}"),
                Some(&embedding),
            )
            .await;
        }

        // Ten vectors sit far below the exhaustive-scan threshold, so this
        // exercises the full-scan path end to end. seg-3 must rank top-1.
        let query_embedding = embedding_with(&[(3, 0.95), (4, 0.05)]);
        let candidates =
            fetch_vector_candidates(&conn, &SearchScope::default_context(), &query_embedding)
                .await
                .unwrap();

        assert!(!candidates.is_empty(), "exhaustive scan returned no rows");
        assert_eq!(candidates[0].segment_id, "seg-3");
        assert_eq!(candidates.len(), 10);
    }

    // TDD: a cached per-context vector count threaded into
    // the vector stage MUST return byte-identical candidates to the live
    // `COUNT(*)`. This pins the "cached count is a pure optimization, never a
    // behavior change" contract: the daemon caches the count on
    // `ProjectState` and the served results must not depend on whether the
    // count came from the cache or a live query.
    #[tokio::test]
    async fn cached_count_yields_identical_candidates_to_live_count() {
        let conn = setup().await;

        for i in 0..10 {
            let embedding = embedding_with(&[(i, 1.0)]);
            insert_segment(
                &conn,
                &format!("seg-{i}"),
                &format!("src/file_{i}.rs"),
                &format!("fn item_{i}() {{ }}"),
                Some(&embedding),
            )
            .await;
        }

        let scope = SearchScope::default_context();
        let query_embedding = embedding_with(&[(3, 0.95), (4, 0.05)]);

        let live = count_vector_rows_for_context(&conn, &scope).await.unwrap();
        assert_eq!(live, 10, "fixture seeds ten context vectors");

        let with_live = fetch_vector_candidates_with_count(&conn, &scope, &query_embedding, None)
            .await
            .unwrap();
        let with_cached =
            fetch_vector_candidates_with_count(&conn, &scope, &query_embedding, Some(live))
                .await
                .unwrap();

        assert_eq!(
            with_cached, with_live,
            "cached count must produce byte-identical candidates to the live count"
        );
        assert_eq!(with_cached[0].segment_id, "seg-3");
    }

    // TDD: under pooling, one pool row can back many
    // `segment_vectors` references, so the exhaustive query must fan out to
    // every reference sharing a `content_key`. The expected ascending-id
    // ordering documents the `ORDER BY ..., s.id` tiebreak contract, which
    // guarantees determinism independent of query plan.
    #[tokio::test]
    async fn exhaustive_scan_fans_out_shared_pool_row_with_deterministic_tiebreak() {
        let conn = setup().await;

        // Five segments share one embedding -> a single pool row with five
        // references. Insert in descending id order to defeat any natural
        // insertion-order ranking.
        let shared = embedding_with(&[(0, 1.0)]);
        for id in ["seg-e", "seg-d", "seg-c", "seg-b", "seg-a"] {
            insert_segment(
                &conn,
                id,
                &format!("src/{id}.rs"),
                "fn shared() {}",
                Some(&shared),
            )
            .await;
        }
        // A distinct, far vector that must not interleave with the shared set.
        let far = embedding_with(&[(7, 1.0)]);
        insert_segment(&conn, "seg-z", "src/z.rs", "fn z() {}", Some(&far)).await;

        // The shared content collapses to exactly one pool row (the fan-out
        // source); the far vector adds the second.
        let pool_rows: i64 = {
            let mut rows = conn
                .query("SELECT COUNT(*) FROM embedding_pool", ())
                .await
                .unwrap();
            rows.next().await.unwrap().unwrap().get(0).unwrap()
        };
        assert_eq!(pool_rows, 2, "identical content must share one pool row");

        let query = serialize_query_embedding(&shared).unwrap();
        let candidates =
            fetch_vector_candidates_exhaustive(&conn, &SearchScope::default_context(), &query)
                .await
                .unwrap();

        // One pool row fanned out to all five referencing segments, ordered by
        // ascending segment id (not the descending insertion order).
        let shared_ids: Vec<&str> = candidates
            .iter()
            .map(|c| c.segment_id.as_str())
            .filter(|id| id.starts_with("seg-") && *id != "seg-z")
            .collect();
        assert_eq!(
            shared_ids,
            vec!["seg-a", "seg-b", "seg-c", "seg-d", "seg-e"],
            "the shared pool vector must fan out to every reference, tie-broken by ascending segment id"
        );
    }

    #[tokio::test]
    async fn exhaustive_scan_respects_context_scope() {
        let conn = setup().await;
        let embedding = embedding_with(&[(0, 1.0)]);
        let serialized = serialize_query_embedding(&embedding).unwrap();

        insert_segment(
            &conn,
            "seg-default",
            "src/default.rs",
            "fn default_context_item() {}",
            Some(&embedding),
        )
        .await;
        conn.execute(
            "UPDATE segments SET context_id = 'ctx-other' WHERE id = 'seg-default'",
            (),
        )
        .await
        .unwrap();
        insert_segment(
            &conn,
            "seg-active",
            "src/active.rs",
            "fn active_context_item() {}",
            Some(&embedding),
        )
        .await;

        let candidates =
            fetch_vector_candidates_exhaustive(&conn, &SearchScope::default_context(), &serialized)
                .await
                .unwrap();

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].segment_id, "seg-active");
    }

    // TDD: the exhaustive vector path must apply the
    // `path_prefix` directory-boundary filter so `src/foo` matches itself and
    // its descendants but not a sibling directory that merely shares the
    // prefix as a string (`src/foobar`).
    #[tokio::test]
    async fn exhaustive_scan_respects_path_prefix_boundary() {
        let conn = setup().await;
        let embedding = embedding_with(&[(0, 1.0)]);
        let serialized = serialize_query_embedding(&embedding).unwrap();

        insert_segment(
            &conn,
            "seg-exact",
            "src/foo",
            "fn exact_prefix_file() {}",
            Some(&embedding),
        )
        .await;
        insert_segment(
            &conn,
            "seg-descendant",
            "src/foo/a.rs",
            "fn descendant_item() {}",
            Some(&embedding),
        )
        .await;
        insert_segment(
            &conn,
            "seg-sibling",
            "src/foobar/a.rs",
            "fn sibling_item() {}",
            Some(&embedding),
        )
        .await;

        let scope = SearchScope::default_context().with_path_prefix("src/foo");
        let candidates = fetch_vector_candidates_exhaustive(&conn, &scope, &serialized)
            .await
            .unwrap();

        let ids: Vec<&str> = candidates.iter().map(|c| c.segment_id.as_str()).collect();
        assert!(ids.contains(&"seg-exact"), "the prefix itself must match");
        assert!(ids.contains(&"seg-descendant"), "descendants must match");
        assert!(
            !ids.contains(&"seg-sibling"),
            "src/foo must not match src/foobar"
        );
        assert_eq!(candidates.len(), 2);
    }

    // TDD: the FTS candidate query must apply the same
    // directory-boundary `path_prefix` filter as the vector stage.
    #[tokio::test]
    async fn fts_candidates_respect_path_prefix_boundary() {
        let conn = setup().await;

        insert_segment(
            &conn,
            "seg-in-scope",
            "src/foo/a.rs",
            "fn widgetsearch() {}",
            None,
        )
        .await;
        insert_segment(
            &conn,
            "seg-sibling",
            "src/foobar/a.rs",
            "fn widgetsearch() {}",
            None,
        )
        .await;

        let scope = SearchScope::default_context().with_path_prefix("src/foo");
        let candidates = fetch_fts_candidates(&conn, &scope, "widgetsearch")
            .await
            .unwrap();

        let ids: Vec<&str> = candidates.iter().map(|c| c.segment_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["seg-in-scope"],
            "src/foo must not match src/foobar"
        );
    }

    // TDD: an unset `path_prefix` must leave full-repo
    // search behavior unchanged.
    #[tokio::test]
    async fn no_path_prefix_leaves_full_repo_behavior_unchanged() {
        let conn = setup().await;

        insert_segment(
            &conn,
            "seg-in-scope",
            "src/foo/a.rs",
            "fn widgetsearch() {}",
            None,
        )
        .await;
        insert_segment(
            &conn,
            "seg-sibling",
            "src/foobar/a.rs",
            "fn widgetsearch() {}",
            None,
        )
        .await;

        let candidates =
            fetch_fts_candidates(&conn, &SearchScope::default_context(), "widgetsearch")
                .await
                .unwrap();

        assert_eq!(candidates.len(), 2, "no prefix must return every match");
    }
}
