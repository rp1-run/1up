# 1up — Modules

## Top-Level Modules

| Module | Purpose | Key Files |
|---|---|---|
| `src/cli` | Command surface, output formatting, and machine-readable follow-up UX. Core commands (`search`, `get`, `symbol`, `impact`, `context`, `structural`) render through the lean line grammar in `src/cli/lean.rs`; maintenance commands keep the three-formatter trio in `src/cli/output.rs`. | `src/cli/mod.rs`, `src/cli/impact.rs`, `src/cli/lean.rs`, `src/cli/output.rs` |
| `src/search` | Hybrid retrieval, symbol/context/structural search, and bounded advisory impact expansion with owner-aware corroboration and primary/contextual trust bucketing. | `src/search/hybrid.rs`, `src/search/impact.rs`, `src/search/mod.rs` |
| `src/indexer` | Incremental scan/parse/embed pipeline that assigns deterministic segment IDs and parser-derived relation edge identities. Scanner enriches files with filesystem metadata; pipeline compares against the `indexed_files` manifest to skip metadata-unchanged files before content reads and tracks `input_prep_ms` for end-to-end timing. | `src/indexer/pipeline.rs`, `src/indexer/parser.rs`, `src/indexer/embedder.rs`, `src/indexer/scanner.rs` |
| `src/storage` | libSQL schema (v12: `FLOAT8(384)` `segment_vectors.embedding_vec` with `compress_neighbors=float8` on the HNSW index, written via the typed `vector8(?)` constructor), segment storage, symbol/relation tables, `indexed_files` manifest, tuned connection initialization (`connect_tuned`), and transactional maintenance with chunked multi-value INSERTs. | `src/storage/schema.rs`, `src/storage/segments.rs`, `src/storage/relations.rs`, `src/storage/queries.rs`, `src/storage/db.rs` |
| `src/daemon` | Background search/watch service with secure IPC, version-aware responses, and per-project scope fallback tracking (`pending_fallback_reason` on `ProjectRunState`). | `src/daemon/search_service.rs`, `src/daemon/worker.rs`, `src/daemon/registry.rs` |
| `src/shared` | Shared types, constants, config, errors, reminder/update helpers, and cross-layer contracts including `SetupTimings`, `IndexScopeInfo`, and `IndexPrefilterInfo` telemetry structs. | `src/shared/types.rs`, `src/shared/constants.rs`, `src/shared/update.rs` |
| `tests` | Black-box CLI and integration coverage. | `tests/integration_tests.rs`, `tests/cli_tests.rs` |
| `benches` | Criterion non-regression and latency guardrails, including impact outcome coverage. | `benches/search_bench.rs` |
| `scripts` | Trust/performance gate automation plus release and security helpers, including pinned-baseline rollout approval. Parallel indexing benchmark covers full, incremental, write-heavy, and daemon refresh scenarios with scope evidence and per-run telemetry. | `scripts/evaluate_impact_trust.sh`, `scripts/benchmark_impact.sh`, `scripts/approve_impact_rollout.sh`, `scripts/lib/impact_fixture.sh`, `scripts/benchmark_parallel_indexing.sh` |
| `evals` | Search-quality evaluation suites and support scripts. | includes `evals/suites/1up-search/search-bench.ts` |

## Key Components

