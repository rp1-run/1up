use libsql::Connection;

use crate::indexer::embedder::Embedder;
use crate::search::intent::detect_intent;
use crate::search::ranking::fuse_results;
use crate::shared::constants::VECTOR_PREFILTER_K;
use crate::shared::errors::{OneupError, SearchError};
use crate::shared::types::{SearchResult, SegmentRole};

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

        let vector_results = if let Some(ref mut embedder) = self.embedder {
            vector_search(self.conn, *embedder, query).await?
        } else {
            Vec::new()
        };

        let fts_results = fts_search(self.conn, query).await?;

        if vector_results.is_empty() && fts_results.is_empty() {
            return Ok(Vec::new());
        }

        Ok(fuse_results(vector_results, fts_results, intent, limit))
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
        let fts_results = fts_search(self.conn, query).await?;

        Ok(fuse_results(Vec::new(), fts_results, intent, limit))
    }
}

async fn vector_search(
    conn: &Connection,
    embedder: &mut Embedder,
    query: &str,
) -> Result<Vec<SearchResult>, OneupError> {
    let query_embedding = embedder.embed_one(query)?;
    let query_q8_bytes = f32_to_q8_bytes(&query_embedding);

    let prefilter_k = VECTOR_PREFILTER_K as i64;

    let mut rows = conn
        .query(
            "SELECT s.id, s.file_path, s.language, s.block_type, s.content,
                    s.line_start, s.line_end, s.role, s.defined_symbols,
                    s.referenced_symbols,
                    vector_distance_cos(s.embedding_q8, ?1) as distance
             FROM segments s
             WHERE s.embedding_q8 IS NOT NULL
             ORDER BY distance ASC
             LIMIT ?2",
            libsql::params![libsql::Value::Blob(query_q8_bytes), prefilter_k],
        )
        .await
        .map_err(|e| SearchError::QueryFailed(format!("int8 prefilter: {e}")))?;

    let mut candidates: Vec<(String, SearchResult, f64)> = Vec::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|e| SearchError::QueryFailed(format!("row iteration: {e}")))?
    {
        let id: String = row
            .get(0)
            .map_err(|e| SearchError::QueryFailed(e.to_string()))?;
        let distance: f64 = row
            .get(10)
            .map_err(|e| SearchError::QueryFailed(e.to_string()))?;
        let result = row_to_search_result(&row)?;
        candidates.push((id, result, distance));
    }

    if candidates.is_empty() {
        return Ok(Vec::new());
    }

    let ids: Vec<String> = candidates.iter().map(|(id, _, _)| id.clone()).collect();
    let mut reranked = rerank_f32(conn, &ids, &query_embedding).await?;

    for (id, result, _) in &candidates {
        if !reranked.iter().any(|(rid, _)| rid == id) {
            reranked.push((id.clone(), f64::MAX));
        }
        let _ = result;
    }

    reranked.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

    let id_to_result: std::collections::HashMap<String, SearchResult> = candidates
        .into_iter()
        .map(|(id, result, _)| (id, result))
        .collect();

    Ok(reranked
        .into_iter()
        .filter_map(|(id, _)| id_to_result.get(&id).cloned())
        .collect())
}

async fn rerank_f32(
    conn: &Connection,
    ids: &[String],
    query_embedding: &[f32],
) -> Result<Vec<(String, f64)>, OneupError> {
    let query_vec_bytes = f32_vec_to_bytes(query_embedding);
    let mut results = Vec::new();

    for id in ids {
        let mut rows = conn
            .query(
                "SELECT vector_distance_cos(embedding, ?1) as distance
                 FROM segments
                 WHERE id = ?2 AND embedding IS NOT NULL",
                libsql::params![libsql::Value::Blob(query_vec_bytes.clone()), id.clone()],
            )
            .await
            .map_err(|e| SearchError::QueryFailed(format!("f32 rerank: {e}")))?;

        if let Some(row) = rows
            .next()
            .await
            .map_err(|e| SearchError::QueryFailed(format!("rerank row: {e}")))?
        {
            let distance: f64 = row
                .get(0)
                .map_err(|e| SearchError::QueryFailed(e.to_string()))?;
            results.push((id.clone(), distance));
        }
    }

    Ok(results)
}

