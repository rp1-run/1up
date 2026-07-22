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
| `src/cli` | Command surface, lean/human/json output (+ stale-branch disclosure hint, `IndexState::Failed` rendering), doctor hint cleanup, MCP launch/setup; one-shot index/reindex (build-aside) and daemon start; build-identity search handshake; `start --scope` + monorepo gate-before-index (+ startup-guard lock self-heal); `gc` three-state source-presence pruning; `stop` deleted-path fallback; torn-read-tolerant status reads |
| `src/mcp` | rmcp stdio server: nine tools over a pure ops layer with structured envelopes, single-sourced guidance, degraded/stale-rebuild readiness; facts-envelope scope suggestions, `get` verbosity + hard batch caps, handle recovery, multi-query search, instance-lock self-heal |
| `src/search` | Hybrid candidate-first retrieval, token-level query-intent classification, scope, symbol, context, structural, impact, read-only overview; exact vector scan only (DiskANN removed) |
| `src/indexer` | Scan (exclusive scope cones via `ScanFilter`, 19-pattern secret set), parse/chunk, markdown doc-sections, embed (content-addressed pool), metadata-prefiltered pipeline (connection-agnostic: live or staging); per-file size/segment caps |
| `src/storage` | libSQL persistence (schema v20: scope meta, embedding pool, **no DiskANN index** — exact `vector_distance_cos` is the only vector path) + build-aside `swap` primitives (no vector-index build step); tolerant read validation; stale-branch snapshot predicate + disclosure stats |
| `src/daemon` | Background index/search service with secure Unix IPC; lifecycle: build-identity handshake drain/restart, single-writer rebuild lock, cooperative cancellation, idle self-exit, deleted-directory deregistration (three-state probe), registry EntryIdentity dedup, startup stale-branch auto-prune, gate parity + cached gate file count (non-Unix stubs) |
| `src/shared` | Cross-cutting (938-line constants module): types, constants (BUILD_IDENTITY, envelope budgets, lock-reap/disclosure/GC-autoprune tunables), secure fs (+ three-state `SourcePresence`), project/worktree identity (reverse-pointer verification), errors, progress (three-state status reads), config, self-update trust pipeline, opportunistic lock-file reaper |
| `tests` / `benches` / `evals` / `scripts` | Black-box CLI/MCP/daemon/release regression tests (+ `build_identity_tests.rs` stamping a throwaway probe crate with the real `build.rs`); Criterion guards; contract-hashed "Luna" eval harness with per-axis baselines; installer + release/security scripts + `justfile` |

## Key Components

