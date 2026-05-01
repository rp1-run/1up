# 1up — Patterns

## Naming And Layout

- Files stay snake_case inside layered directories.
- New surfaces are wired through `mod.rs` exports and `Command` enum arms, not registries.
- CLI commands follow `<Name>Args` plus `async fn exec(args, format)`.
- Helpers stay small verb phrases such as `parse_anchor`, `sanitize_request`, and `format_impact_result`.
- Imports are explicit `crate::...` paths, grouped by std, external crates, then internal modules.

## Data Modeling

- Public contracts use owned structs and enums with serde derives.
- Additive compatibility fields are `Option<T>` with `skip_serializing_if`.
- Internal relation rows keep both stable identity and disambiguation evidence: `raw_target_symbol`, `canonical_target_symbol`, `lookup_canonical_symbol`, `qualifier_fingerprint`, and `edge_identity_kind`.
- Impact-side identity stays additive: definition-owner fingerprints are derived from candidate path, breadcrumb, and defined symbols at evaluation time instead of changing symbol-search contracts.
- New user-facing flows prefer stable envelopes over ad hoc maps: `SearchResult` adds optional `segment_id`, and impact uses `status + resolved_anchor + results + contextual_results + hint + refusal`.
- Start outcomes use `StartResultInfo` with `status`, `pid`, `message`, and optional `progress` so daemon-only startup and indexed startup share one maintenance-command envelope.
- Strong enums and typed request structs keep CLI, storage, and output boundaries explicit.
- Pre-pipeline telemetry uses `SetupTimings` (wall-clock start, `db_prepare_ms`, `model_prepare_ms`) passed from callers so `total_ms` reflects user-perceived elapsed time. `IndexStageTimings` extends additively with optional `db_prepare_ms`, `model_prepare_ms`, and `input_prep_ms`.
- `IndexProgress` gains additive `scope: Option<IndexScopeInfo>` (requested/executed scope, changed paths, fallback reason) and `prefilter: Option<IndexPrefilterInfo>` (discovered, metadata_skipped, content_read, deleted) without changing existing fields or envelope shapes.
- The `indexed_files` manifest stores per-file metadata (path, extension, content hash, size, mtime) as a first-class row for prefilter comparisons.

## Error Handling

- CLI boundaries use `anyhow::bail!` for invalid input and stale local state.
- Search and storage use domain errors such as `SearchError` and `StorageError`.
- Advisory impact failures become structured refusal envelopes instead of hard errors.
- Search degrades to FTS-only when embedding or vector retrieval fails per query.
- Stale or incompatible indexes fail closed and direct the caller to `1up reindex`.

## Validation

- Validation happens at CLI parse boundaries, daemon IPC boundaries, schema gates, and transaction seams.
- Exact-one-anchor validation is explicit for `impact`.
- Schema readiness for impact now requires schema v10 plus `segment_relations.lookup_canonical_symbol`, `segment_relations.qualifier_fingerprint`, and `segment_relations.edge_identity_kind`.
- Paths and scopes are normalized to repo-style slashes before use.
- Symbols are canonicalized before writing symbol and relation rows.
- `1up start` validates startup state by acquiring a per-project guard before project-id, registry, index classification, foreground indexing, and daemon spawn/observe work, then re-reading state under that guard.
- Project identity creation is idempotent: `ensure_project_id` reads first, publishes with create-new semantics, and re-reads if another caller created the id.
- Normal daemon pid-lock contention is classified as running or startup-in-progress without sending termination signals; destructive lifecycle authority stays with explicit commands such as `stop`.
- Relation expansion validates owner alignment, edge compatibility, corroboration count, and ambiguity before a candidate can become a primary result.
- A bare leaf-name match with no second corroborating structural signal is contextual-only or absent rather than primary.
- `IMPORT` and `DOCS` relation matches remain contextual-only even when they clear lookup resolution.
- Broad symbol anchors are rejected early with narrowing hints.

## Output Contracts

- **One shape per core command**: `search`, `get`, `symbol`, `impact`, `context`, and `structural` emit a single lean line-oriented rendering. They do not accept `--format`, `-f`, `--full`, `--brief`, `--human`, or `--verbose-fields`; clap rejects those flags at parse time because they are declared only on maintenance command args.
- **Unified row grammar**: discovery rows follow `<score>  <path>:<l1>-<l2>  <kind>  <breadcrumb>::<symbol>  :<segment_id>[  ~<channel>]` with two ASCII spaces between fields, a 12-char hex segment handle, and an optional trailing `~P`/`~C` channel tag reserved for `impact`. `context` and `structural` drop `<score>` and `:<segment_id>` because they are read-after-pick, not discovery.
- **Integer score**: `SearchResult.score` is a `u32` in 0-100, produced by `normalize_score(rrf)` and monotonic with the RRF ranking.
- **Maintenance commands keep the trio**: `start`, `stop`, `status`, `init`, `index`, `reindex`, and `update` still dispatch through `HumanFormatter`, `PlainFormatter`, and `JsonFormatter` and still accept `--format`/`-f` via their own `Args` struct.
- Plain and JSON modes on maintenance commands keep stable exact identifiers for automation.
- `start --format json` emits additive `status`, `pid`, and `message` fields. Status values are `started`, `already_running`, `startup_in_progress`, and `indexed_and_started`.
- Start output includes `progress` and `work` only when foreground indexing ran. Current-index startup, already-running, and startup-in-progress outcomes omit those fields so automation can distinguish daemon coordination from indexing work.
- Human impact output (on the lean renderer) is still more explanatory because the user opted into deeper follow-up work.
- Additive fields such as `segment_id` and `daemon_version` extend machine-readable contracts without breaking older consumers.
- Timing, scope, and prefilter telemetry fields are optional additions on `IndexProgress` rendered by all three maintenance formatters (human, plain, JSON) without altering existing field names or envelope shapes.

