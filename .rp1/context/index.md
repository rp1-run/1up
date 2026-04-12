# 1up — Knowledge Base Index

**What**: A semantic code search engine that indexes source repositories using tree-sitter AST parsing and ONNX embeddings, providing hybrid vector+FTS search, symbol lookup, structural queries, and context retrieval through a CLI with a background daemon for incremental re-indexing, warm search, and built-in self-update.

**Why**: Gives developers and AI agents fast, ranked code exploration by meaning and structure — not just text matching — with automatic background indexing that keeps results fresh.

## Quick Reference

| Attribute | Value |
|-----------|-------|
| Entry point | `src/main.rs` -> `src/cli/mod.rs` (clap dispatch) |
| Key pattern | Layered + Two-Process Model (CLI + daemon), Candidate-First Search Hydration, Channel-Aware Self-Update |
| Tech stack | Rust, Tokio, libSQL (FTS5 + vector), ONNX Runtime (all-MiniLM-L6-v2), tree-sitter (16 grammars), clap |
| License | Apache-2.0 |
| Distribution | Homebrew, Scoop, GitHub Releases (4 targets), built-in self-update |

## KB File Manifest

| File | Lines | Load For |
|------|-------|----------|
| [concept_map.md](concept_map.md) | ~224 | Understanding domain terminology, type definitions, cross-cutting concerns |
| [architecture.md](architecture.md) | ~304 | System topology, data flows, integration points, deployment |
| [interaction-model.md](interaction-model.md) | ~151 | CLI surfaces, user-visible states, feedback loops, output format semantics |
| [modules.md](modules.md) | ~213 | Component breakdown, file counts, dependencies, metrics |
| [patterns.md](patterns.md) | ~101 | Coding conventions, error handling, testing idioms, I/O patterns |

## Task-Based Loading

| Task | Load Files |
|------|-----------|
| Code review | `patterns.md` |
| Bug investigation | `architecture.md`, `modules.md` |
| Feature work | `modules.md`, `patterns.md` |
| Search/ranking changes | `concept_map.md`, `architecture.md` |
| Daemon/IPC work | `architecture.md`, `modules.md` |
| CLI/UX changes | `interaction-model.md`, `patterns.md` |
| Update system work | `architecture.md`, `modules.md`, `interaction-model.md` |
| Strategic / system-wide | All files |

## How to Load

Agents load this KB automatically via CLAUDE.md instructions:
1. Read `index.md` first (this file)
2. Load additional files based on task type per table above
3. Never load all files unless performing holistic analysis

## Project Structure

```
src/
├── main.rs              # Binary entry point + passive update notification
├── lib.rs               # Library root
├── reminder.md          # Agent instruction source
├── cli/                 # CLI layer (15 files) — clap subcommands + output formatting + update command
├── daemon/              # Daemon layer (10 files) — file watching, IPC, search service, lifecycle, heartbeat
├── indexer/             # Indexer layer (6 files) — pipeline, parser, embedder, scanner, chunker
├── search/              # Search layer (9 files) — hybrid, symbol, structural, context, ranking
├── shared/              # Shared layer (10 files) — types, config, errors, fs, constants, reminder, update
└── storage/             # Storage layer (5 files) — db, schema, segments, queries
tests/                   # Integration + release tests (6 files)
benches/                 # Criterion benchmarks (1 file)
scripts/                 # Benchmark + release scripts (15 files)
evals/                   # Search quality evals (8 files)
packaging/               # Homebrew + Scoop templates
.github/workflows/       # CI/CD + release automation (6 workflows)
```

## Navigation

- **Concepts & terminology** -> [concept_map.md](concept_map.md)
- **System architecture & data flows** -> [architecture.md](architecture.md)
- **CLI interaction model** -> [interaction-model.md](interaction-model.md)
- **Module & component details** -> [modules.md](modules.md)
- **Implementation patterns** -> [patterns.md](patterns.md)
