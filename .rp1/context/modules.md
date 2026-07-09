---
scope: kbRoot
path_pattern: "modules.md"
producer: knowledge-base
type: document
description: "Module and component breakdown with dependency graphs, metrics, and code quality insights for a single-project codebase."
strictness: strict
---
# Module & Component Breakdown

**Project**: 1up (`oneup`) — single Rust binary, local code-discovery substrate (CLI + MCP + daemon + indexer + storage).

## Modules

| Module | Purpose |
|---|---|
| `src/cli` | Command surface, lean/human/json output, doctor hint cleanup, MCP launch/setup; orchestrates one-shot index/reindex (build-aside) and daemon start; runs the search version-handshake |
| `src/mcp` | rmcp stdio server: nine tools over a pure ops layer with structured envelopes, single-sourced guidance + eligibility notes, verbosity-gated hydration, degraded/stale-rebuild readiness |
| `src/search` | Hybrid candidate-first retrieval, intent, scope, symbol, context, structural, impact, read-only overview |
| `src/indexer` | Scan (exclusive scope cones via `ScanFilter`), parse/chunk (incl. Rust field/enum-variant doc comments), markdown doc-sections, embed (content-addressed pool), metadata-prefiltered pipeline (connection-agnostic: writes live or staging) |
| `src/storage` | libSQL persistence (schema v19: scope meta `scope_roots_v1`, embedding pool): schema/segments/relations/queries/db + build-aside `swap` primitives |
| `src/daemon` | Background index/search service with secure Unix IPC; lifecycle: version-handshake drain/restart, single-writer rebuild lock, cooperative cancellation, idle self-exit (non-Unix stubs) |
| `src/shared` | Cross-cutting: types, constants, secure fs, project/worktree identity, errors, progress, config, self-update trust pipeline |
| `tests` / `benches` / `evals` / `scripts` | Black-box CLI/MCP/daemon/release tests (16-scenario monorepo E2E incl. field-doc ranking, verbosity gating, eligibility-note pins, timing-hardened daemon-alive gates); Criterion guards; promptfoo evals; benchmarks + installer + release/security + `justfile` (incl. `reap-daemons`) |

## Key Components

