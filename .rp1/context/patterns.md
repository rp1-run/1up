---
scope: kbRoot
path_pattern: "patterns.md"
producer: knowledge-base
type: document
description: "Implementation patterns, coding conventions, and idioms for a single-project codebase. Hard limit of 150 lines."
strictness: strict
---
# Implementation Patterns

Conventions and idioms for 1up (`oneup`). Prior patterns confirmed/refined against the MCP-envelope hardening, lock-reaping, and ranking-intent work (8bfa534..04e6663); new material folded in below.

## Naming & Organization

- `snake_case` modules inside layer dirs (`src/{cli,daemon,indexer,mcp,search,shared,storage}`). New CLI commands wire through `cli/mod.rs` + a clap `Command` enum arm — never a runtime registry.
- CLI command modules use `<Name>Args` + `pub async fn exec(...)`. Maintenance commands accept `OutputFormat`; core/discovery commands own fixed output and reject `--format`.
- Verb-prefixed helpers (`resolve_`/`classify_`/`ensure_`/`build_`) and RAII openers (`acquire_`/`try_acquire_`/`open_`). New: `deregister_context_ids_if`, `classify_source_missing_contexts`, `stale_branch_autoprune_context_ids` extend the same verb vocabulary.
- Shared literals live in `src/shared/constants.rs` (grew substantially this window) and are imported, never re-hardcoded — even env-var *names* are constants (`ONEUP_*`). Confirmed: new budget constants (`MAX_GET_HANDLES_PER_CALL`, `MAX_CONTEXT_EXPANSION_LINES`, `MAX_SYMBOLS_PER_LIST`) follow the same single-source rule.

## Type Modeling

- Owned structs/enums with serde derives; MCP inputs derive `Deserialize + JsonSchema` (`deny_unknown_fields`), `rename_all="snake_case"` wire names.
- Single source of truth via const arrays (`RETAINED_PUBLIC_TOOLS: [&str; 9]`, `NON_EMBEDDABLE_CHUNK_LANGUAGES`).
- Domain outcomes are dedicated enums (`UpdateStatus`, `ReadinessStatus`, new `QueryTokenClass`), often a three-state collapsed onto `Result` (new: `StatusFileRead<T>` = `Absent | Parsed(T) | Unreadable(err)` — never silently defaults a torn/missing file).
- Additive fields stay back-compatible: `#[serde(default, skip_serializing_if = "Option::is_none")]` (e.g. `ReadinessPayload::scope_proposal`, `ReadRecord::recovered_from`, `SegmentRecord::truncation`/`symbol_counts`). RAII guards consumed by value (`finalize_and_swap(self)`); `RebuildLock` is `#[must_use]`.
- **Load-bearing truncation contract:** `TruncationNote` (reason + scope/symbol counts + a `RecoveryCall{tool, arguments}`) is attached whenever content is bounded — `None` means nothing was omitted, never inferred from payload shape. `RecoveryCall.tool` must be a `RETAINED_PUBLIC_TOOLS` member (debug-asserted) and its `arguments` shape matches `NextAction::arguments` verbatim so it can be re-issued without reshaping.

## Error Handling

- **Two layers:** CLI/MCP boundaries use `anyhow` + `bail!`/`Context`; libraries use `OneupError` with per-domain `thiserror` enums (`UpdateError`, `DaemonError`, `StorageError`; new `StatusReadError{Io, Parse}` in `shared::progress`).
- **Actionable wording:** `DrainTimeout` → "run `1up stop` then retry"; schema errors name found-vs-expected versions + the worktree path.
- **Three outcome shapes:** verified → `Ok`; disproved → hard fail leaving state untouched (`ensure_manifest_acceptable`, `AttestationFailed`); cannot-run → degrade to a safe floor.
- **Cooperative cancellation** is its own typed outcome (`IndexingError::Cancelled`), leaving the context dirty for a later pass. Rebuild runs now carry a process-unique `run_id` (`next_rebuild_run_id`, pid + atomic counter) stamped into published progress so a superseded run can't be confused with the current one.
- **Stat before you lock:** the daemon stats `source_root` *before* acquiring the rebuild lock; if it is gone, `deregister_deleted_project` unwatches + deregisters and returns `Ok(PipelineStats::default())` — never fatal. An independent stat is required because a linked worktree's `state_root` can outlive its `source_root`; a mid-pass lock error re-checks the same race before propagating.
- **Best-effort side work never blocks the primary op:** `.1up/.gitignore` creation, deleted-source cleanup, and the new opportunistic lock reaper all `warn!`/`debug!` and continue on failure rather than failing project resolution, indexing, or CLI startup.

