use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use crate::search::intent::QueryIntent;
use crate::search::retrieval::CandidateRow;
use crate::shared::constants::{
    MAX_RESULTS_PER_FILE, MAX_SEARCH_RESULTS, RRF_K, SYMBOL_WEIGHT, VECTOR_WEIGHT,
};

#[derive(Debug, Clone, PartialEq)]
pub struct RankedCandidate {
    pub candidate: CandidateRow,
    pub score: f64,
}

struct ScoredCandidate {
    candidate: CandidateRow,
    vector_rank: Option<usize>,
    fts_rank: Option<usize>,
    symbol_rank: Option<usize>,
    fused_score: f64,
}

pub fn rank_candidates(
    vector_results: Vec<CandidateRow>,
    fts_results: Vec<CandidateRow>,
    symbol_results: Vec<CandidateRow>,
    query: &str,
    intent: QueryIntent,
    limit: usize,
) -> Vec<RankedCandidate> {
    let mut candidates: HashMap<String, ScoredCandidate> = HashMap::new();

    for (rank, candidate) in vector_results.into_iter().enumerate() {
        let key = fusion_key(&candidate);
        candidates.insert(
            key,
            ScoredCandidate {
                candidate,
                vector_rank: Some(rank),
                fts_rank: None,
                symbol_rank: None,
                fused_score: 0.0,
            },
        );
    }

    for (rank, candidate) in fts_results.into_iter().enumerate() {
        let key = fusion_key(&candidate);
        match candidates.get_mut(&key) {
            Some(existing) => {
                existing.fts_rank = Some(rank);
            }
            None => {
                candidates.insert(
                    key,
                    ScoredCandidate {
                        candidate,
                        vector_rank: None,
                        fts_rank: Some(rank),
                        symbol_rank: None,
                        fused_score: 0.0,
                    },
                );
            }
        }
    }

    for (rank, candidate) in symbol_results.into_iter().enumerate() {
        let key = fusion_key(&candidate);
        match candidates.get_mut(&key) {
            Some(existing) => {
                existing.symbol_rank = Some(rank);
            }
            None => {
                candidates.insert(
                    key,
                    ScoredCandidate {
                        candidate,
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
    sorted.sort_by(compare_scored_candidates);

    let deduped = deduplicate(sorted);
    let capped = apply_per_file_cap(deduped, MAX_RESULTS_PER_FILE);

    let actual_limit = limit.min(MAX_SEARCH_RESULTS);
    capped
        .into_iter()
        .take(actual_limit)
        .map(|candidate| RankedCandidate {
            candidate: candidate.candidate,
            score: candidate.fused_score,
        })
        .collect()
}

fn fusion_key(candidate: &CandidateRow) -> String {
    candidate.segment_id.clone()
}

fn compare_scored_candidates(a: &ScoredCandidate, b: &ScoredCandidate) -> Ordering {
    b.fused_score
        .partial_cmp(&a.fused_score)
        .unwrap_or(Ordering::Equal)
        .then_with(|| a.candidate.file_path.cmp(&b.candidate.file_path))
        .then_with(|| a.candidate.line_number.cmp(&b.candidate.line_number))
        .then_with(|| a.candidate.segment_id.cmp(&b.candidate.segment_id))
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

    score *= intent_boost(&candidate.candidate, query, intent);
    score *= file_path_boost(&candidate.candidate.file_path);
    score *= query_path_boost(query, &candidate.candidate.file_path);
    score *= query_match_boost(query, &candidate.candidate);
    score *= content_kind_boost(&candidate.candidate, intent);
    score *= short_segment_penalty(candidate.candidate.line_count());

    score
}

fn intent_boost(result: &CandidateRow, query: &str, intent: QueryIntent) -> f64 {
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

fn short_segment_penalty(line_count: usize) -> f64 {
    if line_count <= 2 {
        0.6
    } else if line_count <= 5 {
        0.85
    } else {
        1.0
    }
}

fn query_match_boost(query: &str, result: &CandidateRow) -> f64 {
    let terms = query_terms(query);
    if terms.is_empty() {
        return 1.0;
    }

    let term_set = result_term_set(result);
    let matched_count = terms
        .iter()
        .filter(|term| term_set.contains(term.as_str()))
        .count();

    let mut score = 1.0 + 0.06 * matched_count.min(4) as f64;

    if matched_count == terms.len() {
        score += if terms.len() >= 3 { 0.35 } else { 0.18 };
    } else if terms.len() >= 3 && matched_count + 1 == terms.len() {
        score += 0.12;
    }

    if phrase_match_score(&terms, result) {
        score += 0.22;
    }

    if is_natural_language_query(query) && terms.len() >= 3 {
        match matched_count {
            0 => score *= 0.75,
            1 => score *= 0.82,
            _ => {}
        }
    }

    score
}

fn content_kind_boost(result: &CandidateRow, intent: QueryIntent) -> f64 {
    let lower_path = result.file_path.to_lowercase();
    let is_markdown = result.language.eq_ignore_ascii_case("markdown")
        || lower_path.ends_with(".md")
        || lower_path.ends_with(".markdown");

    if is_markdown {
        match intent {
            QueryIntent::Docs => 1.15,
            _ => 0.72,
        }
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

fn result_term_set(result: &CandidateRow) -> HashSet<String> {
    let mut terms = HashSet::new();

    for value in normalized_haystacks(result) {
        terms.extend(tokenize_text(&value));
    }

    terms
}

fn phrase_match_score(query_terms: &[String], result: &CandidateRow) -> bool {
    if query_terms.len() < 2 {
        return false;
    }

    let phrase = query_terms.join(" ");
    normalized_haystacks(result)
        .into_iter()
        .any(|value| normalize_text(&value).contains(&phrase))
}

fn normalized_haystacks(result: &CandidateRow) -> Vec<String> {
    let mut haystacks = vec![result.file_path.clone(), result.block_type.clone()];

    if let Some(breadcrumb) = &result.breadcrumb {
        haystacks.push(breadcrumb.clone());
    }
    if let Some(symbols) = &result.defined_symbols {
        haystacks.push(symbols.join(" "));
    }
    if let Some(symbols) = &result.referenced_symbols {
        haystacks.push(symbols.join(" "));
    }
    if let Some(symbols) = &result.called_symbols {
        haystacks.push(symbols.join(" "));
    }

    haystacks
}

fn normalize_text(value: &str) -> String {
    tokenize_text(value).join(" ")
}

fn tokenize_text(value: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut prev: Option<char> = None;

    for ch in value.chars() {
        if ch.is_alphanumeric() {
            let is_camel_boundary =
                prev.is_some_and(|p| (p.is_lowercase() || p.is_ascii_digit()) && ch.is_uppercase());

            if is_camel_boundary && !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }

            current.extend(ch.to_lowercase());
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }

        prev = Some(ch);
    }

    if !current.is_empty() {
        tokens.push(current);
    }

    tokens
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

    for candidate in candidates {
        let file = &candidate.candidate.file_path;
        let start = candidate.candidate.line_number;
        let end = candidate.candidate.line_end.saturating_add(1);

        let overlaps = seen.iter().any(|(seen_file, seen_start, seen_end)| {
            seen_file == file && ranges_overlap(start, end, *seen_start, *seen_end)
        });

        if !overlaps {
            seen.push((file.clone(), start, end));
            result.push(candidate);
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

    for candidate in candidates {
        let count = file_counts
            .entry(candidate.candidate.file_path.clone())
            .or_insert(0);
        if *count < cap {
            *count += 1;
            result.push(candidate);
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::types::SegmentRole;

    fn make_candidate(
        file: &str,
        line: usize,
        block_type: &str,
        line_count: usize,
    ) -> CandidateRow {
        CandidateRow {
            segment_id: format!("{file}:{line}:{block_type}"),
            file_path: file.to_string(),
            language: if file.ends_with(".md") {
                "markdown".to_string()
            } else {
                "rust".to_string()
            },
            block_type: block_type.to_string(),
            line_number: line,
            line_end: line + line_count.saturating_sub(1),
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
            make_candidate("a.rs", 1, "function", 7),
            make_candidate("b.rs", 10, "function", 7),
        ];
        let fts_results = vec![
            make_candidate("b.rs", 10, "function", 7),
            make_candidate("a.rs", 1, "function", 7),
        ];

        let ranked = rank_candidates(
            vec_results,
            fts_results,
            vec![],
            "foo bar",
            QueryIntent::General,
            10,
        );
        assert_eq!(ranked.len(), 2);
        assert!(ranked[0].score > 0.0);
        assert!(ranked[0].score >= ranked[1].score);
    }

    #[test]
    fn per_file_cap_applied() {
        let vec_results = vec![
            make_candidate("a.rs", 1, "function", 6),
            make_candidate("a.rs", 20, "function", 6),
            make_candidate("a.rs", 40, "function", 6),
            make_candidate("a.rs", 60, "function", 6),
        ];

        let ranked = rank_candidates(vec_results, vec![], vec![], "foo", QueryIntent::General, 20);
        assert!(ranked.len() <= MAX_RESULTS_PER_FILE);
    }

    #[test]
    fn overlap_deduplication() {
        let vec_results = vec![
            make_candidate("a.rs", 1, "function", 6),
            make_candidate("a.rs", 3, "impl", 6),
        ];

        let ranked = rank_candidates(vec_results, vec![], vec![], "foo", QueryIntent::General, 20);
        assert_eq!(ranked.len(), 1);
    }

    #[test]
    fn test_penalty_applied() {
        let normal = make_candidate("src/main.rs", 1, "function", 7);
        let test = make_candidate("tests/test_main.rs", 1, "function", 7);

        let boost_normal = file_path_boost(&normal.file_path);
        let boost_test = file_path_boost(&test.file_path);
        assert!(boost_normal > boost_test);
    }

    #[test]
    fn intent_boosts_definitions() {
        let result = make_candidate("a.rs", 1, "function", 3);
        let general_boost = intent_boost(&result, "foo", QueryIntent::General);
        let def_boost = intent_boost(&result, "foo", QueryIntent::Definition);
        assert!(def_boost > general_boost);
    }

    #[test]
    fn short_segment_penalized() {
        let short_penalty = short_segment_penalty(1);
        let long_penalty = short_segment_penalty(7);
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
        let mut result = make_candidate("scripts/jira_test_tickets.py", 1, "function", 3);
        result.role = Some(SegmentRole::Implementation);

        let impl_boost = intent_boost(&result, "jira test tickets", QueryIntent::General);
        let symbolish_boost = intent_boost(&result, "PolicyRuleValidator", QueryIntent::General);
        assert!(impl_boost > symbolish_boost);
    }

    #[test]
    fn term_coverage_beats_generic_partial_match() {
        let mut generic =
            make_candidate("service/src/main/kotlin/TaskMappers.kt", 10, "function", 4);
        generic.defined_symbols = Some(vec!["toTaskEdge".to_string()]);
        generic.referenced_symbols = Some(vec!["sourceKey".to_string(), "targetKey".to_string()]);

        let mut specific = make_candidate("protos/policy_rules.proto", 20, "chunk", 1);
        specific.breadcrumb = Some("idempotency key preview".to_string());
        specific.defined_symbols = Some(vec!["IdempotencyKeyPreview".to_string()]);

        let generic_boost = query_match_boost("idempotency key preview", &generic);
        let specific_boost = query_match_boost("idempotency key preview", &specific);
        assert!(specific_boost > generic_boost);
    }

    #[test]
    fn markdown_docs_penalized_for_non_docs_query() {
        let mut markdown = make_candidate("routing_guide.md", 1, "chunk", 1);
        markdown.role = Some(SegmentRole::Docs);
        markdown.breadcrumb = Some("request signing secret configuration guide".to_string());

        let mut config =
            make_candidate("service/src/main/resources/app-common.yaml", 1, "chunk", 1);
        config.breadcrumb = Some("request signing secret".to_string());
        config.defined_symbols = Some(vec!["request_signing_secret".to_string()]);

        let docs_penalty = content_kind_boost(&markdown, QueryIntent::General);
        let config_penalty = content_kind_boost(&config, QueryIntent::General);
        let docs_boost = query_match_boost("request signing secret", &markdown);
        let config_boost = query_match_boost("request signing secret", &config);

        assert!(docs_penalty < config_penalty);
        assert!(docs_boost <= config_boost);
    }
}
