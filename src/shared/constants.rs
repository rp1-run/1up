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

/// Maximum search results returned per query.
pub const MAX_SEARCH_RESULTS: usize = 20;

/// Maximum results per file in search output.
pub const MAX_RESULTS_PER_FILE: usize = 3;

/// Default context expansion window (lines) when tree-sitter is unavailable.
pub const CONTEXT_FALLBACK_LINES: usize = 50;

/// Sliding window size (lines) for text chunker.
pub const CHUNK_WINDOW_SIZE: usize = 60;

/// Sliding window overlap (lines) for text chunker.
pub const CHUNK_OVERLAP: usize = 10;

/// Daemon idle timeout in seconds (default: 30 minutes).
pub const DAEMON_IDLE_TIMEOUT_SECS: u64 = 1800;

/// Debounce interval for file watcher events in milliseconds.
pub const WATCHER_DEBOUNCE_MS: u64 = 500;

/// Schema version for database migrations.
pub const SCHEMA_VERSION: u32 = 2;

/// ONNX model filename.
pub const MODEL_FILENAME: &str = "model.onnx";

/// Tokenizer filename.
pub const TOKENIZER_FILENAME: &str = "tokenizer.json";

/// Hugging Face model repository for auto-download.
pub const HF_MODEL_REPO: &str = "sentence-transformers/all-MiniLM-L6-v2";

/// Base URL for Hugging Face model downloads.
pub const HF_BASE_URL: &str = "https://huggingface.co";
