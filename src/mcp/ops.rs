use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use libsql::Connection;
use serde::Serialize;

use crate::daemon::lifecycle;
use crate::daemon::registry::Registry;
use crate::indexer::embedder::{
    self, EmbeddingLoadStatus, EmbeddingRuntime, EmbeddingUnavailableReason,
};
use crate::indexer::pipeline;
use crate::mcp::types::StartMode;
use crate::search::context::ContextEngine;
use crate::search::impact::{ImpactHorizonEngine, ImpactRequest, ImpactResultEnvelope};
use crate::search::overview;
use crate::search::retrieval;
use crate::search::{HybridSearchEngine, SearchScope, StructuralSearchEngine, SymbolSearchEngine};
use crate::shared::config::{self, project_db_path, project_dot_dir};
use crate::shared::constants::{
    DB_LOCK_RETRY_ATTEMPTS, DB_LOCK_RETRY_DELAY_MS, NO_INDEXED_EMBEDDINGS_REASON,
    STALE_REBUILD_REASON,
};
use crate::shared::errors::{OneupError, ProjectError};
use crate::shared::project;
use crate::shared::types::{
    combine_degraded_reasons, ContextAccessScope, ContextResult, DaemonProjectStatus,
    IndexProgress, IndexState, IndexingConfig, ReferenceKind, RunScope, SearchResult, SegmentRole,
    SetupTimings, StructuralSearchReport, SymbolResult, WorktreeContext,
};
use crate::storage::db::{is_lock_error, Db};
use crate::storage::schema;
use crate::storage::segments::{
    count_embeddable_segments_for_context, count_files_for_context, count_segments_for_context,
    count_vector_rows_for_context, get_segment_by_id_for_context,
    get_segment_by_prefix_for_context, get_worktree_context_head_oid, SegmentPrefixLookup,
    StoredSegment,
};
use crate::storage::swap;

const INDEX_PROGRESS_FILE_NAME: &str = "index_status.json";

