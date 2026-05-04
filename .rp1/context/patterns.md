# 1up - Patterns

## Naming And Layout

- Files stay `snake_case` inside layer directories: `src/cli`, `src/daemon`, `src/indexer`, `src/mcp`, `src/search`, `src/shared`, `src/storage` (`src/cli/mod.rs`, `src/mcp/mod.rs`).
- New CLI commands are wired through `mod.rs` module exports and `Command` enum arms, not runtime registries (`src/cli/mod.rs:1`, `src/cli/mod.rs:39`).
- CLI command modules use `<Name>Args` plus `pub async fn exec(...)`; maintenance commands accept `OutputFormat`, core commands own fixed output (`src/cli/search.rs:17`, `src/cli/index.rs:16`).
- MCP is a distinct layer: schema input types in `types.rs`, operation adapters in `ops.rs`, rmcp tool wrappers in `tools.rs`, stdio server in `server.rs` (`src/mcp/types.rs`, `src/mcp/tools.rs`).
- Imports are explicit `crate::...` paths, grouped after std/external imports (`src/search/hybrid.rs`, `src/daemon/worker.rs`).
- Bash scripts use stage-oriented helper functions (`log`, `fail`, `require_cmd`, validators) and source shared release helpers rather than duplicating release logic (`scripts/install/setup.sh`, `scripts/release/common.sh`).

## Data Modeling

- Public contracts use owned structs/enums with serde derives; enum wire names use `snake_case`, `lowercase`, or `SCREAMING_SNAKE_CASE` explicitly (`src/shared/types.rs:13`, `src/search/impact.rs:75`).
- Additive compatibility fields are `Option<T>` or defaulted `Vec<T>` with `skip_serializing_if`; this applies to CLI/daemon/MCP/progress contracts (`src/shared/types.rs:75`, `src/mcp/ops.rs:62`).
- Core discovery structs stay lean and handle-oriented: `SearchResult`, `SymbolResult`, `StructuralResult`, and `ContextResult` keep bodies only where needed for hydration (`src/shared/types.rs:58`, `src/cli/lean.rs`).
- Impact uses stable envelopes: `status`, `resolved_anchor`, primary `results`, optional `contextual_results`, `hint`, and `refusal` (`src/search/impact.rs:145`).
- MCP tools wrap every result in `ToolEnvelope { status, summary, data, next_actions }`; dynamic JSON fields get object schemas for host compatibility (`src/mcp/types.rs:85`, `tests/integration_tests.rs:1293`).
- Indexing telemetry is typed and additive: `SetupTimings`, `IndexStageTimings`, `IndexScopeInfo`, `IndexPrefilterInfo`, and `IndexProgress` (`src/shared/types.rs:312`, `src/shared/types.rs:333`).
- Storage keeps stable identity plus disambiguation evidence: symbols, relations, `lookup_canonical_symbol`, `qualifier_fingerprint`, `edge_identity_kind`, and `indexed_files` metadata (`src/storage/queries.rs:48`, `src/storage/queries.rs:85`).

## Error Handling

- CLI boundaries use `anyhow::Result` and `bail!` for invalid user input and missing local state (`src/cli/impact.rs:55`, `src/cli/get.rs:45`).
- Library/storage/search/daemon layers use `OneupError` with `thiserror` domain enums (`StorageError`, `SearchError`, `DaemonError`, etc.) (`src/shared/errors.rs`).
- Stale or incompatible indexes fail closed via `schema::ensure_current` and append the standard `run 1up reindex` hint (`src/storage/schema.rs:149`).
- Search degrades per query to FTS-only when embedding or vector retrieval fails; user-visible warnings go to stderr and debug detail goes to tracing (`src/search/hybrid.rs:40`, `src/search/hybrid.rs:68`).
- Advisory impact failures are represented as `refused` or `empty` envelopes instead of hard process errors; MCP maps refused/all-failed calls to structured errors (`src/search/impact.rs:145`, `src/mcp/tools.rs:261`).
- Daemon request failures return safe unavailable/busy responses rather than leaking raw failure details across IPC (`src/daemon/search_service.rs:21`, `src/daemon/worker.rs:199`).

