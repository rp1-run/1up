use libsql::Connection;

use crate::indexer::embedder::Embedder;
use crate::search::intent::detect_intent;
use crate::search::intent::QueryIntent;
use crate::search::ranking::{rank_candidates, tokenize_text, RankedCandidate};
use crate::search::retrieval::{self, CandidateRow};
use crate::search::scope::SearchScope;
use crate::search::symbol::SymbolSearchEngine;
use crate::shared::errors::{OneupError, SearchError};
use crate::shared::types::{normalize_score, SearchResult};

pub struct HybridSearchEngine<'a> {
    conn: &'a Connection,
    embedder: Option<&'a mut Embedder>,
    scope: SearchScope,
}

impl<'a> HybridSearchEngine<'a> {
    #[allow(dead_code)]
    pub fn new(conn: &'a Connection, embedder: Option<&'a mut Embedder>) -> Self {
        Self::new_scoped(conn, embedder, SearchScope::default_context())
    }

    pub fn new_scoped(
        conn: &'a Connection,
        embedder: Option<&'a mut Embedder>,
        scope: SearchScope,
    ) -> Self {
        Self {
            conn,
            embedder,
            scope,
        }
    }

    /// Hybrid search with lazy query embedding: the lexical stages, the
    /// exact-lexical short-circuit, and the cheap vector-presence probe all
    /// run before the query is ever embedded, so an index without vector rows
    /// never exercises the embedder.
    pub async fn search(
        &mut self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchResult>, OneupError> {
        if query.trim().is_empty() {
            return Err(SearchError::InvalidQuery("empty query".to_string()).into());
        }

        let intent = detect_intent(query);
        let symbol_results = symbol_search(self.conn, &self.scope, query, intent).await?;
        let fts_results = retrieval::fetch_fts_candidates(self.conn, &self.scope, query).await?;

        let should_fetch_vector = self.embedder.is_some()
            && !is_exact_lexical_hit(query, &symbol_results, &fts_results)
            && retrieval::has_indexed_embeddings(self.conn, &self.scope).await?;
        let vector_results = if should_fetch_vector {
            match self.embed_query(query) {
                Some(embedding) => {
                    fetch_vector_candidates_with_degrade(self.conn, &self.scope, &embedding).await
                }
                None => Vec::new(),
            }
        } else {
            Vec::new()
        };

        rank_and_hydrate(
            vector_results,
            fts_results,
            symbol_results,
            query,
            intent,
            limit,
        )
    }

    pub async fn fts_only_search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchResult>, OneupError> {
        if query.trim().is_empty() {
            return Err(SearchError::InvalidQuery("empty query".to_string()).into());
        }

        let intent = detect_intent(query);
        let symbol_results = symbol_search(self.conn, &self.scope, query, intent).await?;
        let fts_results = retrieval::fetch_fts_candidates(self.conn, &self.scope, query).await?;

        rank_and_hydrate(
            Vec::new(),
            fts_results,
            symbol_results,
            query,
            intent,
            limit,
        )
    }

    fn embed_query(&mut self, query: &str) -> Option<Vec<f32>> {
        let embedder = self.embedder.as_deref_mut()?;
        match embedder.embed_one(query) {
            Ok(embedding) => Some(embedding),
            Err(err) => {
                eprintln!(
                    "warning: semantic query embedding failed ({err}); search is degraded to FTS-only mode for this query"
                );
                tracing::debug!("semantic query embedding failed: {err}");
                None
            }
        }
    }
}

async fn fetch_vector_candidates_with_degrade(
    conn: &Connection,
    scope: &SearchScope,
    query_embedding: &[f32],
) -> Vec<CandidateRow> {
    match retrieval::fetch_vector_candidates(conn, scope, query_embedding).await {
        Ok(results) => results,
        Err(err) => {
            eprintln!(
                "warning: vector retrieval failed ({err}); search is degraded to FTS-only mode for this query"
            );
            tracing::debug!("vector retrieval failed: {err}");
            Vec::new()
        }
    }
}

fn rank_and_hydrate(
    vector_results: Vec<CandidateRow>,
    fts_results: Vec<CandidateRow>,
    symbol_results: Vec<CandidateRow>,
    query: &str,
    intent: QueryIntent,
    limit: usize,
) -> Result<Vec<SearchResult>, OneupError> {
    if vector_results.is_empty() && fts_results.is_empty() && symbol_results.is_empty() {
        return Ok(Vec::new());
    }

    let ranked = rank_candidates(
        vector_results,
        fts_results,
        symbol_results,
        query,
        intent,
        limit,
    );

    Ok(hydrate_ranked_candidates(ranked))
}

fn is_exact_lexical_hit(
    query: &str,
    symbol_results: &[CandidateRow],
    fts_results: &[CandidateRow],
) -> bool {
    if symbol_results.is_empty() && fts_results.is_empty() {
        return false;
    }

    let trimmed = query.trim();
    if trimmed.is_empty() || trimmed.split_whitespace().nth(1).is_some() {
        return false;
    }

    trimmed
        .chars()
        .any(|c| matches!(c, '_' | ':' | '/' | '\\' | '.' | '-' | '#'))
        || trimmed.len() >= 24
}

async fn symbol_search(
    conn: &Connection,
    scope: &SearchScope,
    query: &str,
    intent: QueryIntent,
) -> Result<Vec<CandidateRow>, OneupError> {
    let variants = build_symbol_variants(query, intent);
    if variants.is_empty() {
        return Ok(Vec::new());
    }

    let engine = SymbolSearchEngine::new_scoped(conn, scope.clone());
    let include_usages = matches!(intent, QueryIntent::Usage);
    let mut matches = Vec::new();

    for variant in variants {
        let symbol_matches: Vec<CandidateRow> = if include_usages {
            engine.find_reference_candidates(&variant, true).await?
        } else {
            engine.find_definition_candidates(&variant, true).await?
        };
        matches.extend(symbol_matches);
    }

    let mut deduped = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for candidate in matches {
        let key = candidate_key(&candidate);
        if seen.insert(key) {
            deduped.push(candidate);
        }
    }

    Ok(deduped)
}

/// Maximum identifier tokens extracted from a sentence-length query for the
/// symbol stage. Keeps long queries from fanning out into many symbol
/// lookups.
const MAX_LONG_QUERY_SYMBOL_VARIANTS: usize = 3;

fn build_symbol_variants(query: &str, intent: QueryIntent) -> Vec<String> {
    let words = query_words(query);
    if words.is_empty() || words.iter().all(|word| word.len() < 2) {
        return Vec::new();
    }

    if words.len() > 4 {
        return identifier_like_words(&words);
    }

    let symbolish = query.contains('_')
        || query.chars().any(|c| c.is_uppercase())
        || matches!(intent, QueryIntent::Definition | QueryIntent::Usage)
        || words.len() <= 2;

    if !symbolish {
        return Vec::new();
    }

    vec![words.join(" ")]
}

/// Extracts CamelCase/snake_case tokens from a sentence-length query so it
/// can still engage the symbol stage for identifiers it explicitly mentions.
/// Pure natural-language queries yield nothing and skip symbol weighting.
fn identifier_like_words(words: &[String]) -> Vec<String> {
    let mut variants: Vec<String> = Vec::new();

    for word in words {
        if variants.len() >= MAX_LONG_QUERY_SYMBOL_VARIANTS {
            break;
        }
        if tokenize_text(word).len() > 1 && !variants.contains(word) {
            variants.push(word.clone());
        }
    }

    variants
}

fn query_words(query: &str) -> Vec<String> {
    query
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|word| !word.is_empty())
        .map(|word| word.to_string())
        .collect()
}

