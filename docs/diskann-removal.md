# DiskANN vector-index removal (schema v20)

Schema v20 removes the approximate DiskANN vector index
(`idx_embedding_pool_embedding`, a libsql `libsql_vector_idx` expression index
on `embedding_pool.embedding_vec`) and every code path that consulted it: the
`vector_top_k` candidate queries, the exhaustive-vs-ANN path selection, the
`ONEUP_FORCE_ANN_SEARCH` opt-in, and the deferred index build in the staging
rebuild. The exact `vector_distance_cos` scan
(`SELECT_VECTOR_CANDIDATES_EXHAUSTIVE_FOR_CONTEXT`) is the only vector search
path.

v20 also drops `idx_segment_vectors_content_key`. That index existed solely to
back the ANN fan-out join (mapping `vector_top_k` pool hits back to segments
via `segment_vectors.content_key`); the exact scan drives the join from
`segment_vectors` into `embedding_pool`'s `content_key` primary key, and
benchmarking the v20 query without the index showed no measurable latency
change.

This note preserves the measurements that justified the removal, so the
machinery itself does not have to be kept around as documentation.

## Why it was removed

1. **The ANN path was measured slower at every tested corpus size, and
   superlinear.** Profiling on the emdash corpus (PR #132, commit `9baa4f5`)
   found `vector_top_k` beam traversal over the on-disk neighbor graph
   spending seconds in read-heavy graph walks:

   | corpus size | ANN `vector_top_k` query | exact scan |
   |------------:|-------------------------:|-----------:|
   | ~4,500 vectors | ~7 s (single-thread CPU) | single-digit ms |
   | ~27,000 vectors | ~45 s | tens of ms |

   Contrary to the original "amortizes at scale" assumption, the ANN path got
   *worse* as the corpus grew, not better. PR #132 demoted it to the
   undocumented `ONEUP_FORCE_ANN_SEARCH` opt-in; an undocumented opt-in that
   is measured slower at every tested size is not a useful fallback, so v20
   removes it outright.

2. **The index carried a large storage cost on every index.** On a real
   production index, the DiskANN graph-storage table
   `idx_embedding_pool_embedding_shadow` occupied 114,307,072 bytes (~109 MiB)
   inside a 2.2 GiB `index.db` — paid on every rebuild and every byte of it
   dead weight for a path that was never taken by default.

3. **It complicated the cold-rebuild pipeline.** Building the graph
   incrementally per insert was so expensive that the staging rebuild grew a
   deferred-build mode (`VectorIndexBuild::Deferred` +
   `build_embedding_pool_vector_index`) that intentionally left the staging
   schema incomplete until after the pool was loaded. All of that is gone with
   the index.

## Exact-scan headroom (measured at v20)

The exact scan is a single linear pass of ~N × 384 dot products. Measured with
the production query shape (`SELECT_VECTOR_CANDIDATES_EXHAUSTIVE_FOR_CONTEXT`,
`LIMIT 400`, FLOAT8(384) pool vectors, warm cache; libsql 0.9, Apple Silicon,
release build; median of 5 runs):

| corpus size | median query latency | db size |
|------------:|---------------------:|--------:|
| 100,000 vectors | ~86 ms | ~98 MiB |
| 250,000 vectors | ~219 ms | ~198 MiB |
| 500,000 vectors | ~446 ms | ~362 MiB |

Latency is linear in corpus size (~0.9 µs/vector) and stays sub-second at
500k vectors — roughly 50× the vector count of a large single-repo index
(~10 vectors/file average, so 500k vectors ≈ a 50k-file scoped corpus). For
calibration, the repo-scale guardrail bench
(`search_latency_vector_exhaustive_scan_4k5` in `benches/search_bench.rs`)
pins the ~4.5k-vector case at single-digit milliseconds.

Because the scan's latency still grows linearly, `1up` emits a one-time
`tracing::warn!` when a context's vector count exceeds
`VECTOR_EXACT_SCAN_WARN_THRESHOLD` (262,144). That constant is the renamed
remnant of the old auto-switch cutoff and now serves only the warning.

## Compatibility

Removing the indexes changes the persisted schema contract
(`REQUIRED_SCHEMA_OBJECTS` no longer contains `idx_embedding_pool_embedding`
or `idx_segment_vectors_content_key`, and fresh initializes no longer create
them), so `SCHEMA_VERSION` is bumped to 20. v19 indexes fail closed with the
standard "out of date … run `1up reindex`" guidance; the rebuild sheds both
indexes and the `_shadow` table. No in-place migration is attempted, per the
project's fail-closed schema policy.

## If ANN is ever needed again

Reintroduce it only with measurements showing it beating the exact scan on a
realistic corpus (the old implementation used
`libsql_vector_idx(embedding_vec, 'metric=cosine', 'compress_neighbors=float8',
'max_neighbors=32')`; `max_neighbors=32` was chosen to hold the index under
~80 MiB — the default ~62 for 384d pushed it to ~95 MiB with no measurable
recall gain). Any successor must also solve the problems the old path never
did: `vector_top_k` truncates before per-context and path-prefix filters run,
which starves scoped searches, and the graph must be built post-load in the
staging rebuild to avoid pathological per-insert maintenance.
