#![allow(dead_code)]

use std::collections::{BTreeSet, HashMap, HashSet};

use libsql::Connection;
use serde::Serialize;

use crate::search::retrieval::CandidateRow;
use crate::search::symbol::SymbolSearchEngine;
use crate::shared::constants::{MAX_RESULTS_PER_FILE, MAX_SEARCH_RESULTS};
use crate::shared::errors::{OneupError, SearchError};
use crate::shared::symbols::normalize_symbolish;
use crate::shared::types::SegmentRole;
use crate::storage::relations::{get_inbound_relations, get_outbound_relations, RelationKind};
use crate::storage::segments::{
    get_segment_by_id, get_segments_by_file, get_test_file_paths, StoredSegment,
};

const DEFAULT_IMPACT_DEPTH: usize = 2;
const MAX_IMPACT_DEPTH: usize = 2;
const MAX_FILE_SEEDS: usize = 5;
const MAX_SYMBOL_SEEDS: usize = 3;
const MAX_SYMBOL_FILES: usize = 3;
const MAX_SYMBOL_TOP_LEVEL_DIRS: usize = 2;
const MAX_RELATIONS_PER_HOP: usize = 8;
const MAX_DEFINITION_TARGETS_PER_SYMBOL: usize = 3;
const MAX_TEST_FILE_BUDGET: usize = 12;
const TEST_FILE_QUERY_FACTOR: usize = 2;

