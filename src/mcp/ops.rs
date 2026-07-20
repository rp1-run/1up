use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{bail, Context};
use libsql::Connection;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::daemon::lifecycle;
use crate::daemon::registry::Registry;
use crate::indexer::embedder::{
    self, clear_download_failure, EmbeddingLoadStatus, EmbeddingRuntime, EmbeddingUnavailableReason,
};
use crate::indexer::parser::SupportedLanguage;
use crate::indexer::pipeline;
use crate::indexer::scan_filter::ScanFilter;
use crate::mcp::types::{DirectoryStats, FactsEnvelope, StartMode, TOOL_CONTEXT, TOOL_SYMBOL};
use crate::search::context::{ContextEngine, ScopeWindow};
use crate::search::impact::{ImpactHorizonEngine, ImpactRequest, ImpactResultEnvelope};
use crate::search::overview;
use crate::search::retrieval;
use crate::search::{HybridSearchEngine, SearchScope, StructuralSearchEngine, SymbolSearchEngine};
use crate::shared::config::{self, project_db_path, project_dot_dir};
use crate::shared::constants::{
    DB_LOCK_RETRY_ATTEMPTS, DB_LOCK_RETRY_DELAY_MS, FILE_COUNT_THRESHOLD,
    FILE_COUNT_THRESHOLD_ENV_VAR, MAX_CONTEXT_EXPANSION_LINES, MAX_SYMBOLS_PER_LIST,
    NO_INDEXED_EMBEDDINGS_REASON, PROJECT_STATE_DIR_MODE, SCOPE_TRUNCATION_REASON,
    SECURE_STATE_FILE_MODE, STALE_REBUILD_REASON, STATUS_READ_RETRY_ATTEMPTS,
    STATUS_READ_RETRY_DELAY_MS, SYMBOL_LIST_TRUNCATION_REASON,
};
use crate::shared::errors::{OneupError, ProjectError};
use crate::shared::fs::{atomic_replace, ensure_secure_project_root};
use crate::shared::progress::{read_status_file, StatusFileRead};
use crate::shared::project;
use crate::shared::types::{
    combine_degraded_reasons, DaemonProjectStatus, IndexProgress, IndexScope, IndexState,
    IndexingConfig, ReferenceKind, RunScope, SearchResult, SegmentRole, SetupTimings,
    StructuralSearchReport, SymbolResult, WorktreeContext,
};
use crate::storage::db::{is_lock_error, Db};
use crate::storage::schema;
use crate::storage::segments::{
    count_embeddable_segments_for_context, count_files_for_context, count_segments_for_context,
    count_vector_rows_for_context, get_all_file_paths_for_context, get_segment_by_id_for_context,
    get_segment_by_prefix_for_context, get_segment_ids_by_prefix_for_context,
    get_segments_by_ids_for_context, get_worktree_context_head_oid, SegmentPrefixLookup,
    StoredSegment,
};
use crate::storage::swap;

const INDEX_PROGRESS_FILE_NAME: &str = "index_status.json";

/// Floor length (in characters) for unique-prefix handle recovery. A
/// supplied handle must share at least this many leading characters with a
/// single indexed segment id before recovery may resolve it, giving 32 bits of
/// hex discrimination and forbidding short-prefix guesses.
const MIN_HANDLE_RECOVERY_PREFIX_CHARS: usize = 8;

/// Upper bound on candidate ids fetched at the floor prefix during handle
/// recovery. A fetch that saturates this limit means the floor prefix is too
/// broad to discriminate, so recovery declines rather than guess.
const HANDLE_RECOVERY_CANDIDATE_LIMIT: usize = 32;

/// Upper bound on the bounded process-global failed-handle retry memory.
/// Once exceeded, the oldest-recorded entry is evicted first so the memory can
/// never grow without bound across a long-lived MCP session.
const FAILED_HANDLE_MEMORY_CAP: usize = 128;

