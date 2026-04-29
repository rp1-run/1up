#![allow(dead_code)]

use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context};
use libsql::Connection;
use serde::Serialize;

use crate::indexer::embedder::{EmbeddingLoadStatus, EmbeddingRuntime, EmbeddingUnavailableReason};
use crate::search::context::ContextEngine;
use crate::search::impact::{ImpactHorizonEngine, ImpactRequest, ImpactResultEnvelope};
use crate::search::{HybridSearchEngine, SymbolSearchEngine};
use crate::shared::config::{project_daemon_status_path, project_db_path, project_dot_dir};
use crate::shared::project;
use crate::shared::types::{
    ContextAccessScope, ContextResult, DaemonProjectStatus, IndexProgress, IndexState,
    ReferenceKind, SearchResult, SegmentRole, SymbolResult,
};
use crate::storage::db::Db;
use crate::storage::schema;
use crate::storage::segments::{
    count_files, count_segments, get_segment_by_id, get_segment_by_prefix, SegmentPrefixLookup,
    StoredSegment,
};

const INDEX_PROGRESS_FILE_NAME: &str = "index_status.json";

#[derive(Debug, Clone)]
pub struct McpProjectRoots {
    pub state_root: PathBuf,
    pub source_root: PathBuf,
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
    })
}

pub async fn classify_readiness(state_root: &Path, source_root: &Path) -> ReadinessPayload {
    let project_initialized = project::read_project_id(state_root).is_ok();
    let db_path = project_db_path(state_root);
    let index_present = db_path.exists();
    let index_progress = read_index_progress(state_root);
    let daemon_status = read_daemon_status(state_root);
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
        payload.reason = Some("run oneup_prepare with an explicit indexing mode".to_string());
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
    payload.indexed_files = count_files(&conn).await.ok();
    payload.total_segments = count_segments(&conn).await.ok();

    if payload.total_segments.unwrap_or(0) == 0 {
        payload.status = ReadinessStatus::Missing;
        payload.summary = "No indexed code is available for this repository.".to_string();
        payload.reason = Some("run oneup_prepare with an explicit indexing mode".to_string());
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

pub async fn run_search(
    state_root: &Path,
    query: &str,
    limit: usize,
) -> anyhow::Result<SearchPayload> {
    let current = open_current_index(state_root).await?;
    let mut runtime = EmbeddingRuntime::default();
    let embedding_status = runtime.prepare_for_search(1);
    let degraded_reason = embedding_unavailable_reason(&embedding_status);

    let results = if embedding_status.is_available() {
        let mut engine = HybridSearchEngine::new(&current.conn, runtime.current_embedder());
        engine.search(query, limit).await?
    } else {
        let engine = HybridSearchEngine::new(&current.conn, None);
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

pub async fn read_handles(state_root: &Path, handles: &[String]) -> anyhow::Result<ReadPayload> {
    let current = open_current_index(state_root).await?;
    let mut records = Vec::with_capacity(handles.len());

    for handle in handles {
        records.push(resolve_handle_record(&current.conn, handle).await?);
    }

    Ok(ReadPayload {
        status: aggregate_read_status(&records),
        records,
    })
}

pub fn read_locations(
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
    request: SymbolLookupRequest,
) -> anyhow::Result<SymbolPayload> {
    if request.name.trim().is_empty() {
        bail!("symbol name cannot be empty");
    }

    let current = open_current_index(state_root).await?;
    let engine = SymbolSearchEngine::new(&current.conn);

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
    request: ImpactRequest,
) -> anyhow::Result<ImpactResultEnvelope> {
    let current = open_current_index(state_root).await?;
    let engine = ImpactHorizonEngine::new(&current.conn);
    Ok(engine.explore(request).await?)
}

async fn open_current_index(state_root: &Path) -> anyhow::Result<CurrentIndex> {
    let db_path = project_db_path(state_root);
    if !db_path.exists() {
        bail!(
            "no current index found at {}; call oneup_prepare with an explicit indexing mode",
            db_path.display()
        );
    }

    let db = Db::open_ro(&db_path).await?;
    let conn = db.connect()?;
    schema::ensure_current(&conn).await?;

    Ok(CurrentIndex { conn, _db: db })
}

async fn resolve_handle_record(conn: &Connection, raw_handle: &str) -> anyhow::Result<ReadRecord> {
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

    if let Some(segment) = get_segment_by_id(conn, &normalized).await? {
        return Ok(read_segment(source, segment));
    }

    Ok(match get_segment_by_prefix(conn, &normalized).await? {
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
    })
}

fn read_location_record(source_root: &Path, location: &ReadLocation) -> ReadRecord {
    let source = ReadSource::Location {
        path: location.path.clone(),
        line: location.line,
    };

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

fn read_daemon_status(project_root: &Path) -> Option<DaemonProjectStatus> {
    let path = project_daemon_status_path(project_root);
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
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

fn usize_from_i64(value: i64) -> usize {
    usize::try_from(value).unwrap_or_default()
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

enum LocationError {
    Rejected(String),
    Error(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn read_locations_rejects_parent_escape() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir_all(&root).unwrap();

        let payload = read_locations(
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
    fn read_locations_reads_repo_relative_file() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "fn main() {\n    println!(\"hi\");\n}\n",
        )
        .unwrap();

        let payload = read_locations(
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
}