#[derive(Debug, Clone)]
pub struct McpProjectRoots {
    pub state_root: PathBuf,
    pub source_root: PathBuf,
    pub worktree_context: WorktreeContext,
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
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchPayload {
    pub status: OperationStatus,
    pub results: Vec<SearchHit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub degraded_reason: Option<String>,
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
    _db: Db,
}

pub fn resolve_project(path: &Path) -> anyhow::Result<McpProjectRoots> {
    let resolved = project::resolve_project_root(path)?;
    Ok(McpProjectRoots {
        state_root: resolved.state_root,
        source_root: resolved.source_root,
        worktree_context: resolved.worktree_context,
    })
}

pub async fn check_status(roots: &McpProjectRoots) -> ReadinessPayload {
    classify_readiness(
        &roots.state_root,
        &roots.source_root,
        &roots.worktree_context,
    )
    .await
}

pub async fn start(roots: &McpProjectRoots, mode: StartMode) -> anyhow::Result<ReadinessPayload> {
    let readiness = check_status(roots).await;
    match mode {
        StartMode::IndexIfMissing if readiness.status == ReadinessStatus::Missing => {
            run_index_then_classify(roots, false).await
        }
        StartMode::IndexIfNeeded if index_if_needed_applies(&readiness) => {
            run_index_then_classify(roots, false).await
        }
        StartMode::Reindex => run_index_then_classify(roots, true).await,
        _ => Ok(readiness),
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

    let conn = match db.connect() {
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

    payload.index_readable = true;
    payload.indexed_files = count_files_for_context(&conn, &worktree_context.context_id)
        .await
        .ok();
    payload.total_segments = count_segments_for_context(&conn, &worktree_context.context_id)
        .await
        .ok();
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
    }
}

pub async fn run_search(
    state_root: &Path,
    worktree_context: &WorktreeContext,
    query: &str,
    limit: usize,
) -> anyhow::Result<SearchPayload> {
    retry_on_db_lock(|| async { run_search_once(state_root, worktree_context, query, limit).await })
        .await
}

async fn run_search_once(
    state_root: &Path,
    worktree_context: &WorktreeContext,
    query: &str,
    limit: usize,
) -> anyhow::Result<SearchPayload> {
    let current = open_current_index(state_root).await?;
    let search_scope = SearchScope::from_worktree_context(worktree_context);

    // Cheap vector-presence gate first: when the index holds no embeddings
    // for this context, the embedding model must never be initialized.
    let has_vectors = retrieval::has_indexed_embeddings(&current.conn, &search_scope).await?;
    let (results, embedding_reason) = if has_vectors {
        let mut runtime = EmbeddingRuntime::default();
        let embedding_status = runtime.prepare_for_search(1);
        let embedding_reason = embedding_unavailable_reason(&embedding_status);
        let results = if embedding_status.is_available() {
            let mut engine = HybridSearchEngine::new_scoped(
                &current.conn,
                runtime.current_embedder(),
                search_scope.clone(),
            );
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

    Ok(SearchPayload {
        status,
        results: results.into_iter().map(search_hit).collect(),
        degraded_reason,
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
    let mut records = Vec::with_capacity(handles.len());

    for handle in handles {
        records.push(
            resolve_handle_record(&current.conn, &worktree_context.context_id, handle).await?,
        );
    }

    Ok(ReadPayload {
        status: aggregate_read_status(&records),
        records,
    })
}

pub fn read_context_locations(
    source_root: &Path,
    locations: &[ReadLocation],
) -> anyhow::Result<ReadPayload> {
    let canonical_root = source_root
        .canonicalize()
        .with_context(|| format!("failed to resolve source root {}", source_root.display()))?;
    let mut records = Vec::with_capacity(locations.len());

    for location in locations {
        records.push(read_location_record(&canonical_root, location));
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
) -> anyhow::Result<ReadinessPayload> {
    match run_index(roots, rebuild).await {
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
) -> anyhow::Result<pipeline::PipelineStats> {
    if project::read_project_id(&roots.state_root).is_err() {
        project::ensure_project_id_for_auto_init(&roots.state_root)?;
    }

    let registry = Registry::load()?;
    let indexing_config = config::resolve_indexing_config(
        None,
        None,
        registry.indexing_config_for_context(&roots.worktree_context),
    )?;
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
    if rebuild {
        // Build the refreshed index aside into a staging file and atomically switch
        // it over the served `index.db`, so search keeps serving the prior index
        // (stale-but-available) throughout and is never torn down in place. A
        // failure before the switch drops the guard, leaving the prior index intact.
        let staged = swap::StagingRebuild::open(&roots.state_root).await?;
        setup.db_prepare_ms = db_start.elapsed().as_millis();
        let stats = run_index_pipeline(staged.connection(), roots, &indexing_config, setup).await?;
        staged.finalize_and_swap().await?;
        Ok(stats)
    } else {
        // Incremental write against the live index — unchanged: no rebuild, so no
        // build-aside switch-over is involved.
        let db = Db::open_rw(&config::project_db_path(&roots.state_root)).await?;
        let conn = db.connect_tuned().await?;
        schema::prepare_for_write(&conn).await?;
        setup.db_prepare_ms = db_start.elapsed().as_millis();
        run_index_pipeline(&conn, roots, &indexing_config, setup).await
    }
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
    let mut runtime = EmbeddingRuntime::default();
    runtime
        .prepare_for_indexing_with_progress(indexing_config.embed_threads, false)
        .await;
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

async fn open_current_index(state_root: &Path) -> anyhow::Result<CurrentIndex> {
    let db_path = project_db_path(state_root);
    if !db_path.exists() {
        bail!(
            "no current index found at {}; call oneup_start with an explicit indexing mode",
            db_path.display()
        );
    }

    let db = Db::open_ro(&db_path).await?;
    let conn = db.connect()?;
    schema::ensure_current(&conn, &schema::SchemaContext::new(&db_path, state_root)).await?;

    Ok(CurrentIndex { conn, _db: db })
}

async fn resolve_handle_record(
    conn: &Connection,
    context_id: &str,
    raw_handle: &str,
) -> anyhow::Result<ReadRecord> {
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

    Ok(
        match get_segment_by_prefix_for_context(conn, context_id, &normalized).await? {
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

fn read_location_record(source_root: &Path, location: &ReadLocation) -> ReadRecord {
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

    #[test]
    fn read_context_locations_rejects_parent_escape() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir_all(&root).unwrap();

        let payload = read_context_locations(
            &root,
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

        let payload = run_search(&root, &context, "vectorless_needle", 5)
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

    fn test_segment(id: &str, file_path: &str) -> SegmentInsert {
        SegmentInsert {
            id: id.to_string(),
            file_path: file_path.to_string(),
            language: "rust".to_string(),
            block_type: "function".to_string(),
            content: format!("fn {id}() {{}}"),
            line_start: 1,
            line_end: 1,
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
}
