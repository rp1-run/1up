# 1up - Concept Map

## Reconciliation Notes

| Prior Claim | Status | Update |
|---|---|---|
| Schema v13 stores `segment_vectors.embedding_vec` as `FLOAT8(384)` and uses `vector8(?)`. | confirmed | Current schema/query/storage paths still declare `FLOAT8(384)`, `compress_neighbors=float8`, `max_neighbors=32`, and typed `vector8(?)` insert/query sites. |
| Impact Horizon separates primary likely impact from contextual guidance. | confirmed | `ImpactResultEnvelope.results` and `contextual_results` remain separate; relation scoring still gates primary promotion through owner, edge, path, role, and ambiguity checks. |
| Search emits a machine follow-up segment handle. | refined | `SearchResult.segment_id` is now required in the current shared contract, while display surfaces may shorten it to a 12-char `:<handle>` prefix accepted by get/impact follow-up. |
| Project state lives at the project root. | refined | Current project resolution separates `state_root` from `source_root`, especially for linked worktrees and MCP/daemon flows. |
| Eval and benchmark harnesses gate retrieval/storage changes. | untested | Preserved from prior KB; eval files were outside this pass's assigned concept set. |
| MCP is not modeled in the prior concept map. | contradicted-by-new-evidence | MCP is now a first-class code-discovery surface with stdio serving, status/start, search, get, symbol, context, impact, and structural tools. |

## Core Concepts

