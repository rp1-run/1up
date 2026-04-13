# 1up — Concept Map

## Core Concepts

| Concept | Type | Meaning | Primary Evidence |
|---|---|---|---|
| `Segment` | Entity | Fundamental indexed code block. A stable `segment_id` now also acts as an exact cross-command anchor. | `src/shared/types.rs`, `src/storage/segments.rs` |
| `SearchResult` | Entity | Ranked discovery result hydrated from indexed segments. It now carries an additive optional `segment_id` for machine follow-up. | `src/shared/types.rs`, `src/search/hybrid.rs`, `src/cli/output.rs` |
| `Impact Horizon` | Process | Explicit local-only workflow for bounded likely-impact exploration from a known anchor. | `src/search/impact.rs`, `src/cli/impact.rs` |
| `ImpactAnchor` | Value object | Exact-one-anchor request model: file, file:line, symbol, or segment. | `src/search/impact.rs`, `src/cli/impact.rs` |
| `ImpactCandidate` | Entity | Ranked likely-impact segment with hop distance, advisory score, and reason evidence. | `src/search/impact.rs`, `src/cli/output.rs` |
| `ImpactResultEnvelope` | Value object | Shared output envelope for `expanded`, `expanded_scoped`, and `refused` impact outcomes. | `src/search/impact.rs`, `src/cli/output.rs` |
| `SegmentRelation` | Entity | Persisted unresolved call/reference edge keyed by source segment and canonical target symbol. | `src/storage/relations.rs`, `src/storage/queries.rs`, `src/storage/schema.rs` |
| `Daemon Search IPC` | Interface | Unix-socket discovery protocol for daemon-backed search. Responses now optionally carry `daemon_version`. | `src/daemon/search_service.rs`, `src/cli/search.rs` |

## Terminology

| Term | Meaning |
|---|---|
| `impact` | The explicit CLI workflow for advisory likely-impact exploration. |
| `segment_id` | Stable machine-readable handle emitted by search and accepted by `1up impact --from-segment`. |
| `seed segment` | Resolved starting segment or bounded segment set used before impact expansion begins. |
| `scope` | Repo-relative subtree constraint that narrows anchor resolution and candidate retention. |
| `relation row` | Stored unresolved call/reference record in `segment_relations`. |
| `refusal envelope` | Structured impact response that explains why expansion was refused and what to do next. |
| `daemon_version` | Optional search-response metadata used to warn about CLI/daemon version skew. |
| `schema v8` | Index schema version that includes `segment_relations` and requires reindexing from older layouts. |
| `advisory impact` | Likely-impact guidance derived from relations and heuristics, not exact dependency truth. |

## Relationships

- `SearchResult` provides an exact follow-up handle to `ImpactAnchor` through optional `segment_id`.
- `Impact Horizon` traverses `SegmentRelation` rows plus same-file and test heuristics to build ranked candidates.
- `Impact Horizon` resolves symbol anchors and canonical relation targets through definition lookup at query time.
- `ImpactResultEnvelope` contains `ImpactCandidate` values on success and refusal metadata on rejected requests.
- `SegmentInsert` materializes `SegmentRelation` rows during storage writes.
- `Daemon Search IPC` returns `SearchResult` values but remains a separate surface from Impact Horizon.

## Recurrent Patterns

| Pattern | Context | Application |
|---|---|---|
| Additive search-to-impact handoff | Discovery workflow | Search keeps existing ranking semantics and only adds optional `segment_id` handles for exact follow-up. |
| Bounded advisory expansion | Impact Horizon | Depth, seed count, relation fan-out, and result budgets stay capped; broad symbols are refused with hints. |
| Late relation resolution | Local index graph | Indexing stores unresolved canonical symbol relations and impact resolves exact targets only for active seeds. |
| Transactional relation maintenance | Storage writes | Segment upsert, replace, and delete flows keep `segment_relations` aligned with `segments` and `segment_symbols`. |
| Backward-compatible surface evolution | CLI output and daemon IPC | New fields such as `segment_id` and `daemon_version` stay optional to preserve compatibility. |
| Latency guarding | Verification | Benchmarks and integration tests protect interactive latency and search-stability expectations. |

## Bounded Contexts

### Search Discovery

- Owns ranked discovery, daemon-backed search IPC, and additive machine follow-up handles.
- Does not compute impact expansion or claim dependency truth.

### Impact Analysis

- Owns local `1up impact` request handling, refusal semantics, ranking, and next-step hints.
- Does not use daemon IPC in v1.

### Index Graph Storage

- Owns persisted segments, symbol rows, and unresolved relation rows in the local libSQL index.
- Query-time code resolves targets rather than storing an exact dependency graph.

## Cross-Cutting Concerns

- Ambiguity management: broad symbol anchors are refused with narrowing hints instead of widened into noisy output.
- Advisory semantics: scores are framed as likely/probable guidance, not guaranteed blast radius.
- Interactive latency: budgets and benchmarks keep impact additive without regressing core discovery commands.
- Compatibility: new fields are additive and optional so existing search and IPC consumers continue to deserialize cleanly.
- Reindex safety: schema mismatches fail early with explicit `1up reindex` guidance.