## Storage And I/O

- SQL lives in centralized named constants.
- Schema v11 adds the `indexed_files` manifest table and extends `segment_relations` with lookup-target, qualifier-fingerprint, and edge-identity columns plus a lookup-target index.
- Schema v12 declares `segment_vectors.embedding_vec` as `FLOAT8(384)` and creates `idx_segment_vectors_embedding` via `libsql_vector_idx(embedding_vec, 'metric=cosine', 'compress_neighbors=float8')` to shrink the HNSW graph alongside the column.
- Storage-format shifts follow the schema-bump-forces-reindex pattern: when an on-disk element type, index option, or table definition changes incompatibly, bump `SCHEMA_VERSION` in `src/shared/constants.rs` so existing indexes fail closed with the standard reindex hint instead of silently reading a format-mismatched column. No in-place migration is attempted; `1up reindex` rebuilds to the new format.
- libSQL vector writes and reads must use the element-typed constructor that matches the column: a `FLOAT8` column requires `vector8(?)` in `UPSERT_SEGMENT_VECTOR`, `SELECT_VECTOR_CANDIDATES`, and the chunked multi-value INSERT inside `segments::batch_upsert_vectors`. The untyped `vector(?)` form raises `InputValidationError: vector type differs from column type` at insert. Embedder output stays as JSON-text `Vec<f32>`; the server quantizes on insert.
- Relation rows store unresolved canonical targets alongside lookup/disambiguation evidence and are resolved at query time for bounded seeds.
- Segment, symbol, relation, vector, and `indexed_files` maintenance shares one transactional seam with chunked multi-value INSERTs (SQLITE_MAX_PARAMS=999, per-table chunk sizes derived from column counts).
- Project-local connections apply performance PRAGMAs (WAL, synchronous=NORMAL, cache_size=-32768, mmap_size=268435456, temp_store=MEMORY) via `connect_tuned` at open time.
- File-level prefiltering compares filesystem metadata (size, mtime) against the `indexed_files` manifest before content reads in both full and scoped runs. Metadata matches increment `metadata_skipped` and avoid content reads; missing rows or metadata differences still reach the content-hash correctness backstop.
- Full-run deletion detection is based on manifest paths plus segment paths, so metadata-skipped unchanged files remain discovered files and are not mistaken for deletions.
- Daemon IPC uses tagged serde frames, same-UID authorization, bounded sizes, and read/write deadlines.

## Concurrency

- CLI, search, storage, and daemon entry points are async over Tokio/libSQL.
- Defensive startup uses short-lived cross-process locks: a startup guard serializes CLI startup per project, and registry register/deregister locks reload the current shared registry before saving.
- Impact expansion stays intentionally sequential and budgeted rather than aggressively parallel.
- Intermediate state remains local `HashMap` and `HashSet` aggregates instead of shared mutable globals.

## Testing Style

- Unit tests prefer in-memory DB fixtures plus explicit schema init.
- Integration tests use temp repos/DBs and drive the real CLI in JSON mode.
- Benchmarks act as non-regression guardrails, not just performance snapshots.
- Feature tests assert:
  - refusal hints
  - ranked impact candidates
  - backward-compatible optional fields
  - search ranking stability after `segment_id` handoff

## Feature-Learning Novelty

- Impact relation resolution now combines lookup-symbol retrieval with definition-owner fingerprints, owner-aware shortlisting, edge-identity scoring, role weighting, and ambiguity margins before primary promotion.
- Search-to-impact handoff remains additive and backward-compatible: `SearchResult.segment_id` is optional, and stronger relation modeling does not perturb search ranking or the impact envelope shape.
- Low-signal wrappers, declaration-only matches without corroborating owner or structural support, and `IMPORT`/`DOCS` segments are demoted out of primary results while same-file/test observations stay contextual.
- Performance and trust remain contractual through targeted integration coverage, rollout scripts, and Criterion workloads for qualified-relation, low-signal, expanded, refused, and empty impact requests.
- Connection-level PRAGMA tuning, file-manifest prefiltering, and batched multi-value INSERTs are applied as fast defaults without new user-facing flags, following the "meaningful speed improvements through normal defaults" pattern.
- End-to-end timing propagation and scope/prefilter telemetry are additive, optional typed fields that extend existing contracts without breaking downstream automation.
- Daemon scope fallback tracking (via `pending_fallback_reason`) makes full-promotion behavior visible for troubleshooting without changing refresh semantics.
- Release-binary benchmark evidence covers full, incremental, write-heavy, and daemon refresh scenarios with scope evidence and per-run telemetry in summary JSON.
