use std::collections::HashSet;

use libsql::Connection;

use crate::search::retrieval::CandidateRow;
use crate::shared::errors::{OneupError, SearchError};
use crate::shared::symbols::normalize_symbolish;
use crate::shared::types::{ReferenceKind, SymbolResult};
use crate::storage::queries;
use crate::storage::segments::row_to_stored_segment;

const SYMBOL_FALLBACK_CANONICAL_LIMIT: i64 = 32;
const SYMBOL_PREFIX_SEED_LEN: usize = 3;

pub struct SymbolSearchEngine<'a> {
    conn: &'a Connection,
}

struct SymbolMatch {
    segment_id: String,
    result: SymbolResult,
    candidate: CandidateRow,
}

impl<'a> SymbolSearchEngine<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    pub async fn find_definitions(
        &self,
        name: &str,
        fuzzy: bool,
    ) -> Result<Vec<SymbolResult>, OneupError> {
        Ok(self
            .find_matches(name, ReferenceKind::Definition, fuzzy)
            .await?
            .into_iter()
            .map(|symbol_match| symbol_match.result)
            .collect())
    }

    pub async fn find_references(
        &self,
        name: &str,
        fuzzy: bool,
    ) -> Result<Vec<SymbolResult>, OneupError> {
        Ok(self
            .find_reference_matches(name, fuzzy)
            .await?
            .into_iter()
            .map(|symbol_match| symbol_match.result)
            .collect())
    }

    pub(crate) async fn find_definition_candidates(
        &self,
        name: &str,
        fuzzy: bool,
    ) -> Result<Vec<CandidateRow>, OneupError> {
        Ok(self
            .find_matches(name, ReferenceKind::Definition, fuzzy)
            .await?
            .into_iter()
            .map(candidate_from_symbol_match)
            .collect())
    }

    pub(crate) async fn find_definition_candidates_by_canonical(
        &self,
        canonical_symbol: &str,
    ) -> Result<Vec<CandidateRow>, OneupError> {
        let canonical_symbol = normalize_symbolish(canonical_symbol);
        if canonical_symbol.is_empty() {
            return Ok(Vec::new());
        }

        let mut seen = HashSet::new();
        Ok(self
            .load_matches(ReferenceKind::Definition, &canonical_symbol)
            .await?
            .into_iter()
            .filter_map(|symbol_match| {
                seen.insert(symbol_match.segment_id.clone())
                    .then(|| candidate_from_symbol_match(symbol_match))
            })
            .collect())
    }

    pub(crate) async fn find_reference_candidates(
        &self,
        name: &str,
        fuzzy: bool,
    ) -> Result<Vec<CandidateRow>, OneupError> {
        Ok(self
            .find_reference_matches(name, fuzzy)
            .await?
            .into_iter()
            .map(candidate_from_symbol_match)
            .collect())
    }

    async fn find_matches(
        &self,
        query: &str,
        reference_kind: ReferenceKind,
        fuzzy: bool,
    ) -> Result<Vec<SymbolMatch>, OneupError> {
        let canonical_query = normalize_symbolish(query);
        if canonical_query.is_empty() {
            return Ok(Vec::new());
        }

        let exact = self.load_matches(reference_kind, &canonical_query).await?;
        if !exact.is_empty() {
            return Ok(exact);
        }

        if !fuzzy {
            return Ok(Vec::new());
        }

        let fallback_canonicals = self
            .load_fallback_canonicals(reference_kind, &canonical_query)
            .await?;
        let matching_canonicals = find_matching_symbols(&fallback_canonicals, &canonical_query);

        let mut results = Vec::new();
        let mut seen = HashSet::new();

        for canonical_symbol in matching_canonicals {
            for symbol_match in self.load_matches(reference_kind, &canonical_symbol).await? {
                let dedupe_key =
                    format!("{}:{}", symbol_match.segment_id, symbol_match.result.name);
                if seen.insert(dedupe_key) {
                    results.push(symbol_match);
                }
            }
        }

        Ok(results)
    }

    async fn load_matches(
        &self,
        reference_kind: ReferenceKind,
        canonical_symbol: &str,
    ) -> Result<Vec<SymbolMatch>, OneupError> {
        let mut rows = self
            .conn
            .query(
                queries::SELECT_SYMBOL_MATCHES_BY_CANONICAL,
                libsql::params![reference_kind_label(reference_kind), canonical_symbol],
            )
            .await
            .map_err(|e| SearchError::QueryFailed(format!("symbol lookup failed: {e}")))?;

        let mut results = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| SearchError::QueryFailed(format!("row iteration failed: {e}")))?
        {
            let seg = row_to_stored_segment(&row)?;
            let matched_name: String = row
                .get(16)
                .map_err(|e| SearchError::QueryFailed(format!("read symbol row failed: {e}")))?;
            let candidate = CandidateRow {
                segment_id: seg.id.clone(),
                file_path: seg.file_path.clone(),
                language: seg.language.clone(),
                block_type: seg.block_type.clone(),
                line_number: seg.line_start as usize,
                line_end: seg.line_end as usize,
                breadcrumb: seg.breadcrumb.clone(),
                complexity: Some(seg.complexity as u32),
                role: Some(seg.parsed_role()),
                defined_symbols: some_if_not_empty(seg.parsed_defined_symbols()),
                referenced_symbols: some_if_not_empty(seg.parsed_referenced_symbols()),
                called_symbols: some_if_not_empty(seg.parsed_called_symbols()),
            };
            results.push(SymbolMatch {
                segment_id: seg.id.clone(),
                result: SymbolResult {
                    segment_id: seg.id.clone(),
                    name: matched_name,
                    kind: seg.block_type.clone(),
                    file_path: seg.file_path.clone(),
                    language: seg.language.clone(),
                    line_start: seg.line_start as usize,
                    line_end: seg.line_end as usize,
                    content: seg.content.clone(),
                    reference_kind,
                    breadcrumb: seg.breadcrumb.clone(),
                },
                candidate,
            });
        }

        Ok(results)
    }

    async fn load_fallback_canonicals(
        &self,
        reference_kind: ReferenceKind,
        canonical_query: &str,
    ) -> Result<Vec<String>, OneupError> {
        let mut candidates = Vec::new();
        let mut seen = HashSet::new();
        let prefix_seed = prefix_seed(canonical_query);

        for value in self
            .load_canonical_rows(
                queries::SELECT_DISTINCT_SYMBOL_CANONICALS_BY_PREFIX,
                reference_kind,
                &prefix_seed,
            )
            .await?
        {
            if seen.insert(value.clone()) {
                candidates.push(value);
            }
        }

        for value in self
            .load_canonical_rows(
                queries::SELECT_DISTINCT_SYMBOL_CANONICALS_BY_CONTAINS,
                reference_kind,
                canonical_query,
            )
            .await?
        {
            if seen.insert(value.clone()) {
                candidates.push(value);
            }
        }

        Ok(candidates)
    }

    async fn load_canonical_rows(
        &self,
        query: &str,
        reference_kind: ReferenceKind,
        value: &str,
    ) -> Result<Vec<String>, OneupError> {
        let mut rows = self
            .conn
            .query(
                query,
                libsql::params![
                    reference_kind_label(reference_kind),
                    value,
                    SYMBOL_FALLBACK_CANONICAL_LIMIT,
                ],
            )
            .await
            .map_err(|e| SearchError::QueryFailed(format!("symbol fallback query failed: {e}")))?;

        let mut results = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| SearchError::QueryFailed(format!("row iteration failed: {e}")))?
        {
            results.push(row.get(0).map_err(|e| {
                SearchError::QueryFailed(format!("read canonical symbol failed: {e}"))
            })?);
        }

        Ok(results)
    }

    async fn find_reference_matches(
        &self,
        name: &str,
        fuzzy: bool,
    ) -> Result<Vec<SymbolMatch>, OneupError> {
        let definitions = self
            .find_matches(name, ReferenceKind::Definition, fuzzy)
            .await?;
        let definition_ids: HashSet<String> = definitions
            .iter()
            .map(|symbol_match| symbol_match.segment_id.clone())
            .collect();

        let mut results = definitions;
        let usages = self.find_matches(name, ReferenceKind::Usage, fuzzy).await?;
        for usage in usages {
            if !definition_ids.contains(&usage.segment_id) {
                results.push(usage);
            }
        }

        Ok(results)
    }
}