| Concept | Type | Meaning | Primary Evidence |
|---|---|---|---|
| `Segment` | Entity | Fundamental indexed code block with deterministic id, file span, language, block type, role, symbols, relations, and optional vector row. | `src/shared/types.rs`, `src/storage/segments.rs` |
| `Segment Handle` | Value object | A segment id used for exact follow-up; MCP and lean rows display a short `:<12-char>` prefix, and get lookup resolves full or unique prefix handles. | `src/storage/segments.rs`, `src/mcp/tools.rs`, `src/mcp/ops.rs` |
| `SearchResult` | Entity | Ranked hydrated discovery result carrying a required `segment_id`, integer score, path/span/kind, breadcrumb, content, and optional defined symbols. | `src/shared/types.rs`, `src/search/hybrid.rs` |
| `CandidateRow` | Internal entity | Lightweight retrieval/ranking candidate selected before full segment hydration. | `src/search/retrieval.rs`, `src/search/ranking.rs` |
| `QueryIntent` | Value object | Query classifier (`Definition`, `Flow`, `Usage`, `Docs`, `General`) that influences symbol variant search and rank boosts. | `src/search/intent.rs`, `src/search/ranking.rs` |
| `Hybrid Search` | Process | Candidate-first discovery combining vector, FTS, and exact/fuzzy symbol candidates through RRF ranking before hydration. | `src/search/hybrid.rs`, `src/search/retrieval.rs`, `src/search/ranking.rs` |
| `Symbol Lookup` | Process | Canonical exact/fuzzy definition and usage lookup over `segment_symbols`, with definitions deduped from usage results. | `src/search/symbol.rs`, `src/storage/queries.rs` |
| `Context Retrieval` | Process | Read-after-pick source hydration that returns the smallest tree-sitter enclosing scope or a bounded line fallback. | `src/search/context.rs`, `src/mcp/ops.rs` |
| `Structural Search` | Process | Tree-sitter query execution over indexed or scanned files for AST-pattern matches. | `src/search/structural.rs` |
| `Impact Horizon` | Process | Local-only bounded likely-impact exploration from exactly one anchor. | `src/search/impact.rs`, `src/mcp/tools.rs` |
| `ImpactAnchor` | Value object | Exact-one-anchor request model: file/line, symbol, or segment id. | `src/search/impact.rs`, `src/mcp/types.rs`, `src/mcp/tools.rs` |
| `ImpactCandidate` | Entity | Ranked likely-impact segment with hop distance, advisory score, reason evidence, and role/symbol metadata. | `src/search/impact.rs` |
| `Contextual Guidance` | Contract concept | Lower-confidence impact support kept outside primary results, including same-file/test heuristics and demoted relation matches. | `src/search/impact.rs` |
| `ImpactResultEnvelope` | Value object | Shared impact output envelope for `expanded`, `expanded_scoped`, `empty`, `empty_scoped`, and `refused`, with optional contextual results, hint, and refusal. | `src/search/impact.rs` |
| `ParsedRelation` | Value object | Parser-extracted call/reference/conformance relation with raw symbol and normalized edge identity. | `src/shared/types.rs`, `src/indexer/parser.rs` |
| `SegmentRelation` | Entity | Persisted unresolved relation edge keyed by source segment plus raw/canonical/lookup/qualifier/edge evidence. | `src/storage/relations.rs`, `src/storage/schema.rs` |
| `RelationTargetDescriptor` | Value object | Normalized target descriptor that splits a raw relation into canonical symbol, lookup tail, and qualifier fingerprint. | `src/storage/relations.rs` |
| `Indexed Files Manifest` | Entity | Per-file manifest (`indexed_files`) storing path, extension, hash, size, and mtime for metadata prefiltering and deletion detection. | `src/storage/schema.rs`, `src/storage/segments.rs`, `src/indexer/pipeline.rs` |
| `Indexing Pipeline` | Process | Scans, metadata-prefilters, parses/chunks, embeds, batches, stores, and deletes repository segments transactionally. | `src/indexer/pipeline.rs`, `src/indexer/scanner.rs` |
| `RunScope` | Value object | Index run scope: full repository or a set of changed paths, with fallback to full when scoped correctness is ambiguous. | `src/shared/types.rs`, `src/indexer/pipeline.rs`, `src/daemon/worker.rs` |
| `IndexProgress` | Contract concept | Persisted progress/status telemetry with phase, counts, parallelism, timings, scope, and prefilter counters. | `src/shared/types.rs`, `src/indexer/pipeline.rs` |
| `EmbeddingRuntime` | Service | Warm embedding runtime cache for indexing/search that can load/download verified artifacts for indexing and degrade search without downloading. | `src/indexer/embedder.rs`, `src/daemon/worker.rs` |
| `Verified Model Artifact` | Entity | Hash-pinned ONNX/tokenizer artifact set activated through staging, manifest, and current pointer files under secure XDG state. | `src/indexer/embedder.rs`, `src/shared/constants.rs` |
| `Segment Vector` | Entity | Quantized embedding row stored as `FLOAT8(384)` in `segment_vectors.embedding_vec`. | `src/storage/queries.rs`, `src/storage/schema.rs` |
| `Vector Index` | Entity | libSQL vector index over segment vectors using cosine distance, `compress_neighbors=float8`, and `max_neighbors=32`. | `src/storage/queries.rs` |
| `Schema Version` | Value object | Monotonic schema integer (`SCHEMA_VERSION = 12`) validated before reads/writes; mismatches fail closed with `1up reindex` guidance. | `src/shared/constants.rs`, `src/storage/schema.rs` |
| `Daemon Search IPC` | Interface | Same-UID Unix-socket framed JSON search protocol with bounded payloads, query sanitization, busy/unavailable responses, and optional daemon version metadata. | `src/daemon/search_service.rs`, `src/daemon/worker.rs` |
| `Daemon Project Run State` | State model | Per-project dirty/running/pending scope tracker that collapses file-watch bursts and records full-scope fallback reasons. | `src/daemon/worker.rs` |
| `ResolvedProject` | Value object | Worktree-aware root pair: `state_root` owns `.1up` state and daemon registry, while `source_root` supplies files to scan/read. | `src/shared/project.rs`, `src/mcp/ops.rs` |
| `MCP Stdio Server` | Interface | Agent-facing stdio MCP server exposing 1up tools and instructions that make 1up the primary local code-discovery path. | `src/mcp/server.rs`, `src/mcp/tools.rs`, `src/cli/mcp.rs` |
| `ToolEnvelope` | Contract concept | Uniform MCP response shape: `status`, `summary`, structured `data`, and typed `next_actions`. | `src/mcp/types.rs`, `src/mcp/tools.rs` |
| `ReadinessPayload` | Contract concept | MCP readiness state summarizing initialization, schema/index readability, counts, progress, daemon heartbeat, and degraded embedding state. | `src/mcp/ops.rs` |
| `Recall Eval Harness` | Process | Prior KB concept for cold recall/size gating of retrieval and vector-storage changes. Preserved but not revalidated in this pass. | prior KB, `evals/` |

