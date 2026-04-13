# Module & Component Breakdown

**Project**: 1up
**Analysis Date**: 2026-04-13
**Modules Analyzed**: 10

## Core Modules

### CLI (`src/cli/`)
**Purpose**: User-facing command parsing and output formatting via clap derive
**Files**: 15 | **Lines**: ~4,252

**Components**:
- **Cli** (`mod.rs`): Top-level CLI struct with `Command` enum dispatch, global `--format` (default plain) and `--verbose` flags, `parse_positive_usize` validator for concurrency flags
- **SearchArgs** (`search.rs`): Hybrid search with `--limit` and auto-daemon-start; tries daemon socket first with 250ms timeout, falls back to local; returns `(results, daemon_version)` tuple and emits version-mismatch warning when CLI and daemon versions differ
- **SymbolArgs** (`symbol.rs`): Symbol lookup with optional `--references` flag
- **ContextArgs** (`context.rs`): Context retrieval for `file:line` locations with `--allow-outside-root` flag and ContextAccessScope tracking
- **ImpactArgs** (`impact.rs`): Exact-one-anchor parser for `--from-file`, `--from-symbol`, or `--from-segment`; clamps depth/limit, opens the current index read-only, and dispatches local-only Impact Horizon exploration
- **StructuralArgs** (`structural.rs`): AST pattern search using tree-sitter S-expression queries
- **IndexArgs** (`index.rs`): Explicit indexing with `--jobs`, `--embed-threads`, EmbeddingRuntime-based model management, progress spinners
- **ReindexArgs** (`reindex.rs`): Force clear + full re-index with schema rebuild and `--jobs`/`--embed-threads` flags
- **StartArgs** (`start.rs`): Auto-init project, install fenced agent reminders, index with config, register project in registry, spawn daemon or SIGHUP existing
- **HelloAgentArgs** (`hello_agent.rs`): Output condensed agent instruction (CONDENSED_REMINDER) in plain/json/human formats
- **UpdateArgs** (`update.rs`): Self-update command with `--check` (force fresh manifest fetch), `--status` (display cached update info), and bare `1up update` (refresh-if-stale then apply or print channel instruction); detects InstallChannel (Homebrew/Scoop/Manual/Unknown), stops daemon before binary replacement via `stop_daemon_for_update` with bounded poll
- **Formatter** (`output.rs`): Output formatting trait with JSON, Human, and Plain implementations; progress-aware rendering with parallelism, timing breakdown, and relative "time ago" timestamps; `StatusInfo` includes project_initialized, index health (present/readable), and `last_file_check_at` fields; `UpdateStatusInfo` struct and `UpdateResult` enum with `format_update_status`/`format_update_result` trait methods; search plain/json output can expose additive `segment_id` handles, and impact output renders expanded/scoped/refused envelopes with reasons and hints

**Dependencies**: search, indexer, storage, daemon, shared

### Search (`src/search/`)
**Purpose**: Search engines: hybrid semantic+FTS, symbol lookup, structural AST queries, context retrieval, and bounded advisory impact exploration
**Files**: 10 | **Lines**: ~4,678

**Components**:
- **HybridSearchEngine** (`hybrid.rs`): Shared execution path for local CLI and daemon-backed search; builds symbol variants, embeds queries when available, executes candidate-first fusion, and degrades per query to FTS-only when embedding/vector work fails
- **ImpactHorizonEngine** (`impact.rs`): Local-only advisory expansion from exact file, symbol, or segment anchors; refuses broad symbols with narrowing hints, traverses persisted relations plus same-file/test heuristics, and emits bounded expanded/scoped/refused envelopes
- **RetrievalBackend** (`retrieval.rs`): Backend selection (SqlVectorV2 or FtsOnly) based on index state; fetches vector and FTS `CandidateRow` sets first, then leaves segment hydration until after ranking
- **SymbolSearchEngine** (`symbol.rs`): Exact-first definition and reference lookup over canonicalized symbols stored in `segment_symbols`; prefix/contains fallback seeds fuzzy matching only when exact lookup misses
- **ContextEngine** (`context.rs`): Source context retrieval using tree-sitter scope detection with line-range fallback; supports ContextAccessScope for inside/outside project root tracking
- **StructuralSearchEngine** (`structural.rs`): Tree-sitter S-expression queries across indexed files; fallback to directory scan
- **IntentDetector** (`intent.rs`): Signal-based query classification into Definition, Flow, Usage, Docs, General
- **Ranking** (`ranking.rs`): RRF fusion plus intent/query/path/content boosts, short-segment penalties, overlap dedup, and per-file caps before final hydration
- **Formatter** (`formatter.rs`): Search result formatting utilities

