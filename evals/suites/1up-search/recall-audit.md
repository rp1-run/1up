# Recall Gold Corpus Audit (T6, REQ-005)

Human-reviewed audit of all 15 gold labels in `recall-corpus.jsonl`. Each label was
reviewed by reading the current source for the canonical implementation of the topic
and confirming every gold anchor binds to a real definition (a `(file, symbol)` whose
symbol is defined in that file, or a `(file, line_contains)` whose substring is present).
Anchors that named a symbol which no longer exists are **relevant-but-zero-credit
contradictions**: the file is still the right place, but the anchor can never earn credit
because no retrieved segment defines that symbol. Those were repaired as reviewed human
contracts (a symbol/file correction to the current canonical seam), never by automated
relabeling or by widening gold to make the gate pass.

- Corpus rows: 15 (unchanged by this audit — repairs are symbol/file corrections only)
- Corpus sha256 before audit: `46800759ee4a0c896c17c2d769d414ff6194176c5f363bacef2244253dc3283d`
- Corpus sha256 after audit: `ae668c77e7edd095795d43c23c86e924266ff0b5f769d00965e31ae16decaaab`
- Reviewed at commit: see the T6 commit; anchors verified against the working tree.

## Per-label verdicts

| # | Query | Verdict | Notes |
|---|-------|---------|-------|
| 1 | schema version constant used for migration gating | OK | `SCHEMA_VERSION` (constants.rs), `get_schema_version`, `ensure_current` (schema.rs) all present. |
| 2 | hybrid search combining vector and full-text retrieval | REPAIRED | `execute_search` no longer exists in `src/search/hybrid.rs`; the public entry point is `HybridSearchEngine::search` (renamed from the old `execute_search`, see `0aebaaa`). Symbol `execute_search` → `search`. Sibling anchors `HybridSearchEngine`, `rank_candidates`, `compute_rrf_score` unchanged and present. |
| 3 | batch insert vectors into segment_vectors table | OK | `batch_upsert_vectors`, `replace_file_batch_tx` (segments.rs), `UPSERT_SEGMENT_VECTOR` (queries.rs) present. |
| 4 | vector_top_k candidate retrieval from HNSW index | OK | `fetch_vector_candidates`, `serialize_query_embedding` (retrieval.rs), `SELECT_VECTOR_CANDIDATES` (queries.rs) present. |
| 5 | libsql vector index DDL using libsql_vector_idx | REPAIRED | `CREATE_INDEX_SEGMENT_VECTORS_EMBEDDING` no longer exists: the vector index moved off `segment_vectors` onto the content-addressed `embedding_pool` in schema v17 (`95488d7`). The current const is `CREATE_INDEX_EMBEDDING_POOL_EMBEDDING` (queries.rs), whose body is the `libsql_vector_idx(embedding_vec, ...)` DDL. Symbol → `CREATE_INDEX_EMBEDDING_POOL_EMBEDDING`. The `line_contains "libsql_vector_idx(embedding_vec"` anchor was already correct and is retained. |
| 6 | reject databases with stale schema versions | OK | `ensure_current`, `reindex_required` (schema.rs) and the `"is newer than this binary supports"` line all present. |
| 7 | ONNX embedder producing 384-dimensional float32 vectors | OK | `Embedder`, `EmbeddingRuntime`, `EmbeddingLoadStatus` (embedder.rs) present. |
| 8 | indexed_files manifest for metadata-based file prefilter | OK | `CREATE_INDEXED_FILES_TABLE`, `UPSERT_INDEXED_FILE` (queries.rs), `IndexedFileMeta` (segments.rs), `build_manifest_meta` (pipeline.rs) present. |
| 9 | deterministic segment identifier generation | REPAIRED | `generate_segment_id` is defined in `src/storage/segments.rs` (`pub(crate) fn generate_segment_id`), not `src/indexer/pipeline.rs` — the definition moved to storage while pipeline.rs keeps only the call site `segments::generate_segment_id(...)` (see `2b05ca7`). Anchored to `pipeline.rs` the symbol was relevant-but-zero-credit (no `defined_symbols` / breadcrumb / definition-keyword line could credit a call site). Anchor `{pipeline.rs, generate_segment_id}` → `{src/storage/segments.rs, generate_segment_id}`, the canonical definition seam. Sibling anchor `compute_file_hash` (defined in `src/indexer/pipeline.rs`, `fn compute_file_hash`) is correct and retained. |
| 10 | impact horizon expansion with owner-aware corroboration | OK | `ImpactHorizonEngine`, `explore`, `OWNER_ALIGNMENT_SIGNAL_THRESHOLD` (impact.rs) present. |
| 11 | daemon search IPC over Unix domain socket | OK | `bind_listener`, `request_search` (search_service.rs), `read_json_frame`, `write_json_frame` (ipc.rs) present. |
| 12 | CLI command dispatch for search and impact | OK | `Command`, `run` (cli/mod.rs) and the `Command::Search(args) => search::exec` dispatch line present. |
| 13 | rebuild index after schema version bump | REPAIRED | `src/storage/schema.rs` has no `rebuild` symbol; index rebuild is the build-aside staging + atomic swap in `src/storage/swap.rs`. Anchor `{schema.rs, rebuild}` → `{src/storage/swap.rs, swap_index_into_place}`, the canonical storage rebuild seam. Sibling anchors `exec`, `run_reindex_once` (cli/reindex.rs, the CLI entry) unchanged and present. |
| 14 | relation descriptor with lookup canonical symbol and qualifier fingerprint | OK | `RelationInsert`, `relation_target_descriptor`, `RelationTargetDescriptor`, `build_relation_inserts` (relations.rs) present. |
| 15 | benchmark parallel indexing script emitting JSON summary | OK | All three `line_contains` anchors present in `scripts/benchmark_parallel_indexing.sh`. |

**Summary**: 11 OK, 4 REPAIRED (labels 2, 5, 9, 13 — all relevant-but-zero-credit symbol
drift from renames/schema evolution/symbol relocation, corrected to the current canonical seam).

## Schema-19 recapture (MANUAL, credentialed — never in-agent)

The pinned `recall-baseline.json` and `recall-results.json` remain on **schema 18** with
the pre-audit corpus sha (`46800759…`). They are intentionally left stale in-agent: the
model-credentialed recapture is a Definition-of-Done step and must not run inside the
build agent. After this audit lands, the nightly `embedding-quality` recall gate
(`just eval-recall`) will fail-closed with a baseline/candidate config mismatch (schema
18 vs the binary's schema 19, and corpus sha `46800759…` vs `ae668c77…`) until the
recapture below is performed. That is the intended forcing function.

To recapture on schema 19 (operator, on a model-enabled machine):

1. `just eval-recall-baseline` (int8 pinned leg) — the only sanctioned way to move
   `recall-baseline.json`; it stamps `schema_version: 19`, `model_id`, and corpus
   `{size: 15, sha256: ae668c77…}`.
2. Recapture the fp32 A/B comparison leg (`ONEUP_MODEL_VARIANT=fp32`, compared with
   `allowModelIdMismatch`) so both legs sit on schema 19 against the audited corpus.
3. Commit the regenerated `recall-baseline.json` and `recall-results.json`.

`recall-compare.ts` is schema-agnostic (it compares baseline and candidate schema
versions by equality, not against a hardcoded epoch), so no code change is required for
schema 19 — parity is enforced generically and the drift guard in
`recall-compare.test.ts` confirms the committed baseline and results stay on a single
schema with a shared corpus identity.
