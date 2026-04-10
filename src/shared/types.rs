use std::collections::BTreeSet;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{de, Deserialize, Deserializer, Serialize};

use crate::shared::constants::{
    DEFAULT_INDEX_WRITE_BATCH_FILES, MAX_AUTO_EMBED_THREADS, MAX_AUTO_INDEX_WRITE_BATCH_FILES,
};

/// Role classification for a parsed code segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SegmentRole {
    Definition,
    Implementation,
    Orchestration,
    Import,
    Docs,
}

/// A parsed segment extracted from source code by tree-sitter or the text chunker.
#[derive(Debug, Clone)]
pub struct ParsedSegment {
    pub content: String,
    pub block_type: String,
    pub line_start: usize,
    pub line_end: usize,
    pub language: String,
    #[allow(dead_code)]
    pub breadcrumb: Option<String>,
    pub complexity: u32,
    pub role: SegmentRole,
    pub defined_symbols: Vec<String>,
    pub referenced_symbols: Vec<String>,
    pub called_symbols: Vec<String>,
}

/// A search result returned by hybrid or FTS-only search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub file_path: String,
    pub language: String,
    pub block_type: String,
    pub content: String,
    pub score: f64,
    pub line_number: usize,
    pub line_end: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub breadcrumb: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub complexity: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<SegmentRole>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub defined_symbols: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub referenced_symbols: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub called_symbols: Option<Vec<String>>,
}

impl SearchResult {
    pub fn is_definition_like(&self) -> bool {
        if matches!(self.role, Some(SegmentRole::Definition)) {
            return true;
        }

        let has_symbols = self
            .defined_symbols
            .as_ref()
            .map(|symbols| !symbols.is_empty())
            .unwrap_or(false);

        has_symbols
            && matches!(
                self.block_type.as_str(),
                "function"
                    | "method"
                    | "struct"
                    | "enum"
                    | "trait"
                    | "type"
                    | "class"
                    | "interface"
                    | "module"
                    | "macro"
                    | "constructor"
            )
    }

    pub fn display_kind(&self) -> &'static str {
        if self.is_definition_like() {
            "DEFINITION"
        } else {
            match self.role {
                Some(SegmentRole::Docs) => "DOCS",
                Some(SegmentRole::Import) => "IMPORT",
                Some(SegmentRole::Orchestration) => "FLOW",
                Some(SegmentRole::Implementation) => "IMPLEMENTATION",
                Some(SegmentRole::Definition) => "DEFINITION",
                None => "RESULT",
            }
        }
    }
}

/// Distinguishes between a symbol definition and a usage reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReferenceKind {
    Definition,
    Usage,
}

impl std::fmt::Display for ReferenceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReferenceKind::Definition => write!(f, "definition"),
            ReferenceKind::Usage => write!(f, "usage"),
        }
    }
}

/// A symbol lookup result.
#[derive(Debug, Clone, Serialize)]
pub struct SymbolResult {
    pub name: String,
    pub kind: String,
    pub file_path: String,
    pub language: String,
    pub line_start: usize,
    pub line_end: usize,
    pub content: String,
    pub reference_kind: ReferenceKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub breadcrumb: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub complexity: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<SegmentRole>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub defined_symbols: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub referenced_symbols: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub called_symbols: Option<Vec<String>>,
}

/// A structural search result from AST pattern matching.
#[derive(Debug, Clone, Serialize)]
pub struct StructuralResult {
    pub file_path: String,
    pub language: String,
    pub pattern_name: Option<String>,
    pub content: String,
    pub line_start: usize,
    pub line_end: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextAccessScope {
    ProjectRoot,
    OutsideRoot,
}

impl ContextAccessScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProjectRoot => "project_root",
            Self::OutsideRoot => "outside_root",
        }
    }
}

/// A context retrieval result with the enclosing scope.
#[derive(Debug, Clone, Serialize)]
pub struct ContextResult {
    pub file_path: String,
    pub language: String,
    pub content: String,
    pub line_start: usize,
    pub line_end: usize,
    pub scope_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_scope: Option<ContextAccessScope>,
}