**Dependencies**: storage, indexer (parser, scanner), shared

### Indexer (`src/indexer/`)
**Purpose**: Bounded staged indexing: scan, parse, embed, and single-writer persistence
**Files**: 6 | **Lines**: ~6,680

**Components**:
- **Pipeline** (`pipeline.rs`): Config-aware orchestrator accepting `IndexingConfig` for jobs/embed_threads/write_batch_files plus `RunScope`; prepares scoped or full run inputs, falls back safely when watcher paths affect ignore semantics, uses bounded `spawn_blocking` parse workers with sequence IDs, flushes through a `BTreeMap` reorder buffer, batches embeddings through one ONNX session, and persists `IndexProgress`/`IndexParallelism`/`IndexStageTimings` to `.1up/index_status.json`
- **Parser** (`parser.rs`): Multi-language AST parsing via tree-sitter; 16 language grammars; role classification and symbol collection
- **Embedder** (`embedder.rs`): ONNX engine (all-MiniLM-L6-v2) with configurable intra-op `embed_threads`, verified artifact download and activation (`verified/<artifact-id>/`, `current.json`, `.staging/`), legacy-cache import only after digest validation, batch inference, mean pooling, L2 normalization, and warm runtime reuse keyed by model fingerprints plus thread count via EmbeddingRuntime
- **Scanner** (`scanner.rs`): Directory walking via ignore crate with .gitignore respect and binary filtering
- **Chunker** (`chunker.rs`): Sliding-window text chunking (60-line window, 10-line overlap) for unsupported languages flushed through the same staged pipeline

**Dependencies**: storage, shared

### Storage (`src/storage/`)
**Purpose**: Database access layer using libSQL with FTS5, vector indexing, relation persistence, and transactional file replacement helpers
**Files**: 6 | **Lines**: ~2,751

**Components**:
- **Db** (`db.rs`): Database connection wrapper with `open_rw`/`open_ro`/`open_memory` constructors, lock retry
- **Schema** (`schema.rs`): Schema initialization, validation, vector-model compatibility checks, and rebuild with recovery guidance for schema v8, including `segment_relations`
- **Relations** (`relations.rs`): Relation-row construction plus bounded outbound and inbound relation lookup helpers; transactional replace/delete helpers keep relation rows aligned with segment writes
- **Segments** (`segments.rs`): Segment CRUD, bulk file-hash preload, deleted-file cleanup, transactional replacement helpers with configurable batch size, and maintenance of canonical symbol rows in `segment_symbols` plus synchronized relation cleanup
- **Queries** (`queries.rs`): SQL DDL and query constants for segments, FTS, `segment_symbols`, `segment_relations`, meta, bulk hash preload, and candidate-first vector/FTS retrieval

**Dependencies**: shared

### Daemon (`src/daemon/`)
**Purpose**: Background daemon for file watching, scoped incremental re-indexing, daemon-backed search, persisted per-project indexing settings, and file-check heartbeat persistence; platform-conditional with Unix-only implementations and cross-platform stubs
**Files**: 10 | **Lines**: ~2,810

**Components**:
- **Worker** (`worker.rs`): Main event loop with `tokio::select!`, per-project `ProjectRunState` (one active + one queued via dirty flag), scoped `RunScope::Paths` scheduling, burst-collapsing follow-up scheduling, shared config resolution, semaphore-bounded daemon search handling, a warm `EmbeddingRuntime` cache per project, and throttled file-check heartbeat persistence (`last_file_check_persisted_at` per project, `DAEMON_FILE_CHECK_PERSIST_INTERVAL_MS` = 30s) via `record_file_check`/`persist_daemon_project_status` triggered on startup, SIGHUP, and each event loop tick
- **IPC** (`ipc.rs`): Length-prefixed JSON frame helpers with same-UID peer checks, bounded request/response sizes (16KB/2MB), and 250ms read/write deadlines
- **Lifecycle** (`lifecycle.rs`): Start/stop/ensure daemon with secure PID management, stale detection, SIGHUP signaling, and `send_sigterm`/`is_process_alive` helpers for pre-update daemon shutdown; cross-platform stubs for non-Unix (`lifecycle_stub.rs`)
- **Watcher** (`watcher.rs`): Filesystem event monitoring via notify crate with debounce and non-blocking drain support
- **Registry** (`registry.rs`): Project registration with optional persisted `IndexingConfig` in a JSON-based project list written through atomic, approved-root filesystem helpers
- **Search Service** (`search_service.rs`): Secure Unix domain socket transport for `SearchRequest`/`SearchResponse`; owner-only socket bind, same-UID authorization, request sanitization, and graceful unavailable/busy responses so CLI search can fall back locally; `SearchResponse::Results` now carries `daemon_version` field for version-mismatch detection; `request_search` returns `(Vec<SearchResult>, Option<String>)`; stubs for non-Unix (`search_service_stub.rs`)

