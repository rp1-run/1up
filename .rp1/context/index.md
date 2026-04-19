# 1up — Knowledge Base Index

**What**: A semantic code search engine that indexes local repositories with tree-sitter parsing and ONNX embeddings, then serves hybrid search, symbol lookup, structural queries, context retrieval, and bounded impact exploration through a CLI with an optional background daemon.

**Why**: It gives developers and agents fast code discovery by meaning and structure while keeping indexing local, incremental, and stable enough for interactive workflows.

## Quick Reference

| Attribute | Value |
|---|---|
| Entry point | `src/main.rs` -> `src/cli/mod.rs` |
| Key patterns | Layered CLI + daemon model, candidate-first retrieval, local-only impact analysis, quantized vector storage with widened prefilter |
| Tech stack | Rust, Tokio, libSQL, ONNX Runtime, tree-sitter, clap |
| Distribution | Homebrew, Scoop, GitHub Releases, built-in self-update |
| Schema version | 12 (FLOAT8(384) embeddings, compress_neighbors=float8, max_neighbors=32) |

## KB File Manifest

| File | Lines | Load For |
|---|---:|---|
| [concept_map.md](concept_map.md) | ~125 | Terminology, types, domain relationships, vector/schema vocabulary, impact outcomes |
| [architecture.md](architecture.md) | ~147 | Topology, data flow, storage, daemon boundaries, shrunk-vector-index changes |
| [interaction-model.md](interaction-model.md) | ~131 | CLI states, output contracts, follow-up flows, developer harness surface |
| [modules.md](modules.md) | ~106 | Component ownership, dependencies, evals and scripts modules |
| [patterns.md](patterns.md) | ~91 | Coding, storage, error, output, eval, and test idioms |

## Task-Based Loading

| Task | Load Files |
|---|---|
| Code review | `patterns.md` |
| Bug investigation | `architecture.md`, `modules.md` |
| Feature work | `modules.md`, `patterns.md` |
| Search and ranking changes | `concept_map.md`, `architecture.md`, `interaction-model.md` |
| Impact or relation work | `concept_map.md`, `architecture.md`, `modules.md`, `interaction-model.md`, `patterns.md` |
| Vector/storage/schema changes | `concept_map.md`, `architecture.md`, `modules.md`, `patterns.md` |
| Strategic or system-wide analysis | All files |

## Recent Learnings

- `impact` now separates confident relation-backed likely-impact `results` from heuristic-only or demoted-relation `contextual_results`, and empty expansions return explicit `empty` or `empty_scoped` states instead of anchor-echo fallbacks.
- Search results expose additive machine-readable `segment_id` handles for exact impact follow-up without changing discovery ranking.
- Schema v9 persists relation lookup-target and qualifier-fingerprint evidence in `segment_relations`, enabling bounded structural-confidence scoring without changing the impact envelope.
- Rollout evidence now has dedicated entry points: `just impact-eval` for trust gating and `just impact-bench` for latency gating.
- Schema v11 adds `indexed_files` manifest table for metadata-based unchanged-file prefiltering; indexing path uses tuned connections, batched multi-value INSERTs, and end-to-end timing propagation via `SetupTimings`.
- `IndexProgress` exposes additive `scope` and `prefilter` fields; daemon tracks scope fallback reasons via `pending_fallback_reason`.
- Benchmark script expanded with daemon refresh and scope evidence in summary JSON.
- Schema v12 migrates `segment_vectors.embedding_vec` from `FLOAT32(384)` to `FLOAT8(384)` with `compress_neighbors=float8` + `max_neighbors=32` on the HNSW index. Measured on the 1up repo: `index.db` 281 MB -> ~71 MB (~4x), cold indexing 81 s -> ~36 s, recall 0.00 pt delta under an anchor-based corpus. Writes use the typed `vector8(?)` constructor (generic `vector(?)` rejected at `FLOAT8` columns). `VECTOR_PREFILTER_K` widened 200 -> 400 to absorb quantization noise without latency impact.
- Force-reindex schema evolution: breaking column changes bump `SCHEMA_VERSION` and rely on `ensure_current` to fail closed. No in-place migration.
- Anchor-based recall gold: recall corpora key gold by `{file, symbol}` / `{file, line_contains}` anchors — durable across line drift, version-neutral. Hash-based gold is fragile (any line shift invalidates segment IDs) and was replaced.
- libSQL 0.9.30's `libsql_vector_idx` is DiskANN, not classic HNSW; graph size quantizes by page tier, not linearly in `max_neighbors`. `max_neighbors=32` sits below the 80 MiB page boundary.
- Cold-state eval protocol: daemon must be stopped and `.1up/index.db` wiped before measuring recall/size to avoid ~3 pt bias from transient segment IDs.
- Developer harness: `just eval-recall` and `just bench-vector-index-size` gate storage-format changes without expanding the shipped CLI contract.

## How To Load

Agents load this KB automatically:
1. Read `index.md` first.
2. Load only the files needed for the current task.
3. Avoid loading the full KB unless the work is system-wide.
