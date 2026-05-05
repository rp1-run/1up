use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::Instant;

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
    pub referenced_relations: Vec<ParsedRelation>,
    pub called_symbols: Vec<String>,
    pub called_relations: Vec<ParsedRelation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ParsedRelationKind {
    Call,
    Reference,
    Conformance,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedRelation {
    pub symbol: String,
    pub edge_identity_kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ParsedRelationKind>,
}

/// A search result returned by hybrid or FTS-only search.
///
/// The struct carries only the discovery-side fields that the lean row
/// grammar renders (score, path, line span, kind, breadcrumb, defined
/// symbols, segment handle). `content` is retained in memory so that the
/// `get` command can reuse the hydrated body without a second query, but
/// the lean renderer never emits it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub segment_id: String,
    pub file_path: String,
    pub language: String,
    pub block_type: String,
    pub content: String,
    pub score: u32,
    pub line_number: usize,
    pub line_end: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub breadcrumb: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub defined_symbols: Option<Vec<String>>,
}

/// Normalize a raw RRF score in `[0, ~1]` to an integer in `[0, 100]`.
///
/// The mapping is monotonic, so ordering is preserved. Ties within one
/// integer point are acceptable (already within ranking noise on the
/// corpora we evaluate against).
pub fn normalize_score(rrf: f64) -> u32 {
    (rrf * 100.0).round().clamp(0.0, 100.0) as u32
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
///
/// Like `SearchResult`, this is the lean discovery-side shape: the fat
/// hydrated fields (complexity, role, defined/referenced/called symbols)
/// live on the stored segment and are served by `get`, not discovery.
/// `content` stays on the struct so that in-process callers that already
/// hydrate a full segment can still reuse the body without re-querying.
#[derive(Debug, Clone, Serialize)]
pub struct SymbolResult {
    pub segment_id: String,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorktreeRole {
    Main,
    Linked,
    Unknown,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BranchStatus {
    Named,
    Detached,
    Unreadable,
    #[default]
    Unknown,
}

impl BranchStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Named => "named",
            Self::Detached => "detached",
            Self::Unreadable => "unreadable",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeContext {
    pub context_id: String,
    pub state_root: PathBuf,
    pub source_root: PathBuf,
    pub main_worktree_root: PathBuf,
    pub worktree_role: WorktreeRole,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_dir: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub common_git_dir: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_oid: Option<String>,
    pub branch_status: BranchStatus,
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

/// Pre-pipeline setup timing collected by CLI and daemon callers.
///
/// Captures the wall-clock start and per-stage setup durations that occur
/// before the pipeline runs, so `total_ms` reflects what the user waited for.
#[derive(Debug, Clone)]
pub struct SetupTimings {
    pub run_started_at: Instant,
    pub db_prepare_ms: u128,
    pub model_prepare_ms: u128,
}

impl SetupTimings {
    pub fn new(run_started_at: Instant) -> Self {
        Self {
            run_started_at,
            db_prepare_ms: 0,
            model_prepare_ms: 0,
        }
    }
}

/// Stage-level timing data for an indexing run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexStageTimings {
    pub scan_ms: u128,
    pub parse_ms: u128,
    pub embed_ms: u128,
    pub store_ms: u128,
    pub total_ms: u128,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub db_prepare_ms: Option<u128>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_prepare_ms: Option<u128>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_prep_ms: Option<u128>,
}

/// Scope metadata for an indexing run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexScopeInfo {
    pub requested: String,
    pub executed: String,
    #[serde(default)]
    pub changed_paths: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<String>,
}

/// Prefilter counters for an indexing run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexPrefilterInfo {
    pub discovered: usize,
    pub metadata_skipped: usize,
    pub content_read: usize,
    pub deleted: usize,
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
    Preparing,
    Rebuilding,
    LoadingModel,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_root: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_status: Option<BranchStatus>,
    pub files_total: usize,
    pub files_scanned: usize,
    #[serde(default)]
    pub files_processed: usize,
    pub files_indexed: usize,
    pub files_skipped: usize,
    pub files_deleted: usize,
    pub segments_stored: usize,
    pub embeddings_enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallelism: Option<IndexParallelism>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timings: Option<IndexStageTimings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<IndexScopeInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefilter: Option<IndexPrefilterInfo>,
    pub updated_at: DateTime<Utc>,
}

impl IndexProgress {
    pub fn pending() -> Self {
        Self {
            state: IndexState::Idle,
            phase: IndexPhase::Pending,
            context_id: None,
            source_root: None,
            branch_name: None,
            branch_status: None,
            files_total: 0,
            files_scanned: 0,
            files_processed: 0,
            files_indexed: 0,
            files_skipped: 0,
            files_deleted: 0,
            segments_stored: 0,
            embeddings_enabled: false,
            message: None,
            parallelism: None,
            timings: None,
            scope: None,
            prefilter: None,
            updated_at: Utc::now(),
        }
    }

    pub fn watch(state: IndexState, phase: IndexPhase, message: impl Into<String>) -> Self {
        Self {
            state,
            phase,
            message: Some(message.into()),
            ..Self::pending()
        }
    }
}

/// Latest persisted daemon heartbeat for file checks on a project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonProjectStatus {
    pub last_file_check_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonWatchStatus {
    Watching,
    DaemonStopped,
    SourceMissing,
    Unsupported,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonRefreshState {
    Pending,
    Running,
    Complete,
    Failed,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonContextStatus {
    pub context_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_root: Option<PathBuf>,
    #[serde(default)]
    pub watch_status: DaemonWatchStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_file_check_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_refresh_state: DaemonRefreshState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_refresh_started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_refresh_completed_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_refresh_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_name: Option<String>,
    #[serde(default)]
    pub branch_status: BranchStatus,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonContextStatusFile {
    #[serde(default)]
    pub contexts: BTreeMap<String, DaemonContextStatus>,
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
    fn score_normalization_monotonic() {
        let samples = [0.0_f64, 0.01, 0.1, 0.25, 0.5, 0.75, 0.9, 0.99, 1.0];
        for window in samples.windows(2) {
            let lo = normalize_score(window[0]);
            let hi = normalize_score(window[1]);
            assert!(
                hi >= lo,
                "normalize_score must be monotonic: {} -> {}, {} -> {}",
                window[0],
                lo,
                window[1],
                hi
            );
        }
    }

    #[test]
    fn score_normalization_clamps_to_0_100() {
        assert_eq!(normalize_score(-1.0), 0);
        assert_eq!(normalize_score(0.0), 0);
        assert_eq!(normalize_score(1.0), 100);
        assert_eq!(normalize_score(2.0), 100);
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
