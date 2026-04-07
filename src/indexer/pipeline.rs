use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use libsql::Connection;
use nanospinner::Spinner;
use sha2::{Digest, Sha256};
use tokio::task::JoinSet;
use tracing::{debug, info};

fn spin(msg: impl Into<String>) -> nanospinner::SpinnerHandle {
    use std::io::IsTerminal;
    Spinner::with_writer_tty(msg, std::io::stderr(), std::io::stderr().is_terminal()).start()
}

fn index_progress_path(project_root: &Path) -> std::path::PathBuf {
    config::project_dot_dir(project_root).join(INDEX_PROGRESS_FILE_NAME)
}

fn persist_progress(project_root: &Path, progress: &IndexProgress) {
    let dot_dir = config::project_dot_dir(project_root);
    let payload = match serde_json::to_vec_pretty(progress) {
        Ok(payload) => payload,
        Err(err) => {
            debug!("failed to serialize index progress: {err}");
            return;
        }
    };

    if let Err(err) = std::fs::create_dir_all(&dot_dir) {
        debug!(
            "failed to create progress directory {}: {err}",
            dot_dir.display()
        );
        return;
    }

    let progress_path = index_progress_path(project_root);
    if let Err(err) = std::fs::write(&progress_path, payload) {
        debug!(
            "failed to persist index progress to {}: {err}",
            progress_path.display()
        );
    }
}

fn refresh_progress(
    stats: &mut PipelineStats,
    project_root: &Path,
    state: IndexState,
    phase: IndexPhase,
    files_total: usize,
    parallelism: Option<IndexParallelism>,
    timings: Option<IndexStageTimings>,
) {
    stats.progress = IndexProgress {
        state,
        phase,
        files_total,
        files_scanned: stats.files_scanned,
        files_indexed: stats.files_indexed,
        files_skipped: stats.files_skipped,
        files_deleted: stats.files_deleted,
        segments_stored: stats.segments_stored,
        embeddings_enabled: stats.embeddings_generated,
        parallelism,
        timings,
        updated_at: chrono::Utc::now(),
    };
    persist_progress(project_root, &stats.progress);
}

use crate::indexer::chunker;
use crate::indexer::embedder::Embedder;
use crate::indexer::parser;
use crate::indexer::scanner;
use crate::shared::config;
use crate::shared::constants::{EMBEDDING_DIM, HF_MODEL_REPO};
use crate::shared::errors::{IndexingError, OneupError};
use crate::shared::types::{
    IndexParallelism, IndexPhase, IndexProgress, IndexStageTimings, IndexState, IndexingConfig,
    ParsedSegment, RunScope,
};
use crate::storage::schema;
use crate::storage::segments::{self, FileSegmentBatch, SegmentInsert};

const INDEX_PROGRESS_FILE_NAME: &str = "index_status.json";

#[derive(Debug, Default)]
struct TimingAccumulator {
    scan_ms: u128,
    parse_ms: u128,
    embed_ms: u128,
    store_ms: u128,
}

impl TimingAccumulator {
    fn snapshot(&self, run_started_at: Instant) -> IndexStageTimings {
        IndexStageTimings {
            scan_ms: self.scan_ms,
            parse_ms: self.parse_ms,
            embed_ms: self.embed_ms,
            store_ms: self.store_ms,
            total_ms: run_started_at.elapsed().as_millis(),
        }
    }
}

fn compute_file_hash(content: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content);
    let hash = hasher.finalize();
    hash.iter().map(|b| format!("{:02x}", b)).collect()
}

fn generate_segment_id(file_path: &str, line_start: usize, line_end: usize) -> String {
    let mut hasher = Sha256::new();
    hasher.update(format!("{}:{}:{}", file_path, line_start, line_end).as_bytes());
    let hash = hasher.finalize();
    hash.iter()
        .map(|b| format!("{:02x}", b))
        .collect::<String>()[..16]
        .to_string()
}

fn serialize_embedding(vec: &[f32]) -> Result<String, OneupError> {
    serde_json::to_string(vec)
        .map_err(|e| IndexingError::Pipeline(format!("serialize embedding: {e}")).into())
}

fn should_embed_segment(seg: &ParsedSegment) -> bool {
    if seg.block_type != "chunk" {
        return true;
    }

    !matches!(
        seg.language.as_str(),
        "json"
            | "yaml"
            | "toml"
            | "protobuf"
            | "terraform"
            | "sql"
            | "config"
            | "makefile"
            | "dockerfile"
    )
}

#[derive(Debug, Clone)]
struct ScannedWorkItem {
    sequence_id: usize,
    relative_path: String,
    path: PathBuf,
    extension: String,
    stored_hash: Option<String>,
}

#[derive(Debug)]
struct ParsedWorkItem {
    relative_path: String,
    file_hash: String,
    segments: Vec<ParsedSegment>,
}

#[derive(Debug)]
enum ParseSkipReason {
    EmptySegments,
    Unchanged,
    Unreadable,
    UnsupportedExtension(String),
}

#[derive(Debug)]
enum ParseResultKind {
    Ready(ParsedWorkItem),
    Skipped(ParseSkipReason),
}

#[derive(Debug)]
struct ParseResult {
    sequence_id: usize,
    outcome: ParseResultKind,
    completed_at_ms: u128,
}

fn relative_path_for(project_root: &Path, path: &Path) -> String {
    let project_root = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    canonical
        .strip_prefix(&project_root)
        .unwrap_or(&canonical)
        .to_string_lossy()
        .to_string()
}

fn build_scanned_work_items(
    project_root: &Path,
    scanned: Vec<scanner::ScannedFile>,
    stored_hashes: &HashMap<String, String>,
) -> Vec<ScannedWorkItem> {
    let project_root = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());

    scanned
        .into_iter()
        .enumerate()
        .map(|(sequence_id, scanned_file)| {
            let relative_path = relative_path_for(&project_root, &scanned_file.path);
            let stored_hash = stored_hashes.get(&relative_path).cloned();
            ScannedWorkItem {
                sequence_id,
                relative_path,
                path: scanned_file.path,
                extension: scanned_file.extension,
                stored_hash,
            }
        })
        .collect()
}

struct RunInputs {
    scanned_files: Vec<ScannedWorkItem>,
    deleted_paths: Vec<String>,
}

enum ScopePreparation {
    Ready(RunInputs),
    FallbackToFull(String),
}

fn requires_full_scope_fallback(relative_path: &Path) -> bool {
    relative_path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| matches!(name, ".gitignore" | ".ignore"))
        || relative_path == Path::new(".git").join("info").join("exclude")
}

