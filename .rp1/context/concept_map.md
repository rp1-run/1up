---
scope: kbRoot
path_pattern: "concept_map.md"
producer: knowledge-base
type: document
description: "Domain concepts, terminology glossary, and cross-references for a single-project codebase."
strictness: strict
---
# Domain Concepts & Terminology

**Project**: 1up (`oneup`) — local code-discovery engine
**Domain**: A tree-sitter + ONNX-embedding + libSQL index over a repository, exposed to agents through nine read-only/lifecycle MCP tools so semantic search, symbol lookup, file-line context, structural queries, likely-impact, and an orientation digest become the primary discovery path before raw grep/find. Since v0.1.13 the index can be **scoped to directory cones** on large monorepos behind a refuse-and-propose gate; the v0.1.15–v0.1.17 range drops DiskANN for exact vector scan (schema v20), git-stamps build identity for daemon trust, and hardens install/state-root/MCP-batch surfaces.

## Core Concepts

- **Segment** — fundamental indexed unit: a tree-sitter/text-chunker block with a deterministic id, file span, language, block_type, role, defined/referenced/called symbols, complexity, optional breadcrumb, and an optional quantized embedding row. `NON_EMBEDDABLE_CHUNK_LANGUAGES` (json/yaml/toml/…) are FTS-only.
- **Segment Handle** — a segment id as a durable cross-tool reference; MCP shows a leading-colon 12-char prefix; `oneup_get`/`oneup_impact` resolve a full id or unique prefix and report ambiguity. `oneup_get` now bounds handle-batch cardinality/bytes (see Batch Hydration Caps) and remembers recent failed handles for recovery.
- **WorktreeContext** — worktree-aware identity: `context_id` (scoping key), `state_root`, `source_root`, branch name/ref/status, head_oid. Every storage read is filtered by `context_id` so linked worktrees stay isolated in one shared DB. `resolve_linked_worktree_info` now verifies a mutual reverse-pointer (`<git_dir>/gitdir` must point back to the exact `.git` file being resolved) before trusting a `gitdir`/`commondir` pair, so a forged commondir can no longer redirect `state_root` into an unrelated victim repo — falls back to `.1up` ancestor → git root → project root on verification failure.
- **SearchResult / SearchHit** — lean hydrated discovery row (handle, score 0–100, path/lines, kind, optional breadcrumb/symbol) from hybrid or FTS-only search. `oneup_search` now accepts multiple `queries` per call (≤ `MAX_SEARCH_QUERIES`=4); per-query ranked lists are fused by RRF (dedup by handle, keep best-rank representative), then implementation-intent queries stably sink doc-section results below code.
- **Relation** — directed dependency edge (call/reference/conformance) persisted unresolved per source segment and resolved lazily at impact time; `doc_mention` edges are descriptive and excluded from impact.
- **Build Identity** *(new)* — `BUILD_IDENTITY` (`{semver}+{git-short-hash}[.dirty[.digest]]`, stamped by the new `build.rs` via `ONEUP_BUILD_IDENTITY`, degrading to `+unknown` without git) replaces bare semver for the daemon version-handshake: the CLI trusts a daemon search response only when its stamped identity exactly equals the running binary's; any mismatch (including same-semver-different-build, or an absent stamp) drains + restarts the daemon. Closes the bare-semver trust gap (issue #108).
- **UpdateManifest / UpdateArtifact** — the self-update feed: version, git_tag, published_at, optional `expiry` (RFC3339 anti-freeze deadline), per-platform artifacts each with a mandatory `sha256`, channels, `yanked`, `minimum_safe_version`.
- **StagingRebuild** — RAII guard owning a build-aside staging DB (`index.db.rebuild-<uuid>`); built aside, finalized to one self-contained file, atomically renamed over the served `index.db`. A failed/cancelled rebuild leaves `index.db` intact; Drop removes the orphan. *(v20: no longer builds/defers any vector index — that step was deleted with DiskANN.)*
- **RebuildLock** — single-writer flock on `<state_root>/.1up/rebuild.lock`. One-shot CLI/MCP rebuilds acquire with a bounded wait then fail closed (`RebuildLockContended`); the daemon try-acquires non-blocking and defers.
- **ReadinessPayload** — `oneup_status` classification: ready/missing/indexing/stale/degraded/blocked, plus stats, schema_version, vector coverage, head-drift, and daemon status.
- **RepositoryOverview** — deterministic, size-bounded orientation digest (stats, top symbols, module map, dependency edges, entry points); recommended first call (`oneup_overview`).
- **Scope Roots / scope_globs** *(v0.1.13)* — validated repo-relative directory cones (no absolute/`../` paths) persisted in DB meta (`scope_roots_v1`); converted to exclusive `dir/**` scope_globs. Survive branch switches and daemon restarts (scope carry). Distinct from additive `include_globs`.
- **First-Index Gate + Facts Envelope** — REQ-001 refuse-and-propose: an unscoped first index of an over-threshold repo (`ONEUP_FILE_COUNT_THRESHOLD`, default 3000) stays idle and returns a `FactsEnvelope`. *(refined)* The daemon-side gate walk (`count_files_gitignore_aware`) now reuses the MCP-side `is_under_vcs_dir`/`build_vcs_aware_walker` exclusion so the two gates can never disagree on the same repo; `ProjectState.cached_gate_file_count` caches the walk once a project is gated (cleared only on registry reload/SIGHUP), avoiding a full-tree re-walk on every dirty signal.
- **Persistent Density & Scope-Proposal Caches** *(new)* — `PersistentDensityCache` and `PersistedScopeProposal` persist prior gate computations to state_root for reuse on subsequent gated calls; `is_scope_proposal_fresh` keys freshness to the current walk-cache key so a stale cache from a different repo state is never served.
- **IndexScope** — coverage disclosure on readiness/search payloads: roots, indexed_files, total_files, `coverage_description()`, and `eligibility_note` (plain-text explanation of the unscoped coverage gap). Agents must never infer absence from empty scoped results.
- **Embedding Pool** *(v17+, refined v20)* — content-addressed dedup: `content_key` (hash of content + token window) → shared vector + `ref_count`. The DiskANN graph index built on top of the pool is **removed entirely (schema v20)**: exact `vector_distance_cos` scan is the only vector path at every corpus size, measured linear at ~0.9µs/vector (~86ms@100k, ~446ms@500k warm) vs the removed ANN path's superlinear cost (~7s@4.5k, ~45s@27k) and ~109MiB shadow-table overhead. Measurements preserved in `docs/diskann-removal.md`.
- **Batch Hydration Caps** *(new, #143)* — `oneup_get`'s handles array is capped at `MAX_GET_HANDLES_PER_CALL` (50) and `MAX_GET_REQUEST_HANDLE_BYTES` (16KiB aggregate), enforced in `check_get_handles_cap` **before** any index open or hydration (structured error names cap and received count); previously uncapped, a 50k-handle batch produced a ~12.9MB response. Hydrated content is separately metered against a 2MiB response budget in input order. `HYDRATION_BATCH_MAX_HANDLES` (4) bounds only the *recommended* default batch in next_actions, distinct from the hard cap.
- **Handle Recovery** *(new)* — when a handle/prefix fails to resolve, `attempt_handle_recovery` classifies the failure and recovers a residual unique prefix via `recover_handle_by_unique_prefix`; a process-global `FailedHandleMemory` records recent failed lookups so repeated calls with the same bad handle skip the full DB prefix scan (cleared once resolved).
- **Query Token Classification** *(new)* — `classify_query_token` (Neutral/Identifier/Prose per token) replaces the old blunt prose heuristic: a query reads as natural language only with ≥2 significant terms, ≥2 prose-like words, and prose strictly outnumbering identifier-like words — preserving safeguards for proper-noun prose while classifying snake_case/CamelCase pairs as non-prose.
- **Registry EntryIdentity & Dedup** *(new)* — registry-entry identity narrows to the stable (project_root, source_root, branch_ref) triple, deliberately excluding head_oid/context_id (mutable facts refreshed in place) — folding head_oid into identity was issue #116's root cause. `Registry::load_from_path_with_repair` dedups on every load via `collapse_duplicate_entries` (most-recently-registered survivor, absorbing durable `indexing` config) and best-effort persists the repair under the registry lock; `deregister_context_ids_if` re-validates liveness under the lock before removal.
- **Stale-Branch Auto-Prune** *(new)* — on daemon startup, `prune_stale_branch_contexts_on_startup` conservatively removes stale-branch snapshots of *live* worktrees whose branch no longer exists (shared `segments::is_stale_branch_snapshot` predicate + extra conservative gates); narrower than `1up gc --apply`. Disclosed to users as reclaimable stale-branch contexts (see disclosure floors).
- **Non-Blocking Start** — `oneup_start` spawns the rebuild and waits a bounded budget (`ONEUP_START_RESPONSE_BUDGET_MS`, 2s); fast ops return final readiness, long rebuilds detach and callers poll `oneup_status`.
- **Hard-Fail Unverified Install** *(new, #143)* — `setup.sh::verify_checksum` previously warned-and-continued on a genuine 404 for `SHA256SUMS` (repo unpublished), installing an unverified binary; it now fails closed by default. `ONEUP_SKIP_CHECKSUM=1` is the explicit, loudly-warned escape hatch. Transient-fetch fail-closed and mandatory-verify-when-published paths unchanged.
- **File/Segment Size Caps** *(v0.1.14)* — `MAX_FILE_SIZE_BYTES` (2MB, skipped before read) + `MAX_SEGMENTS_PER_FILE` (1000) bound memory on minified/generated files.
- **Expanded Secret Globs** *(v0.1.14)* — `DEFAULT_SECRET_GLOBS` (19 patterns), centralized in `shared/constants`, applied non-overridably on both scan and MCP read paths.
- **Schema Init Tolerance Window** *(refined)* — `ensure_current_tolerating_init` rides out the transient "tables exist, `schema_version` row absent" init window; now has its own multi-second wait budget (`SCHEMA_INIT_WAIT_ATTEMPTS`=50 × `SCHEMA_INIT_WAIT_DELAY_MS`=100ms ≈ 5s), no longer borrowing the ~450ms DB-lock-retry budget. A genuine version mismatch still fails fast on the first attempt.

## Terminology

- **context_id / state_root / source_root** — context scoping key; state dir (index DB, project id, daemon status) vs the scanned/watched tree (differ for linked worktrees).
- **readiness status** — ready/missing/indexing/stale/degraded/blocked. **operation status** — ok/empty/partial/degraded. **read status** — found/not_found/ambiguous/rejected/error.
- **head drift** — advisory signal comparing indexed-at HEAD vs live HEAD; appends an `oneup_start` (mode `index_if_needed`) action without changing readiness.
- **serve-stale / stale-but-available** — during a rebuild, readers keep serving the prior index; results carry `STALE_REBUILD_REASON` on the `degraded_reason` channel only.
- **degraded_reason / combine_degraded_reasons** — single advisory channel; no-embeddings and rebuild-stale reasons are joined (not dropped) and shared by MCP ops + the daemon worker.
- **BUILD_IDENTITY** *(new)* — compile-time git-stamped build identity (`{semver}+{git}[.dirty[.digest]]`; dirty digest = 8-hex of `git diff HEAD`) used for the daemon/CLI handshake; supersedes bare `VERSION` comparison.
- **EntryIdentity** *(new)* — registry dedup key: (project_root, source_root, branch_ref); deliberately excludes head_oid/context_id (issue #116).
- **reverse-pointer verification** *(new)* — before trusting a linked worktree's gitdir/commondir pointer, confirm `<git_dir>/gitdir` (git's own registered reverse pointer) canonicalizes to the exact `.git` file being resolved; anchoring also requires git_dir's parent to be `<commondir>/worktrees/`.
- **MAX_GET_HANDLES_PER_CALL / MAX_GET_REQUEST_HANDLE_BYTES** *(new)* — hard caps (50 handles; 16KiB aggregate) enforced before any index open on `oneup_get`; `HYDRATION_BATCH_MAX_HANDLES` (4) bounds only the recommended next_actions batch.
- **multi-query fusion** *(new)* — `merge_multi_query_results` fuses per-query ranked lists via RRF, dedups by handle keeping the best-rank representative, truncates to limit.
- **cached_gate_file_count** *(new)* — ProjectState field caching the first-index gate's file-count walk while a project stays gated; invalidated only on registry reload (SIGHUP); paired with `FIRST_INDEX_GATE_BLOCKED_COUNT_TTL_MS` (60s).
- **ONEUP_SKIP_CHECKSUM** *(new)* — explicit installer escape hatch past a checksum-verification failure (unpublished SHA256SUMS 404); default is now fail-closed.
- **disclosure floors** *(new)* — `DISCLOSURE_STALE_CONTEXT_COUNT_FLOOR` / `DISCLOSURE_RECLAIMABLE_BYTES_FLOOR` gate the `1up status`/`list` stale-branch advisory hint so small per-branch accumulation is never nagged about; `GC_STALE_BRANCH_AUTOPRUNE_MAX_AGE_DAYS`=30 bounds startup auto-prune.
- **build-aside + atomic swap** — build into a uuid staging sibling, fold its WAL into one file, atomically rename over `index.db` under the RebuildLock.
- **cooperative cancellation / IndexingError::Cancelled** — a `CancellationToken` pass stops at a committed batch boundary; SIGTERM cancels it, leaves the context dirty, resumes next pass.
- **daemon idle self-exit** — a daemon with zero registered projects self-exits after `DAEMON_IDLE_SHUTDOWN_SECS` (60); a daemon with any project never idles out.
- **mandatory checksum floor / keyless-OIDC attestation / three-state verify** — self-update trust chain: SHA-256 floor, then Sigstore attestation (verified → proceed; disproved → fail closed; cannot-run → degrade to floor).
- **anti-rollback / anti-freeze gate** — `ensure_manifest_acceptable` hard-refuses a manifest older than installed (`ManifestRollback`) or past expiry+skew (`ManifestExpired`).
- **yanked / minimum_safe_version / InstallChannel** — manifest safety signals → urgent `UpdateStatus`; channel detected from binary path selects the upgrade instruction.
- **Hybrid Search** — candidate-first fusion of vector + FTS + symbol via weighted RRF (`RRF_K=60`, `VECTOR_WEIGHT=1.5`, `SYMBOL_WEIGHT=4.0`) before hydration.
- **exact vector scan (sole path)** *(corrected)* — replaces the prior "exhaustive scan vs ANN" dichotomy. `VECTOR_EXHAUSTIVE_SCAN_MAX_VECTORS` is gone; `VECTOR_EXACT_SCAN_WARN_THRESHOLD` (262144) only gates a one-time process-wide `tracing::warn!` about linear latency growth — it never changes which path runs. `ONEUP_FORCE_ANN_SEARCH`, `vector_top_k` queries, and `VECTOR_PREFILTER_CONTEXT_SCALE_LIMIT` are removed.
- **SCHEMA_VERSION** — now **20** (v20 drops the DiskANN index + `_shadow` table entirely; v19 was embedding-pool/scope-metadata). Older schemas fail closed with `1up reindex` guidance; no in-place migration.
- **StatusFileRead (Absent/Parsed/Unreadable)** *(new)* — three-state status-file read: not-yet-written is silently `None`; torn/corrupt retries `STATUS_READ_RETRY_ATTEMPTS` (3 × 50ms) then error-logs and returns `None` — never fabricating empty progress from a partial write.
- **SourcePresence (Present/Absent/Indeterminate)** *(new)* — `probe_source_presence` three-state probe; only definite `Absent` allows destructive prune/deregistration; `Indeterminate` (e.g. unreachable mount) defers with a warning (`DaemonError::SourceProbeIndeterminate`).
- **stale-state liveness** — rebuild locks older than 5 min with no live holder auto-clear; `Running` progress whose `indexer_pid` is dead reads as missing.
- **valid project marker** — a `.1up` ancestor anchors resolution only if it contains `index.db`/`project_id`/`rebuild.lock`.
- **launch_subdir** — invocation directory captured before root clamping; first scope suggestion.
- **SERVER_GUIDANCE** — single-sourced agent routing guidance in `src/mcp/server.rs`, front-loaded to survive 2KB truncation, drift-guarded against `RETAINED_PUBLIC_TOOLS`.

## Key Relationships

- `oneup_overview`/`oneup_status`/`oneup_start` **precede** `oneup_search` → emits handles → `oneup_get`/`oneup_impact` **consume** them.
- `oneup_search` **fuses** multiple `queries` via `merge_multi_query_results` (RRF) before implementation-intent doc-demotion reorders the merged list.
- `oneup_get` **is gated by** `check_get_handles_cap` (count/byte caps before index open) and **uses** `FailedHandleMemory`/`attempt_handle_recovery` on failed handles.
- CLI search **validates** the daemon's stamped `BUILD_IDENTITY` == the running binary's before trusting results, else drains + restarts (supersedes `VERSION` comparison).
- `Registry` load **invokes** `collapse_duplicate_entries` (EntryIdentity dedup) and persists the repair under the registry lock.
- Daemon and MCP first-index gates **share** the VCS-aware walk exclusion (`is_under_vcs_dir`/`build_vcs_aware_walker`) so they can never disagree; each caches its walk (`cached_gate_file_count`).
- `WorktreeContext` resolution **is gated by** reverse-pointer verification; every storage read **is scoped by** `context_id`.
- `RebuildLock` **guards** `StagingRebuild.finalize_and_swap`; an in-flight rebuild **triggers** `STALE_REBUILD_REASON`.
- Daemon startup **invokes** `prune_stale_branch_contexts_on_startup`; `1up status`/`list` **disclose** reclaimable stale-branch contexts above the floors.
- `self_update` **sequences** anti-rollback/expiry gate → checksum floor → three-state attestation → atomic replace; `setup.sh` **is gated by** fail-closed checksum with `ONEUP_SKIP_CHECKSUM` as sole opt-out.
- First-Index Gate **emits** Facts Envelope; a scoped `oneup_start` **records** scope which every rebuild path **re-persists** to staging meta so `finalize_and_swap` preserves it.
- `ScanFilter` precedence: secrets > scope_globs (exclusive cone) > include_globs/overrides > excludes > dotfile hiding; the secrets tier is the non-overridable 19-pattern `DEFAULT_SECRET_GLOBS`.

## Bounded Contexts

1. **Code Discovery (MCP)** — `src/mcp`: nine `oneup_*` tools, `ToolEnvelope`/`next_actions`, batch hydration caps, handle recovery, multi-query fusion, single-sourced guidance.
2. **Search & Retrieval** — `src/search`: hybrid + RRF (single- and multi-query fan-in), exact vector scan (no ANN), query token classification, impact + corroboration, overview.
3. **Index Storage & Lifecycle** — `src/storage`/`src/indexer`: schema v20 (DiskANN removed), content-addressed embedding pool, build-aside swap, cooperative cancellation, exclusive scope cones, size/segment caps, init-tolerant validation.
3a. **Monorepo Scoping & Policy** — `src/mcp/ops.rs` + `src/daemon/{lifecycle,worker}.rs` + `src/shared/project.rs`: gate parity (daemon/MCP), cached_gate_file_count, persistent density/scope-proposal caches, reverse-pointer verification, launch_subdir, marker-validated resolution.
4. **Daemon & Concurrency** — `src/daemon`: Registry EntryIdentity & dedup, BUILD_IDENTITY handshake, stale-branch auto-prune on startup, single-writer lock, inode-swap detection, serve-stale, idle self-exit, deleted-source deregistration.
5. **Supply-Chain Trust & Self-Update** — `src/shared/{update,constants,errors}` + `scripts/install`: manifest, checksum floor, three-state attestation, anti-rollback/expiry, hard-fail unverified install + `ONEUP_SKIP_CHECKSUM`.

## Cross-Cutting Concerns

- **Degradation reporting** — one `degraded_reason` channel (never the result stream), combined + sourced from shared constants so CLI/MCP/daemon can't drift.
- **Fail-closed by default, explicit opt-out for accepted risk** — installer checksum, schema mismatch, and attestation disproof all abort by default; the only ways past are narrow, loudly-warned overrides (`ONEUP_SKIP_CHECKSUM`) or genuine cannot-run degradation.
- **Identity over mutable state** — registry entries (EntryIdentity) and worktree trust (reverse-pointer) key on stable, hard-to-forge identity rather than values that change per HEAD advance or that an attacker controls.
- **Cross-process build coherence** — BUILD_IDENTITY (git-stamped) replaces bare semver for the daemon/CLI handshake, so two builds sharing a version number can't serve each other's results.
- **Bound resource use before expensive work** — `oneup_get` handle caps, per-file size/segment caps, and gate-walk caching all reject or reuse before I/O cost is paid.
- **Gate parity across surfaces** — the first-index gate's walk logic is single-sourced between daemon and MCP so the two entry points can never disagree on over/under-threshold.
- **Never destructively act on ambiguous evidence** — three-state source presence (gc/daemon), three-state status reads, and lock reaping's identity re-check all distinguish "definitely gone" from "could not tell".
- **Atomic, torn-read-safe persistence** — temp+fsync+rename / finalize-and-swap with owner-only modes.
- **Worktree-context isolation** — `context_id` threads every read so linked worktrees share one DB without cross-contamination.
- **Hermetic testing** — `ONEUP_DISABLE_MODEL_DOWNLOADS`; attestation verify is pure/offline against an embedded root; gates take `now`/version as params.
