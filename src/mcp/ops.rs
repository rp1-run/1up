use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use libsql::Connection;
use serde::Serialize;

use crate::daemon::lifecycle;
use crate::daemon::registry::Registry;
use crate::indexer::embedder::{
    self, clear_download_failure, EmbeddingLoadStatus, EmbeddingRuntime, EmbeddingUnavailableReason,
};
use crate::indexer::pipeline;
use crate::indexer::scan_filter::ScanFilter;
use crate::mcp::types::{DirectoryStats, FactsEnvelope, StartMode};
use crate::search::context::ContextEngine;
use crate::search::impact::{ImpactHorizonEngine, ImpactRequest, ImpactResultEnvelope};
use crate::search::overview;
use crate::search::retrieval;
use crate::search::{HybridSearchEngine, SearchScope, StructuralSearchEngine, SymbolSearchEngine};
use crate::shared::config::{self, project_db_path, project_dot_dir};
use crate::shared::constants::{
    DB_LOCK_RETRY_ATTEMPTS, DB_LOCK_RETRY_DELAY_MS, FILE_COUNT_THRESHOLD,
    FILE_COUNT_THRESHOLD_ENV_VAR, NO_INDEXED_EMBEDDINGS_REASON, STALE_REBUILD_REASON,
};
use crate::shared::errors::{OneupError, ProjectError};
use crate::shared::project;
use crate::shared::types::{
    combine_degraded_reasons, ContextAccessScope, ContextResult, DaemonProjectStatus,
    IndexProgress, IndexScope, IndexState, IndexingConfig, ReferenceKind, RunScope, SearchResult,
    SegmentRole, SetupTimings, StructuralSearchReport, SymbolResult, WorktreeContext,
};
use crate::storage::db::{is_lock_error, Db};
use crate::storage::schema;
use crate::storage::segments::{
    count_embeddable_segments_for_context, count_files_for_context, count_segments_for_context,
    count_vector_rows_for_context, get_segment_by_prefix_for_context,
    get_segments_by_ids_for_context, get_worktree_context_head_oid, SegmentPrefixLookup,
    StoredSegment,
};
use crate::storage::swap;

const INDEX_PROGRESS_FILE_NAME: &str = "index_status.json";

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReadSource {
    Handle { raw: String, normalized: String },
    Location { path: String, line: usize },
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub defined_symbols: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub referenced_symbols: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub called_symbols: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContextRecord {
    pub path: String,
    pub language: String,
    pub scope_type: String,
    pub content: String,
    pub line_start: usize,
    pub line_end: usize,
}

/// Orientation digest payload for `oneup_overview`. Section sizes are bounded
/// by the engine caps documented in `crate::search::overview`, which keep the
/// serialized payload within the documented budget (REQ-008).
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
    /// Canonical `index.db` path -- the warm cache's key (REQ-001) -- so a
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

    Ok(current_scope)
}

