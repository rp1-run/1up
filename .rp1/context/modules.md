# 1up - Modules

## Top-Level Modules

| Module | Purpose | Key Files |
|---|---|---|
| `src/lib.rs`, `src/main.rs` | Crate export surface and binary boot path. `main` initializes tracing, runs CLI dispatch, suppresses passive update notices for MCP/internal/update-safe paths, and leaves module ownership below the directory modules. | `src/main.rs`, `src/lib.rs` |
| `src/cli` | User-facing command surface. Retained human discovery commands (`get`, `symbol`, `impact`, `context`) default to readable output and expose `--plain` lean output where supported; hidden compatibility discovery commands (`search`, `structural`) stay lean-only. Maintenance commands (`init`, `index`, `reindex`, `start`, `stop`, `status`, `list`, `update`) render via `output` with worktree/context metadata. Also owns `add-mcp` and `mcp` launch/setup commands. | `src/cli/mod.rs`, `src/cli/discovery_output.rs`, `src/cli/lean.rs`, `src/cli/output.rs`, `src/cli/start.rs`, `src/cli/list.rs`, `src/cli/add_mcp.rs`, `src/cli/mcp.rs` |
| `src/mcp` | Model Context Protocol stdio server for agent-facing code discovery. Exposes readiness/start, ranked search, handle hydration, location context, symbol lookup, likely-impact, and structural tools with structured envelopes and next-action guidance. | `src/mcp/server.rs`, `src/mcp/tools.rs`, `src/mcp/ops.rs`, `src/mcp/types.rs` |
| `src/search` | Retrieval and follow-up engines: hybrid semantic/FTS/symbol ranking, context retrieval, structural AST matching, symbol reference lookup, and bounded likely-impact expansion with primary/contextual trust buckets. | `src/search/hybrid.rs`, `src/search/retrieval.rs`, `src/search/ranking.rs`, `src/search/symbol.rs`, `src/search/impact.rs`, `src/search/context.rs`, `src/search/structural.rs` |
| `src/indexer` | Repository scan, parse/chunk, embed, and storage pipeline. Full and scoped runs use context-scoped `indexed_files` metadata prefiltering, context-derived segment IDs, parser-derived relation evidence, optional embeddings, progress persistence, and batched writes. | `src/indexer/pipeline.rs`, `src/indexer/parser.rs`, `src/indexer/embedder.rs`, `src/indexer/scanner.rs`, `src/indexer/chunker.rs` |
| `src/storage` | libSQL persistence boundary. Owns schema v13, context-scoped rows, vector storage, FTS, symbols, relation descriptors, `indexed_files` manifest, tuned project-local connections, schema compatibility checks, and transactional replace/delete APIs. | `src/storage/schema.rs`, `src/storage/queries.rs`, `src/storage/segments.rs`, `src/storage/relations.rs`, `src/storage/db.rs` |
| `src/daemon` | Background indexing/search service with secure Unix IPC, context-aware registry-backed project watching, daemon lifecycle/lock handling, same-UID socket permissions, and non-Unix stubs. | `src/daemon/worker.rs`, `src/daemon/lifecycle.rs`, `src/daemon/search_service.rs`, `src/daemon/watcher.rs`, `src/daemon/registry.rs`, `src/daemon/ipc.rs` |
| `src/shared` | Cross-layer contracts and utilities: result types, progress telemetry, WorktreeContext, config paths, errors, secure filesystem operations, project identity/worktree resolution, symbol normalization, and self-update helpers. | `src/shared/types.rs`, `src/shared/config.rs`, `src/shared/project.rs`, `src/shared/fs.rs`, `src/shared/update.rs`, `src/shared/symbols.rs` |
| `tests` | Black-box and focused regression coverage for CLI, MCP, daemon behavior, index/search correctness, release assets, installer script, security check, and SQL rewrite invariants. | `tests/integration_tests.rs`, `tests/cli_tests.rs`, `tests/release_assets_tests.rs`, `tests/setup_script_tests.rs`, `tests/security_check_tests.rs` |
| `benches` | Criterion guardrails for symbol lookup, FTS, chunked content search, retrieval backend selection, and impact horizon behavior. | `benches/search_bench.rs` |
| `evals` | TypeScript/promptfoo evaluation support for search quality, recall, MCP tool-use assertions, fixture-cache setup, and search benchmark comparisons. | `evals/suites/1up-search/recall.ts`, `evals/suites/1up-search/search-bench.ts`, `evals/suites/shared/assertions/index.ts`, `evals/suites/shared/extension.ts` |
| `scripts`, `.lefthook`, `packaging` | Operational automation: indexing/vector benchmarks, installer, security evidence, release manifest/archive/package publication, Homebrew/Scoop rendering, MCP smoke verification/recording, and main-branch protection. | `scripts/install/setup.sh`, `scripts/security_check.sh`, `scripts/benchmark_vector_index_size.sh`, `scripts/release/`, `packaging/homebrew/1up.rb.tmpl`, `packaging/scoop/1up.json.tmpl` |

