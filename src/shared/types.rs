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

/// Index scope coverage information showing what directory cones are indexed and repository coverage.
///
/// Disclosed on every readiness check and search result to ensure agents never assume
/// code doesn't exist just because a search was empty. Explicit coverage disclosure forces
/// the next-action (widen scope) to be visible in the transcript.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexScope {
    /// Repo-relative directory roots currently indexed (e.g., ["services/auth", "libs/core"]).
    pub roots: Vec<String>,
    /// Number of files indexed in the current scope (count of segments/files with content).
    pub indexed_files: usize,
    /// Total files in the repository (denominator for coverage calculation).
    pub total_files: usize,
    /// Plain-text explanation of the unscoped index gap (shown only when roots is empty).
    /// Describes how indexed_files and total_files differ and why (e.g., lockfiles, vendored code, excluded by .gitignore).
    /// Omitted when scope roots are populated (scoped index case).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eligibility_note: Option<String>,
}

impl IndexScope {
    /// Returns a human-readable coverage percentage or description.
    pub fn coverage_description(&self) -> String {
        if self.roots.is_empty() {
            "No scope configured".to_string()
        } else {
            let coverage_pct = if self.total_files > 0 {
                (self.indexed_files as f64 / self.total_files as f64 * 100.0) as u32
            } else {
                0
            };
            format!(
                "{} files indexed of {} total ({}%)",
                self.indexed_files, self.total_files, coverage_pct
            )
        }
    }
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
pub enum StructuralSearchStatus {
    Ok,
    Empty,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StructuralDiagnosticKind {
    UnsupportedLanguage,
    InvalidPattern,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StructuralDiagnostic {
    pub kind: StructuralDiagnosticKind,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StructuralSearchReport {
    pub status: StructuralSearchStatus,
    pub results: Vec<StructuralResult>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<StructuralDiagnostic>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supported_languages: Vec<String>,
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

impl WorktreeRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Linked => "linked",
            Self::Unknown => "unknown",
        }
    }
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

    /// Single-source wording for the "results are not branch-filtered" caveat.
    ///
    /// Emitted whenever the active branch cannot be pinned (`Unknown`/`Unreadable`
    /// on the search path; any non-`Named` status on the readiness path, except an
    /// exact detached commit proven un-drifted — `Detached` with
    /// `drifted == Some(false)` — which reads as pinned and is exempted in
    /// `apply_branch_readiness`). Lives on the type so the search scope
    /// (`src/search/scope.rs`) and the readiness payload (`src/mcp/tools.rs`) share
    /// one phrasing and cannot drift, and is kept short so it reads cleanly when
    /// `combine_degraded_reasons` stacks it after another reason such as
    /// `STALE_REBUILD_REASON`.
    pub fn branch_scope_caveat(self) -> String {
        format!(
            "branch context is {}; results are worktree-scoped, not branch-filtered",
            self.as_str()
        )
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
    /// Per-project include globs; a non-empty set re-admits matching files
    /// without treating the absence of a match as exclusion (avoids the
    /// `ignore::OverrideBuilder` whitelist pitfall). Additive, defaults empty.
    #[serde(default)]
    pub include_globs: Vec<String>,
    /// Per-project exclude globs, additive on top of the default secret-file
    /// exclusions applied by `ScanFilter`. Defaults empty.
    #[serde(default)]
    pub exclude_globs: Vec<String>,
    /// Dotfile-directory paths explicitly re-admitted despite the
    /// default-hidden dotfile policy (e.g. `.github/workflows`). Defaults empty.
    #[serde(default)]
    pub index_hidden_dirs: Vec<String>,
    /// Scope roots that were converted to scope_globs for this config.
    /// Used to record scope information in the progress file during indexing.
    /// When non-empty, indicates this is a scoped index (not a full index).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scope_roots: Vec<String>,
    /// Exclusive scope patterns (e.g., "services/**") populated only when scoped indexing
    /// is active. When non-empty, only files matching scope_globs are included.
    /// This is distinct from include_globs which only guarantees inclusion.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scope_globs: Vec<String>,
}

impl IndexingConfig {
    /// Test-only convenience constructor for the pre-glob 3-arg shape; no
    /// production caller remains after `with_glob_config`/`from_sources_with_globs`
    /// took over the CLI/registry/daemon resolution paths.
    #[cfg(test)]
    pub fn new(
        jobs: usize,
        embed_threads: usize,
        write_batch_files: usize,
    ) -> Result<Self, String> {
        Self::with_glob_config(
            jobs,
            embed_threads,
            write_batch_files,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
    }

    /// Like [`Self::new`], additionally setting the glob/dotfile-override
    /// fields directly (used by config resolution and registry deserialization,
    /// which already have concrete `Vec<String>` values to set rather than
    /// per-source `Option`s).
    pub fn with_glob_config(
        jobs: usize,
        embed_threads: usize,
        write_batch_files: usize,
        include_globs: Vec<String>,
        exclude_globs: Vec<String>,
        index_hidden_dirs: Vec<String>,
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
            include_globs,
            exclude_globs,
            index_hidden_dirs,
            scope_roots: Vec::new(),
            scope_globs: Vec::new(),
        })
    }

    pub fn auto() -> Self {
        Self::from_sources(None, None, None).expect("automatic indexing defaults are valid")
    }

    pub fn from_sources(
        jobs: Option<usize>,
        embed_threads: Option<usize>,
        write_batch_files: Option<usize>,
    ) -> Result<Self, String> {
        Self::from_sources_with_globs(jobs, embed_threads, write_batch_files, None, None, None)
    }

    /// Like [`Self::from_sources`], additionally resolving the glob/dotfile-
    /// override fields from an optional source (e.g. CLI flags or a persisted
    /// registry entry), defaulting to empty when `None` so repos with no globs
    /// configured see no behavior change.
    pub fn from_sources_with_globs(
        jobs: Option<usize>,
        embed_threads: Option<usize>,
        write_batch_files: Option<usize>,
        include_globs: Option<Vec<String>>,
        exclude_globs: Option<Vec<String>>,
        index_hidden_dirs: Option<Vec<String>>,
    ) -> Result<Self, String> {
        let jobs = jobs.unwrap_or_else(Self::default_jobs);
        let embed_threads = embed_threads.unwrap_or_else(|| Self::default_embed_threads_for(jobs));
        let write_batch_files =
            write_batch_files.unwrap_or_else(|| Self::default_write_batch_files_for(jobs));

        Self::with_glob_config(
            jobs,
            embed_threads,
            write_batch_files,
            include_globs.unwrap_or_default(),
            exclude_globs.unwrap_or_default(),
            index_hidden_dirs.unwrap_or_default(),
        )
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
        parse_jobs_for_cores(host_parallelism())
    }

    /// Auto-selects the ONNX intra-op thread count for the embed phase from the
    /// resolved parse `jobs`.
    ///
    /// Embedding is the dominant indexing cost and the only sustained CPU work
    /// during the serial flush, but up to `jobs` parse workers overlap it on
    /// the blocking pool. The budget is bounded by [`MAX_AUTO_EMBED_THREADS`];
    /// paired with [`Self::default_jobs`] — which reserves cores for embedding —
    /// the default split keeps `embed_threads + jobs` within physical cores so
    /// the two pools never over-subscribe (ONNX intra-op throughput regresses
    /// ~3.5x past that). Kept pure in `jobs`, not the live core count, so the
    /// value is deterministic for a given configuration regardless of host.
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

/// Parallelism reported by the runtime, floored at one.
fn host_parallelism() -> usize {
    std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1)
}

/// Pure split of `cores` physical cores into parse workers, reserving room for
/// the embed phase.
///
/// Parse workers (this many) overlap the embed-bearing flush on the blocking
/// pool, and the embed phase uses up to [`MAX_AUTO_EMBED_THREADS`] ONNX
/// intra-op threads, so the two pools share physical cores. Total threads must
/// stay within physical cores — ONNX intra-op throughput regresses ~3.5x once
/// `embed_threads + jobs` over-subscribes. Once `cores` can fund a full embed
/// pool alongside an equal-or-larger parse pool
/// (`cores >= 2 * MAX_AUTO_EMBED_THREADS`), parse takes every core but the embed
/// cap; below that the cores split evenly so `embed_threads == jobs`. At least
/// one parse worker. Paired with [`IndexingConfig::default_embed_threads_for`]
/// this guarantees `embed_threads + jobs <= cores` for every `cores >= 2`.
fn parse_jobs_for_cores(cores: usize) -> usize {
    let parse = if cores >= 2 * MAX_AUTO_EMBED_THREADS {
        cores - MAX_AUTO_EMBED_THREADS
    } else {
        cores / 2
    };
    parse.max(1)
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
            #[serde(default)]
            include_globs: Vec<String>,
            #[serde(default)]
            exclude_globs: Vec<String>,
            #[serde(default)]
            index_hidden_dirs: Vec<String>,
        }