- **`storage::swap::StagingRebuild`** *(new)* — RAII build-aside guard; `open` creates the staging DB, `finalize_and_swap` folds WAL + atomically renames over `index.db` (must hold the `RebuildLock`); Drop removes an aborted-rebuild orphan.
- **`daemon::lifecycle`** *(new surface)* — `RebuildLock` (`acquire_rebuild_lock` bounded-wait / `try_acquire_rebuild_lock` non-blocking), `drain_daemon` (SIGTERM+poll, no force-kill), `drain_and_restart_daemon`.
- **`daemon::worker`** — shares one `CancellationToken`; `should_idle_shutdown` self-exit; defers on lock contention; reports `daemon_version` for the handshake.
- **`cli::search`** — `try_daemon_search` returns `(results, daemon_version, degraded_reason)`; `daemon_response_is_authoritative` gates on `VERSION`.
- **`cli::reindex`** — drives `StagingRebuild` + `finalize_and_swap` under the lock (no in-place drop).
- **`cli::doctor` + `cli::hint_cleanup`** *(new)* — opt-in, default-OFF cleanup; pure `classify` (fence detection + advisory unfenced tokens vs `RETAINED_PUBLIC_TOOLS`); `--apply` removes only a 1up-owned fence.
- **`mcp::server` / `mcp::ops`** — `serve_stdio`; single-sourced `SERVER_GUIDANCE` (≤2KB); `run_index` rebuild branch uses `StagingRebuild`; `rebuild_in_progress` flags stale-but-available via `degraded_reason`.
- **`mcp::types`** — `deny_unknown_fields` inputs, `RETAINED_PUBLIC_TOOLS: [&str; 9]` (single source), `ToolEnvelope { status, summary, data, next_actions }`.
- **`shared::update`** *(new surface)* — `ensure_manifest_acceptable` (anti-rollback + expiry), `verify_archive_checksum` (floor), `verify_artifact_attestation` (three-state), atomic `replace_binary`; advisory `build_update_status`.
- **`shared::fs`** — the only sanctioned write/rename/unlink path: `atomic_replace`, `atomic_replace_within_project_root`, `atomic_rename_file_within_root` (backs the swap); root-clamped + symlink-rejecting.
- **`storage::schema`** — `SCHEMA_VERSION=19`; `initialize` (staging, `VectorIndexBuild::Deferred`), `ensure_current` (readers, fail-closed), `read_scope_from_meta`/`write_scope_to_meta` (scope persistence), version/objects/vector/context validation.
- **`indexer::scan_filter::ScanFilter`** *(v0.1.13)* — pure `is_excluded(rel_path, is_dir)` predicate shared by scanner, `oneup_context`, and the watcher; precedence secrets > scope_globs (exclusive cone) > include_globs/overrides > excludes > dotfiles; `with_scope_globs` constructor for scoped indexing.
- **`mcp::ops` scope surface** *(v0.1.13)* — `should_return_facts_envelope`/`generate_facts_envelope` (gate), `compute_new_scope`/`apply_scope_to_indexing_config` (scope ops), `determine_rebuild_mode_for_scope` (narrow=atomic, first-scope=rebuild, widen=incremental), `spawn_rebuild_task` + `start_response_budget` (non-blocking start), `compute_index_scope` (coverage, context-id-aware), persistent + in-process walk/density caches keyed on (repo identity, HEAD, mtime) — cache writes never create `.1up`.
- **`daemon::worker` gate + scope** *(v0.1.13)* — `gate_allows_first_index` (pure, unit-tested; first-index = segments==0, robust to eagerly-created empty schema DB); gated projects consume the pending run and idle; refresh applies scope from meta with progress-file fallback and re-persists it; gate walk runs on `spawn_blocking` with 100-entry cancellation checks.
- **`shared::project`** *(v0.1.13)* — `.1up` ancestors anchor resolution only with a valid marker (`index.db`/`project_id`/`rebuild.lock`); `launch_subdir` captured before clamping and threaded to the facts envelope.
- **`mcp::ops` envelope quality** *(PR #84, daddb79)* — `unscoped_eligibility_note()` single source for the `index_scope` note (status/readiness + search + facts paths); `segment_record()` verbosity gating (`is_verbose`: symbol lists only at `"full"`, `summary` always omitted; `#[serde(skip)] symbol_hint` captured pre-gating drives the `oneup_symbol` next_action in `tools.rs::read_next_actions`); `generate_ranked_suggestions()` (≤3, rank-truthful ordinals, primary phrasing on emptiness — never a leading "Or"); tool dot-dirs excluded from `generate_facts_envelope()` stats.
- **`indexer::parser` field/variant extraction** *(PR #84)* — Rust `field_declaration` + `enum_variant` in `nested_kinds()` flow through the generic body walk (`body` field; a `field_declaration_list` lookup was dead code and is removed); variants → block_type `variant`, role Definition, name in `defined_symbols`, breadcrumb = parent type.

## Dependencies

- **Direction (no cycles):** `cli`/`mcp` → `search`/`indexer` → `storage` → `shared`; everything depends on `shared`.
- `cli/hint_cleanup` → `mcp::types::RETAINED_PUBLIC_TOOLS` (single source for the doctor staleness rule).
- `cli/reindex` + `mcp/ops` → `storage::swap` + `daemon::lifecycle` (rebuild via staging under the lock).
- `cli/search` + `daemon/worker` → `daemon::lifecycle` (handshake drain/restart; lock probe).
- `storage/swap` → `shared::fs` (atomic rename/cleanup) and debug-asserts the caller holds the `RebuildLock`.
- `shared/update` → `shared::fs`, `sigstore_verify`, `sigstore_trust_root`, `reqwest`, `sha2`.
- **External:** rmcp, libsql, ort + tokenizers, tree-sitter grammars, sigstore-verify/-trust-root, reqwest+sha2, notify+nix, tokio/tokio-util, clap.

## Metrics (LOC, approx.)

| Module | Files | LOC |
|---|---:|---:|
| `src/cli` | 23 | ~9.2k |
| `src/search` | 12 | ~10.4k |
| `src/indexer` | 7 | ~10.1k |
| `src/storage` | 7 | ~8.5k |
| `src/shared` | 10 | ~6.5k |
| `src/daemon` | 10 | ~5.5k |
| `src/mcp` | 5 | ~3.7k |

Largest single files: `search/impact.rs`, `storage/segments.rs`.

## Cross-Module Patterns

- **Build-aside rebuild under a single-writer lock** — `cli/reindex` + `mcp/ops` + `storage/swap` + `daemon/lifecycle` + `shared/fs`: readers keep a full valid index; a failed/cancelled rebuild leaves the prior intact; no orphan-WAL replay.
- **Daemon version handshake** — `daemon` + `cli` + `shared`: responses carry `daemon_version`; mismatch → drain+restart, local fallback. No cross-version corruption.
- **Stale-but-available degraded search** — `mcp` + `daemon` + `shared`: in-flight rebuild → `STALE_REBUILD_REASON` on `degraded_reason`, combined not replaced.
- **Independent-channel self-update trust** — `shared/update`: anti-rollback/expiry gate → checksum floor → attestation (verified/degrade/fail-closed).
- **Single source of truth for tool identity + guidance** — `RETAINED_PUBLIC_TOOLS` + `SERVER_GUIDANCE` reused by doctor + drift tests.
- **Secure, root-clamped atomic state writes** — `shared/fs` backs every on-disk mutation (`.1up` + project-root clamps, symlink-rejecting).
- **CLI/MCP dual surface over shared engines + connection-agnostic indexing** — one indexing/search contract; the pipeline writes through a caller-supplied connection (live or staging).

## Boundaries

- **`src/cli`** — visible: `start/status/list/stop/get/symbol/context/impact/doctor`; hidden but callable: `add-mcp/init/search/structural/mcp/index/reindex/update/__worker`. `doctor` is the only user-instruction-file writer (opt-in, `--apply`, fence-only).
- **`src/mcp`** — `serve_stdio(state_root, source_root)`; nine tools behind `ToolEnvelope`; `degraded_reason` rides the payload only.
- **`src/storage`** — schema v19; rebuilds go through `swap` (never in-place drop); switch-over writes clamp to `.1up` under the `RebuildLock`; scope meta re-persisted to staging by every rebuild path.
- **`src/daemon`** — `lifecycle::{acquire_rebuild_lock, try_acquire_rebuild_lock, drain_daemon, drain_and_restart_daemon, ensure_daemon}`; framed-JSON IPC with `daemon_version`; impact/overview run locally, not over IPC.
- **`src/shared`** — secure fs is the only sanctioned write/rename/unlink; `UpdateError`/`DaemonError`/`IndexingError::Cancelled`; constants (`SCHEMA_VERSION=19`, `FILE_COUNT_THRESHOLD=3000` + env override, `STALENESS_THRESHOLD_SECS=300`, `ATTESTATION_*`, `DAEMON_IDLE_SHUTDOWN_SECS=60`, `DAEMON_DRAIN_TIMEOUT_MS=3000`, `REBUILD_LOCK_CONTENTION_TIMEOUT_MS=5000`, `VECTOR_EXHAUSTIVE_SCAN_MAX_VECTORS=262144`, `STALE_REBUILD_REASON`, `DISABLE_MODEL_DOWNLOADS_ENV_VAR`).
