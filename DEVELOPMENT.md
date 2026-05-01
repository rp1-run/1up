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
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
just security-check
```

Additional commands for heavier or optional validation:

```sh
cargo test -- --ignored
just eval-parallel --summary
```

`just security-check` executes the repo security gate and writes retained evidence to `target/security/security-check.json`.

## MCP Adoption Evals

Agent adoption evals live under `evals/suites/1up-search/` and `evals/suites/1up-impact/`. The 1up provider starts the local MCP server with command `1up` and args `["mcp", "--path", "."]`, then grades provider MCP metadata for canonical `oneup_*` calls.

The 1up eval variant expects this chain:

1. `oneup_prepare` when readiness is uncertain.
2. `oneup_search` before any raw discovery tool.
3. `oneup_read` to hydrate returned handles or precise locations.
4. `oneup_symbol` for completeness-oriented definition/reference checks.
5. `oneup_impact` for likely-impact tasks, with primary/contextual interpretation.

Raw `grep`, `rg`, and `find` are graded as wrong discovery tools in the 1up variant. `grep` and `rg` are allowed only for exact literal verification after `oneup_search` narrows scope to precise files.

Useful eval checks:

```sh
cd evals
npm run lint
npm test
npx promptfoo validate -c suites/1up-search/evals.yaml
npx promptfoo validate -c suites/1up-impact/evals.yaml
```

These suites are the adoption evidence for MCP installation readiness: agents should call `oneup_prepare`, discover with `oneup_search`, hydrate with `oneup_read`, and use `oneup_symbol` or `oneup_impact` before falling back to raw file search for supported discovery tasks.

Release evidence records either the retained adoption summary JSON or an explicit skipped reason; it does not introduce a second eval harness.

## MCP Installation Release Checks

The user-facing setup guide is [docs/mcp-installation.md](docs/mcp-installation.md). Keep it aligned with the thin wrapper contract: `1up add-mcp` validates the repository path, selects `bunx` or `npx`, delegates host configuration mutation to external `add-mcp`, and prints manual fallback guidance when delegation cannot continue.

Focused wrapper validation:

```sh
cargo test add_mcp
cargo test --test cli_tests add_mcp
```

Installed-binary MCP smoke should exercise the binary users install, not `target/debug/1up`:

```sh
bash scripts/release/verify_mcp_smoke.sh \
  --binary /path/to/1up \
  --repo /path/to/fixture-repo \
  --output target/release-evidence/mcp-smoke.json
```

Archive verification records MCP smoke beside version smoke:

```sh
bash scripts/release/verify_release_archives.sh --help
```

Maintainer-run live host evidence records either an observed smoke or an explicit skip reason for Codex, Claude Code, Cursor, VS Code/Copilot, and generic MCP clients:

```sh
bash scripts/release/record_mcp_host_smoke.sh --help
```

Release evidence requires each archive verification summary to contain both the version smoke and installed-binary `mcp_smoke_test`. It also includes live-host smoke evidence from `mcp_host_smoke.v1` when a maintainer attaches it, otherwise it records a skipped reason for the supported hosts. Setup modes stay limited to wrapper-mediated `add-mcp`, direct `add-mcp`, and manual setup; there is no native 1up installer, custom config writer, or host adapter fallback.

Release evidence should include archive MCP smoke, live-host recorded or skipped evidence, and adoption-eval evidence or skipped reasons:

```sh
bash scripts/release/generate_release_evidence.sh \
  --manifest target/release/release-manifest.json \
  --merge-gate target/release-evidence/merge-gate.json \
  --security-check target/release-evidence/security-check.json \
  --archive-verification target/release-evidence/archive-verification.json \
  --mcp-host-smoke target/release-evidence/mcp-host-smoke.json \
  --eval-summary target/release-evidence/eval-summary.json \
  --benchmark-summary target/release-evidence/benchmark-summary.json \
  --output target/release-evidence/release-evidence.json
```

The release evidence workflow is [.github/workflows/release-evidence.yml](.github/workflows/release-evidence.yml). Proprietary live-host checks are maintainer evidence, not mandatory CI credentials.

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

For public eval workflows, use the authored suites under `evals/suites/1up-search/` and `evals/suites/1up-impact/`. The adoption suites target the pinned `emdash` fixture through MCP provider metadata. The recall harness remains a separate CLI retrieval-quality gate for ranking and vector-storage changes.

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