/// Scope for an indexing run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunScope {
    Full,
    Paths(BTreeSet<PathBuf>),
}

impl RunScope {
    pub fn from_paths<I>(paths: I) -> Option<Self>
    where
        I: IntoIterator<Item = PathBuf>,
    {
        let paths: BTreeSet<PathBuf> = paths
            .into_iter()
            .filter(|path| !path.as_os_str().is_empty())
            .collect();

        if paths.is_empty() {
            None
        } else {
            Some(Self::Paths(paths))
        }
    }

    pub fn merge(&mut self, other: Self) {
        match other {
            Self::Full => *self = Self::Full,
            Self::Paths(other_paths) => match self {
                Self::Full => {}
                Self::Paths(paths) => {
                    paths.extend(other_paths);
                }
            },
        }
    }
}

/// Shared resolved indexing settings for a single run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IndexingConfig {
    pub jobs: usize,
    pub embed_threads: usize,
    pub write_batch_files: usize,
}

impl IndexingConfig {
    pub fn new(
        jobs: usize,
        embed_threads: usize,
        write_batch_files: usize,
    ) -> Result<Self, String> {
        if jobs == 0 {
            return Err("jobs must be at least 1".to_string());
        }
        if embed_threads == 0 {
            return Err("embed_threads must be at least 1".to_string());
        }
        if write_batch_files == 0 {
            return Err("write_batch_files must be at least 1".to_string());
        }

        Ok(Self {
            jobs,
            embed_threads,
            write_batch_files,
        })
    }

    #[allow(dead_code)]
    pub fn auto() -> Self {
        Self::from_sources(None, None, None).expect("automatic indexing defaults are valid")
    }

    pub fn from_sources(
        jobs: Option<usize>,
        embed_threads: Option<usize>,
        write_batch_files: Option<usize>,
    ) -> Result<Self, String> {
        let jobs = jobs.unwrap_or_else(Self::default_jobs);
        let embed_threads = embed_threads.unwrap_or_else(|| Self::default_embed_threads_for(jobs));
        let write_batch_files =
            write_batch_files.unwrap_or_else(|| Self::default_write_batch_files_for(jobs));

        Self::new(jobs, embed_threads, write_batch_files)
    }

    pub fn reporting_parallelism(
        &self,
        files_total: usize,
        embeddings_enabled: bool,
    ) -> IndexParallelism {
        IndexParallelism {
            jobs_configured: self.jobs,
            jobs_effective: files_total.min(self.jobs),
            embed_threads: if embeddings_enabled {
                self.embed_threads
            } else {
                0
            },
        }
    }

    pub fn default_jobs() -> usize {
        std::thread::available_parallelism()
            .map(std::num::NonZeroUsize::get)
            .unwrap_or(1)
            .saturating_sub(1)
            .max(1)
    }

    pub fn default_embed_threads_for(jobs: usize) -> usize {
        jobs.clamp(1, MAX_AUTO_EMBED_THREADS)
    }

    pub fn default_write_batch_files_for(jobs: usize) -> usize {
        jobs.clamp(
            DEFAULT_INDEX_WRITE_BATCH_FILES,
            MAX_AUTO_INDEX_WRITE_BATCH_FILES,
        )
    }

    pub fn effective_write_batch_files(&self, files_total: usize) -> usize {
        self.write_batch_files.min(files_total.max(1))
    }
}

impl<'de> Deserialize<'de> for IndexingConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawIndexingConfig {
            jobs: Option<usize>,
            embed_threads: Option<usize>,
            write_batch_files: Option<usize>,
        }

        let raw = RawIndexingConfig::deserialize(deserializer)?;
        IndexingConfig::from_sources(raw.jobs, raw.embed_threads, raw.write_batch_files)
            .map_err(de::Error::custom)
    }
}

/// Persisted or reported indexing parallelism values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexParallelism {
    pub jobs_configured: usize,
    pub jobs_effective: usize,
    pub embed_threads: usize,
}

