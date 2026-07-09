---
scope: kbRoot
path_pattern: "patterns.md"
producer: knowledge-base
type: document
description: "Implementation patterns, coding conventions, and idioms for a single-project codebase. Hard limit of 150 lines."
strictness: strict
---
# Implementation Patterns

Conventions and idioms for 1up (`oneup`). Patterns from the supply-chain, daemon-handshake, non-destructive-rebuild, monorepo-scoped-indexing (v0.1.13), and envelope-quality round-4 (PR #84) work are folded in.

## Naming & Organization

- `snake_case` modules inside layer dirs (`src/{cli,daemon,indexer,mcp,search,shared,storage}`). New CLI commands wire through `cli/mod.rs` + a clap `Command` enum arm — never a runtime registry.
- CLI command modules use `<Name>Args` + `pub async fn exec(...)`. Maintenance commands accept `OutputFormat`; core/discovery commands own fixed output and reject `--format`.
- Verb-prefixed helpers (`resolve_`/`classify_`/`ensure_`/`build_`) and RAII openers (`acquire_`/`try_acquire_`/`open_`).
- Shared literals live in `src/shared/constants.rs` and are imported, never re-hardcoded — even env-var *names* are constants (`ONEUP_*`).

## Type Modeling

- Owned structs/enums with serde derives; MCP inputs derive `Deserialize + JsonSchema` (`deny_unknown_fields`), `rename_all="snake_case"` wire names.
- Single source of truth via const arrays (`RETAINED_PUBLIC_TOOLS: [&str; 9]`, `NON_EMBEDDABLE_CHUNK_LANGUAGES`).
- Domain outcomes are dedicated enums (`UpdateStatus`, `ReadinessStatus`), often a three-state collapsed onto `Result`.
- Additive fields stay back-compatible: `#[serde(default, skip_serializing_if = "Option::is_none")]` (e.g. manifest `expiry`, `IndexScope.eligibility_note`). RAII guards consumed by value (`finalize_and_swap(self)`); `RebuildLock` is `#[must_use]`.
- `#[serde(skip)]` hint fields carry pre-gating data for envelope derivation without ever serializing: `SegmentRecord.symbol_hint` (first defined symbol, captured before verbosity gating) drives the `oneup_symbol` next_action while the gated symbol lists are empty. Optional wire params collapse to internal bools (`verbosity: Option<String>` → `is_verbose`); empty vecs are serde-skipped.

## Error Handling

- **Two layers:** CLI/MCP boundaries use `anyhow` + `bail!`/`Context`; libraries use `OneupError` with per-domain `thiserror` enums (`UpdateError`, `DaemonError`, `StorageError`).
- **Actionable wording:** `DrainTimeout` → "run `1up stop` then retry"; schema errors name found-vs-expected versions + the worktree path.
- **Three outcome shapes:** verified → `Ok`; disproved → hard fail leaving state untouched (`ensure_manifest_acceptable`, `AttestationFailed`); cannot-run → degrade to a safe floor.
- **Cooperative cancellation** is its own typed outcome (`IndexingError::Cancelled`), leaving the context dirty for a later pass.

## Validation

- Concentrated at clap parsing, MCP input schemas, filesystem gates, and schema-readiness seams.
- **Decision gates are pure**, taking inputs as parameters (`ensure_manifest_acceptable(manifest, installed, now)`, `should_idle_shutdown(is_empty, empty_for, timeout)`, `gate_allows_first_index(is_first_index, file_count, threshold, scope_recorded)`) — deterministic + unit-testable.
- Repo paths canonicalized + clamped to `source_root`; out-of-root/parent-escape → `Rejected`; 1-based lines enforced. Scope roots validated on construction (repo-relative, no `../`).
- **Filter precedence is a documented contract** (`ScanFilter`): secrets > scope_globs (exclusive cone — the REQ-001/REQ-002 cost boundary, which includes/overrides cannot punch through) > include_globs/override dirs > excludes > dotfile hiding. `include_globs` guarantee inclusion and never exclude non-matches.

## Output Contracts

- `tracing` to stderr; **stdout reserved for protocol/data** so MCP stdio + machine output stay clean — notices/banners go to stderr.
- Degraded/stale wording is centralized in single-source `*_REASON` constants (`STALE_REBUILD_REASON`, `NO_INDEXED_EMBEDDINGS_REASON`) and merged via `combine_degraded_reasons` (never overwritten). Same family: `unscoped_eligibility_note()` single-sources the `index_scope` note so status, search, and facts paths cannot drift.
- Ranked-list wording branches on **emptiness, not index** (`generate_ranked_suggestions`): the first *emitted* entry gets primary phrasing (never a leading "Or") even when higher ranks are skipped, and ordinals stay truthful to actual rank, not emission position.
- MCP `instructions` fit a ~2KB host-truncation budget with the routing rule front-loaded.

## Storage / I-O

- libSQL via a `Db` wrapper; tuned PRAGMAs applied with lock-retry; reads context-scoped (`*_for_context`).
- **Atomic temp-then-rename** for state files (`atomic_replace`) and the index: a rebuild is **build-aside** (uuid-suffixed staging DB), finalized to one self-contained file (`wal_checkpoint(TRUNCATE)`), then atomically renamed over `index.db`; prior sidecars retired first.
- Downloaded artifacts verified against a pinned SHA-256 floor, then keyless-OIDC attestation. Secure-fs enforces canonical paths, clamps writes to an approved root, rejects symlink components.

## Concurrency

- Async over Tokio/libSQL everywhere; MCP via rmcp `serve(stdio())`.
- Indexing parses in parallel (`JoinSet::spawn_blocking`) then reorders deterministically. The daemon worker multiplexes signals/requests/reload/debounce with `tokio::select!` + a `Semaphore` cap.
- **One shared `CancellationToken`** threads every pass; SIGTERM cancels it and indexing stops at the next safe yield point, resumed not restarted. Long synchronous walks run on `spawn_blocking` with periodic token checks so the signal-observing task is never starved.
- **RAII `flock` guards** (`DaemonLock`, single-writer `RebuildLock`) enforce single-owner rebuilds, auto-released on drop/`?` unwind. Stale locks (age > `STALENESS_THRESHOLD_SECS` + dead holder) auto-clear before acquisition; a gated project must consume its pending run (`start_run`/`finish_run`) or the dirty flag re-queues it forever.
- **Non-blocking bounded-wait spawn** (`oneup_start`): spawn the work, `tokio::time::timeout(budget, handle)`; completion within budget returns the final payload, timeout detaches and returns pollable progress; background failures surface as blocked readiness, never just a log line.

## Dependency Injection / Config

- No DI container; deps passed explicitly (`HybridSearchEngine::new_scoped`). Engines built per-request from a connection + a `SearchScope` from the `WorktreeContext`.
- `shared::config` centralizes paths; `state_root` vs `source_root` separated for worktrees. Indexing config resolves CLI → env → registry → defaults. `option_env!` defaults overridable by a same-named runtime env var.

## Extension

- Static extension surfaces, not plugin loaders: clap enum variants + rmcp `#[tool_router]` methods.
- Tree-sitter extraction flows through the generic body walk + `nested_kinds()` — no per-container special cases (a `field_declaration_list` field-name lookup was dead code: struct and enum bodies live under the `body` field). Verify grammar node/field names with a parse probe before extending.
- `RETAINED_PUBLIC_TOOLS` is the single authority: next-actions `debug_assert!` membership; the doctor classifier treats any `oneup_*` token absent from it as stale; doc/test guards pin to it.
- The only opt-in user-file mutation (`doctor --clean-hints --apply`) is default-OFF, preview-by-default, and removes only a byte-exact 1up-owned fence via `atomic_replace_within_project_root`.

## Testing

- Unit tests in `#[cfg(test)]`; integration/CLI tests under `tests/`. Release/script behavior is black-box (spawn scripts + a stdio MCP fixture, assert on emitted JSON).
- **TDD red-first**; behavioral guards over snapshots. Pure injected gates tested with crafted `now`/version/empty-state values.
- **RAII test guards** isolate/clean up: `EnvGuard` (under a static `Mutex`), `ChildGuard`/`DaemonCleanupGuard` reap spawned daemons, real `flock` guards prove exclusion.
- Drift guards source expected values from `CARGO_PKG_VERSION` (`documentation_tool_names_match_retained_public_tools`, `committed_update_manifest_version_not_ahead_of_binary`); swap tests assert all-or-nothing reads under concurrent readers.
- **Eventual-state polling in E2E** — `oneup_start` is non-blocking and the daemon indexes concurrently, so E2E tests assert eventual stable states via `wait_for_status(client, what, predicate)` (deadline + last-payload panic), never single-shot status reads; "degraded" is both a mid-index transient and the FTS-only terminal, so predicates require the full stable shape.
- **E2E timing hardening** — a test asserting the *synchronous* refusal envelope must pin `ONEUP_START_RESPONSE_BUDGET_MS` high (contended CI runners overrun the 2s default and get a detached `"indexing"` ack); a test racing daemon DB-init retries until the terminal `refuse_and_propose_scope` envelope arrives. Both races reproduce on real hardware, not just CI.
- **CI-faithful FTS-only regime** — canonical fake HOME (`$(cd "$(mktemp -d)" && pwd -P)` — secure-fs rejects `/var` symlinks), `.download_failed` markers in Library + XDG model dirs, AND `ONEUP_DISABLE_MODEL_DOWNLOADS=1` (explicit index/start clears the marker by design). Test daemons are killed only by path-scoped pattern (`target/debug/1up __worker`); tests never parent the process they SIGTERM-probe (zombies read as alive), discovering daemons via `pgrep -P <server>`. Assert unit-test cache behavior via cache state, not wall-clock timing.
