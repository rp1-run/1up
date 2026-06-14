---
scope: kbRoot
path_pattern: "patterns.md"
producer: knowledge-base
type: document
description: "Implementation patterns, coding conventions, and idioms for a single-project codebase. Hard limit of 150 lines."
strictness: strict
---
# Implementation Patterns

**Project**: 1up
**Last Updated**: 2026-06-15

## Naming & Organization

**Files**: `snake_case` modules inside layer dirs (`src/cli`, `src/daemon`, `src/indexer`, `src/mcp`, `src/search`, `src/shared`, `src/storage`). New CLI commands are wired via `mod.rs` exports plus a clap `Command` enum arm, never a runtime registry. MCP splits by file role: schema inputs in `types.rs`, operation adapters in `ops.rs`, rmcp `#[tool]` wrappers in `tools.rs`, stdio server in `server.rs`.
**Functions**: CLI command modules use `<Name>Args` + `pub async fn exec(...)`; maintenance commands take `OutputFormat`, core/discovery commands own fixed output and reject `--format`. Verb-prefixed helpers: `resolve_`, `classify_`, `ensure_`, `read_`, `format_`, `apply_`.
**Imports**: explicit `crate::...` paths grouped after std/external blocks. Shared literals/tunables live in `src/shared/constants.rs` and are imported, never re-hardcoded per call site.

Evidence: `src/cli/mod.rs:62`, `src/mcp/tools.rs:35`, `src/shared/constants.rs`

## Type & Data Modeling

**Data Representation**: Owned structs/enums with serde derives. MCP inputs derive `Deserialize + JsonSchema`; payloads derive `Serialize`. `Value` fields that must stay host-compatible get explicit object schemas via `schema_with = json_object_schema`. Single source of truth via const arrays, e.g. `RETAINED_PUBLIC_TOOLS: [&str; 9]`.
**Type Strictness**: Strict typed enums for status/role/state with explicit serde `rename_all = "snake_case"` wire names (`OperationStatus`, `ReadinessStatus`, `ReadStatus`). `usize`<->`i64` conversions go through saturating helpers (`usize_from_i64`).
**Immutability**: Defaults via `#[derive(Default)]` + `#[serde(default)]`; compatibility aliases via `#[serde(alias)]` (`StartMode` auto->index_if_needed, `ImpactInput` segment_id->handle).

Evidence: `src/mcp/types.rs:18`, `src/mcp/ops.rs:49`, `src/shared/update.rs:18`

## Error Handling

**Strategy**: Two-layer split. CLI/MCP boundaries use `anyhow::Result` + `bail!`/`Context`; library layers (storage/search/indexer/daemon/update) use `OneupError` with per-domain `thiserror` enums. Advisory failures are typed envelope statuses (refused/empty/degraded/blocked), not process errors.
**Propagation**: Stale/incompatible indexes fail closed: `schema::ensure_current` returns a reindex-required error with version detail (v{found} vs v{expected}). Transient DB locks retried via generic `retry_on_db_lock` bounded by `DB_LOCK_RETRY_ATTEMPTS`.
**Common Types**: `OneupError`, `StorageError`, `anyhow::Error`

Evidence: `src/shared/errors.rs`, `src/storage/schema.rs:154`, `src/mcp/ops.rs:871`

## Validation & Boundaries

**Location**: Concentrated at clap parsing, MCP input schemas, filesystem gates, and schema-readiness seams. MCP input structs use `#[serde(deny_unknown_fields)]`. Empty-string/empty-collection args rejected early with a structured error + canonical `next_action`.
**Method**: Impact requires exactly one anchor and limits `line` to file anchors in both CLI and MCP. Repo paths are canonicalized and clamped to `source_root`; absolute and parent-escape paths outside the repo become `Rejected` records; 1-based lines enforced.
**Normalization**: Secure dir/file creation rejects symlink components and unexpected leaf types; repo-relative paths normalized to forward slashes to match indexed paths.

Evidence: `src/mcp/types.rs:41`, `src/mcp/ops.rs:1060`, `src/shared/fs.rs:62`

## Observability

**Logging**: `tracing` to stderr, verbosity from global `-v` count; stdout reserved for protocol/data so MCP stdio stays clean. MCP instructions kept under a 2KB budget with a routing substring guaranteed to survive truncation.
**Metrics**: None detected — no metrics backend.
**Tracing**: None detected. Observability is logs, stderr notices, per-context progress JSON (`.1up/index_status.json`), and release/security JSON evidence artifacts.

