/// Embedding vector dimensionality (all-MiniLM-L6-v2).
pub const EMBEDDING_DIM: usize = 384;

/// 1up version from Cargo.toml, embedded at compile time.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Default batch size for embedding inference.
pub const EMBEDDING_BATCH_SIZE: usize = 32;

/// Maximum token length for the embedding model.
pub const EMBEDDING_MAX_TOKENS: usize = 256;

/// Chunk-segment languages excluded from embedding.
///
/// Structural segments always embed; text-chunked configuration and data
/// formats carry little semantic signal per token. This list is shared by the
/// pipeline's embed decision and the storage coverage counters so reported
/// vector coverage always matches what the pipeline would embed.
pub const NON_EMBEDDABLE_CHUNK_LANGUAGES: [&str; 9] = [
    "json",
    "yaml",
    "toml",
    "protobuf",
    "terraform",
    "sql",
    "config",
    "makefile",
    "dockerfile",
];

/// Default number of vector search prefilter candidates (int8 stage).
///
/// Tuned to 400 for schema v13's FLOAT8 vectors: quantization makes the top-K
/// ranking slightly noisier, so a wider candidate pool gives the RRF reranker
/// enough coverage to recover gold segments that drift out of the top 200 but
/// are still in the right neighbourhood. Doubling K closed the recall gap
/// introduced by the FLOAT32 -> FLOAT8 column shift with no measurable search
/// latency impact. The constant serves both vector paths: it is the LIMIT of
/// the exhaustive-scan path (where candidate cost is one sort, so 400 keeps
/// the reranker pool identical to the index path) and the per-context K for
/// the approximate index path above
/// [`VECTOR_EXHAUSTIVE_SCAN_MAX_VECTORS`], where it remains the
/// recall/latency tradeoff originally tuned for quantization noise.
pub const VECTOR_PREFILTER_K: usize = 400;

/// Maximum context-scoped vector count served by the exhaustive-scan path.
///
/// The disk-based approximate vector index answers `vector_top_k` by beam
/// traversal over a neighbor graph, which is read-heavy and pathologically
/// slow at small corpus sizes (observed: ~7s single-thread CPU for one query
/// over ~4.5k vectors). An exhaustive scan is mathematically trivial at this
/// scale: 16384 x 384 int8 dot products is ~6.3M multiply-accumulates, well
/// under 10ms, and it is exact rather than approximate. Below this bound
/// vector candidates come from a full `vector_distance_cos` scan over the
/// context's vectors; above it the graph traversal amortizes and the
/// `vector_top_k` index path takes over.
pub const VECTOR_EXHAUSTIVE_SCAN_MAX_VECTORS: usize = 16384;

/// Maximum number of indexed worktree contexts used to scale vector prefiltering.
///
/// libSQL vector search runs against the shared vector index before context
/// filtering, so linked worktrees dilute the active context's candidate share.
/// Scaling by context count preserves recall while bounding worst-case latency.
pub const VECTOR_PREFILTER_CONTEXT_SCALE_LIMIT: usize = 8;

/// RRF fusion constant.
pub const RRF_K: f64 = 60.0;

/// Weight multiplier for vector search scores in RRF fusion.
pub const VECTOR_WEIGHT: f64 = 1.5;

/// Weight multiplier for exact/fuzzy symbol search scores in RRF fusion.
pub const SYMBOL_WEIGHT: f64 = 4.0;

/// Maximum search results returned per query.
pub const MAX_SEARCH_RESULTS: usize = 20;

/// Maximum size of a framed daemon request payload in bytes.
pub const MAX_DAEMON_REQUEST_BYTES: usize = 16 * 1024;

/// Maximum size of a framed daemon response payload in bytes.
pub const MAX_DAEMON_RESPONSE_BYTES: usize = 2 * 1024 * 1024;

/// Maximum daemon search query length in bytes.
pub const MAX_DAEMON_QUERY_BYTES: usize = 4 * 1024;

/// Read deadline for a single daemon IPC frame.
pub const DAEMON_READ_TIMEOUT_MS: u64 = 250;

/// Write deadline for a single daemon IPC frame.
pub const DAEMON_WRITE_TIMEOUT_MS: u64 = 250;

/// Maximum number of in-flight daemon requests before new clients are shed.
pub const MAX_DAEMON_IN_FLIGHT_REQUESTS: usize = 8;

/// Maximum results per file in search output.
pub const MAX_RESULTS_PER_FILE: usize = 3;

/// Degraded-search reason emitted when the local index holds no vector rows
/// for the active context, so search stays FTS-only without touching the
/// embedder. Shared by the CLI and MCP vectorless gates so degraded-mode
/// wording cannot drift between surfaces.
pub const NO_INDEXED_EMBEDDINGS_REASON: &str =
    "index contains no embeddings for this context; semantic ranking disabled (FTS-only)";

