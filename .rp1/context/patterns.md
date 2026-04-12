# Implementation Patterns

**Project**: 1up
**Last Updated**: 2026-04-12

## Naming & Organization

**Files**: snake_case.rs, one module per concern, grouped by layer (`cli/`, `daemon/`, `indexer/`, `search/`, `storage/`, `shared/`)
**Functions**: snake_case verbs; CLI handlers use `async fn exec(args, format) -> anyhow::Result<()>`; constructors use `new()`, `from_dir()`, `open_rw()`/`open_ro()`; config resolution uses `from_sources()`
**Imports**: Absolute paths (`crate::shared::types::...`), grouped std -> external -> internal, no wildcard imports
**Constants**: `SCREAMING_SNAKE_CASE`, centralized in `shared/constants.rs`; env var names as `pub const &str`; pinned SHA-256 digests as const for artifact verification
**Types**: PascalCase; CLI args as `<Subcommand>Args`; engines as `<Feature>Engine`; IPC messages as `<Noun>Request`/`<Noun>Response`

Evidence: `src/shared/constants.rs`, `src/cli/mod.rs`, `src/daemon/search_service.rs:29-41`

## Type & Data Modeling

**Data Representation**: Plain structs with pub fields; serde Serialize/Deserialize on API-facing and persisted types; `#[serde(skip_serializing_if = "Option::is_none")]` for optional fields; private inner structs for pipeline work items and verified artifact metadata
**Type Strictness**: Strong typing with dedicated enums (SegmentRole, QueryIntent, RetrievalMode, OutputFormat, IndexState, IndexPhase, EmbeddingLoadStatus, EmbeddingUnavailableReason, FenceAction); `FromStr` impls for CLI-facing enums; custom Deserialize impls for validation-on-load
**Immutability**: Structs mutable by default (pub fields); small enums are Copy; Clone derived broadly
**Nullability**: `Option<T>` for nullable fields; `some_if_not_empty(Vec<T>) -> Option<Vec<T>>` helper pattern

Evidence: `src/shared/types.rs:1-98`, `src/indexer/embedder.rs:180-194`, `src/shared/reminder.rs:24-29`

## Error Handling

**Strategy**: Two-tier: thiserror for domain errors (`OneupError` with nested per-module enums), `anyhow::Result` at CLI boundary
**Propagation**: Domain modules return `Result<T, OneupError>` with `#[from]` conversions; CLI uses `bail!()` for user-facing errors; `eprintln!("warning: ...")` for degraded-mode warnings; `debug_assert!()` for internal invariants
**Common Types**: OneupError, StorageError, IndexingError, SearchError, EmbeddingError, ParserError, DaemonError, ConfigError, FilesystemError, ProjectError, FenceError, UpdateError
**Recovery**: Embedding failures -> FTS-only search; vector retrieval failures -> FTS fallback within same query; tree-sitter failures -> text chunking; download failures -> marker file to prevent automatic retry; daemon unavailability -> local execution fallback; `UpdateError::should_invalidate_cache()` for selective cache purge on permanent vs transient errors

Evidence: `src/shared/errors.rs`, `src/search/hybrid.rs:39-51`, `src/indexer/embedder.rs:396-427`

## Validation & Boundaries

**Location**: CLI boundary (clap derive), config resolution entry points, query execution entry points, and filesystem security boundaries
**Method**: clap derive for args; manual guards at function entry; schema version validation before DB read/write; `IndexingConfig::new()` validates all fields > 0; `read_positive_env()` rejects zero and non-numeric env vars; IPC `sanitize_request()` canonicalizes paths and clamps limits
**Filesystem Security**: Approved-root validation pattern: all file operations require an approved_root parameter; every path component checked via `symlink_metadata()`; `normalize_absolute()` resolves `../` before I/O; distinct leaf-type enforcement via `ExpectedLeaf` enum (RegularFile vs Socket)
**Early Rejection**: `bail!()` for missing index, schema mismatch; `FilesystemError::OutsideApprovedRoot` and `FilesystemError::SymlinkComponent` for path escapes

Evidence: `src/shared/config.rs:64-134`, `src/shared/fs.rs:56-136`, `src/daemon/search_service.rs:193-218`

## Observability

**Logging**: tracing crate with `debug!`/`info!`/`warn!`/`error!` macros; `-v`/`-vv` CLI flag for verbosity; debug-level for skipped files, cache hits, fallback decisions; info-level for pipeline summaries and project lifecycle; warn-level for degraded functionality
**Progress**: `IndexProgress` snapshots persisted to `.1up/index_status.json`; human, plain, and JSON formatters render work summaries, effective parallelism, and per-stage timings; nanospinner for interactive CLI progress

Evidence: `src/daemon/worker.rs:236-321`, `src/cli/output.rs`

## Testing Idioms

**Organization**: Unit tests in `#[cfg(test)]` blocks inside modules; integration tests in `tests/`; Criterion benchmarks in `benches/`; repo-scale benchmark scripts in `scripts/`; search quality evals in `evals/`
**Fixtures**: `setup() -> (Db, Connection)` using `Db::open_memory()`; `tempfile::tempdir()` for filesystem and daemon-socket tests; helper builders (`make_result`, `test_segment`); RAII guards: `EnvGuard` for env vars (save/restore via Drop), `HideModelGuard` for model visibility (rename-based); `static Mutex` for env-var test isolation in config tests; search_service tests use `bind_test_listener` with direct socket paths for env-free isolation
**Levels**: Unit coverage for config resolution, warm embedding reuse, secure filesystem ops (symlink rejection, atomic replace, path clamping), IPC framing (round-trip, oversized, timeout), fence parsing and idempotency, IPC backward compat (`daemon_version` present/absent round-trip); integration for symbol lookup, scoped-run fallback, schema rebuild; benchmarks for candidate-first retrieval and warm-query workloads
**Patterns**: `#[tokio::test]` for async; `cfg(unix)` guards on symlink/socket tests for cross-platform CI; parity tests compare jobs=1 vs parallel runs; daemon request tests treat unavailability as fallback path

