use libsql::Connection;

use crate::indexer::embedder::Embedder;
use crate::search::intent::detect_intent;
use crate::search::intent::QueryIntent;
use crate::search::ranking::{rank_candidates, RankedCandidate};
use crate::search::retrieval::{CandidateRow, RetrievalBackend, RetrievalMode};
use crate::search::scope::SearchScope;
use crate::search::symbol::SymbolSearchEngine;
use crate::shared::errors::{OneupError, SearchError};
use crate::shared::types::{normalize_score, SearchResult};
use crate::storage::segments::{get_segment_by_id, StoredSegment};

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

    pub async fn search(
        &mut self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchResult>, OneupError> {
        let query_embedding = self.embed_query(query);
        execute_search(
            self.conn,
            &self.scope,
            query,
            limit,
            query_embedding.as_deref(),
        )
        .await
    }

    pub async fn fts_only_search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchResult>, OneupError> {
        execute_search(self.conn, &self.scope, query, limit, None).await
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

async fn execute_search(
    conn: &Connection,
    scope: &SearchScope,
    query: &str,
    limit: usize,
    query_embedding: Option<&[f32]>,
) -> Result<Vec<SearchResult>, OneupError> {
    if query.trim().is_empty() {
        return Err(SearchError::InvalidQuery("empty query".to_string()).into());
    }

    let intent = detect_intent(query);
    let symbol_results = symbol_search(conn, scope, query, intent).await?;
    let backend = RetrievalBackend::select_scoped(conn, query_embedding, scope.clone()).await?;
    let candidates = match backend.search(query, query_embedding).await {
        Ok(candidates) => candidates,
        Err(err) if matches!(backend.mode(), RetrievalMode::SqlVectorV2) => {
            eprintln!(
                "warning: vector retrieval failed ({err}); search is degraded to FTS-only mode for this query"
            );
            tracing::debug!("vector retrieval failed: {err}");
            RetrievalBackend::select_scoped(conn, None, scope.clone())
                .await?
                .search(query, None)
                .await?
        }
        Err(err) => return Err(err),
    };

    if candidates.vector_results.is_empty()
        && candidates.fts_results.is_empty()
        && symbol_results.is_empty()
    {
        return Ok(Vec::new());
    }

    let ranked = rank_candidates(
        candidates.vector_results,
        candidates.fts_results,
        symbol_results,
        query,
        intent,
        limit,
    );

    hydrate_ranked_candidates(conn, ranked).await
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

fn build_symbol_variants(query: &str, intent: QueryIntent) -> Vec<String> {
    let words = query_words(query);
    if words.is_empty() || words.len() > 4 || words.iter().all(|word| word.len() < 2) {
        return Vec::new();
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

fn query_words(query: &str) -> Vec<String> {
    query
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|word| !word.is_empty())
        .map(|word| word.to_string())
        .collect()
}

async fn hydrate_ranked_candidates(
    conn: &Connection,
    ranked: Vec<RankedCandidate>,
) -> Result<Vec<SearchResult>, OneupError> {
    let mut results = Vec::with_capacity(ranked.len());

    for ranked_candidate in ranked {
        let segment = get_segment_by_id(conn, &ranked_candidate.candidate.segment_id)
            .await?
            .ok_or_else(|| {
                SearchError::QueryFailed(format!(
                    "ranked segment '{}' disappeared before hydration",
                    ranked_candidate.candidate.segment_id
                ))
            })?;
        let mut result = search_result_from_segment(segment);
        result.score = normalize_score(ranked_candidate.score);
        results.push(result);
    }

    Ok(results)
}

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

fn candidate_key(candidate: &CandidateRow) -> String {
    candidate.segment_id.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::shared::types::BranchStatus;

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
}