## Validation

- Concentrated at clap parsing, MCP input schemas, filesystem gates, and schema-readiness seams.
- **Decision gates are pure**, taking inputs as parameters (`ensure_manifest_acceptable`, `should_idle_shutdown`, `gate_allows_first_index`, `recover_handle_by_unique_prefix`, `classify_source_missing_contexts`, `stale_branch_autoprune_context_ids`) — deterministic + unit-testable.
- **Recovery and retry-rejection are pure gates too:** unique-prefix handle recovery walks lengths from `len(supplied)` down to `MIN_HANDLE_RECOVERY_PREFIX_CHARS`(8) over context-scoped candidates; exactly one match at a length yields `Found` (disclosed via `recovered_from`), 2+ yields `Ambiguous`, a floor miss or limit-saturated fetch (`HANDLE_RECOVERY_CANDIDATE_LIMIT` 32) declines. Failed-handle retry rejection keys on index identity via a bounded (`FAILED_HANDLE_MEMORY_CAP` 128, oldest-`seq` eviction) `OnceLock<Mutex<..>>` map; an identity mismatch drops the entry, `Found` clears it, a transient `Error` is never remembered.
- **MCP tool-argument self-correction:** `tools.rs` normalizes ambiguous caller inputs before dispatch rather than rejecting them — `corrected_impact_call`/`impact_path_as_file_anchor` promote a file-looking symbol or a relative path slot to a proper file anchor; `normalize_repo_scope`/`looks_like_file_path` classify free-text inputs. Correction is itself pure and unit-tested per input shape.
- **Query-intent classification is token-level, not string-level:** `classify_query_token` buckets each case-preserved word into `Neutral | Identifier | Prose` (underscore/digit mix/CamelCase-split/long-all-caps → Identifier; short acronyms and Capitalized words → Prose); `is_natural_language_query` requires ≥2 prose words strictly outnumbering identifier words — replaces the old blanket "any uppercase/digit/underscore disqualifies" rule so proper nouns and acronyms in prose queries still get NL ranking treatment.
- Repo paths canonicalized + clamped to `source_root`; out-of-root/parent-escape → `Rejected`; 1-based lines enforced. Scope roots validated on construction (repo-relative, no `../`).
- **Gate before index:** readiness/threshold decisions are pure boolean helpers evaluated *before* any indexing work; a Missing readiness now carries a `ScopeProposalSummary` (ranked top-level dirs, human suggestions) rebuilt from the daemon-persisted proposal so `oneup_status` and a follow-up unscoped `oneup_start` both surface actionable `scope_add` next_actions instead of a generic refusal.
- **Filter precedence is a documented contract** (`ScanFilter`): secrets > scope_globs (exclusive cone) > include_globs/override dirs > excludes > dotfile hiding. The secret tier (`DEFAULT_SECRET_GLOBS`, 19 patterns) is enforced on both the scan path and the MCP read path; configured includes/overrides can never punch through it.

## Output Contracts

- `tracing` to stderr; **stdout reserved for protocol/data**; notices/banners go to stderr.
- Degraded/stale wording centralized in single-source `*_REASON` constants (new: `SCOPE_TRUNCATION_REASON`, `SYMBOL_LIST_TRUNCATION_REASON`) merged via `combine_degraded_reasons`.
- MCP `instructions` fit a ~2KB host-truncation budget with the routing rule front-loaded.
- **Response-size budgets are enumerated constants, enforced at the boundary:** `MAX_GET_RESPONSE_BYTES`, `MAX_GET_REQUEST_HANDLE_BYTES`, `MAX_HANDLE_ECHO_BYTES`, `MAX_GET_HANDLES_PER_CALL` bound `oneup_get` batches; `clamp_summary_bytes` truncates search summaries on a char boundary rather than mid-UTF8. `lean_ready_status` strips build telemetry from a terminal `IndexProgress` but preserves it while running — payload shape is state-dependent by design, not an oversight.

## Storage / I-O