**Dependencies**: indexer, search, storage, shared

### Shared (`src/shared/`)
**Purpose**: Cross-cutting types, config resolution, constants, error types, secure filesystem helpers, symbol canonicalization, fenced agent reminder management, project utilities, and self-update infrastructure
**Files**: 11 | **Lines**: ~4,234

**Components**:
- **Types** (`types.rs`): ParsedSegment, SearchResult, SymbolResult, ContextResult, StructuralResult, `IndexingConfig` (with `from_sources` resolution and validation), `IndexProgress`, `IndexParallelism`, `IndexStageTimings`, `IndexState`, `IndexPhase`, OutputFormat, SegmentRole, ReferenceKind, RunScope, ContextAccessScope, `DaemonProjectStatus` (persisted daemon heartbeat with `last_file_check_at`); `SearchResult` now carries an additive optional `segment_id` handoff handle for segment-backed hits
- **Config** (`config.rs`): XDG-compliant paths plus verified model artifact paths (`verified/`, `.staging/`, `current.json`) and `resolve_indexing_config` with priority: CLI > env > registry > defaults; `read_positive_env` for env var validation; `update_check_cache_path()` for update cache and `project_daemon_status_path()` for per-project daemon heartbeat file
- **Constants** (`constants.rs`): Tuning constants, watcher debounce, env var names, daemon IPC limits and deadlines, secure filesystem modes, verified artifact metadata, fence target files, embedding/search limits, daemon file-check heartbeat interval (`DAEMON_FILE_CHECK_PERSIST_INTERVAL_MS`), and update-related constants: `UPDATE_MANIFEST_URL_ENV_VAR`, `UPDATE_CHECK_CACHE_FILENAME`, `UPDATE_DISABLED_MESSAGE`, `UPDATE_CHECK_TTL_SECS` (24h), `UPDATE_CHECK_TIMEOUT_SECS`, `UPDATE_CHECK_CONNECT_TIMEOUT_SECS`, `UPDATE_DOWNLOAD_TIMEOUT_SECS`, `UPDATE_DOWNLOAD_CONNECT_TIMEOUT_SECS`
- **Errors** (`errors.rs`): OneupError hierarchy with thiserror derives covering StorageError, IndexingError, SearchError, EmbeddingError, ParserError, DaemonError, ConfigError, FilesystemError, ProjectError, FenceError, UpdateError; `UpdateError` has 7 variants (Disabled, FetchFailed, ParseFailed, CacheError, SelfUpdateFailed, DaemonStopFailed, NoArtifactForPlatform, ChecksumMismatch) with `should_invalidate_cache()` method
- **Fs** (`fs.rs`): Approved-root filesystem helpers for secure directory creation, atomic replace, root clamping, and typed file/socket cleanup with symlink rejection at every path component
- **Progress** (`progress.rs`): Shared progress snapshot helpers for work counters, effective parallelism, stage timings, and timestamp propagation used by index and status reporting
- **Project** (`project.rs`): Project identity (UUID) and database path resolution backed by secure project-state helpers
- **Reminder** (`reminder.rs`): Versioned fenced agent reminder management for AGENTS.md/CLAUDE.md files; compile-time CONDENSED_REMINDER from `src/reminder.md`; idempotent fence create/update/replace lifecycle
- **Symbols** (`symbols.rs`): Symbol canonicalization helpers (normalize_symbolish) shared by indexing and search paths
- **Update** (`update.rs`): Complete self-update system (~1,633 lines): `UpdateManifest` (version, git_tag, artifacts, channels, yanked, minimum_safe_version, message), `UpdateArtifact` (target, archive, sha256, url), `UpdateChannels` (github_release, homebrew_tap/formula, scoop_bucket/manifest), `UpdateCheckCache` (serialized to `update-check.json`), `InstallChannel` enum (Homebrew/Scoop/Manual/Unknown), `UpdateStatus` enum (UpToDate/UpdateAvailable/Yanked/BelowMinimumSafe); functions for manifest fetching (`fetch_update_manifest`, `build_update_check_client`), cache lifecycle (`read_compatible_update_cache`, `write_update_cache`, `refresh_cache_if_stale`, `clear_update_cache`, `build_cache_from_manifest`), channel detection (`detect_install_channel`, `detect_channel_from_path`), status assessment (`build_update_status`), binary download/verify/replace (`self_update`, SHA-256 checksum verification, archive extraction, atomic binary swap), passive notification (`format_update_notification`, `updates_enabled`), and `SelfUpdateResult` return type; `current_target_triple()` maps 5 platform targets