fn is_known_extensionless_file(relative_path: &Path) -> bool {
    relative_path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            matches!(
                name.to_ascii_lowercase().as_str(),
                "dockerfile" | "makefile" | "justfile"
            )
        })
}

async fn prepare_full_run_inputs(
    conn: &Connection,
    project_root: &Path,
) -> Result<RunInputs, OneupError> {
    let scanned = scanner::scan_directory(project_root)?;
    let stored_hashes = segments::get_all_file_hashes(conn).await?;
    let scanned_files = build_scanned_work_items(project_root, scanned, &stored_hashes);

    let scanned_paths: HashSet<String> = scanned_files
        .iter()
        .map(|file| file.relative_path.clone())
        .collect();
    let indexed_paths: HashSet<String> = segments::get_all_file_paths(conn)
        .await?
        .into_iter()
        .collect();
    let deleted_paths = indexed_paths.difference(&scanned_paths).cloned().collect();

    Ok(RunInputs {
        scanned_files,
        deleted_paths,
    })
}

async fn prepare_scoped_run_inputs(
    conn: &Connection,
    project_root: &Path,
    changed_paths: &std::collections::BTreeSet<PathBuf>,
) -> Result<ScopePreparation, OneupError> {
    if changed_paths.is_empty() {
        return Ok(ScopePreparation::Ready(RunInputs {
            scanned_files: Vec::new(),
            deleted_paths: Vec::new(),
        }));
    }

    let mut scoped_scan_results: HashMap<String, scanner::ScannedFile> =
        scanner::scan_paths(project_root, changed_paths)?
            .into_iter()
            .map(|file| (relative_path_for(project_root, &file.path), file))
            .collect();

    let mut scanned_files = Vec::new();
    let mut deleted_paths = Vec::new();

    for relative_path in changed_paths {
        if requires_full_scope_fallback(relative_path) {
            return Ok(ScopePreparation::FallbackToFull(format!(
                "path {} changes ignore semantics",
                relative_path.display()
            )));
        }

        let relative_string = relative_path.to_string_lossy().to_string();
        let absolute_path = project_root.join(relative_path);

        if absolute_path.exists() {
            if !absolute_path.is_file() {
                return Ok(ScopePreparation::FallbackToFull(format!(
                    "path {} resolved to a directory",
                    relative_path.display()
                )));
            }

            let stored_hash = segments::get_file_hash(conn, &relative_string).await?;
            if let Some(scanned_file) = scoped_scan_results.remove(&relative_string) {
                scanned_files.push(ScannedWorkItem {
                    sequence_id: scanned_files.len(),
                    relative_path: relative_string,
                    path: scanned_file.path,
                    extension: scanned_file.extension,
                    stored_hash,
                });
                continue;
            }

            if scanner::is_scannable_file(&absolute_path) {
                return Ok(ScopePreparation::FallbackToFull(format!(
                    "path {} is excluded by full-scan ignore semantics",
                    relative_path.display()
                )));
            }

            if stored_hash.is_some() {
                return Ok(ScopePreparation::FallbackToFull(format!(
                    "path {} no longer matches scanner filters",
                    relative_path.display()
                )));
            }

            continue;
        }

        if segments::get_file_hash(conn, &relative_string)
            .await?
            .is_some()
        {
            deleted_paths.push(relative_string);
            continue;
        }

        if relative_path.extension().is_none() && !is_known_extensionless_file(relative_path) {
            return Ok(ScopePreparation::FallbackToFull(format!(
                "path {} disappeared without indexed content",
                relative_path.display()
            )));
        }
    }

    Ok(ScopePreparation::Ready(RunInputs {
        scanned_files,
        deleted_paths,
    }))
}

fn parse_scanned_file(scanned_file: ScannedWorkItem) -> ParseResultKind {
    let content = match std::fs::read_to_string(&scanned_file.path) {
        Ok(content) => content,
        Err(err) => {
            info!(
                "skipping unreadable file {}: {err}",
                scanned_file.path.display()
            );
            return ParseResultKind::Skipped(ParseSkipReason::Unreadable);
        }
    };

    let file_hash = compute_file_hash(content.as_bytes());
    if scanned_file.stored_hash.as_deref() == Some(file_hash.as_str()) {
        debug!("skipping unchanged file: {}", scanned_file.relative_path);
        return ParseResultKind::Skipped(ParseSkipReason::Unchanged);
    }

    let segments = if parser::use_structural_parser(&scanned_file.extension) {
        match parser::parse_file(&content, &scanned_file.extension) {
            Ok(segments) => segments,
            Err(err) => {
                info!(
                    "tree-sitter parse failed for {}, falling back to text chunker: {err}",
                    scanned_file.relative_path
                );
                chunker::chunk_file_default(&content, &scanned_file.extension)
            }
        }
    } else if parser::is_language_supported(&scanned_file.extension) {
        chunker::chunk_file_default(&content, &scanned_file.extension)
    } else {
        debug!(
            "skipping unsupported extension .{}: {}",
            scanned_file.extension, scanned_file.relative_path
        );
        return ParseResultKind::Skipped(ParseSkipReason::UnsupportedExtension(
            scanned_file.extension,
        ));
    };

    if segments.is_empty() {
        debug!(
            "skipping file with no parsed segments: {}",
            scanned_file.relative_path
        );
        return ParseResultKind::Skipped(ParseSkipReason::EmptySegments);
    }

    ParseResultKind::Ready(ParsedWorkItem {
        relative_path: scanned_file.relative_path,
        file_hash,
        segments,
    })
}

fn build_segment_insert(
    relative_path: &str,
    file_hash: &str,
    segment: &ParsedSegment,
    embedding_vec: Option<String>,
) -> SegmentInsert {
    SegmentInsert {
        id: generate_segment_id(relative_path, segment.line_start, segment.line_end),
        file_path: relative_path.to_string(),
        language: segment.language.clone(),
        block_type: segment.block_type.clone(),
        content: segment.content.clone(),
        line_start: segment.line_start as i64,
        line_end: segment.line_end as i64,
        embedding_vec,
        breadcrumb: segment.breadcrumb.clone(),
        complexity: segment.complexity as i64,
        role: format!("{:?}", segment.role).to_uppercase(),
        defined_symbols: serde_json::to_string(&segment.defined_symbols)
            .unwrap_or_else(|_| "[]".into()),
        referenced_symbols: serde_json::to_string(&segment.referenced_symbols)
            .unwrap_or_else(|_| "[]".into()),
        called_symbols: serde_json::to_string(&segment.called_symbols)
            .unwrap_or_else(|_| "[]".into()),
        file_hash: file_hash.to_string(),
    }
}

