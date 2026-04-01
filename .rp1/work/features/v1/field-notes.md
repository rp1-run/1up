# Field Notes: v1

## FN-001: Int8 Vector Search Dimension Mismatch

**Discovered**: T14 (Integration Testing)
**Severity**: Bug (search path broken)
**Status**: Unfixed (out of T14 scope)

**Observation**: When the embedding model is available and embeddings are stored during indexing, the `vector_distance_cos` SQL function fails with:

```
vector_distance: vectors must have the same length: 96 != 384
```

**Root Cause**: The int8 quantization in `pipeline.rs::f32_to_q8_bytes()` produces 384 bytes (one i8 per dimension), but the `VECTOR8(384)` column type in libSQL expects a different byte layout. The query vector produced by the embedder is 384 f32 values (1536 bytes as f32, or 384 bytes as int8). The mismatch (96 vs 384) suggests the query vector bytes are being interpreted differently by `vector_distance_cos` -- likely the query vector is being passed as f32 bytes (384 * 4 = 1536 bytes) while the stored vector is 384 bytes, and libSQL interprets the byte count as the vector length (1536/16 = 96 or 384/1 = 384).

**Impact**: Hybrid search (vector + FTS) is completely broken when embeddings exist. FTS-only search works correctly.

**Workaround**: Integration tests hide the embedding model during search tests to force FTS-only mode.

**Fix Direction**: The query vector passed to `vector_distance_cos` for the `embedding_q8` column needs to be quantized to int8 format (matching `f32_to_q8_bytes`) before being sent as a blob parameter. Currently, `hybrid.rs::vector_search()` sends the f32 query embedding bytes to match against the `embedding_q8` column.

## FN-002: Clippy Error in Embedder Tests

**Discovered**: T14 (Integration Testing)
**Severity**: Minor (test quality)
**Status**: Pre-existing (T6 code)

**Observation**: `src/indexer/embedder.rs:410` contains `available || !available` which clippy flags as a logic bug (always true). The test `model_availability_check` effectively tests nothing.

**Fix**: Replace with `let _ = is_model_available();` or remove the test entirely.