- **`storage::swap::StagingRebuild`** *(revised)* — RAII build-aside guard; `open` calls plain `schema::initialize` (no more `VectorIndexBuild::Deferred`), `finalize_and_swap` folds WAL + atomically renames over `index.db` under the `RebuildLock` and **no longer builds any vector index** — the deferred-DiskANN machinery was deleted wholesale (tests renamed from deferred/immediate to staged/in-place terminology).
- **`shared::lock_reap`** *(new, ~920 lines)* — opportunistic best-effort reaping of provably-stale per-project lock files (`mcp-{key}.lock`, `startup-{key}.lock`; issue #117 saw 4076 accumulated). Single namespace authority (`project_lock_key` SHA-256 32-hex key, `lock_file_name`, `is_reapable_name` as sole parser); pure candidate selector (name + mtime age `LOCK_REAP_MAX_AGE_SECS`=7d + bounded max-heap of `LOCK_REAP_MAX_CANDIDATES_PER_RUN`=128); `#[cfg(unix)]` impure driver: non-blocking flock probe, dev/ino identity re-verify immediately before unlink, `LOCK_REAP_TIME_BUDGET_MS`=250ms wall-clock budget.
- **`shared::fs::SourcePresence` / `probe_source_presence`** *(new)* — three-state presence probe: `NotFound`/`NotADirectory` → Absent, any other error → Indeterminate; backs `DaemonError::SourceProbeIndeterminate` so deleted-directory reclamation and `gc` defer instead of false-pruning a live source on a flaky mount.
- **`shared::progress::StatusFileRead` / `read_status_file`** *(new)* — generic three-state (Absent/Parsed/Unreadable) status-file reader with `StatusReadError{Io,Parse}`; `cli::project_status_files::read_status_for_display` retries Unreadable (`STATUS_READ_RETRY_ATTEMPTS`=3 × 50ms) then error-logs and returns None.
- **`daemon::registry::EntryIdentity`** *(new)* — narrows entry identity to (project_root, source_root, branch_ref), excluding head_oid/context_id (issue #116's duplicate-per-HEAD-advance root cause); `load_from_path_with_repair` → `collapse_duplicate_entries` (most-recent survivor absorbs durable `indexing` config) persisted best-effort under the registry lock; `deregister_context_ids_if` re-validates liveness under the lock before removal.
- **`daemon::worker` gate + prune** *(refined)* — `count_files_gitignore_aware` reuses the MCP `is_under_vcs_dir`/`build_vcs_aware_walker` exclusion (gate parity); `ProjectState.cached_gate_file_count` caches the gated walk (cleared on SIGHUP registry reload; `FIRST_INDEX_GATE_BLOCKED_COUNT_TTL_MS`=60s); `prune_stale_branch_contexts_on_startup` conservatively auto-prunes stale-branch snapshots of live worktrees whose branch is gone (shared `segments::is_stale_branch_snapshot` + extra gates), narrower than `1up gc --apply`.
- **`daemon::lifecycle`** — `RebuildLock` (bounded-wait / non-blocking try), `drain_daemon` (SIGTERM+poll), `drain_and_restart_daemon`; `spawn_daemon` threads `source_root` into `__worker` argv.
- **`cli::search`** — `try_daemon_search` returns `(results, daemon_version, degraded_reason)`; `daemon_response_is_authoritative` gates on full `BUILD_IDENTITY` equality (absent stamp = refused).
- **`cli::gc`** *(refined)* — `prune_reason` matches on `SourcePresence`; only definite Absent prunes as `SourceMissing`; Indeterminate warns and retains; also sweeps orphaned staging DBs and stale-branch snapshots.
- **`cli::mcp` / `cli::start` lock self-heal** *(new)* — `acquire_mcp_instance_lock_in` / `acquire_project_startup_guard_in` call `reap_stale_locks` at mint time and verify `flock_still_names_path` post-flock (reaper race), re-acquiring on mismatch (`LOCK_ACQUIRE_IDENTITY_RETRIES`).
- **`cli::output`** *(refined)* — `disclosure_hint` (stale-context count + reclaimable bytes, floors-gated, identical across human/plain/JSON via `StatusInfo`/`ProjectListItem` fields); `render_index_state_{human,plain}` add `IndexState::Failed` → red "failed".
- **`mcp::ops` batch + recovery surface** *(new)* — `check_get_handles_cap` (`MAX_GET_HANDLES_PER_CALL`=50, `MAX_GET_REQUEST_HANDLE_BYTES`=16KiB) enforced before index open; 2MiB response budget metered in input order; `attempt_handle_recovery`/`recover_handle_by_unique_prefix` + process-global `FailedHandleMemory`; `merge_multi_query_results` (RRF fusion across `queries`, ≤`MAX_SEARCH_QUERIES`=4); `PersistentDensityCache`/`PersistedScopeProposal` + `is_scope_proposal_fresh` (walk-cache-keyed freshness); `corrected_impact_call` tool-argument self-correction.
- **`search::ranking::classify_query_token`** *(new)* — per-token Neutral/Identifier/Prose classes feeding `is_natural_language_query` (≥2 significant terms, ≥2 prose words, prose > identifier), replacing the blunt no-underscore/no-uppercase/no-digit heuristic.
- **`mcp::server` / `mcp::types`** — `serve_stdio`; single-sourced `SERVER_GUIDANCE` (≤2KB); `deny_unknown_fields` inputs; `RETAINED_PUBLIC_TOOLS: [&str; 9]`; `ToolEnvelope { status, summary, data, next_actions }` with `TruncationNote`/recovery-call compaction discipline.
- **`shared::update`** — `ensure_manifest_acceptable` (anti-rollback + expiry), `verify_archive_checksum` (floor), `verify_artifact_attestation` (three-state), atomic `replace_binary`; its private `UPDATE_ENV_MUTEX` was deleted — env-mutating tests now serialize on the single `shared::fs::ENV_MUTEX` (lock order: MODEL_MUTEX → ENV_MUTEX).
- **`shared::fs`** — the only sanctioned write/rename/unlink path: `atomic_replace`, `atomic_replace_within_project_root`, `atomic_rename_file_within_root`; root-clamped + symlink-rejecting.
- **`storage::schema`** — `SCHEMA_VERSION=20` (v20 drops the DiskANN index + shadow table; v19 indexes fail closed → `1up reindex`); `initialize` (plain, no vector-index variant), `ensure_current`, scope meta read/write; `ensure_current_tolerating_init` now has its own budget (`SCHEMA_INIT_WAIT_ATTEMPTS`=50 × `SCHEMA_INIT_WAIT_DELAY_MS`=100ms ≈ 5s, distinct from the ~450ms DB-lock-retry budget).
- **`shared::project`** *(refined, e8ea203)* — `.1up` ancestors anchor resolution only with a valid marker; `resolve_linked_worktree_info` (+~237 lines) adds anchoring (`git_dir` parent must be `<commondir>/worktrees/`) + reverse-pointer verification (`<git_dir>/gitdir` must canonicalize back to the exact `.git` file) before adopting `main_root`/`state_root`; fallback `.1up` ancestor → git root → project root.
- **`shared::constants`** *(grew ~510 → 938 lines)* — new: `BUILD_IDENTITY` (from `build.rs`); MCP budgets (`MAX_SEARCH_QUERIES`=4, `MAX_GET_HANDLES_PER_CALL`=50, `MAX_GET_REQUEST_HANDLE_BYTES`, `MAX_GET_RESPONSE_BYTES`, `SUMMARY_MAX_BYTES`); truncation bounds (`MAX_SYMBOLS_PER_LIST`=20, `MAX_CONTEXT_EXPANSION_LINES`=500, `MAX_WHOLE_SCOPE_LINES`=101, `MAX_RECOVERY_ACTIONS`=3); gate cache TTL (`FIRST_INDEX_GATE_BLOCKED_COUNT_TTL_MS`=60s); GC/disclosure (`GC_STALE_BRANCH_AUTOPRUNE_MAX_AGE_DAYS`=30, `DISCLOSURE_*_FLOOR`); status-read retry (`STATUS_READ_RETRY_ATTEMPTS`=3/50ms); schema-init budget (50×100ms); lock-reap tunables. Removed: `VECTOR_EXHAUSTIVE_SCAN_MAX_VECTORS` (→ `VECTOR_EXACT_SCAN_WARN_THRESHOLD`=262144, warn-only), `VECTOR_PREFILTER_CONTEXT_SCALE_LIMIT`.
- **`build.rs`** *(new, ~273 lines)* — stamps `ONEUP_BUILD_IDENTITY` (`{semver}+{git-short}[.dirty[.digest]]`, 8-hex digest of `git diff HEAD`; `+unknown` without git); exercised by `tests/build_identity_tests.rs` under bare edits and packed-ref-only worktrees.
- **`evals/suites/shared`** *(new subsystem)* — `manifest.ts` freezes prompts/fixtures/grader/axis-mapping into a `contract_hash`; `axes-report.ts` computes independent per-axis scores (factual/retrieval/adoption/reliability + efficiency measures); baselines (`luna-baseline.json`) are stamped with the hash so runs are only comparable against a matching contract — replacing a single blended promptfoo score.

## Dependencies

- **Direction (no cycles):** `cli`/`mcp` → `search`/`indexer` → `storage` → `shared`; everything depends on `shared`.
- `cli::mcp`/`cli::start` → `shared::lock_reap` (single lock-filename grammar shared by creators and reaper).
- daemon reclamation + `cli::gc` → `shared::fs::probe_source_presence` / `DaemonError::SourceProbeIndeterminate` (three-state destructive-prune guard).
- `cli::search` + `daemon::worker` → `shared::constants::BUILD_IDENTITY` (handshake) + `daemon::lifecycle` (drain/restart; lock probe).
- daemon gate → `mcp::ops::{is_under_vcs_dir, build_vcs_aware_walker}` (single-sourced walk exclusion, gate parity).
- `cli` read cmds + `mcp/ops` warm path → `storage::schema::ensure_current_tolerating_init`.
- `cli::status`/`list` → `storage::segments::disclosure_stats` / `is_stale_branch_snapshot` (shared with daemon startup auto-prune).
- `storage/swap` → `storage::schema::initialize` (plain) + `shared::fs`; debug-asserts the caller holds the `RebuildLock`.
- `indexer` + MCP `context` reads → `shared::constants::DEFAULT_SECRET_GLOBS` (19 patterns) + size/segment caps.
- `shared/update` → `shared::fs` (incl. `ENV_MUTEX` for tests), `sigstore_verify`, `sigstore_trust_root`, `reqwest`, `sha2`.
- `evals/suites/shared/manifest.ts` → `axes-report.ts` (GRADED_AXES/EFFICIENCY_MEASURES inside the contract hash).
- **External:** rmcp, libsql, ort + tokenizers, tree-sitter grammars, sigstore-verify/-trust-root, reqwest+sha2 (sha2 also for lock keys), notify+nix, tokio/tokio-util, clap.

## Metrics (LOC, approx.)

| Module | Files | LOC |
|---|---:|---:|
| `src/shared` | 11 | ~11.1k (constants.rs 938, lock_reap.rs ~920) |
| `src/search` | 12 | ~10.4k |
| `src/indexer` | 7 | ~10.1k |
| `src/cli` | 23 | ~9.5k |
| `src/storage` | 7 | ~9.0k (swap.rs 896) |
| `src/daemon` | 10 | ~5.8k |
| `src/mcp` | 5 | ~4.0k |
| `evals` | 20 changed | axes-report.ts +450, assertions +376, manifest.ts +183 |

Largest single files: `search/impact.rs`, `storage/segments.rs`, `shared/constants.rs`.

## Cross-Module Patterns

- **Build-aside rebuild under a single-writer lock** — `cli/reindex` + `mcp/ops` + `storage/swap` + `daemon/lifecycle` + `shared/fs`: readers keep a full valid index; a failed/cancelled rebuild leaves the prior intact.
- **DiskANN removal / exact-scan-only vector search** *(supersedes prior KB)* — the "defer DiskANN build to finalize_and_swap" pattern is REMOVED, not refined: schema v20 drops the index entirely, `swap` builds no vector index, and the old size-based path selector became a warn-only threshold. Measurements in `docs/diskann-removal.md`.
- **Build-identity handshake** — `build.rs` + `daemon` + `cli` + `shared`: responses carry a git-stamped `daemon_version`; mismatch or absence → drain+restart, local fallback. No cross-build corruption.
- **Provably-safe opportunistic cleanup** *(new)* — `shared::lock_reap` generalizes "never destructively act on ambiguous evidence" to lock files: age + flock probe + post-lock dev/ino re-check before any unlink; bounded by count and wall-clock.
- **Three-state presence over boolean existence** *(new)* — `SourcePresence` (fs), `StatusFileRead` (progress), and gc/daemon consumers: destructive decisions distinguish "definitely gone" from "could not tell".
- **Identity-keyed idempotent registration** *(new)* — registry entries keyed by stable `EntryIdentity`; every load self-heals duplicates (issue #116).
- **Gate parity + cached gate walks** *(new)* — daemon and MCP share the VCS-aware walk exclusion; each caches its count so gated repos aren't rewalked per dirty signal.
- **Stale-but-available degraded search** — in-flight rebuild → `STALE_REBUILD_REASON` on `degraded_reason`, combined not replaced.
- **Tolerant read validation** — reads racing fresh-DB init retry the init window (now ≈5s budget) instead of a spurious "reindex required".
- **Monorepo file-count gate + facts envelope** — `cli/start` + `mcp/ops` + `daemon/worker`: over-threshold unscoped first index refused with ranked scope suggestions; explicit `--scope` bypasses.
- **Independent-channel self-update trust** — anti-rollback/expiry gate → checksum floor → attestation (verified/degrade/fail-closed); installer now also fail-closed (`ONEUP_SKIP_CHECKSUM` opt-out).
- **Single source of truth for tool identity + guidance** — `RETAINED_PUBLIC_TOOLS` + `SERVER_GUIDANCE` reused by doctor + drift tests; lock-filename grammar single-sourced in `lock_reap`.
- **Contract-hashed eval baselines** *(new)* — Luna manifest freezes the eval contract; per-axis baselines only compare against a matching hash, preventing silent cross-contract comparisons.
- **Secure, root-clamped atomic state writes** — `shared/fs` backs every on-disk mutation; `ensure_project_gitignore` is the only `.1up/.gitignore` writer.

## Boundaries

- **`src/cli`** — visible: `start/status/list/stop/get/symbol/context/impact/doctor`; hidden but callable: `add-mcp/init/search/structural/mcp/index/reindex/update/__worker`. `start` gains `--scope` (gate) + startup-guard self-heal; `gc` prunes only on definite absence; read commands use the tolerant schema path.
- **`src/mcp`** — `serve_stdio(state_root, source_root)`; nine tools behind `ToolEnvelope`; `oneup_get` batch caps enforced pre-I/O; `degraded_reason` rides the payload only.
- **`src/storage`** — schema v20; exact `vector_distance_cos` is the only vector path (`VECTOR_EXACT_SCAN_WARN_THRESHOLD` is advisory-only, not a path selector); v19 indexes fail closed requiring `1up reindex`; rebuilds go through `swap` (never in-place drop).
- **`src/daemon`** — `lifecycle::{acquire_rebuild_lock, try_acquire_rebuild_lock, drain_daemon, drain_and_restart_daemon, ensure_daemon}`; framed-JSON IPC with build-identity `daemon_version`; registry dedup on load; impact/overview run locally, not over IPC.
- **`src/shared`** — secure fs is the only sanctioned write/rename/unlink; `lock_reap` is the sole authority for which lock-file names may ever be deleted; `probe_source_presence` is the sole sanctioned absence check for destructive prunes; constants: `SCHEMA_VERSION=20`, `BUILD_IDENTITY`, `FILE_COUNT_THRESHOLD=3000`, `DEFAULT_SECRET_GLOBS` (19), `MAX_FILE_SIZE_BYTES=2MB`, `MAX_SEGMENTS_PER_FILE=1000`, `MAX_GET_HANDLES_PER_CALL=50`, `VECTOR_EXACT_SCAN_WARN_THRESHOLD=262144`, `DAEMON_IDLE_SHUTDOWN_SECS=60`, `DAEMON_DRAIN_TIMEOUT_MS=3000`, `REBUILD_LOCK_CONTENTION_TIMEOUT_MS=5000`, `STALE_REBUILD_REASON`, lock-reap/disclosure/autoprune tunables.