Evidence: `src/cli/mod.rs:55`, `src/mcp/server.rs:82`, `src/mcp/ops.rs:40`

## Testing Idioms

**Organization**: Unit tests in module `#[cfg(test)]` blocks; integration/CLI tests under `tests/`. Release/script behavior is black-box: tests spawn release shell scripts and a stdio MCP fixture binary via `std::process::Command`, asserting on emitted JSON evidence.
**Fixtures**: Storage tests use a real libSQL DB in a tempdir + explicit `schema::initialize`; in-memory `Db::open_memory` available. Drift/version guards source the expected value from `CARGO_PKG_VERSION`, never a literal, so a normal bump stays green.
**Levels**: Behavioral guards over snapshots: `documentation_tool_names_match_retained_public_tools` scans docs for `oneup_*` tokens and asserts each is in `RETAINED_PUBLIC_TOOLS`; `committed_update_manifest_version_matches_binary` pins manifest version to the binary.

Evidence: `tests/release_assets_tests.rs:884`, `tests/release_assets_tests.rs:709`, `src/cli/mod.rs:281`

## I/O & Integration

**Database**: libSQL via a `Db` wrapper; write/index connections get tuned PRAGMAs (WAL, `synchronous=NORMAL`, `cache_size`, `mmap_size`, `temp_store=MEMORY`) applied with lock-retry. All counts/reads are context-scoped (`count_files_for_context`, `*_for_context`). Embeddings stored as `FLOAT8(384)` matching `EMBEDDING_DIM=384`; vector writes/reads use `vector8(?)` and `vector_distance_cos`, not generic `vector(?)`.
**HTTP Clients**: External HTTP confined to model-download and update adapters with explicit connect/total timeouts.
**Resilience**: Downloaded artifacts verified against pinned SHA-256 digests and recorded in a verified-artifact manifest before use.

Evidence: `src/storage/db.rs:91`, `src/storage/queries.rs:57`, `src/indexer/embedder.rs:50`

## Concurrency & Async

**Async Usage**: Async over Tokio/libSQL at every entry point (main, CLI, daemon, MCP, search, storage); MCP stdio server runs via rmcp `serve(stdio())`.
**Parallelism**: Indexing parses in parallel with `JoinSet::spawn_blocking` then reorders deterministically. Daemon worker multiplexes signals/requests/reload/debounce with `tokio::select!` and caps in-flight work with a `Semaphore`.
**Safety**: DB lock contention handled by bounded retry + sleep, never busy-spin; per-request engines hold no shared mutable state.

Evidence: `src/indexer/pipeline.rs:1573`, `src/daemon/worker.rs:115`, `src/mcp/ops.rs:871`

## Dependency & Configuration

**DI Pattern**: No DI container; dependencies passed explicitly via constructors/args (`HybridSearchEngine::new_scoped`, `SymbolSearchEngine::new_scoped`, `OneupMcpServer::new`). Engines are constructed per-request from an opened connection + a `SearchScope` derived from the `WorktreeContext`.
**Config Loading**: Path helpers centralized in `shared::config`; `state_root` vs `source_root` separated for worktree support. Indexing config resolved at call time from CLI -> env -> persisted registry -> defaults; non-positive numeric values rejected.
**Initialization**: Embedding runtime is lazily prepared and gated on a cheap vector-presence check, so the model is never loaded when a context holds no embeddings.

Evidence: `src/mcp/ops.rs:642`, `src/mcp/ops.rs:928`, `src/cli/mod.rs:261`

## Extension Mechanisms

**Plugin Pattern**: Extension surfaces are static, not plugin loaders: clap subcommands are enum variants and MCP tools are rmcp `#[tool_router]` macro methods. A named authority list (`RETAINED_PUBLIC_TOOLS`) is the single source of truth referenced everywhere — next-action construction `debug_assert!`s every emitted tool is in it, the doctor hint classifier treats any `oneup_*` token absent from it as stale, and doc/test guards pin to it.
**Hook System**: Agent guidance lives only in MCP (server instructions + tool descriptions + `next_actions`); 1up never writes to user instruction files. The sole opt-in mutation (`doctor --clean-hints --apply`) is default-OFF, preview-by-default, and removes only a byte-exact 1up-owned `<!-- 1up:hint:begin --> / <!-- 1up:hint:end -->` HTML-comment fence via `atomic_replace_within_project_root`.

Evidence: `src/mcp/tools.rs:1171`, `src/cli/hint_cleanup.rs:216`, `src/cli/doctor.rs:76`
