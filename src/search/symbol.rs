use turso::Connection;

use crate::shared::errors::{OneupError, SearchError};
use crate::shared::types::{ReferenceKind, SymbolResult};
use crate::storage::queries;
use crate::storage::segments::row_to_stored_segment;

pub struct SymbolSearchEngine<'a> {
    conn: &'a Connection,
}

impl<'a> SymbolSearchEngine<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    /// Find symbol definitions matching the given name.
    /// Uses LIKE pattern matching on the defined_symbols JSON column,
    /// with post-query filtering for exact or fuzzy matches.
    pub async fn find_definitions(&self, name: &str) -> Result<Vec<SymbolResult>, OneupError> {
        let mut rows = self
            .conn
            .query(queries::SELECT_SYMBOLS_BY_DEFINED, [name])
            .await
            .map_err(|e| {
                SearchError::QueryFailed(format!("symbol definition query failed: {e}"))
            })?;

        let mut results = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| SearchError::QueryFailed(format!("row iteration failed: {e}")))?
        {
            let seg = row_to_stored_segment(&row)?;
            let defined = seg.parsed_defined_symbols();
            let matching = find_matching_symbols(&defined, name);
            for matched_name in matching {
                results.push(SymbolResult {
                    name: matched_name,
                    kind: seg.block_type.clone(),
                    file_path: seg.file_path.clone(),
                    line_start: seg.line_start as usize,
                    line_end: seg.line_end as usize,
                    content: seg.content.clone(),
                    reference_kind: ReferenceKind::Definition,
                });
            }
        }
        Ok(results)
    }

    /// Find both definitions and usages of a symbol.
    /// Definitions come from defined_symbols, usages from referenced_symbols.
    pub async fn find_references(&self, name: &str) -> Result<Vec<SymbolResult>, OneupError> {
        let mut results = self.find_definitions(name).await?;

        let mut rows = self
            .conn
            .query(queries::SELECT_SYMBOLS_BY_REFERENCED, [name])
            .await
            .map_err(|e| SearchError::QueryFailed(format!("symbol reference query failed: {e}")))?;

        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| SearchError::QueryFailed(format!("row iteration failed: {e}")))?
        {
            let seg = row_to_stored_segment(&row)?;
            let referenced = seg.parsed_referenced_symbols();
            let matching = find_matching_symbols(&referenced, name);
            for matched_name in matching {
                results.push(SymbolResult {
                    name: matched_name,
                    kind: seg.block_type.clone(),
                    file_path: seg.file_path.clone(),
                    line_start: seg.line_start as usize,
                    line_end: seg.line_end as usize,
                    content: seg.content.clone(),
                    reference_kind: ReferenceKind::Usage,
                });
            }
        }
        Ok(results)
    }
}

/// Find symbols in the list that match the query name.
/// Supports exact match and fuzzy/partial matching via Levenshtein distance.
fn find_matching_symbols(symbols: &[String], query: &str) -> Vec<String> {
    let query_lower = query.to_lowercase();
    let mut exact = Vec::new();
    let mut partial = Vec::new();

    for sym in symbols {
        let sym_lower = sym.to_lowercase();
        if sym_lower == query_lower {
            exact.push(sym.clone());
        } else if sym_lower.contains(&query_lower)
            || levenshtein(&sym_lower, &query_lower) <= max_edit_distance(query)
        {
            partial.push(sym.clone());
        }
    }

    if !exact.is_empty() {
        exact
    } else {
        partial
    }
}

/// Compute the maximum allowed edit distance for fuzzy matching.
/// Short names allow 1 edit; longer names allow 2.
fn max_edit_distance(query: &str) -> usize {
    if query.len() <= 4 {
        1
    } else {
        2
    }
}

