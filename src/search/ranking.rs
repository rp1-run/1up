use std::collections::HashMap;

use crate::search::intent::QueryIntent;
use crate::shared::constants::{
    MAX_RESULTS_PER_FILE, MAX_SEARCH_RESULTS, RRF_K, SYMBOL_WEIGHT, VECTOR_WEIGHT,
};
use crate::shared::types::SearchResult;

struct ScoredCandidate {
    result: SearchResult,
    vector_rank: Option<usize>,
    fts_rank: Option<usize>,
    symbol_rank: Option<usize>,
    fused_score: f64,
}

pub fn fuse_results(
    vector_results: Vec<SearchResult>,
    fts_results: Vec<SearchResult>,
    symbol_results: Vec<SearchResult>,
    query: &str,
    intent: QueryIntent,
    limit: usize,
) -> Vec<SearchResult> {
    let mut candidates: HashMap<String, ScoredCandidate> = HashMap::new();

    for (rank, r) in vector_results.into_iter().enumerate() {
        let key = candidate_key(&r);
        candidates.insert(
            key,
            ScoredCandidate {
                result: r,
                vector_rank: Some(rank),
                fts_rank: None,
                symbol_rank: None,
                fused_score: 0.0,
            },
        );
    }

    for (rank, r) in fts_results.into_iter().enumerate() {
        let key = candidate_key(&r);
        match candidates.get_mut(&key) {
            Some(existing) => {
                existing.fts_rank = Some(rank);
            }
            None => {
                candidates.insert(
                    key,
                    ScoredCandidate {
                        result: r,
                        vector_rank: None,
                        fts_rank: Some(rank),
                        symbol_rank: None,
                        fused_score: 0.0,
                    },
                );
            }
        }
    }

    for (rank, r) in symbol_results.into_iter().enumerate() {
        let key = candidate_key(&r);
        match candidates.get_mut(&key) {
            Some(existing) => {
                existing.symbol_rank = Some(rank);
            }
            None => {
                candidates.insert(
                    key,
                    ScoredCandidate {
                        result: r,
                        vector_rank: None,
                        fts_rank: None,
                        symbol_rank: Some(rank),
                        fused_score: 0.0,
                    },
                );
            }
        }
    }

    for candidate in candidates.values_mut() {
        candidate.fused_score = compute_rrf_score(candidate, query, intent);
    }

    let mut sorted: Vec<ScoredCandidate> = candidates.into_values().collect();
    sorted.sort_by(|a, b| {
        b.fused_score
            .partial_cmp(&a.fused_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let deduped = deduplicate(sorted);
    let capped = apply_per_file_cap(deduped, MAX_RESULTS_PER_FILE);

    let actual_limit = limit.min(MAX_SEARCH_RESULTS);
    capped
        .into_iter()
        .take(actual_limit)
        .map(|c| {
            let mut r = c.result;
            r.score = c.fused_score;
            r
        })
        .collect()
}

fn candidate_key(r: &SearchResult) -> String {
    format!("{}:{}:{}", r.file_path, r.line_number, r.block_type)
}

fn compute_rrf_score(candidate: &ScoredCandidate, query: &str, intent: QueryIntent) -> f64 {
    let vector_score = candidate
        .vector_rank
        .map(|rank| VECTOR_WEIGHT / (RRF_K + rank as f64 + 1.0))
        .unwrap_or(0.0);

    let fts_score = candidate
        .fts_rank
        .map(|rank| 1.0 / (RRF_K + rank as f64 + 1.0))
        .unwrap_or(0.0);

    let symbol_score = candidate
        .symbol_rank
        .map(|rank| SYMBOL_WEIGHT / (RRF_K + rank as f64 + 1.0))
        .unwrap_or(0.0);

    let mut score = vector_score + fts_score + symbol_score;

    score *= intent_boost(&candidate.result, query, intent);
    score *= file_path_boost(&candidate.result.file_path);
    score *= query_path_boost(query, &candidate.result.file_path);
    score *= short_segment_penalty(&candidate.result.content);

    score
}

fn intent_boost(result: &SearchResult, query: &str, intent: QueryIntent) -> f64 {
    let role_str = result
        .role
        .as_ref()
        .map(|r| format!("{:?}", r))
        .unwrap_or_default()
        .to_uppercase();

    match intent {
        QueryIntent::Definition => {
            if role_str == "DEFINITION" || result.is_definition_like() {
                1.3
            } else {
                1.0
            }
        }
        QueryIntent::Flow => {
            if role_str == "ORCHESTRATION" {
                1.3
            } else if role_str == "IMPLEMENTATION" {
                1.1
            } else {
                1.0
            }
        }
        QueryIntent::Usage => {
            if role_str == "IMPLEMENTATION"
                || role_str == "ORCHESTRATION"
                || result
                    .called_symbols
                    .as_ref()
                    .map(|calls| !calls.is_empty())
                    .unwrap_or(false)
            {
                1.2
            } else {
                1.0
            }
        }
        QueryIntent::Docs => {
            if role_str == "DOCS" {
                1.4
            } else {
                1.0
            }
        }
        QueryIntent::General => {
            if is_natural_language_query(query) {
                if role_str == "ORCHESTRATION" || role_str == "IMPLEMENTATION" {
                    1.15
                } else if role_str == "DOCS" {
                    1.05
                } else {
                    1.0
                }
            } else {
                1.0
            }
        }
    }
}

fn file_path_boost(path: &str) -> f64 {
    let lower = path.to_lowercase();
    if lower.contains("test") || lower.contains("spec") || lower.contains("__test") {
        0.7
    } else if lower.contains("doc") || lower.contains("readme") {
        0.8
    } else if lower.contains("vendor") || lower.contains("node_modules") {
        0.5
    } else {
        1.0
    }
}

fn short_segment_penalty(content: &str) -> f64 {
    let line_count = content.lines().count();
    if line_count <= 2 {
        0.6
    } else if line_count <= 5 {
        0.85
    } else {
        1.0
    }
}

fn query_path_boost(query: &str, path: &str) -> f64 {
    let terms = query_terms(query);
    if terms.is_empty() {
        return 1.0;
    }

    let lower_path = path.to_lowercase();
    let mut score = 1.0;

    let overlap_count = terms
        .iter()
        .filter(|term| lower_path.contains(term.as_str()))
        .count();
    score += 0.06 * overlap_count.min(3) as f64;

    let normalized_path = lower_path.replace(['/', '_', '-'], " ");
    let phrase = terms.join(" ");
    if terms.len() >= 2 && normalized_path.contains(&phrase) {
        score += 0.12;
    }

    score
}

fn is_natural_language_query(query: &str) -> bool {
    let terms = query_terms(query);
    terms.len() >= 2
        && !query.contains('_')
        && !query.chars().any(|c| c.is_uppercase())
        && !query.chars().any(|c| c.is_numeric())
}

fn query_terms(query: &str) -> Vec<String> {
    const STOP_WORDS: &[&str] = &[
        "a", "an", "and", "does", "for", "how", "in", "is", "of", "on", "per", "the", "to", "what",
        "where",
    ];

    query
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter_map(|term| {
            let term = term.to_lowercase();
            if term.len() < 3 || STOP_WORDS.contains(&term.as_str()) {
                None
            } else {
                Some(term)
            }
        })
        .collect()
}

fn deduplicate(candidates: Vec<ScoredCandidate>) -> Vec<ScoredCandidate> {
    let mut seen: Vec<(String, usize, usize)> = Vec::new();
    let mut result = Vec::new();

    for c in candidates {
        let file = &c.result.file_path;
        let start = c.result.line_number;
        let end = start + c.result.content.lines().count();

        let overlaps = seen
            .iter()
            .any(|(f, s, e)| f == file && ranges_overlap(start, end, *s, *e));

        if !overlaps {
            seen.push((file.clone(), start, end));
            result.push(c);
        }
    }

    result
}

fn ranges_overlap(s1: usize, e1: usize, s2: usize, e2: usize) -> bool {
    s1 < e2 && s2 < e1
}

fn apply_per_file_cap(candidates: Vec<ScoredCandidate>, cap: usize) -> Vec<ScoredCandidate> {
    let mut file_counts: HashMap<String, usize> = HashMap::new();
    let mut result = Vec::new();

    for c in candidates {
        let count = file_counts.entry(c.result.file_path.clone()).or_insert(0);
        if *count < cap {
            *count += 1;
            result.push(c);
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::types::SegmentRole;

    fn make_result(file: &str, line: usize, block_type: &str, content: &str) -> SearchResult {
        SearchResult {
            file_path: file.to_string(),
            language: "rust".to_string(),
            block_type: block_type.to_string(),
            content: content.to_string(),
            score: 0.0,
            line_number: line,
            line_end: line + content.lines().count().saturating_sub(1),
            breadcrumb: None,
            complexity: None,
            role: Some(SegmentRole::Definition),
            defined_symbols: None,
            referenced_symbols: None,
            called_symbols: None,
        }
    }

    #[test]
    fn rrf_fusion_produces_ordered_results() {
        let vec_results = vec![
            make_result("a.rs", 1, "function", "fn foo() {\n  let x = 1;\n  let y = 2;\n  let z = 3;\n  let w = 4;\n  let v = 5;\n}"),
            make_result("b.rs", 10, "function", "fn bar() {\n  let a = 1;\n  let b = 2;\n  let c = 3;\n  let d = 4;\n  let e = 5;\n}"),
        ];
        let fts_results = vec![
            make_result("b.rs", 10, "function", "fn bar() {\n  let a = 1;\n  let b = 2;\n  let c = 3;\n  let d = 4;\n  let e = 5;\n}"),
            make_result("a.rs", 1, "function", "fn foo() {\n  let x = 1;\n  let y = 2;\n  let z = 3;\n  let w = 4;\n  let v = 5;\n}"),
        ];

        let fused = fuse_results(
            vec_results,
            fts_results,
            vec![],
            "foo bar",
            QueryIntent::General,
            10,
        );
        assert_eq!(fused.len(), 2);
        assert!(fused[0].score > 0.0);
        assert!(fused[0].score >= fused[1].score);
    }

    #[test]
    fn per_file_cap_applied() {
        let vec_results = vec![
            make_result(
                "a.rs",
                1,
                "function",
                "fn one() {\n  x();\n  y();\n  z();\n  w();\n  v();\n}",
            ),
            make_result(
                "a.rs",
                20,
                "function",
                "fn two() {\n  x();\n  y();\n  z();\n  w();\n  v();\n}",
            ),
            make_result(
                "a.rs",
                40,
                "function",
                "fn three() {\n  x();\n  y();\n  z();\n  w();\n  v();\n}",
            ),
            make_result(
                "a.rs",
                60,
                "function",
                "fn four() {\n  x();\n  y();\n  z();\n  w();\n  v();\n}",
            ),
        ];

        let fused = fuse_results(vec_results, vec![], vec![], "foo", QueryIntent::General, 20);
        assert!(fused.len() <= MAX_RESULTS_PER_FILE);
    }

    #[test]
    fn overlap_deduplication() {
        let vec_results = vec![
            make_result(
                "a.rs",
                1,
                "function",
                "fn foo() {\n  body\n  body\n  body\n  body\n  body\n}",
            ),
            make_result(
                "a.rs",
                3,
                "impl",
                "impl Foo {\n  body\n  body\n  body\n  body\n  body\n}",
            ),
        ];

        let fused = fuse_results(vec_results, vec![], vec![], "foo", QueryIntent::General, 20);
        assert_eq!(fused.len(), 1);
    }

    #[test]
    fn test_penalty_applied() {
        let r1 = make_result("src/main.rs", 1, "function", "fn main() {\n  println!(\"hello\");\n  let x = 1;\n  let y = 2;\n  let z = 3;\n  let w = 4;\n}");
        let r2 = make_result("tests/test_main.rs", 1, "function", "fn test_main() {\n  assert!(true);\n  let x = 1;\n  let y = 2;\n  let z = 3;\n  let w = 4;\n}");

        let boost_normal = file_path_boost(&r1.file_path);
        let boost_test = file_path_boost(&r2.file_path);
        assert!(boost_normal > boost_test);
    }

    #[test]
    fn intent_boosts_definitions() {
        let r = make_result("a.rs", 1, "function", "fn foo() {}");
        let general_boost = intent_boost(&r, "foo", QueryIntent::General);
        let def_boost = intent_boost(&r, "foo", QueryIntent::Definition);
        assert!(def_boost > general_boost);
    }

    #[test]
    fn short_segment_penalized() {
        let short_penalty = short_segment_penalty("x");
        let long_penalty = short_segment_penalty("a\nb\nc\nd\ne\nf\ng");
        assert!(short_penalty < long_penalty);
    }

    #[test]
    fn query_path_overlap_boosts_matching_paths() {
        let path_score = query_path_boost("jira test tickets", "scripts/jira_test_tickets.py");
        let plain_score = query_path_boost("jira test tickets", "src/main.rs");
        assert!(path_score > plain_score);
    }

    #[test]
    fn natural_language_general_queries_prefer_implementation() {
        let mut result = make_result(
            "scripts/jira_test_tickets.py",
            1,
            "function",
            "def main():\n    run()\n    publish()",
        );
        result.role = Some(SegmentRole::Implementation);

        let impl_boost = intent_boost(&result, "jira test tickets", QueryIntent::General);
        let symbolish_boost = intent_boost(&result, "PolicyRuleValidator", QueryIntent::General);
        assert!(impl_boost > symbolish_boost);
    }
}
