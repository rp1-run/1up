# 1up — Concept Map

## Core Concepts

| Concept | Type | Meaning | Primary Evidence |
|---|---|---|---|
| `Segment` | Entity | Fundamental indexed code block. A stable `segment_id` now also acts as an exact cross-command anchor. | `src/shared/types.rs`, `src/storage/segments.rs` |
| `SearchResult` | Entity | Ranked discovery result hydrated from indexed segments. It now carries an additive optional `segment_id` for machine follow-up. | `src/shared/types.rs`, `src/search/hybrid.rs`, `src/cli/output.rs` |
| `Impact Horizon` | Process | Explicit local-only workflow for bounded likely-impact exploration from a known anchor. | `src/search/impact.rs`, `src/cli/impact.rs` |
| `ImpactAnchor` | Value object | Exact-one-anchor request model: file, file:line, symbol, or segment. | `src/search/impact.rs`, `src/cli/impact.rs` |
| `ImpactCandidate` | Entity | Ranked likely-impact segment with hop distance, advisory score, and reason evidence after structural confidence and ambiguity gating. | `src/search/impact.rs`, `src/cli/output.rs` |
| `Contextual Guidance` | Contract concept | Lower-confidence impact support surfaced separately from primary likely impact, including same-file/test heuristics plus ambiguous or low-signal relation matches. | `src/search/impact.rs`, `src/cli/output.rs` |
| `ImpactResultEnvelope` | Value object | Shared output envelope for `expanded`, `expanded_scoped`, `empty`, `empty_scoped`, and `refused` outcomes, with additive optional `contextual_results`. | `src/search/impact.rs`, `src/cli/output.rs` |
| `SegmentRelation` | Entity | Persisted unresolved call/reference edge keyed by source segment plus canonical, lookup-tail, qualifier, and edge-identity evidence. | `src/storage/relations.rs`, `src/storage/queries.rs`, `src/storage/schema.rs` |
| `RelationTargetDescriptor` | Value object | Normalized lookup/disambiguation fields derived from a raw target symbol before relation rows are stored. | `src/storage/relations.rs` |
| `Daemon Search IPC` | Interface | Unix-socket discovery protocol for daemon-backed search. Responses now optionally carry `daemon_version`. | `src/daemon/search_service.rs`, `src/cli/search.rs` |

## Terminology

| Term | Meaning |
|---|---|
| `impact` | The explicit CLI workflow for advisory likely-impact exploration. |
| `segment_id` | Stable machine-readable handle emitted by search and accepted by `1up impact --from-segment`. |
| `seed segment` | Resolved starting segment or bounded segment set used before impact expansion begins. |
| `scope` | Repo-relative subtree constraint that narrows anchor resolution and candidate retention. |
| `relation row` | Stored unresolved call/reference record in `segment_relations`, including canonical, lookup-tail, qualifier-fingerprint, and `edge_identity_kind` fields. |
| `lookup_canonical_symbol` | Normalized tail symbol used to fetch bounded definition candidates for relation resolution and inbound lookup. |
| `qualifier_fingerprint` | Normalized non-tail qualifier tokens used to align relation targets with file paths and breadcrumbs. |
| `definition_owner_fingerprint` | Normalized owner tokens derived at impact-evaluation time from candidate file path, breadcrumb, and enclosing definition symbols. |
| `edge_identity_kind` | Normalized relation-form vocabulary such as `qualified_path`, `member_access`, `method_receiver`, `constructor_like`, `macro_like`, or `bare_identifier`. |
| `structural confidence` | Combined symbol, owner-alignment, edge-identity, path, breadcrumb, scope, role, and relation-kind evidence used before primary promotion. |
| `corroboration signal` | A positive structural signal such as owner alignment, exact identity, compatible edge identity, scope/path affinity, or implementation-like role. |
| `ambiguity margin` | Minimum confidence gap required for the best relation candidate to outrank the runner-up as primary. |
| `refusal envelope` | Structured impact response that explains why expansion was refused and what to do next. |
| `contextual_results` | Additive machine-readable field that holds non-primary same-file, test-only, or demoted relation guidance. |
| `empty` | Impact status meaning the anchor resolved, but no primary likely-impact candidates were found. |
| `empty_scoped` | Scoped variant of `empty`; no primary likely-impact candidates survived within the requested scope. |
| `daemon_version` | Optional search-response metadata used to warn about CLI/daemon version skew. |
| `context_only` | Hint code used when the anchor resolved but only contextual guidance remains after trust-bucket selection. |
| `schema v10` | Index schema version that requires relation lookup-target, qualifier-fingerprint, and edge-identity columns and reindexing from older layouts. |
| `advisory impact` | Likely-impact guidance derived from relations and heuristics, not exact dependency truth. |

