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

    #[error("project error: {0}")]
    Project(#[from] ProjectError),

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
    Transaction(String),
}

#[derive(Error, Debug)]
pub enum IndexingError {
    #[error("scan failed: {0}")]
    Scan(String),

    #[error("file read failed: {path}: {source}")]
    FileRead {
        path: String,
        source: std::io::Error,
    },

    #[error("pipeline failed: {0}")]
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
    AlreadyRunning(u32),

    #[error("daemon not running")]
    NotRunning,

    #[error("pid file error: {0}")]
    PidFileError(String),

    #[error("signal error: {0}")]
    SignalError(String),

    #[error("watcher error: {0}")]
    WatcherError(String),
}

#[derive(Error, Debug)]
pub enum ConfigError {
    #[error("XDG directory not found: {0}")]
    XdgDirNotFound(String),

    #[error("config read failed: {0}")]
    ReadFailed(String),
}

#[derive(Error, Debug)]
pub enum ProjectError {
    #[error("project not initialized (run `1up init` first)")]
    NotInitialized,

    #[error("project already initialized at {0}")]
    AlreadyInitialized(String),

    #[error("project ID read failed: {0}")]
    ReadFailed(String),

    #[error("project ID write failed: {0}")]
    WriteFailed(String),
}

pub type Result<T> = std::result::Result<T, OneupError>;