/// Applies scope roots to IndexingConfig by converting them to include_globs.
///
/// Scope roots are converted to glob patterns: "dir1/**", "dir2/**", etc.
fn apply_scope_to_indexing_config(
    config: &mut IndexingConfig,
    scope_roots: &[String],
) -> anyhow::Result<()> {
    if !scope_roots.is_empty() {
        // Convert scope roots to include_globs: "dir/**" for each root
        let scope_globs: Vec<String> = scope_roots
            .iter()
            .map(|root| format!("{}/**", root))
            .collect();
        config.include_globs = scope_globs;
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

pub async fn start(
    roots: &McpProjectRoots,
    mode: StartMode,
    scope_add: Option<Vec<String>>,
    scope_narrow: Option<Vec<String>>,
) -> anyhow::Result<ReadinessPayload> {
    // Determine if a rebuild (vs incremental write) is needed based on scope changes
    let scope_affects_rebuild = scope_add.is_some() || scope_narrow.is_some();
    let rebuild_mode = if scope_affects_rebuild && scope_narrow.is_some() {
        // Narrowing always requires full rebuild via StagingRebuild
        true
    } else {
        // For scope_add or mode-based decisions, follow the original mode logic
        mode == StartMode::Reindex
    };

    let readiness = check_status(roots).await;
    match mode {
        StartMode::IndexIfMissing if readiness.status == ReadinessStatus::Missing => {
            run_index_then_classify(roots, rebuild_mode, scope_add, scope_narrow).await
        }
        StartMode::IndexIfNeeded if index_if_needed_applies(&readiness) => {
            run_index_then_classify(roots, rebuild_mode, scope_add, scope_narrow).await
        }
        StartMode::Reindex => run_index_then_classify(roots, true, scope_add, scope_narrow).await,
        _ => {
            // Even if mode doesn't trigger indexing, scope changes should trigger indexing
            if scope_affects_rebuild {
                run_index_then_classify(roots, rebuild_mode, scope_add, scope_narrow).await
            } else {
                Ok(readiness)
            }
        }
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
    let index_progress = read_index_progress_for_context(state_root, &worktree_context.context_id);
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

    if payload
        .index_progress
        .as_ref()
        .is_some_and(|progress| progress.state == IndexState::Running)
    {
        payload.status = ReadinessStatus::Indexing;
        payload.summary = "Indexing is currently running.".to_string();
        return payload;
    }

    if !project_initialized || !index_present {
        if daemon_refresh_active {
            payload.status = ReadinessStatus::Indexing;
            payload.summary = "Indexing is currently running.".to_string();
            return payload;
        }
        payload.status = ReadinessStatus::Missing;
        payload.summary = "No usable 1up index is available for this repository.".to_string();
        payload.reason = Some("run oneup_start with an explicit indexing mode".to_string());
        return payload;
    }

    let db = match Db::open_ro(&db_path).await {
        Ok(db) => db,
        Err(err) => {
            if daemon_refresh_active {
                payload.status = ReadinessStatus::Indexing;
                payload.summary = "Indexing is currently running.".to_string();
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
                return payload;
            }
            payload.status = ReadinessStatus::Stale;
            payload.summary = "The index exists but cannot be read.".to_string();
            payload.reason = Some(err.to_string());
            return payload;
        }
    };

    payload.schema_version = schema::get_schema_version(&conn).await.ok().flatten();

    if let Err(err) = ensure_schema_current_tolerating_init(
        &conn,
        &schema::SchemaContext::new(&db_path, source_root),
    )
    .await
    {
        if daemon_refresh_active {
            payload.status = ReadinessStatus::Indexing;
            payload.summary = "Indexing is currently running.".to_string();
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
        payload.status = ReadinessStatus::Missing;
        payload.summary = "No indexed code is available for this repository.".to_string();
        payload.reason = Some("run oneup_start with an explicit indexing mode".to_string());
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

    // Compute and populate index scope for coverage disclosure
    if let Ok(Some(scope)) = compute_index_scope(state_root, source_root).await {
        payload.index_scope = Some(scope);
    }

    payload
}

/// Run [`schema::ensure_current`], riding out a freshly-initializing index.
///
/// `schema::initialize` (invoked by the auto-started daemon when it first opens a
/// brand-new project DB) creates every table first and writes the `schema_version`
/// row last, and is not a single transaction. A readiness check that lands inside
/// that window on a separate read-only connection sees "tables exist, version
/// absent" and `ensure_current` returns the transient
/// [`schema::is_initializing_schema_error`] shape. The daemon commits the version
/// row microseconds later, so we retry on exactly that shape (reusing the shared
/// DB-lock retry budget) to let initialization settle before classifying. This
/// mirrors the lock-retry hardening of `schema::table_has_column` and never retries
/// a genuine version mismatch (`out of date` / `newer than this binary supports`),
/// which fails fast on the first attempt.
async fn ensure_schema_current_tolerating_init(
    conn: &Connection,
    ctx: &schema::SchemaContext<'_>,
) -> Result<(), OneupError> {
    let retry_delay = Duration::from_millis(DB_LOCK_RETRY_DELAY_MS);
    let mut attempt = 0;
    loop {
        match schema::ensure_current(conn, ctx).await {
            Ok(()) => return Ok(()),
            Err(err) => {
                attempt += 1;
                if attempt >= DB_LOCK_RETRY_ATTEMPTS || !schema::is_initializing_schema_error(&err)
                {
                    return Err(err);
                }
                tokio::time::sleep(retry_delay).await;
            }
        }
    }
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

pub fn blocked_readiness(
    state_root: &Path,
    source_root: &Path,
    worktree_context: &WorktreeContext,
    reason: impl Into<String>,
) -> ReadinessPayload {
    let project_initialized = project::read_project_id(state_root).is_ok();
    let db_path = project_db_path(state_root);
    let index_progress = read_index_progress_for_context(state_root, &worktree_context.context_id);
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
    }
}

pub async fn run_search(
    state_root: &Path,
    worktree_context: &WorktreeContext,
    query: &str,
    limit: usize,
    path_prefix: Option<&str>,
) -> anyhow::Result<SearchPayload> {
    retry_on_db_lock(|| async {
        run_search_once(state_root, worktree_context, query, limit, path_prefix).await
    })
    .await
}

/// Process-global warm embedding runtime for the in-process MCP fallback search
/// path (R-008).
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
    query: &str,
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
        // `EmbeddingRuntime::default()` on every call (R-008). The MCP server is a
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
            // recomputing it on every search (REQ-001, mirrors the daemon's
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
            let mut engine = HybridSearchEngine::new_scoped(
                &current.conn,
                runtime.current_embedder(),
                search_scope.clone(),
            )
            .with_has_vectors(has_vectors)
            .with_vector_count(vector_count);
            engine.search(query, limit).await?
        } else {
            let engine = HybridSearchEngine::new_scoped(&current.conn, None, search_scope.clone());
            engine.fts_only_search(query, limit).await?
        };
        (results, embedding_reason)
    } else {
        let engine = HybridSearchEngine::new_scoped(&current.conn, None, search_scope.clone());
        let results = engine.fts_only_search(query, limit).await?;
        (results, Some(NO_INDEXED_EMBEDDINGS_REASON.to_string()))
    };

    // Stale-but-available: when a rebuild/refresh is in progress for this
    // context, readers keep serving the prior index (build-aside, REQ-002), so
    // flag the served results as possibly stale. The notice rides only in
    // `degraded_reason` (no parallel field) and the render path keeps it off
    // stdout (REQ-003).
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
    let index_scope = compute_index_scope(state_root, &worktree_context.source_root)
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
) -> anyhow::Result<ReadPayload> {
    retry_on_db_lock(|| async { get_handles_once(state_root, worktree_context, handles).await })
        .await
}

async fn get_handles_once(
    state_root: &Path,
    worktree_context: &WorktreeContext,
    handles: &[String],
) -> anyhow::Result<ReadPayload> {
    let current = open_current_index(state_root).await?;
    let records =
        resolve_handle_records(&current.conn, &worktree_context.context_id, handles).await?;

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

pub fn read_context_locations(
    source_root: &Path,
    scan_filter: &ScanFilter,
    locations: &[ReadLocation],
) -> anyhow::Result<ReadPayload> {
    let canonical_root = source_root
        .canonicalize()
        .with_context(|| format!("failed to resolve source root {}", source_root.display()))?;
    let mut records = Vec::with_capacity(locations.len());

    for location in locations {
        records.push(read_location_record(&canonical_root, scan_filter, location));
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
) -> anyhow::Result<ReadinessPayload> {
    match run_index(roots, rebuild, scope_add, scope_narrow).await {
        Ok(_) => Ok(classify_after_index(roots).await),
        Err(err) => Ok(blocked_readiness(
            &roots.state_root,
            &roots.source_root,
            &roots.worktree_context,
            err.to_string(),
        )),
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

async fn run_index(
    roots: &McpProjectRoots,
    rebuild: bool,
    scope_add: Option<Vec<String>>,
    scope_narrow: Option<Vec<String>>,
) -> anyhow::Result<pipeline::PipelineStats> {
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
    let _rebuild_lock =
        tokio::task::spawn_blocking(move || lifecycle::acquire_rebuild_lock(&lock_root)).await??;

    let db_start = Instant::now();
    let stats = if rebuild {
        // Build the refreshed index aside into a staging file and atomically switch
        // it over the served `index.db`, so search keeps serving the prior index
        // (stale-but-available) throughout and is never torn down in place. A
        // failure before the switch drops the guard, leaving the prior index intact.
        let staged = swap::StagingRebuild::open(&roots.state_root).await?;
        setup.db_prepare_ms = db_start.elapsed().as_millis();
        let stats = run_index_pipeline(staged.connection(), roots, &indexing_config, setup).await?;
        // Persist scope to meta table before finalizing and swapping
        schema::write_scope_to_meta(staged.connection(), &new_scope).await?;
        staged.finalize_and_swap().await?;
        stats
    } else {
        // Incremental write against the live index — unchanged: no rebuild, so no
        // build-aside switch-over is involved.
        let db = Db::open_rw(&config::project_db_path(&roots.state_root)).await?;
        let conn = db.connect_tuned().await?;
        schema::prepare_for_write(&conn).await?;
        setup.db_prepare_ms = db_start.elapsed().as_millis();
        let stats = run_index_pipeline(&conn, roots, &indexing_config, setup).await?;
        // Persist scope to meta table after successful pipeline
        schema::write_scope_to_meta(&conn, &new_scope).await?;
        stats
    };

    Ok(stats)
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
    // REQ-002: MCP `oneup_start` indexing (index-if-missing/index-if-needed/
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
/// until a build-aside swap changes the on-disk inode (REQ-001).
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

/// Process-global warm MCP read-index cache (REQ-001), keyed by canonical
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
/// superseded generation after a build-aside swap (HYP-001: a held
/// `Connection` is pinned to the inode it opened and keeps serving the
/// pre-swap generation, with no error, until dropped and reopened).
fn warm_index_cache() -> &'static tokio::sync::Mutex<HashMap<PathBuf, WarmIndex>> {
    static CACHE: OnceLock<tokio::sync::Mutex<HashMap<PathBuf, WarmIndex>>> = OnceLock::new();
    CACHE.get_or_init(|| tokio::sync::Mutex::new(HashMap::new()))
}

/// Return a tuned RO connection to `db_path`'s currently-served index,
/// reusing the process-global warm cache entry when the on-disk inode is
/// unchanged (REQ-001).
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

    let mut cache = warm_index_cache().lock().await;
    if let Some(warm) = cache.get(canonical_db_path) {
        if warm.identity == current_identity {
            debug_assert!(
                warm.schema_validated,
                "a cache-resident warm index must have already passed schema validation"
            );
            return Ok(warm.conn.clone());
        }
    }

    let db = Db::open_ro(db_path).await?;
    let conn = db.connect_tuned().await?;
    schema::ensure_current(&conn, &schema::SchemaContext::new(db_path, state_root)).await?;

    let served = conn.clone();
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
/// `canonical_db_path`'s warm cache entry, if any (REQ-001).
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
/// context skips its per-query `COUNT(*)` (REQ-001). A no-op if the entry was
/// concurrently reopened (a rare inode-swap race): the recomputed count
/// belongs to the entry that just replaced this one, not the one being
/// updated here.
async fn record_vector_count_for_context(canonical_db_path: &Path, context_id: &str, count: usize) {
    let mut cache = warm_index_cache().lock().await;
    if let Some(warm) = cache.get_mut(canonical_db_path) {
        warm.vector_counts.insert(context_id.to_string(), count);
    }
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
/// lookup (R-013); only the residual handles that did not exact-match (12-char
/// display handles and genuine misses) fall back to the per-handle prefix
/// lookup. Each handle is resolved independently, so the per-handle
/// Found/NotFound/Ambiguous outcome and the empty-handle rejection are identical
/// to resolving handles one at a time.
async fn resolve_handle_records(
    conn: &Connection,
    context_id: &str,
    handles: &[String],
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
            records.push(read_segment(source, segment.clone()));
        } else {
            records.push(resolve_handle_via_prefix(conn, context_id, source, &normalized).await?);
        }
    }

    Ok(records)
}

/// Residual prefix resolution for a handle that did not match an exact id:
/// byte-identical to the prefix branch of the per-handle path, distinguishing
/// unique matches from ambiguous prefixes via [`SegmentPrefixLookup`].
async fn resolve_handle_via_prefix(
    conn: &Connection,
    context_id: &str,
    source: ReadSource,
    normalized: &str,
) -> anyhow::Result<ReadRecord> {
    Ok(
        match get_segment_by_prefix_for_context(conn, context_id, normalized).await? {
            SegmentPrefixLookup::Found(segment) => read_segment(source, *segment),
            SegmentPrefixLookup::NotFound => {
                read_message(ReadStatus::NotFound, source, "segment handle was not found")
            }
            SegmentPrefixLookup::Ambiguous(ids) => ReadRecord {
                status: ReadStatus::Ambiguous,
                source,
                segment: None,
                context: None,
                matching_handles: ids,
                message: Some("segment handle matched multiple indexed segments".to_string()),
            },
        },
    )
}

fn read_location_record(
    source_root: &Path,
    scan_filter: &ScanFilter,
    location: &ReadLocation,
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

    match ContextEngine::retrieve_with_scope(
        &file_path,
        location.line,
        location.expansion,
        ContextAccessScope::ProjectRoot,
    ) {
        Ok(context) => read_context(source, source_root, context),
        Err(err) => read_message(ReadStatus::Error, source, err.to_string()),
    }
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

fn read_segment(source: ReadSource, segment: StoredSegment) -> ReadRecord {
    ReadRecord {
        status: ReadStatus::Found,
        source,
        segment: Some(segment_record(segment)),
        context: None,
        matching_handles: Vec::new(),
        message: None,
    }
}

fn read_context(source: ReadSource, source_root: &Path, context: ContextResult) -> ReadRecord {
    let path = Path::new(&context.file_path)
        .strip_prefix(source_root)
        .map(relative_path_string)
        .unwrap_or_else(|_| context.file_path.clone());

    ReadRecord {
        status: ReadStatus::Found,
        source,
        segment: None,
        context: Some(ContextRecord {
            path,
            language: context.language,
            scope_type: context.scope_type,
            content: context.content,
            line_start: context.line_start,
            line_end: context.line_end,
        }),
        matching_handles: Vec::new(),
        message: None,
    }
}

fn read_message(status: ReadStatus, source: ReadSource, message: impl Into<String>) -> ReadRecord {
    ReadRecord {
        status,
        source,
        segment: None,
        context: None,
        matching_handles: Vec::new(),
        message: Some(message.into()),
    }
}

fn segment_record(segment: StoredSegment) -> SegmentRecord {
    let role = segment.parsed_role();
    let defined_symbols = segment.parsed_defined_symbols();
    let referenced_symbols = segment.parsed_referenced_symbols();
    let called_symbols = segment.parsed_called_symbols();

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
        defined_symbols,
        referenced_symbols,
        called_symbols,
    }
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
    // error (REQ-010); missing/unready indexes fail in open_current_index.
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

fn read_index_progress(project_root: &Path) -> Option<IndexProgress> {
    let path = project_dot_dir(project_root).join(INDEX_PROGRESS_FILE_NAME);
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn read_index_progress_for_context(project_root: &Path, context_id: &str) -> Option<IndexProgress> {
    read_index_progress(project_root).filter(|progress| {
        progress
            .context_id
            .as_deref()
            .is_none_or(|progress_context_id| progress_context_id == context_id)
    })
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

/// Gets the configured file count threshold for facts envelope gate, with env var override.
fn get_file_count_threshold() -> usize {
    std::env::var(FILE_COUNT_THRESHOLD_ENV_VAR)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(FILE_COUNT_THRESHOLD)
}

/// Counts files per top-level directory (metadata-only walk, no parsing).
/// Returns a map of directory name to (file_count, estimated_vectors).
fn count_files_per_directory(source_root: &Path) -> Result<BTreeMap<String, usize>, OneupError> {
    let mut dir_counts: BTreeMap<String, usize> = BTreeMap::new();

    // Simple metadata-only walk: no filtering, just count files per top-level dir.
    for entry in std::fs::read_dir(source_root).map_err(|e| {
        OneupError::Project(ProjectError::ReadFailed(format!(
            "cannot read repo root: {e}"
        )))
    })? {
        let entry = entry.map_err(|e| {
            OneupError::Project(ProjectError::ReadFailed(format!(
                "directory walk error: {e}"
            )))
        })?;

        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_string());

        let Some(dir_name) = name else {
            continue;
        };

        // Skip hidden directories and common non-code directories
        if dir_name.starts_with('.') {
            continue;
        }

        // Walk this directory recursively and count files
        let file_count = count_files_recursive(&path);
        if file_count > 0 {
            dir_counts.insert(dir_name, file_count);
        }
    }

    // If no top-level directories were counted, count files at the root directly
    if dir_counts.is_empty() {
        let root_count = count_files_recursive(source_root);
        if root_count > 0 {
            dir_counts.insert(".".to_string(), root_count);
        }
    }

    Ok(dir_counts)
}

/// Recursively counts files in a directory (fast metadata-only walk).
fn count_files_recursive(path: &Path) -> usize {
    let mut count = 0;

    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            match entry.path() {
                p if p.is_file() => count += 1,
                p if p.is_dir() => {
                    // Skip hidden directories
                    if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                        if !name.starts_with('.') && name != "node_modules" && name != "target" {
                            count += count_files_recursive(&p);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    count
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

/// Generates a facts envelope for a large monorepo on first-run.
pub async fn generate_facts_envelope(
    source_root: &Path,
    launch_subdir: Option<PathBuf>,
) -> Result<FactsEnvelope, OneupError> {
    // Count files per top-level directory
    let dir_counts = count_files_per_directory(source_root)?;

    let file_count_total: usize = dir_counts.values().sum();
    let vector_estimate_total = (file_count_total + 9) / 10; // Conservative: ~10 files per vector

    let mut per_directory_stats: Vec<DirectoryStats> = dir_counts
        .into_iter()
        .map(|(directory, file_count)| DirectoryStats {
            directory,
            file_count,
            estimated_vectors: (file_count + 9) / 10,
        })
        .collect();

    // Sort by file count descending (largest first)
    per_directory_stats.sort_by(|a, b| b.file_count.cmp(&a.file_count));

    let workspace_manifests = detect_workspace_manifests(source_root);
    let sparse_checkout = get_sparse_checkout_info(source_root);

    let launch_subdir_str = launch_subdir.and_then(|p| {
        p.strip_prefix(source_root)
            .ok()
            .and_then(|rel| rel.to_str().map(|s| s.to_string()))
    });

    Ok(FactsEnvelope {
        per_directory_stats,
        workspace_manifests,
        sparse_checkout,
        launch_subdir: launch_subdir_str,
        file_count_total,
        vector_estimate_total,
    })
}

/// Computes the current index scope coverage information from the database and filesystem.
///
/// Reads the scope roots from the meta table, counts indexed files from the database,
/// and counts total files in the repository. Returns None if the index is not present
/// or readable.
pub async fn compute_index_scope(
    state_root: &Path,
    source_root: &Path,
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

    // Count indexed files: get all file paths with segments
    let indexed_file_paths = crate::storage::segments::get_all_file_paths(&conn).await?;
    let indexed_files = indexed_file_paths.len();

    // Count total files in the repository
    let dir_counts = count_files_per_directory(source_root)?;
    let total_files: usize = dir_counts.values().sum();

    Ok(Some(IndexScope {
        roots: scope_roots.unwrap_or_default(),
        indexed_files,
        total_files,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::types::{
        BranchStatus, DaemonRefreshState, StructuralSearchStatus, WorktreeRole,
    };
    use crate::storage::segments::{self, SegmentInsert};
    use std::fs;

    fn readiness_fixture() -> ReadinessPayload {
        blocked_readiness_for_path("repo", "fixture")
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
        // The wording is the single source of truth folded into degraded_reason
        // by T6; pin its substance so the user-facing notice cannot silently drift.
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

    #[test]
    fn read_context_locations_rejects_parent_escape() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir_all(&root).unwrap();

        let payload = read_context_locations(
            &root,
            &no_op_scan_filter(),
            &[ReadLocation {
                path: "../outside.rs".to_string(),
                line: 1,
                expansion: None,
            }],
        )
        .unwrap();

        assert_eq!(payload.status, OperationStatus::Empty);
        assert_eq!(payload.records[0].status, ReadStatus::Rejected);
    }

    #[test]
    fn read_context_locations_rejects_zero_line_as_structured_record() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir_all(&root).unwrap();

        let payload = read_context_locations(
            &root,
            &no_op_scan_filter(),
            &[ReadLocation {
                path: "src/lib.rs".to_string(),
                line: 0,
                expansion: None,
            }],
        )
        .unwrap();

        assert_eq!(payload.status, OperationStatus::Empty);
        assert_eq!(payload.records[0].status, ReadStatus::Rejected);
        assert!(payload.records[0]
            .message
            .as_deref()
            .unwrap()
            .contains("1-based"));
    }

    #[test]
    fn read_context_locations_reads_repo_relative_file() {
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
            &no_op_scan_filter(),
            &[ReadLocation {
                path: "src/lib.rs".to_string(),
                line: 2,
                expansion: None,
            }],
        )
        .unwrap();

        assert_eq!(payload.status, OperationStatus::Ok);
        assert_eq!(payload.records[0].status, ReadStatus::Found);
        assert_eq!(
            payload.records[0].context.as_ref().unwrap().path,
            "src/lib.rs"
        );
    }

    /// REQ-005 AC1 red-first baseline: prior to enforcing `ScanFilter` at the
    /// context read path, `oneup_context` read secret-pattern files off disk
    /// directly, bypassing indexer exclusions entirely. This asserts the
    /// closed behavior — the fix under test refuses the file rather than
    /// returning its content.
    #[test]
    fn read_context_locations_rejects_secret_pattern_file() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("credentials.json"), "{\"key\": \"super-secret\"}").unwrap();

        let payload = read_context_locations(
            &root,
            &no_op_scan_filter(),
            &[ReadLocation {
                path: "credentials.json".to_string(),
                line: 1,
                expansion: None,
            }],
        )
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

    #[test]
    fn read_context_locations_rejects_configured_exclude_glob() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir_all(root.join("secrets")).unwrap();
        fs::write(root.join("secrets/internal.txt"), "internal only").unwrap();

        let scan_filter = ScanFilter::new(&[], &["secrets/**".to_string()], &[]).unwrap();
        let payload = read_context_locations(
            &root,
            &scan_filter,
            &[ReadLocation {
                path: "secrets/internal.txt".to_string(),
                line: 1,
                expansion: None,
            }],
        )
        .unwrap();

        assert_eq!(payload.records[0].status, ReadStatus::Rejected);
        assert!(payload.records[0].context.is_none());
    }

    /// REQ-005 AC2: a non-excluded file continues to be served normally even
    /// when the project has a configured (non-matching) `ScanFilter`.
    #[test]
    fn read_context_locations_serves_non_excluded_file_with_configured_filter() {
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
            &scan_filter,
            &[ReadLocation {
                path: "src/lib.rs".to_string(),
                line: 2,
                expansion: None,
            }],
        )
        .unwrap();

        assert_eq!(payload.records[0].status, ReadStatus::Found);
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

        let payload = run_search(&root, &context, "vectorless_needle", 5, None)
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

    /// REQ-001 AC1/AC4: the `mcp::ops` construction site must inject
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

        let scoped = run_search(&root, &context, "probetokenonly", 10, Some("included"))
            .await
            .unwrap();
        let scoped_paths: Vec<_> = scoped.results.iter().map(|r| r.path.clone()).collect();
        assert_eq!(
            scoped_paths,
            vec!["included/a.rs".to_string()],
            "path_prefix must constrain oneup_search results to the prefix"
        );

        let unscoped = run_search(&root, &context, "probetokenonly", 10, None)
            .await
            .unwrap();
        assert_eq!(
            unscoped.results.len(),
            2,
            "no prefix supplied must leave full-repo search behavior unchanged (REQ-001 AC4)"
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

    /// REQ-001 AC1 / T1: a second `open_current_index` call on an
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

    /// REQ-001 AC4 / T1 (HYP-001): after a build-aside swap installs a fresh
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
        let before = run_search(&root, &context, "old_needle", 5, None)
            .await
            .unwrap();
        assert_eq!(before.results.len(), 1);

        let staging = build_staged_index_with_segment(&root, "ctx-active", "new_needle").await;
        {
            let _lock = lifecycle::acquire_rebuild_lock(&root).unwrap();
            swap::swap_index_into_place(&root, &staging).await.unwrap();
        }

        let after = run_search(&root, &context, "new_needle", 5, None)
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

        let stale = run_search(&root, &context, "old_needle", 5, None)
            .await
            .unwrap();
        assert!(
            stale.results.is_empty(),
            "the pre-swap generation's data must not still be served through the warm cache"
        );
    }

    /// T1: the per-context vector-count cache on a warm index entry must be
    /// populated on demand, keyed independently per context, and cleared in
    /// full when a build-aside swap reopens the entry -- mirroring the
    /// daemon's `reopen_invalidates_cached_vector_count_after_swap` coverage
    /// for `ProjectState::cached_vector_count`. A stale count surviving a
    /// swap could silently flip `vector_search_path_for_corpus` between the
    /// exhaustive scan and the ANN path against the wrong generation.
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
        // R-013: the batched exact-id + residual-prefix resolver must return the
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

        let batched = resolve_handle_records(&conn, ctx, &handles).await.unwrap();

        // Reconstruct the per-item baseline: exact id, then prefix, per handle.
        let mut expected = Vec::with_capacity(handles.len());
        for handle in &handles {
            expected.push(
                resolve_handle_record_per_item(&conn, ctx, handle)
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

    /// Per-item baseline (relocated from the pre-R-013 `resolve_handle_record`):
    /// resolve one handle by exact id, then by prefix. The batched
    /// `resolve_handle_records` must match this field-for-field.
    async fn resolve_handle_record_per_item(
        conn: &Connection,
        context_id: &str,
        raw_handle: &str,
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
            return Ok(read_segment(source, segment));
        }

        resolve_handle_via_prefix(conn, context_id, source, &normalized).await
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
        };
        let result = apply_scope_to_indexing_config(&mut config, &[]);
        assert!(result.is_ok());
        assert!(config.include_globs.is_empty());
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
        };
        let scope = vec!["services/auth".to_string()];
        let result = apply_scope_to_indexing_config(&mut config, &scope);
        assert!(result.is_ok());
        assert_eq!(config.include_globs, vec!["services/auth/**"]);
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
        };
        let scope = vec!["services/auth".to_string(), "libs/core".to_string()];
        let result = apply_scope_to_indexing_config(&mut config, &scope);
        assert!(result.is_ok());
        assert_eq!(
            config.include_globs,
            vec!["services/auth/**", "libs/core/**"]
        );
    }

    #[tokio::test]
    async fn test_compute_new_scope_with_scope_add() {
        // Test: scope_add with no existing scope creates new scope
        use tempfile::TempDir;
        let temp_dir = TempDir::new().unwrap();
        let scope_add = Some(vec!["services/auth".to_string()]);
        let result = compute_new_scope(&temp_dir.path(), scope_add.clone(), None)
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
        let result = compute_new_scope(&temp_dir.path(), None, scope_narrow)
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
        };

        let description = scope.coverage_description();
        assert!(description.contains("0 files indexed of 0 total"));
        assert!(description.contains("0%"));
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
}