## Relationships

- `SearchResult` provides an exact follow-up handle to `ImpactAnchor` through optional `segment_id`.
- `Impact Horizon` traverses `SegmentRelation` rows plus same-file and test heuristics to build primary likely-impact candidates and contextual guidance.
- `Impact Horizon` resolves relation targets through `lookup_canonical_symbol`, derives `definition_owner_fingerprint` from candidate metadata, then applies owner, edge-identity, path, scope, and role evidence before primary promotion.
- `RelationTargetDescriptor` is derived at write time and materialized into each `SegmentRelation` row for later query-time resolution.
- `ImpactResultEnvelope.results` contains only primary likely-impact candidates, while `contextual_results` carries lower-confidence guidance.
- `ImpactResultEnvelope` uses explicit empty statuses when the anchor resolves but no primary candidates survive confidence and ambiguity gating.
- `edge_identity_kind` and `qualifier_fingerprint` together decide whether a leaf-symbol match has enough corroboration to compete for primary promotion.
- `SegmentInsert` materializes `SegmentRelation` rows during storage writes.
- `Daemon Search IPC` returns `SearchResult` values but remains a separate surface from Impact Horizon.

## Recurrent Patterns

| Pattern | Context | Application |
|---|---|---|
| Additive search-to-impact handoff | Discovery workflow | Search keeps existing ranking semantics and only adds optional `segment_id` handles for exact follow-up. |
| Bounded advisory expansion | Impact Horizon | Depth, seed count, relation fan-out, and result budgets stay capped; broad symbols are refused with hints and relation resolution stays bounded per lookup symbol. |
| Owner-aware corroboration | Impact Horizon | Owner-aligned candidates are shortlisted before truncation, and primary promotion requires more than a leaf-name match when ambiguity exists. |
| Trust bucket separation | Impact Horizon, Output | Primary relation-backed impact stays distinct from demoted relation or heuristic-only contextual guidance and empty-state handling. |
| Late relation resolution | Local index graph | Indexing stores unresolved canonical plus lookup/qualifier/edge relation evidence, and impact resolves exact targets only for active seeds. |
| Transactional relation maintenance | Storage writes | Segment upsert, replace, and delete flows keep `segment_relations` aligned with `segments` and `segment_symbols`. |
| Backward-compatible surface evolution | CLI output and daemon IPC | New fields such as `segment_id` and `daemon_version` stay optional to preserve compatibility. |
| Latency guarding | Verification | Benchmarks and integration tests protect interactive latency and search-stability expectations. |

## Bounded Contexts

### Search Discovery

- Owns ranked discovery, daemon-backed search IPC, and additive machine follow-up handles.
- Does not compute impact expansion or claim dependency truth.

### Impact Analysis

- Owns local `1up impact` request handling, refusal semantics, owner-aware ranking, corroboration gating, and next-step hints.
- Does not use daemon IPC in v1.

### Index Graph Storage

- Owns persisted segments, symbol rows, and unresolved relation rows in the local libSQL index.
- Query-time code resolves targets rather than storing an exact dependency graph.

## Cross-Cutting Concerns

- Ambiguity management: broad symbol anchors are refused with narrowing hints instead of widened into noisy output.
- Advisory semantics: scores are framed as likely/probable guidance, not guaranteed blast radius.
- Empty-state trust: resolved anchors without relation-backed evidence stay empty instead of being echoed back as synthetic success results.
- Interactive latency: budgets and benchmarks keep impact additive without regressing core discovery commands.
- Compatibility: search and IPC surfaces stay additive and optional while relation-row schema changes remain local to the index and guarded by reindex checks.
- Reindex safety: schema mismatches fail early with explicit `1up reindex` guidance.