/// Stage-level timing data for an indexing run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexStageTimings {
    pub scan_ms: u128,
    pub parse_ms: u128,
    pub embed_ms: u128,
    pub store_ms: u128,
    pub total_ms: u128,
}

/// High-level state for the latest indexing run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexState {
    Idle,
    Running,
    Complete,
}

/// Current milestone within an indexing run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexPhase {
    Pending,
    Scanning,
    Parsing,
    Storing,
    Complete,
}

/// Latest persisted indexing progress for a project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexProgress {
    pub state: IndexState,
    pub phase: IndexPhase,
    pub files_total: usize,
    pub files_scanned: usize,
    pub files_indexed: usize,
    pub files_skipped: usize,
    pub files_deleted: usize,
    pub segments_stored: usize,
    pub embeddings_enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallelism: Option<IndexParallelism>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timings: Option<IndexStageTimings>,
    pub updated_at: DateTime<Utc>,
}

impl IndexProgress {
    pub fn pending() -> Self {
        Self {
            state: IndexState::Idle,
            phase: IndexPhase::Pending,
            files_total: 0,
            files_scanned: 0,
            files_indexed: 0,
            files_skipped: 0,
            files_deleted: 0,
            segments_stored: 0,
            embeddings_enabled: false,
            parallelism: None,
            timings: None,
            updated_at: Utc::now(),
        }
    }
}

/// Latest persisted daemon heartbeat for file checks on a project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonProjectStatus {
    pub last_file_check_at: DateTime<Utc>,
}

/// Output format for CLI results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputFormat {
    #[default]
    Json,
    Human,
    Plain,
}

impl std::fmt::Display for OutputFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OutputFormat::Json => write!(f, "json"),
            OutputFormat::Human => write!(f, "human"),
            OutputFormat::Plain => write!(f, "plain"),
        }
    }
}

impl std::str::FromStr for OutputFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "json" => Ok(OutputFormat::Json),
            "human" => Ok(OutputFormat::Human),
            "plain" => Ok(OutputFormat::Plain),
            other => Err(format!("unknown output format: {other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::constants::{MAX_AUTO_EMBED_THREADS, MAX_AUTO_INDEX_WRITE_BATCH_FILES};

    #[test]
    fn default_embed_threads_cap_auto_parallelism() {
        assert_eq!(IndexingConfig::default_embed_threads_for(1), 1);
        assert_eq!(
            IndexingConfig::default_embed_threads_for(MAX_AUTO_EMBED_THREADS + 8),
            MAX_AUTO_EMBED_THREADS
        );
    }

    #[test]
    fn default_write_batch_files_cap_auto_parallelism() {
        assert_eq!(
            IndexingConfig::default_write_batch_files_for(1),
            DEFAULT_INDEX_WRITE_BATCH_FILES
        );
        assert_eq!(
            IndexingConfig::default_write_batch_files_for(MAX_AUTO_INDEX_WRITE_BATCH_FILES + 8),
            MAX_AUTO_INDEX_WRITE_BATCH_FILES
        );
    }

    #[test]
    fn effective_write_batch_files_caps_to_run_size() {
        let config = IndexingConfig::new(6, 4, 8).unwrap();

        assert_eq!(config.effective_write_batch_files(0), 1);
        assert_eq!(config.effective_write_batch_files(1), 1);
        assert_eq!(config.effective_write_batch_files(3), 3);
        assert_eq!(config.effective_write_batch_files(12), 8);
    }

    #[test]
    fn reporting_parallelism_caps_effective_jobs_and_hides_disabled_embeddings() {
        let config = IndexingConfig::new(6, 4, 1).unwrap();

        let without_embeddings = config.reporting_parallelism(2, false);
        assert_eq!(without_embeddings.jobs_configured, 6);
        assert_eq!(without_embeddings.jobs_effective, 2);
        assert_eq!(without_embeddings.embed_threads, 0);

        let with_embeddings = config.reporting_parallelism(2, true);
        assert_eq!(with_embeddings.jobs_effective, 2);
        assert_eq!(with_embeddings.embed_threads, 4);
    }
}
