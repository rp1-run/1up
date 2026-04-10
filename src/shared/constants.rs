/// Embedding vector dimensionality (all-MiniLM-L6-v2).
pub const EMBEDDING_DIM: usize = 384;

/// Default batch size for embedding inference.
pub const EMBEDDING_BATCH_SIZE: usize = 32;

/// Maximum token length for the embedding model.
pub const EMBEDDING_MAX_TOKENS: usize = 256;

/// Default number of vector search prefilter candidates (int8 stage).
pub const VECTOR_PREFILTER_K: usize = 200;

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

/// Schema version for database layout.
pub const SCHEMA_VERSION: u32 = 7;

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

/// Target files for 1up fence installation.
pub const FENCE_TARGET_FILES: &[&str] = &["AGENTS.md", "CLAUDE.md"];