| Component | File | Responsibility | Depends On |
|---|---|---|---|
| `Cli` | `src/cli/mod.rs` | Top-level clap dispatch and default output-format resolution. | `src/cli/impact.rs`, `src/shared/types.rs` |
| `ImpactArgs` | `src/cli/impact.rs` | Exact-anchor CLI for bounded likely-impact exploration. | `src/search/impact.rs`, `src/storage/db.rs`, `src/storage/schema.rs`, `src/shared/config.rs` |
| `Formatter` | `src/cli/output.rs` | Shared rendering for maintenance commands only (`start`, `stop`, `status`, `init`, `index`, `reindex`, `update`): human, plain, and json variants with additive timing/scope/prefilter telemetry in status output. Core-command rendering moved to `LeanRenderer`. | `src/shared/types.rs`, `src/shared/update.rs` |
| `LeanRenderer` | `src/cli/lean.rs` | Single-shape line renderer for the six core agent-facing commands (`search`, `get`, `symbol`, `impact`, `context`, `structural`). Implements the `<score>  <path>:<l1>-<l2>  <kind>  <breadcrumb>::<symbol>  :<segment_id>[  ~<channel>]` grammar with primary/contextual `~P`/`~C` bucketing for impact rows; free functions, no trait dispatch. | `src/search/impact.rs`, `src/shared/types.rs`, `src/storage/segments.rs` |
| `HybridSearchEngine` | `src/search/hybrid.rs` | Candidate-first hybrid search with additive `segment_id` hydration. | `src/search/*`, `src/indexer/embedder.rs`, `src/storage/segments.rs` |
| `ImpactHorizonEngine` | `src/search/impact.rs` | Bounded probable-impact expansion that resolves lookup-symbol relation targets, derives definition-side owner fingerprints, shortlists owner-aligned candidates before truncation, requires corroborating structural signals for primary promotion, and preserves primary/contextual bucketing plus explicit empty outcomes. | `src/storage/relations.rs`, `src/storage/segments.rs`, `src/search/symbol.rs` |
| `Pipeline` | `src/indexer/pipeline.rs` | Convert repository files into deterministic segments, parser-derived relation edge identities, and symbol metadata. Accepts `SetupTimings` from callers for end-to-end timing; compares filesystem metadata against the `indexed_files` manifest to skip unchanged files and tracks `input_prep_ms` and prefilter counters. | `src/indexer/parser.rs`, `src/indexer/embedder.rs`, `src/storage/segments.rs`, `src/indexer/scanner.rs` |
| `Schema` | `src/storage/schema.rs` | Schema init, validation, rebuild, and compatibility gating (v11 with `indexed_files` table). | `src/storage/queries.rs`, `src/shared/constants.rs` |
| `Relations` | `src/storage/relations.rs` | Persist and query relation descriptors with canonical, lookup-tail, qualifier, and edge-identity evidence for outbound and inbound expansion. | `src/storage/queries.rs`, `src/shared/symbols.rs` |
| `Segments` | `src/storage/segments.rs` | Transactional segment replacement, symbol/relation synchronization, and `indexed_files` manifest upsert/delete through chunked multi-value INSERTs (SQLITE_MAX_PARAMS=999). | `src/storage/queries.rs`, `src/storage/relations.rs`, `src/shared/types.rs` |
| `SearchService` | `src/daemon/search_service.rs` | Secure daemon-backed search IPC with optional version metadata. | `src/daemon/ipc.rs`, `src/shared/constants.rs`, `src/shared/types.rs` |
| `ImpactEvidenceScripts` | `scripts/evaluate_impact_trust.sh`, `scripts/benchmark_impact.sh`, `scripts/approve_impact_rollout.sh` | Produce baseline-versus-candidate trust and latency summaries, then approve rollout only when both gates pass against the pinned April 14, 2026 requirements baseline and field notes contain no unresolved blockers. | `scripts/lib/impact_fixture.sh`, `justfile`, candidate/baseline binaries |
| `SharedTypes` | `src/shared/types.rs` | Cross-layer result, config, progress, daemon status, and telemetry contracts including `SetupTimings`, `IndexScopeInfo`, `IndexPrefilterInfo`, and additive `IndexStageTimings` fields (`db_prepare_ms`, `model_prepare_ms`, `input_prep_ms`). | `src/shared/constants.rs`, `serde`, `chrono` |
| `IntegrationTests` | `tests/integration_tests.rs` | Black-box regression coverage for CLI, impact, and search stability. | binary + real local fixtures |
| `SearchBench` | `benches/search_bench.rs` | Criterion latency guardrail suite for discovery plus owner-aligned, low-signal, expanded, refused, empty, and empty-scoped impact paths. | `criterion`, search + storage engines |

## Internal Dependency Chains

- `src/cli` -> `src/search`: CLI dispatches discovery and local impact engines.
- `src/cli` -> `src/storage`: `impact` opens the project-local DB and validates schema before reads.
- `src/search` -> `src/storage`: search and impact hydrate segments and resolve symbol/relation rows at query time.
- `src/search` -> `src/indexer`: hybrid search reuses embedding infrastructure for query embeddings.
- `src/indexer` -> `src/storage`: indexer writes `SegmentInsert` batches, symbols, and relation rows through storage.
- `src/daemon` -> `src/search`: daemon workers execute hybrid search and serialize daemon-backed responses.
- `tests` -> `src/cli`: integration tests execute the binary as a black-box CLI surface.
- `benches` -> `src/search`: benchmarks instantiate real search and impact engines directly.

## Public Boundaries

### CLI Boundary

- Commands: `init`, `start`, `stop`, `status`, `search`, `symbol`, `context`, `impact`, `structural`, `index`, `reindex`, `hello-agent`, `update`
- Format contract: `plain`, `human`, `json`
- Rule: `impact` requires exactly one anchor and runs against the project-local index

### Search Boundary

- Engines: `HybridSearchEngine`, `SymbolSearchEngine`, `StructuralSearchEngine`, `ImpactHorizonEngine`
- Result contracts: `SearchResult`, `SymbolResult`, `ContextResult`, `StructuralResult`, `ImpactResultEnvelope`
- Rule: `search` stays discovery-oriented; `impact` returns advisory `expanded`, `expanded_scoped`, `empty`, `empty_scoped`, or `refused` envelopes where only corroborated, non-ambiguous, owner-consistent relation matches become primary `results` and additive `contextual_results` carries demoted relation, same-file, and test guidance

