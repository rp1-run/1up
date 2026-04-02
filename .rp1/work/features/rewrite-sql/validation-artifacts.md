# Validation Artifacts: rewrite-sql

**Validated**: 2026-04-02T04:51:40Z

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

**Measured**: 2026-04-02T17:38:29+11:00

- Script: `scripts/benchmark_rewrite_sql.sh`
- Baseline ref: `45b5117`
- Candidate ref: `f7da967`
- Raw summary: `target/rewrite-sql-bench/20260402-173829/summary.md`
- Corpus: generated 33-file mixed-language fixture, 3 clean rebuild runs, 7 query runs with 1 warmup

### Clean rebuild latency

| Variant | Median ms | p95 ms |
|---------|-----------|--------|
| Baseline | 1199.45 | 2129.85 |
| Candidate | 1270.06 | 2592.02 |

### Search latency

| Query | Expected file | Baseline median ms | Baseline p95 ms | Candidate median ms | Candidate p95 ms |
|-------|---------------|--------------------|-----------------|---------------------|------------------|
| `config loading host port` | `src/config.rs` | 134.51 | 139.65 | 128.60 | 132.31 |
| `request auth token validation middleware` | `src/auth.rs` | 136.39 | 143.24 | 131.80 | 136.77 |
| `request pipeline json response rendering` | `src/server.rs` | 134.52 | 139.22 | 125.78 | 128.19 |
| `serialize json response payload` | `tools/output.py` | 136.68 | 146.61 | 129.09 | 131.58 |
| `billing invoice total tax calculation` | `web/billing.js` | 134.39 | 138.61 | 128.35 | 134.97 |

### Quality corpus

| Query | Expected file | Baseline top-3 hit | Candidate top-3 hit | Notes |
|-------|---------------|--------------------|---------------------|-------|
| `config loading host port` | `src/config.rs` | Yes | Yes | Same top-3 ordering |
| `request auth token validation middleware` | `src/auth.rs` | No | No | Shared ranking miss; no rewrite regression |
| `request pipeline json response rendering` | `src/server.rs` | Yes | Yes | Same top-3 ordering |
| `serialize json response payload` | `tools/output.py` | Yes | Yes | Same top-3 ordering |
| `billing invoice total tax calculation` | `web/billing.js` | Yes | Yes | Candidate added extra tail results but kept the expected top hit |

### Evidence Summary

- Candidate search latency improved on all five representative queries versus the baseline commit.
- Candidate rebuild latency regressed on this fixture, so rebuild cost should stay part of rollout review.
- The quality corpus stayed flat versus baseline at 4/5 top-3 hits; the missed auth query was already missed before the rewrite, so this evidence shows no material relevance regression from the SQL retrieval change.