        let raw = RawIndexingConfig::deserialize(deserializer)?;
        IndexingConfig::from_sources_with_globs(
            raw.jobs,
            raw.embed_threads,
            raw.write_batch_files,
            Some(raw.include_globs),
            Some(raw.exclude_globs),
            Some(raw.index_hidden_dirs),
        )
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

/// Scope roots for selective monorepo indexing.
///
/// Stores a list of repo-relative directory roots to be indexed. Enforces
/// validation: paths must be repo-relative (no absolute paths or `../` escapes).
/// Paths are canonicalized (trailing slashes trimmed) for consistent comparison.
///
/// Repo-relative paths, no escapes, validation on construction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeRoots {
    roots: Vec<String>,
}

impl ScopeRoots {
    /// Creates a new ScopeRoots from a list of repo-relative paths.
    ///
    /// Validates that all paths are repo-relative (no absolute paths or `../` escapes),
    /// then canonicalizes each path (trims trailing slashes).
    ///
    /// Returns `Err` if any path fails validation.
    pub fn new(paths: Vec<String>) -> Result<Self, String> {
        let roots: Result<Vec<_>, String> = paths
            .into_iter()
            .map(|path| Self::validate_and_canonicalize(&path))
            .collect();

        Ok(Self { roots: roots? })
    }