/// Compute Levenshtein edit distance between two strings.
fn levenshtein(a: &str, b: &str) -> usize {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let m = a_chars.len();
    let n = b_chars.len();

    if m == 0 {
        return n;
    }
    if n == 0 {
        return m;
    }

    let mut prev = (0..=n).collect::<Vec<_>>();
    let mut curr = vec![0; n + 1];

    for i in 1..=m {
        curr[0] = i;
        for j in 1..=n {
            let cost = if a_chars[i - 1] == b_chars[j - 1] {
                0
            } else {
                1
            };
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[n]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{db::Db, schema, segments};

    async fn setup() -> (Db, Connection) {
        let db = Db::open_memory().await.unwrap();
        let conn = db.connect().unwrap();
        schema::initialize(&conn).await.unwrap();
        (db, conn)
    }

    fn make_segment(
        id: &str,
        file_path: &str,
        block_type: &str,
        defined: &str,
        referenced: &str,
    ) -> segments::SegmentInsert {
        segments::SegmentInsert {
            id: id.to_string(),
            file_path: file_path.to_string(),
            language: "rust".to_string(),
            block_type: block_type.to_string(),
            content: format!("fn {id}() {{ }}"),
            line_start: 1,
            line_end: 5,
            embedding: None,
            embedding_q8: None,
            complexity: 1,
            role: "DEFINITION".to_string(),
            defined_symbols: defined.to_string(),
            referenced_symbols: referenced.to_string(),
            file_hash: "abc123".to_string(),
        }
    }

    #[tokio::test]
    async fn find_exact_definition() {
        let (_db, conn) = setup().await;

        let seg = make_segment("s1", "src/lib.rs", "function", r#"["my_func"]"#, "[]");
        segments::upsert_segment(&conn, &seg).await.unwrap();

        let engine = SymbolSearchEngine::new(&conn);
        let results = engine.find_definitions("my_func").await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "my_func");
        assert_eq!(results[0].kind, "function");
        assert_eq!(results[0].reference_kind, ReferenceKind::Definition);
    }

    #[tokio::test]
    async fn find_partial_definition() {
        let (_db, conn) = setup().await;

        let seg = make_segment(
            "s1",
            "src/lib.rs",
            "function",
            r#"["calculate_total"]"#,
            "[]",
        );
        segments::upsert_segment(&conn, &seg).await.unwrap();

        let engine = SymbolSearchEngine::new(&conn);
        let results = engine.find_definitions("total").await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "calculate_total");
    }

    #[tokio::test]
    async fn definitions_ordered_by_block_type_priority() {
        let (_db, conn) = setup().await;

        let s1 = make_segment("s1", "src/a.rs", "module", r#"["Config"]"#, "[]");
        let s2 = make_segment("s2", "src/b.rs", "struct", r#"["Config"]"#, "[]");
        segments::upsert_segment(&conn, &s1).await.unwrap();
        segments::upsert_segment(&conn, &s2).await.unwrap();

        let engine = SymbolSearchEngine::new(&conn);
        let results = engine.find_definitions("Config").await.unwrap();
        assert_eq!(results.len(), 2);
        // struct is in the priority group (CASE 0), module is not (CASE 1)
        assert_eq!(results[0].kind, "struct");
        assert_eq!(results[1].kind, "module");
    }

    #[tokio::test]
    async fn find_references_includes_definitions_and_usages() {
        let (_db, conn) = setup().await;

        let s1 = make_segment("s1", "src/lib.rs", "struct", r#"["MyType"]"#, "[]");
        let s2 = make_segment("s2", "src/main.rs", "function", "[]", r#"["MyType"]"#);
        segments::upsert_segment(&conn, &s1).await.unwrap();
        segments::upsert_segment(&conn, &s2).await.unwrap();

        let engine = SymbolSearchEngine::new(&conn);
        let results = engine.find_references("MyType").await.unwrap();
        assert_eq!(results.len(), 2);

        let defs: Vec<_> = results
            .iter()
            .filter(|r| r.reference_kind == ReferenceKind::Definition)
            .collect();
        let usages: Vec<_> = results
            .iter()
            .filter(|r| r.reference_kind == ReferenceKind::Usage)
            .collect();
        assert_eq!(defs.len(), 1);
        assert_eq!(usages.len(), 1);
        assert_eq!(defs[0].file_path, "src/lib.rs");
        assert_eq!(usages[0].file_path, "src/main.rs");
    }

    #[tokio::test]
    async fn references_excludes_segments_already_in_definitions() {
        let (_db, conn) = setup().await;

        let s1 = make_segment(
            "s1",
            "src/lib.rs",
            "function",
            r#"["process"]"#,
            r#"["process"]"#,
        );
        segments::upsert_segment(&conn, &s1).await.unwrap();

        let engine = SymbolSearchEngine::new(&conn);
        let results = engine.find_references("process").await.unwrap();
        let defs: Vec<_> = results
            .iter()
            .filter(|r| r.reference_kind == ReferenceKind::Definition)
            .collect();
        let usages: Vec<_> = results
            .iter()
            .filter(|r| r.reference_kind == ReferenceKind::Usage)
            .collect();
        assert_eq!(defs.len(), 1);
        assert_eq!(usages.len(), 0);
    }

    #[tokio::test]
    async fn no_results_for_unknown_symbol() {
        let (_db, conn) = setup().await;

        let seg = make_segment("s1", "src/lib.rs", "function", r#"["known"]"#, "[]");
        segments::upsert_segment(&conn, &seg).await.unwrap();

        let engine = SymbolSearchEngine::new(&conn);
        let results = engine.find_definitions("unknown").await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn fuzzy_match_with_levenshtein() {
        let (_db, conn) = setup().await;

        let seg = make_segment("s1", "src/lib.rs", "function", r#"["process_data"]"#, "[]");
        segments::upsert_segment(&conn, &seg).await.unwrap();

        let engine = SymbolSearchEngine::new(&conn);
        let results = engine.find_definitions("process_dat").await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "process_data");
    }

    #[test]
    fn levenshtein_distance() {
        assert_eq!(levenshtein("", ""), 0);
        assert_eq!(levenshtein("abc", "abc"), 0);
        assert_eq!(levenshtein("abc", "ab"), 1);
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("", "hello"), 5);
    }

    #[test]
    fn find_matching_symbols_exact_preferred() {
        let symbols = vec!["foo".to_string(), "foobar".to_string()];
        let result = find_matching_symbols(&symbols, "foo");
        assert_eq!(result, vec!["foo"]);
    }

    #[test]
    fn find_matching_symbols_partial_when_no_exact() {
        let symbols = vec!["foobar".to_string(), "bazfoo".to_string()];
        let result = find_matching_symbols(&symbols, "foo");
        assert_eq!(result.len(), 2);
    }
}
