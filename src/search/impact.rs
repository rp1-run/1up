#![allow(dead_code)]

use std::collections::{BTreeSet, HashMap, HashSet};

use libsql::Connection;
use serde::Serialize;

use crate::search::retrieval::CandidateRow;
use crate::search::symbol::SymbolSearchEngine;
use crate::shared::constants::{MAX_RESULTS_PER_FILE, MAX_SEARCH_RESULTS};
use crate::shared::errors::{OneupError, SearchError};
use crate::shared::symbols::{
    clean_owner_components, normalize_symbolish, split_symbol_components,
    EDGE_IDENTITY_BARE_IDENTIFIER, EDGE_IDENTITY_CONSTRUCTOR_LIKE, EDGE_IDENTITY_MACRO_LIKE,
    EDGE_IDENTITY_MEMBER_ACCESS, EDGE_IDENTITY_METHOD_RECEIVER, EDGE_IDENTITY_QUALIFIED_PATH,
};
use crate::shared::types::SegmentRole;
use crate::storage::relations::{
    get_inbound_relations_by_lookup_symbol, get_outbound_relations, RelationKind, StoredRelation,
};
use crate::storage::segments::{
    get_segment_by_id, get_segments_by_file, get_test_file_paths, StoredSegment,
};

const DEFAULT_IMPACT_DEPTH: usize = 2;
const MAX_IMPACT_DEPTH: usize = 2;
const MAX_FILE_SEEDS: usize = 5;
const MAX_SYMBOL_SEEDS: usize = 3;
const MAX_SYMBOL_FILES: usize = 3;
const MAX_SYMBOL_TOP_LEVEL_DIRS: usize = 2;
const MAX_OUTBOUND_RELATIONS_PER_HOP: usize = 8;
const MAX_INBOUND_RELATIONS_PER_HOP: usize = 8;
const MAX_DEFINITION_TARGETS_PER_SYMBOL: usize = 3;
const MAX_TEST_FILE_BUDGET: usize = 12;
const TEST_FILE_QUERY_FACTOR: usize = 2;

const CALL_WEIGHT: f64 = 1.0;
const CONFORMANCE_WEIGHT: f64 = 1.05;
const SAME_FILE_WEIGHT: f64 = 0.70;
const REFERENCE_WEIGHT: f64 = 0.65;
const TEST_WEIGHT: f64 = 0.55;
const HOP_DECAY: f64 = 0.70;
const SCOPE_BOOST: f64 = 1.10;
const ROLE_BOOST: f64 = 1.05;
const RELATION_SOLO_PRIMARY_THRESHOLD: f64 = 0.45;
const RELATION_PRIMARY_THRESHOLD: f64 = 0.55;
const RELATION_CONTEXTUAL_THRESHOLD: f64 = 0.35;
const RELATION_AMBIGUITY_MARGIN: f64 = 0.08;
const OWNER_ALIGNMENT_SHORTLIST_THRESHOLD: f64 = 0.60;
const OWNER_ALIGNMENT_SIGNAL_THRESHOLD: f64 = 0.60;
const EDGE_IDENTITY_SIGNAL_THRESHOLD: f64 = 0.65;
const PATH_AFFINITY_SIGNAL_THRESHOLD: f64 = 0.30;
const ROLE_SIGNAL_THRESHOLD: f64 = 0.75;
const MIN_PRIMARY_CORROBORATION_SIGNALS: usize = 2;
const LOW_SIGNAL_WRAPPER_PENALTY: f64 = 0.82;
const LOW_SIGNAL_DECLARATION_PENALTY: f64 = 0.68;
const LOW_SIGNAL_UNALIGNED_RECEIVER_PENALTY: f64 = 0.35;

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
    anchor_symbols: HashSet<String>,
    prefer_high_signal_paths: bool,
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

#[derive(Clone)]
struct CandidateObservation {
    candidate: CandidateRow,
    score: f64,
    hop: usize,
    reason: ImpactReason,
    bucket: ObservationBucket,
}

struct ScoredRelationCandidate {
    candidate: CandidateRow,
    confidence: f64,
    definition_owner_fingerprint: String,
    owner_alignment: f64,
    exact_identity: bool,
    corroboration_signals: usize,
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
            let mut hop_aggregates = HashMap::new();

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