#[derive(Debug, Clone)]
pub struct McpProjectRoots {
    pub state_root: PathBuf,
    pub source_root: PathBuf,
    pub worktree_context: WorktreeContext,
    /// The initial launch directory (CWD before project root resolution).
    /// Used to suggest default scope in facts envelope for monorepos.
    /// None if launched from project root or outside any recognized project.
    pub launch_subdir: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationStatus {
    Ok,
    Empty,
    Partial,
    Degraded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadinessStatus {
    Ready,
    Missing,
    Indexing,
    Stale,
    Degraded,
    Blocked,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReadinessPayload {
    pub status: ReadinessStatus,
    pub summary: String,
    pub state_root: String,
    pub source_root: String,
    pub project_initialized: bool,
    pub index_present: bool,
    pub index_readable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indexed_files: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_segments: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vector_rows: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embeddable_segments: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indexed_at_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drifted: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_progress: Option<IndexProgress>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daemon_status: Option<DaemonProjectStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_scope: Option<IndexScope>,
    /// Ranked scope proposal surfaced on a Missing readiness when the daemon
    /// gate fired on an over-threshold unscoped repo. Present only when a fresh
    /// proposal was persisted; the MCP layer turns it into `scope_add`
    /// next_actions so the refusal is actionable rather than generic.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope_proposal: Option<ScopeProposalSummary>,
}

/// Ranked scope suggestions attached to a Missing readiness payload, rebuilt
/// from the daemon-persisted proposal. Mirrors the synchronous facts-envelope
/// suggestions so `oneup_status` and a follow-up unscoped `oneup_start` surface
/// the same actionable cones.
#[derive(Debug, Clone, Serialize)]
pub struct ScopeProposalSummary {
    /// Total gitignore-aware tracked file count that tripped the monorepo gate.
    pub file_count_total: usize,
    /// Human-readable ranked suggestions (e.g. "Index the largest directory: services").
    pub suggestions: Vec<String>,
    /// Ranked top-level directory names (largest first) usable as `scope_add` values.
    pub scope_candidates: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchPayload {
    pub status: OperationStatus,
    pub results: Vec<SearchHit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub degraded_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_scope: Option<IndexScope>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchHit {
    pub handle: String,
    pub path: String,
    pub language: String,
    pub kind: String,
    pub score: u32,
    pub line_start: usize,
    pub line_end: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub breadcrumb: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub defined_symbols: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolInclude {
    Definitions,
    References,
    Both,
}

#[derive(Debug, Clone)]
pub struct SymbolLookupRequest {
    pub name: String,
    pub include: SymbolInclude,
    pub fuzzy: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SymbolPayload {
    pub status: OperationStatus,
    pub definitions: Vec<SymbolRecord>,
    pub references: Vec<SymbolRecord>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SymbolRecord {
    pub handle: String,
    pub name: String,
    pub reference_kind: ReferenceKind,
    pub kind: String,
    pub path: String,
    pub language: String,
    pub line_start: usize,
    pub line_end: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub breadcrumb: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ReadLocation {
    pub path: String,
    pub line: usize,
    pub expansion: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadStatus {
    Found,
    NotFound,
    Ambiguous,
    Rejected,
    Error,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReadPayload {
    pub status: OperationStatus,
    pub records: Vec<ReadRecord>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReadRecord {
    pub status: ReadStatus,
    pub source: ReadSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub segment: Option<SegmentRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<ContextRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub matching_handles: Vec<String>,
    /// Set only on a record recovered via the unique-prefix gate: the
    /// original supplied handle that did not resolve exactly or by prefix but
    /// whose unique canonical prefix mapped to this segment. Additive and
    /// omitted on every non-recovered record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovered_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReadSource {
    Handle { raw: String, normalized: String },
    Location { path: String, line: usize },
}

/// Structured, ready-to-issue recovery call carried by a [`TruncationNote`].
///
/// `tool` MUST be a member of `RETAINED_PUBLIC_TOOLS` (enforced by the
/// `action()` debug-assert when the call is copied into a next_action).
/// `arguments` share [`serde_json::Value`] with `NextAction::arguments`
/// (`{path, line, expansion}` for scope clips, `{name}` for symbol clips) so
/// the note's recovery call can be prepended into `next_actions` verbatim and
/// re-issued by an agent without reshaping.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RecoveryCall {
    pub tool: String,
    pub arguments: Value,
}

/// Load-bearing truncation metadata attached to a record whenever content was
/// bounded. Never best-effort: its presence means an omission
/// occurred and `recovery` states exactly how to fetch the omitted content.
///
/// Scope clips populate the scope fields (`scope_name`, `scope_type`,
/// `full_line_start`/`full_line_end`, `omitted_above`/`omitted_below`);
/// symbol-list clips populate `omitted_symbols`; `recovery` is always present.
/// Absent (`None`) on a record means nothing was bounded.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TruncationNote {
    /// Single-source reason constant (`SCOPE_TRUNCATION_REASON` /
    /// `SYMBOL_LIST_TRUNCATION_REASON`), also rendered in the summary marker.
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_line_start: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_line_end: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub omitted_above: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub omitted_below: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub omitted_symbols: Option<usize>,
    pub recovery: RecoveryCall,
}

/// Constant-size symbol counts emitted at default verbosity when the symbol
/// lists are omitted but non-empty, making the omission explicit without
/// re-inflating the payload. Present only when a list was omitted
/// non-empty; the `oneup_symbol` recovery path retrieves the full lists.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct SymbolCounts {
    pub defined: usize,
    pub referenced: usize,
    pub called: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct SegmentRecord {
    pub handle: String,
    pub path: String,
    pub language: String,
    pub kind: String,
    pub content: String,
    pub line_start: usize,
    pub line_end: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub breadcrumb: Option<String>,
    pub role: SegmentRole,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub defined_symbols: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub referenced_symbols: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub called_symbols: Vec<String>,
    /// Constant-size symbol counts, emitted at default verbosity when the
    /// symbol lists are omitted but non-empty. `None` when the lists
    /// are present (full verbosity) or all counts are zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol_counts: Option<SymbolCounts>,
    /// Load-bearing truncation note set when a symbol list was capped at
    /// [`MAX_SYMBOLS_PER_LIST`]. `None` when nothing was bounded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncation: Option<TruncationNote>,
    /// First symbol defined by the underlying segment, captured before the
    /// verbosity gating applied to `defined_symbols`. Never serialized into
    /// the hydration payload; envelope next_actions read it so defining
    /// segments keep offering oneup_symbol verification at any verbosity.
    #[serde(skip)]
    pub symbol_hint: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContextRecord {
    pub path: String,
    pub language: String,
    pub scope_type: String,
    pub content: String,
    pub line_start: usize,
    pub line_end: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub out_of_scope_disclosure: Option<String>,
    /// Load-bearing truncation note set when a large enclosing scope was
    /// windowed: scope name/type, full scope range, omitted line
    /// counts, and a ready-to-issue `oneup_context` recovery call. `None` when
    /// the whole scope was returned (nothing bounded).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncation: Option<TruncationNote>,
}

/// Orientation digest payload for `oneup_overview`. Section sizes are bounded
/// by the engine caps documented in `crate::search::overview`, which keep the
/// serialized payload within the documented budget.
#[derive(Debug, Clone, Serialize)]
pub struct OverviewPayload {
    pub status: OperationStatus,
    pub stats: OverviewStats,
    pub top_symbols: Vec<OverviewTopSymbol>,
    pub modules: Vec<OverviewModule>,
    pub module_dependencies: Vec<OverviewModuleDependency>,
    pub entry_points: Vec<OverviewEntryPoint>,
}

#[derive(Debug, Clone, Serialize)]
pub struct OverviewStats {
    pub indexed_files: u64,
    pub total_segments: u64,
    pub languages: Vec<OverviewLanguage>,
}

#[derive(Debug, Clone, Serialize)]
pub struct OverviewLanguage {
    pub language: String,
    pub files: u64,
    pub segments: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct OverviewTopSymbol {
    pub name: String,
    pub handle: String,
    pub path: String,
    pub line_start: usize,
    pub line_end: usize,
    pub referencing_files: u64,
    pub definition_count: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct OverviewModule {
    pub module: String,
    pub segments: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct OverviewModuleDependency {
    pub source: String,
    pub target: String,
    pub count: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct OverviewEntryPoint {
    pub handle: String,
    pub path: String,
    pub line_start: usize,
    pub line_end: usize,
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub breadcrumb: Option<String>,
}

struct CurrentIndex {
    conn: Connection,
    /// Canonical `index.db` path -- the warm cache's key -- so a
    /// caller needing the per-context vector-count cache (`run_search_once`)
    /// can look it up without re-resolving/canonicalizing the path itself.
    db_path: PathBuf,
}

pub fn resolve_project(path: &Path) -> anyhow::Result<McpProjectRoots> {
    // Canonicalize the input path first to get the actual launch directory
    let canonical_launch_dir = match path.canonicalize() {
        Ok(p) => p,
        Err(_) => path.to_path_buf(),
    };

    let resolved = project::resolve_project_root(path)?;

    // Capture launch_subdir if the launch directory differs from the clamped source_root.
    // This indicates the user launched from a subdirectory that got clamped to the project root.
    let launch_subdir = if canonical_launch_dir != resolved.source_root
        && canonical_launch_dir.starts_with(&resolved.source_root)
    {
        Some(canonical_launch_dir)
    } else {
        None
    };

    Ok(McpProjectRoots {
        state_root: resolved.state_root,
        source_root: resolved.source_root,
        worktree_context: resolved.worktree_context,
        launch_subdir,
    })
}

/// Loads the current scope from the index if it exists.
/// Returns an empty vec if no scope is recorded or if the index doesn't exist.
async fn load_current_scope(state_root: &Path) -> anyhow::Result<Vec<String>> {
    let db_path = project_db_path(state_root);

    if !db_path.exists() {
        return Ok(vec![]);
    }

    match Db::open_ro(&db_path).await {
        Ok(db) => match db.connect_tuned().await {
            Ok(conn) => Ok(schema::read_scope_from_meta(&conn)
                .await
                .unwrap_or_default()
                .unwrap_or_default()),
            Err(_) => Ok(vec![]),
        },
        Err(_) => Ok(vec![]),
    }
}

/// Determines the correct rebuild mode for the given scope operation.
///
/// - scope_narrow always requires rebuild (atomic rebuild)
/// - scope_add on an unscoped index (first scoped) requires rebuild to avoid stale metadata
/// - scope_add on an already-scoped index (widening) can be incremental
async fn determine_rebuild_mode_for_scope(
    state_root: &Path,
    scope_add: Option<&Vec<String>>,
    scope_narrow: Option<&Vec<String>>,
) -> anyhow::Result<bool> {
    if scope_narrow.is_some() {
        // scope_narrow always requires atomic rebuild
        return Ok(true);
    }

    if scope_add.is_some() {
        // scope_add: check if we're converting from unscoped to scoped
        let current_scope = load_current_scope(state_root).await?;
        if current_scope.is_empty() {
            // First scoped index after unscoped - need rebuild to avoid stale metadata
            return Ok(true);
        } else {
            // Widening existing scope - can be incremental
            return Ok(false);
        }
    }

    // Default: no rebuild needed for other operations
    Ok(false)
}

/// Computes the new scope by loading existing scope and applying scope_add/scope_narrow.
///
/// Returns the resulting scope roots as a Vec<String>.
/// Validates scope_narrow is a subset of existing scope.
async fn compute_new_scope(
    state_root: &Path,
    scope_add: Option<Vec<String>>,
    scope_narrow: Option<Vec<String>>,
) -> anyhow::Result<Vec<String>> {
    let db_path = project_db_path(state_root);

    // Load existing scope if index exists
    let mut current_scope = if db_path.exists() {
        match Db::open_ro(&db_path).await {
            Ok(db) => match db.connect_tuned().await {
                Ok(conn) => schema::read_scope_from_meta(&conn)
                    .await
                    .unwrap_or_default()
                    .unwrap_or_default(),
                Err(_) => vec![],
            },
            Err(_) => vec![],
        }
    } else {
        vec![]
    };

    tracing::debug!(
        "compute_new_scope: current_scope={:?}, scope_add={:?}, scope_narrow={:?}",
        current_scope,
        scope_add,
        scope_narrow
    );

    // Apply scope_narrow if provided (must be subset of current)
    if let Some(narrow_to) = scope_narrow {
        // Validate that narrow_to is a subset of current_scope
        if !current_scope.is_empty() {
            for path in &narrow_to {
                if !current_scope.contains(path) {
                    bail!(
                        "scope_narrow path '{}' is not in current scope: {:?}",
                        path,
                        current_scope
                    );
                }
            }
        }
        current_scope = narrow_to;
    }

    // Apply scope_add if provided (union operation)
    if let Some(add_to) = scope_add {
        for path in add_to {
            // Validate path: no absolute paths, no ../
            if path.starts_with('/') {
                bail!("scope path cannot be absolute: {}", path);
            }
            if path.contains("..") {
                bail!("scope path cannot contain '..': {}", path);
            }
            if !current_scope.contains(&path) {
                current_scope.push(path);
            }
        }
    }

    tracing::debug!(
        "compute_new_scope: returning final_scope={:?}",
        current_scope
    );
    Ok(current_scope)
}

/// Applies scope roots to IndexingConfig by converting them to include_globs.
///
/// Scope roots are converted to glob patterns: "dir1/**", "dir2/**", etc.
/// Also stores the actual scope roots so they can be recorded in progress.
fn apply_scope_to_indexing_config(
    config: &mut IndexingConfig,
    scope_roots: &[String],
) -> anyhow::Result<()> {
    // Always store scope roots in config, even if empty, so they're available
    // during progress recording. Empty scope_roots indicates unscoped (full) index.
    config.scope_roots = scope_roots.to_vec();

    if !scope_roots.is_empty() {
        // Convert scope roots to scope_globs: "dir/**" for each root.
        // These are exclusive patterns used for scoped indexing, distinct from include_globs
        // which only guarantee inclusion and never exclude non-matching files.
        let scope_globs: Vec<String> = scope_roots
            .iter()
            .map(|root| format!("{}/**", root))
            .collect();
        tracing::debug!(
            "apply_scope_to_indexing_config: scope_roots={:?}, scope_globs={:?}",
            scope_roots,
            scope_globs
        );
        config.scope_globs = scope_globs;
    } else {
        tracing::debug!("apply_scope_to_indexing_config: no scope, using empty scope_globs");
    }
    Ok(())
}

pub async fn check_status(roots: &McpProjectRoots) -> ReadinessPayload {
    classify_readiness(
        &roots.state_root,
        &roots.source_root,
        &roots.worktree_context,
    )
    .await
}

/// Allocates a process-unique identity for one `oneup_start` rebuild run,
/// stamped into the records that run publishes (see `IndexProgress::run_id`).
fn next_rebuild_run_id() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!(
        "{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

/// Roots for which the next rebuild should panic on entry (test-only panic
/// injection). Keyed by state root so concurrently running tests can never
/// trip each other's injection.
#[cfg(test)]
static REBUILD_PANIC_ROOTS: OnceLock<Mutex<std::collections::HashSet<PathBuf>>> = OnceLock::new();

/// Arms a one-shot panic at the start of the next rebuild for `state_root`,
/// to exercise the spawn wrapper's panic-recovery arm deterministically.
#[cfg(test)]
fn arm_rebuild_panic_for_test(state_root: &Path) {
    REBUILD_PANIC_ROOTS
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .insert(state_root.to_path_buf());
}

#[cfg(test)]
fn maybe_panic_for_test(state_root: &Path) {
    let armed = REBUILD_PANIC_ROOTS
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .remove(state_root);
    if armed {
        panic!("test-injected rebuild panic");
    }
}

/// Serializes this process's `index_status.json` publication and
/// failure-cleanup writes.
///
/// `record_rebuild_failure_progress` is a read-check-write: it reads the
/// current record, decides ownership, then persists the terminal snapshot.
/// Pre-spawn scope publication (`write_initial_scope_progress`) deliberately
/// runs WITHOUT the rebuild lock — `ops::start` must not block behind a
/// long-running rebuild — so without mutual exclusion a newer start's
/// publication can land between an older failure's ownership check and its
/// write, and the stale `Failed` snapshot would silently overwrite the newer
/// run's `Running` record. Holding this lock across both operations makes the
/// ownership check atomic with respect to publications. Pipeline progress
/// writes need no seat here: a run's pipeline only writes after that run's
/// publication, which this lock already orders after any in-flight cleanup.
/// (Cross-process writers — daemon, CLI — are outside this lock and remain
/// guarded by the PID check alone.)
fn progress_publication_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(Default::default)
}

/// Test-only rendezvous for pausing a failure cleanup between its ownership
/// check and its terminal write (inside the publication lock), so tests can
/// deterministically drive the cleanup-vs-newer-publication interleaving.
/// Keyed by state root so concurrently running tests can never trip each
/// other's gate.
#[cfg(test)]
type CleanupPauseGate = (
    tokio::sync::oneshot::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
);

#[cfg(test)]
static CLEANUP_PAUSE_GATES: OnceLock<Mutex<std::collections::HashMap<PathBuf, CleanupPauseGate>>> =
    OnceLock::new();

/// Arms a one-shot pause in the next failure cleanup for `state_root`.
/// Returns (`reached`, `proceed`): `reached` resolves once the cleanup has
/// passed its ownership check and holds the publication lock; sending on
/// `proceed` lets it continue to the write.
#[cfg(test)]
fn arm_cleanup_pause_for_test(
    state_root: &Path,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
    let (proceed_tx, proceed_rx) = tokio::sync::oneshot::channel();
    CLEANUP_PAUSE_GATES
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .insert(state_root.to_path_buf(), (reached_tx, proceed_rx));
    (reached_rx, proceed_tx)
}

#[cfg(test)]
async fn maybe_pause_cleanup_for_test(state_root: &Path) {
    let gate = CLEANUP_PAUSE_GATES
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .remove(state_root);
    if let Some((reached_tx, proceed_rx)) = gate {
        let _ = reached_tx.send(());
        let _ = proceed_rx.await;
    }
}

/// Human-readable reason extracted from a caught panic payload.
fn panic_reason(panic: &(dyn std::any::Any + Send)) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_string())
}

/// Spawn an indexing rebuild in the background task so oneup_start
/// returns promptly. The rebuild runs asynchronously and updates progress via
/// index_status.json, which agents can poll with oneup_status.
fn spawn_rebuild_task(
    roots: &McpProjectRoots,
    rebuild: bool,
    scope_add: Option<Vec<String>>,
    scope_narrow: Option<Vec<String>>,
    run_id: String,
) -> tokio::task::JoinHandle<ReadinessPayload> {
    let roots = roots.clone();
    tokio::spawn(async move {
        // Catch panics so every failure after the pre-spawn scope publication
        // durably records a terminal state: a panicking rebuild would otherwise
        // strand the persisted `Running` snapshot exactly like an error would.
        // (Panics inside the rebuild-lock-holding section are already converted
        // to errors by `run_index` itself, while the lock is still held; this
        // guards the pre-lock stretch.)
        let outcome = futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(
            run_index_then_classify(&roots, rebuild, scope_add, scope_narrow, &run_id),
        ))
        .await;
        match outcome {
            Ok(payload) => payload,
            Err(panic) => {
                let reason = panic_reason(panic.as_ref());
                tracing::warn!("background rebuild task panicked: {reason}");
                record_rebuild_failure_progress(&roots, &run_id, RebuildLockHeld::No, &reason)
                    .await;
                blocked_readiness(
                    &roots.state_root,
                    &roots.source_root,
                    &roots.worktree_context,
                    format!("indexing task panicked: {reason}"),
                )
                .await
            }
        }
    })
}

/// How long `oneup_start` waits for a spawned rebuild before
/// detaching and returning progress. Fast operations (small repos, drift
/// refreshes, auto-init failures) complete inside the budget so callers get
/// the final readiness — drift cleared, blocked surfaced with its reason —
/// preserving the pre-existing MCP contract. Long rebuilds detach at the
/// budget (well under the 5s acceptance bound) and callers poll
/// `oneup_status`. Env-tunable for tests.
fn start_response_budget() -> std::time::Duration {
    let ms = std::env::var("ONEUP_START_RESPONSE_BUDGET_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(2_000);
    std::time::Duration::from_millis(ms)
}

pub async fn start(
    roots: &McpProjectRoots,
    mode: StartMode,
    scope_add: Option<Vec<String>>,
    scope_narrow: Option<Vec<String>>,
) -> anyhow::Result<ReadinessPayload> {
    // Check readiness first to determine rebuild requirements
    let readiness = check_status(roots).await;

    tracing::debug!(
        "ops::start: mode={:?}, scope_add={:?}, status={:?}",
        mode,
        scope_add,
        readiness.status
    );

    // Determine if a rebuild (vs incremental write) is needed based on scope changes
    // and index state:
    // - scope_narrow always requires rebuild (atomic rebuild via StagingRebuild)
    // - scope_add on unscoped index requires rebuild (fresh staging DB, prevents metadata match)
    // - scope_add on already-scoped index can be incremental (widening existing scope)
    // - Reindex mode requires rebuild
    let scope_affects_rebuild = scope_add.is_some() || scope_narrow.is_some();
    let rebuild_mode = if scope_affects_rebuild {
        // Use scope-aware rebuild decision: check if this is first scoped or widening
        match determine_rebuild_mode_for_scope(
            &roots.state_root,
            scope_add.as_ref(),
            scope_narrow.as_ref(),
        )
        .await
        {
            Ok(mode) => mode,
            Err(e) => {
                tracing::warn!(
                    "failed to determine scope-based rebuild mode, defaulting to rebuild: {}",
                    e
                );
                true // Default to rebuild on error
            }
        }
    } else {
        // For non-scope operations, follow the mode logic
        mode == StartMode::Reindex
    };

    // Make oneup_start non-blocking. Spawn indexing in the background
    // and return immediately with status Indexing + progress metadata.
    let should_spawn = match mode {
        StartMode::IndexIfMissing if readiness.status == ReadinessStatus::Missing => true,
        StartMode::IndexIfNeeded if index_if_needed_applies(&readiness) => true,
        StartMode::Reindex => true,
        _ => scope_affects_rebuild,
    };

    if should_spawn {
        // Per-start identity: cleanup after a failed rebuild keys on this so
        // an older run can never overwrite a record published by a newer
        // overlapping start in the same (long-lived, same-PID) MCP process.
        let run_id = next_rebuild_run_id();
        // Make a requested scope observable BEFORE the rebuild task is spawned:
        // this function can detach at the response budget long before the
        // background task reaches its own progress write (it computes scope,
        // loads the registry, and resolves config first), and a client that
        // polls `oneup_status` immediately after `oneup_start` returns must
        // already see `index_scope`. Publication is part of the start outcome:
        // a scope that cannot be validated or durably recorded returns Blocked
        // with the reason instead of spawning, so any non-blocked scoped start
        // implies the requested scope is already visible to `oneup_status`.
        if scope_affects_rebuild {
            let new_scope =
                match compute_new_scope(&roots.state_root, scope_add.clone(), scope_narrow.clone())
                    .await
                {
                    Ok(new_scope) => new_scope,
                    Err(err) => {
                        return Ok(blocked_readiness(
                            &roots.state_root,
                            &roots.source_root,
                            &roots.worktree_context,
                            format!("invalid index scope request: {err}"),
                        )
                        .await);
                    }
                };
            if let Err(err) = write_initial_scope_progress(roots, &new_scope, &run_id).await {
                return Ok(blocked_readiness(
                    &roots.state_root,
                    &roots.source_root,
                    &roots.worktree_context,
                    format!("failed to record the requested index scope: {err}"),
                )
                .await);
            }
        }

        // Spawn the rebuild in the background so oneup_start returns promptly
        tracing::debug!(
            "ops::start: spawning rebuild (rebuild_mode={}, scope_add={:?})",
            rebuild_mode,
            scope_add
        );
        let rebuild_handle =
            spawn_rebuild_task(roots, rebuild_mode, scope_add, scope_narrow, run_id);
        // Bounded wait — return the final readiness when the rebuild
        // completes inside the budget; otherwise detach (the task keeps
        // running) and return current status with progress for polling.
        match tokio::time::timeout(start_response_budget(), rebuild_handle).await {
            Ok(Ok(payload)) => Ok(payload),
            Ok(Err(join_err)) => {
                tracing::warn!("background rebuild task panicked: {join_err}");
                Ok(check_status(roots).await)
            }
            Err(_elapsed) => Ok(check_status(roots).await),
        }
    } else {
        tracing::debug!("ops::start: NOT spawning (should_spawn=false)");
        Ok(readiness)
    }
}

/// `index_if_needed` indexes when no usable index exists, when the index is
/// degraded, or when the recorded indexed-at HEAD drifted from the live
/// repository HEAD, so the drift advisory in `oneup_status` is self-serve.
fn index_if_needed_applies(readiness: &ReadinessPayload) -> bool {
    matches!(
        readiness.status,
        ReadinessStatus::Missing | ReadinessStatus::Degraded
    ) || readiness.drifted == Some(true)
}

/// Reports whether an index rebuild/refresh is in progress for `context_id`,
/// so a served search can be flagged stale-but-available
/// ([`STALE_REBUILD_REASON`]).
///
/// Derived only from signals that already exist (no new status file or field):
/// - the daemon's `daemon_context_status.json` `last_refresh_state` is
///   `Pending`/`Running` (reusing the same reader and predicate as
///   [`classify_readiness`]), and/or
/// - the single-writer rebuild lock is currently held by another process — a
///   one-shot `1up index`/`reindex` or MCP rebuild — detected by a
///   non-blocking probe of [`lifecycle::try_acquire_rebuild_lock`].
///
/// The lock probe never retains the lock: any guard it acquires is dropped
/// immediately, so it only observes whether some *other* holder exists. A
/// probe error degrades to the refresh-state signal alone rather than failing
/// the read path. This is the MCP/CLI (out-of-process) detector; the daemon's
/// own search path detects from its in-memory refresh state instead, so the
/// daemon does not depend on the MCP layer.
pub(crate) fn rebuild_in_progress(state_root: &Path, context_id: &str) -> bool {
    let refresh_active =
        crate::cli::project_status_files::read_daemon_context_status(state_root, context_id)
            .is_some_and(|status| status.last_refresh_state.is_in_flight());

    refresh_active || rebuild_lock_held(state_root)
}

/// Non-blocking probe of the single-writer rebuild lock: `true` when another
/// process currently holds it. Any guard acquired here is dropped immediately,
/// so the probe never blocks a rebuild owner; a probe error reads as "not
/// held" so a transient lock-file error cannot mask served results.
fn rebuild_lock_held(state_root: &Path) -> bool {
    matches!(lifecycle::try_acquire_rebuild_lock(state_root), Ok(None))
}

pub async fn classify_readiness(
    state_root: &Path,
    source_root: &Path,
    worktree_context: &WorktreeContext,
) -> ReadinessPayload {
    let project_id_result = project::read_project_id(state_root);
    let project_initialized = project_id_result.is_ok();
    let db_path = project_db_path(state_root);
    let index_present = db_path.exists();
    let index_progress =
        read_index_progress_for_context(state_root, &worktree_context.context_id).await;
    let daemon_context_status = crate::cli::project_status_files::read_daemon_context_status(
        state_root,
        &worktree_context.context_id,
    );
    let daemon_refresh_active = daemon_context_status
        .as_ref()
        .is_some_and(|status| status.last_refresh_state.is_in_flight());
    let daemon_status = daemon_context_status
        .as_ref()
        .and_then(|status| {
            status
                .last_file_check_at
                .map(|last_file_check_at| DaemonProjectStatus { last_file_check_at })
        })
        .or_else(|| crate::cli::project_status_files::read_daemon_status(state_root));
    let mut payload = ReadinessPayload {
        status: ReadinessStatus::Missing,
        summary: String::new(),
        state_root: path_string(state_root),
        source_root: path_string(source_root),
        project_initialized,
        index_present,
        index_readable: false,
        schema_version: None,
        indexed_files: None,
        total_segments: None,
        vector_rows: None,
        embeddable_segments: None,
        indexed_at_head: None,
        current_head: None,
        drifted: None,
        reason: None,
        index_progress,
        daemon_status,
        index_scope: None,
        scope_proposal: None,
    };

    if let Err(err) = project_id_result {
        if !is_not_initialized(&err) {
            payload.status = ReadinessStatus::Blocked;
            payload.summary =
                "The repository cannot be prepared for 1up MCP discovery.".to_string();
            payload.reason = Some(err.to_string());
            return payload;
        }
    }

    // A terminal `Failed` progress record keeps the failure visible to status
    // until a later run's progress writes supersede it: Blocked when no
    // usable index backs discovery, Degraded (below) when a previous index is
    // still served. Without this, a detached rebuild failure would silently
    // classify as ready/missing on the next status call.
    let failed_reason = payload
        .index_progress
        .as_ref()
        .filter(|progress| progress.state == IndexState::Failed)
        .map(|progress| {
            progress
                .message
                .clone()
                .unwrap_or_else(|| "the last indexing run failed".to_string())
        });

    if payload
        .index_progress
        .as_ref()
        .is_some_and(|progress| progress.state == IndexState::Running)
    {
        payload.status = ReadinessStatus::Indexing;
        payload.summary = "Indexing is currently running.".to_string();
        // Extract scope from progress file (visible during indexing, independent of swap)
        if let Some(progress) = &payload.index_progress {
            payload.index_scope = extract_scope_from_progress(progress);
        }
        // If scope not in progress yet, try reading from database meta (rebuild in progress)
        if payload.index_scope.is_none() {
            if let Ok(Some(scope)) =
                compute_index_scope(state_root, source_root, &worktree_context.context_id).await
            {
                payload.index_scope = Some(scope);
            }
        }
        return payload;
    }

    if !project_initialized || !index_present {
        if daemon_refresh_active {
            payload.status = ReadinessStatus::Indexing;
            payload.summary = "Indexing is currently running.".to_string();
            // Extract scope from progress file (visible during indexing, independent of swap)
            if let Some(progress) = &payload.index_progress {
                payload.index_scope = extract_scope_from_progress(progress);
            }
            return payload;
        }
        if let Some(reason) = failed_reason {
            payload.status = ReadinessStatus::Blocked;
            payload.summary =
                "The repository cannot be prepared for 1up MCP discovery.".to_string();
            payload.reason = Some(reason);
            return payload;
        }
        payload.status = ReadinessStatus::Missing;
        payload.summary = "No usable 1up index is available for this repository.".to_string();
        payload.reason = Some("run oneup_start with an explicit indexing mode".to_string());
        attach_scope_proposal_if_fresh(&mut payload, state_root, source_root);
        return payload;
    }

    let db = match Db::open_ro(&db_path).await {
        Ok(db) => db,
        Err(err) => {
            if daemon_refresh_active {
                payload.status = ReadinessStatus::Indexing;
                payload.summary = "Indexing is currently running.".to_string();
                // Extract scope from progress file (visible during indexing, independent of swap)
                if let Some(progress) = &payload.index_progress {
                    payload.index_scope = extract_scope_from_progress(progress);
                }
                return payload;
            }
            payload.status = ReadinessStatus::Stale;
            payload.summary = "The index exists but cannot be opened.".to_string();
            payload.reason = Some(err.to_string());
            return payload;
        }
    };

    let conn = match db.connect_tuned().await {
        Ok(conn) => conn,
        Err(err) => {
            if daemon_refresh_active {
                payload.status = ReadinessStatus::Indexing;
                payload.summary = "Indexing is currently running.".to_string();
                // Extract scope from progress file (visible during indexing, independent of swap)
                if let Some(progress) = &payload.index_progress {
                    payload.index_scope = extract_scope_from_progress(progress);
                }
                return payload;
            }
            payload.status = ReadinessStatus::Stale;
            payload.summary = "The index exists but cannot be read.".to_string();
            payload.reason = Some(err.to_string());
            return payload;
        }
    };

    payload.schema_version = schema::get_schema_version(&conn).await.ok().flatten();

    if let Err(err) = schema::ensure_current_tolerating_init(
        &conn,
        &schema::SchemaContext::new(&db_path, source_root),
    )
    .await
    {
        if daemon_refresh_active {
            payload.status = ReadinessStatus::Indexing;
            payload.summary = "Indexing is currently running.".to_string();
            // Extract scope from progress file (visible during indexing, independent of swap)
            if let Some(progress) = &payload.index_progress {
                payload.index_scope = extract_scope_from_progress(progress);
            }
            return payload;
        }
        // A freshly-initializing index (DB file and tables present, but the
        // version row not yet written) survives `is_initializing_schema_error`
        // even after the bounded ride-out above when the writer is slower than
        // our budget. Report it as `missing` rather than `stale`: the same
        // `oneup_start` remediation applies, it self-corrects on the next poll
        // once initialization commits, and we never mislabel a genuinely
        // initializing index as a permanent version mismatch. Real
        // out-of-date / newer-than-supported schemas keep reporting `stale`.
        if schema::is_initializing_schema_error(&err) {
            payload.status = ReadinessStatus::Missing;
            payload.summary = "No usable 1up index is available for this repository.".to_string();
            payload.reason = Some("run oneup_start with an explicit indexing mode".to_string());
            return payload;
        }
        payload.status = ReadinessStatus::Stale;
        payload.summary = "The index schema is stale or incompatible.".to_string();
        payload.reason = Some(err.to_string());
        return payload;
    }

    // index_readable must imply the own-context counts were actually read. If the
    // counts cannot be read right now (e.g. the auto-started daemon holds the write
    // lock during a concurrent refresh), report indexing/stale rather than claiming
    // index_readable with silently-omitted counts — that mismatch (index_readable
    // true alongside a null count) is what made the status-count assertions flaky
    // under parallel CI load.
    let (indexed_files, total_segments) = match (
        count_files_for_context(&conn, &worktree_context.context_id).await,
        count_segments_for_context(&conn, &worktree_context.context_id).await,
    ) {
        (Ok(files), Ok(segments)) => (files, segments),
        _ => {
            if daemon_refresh_active {
                payload.status = ReadinessStatus::Indexing;
                payload.summary = "Indexing is currently running.".to_string();
            } else {
                payload.status = ReadinessStatus::Stale;
                payload.summary = "The index exists but its contents cannot be read.".to_string();
            }
            return payload;
        }
    };
    payload.index_readable = true;
    payload.indexed_files = Some(indexed_files);
    payload.total_segments = Some(total_segments);
    payload.vector_rows = count_vector_rows_for_context(&conn, &worktree_context.context_id)
        .await
        .ok();
    payload.embeddable_segments =
        count_embeddable_segments_for_context(&conn, &worktree_context.context_id)
            .await
            .ok();

    let recorded_head = get_worktree_context_head_oid(&conn, &worktree_context.context_id)
        .await
        .ok()
        .flatten();
    apply_head_drift(
        &mut payload,
        recorded_head,
        worktree_context.head_oid.clone(),
    );

    if payload.total_segments.unwrap_or(0) == 0 {
        if daemon_refresh_active {
            payload.status = ReadinessStatus::Indexing;
            payload.summary = "Indexing is currently running.".to_string();
            return payload;
        }
        if let Some(reason) = failed_reason {
            payload.status = ReadinessStatus::Blocked;
            payload.summary =
                "The repository cannot be prepared for 1up MCP discovery.".to_string();
            payload.reason = Some(reason);
            return payload;
        }
        payload.status = ReadinessStatus::Missing;
        payload.summary = "No indexed code is available for this repository.".to_string();
        payload.reason = Some("run oneup_start with an explicit indexing mode".to_string());
        attach_scope_proposal_if_fresh(&mut payload, state_root, source_root);
        return payload;
    }

    let progress_without_embeddings = payload
        .index_progress
        .as_ref()
        .is_some_and(|progress| !progress.embeddings_enabled);
    let progress_reason = payload
        .index_progress
        .as_ref()
        .and_then(|progress| progress.embedding_unavailable_reason.clone());
    let embedding_reason = embedder::model_unavailable_reason_for_status()
        .map(|reason| unavailable_reason_text(&reason));

    // Compute and populate index scope for coverage disclosure
    // Use the existing connection to avoid contention
    if let Ok(scope_roots) = schema::read_scope_from_meta(&conn).await {
        let indexed_file_paths =
            get_all_file_paths_for_context(&conn, &worktree_context.context_id)
                .await
                .unwrap_or_default();
        let dir_counts = count_files_per_directory(source_root).unwrap_or_default();
        let total_files: usize = dir_counts.values().sum();
        let roots = scope_roots.unwrap_or_default();
        payload.index_scope = Some(IndexScope {
            eligibility_note: unscoped_eligibility_note(&roots),
            roots,
            indexed_files: indexed_file_paths.len(),
            total_files,
        });
    }

    // A recorded rebuild failure over a still-usable index degrades (not
    // blocks) readiness: discovery keeps serving the previous index, but the
    // failure stays visible until a later successful run supersedes it.
    if let Some(reason) = failed_reason {
        payload.status = ReadinessStatus::Degraded;
        payload.summary =
            "The previous index is still served, but the last indexing run failed.".to_string();
        payload.reason = Some(reason);
        return payload;
    }

    if progress_without_embeddings || embedding_reason.is_some() {
        payload.status = ReadinessStatus::Degraded;
        payload.summary =
            "The index is readable, but semantic embeddings are unavailable.".to_string();
        payload.reason = Some(
            embedding_reason
                .or(progress_reason)
                .unwrap_or_else(|| "latest index was built without embeddings".to_string()),
        );
        return payload;
    }

    // The model is claimed available, so a context with embeddable segments
    // but zero stored vector rows means the index and the claim disagree:
    // report it as degraded instead of ready.
    let zero_vector_coverage = matches!(
        (payload.vector_rows, payload.embeddable_segments),
        (Some(0), Some(embeddable)) if embeddable > 0
    );
    if zero_vector_coverage {
        payload.status = ReadinessStatus::Degraded;
        payload.summary =
            "The index is readable, but semantic embeddings are unavailable.".to_string();
        payload.reason = Some(format!(
            "embedding model is available but the index stores no vector rows for this context (0 of {} embeddable segments); run oneup_start with mode \"reindex\"",
            payload.embeddable_segments.unwrap_or(0)
        ));
        return payload;
    }

    payload.status = ReadinessStatus::Ready;
    payload.summary = "The repository is ready for 1up MCP search.".to_string();

    payload
}

/// Populate the advisory head-drift fields from the head OID recorded at the
/// last successful index run and the live repository HEAD. The fields are
/// emitted only when both sides are known; otherwise all three stay absent.
/// Drift never changes the readiness status itself.
fn apply_head_drift(
    payload: &mut ReadinessPayload,
    recorded_head: Option<String>,
    current_head: Option<String>,
) {
    let (Some(recorded), Some(current)) = (recorded_head, current_head) else {
        return;
    };
    payload.drifted = Some(recorded != current);
    payload.indexed_at_head = Some(recorded);
    payload.current_head = Some(current);
}

pub async fn blocked_readiness(
    state_root: &Path,
    source_root: &Path,
    worktree_context: &WorktreeContext,
    reason: impl Into<String>,
) -> ReadinessPayload {
    let project_initialized = project::read_project_id(state_root).is_ok();
    let db_path = project_db_path(state_root);
    let index_progress =
        read_index_progress_for_context(state_root, &worktree_context.context_id).await;
    let daemon_status = crate::cli::project_status_files::read_daemon_status_for_context(
        state_root,
        &worktree_context.context_id,
    );
    ReadinessPayload {
        status: ReadinessStatus::Blocked,
        summary: "The repository cannot be prepared for 1up MCP discovery.".to_string(),
        state_root: path_string(state_root),
        source_root: path_string(source_root),
        project_initialized,
        index_present: db_path.exists(),
        index_readable: false,
        schema_version: None,
        indexed_files: None,
        total_segments: None,
        vector_rows: None,
        embeddable_segments: None,
        indexed_at_head: None,
        current_head: None,
        drifted: None,
        reason: Some(reason.into()),
        index_progress,
        daemon_status,
        index_scope: None,
        scope_proposal: None,
    }
}

pub fn blocked_readiness_for_path(path: &str, reason: impl Into<String>) -> ReadinessPayload {
    let raw_path = Path::new(path);
    ReadinessPayload {
        status: ReadinessStatus::Blocked,
        summary: "The repository cannot be prepared for 1up MCP discovery.".to_string(),
        state_root: path_string(raw_path),
        source_root: path_string(raw_path),
        project_initialized: false,
        index_present: false,
        index_readable: false,
        schema_version: None,
        indexed_files: None,
        total_segments: None,
        vector_rows: None,
        embeddable_segments: None,
        indexed_at_head: None,
        current_head: None,
        drifted: None,
        reason: Some(reason.into()),
        index_progress: None,
        daemon_status: None,
        index_scope: None,
        scope_proposal: None,
    }
}

pub async fn run_search(
    state_root: &Path,
    worktree_context: &WorktreeContext,
    queries: &[String],
    limit: usize,
    path_prefix: Option<&str>,
) -> anyhow::Result<SearchPayload> {
    // Bound search latency to <10s during rebuild. If rebuild is in progress,
    // apply a timeout; otherwise search without timeout (expected to be fast on idle index).
    if rebuild_in_progress(state_root, &worktree_context.context_id) {
        match tokio::time::timeout(
            std::time::Duration::from_secs(10),
            retry_on_db_lock(|| async {
                run_search_once(state_root, worktree_context, queries, limit, path_prefix).await
            }),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => {
                // Timeout: return degraded response with empty results
                Ok(SearchPayload {
                    status: OperationStatus::Degraded,
                    results: vec![],
                    degraded_reason: Some(
                        "Search timed out during ongoing rebuild; try again after rebuilding completes."
                            .to_string(),
                    ),
                    index_scope: compute_index_scope(state_root, &worktree_context.source_root, &worktree_context.context_id)
                        .await
                        .ok()
                        .flatten(),
                })
            }
        }
    } else {
        retry_on_db_lock(|| async {
            run_search_once(state_root, worktree_context, queries, limit, path_prefix).await
        })
        .await
    }
}

/// Reduce the per-query ranked lists into the final result list. A single query
/// is returned untouched; two or more queries are fused with RRF, deduped by
/// handle, and truncated to `limit`. On the assembled list, an implementation-
/// intent query then sinks doc-section results below code results (a stable
/// reorder, no scoring or filtering) so implementation searches surface code
/// first.
fn finalize_search_lists(
    lists: Vec<Vec<SearchResult>>,
    limit: usize,
    queries: &[String],
) -> Vec<SearchResult> {
    let merged = if lists.len() <= 1 {
        lists.into_iter().next().unwrap_or_default()
    } else {
        crate::search::hybrid::merge_multi_query_results(lists, limit)
    };
    crate::search::hybrid::demote_doc_sections_for_implementation_intent(merged, queries)
}

/// Process-global warm embedding runtime for the in-process MCP fallback search
/// path.
///
/// The MCP server process serves many tool calls over its lifetime. When the
/// daemon is unavailable, `run_search_once` embeds the query in-process; keeping a
/// single warmed [`EmbeddingRuntime`] here lets the second and later searches
/// reuse the already-loaded ONNX session instead of cold-loading a fresh
/// `EmbeddingRuntime::default()` per call. A `tokio::sync::Mutex` serializes
/// access because the cached `Embedder` is borrowed `&mut` during a search; MCP
/// queries are served one at a time on this path, so the lock is effectively
/// uncontended.
fn fallback_embedding_runtime() -> &'static tokio::sync::Mutex<EmbeddingRuntime> {
    static RUNTIME: std::sync::OnceLock<tokio::sync::Mutex<EmbeddingRuntime>> =
        std::sync::OnceLock::new();
    RUNTIME.get_or_init(|| tokio::sync::Mutex::new(EmbeddingRuntime::default()))
}

async fn run_search_once(
    state_root: &Path,
    worktree_context: &WorktreeContext,
    queries: &[String],
    limit: usize,
    path_prefix: Option<&str>,
) -> anyhow::Result<SearchPayload> {
    let current = open_current_index(state_root).await?;
    let mut search_scope = SearchScope::from_worktree_context(worktree_context);
    if let Some(prefix) = path_prefix {
        search_scope = search_scope.with_path_prefix(prefix);
    }

    // Cheap vector-presence gate first: when the index holds no embeddings
    // for this context, the embedding model must never be initialized.
    let has_vectors = retrieval::has_indexed_embeddings(&current.conn, &search_scope).await?;
    let (results, embedding_reason) = if has_vectors {
        // Warm the in-process fallback embedding runtime instead of cold-loading
        // `EmbeddingRuntime::default()` on every call. The MCP server is a
        // long-lived process serving many tool calls; this in-process search path
        // runs when the daemon is unavailable, so a per-call cold load would
        // re-read and re-initialize the ONNX session for each query. The runtime
        // is held in a process-global cache and `prepare_for_search` returns
        // `Warm` (a no-op) after the first successful load.
        let mut runtime = fallback_embedding_runtime().lock().await;
        let embedding_status = runtime.prepare_for_search(1)?;
        let embedding_reason = embedding_unavailable_reason(&embedding_status);
        let results = if embedding_status.is_available() {
            // Reuse the warm cache's per-context vector `COUNT(*)` instead of
            // recomputing it on every search (mirrors the daemon's
            // `ProjectState::cached_vector_count`). A cache miss computes it
            // once and records it against the current warm-index generation;
            // `warm_index_connection` clears the whole map entry's counts on
            // a build-aside swap, so a populated value always reflects the
            // currently-open index.
            let vector_count =
                match cached_vector_count_for_context(&current.db_path, search_scope.context_id())
                    .await
                {
                    Some(count) => count,
                    None => {
                        let count =
                            retrieval::count_vector_rows_for_context(&current.conn, &search_scope)
                                .await?;
                        record_vector_count_for_context(
                            &current.db_path,
                            search_scope.context_id(),
                            count,
                        )
                        .await;
                        count
                    }
                };
            // Reuse one warm engine across every query in the multi-query set:
            // `search` takes `&mut self`, so each query runs against the same
            // loaded embedder and cached vector count, then the per-query ranked
            // lists are fused (a single query returns unchanged).
            let mut engine = HybridSearchEngine::new_scoped(
                &current.conn,
                runtime.current_embedder(),
                search_scope.clone(),
            )
            .with_has_vectors(has_vectors)
            .with_vector_count(vector_count);
            let mut lists = Vec::with_capacity(queries.len());
            for query in queries {
                lists.push(engine.search(query, limit).await?);
            }
            finalize_search_lists(lists, limit, queries)
        } else {
            let engine = HybridSearchEngine::new_scoped(&current.conn, None, search_scope.clone());
            let mut lists = Vec::with_capacity(queries.len());
            for query in queries {
                lists.push(engine.fts_only_search(query, limit).await?);
            }
            finalize_search_lists(lists, limit, queries)
        };
        (results, embedding_reason)
    } else {
        let engine = HybridSearchEngine::new_scoped(&current.conn, None, search_scope.clone());
        let mut lists = Vec::with_capacity(queries.len());
        for query in queries {
            lists.push(engine.fts_only_search(query, limit).await?);
        }
        (
            finalize_search_lists(lists, limit, queries),
            Some(NO_INDEXED_EMBEDDINGS_REASON.to_string()),
        )
    };

    // Stale-but-available: when a rebuild/refresh is in progress for this
    // context, readers keep serving the prior index (build-aside), so
    // flag the served results as possibly stale. The notice rides only in
    // `degraded_reason` (no parallel field) and the render path keeps it off
    // stdout.
    let stale_reason = rebuild_in_progress(state_root, &worktree_context.context_id)
        .then(|| STALE_REBUILD_REASON.to_string());
    let degraded_reason = combine_degraded_reasons(
        stale_reason,
        combine_degraded_reasons(embedding_reason, search_scope.degraded_reason()),
    );

    let status = match degraded_reason {
        Some(_) => OperationStatus::Degraded,
        None if results.is_empty() => OperationStatus::Empty,
        None => OperationStatus::Ok,
    };

    // Compute and populate index scope for coverage disclosure
    let index_scope = compute_index_scope(
        state_root,
        &worktree_context.source_root,
        &worktree_context.context_id,
    )
    .await
    .ok()
    .flatten();

    Ok(SearchPayload {
        status,
        results: results.into_iter().map(search_hit).collect(),
        degraded_reason,
        index_scope,
    })
}

pub async fn get_handles(
    state_root: &Path,
    worktree_context: &WorktreeContext,
    handles: &[String],
    verbosity: Option<&str>,
) -> anyhow::Result<ReadPayload> {
    retry_on_db_lock(|| async {
        get_handles_once(state_root, worktree_context, handles, verbosity).await
    })
    .await
}

async fn get_handles_once(
    state_root: &Path,
    worktree_context: &WorktreeContext,
    handles: &[String],
    verbosity: Option<&str>,
) -> anyhow::Result<ReadPayload> {
    let current = open_current_index(state_root).await?;
    let context_id = &worktree_context.context_id;
    // The failed-handle memory is keyed and identity-stamped against the same
    // canonical db path the warm cache uses, so a rejection reflects the exact
    // index generation the retry would otherwise re-query.
    let current_identity = index_file_identity(&current.db_path);
    let normalized: Vec<String> = handles
        .iter()
        .map(|handle| normalize_handle(handle))
        .collect();

    // Pre-pass: reject a handle that already failed this session against this
    // same index identity without re-querying. An identity mismatch drops
    // the stale entry so the handle resolves fresh below; an empty handle is
    // never a memory key (it resolves to the empty-handle rejection instead).
    let mut prejudged: Vec<Option<ReadRecord>> = Vec::with_capacity(handles.len());
    {
        let mut memory = failed_handle_memory()
            .lock()
            .expect("failed-handle memory mutex poisoned");
        for (raw_handle, normalized) in handles.iter().zip(&normalized) {
            if normalized.is_empty() {
                prejudged.push(None);
                continue;
            }
            let key = (
                current.db_path.clone(),
                context_id.clone(),
                normalized.clone(),
            );
            match memory.lookup(&key, current_identity) {
                Some(record) => {
                    let source = ReadSource::Handle {
                        raw: raw_handle.clone(),
                        normalized: normalized.clone(),
                    };
                    prejudged.push(Some(read_rejected_handle(
                        source,
                        record.outcome,
                        record.matching_handles,
                    )));
                }
                None => prejudged.push(None),
            }
        }
    }

    // Resolve only the handles not already rejected, preserving the batched
    // exact-then-prefix path (and its within-call independence) untouched.
    let to_resolve: Vec<String> = handles
        .iter()
        .zip(&prejudged)
        .filter(|(_, slot)| slot.is_none())
        .map(|(handle, _)| handle.clone())
        .collect();
    let resolved =
        resolve_handle_records(&current.conn, context_id, &to_resolve, verbosity).await?;

    // Merge the pre-rejected and freshly-resolved records back into input order.
    let mut resolved = resolved.into_iter();
    let records: Vec<ReadRecord> = prejudged
        .into_iter()
        .map(|slot| {
            slot.unwrap_or_else(|| {
                resolved
                    .next()
                    .expect("resolved records cover every non-rejected handle")
            })
        })
        .collect();

    // Post-pass: remember fresh failures and forget entries a success cleared.
    // Pre-rejected records keep their existing entry (they were never
    // re-queried); a transient `Error` is not remembered so it can be retried.
    {
        let mut memory = failed_handle_memory()
            .lock()
            .expect("failed-handle memory mutex poisoned");
        for (normalized, record) in normalized.iter().zip(&records) {
            if normalized.is_empty() {
                continue;
            }
            let key = (
                current.db_path.clone(),
                context_id.clone(),
                normalized.clone(),
            );
            match record.status {
                ReadStatus::NotFound | ReadStatus::Ambiguous => memory.record_failure(
                    key,
                    current_identity,
                    record.status,
                    record.matching_handles.clone(),
                ),
                ReadStatus::Found => memory.clear(&key),
                ReadStatus::Rejected | ReadStatus::Error => {}
            }
        }
    }

    Ok(ReadPayload {
        status: aggregate_read_status(&records),
        records,
    })
}

/// Resolves the shared [`ScanFilter`] (secret defaults + configured per-project
/// globs/dotfile overrides) governing the given worktree context, matching the
/// same registry-backed resolution `run_index` uses for the indexer so
/// `oneup_context` refuses exactly the files the indexer would exclude.
/// Synchronous: reads only the project registry file, never the index DB.
pub fn resolve_context_scan_filter(
    worktree_context: &WorktreeContext,
) -> anyhow::Result<ScanFilter> {
    let registry = Registry::load()?;
    let indexing_config = config::resolve_indexing_config(
        None,
        None,
        registry.indexing_config_for_context(worktree_context),
    )?;
    Ok(ScanFilter::new(
        &indexing_config.include_globs,
        &indexing_config.exclude_globs,
        &indexing_config.index_hidden_dirs,
    )?)
}

async fn read_scope_for_context(state_root: &Path) -> anyhow::Result<Vec<String>> {
    let db_path = project_db_path(state_root);
    if !db_path.exists() {
        return Ok(Vec::new());
    }

    let db = Db::open_ro(&db_path)
        .await
        .map_err(|e| anyhow::anyhow!("failed to open index: {}", e))?;
    let conn = db
        .connect_tuned()
        .await
        .map_err(|e| anyhow::anyhow!("failed to connect to index: {}", e))?;

    schema::read_scope_from_meta(&conn)
        .await
        .map(|opt| opt.unwrap_or_default())
        .map_err(|e| anyhow::anyhow!("failed to read scope: {}", e))
}

pub async fn read_context_locations(
    state_root: &Path,
    source_root: &Path,
    scan_filter: &ScanFilter,
    locations: &[ReadLocation],
) -> anyhow::Result<ReadPayload> {
    let canonical_root = source_root
        .canonicalize()
        .with_context(|| format!("failed to resolve source root {}", source_root.display()))?;

    // Read scope from database to check if files are out-of-scope
    let scope = read_scope_for_context(state_root).await.unwrap_or_default();

    let mut records = Vec::with_capacity(locations.len());

    for location in locations {
        records.push(read_location_record(
            &canonical_root,
            scan_filter,
            location,
            &scope,
        ));
    }

    Ok(ReadPayload {
        status: aggregate_read_status(&records),
        records,
    })
}

pub async fn lookup_symbol(
    state_root: &Path,
    worktree_context: &WorktreeContext,
    request: SymbolLookupRequest,
) -> anyhow::Result<SymbolPayload> {
    if request.name.trim().is_empty() {
        bail!("symbol name cannot be empty");
    }

    retry_on_db_lock(|| {
        let request = request.clone();
        async move { lookup_symbol_once(state_root, worktree_context, request).await }
    })
    .await
}

async fn lookup_symbol_once(
    state_root: &Path,
    worktree_context: &WorktreeContext,
    request: SymbolLookupRequest,
) -> anyhow::Result<SymbolPayload> {
    let current = open_current_index(state_root).await?;
    let search_scope = SearchScope::from_worktree_context(worktree_context);
    let engine = SymbolSearchEngine::new_scoped(&current.conn, search_scope);

    let (definitions, references) = match request.include {
        SymbolInclude::Definitions => (
            engine
                .find_definitions(&request.name, request.fuzzy)
                .await?,
            Vec::new(),
        ),
        SymbolInclude::References => {
            let results = engine.find_references(&request.name, request.fuzzy).await?;
            (Vec::new(), only_references(results))
        }
        SymbolInclude::Both => {
            let results = engine.find_references(&request.name, request.fuzzy).await?;
            partition_symbol_results(results)
        }
    };

    let status = if definitions.is_empty() && references.is_empty() {
        OperationStatus::Empty
    } else {
        OperationStatus::Ok
    };

    Ok(SymbolPayload {
        status,
        definitions: definitions.into_iter().map(symbol_record).collect(),
        references: references.into_iter().map(symbol_record).collect(),
    })
}

pub async fn explore_impact(
    state_root: &Path,
    worktree_context: &WorktreeContext,
    request: ImpactRequest,
) -> anyhow::Result<ImpactResultEnvelope> {
    retry_on_db_lock(|| {
        let request = request.clone();
        async move { explore_impact_once(state_root, worktree_context, request).await }
    })
    .await
}

async fn explore_impact_once(
    state_root: &Path,
    worktree_context: &WorktreeContext,
    request: ImpactRequest,
) -> anyhow::Result<ImpactResultEnvelope> {
    let current = open_current_index(state_root).await?;
    let search_scope = SearchScope::from_worktree_context(worktree_context);
    let engine = ImpactHorizonEngine::new_scoped(&current.conn, search_scope);
    Ok(engine.explore(request).await?)
}

pub async fn search_structural(
    state_root: &Path,
    source_root: &Path,
    worktree_context: &WorktreeContext,
    pattern: &str,
    language_filter: Option<&str>,
) -> anyhow::Result<StructuralSearchReport> {
    retry_on_db_lock(|| async {
        search_structural_once(
            state_root,
            source_root,
            worktree_context,
            pattern,
            language_filter,
        )
        .await
    })
    .await
}

async fn search_structural_once(
    state_root: &Path,
    source_root: &Path,
    worktree_context: &WorktreeContext,
    pattern: &str,
    language_filter: Option<&str>,
) -> anyhow::Result<StructuralSearchReport> {
    let current = open_current_index(state_root).await?;
    let engine = StructuralSearchEngine::new_scoped(
        source_root,
        &current.conn,
        &worktree_context.context_id,
    );
    Ok(engine.search_report(pattern, language_filter).await?)
}

pub async fn compute_overview(
    state_root: &Path,
    worktree_context: &WorktreeContext,
) -> anyhow::Result<OverviewPayload> {
    retry_on_db_lock(|| async { compute_overview_once(state_root, worktree_context).await }).await
}

async fn compute_overview_once(
    state_root: &Path,
    worktree_context: &WorktreeContext,
) -> anyhow::Result<OverviewPayload> {
    let current = open_current_index(state_root).await?;
    let engine = overview::OverviewEngine::new(&current.conn);
    let digest = engine.compute(&worktree_context.context_id).await?;
    Ok(overview_payload(digest))
}

async fn retry_on_db_lock<T, F, Fut>(mut operation: F) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
{
    for attempt in 0..DB_LOCK_RETRY_ATTEMPTS {
        match operation().await {
            Ok(value) => return Ok(value),
            Err(err) => {
                if !is_lock_error(&err.to_string()) || attempt + 1 == DB_LOCK_RETRY_ATTEMPTS {
                    return Err(err);
                }
                tokio::time::sleep(Duration::from_millis(DB_LOCK_RETRY_DELAY_MS)).await;
            }
        }
    }

    unreachable!("database lock retry loop always returns on success or final failure")
}

async fn run_index_then_classify(
    roots: &McpProjectRoots,
    rebuild: bool,
    scope_add: Option<Vec<String>>,
    scope_narrow: Option<Vec<String>>,
    run_id: &str,
) -> ReadinessPayload {
    match run_index(roots, rebuild, scope_add, scope_narrow, run_id).await {
        Ok(_) => classify_after_index(roots).await,
        Err(err) => {
            // Surface the failure as blocked readiness rather than losing it
            // to a log line; progress remains available via status.
            tracing::warn!("background rebuild task failed: {}", err);
            let reason = err.to_string();
            // Failures inside the rebuild-lock-holding section were already
            // recorded by `run_index` under the lock; this covers pre-lock
            // failures, where only this run's own eager record may be
            // replaced. Re-recording an already-terminal record is a no-op
            // (the ownership guard requires `Running`).
            record_rebuild_failure_progress(roots, run_id, RebuildLockHeld::No, &reason).await;
            blocked_readiness(
                &roots.state_root,
                &roots.source_root,
                &roots.worktree_context,
                reason,
            )
            .await
        }
    }
}

async fn classify_after_index(roots: &McpProjectRoots) -> ReadinessPayload {
    let mut payload = check_status(roots).await;
    for _ in 0..20 {
        if payload.status != ReadinessStatus::Stale {
            return payload;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        payload = check_status(roots).await;
    }
    payload
}

/// Atomically persists `progress` as the project's `index_status.json`
/// (temp + fsync + rename via `atomic_replace`), so a concurrent reader never
/// observes a torn or zero-length file and misreports the index state. The
/// blocking secure-fs write runs on the blocking pool so it never stalls a
/// tokio worker.
async fn write_index_progress_atomic(
    state_root: &Path,
    progress: &IndexProgress,
) -> anyhow::Result<()> {
    let progress_path = config::project_dot_dir(state_root).join(INDEX_PROGRESS_FILE_NAME);
    let payload = serde_json::to_vec_pretty(progress)?;
    let state_root = state_root.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let secure_root = crate::shared::fs::ensure_secure_project_root(&state_root)?;
        crate::shared::fs::atomic_replace(
            &progress_path,
            &payload,
            &secure_root,
            crate::shared::constants::PROJECT_STATE_DIR_MODE,
            crate::shared::constants::SECURE_STATE_FILE_MODE,
        )
    })
    .await??;
    Ok(())
}

/// Writes the initial `index_status.json` carrying the requested scope, so
/// `oneup_status` reports `index_scope` from the moment a scoped rebuild is
/// requested — before the pipeline's own progress updates. No-op for an empty
/// (unscoped) scope.
///
/// Fail-loud: a failed write propagates so publication is part of the caller's
/// outcome — `ops::start` returns Blocked instead of spawning, and `run_index`
/// fails the run. A swallowed failure here would let a "successful" scoped
/// start return while `oneup_status` reports no scope, silently breaking the
/// invariant that a non-blocked scoped start makes the requested scope
/// immediately visible.
async fn write_initial_scope_progress(
    roots: &McpProjectRoots,
    new_scope: &[String],
    run_id: &str,
) -> anyhow::Result<()> {
    if new_scope.is_empty() {
        return Ok(());
    }
    let scope_info = crate::shared::types::IndexScopeInfo {
        requested: format!("scoped:{}", new_scope.len()),
        executed: String::new(), // Will be updated by pipeline
        changed_paths: 0,
        fallback_reason: None,
        roots: new_scope.to_vec(),
    };
    let initial_progress = crate::shared::types::IndexProgress {
        state: crate::shared::types::IndexState::Running,
        phase: crate::shared::types::IndexPhase::Pending,
        context_id: Some(roots.worktree_context.context_id.clone()),
        source_root: Some(roots.source_root.clone()),
        branch_name: roots.worktree_context.branch_name.clone(),
        branch_status: Some(roots.worktree_context.branch_status),
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
        message: Some("Preparing to index...".to_string()),
        parallelism: None,
        timings: None,
        scope: Some(scope_info),
        prefilter: None,
        indexer_pid: Some(std::process::id()),
        run_id: Some(run_id.to_string()),
        updated_at: chrono::Utc::now(),
    };
    // Under the publication lock so this write can never land inside a
    // concurrent failure cleanup's read-check-write window (see
    // `progress_publication_lock`).
    let _publication_guard = progress_publication_lock().lock().await;
    write_index_progress_atomic(&roots.state_root, &initial_progress).await
}

/// Whether the caller currently holds the single-writer rebuild lock, which
/// widens the failure-cleanup ownership rule (see
/// [`record_rebuild_failure_progress`]).
#[derive(Clone, Copy, PartialEq, Eq)]
enum RebuildLockHeld {
    Yes,
    No,
}

/// Durably transitions a `Running` progress record owned by the failed run to
/// a terminal `Failed` snapshot carrying the failure reason.
///
/// `ops::start` publishes a `Running` snapshot before the rebuild task spawns.
/// If the rebuild then fails anywhere — registry load, rebuild-lock
/// acquisition, staging open, the pipeline itself — the failure is otherwise
/// returned only in memory: the persisted snapshot keeps claiming `Running`,
/// and stale-progress repair can never reclaim it because the recorded PID
/// (this long-lived MCP server) stays alive, so `oneup_status` would report a
/// phantom indexing run indefinitely. The terminal state is `Failed`, which
/// readiness classification keeps surfacing (blocked, or degraded over a
/// still-usable index) until a later run's progress writes supersede it.
///
/// Ownership rule — a record is replaced only when ALL of:
/// - it claims `Running` (never overwrite `Complete`/`Failed`/`Idle`);
/// - its PID is this process (another daemon/CLI indexer is never touched);
/// - its `run_id` is this run's — or it has no `run_id` AND this run still
///   holds the single-writer rebuild lock (`RebuildLockHeld::Yes`). Pipeline
///   progress writes carry no `run_id`, and pipelines only run under the
///   rebuild lock, so while this run holds it a `run_id`-less record with our
///   PID can only be this run's own pipeline record. Outside the lock the
///   rule stays strict, so an older start can never overwrite a record
///   published by a newer overlapping start in the same process.
///
/// The whole read-check-write runs under `progress_publication_lock`, which
/// pre-spawn publication also takes: the ownership check would otherwise be
/// checked-then-stale, letting a newer start publish between this run's check
/// and its write.
///
/// Counters and scope from the failed attempt are preserved so the terminal
/// snapshot still shows what was attempted. A cleanup failure is logged, not
/// propagated: the caller's blocked readiness already carries the primary
/// error.
async fn record_rebuild_failure_progress(
    roots: &McpProjectRoots,
    run_id: &str,
    lock_held: RebuildLockHeld,
    reason: &str,
) {
    // Hold the publication lock across the read, the ownership check, and the
    // write: the check is only meaningful if no publication can land in
    // between, and pre-spawn publication does not take the rebuild lock (see
    // `progress_publication_lock`).
    let _publication_guard = progress_publication_lock().lock().await;
    let Some(progress) = read_index_progress(&roots.state_root).await else {
        return;
    };
    if progress.state != IndexState::Running || progress.indexer_pid != Some(std::process::id()) {
        return;
    }
    let owned = match progress.run_id.as_deref() {
        Some(record_run_id) => record_run_id == run_id,
        None => lock_held == RebuildLockHeld::Yes,
    };
    if !owned {
        return;
    }
    #[cfg(test)]
    maybe_pause_cleanup_for_test(&roots.state_root).await;
    let terminal = IndexProgress {
        state: IndexState::Failed,
        message: Some(format!("indexing failed: {reason}")),
        indexer_pid: None,
        run_id: Some(run_id.to_string()),
        updated_at: chrono::Utc::now(),
        ..progress
    };
    if let Err(err) = write_index_progress_atomic(&roots.state_root, &terminal).await {
        tracing::warn!(
            "failed to record the terminal state of a failed rebuild (status may briefly report a stale indexing run): {err}"
        );
    }
}

async fn run_index(
    roots: &McpProjectRoots,
    rebuild: bool,
    scope_add: Option<Vec<String>>,
    scope_narrow: Option<Vec<String>>,
    run_id: &str,
) -> anyhow::Result<pipeline::PipelineStats> {
    #[cfg(test)]
    maybe_panic_for_test(&roots.state_root);
    if project::read_project_id(&roots.state_root).is_err() {
        project::ensure_project_id_for_auto_init(&roots.state_root)?;
    }

    // Load existing scope and compute the new scope based on scope_add/scope_narrow
    let new_scope = compute_new_scope(&roots.state_root, scope_add, scope_narrow).await?;

    let registry = Registry::load()?;
    let mut indexing_config = config::resolve_indexing_config(
        None,
        None,
        registry.indexing_config_for_context(&roots.worktree_context),
    )?;

    // Apply scope to include_globs for ScanFilter
    apply_scope_to_indexing_config(&mut indexing_config, &new_scope)?;

    // Write initial progress file with scope info BEFORE rebuild lock acquisition.
    // This ensures scope is visible during the rebuilding phase, even if the progress file
    // isn't updated again until the pipeline starts running. (The `ops::start` path
    // also writes this before spawning the rebuild task; this write keeps
    // `run_index` self-contained for any caller.)
    write_initial_scope_progress(roots, &new_scope, run_id).await?;

    let mut setup = SetupTimings::new(Instant::now());

    // Single-writer rebuild lock: hold it across the staged build + atomic
    // switch-over (rebuild) or the prepare + pipeline write (incremental) so a
    // concurrent daemon/CLI rebuild of the shared index cannot race this one, and
    // so the switch-over runs under the lock. Released when this function returns
    // (RAII).
    //
    // `acquire_rebuild_lock` is a blocking, bounded-wait retry (std::thread::sleep
    // on contention). On this async MCP path it would block a tokio worker for up
    // to REBUILD_LOCK_CONTENTION_TIMEOUT_MS on the rare cross-process rebuild race,
    // so the blocking acquire is moved to spawn_blocking. The guard
    // (Flock<File>) is Send and is held in this task across the pipeline write.
    let lock_root = roots.state_root.clone();
    let rebuild_lock =
        tokio::task::spawn_blocking(move || lifecycle::acquire_rebuild_lock(&lock_root)).await??;

    // Write scope to index_status.json BEFORE the pipeline starts,
    // ensuring scope is visible to `oneup_status` during indexing (not dependent
    // on finalize_and_swap completion). This is done by the pipeline's initial
    // progress update via IndexRunContext scope information. The scope is already
    // applied to indexing_config.include_globs above, so the file walk will
    // respect the scope globs.

    let db_start = Instant::now();
    let locked_body = async {
        let stats = if rebuild {
            // Build the refreshed index aside into a staging file and atomically switch
            // it over the served `index.db`, so search keeps serving the prior index
            // (stale-but-available) throughout and is never torn down in place. A
            // failure before the switch drops the guard, leaving the prior index intact.
            let staged = swap::StagingRebuild::open(&roots.state_root).await?;
            setup.db_prepare_ms = db_start.elapsed().as_millis();

            // Write scope to meta table BEFORE pipeline starts
            // so clamp_deletion_on_scope_loss can read it during the pipeline
            schema::write_scope_to_meta(staged.connection(), &new_scope).await?;

            let stats =
                run_index_pipeline(staged.connection(), roots, &indexing_config, setup).await?;
            staged.finalize_and_swap().await?;
            stats
        } else {
            // Incremental write against the live index — unchanged: no rebuild, so no
            // build-aside switch-over is involved.
            let db = Db::open_rw(&config::project_db_path(&roots.state_root)).await?;
            let conn = db.connect_tuned().await?;
            schema::prepare_for_write(&conn).await?;
            setup.db_prepare_ms = db_start.elapsed().as_millis();

            // Write scope to meta table BEFORE pipeline starts
            schema::write_scope_to_meta(&conn, &new_scope).await?;

            run_index_pipeline(&conn, roots, &indexing_config, setup).await?
        };
        anyhow::Ok(stats)
    };

    // Record failures (and panics, converted to errors here) WHILE STILL
    // HOLDING the rebuild lock: the pipeline's own progress writes carry no
    // run identity, and only under the single-writer lock is a `run_id`-less
    // `Running` record with this PID provably this run's — see the ownership
    // rule on `record_rebuild_failure_progress`.
    let result = match futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(
        locked_body,
    ))
    .await
    {
        Ok(result) => result,
        Err(panic) => Err(anyhow::anyhow!(
            "indexing task panicked: {}",
            panic_reason(panic.as_ref())
        )),
    };
    if let Err(err) = &result {
        record_rebuild_failure_progress(roots, run_id, RebuildLockHeld::Yes, &err.to_string())
            .await;
    }
    drop(rebuild_lock);
    result
}

/// Load the embedding model and run the indexing pipeline against `conn`.
///
/// Shared by the build-aside rebuild branch (writing into a staging connection)
/// and the incremental branch (writing into the live index): both load the model,
/// stamp the model-prepare timing onto `setup`, and run a one-shot full pass under
/// a fresh, never-cancelled token (this MCP path is not subject to the daemon's
/// SIGTERM drain).
async fn run_index_pipeline(
    conn: &Connection,
    roots: &McpProjectRoots,
    indexing_config: &IndexingConfig,
    mut setup: SetupTimings,
) -> anyhow::Result<pipeline::PipelineStats> {
    let model_start = Instant::now();
    // MCP `oneup_start` indexing (index-if-missing/index-if-needed/
    // reindex) is an explicit, deliberate retry signal, so clear any prior
    // download-failure marker before the model prepare's `is_download_failed()`
    // guard runs. Passive search (cli/search.rs) never does this, so it stays
    // FTS-only until an explicit index/reindex clears the marker.
    clear_download_failure();
    let mut runtime = EmbeddingRuntime::default();
    runtime
        .prepare_for_indexing_with_progress(indexing_config.embed_threads, false)
        .await?;
    setup.model_prepare_ms = model_start.elapsed().as_millis();

    // Scope is always Full in the MCP path because scope is applied
    // via include_globs in IndexingConfig. The pipeline respects include_globs
    // during the scan, so all code paths (scoped and unscoped) use RunScope::Full
    // with appropriate include_globs set or empty.
    pipeline::run_with_context_scope_setup_and_progress_root(
        conn,
        &roots.worktree_context,
        runtime.current_embedder(),
        &RunScope::Full,
        indexing_config,
        None,
        false,
        Some(setup),
        None,
        Some(&roots.state_root),
        &tokio_util::sync::CancellationToken::new(),
    )
    .await
    .map_err(Into::into)
}

/// On-disk `(device, inode)` identity of an `index.db` file, or `None` when
/// the file is absent or cannot be stat'd. Mirrors
/// `daemon::worker::index_file_identity`: two opens of the same path yield
/// the same identity until an atomic rename swaps a different file over it,
/// which is exactly how a build-aside rebuild installs a refreshed index.
#[cfg(unix)]
type IndexFileIdentity = (u64, u64);
/// Non-Unix fallback identity: `(len, modified)`. There is no stable
/// `(dev, ino)` equivalent on Windows, but a build-aside swap installs a
/// freshly written file, so its length and/or mtime always differ from the
/// generation it replaces — sufficient to force the drop + reopen.
#[cfg(not(unix))]
type IndexFileIdentity = (u64, std::time::SystemTime);

fn index_file_identity(index_path: &Path) -> Option<IndexFileIdentity> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(index_path)
            .ok()
            .map(|meta| (meta.dev(), meta.ino()))
    }
    #[cfg(not(unix))]
    {
        std::fs::metadata(index_path)
            .ok()
            .and_then(|meta| meta.modified().ok().map(|mtime| (meta.len(), mtime)))
    }
}

/// A warm, schema-validated MCP read-index handle kept alive across calls
/// until a build-aside swap changes the on-disk inode.
struct WarmIndex {
    // Kept alive only so `conn` (an `Arc`-backed clone) remains valid for the
    // lifetime of the cache entry; never read directly.
    _db: Db,
    conn: Connection,
    /// Always true for a cache-resident entry: an entry is only ever
    /// inserted after `schema::ensure_current` has already succeeded against
    /// it, so this documents that invariant rather than driving new branches.
    schema_validated: bool,
    identity: Option<IndexFileIdentity>,
    /// Per-context vector `COUNT(*)`, mirroring the daemon's
    /// `ProjectState::cached_vector_count`. Cleared in full whenever this
    /// entry is replaced (a swapped-in index has its own vector population).
    vector_counts: HashMap<String, usize>,
}

/// Process-global warm MCP read-index cache, keyed by canonical
/// `db_path` and mirroring `fallback_embedding_runtime`'s process-global
/// shape.
///
/// The MCP server is a long-lived process serving many tool calls. Without
/// this cache, every one of the six index-reading tools re-opens the
/// database, re-applies the tuned PRAGMA profile, and re-runs
/// `schema::ensure_current` (dozens of `sqlite_master` round-trips) on every
/// single call. `warm_index_connection` stats `db_path` on every call and
/// drops + reopens the entry when the on-disk `(dev,ino)` no longer matches
/// the cached one, so a served connection can never continue serving a
/// superseded generation after a build-aside swap (a held
/// `Connection` is pinned to the inode it opened and keeps serving the
/// pre-swap generation, with no error, until dropped and reopened).
fn warm_index_cache() -> &'static tokio::sync::Mutex<HashMap<PathBuf, WarmIndex>> {
    static CACHE: OnceLock<tokio::sync::Mutex<HashMap<PathBuf, WarmIndex>>> = OnceLock::new();
    CACHE.get_or_init(|| tokio::sync::Mutex::new(HashMap::new()))
}

/// Return a tuned RO connection to `db_path`'s currently-served index,
/// reusing the process-global warm cache entry when the on-disk inode is
/// unchanged.
///
/// On a cache hit (inode match) this is a cheap clone of the already
/// schema-validated connection -- `libsql::Connection` is `Arc`-backed, so
/// the clone shares the underlying connection and prepared-statement cache
/// -- with no re-open and no re-`ensure_current`. On a miss (absent entry, or
/// an inode mismatch caused by a build-aside swap installing a fresh index)
/// the stale entry is dropped, a fresh RO connection is opened and
/// schema-validated once, and the entry (including its per-context
/// vector-count cache) is replaced, so a caller can never observe a pre-swap
/// generation through the cache.
async fn warm_index_connection(
    state_root: &Path,
    db_path: &Path,
    canonical_db_path: &Path,
) -> anyhow::Result<Connection> {
    let current_identity = index_file_identity(canonical_db_path);

    // Fast path: serve a validated warm connection under a short-lived lock.
    {
        let cache = warm_index_cache().lock().await;
        if let Some(warm) = cache.get(canonical_db_path) {
            if warm.identity == current_identity {
                debug_assert!(
                    warm.schema_validated,
                    "a cache-resident warm index must have already passed schema validation"
                );
                return Ok(warm.conn.clone());
            }
        }
    }

    // Miss: open and validate WITHOUT holding the cache lock. Use the tolerant
    // validator so a direct read (search/get/symbol/impact/structural/overview)
    // rides out the daemon's first-index window — tables present, version row not
    // yet committed — instead of failing with "reindex required", mirroring the CLI
    // read paths and the readiness classifier (M2). Validating off-lock means that
    // retry never stalls unrelated readers on the global warm cache, and the async
    // sleep yields the runtime.
    let db = Db::open_ro(db_path).await?;
    let conn = db.connect_tuned().await?;
    schema::ensure_current_tolerating_init(&conn, &schema::SchemaContext::new(db_path, state_root))
        .await?;

    // Publish under the lock. A concurrent miss may have populated the entry in the
    // meantime; overwriting with our freshly validated identity is harmless.
    let served = conn.clone();
    let mut cache = warm_index_cache().lock().await;
    cache.insert(
        canonical_db_path.to_path_buf(),
        WarmIndex {
            _db: db,
            conn,
            schema_validated: true,
            identity: current_identity,
            vector_counts: HashMap::new(),
        },
    );
    Ok(served)
}

/// Return the cached per-context vector count recorded against
/// `canonical_db_path`'s warm cache entry, if any.
async fn cached_vector_count_for_context(
    canonical_db_path: &Path,
    context_id: &str,
) -> Option<usize> {
    let cache = warm_index_cache().lock().await;
    cache
        .get(canonical_db_path)
        .and_then(|warm| warm.vector_counts.get(context_id).copied())
}

/// Record a freshly-computed per-context vector count against
/// `canonical_db_path`'s warm cache entry, so the next search for the same
/// context skips its per-query `COUNT(*)`. A no-op if the entry was
/// concurrently reopened (a rare inode-swap race): the recomputed count
/// belongs to the entry that just replaced this one, not the one being
/// updated here.
async fn record_vector_count_for_context(canonical_db_path: &Path, context_id: &str, count: usize) {
    let mut cache = warm_index_cache().lock().await;
    if let Some(warm) = cache.get_mut(canonical_db_path) {
        warm.vector_counts.insert(context_id.to_string(), count);
    }
}

/// Identity-stamped record of a handle lookup that already failed this session.
/// `outcome` is the terminal failure status (`NotFound` or `Ambiguous`)
/// and `matching_handles` carries the candidate ids from an ambiguous failure
/// so a later rejection can still offer disambiguation without re-querying.
/// `seq` records insertion order for oldest-first eviction.
#[derive(Debug, Clone)]
struct FailedHandleRecord {
    identity: Option<IndexFileIdentity>,
    outcome: ReadStatus,
    matching_handles: Vec<String>,
    seq: u64,
}

/// Memory key: canonical `index.db` path, worktree `context_id`, and the
/// normalized handle. Scoping by the canonical db path and context mirrors the
/// warm cache key and the context-scoped storage reads, so a rejection can
/// never leak across indexes or contexts.
type FailedHandleKey = (PathBuf, String, String);

/// Bounded, insertion-ordered memory of handle lookups that already failed.
/// Distinct from [`warm_index_cache`] because it is keyed per (index,
/// context, handle) rather than per index file and holds no live connection.
/// Every method is synchronous, so the guarding mutex is only ever held for a
/// pure in-memory operation (never across an `await`).
#[derive(Default)]
struct FailedHandleMemory {
    entries: HashMap<FailedHandleKey, FailedHandleRecord>,
    next_seq: u64,
}

impl FailedHandleMemory {
    /// Decide a handle's fate against recorded history. A recorded failure
    /// whose stamped identity still matches the current on-disk index is
    /// returned so the caller can reject the identical retry without
    /// re-querying; an identity mismatch (a build-aside swap installed a fresh
    /// index) drops the now-stale entry and returns `None` so the handle
    /// resolves fresh; an absent entry also returns `None`.
    fn lookup(
        &mut self,
        key: &FailedHandleKey,
        current_identity: Option<IndexFileIdentity>,
    ) -> Option<FailedHandleRecord> {
        match self.entries.get(key) {
            Some(record) if record.identity == current_identity => Some(record.clone()),
            Some(_) => {
                self.entries.remove(key);
                None
            }
            None => None,
        }
    }

    /// Record a terminal failure outcome, stamping it with the current index
    /// identity and the next insertion sequence, then evict the oldest entry
    /// while over the cap.
    fn record_failure(
        &mut self,
        key: FailedHandleKey,
        identity: Option<IndexFileIdentity>,
        outcome: ReadStatus,
        matching_handles: Vec<String>,
    ) {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.entries.insert(
            key,
            FailedHandleRecord {
                identity,
                outcome,
                matching_handles,
                seq,
            },
        );
        while self.entries.len() > FAILED_HANDLE_MEMORY_CAP {
            if let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, record)| record.seq)
                .map(|(oldest_key, _)| oldest_key.clone())
            {
                self.entries.remove(&oldest);
            } else {
                break;
            }
        }
    }

    /// Forget any recorded failure for a handle: a fresh success supersedes
    /// a prior failure, so a later identical call is no longer rejected.
    fn clear(&mut self, key: &FailedHandleKey) {
        self.entries.remove(key);
    }
}

/// Process-global failed-handle retry memory, mirroring the process-global
/// shape of [`warm_index_cache`]. Guards a purely in-memory map, so a plain
/// `std::sync::Mutex` suffices: the lock is never held across an `await`.
fn failed_handle_memory() -> &'static Mutex<FailedHandleMemory> {
    static MEMORY: OnceLock<Mutex<FailedHandleMemory>> = OnceLock::new();
    MEMORY.get_or_init(|| Mutex::new(FailedHandleMemory::default()))
}

async fn open_current_index(state_root: &Path) -> anyhow::Result<CurrentIndex> {
    let db_path = project_db_path(state_root);
    if !db_path.exists() {
        bail!(
            "no current index found at {}; call oneup_start with an explicit indexing mode",
            db_path.display()
        );
    }
    let canonical_db_path = db_path
        .canonicalize()
        .with_context(|| format!("failed to resolve current index path {}", db_path.display()))?;

    let conn = warm_index_connection(state_root, &db_path, &canonical_db_path).await?;
    Ok(CurrentIndex {
        conn,
        db_path: canonical_db_path,
    })
}

/// Resolve every handle in `handles` to a [`ReadRecord`], preserving input
/// order. The exact-id pass is collapsed into a single `id IN (...)` batch
/// lookup; only the residual handles that did not exact-match (12-char
/// display handles and genuine misses) fall back to the per-handle prefix
/// lookup. Each handle is resolved independently, so the per-handle
/// Found/NotFound/Ambiguous outcome and the empty-handle rejection are identical
/// to resolving handles one at a time.
async fn resolve_handle_records(
    conn: &Connection,
    context_id: &str,
    handles: &[String],
    verbosity: Option<&str>,
) -> anyhow::Result<Vec<ReadRecord>> {
    let normalized: Vec<String> = handles
        .iter()
        .map(|handle| normalize_handle(handle))
        .collect();

    // One batched exact-id fetch for the non-empty normalized handles. An id with
    // no row simply misses the map and falls through to the prefix residual
    // below — the same per-handle path as resolving exact-then-prefix one at a
    // time. Duplicate ids are harmless (the map keys on id).
    let exact_ids: Vec<String> = normalized
        .iter()
        .filter(|id| !id.is_empty())
        .cloned()
        .collect();
    let segments_by_id = get_segments_by_ids_for_context(conn, context_id, &exact_ids).await?;

    let mut records = Vec::with_capacity(handles.len());
    for (raw_handle, normalized) in handles.iter().zip(normalized) {
        let source = ReadSource::Handle {
            raw: raw_handle.clone(),
            normalized: normalized.clone(),
        };

        if normalized.is_empty() {
            records.push(read_message(
                ReadStatus::NotFound,
                source,
                "empty segment handle",
            ));
        } else if let Some(segment) = segments_by_id.get(&normalized) {
            records.push(read_segment(source, segment.clone(), verbosity));
        } else {
            let record = match resolve_handle_via_prefix(
                conn,
                context_id,
                source.clone(),
                &normalized,
                verbosity,
            )
            .await
            {
                Ok(record) => record,
                Err(err) => isolate_residual_resolution_error(source, err)?,
            };
            records.push(record);
        }
    }

    Ok(records)
}

/// Pure classification gate for a residual per-handle resolution error. A lock
/// error is returned as `Err` so it `?`-propagates and `retry_on_db_lock`
/// retries the whole call; any other error is isolated to a single
/// `ReadStatus::Error` record carrying the error text, so one handle's failure
/// never aborts the batch. Index-level failures (the batched exact-id fetch,
/// `open_current_index`) stay whole-call failures and never reach here.
fn isolate_residual_resolution_error(
    source: ReadSource,
    err: anyhow::Error,
) -> anyhow::Result<ReadRecord> {
    if is_lock_error(&err.to_string()) {
        return Err(err);
    }
    Ok(read_message(ReadStatus::Error, source, err.to_string()))
}

/// Residual prefix resolution for a handle that did not match an exact id:
/// byte-identical to the prefix branch of the per-handle path, distinguishing
/// unique matches from ambiguous prefixes via [`SegmentPrefixLookup`].
async fn resolve_handle_via_prefix(
    conn: &Connection,
    context_id: &str,
    source: ReadSource,
    normalized: &str,
    verbosity: Option<&str>,
) -> anyhow::Result<ReadRecord> {
    Ok(
        match get_segment_by_prefix_for_context(conn, context_id, normalized).await? {
            SegmentPrefixLookup::Found(segment) => read_segment(source, *segment, verbosity),
            SegmentPrefixLookup::NotFound => {
                attempt_handle_recovery(conn, context_id, source, normalized, verbosity).await?
            }
            SegmentPrefixLookup::Ambiguous(ids) => ReadRecord {
                status: ReadStatus::Ambiguous,
                source,
                segment: None,
                context: None,
                matching_handles: ids,
                recovered_from: None,
                message: Some("segment handle matched multiple indexed segments".to_string()),
            },
        },
    )
}

/// Outcome of the pure unique-prefix recovery gate. `candidates` are the
/// ids sharing the floor prefix; the gate itself never issues a query.
#[derive(Debug, Clone, PartialEq, Eq)]
enum HandleRecovery {
    /// Exactly one candidate is uniquely closest at the longest matching prefix
    /// (>= floor); recovery resolves to this id.
    Found(String),
    /// Two or more candidates tie at the longest matching prefix; recovery
    /// declines to guess and surfaces the tied ids for disambiguation.
    Ambiguous(Vec<String>),
    /// No candidate reaches the floor prefix; there is nothing to recover.
    None,
}

/// Recovery path taken when a handle matched no segment exactly or by prefix.
/// Fetches the context-scoped candidate ids sharing the floor prefix
/// (`supplied[..MIN_HANDLE_RECOVERY_PREFIX_CHARS]`, bounded by
/// [`HANDLE_RECOVERY_CANDIDATE_LIMIT`]) and runs the pure recovery gate. A
/// unique longest-prefix candidate is re-fetched by its exact id and returned
/// as a `Found` record disclosing `recovered_from`; a tie yields an explicit
/// `Ambiguous` record; anything else stays `NotFound`. Context scoping is
/// inherited from the storage query, so a foreign-context handle can never be
/// recovered. A candidate fetch that saturates the limit means
/// the floor prefix is too broad to discriminate, so recovery declines.
async fn attempt_handle_recovery(
    conn: &Connection,
    context_id: &str,
    source: ReadSource,
    normalized: &str,
    verbosity: Option<&str>,
) -> anyhow::Result<ReadRecord> {
    const NOT_FOUND_MESSAGE: &str = "segment handle was not found";

    // Below the floor there are not enough characters to discriminate, so a
    // floor prefix cannot even be formed; decline without querying.
    let floor_prefix: String = normalized
        .chars()
        .take(MIN_HANDLE_RECOVERY_PREFIX_CHARS)
        .collect();
    if floor_prefix.chars().count() < MIN_HANDLE_RECOVERY_PREFIX_CHARS {
        return Ok(read_message(
            ReadStatus::NotFound,
            source,
            NOT_FOUND_MESSAGE,
        ));
    }

    let candidates = get_segment_ids_by_prefix_for_context(
        conn,
        context_id,
        &floor_prefix,
        HANDLE_RECOVERY_CANDIDATE_LIMIT,
    )
    .await?;

    // A saturated fetch means the floor prefix matches too many segments to
    // treat any single one as a unique recovery target.
    if candidates.len() >= HANDLE_RECOVERY_CANDIDATE_LIMIT {
        return Ok(read_message(
            ReadStatus::NotFound,
            source,
            NOT_FOUND_MESSAGE,
        ));
    }

    match recover_handle_by_unique_prefix(normalized, &candidates) {
        HandleRecovery::Found(id) => {
            match get_segment_by_id_for_context(conn, context_id, &id).await? {
                Some(segment) => Ok(read_recovered_segment(
                    source,
                    segment,
                    verbosity,
                    normalized.to_string(),
                )),
                // The candidate id vanished between the id fetch and the
                // re-fetch (e.g. a concurrent rebuild); stay truthful.
                None => Ok(read_message(
                    ReadStatus::NotFound,
                    source,
                    NOT_FOUND_MESSAGE,
                )),
            }
        }
        HandleRecovery::Ambiguous(ids) => Ok(ReadRecord {
            status: ReadStatus::Ambiguous,
            source,
            segment: None,
            context: None,
            matching_handles: ids,
            recovered_from: None,
            message: Some(
                "segment handle prefix matched multiple indexed segments in the active context"
                    .to_string(),
            ),
        }),
        HandleRecovery::None => Ok(read_message(
            ReadStatus::NotFound,
            source,
            NOT_FOUND_MESSAGE,
        )),
    }
}

/// Pure longest-common-prefix recovery gate. Walks prefix lengths
/// from the full supplied handle down to [`MIN_HANDLE_RECOVERY_PREFIX_CHARS`];
/// the first (longest) length at which any candidate matches decides the
/// outcome, so a lone match recovers (`Found`) and a tie declines with the tied
/// ids (`Ambiguous`). Candidates are assumed to already share the floor prefix,
/// so the walk terminates at the floor unless the set is empty. Never
/// fuzzy-matches: only shared leading characters count.
fn recover_handle_by_unique_prefix(supplied: &str, candidates: &[String]) -> HandleRecovery {
    let supplied_chars: Vec<char> = supplied.chars().collect();
    if supplied_chars.len() < MIN_HANDLE_RECOVERY_PREFIX_CHARS || candidates.is_empty() {
        return HandleRecovery::None;
    }

    for len in (MIN_HANDLE_RECOVERY_PREFIX_CHARS..=supplied_chars.len()).rev() {
        let prefix: String = supplied_chars[..len].iter().collect();
        let matches: Vec<String> = candidates
            .iter()
            .filter(|id| id.starts_with(&prefix))
            .cloned()
            .collect();
        match matches.len() {
            0 => continue,
            1 => return HandleRecovery::Found(matches.into_iter().next().unwrap()),
            _ => return HandleRecovery::Ambiguous(matches),
        }
    }

    HandleRecovery::None
}

fn read_location_record(
    source_root: &Path,
    scan_filter: &ScanFilter,
    location: &ReadLocation,
    scope_roots: &[String],
) -> ReadRecord {
    let source = ReadSource::Location {
        path: location.path.clone(),
        line: location.line,
    };

    if location.line == 0 {
        return read_message(
            ReadStatus::Rejected,
            source,
            "line must be 1-based for file-line context retrieval",
        );
    }

    let file_path = match resolve_location_path(source_root, &location.path) {
        Ok(path) => path,
        Err(LocationError::Rejected(message)) => {
            return read_message(ReadStatus::Rejected, source, message);
        }
        Err(LocationError::Error(message)) => {
            return read_message(ReadStatus::Error, source, message);
        }
    };

    // `resolve_location_path` guarantees `file_path` (canonical) starts with
    // `source_root` (also canonical), so `strip_prefix` always succeeds here.
    // Still handled explicitly rather than falling back to the absolute path,
    // which would wrongly trip `ScanFilter`'s dotfile check on any dot-prefixed
    // ancestor directory outside the repository (contract: `is_excluded` takes
    // a repo-relative path, never an absolute one).
    let rel_path = match file_path.strip_prefix(source_root) {
        Ok(rel_path) => rel_path,
        Err(_) => {
            return read_message(
                ReadStatus::Error,
                source,
                "failed to resolve repository-relative path for exclusion check",
            );
        }
    };
    if scan_filter.is_excluded(rel_path, false) {
        return read_message(
            ReadStatus::Rejected,
            source,
            "path is excluded from indexing and is not served via context",
        );
    }

    // Check if file is within indexed scope
    let is_in_scope = check_path_in_scope(rel_path, scope_roots);
    let out_of_scope_disclosure = if !is_in_scope && !scope_roots.is_empty() {
        Some(format_out_of_scope_disclosure(scope_roots))
    } else {
        None
    };

    match ContextEngine::retrieve_scope_window(&file_path, location.line, location.expansion) {
        Ok(window) => read_context(
            source,
            source_root,
            &file_path,
            location.line,
            window,
            out_of_scope_disclosure,
        ),
        Err(err) => read_message(ReadStatus::Error, source, err.to_string()),
    }
}

fn check_path_in_scope(rel_path: &Path, scope_roots: &[String]) -> bool {
    if scope_roots.is_empty() {
        return true;
    }

    let rel_path_str = rel_path.to_string_lossy();
    scope_roots.iter().any(|root| {
        rel_path_str.starts_with(root)
            && (rel_path_str.len() == root.len()
                || rel_path_str.chars().nth(root.len()) == Some('/'))
    })
}

fn format_out_of_scope_disclosure(scope_roots: &[String]) -> String {
    let dirs = scope_roots.join(", ");
    format!("This path is outside indexed scope [{}]; content read from file system only. Expand scope to index this file.", dirs)
}

fn resolve_location_path(source_root: &Path, raw_path: &str) -> Result<PathBuf, LocationError> {
    let raw = Path::new(raw_path);
    let candidate = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        join_repo_relative(source_root, raw)?
    };

    if raw.is_absolute() && !candidate.starts_with(source_root) {
        return Err(LocationError::Rejected(
            "path is outside the configured repository".to_string(),
        ));
    }

    let canonical = candidate
        .canonicalize()
        .map_err(|err| LocationError::Error(err.to_string()))?;

    if !canonical.starts_with(source_root) {
        return Err(LocationError::Rejected(
            "path is outside the configured repository".to_string(),
        ));
    }

    Ok(canonical)
}

fn join_repo_relative(source_root: &Path, raw: &Path) -> Result<PathBuf, LocationError> {
    let mut candidate = source_root.to_path_buf();

    for component in raw.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => candidate.push(part),
            Component::ParentDir => {
                candidate.pop();
                if !candidate.starts_with(source_root) {
                    return Err(LocationError::Rejected(
                        "path is outside the configured repository".to_string(),
                    ));
                }
            }
            Component::Prefix(_) | Component::RootDir => {
                return Err(LocationError::Rejected(
                    "path must be relative to the configured repository".to_string(),
                ));
            }
        }
    }

    Ok(candidate)
}

fn read_segment(source: ReadSource, segment: StoredSegment, verbosity: Option<&str>) -> ReadRecord {
    ReadRecord {
        status: ReadStatus::Found,
        source,
        segment: Some(segment_record(segment, verbosity)),
        context: None,
        matching_handles: Vec::new(),
        recovered_from: None,
        message: None,
    }
}

/// Single-source disclosure attached to a record recovered via the unique-prefix
/// gate, stating plainly that the supplied handle did not resolve directly.
const HANDLE_RECOVERY_MESSAGE: &str =
    "segment handle did not resolve exactly or by prefix; recovered via its unique canonical prefix within the active context";

/// `Found` record for a segment resolved through unique-prefix recovery: a
/// normal segment read with the additive `recovered_from` disclosure and a
/// message naming the recovery.
fn read_recovered_segment(
    source: ReadSource,
    segment: StoredSegment,
    verbosity: Option<&str>,
    recovered_from: String,
) -> ReadRecord {
    ReadRecord {
        recovered_from: Some(recovered_from),
        message: Some(HANDLE_RECOVERY_MESSAGE.to_string()),
        ..read_segment(source, segment, verbosity)
    }
}

fn read_context(
    source: ReadSource,
    source_root: &Path,
    file_path: &Path,
    target_line: usize,
    window: ScopeWindow,
    out_of_scope_disclosure: Option<String>,
) -> ReadRecord {
    let path = file_path
        .strip_prefix(source_root)
        .map(relative_path_string)
        .unwrap_or_else(|_| file_path.display().to_string());

    // The `ScopeWindow` contract omits language, so derive it from the file
    // extension here (mirrors the context engine's own extension mapping).
    let language = language_for_path(file_path);

    // Load-bearing truncation note: only when the returned window is a
    // strict subset of the enclosing scope. The recovery re-issues oneup_context
    // at the ORIGINAL target line so the same smallest enclosing scope re-resolves
    // (a midpoint retarget could land in a nested scope), with an expansion large
    // enough to reach the farthest scope edge, clamped to the ceiling.
    let truncation = window.clipped.then(|| {
        let omitted_above = window.line_start.saturating_sub(window.scope_line_start);
        let omitted_below = window.scope_line_end.saturating_sub(window.line_end);
        let reach = target_line
            .saturating_sub(window.scope_line_start)
            .max(window.scope_line_end.saturating_sub(target_line));
        let expansion = reach.min(MAX_CONTEXT_EXPANSION_LINES);
        TruncationNote {
            reason: SCOPE_TRUNCATION_REASON.to_string(),
            scope_name: window.scope_name.clone(),
            scope_type: Some(window.scope_type.clone()),
            full_line_start: Some(window.scope_line_start),
            full_line_end: Some(window.scope_line_end),
            omitted_above: Some(omitted_above),
            omitted_below: Some(omitted_below),
            omitted_symbols: None,
            recovery: RecoveryCall {
                tool: TOOL_CONTEXT.to_string(),
                arguments: serde_json::json!({
                    "locations": [{
                        "path": path.clone(),
                        "line": target_line,
                        "expansion": expansion,
                    }]
                }),
            },
        }
    });

    ReadRecord {
        status: ReadStatus::Found,
        source,
        segment: None,
        context: Some(ContextRecord {
            path,
            language,
            scope_type: window.scope_type,
            content: window.content,
            line_start: window.line_start,
            line_end: window.line_end,
            out_of_scope_disclosure,
            truncation,
        }),
        matching_handles: Vec::new(),
        recovered_from: None,
        message: None,
    }
}

/// Best-effort display language for a context record, derived from the file
/// extension: the supported-language name, else the raw extension, else
/// `unknown` for extensionless files.
fn language_for_path(file_path: &Path) -> String {
    let ext = file_path.extension().and_then(|e| e.to_str()).unwrap_or("");
    match SupportedLanguage::from_extension(ext) {
        Some(lang) => lang.name().to_string(),
        None if ext.is_empty() => "unknown".to_string(),
        None => ext.to_string(),
    }
}

/// `Rejected` record for an identical failed handle retry. The message is
/// built from the remembered `outcome` so it names the original cause
/// truthfully (an ambiguous prefix vs a plain not-found) rather than
/// re-querying to rediscover it. Carries the candidate ids cached from an
/// ambiguous failure (empty when the original failure was a plain not-found) so
/// the follow-up next_actions can still offer disambiguation.
fn read_rejected_handle(
    source: ReadSource,
    outcome: ReadStatus,
    matching_handles: Vec<String>,
) -> ReadRecord {
    let cause = match outcome {
        ReadStatus::Ambiguous => "matched multiple indexed segments",
        _ => "was not found",
    };
    let message = format!(
        "this segment handle already {cause} in the active context this session; repeating the identical call was rejected without re-querying"
    );
    ReadRecord {
        status: ReadStatus::Rejected,
        source,
        segment: None,
        context: None,
        matching_handles,
        recovered_from: None,
        message: Some(message),
    }
}

fn read_message(status: ReadStatus, source: ReadSource, message: impl Into<String>) -> ReadRecord {
    ReadRecord {
        status,
        source,
        segment: None,
        context: None,
        matching_handles: Vec::new(),
        recovered_from: None,
        message: Some(message.into()),
    }
}

fn segment_record(segment: StoredSegment, verbosity: Option<&str>) -> SegmentRecord {
    let role = segment.parsed_role();
    let is_verbose = verbosity.map(|v| v == "full").unwrap_or(false);

    // Resolve the full symbol lists once. Counts and the recovery name are taken
    // from these before any verbosity gating so next_actions and symbol_counts
    // never lose the oneup_symbol follow-up.
    let all_defined = segment.parsed_defined_symbols();
    let all_referenced = segment.parsed_referenced_symbols();
    let all_called = segment.parsed_called_symbols();
    let symbol_hint = all_defined.first().cloned();

    let counts = SymbolCounts {
        defined: all_defined.len(),
        referenced: all_referenced.len(),
        called: all_called.len(),
    };

    let (defined_symbols, referenced_symbols, called_symbols, symbol_counts, truncation) =
        if is_verbose {
            // Full verbosity: emit the lists but cap each at
            // MAX_SYMBOLS_PER_LIST. When any list overflows, attach a
            // load-bearing truncation note with the total omitted
            // count and a ready-to-issue oneup_symbol recovery targeting a
            // symbol this segment actually carries (prefer the defining symbol).
            let omitted = counts.defined.saturating_sub(MAX_SYMBOLS_PER_LIST)
                + counts.referenced.saturating_sub(MAX_SYMBOLS_PER_LIST)
                + counts.called.saturating_sub(MAX_SYMBOLS_PER_LIST);
            let recovery_name = all_defined
                .first()
                .or_else(|| all_referenced.first())
                .or_else(|| all_called.first())
                .cloned();
            let truncation = (omitted > 0)
                .then_some(recovery_name)
                .flatten()
                .map(|name| TruncationNote {
                    reason: SYMBOL_LIST_TRUNCATION_REASON.to_string(),
                    scope_name: None,
                    scope_type: None,
                    full_line_start: None,
                    full_line_end: None,
                    omitted_above: None,
                    omitted_below: None,
                    omitted_symbols: Some(omitted),
                    recovery: RecoveryCall {
                        tool: TOOL_SYMBOL.to_string(),
                        arguments: serde_json::json!({
                            "name": name,
                            "include": "both",
                            "fuzzy": true,
                        }),
                    },
                });
            (
                cap_symbols(all_defined),
                cap_symbols(all_referenced),
                cap_symbols(all_called),
                None,
                truncation,
            )
        } else {
            // Default verbosity: omit the lists but make the omission
            // explicit with constant-size counts when any list is non-empty; the
            // symbol_hint / oneup_symbol next_action remains the recovery path.
            let symbol_counts =
                (counts.defined + counts.referenced + counts.called > 0).then_some(counts);
            (Vec::new(), Vec::new(), Vec::new(), symbol_counts, None)
        };

    SegmentRecord {
        handle: segment.id,
        path: segment.file_path,
        language: segment.language,
        kind: segment.block_type,
        content: segment.content,
        line_start: usize_from_i64(segment.line_start),
        line_end: usize_from_i64(segment.line_end),
        breadcrumb: segment.breadcrumb,
        role,
        summary: None,
        defined_symbols,
        referenced_symbols,
        called_symbols,
        symbol_counts,
        truncation,
        symbol_hint,
    }
}

/// Cap a symbol list at [`MAX_SYMBOLS_PER_LIST`], preserving order.
fn cap_symbols(mut symbols: Vec<String>) -> Vec<String> {
    symbols.truncate(MAX_SYMBOLS_PER_LIST);
    symbols
}

fn search_hit(result: SearchResult) -> SearchHit {
    let defined_symbols = result.defined_symbols.unwrap_or_default();
    let symbol = defined_symbols.first().cloned();

    SearchHit {
        handle: result.segment_id,
        path: result.file_path,
        language: result.language,
        kind: result.block_type,
        score: result.score,
        line_start: result.line_number,
        line_end: result.line_end,
        breadcrumb: result.breadcrumb,
        symbol,
        defined_symbols,
    }
}

fn symbol_record(result: SymbolResult) -> SymbolRecord {
    SymbolRecord {
        handle: result.segment_id,
        name: result.name,
        reference_kind: result.reference_kind,
        kind: result.kind,
        path: result.file_path,
        language: result.language,
        line_start: result.line_start,
        line_end: result.line_end,
        breadcrumb: result.breadcrumb,
    }
}

fn overview_payload(digest: overview::RepositoryOverview) -> OverviewPayload {
    // A ready index with zero segments is a valid empty digest, not an
    // error; missing/unready indexes fail in open_current_index.
    let status = if digest.stats.total_segments == 0 {
        OperationStatus::Empty
    } else {
        OperationStatus::Ok
    };

    OverviewPayload {
        status,
        stats: OverviewStats {
            indexed_files: digest.stats.indexed_files,
            total_segments: digest.stats.total_segments,
            languages: digest
                .stats
                .languages
                .into_iter()
                .map(overview_language)
                .collect(),
        },
        top_symbols: digest
            .top_symbols
            .into_iter()
            .map(overview_top_symbol)
            .collect(),
        modules: digest.modules.into_iter().map(overview_module).collect(),
        module_dependencies: digest
            .module_dependencies
            .into_iter()
            .map(overview_module_dependency)
            .collect(),
        entry_points: digest
            .entry_points
            .into_iter()
            .map(overview_entry_point)
            .collect(),
    }
}

fn overview_language(stat: overview::LanguageBreakdown) -> OverviewLanguage {
    OverviewLanguage {
        language: stat.language,
        files: stat.files,
        segments: stat.segments,
    }
}

fn overview_top_symbol(entry: overview::TopSymbolEntry) -> OverviewTopSymbol {
    OverviewTopSymbol {
        name: entry.name,
        handle: entry.handle,
        path: entry.path,
        line_start: usize_from_i64(entry.line_start),
        line_end: usize_from_i64(entry.line_end),
        referencing_files: entry.referencing_files,
        definition_count: entry.definition_count,
    }
}

fn overview_module(entry: overview::ModuleEntry) -> OverviewModule {
    OverviewModule {
        module: entry.module,
        segments: entry.segments,
    }
}

fn overview_module_dependency(entry: overview::ModuleDependencyEntry) -> OverviewModuleDependency {
    OverviewModuleDependency {
        source: entry.source,
        target: entry.target,
        count: entry.count,
    }
}

fn overview_entry_point(entry: overview::EntryPointEntry) -> OverviewEntryPoint {
    OverviewEntryPoint {
        handle: entry.handle,
        path: entry.path,
        line_start: usize_from_i64(entry.line_start),
        line_end: usize_from_i64(entry.line_end),
        role: entry.role,
        symbol: entry.symbol,
        breadcrumb: entry.breadcrumb,
    }
}

fn aggregate_read_status(records: &[ReadRecord]) -> OperationStatus {
    if records.is_empty() {
        return OperationStatus::Empty;
    }

    if records
        .iter()
        .all(|record| record.status == ReadStatus::Found)
    {
        OperationStatus::Ok
    } else if records
        .iter()
        .any(|record| record.status == ReadStatus::Found)
    {
        OperationStatus::Partial
    } else {
        OperationStatus::Empty
    }
}

fn normalize_handle(raw: &str) -> String {
    raw.strip_prefix(':').unwrap_or(raw).to_string()
}

fn partition_symbol_results(results: Vec<SymbolResult>) -> (Vec<SymbolResult>, Vec<SymbolResult>) {
    results
        .into_iter()
        .partition(|result| result.reference_kind == ReferenceKind::Definition)
}

fn only_references(results: Vec<SymbolResult>) -> Vec<SymbolResult> {
    results
        .into_iter()
        .filter(|result| result.reference_kind == ReferenceKind::Usage)
        .collect()
}

/// Read the index-progress status file for MCP readiness classification.
///
/// Retry-or-propagate policy for the MCP readiness call-site class: an `Absent`
/// file resolves to `None` (no index recorded yet). An `Unreadable` (torn or
/// corrupt) file is retried up to [`STATUS_READ_RETRY_ATTEMPTS`] times with an
/// async [`STATUS_READ_RETRY_DELAY_MS`] `tokio::time::sleep` (async so a rare
/// corrupt-file retry never blocks a tokio worker); if it is still unparseable
/// we `tracing::error!` (visible at default verbosity) and return `None` so
/// readiness degrades to its
/// indeterminate/blocked classification rather than confidently reporting "no
/// index" from a corrupt file. `None` is never treated as valid empty progress.
async fn read_index_progress(project_root: &Path) -> Option<IndexProgress> {
    let path = project_dot_dir(project_root).join(INDEX_PROGRESS_FILE_NAME);
    for attempt in 1..=STATUS_READ_RETRY_ATTEMPTS {
        match read_status_file::<IndexProgress>(&path) {
            StatusFileRead::Absent => return None,
            StatusFileRead::Parsed(progress) => {
                // Check if the progress file is stale (Running state, dead process, age > 5 min).
                // Treat stale progress as if no index exists, so agents don't poll indefinitely.
                if is_index_progress_stale(&progress, &path) {
                    return None;
                }
                return Some(progress);
            }
            StatusFileRead::Unreadable(err) => {
                if attempt == STATUS_READ_RETRY_ATTEMPTS {
                    tracing::error!(
                        "index_status.json at {} is unreadable after {STATUS_READ_RETRY_ATTEMPTS} attempts ({err}); treating readiness as indeterminate, not \"no index\"",
                        path.display(),
                    );
                    return None;
                }
                tokio::time::sleep(Duration::from_millis(STATUS_READ_RETRY_DELAY_MS)).await;
            }
        }
    }
    None
}

async fn read_index_progress_for_context(
    project_root: &Path,
    context_id: &str,
) -> Option<IndexProgress> {
    read_index_progress(project_root).await.filter(|progress| {
        progress
            .context_id
            .as_deref()
            .is_none_or(|progress_context_id| progress_context_id == context_id)
    })
}

/// Extract scope info from progress file during indexing.
/// Scope should be visible during indexing, independent of swap completion.
fn extract_scope_from_progress(progress: &IndexProgress) -> Option<IndexScope> {
    // Extract scope from progress during indexing.
    // The scope roots come from IndexScopeInfo recorded during pipeline startup.
    // Return IndexScope as soon as scope_info is available, even if files_total is 0,
    // so scope is visible from the start of indexing (not just during scanning).
    progress.scope.as_ref().map(|scope_info| {
        IndexScope {
            // Use roots from scope_info (recorded during pipeline startup)
            roots: scope_info.roots.clone(),
            indexed_files: progress.files_indexed,
            total_files: progress.files_total,
            eligibility_note: None,
        }
    })
}

/// Check if an index progress file is stale.
/// A progress file is stale if it claims Running state but the owning process (indexer_pid)
/// is dead AND the file is older than STALENESS_THRESHOLD_SECS (5 minutes).
/// This prevents agents from indefinitely polling a stuck indexing state.
fn is_index_progress_stale(progress: &IndexProgress, status_path: &Path) -> bool {
    use crate::shared::constants::STALENESS_THRESHOLD_SECS;
    use std::fs;

    // Only consider Running state as potentially stale
    if progress.state != IndexState::Running {
        return false;
    }

    // Must have a PID to check liveness
    let Some(pid) = progress.indexer_pid else {
        return false;
    };

    // Process is still alive: not stale
    if lifecycle::is_process_alive(pid) {
        return false;
    }

    // Process is dead. Check file age against staleness threshold.
    let Ok(metadata) = fs::metadata(status_path) else {
        return false;
    };

    let Ok(modified) = metadata.modified() else {
        return false;
    };

    let Ok(elapsed) = SystemTime::now().duration_since(modified) else {
        return false;
    };

    elapsed.as_secs() > STALENESS_THRESHOLD_SECS
}

fn embedding_unavailable_reason(status: &EmbeddingLoadStatus) -> Option<String> {
    match status {
        EmbeddingLoadStatus::Warm
        | EmbeddingLoadStatus::Loaded
        | EmbeddingLoadStatus::Downloaded => None,
        EmbeddingLoadStatus::Unavailable(reason) => Some(unavailable_reason_text(reason)),
    }
}

fn unavailable_reason_text(reason: &EmbeddingUnavailableReason) -> String {
    match reason {
        EmbeddingUnavailableReason::ModelMissing => "embedding model is missing".to_string(),
        EmbeddingUnavailableReason::PreviousDownloadFailed => {
            "embedding model download previously failed".to_string()
        }
        EmbeddingUnavailableReason::ModelDirUnavailable(err) => {
            format!("embedding model directory is unavailable: {err}")
        }
        EmbeddingUnavailableReason::LoadFailed(err) => {
            format!("embedding model failed to load: {err}")
        }
        EmbeddingUnavailableReason::DownloadFailed(err) => {
            format!("embedding model download failed: {err}")
        }
        EmbeddingUnavailableReason::ArtifactsUnverifiable(err) => {
            format!("embedding model artifacts failed verification: {err}")
        }
    }
}

fn usize_from_i64(value: i64) -> usize {
    usize::try_from(value).unwrap_or_default()
}

/// Renders an absolute root path exactly as the OS reports it. Replacing
/// separators here corrupts Windows extended-length prefixes
/// (`\\?\C:\...` would become the invalid `//?/C:/...`).
fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Normalizes a repo-relative path to forward slashes so payload paths match
/// the `/`-separated relative paths stored in the index.
fn relative_path_string(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn is_not_initialized(err: &OneupError) -> bool {
    matches!(err, OneupError::Project(ProjectError::NotInitialized))
}

enum LocationError {
    Rejected(String),
    Error(String),
}

// Cache key for directory walk results, invalidating on repo identity change or HEAD drift.
#[derive(Clone, Eq, PartialEq, Hash, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
struct DirectoryWalkCacheKey {
    repo_identity: String,       // e.g., source_root canonical path
    head_commit: Option<String>, // git HEAD OID, or None if not in a git repo
    root_mtime: Option<u64>,     // filesystem mtime in seconds, or None on error
}

// Static in-process cache for directory walk results.
// Also persists to disk for cross-process reuse (cold-walk latency fix).
static DIRECTORY_WALK_CACHE: OnceLock<
    Mutex<HashMap<DirectoryWalkCacheKey, BTreeMap<String, usize>>>,
> = OnceLock::new();

fn get_directory_walk_cache(
) -> &'static Mutex<HashMap<DirectoryWalkCacheKey, BTreeMap<String, usize>>> {
    DIRECTORY_WALK_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// F5/N4b: Persistent on-disk cache for directory walk results.
/// Stored as JSON in .1up directory, keyed by (repo_id, HEAD, mtime).
/// Survives process restart to avoid re-walking on fresh-process envelope/search calls.
#[derive(Debug, Serialize, serde::Deserialize)]
struct PersistentDirectoryWalkCache {
    entries: HashMap<String, BTreeMap<String, usize>>, // JSON-serializable cache
}

/// F5/N4b: Generate a stable cache key string for persistent storage.
/// Must be deterministic and human-readable for debugging.
fn cache_key_to_string(key: &DirectoryWalkCacheKey) -> String {
    let head = key.head_commit.as_deref().unwrap_or("no_git");
    let mtime = key
        .root_mtime
        .map(|t| t.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    format!("{}_{}_{}", key.repo_identity, head, mtime)
}

/// F5/N4b: Load directory walk cache from persistent storage in .1up directory.
/// Returns empty map on any error (missing file, parse error, etc.); errors are
/// non-fatal since the cache is an optimization.
fn load_persistent_directory_walk_cache(
    state_root: &Path,
) -> HashMap<String, BTreeMap<String, usize>> {
    let cache_path = project_dot_dir(state_root).join("directory_walk_cache.json");
    match std::fs::read_to_string(&cache_path) {
        Ok(content) => match serde_json::from_str::<PersistentDirectoryWalkCache>(&content) {
            Ok(cached) => cached.entries,
            Err(_) => HashMap::new(),
        },
        Err(_) => HashMap::new(),
    }
}

/// F5/N4b: Persist directory walk cache to disk.
/// Stores in .1up directory; errors are logged but don't interrupt operation.
fn save_persistent_directory_walk_cache(
    state_root: &Path,
    entries: &HashMap<String, BTreeMap<String, usize>>,
) {
    let dot_dir = project_dot_dir(state_root);
    // Only write cache if .1up already exists. Never create .1up during blocked/failed
    // indexing attempts (blocked path must leave NO .1up side effects).
    if !dot_dir.exists() {
        return;
    }

    let cache = PersistentDirectoryWalkCache {
        entries: entries.clone(),
    };
    let cache_path = dot_dir.join("directory_walk_cache.json");
    if let Err(e) = std::fs::write(
        &cache_path,
        serde_json::to_string(&cache).unwrap_or_default(),
    ) {
        tracing::warn!("failed to persist directory walk cache: {}", e);
    }
}

/// Builds the walk-cache key (repo identity + HEAD + root mtime) for a repo.
/// Single source of truth so every reader/writer that keys on repo state — the
/// directory-walk cache, the density cache, and the persisted scope proposal —
/// derives a byte-identical key and their staleness comparisons agree.
fn build_directory_walk_cache_key(source_root: &Path) -> DirectoryWalkCacheKey {
    DirectoryWalkCacheKey {
        repo_identity: source_root
            .canonicalize()
            .ok()
            .and_then(|p| p.to_str().map(String::from))
            .unwrap_or_else(|| source_root.to_string_lossy().into_owned()),
        head_commit: get_head_commit(source_root),
        root_mtime: get_root_mtime(source_root),
    }
}

/// Tool/editor dot-directories filtered out of scope suggestions so they never
/// appear as ranked cone candidates. Single-sourced across the synchronous
/// facts envelope and the daemon-persisted scope proposal.
const EXCLUDED_DOT_DIRS: [&str; 5] = [".idea", ".claude", ".vscode", ".1up", ".agentdocs"];

/// Filename of the persisted scope proposal written alongside the directory
/// walk cache inside `.1up`. Keyed by the walk-cache key so a HEAD/mtime drift
/// invalidates it as stale.
const SCOPE_PROPOSAL_FILENAME: &str = "scope_proposal.json";

/// A single ranked directory in a persisted scope proposal. Ordered largest
/// first with dot-directories already filtered; carries only the fields needed
/// to rebuild ranked `scope_add` suggestions cheaply (no vector estimate).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PersistedDirectoryStat {
    directory: String,
    file_count: usize,
}

/// Scope proposal persisted by the daemon gate-fired branch so the MCP
/// Missing-readiness path can surface ranked scope suggestions even when the
/// synchronous `oneup_start` walk is hidden by the daemon-alive timing race.
///
/// `key` is the stringified walk-cache key at write time; a mismatch against
/// the current repo state means the proposal is stale and must not be surfaced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PersistedScopeProposal {
    key: String,
    per_directory_stats: Vec<PersistedDirectoryStat>,
    file_count_total: usize,
}

/// Pure staleness gate: a persisted proposal is fresh only when its recorded
/// walk-cache key exactly matches the current repo state key. A HEAD or mtime
/// drift changes the key, marking the proposal stale.
fn is_scope_proposal_fresh(persisted_key: &str, current_key: &str) -> bool {
    persisted_key == current_key
}

/// Loads the persisted scope proposal from `.1up`, or `None` when the file is
/// absent or unparseable (both non-fatal: the proposal is an optimization, and
/// a caller falls back to the generic next_action).
fn load_persisted_scope_proposal(state_root: &Path) -> Option<PersistedScopeProposal> {
    let path = project_dot_dir(state_root).join(SCOPE_PROPOSAL_FILENAME);
    let content = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Atomically persists a scope proposal into `.1up`, mirroring the walk-cache
/// guard: it never creates `.1up` (a gated attempt must leave no side effects)
/// and rides the secure root-clamped `atomic_replace` idiom. A serialization or
/// write failure returns an error for the best-effort caller to `warn!` on.
fn save_persisted_scope_proposal(
    state_root: &Path,
    proposal: &PersistedScopeProposal,
) -> Result<(), OneupError> {
    let dot_dir = project_dot_dir(state_root);
    // Never create .1up during a blocked/gated attempt (same guard as the
    // directory walk cache): only persist when the project dir already exists.
    if !dot_dir.exists() {
        return Ok(());
    }
    let secure_root = ensure_secure_project_root(state_root)?;
    let payload = serde_json::to_vec_pretty(proposal)
        .map_err(|e| OneupError::Other(anyhow::anyhow!("serialize scope proposal: {e}")))?;
    let path = dot_dir.join(SCOPE_PROPOSAL_FILENAME);
    atomic_replace(
        &path,
        &payload,
        &secure_root,
        PROJECT_STATE_DIR_MODE,
        SECURE_STATE_FILE_MODE,
    )?;
    Ok(())
}

/// Builds and persists a scope proposal for the daemon gate-fired branch.
///
/// Runs the gitignore-aware per-directory walk (cached), ranks the top-level
/// directories largest-first with dot-directories filtered, and writes the
/// result keyed by the current walk-cache key. Best-effort: callers invoke it
/// off the async executor (the walk is synchronous) and `warn!` on error. This
/// is what lets a later `oneup_status` surface ranked scope suggestions when the
/// daemon — not the synchronous MCP walk — fired the monorepo gate.
///
/// Cancel-aware: the walk checks `cancel_token` periodically (same cadence as
/// the daemon's gate walk) so a SIGTERM during proposal building does not pin
/// the daemon's bounded drain for a full repo walk. On cancellation it returns
/// `Ok(())` without persisting — the persist is best-effort and must never
/// error the idle return.
pub fn persist_scope_proposal_for_gate(
    state_root: &Path,
    source_root: &Path,
    cancel_token: &CancellationToken,
) -> Result<(), OneupError> {
    // Stamp the freshness key from BEFORE the walk: if HEAD or the root mtime
    // drift mid-walk, the persisted (pre-change) stats then read back stale —
    // the safe direction — instead of being labelled with the post-change key.
    let key = cache_key_to_string(&build_directory_walk_cache_key(source_root));
    let dir_counts = match count_files_per_directory_cancellable(source_root, cancel_token) {
        Ok(counts) => counts,
        Err(e) if cancel_token.is_cancelled() => {
            tracing::debug!("scope proposal walk cancelled; skipping persist: {e}");
            return Ok(());
        }
        Err(e) => return Err(e),
    };
    let file_count_total: usize = dir_counts.values().sum();

    let mut per_directory_stats: Vec<PersistedDirectoryStat> = dir_counts
        .into_iter()
        .map(|(directory, file_count)| PersistedDirectoryStat {
            directory,
            file_count,
        })
        .collect();
    // Largest first, ties broken by name for a deterministic ranking.
    per_directory_stats.sort_by(|a, b| {
        b.file_count
            .cmp(&a.file_count)
            .then_with(|| a.directory.cmp(&b.directory))
    });
    per_directory_stats.retain(|stat| !EXCLUDED_DOT_DIRS.contains(&stat.directory.as_str()));

    let proposal = PersistedScopeProposal {
        key,
        per_directory_stats,
        file_count_total,
    };
    save_persisted_scope_proposal(state_root, &proposal)
}

/// Attaches a fresh persisted scope proposal to a Missing-readiness payload.
///
/// Loads the proposal the daemon gate-fired branch persisted, verifies it is
/// fresh against the current repo walk-cache key, and — only when fresh —
/// populates `payload.scope_proposal` with ranked suggestions and cone
/// candidates. A stale, absent, or unreadable proposal is a no-op, leaving the
/// generic Missing next_action to stand.
fn attach_scope_proposal_if_fresh(
    payload: &mut ReadinessPayload,
    state_root: &Path,
    source_root: &Path,
) {
    let Some(proposal) = load_persisted_scope_proposal(state_root) else {
        return;
    };
    let current_key = cache_key_to_string(&build_directory_walk_cache_key(source_root));
    if !is_scope_proposal_fresh(&proposal.key, &current_key) {
        return;
    }

    // Rebuild ranked human-readable suggestions from the persisted stats,
    // reusing the same generator the synchronous facts envelope uses so the
    // wording cannot drift. estimated_vectors is irrelevant to ranking.
    let stats: Vec<DirectoryStats> = proposal
        .per_directory_stats
        .iter()
        .map(|stat| DirectoryStats {
            directory: stat.directory.clone(),
            file_count: stat.file_count,
            estimated_vectors: 0,
        })
        .collect();
    // Derive BOTH vectors from the one ranked output so `suggestions[i]`
    // always describes `scope_candidates[i]`: the MCP layer zips them into
    // `scope_add` next_actions, so rank alignment is a contract, not a
    // coincidence. The generator already caps the list. The daemon gate that
    // persists proposals has no launch-subdirectory concept, so none is
    // threaded here (unlike the synchronous facts envelope).
    let ranked = generate_ranked_scope_suggestions(&stats, &None);
    let suggestions: Vec<String> = ranked.iter().map(|s| s.reason.clone()).collect();
    let scope_candidates: Vec<String> = ranked.into_iter().map(|s| s.directory).collect();

    payload.scope_proposal = Some(ScopeProposalSummary {
        file_count_total: proposal.file_count_total,
        suggestions,
        scope_candidates,
    });
}

// FIX B: Density computation cache to avoid second uncached walk on every envelope call.
// Keyed by (repo_identity, HEAD, mtime) - same as directory walk cache key.
static DENSITY_CACHE: OnceLock<Mutex<HashMap<DirectoryWalkCacheKey, f64>>> = OnceLock::new();

fn get_density_cache() -> &'static Mutex<HashMap<DirectoryWalkCacheKey, f64>> {
    DENSITY_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Persistent on-disk cache for computed repository density.
///
/// Stored as JSON in the `.1up` directory under the project's *state root*
/// (`density_cache.json`), keyed by the same `cache_key_to_string` signal as
/// the directory walk cache (`repo_identity` + git HEAD + root mtime). That
/// keying IS the invalidation design: a HEAD move or a root-directory mtime
/// change produces a different key, so a stale entry simply misses and forces
/// a fresh walk that rewrites the map. Entries are never mutated in place or
/// explicitly expired. Because the key cannot see nested working-tree changes,
/// persistent reuse is additionally declined while the git worktree is dirty
/// (see `compute_avg_density_for_repo`). Surviving a process restart lets a
/// cold MCP process reuse a prior walk instead of re-walking the tree
/// (Fixes #87).
#[derive(Debug, Serialize, serde::Deserialize)]
struct PersistentDensityCache {
    entries: HashMap<String, f64>, // key: cache_key_to_string(&DirectoryWalkCacheKey)
}

/// Load the persistent density cache from the `.1up` directory.
/// Returns an empty map on any error (missing file, parse error, etc.); the
/// cache is an optimization, so read failures degrade silently to a fresh walk.
fn load_persistent_density_cache(state_root: &Path) -> HashMap<String, f64> {
    let cache_path = project_dot_dir(state_root).join("density_cache.json");
    match std::fs::read_to_string(&cache_path) {
        Ok(content) => match serde_json::from_str::<PersistentDensityCache>(&content) {
            Ok(cached) => cached.entries,
            Err(e) => {
                tracing::debug!("ignoring unreadable density cache: {}", e);
                HashMap::new()
            }
        },
        Err(_) => HashMap::new(),
    }
}

/// Persist the density cache to disk for cross-process reuse.
/// Only writes if the `.1up` directory already exists (same guard as the
/// directory walk cache: never create `.1up` during a blocked/failed attempt).
/// Write failures are non-fatal and never fail the request.
fn save_persistent_density_cache(state_root: &Path, entries: &HashMap<String, f64>) {
    let dot_dir = project_dot_dir(state_root);
    if !dot_dir.exists() {
        return;
    }

    let cache = PersistentDensityCache {
        entries: entries.clone(),
    };
    let cache_path = dot_dir.join("density_cache.json");
    if let Err(e) = std::fs::write(
        &cache_path,
        serde_json::to_string(&cache).unwrap_or_default(),
    ) {
        tracing::debug!("failed to persist density cache: {}", e);
    }
}

/// Extracts git HEAD OID from the repository.
/// Returns None if not in a git repo or if HEAD cannot be read.
fn get_head_commit(source_root: &Path) -> Option<String> {
    // Try to read HEAD using git command as a simple, reliable approach
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(source_root)
        .arg("rev-parse")
        .arg("HEAD")
        .output()
        .ok()?;

    if output.status.success() {
        let commit = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !commit.is_empty() && commit.len() == 40 {
            // Validate it's a hex OID (40 chars for SHA1)
            if commit.chars().all(|c| c.is_ascii_hexdigit()) {
                return Some(commit);
            }
        }
    }
    None
}

/// Reports whether the git worktree has uncommitted changes (staged, unstaged,
/// or untracked). Returns `None` when the signal is unavailable (not a git
/// repo, or git missing/failed); callers treat that as "no dirtiness signal"
/// and fall back to the identity + HEAD + mtime key contract.
fn is_worktree_dirty(source_root: &Path) -> Option<bool> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(source_root)
        .arg("status")
        .arg("--porcelain")
        .output()
        .ok()?;

    if output.status.success() {
        Some(!output.stdout.is_empty())
    } else {
        None
    }
}

/// Gets the root directory mtime as a cache invalidation signal.
/// Returns None if metadata cannot be read.
fn get_root_mtime(source_root: &Path) -> Option<u64> {
    std::fs::metadata(source_root)
        .ok()?
        .modified()
        .ok()?
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// Gets the configured file count threshold for facts envelope gate, with env var override.
fn get_file_count_threshold() -> usize {
    std::env::var(FILE_COUNT_THRESHOLD_ENV_VAR)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(FILE_COUNT_THRESHOLD)
}

/// Counts files per top-level directory with result caching.
/// Returns a map of directory name to file_count.
/// Cache invalidates on git HEAD drift or root directory mtime change,
/// ensuring <1s repeat call latency while maintaining correctness.
/// F5/N4b: Uses both in-process and persistent (disk-based) caches for cross-process reuse.
///
/// Non-cancellable convenience wrapper for synchronous callers (facts envelope,
/// coverage disclosure, refuse-and-propose) whose walks must run to completion;
/// cancel-aware callers (the daemon gate-fired branch) use
/// [`count_files_per_directory_cancellable`] directly.
fn count_files_per_directory(source_root: &Path) -> Result<BTreeMap<String, usize>, OneupError> {
    count_files_per_directory_cancellable(source_root, &CancellationToken::new())
}

/// Cancel-aware core of [`count_files_per_directory`]. A cold cache runs the
/// full repo walk, which checks `cancel_token` every ~100 entries (mirroring
/// the daemon's `count_files_gitignore_aware` gate walk) so SIGTERM can
/// interrupt it. On cancellation the partial result is discarded — never
/// cached in-process or on disk — and an error is returned for the caller to
/// map into its best-effort semantics.
fn count_files_per_directory_cancellable(
    source_root: &Path,
    cancel_token: &CancellationToken,
) -> Result<BTreeMap<String, usize>, OneupError> {
    // Build cache key from repo identity and current state
    let cache_key = build_directory_walk_cache_key(source_root);

    let cache_key_str = cache_key_to_string(&cache_key);

    // Check in-process cache first (fastest)
    if let Ok(cache) = get_directory_walk_cache().lock() {
        if let Some(cached_result) = cache.get(&cache_key) {
            return Ok(cached_result.clone());
        }
    }

    // Not in in-process cache; try persistent cache (F5/N4b: cross-process)
    // We need state_root to check persistent cache, but we only have source_root.
    // Since generate_facts_envelope is called early in oneup_start before any state
    // is written, we use a heuristic: if source_root is a git repo, the .1up would be
    // at source_root/.1up. This is good enough for the cold-start case.
    let potential_state_root = source_root;
    let persistent_cache = load_persistent_directory_walk_cache(potential_state_root);
    if let Some(cached_result) = persistent_cache.get(&cache_key_str) {
        // Found in persistent cache; restore to in-process cache for this process
        if let Ok(mut in_process) = get_directory_walk_cache().lock() {
            in_process.insert(cache_key.clone(), cached_result.clone());
        }
        return Ok(cached_result.clone());
    }

    // Not in either cache, compute via gitignore-aware walk
    let result = count_files_per_directory_uncached(source_root, cancel_token)?;

    // Store in both caches for future calls
    if let Ok(mut cache) = get_directory_walk_cache().lock() {
        cache.insert(cache_key.clone(), result.clone());
    }
    // F5/N4b: Persist to disk for cross-process reuse
    let mut persistent = persistent_cache;
    persistent.insert(cache_key_str, result.clone());
    save_persistent_directory_walk_cache(potential_state_root, &persistent);

    Ok(result)
}

/// Counts files per top-level directory (metadata-only walk, no parsing).
/// Gitignore-aware directory file count walk.
/// Uses the `ignore` crate to respect `.gitignore` rules, ensuring estimates match
/// the indexer's actual file counts and exclude untracked build trees.
/// Helper to determine if a path is under a VCS directory that should be excluded.
/// N1: VCS directories (.git, .hg, .svn) should never appear in file counts.
fn is_under_vcs_dir(path: &Path) -> bool {
    const VCS_DIRS: &[&str] = &[".git", ".hg", ".svn"];
    for component in path.components() {
        if let std::path::Component::Normal(name) = component {
            let name_str = name.to_string_lossy();
            if VCS_DIRS
                .iter()
                .any(|vcs| name_str.eq_ignore_ascii_case(vcs))
            {
                return true;
            }
        }
    }
    false
}

/// Helper to build a gitignore-aware walker with VCS directories excluded.
/// Excludes .git, .hg, .svn, and other VCS metadata to match indexer behavior.
/// N1: VCS directories (.git, etc.) should never appear in file counts or suggestions.
fn build_vcs_aware_walker(source_root: &Path) -> ignore::WalkBuilder {
    use ignore::WalkBuilder;

    let mut builder = WalkBuilder::new(source_root);
    builder
        .hidden(false) // Include hidden files/dirs for analysis
        .ignore(true); // Respect .gitignore rules

    builder
}

/// Counts files per top-level directory (metadata-only walk, no parsing).
/// Gitignore-aware directory file count walk with VCS directory exclusion.
/// Uses the `ignore` crate to respect `.gitignore` rules, ensuring estimates match
/// the indexer's actual file counts and exclude untracked build trees.
fn count_files_per_directory_uncached(
    source_root: &Path,
    cancel_token: &CancellationToken,
) -> Result<BTreeMap<String, usize>, OneupError> {
    let mut dir_counts: BTreeMap<String, usize> = BTreeMap::new();

    // Build a gitignore-aware walker with VCS directories excluded (N1)
    let walker = build_vcs_aware_walker(source_root).build();

    // Aggregate files by their top-level directory
    for (idx, entry) in walker.flatten().enumerate() {
        // Check cancellation every 100 entries (same cadence as the daemon's
        // count_files_gitignore_aware gate walk) so SIGTERM can interrupt.
        if idx % 100 == 0 && cancel_token.is_cancelled() {
            return Err(OneupError::Other(anyhow::anyhow!(
                "directory walk cancelled"
            )));
        }
        // Only count files, not directories
        if entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
            // Get the relative path from source_root
            if let Ok(rel_path) = entry.path().strip_prefix(source_root) {
                // N1: Skip files under VCS directories
                if is_under_vcs_dir(rel_path) {
                    continue;
                }
                // Extract the first component (top-level directory)
                if let Some(top_level) = rel_path.components().next() {
                    if let Component::Normal(name) = top_level {
                        if let Some(dir_name) = name.to_str() {
                            *dir_counts.entry(dir_name.to_string()).or_insert(0) += 1;
                        }
                    }
                } else {
                    // File at root level
                    *dir_counts.entry(".".to_string()).or_insert(0) += 1;
                }
            }
        }
    }

    // If no top-level directories were counted, count files at the root directly
    if dir_counts.is_empty() {
        let walker = build_vcs_aware_walker(source_root).build();

        let mut root_count = 0;
        for (idx, entry) in walker.flatten().enumerate() {
            if idx % 100 == 0 && cancel_token.is_cancelled() {
                return Err(OneupError::Other(anyhow::anyhow!(
                    "directory walk cancelled"
                )));
            }
            if entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
                if let Ok(rel_path) = entry.path().strip_prefix(source_root) {
                    // N1: Skip files under VCS directories
                    if !is_under_vcs_dir(rel_path) {
                        root_count += 1;
                    }
                }
            }
        }
        if root_count > 0 {
            dir_counts.insert(".".to_string(), root_count);
        }
    }