## Validation And Boundaries

- Validation is concentrated at clap argument parsing, CLI request builders, MCP input schemas, filesystem gates, IPC frames, schema readiness, and transaction seams.
- Core commands intentionally reject presentation flags; only maintenance command args expose `--format`/`-f` (`src/cli/mod.rs:102`, `src/cli/mod.rs:201`).
- MCP input structs use `serde(deny_unknown_fields)` and explicit defaults/aliases for stable host behavior (`src/mcp/types.rs:18`).
- Impact validates exactly one anchor and limits `line` to file anchors in both CLI and MCP paths (`src/cli/impact.rs:55`, `src/mcp/tools.rs:365`).
- Paths are canonicalized and clamped to approved roots; secure state writes reject symlinks and unexpected leaf types (`src/shared/fs.rs:25`, `src/shared/fs.rs:82`).
- Daemon IPC uses same-UID authorization, length-prefixed JSON frames, max byte limits, and read/write timeouts (`src/daemon/ipc.rs`, `src/daemon/search_service.rs:73`).
- Config parsing rejects non-positive numeric env/CLI values and resolves indexing config from CLI, env, persisted registry, then defaults (`src/shared/config.rs:106`, `src/shared/types.rs:205`).
- Install/release scripts fail fast on missing tools, unsupported platforms, bad arguments, malformed manifests, and checksum mismatches (`scripts/install/setup.sh`, `scripts/release/generate_release_evidence.sh`).

## Output Contracts

- Core agent-facing CLI commands emit one lean, line-oriented protocol and no format variants: `search`, `symbol`, `impact`, `context`, `structural`, and `get` (`src/cli/lean.rs`, `src/cli/mod.rs:56`).
- Discovery rows use two ASCII spaces, integer score, `path:l1-l2`, kind, `breadcrumb::symbol`, and a `:12-char-handle`; impact rows add `~P` or `~C` (`src/cli/lean.rs:8`).
- `get` is the fat hydration companion and preserves request order with `segment ... ---` records or `not_found` records (`src/cli/get.rs:14`, `src/cli/lean.rs:165`).
- Maintenance commands still render through `HumanFormatter`, `PlainFormatter`, and `JsonFormatter`; JSON/plain identifiers remain stable for automation (`src/cli/output.rs`, `src/cli/mod.rs:106`).
- MCP text content mirrors the structured envelope summary, while `structuredContent` carries machine data and canonical `oneup_*` next actions (`src/mcp/tools.rs:831`, `tests/integration_tests.rs:152`).
- MCP core discovery responses stay presentation-free: no ANSI color, spinners, or terminal table formatting in prepare, search, read, symbol, or read-location context responses.
- MCP source context is represented through `oneup_read.locations`; status/start behavior is represented through `oneup_prepare` modes, not separate alias tools.
- `add-mcp` delegates to host-owned `bunx/npx add-mcp`; on failure it prints manual generic JSON and Codex TOML snippets instead of mutating host config directly (`src/cli/add_mcp.rs:48`).

## Storage And I/O

- SQL DDL and named queries live in `src/storage/queries.rs`; Rust storage modules call constants or build chunked multi-value statements from table-specific column counts.
- Schema v12 stores embeddings as `FLOAT8(384)` and all vector writes/reads use `vector8(?)`; generic `vector(?)` is not used for the FLOAT8 column (`src/storage/queries.rs:30`, `src/storage/segments.rs:816`).
- Schema changes that alter storage format bump `SCHEMA_VERSION` and require explicit rebuild/reindex rather than in-place migration (`src/shared/constants.rs`, `src/storage/schema.rs:132`).
- Project DB connections use tuned local PRAGMAs on write/indexing paths (`WAL`, `synchronous=NORMAL`, cache, mmap, memory temp store) (`src/storage/db.rs:76`).
- Segment, vector, symbol, relation, and indexed-file manifest updates share transactional replace seams and rollback together (`src/storage/segments.rs:342`, `src/storage/segments.rs:740`).
- File prefiltering uses `indexed_files` size/mtime metadata first, then content hash as the correctness backstop; full deletion detection unions manifest and segment paths (`src/indexer/pipeline.rs:355`).
- External HTTP I/O is wrapped in purpose-specific adapters with timeouts and verified artifacts for model/update/install flows (`src/indexer/embedder.rs`, `scripts/install/setup.sh`).