/// Degraded-search reason emitted when a non-destructive index rebuild is in
/// progress and the prior index is still being served (stale-but-available).
/// Carried on the existing `degraded_reason` channel — never on the
/// machine-readable result stream — and combined with any other degraded
/// reason (e.g. [`NO_INDEXED_EMBEDDINGS_REASON`]) rather than replacing it.
/// Single source of truth for this wording so the rebuild/stale notice cannot
/// drift between the CLI and MCP surfaces.
// Define-ahead-of-use: folded into `degraded_reason` by T6; the `#[allow]`
// drops once that producer lands.
#[allow(dead_code)]
pub const STALE_REBUILD_REASON: &str = "index is rebuilding; results may be stale";

/// Default context expansion window (lines) when tree-sitter is unavailable.
pub const CONTEXT_FALLBACK_LINES: usize = 50;

/// Sliding window size (lines) for text chunker.
pub const CHUNK_WINDOW_SIZE: usize = 60;

/// Sliding window overlap (lines) for text chunker.
pub const CHUNK_OVERLAP: usize = 10;

/// Debounce interval for file watcher events in milliseconds.
pub const WATCHER_DEBOUNCE_MS: u64 = 500;

/// Maximum interval between persisted daemon file-check heartbeats.
pub const DAEMON_FILE_CHECK_PERSIST_INTERVAL_MS: u64 = 30_000;

/// Bounded timeout for a graceful daemon drain (SIGTERM + poll).
///
/// Reused by `1up update`'s pre-update stop and by the post-upgrade
/// version-handshake drain/restart on the search path. ~3s (30 x 100ms order
/// of magnitude) is the conservative bound inherited from the original update
/// stop primitive; it now wraps a genuinely-cancellable indexing pass (the
/// daemon cancels in-flight work on SIGTERM), with the local in-process search
/// as the safety net if a drain still exceeds it.
pub const DAEMON_DRAIN_TIMEOUT_MS: u64 = 3_000;

/// Poll interval while waiting for a drained daemon to exit after SIGTERM.
pub const DAEMON_DRAIN_POLL_INTERVAL_MS: u64 = 100;

/// Whether the post-upgrade daemon auto-restart is gated on an idle/size
/// threshold before draining and restarting on a detected version mismatch.
///
/// Decision (REQ-004, OQ-003): `false` — there is **no** idle/size gating by
/// default. On a detected `daemon_version` mismatch the search path always
/// drains the stale daemon and restarts under the current binary (trigger
/// point: `src/cli/search.rs`). Serving silently wrong-version results is the
/// headline hazard this phase retires, so correctness is preferred over the
/// small risk of interrupting active work; the drain is bounded
/// ([`DAEMON_DRAIN_TIMEOUT_MS`]) and falls back to local in-process search, so
/// an unconditional restart can never strand the user.
///
/// The specific idle/size thresholds are an open owner decision (OQ-003): a
/// future owner can flip this to `true` and add the gating check at the
/// trigger point without re-deriving the rationale recorded here. Provisioned
/// (unused until that owner decision) per the codebase's pre-provisioned-API
/// convention.
#[allow(dead_code)]
pub const DAEMON_AUTO_RESTART_GATING_ENABLED: bool = false;

/// Bounded wait for the single-writer rebuild lock before a synchronous
/// one-shot rebuild (CLI `index`/`reindex`, MCP indexing) fails closed rather
/// than racing a competing rebuild of the shared `.1up/index.db`. The daemon
/// instead acquires the lock non-blockingly and defers the pass, so this bound
/// governs only the user-driven one-shot commands.
pub const REBUILD_LOCK_CONTENTION_TIMEOUT_MS: u64 = 5_000;

/// Poll interval while waiting for a contended rebuild lock to be released.
pub const REBUILD_LOCK_RETRY_INTERVAL_MS: u64 = 200;

/// Number of retries for transient database lock failures.
pub const DB_LOCK_RETRY_ATTEMPTS: usize = 10;

/// Delay between transient database lock retries.
pub const DB_LOCK_RETRY_DELAY_MS: u64 = 50;

/// Owner-only permissions for the XDG-managed state directory.
#[allow(dead_code)]
pub const XDG_STATE_DIR_MODE: u32 = 0o700;

/// Owner-only permissions for the project-local `.1up` directory.
#[allow(dead_code)]
pub const PROJECT_STATE_DIR_MODE: u32 = 0o700;

/// Owner-only permissions for security-sensitive state files.
#[allow(dead_code)]
pub const SECURE_STATE_FILE_MODE: u32 = 0o600;

/// Owner-only permissions for daemon socket files after bind.
#[allow(dead_code)]
pub const SECURE_SOCKET_MODE: u32 = 0o600;

/// Conservative upper bound for auto-selected embedding threads.
pub const MAX_AUTO_EMBED_THREADS: usize = 4;