## Terminology

| Term | Meaning |
|---|---|
| `oneup_status` | MCP readiness tool that checks the configured repository without indexing. |
| `oneup_start` | MCP lifecycle tool that can create, refresh, or rebuild the local index; modes are `index_if_needed`, `index_if_missing`, and `reindex`. |
| `oneup_search` | MCP ranked discovery tool backed by Hybrid Search; returns handles and may mark results degraded when embeddings are unavailable. |
| `oneup_get` | MCP hydration tool for segment handles; returns segment records in request order. |
| `oneup_symbol` | MCP completeness tool for symbol definitions, references, or both. |
| `oneup_context` | MCP file-line context tool for repo-contained source locations. |
| `oneup_impact` | MCP likely-impact tool that maps one handle/symbol/file anchor into `ImpactRequest`. |
| `oneup_structural` | MCP tree-sitter structural search tool with explicit ok, empty, and error diagnostics. |
| `next_actions` | MCP follow-up hints that name another 1up tool and structured arguments. |
| `handle` | MCP-visible segment identifier, normally rendered with a leading colon and shortened for display. |
| `state_root` | Root where `.1up/` state, index DB, project id, daemon status, and registry identity live. |
| `source_root` | Root whose files are scanned, indexed, watched, and read; can differ from `state_root` for linked worktrees. |
| `readiness status` | MCP readiness enum: `ready`, `missing`, `indexing`, `stale`, `degraded`. |
| `operation status` | MCP operation enum: `ok`, `empty`, `partial`, `degraded`. |
| `get/context status` | Per-record get/context enum: `found`, `not_found`, `ambiguous`, `rejected`, `error`. |
| `degraded search` | Search path where semantic embeddings are unavailable and FTS-only retrieval is used. |
| `segment_id` | Required machine-readable segment id on current `SearchResult` and `SymbolResult`; prefix lookup supports shorter handles. |
| `SegmentRole` | Parser role vocabulary: `DEFINITION`, `IMPLEMENTATION`, `ORCHESTRATION`, `IMPORT`, `DOCS`. |
| `lookup_canonical_symbol` | Normalized tail symbol used for bounded relation target lookup. |
| `qualifier_fingerprint` | Normalized non-tail owner tokens used to align relation targets with paths/breadcrumbs/owners. |
| `edge_identity_kind` | Relation-form vocabulary such as `qualified_path`, `member_access`, `method_receiver`, `constructor_like`, `macro_like`, or `bare_identifier`. |
| `conformance relation` | Relation kind for inheritance/implements/trait conformance extracted from Rust, TypeScript, and Java constructs. |
| `definition_owner_fingerprint` | Impact-time owner tokens derived from candidate path, breadcrumb, and defined symbols. |
| `corroboration signal` | Structural support signal required before ambiguous relation matches become primary impact results. |
| `ambiguity margin` | Confidence gap required for the best relation candidate to beat the runner-up. |
| `metadata_skipped` | Prefilter count for indexed files skipped because size and mtime matched the manifest. |
| `content_read` | Prefilter count for files that passed metadata screening and were actually read/parsed. |
| `MCP instance lock` | Per-project Unix lock preventing multiple stdio MCP server instances for one state root. |
| `FLOAT8(384)` | libSQL int8-quantized 384-dim vector column type used by schema v13. |
| `vector8(?)` | Typed libSQL constructor required for reads/writes against `FLOAT8` vector columns. |
| `VECTOR_PREFILTER_K` | Candidate prefilter count (`400`) used by vector/FTS retrieval before RRF reranking. |
| `gold corpus` | Prior KB term for version-neutral recall ground truth keyed by durable anchors, not transient segment ids. |