async fn fts_search(conn: &Connection, query: &str) -> Result<Vec<SearchResult>, OneupError> {
    let fts_query = build_fts_query(query);

    let mut rows = conn
        .query(
            "SELECT s.id, s.file_path, s.language, s.block_type, s.content,
                    s.line_start, s.line_end, s.role, s.defined_symbols,
                    s.referenced_symbols,
                    rank
             FROM segments_fts
             JOIN segments s ON segments_fts.rowid = s.rowid
             WHERE segments_fts MATCH ?1
             ORDER BY rank
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
    let role_str: String = row
        .get(7)
        .map_err(|e| SearchError::QueryFailed(e.to_string()))?;
    let defined_symbols: String = row
        .get(8)
        .map_err(|e| SearchError::QueryFailed(e.to_string()))?;
    let referenced_symbols: String = row
        .get(9)
        .map_err(|e| SearchError::QueryFailed(e.to_string()))?;

    let role = match role_str.as_str() {
        "DEFINITION" => Some(SegmentRole::Definition),
        "IMPLEMENTATION" => Some(SegmentRole::Implementation),
        "ORCHESTRATION" => Some(SegmentRole::Orchestration),
        "IMPORT" => Some(SegmentRole::Import),
        "DOCS" => Some(SegmentRole::Docs),
        _ => None,
    };

    let def_syms: Vec<String> = serde_json::from_str(&defined_symbols).unwrap_or_default();
    let ref_syms: Vec<String> = serde_json::from_str(&referenced_symbols).unwrap_or_default();

    Ok(SearchResult {
        file_path,
        language,
        block_type,
        content,
        score: 0.0,
        line_number: line_start as usize,
        role,
        defined_symbols: if def_syms.is_empty() {
            None
        } else {
            Some(def_syms)
        },
        referenced_symbols: if ref_syms.is_empty() {
            None
        } else {
            Some(ref_syms)
        },
    })
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

fn f32_vec_to_bytes(vec: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(vec.len() * 4);
    for &val in vec {
        bytes.extend_from_slice(&val.to_le_bytes());
    }
    bytes
}

fn f32_to_q8_bytes(vec: &[f32]) -> Vec<u8> {
    let max_abs = vec
        .iter()
        .map(|v| v.abs())
        .fold(0.0f32, f32::max)
        .max(1e-10);

    let scale = 127.0 / max_abs;
    vec.iter().map(|v| (v * scale) as i8 as u8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn q8_quantization_produces_correct_length() {
        let vec = vec![0.1f32, -0.5, 0.3, 0.0, 1.0];
        let q8 = f32_to_q8_bytes(&vec);
        assert_eq!(q8.len(), vec.len());
    }

    #[test]
    fn q8_quantization_matches_pipeline() {
        let vec = vec![0.5f32, -1.0, 0.0, 0.25, -0.75];
        let q8 = f32_to_q8_bytes(&vec);
        let max_abs = 1.0f32;
        let scale = 127.0 / max_abs;
        let expected: Vec<u8> = vec.iter().map(|v| (v * scale) as i8 as u8).collect();
        assert_eq!(q8, expected);
    }

    #[test]
    fn f32_to_bytes_roundtrip() {
        let vec = vec![1.0f32, 2.0, 3.0];
        let bytes = f32_vec_to_bytes(&vec);
        assert_eq!(bytes.len(), 12);

        let reconstructed: Vec<f32> = bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        assert_eq!(reconstructed, vec);
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
            complexity: 1,
            role: "DEFINITION".to_string(),
            defined_symbols: "[\"handle_error\"]".to_string(),
            referenced_symbols: "[]".to_string(),
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
            complexity: 1,
            role: "DEFINITION".to_string(),
            defined_symbols: "[\"validate_input\"]".to_string(),
            referenced_symbols: "[]".to_string(),
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