## Key Components

| Component | File | Responsibility | Depends On |
|---|---|---|---|
| `Cli` / `Command` | `src/cli/mod.rs` | Clap dispatch, visible help command list, and default maintenance format resolution. Public help shows lifecycle commands plus retained human discovery commands: `get`, `symbol`, `context`, and `impact`. Hidden commands include `add-mcp`, `init`, `search`, `structural`, `mcp`, `index`, `reindex`, `update`, and internal `__worker`. | `src/cli/*`, `src/shared/types.rs` |
| `DiscoveryOutput` | `src/cli/discovery_output.rs` | Human-readable default rendering for retained discovery commands, including hydrated segments, symbol matches, source context, and advisory likely-impact output. | `src/cli/lean.rs`, `src/search/impact.rs`, `src/shared/types.rs`, `src/storage/segments.rs` |
| `LeanRenderer` | `src/cli/lean.rs` | Stable lean grammar for hidden discovery commands and retained command `--plain` output, including search/symbol rows, get records, context blocks, structural snippets, and impact `~P`/`~C` channels. | `src/search/impact.rs`, `src/shared/types.rs`, `src/storage/segments.rs` |
| `Formatter` | `src/cli/output.rs` | Human/plain/json maintenance output plus progress/watch rendering for init/index/reindex/start/stop/status/list/update, including source root, context id, branch, watch status, and last-update metadata. | `src/shared/types.rs`, `src/shared/progress.rs`, `src/shared/update.rs` |
| `StartCommand` | `src/cli/start.rs` | Guarded daemon startup. Resolves project identity and `WorktreeContext`, prepares schema/model setup timings, avoids unnecessary foreground indexing on current context indexes, registers daemon settings, and observes final daemon pid. | `src/daemon/lifecycle.rs`, `src/daemon/registry.rs`, `src/indexer/pipeline.rs`, `src/shared/project.rs` |
| `AddMcpCommand` | `src/cli/add_mcp.rs` | Wrapper around `bunx`/`npx add-mcp`; builds the local `1up mcp --path ...` server command and prints manual fallback snippets on setup failure. | external `add-mcp`, shell runners |
| `McpCommand` | `src/cli/mcp.rs` | Starts the MCP stdio server for a resolved project/worktree, enforces one MCP instance per project via secure lock file, and best-effort starts the daemon for MCP search. | `src/mcp/server.rs`, `src/daemon/lifecycle.rs`, `src/shared/project.rs`, `src/shared/fs.rs` |
| `OneupMcpServer` | `src/mcp/server.rs`, `src/mcp/tools.rs` | rmcp server implementation exposing the retained eight-tool inventory: `oneup_status`, `oneup_start`, `oneup_search`, `oneup_get`, `oneup_symbol`, `oneup_context`, `oneup_impact`, and `oneup_structural`. Adds server guidance that MCP search should precede broad grep/rg/find. | `rmcp`, `src/mcp/ops.rs`, `src/mcp/types.rs` |
| `McpOps` | `src/mcp/ops.rs` | Pure operation layer for product-named readiness, start/indexing, search, handle hydration, file-line context, symbol lookup, impact exploration, and structural search over the current local index. | `src/search/*`, `src/storage/*`, `src/indexer/embedder.rs`, `src/shared/project.rs` |
| `ToolEnvelope` / `NextAction` | `src/mcp/types.rs` | MCP input/output schemas with deny-unknown-fields inputs, retained tool constants, structured `status`, `summary`, `data`, and canonical follow-up actions. | `serde`, `schemars`, `rmcp` |
| `HybridSearchEngine` | `src/search/hybrid.rs` | Embeds queries when possible, combines vector, FTS, and symbol candidates, falls back to FTS if vector search fails, ranks candidates, and hydrates lean search results. | `src/search/retrieval.rs`, `src/search/ranking.rs`, `src/search/symbol.rs`, `src/indexer/embedder.rs`, `src/storage/segments.rs` |
| `RetrievalBackend` | `src/search/retrieval.rs` | Chooses SQL vector v2 when embeddings exist and FTS-only otherwise; returns bounded candidate sets for ranking. | `src/storage/queries.rs`, `src/shared/constants.rs` |
| `SymbolSearchEngine` | `src/search/symbol.rs` | Finds definitions/usages with exact, prefix, contains, and fuzzy fallback matching over normalized symbols. | `src/storage/queries.rs`, `src/storage/segments.rs`, `src/shared/symbols.rs` |
| `ImpactHorizonEngine` | `src/search/impact.rs` | Bounded advisory impact from file, line, symbol, or result-handle anchors. Uses relation lookup tails, owner fingerprints, edge identity, path affinity, role signals, scope checks, and test-path guidance to split primary and contextual results. | `src/storage/relations.rs`, `src/storage/segments.rs`, `src/search/symbol.rs`, `src/shared/symbols.rs` |
| `ContextEngine` | `src/search/context.rs` | Reads source context around a location and prefers enclosing tree-sitter scopes, with explicit outside-root access handling. | `src/indexer/parser.rs`, `src/shared/types.rs` |
| `StructuralSearchEngine` | `src/search/structural.rs` | Runs tree-sitter query patterns over context-scoped candidate files and reports ok, empty, or error diagnostics for MCP structural search. | `tree-sitter`, `src/storage/segments.rs` |
| `Pipeline` | `src/indexer/pipeline.rs` | Full/scoped indexing with WorktreeContext, metadata prefilter, deleted-file cleanup, parse worker ordering, optional embedding, context-derived segment IDs, batched file replacement, progress telemetry, and separate scan/progress roots for worktrees. | `src/indexer/{scanner,parser,chunker,embedder}.rs`, `src/storage/*`, `src/shared/types.rs` |
| `Parser` | `src/indexer/parser.rs` | Multi-language tree-sitter parser for structural segments, complexity, roles, symbols, references, calls, conformance relations, owner/edge evidence, and text fallback decisions. | tree-sitter grammars, `src/shared/symbols.rs` |
| `Embedder` / `EmbeddingRuntime` | `src/indexer/embedder.rs` | Verified local ONNX/tokenizer artifact lifecycle, secure model roots, download/activation, warm runtime reuse, batch embeddings, and degraded-mode status. | `ort`, `tokenizers`, `reqwest`, `sha2`, `src/shared/fs.rs` |
| `Schema` | `src/storage/schema.rs` | Initializes/rebuilds/validates schema v13, checks worktree/context objects and required columns, stores embedding model metadata, and fails closed with `1up reindex` guidance for stale/incompatible indexes. | `src/storage/queries.rs`, `src/shared/constants.rs` |
| `Segments` | `src/storage/segments.rs` | Stores and hydrates segments, generates context-derived IDs, resolves 12-char handles, replaces file batches transactionally, synchronizes vectors/symbols/relations, and maintains context-scoped `indexed_files`. | `src/storage/relations.rs`, `src/storage/queries.rs`, `src/shared/types.rs` |
| `Relations` | `src/storage/relations.rs` | Persists call/reference/conformance descriptors with canonical target, lookup tail, qualifier fingerprint, and edge identity; serves outbound and inbound lookups. | `src/shared/symbols.rs`, `src/storage/queries.rs` |
| `DaemonWorker` | `src/daemon/worker.rs` | Loads registered contexts, watches source roots, batches dirty scopes, indexes incrementally, serves bounded context-scoped search requests, persists heartbeat/context status, and records fallback reasons. | `src/daemon/{lifecycle,registry,watcher,search_service}.rs`, `src/indexer/pipeline.rs`, `src/search/hybrid.rs` |
| `SearchService` | `src/daemon/search_service.rs` | Secure Unix-domain daemon search transport with framed JSON, context/source request fields, request sanitization, version metadata, timeouts, busy/unavailable responses, and socket cleanup. | `src/daemon/ipc.rs`, `src/shared/constants.rs` |
| `Registry` | `src/daemon/registry.rs` | Concurrent-safe context-aware project registry. Register/deregister lock, reload, mutate, and atomically save project, source, worktree, branch, and indexing metadata so startup paths do not drop entries. | `src/shared/fs.rs`, `src/shared/types.rs` |
| `ProjectResolution` | `src/shared/project.rs` | Project ID creation, initialized-state checks, WorktreeContext construction, source-root/state-root resolution, and git-worktree mapping to main repo state. | `src/shared/config.rs`, `src/shared/fs.rs` |