/// Builds the ranked results directly from the `CandidateRow`s already carried
/// through ranking, with no per-result storage re-fetch.
///
/// Every retrieval stage (FTS, vector, symbol) SELECTs and fully populates the
/// same `segments` columns into its `CandidateRow`, so the in-memory candidate
/// is byte-identical to a fresh `get_segment_by_id` read of the same row. The
/// only ranking-dependent field is `score`, taken from the fused RRF value.
fn hydrate_ranked_candidates(ranked: Vec<RankedCandidate>) -> Vec<SearchResult> {
    ranked
        .into_iter()
        .map(|ranked_candidate| {
            search_result_from_candidate(ranked_candidate.candidate, ranked_candidate.score)
        })
        .collect()
}

/// Maps an in-memory `CandidateRow` to a `SearchResult`, mirroring the field
/// mapping the prior `get_segment_by_id` path produced. `score` is the
/// normalized fused RRF rank; `defined_symbols` is already the
/// `some_if_not_empty`-collapsed option carried on the candidate.
fn search_result_from_candidate(candidate: CandidateRow, rrf_score: f64) -> SearchResult {
    SearchResult {
        segment_id: candidate.segment_id,
        file_path: candidate.file_path,
        language: candidate.language,
        block_type: candidate.block_type,
        content: candidate.content,
        score: normalize_score(rrf_score),
        line_number: candidate.line_number,
        line_end: candidate.line_end,
        breadcrumb: candidate.breadcrumb,
        defined_symbols: candidate.defined_symbols,
    }
}

