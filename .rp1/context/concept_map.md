---
scope: kbRoot
path_pattern: "concept_map.md"
producer: knowledge-base
type: document
description: "Domain concepts, terminology glossary, and cross-references for a single-project codebase."
strictness: strict
---
# Domain Concepts & Terminology

**Project**: 1up (`oneup`) — v0.1.11
**Domain**: Local code-discovery engine — a tree-sitter + embedding index over a repository, exposed to agents through nine read-only/lifecycle MCP tools so that semantic search, symbol lookup, file-line context, structural queries, likely-impact analysis, and a repository orientation digest become the primary code-discovery path before raw grep/find.

## Core Business Concepts

### Segment
**Definition**: The fundamental indexed unit of code. A segment is one tree-sitter (or text-chunker) block with a deterministic id, file span, language, block type, role, defined/referenced/called symbols, complexity, an optional breadcrumb, and an optional quantized embedding row.
**Implementation**: [`src/shared/types.rs`] (`ParsedSegment`, `SegmentRole`), [`src/storage/schema.rs`] (`segments` table)
**Key Properties**:
- `segment_id`: Deterministic, durable handle used for every exact follow-up across tools.
- `role`: One of `DEFINITION`, `IMPLEMENTATION`, `ORCHESTRATION`, `IMPORT`, `DOCS`; drives intent boosts, impact role boosts, and overview entry-point ranking.
- `block_type` / `language`: Structural kind (e.g. `struct`, `function`, `doc_section`) and one of sixteen supported languages.
- `defined_symbols` / `referenced_symbols` / `called_symbols`: Symbol evidence stored on the segment and surfaced by `oneup_get`.

**Business Rules**:
- Discovery surfaces emit only lean fields; full symbol/role/complexity detail is hydrated by `oneup_get`, never by ranked search.
- Segments excluded from embedding (the `NON_EMBEDDABLE_CHUNK_LANGUAGES` set: json, yaml, toml, protobuf, terraform, sql, config, makefile, dockerfile) are FTS-only.

### Segment Handle
**Definition**: A segment id used as a durable cross-tool reference. MCP surfaces display a leading-colon, 12-character prefix; `oneup_get` and `oneup_impact` resolve a full id or a unique prefix, and report ambiguity when a prefix matches multiple segments.
**Relationships**:
- Produced by `oneup_search` and `oneup_symbol`; consumed by `oneup_get` and `ImpactAnchor::Segment`.
- `oneup_impact` accepts the older `segment_id` field name as a compatibility alias for `handle`.

### WorktreeContext
**Definition**: The worktree-aware identity for a run. It carries a `context_id` (the scoping key), `state_root`, `source_root`, `main_worktree_root`, `worktree_role`, branch name/ref/status, and HEAD oid.
**Implementation**: [`src/shared/types.rs`], [`src/mcp/ops.rs`] (`McpProjectRoots`)
**Relationships**:
- `state_root` owns `.1up` state, the index DB, project id, daemon status, and registry identity; `source_root` is the tree that is scanned, indexed, watched, and read. They differ for linked worktrees.
- `SearchScope::from_worktree_context` derives the `context_id` + `branch_status` used to filter every storage read, so linked worktrees stay isolated inside one shared database.

### SearchResult
**Definition**: A lean, hydrated discovery row returned by hybrid or FTS-only search.
**Implementation**: [`src/shared/types.rs`], [`src/search/hybrid.rs`]
**Key Properties**:
- `segment_id`: Required handle (always present in the current shared contract).
- `score`: Integer in `[0,100]`, a monotonic normalization of the raw RRF score.
- `file_path` / `line_number` / `line_end` / `block_type` / `language`: Location and kind; `breadcrumb` and `defined_symbols` are optional.

