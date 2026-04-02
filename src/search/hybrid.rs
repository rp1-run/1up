use libsql::Connection;

use crate::indexer::embedder::Embedder;
use crate::search::intent::detect_intent;
use crate::search::intent::QueryIntent;
use crate::search::ranking::fuse_results;
use crate::shared::constants::VECTOR_PREFILTER_K;
use crate::shared::errors::{OneupError, SearchError};
use crate::shared::types::{ReferenceKind, SearchResult, SegmentRole, SymbolResult};
use crate::search::symbol::SymbolSearchEngine;

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
        if query.trim().is_empty() {
            return Err(SearchError::InvalidQuery("empty query".to_string()).into());
        }

        let intent = detect_intent(query);
        let symbol_results = symbol_search(self.conn, query, intent).await?;

        let vector_results = if let Some(ref mut embedder) = self.embedder {
            vector_search(self.conn, embedder, query).await?
        } else {
            Vec::new()
        };

        let fts_results = fts_search(self.conn, query).await?;

        if vector_results.is_empty() && fts_results.is_empty() && symbol_results.is_empty() {
            return Ok(Vec::new());
        }

        Ok(fuse_results(
            vector_results,
            fts_results,
            symbol_results,
            intent,
            limit,
        ))
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
        let symbol_results = symbol_search(self.conn, query, intent).await?;
        let fts_results = fts_search(self.conn, query).await?;

        Ok(fuse_results(
            Vec::new(),
            fts_results,
            symbol_results,
            intent,
            limit,
        ))
    }
}

/// Cosine distance between two f32 vectors (1.0 - cosine_similarity).
fn cosine_distance(a: &[f32], b: &[f32]) -> f64 {
    let mut dot = 0.0f64;
    let mut norm_a = 0.0f64;
    let mut norm_b = 0.0f64;
    for (x, y) in a.iter().zip(b.iter()) {
        let (x, y) = (*x as f64, *y as f64);
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    let denom = norm_a.sqrt() * norm_b.sqrt();
    if denom == 0.0 { 1.0 } else { 1.0 - (dot / denom) }
}

/// Parse a JSON array string like "[0.1,0.2,0.3]" into Vec<f32>.
fn parse_embedding_json(json: &str) -> Option<Vec<f32>> {
    serde_json::from_str(json).ok()
}

async fn vector_search(
    conn: &Connection,
    embedder: &mut Embedder,
    query: &str,
) -> Result<Vec<SearchResult>, OneupError> {
    let query_embedding = embedder.embed_one(query)?;

    // Use FTS to prefilter candidates, then rerank by cosine distance.
    // This avoids loading all embeddings into memory.
    let fts_query = build_fts_query(query);
    let prefilter_limit = (VECTOR_PREFILTER_K * 2) as i64;

    let sql = if fts_query.is_empty() {
        // No FTS terms — fall back to a random sample
        "SELECT s.id, s.file_path, s.language, s.block_type, s.content,
                s.line_start, s.line_end, s.breadcrumb, s.complexity,
                s.role, s.defined_symbols, s.referenced_symbols, s.called_symbols,
                s.embedding
         FROM segments s
         WHERE s.embedding IS NOT NULL
         LIMIT ?1".to_string()
    } else {
        // Prefilter with FTS, then rerank by vector distance
        "SELECT s.id, s.file_path, s.language, s.block_type, s.content,
                s.line_start, s.line_end, s.breadcrumb, s.complexity,
                s.role, s.defined_symbols, s.referenced_symbols, s.called_symbols,
                s.embedding
         FROM segments_fts f
         JOIN segments s ON s.rowid = f.rowid
         WHERE segments_fts MATCH ?2 AND s.embedding IS NOT NULL
         LIMIT ?1".to_string()
    };

    let mut rows = if fts_query.is_empty() {
        conn.query(&sql, libsql::params![prefilter_limit])
            .await
            .map_err(|e| SearchError::QueryFailed(format!("vector search: {e}")))?
    } else {
        conn.query(&sql, libsql::params![prefilter_limit, fts_query])
            .await
            .map_err(|e| SearchError::QueryFailed(format!("vector search: {e}")))?
    };

    let mut candidates: Vec<(SearchResult, f64)> = Vec::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|e| SearchError::QueryFailed(format!("row iteration: {e}")))?
    {
        let embedding_json: String = row
            .get(13)
            .map_err(|e| SearchError::QueryFailed(e.to_string()))?;

        if let Some(embedding) = parse_embedding_json(&embedding_json) {
            let distance = cosine_distance(&query_embedding, &embedding);
            let result = row_to_search_result(&row)?;
            candidates.push((result, distance));
        }
    }

    candidates.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    candidates.truncate(VECTOR_PREFILTER_K);

    Ok(candidates.into_iter().map(|(r, _)| r).collect())
}

