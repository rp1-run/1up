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
| `Segment Vector` | Entity | Quantized embedding row in `segment_vectors.embedding_vec` as `FLOAT8(384)` under schema v12. Written via the typed `vector8(?)` constructor; generic `vector(?)` does not auto-cast. | `src/storage/queries.rs`, `src/storage/segments.rs`, `src/storage/schema.rs` |
| `Vector Index` | Entity | libSQL `libsql_vector_idx` on `segment_vectors.embedding_vec` with `metric=cosine`, `compress_neighbors=float8`, `max_neighbors=32`. Delivers ~4x on-disk shrink vs FLOAT32 while staying in a sub-80 MiB DiskANN page tier at repo scale. | `src/storage/queries.rs`, `src/storage/schema.rs` |
| `Schema Version` | Value object | Monotonic integer (currently 12) stamped in `meta`. `ensure_current` rejects older or newer layouts with an explicit `1up reindex` hint. v12 gates the FLOAT32 -> FLOAT8 migration. | `src/shared/constants.rs`, `src/storage/schema.rs` |
| `Recall Eval Harness` | Process | Deterministic anchor-based recall@k measurement run via `just eval-recall`. Decision gate for quantization and prefilter tuning; requires cold state (daemon stopped, index wiped) to avoid contamination. | `evals/suites/1up-search/recall.ts`, `evals/suites/1up-search/recall-corpus.jsonl`, `evals/suites/1up-search/recall-baseline.json` |
| `Anchor-based Gold Corpus` | Value object | Recall gold keyed by durable anchors (`{file, symbol}` or `{file, line_contains}`), not segment IDs or a specific index's top-k output. Survives line drift and avoids version-bias. | `evals/suites/1up-search/recall-corpus.jsonl` |

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
| `schema v12` | Current schema version. Migrates `segment_vectors.embedding_vec` from `FLOAT32(384)` to `FLOAT8(384)` with `compress_neighbors=float8` and `max_neighbors=32`. Version bump forces reindex; no in-place migration. |
| `advisory impact` | Likely-impact guidance derived from relations and heuristics, not exact dependency truth. |
| `FLOAT8(384)` | libSQL int8-quantized 384-dim vector column type used for `segment_vectors.embedding_vec` in schema v12. ~4x smaller than FLOAT32 with no recall impact under an anchor-based corpus. |
| `vector8(?)` | libSQL typed vector constructor required to insert/query `FLOAT8` columns. Generic `vector(?)` raises `InputValidationError: vector type differs from column type`. |
| `libsql_vector_idx` | libSQL 0.9.30 vector index — a DiskANN graph (not classic HNSW) accepting `type`, `metric`, `compress_neighbors`, `max_neighbors`, `alpha`, `search_l`, `insert_l`. |
| `compress_neighbors` | `libsql_vector_idx` option selecting neighbor-list element quantization (`float1bit`, `float8`, `float16`, `float32`). |
| `max_neighbors` | DiskANN fanout cap (32 in v12). Graph size quantizes by discrete page tier, not linearly — observed boundaries at max_n=38 (~71–80 MiB) and 40 (~81 MiB). |
| `VECTOR_PREFILTER_K` | Vector-search prefilter candidate count. Raised 200 -> 400 in v12 to absorb FLOAT8's noisier top-k ranking with no measurable latency hit. Correct lever for FLOAT8 recall, not RRF weights. |
| `gold corpus` | Hand-curated recall ground truth. Must be version-neutral (never seeded from a specific index's top-k) and keyed by durable anchors, not segment IDs. |
| `cold-state measurement` | Required eval protocol: stop the watch daemon, wipe `.1up/index.db`, reindex cold, then run the harness to avoid daemon-induced segment drift (~3 pt bias otherwise). |

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
- Each `Segment` has at most one `Segment Vector` row; `segment_vectors.embedding_vec` is indexed by `Vector Index`.
- `Schema Version` gates the `Vector Index` DDL — v12 mismatch forces reindex rather than in-place migration.
- `Vector Index` pairs `FLOAT8` quantization with widened `VECTOR_PREFILTER_K=400` so the RRF reranker recovers displaced gold neighbors.
- `Recall Eval Harness` consumes `Anchor-based Gold Corpus` and gates schema bumps against REQ-002's 2 pt envelope.

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
| Force-reindex schema evolution | Storage schema | Breaking column changes (e.g. FLOAT32 -> FLOAT8) bump `SCHEMA_VERSION` and rely on `ensure_current` to reject stale layouts. No in-place migration code is written. |
| Quantization with prefilter widening | Vector search | When element-type quantization degrades top-k ranking, widen `VECTOR_PREFILTER_K` rather than reweighting RRF. RRF recovers displaced gold neighbors. |
| Typed vector constructor discipline | libSQL write/read | Every INSERT/SELECT on a `FLOAT8` column uses `vector8(?)`. Generic `vector()` does not auto-cast. Enforced uniformly across query constants and batch-insert format strings. |
| Cold-state eval protocol | Recall measurement | Stop daemon, wipe index, reindex cold, then run harness. Prevents daemon-induced transient segment IDs from biasing recall. |
| Anchor-based gold curation | Eval corpus design | Gold entries key by `{file, symbol}` or `{file, line_contains}` — durable across line shifts and version-neutral. |
| Hypothesis-validated schema change | Storage evolution | Non-obvious libSQL behaviors (e.g. `vector()` auto-cast, DiskANN page-tier quantization) are confirmed via HYP experiments before DDL lands. |

## Bounded Contexts

### Search Discovery

- Owns ranked discovery, daemon-backed search IPC, and additive machine follow-up handles.
- Does not compute impact expansion or claim dependency truth.

### Impact Analysis

- Owns local `1up impact` request handling, refusal semantics, owner-aware ranking, corroboration gating, and next-step hints.
- Does not use daemon IPC in v1.

### Index Graph Storage

- Owns persisted segments, symbol rows, unresolved relation rows, and quantized vector rows in the local libSQL index (schema v12: FLOAT8(384) embeddings + compressed DiskANN graph).
- Query-time code resolves targets rather than storing an exact dependency graph.
- Schema changes evolve via force-reindex rather than in-place migration.

### Eval And Benchmarks

- Owns recall@k and on-disk size measurement under `evals/` and `scripts/`.
- Requires cold-state measurement (daemon stopped, index wiped) for deterministic numbers.
- Gates production schema bumps against REQ-001 (size) and REQ-002 (recall) absolutes.

## Cross-Cutting Concerns

- Ambiguity management: broad symbol anchors are refused with narrowing hints instead of widened into noisy output.
- Advisory semantics: scores are framed as likely/probable guidance, not guaranteed blast radius.
- Empty-state trust: resolved anchors without relation-backed evidence stay empty instead of being echoed back as synthetic success results.
- Interactive latency: budgets and benchmarks keep impact and vector changes additive without regressing core discovery commands; FLOAT8 + `VECTOR_PREFILTER_K=400` verified latency-neutral at repo scale.
- Compatibility: search and IPC surfaces stay additive and optional while relation-row and vector-column schema changes remain local to the index and guarded by reindex checks.
- Reindex safety: schema mismatches fail early with explicit `1up reindex` guidance. Force-reindex is the only migration path for breaking column changes.
- Measurement integrity: eval runs require cold state and version-neutral anchor-based gold to avoid daemon contamination and version-bias regressions.
- Typed libSQL surfaces: `FLOAT8` columns require `vector8(?)` at every write/read site; generic `vector()` does not auto-cast.
