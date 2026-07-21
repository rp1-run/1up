# 1up - Knowledge Base Index

**What**: A local-first code discovery substrate that indexes repositories with tree-sitter parsing, ONNX embeddings, libSQL FTS/vector storage, and relation metadata, then exposes search, get, symbol, context, structural, impact, overview, indexing, daemon, and MCP workflows through a single Rust binary.

**Why**: It gives humans and agents a fast, local, evidence-oriented path from ranked code discovery to exact source hydration, symbol verification, and bounded likely-impact exploration without relying on broad raw search as the first step.

## Quick Reference

| Attribute | Value |
|---|---|
| Type | Single project |
| Entry point | `src/main.rs` -> `src/cli/mod.rs` |
| Primary agent surface | `1up mcp --path <repo>` exposing `oneup_status`, `oneup_start`, `oneup_overview`, `oneup_search`, `oneup_get`, `oneup_symbol`, `oneup_context`, `oneup_impact`, `oneup_structural` |
| Key patterns | Layered CLI + MCP + daemon, orientation-before-discovery, search-before-get/context, candidate-first retrieval (exact vector scan, DiskANN removed), build-aside rebuild + atomic swap, single-writer rebuild lock, git-stamped build-identity handshake, refuse-and-propose monorepo gate (daemon/MCP parity) + exclusive scope cones, non-blocking bounded-wait start, hard request/file caps, three-state presence/status probes, provably-safe lock reaping, identity-keyed registry dedup, reverse-pointer worktree trust, fail-closed supply-chain trust, single-source-of-truth tool guidance |
| Tech stack | Rust, Tokio, libSQL, ONNX Runtime, tree-sitter, rmcp, clap, globset, sigstore-verify, TypeScript evals (bun), shell release scripts |
| Version | 0.1.17 |
| Schema version | 20 (v17 embedding pool, v18 256-token window, v19 scope metadata, **v20 drops the DiskANN index — exact `vector_distance_cos` scan is the only vector path**; `VECTOR_PREFILTER_K=400`) |
| Last generated | 2026-07-21 |

## KB File Manifest

| File | Lines | Load For |
|---|---:|---|
| [concept_map.md](concept_map.md) | 109 | Domain terminology, scoping + code-discovery + supply-chain concepts, MCP tool vocabulary, build identity, batch caps, registry identity, storage/search/impact relationships |
| [architecture.md](architecture.md) | 126 | System topology, data/state layout, MCP/CLI/daemon flows, build-identity handshake, worktree trust verification, monorepo gate parity, rebuild/swap, release & update architecture |
| [interaction-model.md](interaction-model.md) | 85 | Agent and CLI interaction semantics, readiness/scope states, disclosure hints, three-state reads, non-blocking start + polling, output contracts, setup flows |
| [modules.md](modules.md) | 103 | Component ownership, module dependencies, public boundaries, new lock-reap/eval subsystems, metrics |
| [patterns.md](patterns.md) | 92 | Conventions, data modeling, errors, validation, observability, storage, concurrency, testing idioms |

## Task-Based Loading

| Task | Load Files |
|---|---|
| Code review | `patterns.md` |
| Bug investigation | `architecture.md`, `modules.md` |
| Feature work | `modules.md`, `patterns.md` |
| Search, ranking, or symbol changes | `concept_map.md`, `architecture.md`, `interaction-model.md`, `patterns.md` |
| MCP or CLI surface changes | `interaction-model.md`, `modules.md`, `patterns.md` |
| Indexing, storage, schema, vector, swap, or daemon changes | `concept_map.md`, `architecture.md`, `modules.md`, `patterns.md` |
| Release, install, update, attestation, or eval changes | `architecture.md`, `concept_map.md`, `modules.md` |
| Strategic or system-wide analysis | All files |

## Recent Learnings