async fn fts_search(conn: &Connection, query: &str) -> Result<Vec<SearchResult>, OneupError> {
    let fts_query = build_fts_query(query);

    let mut rows = conn
        .query(
            "SELECT s.id, s.file_path, s.language, s.block_type, s.content,
                    s.line_start, s.line_end, s.breadcrumb, s.complexity,
                    s.role, s.defined_symbols, s.referenced_symbols, s.called_symbols,
                    f.rank as score
             FROM segments_fts f
             JOIN segments s ON s.rowid = f.rowid
             WHERE segments_fts MATCH ?1
             ORDER BY f.rank
             LIMIT ?2",
            libsql::params![fts_query, VECTOR_PREFILTER_K as i64],
        )
        .await
        .map_err(|e| SearchError::QueryFailed(format!("FTS search: {e}")))?;

    let mut results = Vec::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|e| SearchError::QueryFailed(format!("FTS row iteration: {e}")))?
    {
        results.push(row_to_search_result(&row)?);
    }

    Ok(results)
}

fn row_to_search_result(row: &libsql::Row) -> Result<SearchResult, OneupError> {
    let file_path: String = row
        .get(1)
        .map_err(|e| SearchError::QueryFailed(e.to_string()))?;
    let language: String = row
        .get(2)
        .map_err(|e| SearchError::QueryFailed(e.to_string()))?;
    let block_type: String = row
        .get(3)
        .map_err(|e| SearchError::QueryFailed(e.to_string()))?;
    let content: String = row
        .get(4)
        .map_err(|e| SearchError::QueryFailed(e.to_string()))?;
    let line_start: i64 = row
        .get(5)
        .map_err(|e| SearchError::QueryFailed(e.to_string()))?;
    let line_end: i64 = row
        .get(6)
        .map_err(|e| SearchError::QueryFailed(e.to_string()))?;
    let breadcrumb: Option<String> = row
        .get(7)
        .map_err(|e| SearchError::QueryFailed(e.to_string()))?;
    let complexity: i64 = row
        .get(8)
        .map_err(|e| SearchError::QueryFailed(e.to_string()))?;
    let role_str: String = row
        .get(9)
        .map_err(|e| SearchError::QueryFailed(e.to_string()))?;
    let defined_symbols: String = row
        .get(10)
        .map_err(|e| SearchError::QueryFailed(e.to_string()))?;
    let referenced_symbols: String = row
        .get(11)
        .map_err(|e| SearchError::QueryFailed(e.to_string()))?;
    let called_symbols: String = row
        .get(12)
        .map_err(|e| SearchError::QueryFailed(e.to_string()))?;

    let role = parse_role(&role_str);

    let def_syms: Vec<String> = serde_json::from_str(&defined_symbols).unwrap_or_default();
    let ref_syms: Vec<String> = serde_json::from_str(&referenced_symbols).unwrap_or_default();
    let call_syms: Vec<String> = serde_json::from_str(&called_symbols).unwrap_or_default();

    Ok(SearchResult {
        file_path,
        language,
        block_type,
        content,
        score: 0.0,
        line_number: line_start as usize,
        line_end: line_end as usize,
        breadcrumb,
        complexity: Some(complexity as u32),
        role,
        defined_symbols: some_if_not_empty(def_syms),
        referenced_symbols: some_if_not_empty(ref_syms),
        called_symbols: some_if_not_empty(call_syms),
    })
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
    if words.is_empty() || words.len() > 4 {
        return Vec::new();
    }

    let symbolish = query.contains('_')
        || query.chars().any(|c| c.is_uppercase())
        || matches!(intent, QueryIntent::Definition | QueryIntent::Usage)
        || words.len() <= 2;

    if !symbolish {
        return Vec::new();
    }

    let mut variants = Vec::new();

    for word in &words {
        if word.len() >= 2 {
            variants.push(word.clone());
        }
    }

    if !words.is_empty() {
        let snake = words.join("_");
        let compact = words.join("");
        let pascal = words
            .iter()
            .map(|word| capitalize(word))
            .collect::<Vec<_>>()
            .join("");

        variants.push(snake);
        variants.push(compact);
        variants.push(pascal.clone());

        if let Some((first, rest)) = words.split_first() {
            let camel = format!(
                "{}{}",
                first.to_lowercase(),
                rest.iter().map(|word| capitalize(word)).collect::<String>()
            );
            variants.push(camel);
        }
    }

    let mut deduped = Vec::new();
    for variant in variants {
        if variant.len() >= 2 && !deduped.contains(&variant) {
            deduped.push(variant);
        }
    }

    deduped
}

