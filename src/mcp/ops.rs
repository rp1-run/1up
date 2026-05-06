use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use libsql::Connection;
use serde::Serialize;

use crate::daemon::registry::Registry;
use crate::indexer::embedder::{EmbeddingLoadStatus, EmbeddingRuntime, EmbeddingUnavailableReason};
use crate::indexer::pipeline;
use crate::mcp::types::StartMode;
use crate::search::context::ContextEngine;
use crate::search::impact::{ImpactHorizonEngine, ImpactRequest, ImpactResultEnvelope};
use crate::search::{HybridSearchEngine, SearchScope, StructuralSearchEngine, SymbolSearchEngine};
use crate::shared::config::{self, project_db_path, project_dot_dir};
use crate::shared::errors::{OneupError, ProjectError};
use crate::shared::project;
use crate::shared::types::{
    ContextAccessScope, ContextResult, DaemonProjectStatus, IndexProgress, IndexState,
    ReferenceKind, RunScope, SearchResult, SegmentRole, SetupTimings, StructuralSearchReport,
    SymbolResult, WorktreeContext,
};
use crate::storage::db::Db;
use crate::storage::schema;
use crate::storage::segments::{
    count_files_for_context, count_segments_for_context, get_segment_by_id_for_context,
    get_segment_by_prefix_for_context, SegmentPrefixLookup, StoredSegment,
};

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
        StartMode::IndexIfNeeded
            if matches!(
                readiness.status,
                ReadinessStatus::Missing | ReadinessStatus::Degraded
            ) =>
        {
            run_index_then_classify(roots, false).await
        }
        StartMode::Reindex => run_index_then_classify(roots, true).await,
        _ => Ok(readiness),
    }
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
    let daemon_status = crate::cli::project_status_files::read_daemon_status_for_context(
        state_root,
        &worktree_context.context_id,
    );
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
        payload.status = ReadinessStatus::Missing;
        payload.summary = "No usable 1up index is available for this repository.".to_string();
        payload.reason = Some("run oneup_start with an explicit indexing mode".to_string());
        return payload;
    }

    let db = match Db::open_ro(&db_path).await {
        Ok(db) => db,
        Err(err) => {
            payload.status = ReadinessStatus::Stale;
            payload.summary = "The index exists but cannot be opened.".to_string();
            payload.reason = Some(err.to_string());
            return payload;
        }
    };

    let conn = match db.connect() {
        Ok(conn) => conn,
        Err(err) => {
            payload.status = ReadinessStatus::Stale;
            payload.summary = "The index exists but cannot be read.".to_string();
            payload.reason = Some(err.to_string());
            return payload;
        }
    };

    payload.schema_version = schema::get_schema_version(&conn).await.ok().flatten();

    if let Err(err) = schema::ensure_current(&conn).await {
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

    if payload.total_segments.unwrap_or(0) == 0 {
        payload.status = ReadinessStatus::Missing;
        payload.summary = "No indexed code is available for this repository.".to_string();
        payload.reason = Some("run oneup_start with an explicit indexing mode".to_string());
        return payload;
    }

    let progress_without_embeddings = payload
        .index_progress
        .as_ref()
        .is_some_and(|progress| !progress.embeddings_enabled);
    let embedding_reason = embedding_unavailable_reason(&embedding_status_for_search());

    if progress_without_embeddings || embedding_reason.is_some() {
        payload.status = ReadinessStatus::Degraded;
        payload.summary =
            "The index is readable, but semantic embeddings are unavailable.".to_string();
        payload.reason = Some(
            embedding_reason
                .unwrap_or_else(|| "latest index was built without embeddings".to_string()),
        );
        return payload;
    }

    payload.status = ReadinessStatus::Ready;
    payload.summary = "The repository is ready for 1up MCP search.".to_string();
    payload
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
    let current = open_current_index(state_root).await?;
    let mut runtime = EmbeddingRuntime::default();
    let embedding_status = runtime.prepare_for_search(1);
    let search_scope = SearchScope::from_worktree_context(worktree_context);
    let degraded_reason = combine_degraded_reasons(
        embedding_unavailable_reason(&embedding_status),
        search_scope.degraded_reason(),
    );

    let results = if embedding_status.is_available() {
        let mut engine =
            HybridSearchEngine::new_scoped(&current.conn, runtime.current_embedder(), search_scope);
        engine.search(query, limit).await?
    } else {
        let engine = HybridSearchEngine::new_scoped(&current.conn, None, search_scope);
        engine.fts_only_search(query, limit).await?
    };

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
    let current = open_current_index(state_root).await?;
    let engine = StructuralSearchEngine::new_scoped(
        source_root,
        &current.conn,
        &worktree_context.context_id,
    );
    Ok(engine.search_report(pattern, language_filter).await?)
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

    let db_path = config::project_db_path(&roots.state_root);
    let registry = Registry::load()?;
    let indexing_config = config::resolve_indexing_config(
        None,
        None,
        registry.indexing_config_for_context(&roots.worktree_context),
    )?;
    let mut setup = SetupTimings::new(Instant::now());

    let db_start = Instant::now();
    let db = Db::open_rw(&db_path).await?;
    let conn = db.connect_tuned().await?;
    if rebuild {
        schema::rebuild(&conn).await?;
    } else {
        schema::prepare_for_write(&conn).await?;
    }
    setup.db_prepare_ms = db_start.elapsed().as_millis();

    let model_start = Instant::now();
    let mut runtime = EmbeddingRuntime::default();
    runtime
        .prepare_for_indexing_with_progress(indexing_config.embed_threads, false)
        .await;
    setup.model_prepare_ms = model_start.elapsed().as_millis();

    pipeline::run_with_context_scope_setup_and_progress_root(
        &conn,
        &roots.worktree_context,
        runtime.current_embedder(),
        &RunScope::Full,
        &indexing_config,
        None,
        false,
        Some(setup),
        None,
        Some(&roots.state_root),
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
    schema::ensure_current(&conn).await?;

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
        .map(path_string)
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

fn embedding_status_for_search() -> EmbeddingLoadStatus {
    let mut runtime = EmbeddingRuntime::default();
    runtime.prepare_for_search(1)
}

fn embedding_unavailable_reason(status: &EmbeddingLoadStatus) -> Option<String> {
    match status {
        EmbeddingLoadStatus::Warm
        | EmbeddingLoadStatus::Loaded
        | EmbeddingLoadStatus::Downloaded => None,
        EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::ModelMissing) => {
            Some("embedding model is missing".to_string())
        }
        EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::PreviousDownloadFailed) => {
            Some("embedding model download previously failed".to_string())
        }
        EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::ModelDirUnavailable(err)) => {
            Some(format!("embedding model directory is unavailable: {err}"))
        }
        EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::LoadFailed(err)) => {
            Some(format!("embedding model failed to load: {err}"))
        }
        EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::DownloadFailed(err)) => {
            Some(format!("embedding model download failed: {err}"))
        }
    }
}

fn combine_degraded_reasons(left: Option<String>, right: Option<String>) -> Option<String> {
    match (left, right) {
        (Some(left), Some(right)) => Some(format!("{left}; {right}")),
        (Some(reason), None) | (None, Some(reason)) => Some(reason),
        (None, None) => None,
    }
}

fn usize_from_i64(value: i64) -> usize {
    usize::try_from(value).unwrap_or_default()
}

fn path_string(path: &Path) -> String {
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
    use crate::shared::types::{BranchStatus, StructuralSearchStatus, WorktreeRole};
    use crate::storage::segments::{self, SegmentInsert};
    use std::fs;

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
