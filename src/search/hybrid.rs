use libsql::Connection;

use crate::indexer::embedder::Embedder;
use crate::search::intent::detect_intent;
use crate::search::intent::QueryIntent;
use crate::search::ranking::fuse_results;
use crate::search::retrieval::{RetrievalBackend, RetrievalMode};
use crate::search::symbol::SymbolSearchEngine;
use crate::shared::errors::{OneupError, SearchError};
use crate::shared::symbols::normalize_symbolish;
use crate::shared::types::{ReferenceKind, SearchResult, SymbolResult};

pub struct HybridSearchEngine<'a> {
    conn: &'a Connection,
    embedder: Option<&'a mut Embedder>,
}

impl<'a> HybridSearchEngine<'a> {
    pub fn new(conn: &'a Connection, embedder: Option<&'a mut Embedder>) -> Self {
        Self { conn, embedder }
    }

    pub async fn search(
        &mut self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchResult>, OneupError> {
        let query_embedding = self.embed_query(query);
        execute_search(self.conn, query, limit, query_embedding.as_deref()).await
    }

    pub async fn fts_only_search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchResult>, OneupError> {
        execute_search(self.conn, query, limit, None).await
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
    query: &str,
    limit: usize,
    query_embedding: Option<&[f32]>,
) -> Result<Vec<SearchResult>, OneupError> {
    if query.trim().is_empty() {
        return Err(SearchError::InvalidQuery("empty query".to_string()).into());
    }

    let intent = detect_intent(query);
    let symbol_results = symbol_search(conn, query, intent).await?;
    let backend = RetrievalBackend::select(conn, query_embedding).await?;
    let candidates = match backend.search(query, query_embedding).await {
        Ok(candidates) => candidates,
        Err(err) if matches!(backend.mode(), RetrievalMode::SqlVectorV2) => {
            eprintln!(
                "warning: vector retrieval failed ({err}); search is degraded to FTS-only mode for this query"
            );
            tracing::debug!("vector retrieval failed: {err}");
            RetrievalBackend::select(conn, None)
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

    Ok(fuse_results(
        candidates.vector_results,
        candidates.fts_results,
        symbol_results,
        query,
        intent,
        limit,
    ))
}

async fn symbol_search(
    conn: &Connection,
    query: &str,
    intent: QueryIntent,
) -> Result<Vec<SearchResult>, OneupError> {
    let variants = build_symbol_variants(query, intent);
    if variants.is_empty() {
        return Ok(Vec::new());
    }

    let engine = SymbolSearchEngine::new(conn);
    let include_usages = matches!(intent, QueryIntent::Usage);
    let mut matches = Vec::new();

    for variant in variants {
        let symbol_matches = if include_usages {
            engine.find_references(&variant).await?
        } else {
            engine.find_definitions(&variant).await?
        };

        for result in symbol_matches {
            if symbol_result_matches_query(&result, query) {
                matches.push(result);
            }
        }
    }

    matches.sort_by_key(|a| symbol_sort_key(a, query));

    let mut deduped = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for symbol in matches {
        let search_result = search_result_from_symbol(symbol);
        let key = candidate_key(&search_result);
        if seen.insert(key) {
            deduped.push(search_result);
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

fn symbol_result_matches_query(result: &SymbolResult, query: &str) -> bool {
    let normalized_query = normalize_symbolish(query);
    let normalized_name = normalize_symbolish(&result.name);

    normalized_name == normalized_query
        || normalized_name.contains(&normalized_query)
        || normalized_query.contains(&normalized_name)
}

fn symbol_sort_key(result: &SymbolResult, query: &str) -> (u8, u8, usize, usize, String) {
    let exactness = if normalize_symbolish(&result.name) == normalize_symbolish(query) {
        0
    } else {
        1
    };
    let ref_kind = match result.reference_kind {
        ReferenceKind::Definition => 0,
        ReferenceKind::Usage => 1,
    };

    (
        ref_kind,
        exactness,
        result.line_start,
        result.name.len(),
        result.file_path.clone(),
    )
}

fn search_result_from_symbol(result: SymbolResult) -> SearchResult {
    SearchResult {
        file_path: result.file_path,
        language: result.language,
        block_type: result.kind,
        content: result.content,
        score: 0.0,
        line_number: result.line_start,
        line_end: result.line_end,
        breadcrumb: result.breadcrumb,
        complexity: result.complexity,
        role: result.role,
        defined_symbols: result.defined_symbols,
        referenced_symbols: result.referenced_symbols,
        called_symbols: result.called_symbols,
    }
}

fn candidate_key(result: &SearchResult) -> String {
    format!(
        "{}:{}:{}",
        result.file_path, result.line_number, result.block_type
    )
}

#[cfg(test)]
mod tests {
    use super::*;

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
            called_symbols: "[]".to_string(),
            file_hash: "symbol123".to_string(),
        };
        crate::storage::segments::upsert_segment(&conn, &insert)
            .await
            .unwrap();

        let results = symbol_search(&conn, "config loader", QueryIntent::Definition)
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
            called_symbols: "[]".to_string(),
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
            called_symbols: "[]".to_string(),
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
}