### Relation (ParsedRelation / SegmentRelation)
**Definition**: A directed dependency edge extracted from source. The parser emits a `ParsedRelation` (raw symbol, edge-identity kind, optional kind); storage persists it as an unresolved `SegmentRelation` keyed by the source segment, with raw/canonical/lookup-canonical/qualifier-fingerprint/edge-identity evidence, resolved lazily at impact time.
**Implementation**: [`src/indexer/parser.rs`], [`src/storage/relations.rs`], [`src/storage/schema.rs`]
**Business Rules**:
- A `RelationTargetDescriptor` normalizes the raw symbol into `canonical_target_symbol`, `lookup_canonical_symbol` (tail), and `qualifier_fingerprint` (owner tokens) for bounded lookup and owner alignment.
- `doc_mention` edges (code identifiers mentioned in documentation) are descriptive evidence and are excluded from impact dependency traversal.

## Technical Concepts

### Hybrid Search
**Purpose**: Candidate-first ranked discovery that fuses three retrieval signals into one ordered result set, then hydrates only the survivors.
**Implementation**: [`src/search/hybrid.rs`], [`src/search/ranking.rs`], [`src/search/retrieval.rs`]
**Usage Examples**:
```text
oneup_search "where are embeddings composed before indexing"
  -> vector candidates (if embeddings present) + FTS candidates + symbol candidates
  -> weighted RRF fusion + boosts/penalties
  -> dedupe overlaps, cap per file (3), hydrate top-N SearchResults
```
- **Lazy embedding**: lexical stages, the exact-lexical short-circuit, and a cheap vector-presence probe all run before the query is ever embedded, so an index with no vectors never exercises the embedder.
- **Exact-lexical short-circuit**: a single-token, identifier-shaped query (containing `_ : / . - #` or ≥ 24 chars) with a symbol/FTS hit skips the vector stage to keep exact-identifier lookups precise.

### Vector Search Path (corpus-adaptive)
**Description**: The nearest-neighbour stage chooses one of two modes by context vector count: an exact `vector_distance_cos` exhaustive scan at or below `VECTOR_EXHAUSTIVE_SCAN_MAX_VECTORS` (16384), or the approximate `vector_top_k` index above it.
**Input/Output**: Query embedding (384-dim) → ranked `CandidateRow`s.
**Performance**: The exhaustive scan is exact and trivially fast at small corpora where graph traversal is read-bound; the approximate index amortizes above the threshold. The candidate pool (`VECTOR_PREFILTER_K` = 400) scales by indexed-context count up to a bound (8) so linked worktrees do not dilute recall.

### RRF Ranking
**Description**: Reciprocal-rank fusion (`RRF_K` = 60) with per-source weights — vector 1.5, symbol 4.0, FTS implicitly 1.0 — followed by multiplicative boosts and penalties.
**Implementation**: [`src/search/ranking.rs`]
- Boosts/penalties: query-intent role boost, path-tier × content-kind, test-tier dampening, query-path overlap, query-term match, breadcrumb relevance, and a short-segment penalty.
- **Test-tier dampening**: natural-language conceptual queries stack an extra `0.72×` on the `0.7` test/bench/fixture path tier (Usage intent exempt) so implementation code beats descriptive test names.
- **Doc-section penalty non-stacking**: for `doc_section` segments the markdown content-kind penalty and docs-path penalty carry the same evidence, so only the stronger applies.
- **Defined-symbol penalty floor**: short segments that define symbols keep a `0.9` floor instead of the full `0.6/0.85` short-segment penalty.

### FTS Query Builder (identifier-aware)
**Description**: Builds an FTS5 MATCH expression with identifier-aware variants so plain prose can reach concatenated tokens.
**Implementation**: [`src/search/retrieval.rs`]
**Input/Output**: Raw query → quoted base terms plus split-phrase variants (CamelCase/snake_case), bounded prefix variants, and stem-prefix variants for inflected words, capped at `MAX_FTS_VARIANT_TERMS` (16). Split parts stay phrase-bound so exact-identifier precision is preserved.