    Ok(dir_counts)
}

/// Counts total tracked files in repository using gitignore-aware walk.
/// Returns the gitignore-aware tracked file count (same definition used by indexer).
/// Excludes VCS directories (N1).
#[allow(dead_code)]
fn count_total_tracked_files(source_root: &Path) -> Result<usize, OneupError> {
    let walker = build_vcs_aware_walker(source_root).build();

    let count = walker
        .into_iter()
        .filter_map(|r| r.ok())
        .filter(|e| e.file_type().map(|ft| ft.is_file()).unwrap_or(false))
        .filter(|e| {
            // N1: Skip files under VCS directories
            e.path()
                .strip_prefix(source_root)
                .map(|rel| !is_under_vcs_dir(rel))
                .unwrap_or(true)
        })
        .count();

    Ok(count)
}

/// Density table for per-language vector count calibration.
/// N2: Both global and per-directory estimates use this table for consistency.
fn get_density_table() -> &'static [(&'static str, f64)] {
    &[
        ("rs", 37.0),   // Rust: measured 37.02 segments/file
        ("kt", 28.0),   // Kotlin: measured ~28 segments/file
        ("java", 28.0), // Java: same as Kotlin
        ("py", 30.0),   // Python: estimated conservative pending measurement
        ("go", 15.0),   // Go: estimated conservative pending measurement
        ("js", 25.0),   // JavaScript: estimated conservative pending measurement
        ("ts", 25.0),   // TypeScript: estimated conservative pending measurement
    ]
}