**Dependencies**: None (foundation module)

## Support Modules

### Tests (`tests/`)
**Files**: 6 | **Lines**: ~4,394
- `integration_tests.rs`: End-to-end pipeline and search tests, including exact/canonical/reference symbol acceptance, incremental freshness checks, Impact Horizon file/symbol/refusal flows, and `search -> segment_id -> impact` round trips that prove search top hits stay stable
- `cli_tests.rs`: CLI subcommand tests via assert_cmd including concurrency flag validation, daemon lifecycle behaviors, degraded search coverage, and update command validation
- `rewrite_sql_verification.rs`: Schema rebuild guidance plus add/edit/delete search freshness under degraded FTS-only indexing
- `release_assets_tests.rs`: Release archive, manifest, and evidence validation tests
- `security_check_tests.rs`: Security audit script verification tests
- `license_consistency_tests.rs`: License metadata consistency validation

### Benchmarks (`benches/`)
**Files**: 1 | **Lines**: ~698
- `search_bench.rs`: Criterion benchmarks for exact/partial symbol lookup, chunked-content retrieval, candidate-first backend selection, hybrid fusion, and Impact Horizon file-anchor/narrow-symbol/refused-symbol workloads used to guard the feature's interactive latency posture

### Scripts (`scripts/`)
**Files**: 16 | **Lines**: ~2,550
- `benchmark_parallel_indexing.sh`: Hyperfine benchmarks for full reindex, scoped follow-up, and write-heavy follow-up indexing
- `benchmark_rewrite_sql.sh`: Baseline-vs-candidate benchmark evidence generator for SQL rewrite work
- `security_check.sh`: Security audit wrapper
- `scripts/release/`: 12 release pipeline scripts covering packaging, evidence generation, manifest rendering, archive verification, and metadata validation

### Evals (`evals/`)
**Files**: 16 | **Lines**: ~3,613
- `suites/1up-search/evals.yaml`: Search quality evaluation definitions comparing 1up-backed vs baseline agent search
- `suites/1up-search/search-bench.ts`: TypeScript search benchmark harness
- `suites/shared/assertions/`: Shared assertion library for eval suites
- `run-parallel.sh`: Parallel eval execution runner
- `summary.sh`: Eval result aggregation

## Module Dependencies

```mermaid
graph TD
    Main[main] --> CLI[cli]
    Main --> Shared[shared]
    CLI --> Search[search]
    CLI --> Indexer[indexer]
    CLI --> Storage[storage]
    CLI --> Daemon[daemon]
    CLI --> Shared
    Daemon --> Search
    Daemon --> Indexer
    Search --> Storage
    Search --> Indexer
    Search --> Shared
    Indexer --> Storage
    Indexer --> Shared
    Daemon --> Storage
    Daemon --> Shared
    Storage --> Shared
```

## Module Metrics

| Module | Files | Lines | Components | Avg File Size |
|--------|-------|-------|------------|---------------|
| cli | 15 | 4,252 | 12 | 283 |
| search | 10 | 4,678 | 9 | 468 |
| indexer | 6 | 6,680 | 5 | 1,113 |
| storage | 6 | 2,751 | 5 | 459 |
| daemon | 10 | 2,810 | 6 | 281 |
| shared | 11 | 4,234 | 10 | 385 |
| tests | 6 | 4,394 | 6 | 732 |
| benches | 1 | 698 | 1 | 698 |
| scripts | 16 | 2,550 | 15 | 159 |
| evals | 16 | 3,613 | 5 | 226 |

## Cross-Module Patterns