## Internal Dependency Chains

- `src/main.rs` -> `src/cli` and `src/shared/update`: binary boot, dispatch, and passive update notification handling.
- `src/cli` -> `src/search`/`src/storage`: core commands open the project-local index, validate schema, run context-scoped engines, and render retained human output or lean `--plain` output.
- `src/cli/mcp.rs` -> `src/mcp`: CLI starts the stdio MCP server and best-effort daemon support.
- `src/mcp` -> `src/search`/`src/storage`/`src/indexer`: MCP tools share local status/start, search, get, context, symbol, impact, structural, and explicit indexing operations with the CLI stack.
- `src/search` -> `src/storage`: retrieval, symbol, context, structural, and impact engines hydrate segments and relation/symbol tables at query time.
- `src/search` -> `src/indexer/embedder.rs`: hybrid search reuses the embedding runtime for query vectors.
- `src/indexer` -> `src/storage`: pipeline writes context-scoped segment, vector, symbol, relation, and manifest rows through transactional storage APIs.
- `src/daemon` -> `src/indexer`/`src/search`: worker refreshes context indexes on file changes and serves daemon-backed search requests.
- `src/shared` -> all runtime modules: shared types, constants, filesystem security, WorktreeContext, project identity, update, symbol normalization, and error contracts.
- `tests`, `benches`, `evals`, and `scripts` -> binary/library surfaces: black-box CLI/MCP tests, direct engine benches, prompt/eval harnesses, and release/installer/security automation.