fn build_segment_batches(
    parsed_files: &[ParsedWorkItem],
    embedder: Option<&mut Embedder>,
    timings: &mut TimingAccumulator,
) -> Result<Vec<Vec<SegmentInsert>>, OneupError> {
    let mut embeddings = if let Some(embedder) = embedder {
        let texts: Vec<&str> = parsed_files
            .iter()
            .flat_map(|file| {
                file.segments
                    .iter()
                    .filter(|segment| should_embed_segment(segment))
                    .map(|segment| segment.content.as_str())
            })
            .collect();
        let embed_started_at = Instant::now();
        let embeddings = embedder.embed_batch(&texts)?;
        timings.embed_ms += embed_started_at.elapsed().as_millis();
        Some(embeddings.into_iter())
    } else {
        None
    };

    let mut batches = Vec::with_capacity(parsed_files.len());
    for file in parsed_files {
        let mut inserts = Vec::with_capacity(file.segments.len());
        for segment in &file.segments {
            let embedding_vec = if should_embed_segment(segment) {
                match embeddings.as_mut() {
                    Some(embeddings) => {
                        let embedding = embeddings.next().ok_or_else(|| {
                            IndexingError::Pipeline(format!(
                                "missing embedding for {}:{}-{}",
                                file.relative_path, segment.line_start, segment.line_end
                            ))
                        })?;
                        Some(serialize_embedding(&embedding)?)
                    }
                    None => None,
                }
            } else {
                None
            };

            inserts.push(build_segment_insert(
                &file.relative_path,
                &file.file_hash,
                segment,
                embedding_vec,
            ));
        }
        batches.push(inserts);
    }

    if let Some(embeddings) = embeddings.as_mut() {
        debug_assert!(
            embeddings.next().is_none(),
            "unexpected trailing embeddings after pipeline run"
        );
    }

    Ok(batches)
}

async fn replace_file_batches(
    conn: &Connection,
    parsed_files: &[ParsedWorkItem],
    segment_batches: &[Vec<SegmentInsert>],
) -> Result<(), OneupError> {
    if parsed_files.len() == 1 {
        return segments::replace_file_segments_tx(
            conn,
            &parsed_files[0].relative_path,
            &segment_batches[0],
        )
        .await;
    }

    let file_batches: Vec<FileSegmentBatch<'_>> = parsed_files
        .iter()
        .zip(segment_batches.iter())
        .map(|(file, segments)| FileSegmentBatch {
            file_path: file.relative_path.as_str(),
            segments,
        })
        .collect();

    segments::replace_file_batch_tx(conn, &file_batches).await
}

async fn store_ready_files(
    conn: &Connection,
    ready_files: &mut Vec<ParsedWorkItem>,
    embedder: Option<&mut Embedder>,
    stats: &mut PipelineStats,
    timings: &mut TimingAccumulator,
) -> Result<(), OneupError> {
    if ready_files.is_empty() {
        return Ok(());
    }

    let parsed_files = std::mem::take(ready_files);
    let segment_batches = build_segment_batches(&parsed_files, embedder, timings)?;
    let segment_count = segment_batches.iter().map(Vec::len).sum::<usize>();

    let store_started_at = Instant::now();
    replace_file_batches(conn, &parsed_files, &segment_batches).await?;
    timings.store_ms += store_started_at.elapsed().as_millis();

    stats.files_indexed += parsed_files.len();
    stats.segments_stored += segment_count;
    Ok(())
}

async fn delete_removed_files(
    conn: &Connection,
    deleted_paths: &[String],
    batch_size: usize,
    timings: &mut TimingAccumulator,
) -> Result<(), OneupError> {
    let store_started_at = Instant::now();

    for chunk in deleted_paths.chunks(batch_size.max(1)) {
        if chunk.len() == 1 {
            segments::replace_file_segments_tx(conn, &chunk[0], &[]).await?;
            continue;
        }

        let file_batches: Vec<FileSegmentBatch<'_>> = chunk
            .iter()
            .map(|path| FileSegmentBatch {
                file_path: path.as_str(),
                segments: &[],
            })
            .collect();
        segments::replace_file_batch_tx(conn, &file_batches).await?;
    }

    timings.store_ms += store_started_at.elapsed().as_millis();
    Ok(())
}

fn current_progress_phase(stats: &PipelineStats) -> IndexPhase {
    if stats.files_indexed > 0 {
        IndexPhase::Storing
    } else {
        IndexPhase::Parsing
    }
}

struct FlushState<'a> {
    stats: &'a mut PipelineStats,
    project_root: &'a Path,
    files_total: usize,
    parallelism: Option<IndexParallelism>,
    timings: &'a mut TimingAccumulator,
    run_started_at: Instant,
    unsupported_extensions: &'a mut HashSet<String>,
}

impl FlushState<'_> {
    fn refresh(&mut self, phase: IndexPhase) {
        refresh_progress(
            self.stats,
            self.project_root,
            IndexState::Running,
            phase,
            self.files_total,
            self.parallelism.clone(),
            Some(self.timings.snapshot(self.run_started_at)),
        );
    }
}

async fn flush_reorder_buffer(
    conn: &Connection,
    reorder_buffer: &mut BTreeMap<usize, ParseResultKind>,
    next_sequence: &mut usize,
    config: &IndexingConfig,
    embedder: &mut Option<&mut Embedder>,
    state: &mut FlushState<'_>,
) -> Result<(), OneupError> {
    let mut ready_files = Vec::new();

    while let Some(result) = reorder_buffer.remove(next_sequence) {
        match result {
            ParseResultKind::Ready(file) => {
                ready_files.push(file);
                *next_sequence += 1;

                if ready_files.len() >= config.write_batch_files {
                    {
                        let embedder = embedder.as_mut().map(|embedder| &mut **embedder);
                        store_ready_files(
                            conn,
                            &mut ready_files,
                            embedder,
                            state.stats,
                            state.timings,
                        )
                        .await?;
                    }
                    state.refresh(IndexPhase::Storing);
                }
            }
            ParseResultKind::Skipped(reason) => {
                if !ready_files.is_empty() {
                    {
                        let embedder = embedder.as_mut().map(|embedder| &mut **embedder);
                        store_ready_files(
                            conn,
                            &mut ready_files,
                            embedder,
                            state.stats,
                            state.timings,
                        )
                        .await?;
                    }
                    state.refresh(IndexPhase::Storing);
                }

                if let ParseSkipReason::UnsupportedExtension(extension) = reason {
                    state.unsupported_extensions.insert(extension);
                }

                state.stats.files_skipped += 1;
                *next_sequence += 1;
                state.refresh(current_progress_phase(state.stats));
            }
        }
    }

    if !ready_files.is_empty() {
        {
            let embedder = embedder.as_mut().map(|embedder| &mut **embedder);
            store_ready_files(conn, &mut ready_files, embedder, state.stats, state.timings).await?;
        }
        state.refresh(IndexPhase::Storing);
    }

    Ok(())
}