    /// Returns `true` if this scope has any roots, `false` if empty.
    ///
    /// Used to determine if a scope has been set.
    #[cfg(test)]
    pub fn is_scoped(&self) -> bool {
        !self.roots.is_empty()
    }

    /// Returns the list of scope roots.
    pub fn roots(&self) -> &[String] {
        &self.roots
    }

    /// Validates and canonicalizes a single path.
    ///
    /// - Rejects absolute paths (starting with `/`)
    /// - Rejects paths containing `../` (directory escape)
    /// - Trims trailing slashes for consistent comparison
    /// - Ensures path is not empty after trimming
    fn validate_and_canonicalize(path: &str) -> Result<String, String> {
        let trimmed = path.trim_end_matches('/');

        // Reject empty paths
        if trimmed.is_empty() {
            return Err("scope path cannot be empty".to_string());
        }

        // Reject absolute paths
        if trimmed.starts_with('/') {
            return Err(format!(
                "scope path must be repo-relative, not absolute: {}",
                trimmed
            ));
        }

        // Reject directory escapes
        if trimmed.contains("../") || trimmed.contains("/..") || trimmed == ".." {
            return Err(format!(
                "scope path cannot contain directory escapes: {}",
                trimmed
            ));
        }

        // Return canonicalized path (trailing slashes removed)
        Ok(trimmed.to_string())
    }
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
    /// Actual scope roots (e.g., ["services/auth", "libs/core"]).
    /// Present when scope was applied (requested/executed start with "scoped:").
    /// Empty for full scans.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roots: Vec<String>,
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
    /// Terminal state of an indexing run that failed after publishing
    /// `Running` progress. Distinct from `Idle` so readiness classification
    /// can keep reporting the failure (blocked/degraded with the recorded
    /// reason) instead of silently reverting to ready/missing; superseded by
    /// the next run's progress writes.
    Failed,
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
    pub embedding_unavailable_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vector_rows: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embeddable_segments: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallelism: Option<IndexParallelism>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timings: Option<IndexStageTimings>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "scope_recorded",
        alias = "scope"
    )]
    pub scope: Option<IndexScopeInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefilter: Option<IndexPrefilterInfo>,
    /// PID of the indexing process, used for liveness checks
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indexer_pid: Option<u32>,
    /// Identity of the `oneup_start` run that published this record (stamped
    /// on the pre-spawn scope snapshot and on terminal failure records). A
    /// PID alone cannot distinguish overlapping starts within the same
    /// long-lived MCP process, so failure cleanup keys on this to avoid
    /// overwriting a newer run's record. Pipeline progress writes leave it
    /// unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
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
            embedding_unavailable_reason: None,
            vector_rows: None,
            embeddable_segments: None,
            message: None,
            parallelism: None,
            timings: None,
            scope: None,
            prefilter: None,
            indexer_pid: None,
            run_id: None,
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