### Storage Boundary

- Components: `Db`, `schema`, `segments`, `relations`, `queries`
- Rule: schema v11 adds `indexed_files` manifest table and extends `segment_relations` with `lookup_canonical_symbol`, `qualifier_fingerprint`, and `edge_identity_kind`; replace/delete flows must keep segments, symbols, relation descriptors, and `indexed_files` rows aligned transactionally through chunked multi-value INSERTs

### Daemon Boundary

- IPC: `SearchRequest`, `SearchResponse`, `request_search`
- Rule: daemon search is Unix-domain and same-UID only; `impact` is intentionally not exposed through daemon IPC in v1

## Cross-Module Patterns

| Pattern | Modules | Why It Matters |
|---|---|---|
| Candidate-first hybrid retrieval | Search, Storage | Keeps discovery selective and performant before hydration. |
| Local impact read path | CLI, Search, Storage | Adds likely-impact exploration without perturbing daemon behavior. |
| Additive search handoff | CLI, Search, Shared, Tests, Benches | Lets agents move from discovery to bounded follow-up without reconstructing anchors. |
| Deterministic segment anchors | Indexer, Search, Storage, CLI | Stable segment IDs underpin the `search -> impact` contract. |
| Transactional relation maintenance | Indexer, Storage, Search | Prevents stale descriptor rows from distorting lookup-symbol relation resolution. |
| Manifest-backed file prefilter | Indexer, Storage | Skips metadata-unchanged files before content reads; content hash remains the correctness backstop. |
| End-to-end timing propagation | CLI, Daemon, Indexer, Shared | `SetupTimings` flows from callers through the pipeline so `total_ms` reflects user-perceived wall-clock time. |
| Batched transactional writes | Indexer, Storage | Chunked multi-value INSERTs reduce SQL chatter for segments, symbols, relations, vectors, and manifest rows. |
| Ambiguity-aware refusal envelopes | Search, CLI, Tests | Preserves trust and keeps advisory semantics explicit. |
| Trust bucket separation | Search, CLI, Shared | Keeps confident relation-backed likely impact distinct from demoted low-signal or heuristic-only contextual guidance and explicit empty outcomes. |
| Latency guardrails | Search, Tests, Benches, Scripts | Ensures impact stays additive without regressing core discovery paths or rollout-gate measurements. |
| Version-aware daemon search | Daemon, CLI, Shared | Supports mismatch warnings and safer upgrades without breaking transport. |

## Feature-Learning Deltas From Impact Horizon

- `src/cli` gained a first-class `impact` command surface and formatter support for impact envelopes.
- `src/search` now resolves relation targets through lookup-symbol fetches plus definition-side owner-fingerprint derivation, owner-aware shortlisting, corroboration scoring, ambiguity margins, and low-signal demotion inside `src/search/impact.rs`.
- `src/storage` now persists relation descriptors in schema v10, including `lookup_canonical_symbol`, `qualifier_fingerprint`, and `edge_identity_kind`, and exposes lookup-target relation queries.
- `src/cli` and `src/cli/output.rs` keep the existing impact envelope stable while surfacing primary-versus-contextual trust buckets for richer relation scoring.
- `tests`, `benches`, and `scripts` now encode qualified-relation, ambiguous-helper, and stronger-candidate-wins regressions, with `impact-rollout-approve` binding both summaries to the pinned April 14, 2026 baseline and current HEAD while honoring unresolved field-note blockers.

## Feature-Learning Deltas From Faster Indexing

- `src/storage/db.rs` gained `connect_tuned` and `apply_project_pragmas` for write-optimized project-local connections.
- `src/storage` now manages the `indexed_files` manifest table (schema v11) and uses chunked multi-value INSERTs (SQLITE_MAX_PARAMS=999) for segments, symbols, relations, vectors, and manifest rows.
- `src/indexer/scanner.rs` enriches `ScannedFile` with filesystem metadata (size, mtime) for prefilter comparisons.
- `src/indexer/pipeline.rs` compares filesystem metadata against the manifest, tracks `input_prep_ms`, and accepts `SetupTimings` plus `daemon_fallback_reason` for end-to-end timing and scope visibility.
- `src/daemon/worker.rs` tracks `pending_fallback_reason` on `ProjectRunState` and passes it through to the pipeline for scope promotion visibility.
- `src/shared/types.rs` added `SetupTimings`, `IndexScopeInfo`, `IndexPrefilterInfo`, and additive `IndexStageTimings` fields.
- `src/cli/output.rs` renders timing, scope, and prefilter fields additively across all three formatters.
- `scripts/benchmark_parallel_indexing.sh` expanded to benchmark daemon refresh and report scope evidence in summary JSON.
