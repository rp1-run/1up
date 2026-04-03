# Module & Component Breakdown

**Project**: 1up
**Analysis Date**: 2026-04-03
**Modules Analyzed**: 8

## Core Modules

### CLI (`src/cli/`)
**Purpose**: User-facing command parsing and output formatting via clap derive
**Files**: 12 | **Lines**: ~695

**Components**:
- **Cli** (`mod.rs`): Top-level CLI struct with `Command` enum dispatch, global `--format` and `--verbose` flags
- **SearchArgs** (`search.rs`): Hybrid search with `--limit` and auto-daemon-start
- **SymbolArgs** (`symbol.rs`): Symbol lookup with optional `--references` flag
- **ContextArgs** (`context.rs`): Context retrieval for `file:line` locations
- **StructuralArgs** (`structural.rs`): AST pattern search using tree-sitter S-expression queries
- **IndexArgs** (`index.rs`): Explicit indexing with embedder auto-download and progress spinners
- **ReindexArgs** (`reindex.rs`): Force clear + full re-index with schema rebuild
- **Formatter** (`output.rs`): Output formatting trait with JSON, Human, and Plain implementations

**Dependencies**: search, indexer, storage, daemon, shared

### Search (`src/search/`)
**Purpose**: Search engines: hybrid semantic+FTS, symbol lookup, structural AST queries, and context retrieval
**Files**: 9 | **Lines**: ~2,211

**Components**:
- **HybridSearchEngine** (`hybrid.rs`): Orchestrates multi-signal search with RRF fusion; builds symbol name variants; degrades to FTS-only when embedder unavailable
- **RetrievalBackend** (`retrieval.rs`): Backend selection (SqlVectorV2 or FtsOnly) based on index state; auto-detects embedded embeddings
- **SymbolSearchEngine** (`symbol.rs`): Definition and reference lookup via SQL LIKE with Levenshtein fuzzy matching
- **ContextEngine** (`context.rs`): Source context retrieval using tree-sitter scope detection with line-range fallback
- **StructuralSearchEngine** (`structural.rs`): Tree-sitter S-expression queries across indexed files; fallback to directory scan
- **IntentDetector** (`intent.rs`): Signal-based query classification into Definition, Flow, Usage, Docs, General
- **Ranking** (`ranking.rs`): RRF fusion, intent boosting, path penalties, per-file caps
- **Formatter** (`formatter.rs`): Search result formatting utilities

**Dependencies**: storage, indexer (parser, scanner), shared

### Indexer (`src/indexer/`)
**Purpose**: Bounded staged indexing: scan, parse, embed, and single-writer persistence
**Files**: 6 | **Lines**: ~4,143

**Components**:
- **Pipeline** (`pipeline.rs`): Config-aware orchestrator with hash preload, deleted-file cleanup, bounded `spawn_blocking` parse workers, deterministic reorder buffer, batched embedding, transactional file replacement, and persisted progress/timing snapshots
- **Parser** (`parser.rs`): Multi-language AST parsing via tree-sitter; 16 language grammars; role classification and symbol collection
- **Embedder** (`embedder.rs`): ONNX engine (all-MiniLM-L6-v2) with auto-download, configurable intra-op threads, batch inference, mean pooling, L2 normalization
- **Scanner** (`scanner.rs`): Directory walking via ignore crate with .gitignore respect and binary filtering
- **Chunker** (`chunker.rs`): Sliding-window text chunking (60-line window, 10-line overlap) for unsupported languages flushed through the same staged pipeline

**Dependencies**: storage, shared

### Storage (`src/storage/`)
**Purpose**: Database access layer using libSQL with FTS5, vector indexing, and transactional file replacement helpers
**Files**: 5 | **Lines**: ~1,233

**Components**:
- **Db** (`db.rs`): Database connection wrapper with `open_rw`/`open_ro`/`open_memory` constructors, lock retry
- **Schema** (`schema.rs`): Schema initialization, validation, and rebuild with recovery guidance
- **Segments** (`segments.rs`): Segment CRUD, bulk file-hash preload, deleted-file cleanup, and single-file or multi-file transactional replacement helpers
- **Queries** (`queries.rs`): SQL DDL and query constants for segments, FTS, meta, bulk hash preload, and vector retrieval

**Dependencies**: shared

### Daemon (`src/daemon/`)
**Purpose**: Background daemon for file watching, non-overlapping incremental re-indexing, and persisted per-project indexing settings
**Files**: 5 | **Lines**: ~783

