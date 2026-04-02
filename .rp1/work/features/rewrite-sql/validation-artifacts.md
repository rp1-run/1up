# Validation Artifacts: rewrite-sql

**Validated**: 2026-04-02T09:09:00Z

## HYP-001 Result Snapshot

- Local experiment used `libsql 0.9.30` with `Builder::new_local(...).build().await`.
- Created `segments.embedding_vec FLOAT32(384)` and `idx_segments_embedding` using `libsql_vector_idx(embedding_vec)`.
- Inserted 3 rows via `vector(?)`.
- `vector_top_k(...)` returned only the `id` column in this local path.
- Joining `vector_top_k(...)` back to `segments` returned `seg-a` then `seg-b` for the nearest-neighbor query.

## HYP-002 Result Snapshot

| Rows | Non-null legacy embeddings | Backfill ms | Index build ms | Vector query ms | Parity | Top result |
|------|----------------------------|-------------|----------------|-----------------|--------|------------|
| 512 | 449 | 11 | 3994 | 5 | true | `seg-target` |
| 1024 | 897 | 27 | 8761 | 5 | true | `seg-target` |
| 2048 | 1793 | 44 | 16468 | 6 | true | `seg-target` |
| 4096 | 3585 | 84 | 32820 | 22 | true | `seg-target` |

## Notes

- The copied-index migration prototype used the current schema-v4-style storage contract where `segments.embedding` is JSON `TEXT`.
- The blocking cost was vector index creation, not JSON-to-vector conversion.

## T4 Benchmark Snapshot

**Measured**: 2026-04-02T20:09:00+11:00

- Script: `scripts/benchmark_rewrite_sql.sh`
- Baseline ref: `45b5117`
- Candidate ref: `d7e89b4`
- Raw summary: `target/rewrite-sql-bench/20260402-200900/summary.md`
- Corpus: generated 33-file mixed-language fixture, 3 clean rebuild runs, 7 query runs with 1 warmup

### Clean rebuild latency

| Variant | Median ms | p95 ms |
|---------|-----------|--------|
| Baseline | 1257.92 | 2296.19 |
| Candidate | 1336.83 | 2267.07 |

### Search latency

| Query | Expected file | Baseline median ms | Baseline p95 ms | Candidate median ms | Candidate p95 ms |
|-------|---------------|--------------------|-----------------|---------------------|------------------|
| `config loading host port` | `src/config.rs` | 137.71 | 145.09 | 131.54 | 137.30 |
| `request auth token validation middleware` | `src/auth.rs` | 137.97 | 146.03 | 129.65 | 135.36 |
| `request pipeline json response rendering` | `src/server.rs` | 136.69 | 140.42 | 134.31 | 138.78 |
| `serialize json response payload` | `tools/output.py` | 139.62 | 144.16 | 133.59 | 137.92 |
| `billing invoice total tax calculation` | `web/billing.js` | 139.47 | 144.92 | 133.77 | 136.07 |

### Quality corpus

| Query | Expected file | Baseline top-3 hit | Candidate top-3 hit | Notes |
|-------|---------------|--------------------|---------------------|-------|
| `config loading host port` | `src/config.rs` | Yes | Yes | Same top-3 ordering |
| `request auth token validation middleware` | `src/auth.rs` | No | No | Shared ranking miss; no rewrite regression |
| `request pipeline json response rendering` | `src/server.rs` | Yes | Yes | Same top-3 ordering |
| `serialize json response payload` | `tools/output.py` | Yes | Yes | Same top-3 ordering |
| `billing invoice total tax calculation` | `web/billing.js` | Yes | Yes | Candidate added extra tail results but kept the expected top hit |

## T4 Operational Stability Follow-up

**Measured**: 2026-04-02T20:09:00+11:00

- Script: `scripts/benchmark_rewrite_sql.sh`
- Raw summary: `target/rewrite-sql-bench/20260402-200900/summary.md`
- Method: repeated baseline-versus-candidate add/edit/delete churn on the same 33-file fixture; manual intervention counts cover command failures and explicit `1up reindex` prompts.

### Operational stability comparison

| Variant | Churn cycles | Freshness checks passed | Command failures | Reindex prompts | Manual interventions |
|---------|--------------|-------------------------|------------------|-----------------|----------------------|
| Baseline | 3 | 12/12 | 0 | 0 | 0 |
| Candidate | 3 | 12/12 | 0 | 0 | 0 |

### Evidence Summary

- The refreshed current-`HEAD` benchmark packet improved candidate median and p95 search latency on all five representative queries versus the baseline commit.
- The earlier one-query p95 miss for `serialize json response payload` did not reproduce in the refreshed rerun, so the latest raw summary above should be treated as the rollout source of truth.
- Candidate rebuild latency still regressed on this fixture, so rebuild cost should stay part of rollout review and continue to block broad rollout.
- The quality corpus stayed flat versus baseline at 4/5 top-3 hits; the missed auth query was already missed before the rewrite, so this evidence shows no material relevance regression from the SQL retrieval change.
- Routine indexing burden stayed flat on the representative churn fixture: baseline and candidate each completed 12/12 freshness checks with zero command failures and zero explicit `1up reindex` prompts.

