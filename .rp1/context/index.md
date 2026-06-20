# 1up - Knowledge Base Index

**What**: A local-first code discovery substrate that indexes repositories with tree-sitter parsing, ONNX embeddings, libSQL FTS/vector storage, and relation metadata, then exposes search, get, symbol, context, structural, impact, overview, indexing, daemon, and MCP workflows through a single Rust binary.

**Why**: It gives humans and agents a fast, local, evidence-oriented path from ranked code discovery to exact source hydration, symbol verification, and bounded likely-impact exploration without relying on broad raw search as the first step.

## Quick Reference

| Attribute | Value |
|---|---|
| Type | Single project |
| Entry point | `src/main.rs` -> `src/cli/mod.rs` |
| Primary agent surface | `1up mcp --path <repo>` exposing `oneup_status`, `oneup_start`, `oneup_overview`, `oneup_search`, `oneup_get`, `oneup_symbol`, `oneup_context`, `oneup_impact`, `oneup_structural` |
| Key patterns | Layered CLI + MCP + daemon, orientation-before-discovery, search-before-get/context, candidate-first retrieval, build-aside rebuild + atomic swap, single-writer rebuild lock, daemon version-handshake, independent-channel supply-chain trust, single-source-of-truth tool guidance |
| Tech stack | Rust, Tokio, libSQL, ONNX Runtime, tree-sitter, rmcp, clap, sigstore-verify, TypeScript evals, shell release scripts |
| Version | 0.1.12 |
| Schema version | 16 (`FLOAT8(384)`, `vector8(?)`, `compress_neighbors=float8`, `max_neighbors=32`, `VECTOR_PREFILTER_K=400`) |
| Last generated | 2026-06-20T09:44:31Z |

## KB File Manifest

| File | Lines | Load For |
|---|---:|---|
| [concept_map.md](concept_map.md) | 73 | Domain terminology, code-discovery + supply-chain concepts, MCP tool vocabulary, storage/search/impact relationships |
| [architecture.md](architecture.md) | 101 | System topology, data/state layout, MCP/CLI/daemon flows, rebuild/swap, release & update architecture |
| [interaction-model.md](interaction-model.md) | 70 | Agent and CLI interaction semantics, readiness/update states, output contracts, setup flows |
| [modules.md](modules.md) | 80 | Component ownership, module dependencies, public boundaries, metrics |
| [patterns.md](patterns.md) | 75 | Conventions, data modeling, errors, validation, output, storage, concurrency, testing idioms |

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

- **Supply-chain trust**: self-update (`src/shared/update.rs`) verifies a mandatory-when-published SHA-256 checksum, then a keyless-OIDC GitHub build attestation as a three-state gate (verified -> proceed; disproved -> fail closed; cannot-run/offline -> degrade to the checksum floor). `ensure_manifest_acceptable` hard-refuses rolled-back or past-expiry manifests before download. `release-assets.yml` emits `actions/attest-build-provenance`; `setup.sh` does the checksum mandatorily + attestation opt-in (gh/cosign).
- **Daemon version-handshake**: a daemon search response stamps `daemon_version`; a stale-binary daemon's results are refused, the daemon is drained + restarted under the current binary, and local in-process search is the fallback (never serve stale results). A single-writer `RebuildLock` (`.1up/rebuild.lock`) ensures exactly one process owns a rebuild; a cooperative `CancellationToken` lets SIGTERM interrupt indexing at a safe boundary.
- **Non-destructive rebuild**: `src/storage/swap.rs` builds a fresh index into a uuid-suffixed staging DB and atomically swaps it over `index.db`; search stays available (stale-but-served via `STALE_REBUILD_REASON` on `degraded_reason`). An empty daemon self-exits after `DAEMON_IDLE_SHUTDOWN_SECS` (60s).
- **Vector search**: `VECTOR_EXHAUSTIVE_SCAN_MAX_VECTORS` is now **262144** (was 16384) — the disk-based `vector_top_k` DiskANN path *worsens* with corpus size (~7s @ 4.5k, ~45s @ 27k), so the exact `vector_distance_cos` scan is preferred for all realistic single-repo sizes.
- **Agent guidance** is single-sourced in `src/mcp` (`SERVER_GUIDANCE`, per-tool descriptions, `next_actions`); `RETAINED_PUBLIC_TOOLS` (nine tools incl. `oneup_overview`) is the single authority, drift-guarded by a doc-token test. `1up doctor --clean-hints` cleans legacy pasted hints (opt-in, preview-first, fence-only).
- Schema is **v16**; incompatible indexes fail closed with `1up reindex` / upgrade guidance (no in-place migration).
- Search remains candidate-first hybrid (vector + FTS + symbol via RRF); `state_root`/`source_root` split via a `WorktreeContext` (`context_id`) keeps linked worktrees isolated in one shared index. Impact stays local-only and advisory (primary vs contextual results never collapse).

## Project Structure

```text
src/
  cli/       # Human CLI, lean/human/json output, doctor --clean-hints, MCP launch/setup, reindex (build-aside)
  mcp/       # rmcp stdio server, 9 tools, ops adapters, structured envelopes, RETAINED_PUBLIC_TOOLS, SERVER_GUIDANCE
  search/    # Hybrid retrieval, ranking, intent, scope, symbol, context, structural, impact, overview
  indexer/   # Scan, parse/chunk, markdown doc-sections, embed, prefilter, cancellable pipeline
  storage/   # libSQL schema v16, SQL, segments, vectors, relations, manifest, build-aside swap
  daemon/    # Registry, lifecycle (rebuild lock, drain/restart), watcher, worker, search IPC, stubs
  shared/    # Types, config, project/worktree roots, secure FS, constants, errors, self-update trust
tests/       # CLI/MCP/release/setup/security regression suites
evals/       # Promptfoo/TypeScript search and MCP adoption evals
scripts/     # Benchmarks, installer, release, security, MCP smoke automation
```

## How To Load

Agents load this KB automatically via progressive disclosure:

1. Read `index.md` first.
2. Load only the files needed for the current task.
3. Avoid loading the full KB unless the work is system-wide.