/// Compute average segments-per-file density for a repository based on language distribution.
/// N2: Shared by both global and per-directory estimates.
/// FIX B: Caches result to avoid re-walking on every envelope call.
///
/// `source_root` is the working tree that is walked and identifies the repo;
/// `state_root` is where `.1up` state lives (distinct from `source_root` for
/// linked git worktrees, whose `.1up` sits under the main worktree).
fn compute_avg_density_for_repo(state_root: &Path, source_root: &Path) -> Result<f64, OneupError> {
    // FIX B: Build cache key same as directory walk cache for consistency.
    // Contract note: this key (repo identity + git HEAD + root-dir mtime)
    // deliberately mirrors the sibling directory-walk cache and cannot see
    // nested working-tree changes — adding, deleting, or renaming files under
    // an existing subdirectory moves neither HEAD nor the root mtime. The
    // in-process cache accepts that bound (it dies with the process); the
    // persistent cache does not, and additionally declines reuse while the
    // worktree is dirty (see below).
    let cache_key = build_directory_walk_cache_key(source_root);

    let cache_key_str = cache_key_to_string(&cache_key);

    // Lookup order: in-process cache (fastest, preserves warm-path behavior).
    if let Ok(cache) = get_density_cache().lock() {
        if let Some(cached_density) = cache.get(&cache_key) {
            return Ok(*cached_density);
        }
    }

    // Not in memory; consult the persistent cache for cross-process reuse
    // (Fixes #87). A stale HEAD/mtime yields a different key, so only a
    // key-matched entry is a hit. Cross-process reuse is only sound when the
    // worktree is verifiably clean: a positively dirty worktree declines
    // persistent reuse (both load and save) and recomputes, matching the
    // pre-cache cold-path behavior. Without a git signal (non-git repo), fall
    // back to the identity + mtime contract shared with the sibling
    // directory-walk cache.
    let allow_persistent = !matches!(is_worktree_dirty(source_root), Some(true));
    let persistent_cache = allow_persistent.then(|| load_persistent_density_cache(state_root));
    if let Some(cached_density) = persistent_cache
        .as_ref()
        .and_then(|cache| cache.get(&cache_key_str))
    {
        // Restore into the in-process cache for the rest of this process.
        if let Ok(mut in_process) = get_density_cache().lock() {
            in_process.insert(cache_key.clone(), *cached_density);
        }
        return Ok(*cached_density);
    }

    // Not in either cache; compute the density by walking the repo.
    let density_table = get_density_table();
    let walker = build_vcs_aware_walker(source_root).build();

    let mut files_by_ext: HashMap<String, usize> = HashMap::new();
    for entry in walker.flatten() {
        if entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
            // N1: Skip files under VCS directories
            if let Ok(rel_path) = entry.path().strip_prefix(source_root) {
                if is_under_vcs_dir(rel_path) {
                    continue;
                }
            }
            if let Some(ext) = entry.path().extension().and_then(|e| e.to_str()) {
                *files_by_ext.entry(ext.to_string()).or_insert(0) += 1;
            }
        }
    }

    let mut total_density = 0.0;
    let mut files_by_known_ext = 0;

    for (ext, count) in &files_by_ext {
        if let Some((_, density)) = density_table.iter().find(|(e, _)| ext.ends_with(e)) {
            total_density += density * *count as f64;
            files_by_known_ext += count;
        }
    }

    let avg_segments_per_file = if files_by_known_ext > 0 {
        total_density / files_by_known_ext as f64
    } else {
        15.0 // Fallback for completely unmeasured ecosystems
    };

    // Populate both caches after the fresh walk: in-process for this process
    // and persistent for future cold processes (Fixes #87). A dirty worktree
    // skips the persistent write: the key cannot distinguish the dirty state,
    // so persisting it would let a later cold process reuse a value the key
    // cannot vouch for.
    if let Ok(mut cache) = get_density_cache().lock() {
        cache.insert(cache_key, avg_segments_per_file);
    }
    if let Some(mut persistent) = persistent_cache {
        persistent.insert(cache_key_str, avg_segments_per_file);
        save_persistent_density_cache(state_root, &persistent);
    }

    Ok(avg_segments_per_file)
}