Evidence: `src/shared/fs.rs:480-697`, `src/daemon/ipc.rs:144-276`, `src/daemon/search_service.rs:245-395`, `tests/cli_tests.rs`

## I/O & Integration

**Database**: libsql via `Db` wrapper; SQL as `pub const &str` in `storage/queries.rs`; async row iteration with column extraction by index; schema versioning via meta table; transactional batch writes; `segment_symbols` stores raw + canonical symbols for exact-first lookup
**IPC**: Length-prefixed JSON frames over Unix domain socket; 4-byte big-endian length header + JSON payload; bounded frame sizes (16KB request, 2MB response, 4KB query); 250ms read/write deadlines; same-UID peer auth via `stream.peer_cred()`; semaphore-based in-flight limiting (8 slots); socket bound with 0o600 permissions; `daemon_version` field in `SearchResponse` with `#[serde(default, skip_serializing_if)]` for backward-compatible IPC evolution
**HTTP Clients**: reqwest with connect/total timeouts for model downloads and update manifest fetches; streaming byte transfer; no retry for HTTP (only DB lock contention)
**File I/O**: ignore crate for gitignore-aware scanning; SHA-256 hashing for incremental detection, artifact verification, and update binary verification; `atomic_replace` for crash-safe writes (temp file + fsync + rename + dir sync) used by index state, update cache, and daemon heartbeat

Evidence: `src/daemon/ipc.rs:20-89`, `src/daemon/search_service.rs:36-47`, `src/shared/update.rs:237-280`, `src/shared/fs.rs:90-136`

## Concurrency & Async

**Async Usage**: Tokio full runtime; all CLI handlers and DB operations async; bounded `JoinSet::spawn_blocking` parse pool capped at `config.jobs`; embedder inference stays synchronous; storage writes remain serialized
**Patterns**: Sequence IDs + BTreeMap reorder buffer for deterministic file order; `write_batch_files` controls transactional batch size; `ProjectRunState` enforces one active run + burst collapse; Semaphore admission control for daemon search; oneshot channels for request-response plumbing between connection tasks and search handler; daemon heartbeat uses throttled periodic persistence (`DAEMON_FILE_CHECK_PERSIST_INTERVAL_MS`) with force bypass for immediate writes; `main.rs` spawns background `tokio::spawn` for update cache refresh, awaits with bounded timeout after command completes
**Warm Cache**: `EmbeddingCompatibilityKey` (model_dir + file fingerprints + embed_threads) keys `WarmRuntime<T>` generic cache; cache-on-match avoids redundant ONNX reload; `cache.clear()` on any error ensures no stale state
**Config Resolution**: Three-tier priority: CLI flags -> env vars -> registry persisted config -> auto defaults

Evidence: `src/indexer/embedder.rs:131-240`, `src/daemon/worker.rs:18-53`, `src/daemon/worker.rs:440-497`, `src/main.rs:32-51`

## Verified Artifact Lifecycle

**Pattern**: Download to `.staging/<artifact_id>/` -> verify SHA-256 against pinned constants -> write `VerifiedArtifactManifest` to `verified/<artifact_id>/manifest.json` -> move files from staging -> atomically switch `current.json` pointer -> legacy flat-file artifacts auto-migrated on first access after digest validation -> cleanup on failure (remove_dir_all on staging)

Evidence: `src/indexer/embedder.rs:64-109`, `src/shared/constants.rs:107-134`

## Update Lifecycle

**Passive Notification**: `main.rs` spawns background cache refresh via `tokio::spawn`, shows `eprintln` notice after command completes; suppressed for JSON output, Worker, and Update commands
**Cache**: 24h TTL (`UPDATE_CHECK_TTL_SECS`), version-pinned (discards cache from different binary version), `atomic_replace` writes with secure permissions, non-fatal failures throughout (debug-level logging only)
**Self-Update**: `detect_install_channel` via resolved binary path heuristics (Homebrew/Scoop/Manual/Unknown); channel-managed installs show upgrade instruction; manual installs do streaming binary download with SHA-256 verification; `stop_daemon_for_update` with bounded polling (30 x 100ms) via `lifecycle::is_process_alive`

Evidence: `src/main.rs:30-51`, `src/shared/update.rs:297-320`, `src/cli/update.rs:93-195`

## Agent Instruction Pattern

**Pattern**: Compile-time `CONDENSED_REMINDER` via `include_str!("../reminder.md")`; versioned fence markers (`<!-- 1up:start:VERSION -->`) for idempotent injection into AGENTS.md/CLAUDE.md; `hello-agent` subcommand emits reminder in all formats; fence operations handle create/update/already-current with malformed detection; coexists with other tools' fences by matching only 1up-prefixed markers

Evidence: `src/shared/reminder.rs:1-154`, `src/cli/hello_agent.rs`, `src/shared/constants.rs:143`

## Conditional Compilation

**Pattern**: `cfg(unix)`/`cfg(not(unix))` guards for platform-specific daemon modules (lifecycle, search_service, worker, watcher, ipc vs stubs); `cfg(unix)` test guards for symlink/socket tests; `cfg(not(windows))`/`cfg(windows)` for ort linking strategy; filesystem permission operations are no-ops on non-Unix

Evidence: `src/daemon/mod.rs`, `src/shared/fs.rs:387-432`, `Cargo.toml` target-specific dependencies
