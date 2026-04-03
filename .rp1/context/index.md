# 1up - Knowledge Base

**Type**: Single Project
**Languages**: Rust
**Updated**: 2026-04-03

## Project Summary

1up is a local code search and indexing CLI tool that combines semantic vector search, full-text search (FTS5), and symbol lookup via Reciprocal Rank Fusion (RRF) to provide high-quality code search results. It uses tree-sitter for multi-language AST parsing, ONNX Runtime for embedding generation, and libSQL for storage, with a background daemon for automatic incremental re-indexing on file changes.

## Quick Reference

| Aspect | Value |
|--------|-------|
| Entry Point | `src/main.rs` -> `src/cli/mod.rs` |
| Key Pattern | Layered Architecture + Two-Process Model (CLI + Daemon) |
| Tech Stack | Rust, tokio, clap, tree-sitter (16 langs), libSQL, ONNX Runtime (all-MiniLM-L6-v2) |

## KB File Manifest

**Progressive Loading**: Load files on-demand based on your task.

| File | Lines | Load For |
|------|-------|----------|
| architecture.md | ~126 | System design, layers, data flows, integration points |
| modules.md | ~158 | Component breakdown, module responsibilities, dependencies |
| patterns.md | ~73 | Code conventions, error handling, testing idioms |
| concept_map.md | ~94 | Domain terminology (Segment, RRF, QueryIntent, etc.) |

## Task-Based Loading

| Task | Files to Load |
|------|---------------|
| Code review | `patterns.md` |
| Bug investigation | `architecture.md`, `modules.md` |
| Feature implementation | `modules.md`, `patterns.md` |
| Strategic analysis | ALL files |

## How to Load

```
Read: .rp1/context/{filename}
```

## Project Structure

```
src/
├── cli/        # CLI subcommands (search, symbol, context, structural, index, etc.)
├── daemon/     # Background file watcher and incremental re-indexing
├── indexer/    # Scanning, parsing (tree-sitter), chunking, embedding (ONNX)
├── search/     # Hybrid search, symbol lookup, structural queries, ranking
├── shared/     # Types, config, constants, errors, project identity
└── storage/    # libSQL database, schema, segment CRUD, queries
tests/          # Integration and CLI tests
benches/        # Criterion search benchmarks
```

## Navigation

- **[architecture.md](architecture.md)**: System design, data flows, integration points
- **[modules.md](modules.md)**: Component breakdown with metrics and dependencies
- **[patterns.md](patterns.md)**: Code conventions and implementation patterns
- **[concept_map.md](concept_map.md)**: Domain terminology and concept boundaries