/// Calibrates vector estimate based on measured language densities.
/// Returns (total_estimate, basis_explanation, low_bound, high_bound).
///
/// Vector estimate must be calibrated against real embedding density.
/// Measured densities:
/// - Rust: 37.02 segments/file (measured on 1up: 148 files → 5479 segments)
/// - Kotlin/Java: ~28 segments/file (measured on a large production monorepo)
/// - Unmeasured: 15-30 conservative range
fn estimate_vector_count(
    total_tracked_files: usize,
    state_root: &Path,
    source_root: &Path,
) -> Result<(usize, String, usize, usize), OneupError> {
    let avg_segments_per_file = compute_avg_density_for_repo(state_root, source_root)?;

    let estimated_count = (total_tracked_files as f64 * avg_segments_per_file) as usize;

    // Label the basis so agents understand confidence
    let basis = format!(
        "estimated ~{:.1} segments per file based on measured Rust (37.02) and Kotlin/Java (28.0) densities, with conservative estimates for unmeasured languages (15-30 range). Actual density varies by language; use this as a rough cost indicator, not a hard budget.",
        avg_segments_per_file
    );

    // Conservative lower and pessimistic upper bounds
    let low_bound = (total_tracked_files as f64 * 15.0) as usize;
    let high_bound = (total_tracked_files as f64 * 40.0) as usize;

    Ok((estimated_count, basis, low_bound, high_bound))
}

