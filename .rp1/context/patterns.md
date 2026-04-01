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

- `Db` wrapper struct with `open_rw()`, `open_ro()`, `open_memory()` constructors using `turso::Builder`
- All Builder calls use `.experimental_index_method(true)` to enable turso-native FTS (tantivy-backed)
- `Builder::new_local()` takes `&str` (paths converted via `.to_str()`)
- Read-only access for queries; read-write for indexing
- Schema migration via `schema::migrate(&conn)` called before any DB operation
- Segments stored with JSON-encoded `defined_symbols` and `referenced_symbols` arrays
- Embedding vectors stored as both f32 (`F32_BLOB(384)`) and int8 quantized (`VECTOR8(384)`)
- FTS uses `CREATE INDEX ... USING fts(content)` with `fts_match()` / `fts_score()` queries (not SQLite FTS5)

## Graceful Degradation

- `is_model_available()` checks for ONNX model on disk
- `is_download_failed()` checks for `.download_failed` marker file
- Three-tier fallback in all embedding-dependent commands:
  1. Model available -> load and use
  2. Download previously failed -> warn, skip embeddings
  3. Model not found -> attempt download, mark failure if it fails
- Search degrades from hybrid (vector + FTS) to FTS-only with a warning
- Unsupported tree-sitter languages fall back to text chunking

## Daemon Communication

- No IPC -- shared-nothing model
- CLI and daemon communicate exclusively through:
  - Turso database (read/write)
  - PID file for liveness checking
  - Project registry JSON file
  - Unix signals (SIGHUP = reload, SIGTERM = shutdown)
- `ensure_daemon()` in `lifecycle.rs` provides auto-start on first query

## Indexing Pipeline

- Incremental by default: SHA-256 file content hash comparison
- Language routing: tree-sitter for supported languages (8), text chunker for others
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
- Benchmarks in `benches/` using `criterion`
- Temp directories via `tempfile` crate for test isolation
- In-memory or temp-file turso DB for storage tests

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