## External Dependencies

| Dependency | Purpose |
|---|---|
| `tokio`, `futures-util` | Async runtime, daemon/MCP/server tasks, channels, timeouts, and async IO. |
| `clap` | CLI parser and subcommand definitions. |
| `libsql` | Project-local SQLite/libSQL storage, FTS, vector index, and transactions. |
| `tree-sitter-*`, `streaming-iterator` | Multi-language parsing and structural query support. |
| `ort`, `tokenizers`, `reqwest` | ONNX embedding runtime, tokenizer loading, model download/update HTTP clients. |
| `rmcp`, `schemars`, `serde`, `serde_json` | MCP server, JSON schemas, structured tool envelopes, and persisted metadata. |
| `notify`, `ignore` | Daemon file watching and gitignore-aware scans. |
| `nix`, `dirs`, `indicatif`, `tracing`, `chrono`, `uuid`, `sha2` | Process/signal locks, XDG paths, progress UI, logging, timestamps, IDs, hashing. |
| `semver`, `flate2`, `tar`, `zip` | Self-update and release archive handling. |
| `assert_cmd`, `predicates`, `criterion`, `tempfile`, `libc` | Tests and benchmarks. |
| `promptfoo`, Bun/TypeScript tooling | Agent/search evals and MCP tool-use assertions. |