                    observe_candidate(&mut aggregates, observation.clone());
                    observe_candidate(&mut hop_aggregates, observation);
                }
            }

            for aggregate in hop_aggregates.into_values() {
                let should_expand = aggregate_bucket(
                    &aggregate,
                    &resolution.anchor_symbols,
                    resolution.prefer_high_signal_paths,
                ) == Some(ObservationBucket::Primary);
                let candidate_for_frontier = aggregate.candidate.clone();
                let candidate_id = candidate_for_frontier.segment_id.clone();

                if should_expand
                    && hop < depth
                    && expanded.insert(candidate_id.clone())
                    && queued.insert(candidate_id)
                {
                    next_frontier.push(candidate_for_frontier);
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

        let finalized = finalize_impact_results(
            aggregates,
            limit,
            &resolution.anchor_symbols,
            resolution.prefer_high_signal_paths,
        );
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
            prefer_high_signal_paths: seeds
                .iter()
                .any(|seed| !is_low_signal_path(&seed.file_path)),
            seeds,
            effective_scope: explicit_scope,
            anchor_symbols: HashSet::new(),
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
        let prefer_high_signal_paths = !is_low_signal_path(&seed.file_path);
        Ok(ResolveOutcome::Resolved(AnchorResolution {
            resolved_anchor: resolved_anchor(
                "segment",
                id.to_string(),
                None,
                explicit_scope.clone(),
                std::slice::from_ref(&seed),
            ),
            seeds: vec![seed],
            prefer_high_signal_paths,
            effective_scope: explicit_scope,
            anchor_symbols: HashSet::new(),
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
        if seeds
            .iter()
            .any(|candidate| !is_low_signal_path(&candidate.file_path))
        {
            seeds.retain(|candidate| !is_low_signal_path(&candidate.file_path));
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
        let mut anchor_symbols = HashSet::new();
        let canonical_name = normalize_symbolish(name);
        if !canonical_name.is_empty() {
            anchor_symbols.insert(canonical_name);
        }
        let prefer_high_signal_paths = seeds
            .iter()
            .any(|seed| !is_low_signal_path(&seed.file_path));

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
            anchor_symbols,
            prefer_high_signal_paths,
        }))
    }

    async fn collect_relation_observations(
        &self,
        source: &CandidateRow,
        scope: Option<&str>,
        hop: usize,
    ) -> Result<Vec<CandidateObservation>, OneupError> {
        let mut observations = Vec::new();
        let outbound = self
            .collect_outbound_relations_with_budget(&source.segment_id)
            .await?;

        for relation in outbound {
            let reason_kind = match relation.relation_kind {
                RelationKind::Call => "calls",
                RelationKind::Conformance => "conforms_to",
                RelationKind::Reference => "references_symbol",
            };
            let weight = base_weight(relation.relation_kind);
            if let Some((target, confidence, bucket)) = self
                .select_relation_target(source, &relation, scope)
                .await?
            {
                observations.push(CandidateObservation {
                    score: weighted_score(weight * confidence, hop, &target, scope),
                    hop,
                    reason: ImpactReason {
                        kind: reason_kind.to_string(),
                        symbol: Some(relation.raw_target_symbol.clone()),
                        from_segment_id: Some(source.segment_id.clone()),
                    },
                    candidate: target,
                    bucket,
                });
            }
        }

        let defined_symbols = source.defined_symbols.clone().unwrap_or_default();
        let mut remaining = MAX_INBOUND_RELATIONS_PER_HOP;
        for symbol in defined_symbols {
            if remaining == 0 {
                break;
            }

            let Some(lookup_symbol) = symbol_lookup_tail(&symbol) else {
                continue;
            };

            let inbound = self
                .collect_inbound_relations_with_budget(&lookup_symbol, &mut remaining)
                .await?;

            for relation in inbound {
                let Some(source_candidate) =
                    self.load_candidate(&relation.source_segment_id).await?
                else {
                    continue;
                };
                let reason_kind = match relation.relation_kind {
                    RelationKind::Call => "called_by",
                    RelationKind::Conformance => "implemented_by",
                    RelationKind::Reference => "references_symbol",
                };
                let emitted_candidates = self
                    .emit_inbound_relation_candidates(&relation, &source_candidate, scope)
                    .await?;

                for candidate in emitted_candidates {
                    if !scope_matches(&candidate.file_path, scope)
                        || is_test_path(&candidate.file_path)
                    {
                        continue;
                    }

                    let scored =
                        score_relation_candidate(&source_candidate, &relation, source, &candidate);
                    let Some(bucket) = relation_observation_bucket(&scored, None) else {
                        continue;
                    };

                    observations.push(CandidateObservation {
                        score: weighted_score(
                            base_weight(relation.relation_kind) * scored.confidence,
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
                        bucket,
                    });
                }
            }
        }

        Ok(observations)
    }

    async fn collect_outbound_relations_with_budget(
        &self,
        source_segment_id: &str,
    ) -> Result<Vec<StoredRelation>, OneupError> {
        let mut relations = Vec::new();
        let mut remaining = MAX_OUTBOUND_RELATIONS_PER_HOP;

        for relation_kind in [
            RelationKind::Conformance,
            RelationKind::Call,
            RelationKind::Reference,
        ] {
            if remaining == 0 {
                break;
            }

            let mut fetched = get_outbound_relations(
                self.conn,
                source_segment_id,
                Some(relation_kind),
                remaining,
            )
            .await?;
            remaining = remaining.saturating_sub(fetched.len());
            relations.append(&mut fetched);
        }

        Ok(relations)
    }

    async fn collect_inbound_relations_with_budget(
        &self,
        lookup_symbol: &str,
        remaining: &mut usize,
    ) -> Result<Vec<StoredRelation>, OneupError> {
        let mut relations = Vec::new();

        for relation_kind in [
            RelationKind::Conformance,
            RelationKind::Call,
            RelationKind::Reference,
        ] {
            if *remaining == 0 {
                break;
            }

            let mut fetched = get_inbound_relations_by_lookup_symbol(
                self.conn,
                lookup_symbol,
                Some(relation_kind),
                *remaining,
            )
            .await?;
            *remaining = (*remaining).saturating_sub(fetched.len());
            relations.append(&mut fetched);
        }

        Ok(relations)
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
        relation: &StoredRelation,
        scope: Option<&str>,
    ) -> Result<Vec<CandidateRow>, OneupError> {
        let engine = SymbolSearchEngine::new(self.conn);
        let mut candidates = engine
            .find_definition_candidates_by_canonical(&relation.lookup_canonical_symbol)
            .await?;
        candidates.retain(|candidate| scope_matches(&candidate.file_path, scope));

        if relation.qualifier_fingerprint.is_empty() {
            return Ok(candidates);
        }

        let exact_owner_matches = candidates
            .iter()
            .filter(|candidate| relation_exact_owner_match(relation, candidate))
            .cloned()
            .collect::<Vec<_>>();
        if !exact_owner_matches.is_empty() {
            return Ok(exact_owner_matches);
        }

        Ok(candidates)
    }

    async fn select_relation_target(
        &self,
        source: &CandidateRow,
        relation: &StoredRelation,
        scope: Option<&str>,
    ) -> Result<Option<(CandidateRow, f64, ObservationBucket)>, OneupError> {
        let scored = self
            .resolve_relation_targets(relation, scope)
            .await?
            .into_iter()
            .filter(|candidate| !is_test_path(&candidate.file_path))
            .filter_map(|candidate| {
                let scored = score_relation_candidate(source, relation, &candidate, &candidate);
                relation_candidate_edge_compatible(relation, &scored).then_some(scored)
            })
            .collect::<Vec<_>>();

        let (preferred, deferred): (Vec<_>, Vec<_>) = scored
            .into_iter()
            .partition(|candidate| !is_test_context_candidate(&candidate.candidate));

        if let Some(selected) = select_best_relation_candidate(preferred) {
            return Ok(Some(selected));
        }

        Ok(select_best_relation_candidate(deferred))
    }

    async fn load_candidate(&self, segment_id: &str) -> Result<Option<CandidateRow>, OneupError> {
        Ok(get_segment_by_id(self.conn, segment_id)
            .await?
            .map(candidate_from_stored_segment))
    }

    async fn emit_inbound_relation_candidates(
        &self,
        relation: &StoredRelation,
        source_candidate: &CandidateRow,
        scope: Option<&str>,
    ) -> Result<Vec<CandidateRow>, OneupError> {
        match relation.relation_kind {
            RelationKind::Conformance => {
                self.resolve_conformance_source_candidates(source_candidate, scope)
                    .await
            }
            RelationKind::Call | RelationKind::Reference => Ok(vec![source_candidate.clone()]),
        }
    }

    async fn resolve_conformance_source_candidates(
        &self,
        source_candidate: &CandidateRow,
        scope: Option<&str>,
    ) -> Result<Vec<CandidateRow>, OneupError> {
        if source_candidate.is_definition_like() {
            return Ok(vec![source_candidate.clone()]);
        }

        let engine = SymbolSearchEngine::new(self.conn);
        let mut emitted = Vec::new();
        let mut seen = HashSet::new();

        for lookup_symbol in conformance_source_lookup_symbols(source_candidate) {
            for candidate in engine
                .find_definition_candidates_by_canonical(&lookup_symbol)
                .await?
            {
                if candidate.segment_id == source_candidate.segment_id
                    || !scope_matches(&candidate.file_path, scope)
                    || is_test_path(&candidate.file_path)
                {
                    continue;
                }

                if seen.insert(candidate.segment_id.clone()) {
                    emitted.push(candidate);
                }
            }
        }

        if emitted.is_empty() {
            Ok(vec![source_candidate.clone()])
        } else {
            Ok(emitted)
        }
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
    anchor_symbols: &HashSet<String>,
    prefer_high_signal_paths: bool,
) -> FinalizedImpactResults {
    let mut primary = Vec::new();
    let mut contextual = Vec::new();

    for aggregate in aggregates.into_values() {
        let aggregate_bucket =
            aggregate_bucket(&aggregate, anchor_symbols, prefer_high_signal_paths);
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

        if aggregate_bucket == Some(ObservationBucket::Primary) {
            let reasons = if primary_score > 0.0 && !primary_reasons.is_empty() {
                primary_reasons
                    .into_iter()
                    .map(|reason| reason.reason)
                    .chain(contextual_reasons.into_iter().map(|reason| reason.reason))
                    .take(3)
                    .collect()
            } else {
                contextual_reasons
                    .into_iter()
                    .map(|reason| reason.reason)
                    .take(3)
                    .collect()
            };
            primary.push(build_impact_candidate(
                candidate,
                primary_score.max(contextual_score),
                primary_hop.or(contextual_hop).unwrap_or_default(),
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

    let primary = rank_impact_candidates(primary, limit);
    let contextual_limit = limit.saturating_sub(primary.len());
    let contextual = if contextual_limit == 0 {
        Vec::new()
    } else {
        rank_impact_candidates(contextual, contextual_limit)
    };

    FinalizedImpactResults {
        primary,
        contextual,
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
        impact_candidate_priority(right)
            .cmp(&impact_candidate_priority(left))
            .then_with(|| {
                impact_candidate_reason_priority(right).cmp(&impact_candidate_reason_priority(left))
            })
            .then_with(|| {
                implemented_by_reason_count(right).cmp(&implemented_by_reason_count(left))
            })
            .then_with(|| direct_reason_count(right).cmp(&direct_reason_count(left)))
            .then_with(|| right.score.total_cmp(&left.score))
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

fn impact_candidate_priority(candidate: &ImpactCandidate) -> usize {
    usize::from(implemented_by_reason_count(candidate) > 0)
}

fn impact_candidate_reason_priority(candidate: &ImpactCandidate) -> usize {
    candidate
        .reasons
        .iter()
        .filter_map(|reason| match reason.kind.as_str() {
            "implemented_by" => Some(4),
            "called_by" => Some(3),
            "calls" => Some(2),
            "conforms_to" => Some(1),
            _ => None,
        })
        .max()
        .unwrap_or_default()
}

fn implemented_by_reason_count(candidate: &ImpactCandidate) -> usize {
    candidate
        .reasons
        .iter()
        .filter(|reason| reason.kind == "implemented_by")
        .count()
}

fn direct_reason_count(candidate: &ImpactCandidate) -> usize {
    candidate
        .reasons
        .iter()
        .filter(|reason| {
            matches!(
                reason.kind.as_str(),
                "implemented_by" | "called_by" | "calls"
            )
        })
        .count()
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
        RelationKind::Conformance => CONFORMANCE_WEIGHT,
        RelationKind::Reference => REFERENCE_WEIGHT,
    }
}

fn score_relation_candidate(
    source: &CandidateRow,
    relation: &StoredRelation,
    target: &CandidateRow,
    emitted_candidate: &CandidateRow,
) -> ScoredRelationCandidate {
    let symbol_score = relation_symbol_score(relation, target);
    let definition_owner_fingerprint = definition_owner_fingerprint(target);
    let owner_alignment = owner_alignment_score(
        &relation.qualifier_fingerprint,
        &definition_owner_fingerprint,
    );
    let scope_affinity = path_affinity_score(&source.file_path, &target.file_path);
    let role_score = relation_role_score(emitted_candidate.role);
    let edge_identity_score =
        relation_edge_identity_score(relation, target, owner_alignment, symbol_score);
    let exact_identity = has_exact_structural_identity(relation, symbol_score);
    let corroboration_signals = relation_corroboration_signal_count(
        exact_identity,
        owner_alignment,
        edge_identity_score,
        scope_affinity,
        role_score,
    );
    let relation_kind_score = match relation.relation_kind {
        RelationKind::Call => 1.0,
        RelationKind::Conformance => 1.05,
        RelationKind::Reference => 0.85,
    };
    let owner_gate = if relation.qualifier_fingerprint.is_empty() {
        1.0
    } else {
        0.55 + (0.45 * owner_alignment.max(edge_identity_score))
    };
    let signal_multiplier = relation_signal_multiplier(
        emitted_candidate,
        relation,
        owner_alignment,
        corroboration_signals,
    );
    let confidence = if symbol_score == 0.0 {
        0.0
    } else {
        ((0.30 * symbol_score)
            + (0.25 * owner_alignment)
            + (0.15 * edge_identity_score)
            + (0.15 * scope_affinity)
            + (0.10 * role_score)
            + (0.05 * relation_kind_score))
            * owner_gate
            * signal_multiplier
    };

    ScoredRelationCandidate {
        candidate: emitted_candidate.clone(),
        confidence,
        definition_owner_fingerprint,
        owner_alignment,
        exact_identity,
        corroboration_signals,
    }
}

fn relation_observation_bucket(
    candidate: &ScoredRelationCandidate,
    runner_up: Option<f64>,
) -> Option<ObservationBucket> {
    if candidate.confidence < RELATION_CONTEXTUAL_THRESHOLD {
        return None;
    }

    let primary_threshold = if runner_up.is_some() {
        RELATION_PRIMARY_THRESHOLD
    } else {
        RELATION_SOLO_PRIMARY_THRESHOLD
    };
    let ambiguous = runner_up
        .map(|next| candidate.confidence - next < RELATION_AMBIGUITY_MARGIN)
        .unwrap_or(false);
    let contextual_only_role = matches!(
        candidate.candidate.role,
        Some(SegmentRole::Import | SegmentRole::Docs)
    ) || is_test_context_candidate(&candidate.candidate);
    let ambiguity_signal =
        candidate.exact_identity || candidate.owner_alignment >= OWNER_ALIGNMENT_SIGNAL_THRESHOLD;

    if !contextual_only_role
        && candidate.corroboration_signals >= MIN_PRIMARY_CORROBORATION_SIGNALS
        && (!ambiguous || ambiguity_signal)
        && candidate.confidence >= primary_threshold
    {
        Some(ObservationBucket::Primary)
    } else {
        Some(ObservationBucket::Contextual)
    }
}

fn relation_symbol_score(relation: &StoredRelation, candidate: &CandidateRow) -> f64 {
    candidate
        .defined_symbols
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .fold(0.0, |best, symbol| {
            let canonical = normalize_symbolish(symbol);
            let tail = symbol_lookup_tail(symbol);
            best.max(if canonical == relation.canonical_target_symbol {
                1.0
            } else if tail.as_deref() == Some(relation.lookup_canonical_symbol.as_str()) {
                0.85
            } else {
                0.0
            })
        })
}

fn shortlist_relation_candidates(
    mut candidates: Vec<ScoredRelationCandidate>,
) -> Vec<ScoredRelationCandidate> {
    candidates.sort_by(compare_scored_relation_candidates);

    let owner_shortlist = candidates.iter().any(|candidate| {
        candidate.owner_alignment >= OWNER_ALIGNMENT_SHORTLIST_THRESHOLD
            && !candidate.definition_owner_fingerprint.is_empty()
    });
    if owner_shortlist {
        candidates
            .retain(|candidate| candidate.owner_alignment >= OWNER_ALIGNMENT_SHORTLIST_THRESHOLD);
    }

    candidates.truncate(MAX_DEFINITION_TARGETS_PER_SYMBOL);
    candidates
}

fn select_best_relation_candidate(
    candidates: Vec<ScoredRelationCandidate>,
) -> Option<(CandidateRow, f64, ObservationBucket)> {
    let candidates = shortlist_relation_candidates(candidates);
    let runner_up = candidates.get(1).map(|candidate| candidate.confidence);
    let best = candidates.into_iter().next()?;
    let bucket = relation_observation_bucket(&best, runner_up)?;
    Some((best.candidate, best.confidence, bucket))
}

fn compare_scored_relation_candidates(
    left: &ScoredRelationCandidate,
    right: &ScoredRelationCandidate,
) -> std::cmp::Ordering {
    right
        .confidence
        .total_cmp(&left.confidence)
        .then_with(|| right.corroboration_signals.cmp(&left.corroboration_signals))
        .then_with(|| right.owner_alignment.total_cmp(&left.owner_alignment))
        .then_with(|| left.candidate.file_path.cmp(&right.candidate.file_path))
        .then_with(|| left.candidate.line_number.cmp(&right.candidate.line_number))
        .then_with(|| left.candidate.segment_id.cmp(&right.candidate.segment_id))
}

fn aggregate_bucket(
    aggregate: &CandidateAggregate,
    anchor_symbols: &HashSet<String>,
    prefer_high_signal_paths: bool,
) -> Option<ObservationBucket> {
    if is_test_context_candidate(&aggregate.candidate) {
        return (aggregate.primary_score > 0.0 || aggregate.contextual_score > 0.0)
            .then_some(ObservationBucket::Contextual);
    }
    if prefer_high_signal_paths && is_low_signal_path(&aggregate.candidate.file_path) {
        return (aggregate.primary_score > 0.0 || aggregate.contextual_score > 0.0)
            .then_some(ObservationBucket::Contextual);
    }
    if aggregate.primary_score > 0.0 && !aggregate.primary_reasons.is_empty() {
        return Some(ObservationBucket::Primary);
    }
    if aggregate.contextual_score <= 0.0 || aggregate.contextual_reasons.is_empty() {
        return None;
    }
    if repeated_anchor_relation_support(aggregate, anchor_symbols) {
        return Some(ObservationBucket::Primary);
    }

    Some(ObservationBucket::Contextual)
}

fn repeated_anchor_relation_support(
    aggregate: &CandidateAggregate,
    anchor_symbols: &HashSet<String>,
) -> bool {
    if anchor_symbols.is_empty() {
        return false;
    }

    let mut supporting_sources = HashSet::new();
    let mut supporting_reasons = 0usize;

    for contribution in aggregate.contextual_reasons.values() {
        if !matches!(
            contribution.reason.kind.as_str(),
            "references_symbol" | "calls" | "called_by"
        ) {
            continue;
        }
        let Some(symbol) = contribution.reason.symbol.as_deref() else {
            continue;
        };
        if !anchor_symbols.contains(&normalize_symbolish(symbol)) {
            continue;
        }
        let Some(from_segment_id) = contribution.reason.from_segment_id.as_deref() else {
            continue;
        };

        supporting_reasons += 1;
        supporting_sources.insert(from_segment_id.to_string());
    }

    supporting_reasons >= 2 && supporting_sources.len() >= 2
}

fn relation_exact_owner_match(relation: &StoredRelation, candidate: &CandidateRow) -> bool {
    let qualifier = qualifier_components(&relation.qualifier_fingerprint);
    let definition = qualifier_components(&definition_owner_fingerprint(candidate));
    if qualifier.is_empty() || definition.len() < qualifier.len() {
        return false;
    }

    definition[definition.len() - qualifier.len()..] == qualifier
}

fn owner_alignment_score(qualifier_fingerprint: &str, definition_owner_fingerprint: &str) -> f64 {
    let qualifier = qualifier_components(qualifier_fingerprint);
    let definition = qualifier_components(definition_owner_fingerprint);
    if qualifier.is_empty() || definition.is_empty() {
        return 0.0;
    }

    component_alignment_score(&qualifier, &definition)
}

fn definition_owner_fingerprint(candidate: &CandidateRow) -> String {
    let mut components = path_components(&candidate.file_path);

    if let Some(breadcrumb) = candidate.breadcrumb.as_deref() {
        extend_unique_components(&mut components, breadcrumb_components(breadcrumb));
    }
    if let Some(defined_symbols) = candidate.defined_symbols.as_deref() {
        for symbol in defined_symbols {
            extend_unique_components(&mut components, symbol_owner_components(symbol));
        }
    }

    components.join("/")
}

fn extend_unique_components(into: &mut Vec<String>, components: Vec<String>) {
    for component in components {
        if !into.contains(&component) {
            into.push(component);
        }
    }
}

fn symbol_owner_components(symbol: &str) -> Vec<String> {
    let mut components = clean_owner_components(&split_symbol_components(symbol));
    if components.len() <= 1 {
        return Vec::new();
    }
    components.pop();
    components
}

fn relation_edge_identity_score(
    relation: &StoredRelation,
    target: &CandidateRow,
    owner_alignment: f64,
    symbol_score: f64,
) -> f64 {
    let structural_alignment = if relation.qualifier_fingerprint.is_empty() {
        symbol_score
    } else {
        owner_alignment
    };

    match relation.edge_identity_kind.as_str() {
        EDGE_IDENTITY_QUALIFIED_PATH => 0.35 + (0.65 * owner_alignment),
        EDGE_IDENTITY_MEMBER_ACCESS | EDGE_IDENTITY_METHOD_RECEIVER => {
            let role_bonus = if matches!(
                target.role,
                Some(
                    SegmentRole::Definition
                        | SegmentRole::Implementation
                        | SegmentRole::Orchestration
                )
            ) {
                1.0
            } else {
                0.8
            };
            (0.40 + (0.60 * structural_alignment)) * role_bonus
        }
        EDGE_IDENTITY_CONSTRUCTOR_LIKE => {
            if matches!(
                target.block_type.as_str(),
                "constructor" | "class" | "struct" | "enum"
            ) {
                1.0
            } else {
                0.0
            }
        }
        EDGE_IDENTITY_MACRO_LIKE => {
            if target.block_type == "macro" {
                1.0
            } else {
                0.0
            }
        }
        EDGE_IDENTITY_BARE_IDENTIFIER => {
            if relation.qualifier_fingerprint.is_empty() {
                0.30
            } else {
                0.45 + (0.40 * owner_alignment)
            }
        }
        _ => 0.35 + (0.50 * structural_alignment),
    }
}

fn has_exact_structural_identity(relation: &StoredRelation, symbol_score: f64) -> bool {
    if relation.relation_kind == RelationKind::Conformance {
        return symbol_score >= 0.85;
    }

    symbol_score >= 1.0
        && !(relation.qualifier_fingerprint.is_empty()
            && relation.edge_identity_kind == EDGE_IDENTITY_BARE_IDENTIFIER)
}

fn relation_corroboration_signal_count(
    exact_identity: bool,
    owner_alignment: f64,
    edge_identity_score: f64,
    scope_affinity: f64,
    role_score: f64,
) -> usize {
    usize::from(exact_identity)
        + usize::from(owner_alignment >= OWNER_ALIGNMENT_SIGNAL_THRESHOLD)
        + usize::from(edge_identity_score >= EDGE_IDENTITY_SIGNAL_THRESHOLD)
        + usize::from(scope_affinity >= PATH_AFFINITY_SIGNAL_THRESHOLD)
        + usize::from(role_score >= ROLE_SIGNAL_THRESHOLD)
}

fn component_alignment_score(needle: &[String], haystack: &[String]) -> f64 {
    if needle.is_empty() || haystack.is_empty() {
        return 0.0;
    }

    let mut matched = 0usize;
    for token in haystack {
        if matched < needle.len() && *token == needle[matched] {
            matched += 1;
        }
    }

    let subsequence = matched as f64 / needle.len() as f64;
    let mut suffix = 0usize;
    for (left, right) in needle.iter().rev().zip(haystack.iter().rev()) {
        if left == right {
            suffix += 1;
        } else {
            break;
        }
    }

    let suffix = suffix as f64 / needle.len() as f64;
    (0.65 * subsequence) + (0.35 * suffix)
}

fn path_affinity_score(left: &str, right: &str) -> f64 {
    let left = path_components(left);
    let right = path_components(right);
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }

    let shared = left
        .iter()
        .zip(right.iter())
        .take_while(|(left, right)| left == right)
        .count();
    if shared == 0 {
        0.0
    } else {
        shared as f64 / left.len().max(right.len()) as f64
    }
}

fn relation_role_score(role: Option<SegmentRole>) -> f64 {
    match role {
        Some(SegmentRole::Implementation) => 1.0,
        Some(SegmentRole::Orchestration) => 0.95,
        Some(SegmentRole::Definition) => 0.75,
        Some(SegmentRole::Import) => 0.10,
        Some(SegmentRole::Docs) => 0.05,
        None => 0.60,
    }
}

fn relation_signal_multiplier(
    candidate: &CandidateRow,
    relation: &StoredRelation,
    owner_alignment: f64,
    corroboration_signals: usize,
) -> f64 {
    let mut multiplier = 1.0;

    if corroboration_signals < MIN_PRIMARY_CORROBORATION_SIGNALS {
        multiplier *= 0.85;
    }

    if is_wrapper_like_candidate(candidate) {
        multiplier *= LOW_SIGNAL_WRAPPER_PENALTY;
    }

    if owner_alignment == 0.0
        && !relation.qualifier_fingerprint.is_empty()
        && matches!(
            relation.edge_identity_kind.as_str(),
            EDGE_IDENTITY_MEMBER_ACCESS | EDGE_IDENTITY_METHOD_RECEIVER
        )
    {
        multiplier *= LOW_SIGNAL_UNALIGNED_RECEIVER_PENALTY;
    }

    if relation.relation_kind != RelationKind::Conformance
        && owner_alignment == 0.0
        && is_declaration_like_candidate(candidate)
    {
        multiplier *= LOW_SIGNAL_DECLARATION_PENALTY;
    }

    multiplier
}

fn relation_candidate_edge_compatible(
    relation: &StoredRelation,
    candidate: &ScoredRelationCandidate,
) -> bool {
    match relation.edge_identity_kind.as_str() {
        EDGE_IDENTITY_MACRO_LIKE => candidate.candidate.block_type == "macro",
        EDGE_IDENTITY_CONSTRUCTOR_LIKE => matches!(
            candidate.candidate.block_type.as_str(),
            "constructor" | "class" | "struct" | "enum"
        ),
        _ => true,
    }
}

fn is_wrapper_like_candidate(candidate: &CandidateRow) -> bool {
    let complexity = candidate.complexity.unwrap_or_default();
    let call_count = candidate
        .called_symbols
        .as_ref()
        .map(|symbols| symbols.len())
        .unwrap_or_default();

    matches!(
        candidate.role,
        Some(SegmentRole::Implementation | SegmentRole::Orchestration)
    ) && candidate.line_count() <= 4
        && complexity <= 1
        && call_count <= 1
}

fn is_declaration_like_candidate(candidate: &CandidateRow) -> bool {
    matches!(candidate.role, Some(SegmentRole::Definition))
        && matches!(
            candidate.block_type.as_str(),
            "struct" | "enum" | "trait" | "type" | "class" | "interface" | "module"
        )
}

fn qualifier_components(qualifier_fingerprint: &str) -> Vec<String> {
    clean_owner_components(&split_symbol_components(qualifier_fingerprint))
}

fn path_components(path: &str) -> Vec<String> {
    clean_owner_components(&split_symbol_components(path))
}

fn breadcrumb_components(breadcrumb: &str) -> Vec<String> {
    clean_owner_components(&split_symbol_components(breadcrumb))
}

fn symbol_lookup_tail(symbol: &str) -> Option<String> {
    split_symbol_components(symbol).into_iter().last()
}

fn conformance_source_lookup_symbols(candidate: &CandidateRow) -> Vec<String> {
    let mut lookups = Vec::new();
    let mut seen = HashSet::new();

    for symbol in candidate.defined_symbols.as_deref().unwrap_or(&[]) {
        let trimmed = symbol
            .split(['<', '(', '['])
            .next()
            .unwrap_or(symbol)
            .trim();
        for lookup in [
            normalize_symbolish(trimmed),
            symbol_lookup_tail(trimmed).unwrap_or_default(),
        ] {
            if !lookup.is_empty() && seen.insert(lookup.clone()) {
                lookups.push(lookup);
            }
        }
    }

    lookups
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
    path_in_dir(&lower, "tests")
        || path_in_dir(&lower, "test")
        || path_in_dir(&lower, "spec")
        || path_in_dir(&lower, "__tests__")
        || lower.ends_with("_test.rs")
        || lower.ends_with("_tests.rs")
        || lower.ends_with("_spec.rs")
        || lower.ends_with(".test.ts")
        || lower.ends_with(".spec.ts")
        || lower.ends_with(".test.js")
        || lower.ends_with(".spec.js")
}

fn is_test_context_candidate(candidate: &CandidateRow) -> bool {
    is_test_path(&candidate.file_path)
        || has_test_context_token(&candidate.file_path)
        || candidate.block_type.eq_ignore_ascii_case("test")
        || candidate
            .breadcrumb
            .as_deref()
            .is_some_and(has_test_context_token)
        || candidate
            .defined_symbols
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .any(|symbol| has_test_context_token(symbol))
}

fn has_test_context_token(value: &str) -> bool {
    value
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(|token| token.to_ascii_lowercase())
        .any(|token| matches!(token.as_str(), "test" | "tests" | "spec" | "specs"))
}

fn is_low_signal_path(file_path: &str) -> bool {
    let lower = file_path.to_ascii_lowercase();
    is_test_path(file_path)
        || path_in_dir(&lower, "evals")
        || path_in_dir(&lower, "benches")
        || path_in_dir(&lower, "examples")
        || path_in_dir(&lower, "vendor")
        || lower.contains("node_modules")
}

fn path_in_dir(lower_path: &str, dir: &str) -> bool {
    lower_path == dir
        || lower_path.starts_with(&format!("{dir}/"))
        || lower_path.contains(&format!("/{dir}/"))
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
    use crate::shared::types::{ParsedRelation, ParsedRelationKind};
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
            referenced_relations: "[]".to_string(),
            called_symbols: serde_json::to_string(fixture.called_symbols).unwrap(),
            called_relations: "[]".to_string(),
            file_hash: format!("hash-{}", fixture.file_path),
        }
    }

    fn make_segment_with_called_relations(
        fixture: SegmentFixture<'_>,
        called_relations: &[ParsedRelation],
    ) -> segments::SegmentInsert {
        let mut segment = make_segment(fixture);
        segment.called_relations = serde_json::to_string(called_relations).unwrap();
        segment
    }

    fn make_segment_with_referenced_relations(
        fixture: SegmentFixture<'_>,
        referenced_relations: &[ParsedRelation],
    ) -> segments::SegmentInsert {
        let mut segment = make_segment(fixture);
        segment.referenced_relations = serde_json::to_string(referenced_relations).unwrap();
        segment
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
                    id: "cfg-admin",
                    file_path: "src/admin/config.rs",
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
    async fn trait_anchor_surfaces_impl_relation_site_for_rust_conformance() {
        let (_db, conn) = setup().await;
        insert_segments(
            &conn,
            vec![
                make_segment(SegmentFixture {
                    id: "validator-trait",
                    file_path: "src/auth/validator.rs",
                    line_start: 1,
                    block_type: "trait",
                    role: "DEFINITION",
                    defined_symbols: &["Validator"],
                    referenced_symbols: &[],
                    called_symbols: &[],
                }),
                make_segment(SegmentFixture {
                    id: "config-struct",
                    file_path: "src/auth/config.rs",
                    line_start: 1,
                    block_type: "struct",
                    role: "DEFINITION",
                    defined_symbols: &["Config"],
                    referenced_symbols: &[],
                    called_symbols: &[],
                }),
                make_segment_with_referenced_relations(
                    SegmentFixture {
                        id: "config-impl",
                        file_path: "src/auth/config.rs",
                        line_start: 10,
                        block_type: "impl",
                        role: "IMPLEMENTATION",
                        defined_symbols: &["Config<T>"],
                        referenced_symbols: &["Validator"],
                        called_symbols: &[],
                    },
                    &[ParsedRelation {
                        symbol: "crate::auth::Validator".to_string(),
                        edge_identity_kind: EDGE_IDENTITY_QUALIFIED_PATH.to_string(),
                        kind: Some(ParsedRelationKind::Conformance),
                    }],
                ),
            ],
        )
        .await;

        let engine = ImpactHorizonEngine::new(&conn);
        let result = engine
            .explore(ImpactRequest {
                anchor: ImpactAnchor::Symbol {
                    name: "Validator".to_string(),
                },
                scope: None,
                depth: 2,
                limit: 10,
            })
            .await
            .unwrap();

        assert_eq!(result.status, ImpactStatus::Expanded);
        assert_eq!(result.results.len(), 1);
        assert_eq!(result.results[0].segment_id, "config-impl");
        assert_eq!(result.results[0].reasons[0].kind, "implemented_by");
        assert!(result
            .results
            .iter()
            .all(|candidate| candidate.segment_id != "config-struct"));
    }

    #[tokio::test]
    async fn symbol_anchor_prioritizes_inbound_conformance_within_budget() {
        let (_db, conn) = setup().await;
        let mut fixtures = vec![
            make_segment(SegmentFixture {
                id: "formatter-trait",
                file_path: "src/ui/formatter.rs",
                line_start: 1,
                block_type: "trait",
                role: "DEFINITION",
                defined_symbols: &["Formatter"],
                referenced_symbols: &[],
                called_symbols: &[],
            }),
            make_segment_with_referenced_relations(
                SegmentFixture {
                    id: "plain-formatter",
                    file_path: "src/ui/plain_formatter.rs",
                    line_start: 1,
                    block_type: "class",
                    role: "DEFINITION",
                    defined_symbols: &["PlainFormatter"],
                    referenced_symbols: &["Formatter"],
                    called_symbols: &[],
                },
                &[ParsedRelation {
                    symbol: "Formatter".to_string(),
                    edge_identity_kind: EDGE_IDENTITY_BARE_IDENTIFIER.to_string(),
                    kind: Some(ParsedRelationKind::Conformance),
                }],
            ),
        ];

        for idx in 0..MAX_INBOUND_RELATIONS_PER_HOP {
            fixtures.push(make_segment_with_referenced_relations(
                SegmentFixture {
                    id: Box::leak(format!("formatter-reference-{idx}").into_boxed_str()),
                    file_path: Box::leak(format!("src/ui/render_{idx}.rs").into_boxed_str()),
                    line_start: idx + 1,
                    block_type: "function",
                    role: "IMPLEMENTATION",
                    defined_symbols: &[],
                    referenced_symbols: &["Formatter"],
                    called_symbols: &[],
                },
                &[ParsedRelation {
                    symbol: "Formatter".to_string(),
                    edge_identity_kind: EDGE_IDENTITY_BARE_IDENTIFIER.to_string(),
                    kind: Some(ParsedRelationKind::Reference),
                }],
            ));
        }

        insert_segments(&conn, fixtures).await;

        let engine = ImpactHorizonEngine::new(&conn);
        let result = engine
            .explore(ImpactRequest {
                anchor: ImpactAnchor::Symbol {
                    name: "Formatter".to_string(),
                },
                scope: None,
                depth: 2,
                limit: 10,
            })
            .await
            .unwrap();

        assert_eq!(result.status, ImpactStatus::Expanded);
        assert_eq!(result.results[0].segment_id, "plain-formatter");
        assert_eq!(result.results[0].reasons[0].kind, "implemented_by");
        if let Some(contextual) = result.contextual_results {
            assert!(contextual
                .iter()
                .all(|candidate| candidate.segment_id != "plain-formatter"));
        }
    }

    #[tokio::test]
    async fn trait_anchor_prioritizes_multiple_implementors_over_same_file_helpers() {
        let (_db, conn) = setup().await;
        insert_segments(
            &conn,
            vec![
                make_segment(SegmentFixture {
                    id: "formatter-trait",
                    file_path: "src/ui/output.rs",
                    line_start: 1,
                    block_type: "trait",
                    role: "DEFINITION",
                    defined_symbols: &["Formatter"],
                    referenced_symbols: &[],
                    called_symbols: &[],
                }),
                make_segment_with_referenced_relations(
                    SegmentFixture {
                        id: "json-formatter",
                        file_path: "src/ui/output.rs",
                        line_start: 20,
                        block_type: "impl",
                        role: "IMPLEMENTATION",
                        defined_symbols: &["JsonFormatter"],
                        referenced_symbols: &["Formatter"],
                        called_symbols: &["to_json"],
                    },
                    &[ParsedRelation {
                        symbol: "Formatter".to_string(),
                        edge_identity_kind: EDGE_IDENTITY_BARE_IDENTIFIER.to_string(),
                        kind: Some(ParsedRelationKind::Conformance),
                    }],
                ),
                make_segment_with_referenced_relations(
                    SegmentFixture {
                        id: "human-formatter",
                        file_path: "src/ui/output.rs",
                        line_start: 60,
                        block_type: "impl",
                        role: "IMPLEMENTATION",
                        defined_symbols: &["HumanFormatter"],
                        referenced_symbols: &["Formatter"],
                        called_symbols: &["format_message"],
                    },
                    &[ParsedRelation {
                        symbol: "Formatter".to_string(),
                        edge_identity_kind: EDGE_IDENTITY_BARE_IDENTIFIER.to_string(),
                        kind: Some(ParsedRelationKind::Conformance),
                    }],
                ),
                make_segment_with_referenced_relations(
                    SegmentFixture {
                        id: "plain-formatter",
                        file_path: "src/ui/output.rs",
                        line_start: 100,
                        block_type: "impl",
                        role: "IMPLEMENTATION",
                        defined_symbols: &["PlainFormatter"],
                        referenced_symbols: &["Formatter"],
                        called_symbols: &["render_rows"],
                    },
                    &[ParsedRelation {
                        symbol: "Formatter".to_string(),
                        edge_identity_kind: EDGE_IDENTITY_BARE_IDENTIFIER.to_string(),
                        kind: Some(ParsedRelationKind::Conformance),
                    }],
                ),
                make_segment(SegmentFixture {
                    id: "formatter-for",
                    file_path: "src/ui/output.rs",
                    line_start: 140,
                    block_type: "function",
                    role: "IMPLEMENTATION",
                    defined_symbols: &["formatter_for"],
                    referenced_symbols: &[
                        "Formatter",
                        "JsonFormatter",
                        "HumanFormatter",
                        "PlainFormatter",
                    ],
                    called_symbols: &[],
                }),
                make_segment(SegmentFixture {
                    id: "to-json",
                    file_path: "src/ui/output.rs",
                    line_start: 160,
                    block_type: "function",
                    role: "IMPLEMENTATION",
                    defined_symbols: &["to_json"],
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
                    name: "Formatter".to_string(),
                },
                scope: None,
                depth: 2,
                limit: 10,
            })
            .await
            .unwrap();

        assert_eq!(result.status, ImpactStatus::Expanded);
        let result_ids = result
            .results
            .iter()
            .map(|candidate| candidate.segment_id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(result_ids.len(), 3);
        assert!(result_ids.contains(&"json-formatter"));
        assert!(result_ids.contains(&"human-formatter"));
        assert!(result_ids.contains(&"plain-formatter"));
        assert!(!result_ids.contains(&"formatter-for"));
        assert!(!result_ids.contains(&"to-json"));
        assert!(result.results.iter().all(|candidate| candidate
            .reasons
            .iter()
            .any(|reason| reason.kind == "implemented_by")));
    }

    #[tokio::test]
    async fn segment_anchor_retains_outbound_conformance_when_calls_fill_budget() {
        let (_db, conn) = setup().await;
        let mut fixtures = vec![make_segment(SegmentFixture {
            id: "formatter-trait",
            file_path: "src/ui/formatter.rs",
            line_start: 1,
            block_type: "trait",
            role: "DEFINITION",
            defined_symbols: &["Formatter"],
            referenced_symbols: &[],
            called_symbols: &[],
        })];

        for idx in 0..MAX_OUTBOUND_RELATIONS_PER_HOP {
            fixtures.push(make_segment(SegmentFixture {
                id: Box::leak(format!("format-helper-{idx}").into_boxed_str()),
                file_path: Box::leak(format!("src/ui/helpers_{idx}.rs").into_boxed_str()),
                line_start: idx + 1,
                block_type: "function",
                role: "IMPLEMENTATION",
                defined_symbols: &[Box::leak(format!("format_helper_{idx}").into_boxed_str())],
                referenced_symbols: &[],
                called_symbols: &[],
            }));
        }

        let mut plain_formatter = make_segment(SegmentFixture {
            id: "plain-formatter",
            file_path: "src/ui/plain_formatter.rs",
            line_start: 1,
            block_type: "class",
            role: "DEFINITION",
            defined_symbols: &["PlainFormatter"],
            referenced_symbols: &["Formatter"],
            called_symbols: &[
                "format_helper_0",
                "format_helper_1",
                "format_helper_2",
                "format_helper_3",
                "format_helper_4",
                "format_helper_5",
                "format_helper_6",
                "format_helper_7",
            ],
        });
        plain_formatter.called_relations = serde_json::to_string(&[
            ParsedRelation {
                symbol: "format_helper_0".to_string(),
                edge_identity_kind: EDGE_IDENTITY_BARE_IDENTIFIER.to_string(),
                kind: Some(ParsedRelationKind::Call),
            },
            ParsedRelation {
                symbol: "format_helper_1".to_string(),
                edge_identity_kind: EDGE_IDENTITY_BARE_IDENTIFIER.to_string(),
                kind: Some(ParsedRelationKind::Call),
            },
            ParsedRelation {
                symbol: "format_helper_2".to_string(),
                edge_identity_kind: EDGE_IDENTITY_BARE_IDENTIFIER.to_string(),
                kind: Some(ParsedRelationKind::Call),
            },
            ParsedRelation {
                symbol: "format_helper_3".to_string(),
                edge_identity_kind: EDGE_IDENTITY_BARE_IDENTIFIER.to_string(),
                kind: Some(ParsedRelationKind::Call),
            },
            ParsedRelation {
                symbol: "format_helper_4".to_string(),
                edge_identity_kind: EDGE_IDENTITY_BARE_IDENTIFIER.to_string(),
                kind: Some(ParsedRelationKind::Call),
            },
            ParsedRelation {
                symbol: "format_helper_5".to_string(),
                edge_identity_kind: EDGE_IDENTITY_BARE_IDENTIFIER.to_string(),
                kind: Some(ParsedRelationKind::Call),
            },
            ParsedRelation {
                symbol: "format_helper_6".to_string(),
                edge_identity_kind: EDGE_IDENTITY_BARE_IDENTIFIER.to_string(),
                kind: Some(ParsedRelationKind::Call),
            },
            ParsedRelation {
                symbol: "format_helper_7".to_string(),
                edge_identity_kind: EDGE_IDENTITY_BARE_IDENTIFIER.to_string(),
                kind: Some(ParsedRelationKind::Call),
            },
        ])
        .unwrap();
        plain_formatter.referenced_relations = serde_json::to_string(&[ParsedRelation {
            symbol: "Formatter".to_string(),
            edge_identity_kind: EDGE_IDENTITY_BARE_IDENTIFIER.to_string(),
            kind: Some(ParsedRelationKind::Conformance),
        }])
        .unwrap();
        fixtures.push(plain_formatter);

        insert_segments(&conn, fixtures).await;

        let engine = ImpactHorizonEngine::new(&conn);
        let result = engine
            .explore(ImpactRequest {
                anchor: ImpactAnchor::Segment {
                    id: "plain-formatter".to_string(),
                },
                scope: None,
                depth: 1,
                limit: 10,
            })
            .await
            .unwrap();

        assert_eq!(result.status, ImpactStatus::Expanded);
        assert!(result
            .results
            .iter()
            .any(|candidate| candidate.segment_id == "formatter-trait"));
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
    async fn segment_anchor_demotes_leaf_only_relation_but_keeps_contextual_guidance() {
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

        assert_eq!(result.status, ImpactStatus::Empty);
        assert!(result.results.is_empty());
        let contextual = result
            .contextual_results
            .expect("same-file, leaf-only, and test candidates should remain contextual");
        assert_eq!(contextual.len(), 3);

        let find_contextual = |id: &str| contextual.iter().find(|c| c.segment_id == id).unwrap();
        assert_eq!(find_contextual("parse-config").reasons[0].kind, "same_file");
        assert!(matches!(
            find_contextual("config-test").reasons[0].kind.as_str(),
            "test_for_file" | "test_for_symbol"
        ));
        assert_eq!(find_contextual("boot-app").reasons[0].kind, "called_by");
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
    async fn qualified_relation_target_prefers_matching_scope() {
        let (_db, conn) = setup().await;
        insert_segments(
            &conn,
            vec![
                make_segment(SegmentFixture {
                    id: "reload-auth",
                    file_path: "src/auth/runtime.rs",
                    line_start: 1,
                    block_type: "function",
                    role: "ORCHESTRATION",
                    defined_symbols: &["reload_auth"],
                    referenced_symbols: &[],
                    called_symbols: &["crate::auth::config::load_config"],
                }),
                make_segment(SegmentFixture {
                    id: "auth-load-config",
                    file_path: "src/auth/config.rs",
                    line_start: 10,
                    block_type: "function",
                    role: "DEFINITION",
                    defined_symbols: &["load_config"],
                    referenced_symbols: &[],
                    called_symbols: &[],
                }),
                make_segment(SegmentFixture {
                    id: "cache-load-config",
                    file_path: "src/cache/config.rs",
                    line_start: 10,
                    block_type: "function",
                    role: "DEFINITION",
                    defined_symbols: &["load_config"],
                    referenced_symbols: &[],
                    called_symbols: &[],
                }),
            ],
        )
        .await;

        let engine = ImpactHorizonEngine::new(&conn);
        let result = engine
            .explore(ImpactRequest {
                anchor: ImpactAnchor::Segment {
                    id: "reload-auth".to_string(),
                },
                scope: None,
                depth: 2,
                limit: 10,
            })
            .await
            .unwrap();

        assert_eq!(result.status, ImpactStatus::Expanded);
        assert_eq!(result.results.len(), 1);
        assert_eq!(result.results[0].segment_id, "auth-load-config");
        assert!(result
            .results
            .iter()
            .all(|candidate| candidate.segment_id != "cache-load-config"));
    }

    #[tokio::test]
    async fn owner_aligned_target_shortlist_happens_before_truncation() {
        let (_db, conn) = setup().await;
        insert_segments(
            &conn,
            vec![
                make_segment_with_called_relations(
                    SegmentFixture {
                        id: "boot-auth",
                        file_path: "src/bootstrap.rs",
                        line_start: 1,
                        block_type: "function",
                        role: "ORCHESTRATION",
                        defined_symbols: &["boot_auth"],
                        referenced_symbols: &[],
                        called_symbols: &["crate::auth::config::load_config"],
                    },
                    &[ParsedRelation {
                        symbol: "crate::auth::config::load_config".to_string(),
                        edge_identity_kind: EDGE_IDENTITY_QUALIFIED_PATH.to_string(),
                        kind: None,
                    }],
                ),
                make_segment(SegmentFixture {
                    id: "accounts-load-config",
                    file_path: "src/accounts/config.rs",
                    line_start: 10,
                    block_type: "function",
                    role: "DEFINITION",
                    defined_symbols: &["load_config"],
                    referenced_symbols: &[],
                    called_symbols: &[],
                }),
                make_segment(SegmentFixture {
                    id: "adapter-load-config",
                    file_path: "src/adapter/config.rs",
                    line_start: 10,
                    block_type: "function",
                    role: "DEFINITION",
                    defined_symbols: &["load_config"],
                    referenced_symbols: &[],
                    called_symbols: &[],
                }),
                make_segment(SegmentFixture {
                    id: "admin-load-config",
                    file_path: "src/admin/config.rs",
                    line_start: 10,
                    block_type: "function",
                    role: "DEFINITION",
                    defined_symbols: &["load_config"],
                    referenced_symbols: &[],
                    called_symbols: &[],
                }),
                make_segment(SegmentFixture {
                    id: "auth-load-config",
                    file_path: "src/auth/config.rs",
                    line_start: 10,
                    block_type: "function",
                    role: "DEFINITION",
                    defined_symbols: &["load_config"],
                    referenced_symbols: &[],
                    called_symbols: &[],
                }),
            ],
        )
        .await;

        let engine = ImpactHorizonEngine::new(&conn);
        let result = engine
            .explore(ImpactRequest {
                anchor: ImpactAnchor::Segment {
                    id: "boot-auth".to_string(),
                },
                scope: None,
                depth: 2,
                limit: 10,
            })
            .await
            .unwrap();

        assert_eq!(result.status, ImpactStatus::Expanded);
        assert_eq!(result.results.len(), 1);
        assert_eq!(result.results[0].segment_id, "auth-load-config");
    }

    #[tokio::test]
    async fn ambiguous_helper_relation_stays_out_of_primary_results() {
        let (_db, conn) = setup().await;
        insert_segments(
            &conn,
            vec![
                make_segment(SegmentFixture {
                    id: "boot-app",
                    file_path: "src/app.rs",
                    line_start: 1,
                    block_type: "function",
                    role: "ORCHESTRATION",
                    defined_symbols: &["boot_app"],
                    referenced_symbols: &[],
                    called_symbols: &["load_config"],
                }),
                make_segment(SegmentFixture {
                    id: "auth-load-config",
                    file_path: "src/auth/config.rs",
                    line_start: 10,
                    block_type: "function",
                    role: "DEFINITION",
                    defined_symbols: &["load_config"],
                    referenced_symbols: &[],
                    called_symbols: &[],
                }),
                make_segment(SegmentFixture {
                    id: "cache-load-config",
                    file_path: "src/cache/config.rs",
                    line_start: 10,
                    block_type: "function",
                    role: "DEFINITION",
                    defined_symbols: &["load_config"],
                    referenced_symbols: &[],
                    called_symbols: &[],
                }),
            ],
        )
        .await;

        let engine = ImpactHorizonEngine::new(&conn);
        let result = engine
            .explore(ImpactRequest {
                anchor: ImpactAnchor::Segment {
                    id: "boot-app".to_string(),
                },
                scope: None,
                depth: 2,
                limit: 10,
            })
            .await
            .unwrap();

        assert_eq!(result.status, ImpactStatus::Empty);
        assert!(result.results.is_empty());
        let contextual = result
            .contextual_results
            .expect("ambiguous helper matches should remain contextual");
        assert_eq!(contextual.len(), 1);
        assert_eq!(contextual[0].reasons[0].kind, "calls");
    }

    #[tokio::test]
    async fn symbol_anchor_promotes_repeated_reference_support_before_expanding() {
        let (_db, conn) = setup().await;
        insert_segments(
            &conn,
            vec![
                make_segment(SegmentFixture {
                    id: "search-result-struct",
                    file_path: "src/shared/types.rs",
                    line_start: 1,
                    block_type: "struct",
                    role: "DEFINITION",
                    defined_symbols: &["SearchResult"],
                    referenced_symbols: &[],
                    called_symbols: &[],
                }),
                make_segment(SegmentFixture {
                    id: "search-result-alias",
                    file_path: "src/daemon/types.rs",
                    line_start: 1,
                    block_type: "type",
                    role: "DEFINITION",
                    defined_symbols: &["SearchResult"],
                    referenced_symbols: &[],
                    called_symbols: &[],
                }),
                make_segment(SegmentFixture {
                    id: "try-daemon-search",
                    file_path: "src/cli/search.rs",
                    line_start: 10,
                    block_type: "function",
                    role: "IMPLEMENTATION",
                    defined_symbols: &["try_daemon_search"],
                    referenced_symbols: &["SearchResult"],
                    called_symbols: &[],
                }),
                make_segment(SegmentFixture {
                    id: "format-search-results",
                    file_path: "src/cli/output.rs",
                    line_start: 20,
                    block_type: "function",
                    role: "IMPLEMENTATION",
                    defined_symbols: &["format_search_results"],
                    referenced_symbols: &["SearchResult"],
                    called_symbols: &[],
                }),
                make_segment(SegmentFixture {
                    id: "exec",
                    file_path: "src/cli/search.rs",
                    line_start: 40,
                    block_type: "function",
                    role: "ORCHESTRATION",
                    defined_symbols: &["exec"],
                    referenced_symbols: &[],
                    called_symbols: &["try_daemon_search", "format_search_results"],
                }),
            ],
        )
        .await;

        let engine = ImpactHorizonEngine::new(&conn);
        let result = engine
            .explore(ImpactRequest {
                anchor: ImpactAnchor::Symbol {
                    name: "SearchResult".to_string(),
                },
                scope: None,
                depth: 2,
                limit: 10,
            })
            .await
            .unwrap();

        assert_eq!(result.status, ImpactStatus::Expanded);
        let result_ids = result
            .results
            .iter()
            .map(|candidate| candidate.segment_id.as_str())
            .collect::<Vec<_>>();
        assert!(result_ids.contains(&"try-daemon-search"));
        assert!(result_ids.contains(&"format-search-results"));
        assert!(result_ids.contains(&"exec"));
    }

    #[tokio::test]
    async fn symbol_anchor_prefers_high_signal_seeds_and_demotes_eval_paths() {
        let (_db, conn) = setup().await;
        insert_segments(
            &conn,
            vec![
                make_segment(SegmentFixture {
                    id: "search-result-struct",
                    file_path: "src/shared/types.rs",
                    line_start: 1,
                    block_type: "struct",
                    role: "DEFINITION",
                    defined_symbols: &["SearchResult"],
                    referenced_symbols: &[],
                    called_symbols: &[],
                }),
                make_segment(SegmentFixture {
                    id: "search-result-impl",
                    file_path: "src/shared/types.rs",
                    line_start: 20,
                    block_type: "impl",
                    role: "IMPLEMENTATION",
                    defined_symbols: &["SearchResult"],
                    referenced_symbols: &[],
                    called_symbols: &[],
                }),
                make_segment(SegmentFixture {
                    id: "search-result-bench-type",
                    file_path: "evals/suites/1up-search/search-bench.ts",
                    line_start: 1,
                    block_type: "interface",
                    role: "DEFINITION",
                    defined_symbols: &["SearchResult"],
                    referenced_symbols: &[],
                    called_symbols: &[],
                }),
                make_segment(SegmentFixture {
                    id: "try-daemon-search",
                    file_path: "src/cli/search.rs",
                    line_start: 10,
                    block_type: "function",
                    role: "IMPLEMENTATION",
                    defined_symbols: &["try_daemon_search"],
                    referenced_symbols: &["SearchResult"],
                    called_symbols: &[],
                }),
                make_segment(SegmentFixture {
                    id: "format-search-results",
                    file_path: "src/cli/output.rs",
                    line_start: 20,
                    block_type: "function",
                    role: "IMPLEMENTATION",
                    defined_symbols: &["format_search_results"],
                    referenced_symbols: &["SearchResult"],
                    called_symbols: &[],
                }),
                make_segment(SegmentFixture {
                    id: "run-oneup-search",
                    file_path: "evals/suites/1up-search/search-bench.ts",
                    line_start: 30,
                    block_type: "function",
                    role: "IMPLEMENTATION",
                    defined_symbols: &["runOneupSearch"],
                    referenced_symbols: &["SearchResult"],
                    called_symbols: &[],
                }),
                make_segment(SegmentFixture {
                    id: "exec",
                    file_path: "src/cli/search.rs",
                    line_start: 40,
                    block_type: "function",
                    role: "ORCHESTRATION",
                    defined_symbols: &["exec"],
                    referenced_symbols: &[],
                    called_symbols: &["try_daemon_search", "format_search_results"],
                }),
            ],
        )
        .await;

        let engine = ImpactHorizonEngine::new(&conn);
        let result = engine
            .explore(ImpactRequest {
                anchor: ImpactAnchor::Symbol {
                    name: "SearchResult".to_string(),
                },
                scope: None,
                depth: 2,
                limit: 10,
            })
            .await
            .unwrap();

        assert_eq!(result.status, ImpactStatus::Expanded);
        assert_eq!(
            result
                .resolved_anchor
                .as_ref()
                .expect("symbol anchor should resolve")
                .matched_files,
            vec!["src/shared/types.rs".to_string()]
        );
        let result_ids = result
            .results
            .iter()
            .map(|candidate| candidate.segment_id.as_str())
            .collect::<Vec<_>>();
        assert!(result_ids.contains(&"try-daemon-search"));
        assert!(result_ids.contains(&"format-search-results"));
        assert!(result_ids.contains(&"exec"));
        assert!(!result_ids.contains(&"run-oneup-search"));
        let contextual = result
            .contextual_results
            .expect("low-signal eval consumers should remain contextual");
        assert!(contextual
            .iter()
            .any(|candidate| candidate.segment_id == "run-oneup-search"));
    }

    #[tokio::test]
    async fn macro_like_relations_do_not_resolve_to_function_candidates() {
        let (_db, conn) = setup().await;
        insert_segments(
            &conn,
            vec![
                make_segment_with_called_relations(
                    SegmentFixture {
                        id: "search-result",
                        file_path: "src/shared/types.rs",
                        line_start: 1,
                        block_type: "function",
                        role: "IMPLEMENTATION",
                        defined_symbols: &["SearchResult"],
                        referenced_symbols: &[],
                        called_symbols: &["matches"],
                    },
                    &[ParsedRelation {
                        symbol: "matches".to_string(),
                        edge_identity_kind: EDGE_IDENTITY_MACRO_LIKE.to_string(),
                        kind: None,
                    }],
                ),
                make_segment(SegmentFixture {
                    id: "expected-leaf-matches",
                    file_path: "src/shared/fs.rs",
                    line_start: 10,
                    block_type: "function",
                    role: "DEFINITION",
                    defined_symbols: &["matches"],
                    referenced_symbols: &[],
                    called_symbols: &[],
                }),
            ],
        )
        .await;

        let engine = ImpactHorizonEngine::new(&conn);
        let result = engine
            .explore(ImpactRequest {
                anchor: ImpactAnchor::Segment {
                    id: "search-result".to_string(),
                },
                scope: None,
                depth: 2,
                limit: 10,
            })
            .await
            .unwrap();

        assert!(result
            .results
            .iter()
            .all(|candidate| candidate.segment_id != "expected-leaf-matches"));
        assert!(result
            .contextual_results
            .unwrap_or_default()
            .iter()
            .all(|candidate| candidate.segment_id != "expected-leaf-matches"));
    }

    #[tokio::test]
    async fn qualified_inbound_relation_prefers_matching_seed() {
        let (_db, conn) = setup().await;
        insert_segments(
            &conn,
            vec![
                make_segment(SegmentFixture {
                    id: "auth-load-config",
                    file_path: "src/auth/config.rs",
                    line_start: 10,
                    block_type: "function",
                    role: "DEFINITION",
                    defined_symbols: &["load_config"],
                    referenced_symbols: &[],
                    called_symbols: &[],
                }),
                make_segment(SegmentFixture {
                    id: "cache-load-config",
                    file_path: "src/cache/config.rs",
                    line_start: 10,
                    block_type: "function",
                    role: "DEFINITION",
                    defined_symbols: &["load_config"],
                    referenced_symbols: &[],
                    called_symbols: &[],
                }),
                make_segment(SegmentFixture {
                    id: "auth-runtime",
                    file_path: "src/auth/runtime.rs",
                    line_start: 1,
                    block_type: "function",
                    role: "ORCHESTRATION",
                    defined_symbols: &["auth_runtime"],
                    referenced_symbols: &[],
                    called_symbols: &["crate::auth::config::load_config"],
                }),
                make_segment(SegmentFixture {
                    id: "cache-runtime",
                    file_path: "src/cache/runtime.rs",
                    line_start: 1,
                    block_type: "function",
                    role: "ORCHESTRATION",
                    defined_symbols: &["cache_runtime"],
                    referenced_symbols: &[],
                    called_symbols: &["crate::cache::config::load_config"],
                }),
            ],
        )
        .await;

        let engine = ImpactHorizonEngine::new(&conn);
        let result = engine
            .explore(ImpactRequest {
                anchor: ImpactAnchor::Segment {
                    id: "auth-load-config".to_string(),
                },
                scope: None,
                depth: 2,
                limit: 10,
            })
            .await
            .unwrap();

        assert_eq!(result.status, ImpactStatus::Expanded);
        assert_eq!(result.results.len(), 1);
        assert_eq!(result.results[0].segment_id, "auth-runtime");
        assert!(result
            .results
            .iter()
            .all(|candidate| candidate.segment_id != "cache-runtime"));
    }

    #[tokio::test]
    async fn low_signal_wrapper_yields_to_stronger_primary_candidate() {
        let (_db, conn) = setup().await;
        insert_segments(
            &conn,
            vec![
                make_segment(SegmentFixture {
                    id: "warm-cache-key",
                    file_path: "src/cache/runtime.rs",
                    line_start: 1,
                    block_type: "function",
                    role: "DEFINITION",
                    defined_symbols: &["warm_cache_key"],
                    referenced_symbols: &[],
                    called_symbols: &[],
                }),
                segments::SegmentInsert {
                    id: "prime-cache".to_string(),
                    file_path: "src/cache/priming.rs".to_string(),
                    language: "rust".to_string(),
                    block_type: "function".to_string(),
                    content: "pub fn prime_cache() -> &'static str {\n    warm_cache_key()\n}\n"
                        .to_string(),
                    line_start: 1,
                    line_end: 3,
                    embedding_vec: None,
                    breadcrumb: Some("cache".to_string()),
                    complexity: 1,
                    role: "ORCHESTRATION".to_string(),
                    defined_symbols: "[\"prime_cache\"]".to_string(),
                    referenced_symbols: "[]".to_string(),
                    referenced_relations: "[]".to_string(),
                    called_symbols: "[\"warm_cache_key\"]".to_string(),
                    called_relations: "[]".to_string(),
                    file_hash: "hash-src/cache/priming.rs".to_string(),
                },
                segments::SegmentInsert {
                    id: "warm-cache-for-request".to_string(),
                    file_path: "src/cache/worker.rs".to_string(),
                    language: "rust".to_string(),
                    block_type: "function".to_string(),
                    content: "pub fn warm_cache_for_request(user_key: &str) -> String {\n    let normalized = user_key.trim().to_lowercase();\n    if normalized.is_empty() {\n        return warm_cache_key().to_string();\n    }\n    format!(\"{}:{}\", warm_cache_key(), normalized)\n}\n"
                        .to_string(),
                    line_start: 1,
                    line_end: 7,
                    embedding_vec: None,
                    breadcrumb: Some("cache".to_string()),
                    complexity: 3,
                    role: "ORCHESTRATION".to_string(),
                    defined_symbols: "[\"warm_cache_for_request\"]".to_string(),
                    referenced_symbols: "[\"user_key\"]".to_string(),
                    referenced_relations: "[]".to_string(),
                    called_symbols: "[\"warm_cache_key\",\"normalize_cache_key\"]".to_string(),
                    called_relations: "[]".to_string(),
                    file_hash: "hash-src/cache/worker.rs".to_string(),
                },
            ],
        )
        .await;

        let engine = ImpactHorizonEngine::new(&conn);
        let result = engine
            .explore(ImpactRequest {
                anchor: ImpactAnchor::Symbol {
                    name: "warm_cache_key".to_string(),
                },
                scope: None,
                depth: 2,
                limit: 10,
            })
            .await
            .unwrap();

        assert_eq!(result.status, ImpactStatus::Expanded);
        assert_eq!(result.results.len(), 2);
        assert_eq!(result.results[0].segment_id, "warm-cache-for-request");
        assert_eq!(result.results[1].segment_id, "prime-cache");
    }

    #[tokio::test]
    async fn inline_test_context_stays_contextual_for_symbol_anchor() {
        let (_db, conn) = setup().await;
        let mut inline_test = make_segment_with_called_relations(
            SegmentFixture {
                id: "warm-cache-inline-test",
                file_path: "src/cache/coverage.rs",
                line_start: 20,
                block_type: "function",
                role: "IMPLEMENTATION",
                defined_symbols: &["inline_warm_cache_test"],
                referenced_symbols: &[],
                called_symbols: &["warm_cache_key"],
            },
            &[ParsedRelation {
                symbol: "warm_cache_key".to_string(),
                edge_identity_kind: EDGE_IDENTITY_BARE_IDENTIFIER.to_string(),
                kind: Some(ParsedRelationKind::Call),
            }],
        );
        inline_test.breadcrumb = Some("cache::tests".to_string());

        insert_segments(
            &conn,
            vec![
                make_segment(SegmentFixture {
                    id: "warm-cache-key",
                    file_path: "src/cache/runtime.rs",
                    line_start: 1,
                    block_type: "function",
                    role: "DEFINITION",
                    defined_symbols: &["warm_cache_key"],
                    referenced_symbols: &[],
                    called_symbols: &[],
                }),
                segments::SegmentInsert {
                    id: "warm-cache-for-request".to_string(),
                    file_path: "src/cache/worker.rs".to_string(),
                    language: "rust".to_string(),
                    block_type: "function".to_string(),
                    content: "pub fn warm_cache_for_request(user_key: &str) -> String {\n    let normalized = user_key.trim().to_lowercase();\n    if normalized.is_empty() {\n        return warm_cache_key().to_string();\n    }\n    format!(\"{}:{}\", warm_cache_key(), normalized)\n}\n"
                        .to_string(),
                    line_start: 1,
                    line_end: 7,
                    embedding_vec: None,
                    breadcrumb: Some("cache".to_string()),
                    complexity: 3,
                    role: "ORCHESTRATION".to_string(),
                    defined_symbols: "[\"warm_cache_for_request\"]".to_string(),
                    referenced_symbols: "[\"user_key\"]".to_string(),
                    referenced_relations: "[]".to_string(),
                    called_symbols: "[\"warm_cache_key\",\"normalize_cache_key\"]".to_string(),
                    called_relations: "[]".to_string(),
                    file_hash: "hash-src/cache/worker.rs".to_string(),
                },
                inline_test,
            ],
        )
        .await;

        let engine = ImpactHorizonEngine::new(&conn);
        let result = engine
            .explore(ImpactRequest {
                anchor: ImpactAnchor::Symbol {
                    name: "warm_cache_key".to_string(),
                },
                scope: None,
                depth: 2,
                limit: 10,
            })
            .await
            .unwrap();

        assert_eq!(result.status, ImpactStatus::Expanded);
        assert_eq!(result.results[0].segment_id, "warm-cache-for-request");
        assert!(result
            .results
            .iter()
            .all(|candidate| candidate.segment_id != "warm-cache-inline-test"));
        let contextual = result
            .contextual_results
            .expect("inline test consumers should remain contextual");
        assert!(contextual
            .iter()
            .any(|candidate| candidate.segment_id == "warm-cache-inline-test"));
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