/// Minimum number of files written per auto-selected storage transaction.
pub const DEFAULT_INDEX_WRITE_BATCH_FILES: usize = 4;

/// Conservative upper bound for auto-selected storage transaction batches.
pub const MAX_AUTO_INDEX_WRITE_BATCH_FILES: usize = 16;

/// Environment variable for parse worker count.
pub const INDEX_JOBS_ENV_VAR: &str = "ONEUP_INDEX_JOBS";

/// Environment variable for ONNX intra-op thread count.
pub const EMBED_THREADS_ENV_VAR: &str = "ONEUP_EMBED_THREADS";

/// Environment variable for storage writer batch sizing.
pub const INDEX_WRITE_BATCH_FILES_ENV_VAR: &str = "ONEUP_INDEX_WRITE_BATCH_FILES";

/// Environment variable that disables embedding model auto-download when set
/// to any non-empty value other than `0`. CI sets this so test suites stay
/// hermetic: no spawned `1up` process may reach the network for model
/// artifacts, and model availability cannot flip mid-suite.
pub const DISABLE_MODEL_DOWNLOADS_ENV_VAR: &str = "ONEUP_DISABLE_MODEL_DOWNLOADS";

/// Schema version for database layout.
///
/// v16: markdown heading breadcrumbs store cleaned heading text (inline
/// HTML stripped, link text kept, whitespace collapsed). Stored breadcrumbs
/// and the embedding text composed from them change shape, so indexes built
/// at earlier versions are incompatible and require `1up reindex`.
pub const SCHEMA_VERSION: u32 = 16;

/// Context id used by legacy indexing paths until callers pass an explicit worktree context.
pub const DEFAULT_INDEX_CONTEXT_ID: &str = "default";

/// ONNX model filename.
pub const MODEL_FILENAME: &str = "model.onnx";

/// Tokenizer filename.
pub const TOKENIZER_FILENAME: &str = "tokenizer.json";

/// Verified model artifact store directory name.
pub const MODEL_VERIFIED_DIRNAME: &str = "verified";

/// Model artifact staging directory name.
pub const MODEL_STAGING_DIRNAME: &str = ".staging";

/// Active model artifact pointer filename.
pub const MODEL_CURRENT_MANIFEST_FILENAME: &str = "current.json";

/// Verified model artifact manifest filename.
pub const MODEL_ARTIFACT_MANIFEST_FILENAME: &str = "manifest.json";

/// Schema version for verified model artifact metadata.
pub const MODEL_ARTIFACT_MANIFEST_VERSION: u32 = 1;

/// Connect timeout for model downloads.
pub const MODEL_DOWNLOAD_CONNECT_TIMEOUT_SECS: u64 = 10;

/// Total request timeout for model downloads.
pub const MODEL_DOWNLOAD_TIMEOUT_SECS: u64 = 300;

/// Pinned SHA-256 digest for the ONNX embedding model.
pub const MODEL_ONNX_SHA256: &str =
    "6fd5d72fe4589f189f8ebc006442dbb529bb7ce38f8082112682524616046452";

/// Pinned SHA-256 digest for the tokenizer artifact.
pub const TOKENIZER_SHA256: &str =
    "be50c3628f2bf5bb5e3a7f17b1f74611b2561a3a27eeab05e5aa30f411572037";

/// Hugging Face model repository for auto-download.
pub const HF_MODEL_REPO: &str = "sentence-transformers/all-MiniLM-L6-v2";

/// Base URL for Hugging Face model downloads.
pub const HF_BASE_URL: &str = "https://huggingface.co";

/// Build/runtime env var that enables the update manifest endpoint.
///
/// Release builds set this at compile time. A runtime value overrides the
/// baked value; an empty runtime value disables updates for the current
/// process.
pub const UPDATE_MANIFEST_URL_ENV_VAR: &str = "ONEUP_UPDATE_MANIFEST_URL";

/// Filename for the local update-check cache.
pub const UPDATE_CHECK_CACHE_FILENAME: &str = "update-check.json";

/// User-facing message shown when the binary was built without update support.
pub const UPDATE_DISABLED_MESSAGE: &str = "Updates are disabled for this build.";

/// Time-to-live for the update-check cache in seconds (24 hours).
pub const UPDATE_CHECK_TTL_SECS: u64 = 86_400;

/// HTTP request timeout for update manifest fetches in seconds.
pub const UPDATE_CHECK_TIMEOUT_SECS: u64 = 5;

/// TCP connect timeout for update manifest fetches in seconds.
pub const UPDATE_CHECK_CONNECT_TIMEOUT_SECS: u64 = 3;

/// HTTP request timeout for update binary downloads in seconds.
pub const UPDATE_DOWNLOAD_TIMEOUT_SECS: u64 = 300;

/// TCP connect timeout for update binary downloads in seconds.
pub const UPDATE_DOWNLOAD_CONNECT_TIMEOUT_SECS: u64 = 10;
