# 1up - Knowledge Base Index

**What**: A local-first code discovery substrate that indexes repositories with tree-sitter parsing, ONNX embeddings, libSQL FTS/vector storage, and relation metadata, then exposes search, read, symbol, context, structural, impact, indexing, daemon, and MCP workflows through a single Rust binary.

**Why**: It gives humans and agents a fast, local, evidence-oriented path from ranked code discovery to exact source hydration, symbol verification, and bounded likely-impact exploration without relying on broad raw search as the first step.

## Quick Reference

| Attribute | Value |
|---|---|
| Type | Single project |
| Entry point | `src/main.rs` -> `src/cli/mod.rs` |
| Primary agent surface | `1up mcp --path <repo>` exposing `oneup_prepare`, `oneup_search`, `oneup_read`, `oneup_symbol`, `oneup_impact` |
| Key patterns | Layered CLI + MCP + daemon, search-before-read, candidate-first retrieval, local-only advisory impact, metadata-prefiltered indexing |
| Tech stack | Rust, Tokio, libSQL, ONNX Runtime, tree-sitter, rmcp, clap, TypeScript evals, shell release scripts |
| Version | 0.1.8 |
| Schema version | 12 (`FLOAT8(384)`, `vector8(?)`, `compress_neighbors=float8`, `max_neighbors=32`, `VECTOR_PREFILTER_K=400`) |
| Last generated | 2026-05-01T00:45:02Z |

## KB File Manifest

| File | Lines | Load For |
|---|---:|---|
| [concept_map.md](concept_map.md) | 176 | Domain terminology, code-discovery concepts, MCP tool vocabulary, storage/search/impact relationships |
| [architecture.md](architecture.md) | 183 | System topology, data/state layout, MCP/CLI/daemon flows, release and indexing architecture |
| [interaction-model.md](interaction-model.md) | 169 | Agent and CLI interaction semantics, readiness states, output contracts, setup/onboarding flows |
| [modules.md](modules.md) | 158 | Component ownership, module dependencies, public boundaries, tests/evals/scripts organization |
| [patterns.md](patterns.md) | 92 | Coding conventions, data modeling, errors, validation, output, storage, concurrency, testing idioms |

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
| Release, install, packaging, or eval changes | `architecture.md`, `modules.md`, `interaction-model.md` |
| Strategic or system-wide analysis | All files |

## Recent Learnings

- MCP is now a first-class top-level module and the canonical agent surface, not an incidental CLI/daemon behavior. Agents should use canonical `oneup_*` MCP tools for discovery before broad raw search.
- The former reminder/skill/`hello-agent` adoption path is historical; current onboarding uses server identity `oneup`, `1up mcp --path <repo>`, repo instruction files, setup docs, host reload/trust prompts, and adoption evals.
- Search is explicitly search -> read -> verify: ranked discovery returns compact segment handles, `oneup_read`/`get` hydrates selected evidence, and `oneup_symbol` is the completeness-oriented path for known symbols.
- Core discovery CLI commands now use one lean row grammar and reject `--format`; maintenance commands keep human/plain/json formatting.
- MCP tools return structured `ToolEnvelope` responses with `status`, `summary`, `data`, and `next_actions`, making prepare/search/read/symbol/impact a guided loop.
- Project resolution separates `state_root` from `source_root`, allowing linked worktrees to reuse main-worktree `.1up` state while scanning the active source tree.
- Schema v12 remains current: `segment_vectors.embedding_vec` is `FLOAT8(384)`, vector writes and reads use `vector8(?)`, and incompatible storage formats fail closed with `1up reindex` guidance.
- Indexing is metadata-first and transactional: `indexed_files` skips unchanged size/mtime rows before content reads, scoped runs fall back to full when unsafe, and file replacement updates segments, vectors, symbols, relations, and manifest rows together.
- Impact remains local-only and advisory. Primary likely-impact `results` must stay separate from lower-confidence `contextual_results`, and `refused`/`empty` states carry narrowing guidance.
- Release architecture now validates MCP as a public contract: archive smoke checks list canonical tools, call `oneup_prepare`, verify structured content, and ensure stdout remains JSON-RPC clean.

## Project Structure

```text
src/
  cli/       # Human CLI commands, lean core output, maintenance renderers, MCP launch/setup
  mcp/       # rmcp stdio server, tool schemas, operation adapters, structured envelopes
  search/    # Hybrid retrieval, ranking, symbol, context, structural, impact engines
  indexer/   # Scan, parse/chunk, embed, metadata prefilter, progress, storage pipeline
  storage/   # libSQL schema v12, SQL, segments, vectors, symbols, relations, manifest
  daemon/    # Registry, lifecycle, watcher, worker, secure search IPC, platform stubs
  shared/    # Types, config, project roots, secure FS, symbols, errors, update helpers
tests/       # CLI/MCP/release/setup/security regression suites
evals/       # Promptfoo/TypeScript search and MCP adoption evals
scripts/     # Benchmarks, installer, release, security, MCP smoke automation
packaging/   # Homebrew and Scoop templates
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
