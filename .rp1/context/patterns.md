# Patterns

## Error Handling

- **Library errors**: `thiserror` for typed errors in `shared/errors.rs` (`OneupError`, `ConfigError`, etc.)
- **Application errors**: `anyhow::Result` in CLI handlers and top-level orchestration
- **Pattern**: Domain modules return typed errors; CLI `exec()` functions use `anyhow::Result` and `anyhow::bail!()` for early exits

## Async Runtime

- Tokio with `features = ["full"]`
- All CLI `exec()` handlers are `async fn`
- Main entry uses `#[tokio::main]`
- Daemon worker loop uses `tokio::select!` for multiplexing signals, timers, and file events

## CLI Structure

- Clap derive-based: `Cli` struct with global flags, `Command` enum with subcommand variants
- Each subcommand has:
  - An `Args` struct (clap derive) in its own file
  - An `async fn exec(args, format) -> anyhow::Result<()>` handler
- Global `--format` flag (`OutputFormat` enum: json/human/plain) and `--verbose` flag
- `Formatter` trait with `JsonFormatter`, `HumanFormatter`, `PlainFormatter` -- chosen via `formatter_for(format)`

## Database Access

- `Db` wrapper struct with `open_rw()`, `open_ro()`, `open_memory()` constructors using `libsql::Builder::new_local()`
- Local opens retry on lock-style errors before surfacing a connection failure
- `Builder::new_local()` takes `&str` (paths converted via `.to_str()`)
- Read-only access for queries; read-write for indexing
- `schema::prepare_for_write(&conn)` initializes empty databases or rejects stale indexes; `schema::ensure_current(&conn)` guards read paths; `schema::rebuild(&conn)` is the explicit clean-rebuild entry point
- Segments store JSON-encoded symbol arrays and a nullable native vector column (`embedding_vec`) written through `vector(?8)`
- Vector retrieval uses `vector_top_k('idx_segments_embedding', vector(?), ?)` and hydrates candidates from `segments`
- Keyword retrieval uses SQLite FTS5 `MATCH` against `segments_fts`

## Graceful Degradation

- `is_model_available()` checks for ONNX model on disk
- `is_download_failed()` checks for `.download_failed` marker file
- Missing, stale, or partial indexes are recovery-path errors, not graceful fallbacks; read and write paths surface explicit run `1up reindex` guidance
- `index` and `reindex` continue without embeddings when the model is missing, previously failed to download, or fails to load
- Search degrades to `FtsOnly` with a warning when the model is unavailable or query embedding generation fails
- `HybridSearchEngine` also degrades only the current invocation to `FtsOnly` if `SqlVectorV2` retrieval fails at query time
- Unsupported tree-sitter languages fall back to text chunking

## Daemon Communication

- No IPC -- shared-nothing model
- CLI and daemon communicate exclusively through:
  - libSQL database (read/write)
  - PID file for liveness checking
  - Project registry JSON file
  - Unix signals (SIGHUP = reload, SIGTERM = shutdown)
- `ensure_daemon()` in `lifecycle.rs` provides auto-start on first query

## Indexing Pipeline

- Incremental by default: SHA-256 file content hash comparison
- Language routing: tree-sitter for supported languages (9), text chunker for others
- Batch embedding: configurable batch size (default 32)
- Deleted file detection: segments for missing files removed
- Progress reporting via `indicatif`

## Search Ranking

- Reciprocal Rank Fusion (RRF) combining vector and FTS rankings
- Intent detection classifies queries into categories (DEFINITION, FLOW, USAGE, DOCS, GENERAL) for role-based boosting
- Penalties for test files, doc files, vendor paths, short segments
- Overlap deduplication and per-file result caps (max 3 per file)
- Configurable constants in `shared/constants.rs`

## Testing Conventions

- Unit tests in `#[cfg(test)]` modules within source files
- Integration tests in `tests/` directory using `assert_cmd` and `predicates`
- `tests/rewrite_sql_verification.rs` covers stale-v4 guidance, partial-v5 guidance, degraded FTS-only search, freshness after add/edit/delete, and the read-only guarantee for source files
- Benchmarks in `benches/` using `criterion`
- `benches/search_bench.rs` exercises retrieval and fusion paths with schema-v5 data shapes
- `scripts/benchmark_rewrite_sql.sh` is the repeatable baseline-vs-candidate latency and quality harness for the rewrite rollout
- Temp directories via `tempfile` crate for test isolation
- In-memory or temp-file libSQL DB for storage tests

## File Organization

- One module per concern, grouped into directories by layer (cli, daemon, indexer, search, storage, shared)
- `mod.rs` files export submodules and key types
- Domain types centralized in `shared/types.rs`
- SQL queries centralized in `storage/queries.rs`
- Constants centralized in `shared/constants.rs`

## Naming Conventions

- Types: `PascalCase` (e.g., `ParsedSegment`, `HybridSearchEngine`)
- Functions: `snake_case` (e.g., `parse_file`, `find_definitions`)
- Constants: `SCREAMING_SNAKE_CASE` (e.g., `EMBEDDING_DIM`, `RRF_K`)
- Module files: `snake_case.rs`
- CLI subcommand args: `<SubcommandName>Args` (e.g., `SearchArgs`, `SymbolArgs`)
- CLI handlers: `exec(args, format)` pattern