/// Detects workspace manifest files in the repo.
fn detect_workspace_manifests(source_root: &Path) -> Vec<String> {
    let mut manifests = Vec::new();

    let manifest_names = ["Cargo.toml", "package.json"];

    // Check root
    for name in manifest_names {
        if source_root.join(name).exists() {
            manifests.push(name.to_string());
        }
    }

    // Check top-level directories for manifests
    if let Ok(entries) = std::fs::read_dir(source_root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(dir_name) = path.file_name().and_then(|n| n.to_str()) {
                    if !dir_name.starts_with('.') {
                        for name in manifest_names {
                            let manifest_path = path.join(name);
                            if manifest_path.exists() {
                                let rel_path = format!("{}/{}", dir_name, name);
                                if !manifests.contains(&rel_path) {
                                    manifests.push(rel_path);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    manifests.sort();
    manifests
}

/// Parses git sparse-checkout if active.
fn get_sparse_checkout_info(source_root: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(source_root)
        .arg("sparse-checkout")
        .arg("list")
        .output();

    match output {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let trimmed = stdout.trim();
            if !trimmed.is_empty() {
                Some(trimmed.to_string())
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Checks if a facts envelope should be returned instead of indexing.
/// Returns true if:
/// 1. No index exists (status: Missing)
/// 2. No scope has been configured (scope_roots is None in meta table)
/// 3. File count exceeds the threshold
pub async fn should_return_facts_envelope(
    _state_root: &Path,
    source_root: &Path,
    readiness: &ReadinessPayload,
) -> Result<bool, OneupError> {
    // Only return facts on first-run when index is missing
    if readiness.status != ReadinessStatus::Missing {
        return Ok(false);
    }

    let threshold = get_file_count_threshold();

    // Quick file count: check directory sizes
    let dir_counts = count_files_per_directory(source_root)?;
    let total_files: usize = dir_counts.values().sum();

    Ok(total_files > threshold)
}

/// A ranked scope suggestion pairing a target directory with a coherent,
/// standalone reason. Single source of truth for both the display
/// `FactsEnvelope.suggestions` strings and the facts-envelope `next_actions`,
/// which also needs the `directory` to build the `scope_add` argument.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ScopeSuggestion {
    pub directory: String,
    pub reason: String,
}

/// Generates ranked scope suggestions from the largest top-level directories.
///
/// Returns up to 3 suggestions, each carrying the target directory and a
/// coherent standalone reason. The first emitted reason is a primary imperative
/// only when no `launch_subdir` precedes it; when a `launch_subdir` is present
/// (shown separately in the display envelope, or emitted as the leading
/// `next_action`), every scope suggestion reads as an alternative so the first
/// one is never a dangling primary. No reason ever begins with "Or ". The
/// `launch_subdir` directory is skipped to avoid duplicating its dedicated
/// suggestion, while ordinals stay truthful to the actual rank.
pub(crate) fn generate_ranked_scope_suggestions(
    per_directory_stats: &[DirectoryStats],
    launch_subdir: &Option<String>,
) -> Vec<ScopeSuggestion> {
    let mut suggestions: Vec<ScopeSuggestion> = Vec::new();

    for (idx, stat) in per_directory_stats.iter().take(3).enumerate() {
        // Skip suggesting the launch_subdir (redundant with its dedicated suggestion).
        if let Some(subdir) = launch_subdir {
            if &stat.directory == subdir {
                continue;
            }
        }

        let is_primary = suggestions.is_empty() && launch_subdir.is_none();
        let reason = match (is_primary, idx) {
            (true, 0) => format!("Index the largest directory: {}", stat.directory),
            (true, _) => format!(
                "Index the {} largest directory: {}",
                ordinal(idx + 1),
                stat.directory
            ),
            (false, 0) => format!(
                "Alternatively, index the largest directory: {}",
                stat.directory
            ),
            (false, _) => format!(
                "Alternatively, index the {} largest directory: {}",
                ordinal(idx + 1),
                stat.directory
            ),
        };

        suggestions.push(ScopeSuggestion {
            directory: stat.directory.clone(),
            reason,
        });
    }

    suggestions
}

/// Display-string view of the ranked scope suggestions for
/// `FactsEnvelope.suggestions`, derived from the structured source of truth.
fn generate_ranked_suggestions(
    per_directory_stats: &[DirectoryStats],
    launch_subdir: &Option<String>,
) -> Vec<String> {
    generate_ranked_scope_suggestions(per_directory_stats, launch_subdir)
        .into_iter()
        .map(|s| s.reason)
        .collect()
}

/// Helper to convert numeric position to ordinal (1st, 2nd, 3rd, etc.)
fn ordinal(n: usize) -> &'static str {
    match n {
        1 => "1st",
        2 => "2nd",
        3 => "3rd",
        _ => "next",
    }
}

/// Generates a facts envelope for a large monorepo on first-run.
/// Uses gitignore-aware file counts and calibrated vector estimates based on measured
/// language densities.
/// Per-directory estimates use the same calibrated density as the global estimate.
///
/// `state_root` locates `.1up` state (persistent density cache); it differs
/// from `source_root` for linked git worktrees, whose `.1up` lives under the
/// main worktree.
pub async fn generate_facts_envelope(
    state_root: &Path,
    source_root: &Path,
    launch_subdir: Option<PathBuf>,
) -> Result<FactsEnvelope, OneupError> {
    // Count files per top-level directory (gitignore-aware, N1: excludes .git)
    let dir_counts = count_files_per_directory(source_root)?;
    let file_count_total: usize = dir_counts.values().sum();

    // N2: Compute calibrated density once, use for both global and per-directory estimates.
    // Kept as a separate walk from count_files_per_directory above: the two aggregate
    // differently (per-directory counts vs per-extension density), own distinct in-process
    // and persistent caches, and are each called independently elsewhere. Folding them into
    // one walk would be broad restructuring across those call sites; the persistent density
    // cache (Fixes #87) already keeps this walk to at most once per repo state per process.
    let avg_segments_per_file = compute_avg_density_for_repo(state_root, source_root)?;

    // Build directory stats with calibrated estimates (N2: consistent with global estimate)
    let mut per_directory_stats: Vec<DirectoryStats> = dir_counts
        .into_iter()
        .map(|(directory, file_count)| DirectoryStats {
            directory,
            file_count,
            estimated_vectors: (file_count as f64 * avg_segments_per_file) as usize,
        })
        .collect();

    // Sort by file count descending (largest first)
    per_directory_stats.sort_by_key(|b| std::cmp::Reverse(b.file_count));

    // Filter out tool/editor dot-directories from stats
    // Excludes: .idea, .claude, .vscode, .1up, .agentdocs (noise in suggestions)
    per_directory_stats.retain(|stat| !EXCLUDED_DOT_DIRS.contains(&stat.directory.as_str()));

    // Calibrate global vector estimate (N2: reuses same computed density)
    let (vector_estimate_total, basis, low_bound, high_bound) =
        estimate_vector_count(file_count_total, state_root, source_root)?;

    let workspace_manifests = detect_workspace_manifests(source_root);
    let sparse_checkout = get_sparse_checkout_info(source_root);

    let launch_subdir_str = launch_subdir.and_then(|p| {
        p.strip_prefix(source_root)
            .ok()
            .and_then(|rel| rel.to_str().map(|s| s.to_string()))
    });

    // Generate ranked suggestions without dangling "Or" wording
    // Provide up to 3 top directories as ranked suggestions
    let suggestions = generate_ranked_suggestions(&per_directory_stats, &launch_subdir_str);

    Ok(FactsEnvelope {
        per_directory_stats,
        workspace_manifests,
        sparse_checkout,
        launch_subdir: launch_subdir_str,
        suggestions,
        file_count_total,
        vector_estimate_total,
        vector_estimate_basis: Some(basis),
        vector_estimate_low: Some(low_bound),
        vector_estimate_high: Some(high_bound),
    })
}

/// Computes the current index scope coverage information from the database and filesystem.
///
/// Reads the scope roots from the meta table, counts indexed files from the database,
/// and counts total files in the repository. Returns None if the index is not present
/// or readable.
///
/// When roots is empty (unscoped full index), populates eligibility_note to explain
/// why indexed_files < total_files (lockfiles, vendor dirs, excluded by .gitignore, etc).
pub async fn compute_index_scope(
    state_root: &Path,
    source_root: &Path,
    context_id: &str,
) -> Result<Option<IndexScope>, OneupError> {
    // Try to open the database
    let db_path = project_db_path(state_root);
    if !db_path.exists() {
        return Ok(None);
    }

    let db = Db::open_ro(&db_path).await?;
    let conn = db.connect_tuned().await?;

    // Read scope roots from meta table
    let scope_roots = schema::read_scope_from_meta(&conn).await?;

    // Count indexed files: get all file paths with segments for this context
    let indexed_file_paths =
        crate::storage::segments::get_all_file_paths_for_context(&conn, context_id).await?;
    let indexed_files = indexed_file_paths.len();

    // Count total files in the repository
    let dir_counts = count_files_per_directory(source_root)?;
    let total_files: usize = dir_counts.values().sum();

    let roots = scope_roots.unwrap_or_default();
    let eligibility_note = unscoped_eligibility_note(&roots);

    Ok(Some(IndexScope {
        roots,
        indexed_files,
        total_files,
        eligibility_note,
    }))
}

/// Eligibility note explaining the indexed/total gap for an unscoped (full)
/// index. Single-sourced here so the readiness/status and search paths
/// disclose identical semantics: populated only when `roots` is empty, and
/// always absent for scoped indexes.
fn unscoped_eligibility_note(roots: &[String]) -> Option<String> {
    if roots.is_empty() {
        Some(
            "Full index (no scope recorded). Indexed files = code and doc files. \
             Total files = all git-tracked files + gitignore-excluded files walked for statistics."
                .to_string(),
        )
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::types::{
        BranchStatus, DaemonRefreshState, IndexPhase, StructuralSearchStatus, WorktreeRole,
    };
    use crate::storage::segments::{self, SegmentInsert};
    use chrono::Utc;
    use std::fs;

    fn readiness_fixture() -> ReadinessPayload {
        blocked_readiness_for_path("repo", "fixture")
    }

    #[test]
    fn truncation_note_round_trips_with_scope_recovery() {
        let note = TruncationNote {
            reason: crate::shared::constants::SCOPE_TRUNCATION_REASON.to_string(),
            scope_name: Some("load_plugins".to_string()),
            scope_type: Some("function".to_string()),
            full_line_start: Some(71),
            full_line_end: Some(588),
            omitted_above: Some(13),
            omitted_below: Some(498),
            omitted_symbols: None,
            recovery: RecoveryCall {
                tool: "oneup_context".to_string(),
                arguments: serde_json::json!({"path": "manager.ts", "line": 87, "expansion": 500}),
            },
        };

        let value = serde_json::to_value(&note).unwrap();
        // Absent-when-None: symbol-clip-only field is not serialized for a scope clip.
        assert!(value.get("omitted_symbols").is_none());
        let restored: TruncationNote = serde_json::from_value(value).unwrap();
        assert_eq!(restored, note);
        assert_eq!(restored.recovery.tool, "oneup_context");
        assert_eq!(restored.recovery.arguments["line"], serde_json::json!(87));
    }

    #[test]
    fn truncation_note_round_trips_with_symbol_recovery() {
        let note = TruncationNote {
            reason: crate::shared::constants::SYMBOL_LIST_TRUNCATION_REASON.to_string(),
            scope_name: None,
            scope_type: None,
            full_line_start: None,
            full_line_end: None,
            omitted_above: None,
            omitted_below: None,
            omitted_symbols: Some(42),
            recovery: RecoveryCall {
                tool: "oneup_symbol".to_string(),
                arguments: serde_json::json!({"name": "Db"}),
            },
        };

        let value = serde_json::to_value(&note).unwrap();
        // Absent-when-None: scope-only fields are omitted for a symbol clip.
        assert!(value.get("scope_name").is_none());
        assert!(value.get("full_line_start").is_none());
        let restored: TruncationNote = serde_json::from_value(value).unwrap();
        assert_eq!(restored, note);
    }

    #[test]
    fn symbol_counts_round_trips() {
        let counts = SymbolCounts {
            defined: 1,
            referenced: 12,
            called: 0,
        };
        let restored: SymbolCounts =
            serde_json::from_value(serde_json::to_value(counts).unwrap()).unwrap();
        assert_eq!(restored, counts);
    }

    fn segment_with_symbols(defined: usize, referenced: usize, called: usize) -> StoredSegment {
        let names = |prefix: &str, n: usize| -> String {
            let v: Vec<String> = (0..n).map(|i| format!("{prefix}{i}")).collect();
            serde_json::to_string(&v).unwrap()
        };
        StoredSegment {
            id: "seg-1".to_string(),
            file_path: "src/lib.rs".to_string(),
            language: "rust".to_string(),
            block_type: "function".to_string(),
            content: "fn f() {}".to_string(),
            line_start: 1,
            line_end: 3,
            breadcrumb: None,
            complexity: 0,
            role: "DEFINITION".to_string(),
            defined_symbols: names("def", defined),
            referenced_symbols: names("ref", referenced),
            called_symbols: names("call", called),
            file_hash: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn segment_record_full_verbosity_caps_lists_and_notes_symbol_truncation() {
        let record = segment_record(segment_with_symbols(25, 3, 0), Some("full"));

        // Each list is capped at MAX_SYMBOLS_PER_LIST; content is untouched.
        assert_eq!(record.defined_symbols.len(), MAX_SYMBOLS_PER_LIST);
        assert_eq!(record.referenced_symbols.len(), 3);
        assert_eq!(record.content, "fn f() {}");
        // Counts belong to default verbosity only.
        assert!(record.symbol_counts.is_none());

        let note = record
            .truncation
            .expect("overflowing list must produce a note");
        assert_eq!(note.reason, SYMBOL_LIST_TRUNCATION_REASON);
        assert_eq!(note.omitted_symbols, Some(5));
        assert_eq!(note.recovery.tool, TOOL_SYMBOL);
        assert_eq!(note.recovery.arguments["name"], serde_json::json!("def0"));
    }

    #[test]
    fn segment_record_full_verbosity_without_overflow_has_no_truncation() {
        let record = segment_record(segment_with_symbols(2, 1, 0), Some("full"));
        assert_eq!(record.defined_symbols.len(), 2);
        assert!(record.truncation.is_none());
        assert!(record.symbol_counts.is_none());
    }

    #[test]
    fn segment_record_default_verbosity_emits_counts_and_omits_lists() {
        let record = segment_record(segment_with_symbols(25, 3, 0), None);

        assert!(record.defined_symbols.is_empty());
        assert!(record.referenced_symbols.is_empty());
        assert!(record.called_symbols.is_empty());
        // Symbol capping is a full-verbosity concern; default verbosity discloses
        // the omission through counts, not a truncation note.
        assert!(record.truncation.is_none());
        assert_eq!(
            record.symbol_counts,
            Some(SymbolCounts {
                defined: 25,
                referenced: 3,
                called: 0,
            })
        );
        assert_eq!(record.symbol_hint.as_deref(), Some("def0"));
    }

    #[test]
    fn segment_record_default_verbosity_omits_counts_when_all_empty() {
        let record = segment_record(segment_with_symbols(0, 0, 0), None);
        assert!(record.symbol_counts.is_none());
        assert!(record.truncation.is_none());
        assert!(record.symbol_hint.is_none());
    }

    fn scope_window(clipped: bool) -> ScopeWindow {
        ScopeWindow {
            content: "windowed body".to_string(),
            line_start: 84,
            line_end: 90,
            scope_line_start: 71,
            scope_line_end: 588,
            scope_type: "function".to_string(),
            scope_name: Some("load_plugins".to_string()),
            clipped,
        }
    }

    #[test]
    fn read_context_maps_clipped_window_to_recoverable_truncation() {
        let source_root = Path::new("/repo");
        let file_path = Path::new("/repo/src/manager.ts");
        let record = read_context(
            ReadSource::Location {
                path: "src/manager.ts".to_string(),
                line: 87,
            },
            source_root,
            file_path,
            87,
            scope_window(true),
            None,
        );

        let context = record.context.expect("context record present");
        // Window range and language surface; content appears once.
        assert_eq!((context.line_start, context.line_end), (84, 90));
        assert_eq!(context.language, "typescript");
        assert_eq!(context.content, "windowed body");

        let note = context.truncation.expect("clipped window carries a note");
        assert_eq!(note.reason, SCOPE_TRUNCATION_REASON);
        assert_eq!(note.scope_name.as_deref(), Some("load_plugins"));
        assert_eq!(note.scope_type.as_deref(), Some("function"));
        assert_eq!(
            (note.full_line_start, note.full_line_end),
            (Some(71), Some(588))
        );
        assert_eq!(note.omitted_above, Some(13));
        assert_eq!(note.omitted_below, Some(498));
        assert!(note.omitted_symbols.is_none());

        // Recovery re-issues oneup_context at the ORIGINAL target line, expansion
        // reaching the farthest scope edge (max(87-71, 588-87)=501) clamped to 500,
        // shaped as a directly re-issuable ContextInput.
        assert_eq!(note.recovery.tool, TOOL_CONTEXT);
        let location = &note.recovery.arguments["locations"][0];
        assert_eq!(location["path"], serde_json::json!("src/manager.ts"));
        assert_eq!(location["line"], serde_json::json!(87));
        assert_eq!(
            location["expansion"],
            serde_json::json!(MAX_CONTEXT_EXPANSION_LINES)
        );
    }

    #[test]
    fn read_context_whole_scope_has_no_truncation() {
        let source_root = Path::new("/repo");
        let file_path = Path::new("/repo/src/manager.ts");
        let record = read_context(
            ReadSource::Location {
                path: "src/manager.ts".to_string(),
                line: 87,
            },
            source_root,
            file_path,
            87,
            scope_window(false),
            None,
        );

        let context = record.context.expect("context record present");
        assert!(context.truncation.is_none());
    }

    #[test]
    fn head_drift_is_false_when_recorded_matches_current() {
        let mut payload = readiness_fixture();

        apply_head_drift(
            &mut payload,
            Some("abc123".to_string()),
            Some("abc123".to_string()),
        );

        assert_eq!(payload.drifted, Some(false));
        assert_eq!(payload.indexed_at_head.as_deref(), Some("abc123"));
        assert_eq!(payload.current_head.as_deref(), Some("abc123"));
    }

    #[test]
    fn head_drift_is_true_with_both_heads_when_oids_differ() {
        let mut payload = readiness_fixture();

        apply_head_drift(
            &mut payload,
            Some("abc123".to_string()),
            Some("def456".to_string()),
        );

        assert_eq!(payload.drifted, Some(true));
        assert_eq!(payload.indexed_at_head.as_deref(), Some("abc123"));
        assert_eq!(payload.current_head.as_deref(), Some("def456"));
    }

    #[test]
    fn head_drift_fields_stay_absent_when_either_head_is_missing() {
        let mut payload = readiness_fixture();
        apply_head_drift(&mut payload, None, Some("def456".to_string()));
        assert_eq!(payload.drifted, None);
        assert_eq!(payload.indexed_at_head, None);
        assert_eq!(payload.current_head, None);

        let mut payload = readiness_fixture();
        apply_head_drift(&mut payload, Some("abc123".to_string()), None);
        assert_eq!(payload.drifted, None);
        assert_eq!(payload.indexed_at_head, None);
        assert_eq!(payload.current_head, None);

        let value = serde_json::to_value(&payload).unwrap();
        assert!(value.get("drifted").is_none());
        assert!(value.get("indexed_at_head").is_none());
        assert!(value.get("current_head").is_none());
    }

    #[test]
    fn index_if_needed_triggers_on_drift_but_not_on_clean_ready() {
        let mut readiness = readiness_fixture();
        readiness.status = ReadinessStatus::Ready;
        assert!(!index_if_needed_applies(&readiness));

        readiness.drifted = Some(true);
        assert!(index_if_needed_applies(&readiness));

        readiness.drifted = Some(false);
        assert!(!index_if_needed_applies(&readiness));

        readiness.status = ReadinessStatus::Missing;
        assert!(index_if_needed_applies(&readiness));

        readiness.status = ReadinessStatus::Degraded;
        assert!(index_if_needed_applies(&readiness));
    }

    fn write_refresh_state(state_root: &Path, context_id: &str, state: DaemonRefreshState) {
        use crate::shared::types::{
            DaemonContextStatus, DaemonContextStatusFile, DaemonWatchStatus,
        };
        use std::collections::BTreeMap;

        fs::create_dir_all(project_dot_dir(state_root)).unwrap();
        let file = DaemonContextStatusFile {
            contexts: BTreeMap::from([(
                context_id.to_string(),
                DaemonContextStatus {
                    context_id: context_id.to_string(),
                    source_root: Some(state_root.to_path_buf()),
                    watch_status: DaemonWatchStatus::Watching,
                    last_file_check_at: None,
                    last_refresh_state: state,
                    last_refresh_started_at: None,
                    last_refresh_completed_at: None,
                    last_refresh_error: None,
                    branch_name: Some("main".to_string()),
                    branch_status: BranchStatus::Named,
                },
            )]),
        };
        fs::write(
            project_dot_dir(state_root).join("daemon_context_status.json"),
            serde_json::to_string(&file).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn stale_rebuild_reason_states_rebuilding_and_stale() {
        // The wording is the single source of truth folded into degraded_reason;
        // pin its substance so the user-facing notice cannot silently drift.
        assert_eq!(
            crate::shared::constants::STALE_REBUILD_REASON,
            "index is rebuilding; results may be stale"
        );
    }

    // Canonicalize: the secure-fs rebuild-lock root rejects symlinked path
    // components (macOS `tempdir()` lives under the `/var -> /private/var`
    // symlink), so the lock probe runs its genuine no-holder path on all
    // platforms instead of error-degrading to "not held".
    fn canonical_state_root(temp: &tempfile::TempDir) -> PathBuf {
        temp.path().canonicalize().unwrap()
    }

    #[test]
    fn rebuild_in_progress_false_when_idle() {
        let temp = tempfile::tempdir().unwrap();
        let state_root = canonical_state_root(&temp);
        // No daemon status file and no rebuild lock held: not rebuilding.
        assert!(!rebuild_in_progress(&state_root, "ctx"));

        // A completed refresh is not in progress either.
        write_refresh_state(&state_root, "ctx", DaemonRefreshState::Complete);
        assert!(!rebuild_in_progress(&state_root, "ctx"));
    }

    #[test]
    fn rebuild_in_progress_true_when_refresh_running_or_pending() {
        let temp = tempfile::tempdir().unwrap();
        let state_root = canonical_state_root(&temp);

        write_refresh_state(&state_root, "ctx", DaemonRefreshState::Running);
        assert!(rebuild_in_progress(&state_root, "ctx"));

        write_refresh_state(&state_root, "ctx", DaemonRefreshState::Pending);
        assert!(rebuild_in_progress(&state_root, "ctx"));
    }

    #[test]
    fn rebuild_in_progress_scoped_to_requested_context() {
        let temp = tempfile::tempdir().unwrap();
        let state_root = canonical_state_root(&temp);
        // A running refresh on a different context must not flag this one.
        write_refresh_state(&state_root, "other", DaemonRefreshState::Running);
        assert!(!rebuild_in_progress(&state_root, "ctx"));
    }

    #[cfg(unix)]
    #[test]
    fn rebuild_in_progress_true_when_rebuild_lock_held() {
        let temp = tempfile::tempdir().unwrap();
        let state_root = canonical_state_root(&temp);
        // No refresh state recorded: the only signal is the held lock.
        let _held = lifecycle::acquire_rebuild_lock(&state_root).unwrap();
        assert!(rebuild_in_progress(&state_root, "ctx"));
    }

    fn no_op_scan_filter() -> ScanFilter {
        ScanFilter::new(&[], &[], &[]).unwrap()
    }

    #[tokio::test]
    async fn read_context_locations_rejects_parent_escape() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir_all(&root).unwrap();

        let payload = read_context_locations(
            &root,
            &root,
            &no_op_scan_filter(),
            &[ReadLocation {
                path: "../outside.rs".to_string(),
                line: 1,
                expansion: None,
            }],
        )
        .await
        .unwrap();

        assert_eq!(payload.status, OperationStatus::Empty);
        assert_eq!(payload.records[0].status, ReadStatus::Rejected);
    }

    #[tokio::test]
    async fn read_context_locations_rejects_zero_line_as_structured_record() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir_all(&root).unwrap();

        let payload = read_context_locations(
            &root,
            &root,
            &no_op_scan_filter(),
            &[ReadLocation {
                path: "src/lib.rs".to_string(),
                line: 0,
                expansion: None,
            }],
        )
        .await
        .unwrap();

        assert_eq!(payload.status, OperationStatus::Empty);
        assert_eq!(payload.records[0].status, ReadStatus::Rejected);
        assert!(payload.records[0]
            .message
            .as_deref()
            .unwrap()
            .contains("1-based"));
    }

    #[tokio::test]
    async fn read_context_locations_reads_repo_relative_file() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "fn main() {\n    println!(\"hi\");\n}\n",
        )
        .unwrap();

        let payload = read_context_locations(
            &root,
            &root,
            &no_op_scan_filter(),
            &[ReadLocation {
                path: "src/lib.rs".to_string(),
                line: 2,
                expansion: None,
            }],
        )
        .await
        .unwrap();

        assert_eq!(payload.status, OperationStatus::Ok);
        assert_eq!(payload.records[0].status, ReadStatus::Found);
        assert_eq!(
            payload.records[0].context.as_ref().unwrap().path,
            "src/lib.rs"
        );
    }

    /// Red-first baseline: prior to enforcing `ScanFilter` at the
    /// context read path, `oneup_context` read secret-pattern files off disk
    /// directly, bypassing indexer exclusions entirely. This asserts the
    /// closed behavior — the fix under test refuses the file rather than
    /// returning its content.
    #[tokio::test]
    async fn read_context_locations_rejects_secret_pattern_file() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("credentials.json"), "{\"key\": \"super-secret\"}").unwrap();

        let payload = read_context_locations(
            &root,
            &root,
            &no_op_scan_filter(),
            &[ReadLocation {
                path: "credentials.json".to_string(),
                line: 1,
                expansion: None,
            }],
        )
        .await
        .unwrap();

        assert_eq!(payload.status, OperationStatus::Empty);
        assert_eq!(payload.records[0].status, ReadStatus::Rejected);
        assert!(payload.records[0].context.is_none());
        assert!(payload.records[0]
            .message
            .as_deref()
            .unwrap()
            .contains("excluded"));
    }

    #[tokio::test]
    async fn read_context_locations_rejects_configured_exclude_glob() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir_all(root.join("secrets")).unwrap();
        fs::write(root.join("secrets/internal.txt"), "internal only").unwrap();

        let scan_filter = ScanFilter::new(&[], &["secrets/**".to_string()], &[]).unwrap();
        let payload = read_context_locations(
            &root,
            &root,
            &scan_filter,
            &[ReadLocation {
                path: "secrets/internal.txt".to_string(),
                line: 1,
                expansion: None,
            }],
        )
        .await
        .unwrap();

        assert_eq!(payload.records[0].status, ReadStatus::Rejected);
        assert!(payload.records[0].context.is_none());
    }

    /// A non-excluded file continues to be served normally even
    /// when the project has a configured (non-matching) `ScanFilter`.
    #[tokio::test]
    async fn read_context_locations_serves_non_excluded_file_with_configured_filter() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "fn main() {\n    println!(\"hi\");\n}\n",
        )
        .unwrap();

        let scan_filter = ScanFilter::new(&[], &["secrets/**".to_string()], &[]).unwrap();
        let payload = read_context_locations(
            &root,
            &root,
            &scan_filter,
            &[ReadLocation {
                path: "src/lib.rs".to_string(),
                line: 2,
                expansion: None,
            }],
        )
        .await
        .unwrap();

        assert_eq!(payload.records[0].status, ReadStatus::Found);
    }

    #[test]
    fn check_path_in_scope_matches_root_files() {
        let scope = vec!["src".to_string()];
        let path = std::path::Path::new("src/lib.rs");
        assert!(check_path_in_scope(path, &scope));
    }

    #[test]
    fn check_path_in_scope_matches_nested_files() {
        let scope = vec!["src/components".to_string()];
        let path = std::path::Path::new("src/components/button.rs");
        assert!(check_path_in_scope(path, &scope));
    }

    #[test]
    fn check_path_in_scope_rejects_different_prefix() {
        let scope = vec!["src".to_string()];
        let path = std::path::Path::new("extra/utils.rs");
        assert!(!check_path_in_scope(path, &scope));
    }

    #[test]
    fn check_path_in_scope_rejects_partial_prefix_match() {
        let scope = vec!["src".to_string()];
        let path = std::path::Path::new("src_extra/file.rs");
        assert!(!check_path_in_scope(path, &scope));
    }

    #[test]
    fn check_path_in_scope_accepts_multiple_roots() {
        let scope = vec!["src".to_string(), "lib".to_string()];
        assert!(check_path_in_scope(
            std::path::Path::new("src/main.rs"),
            &scope
        ));
        assert!(check_path_in_scope(
            std::path::Path::new("lib/util.rs"),
            &scope
        ));
        assert!(!check_path_in_scope(
            std::path::Path::new("extra/other.rs"),
            &scope
        ));
    }

    #[test]
    fn check_path_in_scope_empty_scope_returns_true() {
        let scope: Vec<String> = vec![];
        let path = std::path::Path::new("any/file.rs");
        assert!(check_path_in_scope(path, &scope));
    }

    #[test]
    fn format_out_of_scope_disclosure_formats_correctly() {
        let scope = vec!["src".to_string(), "lib".to_string()];
        let disclosure = format_out_of_scope_disclosure(&scope);
        assert!(disclosure.contains("outside indexed scope"));
        assert!(disclosure.contains("src, lib"));
        assert!(disclosure.contains("Expand scope"));
    }

    #[test]
    fn symbol_partition_keeps_definitions_and_usages_distinct() {
        let results = vec![
            SymbolResult {
                segment_id: "def".to_string(),
                name: "Thing".to_string(),
                kind: "struct".to_string(),
                file_path: "src/lib.rs".to_string(),
                language: "rust".to_string(),
                line_start: 1,
                line_end: 2,
                content: "struct Thing;".to_string(),
                reference_kind: ReferenceKind::Definition,
                breadcrumb: None,
            },
            SymbolResult {
                segment_id: "usage".to_string(),
                name: "Thing".to_string(),
                kind: "function".to_string(),
                file_path: "src/main.rs".to_string(),
                language: "rust".to_string(),
                line_start: 3,
                line_end: 4,
                content: "let _ = Thing;".to_string(),
                reference_kind: ReferenceKind::Usage,
                breadcrumb: None,
            },
        ];

        let (definitions, references) = partition_symbol_results(results);

        assert_eq!(definitions.len(), 1);
        assert_eq!(references.len(), 1);
        assert_eq!(definitions[0].reference_kind, ReferenceKind::Definition);
        assert_eq!(references[0].reference_kind, ReferenceKind::Usage);
    }

    #[tokio::test]
    async fn search_structural_uses_worktree_context_scope() {
        let temp_root = std::env::current_dir().unwrap().join("target/oneup-tests");
        fs::create_dir_all(&temp_root).unwrap();
        let temp = tempfile::tempdir_in(temp_root).unwrap();
        let root = temp.path().join("repo");
        fs::create_dir_all(root.join(".1up")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/active.rs"), "fn active() {}\n").unwrap();
        fs::write(root.join("src/other.rs"), "fn other() {}\n").unwrap();

        let db = Db::open_rw(&project_db_path(&root)).await.unwrap();
        let conn = db.connect().unwrap();
        schema::initialize(&conn).await.unwrap();

        let active = test_segment("active", "src/active.rs");
        let other = test_segment("other", "src/other.rs");
        segments::replace_file_segments_for_context_tx(
            &conn,
            "ctx-active",
            "src/active.rs",
            &[active],
        )
        .await
        .unwrap();
        segments::replace_file_segments_for_context_tx(
            &conn,
            "ctx-other",
            "src/other.rs",
            &[other],
        )
        .await
        .unwrap();

        let context = WorktreeContext {
            context_id: "ctx-active".to_string(),
            state_root: root.clone(),
            source_root: root.clone(),
            main_worktree_root: root.clone(),
            worktree_role: WorktreeRole::Main,
            git_dir: None,
            common_git_dir: None,
            branch_name: None,
            branch_ref: None,
            head_oid: None,
            branch_status: BranchStatus::Unknown,
        };

        let payload = search_structural(
            &root,
            &root,
            &context,
            "(function_item name: (identifier) @name)",
            Some("rust"),
        )
        .await
        .unwrap();

        assert_eq!(payload.status, StructuralSearchStatus::Ok);
        assert_eq!(payload.results.len(), 1);
        assert_eq!(payload.results[0].content, "active");
    }

    #[tokio::test]
    async fn search_without_indexed_vectors_stays_fts_only_with_explicit_reason() {
        // Defect C query-side regression: when the index holds no vector rows
        // for the active context, local search must take the FTS-only branch
        // that never constructs an embedding runtime, and it must report the
        // explicit degraded reason instead of silently downgrading.
        let temp_root = std::env::current_dir().unwrap().join("target/oneup-tests");
        fs::create_dir_all(&temp_root).unwrap();
        let temp = tempfile::tempdir_in(temp_root).unwrap();
        let root = temp.path().join("repo");
        fs::create_dir_all(root.join(".1up")).unwrap();

        let db = Db::open_rw(&project_db_path(&root)).await.unwrap();
        let conn = db.connect().unwrap();
        schema::initialize(&conn).await.unwrap();
        segments::replace_file_segments_for_context_tx(
            &conn,
            "ctx-active",
            "src/active.rs",
            &[test_segment("vectorless_needle", "src/active.rs")],
        )
        .await
        .unwrap();
        drop(conn);

        let context = WorktreeContext {
            context_id: "ctx-active".to_string(),
            state_root: root.clone(),
            source_root: root.clone(),
            main_worktree_root: root.clone(),
            worktree_role: WorktreeRole::Main,
            git_dir: None,
            common_git_dir: None,
            branch_name: Some("main".to_string()),
            branch_ref: Some("refs/heads/main".to_string()),
            head_oid: None,
            branch_status: BranchStatus::Named,
        };

        let payload = run_search(&root, &context, &["vectorless_needle".to_string()], 5, None)
            .await
            .unwrap();

        assert_eq!(payload.status, OperationStatus::Degraded);
        assert_eq!(
            payload.degraded_reason.as_deref(),
            Some(NO_INDEXED_EMBEDDINGS_REASON),
            "the FTS-only path must carry the explicit no-embeddings reason"
        );
        assert!(
            !payload.results.is_empty(),
            "FTS-only search should still return lexical hits"
        );
    }

    /// The `mcp::ops` construction site must inject
    /// `path_prefix` into `SearchScope` so a scoped `oneup_search` never leaks
    /// results outside the prefix, while an unscoped call keeps the full-repo
    /// result set (mirrors the equivalent cli::search and daemon::worker
    /// coverage for the other two request-layer construction sites).
    #[tokio::test]
    async fn run_search_with_path_prefix_scopes_to_prefix() {
        let temp_root = std::env::current_dir().unwrap().join("target/oneup-tests");
        fs::create_dir_all(&temp_root).unwrap();
        let temp = tempfile::tempdir_in(temp_root).unwrap();
        let root = temp.path().join("repo");
        fs::create_dir_all(root.join(".1up")).unwrap();

        let db = Db::open_rw(&project_db_path(&root)).await.unwrap();
        let conn = db.connect().unwrap();
        schema::initialize(&conn).await.unwrap();
        segments::replace_file_segments_for_context_tx(
            &conn,
            "ctx-active",
            "included/a.rs",
            &[test_segment("probetokenonly_included", "included/a.rs")],
        )
        .await
        .unwrap();
        segments::replace_file_segments_for_context_tx(
            &conn,
            "ctx-active",
            "other/b.rs",
            &[test_segment("probetokenonly_other", "other/b.rs")],
        )
        .await
        .unwrap();
        drop(conn);

        let context = WorktreeContext {
            context_id: "ctx-active".to_string(),
            state_root: root.clone(),
            source_root: root.clone(),
            main_worktree_root: root.clone(),
            worktree_role: WorktreeRole::Main,
            git_dir: None,
            common_git_dir: None,
            branch_name: Some("main".to_string()),
            branch_ref: Some("refs/heads/main".to_string()),
            head_oid: None,
            branch_status: BranchStatus::Named,
        };

        let scoped = run_search(
            &root,
            &context,
            &["probetokenonly".to_string()],
            10,
            Some("included"),
        )
        .await
        .unwrap();
        let scoped_paths: Vec<_> = scoped.results.iter().map(|r| r.path.clone()).collect();
        assert_eq!(
            scoped_paths,
            vec!["included/a.rs".to_string()],
            "path_prefix must constrain oneup_search results to the prefix"
        );

        let unscoped = run_search(&root, &context, &["probetokenonly".to_string()], 10, None)
            .await
            .unwrap();
        assert_eq!(
            unscoped.results.len(),
            2,
            "no prefix supplied must leave full-repo search behavior unchanged"
        );
    }

    /// Build a self-contained, finalized staging index at
    /// `<state_root>/.1up/index.db.rebuild-<uuid>` holding one segment for
    /// `context_id`, ready for `swap::swap_index_into_place`. Mirrors
    /// `daemon::worker`'s `staged_index` test helper.
    async fn build_staged_index_with_segment(
        state_root: &Path,
        context_id: &str,
        segment_id: &str,
    ) -> PathBuf {
        let scratch = tempfile::tempdir().unwrap();
        let scratch_root = scratch.path().canonicalize().unwrap().join("scratch");
        fs::create_dir_all(&scratch_root).unwrap();
        let scratch_index = project_db_path(&scratch_root);
        crate::shared::fs::ensure_secure_project_root(&scratch_root).unwrap();

        let db = Db::open_rw(&scratch_index).await.unwrap();
        let conn = db.connect_tuned().await.unwrap();
        schema::initialize(&conn).await.unwrap();
        segments::replace_file_segments_for_context_tx(
            &conn,
            context_id,
            "src/new.rs",
            &[test_segment(segment_id, "src/new.rs")],
        )
        .await
        .unwrap();
        drop(conn);
        swap::finalize_staged_db(db, &scratch_index).await.unwrap();

        let staging = config::project_staging_db_path(state_root);
        std::fs::rename(&scratch_index, &staging).unwrap();
        staging
    }

    /// A second `open_current_index` call on an
    /// unchanged-inode `db_path` must reuse the cached tuned RO connection and
    /// skip `ensure_current`/schema re-validation, rather than opening a fresh
    /// connection per call.
    ///
    /// This is observed behaviorally: SQLite `TEMP` objects are private to the
    /// connection that created them, so a `TEMP TABLE` created on the first
    /// call's connection is visible on the second call's connection only if
    /// the warm cache served the *same* underlying connection rather than
    /// opening a new one.
    #[tokio::test]
    async fn open_current_index_reuses_warm_connection_when_inode_is_unchanged() {
        let temp_root = std::env::current_dir().unwrap().join("target/oneup-tests");
        fs::create_dir_all(&temp_root).unwrap();
        let temp = tempfile::tempdir_in(temp_root).unwrap();
        let root = temp.path().join("repo");
        fs::create_dir_all(root.join(".1up")).unwrap();

        let db = Db::open_rw(&project_db_path(&root)).await.unwrap();
        let conn = db.connect().unwrap();
        schema::initialize(&conn).await.unwrap();
        drop(conn);
        drop(db);

        let first = open_current_index(&root).await.unwrap();
        first
            .conn
            .execute("CREATE TEMP TABLE warm_probe(x)", ())
            .await
            .unwrap();

        let second = open_current_index(&root).await.unwrap();
        second
            .conn
            .query("SELECT x FROM warm_probe", ())
            .await
            .expect(
                "a second open_current_index call on an unchanged inode must reuse the \
                 first call's warm connection instead of opening a fresh one",
            );
    }

    /// After a build-aside swap installs a fresh
    /// index generation on a new inode, the next MCP read must observe the new
    /// generation's data -- never silently continue serving the pre-swap
    /// generation through the warm cache -- and must still surface the
    /// correct degraded reason (here, `NO_INDEXED_EMBEDDINGS_REASON`, since
    /// neither generation's fixture segments carry embeddings).
    #[tokio::test]
    async fn open_current_index_reopens_and_serves_new_generation_after_swap() {
        let temp_root = std::env::current_dir().unwrap().join("target/oneup-tests");
        fs::create_dir_all(&temp_root).unwrap();
        let temp = tempfile::tempdir_in(temp_root).unwrap();
        let root = temp.path().join("repo");
        fs::create_dir_all(root.join(".1up")).unwrap();

        let db = Db::open_rw(&project_db_path(&root)).await.unwrap();
        let conn = db.connect().unwrap();
        schema::initialize(&conn).await.unwrap();
        segments::replace_file_segments_for_context_tx(
            &conn,
            "ctx-active",
            "src/old.rs",
            &[test_segment("old_needle", "src/old.rs")],
        )
        .await
        .unwrap();
        drop(conn);
        drop(db);

        let context = WorktreeContext {
            context_id: "ctx-active".to_string(),
            state_root: root.clone(),
            source_root: root.clone(),
            main_worktree_root: root.clone(),
            worktree_role: WorktreeRole::Main,
            git_dir: None,
            common_git_dir: None,
            branch_name: Some("main".to_string()),
            branch_ref: Some("refs/heads/main".to_string()),
            head_oid: None,
            branch_status: BranchStatus::Named,
        };

        // Warm the process-global cache against the pre-swap generation.
        let before = run_search(&root, &context, &["old_needle".to_string()], 5, None)
            .await
            .unwrap();
        assert_eq!(before.results.len(), 1);

        let staging = build_staged_index_with_segment(&root, "ctx-active", "new_needle").await;
        {
            let _lock = lifecycle::acquire_rebuild_lock(&root).unwrap();
            swap::swap_index_into_place(&root, &staging).await.unwrap();
        }

        let after = run_search(&root, &context, &["new_needle".to_string()], 5, None)
            .await
            .unwrap();
        assert_eq!(
            after.results.len(),
            1,
            "a post-swap read must observe the new generation's data"
        );
        assert_eq!(after.status, OperationStatus::Degraded);
        assert_eq!(
            after.degraded_reason.as_deref(),
            Some(NO_INDEXED_EMBEDDINGS_REASON),
            "the post-swap read must carry the correct degraded reason, not a stale/silent one"
        );

        let stale = run_search(&root, &context, &["old_needle".to_string()], 5, None)
            .await
            .unwrap();
        assert!(
            stale.results.is_empty(),
            "the pre-swap generation's data must not still be served through the warm cache"
        );
    }

    /// The per-context vector-count cache on a warm index entry must be
    /// populated on demand, keyed independently per context, and cleared in
    /// full when a build-aside swap reopens the entry -- mirroring the
    /// daemon's `reopen_invalidates_cached_vector_count_after_swap` coverage
    /// for `ProjectState::cached_vector_count`, so a stale count never
    /// describes the wrong generation to the vector stage's diagnostics.
    #[tokio::test]
    async fn vector_count_cache_is_scoped_per_context_and_cleared_by_reopen() {
        let temp_root = std::env::current_dir().unwrap().join("target/oneup-tests");
        fs::create_dir_all(&temp_root).unwrap();
        let temp = tempfile::tempdir_in(temp_root).unwrap();
        let root = temp.path().join("repo");
        fs::create_dir_all(root.join(".1up")).unwrap();

        let db = Db::open_rw(&project_db_path(&root)).await.unwrap();
        let conn = db.connect().unwrap();
        schema::initialize(&conn).await.unwrap();
        drop(conn);
        drop(db);

        let current = open_current_index(&root).await.unwrap();
        assert_eq!(
            cached_vector_count_for_context(&current.db_path, "ctx-a").await,
            None
        );

        record_vector_count_for_context(&current.db_path, "ctx-a", 42).await;
        assert_eq!(
            cached_vector_count_for_context(&current.db_path, "ctx-a").await,
            Some(42)
        );
        assert_eq!(
            cached_vector_count_for_context(&current.db_path, "ctx-b").await,
            None,
            "the vector-count cache must be scoped per context, not shared across contexts"
        );

        let staging = build_staged_index_with_segment(&root, "ctx-a", "new_needle").await;
        {
            let _lock = lifecycle::acquire_rebuild_lock(&root).unwrap();
            swap::swap_index_into_place(&root, &staging).await.unwrap();
        }

        let reopened = open_current_index(&root).await.unwrap();
        assert_eq!(
            cached_vector_count_for_context(&reopened.db_path, "ctx-a").await,
            None,
            "a build-aside swap must invalidate any cached per-context vector count"
        );
    }

    #[tokio::test]
    async fn get_handles_batched_matches_per_item_outcomes_and_order() {
        // The batched exact-id + residual-prefix resolver must return the
        // same per-handle Found/NotFound/Ambiguous outcomes, in input order, as
        // resolving each handle one at a time (exact id, then prefix).
        let db = Db::open_memory().await.unwrap();
        let conn = db.connect().unwrap();
        schema::initialize(&conn).await.unwrap();

        let ctx = "ctx-get";
        for (id, file) in [
            ("aaaa1111bbbb2222", "src/a.rs"),
            ("aaaa1111cccc3333", "src/b.rs"),
            ("dddd4444eeee5555", "src/c.rs"),
        ] {
            segments::upsert_segment_for_context(&conn, ctx, &test_segment(id, file))
                .await
                .unwrap();
        }

        let handles = vec![
            "dddd4444eeee5555".to_string(),  // exact id (batch) -> Found
            ":dddd4444eeee5555".to_string(), // ':'-stripped exact id -> Found
            "dddd4444eeee".to_string(),      // unique 12-char prefix -> residual Found
            "aaaa1111".to_string(),          // ambiguous prefix -> residual Ambiguous
            String::new(),                   // empty -> NotFound (empty)
            ":".to_string(),                 // normalizes to empty -> NotFound (empty)
            "zzzznotfound0000".to_string(),  // full id, no row -> residual NotFound
            "dddd4444eeee5555".to_string(),  // duplicate of #1 -> Found (independent)
        ];

        let batched = resolve_handle_records(&conn, ctx, &handles, None)
            .await
            .unwrap();

        // Reconstruct the per-item baseline: exact id, then prefix, per handle.
        let mut expected = Vec::with_capacity(handles.len());
        for handle in &handles {
            expected.push(
                resolve_handle_record_per_item(&conn, ctx, handle, None)
                    .await
                    .unwrap(),
            );
        }

        assert_eq!(
            serde_json::to_value(&batched).unwrap(),
            serde_json::to_value(&expected).unwrap(),
            "batched get must match the per-item path field-for-field, in order"
        );

        // Pin the concrete outcomes + order so the test still has teeth if both
        // paths regressed together.
        let statuses: Vec<ReadStatus> = batched.iter().map(|record| record.status).collect();
        assert_eq!(
            statuses,
            vec![
                ReadStatus::Found,
                ReadStatus::Found,
                ReadStatus::Found,
                ReadStatus::Ambiguous,
                ReadStatus::NotFound,
                ReadStatus::NotFound,
                ReadStatus::NotFound,
                ReadStatus::Found,
            ]
        );
        assert_eq!(
            batched[3].matching_handles.len(),
            2,
            "ambiguous prefix surfaces both colliding ids"
        );
        assert_eq!(
            batched[0].segment.as_ref().unwrap().handle,
            "dddd4444eeee5555"
        );
        assert_eq!(
            batched[7].segment.as_ref().unwrap().handle,
            "dddd4444eeee5555",
            "a duplicate handle resolves independently and preserves order"
        );
    }

    #[test]
    fn residual_resolution_error_isolates_non_lock_and_propagates_lock() {
        let source = || ReadSource::Handle {
            raw: ":deadbeefcafe".to_string(),
            normalized: "deadbeefcafe".to_string(),
        };

        let isolated =
            isolate_residual_resolution_error(source(), anyhow::anyhow!("segment decode failed"))
                .expect("a non-lock error is isolated to a record, not propagated");
        assert_eq!(isolated.status, ReadStatus::Error);
        assert_eq!(
            isolated.message.as_deref(),
            Some("segment decode failed"),
            "the error text is surfaced on the isolated record"
        );
        assert!(isolated.segment.is_none());
        match &isolated.source {
            ReadSource::Handle { raw, .. } => assert_eq!(raw, ":deadbeefcafe"),
            other => panic!("the requested handle must be preserved, got {other:?}"),
        }

        let propagated =
            isolate_residual_resolution_error(source(), anyhow::anyhow!("database is locked"));
        assert!(
            propagated.is_err(),
            "a lock error must propagate for whole-call retry, not become a record"
        );
    }

    #[tokio::test]
    async fn resolve_handle_records_aggregate_reflects_found_mix() {
        let db = Db::open_memory().await.unwrap();
        let conn = db.connect().unwrap();
        schema::initialize(&conn).await.unwrap();

        let ctx = "ctx-aggregate";
        for (id, file) in [
            ("aaaa1111bbbb2222", "src/a.rs"),
            ("cccc3333dddd4444", "src/b.rs"),
        ] {
            segments::upsert_segment_for_context(&conn, ctx, &test_segment(id, file))
                .await
                .unwrap();
        }

        let all_valid = resolve_handle_records(
            &conn,
            ctx,
            &[
                "aaaa1111bbbb2222".to_string(),
                "cccc3333dddd4444".to_string(),
            ],
            None,
        )
        .await
        .unwrap();
        assert_eq!(aggregate_read_status(&all_valid), OperationStatus::Ok);
        assert_eq!(
            all_valid
                .iter()
                .map(|record| record.status)
                .collect::<Vec<_>>(),
            vec![ReadStatus::Found, ReadStatus::Found]
        );

        let mixed = resolve_handle_records(
            &conn,
            ctx,
            &[
                "aaaa1111bbbb2222".to_string(),   // valid
                "zzzznotarealhandle".to_string(), // mistyped miss
            ],
            None,
        )
        .await
        .unwrap();
        assert_eq!(aggregate_read_status(&mixed), OperationStatus::Partial);
        assert_eq!(mixed[0].status, ReadStatus::Found);
        assert_eq!(
            mixed[0].segment.as_ref().unwrap().handle,
            "aaaa1111bbbb2222",
            "the valid handle's content survives the sibling miss"
        );
        assert_ne!(
            mixed[1].status,
            ReadStatus::Found,
            "the mistyped handle is an isolated non-found record, not a batch abort"
        );

        let all_invalid = resolve_handle_records(
            &conn,
            ctx,
            &[
                "zzzznotarealhandle".to_string(),
                "yyyyalsomissing0000".to_string(),
            ],
            None,
        )
        .await
        .unwrap();
        assert_eq!(aggregate_read_status(&all_invalid), OperationStatus::Empty);
    }

    // Observed warm-suite failure fixture (design.md): a single dropped
    // character late in the handle leaves a 28-char unique shared prefix.
    const RECOVERY_SUPPLIED: &str = "0b25cc46a316205a1afe69ccd1137e2";
    const RECOVERY_TRUE_ID: &str = "0b25cc46a316205a1afe69ccd11337e2";

    #[test]
    fn recover_handle_gate_resolves_unique_longest_prefix() {
        // A lone candidate uniquely closest at a >= floor prefix recovers.
        let candidates = vec![RECOVERY_TRUE_ID.to_string()];
        assert_eq!(
            recover_handle_by_unique_prefix(RECOVERY_SUPPLIED, &candidates),
            HandleRecovery::Found(RECOVERY_TRUE_ID.to_string())
        );
    }

    #[test]
    fn recover_handle_gate_prefers_the_strictly_closer_candidate() {
        // One candidate shares a 28-char prefix, the other diverges from the
        // supplied handle right at the floor: the strictly closer id wins
        // uniquely rather than tying. (`0` differs from the supplied char at
        // index 8, so `far` shares exactly the 8-char floor.)
        let far = format!("{}000000000000000000000000", &RECOVERY_SUPPLIED[..8]);
        let candidates = vec![RECOVERY_TRUE_ID.to_string(), far];
        assert_eq!(
            recover_handle_by_unique_prefix(RECOVERY_SUPPLIED, &candidates),
            HandleRecovery::Found(RECOVERY_TRUE_ID.to_string())
        );
    }

    #[test]
    fn recover_handle_gate_declines_on_a_tie() {
        // Two candidates both diverge from the supplied handle at the floor
        // (char index 8 is `f`, not the supplied `a`) and differ only from each
        // other afterward, so they tie at the 8-char prefix; the gate declines
        // to guess and returns both.
        let a = format!("{}f{}", &RECOVERY_SUPPLIED[..8], "a".repeat(23));
        let b = format!("{}f{}", &RECOVERY_SUPPLIED[..8], "b".repeat(23));
        match recover_handle_by_unique_prefix(RECOVERY_SUPPLIED, &[a.clone(), b.clone()]) {
            HandleRecovery::Ambiguous(ids) => {
                assert_eq!(ids.len(), 2);
                assert!(ids.contains(&a) && ids.contains(&b));
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn recover_handle_gate_declines_below_the_floor_or_without_candidates() {
        // A supplied handle shorter than the floor can never discriminate.
        assert_eq!(
            recover_handle_by_unique_prefix("0b25cc", &[RECOVERY_TRUE_ID.to_string()]),
            HandleRecovery::None
        );
        // No candidate to recover to.
        assert_eq!(
            recover_handle_by_unique_prefix(RECOVERY_SUPPLIED, &[]),
            HandleRecovery::None
        );
    }

    /// Seed a memory index with one segment per (context, id) pair and resolve a
    /// single handle through the full residual + recovery path.
    async fn resolve_one(conn: &Connection, context_id: &str, handle: &str) -> ReadRecord {
        let records = resolve_handle_records(conn, context_id, &[handle.to_string()], None)
            .await
            .unwrap();
        records.into_iter().next().unwrap()
    }

    #[tokio::test]
    async fn resolve_handle_recovers_unique_prefix_and_discloses_source() {
        let db = Db::open_memory().await.unwrap();
        let conn = db.connect().unwrap();
        schema::initialize(&conn).await.unwrap();

        let ctx = "ctx-recovery";
        segments::upsert_segment_for_context(
            &conn,
            ctx,
            &test_segment(RECOVERY_TRUE_ID, "src/a.rs"),
        )
        .await
        .unwrap();

        let record = resolve_one(&conn, ctx, RECOVERY_SUPPLIED).await;
        assert_eq!(record.status, ReadStatus::Found);
        assert_eq!(
            record.segment.as_ref().unwrap().handle,
            RECOVERY_TRUE_ID,
            "recovery hydrates the intended segment"
        );
        assert_eq!(
            record.recovered_from.as_deref(),
            Some(RECOVERY_SUPPLIED),
            "a recovered record discloses the supplied handle"
        );
        assert!(
            record.message.is_some(),
            "recovery is explained in the message"
        );
    }

    #[tokio::test]
    async fn resolve_handle_recovery_never_guesses_a_tie() {
        let db = Db::open_memory().await.unwrap();
        let conn = db.connect().unwrap();
        schema::initialize(&conn).await.unwrap();

        let ctx = "ctx-recovery-tie";
        let a = format!("{}f{}", &RECOVERY_SUPPLIED[..8], "a".repeat(23));
        let b = format!("{}f{}", &RECOVERY_SUPPLIED[..8], "b".repeat(23));
        segments::upsert_segment_for_context(&conn, ctx, &test_segment(&a, "src/a.rs"))
            .await
            .unwrap();
        segments::upsert_segment_for_context(&conn, ctx, &test_segment(&b, "src/b.rs"))
            .await
            .unwrap();

        let record = resolve_one(&conn, ctx, RECOVERY_SUPPLIED).await;
        assert_eq!(record.status, ReadStatus::Ambiguous);
        assert!(
            record.segment.is_none(),
            "an ambiguous handle is never hydrated"
        );
        assert_eq!(record.matching_handles.len(), 2);
        assert!(record.recovered_from.is_none());
    }

    #[tokio::test]
    async fn resolve_handle_recovery_is_context_scoped() {
        // The intended segment lives only in a foreign context, so the supplied
        // handle must never recover in the active context.
        let db = Db::open_memory().await.unwrap();
        let conn = db.connect().unwrap();
        schema::initialize(&conn).await.unwrap();

        segments::upsert_segment_for_context(
            &conn,
            "ctx-foreign",
            &test_segment(RECOVERY_TRUE_ID, "src/a.rs"),
        )
        .await
        .unwrap();

        let record = resolve_one(&conn, "ctx-active", RECOVERY_SUPPLIED).await;
        assert_eq!(record.status, ReadStatus::NotFound);
        assert!(record.recovered_from.is_none());
    }

    #[tokio::test]
    async fn resolve_handle_without_a_floor_prefix_candidate_stays_not_found() {
        let db = Db::open_memory().await.unwrap();
        let conn = db.connect().unwrap();
        schema::initialize(&conn).await.unwrap();

        let ctx = "ctx-miss";
        segments::upsert_segment_for_context(
            &conn,
            ctx,
            &test_segment(RECOVERY_TRUE_ID, "src/a.rs"),
        )
        .await
        .unwrap();

        // Shares no floor prefix with any indexed id.
        let record = resolve_one(&conn, ctx, "ffffffffa316205a1afe69ccd1137e2").await;
        assert_eq!(record.status, ReadStatus::NotFound);
        assert!(record.recovered_from.is_none());
    }

    // Failed-handle retry memory gate. Exercised on a fresh local
    // `FailedHandleMemory` rather than the process-global one so the matrix is
    // deterministic and parallel-safe.

    /// Fabricate a distinct, comparable index identity in a platform-agnostic
    /// way (real ids come from `index_file_identity`; the gate only compares
    /// them for equality).
    #[cfg(unix)]
    fn fake_identity(seed: u64) -> Option<IndexFileIdentity> {
        Some((1, seed))
    }
    #[cfg(not(unix))]
    fn fake_identity(seed: u64) -> Option<IndexFileIdentity> {
        Some((seed, std::time::SystemTime::UNIX_EPOCH))
    }

    fn memory_key(handle: &str) -> FailedHandleKey {
        (
            PathBuf::from("/tmp/index.db"),
            "ctx".to_string(),
            handle.to_string(),
        )
    }

    #[test]
    fn failed_handle_memory_rejects_identical_failure_under_same_identity() {
        let mut memory = FailedHandleMemory::default();
        let key = memory_key("deadbeefdeadbeef");
        let candidates = vec!["deadbeefdeadbeef1111".to_string()];
        memory.record_failure(
            key.clone(),
            fake_identity(7),
            ReadStatus::Ambiguous,
            candidates.clone(),
        );

        let hit = memory
            .lookup(&key, fake_identity(7))
            .expect("an identical failure under the same identity is a memory hit");
        assert_eq!(hit.outcome, ReadStatus::Ambiguous);
        assert_eq!(
            hit.matching_handles, candidates,
            "the rejection carries the cached candidate ids for disambiguation"
        );
        // The entry survives the reject (a retry is rejected without re-query,
        // not consumed), so a second identical retry is still rejected.
        assert!(memory.lookup(&key, fake_identity(7)).is_some());
    }

    #[test]
    fn failed_handle_memory_drops_stale_entry_on_identity_change() {
        let mut memory = FailedHandleMemory::default();
        let key = memory_key("deadbeefdeadbeef");
        memory.record_failure(
            key.clone(),
            fake_identity(1),
            ReadStatus::NotFound,
            Vec::new(),
        );

        // A build-aside swap installed a fresh index: the mismatched identity
        // drops the stale entry and declines to reject, so the handle resolves
        // fresh.
        assert!(memory.lookup(&key, fake_identity(2)).is_none());
        // And the now-dropped entry no longer rejects even at the original
        // identity.
        assert!(memory.lookup(&key, fake_identity(1)).is_none());
    }

    #[test]
    fn failed_handle_memory_clear_forgets_a_prior_failure() {
        let mut memory = FailedHandleMemory::default();
        let key = memory_key("deadbeefdeadbeef");
        memory.record_failure(
            key.clone(),
            fake_identity(1),
            ReadStatus::NotFound,
            Vec::new(),
        );
        memory.clear(&key);
        assert!(
            memory.lookup(&key, fake_identity(1)).is_none(),
            "a success clears the entry, so the next identical call resolves fresh"
        );
    }

    #[test]
    fn failed_handle_memory_evicts_oldest_over_cap() {
        let mut memory = FailedHandleMemory::default();
        // Insert one past the cap; the very first (oldest) entry is evicted.
        for i in 0..=FAILED_HANDLE_MEMORY_CAP {
            memory.record_failure(
                memory_key(&format!("handle-{i:04}")),
                fake_identity(1),
                ReadStatus::NotFound,
                Vec::new(),
            );
        }
        assert_eq!(memory.entries.len(), FAILED_HANDLE_MEMORY_CAP);
        assert!(
            memory
                .lookup(&memory_key("handle-0000"), fake_identity(1))
                .is_none(),
            "the oldest entry is evicted first once over the cap"
        );
        assert!(
            memory
                .lookup(
                    &memory_key(&format!("handle-{:04}", FAILED_HANDLE_MEMORY_CAP)),
                    fake_identity(1)
                )
                .is_some(),
            "the newest entry survives eviction"
        );
    }

    /// Per-item baseline (relocated from the pre-batching `resolve_handle_record`):
    /// resolve one handle by exact id, then by prefix. The batched
    /// `resolve_handle_records` must match this field-for-field.
    async fn resolve_handle_record_per_item(
        conn: &Connection,
        context_id: &str,
        raw_handle: &str,
        verbosity: Option<&str>,
    ) -> anyhow::Result<ReadRecord> {
        use crate::storage::segments::get_segment_by_id_for_context;

        let normalized = normalize_handle(raw_handle);
        let source = ReadSource::Handle {
            raw: raw_handle.to_string(),
            normalized: normalized.clone(),
        };

        if normalized.is_empty() {
            return Ok(read_message(
                ReadStatus::NotFound,
                source,
                "empty segment handle",
            ));
        }

        if let Some(segment) = get_segment_by_id_for_context(conn, context_id, &normalized).await? {
            return Ok(read_segment(source, segment, verbosity));
        }

        resolve_handle_via_prefix(conn, context_id, source, &normalized, verbosity).await
    }

    fn test_segment(id: &str, file_path: &str) -> SegmentInsert {
        SegmentInsert {
            id: id.to_string(),
            file_path: file_path.to_string(),
            language: "rust".to_string(),
            block_type: "function".to_string(),
            content: format!("fn {id}() {{}}"),
            line_start: 1,
            line_end: 1,
            content_key: None,
            embedding_vec: None,
            breadcrumb: None,
            complexity: 1,
            role: "DEFINITION".to_string(),
            defined_symbols: format!("[\"{id}\"]"),
            referenced_symbols: "[]".to_string(),
            referenced_relations: "[]".to_string(),
            called_symbols: "[]".to_string(),
            called_relations: "[]".to_string(),
            file_hash: format!("hash-{id}"),
        }
    }

    #[test]
    fn test_apply_scope_to_indexing_config_empty_scope() {
        let mut config = IndexingConfig {
            jobs: 4,
            embed_threads: 2,
            write_batch_files: 100,
            include_globs: vec![],
            exclude_globs: vec![],
            index_hidden_dirs: vec![],
            scope_roots: vec![],
            scope_globs: vec![],
        };
        let result = apply_scope_to_indexing_config(&mut config, &[]);
        assert!(result.is_ok());
        assert!(config.scope_globs.is_empty());
    }

    #[test]
    fn test_apply_scope_to_indexing_config_single_root() {
        let mut config = IndexingConfig {
            jobs: 4,
            embed_threads: 2,
            write_batch_files: 100,
            include_globs: vec![],
            exclude_globs: vec![],
            index_hidden_dirs: vec![],
            scope_roots: vec![],
            scope_globs: vec![],
        };
        let scope = vec!["services/auth".to_string()];
        let result = apply_scope_to_indexing_config(&mut config, &scope);
        assert!(result.is_ok());
        assert_eq!(config.scope_globs, vec!["services/auth/**"]);
    }

    #[test]
    fn test_apply_scope_to_indexing_config_multiple_roots() {
        let mut config = IndexingConfig {
            jobs: 4,
            embed_threads: 2,
            write_batch_files: 100,
            include_globs: vec![],
            exclude_globs: vec![],
            index_hidden_dirs: vec![],
            scope_roots: vec![],
            scope_globs: vec![],
        };
        let scope = vec!["services/auth".to_string(), "libs/core".to_string()];
        let result = apply_scope_to_indexing_config(&mut config, &scope);
        assert!(result.is_ok());
        assert_eq!(config.scope_globs, vec!["services/auth/**", "libs/core/**"]);
    }

    #[tokio::test]
    async fn test_compute_new_scope_with_scope_add() {
        // Test: scope_add with no existing scope creates new scope
        use tempfile::TempDir;
        let temp_dir = TempDir::new().unwrap();
        let scope_add = Some(vec!["services/auth".to_string()]);
        let result = compute_new_scope(temp_dir.path(), scope_add.clone(), None)
            .await
            .unwrap();
        assert_eq!(result, vec!["services/auth"]);
    }

    #[tokio::test]
    async fn test_compute_new_scope_with_scope_narrow_empty_current() {
        // Test: scope_narrow on empty current scope (should work)
        use tempfile::TempDir;
        let temp_dir = TempDir::new().unwrap();
        let scope_narrow = Some(vec!["services/auth".to_string()]);
        let result = compute_new_scope(temp_dir.path(), None, scope_narrow)
            .await
            .unwrap();
        assert_eq!(result, vec!["services/auth"]);
    }

    #[test]
    fn test_scope_add_validation_rejects_absolute_paths() {
        // This test documents the validation in compute_new_scope
        let absolute_path = "/services/auth".to_string();
        let result: anyhow::Result<Vec<String>> = if absolute_path.starts_with('/') {
            Err(anyhow::anyhow!(
                "scope path cannot be absolute: {}",
                absolute_path
            ))
        } else {
            Ok(vec![absolute_path])
        };
        assert!(result.is_err());
    }

    #[test]
    fn test_scope_add_validation_rejects_escape_sequences() {
        // This test documents the validation in compute_new_scope
        let escape_path = "services/../admin".to_string();
        let result: anyhow::Result<Vec<String>> = if escape_path.contains("..") {
            Err(anyhow::anyhow!(
                "scope path cannot contain '..': {}",
                escape_path
            ))
        } else {
            Ok(vec![escape_path])
        };
        assert!(result.is_err());
    }

    #[test]
    fn index_scope_serializes_and_deserializes() {
        use crate::shared::types::IndexScope;

        let scope = IndexScope {
            roots: vec!["services/auth".to_string(), "libs/core".to_string()],
            indexed_files: 150,
            total_files: 2500,
            eligibility_note: None,
        };

        let json = serde_json::to_string(&scope).expect("should serialize");
        let deserialized: IndexScope = serde_json::from_str(&json).expect("should deserialize");

        assert_eq!(deserialized.roots, scope.roots);
        assert_eq!(deserialized.indexed_files, scope.indexed_files);
        assert_eq!(deserialized.total_files, scope.total_files);
    }

    #[test]
    fn index_scope_coverage_description_for_empty_scope() {
        use crate::shared::types::IndexScope;

        let scope = IndexScope {
            roots: vec![],
            indexed_files: 0,
            total_files: 1000,
            eligibility_note: None,
        };

        assert_eq!(scope.coverage_description(), "No scope configured");
    }

    #[test]
    fn index_scope_coverage_description_calculates_percentage() {
        use crate::shared::types::IndexScope;

        let scope = IndexScope {
            roots: vec!["services/auth".to_string()],
            indexed_files: 150,
            total_files: 600,
            eligibility_note: None,
        };

        let description = scope.coverage_description();
        assert!(description.contains("150 files indexed of 600 total"));
        assert!(description.contains("25%"));
    }

    #[test]
    fn index_scope_coverage_description_handles_zero_total() {
        use crate::shared::types::IndexScope;

        let scope = IndexScope {
            roots: vec!["services/auth".to_string()],
            indexed_files: 0,
            total_files: 0,
            eligibility_note: None,
        };

        let description = scope.coverage_description();
        assert!(description.contains("0 files indexed of 0 total"));
        assert!(description.contains("0%"));
    }

    #[test]
    fn index_scope_eligibility_note_populated_for_unscoped_index() {
        use crate::shared::types::IndexScope;

        let scope = IndexScope {
            roots: vec![],
            indexed_files: 149,
            total_files: 210,
            eligibility_note: Some(
                "Full index (no scope recorded). Indexed files = code and doc files. \
                 Total files = all git-tracked files + gitignore-excluded files walked for statistics."
                    .to_string(),
            ),
        };

        assert!(scope.eligibility_note.is_some());
        let note = scope.eligibility_note.unwrap();
        assert!(note.contains("Full index"));
        assert!(note.contains("no scope recorded"));
        assert!(note.contains("Indexed files"));
        assert!(note.contains("Total files"));
    }

    #[test]
    fn unscoped_eligibility_note_populated_only_for_empty_roots() {
        let note = unscoped_eligibility_note(&[]).expect("unscoped index must carry a note");
        assert!(!note.is_empty());
        assert!(note.contains("Full index"));
        assert!(note.contains("no scope recorded"));

        assert!(unscoped_eligibility_note(&["services/auth".to_string()]).is_none());
    }

    #[test]
    fn index_scope_eligibility_note_absent_for_scoped_index() {
        use crate::shared::types::IndexScope;

        let scope = IndexScope {
            roots: vec!["services/auth".to_string(), "libs/core".to_string()],
            indexed_files: 150,
            total_files: 2500,
            eligibility_note: None,
        };

        assert!(scope.eligibility_note.is_none());
    }

    #[test]
    fn resolve_project_captures_launch_subdir_when_invoked_from_subdirectory() {
        let temp_dir = tempfile::tempdir().unwrap();
        let repo_root = temp_dir.path().canonicalize().unwrap();
        let services_dir = repo_root.join("services");
        let auth_dir = services_dir.join("auth");
        std::fs::create_dir_all(&auth_dir).unwrap();
        std::fs::create_dir_all(repo_root.join(".git")).unwrap();

        // Invoke from subdirectory
        let roots = resolve_project(&auth_dir).expect("should resolve project");

        // Verify launch_subdir is captured
        assert_eq!(roots.source_root, repo_root);
        assert!(roots.launch_subdir.is_some());
        let launch_subdir = roots.launch_subdir.unwrap();
        assert_eq!(launch_subdir, auth_dir);
    }

    #[test]
    fn resolve_project_sets_launch_subdir_none_when_invoked_from_project_root() {
        let temp_dir = tempfile::tempdir().unwrap();
        let repo_root = temp_dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(repo_root.join(".git")).unwrap();

        // Invoke from project root
        let roots = resolve_project(&repo_root).expect("should resolve project");

        // Verify launch_subdir is None when invoked from root
        assert_eq!(roots.source_root, repo_root);
        assert!(roots.launch_subdir.is_none());
    }

    #[test]
    fn extract_scope_from_progress_extracts_scope_info() {
        use crate::shared::types::{BranchStatus, IndexPhase, IndexScopeInfo};
        use chrono::Utc;

        // Create a progress with scope info
        let progress = IndexProgress {
            state: IndexState::Running,
            phase: IndexPhase::Scanning,
            context_id: Some("test".to_string()),
            source_root: None,
            branch_name: None,
            branch_status: Some(BranchStatus::Unknown),
            files_total: 100,
            files_scanned: 50,
            files_processed: 25,
            files_indexed: 20,
            files_skipped: 5,
            files_deleted: 0,
            segments_stored: 100,
            embeddings_enabled: true,
            embedding_unavailable_reason: None,
            vector_rows: None,
            embeddable_segments: None,
            message: None,
            parallelism: None,
            timings: None,
            scope: Some(IndexScopeInfo {
                requested: "scoped:2".to_string(),
                executed: "scoped:50".to_string(),
                changed_paths: 50,
                fallback_reason: None,
                roots: vec!["services/auth".to_string(), "libs/core".to_string()],
            }),
            prefilter: None,
            indexer_pid: None,
            run_id: None,
            updated_at: Utc::now(),
        };

        // Extract scope from progress
        let scope = extract_scope_from_progress(&progress);

        // Verify scope is extracted correctly
        assert!(scope.is_some());
        let scope = scope.unwrap();
        // Roots should now be populated from IndexScopeInfo during indexing
        assert_eq!(
            scope.roots,
            vec!["services/auth".to_string(), "libs/core".to_string()]
        );
        assert_eq!(scope.indexed_files, 20);
        assert_eq!(scope.total_files, 100);
    }

    #[test]
    fn extract_scope_from_progress_returns_none_when_no_scope_info() {
        use crate::shared::types::{BranchStatus, IndexPhase};
        use chrono::Utc;

        // Create a progress without scope info
        let progress = IndexProgress {
            state: IndexState::Running,
            phase: IndexPhase::Scanning,
            context_id: Some("test".to_string()),
            source_root: None,
            branch_name: None,
            branch_status: Some(BranchStatus::Unknown),
            files_total: 100,
            files_scanned: 50,
            files_processed: 25,
            files_indexed: 20,
            files_skipped: 5,
            files_deleted: 0,
            segments_stored: 100,
            embeddings_enabled: true,
            embedding_unavailable_reason: None,
            vector_rows: None,
            embeddable_segments: None,
            message: None,
            parallelism: None,
            timings: None,
            scope: None, // No scope info
            prefilter: None,
            indexer_pid: None,
            run_id: None,
            updated_at: Utc::now(),
        };

        // Extract scope from progress
        let scope = extract_scope_from_progress(&progress);

        // Verify scope is None when no scope info
        assert!(scope.is_none());
    }

    #[test]
    fn progress_with_idle_state_is_not_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let status_path = tmp.path().join("index_status.json");

        let progress = IndexProgress {
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
            indexer_pid: Some(std::process::id()),
            run_id: None,
            updated_at: Utc::now(),
        };

        // Idle state should never be stale regardless of process or file age
        assert!(!is_index_progress_stale(&progress, &status_path));
    }

    #[test]
    fn running_progress_with_live_process_is_not_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let status_path = tmp.path().join("index_status.json");

        // Create the status file so we can check its mtime
        fs::write(&status_path, "{}").unwrap();

        let progress = IndexProgress {
            state: IndexState::Running,
            phase: IndexPhase::Scanning,
            context_id: None,
            source_root: None,
            branch_name: None,
            branch_status: None,
            files_total: 100,
            files_scanned: 50,
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
            indexer_pid: Some(std::process::id()), // Current process is alive
            run_id: None,
            updated_at: Utc::now(),
        };

        // Running state with a live process should not be stale
        assert!(!is_index_progress_stale(&progress, &status_path));
    }

    #[test]
    fn running_progress_without_pid_is_not_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let status_path = tmp.path().join("index_status.json");
        fs::write(&status_path, "{}").unwrap();

        let progress = IndexProgress {
            state: IndexState::Running,
            phase: IndexPhase::Scanning,
            context_id: None,
            source_root: None,
            branch_name: None,
            branch_status: None,
            files_total: 100,
            files_scanned: 50,
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
            indexer_pid: None, // No PID recorded
            run_id: None,
            updated_at: Utc::now(),
        };

        // Running state without a PID can't be checked for liveness, so not stale
        assert!(!is_index_progress_stale(&progress, &status_path));
    }

    #[tokio::test]
    async fn spawn_rebuild_task_spawns_non_blocking() {
        use std::time::Instant;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let state_root = tmp.path().to_path_buf();
        let source_root = tmp.path().to_path_buf();

        // Create minimal project structure
        std::fs::create_dir_all(state_root.join(".1up")).unwrap();

        // Create a minimal WorktreeContext
        let worktree_context = WorktreeContext {
            context_id: "test".to_string(),
            state_root: state_root.clone(),
            source_root: source_root.clone(),
            main_worktree_root: source_root.clone(),
            worktree_role: crate::shared::types::WorktreeRole::Main,
            git_dir: None,
            common_git_dir: None,
            branch_name: None,
            branch_ref: None,
            head_oid: None,
            branch_status: crate::shared::types::BranchStatus::Unknown,
        };

        let roots = McpProjectRoots {
            state_root: state_root.clone(),
            source_root: source_root.clone(),
            worktree_context,
            launch_subdir: None,
        };

        // Measure time to call spawn_rebuild_task
        let start = Instant::now();
        let _rebuild_handle = spawn_rebuild_task(&roots, true, None, None, next_rebuild_run_id());
        let elapsed = start.elapsed();

        // Should return almost immediately, not await the full pipeline
        // (which would take seconds or more). A spawned task should return
        // in just a few milliseconds.
        assert!(
            elapsed.as_millis() < 100,
            "spawn_rebuild_task should return immediately, took {} ms",
            elapsed.as_millis()
        );

        // Give spawned task a brief moment to initialize
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    /// Minimal roots over a temp dir for exercising the progress-file
    /// publication helpers directly. Canonicalized because the secure-fs
    /// write path refuses symlinked components (macOS `/var` → `/private/var`).
    fn scope_progress_test_roots(state_root: &Path) -> McpProjectRoots {
        let state_root = &state_root.canonicalize().unwrap();
        std::fs::create_dir_all(state_root.join(".1up")).unwrap();
        let worktree_context = WorktreeContext {
            context_id: "test-context".to_string(),
            state_root: state_root.to_path_buf(),
            source_root: state_root.to_path_buf(),
            main_worktree_root: state_root.to_path_buf(),
            worktree_role: crate::shared::types::WorktreeRole::Main,
            git_dir: None,
            common_git_dir: None,
            branch_name: None,
            branch_ref: None,
            head_oid: None,
            branch_status: crate::shared::types::BranchStatus::Unknown,
        };
        McpProjectRoots {
            state_root: state_root.to_path_buf(),
            source_root: state_root.to_path_buf(),
            worktree_context,
            launch_subdir: None,
        }
    }

    /// The pre-spawn publication `ops::start` performs (fail-loud, before
    /// `spawn_rebuild_task`) must persist the EXACT requested scope in a
    /// `Running` snapshot owned by this process. This pins the publication
    /// contents deterministically, independent of rebuild-task scheduling.
    #[tokio::test]
    async fn write_initial_scope_progress_publishes_exact_requested_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = scope_progress_test_roots(tmp.path());
        let requested = vec!["services/auth".to_string(), "libs/core".to_string()];

        write_initial_scope_progress(&roots, &requested, "run-a")
            .await
            .expect("publication must succeed on a healthy state root");

        let progress = read_index_progress(&roots.state_root)
            .await
            .expect("published progress must be readable");
        assert_eq!(progress.state, IndexState::Running);
        assert_eq!(progress.indexer_pid, Some(std::process::id()));
        assert_eq!(progress.run_id.as_deref(), Some("run-a"));
        assert_eq!(progress.context_id.as_deref(), Some("test-context"));
        let scope = progress.scope.expect("published progress must carry scope");
        assert_eq!(scope.roots, requested);
    }

    /// A publication failure must make the scoped start Blocked WITHOUT
    /// spawning a rebuild: `index_status.json` planted as a directory makes
    /// the atomic write fail deterministically, and the returned reason
    /// proves the pre-spawn branch (not a later rebuild failure) fired.
    #[tokio::test]
    async fn publication_write_failure_blocks_scoped_start_without_spawning() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = scope_progress_test_roots(tmp.path());
        std::fs::create_dir_all(roots.state_root.join(".1up").join(INDEX_PROGRESS_FILE_NAME))
            .unwrap();

        let payload = start(
            &roots,
            StartMode::IndexIfMissing,
            Some(vec!["services/auth".to_string()]),
            None,
        )
        .await
        .expect("start itself must not error");

        assert_eq!(
            payload.status,
            ReadinessStatus::Blocked,
            "a scoped start whose scope cannot be recorded must be blocked"
        );
        let reason = payload.reason.unwrap_or_default();
        assert!(
            reason.contains("failed to record the requested index scope"),
            "the blocked reason must identify the pre-spawn publication branch \
             (proving no rebuild was spawned); got: {reason}"
        );
        // A spawned rebuild would have auto-initialized the project id within
        // its first milliseconds; its continued absence corroborates that the
        // publication failure prevented the spawn entirely.
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(
            project::read_project_id(&roots.state_root).is_err(),
            "no rebuild may run after a failed scope publication"
        );
    }

    /// A rebuild failure after the pre-spawn publication must not strand the
    /// persisted `Running` snapshot: the recorded PID is this long-lived
    /// process, so stale-progress repair can never reclaim it on its own.
    #[tokio::test]
    async fn failed_rebuild_transitions_owned_running_progress_to_terminal() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = scope_progress_test_roots(tmp.path());
        let requested = vec!["services/auth".to_string()];
        write_initial_scope_progress(&roots, &requested, "run-a")
            .await
            .unwrap();

        record_rebuild_failure_progress(
            &roots,
            "run-a",
            RebuildLockHeld::No,
            "registry load failed",
        )
        .await;

        let progress = read_index_progress(&roots.state_root)
            .await
            .expect("terminal progress must be readable");
        assert_eq!(
            progress.state,
            IndexState::Failed,
            "failed rebuild must record the terminal Failed state, not stay Running"
        );
        assert_eq!(progress.indexer_pid, None);
        assert!(
            progress
                .message
                .as_deref()
                .unwrap_or_default()
                .contains("registry load failed"),
            "terminal snapshot must carry the failure reason; got {:?}",
            progress.message
        );
        let scope = progress.scope.expect("failed attempt keeps its scope");
        assert_eq!(
            scope.roots, requested,
            "scope of the failed attempt is preserved"
        );
    }

    /// An older run's failure cleanup must never overwrite a record published
    /// by a NEWER overlapping start in the same process: both share this PID,
    /// so ownership keys on the per-run identity.
    #[tokio::test]
    async fn older_runs_failure_cleanup_leaves_newer_starts_record_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = scope_progress_test_roots(tmp.path());
        // The newer start (run-b) published after run-a's snapshot.
        write_initial_scope_progress(&roots, &["services/auth".to_string()], "run-b")
            .await
            .unwrap();

        // The older run-a now fails outside the rebuild lock.
        record_rebuild_failure_progress(&roots, "run-a", RebuildLockHeld::No, "boom").await;

        let progress = read_index_progress(&roots.state_root)
            .await
            .expect("the newer run's record must survive");
        assert_eq!(
            progress.state,
            IndexState::Running,
            "run-a's failure must not clobber run-b's Running record"
        );
        assert_eq!(progress.run_id.as_deref(), Some("run-b"));
    }

    /// Outside the rebuild lock, a `run_id`-less `Running` record (a pipeline
    /// write) is not provably the failed run's, so strict cleanup must leave
    /// it alone; under the lock the same record can only be the lock-holder's
    /// own pipeline record and must be reclaimed.
    #[tokio::test]
    async fn pipeline_record_cleanup_requires_holding_the_rebuild_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = scope_progress_test_roots(tmp.path());
        let pipeline_record = IndexProgress {
            state: IndexState::Running,
            context_id: Some("test-context".to_string()),
            indexer_pid: Some(std::process::id()),
            run_id: None,
            ..IndexProgress::pending()
        };
        write_index_progress_atomic(&roots.state_root, &pipeline_record)
            .await
            .unwrap();

        record_rebuild_failure_progress(&roots, "run-a", RebuildLockHeld::No, "boom").await;
        let progress = read_index_progress(&roots.state_root).await.unwrap();
        assert_eq!(
            progress.state,
            IndexState::Running,
            "without the lock, an identity-less pipeline record must not be claimed"
        );

        record_rebuild_failure_progress(&roots, "run-a", RebuildLockHeld::Yes, "boom").await;
        let progress = read_index_progress(&roots.state_root).await.unwrap();
        assert_eq!(
            progress.state,
            IndexState::Failed,
            "under the lock, the holder's own pipeline record must be reclaimed"
        );
    }

    /// The ownership check must be atomic with the write it guards: a newer
    /// start's publication (which never takes the rebuild lock) must not be
    /// able to land between an older failure cleanup's read-check and its
    /// terminal write, or the stale `Failed` snapshot would overwrite the
    /// newer run's `Running` record. This drives that exact interleaving via
    /// the test gate: run-a's cleanup is paused after its ownership check,
    /// run-b publishes concurrently, and the newer record must win.
    #[tokio::test]
    async fn stale_failure_cleanup_cannot_clobber_a_concurrent_newer_publication() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = scope_progress_test_roots(tmp.path());
        write_initial_scope_progress(&roots, &["services/auth".to_string()], "run-a")
            .await
            .unwrap();

        // Pause run-a's cleanup between its ownership check (which passes:
        // the record IS run-a's right now) and its terminal write.
        let (reached, proceed) = arm_cleanup_pause_for_test(&roots.state_root);
        let cleanup_roots = roots.clone();
        let cleanup = tokio::spawn(async move {
            record_rebuild_failure_progress(&cleanup_roots, "run-a", RebuildLockHeld::No, "boom")
                .await;
        });
        reached.await.expect("cleanup must reach its paused write");

        // A newer start (run-b) publishes while the cleanup's check is stale.
        // It must block until the in-flight cleanup finishes its write.
        let publish_roots = roots.clone();
        let mut publication = tokio::spawn(async move {
            write_initial_scope_progress(&publish_roots, &["libs/core".to_string()], "run-b").await
        });
        let raced =
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut publication).await;
        assert!(
            raced.is_err(),
            "run-b's publication must not land inside run-a's read-check-write window"
        );

        proceed.send(()).expect("paused cleanup must be waiting");
        cleanup.await.unwrap();
        publication.await.unwrap().unwrap();

        let progress = read_index_progress(&roots.state_root)
            .await
            .expect("the newer run's record must survive");
        assert_eq!(
            progress.state,
            IndexState::Running,
            "run-a's stale failure write must not clobber run-b's Running record"
        );
        assert_eq!(progress.run_id.as_deref(), Some("run-b"));
    }

    /// A persisted `Failed` record must keep the failure visible to readiness
    /// classification: with no usable index it classifies Blocked (not
    /// Missing), carrying the recorded reason.
    #[tokio::test]
    async fn failed_progress_classifies_blocked_when_no_usable_index_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = scope_progress_test_roots(tmp.path());
        write_initial_scope_progress(&roots, &["services/auth".to_string()], "run-a")
            .await
            .unwrap();
        record_rebuild_failure_progress(
            &roots,
            "run-a",
            RebuildLockHeld::No,
            "staging open failed",
        )
        .await;

        let payload = classify_readiness(
            &roots.state_root,
            &roots.source_root,
            &roots.worktree_context,
        )
        .await;

        assert_eq!(
            payload.status,
            ReadinessStatus::Blocked,
            "a recorded rebuild failure with no usable index must classify Blocked, not Missing"
        );
        assert!(
            payload
                .reason
                .as_deref()
                .unwrap_or_default()
                .contains("staging open failed"),
            "readiness must carry the recorded failure reason; got {:?}",
            payload.reason
        );
    }

    /// A `Failed` record over a still-usable index degrades readiness (the
    /// previous index keeps serving discovery) instead of reporting Ready as
    /// if the failure never happened; a later run's writes supersede it.
    #[tokio::test]
    async fn failed_progress_degrades_readiness_over_usable_index() {
        use crate::storage::segments::{
            replace_file_segments_for_context_tx_with_meta, upsert_worktree_context,
            IndexedFileMeta, SegmentInsert,
        };

        let tmp = tempfile::tempdir().unwrap();
        let roots = scope_progress_test_roots(tmp.path());
        std::fs::write(
            roots.state_root.join(".1up").join("project_id"),
            "failed-progress-project",
        )
        .unwrap();

        // Seed a minimal usable index for the test context.
        let db = Db::open_rw(&project_db_path(&roots.state_root))
            .await
            .unwrap();
        let conn = db.connect().unwrap();
        schema::initialize(&conn).await.unwrap();
        upsert_worktree_context(&conn, &roots.worktree_context, "failed-progress-project")
            .await
            .unwrap();
        let segment = SegmentInsert {
            id: "failed-progress-segment".to_string(),
            file_path: "src/lib.rs".to_string(),
            language: "rust".to_string(),
            block_type: "function".to_string(),
            content: "pub fn seeded() {}\n".to_string(),
            line_start: 1,
            line_end: 1,
            content_key: None,
            embedding_vec: None,
            breadcrumb: None,
            complexity: 1,
            role: "source".to_string(),
            defined_symbols: "[]".to_string(),
            referenced_symbols: "[]".to_string(),
            referenced_relations: "[]".to_string(),
            called_symbols: "[]".to_string(),
            called_relations: "[]".to_string(),
            file_hash: "seeded-hash".to_string(),
        };
        let meta = IndexedFileMeta {
            extension: "rs".to_string(),
            file_hash: segment.file_hash.clone(),
            file_size: segment.content.len() as i64,
            modified_ns: 1,
        };
        replace_file_segments_for_context_tx_with_meta(
            &conn,
            &roots.worktree_context.context_id,
            "src/lib.rs",
            &[segment],
            Some(&meta),
        )
        .await
        .unwrap();
        drop(conn);
        drop(db);

        write_initial_scope_progress(&roots, &["services/auth".to_string()], "run-a")
            .await
            .unwrap();
        record_rebuild_failure_progress(&roots, "run-a", RebuildLockHeld::No, "swap failed").await;

        let payload = classify_readiness(
            &roots.state_root,
            &roots.source_root,
            &roots.worktree_context,
        )
        .await;

        assert_eq!(
            payload.status,
            ReadinessStatus::Degraded,
            "a recorded rebuild failure over a usable index must degrade, not report Ready"
        );
        assert!(
            payload
                .reason
                .as_deref()
                .unwrap_or_default()
                .contains("swap failed"),
            "readiness must carry the recorded failure reason; got {:?}",
            payload.reason
        );
        assert!(
            payload.index_readable,
            "the previous index must still be reported usable"
        );
    }

    /// End-to-end over the spawn path: a rebuild that fails after the
    /// pre-spawn publication must surface Blocked AND durably transition the
    /// persisted snapshot out of `Running` — otherwise `oneup_status` reports
    /// a phantom indexing run forever, because the snapshot's PID (this
    /// long-lived process) defeats stale-progress repair.
    #[tokio::test]
    async fn failed_spawned_rebuild_returns_blocked_and_clears_running_progress() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = scope_progress_test_roots(tmp.path());
        let requested = vec!["services/auth".to_string()];
        let run_id = next_rebuild_run_id();
        write_initial_scope_progress(&roots, &requested, &run_id)
            .await
            .unwrap();
        // Force a deterministic rebuild failure: `index.db` as a directory can
        // be neither opened nor staged.
        std::fs::create_dir_all(roots.state_root.join(".1up").join("index.db")).unwrap();

        let payload = spawn_rebuild_task(&roots, true, Some(requested), None, run_id)
            .await
            .expect("rebuild task must not panic the join handle");

        assert_eq!(
            payload.status,
            ReadinessStatus::Blocked,
            "a failed rebuild must surface as blocked readiness"
        );
        let progress = read_index_progress(&roots.state_root)
            .await
            .expect("progress must remain readable after a failed rebuild");
        assert_ne!(
            progress.state,
            IndexState::Running,
            "a failed rebuild must not leave the persisted snapshot claiming Running"
        );
        // The durable failure must also survive CLASSIFICATION: a later status
        // call reports Blocked (with the reason), not Missing/Ready — the
        // failure may not silently disappear from readiness.
        let classified = classify_readiness(
            &roots.state_root,
            &roots.source_root,
            &roots.worktree_context,
        )
        .await;
        assert_eq!(
            classified.status,
            ReadinessStatus::Blocked,
            "a later status call must keep reporting the recorded failure"
        );
    }

    /// A rebuild that PANICS after the pre-spawn publication must behave like
    /// an error: blocked readiness with the panic reason, and a durable
    /// terminal record replacing the `Running` snapshot.
    #[tokio::test]
    async fn panicking_spawned_rebuild_returns_blocked_and_records_terminal_state() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = scope_progress_test_roots(tmp.path());
        let requested = vec!["services/auth".to_string()];
        let run_id = next_rebuild_run_id();
        write_initial_scope_progress(&roots, &requested, &run_id)
            .await
            .unwrap();
        arm_rebuild_panic_for_test(&roots.state_root);

        let payload = spawn_rebuild_task(&roots, true, Some(requested), None, run_id)
            .await
            .expect("the panic must be contained inside the task");

        assert_eq!(
            payload.status,
            ReadinessStatus::Blocked,
            "a panicking rebuild must surface as blocked readiness"
        );
        assert!(
            payload
                .reason
                .as_deref()
                .unwrap_or_default()
                .contains("test-injected rebuild panic"),
            "the blocked reason must carry the panic message; got {:?}",
            payload.reason
        );
        let progress = read_index_progress(&roots.state_root)
            .await
            .expect("progress must remain readable after a panicked rebuild");
        assert_eq!(
            progress.state,
            IndexState::Failed,
            "a panicking rebuild must durably record the terminal Failed state"
        );
    }

    /// A `Running` record stamped by ANOTHER process (a concurrent daemon or
    /// CLI indexer) is not ours to clean up; the failure recorder must leave
    /// it untouched.
    #[tokio::test]
    async fn failed_rebuild_leaves_foreign_running_progress_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = scope_progress_test_roots(tmp.path());
        let foreign = IndexProgress {
            state: IndexState::Running,
            context_id: Some("test-context".to_string()),
            indexer_pid: Some(std::process::id().wrapping_add(1)),
            ..IndexProgress::pending()
        };
        write_index_progress_atomic(&roots.state_root, &foreign)
            .await
            .unwrap();

        record_rebuild_failure_progress(&roots, "run-a", RebuildLockHeld::Yes, "boom").await;

        let progress = read_index_progress(&roots.state_root)
            .await
            .expect("foreign progress must still be readable");
        assert_eq!(progress.state, IndexState::Running);
        assert_eq!(
            progress.indexer_pid,
            Some(std::process::id().wrapping_add(1)),
            "another indexer's Running record must not be clobbered"
        );
    }

    #[test]
    fn test_density_table_has_expected_entries() {
        let table = get_density_table();
        // N2: Verify calibrated density table exists and has measured values
        assert!(
            table.iter().any(|(ext, _)| *ext == "rs"),
            "Rust density must be present"
        );
        assert!(
            table.iter().any(|(ext, _)| *ext == "java"),
            "Java density must be present"
        );

        // Verify measured densities are reasonable
        if let Some((_, rust_density)) = table.iter().find(|(ext, _)| *ext == "rs") {
            assert!(
                *rust_density > 35.0 && *rust_density < 40.0,
                "Rust density 37.02 expected"
            );
        }
    }

    #[test]
    fn test_per_directory_vector_estimates_consistent_with_global() {
        // N2: Verify per-directory estimates use same calibration as global estimate
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // Create a simple Rust repository structure
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src").join("main.rs"), "fn main() {}").unwrap();
        fs::write(root.join("src").join("lib.rs"), "pub fn lib() {}").unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\nname=\"test\"\n").unwrap();

        // Compute global estimate
        let (global_estimate, _basis, _low, _high) =
            estimate_vector_count(3, root, root).expect("estimate should work");

        // Compute per-directory density
        let avg_density =
            compute_avg_density_for_repo(root, root).expect("density computation should work");

        // Per-file estimate should match global density (3 files * avg_density)
        let per_dir_estimate = (3.0 * avg_density) as usize;
        assert_eq!(
            per_dir_estimate, global_estimate,
            "per-directory estimate should match global total when summed"
        );
    }

    #[test]
    fn test_density_cache_hits_on_second_call() {
        // FIX B: Verify density computation caches result to avoid re-walking
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // Create a simple repository structure
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src").join("main.rs"), "fn main() {}").unwrap();
        fs::write(root.join("src").join("lib.rs"), "pub fn lib() {}").unwrap();

        // First call should compute and populate the cache. Assert cache
        // STATE rather than wall-clock timing: a duration assertion flakes
        // under full parallel suite load regardless of cache behavior.
        let density1 = compute_avg_density_for_repo(root, root)
            .expect("first density computation should work");

        let canonical_root = root.canonicalize().unwrap();
        let cached_entries = get_density_cache()
            .lock()
            .map(|cache| {
                cache
                    .keys()
                    .filter(|key| key.repo_identity == canonical_root.to_string_lossy())
                    .count()
            })
            .unwrap();
        assert!(
            cached_entries > 0,
            "first density computation should populate the cache for this repo"
        );

        // Second call must return the identical cached value.
        let density2 = compute_avg_density_for_repo(root, root)
            .expect("second density computation should work");
        assert_eq!(
            density1, density2,
            "density should be consistent across calls"
        );
    }

    /// Builds the persistent density cache key string for a repo through the
    /// production key builder, so a key-shape change can never silently
    /// diverge these tests from compute_avg_density_for_repo.
    fn density_cache_key_str(root: &Path) -> String {
        cache_key_to_string(&build_directory_walk_cache_key(root))
    }

    /// Drops in-process density entries for `root` so a subsequent call
    /// behaves like a cold (fresh-process) lookup.
    fn clear_in_process_density_entries(root: &Path) {
        let identity = root
            .canonicalize()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| root.to_string_lossy().into_owned());
        if let Ok(mut cache) = get_density_cache().lock() {
            cache.retain(|key, _| key.repo_identity != identity);
        }
    }

    /// Runs a git command in `root`, asserting success.
    fn run_git(root: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .expect("git should be runnable in tests");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn test_persistent_density_cache_hit_avoids_walk() {
        // Fixes #87: a persisted entry keyed to the current repo state is used
        // without re-walking. Seed a sentinel value distinct from any real walk
        // result and assert it is returned verbatim.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src").join("main.rs"), "fn main() {}").unwrap();
        // .1up must exist for the persistent cache to be written.
        fs::create_dir_all(root.join(".1up")).unwrap();

        let sentinel = 123.5_f64;
        let mut entries = HashMap::new();
        entries.insert(density_cache_key_str(root), sentinel);
        save_persistent_density_cache(root, &entries);

        let density =
            compute_avg_density_for_repo(root, root).expect("density computation should work");
        assert_eq!(
            density, sentinel,
            "a key-matched persisted entry must be returned without walking"
        );
    }

    #[test]
    fn test_stale_persistent_density_entry_falls_back_and_rewrites() {
        // Fixes #87: a persisted entry under a non-matching (stale) key must miss,
        // forcing a fresh walk, and the fresh result must be written under the
        // current key.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src").join("main.rs"), "fn main() {}").unwrap();
        fs::write(root.join("src").join("lib.rs"), "pub fn lib() {}").unwrap();
        fs::create_dir_all(root.join(".1up")).unwrap();

        let sentinel = 999.0_f64;
        let stale_key = format!("{}_stale_head_0", root.to_string_lossy());
        let mut entries = HashMap::new();
        entries.insert(stale_key.clone(), sentinel);
        save_persistent_density_cache(root, &entries);

        let density =
            compute_avg_density_for_repo(root, root).expect("density computation should work");
        assert_ne!(
            density, sentinel,
            "a stale-keyed entry must not be used; a fresh walk should run"
        );
        assert!(
            density > 0.0,
            "fresh walk should produce a positive density; got {}",
            density
        );

        // The fresh result must be persisted under the current key (rewrite),
        // while the stale entry is preserved untouched.
        let reloaded = load_persistent_density_cache(root);
        let current_key = density_cache_key_str(root);
        assert_eq!(
            reloaded.get(&current_key).copied(),
            Some(density),
            "fresh walk result must be rewritten under the current key"
        );
        assert_eq!(
            reloaded.get(&stale_key).copied(),
            Some(sentinel),
            "the stale entry should remain in the persisted map"
        );
    }

    #[test]
    fn test_corrupt_persistent_density_cache_degrades_gracefully() {
        // Fixes #87: an unparseable cache file must not fail the request; the
        // function degrades to a fresh walk.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src").join("main.rs"), "fn main() {}").unwrap();
        fs::create_dir_all(root.join(".1up")).unwrap();
        fs::write(
            root.join(".1up").join("density_cache.json"),
            "{ not valid json ]]]",
        )
        .unwrap();

        let density = compute_avg_density_for_repo(root, root)
            .expect("corrupt cache must degrade to a walk, not error");
        assert!(
            density > 0.0,
            "fresh walk should produce a positive density; got {}",
            density
        );
    }

    #[test]
    fn test_split_roots_persist_density_under_state_root() {
        // Linked git worktrees keep `.1up` under the main worktree's
        // state_root, not under the walked source_root. The persistent density
        // cache must save to and load from state_root even when the two roots
        // differ; persisting at source_root would miss on every cold process.
        let tmp = tempfile::tempdir().unwrap();
        let source_root = tmp.path().join("source");
        let state_root = tmp.path().join("state");

        fs::create_dir_all(source_root.join("src")).unwrap();
        fs::write(source_root.join("src").join("main.rs"), "fn main() {}").unwrap();
        // `.1up` exists only under state_root (linked-worktree shape); the
        // save guard requires the directory to pre-exist.
        fs::create_dir_all(state_root.join(".1up")).unwrap();

        let density = compute_avg_density_for_repo(&state_root, &source_root)
            .expect("density computation should work");
        assert!(density > 0.0, "walk should produce a positive density");

        // The persistent save must land under state_root/.1up, and must not
        // create `.1up` (or a cache file) under source_root.
        assert!(
            state_root.join(".1up").join("density_cache.json").exists(),
            "persistent density cache must be written under state_root/.1up"
        );
        assert!(
            !source_root.join(".1up").exists(),
            "no .1up must be created under source_root"
        );

        // Simulate a fresh process: drop the in-process entry, seed a sentinel
        // under the current key at state_root, and verify the cold lookup hits
        // the state_root file (returned verbatim, without re-walking).
        clear_in_process_density_entries(&source_root);
        let sentinel = 222.25_f64;
        let mut entries = load_persistent_density_cache(&state_root);
        entries.insert(density_cache_key_str(&source_root), sentinel);
        save_persistent_density_cache(&state_root, &entries);

        let cold = compute_avg_density_for_repo(&state_root, &source_root)
            .expect("cold density computation should work");
        assert_eq!(
            cold, sentinel,
            "a cold lookup must hit the persistent cache at state_root"
        );
    }

    #[test]
    fn test_dirty_worktree_declines_persistent_density_reuse() {
        // The persistent key (identity + HEAD + root mtime) cannot see nested
        // working-tree changes: adding a file under src/ moves neither HEAD
        // nor the root mtime. A dirty worktree must therefore decline
        // persistent reuse (load and save) and recompute.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src").join("main.rs"), "fn main() {}").unwrap();
        // Ignore .1up so writing the cache file itself does not dirty the tree.
        fs::write(root.join(".gitignore"), ".1up/\n").unwrap();
        run_git(root, &["init", "-q"]);
        run_git(root, &["add", "-A"]);
        run_git(
            root,
            &[
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.com",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "-m",
                "seed",
            ],
        );
        fs::create_dir_all(root.join(".1up")).unwrap();

        // Clean worktree: a key-matched persisted sentinel is reused cold.
        let sentinel = 321.5_f64;
        let seeded_key = density_cache_key_str(root);
        let mut entries = HashMap::new();
        entries.insert(seeded_key.clone(), sentinel);
        save_persistent_density_cache(root, &entries);
        clear_in_process_density_entries(root);

        let clean = compute_avg_density_for_repo(root, root)
            .expect("clean density computation should work");
        assert_eq!(
            clean, sentinel,
            "a clean worktree must allow persistent reuse"
        );

        // Mutate a nested directory WITHOUT moving HEAD or the root mtime:
        // the cache key stays identical, but the worktree is now dirty.
        fs::write(root.join("src").join("extra.py"), "x = 1\n").unwrap();
        clear_in_process_density_entries(root);

        let dirty = compute_avg_density_for_repo(root, root)
            .expect("dirty density computation should work");
        assert_ne!(
            dirty, sentinel,
            "a dirty worktree must decline persistent reuse and recompute"
        );
        assert!(
            dirty > 0.0,
            "dirty recompute should produce a positive density; got {}",
            dirty
        );

        // The dirty recompute must not be persisted either: the key cannot
        // vouch for the dirty state, so the seeded entry stays untouched.
        let reloaded = load_persistent_density_cache(root);
        assert_eq!(
            reloaded.get(&seeded_key).copied(),
            Some(sentinel),
            "a dirty recompute must not overwrite the persisted entry"
        );
    }

    #[test]
    fn test_directory_walk_excludes_git() {
        // N1: Verify .git directory is excluded from counts
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // Create a tracked file in src/ directory
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src").join("main.rs"), "fn main() {}").unwrap();

        // Create .git directory with many files (should be excluded)
        fs::create_dir_all(root.join(".git").join("objects")).unwrap();
        for i in 0..10 {
            fs::write(
                root.join(".git").join("objects").join(format!("obj_{}", i)),
                "",
            )
            .unwrap();
        }

        // Count files per directory
        let counts = count_files_per_directory(root).expect("count should work");

        // .git should NOT appear in the counts (N1)
        assert!(
            !counts.contains_key(".git"),
            "N1: .git directory should be excluded from counts; got {:?}",
            counts
        );

        // src directory should have files counted
        assert!(
            counts.get("src").map(|c| *c == 1).unwrap_or(false),
            "N1: src should have 1 file counted; got {:?}",
            counts
        );

        // Verify total is just 1 (not 11 with .git files)
        let total: usize = counts.values().sum();
        assert_eq!(
            total, 1,
            "N1: should count only 1 file (not include .git); got {:?}",
            counts
        );
    }

    #[tokio::test]
    async fn test_generate_facts_envelope_excludes_dot_directories() {
        // Verify that tool/editor dot-directories are filtered from
        // per_directory_stats in the facts envelope. The envelope is used for
        // scope suggestions; dot-directories are noise and should not appear.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // Create a repo structure with real directories and tool/editor dot-directories
        // Real directories (should appear in stats):
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(root.join("src/lib.rs"), "pub mod utils;").unwrap();

        fs::create_dir_all(root.join("libs")).unwrap();
        fs::write(root.join("libs/helper.rs"), "pub fn help() {}").unwrap();

        // Tool/editor dot-directories (should be EXCLUDED from stats):
        fs::create_dir_all(root.join(".idea")).unwrap();
        for i in 0..3 {
            fs::write(
                root.join(".idea").join(format!("config_{}", i)),
                "IDE config",
            )
            .unwrap();
        }

        fs::create_dir_all(root.join(".vscode")).unwrap();
        fs::write(root.join(".vscode/settings.json"), "{}").unwrap();
        fs::write(root.join(".vscode/launch.json"), "{}").unwrap();

        fs::create_dir_all(root.join(".1up")).unwrap();
        fs::write(root.join(".1up/cache.db"), "cache").unwrap();
        fs::write(root.join(".1up/meta.json"), "{}").unwrap();

        fs::create_dir_all(root.join(".agentdocs")).unwrap();
        fs::write(root.join(".agentdocs/index.md"), "# Docs").unwrap();

        fs::create_dir_all(root.join(".claude")).unwrap();
        fs::write(root.join(".claude/config.json"), "{}").unwrap();

        // Generate the facts envelope
        let envelope = generate_facts_envelope(root, root, None)
            .await
            .expect("facts envelope should be generated");

        // Verify real directories are present
        let dir_names: Vec<&str> = envelope
            .per_directory_stats
            .iter()
            .map(|s| s.directory.as_str())
            .collect();

        assert!(
            dir_names.contains(&"src"),
            "src directory should appear in per_directory_stats; got {:?}",
            dir_names
        );

        assert!(
            dir_names.contains(&"libs"),
            "libs directory should appear in per_directory_stats; got {:?}",
            dir_names
        );

        // Verify NO dot-directories appear (acceptance criterion 3)
        for dir in &envelope.per_directory_stats {
            assert!(
                !dir.directory.starts_with('.'),
                "Dot-directory should be filtered out; found: {}",
                dir.directory
            );
        }

        // Verify specific excluded dot-directories are NOT present
        let excluded = [".idea", ".claude", ".vscode", ".1up", ".agentdocs"];
        for excluded_dir in &excluded {
            assert!(
                !dir_names.contains(excluded_dir),
                "Excluded dot-directory {} should not appear; got {:?}",
                excluded_dir,
                dir_names
            );
        }

        // Verify ordering: largest real directory comes first (src has 2 files, libs has 1)
        assert_eq!(
            envelope.per_directory_stats[0].directory, "src",
            "Largest real directory (src) should be first after filtering and sorting"
        );
        assert_eq!(
            envelope.per_directory_stats[0].file_count, 2,
            "src should have 2 files"
        );

        assert_eq!(
            envelope.per_directory_stats[1].directory, "libs",
            "Second largest directory (libs) should be second"
        );
        assert_eq!(
            envelope.per_directory_stats[1].file_count, 1,
            "libs should have 1 file"
        );
    }

    fn ranked_stats(directories: &[&str]) -> Vec<DirectoryStats> {
        directories
            .iter()
            .enumerate()
            .map(|(idx, directory)| DirectoryStats {
                directory: directory.to_string(),
                file_count: 100 - idx * 10,
                estimated_vectors: (100 - idx * 10) * 10,
            })
            .collect()
    }

    #[test]
    fn test_ranked_suggestions_no_leading_or_when_top_dir_is_launch_subdir() {
        // When the launch_subdir is the top-ranked directory, it is skipped and
        // every remaining scope suggestion reads as an alternative (the launch
        // action is the implied primary). No reason begins with "Or ".
        let stats = ranked_stats(&["services", "libs", "tools"]);
        let launch_subdir = Some("services".to_string());

        let suggestions = generate_ranked_suggestions(&stats, &launch_subdir);

        assert_eq!(
            suggestions,
            vec![
                "Alternatively, index the 2nd largest directory: libs".to_string(),
                "Alternatively, index the 3rd largest directory: tools".to_string(),
            ]
        );
    }

    #[test]
    fn test_ranked_suggestions_without_launch_subdir_first_is_primary() {
        // No launch_subdir: the first suggestion is the primary imperative and
        // the rest are alternatives. Multiple ranked suggestions, none with a
        // leading "Or ".
        let stats = ranked_stats(&["services", "libs", "tools"]);

        let suggestions = generate_ranked_suggestions(&stats, &None);

        assert_eq!(
            suggestions,
            vec![
                "Index the largest directory: services".to_string(),
                "Alternatively, index the 2nd largest directory: libs".to_string(),
                "Alternatively, index the 3rd largest directory: tools".to_string(),
            ]
        );
    }

    #[test]
    fn test_ranked_suggestions_single_directory() {
        let stats = ranked_stats(&["services"]);

        let suggestions = generate_ranked_suggestions(&stats, &None);

        assert_eq!(
            suggestions,
            vec!["Index the largest directory: services".to_string()]
        );
    }

    #[test]
    fn test_ranked_suggestions_skip_second_ranked_launch_subdir() {
        // When the launch_subdir is a non-top-ranked directory it is skipped;
        // because a launch_subdir is present, the remaining scope suggestions
        // all read as alternatives with truthful ordinals and no leading "Or ".
        let stats = ranked_stats(&["services", "libs", "tools"]);
        let launch_subdir = Some("libs".to_string());

        let suggestions = generate_ranked_suggestions(&stats, &launch_subdir);

        assert_eq!(
            suggestions,
            vec![
                "Alternatively, index the largest directory: services".to_string(),
                "Alternatively, index the 3rd largest directory: tools".to_string(),
            ]
        );
    }

    #[test]
    fn test_ranked_scope_suggestions_pair_directory_with_reason() {
        // Structured suggestions carry both the target directory (for scope_add)
        // and the coherent reason, sharing the display source of truth.
        let stats = ranked_stats(&["services", "libs", "tools"]);

        let suggestions = generate_ranked_scope_suggestions(&stats, &None);

        assert_eq!(
            suggestions,
            vec![
                ScopeSuggestion {
                    directory: "services".to_string(),
                    reason: "Index the largest directory: services".to_string(),
                },
                ScopeSuggestion {
                    directory: "libs".to_string(),
                    reason: "Alternatively, index the 2nd largest directory: libs".to_string(),
                },
                ScopeSuggestion {
                    directory: "tools".to_string(),
                    reason: "Alternatively, index the 3rd largest directory: tools".to_string(),
                },
            ]
        );

        // The display strings are derived from the same structured source.
        let display = generate_ranked_suggestions(&stats, &None);
        let reasons: Vec<String> = suggestions.into_iter().map(|s| s.reason).collect();
        assert_eq!(display, reasons);
    }

    // --- Persisted scope proposal (daemon gate-fired surface, issue #86) ---

    fn sample_proposal(key: &str) -> PersistedScopeProposal {
        PersistedScopeProposal {
            key: key.to_string(),
            per_directory_stats: vec![
                PersistedDirectoryStat {
                    directory: "services".to_string(),
                    file_count: 2000,
                },
                PersistedDirectoryStat {
                    directory: "libs".to_string(),
                    file_count: 800,
                },
            ],
            file_count_total: 2800,
        }
    }

    #[test]
    fn scope_proposal_persist_load_round_trips() {
        let temp = tempfile::tempdir().unwrap();
        let state_root = canonical_state_root(&temp);
        std::fs::create_dir_all(project_dot_dir(&state_root)).unwrap();

        let proposal = sample_proposal("repo_HEAD1_1000");
        save_persisted_scope_proposal(&state_root, &proposal).unwrap();

        let loaded = load_persisted_scope_proposal(&state_root).expect("proposal round-trips");
        assert_eq!(loaded, proposal);
    }

    #[test]
    fn scope_proposal_save_is_noop_without_dot_dir() {
        // The gated path must never create .1up as a side effect.
        let temp = tempfile::tempdir().unwrap();
        let state_root = canonical_state_root(&temp);

        save_persisted_scope_proposal(&state_root, &sample_proposal("k")).unwrap();

        assert!(!project_dot_dir(&state_root).exists());
        assert!(load_persisted_scope_proposal(&state_root).is_none());
    }

    #[test]
    fn cancelled_directory_walk_returns_err_not_counts() {
        // The proposal walk observes the daemon pass's cancellation token; a
        // cancelled walk must surface as an error, never as partial (or empty)
        // per-directory counts that downstream code could persist as a
        // proposal. The cadence check runs at idx 0, so a pre-cancelled token
        // aborts deterministically on the first walked entry.
        let temp = tempfile::tempdir().unwrap();
        let source_root = canonical_state_root(&temp);
        std::fs::create_dir_all(source_root.join("services")).unwrap();
        for i in 0..5 {
            std::fs::write(source_root.join("services").join(format!("f{i}.rs")), "x").unwrap();
        }

        let cancel_token = CancellationToken::new();
        cancel_token.cancel();

        let result = count_files_per_directory_cancellable(&source_root, &cancel_token);
        assert!(
            result.is_err(),
            "a pre-cancelled walk must return Err, got {result:?}"
        );
    }

    #[test]
    fn persist_scope_proposal_skips_persist_when_cancelled() {
        // A SIGTERM-cancelled proposal walk is a benign shutdown outcome: the
        // persist call reports Ok (nothing for the daemon to warn about) and
        // writes NO scope_proposal.json — a proposal from an aborted walk
        // would rank cones from a partial count.
        let temp = tempfile::tempdir().unwrap();
        let state_root = canonical_state_root(&temp);
        std::fs::create_dir_all(project_dot_dir(&state_root)).unwrap();
        std::fs::create_dir_all(state_root.join("services")).unwrap();
        for i in 0..5 {
            std::fs::write(state_root.join("services").join(format!("f{i}.rs")), "x").unwrap();
        }

        let cancel_token = CancellationToken::new();
        cancel_token.cancel();

        persist_scope_proposal_for_gate(&state_root, &state_root, &cancel_token)
            .expect("a cancelled proposal walk is not an error");

        assert!(
            !project_dot_dir(&state_root)
                .join(SCOPE_PROPOSAL_FILENAME)
                .exists(),
            "a cancelled walk must not persist a proposal"
        );
        assert!(load_persisted_scope_proposal(&state_root).is_none());
    }

    #[test]
    fn attach_scope_proposal_surfaces_ranked_suggestions_when_fresh() {
        let temp = tempfile::tempdir().unwrap();
        let source_root = temp.path().join("repo");
        std::fs::create_dir_all(&source_root).unwrap();
        let source_root = source_root.canonicalize().unwrap();
        let state_root = source_root.clone();
        std::fs::create_dir_all(project_dot_dir(&state_root)).unwrap();

        // Persist a proposal keyed by the CURRENT walk-cache key so it is fresh.
        let current_key = cache_key_to_string(&build_directory_walk_cache_key(&source_root));
        save_persisted_scope_proposal(&state_root, &sample_proposal(&current_key)).unwrap();

        let mut payload = blocked_readiness_for_path("repo", "fixture");
        attach_scope_proposal_if_fresh(&mut payload, &state_root, &source_root);

        let proposal = payload.scope_proposal.expect("fresh proposal attaches");
        assert_eq!(proposal.file_count_total, 2800);
        assert_eq!(
            proposal.scope_candidates,
            vec!["services".to_string(), "libs".to_string()]
        );
        // Suggestions are rebuilt from the persisted stats (largest first).
        assert_eq!(
            proposal.suggestions.first().map(String::as_str),
            Some("Index the largest directory: services")
        );
    }

    #[test]
    fn attach_scope_proposal_is_noop_when_stale() {
        let temp = tempfile::tempdir().unwrap();
        let source_root = temp.path().join("repo");
        std::fs::create_dir_all(&source_root).unwrap();
        let source_root = source_root.canonicalize().unwrap();
        let state_root = source_root.clone();
        std::fs::create_dir_all(project_dot_dir(&state_root)).unwrap();

        // A key that cannot match the current repo state -> stale -> fall back.
        save_persisted_scope_proposal(&state_root, &sample_proposal("stale_key_xyz")).unwrap();

        let mut payload = blocked_readiness_for_path("repo", "fixture");
        attach_scope_proposal_if_fresh(&mut payload, &state_root, &source_root);

        assert!(payload.scope_proposal.is_none());
    }

    #[test]
    fn attach_scope_proposal_is_noop_when_absent() {
        let temp = tempfile::tempdir().unwrap();
        let source_root = temp.path().join("repo");
        std::fs::create_dir_all(&source_root).unwrap();
        let source_root = source_root.canonicalize().unwrap();
        let state_root = source_root.clone();
        std::fs::create_dir_all(project_dot_dir(&state_root)).unwrap();

        let mut payload = blocked_readiness_for_path("repo", "fixture");
        attach_scope_proposal_if_fresh(&mut payload, &state_root, &source_root);

        assert!(payload.scope_proposal.is_none());
    }

    // --- Walk-cache key sensitivity (real components behind the freshness gate) ---

    /// Bumps the source root's mtime past whole-second granularity (the walk
    /// cache key stores seconds) without a wall-clock dependency: sets an
    /// explicit future mtime through std's `File::set_modified`, falling back
    /// to a >1s sleep plus a direct-child touch on platforms where opening a
    /// directory handle fails (e.g. Windows).
    fn bump_root_mtime_past_second(repo: &Path, baseline_secs: u64) {
        let target = SystemTime::UNIX_EPOCH + Duration::from_secs(baseline_secs + 5);
        let bumped = std::fs::File::open(repo)
            .and_then(|dir| dir.set_modified(target))
            .is_ok();
        if !bumped {
            std::thread::sleep(Duration::from_millis(1100));
            fs::write(repo.join("mtime_fallback_touch"), "x").unwrap();
        }
    }

    /// The freshness matrix above exercises `is_scope_proposal_fresh` with
    /// synthetic strings; this pins the REAL key builder to each component:
    /// a direct-child change moves the root-mtime component, a new commit
    /// moves the HEAD component, and distinct roots yield distinct identities.
    #[test]
    fn walk_cache_key_tracks_real_head_mtime_and_repo_identity() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        let repo = repo.canonicalize().unwrap();

        run_git(&repo, &["init"]);
        run_git(&repo, &["config", "user.email", "test@example.com"]);
        run_git(&repo, &["config", "user.name", "Test User"]);
        fs::write(repo.join("seed.txt"), "seed").unwrap();
        run_git(&repo, &["add", "."]);
        run_git(&repo, &["commit", "-m", "initial"]);

        let baseline_key = build_directory_walk_cache_key(&repo);
        let baseline = cache_key_to_string(&baseline_key);
        let baseline_mtime = baseline_key.root_mtime.expect("root mtime readable");

        // (a) Root mtime component: create a direct child of the source root
        // (which touches the root directory's mtime) and push the mtime past
        // second granularity so the seconds-resolution key must move.
        fs::write(repo.join("new_child.txt"), "content").unwrap();
        bump_root_mtime_past_second(&repo, baseline_mtime);
        let after_mtime_key = build_directory_walk_cache_key(&repo);
        assert_ne!(
            baseline_key.root_mtime, after_mtime_key.root_mtime,
            "direct-child change must move the root mtime component"
        );
        assert_ne!(
            baseline,
            cache_key_to_string(&after_mtime_key),
            "root mtime drift must change the stringified key"
        );

        // (b) HEAD component: advance HEAD with a real commit. Commits write
        // under .git/ (not a direct child of the root), so against the
        // post-mtime key only the HEAD component moves.
        run_git(&repo, &["add", "."]);
        run_git(&repo, &["commit", "-m", "advance HEAD"]);
        let after_commit_key = build_directory_walk_cache_key(&repo);
        assert_ne!(
            after_mtime_key.head_commit, after_commit_key.head_commit,
            "a new commit must move the HEAD component"
        );
        assert_ne!(
            cache_key_to_string(&after_mtime_key),
            cache_key_to_string(&after_commit_key),
            "HEAD drift must change the stringified key"
        );

        // (c) Repo identity component: two distinct roots produce distinct
        // keys regardless of their HEAD/mtime state.
        let other = temp.path().join("other");
        fs::create_dir_all(&other).unwrap();
        let other = other.canonicalize().unwrap();
        let other_key = build_directory_walk_cache_key(&other);
        assert_ne!(
            after_commit_key.repo_identity, other_key.repo_identity,
            "distinct roots must have distinct repo identities"
        );
        assert_ne!(
            cache_key_to_string(&after_commit_key),
            cache_key_to_string(&other_key),
            "distinct roots must produce distinct stringified keys"
        );
    }
}
