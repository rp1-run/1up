# 1up — Modules

## Top-Level Modules

| Module | Purpose | Key Files |
|---|---|---|
| `src/cli` | Command surface, output formatting, and machine-readable follow-up UX. | `src/cli/mod.rs`, `src/cli/impact.rs`, `src/cli/output.rs` |
| `src/search` | Hybrid retrieval, symbol/context/structural search, and bounded advisory impact expansion with primary/contextual trust bucketing. | `src/search/hybrid.rs`, `src/search/impact.rs`, `src/search/mod.rs` |
| `src/indexer` | Incremental scan/parse/embed pipeline that assigns deterministic segment IDs. | `src/indexer/pipeline.rs`, `src/indexer/parser.rs`, `src/indexer/embedder.rs` |
| `src/storage` | libSQL schema, segment storage, symbol/relation tables, and transactional maintenance. | `src/storage/schema.rs`, `src/storage/segments.rs`, `src/storage/relations.rs`, `src/storage/queries.rs` |
| `src/daemon` | Background search/watch service with secure IPC and version-aware responses. | `src/daemon/search_service.rs`, `src/daemon/worker.rs`, `src/daemon/registry.rs` |
| `src/shared` | Shared types, constants, config, errors, reminder/update helpers, and cross-layer contracts. | `src/shared/types.rs`, `src/shared/constants.rs`, `src/shared/update.rs` |
| `tests` | Black-box CLI and integration coverage. | `tests/integration_tests.rs`, `tests/cli_tests.rs` |
| `benches` | Criterion non-regression and latency guardrails, including impact outcome coverage. | `benches/search_bench.rs` |
| `scripts` | Trust/performance gate automation plus release and security helpers, including pinned-baseline rollout approval. | `scripts/evaluate_impact_trust.sh`, `scripts/benchmark_impact.sh`, `scripts/approve_impact_rollout.sh`, `scripts/lib/impact_fixture.sh` |
| `evals` | Search-quality evaluation suites and support scripts. | includes `evals/suites/1up-search/search-bench.ts` |

## Key Components

| Component | File | Responsibility | Depends On |
|---|---|---|---|
| `Cli` | `src/cli/mod.rs` | Top-level clap dispatch and default output-format resolution. | `src/cli/impact.rs`, `src/shared/types.rs` |
| `ImpactArgs` | `src/cli/impact.rs` | Exact-anchor CLI for bounded likely-impact exploration. | `src/search/impact.rs`, `src/storage/db.rs`, `src/storage/schema.rs`, `src/shared/config.rs` |
| `Formatter` | `src/cli/output.rs` | Shared rendering for search, status, update, and impact results, including primary/contextual separation and explicit empty states. | `src/search/impact.rs`, `src/shared/types.rs`, `src/shared/update.rs` |
| `HybridSearchEngine` | `src/search/hybrid.rs` | Candidate-first hybrid search with additive `segment_id` hydration. | `src/search/*`, `src/indexer/embedder.rs`, `src/storage/segments.rs` |
| `ImpactHorizonEngine` | `src/search/impact.rs` | Bounded probable-impact expansion that resolves lookup-symbol relation targets, scores qualifier/path/breadcrumb/scope/role evidence, demotes ambiguous or low-signal matches, and preserves primary/contextual bucketing plus explicit empty outcomes. | `src/storage/relations.rs`, `src/storage/segments.rs`, `src/search/symbol.rs` |
| `Pipeline` | `src/indexer/pipeline.rs` | Convert repository files into deterministic segments and symbol metadata. | `src/indexer/parser.rs`, `src/indexer/embedder.rs`, `src/storage/segments.rs` |
| `Schema` | `src/storage/schema.rs` | Schema init, validation, rebuild, and compatibility gating. | `src/storage/queries.rs`, `src/shared/constants.rs` |
| `Relations` | `src/storage/relations.rs` | Persist and query relation descriptors with canonical, lookup-tail, and qualifier evidence for outbound and inbound expansion. | `src/storage/queries.rs`, `src/shared/symbols.rs` |
| `Segments` | `src/storage/segments.rs` | Transactional segment replacement and symbol/relation synchronization. | `src/storage/queries.rs`, `src/storage/relations.rs`, `src/shared/types.rs` |
| `SearchService` | `src/daemon/search_service.rs` | Secure daemon-backed search IPC with optional version metadata. | `src/daemon/ipc.rs`, `src/shared/constants.rs`, `src/shared/types.rs` |
| `ImpactEvidenceScripts` | `scripts/evaluate_impact_trust.sh`, `scripts/benchmark_impact.sh`, `scripts/approve_impact_rollout.sh` | Produce baseline-versus-candidate trust and latency summaries, then approve rollout only when both gates pass against the pinned April 14, 2026 requirements baseline and field notes contain no unresolved blockers. | `scripts/lib/impact_fixture.sh`, `justfile`, candidate/baseline binaries |
| `SharedTypes` | `src/shared/types.rs` | Cross-layer result, config, progress, and daemon status contracts. | `src/shared/constants.rs`, `serde`, `chrono` |
| `IntegrationTests` | `tests/integration_tests.rs` | Black-box regression coverage for CLI, impact, and search stability. | binary + real local fixtures |
| `SearchBench` | `benches/search_bench.rs` | Criterion latency guardrail suite for discovery plus expanded, refused, empty, and empty-scoped impact paths. | `criterion`, search + storage engines |

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
- Rule: `search` stays discovery-oriented; `impact` returns advisory `expanded`, `expanded_scoped`, `empty`, `empty_scoped`, or `refused` envelopes where only confident non-ambiguous relation matches become primary `results` and additive `contextual_results` carries demoted relation, same-file, and test guidance

### Storage Boundary

- Components: `Db`, `schema`, `segments`, `relations`, `queries`
- Rule: schema v9 extends `segment_relations` with `lookup_canonical_symbol` and `qualifier_fingerprint`; replace/delete flows must keep segments, symbols, and relation descriptors aligned transactionally

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
| Ambiguity-aware refusal envelopes | Search, CLI, Tests | Preserves trust and keeps advisory semantics explicit. |
| Trust bucket separation | Search, CLI, Shared | Keeps confident relation-backed likely impact distinct from demoted low-signal or heuristic-only contextual guidance and explicit empty outcomes. |
| Latency guardrails | Search, Tests, Benches, Scripts | Ensures impact stays additive without regressing core discovery paths or rollout-gate measurements. |
| Version-aware daemon search | Daemon, CLI, Shared | Supports mismatch warnings and safer upgrades without breaking transport. |

## Feature-Learning Deltas From Impact Horizon

- `src/cli` gained a first-class `impact` command surface and formatter support for impact envelopes.
- `src/search` now resolves relation targets through lookup-symbol fetches plus qualifier/path/breadcrumb/scope/role scoring, ambiguity margins, and low-signal demotion inside `src/search/impact.rs`.
- `src/storage` now persists relation descriptors in schema v9, including `lookup_canonical_symbol` and `qualifier_fingerprint`, and exposes lookup-target relation queries.
- `src/cli` and `src/cli/output.rs` keep the existing impact envelope stable while surfacing primary-versus-contextual trust buckets for richer relation scoring.
- `tests`, `benches`, and `scripts` now encode qualified-relation, ambiguous-helper, and stronger-candidate-wins regressions, with `impact-rollout-approve` binding both summaries to the pinned April 14, 2026 baseline and current HEAD while honoring unresolved field-note blockers.
