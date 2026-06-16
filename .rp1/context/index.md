# 1up - Knowledge Base Index

**What**: A local-first code discovery substrate that indexes repositories with tree-sitter parsing, ONNX embeddings, libSQL FTS/vector storage, and relation metadata, then exposes search, get, symbol, context, structural, impact, overview, indexing, daemon, and MCP workflows through a single Rust binary.

**Why**: It gives humans and agents a fast, local, evidence-oriented path from ranked code discovery to exact source hydration, symbol verification, and bounded likely-impact exploration without relying on broad raw search as the first step.

## Quick Reference

| Attribute | Value |
|---|---|
| Type | Single project |
| Entry point | `src/main.rs` -> `src/cli/mod.rs` |
| Primary agent surface | `1up mcp --path <repo>` exposing `oneup_status`, `oneup_start`, `oneup_overview`, `oneup_search`, `oneup_get`, `oneup_symbol`, `oneup_context`, `oneup_impact`, `oneup_structural` |
| Key patterns | Layered CLI + MCP + daemon, orientation-before-discovery, search-before-get/context, candidate-first retrieval, local-only advisory impact, metadata-prefiltered indexing, single-source-of-truth tool guidance |
| Tech stack | Rust, Tokio, libSQL, ONNX Runtime, tree-sitter, rmcp, clap, TypeScript evals, shell release scripts |
| Version | 0.1.11 |
| Schema version | 16 (`FLOAT8(384)`, `vector8(?)`, `compress_neighbors=float8`, `max_neighbors=32`, `VECTOR_PREFILTER_K=400`; v16 stores cleaned markdown heading breadcrumbs) |
| Last generated | 2026-06-14T23:12:20Z |

## KB File Manifest

| File | Lines | Load For |
|---|---:|---|
| [concept_map.md](concept_map.md) | 165 | Domain terminology, code-discovery concepts, MCP tool vocabulary, storage/search/impact relationships |
| [architecture.md](architecture.md) | 244 | System topology, data/state layout, MCP/CLI/daemon flows, release and indexing architecture |
| [interaction-model.md](interaction-model.md) | 115 | Agent and CLI interaction semantics, readiness states, output contracts, setup/onboarding flows |
| [modules.md](modules.md) | 214 | Component ownership, module dependencies, public boundaries, tests/evals/scripts organization |
| [patterns.md](patterns.md) | 91 | Coding conventions, data modeling, errors, validation, output, storage, concurrency, testing idioms |

## Task-Based Loading

| Task | Load Files |
|---|---|
| Code review | `patterns.md` |
| Bug investigation | `architecture.md`, `modules.md` |
| Feature work | `modules.md`, `patterns.md` |
| Search, ranking, or symbol changes | `concept_map.md`, `architecture.md`, `interaction-model.md`, `patterns.md` |
| MCP or CLI surface changes | `interaction-model.md`, `modules.md`, `patterns.md` |
| Impact or relation work | `concept_map.md`, `architecture.md`, `modules.md`, `interaction-model.md`, `patterns.md` |
| Indexing, storage, schema, vector, or daemon changes | `concept_map.md`, `architecture.md`, `modules.md`, `patterns.md` |
| Release, install, distribution, or eval changes | `architecture.md`, `modules.md`, `interaction-model.md` |
| Strategic or system-wide analysis | All files |

## Recent Learnings

- The retained MCP tool set is now **nine** tools: `oneup_overview` was added as a deterministic repository orientation digest (recommended first call on an unfamiliar repo). `RETAINED_PUBLIC_TOOLS` (`src/mcp/types.rs`) is the single source of truth; the legacy `oneup_prepare`/`oneup_read` names do not exist.
- Agent guidance is now single-sourced in `src/mcp` (server `instructions`, per-tool descriptions, and `next_actions`). README/docs no longer instruct pasting a hint into user instruction files; `AGENTS.md`/`CLAUDE.md`/`DEVELOPMENT.md` discovery blocks are one-line in-band pointers. 1up writes nothing to user instruction files by default.
- A new opt-in `1up doctor --clean-hints` command (visible top-level) cleans legacy pasted hints: read-only preview by default, mutation gated behind `--apply`, fence-only auto-remove of a 1up-owned `<!-- 1up:hint:begin/end -->` span, and detect-and-advise (never auto-edit) for unfenced stale tokens. Backed by the pure `src/cli/hint_cleanup.rs` classifier and `atomic_replace_within_project_root` (`src/shared/fs.rs`).
- Schema is now **v16** (was 13): `segment_vectors.embedding_vec` is `FLOAT8(384)`, vector writes/reads use `vector8(?)`, and incompatible formats fail closed with `1up reindex` guidance. v16 stores cleaned markdown heading-breadcrumb text.
- A documentation tool-name pin test (`tests/release_assets_tests.rs`) asserts every `oneup_*` token in repo docs is a `RETAINED_PUBLIC_TOOLS` member, and a committed-manifest test pins `update-manifest.json` version to `CARGO_PKG_VERSION` (hard-fail on drift). The benchmark schema gate derives its expected version from `constants.rs` rather than a hardcoded literal.
- MCP `instructions` are engineered to fit a ~2KB host truncation budget with the "use 1up before raw grep" routing rule front-loaded so it survives truncation.
- Search remains search -> get/context -> verify with candidate-first hybrid retrieval (vector + FTS + symbol via RRF); the vector stage uses an exact exhaustive scan for small corpora and an approximate index above `VECTOR_EXHAUSTIVE_SCAN_MAX_VECTORS`.
- Project resolution separates `state_root` from `source_root` via a `WorktreeContext` carrying `context_id`, so linked worktrees share one `.1up` index scoped logically per context.
- Impact remains local-only and advisory: primary likely-impact `results` stay separate from lower-confidence `contextual_results`, and `refused`/`empty` states carry narrowing guidance.

## Project Structure

```text
src/
  cli/       # Human CLI commands, lean core output, maintenance renderers, doctor cleanup, MCP launch/setup
  mcp/       # rmcp stdio server, 9 tool schemas, operation adapters, structured envelopes, RETAINED_PUBLIC_TOOLS
  search/    # Hybrid retrieval, ranking, intent, scope, symbol, context, structural, impact, overview engines
  indexer/   # Scan, parse/chunk, markdown doc-sections, embed, metadata prefilter, progress, storage pipeline
  storage/   # libSQL schema v16, SQL, segments, vectors, symbols, relations, manifest
  daemon/    # Registry, lifecycle, watcher, worker, secure search IPC, platform stubs
  shared/    # Types, config, project/worktree roots, secure FS, symbols, errors, update helpers
tests/       # CLI/MCP/release/setup/security regression suites
evals/       # Promptfoo/TypeScript search and MCP adoption evals
scripts/     # Benchmarks, installer, release, security, MCP smoke automation
```

## Navigation

- **[concept_map.md](concept_map.md)**: Terminology and conceptual relationships.
- **[architecture.md](architecture.md)**: System design, data flow, storage, deployment, and release topology.
- **[interaction-model.md](interaction-model.md)**: User/agent-visible states, output contracts, and setup/discovery loops.
- **[modules.md](modules.md)**: Module/component inventory and dependency boundaries.
- **[patterns.md](patterns.md)**: Implementation idioms and local engineering conventions.

## How To Load

Agents load this KB automatically:

1. Read `index.md` first.
2. Load only the files needed for the current task.
3. Avoid loading the full KB unless the work is system-wide.