- **DiskANN removed — schema v20 (v0.1.15–v0.1.17)**: the DiskANN graph index, its `_shadow` table, the `Immediate`/`Deferred` build-before-swap machinery, `ONEUP_FORCE_ANN_SEARCH`, and `vector_top_k` queries are deleted wholesale; exact `vector_distance_cos` scan is the only vector path at every corpus size (measured linear ~0.9µs/vector: ~86ms@100k, ~446ms@500k vs the ANN path's superlinear ~7s@4.5k, ~45s@27k and ~109MiB overhead — `docs/diskann-removal.md`). `VECTOR_EXHAUSTIVE_SCAN_MAX_VECTORS` became `VECTOR_EXACT_SCAN_WARN_THRESHOLD` (262144, one-time advisory warn only). v19 indexes fail closed → `1up reindex`.
- **Build identity replaces bare semver (issue #108)**: new `build.rs` stamps `ONEUP_BUILD_IDENTITY = {semver}+{git-short}[.dirty[.digest]]`; `daemon_response_is_authoritative` requires exact equality with the running binary's `BUILD_IDENTITY` — same-semver-different-build or an absent stamp is refused, drained, and restarted (previously an absent version was trusted). Regression suite `tests/build_identity_tests.rs` stamps a throwaway probe crate with the real build script.
- **State-root/trust hardening**: `resolve_linked_worktree_info` (e8ea203) requires anchoring (`git_dir` parent == `<commondir>/worktrees/`) + a reverse pointer (`<git_dir>/gitdir` canonicalizes back to the exact `.git` file) before adopting a commondir-derived `main_root` — a forged `.git` file can no longer redirect `.1up/` state into a victim repo. `setup.sh` (04e6663) now fails closed on an unpublished `SHA256SUMS` (was warn-and-continue); `ONEUP_SKIP_CHECKSUM=1` is the loud opt-out; default install dir moved to `$HOME/.local/bin`.
- **MCP surface hardening + turn efficiency**: `oneup_get` handle batches hard-capped (`MAX_GET_HANDLES_PER_CALL`=50, `MAX_GET_REQUEST_HANDLE_BYTES`=16KiB, checked before any index open; a 50k-handle batch used to yield ~12.9MB) with a 2MiB response budget; failed handles get classification + unique-prefix recovery + a process-global `FailedHandleMemory`; `oneup_search` accepts up to 4 `queries` fused via RRF; envelope compaction keeps summaries constant-sized with `TruncationNote` + capped recovery next_actions; `classify_query_token` replaces the blunt prose heuristic in ranking.
- **Daemon correctness sweep**: registry entries dedup on every load by `EntryIdentity` (project_root, source_root, branch_ref — excluding head_oid, issue #116's duplicate-per-commit root cause); the first-index gate walk is single-sourced with the MCP VCS-dir exclusion so the two gates can never disagree (bfc181b), and the gated file count is cached; daemon startup conservatively auto-prunes stale-branch snapshots of live worktrees; `status`/`list` disclose reclaimable stale-branch accumulation above floors ("run 1up gc").
- **Never destructively act on ambiguous evidence** (generalized): gc/daemon source checks use three-state `SourcePresence` (Indeterminate retains + warns, never prunes); status-file reads use three-state `StatusFileRead` (torn reads retry 3×50ms, then error-log — never fabricate empty progress); new `shared::lock_reap` (~920 lines) reaps stale `mcp-*/startup-*.lock` files (issue #117: 4076 accumulated) only after age + flock probe + post-lock dev/ino identity re-check, bounded at 128 candidates/250ms; lock acquirers self-heal against reaper races (`flock_still_names_path`).
- **Eval system rebuilt (Luna)**: `evals/suites/shared/{manifest,axes-report}.ts` freeze prompts/fixtures/grader into a `contract_hash` and score independent per-axis results (factual/retrieval/adoption/reliability + efficiency), replacing one blended promptfoo score; baselines only compare against a matching hash. Recall gate baselines must be captured on the CI runner platform (`embedding-quality.yml` `capture_baseline` dispatch) — int8 inference differs ~2.2pp between macOS-arm64 and the Linux gate, exceeding the 0.02 tolerance. CI now executes the Windows ancestor-guard self-test (`verify_mcp_smoke.sh --self-test-only`).
- **Monorepo-scoped indexing (v0.1.13–14, still current)**: over-threshold repos (`ONEUP_FILE_COUNT_THRESHOLD`, 3000) refuse a first unscoped index; `oneup_start`/`1up start --scope` return a facts envelope with ranked, gitignore-aware scope suggestions; scope roots persist in DB meta (`scope_roots_v1`) as exclusive `ScanFilter` cones (secrets > scope > include > exclude > dotfile); widening incremental, narrowing atomic rebuild. Per-file caps (2MB/1000 segments) and the 19-pattern non-overridable `DEFAULT_SECRET_GLOBS` bound the scan.
- **Supply-chain trust (self-update)**: `ensure_manifest_acceptable` (anti-rollback/expiry) → mandatory SHA-256 floor → keyless-OIDC attestation as a three-state gate (verified/disproved-fail-closed/cannot-run-degrade); schema-init reads ride a dedicated ≈5s tolerance budget (`SCHEMA_INIT_WAIT_ATTEMPTS`=50×100ms) distinct from the DB-lock retry budget.
- Search remains candidate-first hybrid (vector + FTS + symbol via weighted RRF); `WorktreeContext` (`context_id`) keeps linked worktrees isolated in one shared index; impact stays local-only and advisory.

## Project Structure

```text
src/
  cli/       # Human CLI, lean/human/json output (+ disclosure hints), doctor, gc (3-state prune), MCP launch, reindex (build-aside), lock self-heal
  mcp/       # rmcp stdio server, 9 tools, ops adapters, batch caps, handle recovery, multi-query fusion, RETAINED_PUBLIC_TOOLS, SERVER_GUIDANCE
  search/    # Hybrid retrieval, ranking (token-level intent), scope, symbol, context, structural, impact, overview; exact vector scan
  indexer/   # Scan (exclusive scope cones), parse/chunk, markdown doc-sections, embed (pool dedup), prefilter, cancellable pipeline
  storage/   # libSQL schema v20 (no DiskANN), SQL, segments (+ stale-branch predicate), embedding pool, relations, scope meta, build-aside swap
  daemon/    # Registry (EntryIdentity dedup), lifecycle (rebuild lock, drain/restart), watcher, worker (gate parity, auto-prune), search IPC, stubs
  shared/    # Types, config, project/worktree roots (reverse-pointer trust), secure FS (SourcePresence), constants (BUILD_IDENTITY), lock_reap, progress, self-update trust
build.rs     # Git-stamped ONEUP_BUILD_IDENTITY
tests/       # CLI/MCP/release/setup/security/build-identity regression suites
evals/       # Contract-hashed Luna eval harness (per-axis baselines), recall gate, promptfoo suites
scripts/     # Benchmarks, installer (fail-closed checksum), release, security, MCP smoke automation
```

## How To Load

Agents load this KB automatically via progressive disclosure:

1. Read `index.md` first.
2. Load only the files needed for the current task.
3. Avoid loading the full KB unless the work is system-wide.