## Metrics

| Module | Files Analyzed | Lines Analyzed | Component Count |
|---|---:|---:|---:|
| `src root` | 2 | 107 | 2 |
| `src/cli` | 18 | 5,737 | 13 commands/renderers |
| `src/mcp` | 5 | 1,892 | 8 tools + ops/types/server |
| `src/search` | 10 | 7,223 | 7 engines/helpers |
| `src/indexer` | 6 | 7,650 | 5 pipeline stages |
| `src/storage` | 6 | 4,160 | 5 persistence components |
| `src/daemon` | 10 | 3,440 | 6 runtime components/stubs |
| `src/shared` | 10 | 4,679 | 8 shared contract/helper areas |
| `tests` | 7 | 8,069 | CLI/MCP/release/setup/security suites |
| `benches` | 1 | 1,033 | Criterion suite |
| `evals` | 8 | 2,489 | recall, benchmark, fixture, assertion helpers |
| `scripts` | 20 | 3,733 | benchmark/install/release/security helpers |
| `.lefthook`, `packaging` | 3 | 77 | git hook + package templates |

## Public Boundaries

### CLI Boundary

- Public help-visible commands: `start`, `status`, `list`, `stop`, `get`, `symbol`, `context`, `impact`.
- Hidden compatibility/setup/maintenance commands: `add-mcp`, `init`, `search`, `structural`, `mcp`, `index`, `reindex`, `update`.
- Hidden internal command: `__worker`.
- Removed boundary: prior `hello-agent` is no longer present; tests assert its removal.
- Retained human discovery commands default to readable output and intentionally reject legacy `--format` flags; `--plain` delegates to the lean protocol when present.
- Hidden `search` and `structural` remain available for compatibility but are not advertised as supported P4 human commands.
- Maintenance command output supports `plain`, `human`, and `json`; status/list/start outputs include context-aware source root, worktree, branch, watch, and update metadata, and JSON maintenance output suppresses passive update notices.
- `impact` remains local-index only and requires exactly one public anchor: handle, symbol, or file/line; hidden segment input is compatibility-only.

### MCP Boundary

- Server: `1up mcp --path <repo-or-worktree>` over stdio with one instance per project lock.
- Tools: `oneup_status`, `oneup_start`, `oneup_search`, `oneup_get`, `oneup_symbol`, `oneup_context`, `oneup_impact`, `oneup_structural`.
- Start modes: `index_if_needed` (alias `auto`), `index_if_missing`, and `reindex`; readiness checks without indexing use `oneup_status`.
- Tool output contract: presentation-free `ToolEnvelope { status, summary, data, next_actions }`; all tools attach canonical follow-up actions from the retained eight-tool inventory.
- Core discovery loop: readiness/status maps to `oneup_status`; explicit start/index/reindex behavior maps to `oneup_start`; ranked discovery uses `oneup_search`; evidence hydration uses `oneup_get.handles`; file-line context uses `oneup_context.locations`; symbol completeness uses `oneup_symbol`.
- Readiness and search counts are scoped to the resolved `WorktreeContext`, while linked worktrees keep shared state under the main worktree.
- Removed public tools: the former combined readiness/indexing and combined read/context tools are no longer registered.
- MCP search is ranked discovery, not exhaustive proof; guidance instructs agents to hydrate with `oneup_get` before relying on results.
- Focused MCP smoke coverage exercises status, start, search, handle get, symbol lookup, read-location context, impact, and structural search; broader fixtures cover branch filtering, daemon refresh, benchmarks, and installer behavior.

### Search Boundary

- Engines: `HybridSearchEngine`, `SymbolSearchEngine`, `StructuralSearchEngine`, `ContextEngine`, `ImpactHorizonEngine`.
- Result contracts: `SearchResult`, `SymbolResult`, `StructuralResult`, `ContextResult`, `ImpactResultEnvelope`.
- `search` stays discovery-oriented; `impact` returns advisory `expanded`, `expanded_scoped`, `empty`, `empty_scoped`, or `refused` envelopes with primary `results` and optional `contextual_results`.

