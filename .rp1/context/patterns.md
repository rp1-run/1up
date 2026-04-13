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
- New user-facing flows prefer stable envelopes over ad hoc maps:
  - `SearchResult` adds optional `segment_id`
  - impact uses `status + resolved_anchor + results + hint + refusal`
- Strong enums and typed request structs keep CLI, storage, and output boundaries explicit.

## Error Handling

- CLI boundaries use `anyhow::bail!` for invalid input and stale local state.
- Search and storage use domain errors such as `SearchError` and `StorageError`.
- Advisory impact failures become structured refusal envelopes instead of hard errors.
- Search degrades to FTS-only when embedding or vector retrieval fails per query.
- Stale or incompatible indexes fail closed and direct the caller to `1up reindex`.

## Validation

- Validation happens at CLI parse boundaries, daemon IPC boundaries, schema gates, and transaction seams.
- Exact-one-anchor validation is explicit for `impact`.
- Paths and scopes are normalized to repo-style slashes before use.
- Symbols are canonicalized before writing symbol and relation rows.
- Broad symbol anchors are rejected early with narrowing hints.

## Output Contracts

- Search-like commands default to `plain`; lifecycle commands default to `human`.
- Human output stays concise and selective.
- Plain and JSON modes keep stable exact identifiers for automation.
- Human impact output is more explanatory because the user already opted into a deeper follow-up workflow.
- Additive fields such as `segment_id` and `daemon_version` extend machine-readable contracts without breaking older consumers.

## Storage And I/O

- SQL lives in centralized named constants.
- Schema v8 adds `segment_relations`.
- Relation rows store unresolved canonical targets and are resolved at query time for bounded seeds.
- Segment, symbol, and relation maintenance shares one transactional seam.
- Daemon IPC uses tagged serde frames, same-UID authorization, bounded sizes, and read/write deadlines.

## Concurrency

- CLI, search, storage, and daemon entry points are async over Tokio/libSQL.
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

- Impact Horizon is a local-only advisory read path with bounded expansion, scope-aware narrowing, same-file/test heuristics, and structured refusal/hint envelopes.
- Search-to-impact handoff is additive and backward-compatible: `SearchResult.segment_id` is optional and follow-up impact requests must not perturb search ranking.
- Relation maintenance moved to the storage seam so segment, symbol, and relation rows stay synchronized during replace/delete flows.
- Performance is treated as a contract through targeted integration coverage and Criterion workloads for both expanded and refused impact requests.
