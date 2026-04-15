use libsql::{Connection, Row};

use crate::shared::constants::VECTOR_PREFILTER_K;
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
pub enum RetrievalMode {
    SqlVectorV2,
    FtsOnly,
}

pub struct RetrievedCandidates {
    pub vector_results: Vec<CandidateRow>,
    pub fts_results: Vec<CandidateRow>,
}

pub enum RetrievalBackend<'a> {
    SqlVectorV2(SqlVectorV2<'a>),
    FtsOnly(FtsOnly<'a>),
}

pub struct SqlVectorV2<'a> {
    conn: &'a Connection,
}

pub struct FtsOnly<'a> {
    conn: &'a Connection,
}

impl<'a> RetrievalBackend<'a> {
    pub async fn select(
        conn: &'a Connection,
        query_embedding: Option<&[f32]>,
    ) -> Result<Self, OneupError> {
        if query_embedding.is_some() && has_indexed_embeddings(conn).await? {
            Ok(Self::SqlVectorV2(SqlVectorV2 { conn }))
        } else {
            Ok(Self::FtsOnly(FtsOnly { conn }))
        }
    }

    pub fn mode(&self) -> RetrievalMode {
        match self {
            Self::SqlVectorV2(_) => RetrievalMode::SqlVectorV2,
            Self::FtsOnly(_) => RetrievalMode::FtsOnly,
        }
    }

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
    async fn search(
        &self,
        query: &str,
        query_embedding: &[f32],
    ) -> Result<RetrievedCandidates, OneupError> {
        let (vector_results, fts_results) = tokio::try_join!(
            fetch_vector_candidates(self.conn, query_embedding),
            fetch_fts_candidates(self.conn, query),
        )?;

        Ok(RetrievedCandidates {
            vector_results,
            fts_results,
        })
    }
}

impl<'a> FtsOnly<'a> {
    async fn search(&self, query: &str) -> Result<RetrievedCandidates, OneupError> {
        Ok(RetrievedCandidates {
            vector_results: Vec::new(),
            fts_results: fetch_fts_candidates(self.conn, query).await?,
        })
    }
}

async fn has_indexed_embeddings(conn: &Connection) -> Result<bool, OneupError> {
    let mut rows = conn
        .query(queries::SELECT_HAS_INDEXED_EMBEDDINGS, ())
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

async fn fetch_vector_candidates(
    conn: &Connection,
    query_embedding: &[f32],
) -> Result<Vec<CandidateRow>, OneupError> {
    let query_embedding = serialize_query_embedding(query_embedding)?;
    let mut rows = conn
        .query(
            queries::SELECT_VECTOR_CANDIDATES,
            libsql::params![query_embedding, VECTOR_PREFILTER_K as i64],
        )
        .await
        .map_err(|e| SearchError::QueryFailed(format!("vector search: {e}")))?;

    let mut results = Vec::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|e| SearchError::QueryFailed(format!("vector row iteration: {e}")))?
    {
        results.push(row_to_candidate_row(&row)?);
    }

    Ok(results)
}

async fn fetch_fts_candidates(
    conn: &Connection,
    query: &str,
) -> Result<Vec<CandidateRow>, OneupError> {
    let fts_query = build_fts_query(query);
    if fts_query.is_empty() {
        return Ok(Vec::new());
    }

    let mut rows = conn
        .query(
            queries::SELECT_FTS_CANDIDATES,
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
        results.push(row_to_candidate_row(&row)?);
    }

    Ok(results)
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

fn build_fts_query(query: &str) -> String {
    let terms: Vec<&str> = query
        .split_whitespace()
        .filter(|term| term.len() >= 2)
        .collect();

    if terms.is_empty() {
        return String::new();
    }

    terms
        .iter()
        .map(|term| {
            let cleaned: String = term
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if cleaned.is_empty() {
                String::new()
            } else {
                format!("\"{cleaned}\"")
            }
        })
        .filter(|term| !term.is_empty())
        .collect::<Vec<_>>()
        .join(" OR ")
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::storage::db::Db;
    use crate::storage::schema;

    fn embedding_with(values: &[(usize, f32)]) -> Vec<f32> {
        let mut embedding = vec![0.0; 384];
        for (idx, value) in values {
            embedding[*idx] = *value;
        }
        embedding
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
                conn.execute(
                    "INSERT INTO segment_vectors (segment_id, embedding_vec, created_at, updated_at)
                     VALUES (?1, vector(?2), datetime('now'), datetime('now'))",
                    libsql::params![id, embedding],
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
}