fn candidate_key(candidate: &CandidateRow) -> String {
    candidate.segment_id.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::search::scope::SearchScope;
    use crate::search::symbol::SymbolSearchEngine;
    use crate::shared::types::{BranchStatus, SegmentRole};
    use crate::storage::segments::StoredSegment;

    /// Test-only reference implementation of the pre-change `get_segment_by_id`
    /// hydration path. The equivalence test pins `search_result_from_candidate`
    /// to this baseline so any future drift in the in-memory mapping fails CI.
    fn search_result_from_segment(segment: StoredSegment) -> SearchResult {
        let defined_symbols = some_if_not_empty(segment.parsed_defined_symbols());

        SearchResult {
            segment_id: segment.id,
            file_path: segment.file_path,
            language: segment.language,
            block_type: segment.block_type,
            content: segment.content,
            score: 0,
            line_number: segment.line_start as usize,
            line_end: segment.line_end as usize,
            breadcrumb: segment.breadcrumb,
            defined_symbols,
        }
    }

    fn some_if_not_empty(values: Vec<String>) -> Option<Vec<String>> {
        if values.is_empty() {
            None
        } else {
            Some(values)
        }
    }

    fn assert_search_results_equal(actual: &SearchResult, expected: &SearchResult) {
        assert_eq!(actual.segment_id, expected.segment_id, "segment_id");
        assert_eq!(actual.file_path, expected.file_path, "file_path");
        assert_eq!(actual.language, expected.language, "language");
        assert_eq!(actual.block_type, expected.block_type, "block_type");
        assert_eq!(actual.content, expected.content, "content");
        assert_eq!(actual.score, expected.score, "score");
        assert_eq!(actual.line_number, expected.line_number, "line_number");
        assert_eq!(actual.line_end, expected.line_end, "line_end");
        assert_eq!(actual.breadcrumb, expected.breadcrumb, "breadcrumb");
        assert_eq!(
            actual.defined_symbols, expected.defined_symbols,
            "defined_symbols"
        );
    }

    #[test]
    fn symbol_variants_keep_one_canonical_query() {
        let variants = build_symbol_variants("config loader", QueryIntent::Definition);

        assert_eq!(variants, vec!["config loader".to_string()]);
    }

    #[test]
    fn symbol_variants_skip_non_symbolish_long_queries() {
        let variants = build_symbol_variants("how do I load runtime config", QueryIntent::General);

        assert!(variants.is_empty());
    }

    #[test]
    fn symbol_variants_extract_camelcase_identifier_from_long_queries() {
        let variants = build_symbol_variants(
            "impact horizon expansion with ImpactHorizonEngine corroboration",
            QueryIntent::General,
        );

        assert_eq!(variants, vec!["ImpactHorizonEngine".to_string()]);
    }

    #[test]
    fn symbol_variants_extract_snake_case_identifier_from_long_queries() {
        let variants = build_symbol_variants(
            "benchmark parallel indexing script emitting summary_json output",
            QueryIntent::General,
        );

        assert_eq!(variants, vec!["summary_json".to_string()]);
    }

    #[test]
    fn symbol_variants_stay_empty_for_long_capitalized_natural_language() {
        let variants = build_symbol_variants(
            "Where does the daemon watch project files for changes",
            QueryIntent::General,
        );

        assert!(variants.is_empty());
    }

    #[test]
    fn search_result_from_segment_preserves_segment_id() {
        let result = search_result_from_segment(StoredSegment {
            id: "seg-123".to_string(),
            file_path: "src/lib.rs".to_string(),
            language: "rust".to_string(),
            block_type: "function".to_string(),
            content: "fn needle() {}".to_string(),
            line_start: 7,
            line_end: 9,
            breadcrumb: Some("needle".to_string()),
            complexity: 2,
            role: "DEFINITION".to_string(),
            defined_symbols: "[\"needle\"]".to_string(),
            referenced_symbols: "[]".to_string(),
            called_symbols: "[]".to_string(),
            file_hash: "hash".to_string(),
            created_at: "2026-04-13T00:00:00Z".to_string(),
            updated_at: "2026-04-13T00:00:00Z".to_string(),
        });

        assert_eq!(result.segment_id, "seg-123");
    }

    /// REQ-001: building a `SearchResult` from the in-memory `CandidateRow`
    /// (no re-fetch) must be byte-identical to the prior `StoredSegment` path
    /// across every field, including `defined_symbols` parsing and the
    /// `line_start -> line_number` mapping.
    #[test]
    fn search_result_from_candidate_matches_segment_path() {
        let defined_json = "[\"needle\",\"helper\"]";
        let segment = StoredSegment {
            id: "seg-123".to_string(),
            file_path: "src/lib.rs".to_string(),
            language: "rust".to_string(),
            block_type: "function".to_string(),
            content: "fn needle() { helper(); }".to_string(),
            line_start: 7,
            line_end: 9,
            breadcrumb: Some("module::needle".to_string()),
            complexity: 2,
            role: "DEFINITION".to_string(),
            defined_symbols: defined_json.to_string(),
            referenced_symbols: "[]".to_string(),
            called_symbols: "[\"helper\"]".to_string(),
            file_hash: "hash".to_string(),
            created_at: "2026-04-13T00:00:00Z".to_string(),
            updated_at: "2026-04-13T00:00:00Z".to_string(),
        };
        // The candidate carries the same row's fields, transformed exactly as
        // `row_to_candidate_row` / the symbol stage produce them.
        let candidate = CandidateRow {
            segment_id: segment.id.clone(),
            file_path: segment.file_path.clone(),
            language: segment.language.clone(),
            block_type: segment.block_type.clone(),
            line_number: segment.line_start as usize,
            line_end: segment.line_end as usize,
            breadcrumb: segment.breadcrumb.clone(),
            complexity: Some(segment.complexity as u32),
            role: Some(SegmentRole::Definition),
            defined_symbols: some_if_not_empty(segment.parsed_defined_symbols()),
            referenced_symbols: None,
            called_symbols: some_if_not_empty(segment.parsed_called_symbols()),
            content: segment.content.clone(),
        };

        // `search_result_from_segment` sets score 0; compare at the same score
        // so every other field is asserted directly.
        let expected = search_result_from_segment(segment);
        let actual = search_result_from_candidate(candidate, 0.0);

        assert_search_results_equal(&actual, &expected);
        assert_eq!(
            actual.defined_symbols,
            Some(vec!["needle".to_string(), "helper".to_string()]),
            "defined_symbols must be parsed from the JSON column"
        );
        assert_eq!(
            actual.line_number, 7,
            "line_number must map from line_start"
        );
    }

    /// REQ-001: empty `defined_symbols` collapses to `None` on both paths.
    #[test]
    fn search_result_from_candidate_collapses_empty_defined_symbols() {
        let segment = StoredSegment {
            id: "seg-empty".to_string(),
            file_path: "src/lib.rs".to_string(),
            language: "rust".to_string(),
            block_type: "function".to_string(),
            content: "fn anon() {}".to_string(),
            line_start: 1,
            line_end: 1,
            breadcrumb: None,
            complexity: 1,
            role: "DEFINITION".to_string(),
            defined_symbols: "[]".to_string(),
            referenced_symbols: "[]".to_string(),
            called_symbols: "[]".to_string(),
            file_hash: "hash".to_string(),
            created_at: "2026-04-13T00:00:00Z".to_string(),
            updated_at: "2026-04-13T00:00:00Z".to_string(),
        };
        let candidate = CandidateRow {
            segment_id: segment.id.clone(),
            file_path: segment.file_path.clone(),
            language: segment.language.clone(),
            block_type: segment.block_type.clone(),
            line_number: segment.line_start as usize,
            line_end: segment.line_end as usize,
            breadcrumb: None,
            complexity: Some(1),
            role: Some(SegmentRole::Definition),
            defined_symbols: some_if_not_empty(segment.parsed_defined_symbols()),
            referenced_symbols: None,
            called_symbols: None,
            content: segment.content.clone(),
        };

        let expected = search_result_from_segment(segment);
        let actual = search_result_from_candidate(candidate, 0.0);

        assert_search_results_equal(&actual, &expected);
        assert_eq!(actual.defined_symbols, None);
    }

    /// REQ-001 guard: the three retrieval stages MUST populate identical
    /// `content`/`breadcrumb`/`defined_symbols` for the same `segment_id`.
    /// This is the invariant that makes direct (re-fetch-free) hydration
    /// byte-identical; a future stage SELECT dropping a column fails here.
    #[tokio::test]
    async fn three_stage_candidate_rows_carry_identical_hydration_fields() {
        use crate::storage::segments::{upsert_segment, SegmentInsert};

        let db = crate::storage::db::Db::open_memory().await.unwrap();
        let conn = db.connect().unwrap();
        crate::storage::schema::initialize(&conn).await.unwrap();

        // One segment that matches all three stages: it has a symbol, FTS-able
        // content, and a pooled embedding so the vector stage returns it too.
        // The real write path populates segment_symbols (symbol stage) and the
        // external-content FTS index (FTS stage) via the schema triggers.
        let embedding = vec![1.0_f32; 384];
        let serialized = serde_json::to_string(&embedding).unwrap();
        let content_key = {
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(serialized.as_bytes());
            hasher
                .finalize()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        };
        let insert = SegmentInsert {
            id: "seg-parity".to_string(),
            file_path: "src/parity.rs".to_string(),
            language: "rust".to_string(),
            block_type: "function".to_string(),
            content: "fn config_loader() { load(); }".to_string(),
            line_start: 4,
            line_end: 8,
            content_key: Some(content_key.clone()),
            embedding_vec: Some(serialized.clone()),
            breadcrumb: Some("mod::config_loader".to_string()),
            complexity: 2,
            role: "DEFINITION".to_string(),
            defined_symbols: "[\"config_loader\"]".to_string(),
            referenced_symbols: "[]".to_string(),
            referenced_relations: "[]".to_string(),
            called_symbols: "[\"load\"]".to_string(),
            called_relations: "[]".to_string(),
            file_hash: "hash-parity".to_string(),
        };
        upsert_segment(&conn, &insert).await.unwrap();

        let scope = SearchScope::default_context();

        let fts = retrieval::fetch_fts_candidates(&conn, &scope, "config loader")
            .await
            .unwrap();
        let vector = retrieval::fetch_vector_candidates(&conn, &scope, &embedding)
            .await
            .unwrap();
        let symbol = SymbolSearchEngine::new_scoped(&conn, scope.clone())
            .find_definition_candidates("config_loader", true)
            .await
            .unwrap();

        let pick = |rows: &[CandidateRow]| -> CandidateRow {
            rows.iter()
                .find(|c| c.segment_id == "seg-parity")
                .expect("seg-parity must appear in every stage")
                .clone()
        };
        let fts_row = pick(&fts);
        let vector_row = pick(&vector);
        let symbol_row = pick(&symbol);

        for other in [&vector_row, &symbol_row] {
            assert_eq!(fts_row.content, other.content, "content parity");
            assert_eq!(fts_row.breadcrumb, other.breadcrumb, "breadcrumb parity");
            assert_eq!(
                fts_row.defined_symbols, other.defined_symbols,
                "defined_symbols parity"
            );
            assert_eq!(fts_row.line_number, other.line_number, "line_number parity");
            assert_eq!(fts_row.line_end, other.line_end, "line_end parity");
        }
    }

    #[test]
    fn exact_lexical_hit_short_circuits_identifier_queries() {
        let hit = test_candidate("seg-lexical");

        assert!(is_exact_lexical_hit(
            "test_exact_lexical_short_circuit_token",
            &[],
            &[hit]
        ));
    }

    #[test]
    fn exact_lexical_hit_keeps_natural_language_semantic_path() {
        let hit = test_candidate("seg-lexical");

        assert!(!is_exact_lexical_hit("config loader", &[], &[hit]));
    }

    #[tokio::test]
    async fn symbol_search_matches_canonical_symbol_queries() {
        let db = crate::storage::db::Db::open_memory().await.unwrap();
        let conn = db.connect().unwrap();
        crate::storage::schema::initialize(&conn).await.unwrap();

        let insert = crate::storage::segments::SegmentInsert {
            id: "test-seg-symbol".to_string(),
            file_path: "src/lib.rs".to_string(),
            language: "rust".to_string(),
            block_type: "struct".to_string(),
            content: "struct ConfigLoader;".to_string(),
            line_start: 1,
            line_end: 1,
            content_key: None,
            embedding_vec: None,
            breadcrumb: None,
            complexity: 1,
            role: "DEFINITION".to_string(),
            defined_symbols: "[\"ConfigLoader\"]".to_string(),
            referenced_symbols: "[]".to_string(),
            referenced_relations: "[]".to_string(),
            called_symbols: "[]".to_string(),
            called_relations: "[]".to_string(),
            file_hash: "symbol123".to_string(),
        };
        crate::storage::segments::upsert_segment(&conn, &insert)
            .await
            .unwrap();

        let results = symbol_search(
            &conn,
            &SearchScope::default_context(),
            "config loader",
            QueryIntent::Definition,
        )
        .await
        .unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].file_path, "src/lib.rs");
        assert_eq!(results[0].block_type, "struct");
    }

    #[tokio::test]
    async fn fts_only_search_without_embedder() {
        let db = crate::storage::db::Db::open_memory().await.unwrap();
        let conn = db.connect().unwrap();
        crate::storage::schema::initialize(&conn).await.unwrap();

        let insert = crate::storage::segments::SegmentInsert {
            id: "test-seg-1".to_string(),
            file_path: "src/main.rs".to_string(),
            language: "rust".to_string(),
            block_type: "function".to_string(),
            content: "fn handle_error() { eprintln!(\"error occurred\"); }".to_string(),
            line_start: 1,
            line_end: 3,
            content_key: None,
            embedding_vec: None,
            breadcrumb: None,
            complexity: 1,
            role: "DEFINITION".to_string(),
            defined_symbols: "[\"handle_error\"]".to_string(),
            referenced_symbols: "[]".to_string(),
            referenced_relations: "[]".to_string(),
            called_symbols: "[]".to_string(),
            called_relations: "[]".to_string(),
            file_hash: "abc123".to_string(),
        };
        crate::storage::segments::upsert_segment(&conn, &insert)
            .await
            .unwrap();

        let engine = HybridSearchEngine::new(&conn, None);
        let results = engine.fts_only_search("error", 10).await.unwrap();

        assert!(
            !results.is_empty(),
            "FTS-only search should return results without embedder"
        );
        assert_eq!(results[0].file_path, "src/main.rs");
    }

    #[tokio::test]
    async fn search_with_none_embedder_uses_fts_only() {
        let db = crate::storage::db::Db::open_memory().await.unwrap();
        let conn = db.connect().unwrap();
        crate::storage::schema::initialize(&conn).await.unwrap();

        let insert = crate::storage::segments::SegmentInsert {
            id: "test-seg-2".to_string(),
            file_path: "src/lib.rs".to_string(),
            language: "rust".to_string(),
            block_type: "function".to_string(),
            content: "fn validate_input(data: &str) -> bool { !data.is_empty() }".to_string(),
            line_start: 10,
            line_end: 12,
            content_key: None,
            embedding_vec: None,
            breadcrumb: None,
            complexity: 1,
            role: "DEFINITION".to_string(),
            defined_symbols: "[\"validate_input\"]".to_string(),
            referenced_symbols: "[]".to_string(),
            referenced_relations: "[]".to_string(),
            called_symbols: "[]".to_string(),
            called_relations: "[]".to_string(),
            file_hash: "def456".to_string(),
        };
        crate::storage::segments::upsert_segment(&conn, &insert)
            .await
            .unwrap();

        let mut engine = HybridSearchEngine::new(&conn, None);
        let results = engine.search("validate input", 10).await.unwrap();

        assert!(
            !results.is_empty(),
            "search with None embedder should fall back to FTS"
        );
    }

    #[tokio::test]
    async fn search_filters_results_to_active_context() {
        let db = crate::storage::db::Db::open_memory().await.unwrap();
        let conn = db.connect().unwrap();
        crate::storage::schema::initialize(&conn).await.unwrap();

        let main = scoped_insert("seg-main", "src/main.rs", "fn branch_needle() {}");
        let other = scoped_insert("seg-other", "src/other.rs", "fn branch_needle() {}");
        crate::storage::segments::upsert_segment_for_context(&conn, "ctx-main", &main)
            .await
            .unwrap();
        crate::storage::segments::upsert_segment_for_context(&conn, "ctx-other", &other)
            .await
            .unwrap();

        let scope = SearchScope::new("ctx-main", BranchStatus::Named);
        let engine = HybridSearchEngine::new_scoped(&conn, None, scope);
        let results = engine.fts_only_search("branch_needle", 10).await.unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].segment_id, "seg-main");
    }

    fn scoped_insert(
        id: &str,
        file_path: &str,
        content: &str,
    ) -> crate::storage::segments::SegmentInsert {
        crate::storage::segments::SegmentInsert {
            id: id.to_string(),
            file_path: file_path.to_string(),
            language: "rust".to_string(),
            block_type: "function".to_string(),
            content: content.to_string(),
            line_start: 1,
            line_end: 3,
            content_key: None,
            embedding_vec: None,
            breadcrumb: None,
            complexity: 1,
            role: "DEFINITION".to_string(),
            defined_symbols: "[]".to_string(),
            referenced_symbols: "[]".to_string(),
            referenced_relations: "[]".to_string(),
            called_symbols: "[]".to_string(),
            called_relations: "[]".to_string(),
            file_hash: format!("hash-{id}"),
        }
    }

    fn test_candidate(id: &str) -> CandidateRow {
        CandidateRow {
            segment_id: id.to_string(),
            file_path: "src/lib.rs".to_string(),
            language: "rust".to_string(),
            block_type: "function".to_string(),
            line_number: 1,
            line_end: 1,
            breadcrumb: None,
            complexity: None,
            role: None,
            defined_symbols: None,
            referenced_symbols: None,
            called_symbols: None,
            content: String::new(),
        }
    }
}