- libSQL via a `Db` wrapper; tuned PRAGMAs applied with lock-retry; reads context-scoped (`*_for_context`).
- **Ride out the schema-init window:** read paths call `ensure_current_tolerating_init`, retrying only the transient "tables present, `schema_version` absent" shape (10 attempts, 50 ms, ~450 ms total); a genuine version mismatch fails fast. `SCHEMA_VERSION` stays 19.
- **Generalized three-state status-file read (`shared::progress::read_status_file`)** is the same idiom now factored out for reuse beyond the schema window: one read + one parse attempt, `Absent`/`Parsed`/`Unreadable` — never a silent default; retry policy (`STATUS_READ_RETRY_ATTEMPTS`/`_DELAY_MS`) stays a call-site decision, kept out of the pure classifier.
- **Pre-read resource guards:** file-size/extension checks before hashing/reading; every parser capped at `MAX_SEGMENTS_PER_FILE`(1000).
- **Atomic temp-then-rename** for state files (`atomic_replace`) and the index (build-aside staging DB, `wal_checkpoint(TRUNCATE)`, atomic rename).
- **Opportunistic best-effort reaping of accumulated per-project artifacts** (`shared::lock_reap`): a file is deleted only when stale on two independent axes — mtime older than a threshold AND a non-blocking exclusive `flock` probe succeeds — with the lock held across the unlink and a filesystem-identity re-check (`dev`/`ino` unchanged since scan) immediately before deleting, so a concurrent recreate at the same pathname can't be destroyed. Bounded by both a candidate count and a wall-clock budget enforced mid-scan, so the sweep can never meaningfully delay the boundary (CLI startup) it hangs off. The module is also the sole namespace authority: the same key/name functions mint and parse the filenames, so the reaper can never touch a file it wouldn't itself have created.
- Downloaded artifacts verified against a pinned SHA-256 floor, then keyless-OIDC attestation. Secure-fs enforces canonical paths, clamps writes to an approved root, rejects symlink components.

## Concurrency

- Async over Tokio/libSQL everywhere; MCP via rmcp `serve(stdio())`.
- Indexing parses in parallel (`JoinSet::spawn_blocking`) then reorders deterministically. The daemon worker multiplexes signals/requests/reload/debounce with `tokio::select!` + a `Semaphore` cap.
- **One shared `CancellationToken`** threads every pass; SIGTERM cancels it, resumed not restarted. Long synchronous walks run on `spawn_blocking` with periodic token checks.
- **RAII `flock` guards** (`DaemonLock`, single-writer `RebuildLock`) enforce single-owner rebuilds, auto-released on drop/`?` unwind. Stale locks (age + dead holder) auto-clear before acquisition.
- **Non-blocking bounded-wait spawn** (`oneup_start`): spawn the work, `tokio::time::timeout(budget, handle)`; timeout detaches and returns pollable progress; background failures surface as blocked readiness, never just a log line. A spawned rebuild now records a process-unique `run_id` per invocation so status polling can distinguish successive runs of the same project.

## Dependency Injection / Config

- No DI container; deps passed explicitly (`HybridSearchEngine::new_scoped`). Engines built per-request from a connection + a `SearchScope`.
- `shared::config` centralizes paths; `state_root` vs `source_root` separated for worktrees. Indexing config resolves CLI → env → registry → defaults.

## Extension

- Static extension surfaces, not plugin loaders: clap enum variants + rmcp `#[tool_router]` methods.
- `RETAINED_PUBLIC_TOOLS` is the single authority for tool identity; every generated `RecoveryCall`/`NextAction` is debug-asserted against it, so a truncation note can never point at a non-public tool.
- The only opt-in user-file mutation (`doctor --clean-hints --apply`) is default-OFF, preview-by-default.

## Testing

- Unit tests in `#[cfg(test)]`; integration/CLI tests under `tests/`. Release/script behavior is black-box (spawn scripts + a stdio MCP fixture, assert on emitted JSON).
- **TDD red-first**; behavioral guards over snapshots. Pure injected gates tested with crafted `now`/version/empty-state values (`ranking.rs`'s new `classify_query_token`/`corrected_impact_call` tests follow the same one-input-shape-per-test style).
- **RAII test guards** isolate/clean up: `EnvGuard`, `ChildGuard`/`DaemonCleanupGuard`, real `flock` guards.
- **Test-only fault injection via a keyed static, not a mock framework:** `REBUILD_PANIC_ROOTS` (`OnceLock<Mutex<HashSet<PathBuf>>>`, `#[cfg(test)]`-gated) arms a one-shot panic keyed by `state_root` so concurrent tests can't trip each other's injection, exercising the spawn wrapper's panic-recovery arm deterministically without a mocking layer.
- Drift guards source expected values from `CARGO_PKG_VERSION`; swap tests assert all-or-nothing reads under concurrent readers.
- **Eventual-state polling in E2E** — assert via `wait_for_status(client, what, predicate)` (deadline + last-payload panic), never single-shot status reads.
- **CI-faithful FTS-only regime** — canonical fake HOME, `.download_failed` markers, `ONEUP_DISABLE_MODEL_DOWNLOADS=1`; test daemons killed only by path-scoped pattern via `pgrep -P <server>`.
