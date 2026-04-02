# Modules

Single crate (`oneup`, binary name `1up`) with the following module hierarchy:

## `src/cli/`

CLI layer. Clap derive-based argument structs and `exec()` async handlers for each subcommand.

| File | Responsibility |
|------|---------------|
| `mod.rs` | `Cli` struct (global flags), `Command` enum (all subcommands), `run()` dispatch |
| `init.rs` | `InitArgs` -- create `.1up/project_id` |
| `start.rs` | `StartArgs` -- index + spawn daemon |
| `stop.rs` | `StopArgs` -- deregister project, signal daemon |
| `status.rs` | `StatusArgs` -- report daemon state + index stats |
| `symbol.rs` | `SymbolArgs` -- symbol lookup with optional `--references` |
| `search.rs` | `SearchArgs` -- hybrid search with `--limit` |
| `context.rs` | `ContextArgs` -- context retrieval with `--expansion` |
| `index.rs` | `IndexArgs` -- explicit indexing with embedder auto-download |
| `reindex.rs` | `ReindexArgs` -- clear + full re-index |
| `output.rs` | `Formatter` trait with `JsonFormatter`, `HumanFormatter`, `PlainFormatter` implementations |

## `src/daemon/`

Background daemon for file watching and incremental re-indexing.

| File | Responsibility |
|------|---------------|
| `mod.rs` | Module exports |
| `lifecycle.rs` | PID file CRUD, process liveness check (via `nix` signal(0)), SIGHUP/SIGTERM sending, detached daemon spawning with `setsid`, `ensure_daemon()` for auto-start |
| `registry.rs` | JSON-serialized project registry (`projects.json`) with register/deregister/load/save |
| `watcher.rs` | `notify::RecommendedWatcher` wrapper with mpsc channel, watch/unwatch/drain_events, debounced collection, path filtering (binary files, .git, node_modules) |
| `worker.rs` | Main daemon loop using `tokio::select!` over SIGHUP (reload), SIGTERM (shutdown), and file events (incremental re-index via pipeline); runs until explicitly stopped |

## `src/indexer/`

Indexing pipeline: scan, parse, chunk, embed, store.

| File | Responsibility |
|------|---------------|
| `mod.rs` | Module exports |
| `scanner.rs` | Directory walking via `ignore` crate `WalkBuilder`, .gitignore respect, default directory exclusions, binary extension filtering, language detection from file extension |
| `parser.rs` | `SupportedLanguage` enum mapping 9 languages to tree-sitter grammars; `parse_file()` walks AST root, extracts segments with role classification, symbol collection, complexity scoring, container recursion for nested methods |
| `chunker.rs` | Sliding-window text chunking with configurable window size and overlap for unsupported languages |
| `embedder.rs` | `Embedder` struct wrapping `ort::Session` and `tokenizers::Tokenizer`; async auto-download from HuggingFace; batch inference with mean pooling + L2 normalization; `is_model_available()` and `is_download_failed()` for graceful degradation |
| `pipeline.rs` | Orchestrates scan -> hash check -> parse/chunk -> embed -> store; SHA-256 incremental detection; delete-and-rewrite per changed file; writes `SegmentInsert` rows through `segments::upsert_segment`; stores nullable `embedding_vec` values when embeddings are present and null vectors when they are not; progress bar |

## `src/search/`

Search engines: hybrid semantic+FTS, symbol lookup, context retrieval.

| File | Responsibility |
|------|---------------|
| `mod.rs` | Declares search submodules and re-exports `HybridSearchEngine`, `StructuralSearchEngine`, and `SymbolSearchEngine` |
| `hybrid.rs` | `HybridSearchEngine` -- query embedding, intent detection, symbol lookup, retrieval backend dispatch, RRF fusion, and vector-failure fallback to `FtsOnly` |
| `ranking.rs` | RRF fusion, intent-based role boosting, file path penalties (test/doc/vendor), short segment penalties, overlap deduplication, per-file caps |
| `intent.rs` | Query intent detection via keyword signal scoring: DEFINITION, FLOW, USAGE, DOCS, GENERAL |
| `retrieval.rs` | `RetrievalBackend`, `SqlVectorV2`, and `FtsOnly`; selects the backend, runs `vector_top_k(...)` or FTS queries, and hydrates `SearchResult` rows from shared SQL result shapes |
| `symbol.rs` | `SymbolSearchEngine` -- SQL LIKE queries on defined_symbols/referenced_symbols JSON columns, Levenshtein fuzzy matching, block_type priority ordering |
| `context.rs` | `ContextEngine` -- reads source from disk, tree-sitter parse to find smallest enclosing scope node, line-range fallback; `parse_location()` for `file:line` format |
| `formatter.rs` | Search result formatting utilities |

## `src/storage/`

Database access layer using libSQL.

| File | Responsibility |
|------|---------------|
| `mod.rs` | Module exports |
| `db.rs` | `Db` wrapper with `open_rw`/`open_ro`/`open_memory` constructors using `libsql::Builder`; retries local-open lock failures before surfacing an error |
| `schema.rs` | Schema-v5 initialization and validation: `prepare_for_write()`, `ensure_current()`, and `rebuild()`; owns required-object checks and explicit `1up reindex` recovery errors |
| `queries.rs` | SQL DDL and query constants for `segments`, `segments_fts`, `meta`, `idx_segments_embedding`, FTS5 `MATCH`, and `vector_top_k(...)` retrieval |
| `segments.rs` | Segment CRUD and meta helpers; upserts serialize embeddings into `embedding_vec`, supports file-hash lookups, and exposes segment/meta helpers for storage tests and maintenance paths |

## `src/shared/`

Shared types, configuration, and utilities.

| File | Responsibility |
|------|---------------|
| `mod.rs` | Module exports |
| `config.rs` | XDG path resolution (`config_dir`, `data_dir`, `model_dir`, `pid_file_path`, `projects_registry_path`, `project_db_path`, `project_dot_dir`) |
| `constants.rs` | Tunables: embedding dimensions (384), batch size (32), RRF_K (60), vector weight (1.5), result limits, chunk sizes, watcher debounce, schema version |
| `errors.rs` | Error types via `thiserror` (`OneupError`, `ConfigError`, etc.) |
| `types.rs` | Core domain types: `ParsedSegment`, `SearchResult`, `SymbolResult`, `ContextResult`, `OutputFormat`, `SegmentRole`, `ReferenceKind` |
| `project.rs` | Project ID read/write utilities, `is_initialized()` check |

## `src/main.rs`

Entry point. Initializes `tracing-subscriber`, parses `Cli` via clap, dispatches to `cli::run()`.

## `src/lib.rs`

Re-exports modules for benchmark and integration test access.
