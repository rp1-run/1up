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
**Domain**: A tree-sitter + ONNX-embedding + libSQL index over a repository, exposed to agents through nine read-only/lifecycle MCP tools so semantic search, symbol lookup, file-line context, structural queries, likely-impact, and an orientation digest become the primary discovery path before raw grep/find. Since v0.1.13 the index can be **scoped to directory cones** on large monorepos behind a refuse-and-propose gate.

## Core Concepts

- **Segment** — fundamental indexed unit: a tree-sitter/text-chunker block with a deterministic id, file span, language, block_type, role, defined/referenced/called symbols, complexity, optional breadcrumb, and an optional quantized embedding row. `NON_EMBEDDABLE_CHUNK_LANGUAGES` (json/yaml/toml/…) are FTS-only.
- **Segment Handle** — a segment id as a durable cross-tool reference; MCP shows a leading-colon 12-char prefix; `oneup_get`/`oneup_impact` resolve a full id or unique prefix and report ambiguity.
- **WorktreeContext** — worktree-aware identity: `context_id` (scoping key), `state_root`, `source_root`, branch name/ref/status, head_oid. Every storage read is filtered by `context_id` so linked worktrees stay isolated in one shared DB.
- **SearchResult / SearchHit** — lean hydrated discovery row (handle, score 0–100, path/lines, kind, optional breadcrumb/symbol) from hybrid or FTS-only search.
- **Relation** — directed dependency edge (call/reference/conformance) persisted unresolved per source segment and resolved lazily at impact time; `doc_mention` edges are descriptive and excluded from impact.
- **UpdateManifest / UpdateArtifact** *(new)* — the self-update feed: version, git_tag, published_at, optional `expiry` (RFC3339 anti-freeze deadline), per-platform artifacts each with a mandatory `sha256`, channels, `yanked`, `minimum_safe_version`.
- **StagingRebuild** *(new)* — RAII guard owning a build-aside staging DB (`index.db.rebuild-<uuid>`); the refreshed index is built aside, finalized to one self-contained file, then atomically renamed over the served `index.db`. A failed/cancelled rebuild leaves `index.db` intact; Drop removes the orphan.
- **RebuildLock** *(new)* — single-writer flock on `<state_root>/.1up/rebuild.lock`. One-shot CLI/MCP rebuilds acquire with a bounded wait then fail closed (`RebuildLockContended`); the daemon try-acquires non-blocking and defers.
- **ReadinessPayload** — `oneup_status` classification: ready/missing/indexing/stale/degraded/blocked, plus stats, schema_version, vector coverage, head-drift, and daemon status.
- **RepositoryOverview** — deterministic, size-bounded orientation digest (stats, top symbols, module map, dependency edges, entry points); recommended first call (`oneup_overview`).
- **Scope Roots / scope_globs** *(new v0.1.13)* — validated repo-relative directory cones (no absolute/`../` paths) persisted in DB meta (`scope_roots_v1`); converted to exclusive `dir/**` scope_globs. Survive branch switches and daemon restarts (scope carry). Distinct from additive `include_globs`.
- **First-Index Gate + Facts Envelope** *(new)* — REQ-001 refuse-and-propose: a first index (segments == 0, robust to the empty schema DB created at startup) of an over-threshold repo (`ONEUP_FILE_COUNT_THRESHOLD`, default 3000) without a recorded scope stays idle and returns a `FactsEnvelope` — gitignore-aware per-directory stats, workspace manifests, sparse-checkout, `launch_subdir` first-suggestion, calibrated vector estimates (measured density, low/high bounds + basis). Gated daemon consumes the pending run and idles (no walk loop).
- **IndexScope** *(new)* — coverage disclosure on readiness/search payloads: roots, indexed_files, total_files, `coverage_description()`, and `eligibility_note` *(new v0.1.14)* — a plain-text explanation of the unscoped index coverage gap (why indexed_files < total_files: lockfiles, vendored code, gitignored paths), populated only when scope roots are empty. Agents must never infer absence from empty scoped results.
- **Embedding Pool** *(v17+)* — content-addressed dedup: `content_key` (hash of content + token window) → shared vector + `ref_count`; DiskANN built deferred (`VectorIndexBuild::Deferred`) after pool load, before swap. Replaces per-segment 1:1 vectors.
- **Non-Blocking Start** *(new)* — REQ-012: `oneup_start` spawns the rebuild (`spawn_rebuild_task`) and waits a bounded budget (`ONEUP_START_RESPONSE_BUDGET_MS`, default 2s); fast ops return final readiness (drift cleared, blocked surfaced), long rebuilds detach and callers poll `oneup_status`.
- **Verbosity Parameter** *(new)* — `GetInput.verbosity` (`default`|`full`, default `default`) gates symbol-list detail in `oneup_get` hydration: `default` omits `defined/referenced/called_symbols`; `full` populates them. Segment `summary` remains `None` unconditionally. Trims routine payloads while keeping opt-in symbol depth.
- **symbol_hint** *(new)* — first defined symbol per segment, captured before verbosity gating and `#[serde(skip)]` (never serialized into the payload); the envelope's `next_actions` read it so defining segments keep offering an `oneup_symbol` follow-up even at `default` verbosity.
- **Struct/Enum Field Introspection** *(new)* — the Rust parser now treats `struct_item`/`enum_item` as containers and emits `field_declaration`→`field` and `enum_variant`→`variant` nested segments (role Definition) carrying their doc comments and defined-symbol names, so field/variant docs become searchable.
- **Schema Init Tolerance Window** *(new v0.1.14)* — `ensure_current_tolerating_init` (moved out of `mcp/ops` into `storage/schema` for reuse) rides out the transient "tables exist, `schema_version` row absent" window when a reader races the daemon's first index or a rebuild's atomic swap; retries only that init shape (budget ≈450 ms = `DB_LOCK_RETRY_ATTEMPTS` 10 × `DB_LOCK_RETRY_DELAY_MS` 50 ms), while a genuine version mismatch is a distinct shape that still fails fast on the first attempt.
- **File/Segment Size Caps** *(new v0.1.14)* — `MAX_FILE_SIZE_BYTES` (2 MB, skipped before read) + `MAX_SEGMENTS_PER_FILE` (1000, excess dropped with a warning) bound memory on minified/generated files (prevents the 9.4 MB → 1.49 GB RSS OOM).
- **Expanded Secret Globs** *(new v0.1.14)* — `DEFAULT_SECRET_GLOBS` grows 4 → 19 patterns (API creds, `*service-account*.json`, `.netrc`/`.pgpass`/`.git-credentials`, `.aws/credentials`, SSH/TLS keys `id_rsa*`/`id_ed25519`/`id_ed25519.pub`/`*.p12`/`*.pfx`, `.env.*`), centralized in `shared/constants` and applied non-overridably on both the scan and MCP read paths.
- **Project Gitignore** *(new v0.1.14)* — `ensure_project_gitignore` writes `.1up/.gitignore` = `*` at init/start/index/reindex/MCP auto-init; idempotent, symlink-safe (never clobbers a regular file, refuses a symlink leaf), best-effort so a failure never blocks project resolution.
- **Deleted Source Root Deregistration** *(new v0.1.14)* — the daemon worker detects a deleted `source_root` (main repo where `state_root == source_root`, or a linked worktree whose state_root survives) before the rebuild lock and rechecks on lock-acquisition error, then cleanly deregisters (`Registry` + `unwatch`) and returns default stats so the loop keeps serving other projects (fixes the CPU-spin blocker).

