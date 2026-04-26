# Development

Internal build, test, and engineering reference for `1up`.

`README.md` is the install-first user guide. This document is the secondary source-build and repo-maintainer reference.

## Source Build

Clone the public repository and build from source:

```sh
git clone https://github.com/rp1-run/1up.git
cd 1up
cargo build --release
```

Install a development binary into Cargo's bin directory:

```sh
cargo install --path .
```

For a local macOS developer install that copies the release binary into `~/.local/bin/1up` and applies ad hoc codesigning, use:

```sh
just install
```

## Testing

Run the default validation set before opening a pull request:

```sh
cargo fmt --check
cargo test
just security-check
```

Additional commands for heavier or optional validation:

```sh
cargo test -- --ignored
just eval-parallel --summary
```

`just security-check` executes the repo security gate and writes retained evidence to `target/security/security-check.json`.

## Benchmarking

```sh
just bench
just bench-parallel
```

`just bench` runs the public search benchmark on the pinned `emdash` fixture and compares `1up` against raw `rg` command sequences for the same task prompts.

`just bench-parallel` wraps `scripts/benchmark_parallel_indexing.sh`. It checks out the pinned `emdash` fixture, warms the indexing environment once, and then measures three scenarios across serial, auto, and constrained settings:

- full reindex
- scoped follow-up indexing after a small edit
- write-heavy follow-up indexing after 24 file rewrites

Artifacts land under `target/parallel-index-bench/<repo>-<timestamp>/` as `full-index.json`, `incremental-index.json`, `write-heavy-index.json`, and `summary.json`.

For public eval workflows, use the authored suite under `evals/suites/1up-search/`. That suite also targets the same pinned `emdash` fixture.

## Release And Governance Docs

- Release history: [CHANGELOG.md](CHANGELOG.md)
- Release runbook: [RELEASE.md](RELEASE.md)
- Contributor and merge policy: [CONTRIBUTING.md](CONTRIBUTING.md)

## Architecture

1up uses a layered architecture with a two-process model:

- **CLI** (`src/cli/`): clap-derived subcommands and output formatting
- **Indexer** (`src/indexer/`): staged scan, parse, embed, and store pipeline
- **Search** (`src/search/`): hybrid ranking, exact-first symbol lookup, structural queries, and context retrieval
- **Daemon** (`src/daemon/`): background file watcher with scoped follow-up indexing and daemon-backed search reuse
- **Storage** (`src/storage/`): libSQL with FTS5 and native vector search
- **Shared** (`src/shared/`): types, config resolution, secure filesystem helpers, constants, and errors

CLI and daemon both converge on `pipeline::run_with_config` for indexing. Runtime state is shared through the database, PID file, project registry, and Unix signals rather than process-local memory.

## Indexing Pipeline

The indexing pipeline is staged and bounded:

1. **Scope**: `RunScope::Full` scans the whole tree; `RunScope::Paths` only touches changed files and tracked deletions.
2. **Fallback**: scoped runs escalate to a full scan when ignore files, directories, or hidden-path semantics can affect correctness.
3. **Delete**: remove segments for indexed files that disappeared.
4. **Parse**: dispatch changed files to a bounded `spawn_blocking` worker pool (`--jobs`).
5. **Reorder**: a `BTreeMap` buffer restores deterministic file order.
6. **Embed**: batch embeddings through a single ONNX session (`--embed-threads`).
7. **Store**: flush ready files through the single writer with adaptive batching.
8. **Progress**: persist `IndexProgress` to `.1up/index_status.json`.

If the embedding model is unavailable, indexing still succeeds for full-text and symbol search.

## Concurrency Configuration

Settings resolve through this priority chain:

1. CLI flags (`--jobs`, `--embed-threads`)
2. Environment variables (`ONEUP_INDEX_JOBS`, `ONEUP_EMBED_THREADS`, `ONEUP_INDEX_WRITE_BATCH_FILES`)
3. Per-project settings persisted in the daemon registry
4. Automatic defaults based on available cores

`1up start` persists the resolved settings so the daemon can reuse them later. The daemon reloads settings on `SIGHUP`.

## Daemon And Storage Internals

The daemon is a detached `1up __worker` process with a `tokio::select!` event loop that coordinates file watching, scoped re-indexing, and daemon-backed search requests. IPC uses length-prefixed JSON frames over `~/.local/share/1up/daemon.sock` with same-UID checks, bounded request and response sizes, and short timeouts so the CLI can fall back to local execution.

All daemon-managed state goes through `src/shared/fs.rs`. The XDG data root (`~/.local/share/1up/`) and each project's `.1up/` directory are created with owner-only permissions. Sensitive files such as `daemon.pid`, `projects.json`, `project_id`, and verified model manifests are written atomically and validated against approved roots.