fn query_words(query: &str) -> Vec<String> {
    query
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|word| !word.is_empty())
        .map(|word| word.to_string())
        .collect()
}

fn capitalize(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str().to_lowercase()),
        None => String::new(),
    }
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

fn normalize_symbolish(value: &str) -> String {
    value
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
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

fn candidate_key(result: &SearchResult) -> String {
    format!(
        "{}:{}:{}",
        result.file_path, result.line_number, result.block_type
    )
}

fn build_fts_query(query: &str) -> String {
    let terms: Vec<&str> = query.split_whitespace().filter(|t| t.len() >= 2).collect();

    if terms.is_empty() {
        return query.to_string();
    }

    terms
        .iter()
        .map(|t| {
            let cleaned: String = t
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if cleaned.is_empty() {
                String::new()
            } else {
                format!("\"{}\"", cleaned)
            }
        })
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" OR ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f32_to_json_array(vec: &[f32]) -> String {
        let parts: Vec<String> = vec.iter().map(|v| format!("{v}")).collect();
        format!("[{}]", parts.join(","))
    }

    fn f32_to_q8_json(vec: &[f32]) -> String {
        let max_abs = vec.iter().map(|v| v.abs()).fold(0.0f32, f32::max).max(1e-10);
        let scale = 127.0 / max_abs;
        let parts: Vec<String> = vec.iter().map(|v| format!("{}", (v * scale) as i8 as u8)).collect();
        format!("[{}]", parts.join(","))
    }

    #[test]
    fn fts_query_building() {
        let q = build_fts_query("error handling network");
        assert!(q.contains("\"error\""));
        assert!(q.contains("\"handling\""));
        assert!(q.contains("\"network\""));
        assert!(q.contains(" OR "));
    }

    #[test]
    fn fts_query_skips_short_terms() {
        let q = build_fts_query("a is the error");
        assert!(q.contains("\"is\""));
        assert!(q.contains("\"the\""));
        assert!(q.contains("\"error\""));
        assert!(!q.contains("\"a\""));
    }

    #[test]
    fn fts_query_handles_empty() {
        let q = build_fts_query("");
        assert_eq!(q, "");
    }

    #[test]
    fn q8_json_produces_correct_format() {
        let vec = vec![0.1f32, -0.5, 0.3, 0.0, 1.0];
        let json = f32_to_q8_json(&vec);
        assert!(json.starts_with('['));
        assert!(json.ends_with(']'));
        let parsed: Vec<u8> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.len(), vec.len());
    }

    #[test]
    fn f32_json_array_format() {
        let vec = vec![1.0f32, 2.0, 3.0];
        let json = f32_to_json_array(&vec);
        assert!(json.starts_with('['));
        assert!(json.ends_with(']'));
        let parsed: Vec<f32> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, vec);
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
            embedding: None,
            embedding_q8: None,
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
            embedding: None,
            embedding_q8: None,
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
