# Development

Internal architecture and developer reference for 1up.

## Building

```sh
cargo build --release
just install   # builds release, copies to ~/.local/bin/1up, codesigns on macOS
```

## Benchmarking

```sh
just bench-parallel [repo]                # release-binary indexing benchmarks via hyperfine
bash scripts/benchmark_builderbot.sh [repo]   # 1up vs osgrep on clean index + warm queries
```

`just bench-parallel` wraps `scripts/benchmark_parallel_indexing.sh`. It snapshots the target
repo, warms the indexing environment once, and then measures three scenarios across serial,
auto, and constrained settings:

- full reindex
- scoped follow-up indexing after a small edit
- write-heavy follow-up indexing after 24 file rewrites

Artifacts land under `target/parallel-index-bench/<repo>-<timestamp>/` as `full-index.json`,
`incremental-index.json`, `write-heavy-index.json`, and `summary.json`.

`scripts/benchmark_builderbot.sh` captures a tracked-file snapshot and compares `1up` against
`osgrep` for clean indexing, warm search queries, and symbol lookups when the corpus has enough
structurally parsed files. It writes per-query hyperfine JSON, preview outputs, and
`summary.md` under `target/benchmarks/<repo>-<timestamp>/`.

## Architecture

Layered architecture with a two-process model:

- **CLI** (`src/cli/`): clap-derived subcommands and output formatting (human/json/plain)
- **Indexer** (`src/indexer/`): staged pipeline — scan, parse, embed, store
- **Search** (`src/search/`): candidate-first hybrid fusion, exact-first symbol lookup, structural queries, context retrieval
- **Daemon** (`src/daemon/`): background file watcher with scoped follow-up indexing and daemon-backed search reuse
- **Storage** (`src/storage/`): libSQL with FTS5 and native vector search
- **Shared** (`src/shared/`): types, config resolution, constants, errors

CLI and daemon both converge on `pipeline::run_with_config` for indexing. No runtime state is shared between processes — they communicate through the database, PID file, project registry, and Unix signals.

## Indexing Pipeline

The pipeline now starts with scope planning instead of assuming every follow-up run must rescan
the full repository:

1. **Scope** — `RunScope::Full` scans the whole tree; `RunScope::Paths` uses
   `scanner::scan_paths()` plus stored file hashes to touch only changed files and tracked
   deletions.
2. **Fallback** — scoped runs escalate to a full scan when a path can change scanner semantics,
   such as `.gitignore`, `.ignore`, `.git/info/exclude`, directories, or hidden/excluded files
   that no longer reconcile cleanly with indexed state.
3. **Delete** — remove segments for indexed files that disappeared. Deletes use the same batch
   transaction helpers as writes.
4. **Parse** — dispatch changed files to a bounded `spawn_blocking` worker pool (`--jobs`), each
   tagged with a sequence ID.
5. **Reorder** — `BTreeMap` reorder buffer restores deterministic file order from out-of-order
   worker completions.
6. **Embed** — batch embeddings through a single ONNX session (`--embed-threads` controls
   intra-op threads).
7. **Store** — flush ready files through the single writer. `write_batch_files` defaults scale
   with `jobs`, then `effective_write_batch_files()` caps each run to the amount of work that is
   actually ready so small follow-up runs do not over-batch.
8. **Progress** — persist `IndexProgress` (state, phase, work counters, parallelism, stage
   timings) to `.1up/index_status.json`.

The daemon feeds these scoped follow-up runs from watcher events and collapses bursts into one
queued rerun. If the embedding model is unavailable, the pipeline stores null vectors and indexing
still succeeds for full-text and symbol search.

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
  daemon.sock                   # Daemon search socket
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

`1up search` tries the daemon first when the project is registered. The CLI sends
`SearchRequest { project_root, query, limit }` over `~/.local/share/1up/daemon.sock` and waits up
to 250ms; if the daemon is unavailable, rejects the project, or times out, the CLI falls back to
the same search stack locally.

Both the daemon and the local path execute the same search pipeline:

- **Intent detection** classifies the query (`Definition`, `Usage`, `Flow`, `Docs`, `General`).
- **Exact-first symbol lookup** queries the `segment_symbols` table by normalized
  `canonical_symbol` values. Only exact misses fan out into prefix/contains candidate loads and
  fuzzy matching.
- **Candidate-first retrieval** fetches vector and FTS candidate rows first, ranks them with RRF
  plus intent/query/path/content boosts, and hydrates only the final ranked segment IDs from
  storage.
- **Warm runtime reuse** lets the daemon keep a per-project `EmbeddingRuntime`, so repeated
  searches can reuse a loaded model when the model files and `embed_threads` setting have not
  changed.

If query embedding or vector retrieval fails, that query degrades to `FtsOnly`; if the model is
missing entirely, both daemon and CLI stay in FTS-only mode and still return symbol and FTS
matches.

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
├── cli/                 # CLI subcommands and output formatting (13 files)
├── daemon/              # Background file watcher, registry, lifecycle, search service (6 files)
├── indexer/             # Scanner, parser, chunker, embedder, pipeline (6 files)
├── search/              # Hybrid, symbol, structural, context, ranking (9 files)
├── shared/              # Types, config, constants, errors, project, symbol helpers (7 files)
└── storage/             # DB wrapper, schema, segments, queries (5 files)
tests/                   # Integration, CLI, SQL verification
benches/                 # Criterion search benchmarks
scripts/                 # benchmark_parallel_indexing.sh, benchmark_builderbot.sh, benchmark_rewrite_sql.sh
```