## Concurrency

- Runtime code is async over Tokio/libSQL at CLI, daemon, MCP, search, and storage entry points (`src/main.rs:14`, `src/mcp/server.rs:51`).
- Indexing parallelizes parsing with `JoinSet::spawn_blocking`, then preserves deterministic storage order through a sequence-id reorder buffer (`src/indexer/pipeline.rs:1303`).
- Embedding and storage stay bounded: embeddings are batched after parse readiness, and DB writes are chunked by SQLite parameter limits (`src/indexer/pipeline.rs:618`, `src/storage/queries.rs:499`).
- Daemon worker uses `tokio::select!` for signals, search requests, registry reload, and watcher debounce; request concurrency is capped by a semaphore and bridged via mpsc/oneshot (`src/daemon/worker.rs:81`).
- Cross-process coordination uses flock-style locks for daemon PID, registry writes, startup guard, and one MCP instance per project (`src/daemon/lifecycle.rs`, `src/daemon/registry.rs`, `src/cli/mcp.rs`).
- Impact expansion is intentionally bounded and sequential with small per-hop budgets rather than broad parallel graph traversal (`src/search/impact.rs:26`).

## Configuration And Dependency Boundaries

- There is no DI container; dependencies are passed explicitly through constructors and function arguments (`HybridSearchEngine::new`, `SymbolSearchEngine::new`, `OneupMcpServer::new`).
- Config path helpers are centralized under `shared::config`; state root and source root are separated for worktree support (`src/shared/config.rs`, `src/shared/project.rs:183`).
- Persisted daemon registry entries can carry indexing config; command/env overrides are resolved at call time rather than embedded in long-lived globals (`src/daemon/registry.rs:16`, `src/shared/config.rs:106`).
- Extension surfaces are static: clap subcommands are enum variants, MCP tools are rmcp macro-registered methods, and there is no dynamic plugin loader (`src/cli/mod.rs:39`, `src/mcp/tools.rs:37`).

## Observability

- Logs use `tracing` with verbosity from global `-v`; tracing writes to stderr to keep stdout protocols clean (`src/main.rs:18`).
- User-facing degradation and version-skew warnings use stderr; stable command data stays on stdout (`src/cli/search.rs:51`, `src/cli/search.rs:77`).
- Index progress is persisted as `.1up/index_status.json` and can also stream over watch renderers; progress includes optional timing, scope, prefilter, and parallelism fields (`src/indexer/pipeline.rs:15`, `src/cli/output.rs`).
- Release/security workflows emit JSON evidence artifacts with command, status, excerpts, and validation summaries (`scripts/security_check.sh`, `scripts/release/generate_release_evidence.sh`).
- No metrics backend or distributed tracing is present; observability is logs, stderr notices, progress JSON, eval reports, and release evidence.

## Testing And Evals

- Unit tests sit in module `#[cfg(test)]` blocks and prefer in-memory DB fixtures plus explicit `schema::initialize` (`src/storage/segments.rs`, `src/storage/schema.rs`).
- CLI/integration tests use `assert_cmd`, temp repos/homes, real binary execution, and JSON output for maintenance assertions (`tests/cli_tests.rs`, `tests/integration_tests.rs`).
- MCP tests drive the stdio protocol directly, assert tool schemas, structured envelopes, canonical next actions, and host-facing instructions (`tests/integration_tests.rs:52`, `tests/integration_tests.rs:1239`).
- Script tests are black-box and fixture-driven: setup.sh is tested through a local HTTP server and PATH shim rather than script modification (`tests/setup_script_tests.rs`).
- Evals parse lean CLI output and use anchor-based gold `{file, symbol}` / `{file, line_contains}` instead of fragile segment hashes (`evals/suites/1up-search/recall.ts`).
- Benchmarks are regression guardrails for search, impact, vector index size, and parallel indexing rather than only performance snapshots (`benches/search_bench.rs`, `scripts/benchmark_vector_index_size.sh`).