### Impact Horizon
**Description**: Local-only, bounded likely-impact exploration from exactly one anchor (handle, symbol, or file[:line]). It walks outbound/inbound relations plus same-file and test heuristics, scores corroboration, and splits results into primary vs contextual buckets.
**Implementation**: [`src/search/impact.rs`], [`src/mcp/tools.rs`]
**Input/Output**: `ImpactRequest{anchor, scope, depth, limit}` → `ImpactResultEnvelope{status, resolved_anchor, results, contextual_results, hint, refusal}` where status is `expanded`, `expanded_scoped`, `empty`, `empty_scoped`, or `refused`.
- A relation target is scored by blending symbol score, owner alignment, edge-identity score, path affinity, and role score under an owner gate and a signal multiplier.
- Promotion to **primary** requires at least two corroboration signals and clearing the ambiguity margin; otherwise the candidate stays **contextual**. Symbol anchors that match too broadly are refused with a narrowing hint; impact is advisory and does not run through daemon IPC.

### Repository Overview
**Description**: A deterministic, size-bounded orientation digest for one context: stats (files/segments/languages), most-referenced types, a module map, cross-module dependency edges, and entry points. It is a pure read path over bounded SQL aggregates — no schema changes, no embeddings, no persisted artifacts — and recomputes identically on an unchanged index.
**Implementation**: [`src/search/overview.rs`] (`OverviewEngine`, `RepositoryOverview`), [`src/mcp/ops.rs`] (`OverviewPayload`)
- Top types rank by distinct referencing files with ambiguity skipping and truncation detection; the dominant module expands one level when it holds ≥ 60% of segments and has true children; module and dependency-edge granularity always agree.

### MCP Tool Surface
**Purpose**: Make 1up the primary local code-discovery path for agents, with structured envelopes and explicit follow-up choreography.
**Implementation**: [`src/mcp/tools.rs`] (`OneupMcpServer`), [`src/mcp/types.rs`] (`RETAINED_PUBLIC_TOOLS`, `ToolEnvelope`, `NextAction`), [`src/mcp/ops.rs`]
**Usage Examples**:
```text
nine retained tools:
  oneup_status     readiness without indexing
  oneup_start      create / refresh / rebuild the index (index_if_needed | index_if_missing | reindex)
  oneup_overview   deterministic orientation digest (recommended first call on an unfamiliar repo)
  oneup_search     ranked semantic + lexical + symbol discovery -> handles
  oneup_get        hydrate selected handles -> full segments
  oneup_context    repository-scoped file-line context
  oneup_symbol     definitions / references / both (optional fuzzy)
  oneup_impact     likely-impact from one handle/symbol/file anchor
  oneup_structural tree-sitter S-expression query over indexed source
```
- Every tool returns a `ToolEnvelope{status, summary, data, next_actions}`. `next_actions` name a retained tool plus JSON arguments and are debug-asserted to never point outside `RETAINED_PUBLIC_TOOLS`, encoding the `status`/`start`/`overview` → `search` → `get`/`context` → `symbol`/`impact`/`structural` workflow.

### Schema, Storage, and Model Artifacts
**Purpose**: Persist the index graph and validate compatibility; activate hash-pinned embedding artifacts safely.
**Implementation**: [`src/storage/schema.rs`], [`src/storage/relations.rs`], [`src/indexer/embedder.rs`], [`src/shared/constants.rs`]
- `SCHEMA_VERSION` = 16, validated before every read/write. Older indexes fail closed with reindex guidance; newer indexes fail with upgrade guidance. There is no in-place migration — breaking changes bump the version and force `1up reindex`.
- Schema objects include `worktree_contexts`, `segments`, `segment_vectors`, `segment_symbols`, `segment_relations`, `indexed_files`, `segments_fts`, and `meta`; the symbol and FTS tables are trigger-maintained.
- Embeddings are int8-quantized 384-dim rows stored as `FLOAT8(384)` in `segment_vectors.embedding_vec`, written/read through the typed `vector8(?)` constructor.
- The model (all-MiniLM-L6-v2) is activated via a `.staging` → `verified` manifest → `current.json` pointer flow under owner-only XDG state, with compiled-in SHA-256 digests; CI sets `ONEUP_DISABLE_MODEL_DOWNLOADS` to keep test suites hermetic.

## Terminology Glossary