## Relationships

- `MCP Stdio Server` exposes `oneup_status`, `oneup_start`, `oneup_search`, `oneup_get`, `oneup_symbol`, `oneup_context`, `oneup_impact`, and `oneup_structural` as tool-router methods on `OneupMcpServer`.
- `ToolEnvelope` wraps every MCP payload and uses `next_actions` to encode the expected status/start/search -> get/context -> symbol/impact/structural workflow.
- `ResolvedProject` feeds MCP, daemon, and indexing flows with distinct `state_root` and `source_root` responsibilities.
- `oneup_status` checks readiness, and `oneup_start` can create/rebuild the local index through `Db`, `schema`, `EmbeddingRuntime`, and the `Indexing Pipeline`.
- `oneup_search` calls `Hybrid Search`; `oneup_get` hydrates `Segment Handle` values through storage; `oneup_context` reads file locations through `Context Retrieval`.
- `Hybrid Search` combines vector, FTS, and symbol candidates into `CandidateRow` values, ranks them, then hydrates `SearchResult` from `Segment` rows.
- `SearchResult.segment_id` and `SymbolResult.segment_id` provide exact handles for `oneup_get` and `ImpactAnchor::Segment`.
- `Indexing Pipeline` turns parser/chunker output into `SegmentInsert` batches, `Segment Vector` rows, `segment_symbols`, `SegmentRelation` rows, and `Indexed Files Manifest` rows.
- `ParsedRelation` rows become `SegmentRelation` records through `RelationTargetDescriptor` normalization.
- `Impact Horizon` resolves relation targets through `lookup_canonical_symbol`, scores owner/edge/path/role evidence, and separates primary `results` from `contextual_results`.
- `Daemon Project Run State` supplies scoped/full `RunScope` values and fallback reasons into `Indexing Pipeline` progress telemetry.
- `Schema Version` gates all current storage reads and writes; stale or incomplete schemas produce reindex-required errors.

## Recurrent Patterns

| Pattern | Context | Application |
|---|---|---|
| Search-before-get workflow | MCP and lean CLI | Discovery returns handles first; callers hydrate selected records with `oneup_get`/`get` before relying on content. |
| Structured follow-up guidance | MCP tools | Every tool result carries `next_actions` rather than leaving agents to infer the next command. |
| State/source root split | Project resolution | Worktree-aware operations store state in the main repo while indexing/reading the active source tree. |
| Candidate-first retrieval | Search | Vector, FTS, and symbol paths produce lightweight candidates before full segment hydration. |
| RRF with intent boosts | Ranking | Reciprocal rank fusion is adjusted by query intent, path, text-match, content kind, and short-segment penalties. |
| Graceful semantic degradation | Search, MCP, Daemon | Missing/load-failed embeddings fall back to FTS-only search and readiness/status explains degradation. |
| Descriptor-backed relation resolution | Impact/storage | Indexing stores unresolved relation descriptors; impact resolves bounded candidate definitions only for active seeds. |
| Trust bucket separation | Impact | Confident relation-backed likely impact stays primary; weak or heuristic guidance stays contextual or empty. |
| Manifest-backed prefilter | Indexing | Size/mtime matching skips file reads, while content hash remains the correctness backstop when metadata changes. |
| Batched transactional writes | Storage | Segment, vector, symbol, relation, and manifest mutations use chunked multi-value inserts inside file-batch transactions. |
| Force-reindex schema evolution | Storage | Breaking storage changes bump `SCHEMA_VERSION`; no in-place migration is attempted. |
| Secure local state | Project/storage/daemon/MCP | `.1up`, XDG state, model artifacts, DB paths, sockets, and lock files are validated or permissioned before use. |
| Bounded daemon concurrency | Daemon IPC | Search requests are limited by a semaphore and return busy/unavailable responses under saturation. |