/// Statistics returned after a pipeline run.
#[derive(Debug, Clone)]
pub struct PipelineStats {
    pub files_scanned: usize,
    pub files_indexed: usize,
    pub files_skipped: usize,
    pub files_deleted: usize,
    pub segments_stored: usize,
    pub embeddings_generated: bool,
    pub progress: IndexProgress,
}

impl Default for PipelineStats {
    fn default() -> Self {
        Self {
            files_scanned: 0,
            files_indexed: 0,
            files_skipped: 0,
            files_deleted: 0,
            segments_stored: 0,
            embeddings_generated: false,
            progress: IndexProgress::pending(),
        }
    }
}

/// Run the indexing pipeline on a project root directory.
///
/// Scans for source files, computes SHA-256 hashes for incremental detection,
/// parses/chunks files, generates embeddings, and stores segments in the database.
/// Deleted files have their segments removed.
#[allow(dead_code)]
pub async fn run(
    conn: &Connection,
    project_root: &Path,
    embedder: Option<&mut Embedder>,
) -> Result<PipelineStats, OneupError> {
    run_with_config(conn, project_root, embedder, &IndexingConfig::auto()).await
}

pub async fn run_with_config(
    conn: &Connection,
    project_root: &Path,
    embedder: Option<&mut Embedder>,
    config: &IndexingConfig,
) -> Result<PipelineStats, OneupError> {
    run_with_scope(conn, project_root, embedder, &RunScope::Full, config).await
}

pub async fn run_with_scope(
    conn: &Connection,
    project_root: &Path,
    embedder: Option<&mut Embedder>,
    scope: &RunScope,
    config: &IndexingConfig,
) -> Result<PipelineStats, OneupError> {
    let run_inputs = match scope {
        RunScope::Full => prepare_full_run_inputs(conn, project_root).await?,
        RunScope::Paths(changed_paths) => {
            match prepare_scoped_run_inputs(conn, project_root, changed_paths).await? {
                ScopePreparation::Ready(run_inputs) => run_inputs,
                ScopePreparation::FallbackToFull(reason) => {
                    info!(
                        "scoped run for {} fell back to a full scan: {}",
                        project_root.display(),
                        reason
                    );
                    prepare_full_run_inputs(conn, project_root).await?
                }
            }
        }
    };

    execute_run_with_inputs(conn, project_root, embedder, config, run_inputs).await
}

