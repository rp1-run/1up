use thiserror::Error;

/// Top-level error type for 1up operations.
#[derive(Error, Debug)]
pub enum OneupError {
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),

    #[error("indexing error: {0}")]
    Indexing(#[from] IndexingError),

    #[error("search error: {0}")]
    Search(#[from] SearchError),

    #[error("embedding error: {0}")]
    Embedding(#[from] EmbeddingError),

    #[error("parser error: {0}")]
    Parser(#[from] ParserError),

    #[error("daemon error: {0}")]
    Daemon(#[from] DaemonError),

    #[error("config error: {0}")]
    Config(#[from] ConfigError),

    #[error("filesystem error: {0}")]
    Filesystem(#[from] FilesystemError),

    #[error("project error: {0}")]
    Project(#[from] ProjectError),

    #[error("fence error: {0}")]
    Fence(#[from] FenceError),

    #[error("update error: {0}")]
    Update(#[from] UpdateError),

    #[error("{0}")]
    Other(#[from] anyhow::Error),
}

#[derive(Error, Debug)]
pub enum StorageError {
    #[error("database connection failed: {0}")]
    Connection(String),

    #[error("schema migration failed: {0}")]
    Migration(String),

    #[error("query failed: {0}")]
    Query(String),

    #[error("transaction failed: {0}")]
    #[allow(dead_code)]
    Transaction(String),
}

#[derive(Error, Debug)]
pub enum IndexingError {
    #[error("scan failed: {0}")]
    Scan(String),

    #[error("file read failed: {path}: {source}")]
    #[allow(dead_code)]
    FileRead {
        path: String,
        source: std::io::Error,
    },

    #[error("pipeline failed: {0}")]
    #[allow(dead_code)]
    Pipeline(String),
}

#[derive(Error, Debug)]
pub enum SearchError {
    #[error("query execution failed: {0}")]
    QueryFailed(String),

    #[error("invalid query: {0}")]
    InvalidQuery(String),
}

#[derive(Error, Debug)]
pub enum EmbeddingError {
    #[error("model not available: {0}")]
    ModelNotAvailable(String),

    #[error("model download failed: {0}")]
    DownloadFailed(String),

    #[error("inference failed: {0}")]
    InferenceFailed(String),

    #[error("tokenization failed: {0}")]
    TokenizationFailed(String),
}

#[derive(Error, Debug)]
pub enum ParserError {
    #[error("unsupported language: {0}")]
    UnsupportedLanguage(String),

    #[error("parse failed: {0}")]
    ParseFailed(String),
}

#[derive(Error, Debug)]
pub enum DaemonError {
    #[error("daemon already running (pid: {0})")]
    #[allow(dead_code)]
    AlreadyRunning(u32),

    #[error("daemon not running")]
    #[allow(dead_code)]
    NotRunning,

    #[error("pid file error: {0}")]
    PidFileError(String),

    #[error("signal error: {0}")]
    SignalError(String),

    #[error("request error: {0}")]
    RequestError(String),

    #[error("watcher error: {0}")]
    WatcherError(String),
}

#[derive(Error, Debug)]
pub enum ConfigError {
    #[error("XDG directory not found: {0}")]
    XdgDirNotFound(String),

    #[error("config read failed: {0}")]
    #[allow(dead_code)]
    ReadFailed(String),
}

#[allow(dead_code)]
#[derive(Error, Debug)]
pub enum FilesystemError {
    #[error("invalid filesystem path: {0}")]
    InvalidPath(String),

    #[error("path contains a symlinked component: {0}")]
    SymlinkComponent(String),

    #[error("path is outside approved root {root}: {path}")]
    OutsideApprovedRoot { path: String, root: String },

    #[error("unexpected filesystem object at {path}: expected {expected}, found {found}")]
    UnexpectedType {
        path: String,
        expected: String,
        found: String,
    },

    #[error("filesystem operation failed for {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
}

#[derive(Error, Debug)]
pub enum ProjectError {
    #[error("project not initialized (run `1up init` first)")]
    NotInitialized,

    #[error("project already initialized at {0}")]
    #[allow(dead_code)]
    AlreadyInitialized(String),

    #[error("project ID read failed: {0}")]
    #[allow(dead_code)]
    ReadFailed(String),

    #[error("project ID write failed: {0}")]
    WriteFailed(String),
}

#[derive(Error, Debug)]
pub enum FenceError {
    #[error("malformed 1up fence: {0}")]
    Malformed(String),
}

#[derive(Error, Debug)]
pub enum UpdateError {
    #[error("update check failed: {0}")]
    FetchFailed(String),

    #[error("manifest parse failed: {0}")]
    ParseFailed(String),

    #[error("update cache error: {0}")]
    CacheError(String),

    #[error("self-update failed: {0}")]
    SelfUpdateFailed(String),

    #[error("daemon stop required for update but failed: {0}")]
    DaemonStopFailed(String),

    #[error("no artifact available for platform: {0}")]
    NoArtifactForPlatform(String),

    #[error("checksum verification failed")]
    ChecksumMismatch,
}

#[allow(dead_code)]
pub type Result<T> = std::result::Result<T, OneupError>;