## Terminology

- **context_id / state_root / source_root** — context scoping key; state dir (index DB, project id, daemon status) vs the scanned/watched tree (differ for linked worktrees).
- **readiness status** — ready/missing/indexing/stale/degraded/blocked. **operation status** — ok/empty/partial/degraded. **read status** — found/not_found/ambiguous/rejected/error.
- **head drift** — advisory signal comparing indexed-at HEAD vs live HEAD; appends an `oneup_start` (mode `index_if_needed`) action without changing readiness.
- **serve-stale / stale-but-available** *(new)* — during a rebuild, readers keep serving the prior index; results carry `STALE_REBUILD_REASON` ("index is rebuilding; results may be stale") on the `degraded_reason` channel only.
- **degraded_reason / combine_degraded_reasons** *(new)* — single advisory channel; no-embeddings and rebuild-stale reasons are joined (not dropped) and shared by MCP ops + the daemon worker.
- **build-aside + atomic swap** *(new)* — build into a uuid staging sibling, fold its WAL into one file, atomically rename over `index.db` under the RebuildLock.
- **cooperative cancellation / IndexingError::Cancelled** *(new)* — a `CancellationToken` pass stops at a committed batch boundary (consistent-but-incomplete); SIGTERM cancels it, leaves the context dirty, and resumes next pass.
- **daemon version-handshake** *(new)* — a daemon search response stamps `daemon_version`; the CLI accepts it only when it equals the running `VERSION`, else drains + restarts the daemon under the current binary (local search is the fallback). Prevents serving stale-binary results.
- **daemon idle self-exit** *(new)* — a daemon with zero registered projects self-exits after `DAEMON_IDLE_SHUTDOWN_SECS` (60; override `ONEUP_DAEMON_IDLE_SHUTDOWN_SECS`); a daemon with any project never idles out.
- **mandatory checksum floor** *(new)* — SHA-256 of the downloaded archive verified against the manifest before attestation/activation; mismatch fails closed.
- **keyless-OIDC release attestation** *(new)* — GitHub Sigstore artifact attestation fetched by archive digest and verified against the embedded production trusted root, pinning the OIDC issuer + the project's `release-assets.yml` signer identity (Rekor inclusion proof on; SCT skipped — GitHub certs carry none).
- **three-state verify** *(new)* — verified → proceed; disproved (bad sig/issuer/signer/digest) → fail closed (`AttestationFailed`); cannot-run (offline/404/rate-limited) → degrade to the checksum floor. cannot-run is deliberately distinct from disproof.
- **anti-rollback / anti-freeze gate** *(new)* — `ensure_manifest_acceptable` hard-refuses a manifest version older than installed (`ManifestRollback`) or past expiry+clock-skew (`ManifestExpired`); distinct from the advisory `build_update_status`.
- **yanked / minimum_safe_version / InstallChannel** *(new)* — manifest safety signals → urgent `UpdateStatus`; channel (Homebrew/Scoop/Manual/Unknown) detected from the binary path selects the upgrade instruction.
- **Hybrid Search** — candidate-first fusion of vector + FTS + symbol via weighted RRF (`RRF_K=60`, `VECTOR_WEIGHT=1.5`, `SYMBOL_WEIGHT=4.0`) before hydration.
- **exhaustive scan vs ANN index** *(corrected)* — exact `vector_distance_cos` scan at/below `VECTOR_EXHAUSTIVE_SCAN_MAX_VECTORS` (**now 262144**, raised from 16384), else the disk-based `vector_top_k` DiskANN. The exact scan is a linear pass that stays sub-second past 256k vectors; `vector_top_k` *worsens* with corpus size (~7s @ 4.5k, ~45s @ 27k) — the prior "amortizes at scale" claim was inverted by measurement.
- **VECTOR_PREFILTER_K** — candidate count (400), scaled by indexed-context count up to `VECTOR_PREFILTER_CONTEXT_SCALE_LIMIT` (8).
- **SCHEMA_VERSION** — currently **19** (v17 embedding pool, v18 256-token window/perf, v19 scope metadata); validated before every read/write; older → reindex (fail-closed with `1up reindex` guidance and an MCP `oneup_start {mode: reindex}` next_action), newer → upgrade; no in-place migration.
- **stale-state liveness** *(new)* — REQ-010: rebuild locks older than 5 min with no live holder auto-clear before acquisition; `Running` progress whose `indexer_pid` is dead is treated as missing. Daemons stay SIGTERM-responsive mid-index via the cancellation token (gate walk runs on `spawn_blocking`).
- **valid project marker** *(new)* — a `.1up` ancestor anchors project resolution only if it contains `index.db`/`project_id`/`rebuild.lock`; empty installer cruft cannot hijack resolution (F9).
- **launch_subdir** *(new)* — invocation directory captured before root clamping; threaded through `serve_stdio` and offered as the first scope suggestion.
- **SERVER_GUIDANCE** *(new)* — single-sourced agent routing guidance in `src/mcp/server.rs`, front-loaded to survive 2KB truncation and drift-guarded so every `oneup_*` token it names exists in `RETAINED_PUBLIC_TOOLS`.

