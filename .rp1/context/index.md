# 1up — Knowledge Base Index

**1up** is a local code search and indexing CLI that combines semantic vector search (ONNX embeddings), FTS5 full-text search, symbol lookup, and structural AST queries over a libSQL database. A background daemon watches for file changes and incrementally re-indexes with configurable bounded parallelism.

## Quick Reference

| Aspect | Value |
|--------|-------|
| Entry point | `src/main.rs` -> `src/cli/mod.rs` |
| Key pattern | Staged single-writer pipeline with bounded parallel parse workers |
| Tech stack | Rust, libSQL, ONNX Runtime, tree-sitter (16 languages), tokio |
| Architecture | Layered + two-process model (CLI + daemon) |

## KB File Manifest

| File | Lines | Load For |
|------|-------|----------|
| [concept_map.md](concept_map.md) | ~120 | Domain terminology, concept relationships |
| [architecture.md](architecture.md) | ~156 | System layers, data flows, integration points |
| [interaction-model.md](interaction-model.md) | ~98 | CLI surfaces, feedback loops, output modes |
| [modules.md](modules.md) | ~166 | Component breakdown, dependencies, metrics |
| [patterns.md](patterns.md) | ~74 | Coding conventions, error handling, concurrency |

## Task-Based Loading

| Task | Load These Files |
|------|-----------------|
| Code review | `patterns.md` |
| Bug investigation | `architecture.md`, `modules.md` |
| Feature work | `modules.md`, `patterns.md` |
| CLI/UX changes | `interaction-model.md`, `modules.md` |
| Strategic / system-wide | All files |

## How to Load

1. Always read `index.md` first (this file)
2. Load additional files based on task type above
3. Only load all files for holistic analysis

## Project Structure

```
src/
├── main.rs              # Entry point
├── cli/                 # CLI commands and output formatting (12 files)
├── daemon/              # Background file watcher and registry (5 files)
├── indexer/             # Scan, parse, embed, pipeline (6 files)
├── search/              # Hybrid, symbol, structural, context search (9 files)
├── shared/              # Types, config, constants, errors (6 files)
└── storage/             # libSQL DB, schema, segments, queries (5 files)
tests/                   # Integration, CLI, SQL verification (3 files)
benches/                 # Search benchmarks (1 file)
scripts/                 # Benchmark tooling
```

## Navigation

- **Concepts & terminology** -> [concept_map.md](concept_map.md)
- **System architecture & data flows** -> [architecture.md](architecture.md)
- **CLI interaction model** -> [interaction-model.md](interaction-model.md)
- **Module & component details** -> [modules.md](modules.md)
- **Implementation patterns** -> [patterns.md](patterns.md)