- **Layered Architecture**: CLI -> Search/Indexer -> Storage -> Shared; strict dependency hierarchy
- **Scoped Follow-Up Indexing**: Daemon watcher events become `RunScope::Paths`; the indexer scans only changed files/deletions unless safety rules force a full scan
- **Staged Pipeline with Bounded Parallelism**: Pipeline uses bounded spawn_blocking parse workers, sequence-ID reorder buffer, single embed session, and transactional writer
- **Adaptive Writer Batching**: `write_batch_files` scales with configured jobs but is capped to the amount of work ready for each run
- **Layered Config Resolution**: IndexingConfig resolved via priority chain: CLI flags > env vars > registry > computed defaults
- **Progress Persistence**: Pipeline persists IndexProgress to `.1up/index_status.json` at phase transitions; status reads them back
- **Daemon-Backed Warm Search Reuse**: CLI search prefers daemon socket requests so repeated searches can reuse the daemon's warm embedding runtime
- **Exact-First Symbol Retrieval**: Storage persists canonical symbol rows and search only widens into prefix/contains/fuzzy matching after exact misses
- **Candidate-First Hybrid Retrieval**: Search ranks vector/FTS/symbol candidates before hydrating final segment bodies by ID
- **Impact Horizon Read Path**: The CLI-only `impact` command resolves exact file/symbol/segment anchors, expands through persisted relation rows plus same-file/test heuristics, and returns bounded advisory results without changing daemon IPC
- **Additive Search Handoff**: Search hydrates segment-backed hits with optional `segment_id` handles in machine-readable output so agent loops can pass exact anchors into `impact --from-segment` without altering search ranking or candidate selection
- **Transactional Relation Maintenance**: Storage replaces `segment_relations` in the same transaction as segment writes and file deletes so impact reads always see relation rows aligned with the indexed content
- **Verified Artifact Activation**: Embedder only activates model artifacts after staged writes, digest validation, manifest persistence, and atomic `current.json` replacement
- **Secure State Lifecycle**: Shared filesystem helpers enforce approved roots, owner-only permissions, atomic replacement, and typed cleanup for daemon and project state
- **Platform-Conditional Compilation**: Daemon modules use cfg(unix)/cfg(not(unix)) to swap real implementations with stub modules
- **Fenced Agent Reminder Management**: Start command installs versioned 1up instruction fences in AGENTS.md/CLAUDE.md; fences are idempotent and coexist with other tools' fences
- **Graceful Degradation**: Missing embedder degrades hybrid search to FTS-only with user warnings; daemon unavailability triggers local search fallback
- **Auto-Start Daemon**: Search commands auto-start daemon via `lifecycle::ensure_daemon()`; start auto-inits project if needed
- **SIGHUP Reload**: Daemon handles SIGHUP to reload registry and per-project settings without restart
- **Passive Update Notification**: `main.rs` spawns a background `refresh_cache_if_stale()` task alongside the primary command; after command completion, `format_update_notification()` emits a one-line stderr notice when an update is available; suppressed for JSON output, internal Worker, and the Update command itself
- **Daemon Heartbeat Persistence**: Worker persists `DaemonProjectStatus` (last_file_check_at) to `.1up/daemon_status.json` on startup, SIGHUP, and each event loop tick, throttled to one write per 30 seconds per project; status command reads this file to display daemon liveness to users
- **Version Mismatch Detection**: Daemon search responses carry `daemon_version`; CLI search compares it against the binary's `VERSION` and warns when they differ, prompting `1up stop` to restart under the current binary
- **Self-Update Lifecycle**: Update command detects install channel, stops daemon before binary replacement with bounded poll, downloads platform-specific archive, verifies SHA-256 checksum, extracts and atomically swaps binary; Homebrew/Scoop channels receive upgrade instructions instead of direct self-update
- **Cache-Gated Update Checks**: Update check results are cached to `update-check.json` with 24h TTL; passive notification and `1up update` use stale-cache refresh to avoid redundant network requests; cache is invalidated on permanent errors or disabled builds

## External Dependencies

| Crate | Version | Purpose | Used By |
|-------|---------|---------|---------|
| clap | 4 | CLI argument parsing (derive) | cli |
| libsql | 0.9 | SQLite with vector + FTS5 (core features only) | storage, search |
| tree-sitter | 0.26 | Multi-language AST parsing | indexer, search |
| ort | 2.0.0-rc.12 | ONNX runtime for embeddings (download-binaries on non-Windows, load-dynamic on Windows) | indexer |
| tokenizers | 0.22 | HuggingFace tokenizer | indexer |
| notify | 7 | Filesystem event watching | daemon |
| ignore | 0.4 | .gitignore-aware directory walking | indexer |
| reqwest | 0.13 | HTTP client for model download and update manifest/binary fetch | indexer, shared (update) |
| sha2 | 0.11 | SHA-256 for incremental detection, artifact verification, and update checksum | indexer, shared (update) |
| semver | - | Semantic version parsing for update status comparison | shared (update) |
| flate2 | - | Gzip decompression for update archive extraction | shared (update) |
| tar | - | Tar archive extraction for update binary retrieval | shared (update) |
| nix | 0.31 | Unix signal/process management | daemon |
| thiserror | 2 | Derive-based error types | shared |
| nanospinner | - | Terminal progress spinners | cli, indexer |
| chrono | - | Timestamp serialization | shared |
| colored | - | Terminal color output | cli |
| uuid | - | Project identity generation | shared |