impl DaemonRefreshState {
    /// Whether this state means an index refresh/rebuild is currently in
    /// flight, so a served search should be flagged stale-but-available.
    ///
    /// Single source of truth for the `Pending`/`Running` == in-flight mapping.
    /// Lives on the type (in `shared`) rather than in any one consumer so the
    /// readiness classifier and the MCP detector (`src/mcp/ops.rs`) and the
    /// daemon's own search path (`src/daemon/worker.rs`, which cannot depend on
    /// the MCP layer) all share one predicate and cannot drift.
    pub fn is_in_flight(self) -> bool {
        matches!(
            self,
            DaemonRefreshState::Pending | DaemonRefreshState::Running
        )
    }
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

/// Merges two optional `degraded_reason` fragments into one, joining both with
/// `"; "` when present so neither is silently dropped. Lives in `shared` so
/// both served-search surfaces — MCP (`src/mcp/ops.rs`) and the daemon
/// (`src/daemon/worker.rs`, which cannot depend on the MCP layer) — fold the
/// stale-rebuild reason in through one combiner rather than duplicating the
/// join.
pub fn combine_degraded_reasons(left: Option<String>, right: Option<String>) -> Option<String> {
    match (left, right) {
        (Some(left), Some(right)) => Some(format!("{left}; {right}")),
        (Some(reason), None) | (None, Some(reason)) => Some(reason),
        (None, None) => None,
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
    fn embed_threads_plus_parse_jobs_never_oversubscribe_cores() {
        // Pure gate: the default parse/embed split must keep
        // embed_threads + jobs within physical cores at every realistic core
        // count, because the parse pool overlaps the embed-bearing flush and
        // ONNX intra-op throughput regresses ~3.5x once the two over-subscribe.
        // cores=1 is the only degenerate point: parsing and embedding each need
        // at least one thread, so the floor budget is 2.
        for cores in 1..=128usize {
            let jobs = parse_jobs_for_cores(cores);
            let embed = IndexingConfig::default_embed_threads_for(jobs);
            assert!(jobs >= 1, "cores={cores}: at least one parse worker");
            assert!(embed >= 1, "cores={cores}: at least one embed thread");
            let budget = cores.max(2);
            assert!(
                embed + jobs <= budget,
                "cores={cores}: embed({embed}) + jobs({jobs}) = {} over-subscribes budget {budget}",
                embed + jobs
            );
        }
    }

    #[test]
    fn embed_threads_scale_past_legacy_cap_on_higher_core_hosts() {
        // The default embed-thread bound raises the cap past the legacy fixed
        // value of 4. On hosts
        // with enough cores to fund it within the gate (>= 10, where the even
        // split leaves an embed pool > 4), the embed phase now uses more
        // intra-op threads than the old cap while still honoring the budget. If
        // the cap were not raised these per-core assertions would fail.
        const LEGACY_EMBED_CAP: usize = 4;
        for cores in 10..=64usize {
            let jobs = parse_jobs_for_cores(cores);
            let embed = IndexingConfig::default_embed_threads_for(jobs);
            assert!(
                embed > LEGACY_EMBED_CAP,
                "cores={cores}: embed({embed}) should exceed the legacy cap {LEGACY_EMBED_CAP}"
            );
            assert!(
                embed + jobs <= cores,
                "cores={cores}: embed({embed}) + jobs({jobs}) must stay within cores"
            );
        }
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
    fn indexing_config_deserializes_pre_change_json_with_empty_glob_defaults() {
        let legacy_json = r#"{"jobs": 4, "embed_threads": 2, "write_batch_files": 1}"#;

        let config: IndexingConfig = serde_json::from_str(legacy_json).unwrap();

        assert_eq!(config.jobs, 4);
        assert_eq!(config.embed_threads, 2);
        assert_eq!(config.write_batch_files, 1);
        assert!(config.include_globs.is_empty());
        assert!(config.exclude_globs.is_empty());
        assert!(config.index_hidden_dirs.is_empty());
    }

    #[test]
    fn indexing_config_deserializes_globs_when_present() {
        let json = r#"{
            "jobs": 4,
            "embed_threads": 2,
            "write_batch_files": 1,
            "include_globs": ["src/**"],
            "exclude_globs": ["vendor/**"],
            "index_hidden_dirs": [".github/workflows"]
        }"#;

        let config: IndexingConfig = serde_json::from_str(json).unwrap();

        assert_eq!(config.include_globs, vec!["src/**".to_string()]);
        assert_eq!(config.exclude_globs, vec!["vendor/**".to_string()]);
        assert_eq!(
            config.index_hidden_dirs,
            vec![".github/workflows".to_string()]
        );
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

    #[test]
    fn refresh_state_in_flight_only_for_pending_or_running() {
        assert!(DaemonRefreshState::Pending.is_in_flight());
        assert!(DaemonRefreshState::Running.is_in_flight());
        assert!(!DaemonRefreshState::Complete.is_in_flight());
        assert!(!DaemonRefreshState::Failed.is_in_flight());
        assert!(!DaemonRefreshState::Unknown.is_in_flight());
    }

    #[test]
    fn combine_degraded_reasons_joins_both_without_dropping_either() {
        // Both present: joined by "; ", neither silently dropped.
        assert_eq!(
            combine_degraded_reasons(Some("stale".to_string()), Some("no embeddings".to_string())),
            Some("stale; no embeddings".to_string())
        );
        // Exactly one present passes through unchanged.
        assert_eq!(
            combine_degraded_reasons(Some("stale".to_string()), None),
            Some("stale".to_string())
        );
        assert_eq!(
            combine_degraded_reasons(None, Some("no embeddings".to_string())),
            Some("no embeddings".to_string())
        );
        // No stale reason (rebuild idle) leaves no stale fragment.
        assert_eq!(combine_degraded_reasons(None, None), None);
    }

    // ScopeRoots validation and helper tests

    #[test]
    fn scope_roots_accepts_valid_repo_relative_paths() {
        let roots = ScopeRoots::new(vec!["services/auth".to_string(), "libs/core".to_string()])
            .expect("valid repo-relative paths");

        assert!(roots.is_scoped());
        assert_eq!(roots.roots(), &["services/auth", "libs/core"]);
    }

    #[test]
    fn scope_roots_accepts_single_level_paths() {
        let roots =
            ScopeRoots::new(vec!["src".to_string()]).expect("single-level repo-relative path");

        assert!(roots.is_scoped());
        assert_eq!(roots.roots(), &["src"]);
    }

    #[test]
    fn scope_roots_canonicalizes_trailing_slashes() {
        let roots = ScopeRoots::new(vec!["services/auth/".to_string(), "libs/core/".to_string()])
            .expect("paths with trailing slashes");

        // Trailing slashes trimmed
        assert_eq!(roots.roots(), &["services/auth", "libs/core"]);
    }

    #[test]
    fn scope_roots_rejects_absolute_paths() {
        let result = ScopeRoots::new(vec!["/repo/services".to_string()]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("absolute"));
    }

    #[test]
    fn scope_roots_rejects_parent_directory_escapes() {
        let result = ScopeRoots::new(vec!["../other".to_string()]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("escape"));

        let result = ScopeRoots::new(vec!["services/../etc".to_string()]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("escape"));

        let result = ScopeRoots::new(vec!["services/auth/..".to_string()]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("escape"));
    }

    #[test]
    fn scope_roots_rejects_empty_paths() {
        let result = ScopeRoots::new(vec!["".to_string()]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("empty"));

        let result = ScopeRoots::new(vec!["/".to_string()]);
        assert!(result.is_err());
    }

    #[test]
    fn scope_roots_empty_scope_not_scoped() {
        let roots = ScopeRoots::new(vec![]).expect("empty list is valid");
        assert!(!roots.is_scoped());
        assert_eq!(roots.roots(), &[] as &[String]);
    }

    #[test]
    fn scope_roots_serialization_deserialize_roundtrip() {
        let original = ScopeRoots::new(vec!["services/auth".to_string(), "libs/core".to_string()])
            .expect("valid paths");

        let json = serde_json::to_string(&original).expect("serialize");
        let deserialized: ScopeRoots = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(original, deserialized);
        assert_eq!(deserialized.roots(), &["services/auth", "libs/core"]);
    }

    #[test]
    fn scope_roots_serialization_empty_scope() {
        let empty = ScopeRoots::new(vec![]).expect("empty list");

        let json = serde_json::to_string(&empty).expect("serialize");
        let deserialized: ScopeRoots = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(empty, deserialized);
        assert!(!deserialized.is_scoped());
    }
}