## Bounded Contexts

### MCP Code Discovery

- Owns stdio server instructions, tool schemas, envelopes, summaries, and next-action choreography for agent-facing discovery.
- Delegates actual status/start, search, get, context, symbol, impact, and structural work to search/storage/indexing/shared modules.

### Search Discovery

- Owns hybrid retrieval, query intent, ranking, symbol lookup, context retrieval, and structural pattern search.
- Does not claim exhaustive dependency truth; ranked discovery must be followed by read/symbol checks when completeness matters.

### Impact Analysis

- Owns local likely-impact expansion, anchor validation, refusal/empty semantics, relation scoring, and primary/contextual separation.
- Does not run through daemon IPC and remains advisory.

### Indexing And Embeddings

- Owns scan, parse/chunk, embedding preparation, metadata prefiltering, scoped/full fallback, progress telemetry, and batched storage writes.
- Search can operate without embeddings; indexing is where downloads and model compatibility checks occur.

### Index Graph Storage

- Owns schema v13, segments, vectors, symbols, relations, indexed-file manifest rows, meta records, and DB path validation.
- Uses force-reindex for incompatible schema/model changes.

### Daemon Coordination

- Owns file watching, registry-driven projects, source-root watching, dirty run coalescing, heartbeat persistence, and bounded search IPC.
- Does not own MCP stdio transport, but MCP can auto-start the daemon when safe.

### Project Identity And Roots

- Owns project id creation, secure `.1up` state, auto-init safety, linked-worktree detection, and state/source root resolution.
- Automatic creation is refused outside an existing 1up project or git root.

### Eval And Benchmarks

- Preserved prior context for recall/size/latency gates; not revalidated by the assigned files in this pass.

## Cross-Cutting Concerns

- Compatibility: public surfaces evolve through stable structs/enums and additive fields where possible; storage incompatibility is handled by schema version checks.
- Ambiguity management: impact refuses broad anchors; handle prefix get calls report ambiguity; MCP context locations reject path escapes.
- Advisory semantics: impact scores and reasons are likely-impact guidance, not exact dependency truth.
- Performance: metadata prefiltering, write batching, tuned PRAGMAs, warm embedding runtime, bounded vector prefiltering, and daemon request limits protect interactive latency.
- Security: same-UID daemon IPC, secure socket/file modes, validated project DB paths, repo-contained location reads, and verified model artifact hashes defend local state boundaries.
- Observability: `IndexProgress`, daemon heartbeat, readiness payloads, setup timings, scope info, and prefilter counters make local state inspectable.
- Degradation transparency: missing embeddings, stale schemas, unreadable indexes, and busy/unavailable daemon states surface explicit statuses and retry guidance.

## Novelty Scan Findings

- MCP is now a reusable domain boundary, not just a wrapper: it defines typed inputs, structured envelopes, summaries, and tool-to-tool workflows.
- `SearchResult.segment_id` has moved from optional/additive prior wording to a required current handle in shared search results.
- Project resolution now has durable state/source root semantics that future worktree, daemon, and MCP changes must preserve.
- Search has an explicit intent/ranking layer beyond vector + FTS; future rank changes should account for `QueryIntent`, symbol candidates, per-file caps, and content-kind/path boosts.
- Embedding availability is a first-class readiness/degradation axis across MCP, daemon, indexing, and search.
