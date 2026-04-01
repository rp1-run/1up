---
rp1_doc_id: 6326034d-8487-468e-8223-cc0c8fe599ed
---
# Project Charter: 1up

**Version**: 1.0.0
**Status**: Complete
**Created**: 2026-04-01

## Problem & Context

Agent harnesses operating on source repositories lack a unified, reliable local search substrate. Existing solutions require juggling multiple disparate tools for content search, file discovery, symbol lookup, and structural queries. The current TypeScript/Bun implementation proves the concept but needs a rewrite in Rust for better performance in iterative agent loops where latency compounds.

## Target Users

**Primary**: Agent harness builders who need a fast, predictable, machine-readable search interface over local source repositories. These users integrate 1up as a substrate within automated agent workflows.

**Secondary**: Power users and developers who manually inspect repositories and want a single CLI combining content search, file/path discovery, symbol lookup, and structural search.

Design prioritizes machine use first, human use second.

## Value Proposition

1up provides a single tool with multiple search modes — content search, file/path discovery, symbol lookup, and structural search — all behind a unified CLI with machine-readable output as a first-class concern. It combines hybrid semantic + full-text search with vector embeddings, tree-sitter parsing, and reciprocal rank fusion ranking. It is read-only and safe, works on imperfect codebases with graceful degradation, and delivers predictable structured output optimized for agent consumption.

### Core Principles

1. **One tool, multiple search modes** — harness shouldn't need separate tools for content, path, symbol, and context search
2. **Read-only and safe by default** — never modifies repository or user environment
3. **Fast enough for iterative agent loops** — optimized for low-latency repeated use
4. **Predictable output** — stable, structured, easy to rank or post-process
5. **Works on imperfect codebases** — partially broken, multi-language, monorepos, partially generated, inconsistently organized
6. **Graceful degradation** — if structural/symbol mode is unavailable, produce useful fallback results instead of failing

## Scope

**In scope (v1)**:
- Full port of the existing TypeScript/Bun implementation to Rust
- Ripgrep-powered file discovery and text search alongside semantic and FTS search modes
- All existing search modes: text, path, symbol, reference, structural, context retrieval
- Hybrid search with vector embeddings, tree-sitter parsing, RRF ranking
- JSON-first machine-readable output
- Daemon architecture for file watching and incremental indexing

**Out of scope (v1)**:
- Writing/editing files or repository mutation of any kind
- Refactoring or rename operations
- Code execution or test running
- Full IDE language server semantics
- Indexing remote repositories
- Semantic/vector retrieval as a hard requirement (graceful degradation when unavailable)

## Success Criteria

1. **Feature parity**: All existing TypeScript/Bun features are fully replicated in the Rust implementation
2. **Performance**: Measurable speed and latency improvements over the TypeScript version, validated through benchmarking
3. **Search quality**: No regression in search relevance or ranking quality compared to the TS version
4. **Harness integration**: A harness builder can invoke commands, parse JSON, and make decisions without brittle scraping