const CALL_WEIGHT: f64 = 1.0;
const SAME_FILE_WEIGHT: f64 = 0.70;
const REFERENCE_WEIGHT: f64 = 0.65;
const TEST_WEIGHT: f64 = 0.55;
const HOP_DECAY: f64 = 0.70;
const SCOPE_BOOST: f64 = 1.10;
const ROLE_BOOST: f64 = 1.05;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImpactAnchor {
    File { path: String, line: Option<usize> },
    Symbol { name: String },
    Segment { id: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImpactRequest {
    pub anchor: ImpactAnchor,
    pub scope: Option<String>,
    pub depth: usize,
    pub limit: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ImpactStatus {
    Expanded,
    ExpandedScoped,
    Empty,
    EmptyScoped,
    Refused,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedImpactAnchor {
    pub kind: String,
    pub value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub seed_segment_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub matched_files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImpactReason {
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_segment_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImpactHint {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_segment_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImpactRefusal {
    pub reason: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImpactCandidate {
    pub segment_id: String,
    pub file_path: String,
    pub language: String,
    pub block_type: String,
    pub line_start: usize,
    pub line_end: usize,
    pub score: f64,
    pub hop: usize,
    pub reasons: Vec<ImpactReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub breadcrumb: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub complexity: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<SegmentRole>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub defined_symbols: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImpactResultEnvelope {
    pub status: ImpactStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_anchor: Option<ResolvedImpactAnchor>,
    pub results: Vec<ImpactCandidate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contextual_results: Option<Vec<ImpactCandidate>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<ImpactHint>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refusal: Option<ImpactRefusal>,
}

#[allow(dead_code)]
pub struct ImpactHorizonEngine<'a> {
    conn: &'a Connection,
}

struct AnchorResolution {
    resolved_anchor: ResolvedImpactAnchor,
    seeds: Vec<CandidateRow>,
    effective_scope: Option<String>,
}

enum ResolveOutcome {
    Resolved(AnchorResolution),
    Refused(ImpactResultEnvelope),
}

struct CandidateAggregate {
    candidate: CandidateRow,
    primary_score: f64,
    primary_hop: Option<usize>,
    primary_reasons: HashMap<String, ReasonContribution>,
    contextual_score: f64,
    contextual_hop: Option<usize>,
    contextual_reasons: HashMap<String, ReasonContribution>,
}

struct ReasonContribution {
    reason: ImpactReason,
    score: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ObservationBucket {
    Primary,
    Contextual,
}

struct CandidateObservation {
    candidate: CandidateRow,
    score: f64,
    hop: usize,
    reason: ImpactReason,
    bucket: ObservationBucket,
}

struct FinalizedImpactResults {
    primary: Vec<ImpactCandidate>,
    contextual: Vec<ImpactCandidate>,
}

impl<'a> ImpactHorizonEngine<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    pub async fn explore(
        &self,
        request: ImpactRequest,
    ) -> Result<ImpactResultEnvelope, OneupError> {
        let depth = clamp_depth(request.depth);
        let limit = clamp_limit(request.limit);
        let explicit_scope = normalize_scope(request.scope.as_deref());
        let test_file_budget = clamp_test_file_budget(limit, depth);
        let test_observation_budget = clamp_test_observation_budget(limit);

        let resolution = match &request.anchor {
            ImpactAnchor::File { path, line } => {
                self.resolve_file_anchor(path, *line, explicit_scope.clone())
                    .await?
            }
            ImpactAnchor::Symbol { name } => {
                self.resolve_symbol_anchor(name, explicit_scope.clone())
                    .await?
            }
            ImpactAnchor::Segment { id } => {
                self.resolve_segment_anchor(id, explicit_scope.clone())
                    .await?
            }
        };

        let resolution = match resolution {
            ResolveOutcome::Resolved(resolution) => resolution,
            ResolveOutcome::Refused(result) => return Ok(result),
        };

        let seed_ids: HashSet<String> = resolution
            .seeds
            .iter()
            .map(|seed| seed.segment_id.clone())
            .collect();
        let mut aggregates = HashMap::new();
        let mut frontier = resolution.seeds.clone();
        let mut expanded = seed_ids.clone();

        for hop in 1..=depth {
            let mut next_frontier = Vec::new();
            let mut queued = HashSet::new();

            for source in &frontier {
                let observations = self
                    .collect_relation_observations(
                        source,
                        resolution.effective_scope.as_deref(),
                        hop,
                    )
                    .await?;

                for observation in observations {
                    if seed_ids.contains(&observation.candidate.segment_id) {
                        continue;
                    }

                    let candidate_for_frontier = observation.candidate.clone();
                    let candidate_id = candidate_for_frontier.segment_id.clone();
                    observe_candidate(&mut aggregates, observation);

                    if hop < depth
                        && expanded.insert(candidate_id.clone())
                        && queued.insert(candidate_id)
                    {
                        next_frontier.push(candidate_for_frontier);
                    }
                }
            }

            frontier = next_frontier;
        }

        for observation in self
            .collect_same_file_observations(
                &resolution.seeds,
                &seed_ids,
                resolution.effective_scope.as_deref(),
            )
            .await?
        {
            observe_candidate(&mut aggregates, observation);
        }

        for observation in self
            .collect_test_observations(
                &resolution.seeds,
                &seed_ids,
                resolution.effective_scope.as_deref(),
                test_file_budget,
                test_observation_budget,
            )
            .await?
        {
            observe_candidate(&mut aggregates, observation);
        }

        let finalized = finalize_impact_results(aggregates, limit);
        let status = match (
            !finalized.primary.is_empty(),
            resolution.resolved_anchor.scope.is_some(),
        ) {
            (true, true) => ImpactStatus::ExpandedScoped,
            (true, false) => ImpactStatus::Expanded,
            (false, true) => ImpactStatus::EmptyScoped,
            (false, false) => ImpactStatus::Empty,
        };
        let contextual_results = (!finalized.contextual.is_empty()).then_some(finalized.contextual);
        let hint = outcome_hint(
            &status,
            &finalized.primary,
            contextual_results.as_deref(),
            resolution.effective_scope.clone(),
        );

        Ok(ImpactResultEnvelope {
            status,
            resolved_anchor: Some(resolution.resolved_anchor),
            results: finalized.primary,
            contextual_results,
            hint,
            refusal: None,
        })
    }

    async fn resolve_file_anchor(
        &self,
        path: &str,
        line: Option<usize>,
        explicit_scope: Option<String>,
    ) -> Result<ResolveOutcome, OneupError> {
        let normalized_path = normalize_path(path);
        let stored = get_segments_by_file(self.conn, &normalized_path).await?;
        if stored.is_empty() {
            return Ok(ResolveOutcome::Refused(refused_result(
                "anchor_not_indexed",
                format!("No indexed segments were found for `{normalized_path}`."),
                impact_hint(
                    "reindex_or_search",
                    "Reindex the project or choose an exact segment anchor from search results.",
                    explicit_scope,
                    None,
                ),
            )));
        }

        let mut candidates: Vec<CandidateRow> = stored
            .into_iter()
            .map(candidate_from_stored_segment)
            .collect();

        let seeds: Vec<CandidateRow> = if let Some(line) = line {
            if line == 0 {
                return Err(SearchError::InvalidQuery(
                    "impact file anchors require line numbers >= 1".to_string(),
                )
                .into());
            }

            candidates
                .into_iter()
                .min_by(|left, right| compare_line_distance(left, right, line))
                .into_iter()
                .collect::<Vec<_>>()
        } else {
            candidates.sort_by(compare_file_seed_priority);
            candidates
                .into_iter()
                .take(MAX_FILE_SEEDS)
                .collect::<Vec<_>>()
        };

        if let Some(scope) = explicit_scope.as_deref() {
            if let Some(refusal) =
                out_of_scope_anchor_refusal("file", &normalized_path, scope, &seeds)
            {
                return Ok(ResolveOutcome::Refused(refusal));
            }
        }

        Ok(ResolveOutcome::Resolved(AnchorResolution {
            resolved_anchor: resolved_anchor(
                if line.is_some() { "file_line" } else { "file" },
                normalized_path,
                line,
                explicit_scope.clone(),
                &seeds,
            ),
            seeds,
            effective_scope: explicit_scope,
        }))
    }

    async fn resolve_segment_anchor(
        &self,
        id: &str,
        explicit_scope: Option<String>,
    ) -> Result<ResolveOutcome, OneupError> {
        let Some(segment) = get_segment_by_id(self.conn, id).await? else {
            return Ok(ResolveOutcome::Refused(refused_result(
                "anchor_not_found",
                format!("Segment `{id}` is not present in the current index."),
                impact_hint(
                    "refresh_anchor",
                    "Choose a segment id from the current index and retry.",
                    explicit_scope,
                    None,
                ),
            )));
        };

        let seed = candidate_from_stored_segment(segment);
        if let Some(scope) = explicit_scope.as_deref() {
            if let Some(refusal) =
                out_of_scope_anchor_refusal("segment", id, scope, std::slice::from_ref(&seed))
            {
                return Ok(ResolveOutcome::Refused(refusal));
            }
        }
        Ok(ResolveOutcome::Resolved(AnchorResolution {
            resolved_anchor: resolved_anchor(
                "segment",
                id.to_string(),
                None,
                explicit_scope.clone(),
                std::slice::from_ref(&seed),
            ),
            seeds: vec![seed],
            effective_scope: explicit_scope,
        }))
    }

    async fn resolve_symbol_anchor(
        &self,
        name: &str,
        explicit_scope: Option<String>,
    ) -> Result<ResolveOutcome, OneupError> {
        let engine = SymbolSearchEngine::new(self.conn);
        let mut seeds = engine.find_definition_candidates(name).await?;
        if let Some(scope) = explicit_scope.as_deref() {
            seeds.retain(|candidate| scope_matches(&candidate.file_path, Some(scope)));
        }

        if seeds.is_empty() {
            return Ok(ResolveOutcome::Refused(refused_result(
                "anchor_not_found",
                format!("No indexed definitions matched symbol `{name}`."),
                impact_hint(
                    "narrow_with_scope",
                    "Retry with `--scope`, `--from-file`, or an exact `--from-segment` anchor.",
                    explicit_scope,
                    None,
                ),
            )));
        }

        let distinct_files: BTreeSet<String> =
            seeds.iter().map(|seed| seed.file_path.clone()).collect();
        let top_level_dirs: BTreeSet<String> = distinct_files
            .iter()
            .filter_map(|file| top_level_dir(file))
            .collect();

        if seeds.len() > MAX_SYMBOL_SEEDS
            || distinct_files.len() > MAX_SYMBOL_FILES
            || (explicit_scope.is_none() && top_level_dirs.len() > MAX_SYMBOL_TOP_LEVEL_DIRS)
        {
            return Ok(ResolveOutcome::Refused(refused_result(
                "symbol_too_broad",
                format!(
                    "Symbol `{name}` matched {} definitions across {} files.",
                    seeds.len(),
                    distinct_files.len()
                ),
                impact_hint(
                    "narrow_with_scope",
                    "Retry with `--scope`, `--from-file path[:line]`, or an exact `--from-segment` anchor.",
                    suggested_scope_from_candidates(&seeds).or(explicit_scope),
                    None,
                ),
            )));
        }

        let implicit_scope = if explicit_scope.is_none() {
            common_parent_scope(
                &distinct_files
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
            )
        } else {
            None
        };
        let effective_scope = explicit_scope.clone().or(implicit_scope);

        Ok(ResolveOutcome::Resolved(AnchorResolution {
            resolved_anchor: resolved_anchor(
                "symbol",
                name.to_string(),
                None,
                effective_scope.clone(),
                &seeds,
            ),
            seeds,
            effective_scope,
        }))
    }

    async fn collect_relation_observations(
        &self,
        source: &CandidateRow,
        scope: Option<&str>,
        hop: usize,
    ) -> Result<Vec<CandidateObservation>, OneupError> {
        let mut observations = Vec::new();
        let outbound =
            get_outbound_relations(self.conn, &source.segment_id, None, MAX_RELATIONS_PER_HOP)
                .await?;

        for relation in outbound {
            let reason_kind = match relation.relation_kind {
                RelationKind::Call => "calls",
                RelationKind::Reference => "references_symbol",
            };
            let weight = base_weight(relation.relation_kind);
            let targets = self
                .resolve_relation_targets(&relation.canonical_target_symbol, scope)
                .await?;

            for target in targets {
                if is_test_path(&target.file_path) {
                    continue;
                }
                observations.push(CandidateObservation {
                    score: weighted_score(weight, hop, &target, scope),
                    hop,
                    reason: ImpactReason {
                        kind: reason_kind.to_string(),
                        symbol: Some(relation.raw_target_symbol.clone()),
                        from_segment_id: Some(source.segment_id.clone()),
                    },
                    candidate: target,
                    bucket: ObservationBucket::Primary,
                });
            }
        }

        let defined_symbols = source.defined_symbols.clone().unwrap_or_default();
        let mut remaining = MAX_RELATIONS_PER_HOP;
        for symbol in defined_symbols {
            if remaining == 0 {
                break;
            }

            let canonical = normalize_symbolish(&symbol);
            if canonical.is_empty() {
                continue;
            }

            let inbound = get_inbound_relations(self.conn, &canonical, None, remaining).await?;
            remaining = remaining.saturating_sub(inbound.len());

            for relation in inbound {
                let Some(candidate) = self.load_candidate(&relation.source_segment_id).await?
                else {
                    continue;
                };
                if !scope_matches(&candidate.file_path, scope) || is_test_path(&candidate.file_path)
                {
                    continue;
                }

                let reason_kind = match relation.relation_kind {
                    RelationKind::Call => "called_by",
                    RelationKind::Reference => "references_symbol",
                };

                observations.push(CandidateObservation {
                    score: weighted_score(
                        base_weight(relation.relation_kind),
                        hop,
                        &candidate,
                        scope,
                    ),
                    hop,
                    reason: ImpactReason {
                        kind: reason_kind.to_string(),
                        symbol: Some(symbol.clone()),
                        from_segment_id: Some(source.segment_id.clone()),
                    },
                    candidate,
                    bucket: ObservationBucket::Primary,
                });
            }
        }

        Ok(observations)
    }

    async fn collect_same_file_observations(
        &self,
        seeds: &[CandidateRow],
        seed_ids: &HashSet<String>,
        scope: Option<&str>,
    ) -> Result<Vec<CandidateObservation>, OneupError> {
        let mut observations = Vec::new();
        let mut seen_files = HashSet::new();

        for seed in seeds {
            if !seen_files.insert(seed.file_path.clone()) {
                continue;
            }

            for sibling in get_segments_by_file(self.conn, &seed.file_path).await? {
                let candidate = candidate_from_stored_segment(sibling);
                if seed_ids.contains(&candidate.segment_id)
                    || !scope_matches(&candidate.file_path, scope)
                {
                    continue;
                }

                observations.push(CandidateObservation {
                    score: weighted_score(SAME_FILE_WEIGHT, 1, &candidate, scope),
                    hop: 1,
                    reason: ImpactReason {
                        kind: "same_file".to_string(),
                        symbol: None,
                        from_segment_id: Some(seed.segment_id.clone()),
                    },
                    candidate,
                    bucket: ObservationBucket::Contextual,
                });
            }
        }

        Ok(observations)
    }

    async fn collect_test_observations(
        &self,
        seeds: &[CandidateRow],
        seed_ids: &HashSet<String>,
        scope: Option<&str>,
        max_files: usize,
        max_observations: usize,
    ) -> Result<Vec<CandidateObservation>, OneupError> {
        if max_files == 0 || max_observations == 0 {
            return Ok(Vec::new());
        }

        let anchor_files: Vec<String> = seeds.iter().map(|seed| seed.file_path.clone()).collect();
        let anchor_symbols: HashSet<String> = seeds
            .iter()
            .flat_map(|seed| seed.defined_symbols.clone().unwrap_or_default())
            .map(|symbol| normalize_symbolish(&symbol))
            .filter(|symbol| !symbol.is_empty())
            .collect();

        let mut candidate_files = get_test_file_paths(
            self.conn,
            scope,
            max_files.saturating_mul(TEST_FILE_QUERY_FACTOR),
        )
        .await?;
        candidate_files.sort_by(|left, right| {
            test_file_priority(left, &anchor_files)
                .cmp(&test_file_priority(right, &anchor_files))
                .then_with(|| left.cmp(right))
        });
        candidate_files.truncate(max_files);

        let mut observations = Vec::new();
        for file_path in candidate_files {
            if observations.len() >= max_observations {
                break;
            }
            let matches_file = anchor_files
                .iter()
                .any(|anchor_file| shares_test_stem(anchor_file, &file_path));
            if !matches_file && anchor_symbols.is_empty() {
                continue;
            }

            for segment in get_segments_by_file(self.conn, &file_path).await? {
                if observations.len() >= max_observations {
                    break;
                }
                let content = segment.content.to_ascii_lowercase();
                let symbol_match = segment
                    .parsed_defined_symbols()
                    .into_iter()
                    .chain(segment.parsed_referenced_symbols())
                    .chain(segment.parsed_called_symbols())
                    .find(|symbol| anchor_symbols.contains(&normalize_symbolish(symbol)))
                    .or_else(|| {
                        anchor_symbols
                            .iter()
                            .find_map(|symbol| content.contains(symbol).then_some(symbol.clone()))
                    });
                let candidate = candidate_from_stored_segment(segment);
                if seed_ids.contains(&candidate.segment_id) {
                    continue;
                }

                if !matches_file && symbol_match.is_none() {
                    continue;
                }

                observations.push(CandidateObservation {
                    score: weighted_score(TEST_WEIGHT, 1, &candidate, scope),
                    hop: 1,
                    reason: ImpactReason {
                        kind: if matches_file {
                            "test_for_file".to_string()
                        } else {
                            "test_for_symbol".to_string()
                        },
                        symbol: symbol_match,
                        from_segment_id: None,
                    },
                    candidate,
                    bucket: ObservationBucket::Contextual,
                });
            }
        }

        Ok(observations)
    }

    async fn resolve_relation_targets(
        &self,
        canonical_symbol: &str,
        scope: Option<&str>,
    ) -> Result<Vec<CandidateRow>, OneupError> {
        let engine = SymbolSearchEngine::new(self.conn);
        let mut candidates = engine.find_definition_candidates(canonical_symbol).await?;
        candidates.retain(|candidate| {
            scope_matches(&candidate.file_path, scope)
                && candidate
                    .defined_symbols
                    .as_ref()
                    .map(|symbols| {
                        symbols
                            .iter()
                            .any(|symbol| normalize_symbolish(symbol) == canonical_symbol)
                    })
                    .unwrap_or(false)
        });
        candidates.truncate(MAX_DEFINITION_TARGETS_PER_SYMBOL);
        Ok(candidates)
    }

    async fn load_candidate(&self, segment_id: &str) -> Result<Option<CandidateRow>, OneupError> {
        Ok(get_segment_by_id(self.conn, segment_id)
            .await?
            .map(candidate_from_stored_segment))
    }
}

fn observe_candidate(
    aggregates: &mut HashMap<String, CandidateAggregate>,
    observation: CandidateObservation,
) {
    let entry = aggregates
        .entry(observation.candidate.segment_id.clone())
        .or_insert_with(|| CandidateAggregate {
            candidate: observation.candidate.clone(),
            primary_score: 0.0,
            primary_hop: None,
            primary_reasons: HashMap::new(),
            contextual_score: 0.0,
            contextual_hop: None,
            contextual_reasons: HashMap::new(),
        });

    let (score, hop, reasons) = match observation.bucket {
        ObservationBucket::Primary => (
            &mut entry.primary_score,
            &mut entry.primary_hop,
            &mut entry.primary_reasons,
        ),
        ObservationBucket::Contextual => (
            &mut entry.contextual_score,
            &mut entry.contextual_hop,
            &mut entry.contextual_reasons,
        ),
    };

    *score += observation.score;
    *hop = Some(hop.map_or(observation.hop, |existing| existing.min(observation.hop)));

    let key = reason_key(&observation.reason);
    match reasons.get_mut(&key) {
        Some(existing) => {
            if observation.score > existing.score {
                existing.score = observation.score;
                existing.reason = observation.reason;
            }
        }
        None => {
            reasons.insert(
                key,
                ReasonContribution {
                    reason: observation.reason,
                    score: observation.score,
                },
            );
        }
    }
}

fn finalize_impact_results(
    aggregates: HashMap<String, CandidateAggregate>,
    limit: usize,
) -> FinalizedImpactResults {
    let mut primary = Vec::new();
    let mut contextual = Vec::new();

    for aggregate in aggregates.into_values() {
        let CandidateAggregate {
            candidate,
            primary_score,
            primary_hop,
            primary_reasons,
            contextual_score,
            contextual_hop,
            contextual_reasons,
        } = aggregate;

        let mut primary_reasons: Vec<ReasonContribution> = primary_reasons.into_values().collect();
        primary_reasons.sort_by(|left, right| right.score.total_cmp(&left.score));

        let mut contextual_reasons: Vec<ReasonContribution> =
            contextual_reasons.into_values().collect();
        contextual_reasons.sort_by(|left, right| right.score.total_cmp(&left.score));

        if primary_score > 0.0 && !primary_reasons.is_empty() {
            let reasons = primary_reasons
                .into_iter()
                .map(|reason| reason.reason)
                .chain(contextual_reasons.into_iter().map(|reason| reason.reason))
                .take(3)
                .collect();
            primary.push(build_impact_candidate(
                candidate,
                primary_score,
                primary_hop.unwrap_or_default(),
                reasons,
            ));
        } else if contextual_score > 0.0 && !contextual_reasons.is_empty() {
            let reasons = contextual_reasons
                .into_iter()
                .map(|reason| reason.reason)
                .take(3)
                .collect();
            contextual.push(build_impact_candidate(
                candidate,
                contextual_score,
                contextual_hop.unwrap_or_default(),
                reasons,
            ));
        }
    }

    FinalizedImpactResults {
        primary: rank_impact_candidates(primary, limit),
        contextual: rank_impact_candidates(contextual, limit),
    }
}

fn build_impact_candidate(
    candidate: CandidateRow,
    score: f64,
    hop: usize,
    reasons: Vec<ImpactReason>,
) -> ImpactCandidate {
    ImpactCandidate {
        segment_id: candidate.segment_id,
        file_path: candidate.file_path,
        language: candidate.language,
        block_type: candidate.block_type,
        line_start: candidate.line_number,
        line_end: candidate.line_end,
        score,
        hop,
        reasons,
        breadcrumb: candidate.breadcrumb,
        complexity: candidate.complexity,
        role: candidate.role,
        defined_symbols: candidate.defined_symbols,
    }
}

fn rank_impact_candidates(mut ranked: Vec<ImpactCandidate>, limit: usize) -> Vec<ImpactCandidate> {
    ranked.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.hop.cmp(&right.hop))
            .then_with(|| left.file_path.cmp(&right.file_path))
            .then_with(|| left.line_start.cmp(&right.line_start))
            .then_with(|| left.segment_id.cmp(&right.segment_id))
    });

    let mut per_file = HashMap::new();
    let mut results = Vec::new();
    for candidate in ranked {
        let file_count = per_file
            .entry(candidate.file_path.clone())
            .or_insert(0usize);
        if *file_count >= MAX_RESULTS_PER_FILE {
            continue;
        }
        *file_count += 1;
        results.push(candidate);
        if results.len() >= limit {
            break;
        }
    }

    results
}

fn outcome_hint(
    status: &ImpactStatus,
    results: &[ImpactCandidate],
    contextual_results: Option<&[ImpactCandidate]>,
    scope: Option<String>,
) -> Option<ImpactHint> {
    match status {
        ImpactStatus::Expanded | ImpactStatus::ExpandedScoped => {
            let first = results.first()?;
            Some(impact_hint(
                "inspect_candidate",
                &format!(
                    "Inspect `{}` next or reuse segment `{}` for a narrower follow-up.",
                    first.file_path, first.segment_id
                ),
                scope,
                Some(first.segment_id.clone()),
            ))
        }
        ImpactStatus::Empty | ImpactStatus::EmptyScoped => {
            if let Some(first) = contextual_results.and_then(|results| results.first()) {
                Some(impact_hint(
                    "context_only",
                    &format!(
                        "No likely-impact candidates were found. Review the contextual guidance or reuse segment `{}` for a narrower follow-up.",
                        first.segment_id
                    ),
                    scope,
                    Some(first.segment_id.clone()),
                ))
            } else {
                Some(impact_hint(
                    "no_likely_impact",
                    "No likely-impact candidates were found for the resolved anchor.",
                    scope,
                    None,
                ))
            }
        }
        ImpactStatus::Refused => None,
    }
}

fn resolved_anchor(
    kind: &str,
    value: String,
    line: Option<usize>,
    scope: Option<String>,
    seeds: &[CandidateRow],
) -> ResolvedImpactAnchor {
    let matched_files: Vec<String> = seeds
        .iter()
        .map(|seed| seed.file_path.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    ResolvedImpactAnchor {
        kind: kind.to_string(),
        value,
        line,
        scope,
        seed_segment_ids: seeds.iter().map(|seed| seed.segment_id.clone()).collect(),
        matched_files,
    }
}

fn refused_result(reason: &str, message: String, hint: ImpactHint) -> ImpactResultEnvelope {
    ImpactResultEnvelope {
        status: ImpactStatus::Refused,
        resolved_anchor: None,
        results: Vec::new(),
        contextual_results: None,
        hint: Some(hint),
        refusal: Some(ImpactRefusal {
            reason: reason.to_string(),
            message,
        }),
    }
}

fn impact_hint(
    code: &str,
    message: &str,
    suggested_scope: Option<String>,
    suggested_segment_id: Option<String>,
) -> ImpactHint {
    ImpactHint {
        code: code.to_string(),
        message: message.to_string(),
        suggested_scope,
        suggested_segment_id,
    }
}

fn candidate_from_stored_segment(segment: StoredSegment) -> CandidateRow {
    let role = Some(segment.parsed_role());
    let defined_symbols = some_if_not_empty(segment.parsed_defined_symbols());
    let referenced_symbols = some_if_not_empty(segment.parsed_referenced_symbols());
    let called_symbols = some_if_not_empty(segment.parsed_called_symbols());

    CandidateRow {
        segment_id: segment.id,
        file_path: segment.file_path,
        language: segment.language,
        block_type: segment.block_type,
        line_number: segment.line_start as usize,
        line_end: segment.line_end as usize,
        breadcrumb: segment.breadcrumb,
        complexity: Some(segment.complexity as u32),
        role,
        defined_symbols,
        referenced_symbols,
        called_symbols,
    }
}

fn compare_file_seed_priority(left: &CandidateRow, right: &CandidateRow) -> std::cmp::Ordering {
    file_seed_priority(left)
        .cmp(&file_seed_priority(right))
        .then_with(|| left.line_number.cmp(&right.line_number))
        .then_with(|| left.file_path.cmp(&right.file_path))
        .then_with(|| left.segment_id.cmp(&right.segment_id))
}

fn file_seed_priority(candidate: &CandidateRow) -> usize {
    if candidate.is_definition_like() {
        0
    } else if matches!(
        candidate.role,
        Some(SegmentRole::Orchestration | SegmentRole::Implementation)
    ) {
        1
    } else {
        2
    }
}

fn compare_line_distance(
    left: &CandidateRow,
    right: &CandidateRow,
    target_line: usize,
) -> std::cmp::Ordering {
    line_distance(left, target_line)
        .cmp(&line_distance(right, target_line))
        .then_with(|| left.line_number.cmp(&right.line_number))
        .then_with(|| left.segment_id.cmp(&right.segment_id))
}

fn line_distance(candidate: &CandidateRow, target_line: usize) -> usize {
    if target_line < candidate.line_number {
        candidate.line_number - target_line
    } else {
        target_line.saturating_sub(candidate.line_end)
    }
}

fn weighted_score(base: f64, hop: usize, candidate: &CandidateRow, scope: Option<&str>) -> f64 {
    let mut score = base * HOP_DECAY.powi(hop as i32);
    if scope.is_some() && scope_matches(&candidate.file_path, scope) {
        score *= SCOPE_BOOST;
    }
    if matches!(
        candidate.role,
        Some(SegmentRole::Implementation | SegmentRole::Orchestration)
    ) {
        score *= ROLE_BOOST;
    }
    score
}

fn base_weight(relation_kind: RelationKind) -> f64 {
    match relation_kind {
        RelationKind::Call => CALL_WEIGHT,
        RelationKind::Reference => REFERENCE_WEIGHT,
    }
}

fn reason_key(reason: &ImpactReason) -> String {
    format!(
        "{}|{}|{}",
        reason.kind,
        reason.symbol.clone().unwrap_or_default(),
        reason.from_segment_id.clone().unwrap_or_default()
    )
}

fn clamp_depth(depth: usize) -> usize {
    if depth == 0 {
        DEFAULT_IMPACT_DEPTH
    } else {
        depth.clamp(1, MAX_IMPACT_DEPTH)
    }
}

fn clamp_limit(limit: usize) -> usize {
    if limit == 0 {
        MAX_SEARCH_RESULTS
    } else {
        limit.min(MAX_SEARCH_RESULTS)
    }
}

fn clamp_test_file_budget(limit: usize, depth: usize) -> usize {
    limit
        .max(1)
        .saturating_mul(depth.max(1))
        .clamp(2, MAX_TEST_FILE_BUDGET)
}

fn clamp_test_observation_budget(limit: usize) -> usize {
    limit.max(1).saturating_mul(MAX_RESULTS_PER_FILE)
}

fn some_if_not_empty(values: Vec<String>) -> Option<Vec<String>> {
    if values.is_empty() {
        None
    } else {
        Some(values)
    }
}

fn normalize_scope(scope: Option<&str>) -> Option<String> {
    scope.and_then(|value| {
        let normalized = normalize_path(value);
        (!normalized.is_empty() && normalized != ".").then_some(normalized)
    })
}

fn normalize_path(path: &str) -> String {
    path.trim()
        .replace('\\', "/")
        .trim_start_matches("./")
        .trim_matches('/')
        .to_string()
}

fn scope_matches(file_path: &str, scope: Option<&str>) -> bool {
    match scope {
        None => true,
        Some(scope) => file_path == scope || file_path.starts_with(&format!("{scope}/")),
    }
}

fn top_level_dir(file_path: &str) -> Option<String> {
    file_path
        .split('/')
        .find(|component| !component.is_empty())
        .map(ToString::to_string)
}

fn parent_scope(file_path: &str) -> Option<String> {
    let mut components: Vec<&str> = file_path
        .split('/')
        .filter(|component| !component.is_empty())
        .collect();
    if components.len() <= 1 {
        return None;
    }
    components.pop();
    Some(components.join("/"))
}

fn common_parent_scope(files: &[&str]) -> Option<String> {
    if files.len() < 2 {
        return None;
    }

    let mut prefixes: Vec<Vec<String>> = files
        .iter()
        .filter_map(|file| {
            parent_scope(file).map(|scope| scope.split('/').map(ToString::to_string).collect())
        })
        .collect();
    if prefixes.len() != files.len() || prefixes.is_empty() {
        return None;
    }

    let mut common = prefixes.remove(0);
    for parts in prefixes {
        let shared = common
            .iter()
            .zip(parts.iter())
            .take_while(|(left, right)| left == right)
            .count();
        common.truncate(shared);
        if common.is_empty() {
            return None;
        }
    }

    (common.len() >= 2).then(|| common.join("/"))
}

fn suggested_scope_from_candidates(candidates: &[CandidateRow]) -> Option<String> {
    let mut counts = HashMap::new();
    for candidate in candidates {
        if let Some(scope) = parent_scope(&candidate.file_path) {
            *counts.entry(scope).or_insert(0usize) += 1;
        }
    }

    counts
        .into_iter()
        .max_by(|left, right| {
            left.1
                .cmp(&right.1)
                .then_with(|| right.0.len().cmp(&left.0.len()))
                .then_with(|| left.0.cmp(&right.0))
        })
        .map(|(scope, _)| scope)
}

fn out_of_scope_anchor_refusal(
    anchor_kind: &str,
    anchor_value: &str,
    scope: &str,
    seeds: &[CandidateRow],
) -> Option<ImpactResultEnvelope> {
    let first_out_of_scope = seeds
        .iter()
        .find(|seed| !scope_matches(&seed.file_path, Some(scope)))?;
    let suggested_scope = suggested_scope_from_candidates(seeds)
        .or_else(|| parent_scope(&first_out_of_scope.file_path));

    Some(refused_result(
        "anchor_out_of_scope",
        format!(
            "{anchor_kind} anchor `{anchor_value}` resolves to `{}`, which is outside requested scope `{scope}`.",
            first_out_of_scope.file_path
        ),
        impact_hint(
            "align_anchor_and_scope",
            "Choose an anchor inside the requested scope or retry without `--scope`.",
            suggested_scope,
            Some(first_out_of_scope.segment_id.clone()),
        ),
    ))
}

fn is_test_path(file_path: &str) -> bool {
    let lower = file_path.to_ascii_lowercase();
    lower.contains("/tests/")
        || lower.contains("/test/")
        || lower.contains("/spec/")
        || lower.contains("/__tests__/")
        || lower.ends_with("_test.rs")
        || lower.ends_with("_spec.rs")
        || lower.ends_with(".test.ts")
        || lower.ends_with(".spec.ts")
        || lower.ends_with(".test.js")
        || lower.ends_with(".spec.js")
}

fn test_file_priority(file_path: &str, anchor_files: &[String]) -> usize {
    if anchor_files
        .iter()
        .any(|anchor_file| shares_test_stem(anchor_file, file_path))
    {
        0
    } else {
        1
    }
}

fn shares_test_stem(anchor_file: &str, test_file: &str) -> bool {
    let Some(anchor_stem) = normalized_stem(anchor_file) else {
        return false;
    };
    let Some(test_stem) = normalized_stem(test_file) else {
        return false;
    };

    anchor_stem == test_stem || test_stem.contains(&anchor_stem) || anchor_stem.contains(&test_stem)
}

fn normalized_stem(path: &str) -> Option<String> {
    let file_name = path.rsplit('/').next()?;
    let stem = file_name.split('.').next()?;
    Some(
        stem.to_ascii_lowercase()
            .trim_start_matches("test_")
            .trim_start_matches("spec_")
            .trim_end_matches("_test")
            .trim_end_matches("_spec")
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{db::Db, schema, segments};

    struct SegmentFixture<'a> {
        id: &'a str,
        file_path: &'a str,
        line_start: usize,
        block_type: &'a str,
        role: &'a str,
        defined_symbols: &'a [&'a str],
        referenced_symbols: &'a [&'a str],
        called_symbols: &'a [&'a str],
    }

    async fn setup() -> (Db, Connection) {
        let db = Db::open_memory().await.unwrap();
        let conn = db.connect().unwrap();
        schema::initialize(&conn).await.unwrap();
        (db, conn)
    }

    fn make_segment(fixture: SegmentFixture<'_>) -> segments::SegmentInsert {
        segments::SegmentInsert {
            id: fixture.id.to_string(),
            file_path: fixture.file_path.to_string(),
            language: "rust".to_string(),
            block_type: fixture.block_type.to_string(),
            content: format!("fn {}() {{}}", fixture.id),
            line_start: fixture.line_start as i64,
            line_end: (fixture.line_start + 4) as i64,
            embedding_vec: None,
            breadcrumb: None,
            complexity: 1,
            role: fixture.role.to_string(),
            defined_symbols: serde_json::to_string(fixture.defined_symbols).unwrap(),
            referenced_symbols: serde_json::to_string(fixture.referenced_symbols).unwrap(),
            called_symbols: serde_json::to_string(fixture.called_symbols).unwrap(),
            file_hash: format!("hash-{}", fixture.file_path),
        }
    }

    async fn insert_segments(conn: &Connection, segments_to_insert: Vec<segments::SegmentInsert>) {
        for segment in segments_to_insert {
            segments::upsert_segment(conn, &segment).await.unwrap();
        }
    }

    #[tokio::test]
    async fn symbol_anchor_refuses_broad_match_sets() {
        let (_db, conn) = setup().await;
        insert_segments(
            &conn,
            vec![
                make_segment(SegmentFixture {
                    id: "cfg-auth",
                    file_path: "src/auth/config.rs",
                    line_start: 1,
                    block_type: "struct",
                    role: "DEFINITION",
                    defined_symbols: &["Config"],
                    referenced_symbols: &[],
                    called_symbols: &[],
                }),
                make_segment(SegmentFixture {
                    id: "cfg-cache",
                    file_path: "src/cache/config.rs",
                    line_start: 1,
                    block_type: "struct",
                    role: "DEFINITION",
                    defined_symbols: &["Config"],
                    referenced_symbols: &[],
                    called_symbols: &[],
                }),
                make_segment(SegmentFixture {
                    id: "cfg-ui",
                    file_path: "src/ui/config.rs",
                    line_start: 1,
                    block_type: "struct",
                    role: "DEFINITION",
                    defined_symbols: &["Config"],
                    referenced_symbols: &[],
                    called_symbols: &[],
                }),
                make_segment(SegmentFixture {
                    id: "cfg-tests",
                    file_path: "tests/config_test.rs",
                    line_start: 1,
                    block_type: "function",
                    role: "DEFINITION",
                    defined_symbols: &["Config"],
                    referenced_symbols: &[],
                    called_symbols: &[],
                }),
            ],
        )
        .await;

        let engine = ImpactHorizonEngine::new(&conn);
        let result = engine
            .explore(ImpactRequest {
                anchor: ImpactAnchor::Symbol {
                    name: "Config".to_string(),
                },
                scope: None,
                depth: 2,
                limit: 20,
            })
            .await
            .unwrap();

        assert_eq!(result.status, ImpactStatus::Refused);
        assert_eq!(result.refusal.unwrap().reason, "symbol_too_broad");
        assert_eq!(result.hint.unwrap().code, "narrow_with_scope");
    }

    #[tokio::test]
    async fn symbol_anchor_expands_with_explicit_scope() {
        let (_db, conn) = setup().await;
        insert_segments(
            &conn,
            vec![
                make_segment(SegmentFixture {
                    id: "cfg-auth",
                    file_path: "src/auth/config.rs",
                    line_start: 1,
                    block_type: "struct",
                    role: "DEFINITION",
                    defined_symbols: &["Config"],
                    referenced_symbols: &[],
                    called_symbols: &[],
                }),
                make_segment(SegmentFixture {
                    id: "cfg-auth-builder",
                    file_path: "src/auth/builder.rs",
                    line_start: 10,
                    block_type: "function",
                    role: "ORCHESTRATION",
                    defined_symbols: &["build_auth"],
                    referenced_symbols: &[],
                    called_symbols: &["Config"],
                }),
                make_segment(SegmentFixture {
                    id: "cfg-cache",
                    file_path: "src/cache/config.rs",
                    line_start: 1,
                    block_type: "struct",
                    role: "DEFINITION",
                    defined_symbols: &["Config"],
                    referenced_symbols: &[],
                    called_symbols: &[],
                }),
            ],
        )
        .await;

        let engine = ImpactHorizonEngine::new(&conn);
        let result = engine
            .explore(ImpactRequest {
                anchor: ImpactAnchor::Symbol {
                    name: "Config".to_string(),
                },
                scope: Some("src/auth".to_string()),
                depth: 2,
                limit: 20,
            })
            .await
            .unwrap();

        assert_eq!(result.status, ImpactStatus::ExpandedScoped);
        let resolved = result.resolved_anchor.unwrap();
        assert_eq!(resolved.scope, Some("src/auth".to_string()));
        assert_eq!(resolved.seed_segment_ids, vec!["cfg-auth".to_string()]);
        assert_eq!(result.results[0].segment_id, "cfg-auth-builder");
    }

    #[tokio::test]
    async fn file_line_anchor_chooses_nearest_segment() {
        let (_db, conn) = setup().await;
        insert_segments(
            &conn,
            vec![
                make_segment(SegmentFixture {
                    id: "early",
                    file_path: "src/config.rs",
                    line_start: 1,
                    block_type: "function",
                    role: "DEFINITION",
                    defined_symbols: &["early"],
                    referenced_symbols: &[],
                    called_symbols: &[],
                }),
                make_segment(SegmentFixture {
                    id: "late",
                    file_path: "src/config.rs",
                    line_start: 20,
                    block_type: "function",
                    role: "DEFINITION",
                    defined_symbols: &["late"],
                    referenced_symbols: &[],
                    called_symbols: &[],
                }),
            ],
        )
        .await;

        let engine = ImpactHorizonEngine::new(&conn);
        let result = engine
            .explore(ImpactRequest {
                anchor: ImpactAnchor::File {
                    path: "src/config.rs".to_string(),
                    line: Some(18),
                },
                scope: None,
                depth: 1,
                limit: 5,
            })
            .await
            .unwrap();

        assert_eq!(result.status, ImpactStatus::Empty);
        let resolved = result.resolved_anchor.unwrap();
        assert_eq!(resolved.kind, "file_line");
        assert_eq!(resolved.seed_segment_ids, vec!["late".to_string()]);
        assert!(result.results.is_empty());
        let contextual = result
            .contextual_results
            .expect("same-file observations should remain contextual");
        assert_eq!(contextual.len(), 1);
        assert_eq!(contextual[0].segment_id, "early");
        assert_eq!(contextual[0].reasons[0].kind, "same_file");
        assert!(
            result
                .results
                .iter()
                .chain(contextual.iter())
                .all(|candidate| candidate.segment_id != "late"),
            "resolved anchor seed should never echo back as an impact candidate"
        );
    }

    #[tokio::test]
    async fn file_anchor_refuses_when_scope_excludes_anchor_file() {
        let (_db, conn) = setup().await;
        insert_segments(
            &conn,
            vec![make_segment(SegmentFixture {
                id: "auth-runtime",
                file_path: "src/auth/runtime.rs",
                line_start: 1,
                block_type: "function",
                role: "IMPLEMENTATION",
                defined_symbols: &["load_runtime"],
                referenced_symbols: &[],
                called_symbols: &[],
            })],
        )
        .await;

        let engine = ImpactHorizonEngine::new(&conn);
        let result = engine
            .explore(ImpactRequest {
                anchor: ImpactAnchor::File {
                    path: "src/auth/runtime.rs".to_string(),
                    line: None,
                },
                scope: Some("src/cache".to_string()),
                depth: 1,
                limit: 5,
            })
            .await
            .unwrap();

        assert_eq!(result.status, ImpactStatus::Refused);
        assert_eq!(result.refusal.unwrap().reason, "anchor_out_of_scope");
        assert_eq!(result.hint.unwrap().code, "align_anchor_and_scope");
    }

    #[tokio::test]
    async fn scoped_file_anchor_without_relations_returns_empty_scoped() {
        let (_db, conn) = setup().await;
        insert_segments(
            &conn,
            vec![make_segment(SegmentFixture {
                id: "auth-runtime",
                file_path: "src/auth/runtime.rs",
                line_start: 1,
                block_type: "function",
                role: "IMPLEMENTATION",
                defined_symbols: &["load_runtime"],
                referenced_symbols: &[],
                called_symbols: &[],
            })],
        )
        .await;

        let engine = ImpactHorizonEngine::new(&conn);
        let result = engine
            .explore(ImpactRequest {
                anchor: ImpactAnchor::File {
                    path: "src/auth/runtime.rs".to_string(),
                    line: None,
                },
                scope: Some("src/auth".to_string()),
                depth: 1,
                limit: 5,
            })
            .await
            .unwrap();

        assert_eq!(result.status, ImpactStatus::EmptyScoped);
        assert!(result.results.is_empty());
        assert!(result.contextual_results.is_none());
        assert_eq!(result.hint.unwrap().code, "no_likely_impact");
    }

    #[tokio::test]
    async fn segment_anchor_refuses_when_scope_excludes_anchor_file() {
        let (_db, conn) = setup().await;
        insert_segments(
            &conn,
            vec![make_segment(SegmentFixture {
                id: "auth-runtime",
                file_path: "src/auth/runtime.rs",
                line_start: 1,
                block_type: "function",
                role: "IMPLEMENTATION",
                defined_symbols: &["load_runtime"],
                referenced_symbols: &[],
                called_symbols: &[],
            })],
        )
        .await;

        let engine = ImpactHorizonEngine::new(&conn);
        let result = engine
            .explore(ImpactRequest {
                anchor: ImpactAnchor::Segment {
                    id: "auth-runtime".to_string(),
                },
                scope: Some("src/cache".to_string()),
                depth: 1,
                limit: 5,
            })
            .await
            .unwrap();

        assert_eq!(result.status, ImpactStatus::Refused);
        assert_eq!(result.refusal.unwrap().reason, "anchor_out_of_scope");
    }

    #[tokio::test]
    async fn segment_anchor_ranks_relation_same_file_and_test_candidates() {
        let (_db, conn) = setup().await;
        insert_segments(
            &conn,
            vec![
                make_segment(SegmentFixture {
                    id: "load-config",
                    file_path: "src/config.rs",
                    line_start: 1,
                    block_type: "function",
                    role: "DEFINITION",
                    defined_symbols: &["load_config"],
                    referenced_symbols: &[],
                    called_symbols: &[],
                }),
                make_segment(SegmentFixture {
                    id: "parse-config",
                    file_path: "src/config.rs",
                    line_start: 20,
                    block_type: "function",
                    role: "IMPLEMENTATION",
                    defined_symbols: &["parse_config"],
                    referenced_symbols: &[],
                    called_symbols: &[],
                }),
                make_segment(SegmentFixture {
                    id: "boot-app",
                    file_path: "src/app.rs",
                    line_start: 5,
                    block_type: "function",
                    role: "ORCHESTRATION",
                    defined_symbols: &["boot_app"],
                    referenced_symbols: &[],
                    called_symbols: &["load_config"],
                }),
                make_segment(SegmentFixture {
                    id: "config-test",
                    file_path: "tests/config_test.rs",
                    line_start: 3,
                    block_type: "function",
                    role: "DEFINITION",
                    defined_symbols: &["config_test"],
                    referenced_symbols: &["load_config"],
                    called_symbols: &[],
                }),
            ],
        )
        .await;

        let engine = ImpactHorizonEngine::new(&conn);
        let result = engine
            .explore(ImpactRequest {
                anchor: ImpactAnchor::Segment {
                    id: "load-config".to_string(),
                },
                scope: None,
                depth: 2,
                limit: 20,
            })
            .await
            .unwrap();

        assert_eq!(result.status, ImpactStatus::Expanded);
        assert_eq!(result.results.len(), 1);
        assert_eq!(result.results[0].segment_id, "boot-app");
        assert_eq!(result.results[0].reasons[0].kind, "called_by");
        let contextual = result
            .contextual_results
            .expect("same-file and test candidates should remain contextual");
        assert_eq!(contextual.len(), 2);
        assert_eq!(contextual[0].segment_id, "parse-config");
        assert_eq!(contextual[0].reasons[0].kind, "same_file");
        assert_eq!(contextual[1].segment_id, "config-test");
        assert!(matches!(
            contextual[1].reasons[0].kind.as_str(),
            "test_for_file" | "test_for_symbol"
        ));
        assert!(
            result
                .results
                .iter()
                .chain(contextual.iter())
                .all(|candidate| candidate.segment_id != "load-config"),
            "resolved anchor seed should never echo back as an impact candidate"
        );
    }

    #[tokio::test]
    async fn collect_test_observations_honors_file_budget() {
        let (_db, conn) = setup().await;
        let seed = make_segment(SegmentFixture {
            id: "load-config",
            file_path: "src/config.rs",
            line_start: 1,
            block_type: "function",
            role: "DEFINITION",
            defined_symbols: &["load_config"],
            referenced_symbols: &[],
            called_symbols: &[],
        });

        let mut fixtures = vec![seed];
        for idx in 1..=4 {
            fixtures.push(make_segment(SegmentFixture {
                id: Box::leak(format!("config-test-{idx}").into_boxed_str()),
                file_path: Box::leak(format!("tests/config_test_{idx}.rs").into_boxed_str()),
                line_start: idx,
                block_type: "function",
                role: "DEFINITION",
                defined_symbols: &[],
                referenced_symbols: &["load_config"],
                called_symbols: &[],
            }));
        }
        insert_segments(&conn, fixtures).await;

        let engine = ImpactHorizonEngine::new(&conn);
        let seed = CandidateRow {
            segment_id: "load-config".to_string(),
            file_path: "src/config.rs".to_string(),
            language: "rust".to_string(),
            block_type: "function".to_string(),
            line_number: 1,
            line_end: 5,
            breadcrumb: None,
            complexity: Some(1),
            role: Some(SegmentRole::Definition),
            defined_symbols: Some(vec!["load_config".to_string()]),
            referenced_symbols: None,
            called_symbols: None,
        };
        let seed_ids = HashSet::from(["load-config".to_string()]);

        let observations = engine
            .collect_test_observations(&[seed], &seed_ids, None, 2, 10)
            .await
            .unwrap();

        assert_eq!(observations.len(), 2);
        assert!(observations
            .iter()
            .all(|observation| observation.candidate.file_path.starts_with("tests/")));
    }
}
