use libsql::Connection;

use crate::indexer::embedder::Embedder;
use crate::indexer::markdown::DOC_SECTION_BLOCK_TYPE;
use crate::search::intent::detect_intent;
use crate::search::intent::QueryIntent;
use crate::search::ranking::{query_words, rank_candidates, tokenize_text, RankedCandidate};
use crate::search::retrieval::{self, CandidateRow};
use crate::search::scope::SearchScope;
use crate::search::symbol::SymbolSearchEngine;
use crate::shared::constants::RRF_K;
use crate::shared::errors::{OneupError, SearchError};
use crate::shared::types::{normalize_score, SearchResult};

pub struct HybridSearchEngine<'a> {
    conn: &'a Connection,
    embedder: Option<&'a mut Embedder>,
    scope: SearchScope,
    /// Pre-probed vector-presence flag (R-007). Every live caller already runs
    /// the cheap `has_indexed_embeddings` probe before deciding whether to warm
    /// the embedder, so threading the result in here removes the engine's
    /// duplicate probe. `None` means "not supplied" and the engine falls back to
    /// probing live, preserving the prior behaviour for callers that do not set
    /// it (e.g. unit tests).
    has_vectors: Option<bool>,
    /// Pre-computed per-context vector `COUNT(*)` (R-007), cached by the daemon
    /// on `ProjectState` and invalidated on index swap. When `Some`, the vector
    /// stage skips its per-query `COUNT(*)` for path selection; it MUST equal the
    /// live count so selection is identical. `None` falls back to the live count.
    vector_count: Option<usize>,
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
            has_vectors: None,
            vector_count: None,
        }
    }

    /// Supply the already-probed vector-presence flag so the vector stage skips
    /// its duplicate `has_indexed_embeddings` probe (R-007). The supplied value
    /// MUST match the live probe for the open index.
    pub fn with_has_vectors(mut self, has_vectors: bool) -> Self {
        self.has_vectors = Some(has_vectors);
        self
    }

    /// Supply a cached per-context vector count so the vector stage skips its
    /// per-query `COUNT(*)` for path selection (R-007). The supplied value MUST
    /// equal the live `COUNT(*)` for the open index.
    pub fn with_vector_count(mut self, vector_count: usize) -> Self {
        self.vector_count = Some(vector_count);
        self
    }

    /// Hybrid search with lazy query embedding: the lexical stages, the
    /// exact-lexical short-circuit, and the cheap vector-presence probe all
    /// run before the query is ever embedded, so an index without vector rows
    /// never exercises the embedder.
    ///
    /// The symbol and FTS stages are independent read-only queries over the
    /// same connection, so they run concurrently via `tokio::try_join!`
    /// (R-002, reviving the dead `SqlVectorV2` concurrency pattern on the live
    /// path). The vector stage stays sequenced after them because its
    /// `is_exact_lexical_hit` / `has_indexed_embeddings` gate depends on the
    /// lexical results — embedding it concurrently would break the
    /// short-circuit's "an index without vector rows never exercises the
    /// embedder" guarantee. Fusion stays timing-independent: each stage's rank
    /// comes from its own SQL `ORDER BY`, and RRF is keyed by `segment_id`, so
    /// overlapping the lexical reads cannot change the ranked output.
    pub async fn search(
        &mut self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchResult>, OneupError> {
        if query.trim().is_empty() {
            return Err(SearchError::InvalidQuery("empty query".to_string()).into());
        }

        let intent = detect_intent(query);
        let (symbol_results, fts_results) = tokio::try_join!(
            symbol_search(self.conn, &self.scope, query, intent),
            retrieval::fetch_fts_candidates(self.conn, &self.scope, query),
        )?;

        // Use the caller-supplied vector-presence flag when present, falling back
        // to the live probe otherwise (R-007: removes the engine's duplicate
        // `has_indexed_embeddings` probe on the live daemon/MCP/CLI paths, which
        // already ran it before warming the embedder). The exact-lexical-hit
        // short-circuit and `&mut embedder` discipline are unchanged.
        let should_fetch_vector = self.embedder.is_some()
            && !is_exact_lexical_hit(query, &symbol_results, &fts_results)
            && self.has_indexed_embeddings().await?;
        let cached_vector_count = self.vector_count;
        let vector_results = if should_fetch_vector {
            match self.embed_query(query) {
                Some(embedding) => {
                    fetch_vector_candidates_with_degrade(
                        self.conn,
                        &self.scope,
                        &embedding,
                        cached_vector_count,
                    )
                    .await
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

    /// Embeds the query for the vector stage, running the CPU-bound ONNX
    /// inference off the cooperative scheduler so it does not stall other
    /// async work on the multi-thread runtime (R-002).
    ///
    /// Uses `block_in_place` rather than `spawn_blocking`: the embedder is a
    /// borrowed `&mut Embedder` lent by the caller's (warm) runtime, and
    /// `spawn_blocking` requires a `'static + Send` closure, which would force
    /// either an `Arc<Mutex<Embedder>>` or moving ownership through the engine
    /// — both of which would break the `&mut embedder` borrow discipline the
    /// three live callers rely on. `block_in_place` keeps that exact borrow,
    /// runs the identical `embed_one` call (so the vector is bit-for-bit
    /// unchanged), and tells the runtime to relocate this worker's other tasks
    /// while the inference runs. Every live caller runs on the multi-thread
    /// `#[tokio::main]` runtime, which `block_in_place` requires.
    /// Vector-presence gate: the caller-supplied flag when set (R-007), else a
    /// live probe. Identical to the live probe by contract — `with_has_vectors`
    /// callers pass the live result they already computed.
    async fn has_indexed_embeddings(&self) -> Result<bool, OneupError> {
        match self.has_vectors {
            Some(has_vectors) => Ok(has_vectors),
            None => retrieval::has_indexed_embeddings(self.conn, &self.scope).await,
        }
    }

    fn embed_query(&mut self, query: &str) -> Option<Vec<f32>> {
        let embedder = self.embedder.as_deref_mut()?;
        match tokio::task::block_in_place(|| embedder.embed_one(query)) {
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
    cached_count: Option<usize>,
) -> Vec<CandidateRow> {
    match retrieval::fetch_vector_candidates_with_count(conn, scope, query_embedding, cached_count)
        .await
    {
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

/// Fuse several per-query ranked result lists into one deduped-by-handle ranked
/// list using Reciprocal Rank Fusion (the multi-query `oneup_search` path).
///
/// Each input list is independently ranked with rank 0 at the top. A segment's
/// fused score is `Σ 1/(RRF_K + rank)` over every list it appears in, so a
/// segment several aspects of a multi-part question all surface outranks one
/// that only matched a single aspect. The list is deduplicated by `segment_id`
/// (the durable handle), keeping the representative row from the list where the
/// segment ranked best (lowest rank) and re-deriving its displayed `score` from
/// the fused RRF total. Ordering is fully deterministic: fused score descending,
/// then best rank ascending, then `segment_id` ascending. The fused list is
/// truncated to `limit`.
///
/// A single-element `lists` is returned untouched (aside from the `limit`
/// truncation the single-query path already applies), so the caller only reaches
/// this merge when there is more than one query to fuse.
pub fn merge_multi_query_results(lists: Vec<Vec<SearchResult>>, limit: usize) -> Vec<SearchResult> {
    use std::collections::HashMap;

    // Per handle: (fused RRF score, best rank seen, representative row).
    let mut fused: HashMap<String, (f64, usize, SearchResult)> = HashMap::new();
    for list in lists {
        for (rank, result) in list.into_iter().enumerate() {
            let contribution = 1.0 / (RRF_K + rank as f64);
            match fused.get_mut(&result.segment_id) {
                Some(entry) => {
                    entry.0 += contribution;
                    if rank < entry.1 {
                        entry.1 = rank;
                        entry.2 = result;
                    }
                }
                None => {
                    fused.insert(result.segment_id.clone(), (contribution, rank, result));
                }
            }
        }
    }

    let mut merged: Vec<(f64, usize, SearchResult)> = fused.into_values().collect();
    merged.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.1.cmp(&b.1))
            .then_with(|| a.2.segment_id.cmp(&b.2.segment_id))
    });

    merged
        .into_iter()
        .take(limit)
        .map(|(fused_score, _, mut result)| {
            result.score = normalize_score(fused_score);
            result
        })
        .collect()
}

/// High-precision query tokens that signal the searcher is after implementation
/// code (a component, handler, route, ...) rather than prose. Kept deliberately
/// narrow so [`demote_doc_sections_for_implementation_intent`] fires only on an
/// unambiguous implementation query. Matched whole-token and case-insensitively.
const IMPLEMENTATION_INTENT_MARKERS: [&str; 23] = [
    "component",
    "components",
    "handler",
    "handlers",
    "endpoint",
    "endpoints",
    "implementation",
    "implemented",
    "function",
    "functions",
    "class",
    "classes",
    "hook",
    "hooks",
    "route",
    "routes",
    "renderer",
    "rendering",
    "ui",
    "page",
    "pages",
    "wizard",
    "pipeline",
];

/// Whether any query in the set carries a high-precision implementation-intent
/// marker (whole-token, case-insensitive). For a multi-query call this is the
/// union of every query's tokens, so one implementation-flavored aspect is
/// enough to signal intent.
pub fn query_signals_implementation_intent(queries: &[String]) -> bool {
    queries.iter().any(|query| {
        query
            .split(|c: char| !c.is_alphanumeric())
            .filter(|token| !token.is_empty())
            .any(|token| {
                IMPLEMENTATION_INTENT_MARKERS
                    .iter()
                    .any(|marker| token.eq_ignore_ascii_case(marker))
            })
    })
}

/// When the query set signals implementation intent and the ranked list holds
/// at least one non-doc result, stable-partition the list so `doc_section`
/// results fall below every code result, preserving relative order within each
/// group. Scores are untouched and nothing is dropped — every result stays
/// present. Without implementation intent (or when every result is a doc
/// section) the list is returned unchanged, so non-implementation searches are
/// byte-identical to before.
pub fn demote_doc_sections_for_implementation_intent(
    results: Vec<SearchResult>,
    queries: &[String],
) -> Vec<SearchResult> {
    if !query_signals_implementation_intent(queries) {
        return results;
    }
    let has_non_doc = results
        .iter()
        .any(|result| result.block_type != DOC_SECTION_BLOCK_TYPE);
    if !has_non_doc {
        return results;
    }

    let mut code = Vec::with_capacity(results.len());
    let mut docs = Vec::new();
    for result in results {
        if result.block_type == DOC_SECTION_BLOCK_TYPE {
            docs.push(result);
        } else {
            code.push(result);
        }
    }
    code.extend(docs);
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::search::scope::SearchScope;
    use crate::search::symbol::SymbolSearchEngine;
    use crate::shared::types::{BranchStatus, SegmentRole};
    use crate::storage::segments::StoredSegment;

    fn result_with_id(segment_id: &str) -> SearchResult {
        SearchResult {
            segment_id: segment_id.to_string(),
            file_path: format!("src/{segment_id}.rs"),
            language: "rust".to_string(),
            block_type: "function".to_string(),
            content: String::new(),
            score: 0,
            line_number: 1,
            line_end: 2,
            breadcrumb: None,
            defined_symbols: None,
        }
    }

    #[test]
    fn merge_multi_query_dedups_by_handle_and_keeps_best_rank() {
        // Query A ranks [a, b, c]; query B ranks [b, d, a]. Every handle is
        // deduped once; `b` (ranks 1 and 0) and `a` (ranks 0 and 2) appear in
        // both lists, so they outrank the single-list `c` and `d`.
        let list_a = vec![
            result_with_id("a"),
            result_with_id("b"),
            result_with_id("c"),
        ];
        let list_b = vec![
            result_with_id("b"),
            result_with_id("d"),
            result_with_id("a"),
        ];

        let merged = merge_multi_query_results(vec![list_a, list_b], 10);

        // Fused RRF: b = 1/61 + 1/60, a = 1/60 + 1/62 (both matched twice, so
        // they lead), then the single-list handles by their own rank — d (rank 1)
        // ahead of c (rank 2).
        let ids: Vec<&str> = merged.iter().map(|r| r.segment_id.as_str()).collect();
        assert_eq!(ids, vec!["b", "a", "d", "c"], "fused ranking: {ids:?}");
        assert_eq!(
            merged.len(),
            4,
            "each handle must appear exactly once after de-duplication"
        );
        // A fused handle scores higher than a single-list handle.
        assert!(
            merged[0].score >= merged[2].score,
            "a handle matched by both queries should not rank below a single-query handle"
        );
    }

    #[test]
    fn merge_multi_query_truncates_to_limit() {
        let list = vec![
            result_with_id("a"),
            result_with_id("b"),
            result_with_id("c"),
        ];
        let merged = merge_multi_query_results(vec![list], 2);
        assert_eq!(merged.len(), 2);
    }

    fn doc_result(segment_id: &str) -> SearchResult {
        SearchResult {
            file_path: format!("docs/{segment_id}.md"),
            language: "markdown".to_string(),
            block_type: DOC_SECTION_BLOCK_TYPE.to_string(),
            ..result_with_id(segment_id)
        }
    }

    #[test]
    fn implementation_intent_detects_whole_token_markers_case_insensitively() {
        // A marker token anywhere in any query fires, case-insensitively.
        assert!(query_signals_implementation_intent(&[
            "admin search Page COMPONENT".to_string()
        ]));
        // Union across a multi-query set: one implementation aspect is enough.
        assert!(query_signals_implementation_intent(&[
            "where does auth live".to_string(),
            "login route".to_string(),
        ]));
    }

    #[test]
    fn implementation_intent_ignores_non_marker_and_substring_matches() {
        // No marker token present.
        assert!(!query_signals_implementation_intent(&[
            "search configuration schema".to_string()
        ]));
        // Whole-token only: "configuration" must not match "function", nor
        // "components" spelled inside a larger word be a false positive here.
        assert!(!query_signals_implementation_intent(&[
            "reusable subcomponents catalogue".to_string()
        ]));
        // Empty set never signals intent.
        assert!(!query_signals_implementation_intent(&[]));
    }

    #[test]
    fn demotion_sinks_docs_below_code_preserving_relative_order() {
        let results = vec![
            doc_result("d1"),
            result_with_id("c1"),
            doc_result("d2"),
            result_with_id("c2"),
        ];
        let queries = vec!["admin search page component".to_string()];

        let demoted = demote_doc_sections_for_implementation_intent(results, &queries);
        let ids: Vec<&str> = demoted.iter().map(|r| r.segment_id.as_str()).collect();

        // Code keeps its relative order, then docs keep theirs, all below code.
        assert_eq!(ids, vec!["c1", "c2", "d1", "d2"]);
    }

    #[test]
    fn demotion_is_a_noop_without_implementation_intent() {
        let results = vec![doc_result("d1"), result_with_id("c1"), doc_result("d2")];
        let queries = vec!["request signing secret configuration".to_string()];

        let demoted = demote_doc_sections_for_implementation_intent(results.clone(), &queries);
        let before: Vec<&str> = results.iter().map(|r| r.segment_id.as_str()).collect();
        let after: Vec<&str> = demoted.iter().map(|r| r.segment_id.as_str()).collect();

        // Order is byte-identical to the input when intent does not fire.
        assert_eq!(after, before);
    }

    #[test]
    fn demotion_is_a_noop_when_every_result_is_a_doc_section() {
        let results = vec![doc_result("d1"), doc_result("d2"), doc_result("d3")];
        let queries = vec!["admin search page component".to_string()];

        let demoted = demote_doc_sections_for_implementation_intent(results.clone(), &queries);
        let before: Vec<&str> = results.iter().map(|r| r.segment_id.as_str()).collect();
        let after: Vec<&str> = demoted.iter().map(|r| r.segment_id.as_str()).collect();

        // No non-doc result to lift, so the list is returned unchanged.
        assert_eq!(after, before);
    }

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

    // TDD (REQ-006 / T6 AC2): the threaded `has_vectors` flag must be the
    // authoritative vector-presence signal — identical to the live
    // `has_indexed_embeddings` probe the caller already ran — and replace the
    // engine's duplicate probe. This pins both halves: `with_has_vectors(true)`
    // matches the live probe on an index that has vectors, and the value
    // supplied is what the gate uses (a `false` flag forces FTS-only even when
    // the index physically holds vectors, proving the live re-probe is gone).
    #[tokio::test]
    async fn threaded_has_vectors_flag_drives_vector_gate_and_matches_live_probe() {
        use crate::storage::segments::{upsert_segment, SegmentInsert};

        let db = crate::storage::db::Db::open_memory().await.unwrap();
        let conn = db.connect().unwrap();
        crate::storage::schema::initialize(&conn).await.unwrap();

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
            id: "seg-vec".to_string(),
            file_path: "src/vec.rs".to_string(),
            language: "rust".to_string(),
            block_type: "function".to_string(),
            content: "fn vector_target() { run(); }".to_string(),
            line_start: 1,
            line_end: 3,
            content_key: Some(content_key),
            embedding_vec: Some(serialized),
            breadcrumb: None,
            complexity: 1,
            role: "DEFINITION".to_string(),
            defined_symbols: "[\"vector_target\"]".to_string(),
            referenced_symbols: "[]".to_string(),
            referenced_relations: "[]".to_string(),
            called_symbols: "[]".to_string(),
            called_relations: "[]".to_string(),
            file_hash: "hash-vec".to_string(),
        };
        upsert_segment(&conn, &insert).await.unwrap();

        let scope = SearchScope::default_context();

        // The index physically holds vectors, so the live probe is true.
        let live = retrieval::has_indexed_embeddings(&conn, &scope)
            .await
            .unwrap();
        assert!(live, "fixture seeds an indexed embedding");

        // `with_has_vectors(true)` matches the live probe.
        let engine_true =
            HybridSearchEngine::new_scoped(&conn, None, scope.clone()).with_has_vectors(true);
        assert_eq!(engine_true.has_indexed_embeddings().await.unwrap(), live);

        // A `false` flag is authoritative — the gate uses it, not a live re-probe.
        let engine_false =
            HybridSearchEngine::new_scoped(&conn, None, scope.clone()).with_has_vectors(false);
        assert!(
            !engine_false.has_indexed_embeddings().await.unwrap(),
            "the supplied flag must drive the gate, proving the duplicate live probe is gone"
        );

        // Unset falls back to the live probe (default behaviour preserved).
        let engine_unset = HybridSearchEngine::new_scoped(&conn, None, scope);
        assert_eq!(engine_unset.has_indexed_embeddings().await.unwrap(), live);
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

    fn symbol_fts_insert(
        id: &str,
        file_path: &str,
        content: &str,
        defined_symbols: &str,
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
            defined_symbols: defined_symbols.to_string(),
            referenced_symbols: "[]".to_string(),
            referenced_relations: "[]".to_string(),
            called_symbols: "[]".to_string(),
            called_relations: "[]".to_string(),
            file_hash: format!("hash-{id}"),
        }
    }

    /// REQ-002 (T2): `search` fuses the concurrently-run (`try_join!`) symbol
    /// and FTS stages identically to the prior sequential awaits. The corpus
    /// is split so one segment matches *only* the symbol stage and the other
    /// *only* FTS, and the per-stage RRF weights differ (`SYMBOL_WEIGHT=4.0`
    /// vs FTS `1.0`), so the symbol-only hit must rank first. This is the
    /// discriminating assertion: swapping the two `try_join!` outputs (mis-
    /// attributing symbol rows as FTS and vice versa) flips the order, so the
    /// test fails — verified by construction. The `None` embedder keeps the
    /// vector stage out, isolating the lexical-fusion path that changed and
    /// keeping the test hermetic (no model required).
    #[tokio::test]
    async fn search_fuses_concurrent_symbol_and_fts_stages_in_weighted_order() {
        let db = crate::storage::db::Db::open_memory().await.unwrap();
        let conn = db.connect().unwrap();
        crate::storage::schema::initialize(&conn).await.unwrap();

        // Symbol-only hit: its canonical symbol matches "render widget"; its
        // content carries none of the query tokens or their prefixes, so the
        // FTS stage cannot reach it.
        let symbol_only = symbol_fts_insert(
            "seg-symbol-only",
            "src/symbol.rs",
            "fn zzz_handler() { qqq_helper(); }",
            "[\"render_widget\"]",
        );
        // FTS-only hit: its content matches "render"/"widget", but its symbol
        // does not canonicalize to the query, so the symbol stage skips it.
        let fts_only = symbol_fts_insert(
            "seg-fts-only",
            "src/text.rs",
            "// render the widget in this routine",
            "[\"unrelated_symbol\"]",
        );
        crate::storage::segments::upsert_segment(&conn, &symbol_only)
            .await
            .unwrap();
        crate::storage::segments::upsert_segment(&conn, &fts_only)
            .await
            .unwrap();

        let mut engine = HybridSearchEngine::new(&conn, None);
        let results = engine.search("render widget", 10).await.unwrap();

        let ids: Vec<&str> = results.iter().map(|r| r.segment_id.as_str()).collect();
        assert!(
            ids.contains(&"seg-symbol-only"),
            "symbol-only hit must surface: {ids:?}"
        );
        assert!(
            ids.contains(&"seg-fts-only"),
            "fts-only hit must surface: {ids:?}"
        );
        assert_eq!(
            ids[0], "seg-symbol-only",
            "symbol-stage weight ({}) must outrank the fts-stage hit ({}); a swap of the try_join! outputs would flip this: {ids:?}",
            crate::shared::constants::SYMBOL_WEIGHT,
            1.0
        );
        // The symbol-only hit carries strictly more fused weight than the
        // fts-only hit, so its normalized score is the larger of the two.
        let score_of = |id: &str| results.iter().find(|r| r.segment_id == id).unwrap().score;
        assert!(
            score_of("seg-symbol-only") > score_of("seg-fts-only"),
            "symbol-only score must exceed fts-only score: {ids:?}"
        );
    }

    /// REQ-002 (T2): the query embedding now runs through `block_in_place`
    /// (off the cooperative scheduler) rather than inline. The produced vector
    /// must be bit-for-bit identical to the direct synchronous call — the
    /// numerics acceptance gate. Runs on a multi-thread runtime because
    /// `block_in_place` panics on a current-thread runtime, and is gated on
    /// model availability like the other real-inference tests (hermetic CI
    /// disables model downloads).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn query_embedding_is_bit_stable_through_block_in_place() {
        use crate::indexer::embedder::{
            is_model_available, EmbeddingRuntime, Fp32VariantTestGuard,
        };

        // Pin the always-provisioned FP32 baseline: this test verifies
        // variant-agnostic query-embedding stability and must not depend on the
        // INT8 default artifact being present locally (provisioned by T4).
        let _variant = Fp32VariantTestGuard::set();
        if !is_model_available() {
            eprintln!("skipping: model not available");
            return;
        }

        let mut runtime = EmbeddingRuntime::default();
        let status = runtime.prepare_for_search(1).unwrap();
        assert!(
            status.is_available(),
            "expected an available embedder, got {status:?}"
        );
        let embedder = runtime
            .current_embedder()
            .expect("available runtime must expose an embedder");

        let query = "where is the query embedding computed";
        let synchronous = embedder.embed_one(query).unwrap();
        let off_runtime = tokio::task::block_in_place(|| embedder.embed_one(query)).unwrap();

        assert_eq!(
            synchronous, off_runtime,
            "block_in_place must run the identical inference and not perturb the vector"
        );
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