fn some_if_not_empty(values: Vec<String>) -> Option<Vec<String>> {
    if values.is_empty() {
        None
    } else {
        Some(values)
    }
}

fn reference_kind_label(reference_kind: ReferenceKind) -> &'static str {
    match reference_kind {
        ReferenceKind::Definition => "definition",
        ReferenceKind::Usage => "usage",
    }
}

fn prefix_seed(canonical_query: &str) -> String {
    canonical_query
        .chars()
        .take(SYMBOL_PREFIX_SEED_LEN)
        .collect()
}

fn find_matching_symbols(symbols: &[String], query: &str) -> Vec<String> {
    let canonical_query = normalize_symbolish(query);
    let mut exact = Vec::new();
    let mut partial = Vec::new();

    for symbol in symbols {
        let canonical_symbol = normalize_symbolish(symbol);
        if canonical_symbol == canonical_query {
            exact.push(symbol.clone());
        } else if canonical_symbol.contains(&canonical_query)
            || (canonical_query.contains(&canonical_symbol)
                && canonical_symbol.len() * 2 >= canonical_query.len())
            || levenshtein(&canonical_symbol, &canonical_query)
                <= max_edit_distance(&canonical_query)
        {
            partial.push(symbol.clone());
        }
    }

    if !exact.is_empty() {
        exact
    } else {
        partial
    }
}