## Key Relationships

- `oneup_overview`/`oneup_status`/`oneup_start` **precede** `oneup_search` → emits handles → `oneup_get`/`oneup_impact` **consume** them.
- `WorktreeContext` **scopes** every storage read (via `SearchScope::from_worktree_context`).
- `RebuildLock` **guards** `StagingRebuild.finalize_and_swap`; an in-flight rebuild (lock held or daemon refresh in flight) **triggers** `STALE_REBUILD_REASON`.
- `self_update` **sequences** anti-rollback/expiry gate → checksum floor → three-state attestation → atomic replace.
- CLI search **validates** `daemon_version` == `VERSION` before trusting daemon results.
- `SERVER_GUIDANCE` is **constrained by** `RETAINED_PUBLIC_TOOLS` (drift-guard test).
- First-Index Gate **emits** Facts Envelope; a scoped `oneup_start` **records** scope (progress decision + DB meta) which the daemon refresh **reads** (meta, falling back to the recorded decision) and **re-persists** — every rebuild path writes scope to the staging connection so `finalize_and_swap` preserves it.
- `ScanFilter` precedence: secrets > scope_globs (exclusive cone) > include_globs/override dirs > exclude_globs > dotfile hiding — configured includes cannot punch through the cone; the secrets tier is the non-overridable `DEFAULT_SECRET_GLOBS` (4 → 19), shared by the scanner and the `oneup_context` read path.
- `warm_index_connection` and every CLI read command (`search`/`status`/`get`/`symbol`/`impact`/`list`/`structural`) **use** `ensure_current_tolerating_init`, so a read right after `reindex` (or during a daemon rebuild) rides out the init window instead of surfacing a spurious "reindex required".
- `oneup_get` **gates** symbol lists by `verbosity`; the envelope's `next_actions` still **read** `symbol_hint` so a defining segment keeps proposing `oneup_symbol` at `default` verbosity.