async fn execute_run_with_inputs(
    conn: &Connection,
    project_root: &Path,
    embedder: Option<&mut Embedder>,
    config: &IndexingConfig,
    run_inputs: RunInputs,
) -> Result<PipelineStats, OneupError> {
    let run_started_at = Instant::now();
    let mut stats = PipelineStats::default();
    let mut timings = TimingAccumulator::default();
    let mut embedder = embedder;
    let RunInputs {
        scanned_files,
        deleted_paths,
    } = run_inputs;

    let has_embedder = embedder.is_some();
    stats.embeddings_generated = has_embedder;
    let mut parallelism = Some(config.reporting_parallelism(0, has_embedder));

    if !has_embedder {
        info!("embedding model not available: indexing without embeddings (semantic search will be degraded, FTS-only mode active)");
    } else {
        schema::check_embedding_model_compatible(conn, HF_MODEL_REPO, EMBEDDING_DIM).await?;
    }

    refresh_progress(
        &mut stats,
        project_root,
        IndexState::Running,
        IndexPhase::Scanning,
        0,
        parallelism.clone(),
        Some(timings.snapshot(run_started_at)),
    );

    let progress_spinner = spin("Scanning files");
    let scan_started_at = Instant::now();

    stats.files_scanned = scanned_files.len();
    progress_spinner.update(format!("Scanning {} files", scanned_files.len()));
    let total_files = scanned_files.len();
    timings.scan_ms = scan_started_at.elapsed().as_millis();
    parallelism = Some(config.reporting_parallelism(total_files, has_embedder));

    if let Some(parallelism) = &parallelism {
        info!(
            "scan stage complete: {} files discovered in {}ms (jobs configured {}, effective {}, embed threads {})",
            total_files,
            timings.scan_ms,
            parallelism.jobs_configured,
            parallelism.jobs_effective,
            parallelism.embed_threads,
        );
    }

    if !deleted_paths.is_empty() {
        let store_before_delete = timings.store_ms;
        delete_removed_files(conn, &deleted_paths, config.write_batch_files, &mut timings).await?;
        for path in &deleted_paths {
            debug!("removed segments for deleted file: {path}");
        }
        stats.files_deleted = deleted_paths.len();
        info!(
            "delete cleanup complete: {} files removed in {}ms",
            deleted_paths.len(),
            timings.store_ms.saturating_sub(store_before_delete),
        );
    }

    refresh_progress(
        &mut stats,
        project_root,
        IndexState::Running,
        IndexPhase::Parsing,
        total_files,
        parallelism.clone(),
        Some(timings.snapshot(run_started_at)),
    );

    progress_spinner.update(format!("Processing files (0/{total_files})"));
    let parse_started_at = Instant::now();

    let mut reorder_buffer = BTreeMap::new();
    let mut parse_workers = JoinSet::new();
    let mut next_to_dispatch = 0usize;
    let mut next_to_flush = 0usize;
    let mut unsupported_extensions: HashSet<String> = HashSet::new();
    {
        let mut flush_state = FlushState {
            stats: &mut stats,
            project_root,
            files_total: total_files,
            parallelism: parallelism.clone(),
            timings: &mut timings,
            run_started_at,
            unsupported_extensions: &mut unsupported_extensions,
        };

        while next_to_dispatch < total_files || !parse_workers.is_empty() {
            while next_to_dispatch < total_files && parse_workers.len() < config.jobs {
                let scanned_file = scanned_files[next_to_dispatch].clone();
                let sequence_id = scanned_file.sequence_id;
                parse_workers.spawn_blocking(move || ParseResult {
                    sequence_id,
                    outcome: parse_scanned_file(scanned_file),
                    completed_at_ms: parse_started_at.elapsed().as_millis(),
                });
                next_to_dispatch += 1;
            }

            let Some(parse_result) = parse_workers.join_next().await else {
                break;
            };

            let parse_result = parse_result
                .map_err(|err| IndexingError::Pipeline(format!("parse worker failed: {err}")))?;
            flush_state.timings.parse_ms = flush_state
                .timings
                .parse_ms
                .max(parse_result.completed_at_ms);
            let previous = reorder_buffer.insert(parse_result.sequence_id, parse_result.outcome);
            debug_assert!(
                previous.is_none(),
                "duplicate parse result sequence {}",
                parse_result.sequence_id
            );

            flush_reorder_buffer(
                conn,
                &mut reorder_buffer,
                &mut next_to_flush,
                config,
                &mut embedder,
                &mut flush_state,
            )
            .await?;

            progress_spinner.update(format!("Processing files ({next_to_flush}/{total_files})"));
        }

        flush_reorder_buffer(
            conn,
            &mut reorder_buffer,
            &mut next_to_flush,
            config,
            &mut embedder,
            &mut flush_state,
        )
        .await?;
    }

    if !unsupported_extensions.is_empty() {
        let mut exts: Vec<&str> = unsupported_extensions.iter().map(|s| s.as_str()).collect();
        exts.sort();
        debug!("skipped unsupported file types: .{}", exts.join(", ."));
    }

    info!(
        "parse stage complete: {} files processed in {}ms",
        total_files, timings.parse_ms
    );

    progress_spinner.success_with(format!(
        "Processed {} files: {} indexed, {} skipped, {} deleted, {} segments",
        total_files,
        stats.files_indexed,
        stats.files_skipped,
        stats.files_deleted,
        stats.segments_stored,
    ));

    let final_timings = timings.snapshot(run_started_at);

    if let Some(parallelism) = &parallelism {
        info!(
            "pipeline complete: {} scanned, {} indexed, {} skipped, {} deleted, {} segments | jobs configured {}, effective {}, embed threads {} | timings scan={}ms parse={}ms embed={}ms store={}ms total={}ms",
            stats.files_scanned,
            stats.files_indexed,
            stats.files_skipped,
            stats.files_deleted,
            stats.segments_stored,
            parallelism.jobs_configured,
            parallelism.jobs_effective,
            parallelism.embed_threads,
            final_timings.scan_ms,
            final_timings.parse_ms,
            final_timings.embed_ms,
            final_timings.store_ms,
            final_timings.total_ms,
        );
    }

    refresh_progress(
        &mut stats,
        project_root,
        IndexState::Complete,
        IndexPhase::Complete,
        total_files,
        parallelism,
        Some(final_timings),
    );

    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{db::Db, schema};
    use std::fs;

    async fn setup() -> (Db, Connection) {
        let db = Db::open_memory().await.unwrap();
        let conn = db.connect().unwrap();
        schema::initialize(&conn).await.unwrap();
        (db, conn)
    }

    #[derive(Debug, PartialEq, Eq)]
    struct SegmentSnapshot {
        id: String,
        file_path: String,
        language: String,
        block_type: String,
        content: String,
        line_start: i64,
        line_end: i64,
        breadcrumb: Option<String>,
        complexity: i64,
        role: String,
        defined_symbols: String,
        referenced_symbols: String,
        called_symbols: String,
        file_hash: String,
    }

    async fn snapshot_segments(conn: &Connection) -> Vec<SegmentSnapshot> {
        let mut snapshots = Vec::new();
        for file_path in segments::get_all_file_paths(conn).await.unwrap() {
            for segment in segments::get_segments_by_file(conn, &file_path)
                .await
                .unwrap()
            {
                snapshots.push(SegmentSnapshot {
                    id: segment.id,
                    file_path: segment.file_path,
                    language: segment.language,
                    block_type: segment.block_type,
                    content: segment.content,
                    line_start: segment.line_start,
                    line_end: segment.line_end,
                    breadcrumb: segment.breadcrumb,
                    complexity: segment.complexity,
                    role: segment.role,
                    defined_symbols: segment.defined_symbols,
                    referenced_symbols: segment.referenced_symbols,
                    called_symbols: segment.called_symbols,
                    file_hash: segment.file_hash,
                });
            }
        }
        snapshots
    }

    #[tokio::test]
    async fn index_temp_directory_without_embedder() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("main.rs"),
            "fn hello() {\n    println!(\"hello\");\n}\n",
        )
        .unwrap();
        fs::write(
            tmp.path().join("notes.md"),
            "# Notes\n\nSome content here.\n",
        )
        .unwrap();

        let (_db, conn) = setup().await;
        let stats = run(&conn, tmp.path(), None).await.unwrap();

        assert_eq!(stats.files_scanned, 2);
        assert!(stats.files_indexed > 0);
        assert_eq!(stats.files_deleted, 0);
        assert!(!stats.embeddings_generated);
        assert!(stats.segments_stored > 0);

        let count = segments::count_segments(&conn).await.unwrap();
        assert!(count > 0);
    }

    #[tokio::test]
    async fn incremental_indexing_skips_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("main.rs"),
            "fn hello() {\n    println!(\"hello\");\n}\n",
        )
        .unwrap();

        let (_db, conn) = setup().await;

        let stats1 = run(&conn, tmp.path(), None).await.unwrap();
        assert!(stats1.files_indexed > 0);

        let stats2 = run(&conn, tmp.path(), None).await.unwrap();
        assert_eq!(stats2.files_indexed, 0);
        assert_eq!(stats2.files_skipped, 1);
    }

    #[tokio::test]
    async fn incremental_indexing_reindexes_changed() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("main.rs"), "fn hello() {}\n").unwrap();

        let (_db, conn) = setup().await;

        let stats1 = run(&conn, tmp.path(), None).await.unwrap();
        assert!(stats1.files_indexed > 0);

        fs::write(tmp.path().join("main.rs"), "fn hello() {}\nfn world() {}\n").unwrap();

        let stats2 = run(&conn, tmp.path(), None).await.unwrap();
        assert!(stats2.files_indexed > 0);
    }

    #[tokio::test]
    async fn deleted_files_removed_from_index() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a.rs"), "fn a() {}\n").unwrap();
        fs::write(tmp.path().join("b.rs"), "fn b() {}\n").unwrap();

        let (_db, conn) = setup().await;

        run(&conn, tmp.path(), None).await.unwrap();
        let paths1 = segments::get_all_file_paths(&conn).await.unwrap();
        assert_eq!(paths1.len(), 2);

        fs::remove_file(tmp.path().join("b.rs")).unwrap();

        let stats = run(&conn, tmp.path(), None).await.unwrap();
        assert_eq!(stats.files_deleted, 1);

        let paths2 = segments::get_all_file_paths(&conn).await.unwrap();
        assert_eq!(paths2.len(), 1);
    }

    #[tokio::test]
    async fn scoped_run_updates_only_changed_paths_and_deletions() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a.rs"), "fn a() {}\n").unwrap();
        fs::write(tmp.path().join("b.rs"), "fn b() {}\n").unwrap();
        fs::write(tmp.path().join("keep.rs"), "fn keep() {}\n").unwrap();

        let (_db, conn) = setup().await;
        run(&conn, tmp.path(), None).await.unwrap();

        fs::write(tmp.path().join("a.rs"), "fn a() {}\nfn a2() {}\n").unwrap();
        fs::remove_file(tmp.path().join("b.rs")).unwrap();
        fs::write(tmp.path().join("c.rs"), "fn c() {}\n").unwrap();

        let scope = RunScope::from_paths(["a.rs", "b.rs", "c.rs"].map(PathBuf::from)).unwrap();
        let stats = run_with_scope(
            &conn,
            tmp.path(),
            None,
            &scope,
            &IndexingConfig::new(2, 1, 1).unwrap(),
        )
        .await
        .unwrap();

        assert_eq!(stats.files_scanned, 2);
        assert_eq!(stats.files_deleted, 1);
        assert_eq!(stats.files_indexed, 2);

        let paths = segments::get_all_file_paths(&conn).await.unwrap();
        assert_eq!(paths, vec!["a.rs", "c.rs", "keep.rs"]);
    }

    #[tokio::test]
    async fn scoped_run_falls_back_to_full_scan_for_directory_scope() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src").join("lib.rs"), "pub fn alpha() {}\n").unwrap();
        fs::write(tmp.path().join("top.rs"), "pub fn top() {}\n").unwrap();

        let (_db, conn) = setup().await;
        run(&conn, tmp.path(), None).await.unwrap();

        fs::write(
            tmp.path().join("top.rs"),
            "pub fn top() {}\npub fn beta() {}\n",
        )
        .unwrap();

        let scope = RunScope::from_paths(["src", "top.rs"].map(PathBuf::from)).unwrap();
        let stats = run_with_scope(
            &conn,
            tmp.path(),
            None,
            &scope,
            &IndexingConfig::new(2, 1, 1).unwrap(),
        )
        .await
        .unwrap();

        assert_eq!(stats.files_scanned, 2);
        assert_eq!(stats.files_deleted, 0);
    }

    #[tokio::test]
    async fn scoped_run_falls_back_to_full_scan_for_hidden_existing_path() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("visible.rs"), "pub fn visible() {}\n").unwrap();

        let (_db, conn) = setup().await;
        run(&conn, tmp.path(), None).await.unwrap();

        fs::write(tmp.path().join(".hidden.rs"), "pub fn hidden() {}\n").unwrap();

        let scope = RunScope::from_paths([".hidden.rs"].map(PathBuf::from)).unwrap();
        let stats = run_with_scope(
            &conn,
            tmp.path(),
            None,
            &scope,
            &IndexingConfig::new(2, 1, 1).unwrap(),
        )
        .await
        .unwrap();

        assert_eq!(stats.files_scanned, 1);
        assert_eq!(stats.files_skipped, 1);
        assert_eq!(stats.files_deleted, 0);
        let paths = segments::get_all_file_paths(&conn).await.unwrap();
        assert_eq!(paths, vec!["visible.rs"]);
    }

    #[tokio::test]
    async fn scoped_run_falls_back_to_full_scan_for_git_excluded_existing_path() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".git").join("info")).unwrap();
        fs::write(tmp.path().join("visible.rs"), "pub fn visible() {}\n").unwrap();

        let (_db, conn) = setup().await;
        run(&conn, tmp.path(), None).await.unwrap();

        fs::write(
            tmp.path().join(".git").join("info").join("exclude"),
            "ignored.rs\n",
        )
        .unwrap();
        fs::write(tmp.path().join("ignored.rs"), "pub fn ignored() {}\n").unwrap();

        let scope = RunScope::from_paths(["ignored.rs"].map(PathBuf::from)).unwrap();
        let stats = run_with_scope(
            &conn,
            tmp.path(),
            None,
            &scope,
            &IndexingConfig::new(2, 1, 1).unwrap(),
        )
        .await
        .unwrap();

        assert_eq!(stats.files_scanned, 1);
        assert_eq!(stats.files_skipped, 1);
        assert_eq!(stats.files_deleted, 0);
        let paths = segments::get_all_file_paths(&conn).await.unwrap();
        assert_eq!(paths, vec!["visible.rs"]);
    }

    #[tokio::test]
    async fn scoped_run_falls_back_to_full_scan_for_git_exclude_file_change() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".git").join("info")).unwrap();
        fs::write(tmp.path().join("visible.rs"), "pub fn visible() {}\n").unwrap();
        fs::write(tmp.path().join("ignored.rs"), "pub fn ignored() {}\n").unwrap();

        let (_db, conn) = setup().await;
        run(&conn, tmp.path(), None).await.unwrap();

        fs::write(
            tmp.path().join(".git").join("info").join("exclude"),
            "ignored.rs\n",
        )
        .unwrap();

        let scope = RunScope::from_paths([PathBuf::from(".git/info/exclude")]).unwrap();
        let stats = run_with_scope(
            &conn,
            tmp.path(),
            None,
            &scope,
            &IndexingConfig::new(2, 1, 1).unwrap(),
        )
        .await
        .unwrap();

        assert_eq!(stats.files_scanned, 1);
        assert_eq!(stats.files_skipped, 1);
        assert_eq!(stats.files_deleted, 1);
        let paths = segments::get_all_file_paths(&conn).await.unwrap();
        assert_eq!(paths, vec!["visible.rs"]);
    }

    #[tokio::test]
    async fn parallel_pipeline_matches_single_job_for_incremental_changes() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("lib.rs"), "pub fn alpha() {}\n").unwrap();
        fs::write(
            tmp.path().join("notes.md"),
            "# Notes\n\nparallel indexing keeps results fresh.\n",
        )
        .unwrap();
        fs::write(tmp.path().join("config.ini"), "host=localhost\nport=5432\n").unwrap();
        fs::write(tmp.path().join("opaque.xyz"), "opaque\n").unwrap();

        let (_serial_db, serial_conn) = setup().await;
        let (_parallel_db, parallel_conn) = setup().await;
        let serial_config = IndexingConfig::new(1, 1, 1).unwrap();
        let parallel_config = IndexingConfig::new(4, 1, 2).unwrap();

        let serial_first = run_with_config(&serial_conn, tmp.path(), None, &serial_config)
            .await
            .unwrap();
        let parallel_first = run_with_config(&parallel_conn, tmp.path(), None, &parallel_config)
            .await
            .unwrap();

        assert!(serial_first.files_indexed > 0);
        assert!(parallel_first.files_indexed > 0);
        assert!(serial_first.segments_stored > 0);
        assert!(parallel_first.segments_stored > 0);
        assert_eq!(parallel_first.files_indexed, serial_first.files_indexed);
        assert_eq!(parallel_first.files_skipped, serial_first.files_skipped);
        assert_eq!(parallel_first.files_deleted, serial_first.files_deleted);
        assert_eq!(
            snapshot_segments(&parallel_conn).await,
            snapshot_segments(&serial_conn).await
        );

        fs::write(
            tmp.path().join("lib.rs"),
            "pub fn alpha() {}\npub fn beta() {}\n",
        )
        .unwrap();
        fs::remove_file(tmp.path().join("notes.md")).unwrap();
        fs::write(
            tmp.path().join("readme.txt"),
            "parallel indexing keeps writes ordered\nwhile skipping unchanged files\n",
        )
        .unwrap();

        let serial_second = run_with_config(&serial_conn, tmp.path(), None, &serial_config)
            .await
            .unwrap();
        let parallel_second = run_with_config(&parallel_conn, tmp.path(), None, &parallel_config)
            .await
            .unwrap();

        assert!(serial_second.files_indexed > 0);
        assert!(parallel_second.files_indexed > 0);
        assert!(serial_second.segments_stored > 0);
        assert!(parallel_second.segments_stored > 0);
        assert_eq!(parallel_second.files_indexed, serial_second.files_indexed);
        assert_eq!(parallel_second.files_skipped, serial_second.files_skipped);
        assert_eq!(parallel_second.files_deleted, serial_second.files_deleted);
        assert_eq!(
            snapshot_segments(&parallel_conn).await,
            snapshot_segments(&serial_conn).await
        );
    }

    #[tokio::test]
    async fn persisted_progress_snapshot_includes_parallelism_and_timings() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("lib.rs"), "pub fn alpha() {}\n").unwrap();

        let (_db, conn) = setup().await;
        let config = IndexingConfig::new(3, 2, 1).unwrap();
        let stats = run_with_config(&conn, tmp.path(), None, &config)
            .await
            .unwrap();
        let timings = stats.progress.timings.as_ref().unwrap();

        assert_eq!(
            stats.progress.parallelism.as_ref().unwrap().jobs_configured,
            3
        );
        assert_eq!(
            stats.progress.parallelism.as_ref().unwrap().jobs_effective,
            1
        );
        assert_eq!(
            stats.progress.parallelism.as_ref().unwrap().embed_threads,
            0
        );
        assert!(timings.total_ms >= timings.scan_ms);

        let progress_path = index_progress_path(tmp.path());
        let persisted: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(progress_path).unwrap()).unwrap();

        assert_eq!(persisted["parallelism"]["jobs_configured"], 3);
        assert_eq!(persisted["parallelism"]["jobs_effective"], 1);
        assert_eq!(persisted["parallelism"]["embed_threads"], 0);
        assert!(
            persisted["timings"]["total_ms"].as_u64().unwrap()
                >= persisted["timings"]["scan_ms"].as_u64().unwrap()
        );
    }

    #[tokio::test]
    async fn persisted_progress_reports_embed_threads_when_embeddings_enabled() {
        if !crate::indexer::embedder::is_model_available() {
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("lib.rs"), "pub fn alpha() {}\n").unwrap();

        let (_db, conn) = setup().await;
        let config = IndexingConfig::new(2, 2, 1).unwrap();
        let model_dir = crate::shared::config::model_dir().unwrap();
        let mut embedder =
            Embedder::from_dir_with_threads(&model_dir, config.embed_threads).unwrap();

        let stats = run_with_config(&conn, tmp.path(), Some(&mut embedder), &config)
            .await
            .unwrap();

        assert!(stats.embeddings_generated);
        assert_eq!(
            stats.progress.parallelism.as_ref().unwrap().jobs_configured,
            config.jobs
        );
        assert_eq!(
            stats.progress.parallelism.as_ref().unwrap().jobs_effective,
            1
        );
        assert_eq!(
            stats.progress.parallelism.as_ref().unwrap().embed_threads,
            config.embed_threads
        );

        let progress_path = index_progress_path(tmp.path());
        let persisted: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(progress_path).unwrap()).unwrap();
        assert_eq!(persisted["parallelism"]["embed_threads"], 2);
    }

    #[tokio::test]
    async fn empty_directory_produces_no_segments() {
        let tmp = tempfile::tempdir().unwrap();

        let (_db, conn) = setup().await;
        let stats = run(&conn, tmp.path(), None).await.unwrap();

        assert_eq!(stats.files_scanned, 0);
        assert_eq!(stats.files_indexed, 0);
        assert_eq!(stats.segments_stored, 0);
    }

    #[tokio::test]
    async fn routes_supported_language_to_parser() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("lib.rs"),
            "pub struct Foo {\n    pub x: i32,\n}\n\nimpl Foo {\n    pub fn new() -> Self {\n        Self { x: 0 }\n    }\n}\n",
        )
        .unwrap();

        let (_db, conn) = setup().await;
        let stats = run(&conn, tmp.path(), None).await.unwrap();

        assert_eq!(stats.files_indexed, 1);
        assert!(stats.segments_stored > 0);

        let segs = segments::get_segments_by_file(&conn, "lib.rs")
            .await
            .unwrap();
        let has_struct = segs.iter().any(|s| s.block_type == "struct");
        assert!(has_struct, "parser should extract struct segments");
    }

    #[tokio::test]
    async fn indexes_text_documents_via_chunking() {
        let tmp = tempfile::tempdir().unwrap();
        let lines: Vec<String> = (1..=100).map(|i| format!("line {i}")).collect();
        fs::write(tmp.path().join("readme.txt"), lines.join("\n")).unwrap();

        let (_db, conn) = setup().await;
        let stats = run(&conn, tmp.path(), None).await.unwrap();

        assert_eq!(stats.files_indexed, 1);
        assert!(stats.segments_stored > 0);

        let segs = segments::get_segments_by_file(&conn, "readme.txt")
            .await
            .unwrap();
        assert!(segs.iter().all(|s| s.block_type == "chunk"));
    }

    #[tokio::test]
    async fn segment_ids_are_deterministic() {
        let id1 = generate_segment_id("src/main.rs", 1, 10);
        let id2 = generate_segment_id("src/main.rs", 1, 10);
        let id3 = generate_segment_id("src/main.rs", 1, 11);
        assert_eq!(id1, id2);
        assert_ne!(id1, id3);
    }

    #[tokio::test]
    async fn file_hash_computation_is_consistent() {
        let hash1 = compute_file_hash(b"hello world");
        let hash2 = compute_file_hash(b"hello world");
        let hash3 = compute_file_hash(b"hello world!");
        assert_eq!(hash1, hash2);
        assert_ne!(hash1, hash3);
    }

    #[test]
    fn selective_embedding_skips_low_semantic_chunk_formats() {
        let code_chunk = ParsedSegment {
            content: "fn hello() {}".into(),
            block_type: "function".into(),
            line_start: 1,
            line_end: 1,
            language: "rust".into(),
            breadcrumb: None,
            complexity: 0,
            role: crate::shared::types::SegmentRole::Definition,
            defined_symbols: vec!["hello".into()],
            referenced_symbols: Vec::new(),
            called_symbols: Vec::new(),
        };
        assert!(should_embed_segment(&code_chunk));

        let markdown_chunk = ParsedSegment {
            block_type: "chunk".into(),
            language: "markdown".into(),
            ..code_chunk.clone()
        };
        assert!(should_embed_segment(&markdown_chunk));

        let proto_chunk = ParsedSegment {
            block_type: "chunk".into(),
            language: "protobuf".into(),
            ..code_chunk.clone()
        };
        assert!(!should_embed_segment(&proto_chunk));

        let yaml_chunk = ParsedSegment {
            block_type: "chunk".into(),
            language: "yaml".into(),
            ..code_chunk
        };
        assert!(!should_embed_segment(&yaml_chunk));
    }

    #[tokio::test]
    async fn skips_unknown_file_types() {
        let tmp = tempfile::tempdir().unwrap();
        let lines: Vec<String> = (1..=30)
            .map(|i| format!("config_line_{i} = value"))
            .collect();
        fs::write(tmp.path().join("config.ini"), lines.join("\n")).unwrap();
        fs::write(tmp.path().join("archive.xyz"), "opaque").unwrap();

        let (_db, conn) = setup().await;
        let stats = run(&conn, tmp.path(), None).await.unwrap();

        assert_eq!(stats.files_indexed, 1);
        assert_eq!(stats.files_skipped, 1);

        let ini_segs = segments::get_segments_by_file(&conn, "config.ini")
            .await
            .unwrap();
        assert!(
            !ini_segs.is_empty(),
            "ini files should now be chunk-indexed"
        );
    }

    #[tokio::test]
    async fn pipeline_without_embedder_stores_no_embeddings() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("main.rs"),
            "fn hello() {\n    println!(\"hi\");\n}\n",
        )
        .unwrap();

        let (_db, conn) = setup().await;
        let stats = run(&conn, tmp.path(), None).await.unwrap();

        assert!(!stats.embeddings_generated);
        assert!(stats.files_indexed > 0);
    }

    #[tokio::test]
    async fn low_semantic_chunked_files_are_indexed_without_embeddings() {
        if !crate::indexer::embedder::is_model_available() {
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        let lines: Vec<String> = (1..=80).map(|i| format!("field_{i} = value_{i}")).collect();
        fs::write(tmp.path().join("config.ini"), lines.join("\n")).unwrap();
        fs::write(
            tmp.path().join("notes.md"),
            "# Heading\n\nThis explains the system.\n",
        )
        .unwrap();

        let (_db, conn) = setup().await;
        let mut embedder = Embedder::new().await.unwrap();
        let stats = run(&conn, tmp.path(), Some(&mut embedder)).await.unwrap();

        assert!(stats.embeddings_generated);
        assert_eq!(stats.files_indexed, 2);

        let mut rows = conn
            .query(
                "SELECT s.file_path, COUNT(v.segment_id) > 0
                 FROM segments AS s
                 LEFT JOIN segment_vectors AS v ON v.segment_id = s.id
                 GROUP BY s.id, s.file_path, s.line_start
                 ORDER BY s.file_path, s.line_start",
                (),
            )
            .await
            .unwrap();

        let mut saw_ini_without_embedding = false;
        let mut saw_markdown_with_embedding = false;
        while let Some(row) = rows.next().await.unwrap() {
            let file_path: String = row.get(0).unwrap();
            let has_embedding: i64 = row.get(1).unwrap();
            if file_path == "config.ini" && has_embedding == 0 {
                saw_ini_without_embedding = true;
            }
            if file_path == "notes.md" && has_embedding == 1 {
                saw_markdown_with_embedding = true;
            }
        }

        assert!(saw_ini_without_embedding);
        assert!(saw_markdown_with_embedding);
    }

    #[tokio::test]
    async fn mixed_code_docs_and_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("lib.rs"), "pub fn foo() {}\n").unwrap();
        fs::write(tmp.path().join("notes.txt"), "some notes\n").unwrap();
        fs::write(tmp.path().join("config.ini"), "key=val\n").unwrap();
        fs::write(tmp.path().join("opaque.xyz"), "blob\n").unwrap();

        let (_db, conn) = setup().await;
        let stats = run(&conn, tmp.path(), None).await.unwrap();

        assert_eq!(stats.files_indexed, 3, "rs + txt + ini should be indexed");
        assert_eq!(
            stats.files_skipped, 1,
            "unknown extension should be skipped"
        );

        let rs_segs = segments::get_segments_by_file(&conn, "lib.rs")
            .await
            .unwrap();
        assert!(
            rs_segs.iter().any(|s| s.block_type != "chunk"),
            "rust files should produce structural segments"
        );

        let txt_segs = segments::get_segments_by_file(&conn, "notes.txt")
            .await
            .unwrap();
        assert!(
            txt_segs.iter().all(|s| s.block_type == "chunk"),
            "txt files should produce chunk segments"
        );

        let ini_segs = segments::get_segments_by_file(&conn, "config.ini")
            .await
            .unwrap();
        assert!(
            ini_segs.iter().all(|s| s.block_type == "chunk"),
            "ini files should produce chunk segments"
        );
    }
}