### Storage Boundary

- Schema version: 13.
- Worktree/context schema: `worktree_contexts` plus `context_id` on segments, symbols, relations, and indexed files.
- Vector storage: `segment_vectors.embedding_vec FLOAT8(384)`, `vector8(?)` writes, and `libsql_vector_idx(..., compress_neighbors=float8, max_neighbors=32)`.
- Manifest: `indexed_files(context_id, file_path, extension, file_hash, file_size, modified_ns)` supports metadata prefiltering.
- Replace/delete flows must keep context-scoped segments, vectors, symbols, relations, FTS, and manifest rows transactionally aligned.

### Daemon Boundary

- Secure Unix IPC via daemon socket and framed JSON `SearchRequest`/`SearchResponse`; non-Unix uses explicit stubs.
- Daemon search carries version metadata and bounded unavailable/busy responses.
- Registry entries and daemon status are context-aware: context id, source root, main worktree, worktree role, branch/ref/head status, watch status, and last refresh/update metadata are preserved.
- Registry and pid-lock handling are non-destructive under startup contention.
- `impact` is still not daemon IPC; it runs locally through CLI/MCP storage reads.

## Cross-Module Patterns

| Pattern | Modules | Why It Matters |
|---|---|---|
| CLI/MCP dual surface over shared engines | CLI, MCP, Search, Storage, Indexer | Agents and humans use different transports while preserving one local indexing/search contract. |
| Lean handle handoff | CLI, MCP, Search, Storage, Tests, Evals | `search -> get/context -> impact/symbol` flows use short durable segment handles without embedding full bodies in discovery output. |
| Candidate-first hybrid retrieval | Search, Storage, Indexer | Combines vector, FTS, and symbol candidates before hydration and ranking. |
| Local-only impact trust buckets | Search, CLI, MCP, Storage | Keeps advisory blast-radius exploration bounded, relation-aware, and explicit about primary vs contextual confidence. |
| Manifest-backed file prefilter | Indexer, Storage, CLI, Daemon | Skips context-specific metadata-unchanged files before content reads and reports discovered/metadata-skipped/content-read/deleted counters. |
| Transactional search index maintenance | Indexer, Storage | Batched replacement keeps context-scoped segments, vectors, symbols, relations, FTS, and manifest rows synchronized. |
| Secure local state | Shared, Storage, Daemon, CLI, MCP | XDG/project roots, pid/socket/lock files, model artifacts, and DB paths are validated and permissioned before use. |
| Worktree-aware project identity | Shared, CLI, MCP, Indexer, Daemon, Search | Source roots can differ from state roots while `context_id` and branch metadata keep worktree search, progress, and status scoped correctly. |
| Version-aware degraded paths | CLI, Daemon, MCP, Search, Indexer | Stale schema, missing embeddings, daemon mismatch, and model failures degrade or refuse with explicit guidance instead of silent corruption. |
| Release/eval evidence gates | Scripts, Tests, Evals, Packaging | Release assets, package manifests, MCP smoke, security evidence, recall, and vector-size gates are tested as first-class modules. |

## Reconciliation Notes

- Confirmed: prior CLI/search/indexer/storage/daemon/shared/test/bench/eval/script module boundaries still fit the current code.
- Refined: `src/cli` is now explicitly split between retained human discovery rendering, lean/plain discovery rendering, and maintenance-format rendering; `src/cli/output.rs` no longer owns core discovery result rendering.
- Added: `src/mcp` is a standalone top-level module with a real public contract and should no longer be treated as incidental daemon/CLI behavior.
- Added: installer and release automation now include MCP setup, wrapper-first install guidance, host smoke recording, and smoke verification.
- Contradicted: `hello-agent` should be removed from the public CLI boundary.
- Preserved: schema v13, `indexed_files`, relation evidence columns, trust buckets, timing propagation, and daemon startup/idempotency claims remain supported by current files.