## Bounded Contexts

1. **Code Discovery (MCP)** — `src/mcp`: nine `oneup_*` tools, `ToolEnvelope`/`next_actions`, status enums, single-sourced guidance.
2. **Search & Retrieval** — `src/search`: hybrid + RRF, corpus-adaptive vector path (262144), impact + corroboration, overview.
3. **Index Storage & Lifecycle** — `src/storage`/`src/indexer`: schema v19, content-addressed embedding pool, build-aside swap, cooperative cancellation, exclusive scope-cone scanning, size/segment caps, field/variant introspection, init-tolerant schema validation.
3a. **Monorepo Scoping & Policy** — `src/mcp/ops.rs` + `src/daemon/{lifecycle,worker}.rs` + `src/shared/project.rs`: gate logic, facts envelope, scope persistence/carry, launch_subdir, marker-validated resolution.
4. **Daemon & Concurrency** — `src/daemon`: single-writer lock, version-handshake, idle self-exit, inode-swap detection, serve-stale, deleted-source-root deregistration.
5. **Supply-Chain Trust & Self-Update** — `src/shared/{update,constants,errors}`: manifest, checksum floor, three-state attestation, anti-rollback/expiry, yanked/minimum-safe, InstallChannel.

## Cross-Cutting Concerns

- **Degradation reporting** — one `degraded_reason` channel (never the result stream), combined + sourced from shared constants so CLI/MCP/daemon can't drift.
- **Fail-closed with graceful degrade** — schema mismatch + attestation disproof abort; genuine cannot-run (offline, in-flight rebuild) degrades to a safe served path.
- **Atomic, torn-read-safe persistence** — temp+fsync+rename / finalize-and-swap with owner-only modes.
- **Worktree-context isolation** — `context_id` threads every read so linked worktrees share one DB without cross-contamination.
- **Cross-process coordination** — single-writer lock + version-handshake + non-blocking lock probes coordinate CLI/daemon/MCP without serving wrong-version or torn results.
- **Bounded resource use** *(new v0.1.14)* — per-file size/segment caps skip pathological minified/generated files before they OOM; a deleted source root deregisters (returning default stats) instead of spinning the daemon loop.
- **Non-overridable secret exclusion** *(new v0.1.14)* — `DEFAULT_SECRET_GLOBS` (centralized in `shared/constants`) is enforced regardless of config on both scan and MCP read paths, above every include/scope tier.
- **Hermetic testing** — `ONEUP_DISABLE_MODEL_DOWNLOADS`; attestation verify is pure/offline against an embedded root; gates take `now`/version as params.