### Business Terms
- **context_id**: Worktree-context scoping key; every storage query filters to the active `context_id` so linked worktrees stay isolated in one shared database.
- **state_root**: Root holding `.1up` state, the index DB, project id, daemon status, and registry identity.
- **source_root**: Root whose files are scanned, indexed, watched, and read; can differ from `state_root` for linked worktrees.
- **readiness status**: MCP readiness enum — `ready`, `missing`, `indexing`, `stale`, `degraded`, `blocked`.
- **operation status**: MCP per-operation enum — `ok`, `empty`, `partial`, `degraded`.
- **get/context status**: Per-record read enum — `found`, `not_found`, `ambiguous`, `rejected`, `error`.
- **head drift**: Advisory readiness signal comparing the indexed-at HEAD with the live HEAD; `drifted = true` makes `index_if_needed` self-serve but never changes readiness status on its own.
- **degraded search**: Search path where embeddings are unavailable (missing model or no indexed vectors), so FTS-only retrieval runs and an explicit reason is reported.
- **trust bucket**: The primary-vs-contextual split in impact; confident corroborated relation impact stays primary, weak or heuristic guidance stays contextual or empty.

### Technical Terms
- **Hybrid Search**: Candidate-first fusion of vector, FTS, and symbol candidates via weighted RRF before hydration. See [`src/search/hybrid.rs`].
- **RRF weights**: `RRF_K` = 60, `VECTOR_WEIGHT` = 1.5, `SYMBOL_WEIGHT` = 4.0 (FTS implicitly 1.0).
- **exhaustive scan vs ANN index**: The two vector stages — exact `vector_distance_cos` at or below 16384 context vectors, approximate `vector_top_k` above it.
- **VECTOR_PREFILTER_K**: Candidate prefilter count (400), scaled by indexed-context count up to `VECTOR_PREFILTER_CONTEXT_SCALE_LIMIT` (8).
- **exact-lexical short-circuit**: Identifier-shaped single-token queries with a hit skip the vector stage to preserve precision and avoid embedding.
- **QueryIntent**: Query classifier (`Definition`, `Flow`, `Usage`, `Docs`, `General`) from keyword signal counts; influences symbol-variant search and rank boosts.
- **edge identity kind**: Relation-form vocabulary — `bare_identifier`, `qualified_path`, `member_access`, `method_receiver`, `constructor_like`, `macro_like`, `doc_mention`.
- **lookup_canonical_symbol**: Normalized tail symbol used for bounded relation target lookup and the `segment_relations` lookup-target index.
- **qualifier_fingerprint**: Normalized non-tail owner tokens used to align relation targets with definition owners (owner alignment).
- **corroboration signals**: Count of structural supports (exact identity, owner alignment, edge identity, path affinity, role) required (≥ 2) before an ambiguous relation match becomes primary impact.
- **conformance relation**: Inheritance/implements/trait-conformance relation kind; weighted slightly above calls in impact.
- **doc_mention relation**: Edge for a code identifier mentioned in documentation; descriptive evidence, excluded from impact traversal.
- **metadata_skipped / content_read**: Prefilter counters — files skipped because size + mtime matched the manifest vs files actually read and parsed.
- **low-signal / test path**: Path classes demoted or excluded — `is_test_path` (tests/spec/`__tests__`/`_test` suffixes) and `is_low_signal_path` (adds evals/benches/examples/vendor/node_modules).
- **doc_section**: Block type for a heading-scoped markdown documentation segment (role `DOCS`) with a document-rooted breadcrumb.
- **ToolEnvelope / next_actions**: Uniform MCP response (`status`, `summary`, `data`, `next_actions`); follow-up hints naming a retained tool and arguments.
- **SCHEMA_VERSION**: Monotonic schema integer (currently 16); mismatches fail closed with reindex or upgrade guidance.

## Cross-References
- **Search & retrieval pipeline (hybrid → ranking → vector path)**: See [architecture.md] and [modules.md#search]
- **Index graph storage & schema v16**: See [modules.md#storage] and [architecture.md]
- **Indexing, embeddings, and model artifacts**: See [modules.md#indexer]
- **MCP tool surface, envelopes, and readiness**: See [modules.md#mcp]
- **Ranking, impact corroboration, and degradation patterns**: See [patterns.md]
