# Implementation Patterns

**Project**: 1up
**Last Updated**: 2026-04-03

## Naming & Organization

**Files**: snake_case.rs, one module per concern, grouped by layer (`cli/`, `daemon/`, `indexer/`, `search/`, `storage/`, `shared/`)
**Functions**: snake_case verbs; CLI handlers use `async fn exec(args, format) -> anyhow::Result<()>`; constructors use `new()`, `from_dir()`, `open_rw()`/`open_ro()`
**Imports**: Absolute paths (`crate::shared::types::...`), grouped std -> external -> internal, no wildcard imports
**Constants**: `SCREAMING_SNAKE_CASE`, centralized in `shared/constants.rs`
**Types**: PascalCase; CLI args as `<Subcommand>Args`; engines as `<Feature>Engine`

Evidence: `src/shared/constants.rs`, `src/cli/mod.rs`, `src/search/mod.rs`

## Type & Data Modeling

**Data Representation**: Plain structs with pub fields; serde Serialize/Deserialize on API-facing types; `#[serde(skip_serializing_if = "Option::is_none")]` for optional fields
**Type Strictness**: Strong typing with dedicated enums (SegmentRole, QueryIntent, RetrievalMode, OutputFormat, ReferenceKind); `FromStr` impls for CLI-facing enums
**Immutability**: Structs mutable by default (pub fields); small enums are Copy; Clone derived broadly
**Nullability**: `Option<T>` for nullable fields; `some_if_not_empty(Vec<T>) -> Option<Vec<T>>` helper pattern

Evidence: `src/shared/types.rs:1-98`, `src/search/retrieval.rs:244-250`

## Error Handling

**Strategy**: Two-tier: thiserror for domain errors (`OneupError` with nested per-module enums), `anyhow::Result` at CLI boundary
**Propagation**: Domain modules return `Result<T, OneupError>` with `#[from]` conversions; CLI uses `bail!()` for user-facing errors; `eprintln!("warning: ...")` for degraded-mode warnings
**Common Types**: OneupError, StorageError, IndexingError, SearchError, EmbeddingError, ParserError, DaemonError, ConfigError, ProjectError
**Recovery**: Embedding failures -> FTS-only search; vector retrieval failures -> FTS fallback within same query; tree-sitter failures -> text chunking

Evidence: `src/shared/errors.rs`, `src/search/hybrid.rs:39-51`, `src/indexer/pipeline.rs:153-173`

## Validation & Boundaries

**Location**: CLI boundary (clap derive) and query execution entry points
**Method**: clap derive for args; manual guards at function entry (empty query, line range bounds, file existence); schema version validation before DB read/write
**Early Rejection**: `bail!()` for missing index, schema mismatch, out-of-range lines; `SearchError::InvalidQuery` for invalid queries

Evidence: `src/cli/search.rs:38-43`, `src/storage/schema.rs:110-132`

## Observability

**Logging**: tracing crate with `debug!`/`info!` macros; `-v`/`-vv` CLI flag for verbosity; debug-level for skipped files, queued follow-up runs, and fallback decisions; info-level for pipeline summaries with configured/effective workers and per-stage timings
**Metrics**: None detected
**Tracing**: None detected
**Progress**: `IndexProgress` snapshots persisted to `.1up/index_status.json`; human, plain, and JSON formatters render work summaries, effective parallelism, and scan/parse/embed/store/total timings; nanospinner remains the interactive CLI progress indicator

Evidence: `src/indexer/pipeline.rs`, `src/cli/output.rs`, `src/daemon/worker.rs`

## Testing Idioms

**Organization**: Unit tests in `#[cfg(test)]` within source files; integration tests in `tests/`; manual performance workflow in `scripts/benchmark_parallel_indexing.sh` exposed as `just bench-parallel`
**Fixtures**: `setup() -> (Db, Connection)` using `Db::open_memory()`; `tempfile::tempdir()` for filesystem and repo-copy tests; helper builders such as `make_result`, `test_segment`, and `make_segment`
**Levels**: Unit-dominant with edge case coverage; integration via assert_cmd; parity tests compare `jobs=1` and parallel incremental runs; model-dependent tests still guard on `is_model_available()` when embeddings are required
**Patterns**: `#[tokio::test]` for async; descriptive regression names; storage helper tests validate transactional rollback; CLI coverage validates new concurrency flags and help text

Evidence: `src/indexer/pipeline.rs`, `src/storage/segments.rs`, `tests/cli_tests.rs`, `scripts/benchmark_parallel_indexing.sh`

## I/O & Integration

**Database**: libsql via `Db` wrapper with `open_rw`/`open_ro`/`open_memory`; SQL as `pub const &str` in `storage/queries.rs`; async row iteration with column extraction by index; schema versioning via meta table
**HTTP Clients**: reqwest for model downloads with streaming byte transfer; no retry for HTTP (only DB lock contention)
**File I/O**: ignore crate (WalkBuilder) for gitignore-aware scanning; SHA-256 hashing for incremental detection; `std::fs` for synchronous reads in parser/context

Evidence: `src/storage/queries.rs`, `src/indexer/scanner.rs:62-110`, `src/indexer/embedder.rs:310-367`

## Concurrency & Async

**Async Usage**: Tokio full runtime; all CLI handlers and DB operations async; file-local parse work runs through a bounded `JoinSet::spawn_blocking` pool; embedder inference stays synchronous inside one ONNX session; storage writes remain serialized
**Patterns**: Sequence IDs plus a `BTreeMap` reorder buffer restore deterministic file order before flush; `write_batch_files` controls transactional replacement batch size; daemon `ProjectRunState` enforces one active run per project and collapses bursts into at most one queued follow-up pass

Evidence: `src/indexer/pipeline.rs`, `src/daemon/worker.rs`, `src/shared/types.rs`