## T5 Rollout Review

**Prepared**: 2026-04-02

### Supported Adoption Path

1. Treat any pre-rewrite local index as disposable cache.
2. After upgrading to the schema-v5 binary, run `1up reindex [path]`.
3. `1up reindex` recreates the local search schema from scratch and repopulates `segments`, `segments_fts`, and `segments.embedding_vec` for the rebuilt index.
4. After a successful rebuild, `1up search` and `1up symbol` keep their current CLI and JSON contracts; `1up search` uses `SqlVectorV2` when embeddings are present and warns before degrading to `FtsOnly` when semantic retrieval is unavailable.
5. Unsupported paths: in-place migration, legacy schema-v4 reads, compatibility windows, and partial-index recovery without an explicit clean rebuild.

### Recovery Matrix

| Local index state | Expected behavior | Supported recovery |
|-------------------|-------------------|--------------------|
| Missing `.1up/index.db` | `search` and `symbol` fail closed with explicit `1up reindex` guidance | Run `1up reindex [path]` to create a fresh schema-v5 index |
| Stale v4 index | schema validation fails with `found v4`, `expected v5`, and `1up reindex` guidance | Run `1up reindex [path]`; no migration bridge is supported |
| Partial v5 index | schema validation fails closed on missing required objects such as `idx_segments_embedding` | Run `1up reindex [path]`; partial recovery is not supported |
| Current v5 index without semantic retrieval | `search` warns and degrades to `FtsOnly`, while `symbol` remains available | Indexed search stays usable; rerun `1up index` later if model download/load issues are resolved |

### Recovered State After Clean Rebuild

- `.1up/index.db` is recreated as schema v5 with `segments.embedding_vec` and `idx_segments_embedding`.
- `1up search` and `1up symbol` stop emitting stale-index rebuild guidance.
- `1up search` returns the existing machine-readable result shape on rebuilt indexes, using native-vector retrieval when embeddings are available.
- Incremental `1up index` resumes the add, edit, and delete freshness behavior validated in the rewrite verification suite.

### Maintainer Rollout Review

| Dimension | Evidence | Status | Notes |
|-----------|----------|--------|-------|
| Query latency | `target/rewrite-sql-bench/20260402-200900/summary.md` | Pass | Refreshed current-`HEAD` evidence shows candidate median and p95 improved on all five representative search queries |
| Search quality | `target/rewrite-sql-bench/20260402-200900/summary.md` | Pass | Candidate stayed at 4/5 top-3 hits, matching baseline; the missed auth query is pre-existing |
| Stale, missing, and partial index handling | `tests/rewrite_sql_verification.rs`, `tests/integration_tests.rs` | Pass | Query commands fail closed with explicit `1up reindex` guidance instead of legacy reads |
| Freshness after rebuild | `tests/rewrite_sql_verification.rs` | Pass | Rebuilt schema-v5 indexes stayed current across add, edit, and delete flows |
| Routine operational burden | `target/rewrite-sql-bench/20260402-200900/summary.md` | Pass | Baseline and candidate both completed the repeated add/edit/delete churn check with 12/12 freshness checks, zero command failures, and zero rebuild prompts |
| Graceful degradation | `tests/rewrite_sql_verification.rs` | Pass | Search warns and still returns FTS-backed results when semantic retrieval is unavailable |
| Read-only repository guarantee | `tests/rewrite_sql_verification.rs` | Pass | Index and search flows leave source files unchanged |
| Clean rebuild cost | `target/rewrite-sql-bench/20260402-200900/summary.md` | Block broad rollout | Candidate rebuild median still regressed on the benchmark fixture; broad rollout stays blocked until this required comparison category is resolved or explicitly signed off by maintainers |

### Recommendation

The rewrite remains a clean-break feature: stale local indexes should be discarded and rebuilt with `1up reindex`, not migrated. The refreshed current-`HEAD` benchmark packet clears the earlier `serialize json response payload` p95 concern and keeps steady-state query behavior, quality, degradation, and operational-burden evidence in a reviewable state, but the recorded clean-rebuild regression still blocks broad rollout until maintainers explicitly sign off or the rebuild-cost comparison is improved.

### Open Follow-up

- Use `target/rewrite-sql-bench/20260402-200900/summary.md` as the current rollout-evidence packet for any future verification or maintainer review.
- Remaining unblocker for broad rollout: either improve clean rebuild cost on the representative fixture or record explicit maintainer sign-off accepting the current rebuild regression against the refreshed benchmark packet.
