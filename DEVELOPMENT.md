# Development

Internal architecture and developer reference for 1up.

## Building

```sh
cargo build --release
just install   # builds release, copies to ~/.local/bin/1up, codesigns on macOS
```

## Benchmarking

```sh
just bench-parallel [repo]   # parallel indexing benchmarks via hyperfine
```

Compares serial (`--jobs 1`), auto, and constrained concurrency across full reindex and incremental index runs.

## Architecture

Layered architecture with a two-process model:

- **CLI** (`src/cli/`): clap-derived subcommands and output formatting (human/json/plain)
- **Indexer** (`src/indexer/`): staged pipeline — scan, parse, embed, store
- **Search** (`src/search/`): hybrid RRF fusion, symbol lookup, structural queries, context retrieval
- **Daemon** (`src/daemon/`): background file watcher with incremental re-indexing
- **Storage** (`src/storage/`): libSQL with FTS5 and native vector search
- **Shared** (`src/shared/`): types, config resolution, constants, errors

CLI and daemon both converge on `pipeline::run_with_config` for indexing. No runtime state is shared between processes — they communicate through the database, PID file, project registry, and Unix signals.

## Indexing Pipeline

The pipeline runs in stages:

1. **Scan** — walk the directory (ignore-crate, .gitignore-aware), preload stored file hashes
2. **Delete** — remove segments for files no longer present
3. **Parse** — dispatch changed files to a bounded `spawn_blocking` worker pool (`--jobs`), each tagged with a sequence ID
4. **Reorder** — `BTreeMap` reorder buffer restores deterministic file order from out-of-order worker completions
5. **Embed** — batch embeddings through a single ONNX session (`--embed-threads` controls intra-op threads)
6. **Store** — transactional single-writer replaces file segments in batches (`write_batch_files`)
7. **Progress** — persist `IndexProgress` (state, phase, work counters, parallelism, stage timings) to `.1up/index_status.json`

If the embedding model is unavailable, the pipeline stores null vectors and indexing still succeeds for full-text search.

## Concurrency Configuration

Settings resolve through a priority chain:

1. CLI flags (`--jobs`, `--embed-threads`)
2. Environment variables (`ONEUP_INDEX_JOBS`, `ONEUP_EMBED_THREADS`, `ONEUP_INDEX_WRITE_BATCH_FILES`)
3. Per-project settings persisted in the daemon registry
4. Automatic defaults (available cores - 1 for jobs, clamped embed threads)

`1up start` persists the resolved settings so the daemon reuses them. The daemon reloads settings on `SIGHUP`.

## Daemon Internals

The daemon is a detached `1up __worker` process (hidden subcommand) with a `tokio::select!` event loop:

- **File events**: drained from notify watchers, filtered, and mapped to owning projects. Each project's `ProjectRunState` enforces one active indexing pass at a time; file-change bursts collapse into at most one queued follow-up run.
- **SIGHUP**: reloads the project registry, adds/removes watchers, and refreshes per-project indexing settings.
- **SIGTERM**: unwatches all directories, cleans up the PID file, and exits.

`1up start` registers the project (with optional `IndexingConfig`) and spawns the worker if not already running, or sends `SIGHUP` to reload. `1up stop` deregisters the project and sends `SIGTERM` if no projects remain, `SIGHUP` otherwise.

## Storage Layout

```
~/.config/1up/                  # XDG config (reserved for future use)
~/.local/share/1up/             # XDG data
  daemon.pid                    # Daemon PID file
  projects.json                 # Global project registry (includes per-project IndexingConfig)
  models/
    all-MiniLM-L6-v2/
      model.onnx                # ONNX embedding model (auto-downloaded)
      tokenizer.json            # WordPiece tokenizer (auto-downloaded)

<project-root>/
  .1up/
    project_id                  # UUID identifying this project
    index.db                    # libSQL database (segments, FTS, vectors)
    index_status.json           # Latest indexing progress snapshot
```

## Search Internals

Search combines three retrieval channels via Reciprocal Rank Fusion (RRF):

- **Symbol matches**: SQL LIKE with Levenshtein fuzzy matching
- **Vector retrieval**: int8-quantized similarity prefilter (top-200), then full ranking
- **FTS5 keyword matches**: SQLite full-text search

Post-fusion quality pipeline: intent boost -> query match boost -> content kind boost -> file path penalty (test/vendor) -> short segment penalty -> overlap dedup -> per-file cap (3) -> limit.

If the embedding model is unavailable or a vector query fails, search degrades to `FtsOnly` with a warning.

## Testing

```sh
cargo test                  # unit + integration tests
cargo test -- --ignored     # tests requiring the embedding model
```

- Unit tests: `#[cfg(test)]` modules within source files
- Integration tests: `tests/` directory (assert_cmd for CLI, pipeline parity tests)
- RAII test guards: `EnvGuard` for env vars, `HideModelGuard` for model availability
- Parallel parity tests verify `--jobs 1` matches auto-parallel output

## Project Structure

```
src/
├── main.rs              # Entry point
├── cli/                 # CLI subcommands and output formatting (12 files)
├── daemon/              # Background file watcher, registry, lifecycle (5 files)
├── indexer/             # Scanner, parser, chunker, embedder, pipeline (6 files)
├── search/              # Hybrid, symbol, structural, context, ranking (9 files)
├── shared/              # Types, config, constants, errors, project (6 files)
└── storage/             # DB wrapper, schema, segments, queries (5 files)
tests/                   # Integration, CLI, SQL verification
benches/                 # Criterion search benchmarks
scripts/                 # benchmark_parallel_indexing.sh
```