**Components**:
- **Worker** (`worker.rs`): Main event loop with `tokio::select!`, per-project run state, burst-collapsing follow-up scheduling, and shared config resolution before dispatch into `run_with_config`
- **Lifecycle** (`lifecycle.rs`): Start/stop/ensure daemon with PID management and stale detection
- **Watcher** (`watcher.rs`): Filesystem event monitoring via notify crate with debounce and non-blocking drain support
- **Registry** (`registry.rs`): Project registration plus optional persisted `IndexingConfig` in the JSON-based project list

**Dependencies**: indexer, storage, shared

### Shared (`src/shared/`)
**Purpose**: Cross-cutting types, config resolution, constants, error types, and project utilities
**Files**: 6 | **Lines**: ~498

**Components**:
- **Types** (`types.rs`): ParsedSegment, SearchResult, SymbolResult, ContextResult, StructuralResult, `IndexingConfig`, `IndexProgress`, `IndexParallelism`, `IndexStageTimings`, OutputFormat, SegmentRole
- **Config** (`config.rs`): XDG-compliant paths plus indexing config resolution across CLI flags, env vars, registry values, and defaults
- **Constants** (`constants.rs`): Tuning constants, watcher debounce, env var names, and embedding/search limits
- **Errors** (`errors.rs`): OneupError hierarchy with thiserror derives
- **Project** (`project.rs`): Project identity (UUID) and database path resolution

**Dependencies**: None (foundation module)

## Support Modules

### Tests (`tests/`)
**Files**: 3 | **Lines**: ~1,176
- `integration_tests.rs`: End-to-end pipeline and search tests
- `cli_tests.rs`: CLI subcommand tests via assert_cmd
- `rewrite_sql_verification.rs`: SQL schema verification

### Benchmarks (`benches/`)
**Files**: 1 | **Lines**: ~399
- `search_bench.rs`: Criterion benchmarks for symbol lookup, FTS, retrieval backends

## Module Dependencies

```mermaid
graph TD
    CLI[cli] --> Search[search]
    CLI --> Indexer[indexer]
    CLI --> Storage[storage]
    CLI --> Daemon[daemon]
    CLI --> Shared[shared]
    Search --> Storage
    Search --> Indexer
    Search --> Shared
    Indexer --> Storage
    Indexer --> Shared
    Daemon --> Indexer
    Daemon --> Storage
    Daemon --> Shared
    Storage --> Shared
```

## Module Metrics

| Module | Files | Lines | Components | Avg File Size |
|--------|-------|-------|------------|---------------|
| cli | 12 | 695 | 8 | 58 |
| search | 9 | 2,211 | 6 | 246 |
| indexer | 6 | 4,143 | 5 | 691 |
| storage | 5 | 1,233 | 4 | 247 |
| daemon | 5 | 783 | 4 | 157 |
| shared | 6 | 498 | 5 | 83 |
| tests | 3 | 1,176 | 3 | 392 |
| benches | 1 | 399 | 1 | 399 |

## Cross-Module Patterns

- **Layered Architecture**: CLI -> Search/Indexer -> Storage -> Shared; strict dependency hierarchy
- **Graceful Degradation**: Missing embedder degrades hybrid search to FTS-only with user warnings
- **Schema Versioning**: Storage validates schema on read/write; mismatches direct to `1up reindex`
- **Auto-Start Daemon**: Search commands auto-start daemon via `lifecycle::ensure_daemon()` if project initialized
- **Staged Pipeline**: Bounded parse concurrency, deterministic reorder, single embed session, and transactional single-writer storage

## External Dependencies

| Crate | Version | Purpose | Used By |
|-------|---------|---------|---------|
| clap | 4 | CLI argument parsing (derive) | cli |
| libsql | 0.9 | SQLite with vector + FTS5 | storage, search |
| tree-sitter | 0.26 | Multi-language AST parsing | indexer, search |
| ort | 2.0.0-rc.12 | ONNX runtime for embeddings | indexer |
| tokenizers | 0.22 | HuggingFace tokenizer | indexer |
| notify | 7 | Filesystem event watching | daemon |
| ignore | 0.4 | .gitignore-aware directory walking | indexer |
| reqwest | 0.13 | HTTP client for model download | indexer |
| sha2 | 0.11 | SHA-256 for incremental detection | indexer |
| nix | 0.31 | Unix signal/process management | daemon |
| thiserror | 2 | Derive-based error types | shared |