fn candidate_from_symbol_match(symbol_match: SymbolMatch) -> CandidateRow {
    symbol_match.candidate
}

fn max_edit_distance(query: &str) -> usize {
    if query.len() <= 4 {
        1
    } else {
        2
    }
}

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
            embedding_vec: None,
            breadcrumb: None,
            complexity: 1,
            role: "DEFINITION".to_string(),
            defined_symbols: defined.to_string(),
            referenced_symbols: referenced.to_string(),
            referenced_relations: "[]".to_string(),
            called_symbols: "[]".to_string(),
            called_relations: "[]".to_string(),
            file_hash: "abc123".to_string(),
        }
    }

    #[tokio::test]
    async fn find_exact_definition() {
        let (_db, conn) = setup().await;

        let seg = make_segment("s1", "src/lib.rs", "function", r#"["my_func"]"#, "[]");
        segments::upsert_segment(&conn, &seg).await.unwrap();

        let engine = SymbolSearchEngine::new(&conn);
        let results = engine.find_definitions("my_func", false).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "my_func");
        assert_eq!(results[0].kind, "function");
        assert_eq!(results[0].reference_kind, ReferenceKind::Definition);
    }

    #[tokio::test]
    async fn find_canonical_definition() {
        let (_db, conn) = setup().await;

        let seg = make_segment("s1", "src/lib.rs", "struct", r#"["ConfigLoader"]"#, "[]");
        segments::upsert_segment(&conn, &seg).await.unwrap();

        let engine = SymbolSearchEngine::new(&conn);
        let results = engine
            .find_definitions("config_loader", false)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "ConfigLoader");
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
        let results = engine.find_definitions("total", true).await.unwrap();
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
        let results = engine.find_definitions("Config", false).await.unwrap();
        assert_eq!(results.len(), 2);
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
        let results = engine.find_references("MyType", false).await.unwrap();
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
        let results = engine.find_references("process", false).await.unwrap();
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
        let results = engine.find_definitions("unknown", false).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn fuzzy_match_with_levenshtein() {
        let (_db, conn) = setup().await;

        let seg = make_segment("s1", "src/lib.rs", "function", r#"["process_data"]"#, "[]");
        segments::upsert_segment(&conn, &seg).await.unwrap();

        let engine = SymbolSearchEngine::new(&conn);
        let results = engine.find_definitions("process_dat", true).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "process_data");
    }

    #[tokio::test]
    async fn canonical_definition_candidates_skip_fuzzy_fallback() {
        let (_db, conn) = setup().await;

        let exact = make_segment("s1", "src/auth.rs", "function", r#"["load_config"]"#, "[]");
        let partial = make_segment(
            "s2",
            "src/cache.rs",
            "function",
            r#"["load_configuration"]"#,
            "[]",
        );
        segments::upsert_segment(&conn, &exact).await.unwrap();
        segments::upsert_segment(&conn, &partial).await.unwrap();

        let engine = SymbolSearchEngine::new(&conn);
        let results = engine
            .find_definition_candidates_by_canonical("loadconfig")
            .await
            .unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].segment_id, "s1");
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

    #[tokio::test]
    async fn exact_only_returns_empty_when_no_exact_match() {
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
        let results = engine.find_definitions("total", false).await.unwrap();
        assert!(results.is_empty(), "exact-only should not fuzzy-match");
    }

    #[tokio::test]
    async fn fuzzy_flag_returns_results_when_exact_fails() {
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
        let results = engine.find_definitions("total", true).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "calculate_total");
    }

    #[test]
    fn reverse_containment_rejects_short_candidates() {
        // "create" (6 chars) is contained in "createhandler" (13 chars)
        // but 6*2=12 < 13, so the 50% length guard rejects it
        let symbols = vec!["create".to_string()];
        let result = find_matching_symbols(&symbols, "createhandler");
        assert!(result.is_empty(), "short substring should be rejected");

        // "createh" (7 chars) is contained in "createhandler" (13 chars)
        // and 7*2=14 >= 13, so it passes the guard
        let symbols = vec!["createh".to_string()];
        let result = find_matching_symbols(&symbols, "createhandler");
        assert_eq!(result, vec!["createh"]);
    }
}
