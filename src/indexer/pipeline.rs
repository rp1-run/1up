use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use libsql::Connection;
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

fn index_progress_path(project_root: &Path) -> std::path::PathBuf {
    config::project_dot_dir(project_root).join(INDEX_PROGRESS_FILE_NAME)
}

fn persist_progress(project_root: &Path, progress: &IndexProgress) {
    let payload = match serde_json::to_vec_pretty(progress) {
        Ok(payload) => payload,
        Err(err) => {
            debug!("failed to serialize index progress: {err}");
            return;
        }
    };

    // Write the progress file atomically (temp + fsync + rename) rather than
    // truncate-then-write. A plain `std::fs::write` opens the target with
    // `O_TRUNC`, so a concurrent reader (`1up status`/`list`/MCP, which parse
    // this file and silently drop a parse error to `None`) can observe a
    // zero-length or partial document and report `index_progress: null` for a
    // fully-ready index. The atomic rename guarantees every reader sees either
    // the previous complete document or the new one, never a torn one.
    let secure_root = match ensure_secure_project_root(project_root) {
        Ok(root) => root,
        Err(err) => {
            debug!(
                "failed to prepare secure project root for index progress {}: {err}",
                project_root.display()
            );
            return;
        }
    };

    let progress_path = index_progress_path(project_root);
    if let Err(err) = atomic_replace(
        &progress_path,
        &payload,
        &secure_root,
        PROJECT_STATE_DIR_MODE,
        SECURE_STATE_FILE_MODE,
    ) {
        debug!(
            "failed to persist index progress to {}: {err}",
            progress_path.display()
        );
    }
}

fn emit_progress(progress_tx: Option<&ProgressSender>, progress: &IndexProgress) {
    if let Some(progress_tx) = progress_tx {
        let _ = progress_tx.send(progress.clone());
    }
}

fn pipeline_progress_message(
    stats: &PipelineStats,
    phase: IndexPhase,
    files_total: usize,
) -> Option<String> {
    let message = match phase {
        IndexPhase::Pending => return None,
        IndexPhase::Preparing => "Preparing database".to_string(),
        IndexPhase::Rebuilding => "Rebuilding database".to_string(),
        IndexPhase::LoadingModel => "Loading embedding model".to_string(),
        IndexPhase::Scanning => {
            if files_total == 0 {
                "Scanning files".to_string()
            } else {
                format!("Scanning {files_total} files")
            }
        }
        IndexPhase::Parsing | IndexPhase::Storing => "Processing files".to_string(),
        IndexPhase::Complete => format!(
            "Processed {files_total} files: {} indexed, {} skipped, {} deleted, {} segments{}",
            stats.files_indexed,
            stats.files_skipped,
            stats.files_deleted,
            stats.segments_stored,
            if stats.embeddings_generated {
                ""
            } else {
                " [no embeddings]"
            },
        ),
    };

    Some(message)
}

fn pipeline_progress_ui_state(
    stats: &PipelineStats,
    phase: IndexPhase,
    files_total: usize,
) -> ProgressState {
    let message = pipeline_progress_message(stats, phase, files_total).unwrap_or_default();
    match phase {
        IndexPhase::Parsing | IndexPhase::Storing if files_total > 0 => {
            ProgressState::items(message, stats.files_processed as u64, files_total as u64)
        }
        _ => ProgressState::spinner(message),
    }
}

struct ProgressUpdate {
    state: IndexState,
    phase: IndexPhase,
    files_total: usize,
    parallelism: Option<IndexParallelism>,
    timings: Option<IndexStageTimings>,
    scope: Option<IndexScopeInfo>,
    prefilter: Option<IndexPrefilterInfo>,
    persist: bool,
}

fn refresh_progress(
    stats: &mut PipelineStats,
    project_root: &Path,
    context: &IndexRunContext,
    progress_tx: Option<&ProgressSender>,
    update: ProgressUpdate,
) {
    // Preserve scope once it's recorded in the first progress update.
    // Subsequent updates may not include scope, but we don't want to overwrite it with None.
    let scope = update.scope.or_else(|| stats.progress.scope.clone());

    stats.progress = IndexProgress {
        state: update.state,
        phase: update.phase,
        context_id: Some(context.context_id.clone()),
        source_root: Some(context.source_root.clone()),
        branch_name: context.branch_name.clone(),
        branch_status: Some(context.branch_status),
        files_total: update.files_total,
        files_scanned: stats.files_scanned,
        files_processed: stats.files_processed,
        files_indexed: stats.files_indexed,
        files_skipped: stats.files_skipped,
        files_deleted: stats.files_deleted,
        segments_stored: stats.segments_stored,
        embeddings_enabled: stats.embeddings_generated,
        embedding_unavailable_reason: stats.embedding_unavailable_reason.clone(),
        vector_rows: stats.vector_rows,
        embeddable_segments: stats.embeddable_segments,
        message: pipeline_progress_message(stats, update.phase, update.files_total),
        parallelism: update.parallelism,
        timings: update.timings,
        scope,
        prefilter: update.prefilter,
        indexer_pid: Some(std::process::id()),
        updated_at: chrono::Utc::now(),
    };
    if update.persist {
        persist_progress(project_root, &stats.progress);
    }
    emit_progress(progress_tx, &stats.progress);
}

use crate::indexer::chunker;
use crate::indexer::embedder::Embedder;
use crate::indexer::markdown;
use crate::indexer::parser;
use crate::indexer::scanner;
use crate::shared::config;
use crate::shared::constants::{
    DEFAULT_INDEX_CONTEXT_ID, EMBEDDING_DIM, EMBEDDING_MAX_TOKENS,
    GC_SUPERSEDED_SAME_SOURCE_KEEP_COUNT, GC_SUPERSEDED_SAME_SOURCE_MAX_AGE_DAYS, HF_MODEL_REPO,
    MAX_FILE_SIZE_BYTES, MAX_SEGMENTS_PER_FILE, NON_EMBEDDABLE_CHUNK_LANGUAGES,
    PROGRESS_PERSIST_THROTTLE_MS, PROJECT_STATE_DIR_MODE, SECURE_STATE_FILE_MODE,
};
use crate::shared::errors::{IndexingError, OneupError};
use crate::shared::fs::{atomic_replace, ensure_secure_project_root};
use crate::shared::progress::{ProgressState, ProgressUi};
use crate::shared::types::{
    BranchStatus, IndexParallelism, IndexPhase, IndexPrefilterInfo, IndexProgress, IndexScopeInfo,
    IndexStageTimings, IndexState, IndexingConfig, ParsedSegment, RunScope, SetupTimings,
    WorktreeContext,
};
use crate::storage::schema;
use crate::storage::segments::{self, FileSegmentBatch, IndexedFileMeta, SegmentInsert};

const INDEX_PROGRESS_FILE_NAME: &str = "index_status.json";
pub type ProgressSender = Sender<IndexProgress>;

#[derive(Debug, Clone)]
struct IndexRunContext {
    context_id: String,
    source_root: PathBuf,
    branch_name: Option<String>,
    branch_status: BranchStatus,
}

impl IndexRunContext {
    fn legacy(project_root: &Path) -> Self {
        Self {
            context_id: DEFAULT_INDEX_CONTEXT_ID.to_string(),
            source_root: project_root.to_path_buf(),
            branch_name: None,
            branch_status: BranchStatus::Unknown,
        }
    }

    fn from_worktree(context: &WorktreeContext) -> Self {
        Self {
            context_id: context.context_id.clone(),
            source_root: context.source_root.clone(),
            branch_name: context.branch_name.clone(),
            branch_status: context.branch_status,
        }
    }
}

#[derive(Debug, Default)]
struct TimingAccumulator {
    scan_ms: u128,
    parse_ms: u128,
    embed_ms: u128,
    store_ms: u128,
    db_prepare_ms: Option<u128>,
    model_prepare_ms: Option<u128>,
    input_prep_ms: Option<u128>,
}

impl TimingAccumulator {
    fn snapshot(&self, run_started_at: Instant) -> IndexStageTimings {
        IndexStageTimings {
            scan_ms: self.scan_ms,
            parse_ms: self.parse_ms,
            embed_ms: self.embed_ms,
            store_ms: self.store_ms,
            total_ms: run_started_at.elapsed().as_millis(),
            db_prepare_ms: self.db_prepare_ms,
            model_prepare_ms: self.model_prepare_ms,
            input_prep_ms: self.input_prep_ms,
        }
    }
}

fn compute_file_hash(content: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content);
    let hash = hasher.finalize();
    hash.iter().map(|b| format!("{:02x}", b)).collect()
}

fn serialize_embedding(vec: &[f32]) -> Result<String, OneupError> {
    serde_json::to_string(vec)
        .map_err(|e| IndexingError::Pipeline(format!("serialize embedding: {e}")).into())
}

/// Maximum length in characters of the contextual header prepended to
/// embedding input text.
///
/// The tokenizer truncates input from the right at `EMBEDDING_MAX_TOKENS`, so
/// an unbounded header could crowd a segment's actual content out of the
/// model window. 160 characters is roughly 40-80 wordpiece tokens, a small
/// fraction of the 256-token cap, so the header stays a topical prefix and
/// leaves the bulk of the window for segment content.
const EMBEDDING_HEADER_MAX_CHARS: usize = 160;

/// Maximum composed embedding-input length in characters.
///
/// A coarse pre-tokenization clamp that bounds tokenizer work on pathologically
/// long segments. The tokenizer hard-truncates at `EMBEDDING_MAX_TOKENS` (256)
/// tokens regardless, so this clamp is kept deliberately generous — ~32
/// characters per token, far above any realistic 256-token span (wordpiece
/// tokens are usually a handful of characters and practically never exceed 16) —
/// so it never trims content the 256-token truncation would have kept. It
/// scales with the token cap (`EMBEDDING_MAX_TOKENS * 32`), so widening the
/// window to 256 doubled the budget from 4096 to 8192 in lockstep.
const EMBEDDING_INPUT_MAX_CHARS: usize = EMBEDDING_MAX_TOKENS * 32;

// Compile-time guard: the pre-tokenization char clamp tracks the 256-token
// window (`EMBEDDING_MAX_TOKENS * 32 == 8192`). It breaks the build the instant
// the derivation stops yielding 8192 — either because the char-per-token factor
// drifts or the token cap moves without a conscious review of this budget.
// Shrinking it would trim long segments before tokenization and starve the
// wider window of the very content it was widened to capture.
const _: () = assert!(
    EMBEDDING_INPUT_MAX_CHARS == 8192,
    "embedding char clamp must remain 8192 to feed the full 256-token window"
);

/// Builds the text passed to the embedder for one segment.
///
/// Prepends a bounded `{language} {path stem} {breadcrumb} {defined symbols}`
/// header to the segment content so tiny definition segments (3-line structs,
/// one-line variable assignments) carry topical context into the embedding.
/// Missing breadcrumbs and empty symbol lists are skipped. Only the embedding
/// input changes: stored segment content and segment ids are untouched.
fn compose_embedding_text(relative_path: &str, segment: &ParsedSegment) -> String {
    let mut header_parts: Vec<&str> = Vec::new();

    if !segment.language.is_empty() {
        header_parts.push(segment.language.as_str());
    }

    let path_stem = Path::new(relative_path)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_default();
    if !path_stem.is_empty() {
        header_parts.push(path_stem);
    }

    if let Some(breadcrumb) = segment.breadcrumb.as_deref() {
        if !breadcrumb.is_empty() {
            header_parts.push(breadcrumb);
        }
    }

    let symbols = segment.defined_symbols.join(" ");
    if !symbols.is_empty() {
        header_parts.push(symbols.as_str());
    }

    let header = truncate_chars(header_parts.join(" "), EMBEDDING_HEADER_MAX_CHARS);
    let text = if header.is_empty() {
        segment.content.clone()
    } else {
        format!("{header}\n{}", segment.content)
    };

    truncate_chars(text, EMBEDDING_INPUT_MAX_CHARS)
}

fn truncate_chars(mut text: String, max_chars: usize) -> String {
    if let Some((idx, _)) = text.char_indices().nth(max_chars) {
        text.truncate(idx);
    }
    text
}

/// Content-addressed key for a chunk embedding.
///
/// Deterministic SHA-256 hex over the embedding-model identity (`model_id` plus
/// `embedding_dim` — the same identity gated by
/// [`schema::check_embedding_model_compatible`] and recorded as
/// `meta.embedding_model`), the token window (`max_tokens`), and the exact
/// embedder input produced by [`compose_embedding_text`].
///
/// Because the embedding input uses the file stem (e.g., "utils" from
/// "services/auth/utils.rs"), it is identical for the same file name across
/// different directories and scope cones, so identical content in different cones
/// yields an identical key and shares a single embedding pool row.
/// Folding the model identity into the key makes embeddings produced by a
/// different model resolve to a different key, so changing the model
/// automatically invalidates reuse of older vectors. `max_tokens` is folded in
/// for the same reason: the same text embedded at a 128- vs 256-token window
/// yields numerically different vectors once its tail crosses 128 tokens, so
/// mixing windows in the content-addressed pool would silently serve stale
/// truncated vectors after the v18 window widening.
///
/// The full 256-bit digest is returned (rather than the 128-bit prefix used for
/// segment ids) because a key collision would silently share a wrong embedding
/// across distinct content, which must never happen for the search-identical
/// guarantee. The `model_id`/`embedding_dim`/`max_tokens`/`embed_input` fields
/// are `\0`-delimited so adjacent fields can never bleed into one another.
fn embedding_content_key(
    model_id: &str,
    embedding_dim: usize,
    max_tokens: usize,
    embed_input: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(model_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(embedding_dim.to_string().as_bytes());
    hasher.update(b"\0");
    hasher.update(max_tokens.to_string().as_bytes());
    hasher.update(b"\0");
    hasher.update(embed_input.as_bytes());
    let hash = hasher.finalize();
    hash.iter().map(|b| format!("{:02x}", b)).collect()
}

fn should_embed_segment(seg: &ParsedSegment) -> bool {
    if seg.block_type != "chunk" {
        return true;
    }

    !NON_EMBEDDABLE_CHUNK_LANGUAGES.contains(&seg.language.as_str())
}

#[derive(Debug, Clone)]
struct ScannedWorkItem {
    sequence_id: usize,
    relative_path: String,
    path: PathBuf,
    extension: String,
    stored_hash: Option<String>,
    file_size: u64,
    modified_ns: i64,
}

#[derive(Debug)]
struct ParsedWorkItem {
    relative_path: String,
    file_hash: String,
    extension: String,
    file_size: u64,
    modified_ns: i64,
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

#[allow(dead_code)]
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
                file_size: scanned_file.file_size,
                modified_ns: scanned_file.modified_ns,
            }
        })
        .collect()
}

struct RunInputs {
    scanned_files: Vec<ScannedWorkItem>,
    discovered_count: usize,
    deleted_paths: Vec<String>,
    metadata_unchanged_count: usize,
}

enum ScopePreparation {
    Ready(RunInputs),
    FallbackToFull(String),
}

struct ScopeResolution {
    inputs: RunInputs,
    requested_scope: String,
    executed_scope: String,
    changed_path_count: usize,
    fallback_reason: Option<String>,
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

fn indexed_metadata_matches(
    indexed_file_size: i64,
    indexed_modified_ns: i64,
    scanned_file: &scanner::ScannedFile,
) -> bool {
    i64::try_from(scanned_file.file_size).is_ok_and(|file_size| indexed_file_size == file_size)
        && indexed_modified_ns == scanned_file.modified_ns
}

async fn prepare_full_run_inputs(
    conn: &Connection,
    project_root: &Path,
    context_id: &str,
    config: &IndexingConfig,
) -> Result<RunInputs, OneupError> {
    let scanned = scanner::scan_directory(project_root, config)?;
    let discovered_count = scanned.len();
    tracing::debug!(
        "prepare_full_run_inputs: discovered {} files, scope_roots={:?}, include_globs={:?}",
        discovered_count,
        config.scope_roots,
        config.include_globs
    );
    let manifest = segments::get_all_indexed_files_for_context(conn, context_id).await?;
    tracing::debug!(
        "prepare_full_run_inputs: loaded manifest with {} entries for context_id={}",
        manifest.len(),
        context_id
    );

    let project_root_canonical = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());

    let mut scanned_files = Vec::new();
    let mut scanned_paths = HashSet::new();
    let mut metadata_unchanged_count = 0usize;
    for scanned_file in scanned {
        let relative_path = relative_path_for(&project_root_canonical, &scanned_file.path);
        scanned_paths.insert(relative_path.clone());

        let stored_hash = match manifest.get(&relative_path) {
            Some(entry)
                if indexed_metadata_matches(entry.file_size, entry.modified_ns, &scanned_file) =>
            {
                metadata_unchanged_count += 1;
                continue;
            }
            Some(entry) => Some(entry.file_hash.clone()),
            None => segments::get_file_hash_for_context(conn, context_id, &relative_path).await?,
        };

        scanned_files.push(ScannedWorkItem {
            sequence_id: scanned_files.len(),
            relative_path,
            path: scanned_file.path,
            extension: scanned_file.extension,
            stored_hash,
            file_size: scanned_file.file_size,
            modified_ns: scanned_file.modified_ns,
        });
    }

    let manifest_paths: HashSet<&String> = manifest.keys().collect();
    let indexed_paths: HashSet<String> = segments::get_all_file_paths_for_context(conn, context_id)
        .await?
        .into_iter()
        .collect();
    let all_known_paths: HashSet<&String> = manifest_paths
        .into_iter()
        .chain(indexed_paths.iter())
        .collect();
    let deleted_paths = all_known_paths
        .into_iter()
        .filter(|p| !scanned_paths.contains(*p))
        .cloned()
        .collect();

    Ok(RunInputs {
        scanned_files,
        discovered_count,
        deleted_paths,
        metadata_unchanged_count,
    })
}

async fn prepare_scoped_run_inputs(
    conn: &Connection,
    project_root: &Path,
    context_id: &str,
    changed_paths: &std::collections::BTreeSet<PathBuf>,
    config: &IndexingConfig,
) -> Result<ScopePreparation, OneupError> {
    if changed_paths.is_empty() {
        return Ok(ScopePreparation::Ready(RunInputs {
            scanned_files: Vec::new(),
            discovered_count: 0,
            deleted_paths: Vec::new(),
            metadata_unchanged_count: 0,
        }));
    }

    let scoped_scan = scanner::scan_paths(project_root, changed_paths, config)?;
    let discovered_count = scoped_scan.len();
    let mut scoped_scan_results: HashMap<String, scanner::ScannedFile> = scoped_scan
        .into_iter()
        .map(|file| (relative_path_for(project_root, &file.path), file))
        .collect();

    let mut scanned_files = Vec::new();
    let mut deleted_paths = Vec::new();
    let mut metadata_unchanged_count = 0usize;

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

            if let Some(scanned_file) = scoped_scan_results.remove(&relative_string) {
                if let Some(entry) =
                    segments::get_indexed_file_for_context(conn, context_id, &relative_string)
                        .await?
                {
                    if indexed_metadata_matches(entry.file_size, entry.modified_ns, &scanned_file) {
                        metadata_unchanged_count += 1;
                        continue;
                    }
                }

                let stored_hash =
                    segments::get_file_hash_for_context(conn, context_id, &relative_string).await?;
                scanned_files.push(ScannedWorkItem {
                    sequence_id: scanned_files.len(),
                    relative_path: relative_string,
                    path: scanned_file.path,
                    extension: scanned_file.extension,
                    stored_hash,
                    file_size: scanned_file.file_size,
                    modified_ns: scanned_file.modified_ns,
                });
                continue;
            }

            if scanner::is_scannable_file(&absolute_path) {
                return Ok(ScopePreparation::FallbackToFull(format!(
                    "path {} is excluded by full-scan ignore semantics",
                    relative_path.display()
                )));
            }

            let stored_hash =
                segments::get_file_hash_for_context(conn, context_id, &relative_string).await?;
            if stored_hash.is_some() {
                return Ok(ScopePreparation::FallbackToFull(format!(
                    "path {} no longer matches scanner filters",
                    relative_path.display()
                )));
            }

            continue;
        }

        if segments::get_file_hash_for_context(conn, context_id, &relative_string)
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
        discovered_count,
        deleted_paths,
        metadata_unchanged_count,
    }))
}

fn parse_scanned_file(scanned_file: ScannedWorkItem) -> ParseResultKind {
    // Check the per-file size cap before reading into memory to prevent OOM
    // on large minified/generated files. Skip and warn if over cap; zero segments.
    if scanned_file.file_size > MAX_FILE_SIZE_BYTES {
        warn!(
            "skipping file exceeding {}MB size cap ({}B): {}",
            MAX_FILE_SIZE_BYTES / (1024 * 1024),
            scanned_file.file_size,
            scanned_file.relative_path
        );
        return ParseResultKind::Skipped(ParseSkipReason::Unreadable);
    }

    // Check unsupported extension before hash/read to avoid re-reading
    // large unsupported files (e.g., .sql, .bin) on every scan.
    if !matches!(
        parser::SupportedLanguage::from_extension(&scanned_file.extension),
        Some(parser::SupportedLanguage::Markdown)
    ) && !parser::use_structural_parser(&scanned_file.extension)
        && !parser::is_language_supported(&scanned_file.extension)
    {
        debug!(
            "skipping unsupported extension .{}: {}",
            scanned_file.extension, scanned_file.relative_path
        );
        return ParseResultKind::Skipped(ParseSkipReason::UnsupportedExtension(
            scanned_file.extension,
        ));
    }

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

    let mut segments = if matches!(
        parser::SupportedLanguage::from_extension(&scanned_file.extension),
        Some(parser::SupportedLanguage::Markdown)
    ) {
        let file_stem = Path::new(&scanned_file.relative_path)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or_default();
        markdown::parse_markdown_file(&content, file_stem)
    } else if parser::use_structural_parser(&scanned_file.extension) {
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

    // Enforce the per-file segment cap uniformly across ALL parser
    // outputs — markdown, tree-sitter, and the fallback chunker — not just the
    // chunker (which caps internally). A pathologically dense file (e.g. thousands
    // of tiny top-level definitions) would otherwise blow past the cap and bloat
    // the index. The 2 MiB size cap above bounds worst-case content, but this is
    // the documented global segment guard.
    if segments.len() > MAX_SEGMENTS_PER_FILE {
        warn!(
            "capping segments for {} at {} (parser produced {})",
            scanned_file.relative_path,
            MAX_SEGMENTS_PER_FILE,
            segments.len()
        );
        segments.truncate(MAX_SEGMENTS_PER_FILE);
    }

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
        extension: scanned_file.extension,
        file_size: scanned_file.file_size,
        modified_ns: scanned_file.modified_ns,
        segments,
    })
}

fn build_segment_insert(
    context_id: &str,
    relative_path: &str,
    file_hash: &str,
    segment: &ParsedSegment,
    content_key: Option<String>,
    embedding_vec: Option<String>,
) -> SegmentInsert {
    SegmentInsert {
        id: segments::generate_segment_id(
            context_id,
            relative_path,
            segment.line_start,
            segment.line_end,
        ),
        file_path: relative_path.to_string(),
        language: segment.language.clone(),
        block_type: segment.block_type.clone(),
        content: segment.content.clone(),
        line_start: segment.line_start as i64,
        line_end: segment.line_end as i64,
        content_key,
        embedding_vec,
        breadcrumb: segment.breadcrumb.clone(),
        complexity: segment.complexity as i64,
        role: format!("{:?}", segment.role).to_uppercase(),
        defined_symbols: serde_json::to_string(&segment.defined_symbols)
            .unwrap_or_else(|_| "[]".into()),
        referenced_symbols: serde_json::to_string(&segment.referenced_symbols)
            .unwrap_or_else(|_| "[]".into()),
        referenced_relations: serde_json::to_string(&segment.referenced_relations)
            .unwrap_or_else(|_| "[]".into()),
        called_symbols: serde_json::to_string(&segment.called_symbols)
            .unwrap_or_else(|_| "[]".into()),
        called_relations: serde_json::to_string(&segment.called_relations)
            .unwrap_or_else(|_| "[]".into()),
        file_hash: file_hash.to_string(),
    }
}

/// An embeddable segment paired with the content key that addresses its shared
/// embedding. Collected in deterministic file/segment order so embedder outputs
/// can be mapped back to the exact segments that produced them.
struct EmbeddableSegment {
    content_key: String,
    embed_input: String,
}

/// Selects the embed inputs that genuinely need embedding.
///
/// `embeddable` lists every embeddable segment's content key plus embed input in
/// deterministic order; `present` holds the content keys already stored in
/// `embedding_pool` under the current model. The returned `(content_key,
/// embed_input)` pairs are exactly the misses: pool hits are dropped (their
/// shared vector is reused) and within-batch duplicates are collapsed, so each
/// distinct new `(content, model)` pair is embedded at most once.
/// Input order is preserved so embedding stays deterministic.
fn plan_embedding_work<'a>(
    embeddable: &'a [EmbeddableSegment],
    present: &HashSet<String>,
) -> Vec<(&'a str, &'a str)> {
    let mut planned: HashSet<&str> = HashSet::new();
    let mut misses = Vec::new();
    for segment in embeddable {
        let key = segment.content_key.as_str();
        if present.contains(key) {
            continue;
        }
        if !planned.insert(key) {
            continue;
        }
        misses.push((key, segment.embed_input.as_str()));
    }
    misses
}

/// The connection-touching plan for one batch: every embeddable segment's
/// resolved content key (`embeddable`, deterministic file/segment order) plus
/// the distinct `(content_key, embed_input)` pairs that still need embedding
/// (`misses`, pool hits already excluded). Produced by `plan_segment_batches`,
/// which is the *only* phase of the embed<->store flush that touches `conn`:
/// the caller must keep this lookup from overlapping the store task's
/// write on the same connection (Open Risk mitigation), while the CPU-only
/// `embed_planned_batch` step that follows is free to run concurrently with
/// the previous batch's store.
struct SegmentBatchPlan {
    embeddable: Vec<EmbeddableSegment>,
    misses: Vec<(String, String)>,
}

/// Phase A (connection-touching): derives content keys for every embeddable
/// segment and looks up which are already pooled. Returns `None` when there
/// is no active embedder, mirroring the prior no-embedder storage path.
async fn plan_segment_batches(
    conn: &Connection,
    parsed_files: &[ParsedWorkItem],
    embedder: Option<&Embedder>,
) -> Result<Option<SegmentBatchPlan>, OneupError> {
    let Some(embedder) = embedder else {
        return Ok(None);
    };

    // Fold the loaded model variant (INT8 default vs FP32 fallback)
    // into the content key so swapping the variant resolves to distinct keys and
    // forces a clean re-embed instead of reusing numerically-different vectors.
    let model_id = embedder.model_id();

    // Derive the content key + embed input for every embeddable segment, in
    // deterministic file/segment order.
    let embeddable: Vec<EmbeddableSegment> = parsed_files
        .iter()
        .flat_map(|file| {
            file.segments
                .iter()
                .filter(|segment| should_embed_segment(segment))
                .map(|segment| {
                    let embed_input = compose_embedding_text(&file.relative_path, segment);
                    let content_key = embedding_content_key(
                        &model_id,
                        EMBEDDING_DIM,
                        EMBEDDING_MAX_TOKENS,
                        &embed_input,
                    );
                    EmbeddableSegment {
                        content_key,
                        embed_input,
                    }
                })
        })
        .collect();

    // Look up which keys already live in the pool, then embed only the misses so
    // content already embedded under this model is never re-embedded.
    let distinct_keys: Vec<&str> = {
        let mut seen = HashSet::new();
        embeddable
            .iter()
            .map(|segment| segment.content_key.as_str())
            .filter(|key| seen.insert(*key))
            .collect()
    };
    let present = segments::existing_embedding_pool_keys(conn, &distinct_keys).await?;
    let misses = plan_embedding_work(&embeddable, &present)
        .into_iter()
        .map(|(key, input)| (key.to_string(), input.to_string()))
        .collect();

    Ok(Some(SegmentBatchPlan { embeddable, misses }))
}

/// Phase B (pure CPU): embeds the plan's misses. Never touches `conn`, so it
/// is safe to run while a previous batch's store write is in flight on the
/// store task (the store double buffer).
fn embed_planned_batch(
    embedder: &mut Embedder,
    plan: &SegmentBatchPlan,
    timings: &mut TimingAccumulator,
) -> Result<HashMap<String, String>, OneupError> {
    let miss_inputs: Vec<&str> = plan
        .misses
        .iter()
        .map(|(_, input)| input.as_str())
        .collect();
    let embed_started_at = Instant::now();
    let vectors = embedder.embed_batch(&miss_inputs)?;
    timings.embed_ms += embed_started_at.elapsed().as_millis();

    if vectors.len() != plan.misses.len() {
        return Err(IndexingError::Pipeline(format!(
            "embedder returned {} vectors for {} miss-set inputs",
            vectors.len(),
            plan.misses.len()
        ))
        .into());
    }

    // Map each freshly embedded key to its serialized vector. An embeddable
    // segment whose key is absent here is a pool hit: the shared vector already
    // exists, so the segment carries only its `content_key` reference.
    let mut key_to_vec: HashMap<String, String> = HashMap::with_capacity(plan.misses.len());
    for ((key, _input), vector) in plan.misses.iter().zip(vectors) {
        key_to_vec.insert(key.clone(), serialize_embedding(&vector)?);
    }
    Ok(key_to_vec)
}

/// Phase C (pure CPU): assembles segment inserts from the plan's resolved
/// content keys plus the freshly embedded vectors. Never touches `conn`.
fn assemble_segment_batches(
    context_id: &str,
    parsed_files: &[ParsedWorkItem],
    plan: &SegmentBatchPlan,
    key_to_vec: &HashMap<String, String>,
) -> Result<Vec<Vec<SegmentInsert>>, OneupError> {
    let mut resolved_keys = plan.embeddable.iter();
    let mut batches = Vec::with_capacity(parsed_files.len());
    for file in parsed_files {
        let mut inserts = Vec::with_capacity(file.segments.len());
        for segment in &file.segments {
            let (content_key, embedding_vec) = if should_embed_segment(segment) {
                let resolved = resolved_keys.next().ok_or_else(|| {
                    IndexingError::Pipeline(format!(
                        "missing content key for {}:{}-{}",
                        file.relative_path, segment.line_start, segment.line_end
                    ))
                })?;
                let embedding_vec = key_to_vec.get(resolved.content_key.as_str()).cloned();
                (Some(resolved.content_key.clone()), embedding_vec)
            } else {
                (None, None)
            };

            inserts.push(build_segment_insert(
                context_id,
                &file.relative_path,
                &file.file_hash,
                segment,
                content_key,
                embedding_vec,
            ));
        }
        batches.push(inserts);
    }

    debug_assert!(
        resolved_keys.next().is_none(),
        "unexpected trailing embeddable segments after pipeline run"
    );

    Ok(batches)
}

/// No active embedder: segments are stored without content keys or vectors,
/// exactly as before content-addressed pooling.
fn assemble_without_embedder(
    context_id: &str,
    parsed_files: &[ParsedWorkItem],
) -> Vec<Vec<SegmentInsert>> {
    parsed_files
        .iter()
        .map(|file| {
            file.segments
                .iter()
                .map(|segment| {
                    build_segment_insert(
                        context_id,
                        &file.relative_path,
                        &file.file_hash,
                        segment,
                        None,
                        None,
                    )
                })
                .collect()
        })
        .collect()
}

fn build_manifest_meta(file: &ParsedWorkItem) -> IndexedFileMeta {
    IndexedFileMeta {
        extension: file.extension.clone(),
        file_hash: file.file_hash.clone(),
        file_size: file.file_size as i64,
        modified_ns: file.modified_ns,
    }
}

async fn replace_file_batches(
    conn: &Connection,
    context_id: &str,
    parsed_files: &[ParsedWorkItem],
    segment_batches: &[Vec<SegmentInsert>],
) -> Result<(), OneupError> {
    let manifest_metas: Vec<IndexedFileMeta> =
        parsed_files.iter().map(build_manifest_meta).collect();

    if parsed_files.len() == 1 {
        return segments::replace_file_segments_for_context_tx_with_meta(
            conn,
            context_id,
            &parsed_files[0].relative_path,
            &segment_batches[0],
            Some(&manifest_metas[0]),
        )
        .await;
    }

    let file_batches: Vec<FileSegmentBatch<'_>> = parsed_files
        .iter()
        .zip(segment_batches.iter())
        .zip(manifest_metas.iter())
        .map(|((file, segments), meta)| FileSegmentBatch {
            file_path: file.relative_path.as_str(),
            segments,
            manifest_meta: Some(meta),
        })
        .collect();

    segments::replace_file_batch_for_context_tx(conn, context_id, &file_batches).await
}

/// One fully-assembled batch (parsed files + resolved segment inserts) ready
/// to be written to `index.db`.
struct StoreJob {
    parsed_files: Vec<ParsedWorkItem>,
    segment_batches: Vec<Vec<SegmentInsert>>,
}

/// Stats produced by a completed store write, folded into the run's
/// `PipelineStats`/`TimingAccumulator` once the producer awaits them.
struct StoreOutcome {
    store_ms: u128,
    files_indexed: usize,
    segments_stored: usize,
}

/// Overlaps the pure-CPU `Embedder::embed_batch` call for batch N+1 with the
/// libSQL write of batch N: a dedicated task owns the write
/// connection and drains `StoreJob`s from a depth-1 channel while the
/// producer keeps preparing the next batch. `await_inflight` is the single
/// synchronization point the producer must pass through before it touches
/// the connection again for the next batch's pool-key lookup
/// (`plan_segment_batches`), so the lookup and the store write are never in
/// flight on the same connection at once (Open Risk mitigation); the
/// single-writer invariant to `index.db` is preserved because only this task
/// ever calls `replace_file_batches`.
struct StoreDoubleBuffer {
    job_tx: mpsc::Sender<StoreJob>,
    outcome_rx: mpsc::Receiver<Result<StoreOutcome, OneupError>>,
    handle: JoinHandle<()>,
    inflight: bool,
    pending: Option<StoreJob>,
}

impl StoreDoubleBuffer {
    fn spawn(conn: Connection, context_id: String) -> Self {
        let (job_tx, mut job_rx) = mpsc::channel::<StoreJob>(1);
        let (outcome_tx, outcome_rx) = mpsc::channel::<Result<StoreOutcome, OneupError>>(1);
        let handle = tokio::spawn(async move {
            while let Some(job) = job_rx.recv().await {
                let started_at = Instant::now();
                let files_indexed = job.parsed_files.len();
                let segments_stored = job.segment_batches.iter().map(Vec::len).sum::<usize>();
                let result = replace_file_batches(
                    &conn,
                    &context_id,
                    &job.parsed_files,
                    &job.segment_batches,
                )
                .await;
                let outcome = result.map(|()| StoreOutcome {
                    store_ms: started_at.elapsed().as_millis(),
                    files_indexed,
                    segments_stored,
                });
                if outcome_tx.send(outcome).await.is_err() {
                    break;
                }
            }
        });
        Self {
            job_tx,
            outcome_rx,
            handle,
            inflight: false,
            pending: None,
        }
    }

    /// Blocks until the in-flight store (if any) has fully finished, folding
    /// its stats into `stats`/`timings`. Callers must run this before
    /// touching the connection again on the producer side.
    async fn await_inflight(
        &mut self,
        stats: &mut PipelineStats,
        timings: &mut TimingAccumulator,
    ) -> Result<(), OneupError> {
        if !self.inflight {
            return Ok(());
        }
        self.inflight = false;
        let outcome = self.outcome_rx.recv().await.ok_or_else(|| {
            IndexingError::Pipeline("store task ended unexpectedly".to_string())
        })??;
        timings.store_ms += outcome.store_ms;
        stats.files_indexed += outcome.files_indexed;
        stats.segments_stored += outcome.segments_stored;
        Ok(())
    }

    /// Hands the pending batch (already embedded and assembled by a previous
    /// call) to the store task, if there is one. Non-blocking beyond channel
    /// backpressure: the caller's subsequent CPU-only work overlaps with this
    /// write.
    async fn dispatch_pending(&mut self) -> Result<(), OneupError> {
        let Some(job) = self.pending.take() else {
            return Ok(());
        };
        self.job_tx
            .send(job)
            .await
            .map_err(|_| IndexingError::Pipeline("store task unavailable".to_string()))?;
        self.inflight = true;
        Ok(())
    }

    fn set_pending(&mut self, job: StoreJob) {
        self.pending = Some(job);
    }

    /// Flushes the last pending batch and waits for the store task to drain,
    /// folding final stats into `stats`/`timings`. Must be called once, after
    /// the last `store_ready_files` call, before embedding coverage is
    /// verified against the database.
    ///
    /// Order matters: a call from the last `store_ready_files` invocation may
    /// still have a dispatch in flight (its own `await_inflight` only waits
    /// on the dispatch *before* it), so any outstanding write must be
    /// awaited before the final pending batch is dispatched — otherwise two
    /// jobs would be in flight against a depth-1 channel at once and one
    /// batch's outcome would never be folded in.
    async fn finish(
        mut self,
        stats: &mut PipelineStats,
        timings: &mut TimingAccumulator,
    ) -> Result<(), OneupError> {
        self.await_inflight(stats, timings).await?;
        self.dispatch_pending().await?;
        self.await_inflight(stats, timings).await?;
        drop(self.job_tx);
        let _ = self.handle.await;
        Ok(())
    }
}

async fn store_ready_files(
    conn: &Connection,
    double_buffer: &mut StoreDoubleBuffer,
    context_id: &str,
    ready_files: &mut Vec<ParsedWorkItem>,
    embedder: Option<&mut Embedder>,
    stats: &mut PipelineStats,
    timings: &mut TimingAccumulator,
) -> Result<(), OneupError> {
    if ready_files.is_empty() {
        return Ok(());
    }

    let parsed_files = std::mem::take(ready_files);

    // The pool-key lookup below touches `conn`; make sure the store task's
    // in-flight write (if any) has fully finished first so the lookup and the
    // write never race on the shared connection (Open Risk mitigation).
    double_buffer.await_inflight(stats, timings).await?;

    let plan = plan_segment_batches(conn, &parsed_files, embedder.as_deref()).await?;

    // Hand the previous batch (already embedded and assembled) to the store
    // task now, so its write overlaps with this batch's pure-CPU embed step
    // below.
    double_buffer.dispatch_pending().await?;

    let segment_batches = match (embedder, plan) {
        (Some(embedder), Some(plan)) => {
            let key_to_vec = embed_planned_batch(embedder, &plan, timings)?;
            assemble_segment_batches(context_id, &parsed_files, &plan, &key_to_vec)?
        }
        _ => assemble_without_embedder(context_id, &parsed_files),
    };

    double_buffer.set_pending(StoreJob {
        parsed_files,
        segment_batches,
    });
    Ok(())
}

/// Reconciles the reported embedding state with what the database actually
/// holds for this context. A run that had a working embedder but left a
/// context with embeddable segments and zero stored vector rows must not
/// report embeddings as enabled; it reports `embeddings_generated: false`
/// with an explicit reason instead. Count failures are advisory: the indexed
/// data is already committed, so they log a warning and leave coverage unset.
async fn verify_stored_embedding_outcome(
    conn: &Connection,
    context_id: &str,
    stats: &mut PipelineStats,
) {
    let vector_rows = match segments::count_vector_rows_for_context(conn, context_id).await {
        Ok(count) => count,
        Err(err) => {
            warn!("failed to count stored vector rows for context {context_id}: {err}");
            return;
        }
    };
    let embeddable_segments =
        match segments::count_embeddable_segments_for_context(conn, context_id).await {
            Ok(count) => count,
            Err(err) => {
                warn!("failed to count embeddable segments for context {context_id}: {err}");
                return;
            }
        };

    stats.vector_rows = Some(vector_rows);
    stats.embeddable_segments = Some(embeddable_segments);

    if stats.embeddings_generated && embeddable_segments > 0 && vector_rows == 0 {
        warn!(
            "embedder was available but context {context_id} stored 0 vector rows for {embeddable_segments} embeddable segments; reporting embeddings as unavailable"
        );
        stats.embeddings_generated = false;
        stats.embedding_unavailable_reason = Some(format!(
            "embedder was available but no vector rows were stored ({embeddable_segments} embeddable segments); run `1up reindex` to rebuild"
        ));
    }
}

/// Fail-closed safety check for segment deletion on scope metadata loss.
///
/// When scope is configured and then lost (e.g., meta table corruption), the
/// delete reconciliation must never silently delete all segments and re-index
/// the whole repo. This function performs the fail-closed clamp:
///
/// 1. If scope metadata is present: validate every deleted segment is under a
///    scope root (safety fence, currently not enforced but records the intent).
/// 2. If scope metadata is missing but segment coverage exists: clamp deletion
///    to recorded paths and emit a warning. Never delete coverage unaccounted
///    for in the recorded index.
/// 3. If no scope and no coverage: proceed normally (empty index).
///
/// This is the critical safety gate preventing silent whole-repo
/// re-indexing on scope state loss. The clamp to recorded coverage ensures
/// a failed rebuild always leaves the prior index intact for manual inspection.
async fn clamp_deletion_on_scope_loss(
    conn: &Connection,
    context_id: &str,
    requested_deletes: &[String],
) -> Result<(Vec<String>, Option<String>), OneupError> {
    // Read scope metadata (source of truth for scope coverage).
    let scope = match schema::read_scope_from_meta(conn).await {
        Ok(scope) => scope,
        Err(err) => {
            warn!(
                "failed to read scope metadata for context {}: {}; proceeding with requested deletes (unscoped fallback)",
                context_id, err
            );
            return Ok((requested_deletes.to_vec(), None));
        }
    };

    // Get all recorded file paths (coverage that exists in the index).
    let recorded_paths: HashSet<String> = match segments::get_all_file_paths_for_context(
        conn, context_id,
    )
    .await
    {
        Ok(paths) => paths.into_iter().collect(),
        Err(err) => {
            warn!(
                "failed to read recorded file paths for context {}: {}; proceeding with requested deletes",
                context_id, err
            );
            return Ok((requested_deletes.to_vec(), None));
        }
    };

    match scope {
        Some(_scope_roots) => {
            // Scope is present: validate that deletions stay within scope.
            // (Note: For v1, we allow deletion of any recorded file. Stricter
            // enforcement of "must be under scope root" can be added in v2 if
            // needed. For now, the mere presence of scope is enough to know
            // coverage was intentional; the clamp below handles the loss case.)
            Ok((requested_deletes.to_vec(), None))
        }
        None => {
            // No scope metadata. Check if we have recorded coverage anyway.
            // This detects the case where metadata was lost but segments remain.
            if recorded_paths.is_empty() {
                // No scope, no coverage -> clean state, proceed normally.
                Ok((requested_deletes.to_vec(), None))
            } else {
                // Scope metadata lost, but coverage exists -> clamp to recorded paths.
                // This ensures we never silently delete all segments and re-index.
                let clamped: Vec<String> = requested_deletes
                    .iter()
                    .filter(|path| recorded_paths.contains(*path))
                    .cloned()
                    .collect();

                let warning = format!(
                    "Scope metadata lost; clamped deletion to {} recorded indexed paths. \
                    Rebuild to refresh scope.",
                    recorded_paths.len()
                );
                warn!("{}", &warning);

                Ok((clamped, Some(warning)))
            }
        }
    }
}

async fn delete_removed_files(
    conn: &Connection,
    context_id: &str,
    deleted_paths: &[String],
    batch_size: usize,
    timings: &mut TimingAccumulator,
) -> Result<(), OneupError> {
    let store_started_at = Instant::now();

    for chunk in deleted_paths.chunks(batch_size.max(1)) {
        if chunk.len() == 1 {
            segments::replace_file_segments_for_context_tx(conn, context_id, &chunk[0], &[])
                .await?;
            continue;
        }

        let file_batches: Vec<FileSegmentBatch<'_>> = chunk
            .iter()
            .map(|path| FileSegmentBatch {
                file_path: path.as_str(),
                segments: &[],
                manifest_meta: None,
            })
            .collect();
        segments::replace_file_batch_for_context_tx(conn, context_id, &file_batches).await?;
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

const PROGRESS_PERSIST_THROTTLE: Duration = Duration::from_millis(PROGRESS_PERSIST_THROTTLE_MS);

/// Pure decision gate for `FlushState::refresh`: should this call actually
/// write `index_status.json`, or is it within the throttle window of the
/// last write? `force` (the terminal `Complete` phase) always wins.
fn should_persist_progress(last_persisted_at: Option<Instant>, now: Instant, force: bool) -> bool {
    force
        || last_persisted_at
            .is_none_or(|last| now.duration_since(last) >= PROGRESS_PERSIST_THROTTLE)
}

struct FlushState<'a> {
    stats: &'a mut PipelineStats,
    project_root: &'a Path,
    context: &'a IndexRunContext,
    files_total: usize,
    content_read_count: usize,
    parallelism: Option<IndexParallelism>,
    timings: &'a mut TimingAccumulator,
    run_started_at: Instant,
    unsupported_extensions: &'a mut HashSet<String>,
    progress_tx: Option<ProgressSender>,
    scope: Option<IndexScopeInfo>,
    prefilter: Option<IndexPrefilterInfo>,
    last_persisted_at: Option<Instant>,
}

impl FlushState<'_> {
    fn refresh(&mut self, phase: IndexPhase, persist: bool) {
        self.refresh_at(phase, persist, Instant::now());
    }

    /// `now`-injectable core of `refresh`, so the throttle gate is
    /// deterministically testable without depending on wall-clock timing.
    fn refresh_at(&mut self, phase: IndexPhase, persist: bool, now: Instant) {
        let persist = persist
            && should_persist_progress(self.last_persisted_at, now, phase == IndexPhase::Complete);
        if persist {
            self.last_persisted_at = Some(now);
        }
        refresh_progress(
            self.stats,
            self.project_root,
            self.context,
            self.progress_tx.as_ref(),
            ProgressUpdate {
                state: IndexState::Running,
                phase,
                files_total: self.files_total,
                parallelism: self.parallelism.clone(),
                timings: Some(self.timings.snapshot(self.run_started_at)),
                scope: self.scope.clone(),
                prefilter: self.prefilter.clone(),
                persist,
            },
        );
    }
}

async fn flush_reorder_buffer(
    conn: &Connection,
    double_buffer: &mut StoreDoubleBuffer,
    reorder_buffer: &mut BTreeMap<usize, ParseResultKind>,
    next_sequence: &mut usize,
    config: &IndexingConfig,
    embedder: &mut Option<&mut Embedder>,
    state: &mut FlushState<'_>,
) -> Result<(), OneupError> {
    let mut ready_files = Vec::new();
    let write_batch_files = config.effective_write_batch_files(state.content_read_count);

    while let Some(result) = reorder_buffer.remove(next_sequence) {
        match result {
            ParseResultKind::Ready(file) => {
                ready_files.push(file);
                *next_sequence += 1;

                if ready_files.len() >= write_batch_files {
                    {
                        let embedder = embedder.as_mut().map(|embedder| &mut **embedder);
                        store_ready_files(
                            conn,
                            double_buffer,
                            &state.context.context_id,
                            &mut ready_files,
                            embedder,
                            state.stats,
                            state.timings,
                        )
                        .await?;
                    }
                    state.refresh(IndexPhase::Storing, true);
                }
            }
            ParseResultKind::Skipped(reason) => {
                if !ready_files.is_empty() {
                    {
                        let embedder = embedder.as_mut().map(|embedder| &mut **embedder);
                        store_ready_files(
                            conn,
                            double_buffer,
                            &state.context.context_id,
                            &mut ready_files,
                            embedder,
                            state.stats,
                            state.timings,
                        )
                        .await?;
                    }
                    state.refresh(IndexPhase::Storing, true);
                }

                if let ParseSkipReason::UnsupportedExtension(extension) = reason {
                    state.unsupported_extensions.insert(extension);
                }

                state.stats.files_skipped += 1;
                *next_sequence += 1;
                state.refresh(current_progress_phase(state.stats), true);
            }
        }
    }

    if !ready_files.is_empty() {
        {
            let embedder = embedder.as_mut().map(|embedder| &mut **embedder);
            store_ready_files(
                conn,
                double_buffer,
                &state.context.context_id,
                &mut ready_files,
                embedder,
                state.stats,
                state.timings,
            )
            .await?;
        }
        state.refresh(IndexPhase::Storing, true);
    }

    Ok(())
}

/// Statistics returned after a pipeline run.
#[derive(Debug, Clone)]
pub struct PipelineStats {
    pub files_scanned: usize,
    pub files_processed: usize,
    pub files_indexed: usize,
    pub files_skipped: usize,
    pub files_deleted: usize,
    pub segments_stored: usize,
    /// Reflects the stored outcome, not embedder presence: true only when the
    /// run had an embedder and the context's stored vector coverage is
    /// consistent with it (see `verify_stored_embedding_outcome`).
    pub embeddings_generated: bool,
    pub embedding_unavailable_reason: Option<String>,
    pub vector_rows: Option<u64>,
    pub embeddable_segments: Option<u64>,
    pub progress: IndexProgress,
}

impl Default for PipelineStats {
    fn default() -> Self {
        Self {
            files_scanned: 0,
            files_processed: 0,
            files_indexed: 0,
            files_skipped: 0,
            files_deleted: 0,
            segments_stored: 0,
            embeddings_generated: false,
            embedding_unavailable_reason: None,
            vector_rows: None,
            embeddable_segments: None,
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
    run_with_config_with_progress_ui(conn, project_root, embedder, config, true).await
}

pub async fn run_with_config_with_progress_ui(
    conn: &Connection,
    project_root: &Path,
    embedder: Option<&mut Embedder>,
    config: &IndexingConfig,
    show_progress_ui: bool,
) -> Result<PipelineStats, OneupError> {
    run_with_config_and_progress_ui(conn, project_root, embedder, config, None, show_progress_ui)
        .await
}

#[allow(dead_code)]
pub async fn run_with_config_and_progress(
    conn: &Connection,
    project_root: &Path,
    embedder: Option<&mut Embedder>,
    config: &IndexingConfig,
    progress_tx: Option<ProgressSender>,
) -> Result<PipelineStats, OneupError> {
    run_with_config_and_progress_ui(conn, project_root, embedder, config, progress_tx, true).await
}

pub async fn run_with_config_and_progress_ui(
    conn: &Connection,
    project_root: &Path,
    embedder: Option<&mut Embedder>,
    config: &IndexingConfig,
    progress_tx: Option<ProgressSender>,
    show_progress_ui: bool,
) -> Result<PipelineStats, OneupError> {
    run_with_scope_and_progress_ui(
        conn,
        project_root,
        embedder,
        &RunScope::Full,
        config,
        progress_tx,
        show_progress_ui,
    )
    .await
}

#[allow(dead_code)]
pub async fn run_with_scope(
    conn: &Connection,
    project_root: &Path,
    embedder: Option<&mut Embedder>,
    scope: &RunScope,
    config: &IndexingConfig,
) -> Result<PipelineStats, OneupError> {
    run_with_scope_and_progress_ui(conn, project_root, embedder, scope, config, None, true).await
}

#[allow(dead_code)]
pub async fn run_with_scope_and_progress(
    conn: &Connection,
    project_root: &Path,
    embedder: Option<&mut Embedder>,
    scope: &RunScope,
    config: &IndexingConfig,
    progress_tx: Option<ProgressSender>,
) -> Result<PipelineStats, OneupError> {
    run_with_scope_and_progress_ui(
        conn,
        project_root,
        embedder,
        scope,
        config,
        progress_tx,
        true,
    )
    .await
}

pub async fn run_with_scope_and_progress_ui(
    conn: &Connection,
    project_root: &Path,
    embedder: Option<&mut Embedder>,
    scope: &RunScope,
    config: &IndexingConfig,
    progress_tx: Option<ProgressSender>,
    show_progress_ui: bool,
) -> Result<PipelineStats, OneupError> {
    run_with_scope_and_setup(
        conn,
        project_root,
        embedder,
        scope,
        config,
        progress_tx,
        show_progress_ui,
        None,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn run_with_scope_and_setup(
    conn: &Connection,
    project_root: &Path,
    embedder: Option<&mut Embedder>,
    scope: &RunScope,
    config: &IndexingConfig,
    progress_tx: Option<ProgressSender>,
    show_progress_ui: bool,
    setup_timings: Option<SetupTimings>,
    daemon_fallback_reason: Option<String>,
) -> Result<PipelineStats, OneupError> {
    run_with_scope_setup_and_progress_root(
        conn,
        project_root,
        embedder,
        scope,
        config,
        progress_tx,
        show_progress_ui,
        setup_timings,
        daemon_fallback_reason,
        None,
    )
    .await
}

/// Like [`run_with_scope_and_setup`], but accepts a separate
/// `progress_root` where `.1up/` state (progress files) should be
/// written. When `None`, `project_root` is used for both scanning
/// and progress (the default for daemon callers where they are the
/// same). CLI callers running from a git worktree pass the main
/// repo root here so that progress is written beside the index.
#[allow(clippy::too_many_arguments)]
pub async fn run_with_scope_setup_and_progress_root(
    conn: &Connection,
    project_root: &Path,
    embedder: Option<&mut Embedder>,
    scope: &RunScope,
    config: &IndexingConfig,
    progress_tx: Option<ProgressSender>,
    show_progress_ui: bool,
    setup_timings: Option<SetupTimings>,
    daemon_fallback_reason: Option<String>,
    progress_root: Option<&Path>,
) -> Result<PipelineStats, OneupError> {
    let context = IndexRunContext::legacy(project_root);
    // Synchronous one-shot callers (CLI/MCP rebuilds, tests) are not subject to
    // the daemon's SIGTERM drain, so they run under a fresh token that is never
    // cancelled. Only the daemon threads a live token (see `worker.rs`).
    let cancel_token = CancellationToken::new();
    run_with_index_context_scope_setup_and_progress_root(
        conn,
        project_root,
        embedder,
        scope,
        config,
        progress_tx,
        show_progress_ui,
        setup_timings,
        daemon_fallback_reason,
        progress_root,
        context,
        &cancel_token,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn run_with_context_scope_setup_and_progress_root(
    conn: &Connection,
    context: &WorktreeContext,
    embedder: Option<&mut Embedder>,
    scope: &RunScope,
    config: &IndexingConfig,
    progress_tx: Option<ProgressSender>,
    show_progress_ui: bool,
    setup_timings: Option<SetupTimings>,
    daemon_fallback_reason: Option<String>,
    progress_root: Option<&Path>,
    cancel_token: &CancellationToken,
) -> Result<PipelineStats, OneupError> {
    let stats = run_with_index_context_scope_setup_and_progress_root(
        conn,
        &context.source_root,
        embedder,
        scope,
        config,
        progress_tx,
        show_progress_ui,
        setup_timings,
        daemon_fallback_reason,
        progress_root,
        IndexRunContext::from_worktree(context),
        cancel_token,
    )
    .await?;

    record_indexed_head(conn, context).await;

    Ok(stats)
}

/// Persist the worktree context row, including the repository head commit OID
/// this run indexed at, so readiness checks can compare it against the live
/// repository HEAD. Recording failure is logged instead of failing the run:
/// the index data is already committed and the recorded head is advisory
/// freshness metadata.
///
/// Also runs the opt-in (default OFF) migration-time `SupersededSameSource`
/// prune when [`config::migration_gc_prune_enabled`] reports the switch is on
/// (automatic pruning on every index run is not a
/// default-on behavior until the planning gate finalizes enablement).
async fn record_indexed_head(conn: &Connection, context: &WorktreeContext) {
    let project_id =
        crate::shared::project::read_project_id(&context.state_root).unwrap_or_default();
    if let Err(err) = segments::upsert_worktree_context(conn, context, &project_id).await {
        warn!(
            "failed to record indexed head for context {}: {err}",
            context.context_id
        );
    }

    if config::migration_gc_prune_enabled() {
        prune_superseded_same_source_contexts_on_migration(conn, context).await;
    }
}

/// True when `updated_at` (`worktree_contexts.updated_at`, a `datetime('now')`
/// TEXT value `YYYY-MM-DD HH:MM:SS`, UTC) is at least `min_age` old relative to
/// `now`. Unparseable input degrades to `false`: a migration-time prune must
/// never fire on ambiguous data. Duplicated in miniature from `cli::gc`'s
/// helper of the same name — see the layering note on
/// [`superseded_same_source_context_ids`] for why this cannot be shared
/// directly.
fn context_age_at_least(
    updated_at: &str,
    now: chrono::DateTime<chrono::Utc>,
    min_age: chrono::Duration,
) -> bool {
    match chrono::NaiveDateTime::parse_from_str(updated_at, "%Y-%m-%d %H:%M:%S") {
        Ok(parsed) => now - parsed.and_utc() >= min_age,
        Err(_) => false,
    }
}

/// Determine which recorded contexts qualify for the opt-in migration-time
/// `SupersededSameSource` prune: same `source_root` as `active` but ranked
/// beyond [`GC_SUPERSEDED_SAME_SOURCE_KEEP_COUNT`] most-recently-updated
/// same-source peers (active is always present in `contexts` and, having just
/// been recorded, is typically the most recent) and older than
/// [`GC_SUPERSEDED_SAME_SOURCE_MAX_AGE_DAYS`].
///
/// Deliberately narrower than `cli::gc`'s four-reason `prune_reason`
/// classifier — this hook only ever evaluates `SupersededSameSource`, never
/// `SourceMissing`/`StaleBranchSnapshot`/`NestedSubdirContext` (those stay a
/// manual `1up gc` decision) — and deliberately does not call into
/// `cli::gc::prune_reason` to get it: this crate's dependency direction is
/// `cli`/`mcp` -> `search`/`indexer` -> `storage` -> `shared` (no cycles), and
/// `daemon` already depends on `indexer::pipeline`, so `indexer` reaching back
/// into `cli` or `daemon` here would invert that direction into a cycle. The
/// small amount of duplicated policy logic (this function plus
/// [`context_age_at_least`]) is the deliberate tradeoff (patterns.md: "prefer
/// duplication over wrong abstraction"), mirroring the daemon's own
/// self-contained `source_missing_context_ids` rather than reusing `cli::gc`.
fn superseded_same_source_context_ids(
    contexts: &[segments::IndexedContextRow],
    active: &WorktreeContext,
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<String> {
    let mut same_source: Vec<&segments::IndexedContextRow> = contexts
        .iter()
        .filter(|c| c.source_root == active.source_root)
        .collect();
    same_source.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then(a.context_id.cmp(&b.context_id))
    });

    let max_age = chrono::Duration::days(GC_SUPERSEDED_SAME_SOURCE_MAX_AGE_DAYS);
    same_source
        .into_iter()
        .enumerate()
        .filter_map(|(index, ctx)| {
            let rank = index + 1;
            // `state_root == active.state_root` (same worktree, a different
            // branch's snapshot) is `StaleBranchSnapshot` territory in
            // `cli::gc`'s classifier, not `SupersededSameSource` — that stays a
            // manual `1up gc` decision, never auto-pruned by this opt-in hook.
            let qualifies = ctx.context_id != active.context_id
                && ctx.state_root != active.state_root
                && rank > GC_SUPERSEDED_SAME_SOURCE_KEEP_COUNT
                && context_age_at_least(&ctx.updated_at, now, max_age);
            qualifies.then(|| ctx.context_id.clone())
        })
        .collect()
}

/// Opt-in migration-time counterpart to `1up gc --apply`'s `SupersededSameSource`
/// enforcement, run once per successful index. Mirrors the daemon startup
/// source-missing prune (`daemon::worker::prune_source_missing_contexts_on_startup`):
/// deletes rows only, no inline `VACUUM` (full compaction stays exclusive to
/// explicit `1up gc --apply` under the rebuild lock, so this never competes with
/// concurrent searches), and every step is best-effort so a prune
/// failure can never fail the index run that just succeeded.
///
/// Registry/daemon-status bookkeeping (which `1up gc --apply` and the daemon
/// startup prune both perform) is intentionally not attempted here for the
/// same layering reason documented on [`superseded_same_source_context_ids`]:
/// `crate::cli::project_status_files` and `crate::daemon::registry` both sit
/// above `indexer` in the dependency direction. A pruned context can briefly
/// linger in the daemon status file or project registry until the next
/// `1up gc` or daemon restart reconciles it; the index rows themselves — the
/// actual reclaimed space — are gone immediately.
async fn prune_superseded_same_source_contexts_on_migration(
    conn: &Connection,
    context: &WorktreeContext,
) {
    let contexts = match segments::list_worktree_contexts(conn).await {
        Ok(contexts) => contexts,
        Err(err) => {
            warn!(
                "failed to list worktree contexts for migration-time prune of {}: {err}",
                context.context_id
            );
            return;
        }
    };

    let pruned = superseded_same_source_context_ids(&contexts, context, chrono::Utc::now());
    if pruned.is_empty() {
        return;
    }

    let mut removed = Vec::with_capacity(pruned.len());
    for context_id in &pruned {
        match segments::delete_context(conn, context_id).await {
            Ok(_) => removed.push(context_id.as_str()),
            Err(err) => warn!(
                "failed to prune superseded-same-source context {context_id} at migration time: {err}"
            ),
        }
    }
    if !removed.is_empty() {
        info!(
            "migration-time prune removed {} superseded-same-source context(s) for {}: {}",
            removed.len(),
            context.source_root.display(),
            removed.join(", ")
        );
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_with_index_context_scope_setup_and_progress_root(
    conn: &Connection,
    project_root: &Path,
    embedder: Option<&mut Embedder>,
    scope: &RunScope,
    config: &IndexingConfig,
    progress_tx: Option<ProgressSender>,
    show_progress_ui: bool,
    setup_timings: Option<SetupTimings>,
    daemon_fallback_reason: Option<String>,
    progress_root: Option<&Path>,
    context: IndexRunContext,
    cancel_token: &CancellationToken,
) -> Result<PipelineStats, OneupError> {
    let progress_root = progress_root.unwrap_or(project_root);
    let input_prep_start = Instant::now();
    let resolution = match scope {
        RunScope::Full => {
            let run_inputs =
                prepare_full_run_inputs(conn, project_root, &context.context_id, config).await?;
            let changed_count = run_inputs.scanned_files.len();

            // Detect scope applied via include_globs (MCP path with scope_roots)
            let (requested_scope_str, executed_scope_str) = if !config.scope_roots.is_empty() {
                // Scoped via include_globs: requested = scope root count, executed = actual scanned
                (
                    format!("scoped:{}", config.scope_roots.len()),
                    format!("scoped:{}", changed_count),
                )
            } else {
                // Unscoped full scan
                ("full".to_string(), "full".to_string())
            };

            ScopeResolution {
                inputs: run_inputs,
                requested_scope: requested_scope_str,
                executed_scope: executed_scope_str,
                changed_path_count: changed_count,
                fallback_reason: daemon_fallback_reason,
            }
        }
        RunScope::Paths(changed_paths) => {
            let requested_count = changed_paths.len();
            match prepare_scoped_run_inputs(
                conn,
                project_root,
                &context.context_id,
                changed_paths,
                config,
            )
            .await?
            {
                ScopePreparation::Ready(run_inputs) => {
                    let changed_count = run_inputs.scanned_files.len();
                    ScopeResolution {
                        inputs: run_inputs,
                        requested_scope: format!("scoped:{requested_count}"),
                        executed_scope: format!("scoped:{changed_count}"),
                        changed_path_count: changed_count,
                        fallback_reason: None,
                    }
                }
                ScopePreparation::FallbackToFull(reason) => {
                    info!(
                        "scoped run for {} fell back to a full scan: {}",
                        project_root.display(),
                        reason
                    );
                    let run_inputs =
                        prepare_full_run_inputs(conn, project_root, &context.context_id, config)
                            .await?;
                    let changed_count = run_inputs.scanned_files.len();
                    ScopeResolution {
                        inputs: run_inputs,
                        requested_scope: format!("scoped:{requested_count}"),
                        executed_scope: "full".to_string(),
                        changed_path_count: changed_count,
                        fallback_reason: Some(reason),
                    }
                }
            }
        }
    };
    let input_prep_ms = input_prep_start.elapsed().as_millis();

    execute_run_with_inputs(
        conn,
        progress_root,
        &context,
        embedder,
        config,
        resolution,
        input_prep_ms,
        progress_tx,
        show_progress_ui,
        setup_timings,
        cancel_token,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn execute_run_with_inputs(
    conn: &Connection,
    project_root: &Path,
    context: &IndexRunContext,
    embedder: Option<&mut Embedder>,
    config: &IndexingConfig,
    resolution: ScopeResolution,
    input_prep_ms: u128,
    progress_tx: Option<ProgressSender>,
    show_progress_ui: bool,
    setup_timings: Option<SetupTimings>,
    cancel_token: &CancellationToken,
) -> Result<PipelineStats, OneupError> {
    let run_started_at = setup_timings
        .as_ref()
        .map(|st| st.run_started_at)
        .unwrap_or_else(Instant::now);
    let mut stats = PipelineStats::default();
    let mut timings = TimingAccumulator {
        input_prep_ms: Some(input_prep_ms),
        db_prepare_ms: setup_timings.as_ref().map(|st| st.db_prepare_ms),
        model_prepare_ms: setup_timings.as_ref().map(|st| st.model_prepare_ms),
        ..Default::default()
    };
    let mut embedder = embedder;
    let ScopeResolution {
        inputs,
        requested_scope,
        executed_scope,
        changed_path_count,
        fallback_reason,
    } = resolution;
    let RunInputs {
        scanned_files,
        discovered_count,
        deleted_paths,
        metadata_unchanged_count,
    } = inputs;
    let content_read_count = scanned_files.len();

    let has_embedder = embedder.is_some();
    stats.embeddings_generated = has_embedder;
    stats.files_skipped += metadata_unchanged_count;
    let mut parallelism = Some(config.reporting_parallelism(0, has_embedder));

    if !has_embedder {
        info!("embedding model not available: indexing without embeddings (semantic search will be degraded, FTS-only mode active)");
        stats.embedding_unavailable_reason =
            Some("embedding model unavailable; indexed without embeddings".to_string());
    } else {
        // Use the loaded variant's identity (INT8 vs FP32) so an index
        // built under one variant fails closed with reindex guidance when reopened
        // under the other while vectors exist — the FP32->INT8 re-embed gate.
        let model_id = embedder
            .as_ref()
            .map(|embedder| embedder.model_id())
            .unwrap_or_else(|| HF_MODEL_REPO.to_string());
        schema::check_embedding_model_compatible(conn, &model_id, EMBEDDING_DIM).await?;
    }

    let scope_info = Some(IndexScopeInfo {
        requested: requested_scope,
        executed: executed_scope,
        changed_paths: changed_path_count,
        fallback_reason,
        // Include scope roots from config if this is a scoped run
        roots: config.scope_roots.clone(),
    });

    refresh_progress(
        &mut stats,
        project_root,
        context,
        progress_tx.as_ref(),
        ProgressUpdate {
            state: IndexState::Running,
            phase: IndexPhase::Scanning,
            files_total: 0,
            parallelism: parallelism.clone(),
            timings: Some(timings.snapshot(run_started_at)),
            scope: scope_info.clone(),
            prefilter: None,
            persist: true,
        },
    );

    let mut progress_ui = ProgressUi::stderr_if(
        pipeline_progress_ui_state(&stats, IndexPhase::Scanning, 0),
        show_progress_ui,
    );
    let scan_started_at = Instant::now();

    stats.files_scanned = discovered_count;
    let total_files = discovered_count;
    progress_ui.set_state(pipeline_progress_ui_state(
        &stats,
        IndexPhase::Scanning,
        total_files,
    ));
    timings.scan_ms = scan_started_at.elapsed().as_millis();
    parallelism = Some(config.reporting_parallelism(content_read_count, has_embedder));

    let prefilter_info = Some(IndexPrefilterInfo {
        discovered: discovered_count,
        metadata_skipped: metadata_unchanged_count,
        content_read: content_read_count,
        deleted: deleted_paths.len(),
    });

    if metadata_unchanged_count > 0 {
        info!(
            "metadata prefilter: {} files skipped (unchanged), {} files need processing",
            metadata_unchanged_count, content_read_count,
        );
    }

    if let Some(parallelism) = &parallelism {
        info!(
            "scan stage complete: {} files discovered ({} metadata-unchanged, {} to process) in {}ms (jobs configured {}, effective {}, embed threads {})",
            stats.files_scanned,
            metadata_unchanged_count,
            content_read_count,
            timings.scan_ms,
            parallelism.jobs_configured,
            parallelism.jobs_effective,
            parallelism.embed_threads,
        );
    }

    if !deleted_paths.is_empty() {
        // Apply fail-closed clamp on scope metadata loss.
        // If scope was configured but metadata is lost, clamp to recorded coverage
        // and emit a warning. Never silently delete all segments and re-index.
        let (clamped_deletes, scope_loss_warning) =
            clamp_deletion_on_scope_loss(conn, &context.context_id, &deleted_paths).await?;

        if let Some(warning_msg) = scope_loss_warning {
            warn!("scope metadata loss detected: {}", warning_msg);
            stats.embedding_unavailable_reason =
                Some(format!("scope metadata loss on rebuild: {}", warning_msg));
        }

        let store_before_delete = timings.store_ms;
        delete_removed_files(
            conn,
            &context.context_id,
            &clamped_deletes,
            config.effective_write_batch_files(clamped_deletes.len()),
            &mut timings,
        )
        .await?;
        for path in &clamped_deletes {
            debug!("removed segments for deleted file: {path}");
        }
        stats.files_deleted = clamped_deletes.len();
        info!(
            "delete cleanup complete: {} files removed in {}ms{}",
            clamped_deletes.len(),
            timings.store_ms.saturating_sub(store_before_delete),
            if clamped_deletes.len() < deleted_paths.len() {
                format!(
                    " ({} paths clamped due to scope metadata loss)",
                    deleted_paths.len() - clamped_deletes.len()
                )
            } else {
                String::new()
            },
        );
    }

    refresh_progress(
        &mut stats,
        project_root,
        context,
        progress_tx.as_ref(),
        ProgressUpdate {
            state: IndexState::Running,
            phase: IndexPhase::Parsing,
            files_total: total_files,
            parallelism: parallelism.clone(),
            timings: Some(timings.snapshot(run_started_at)),
            scope: scope_info.clone(),
            prefilter: prefilter_info.clone(),
            persist: true,
        },
    );

    progress_ui.set_state(pipeline_progress_ui_state(
        &stats,
        IndexPhase::Parsing,
        content_read_count,
    ));
    let parse_started_at = Instant::now();

    let mut double_buffer = StoreDoubleBuffer::spawn(conn.clone(), context.context_id.clone());
    let mut reorder_buffer = BTreeMap::new();
    let mut parse_workers = JoinSet::new();
    let mut next_to_dispatch = 0usize;
    let mut next_to_flush = 0usize;
    let mut unsupported_extensions: HashSet<String> = HashSet::new();
    {
        let mut flush_state = FlushState {
            stats: &mut stats,
            project_root,
            context,
            files_total: total_files,
            content_read_count,
            parallelism: parallelism.clone(),
            timings: &mut timings,
            run_started_at,
            unsupported_extensions: &mut unsupported_extensions,
            progress_tx: progress_tx.clone(),
            scope: scope_info.clone(),
            prefilter: prefilter_info.clone(),
            last_persisted_at: None,
        };

        while next_to_dispatch < content_read_count || !parse_workers.is_empty() {
            // Safe yield point: stop dispatching new files once cancelled. The
            // already-spawned parse workers are pure CPU and write nothing, so
            // abandoning their pending results leaves the index at the last
            // committed batch boundary (incomplete, never corrupt).
            if cancel_token.is_cancelled() {
                debug!("indexing cancelled at dispatch boundary; stopping at last committed batch");
                double_buffer
                    .finish(flush_state.stats, flush_state.timings)
                    .await?;
                return Err(IndexingError::Cancelled.into());
            }

            while next_to_dispatch < content_read_count && parse_workers.len() < config.jobs {
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
            flush_state.stats.files_processed += 1;
            let previous = reorder_buffer.insert(parse_result.sequence_id, parse_result.outcome);
            debug_assert!(
                previous.is_none(),
                "duplicate parse result sequence {}",
                parse_result.sequence_id
            );

            // Safe yield point: never enter a flush once cancelled.
            // `flush_reorder_buffer` is the only writer (embed-then-commit per
            // batch), so checking immediately before it — never inside it —
            // guarantees an aborted pass stops at a committed boundary.
            if cancel_token.is_cancelled() {
                debug!("indexing cancelled before flush; stopping at last committed batch");
                double_buffer
                    .finish(flush_state.stats, flush_state.timings)
                    .await?;
                return Err(IndexingError::Cancelled.into());
            }

            flush_reorder_buffer(
                conn,
                &mut double_buffer,
                &mut reorder_buffer,
                &mut next_to_flush,
                config,
                &mut embedder,
                &mut flush_state,
            )
            .await?;

            flush_state.refresh(current_progress_phase(flush_state.stats), false);
            progress_ui.set_state(pipeline_progress_ui_state(
                flush_state.stats,
                current_progress_phase(flush_state.stats),
                content_read_count,
            ));
        }

        // Safe yield point before the tail flush, for the same reason as the
        // in-loop check above.
        if cancel_token.is_cancelled() {
            debug!("indexing cancelled before tail flush; stopping at last committed batch");
            double_buffer
                .finish(flush_state.stats, flush_state.timings)
                .await?;
            return Err(IndexingError::Cancelled.into());
        }

        flush_reorder_buffer(
            conn,
            &mut double_buffer,
            &mut reorder_buffer,
            &mut next_to_flush,
            config,
            &mut embedder,
            &mut flush_state,
        )
        .await?;
    }

    double_buffer.finish(&mut stats, &mut timings).await?;

    if !unsupported_extensions.is_empty() {
        let mut exts: Vec<&str> = unsupported_extensions.iter().map(|s| s.as_str()).collect();
        exts.sort();
        debug!("skipped unsupported file types: .{}", exts.join(", ."));
    }

    info!(
        "parse stage complete: {} files processed in {}ms",
        content_read_count, timings.parse_ms
    );

    verify_stored_embedding_outcome(conn, &context.context_id, &mut stats).await;

    progress_ui.success_with(format!(
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
        context,
        progress_tx.as_ref(),
        ProgressUpdate {
            state: IndexState::Complete,
            phase: IndexPhase::Complete,
            files_total: total_files,
            parallelism,
            timings: Some(final_timings),
            scope: scope_info,
            prefilter: prefilter_info,
            persist: true,
        },
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

    fn test_worktree_context(
        state_root: &Path,
        source_root: &Path,
        context_id: &str,
        branch_name: &str,
    ) -> WorktreeContext {
        WorktreeContext {
            context_id: context_id.to_string(),
            state_root: state_root.to_path_buf(),
            source_root: source_root.to_path_buf(),
            main_worktree_root: state_root.to_path_buf(),
            worktree_role: if state_root == source_root {
                crate::shared::types::WorktreeRole::Main
            } else {
                crate::shared::types::WorktreeRole::Linked
            },
            git_dir: None,
            common_git_dir: None,
            branch_name: Some(branch_name.to_string()),
            branch_ref: Some(format!("refs/heads/{branch_name}")),
            head_oid: Some(format!("{branch_name:0>40}")),
            branch_status: BranchStatus::Named,
        }
    }

    fn synthetic_parsed_file(index: usize) -> ParsedWorkItem {
        ParsedWorkItem {
            relative_path: format!("synthetic_{index}.rs"),
            file_hash: format!("hash-{index}"),
            extension: "rs".to_string(),
            file_size: 0,
            modified_ns: 0,
            segments: Vec::new(),
        }
    }

    /// The store task must decouple the producer from the write's actual
    /// completion. `dispatch_pending` (the handoff of a prior batch to the
    /// store task) must return well before that batch's libSQL write
    /// finishes, so the flush stage no longer fully serializes
    /// embed-then-store — the caller is free to spend that window on the
    /// next batch's pure-CPU embed step. Previously, `store_ready_files` ran
    /// `build_segment_batches` (embed) and the write fully sequentially, so
    /// dispatch and completion were the same event; this assertion fails
    /// against that prior shape.
    #[tokio::test]
    async fn store_double_buffer_dispatch_returns_before_write_completes() {
        let (_db, conn) = setup().await;
        let context_id = "ctx-double-buffer";

        // Large enough that the real libSQL write takes measurably longer
        // than a bounded-channel send, making the decoupling observable
        // without depending on the embedder (out of scope for this
        // handoff-only assertion).
        let file_count = 2000;
        let parsed_files: Vec<ParsedWorkItem> =
            (0..file_count).map(synthetic_parsed_file).collect();
        let segment_batches: Vec<Vec<SegmentInsert>> =
            (0..file_count).map(|_| Vec::new()).collect();

        let mut double_buffer = StoreDoubleBuffer::spawn(conn.clone(), context_id.to_string());
        double_buffer.set_pending(StoreJob {
            parsed_files,
            segment_batches,
        });

        let dispatch_started_at = Instant::now();
        double_buffer.dispatch_pending().await.unwrap();
        let dispatch_elapsed_ms = dispatch_started_at.elapsed().as_millis();

        let mut stats = PipelineStats::default();
        let mut timings = TimingAccumulator::default();
        double_buffer
            .await_inflight(&mut stats, &mut timings)
            .await
            .unwrap();

        assert_eq!(
            stats.files_indexed, file_count,
            "await_inflight must attribute the completed batch's file count"
        );
        assert!(
            timings.store_ms >= 5,
            "the synthetic batch must take measurable store time to make the \
             handoff observable, took {}ms",
            timings.store_ms
        );
        assert!(
            dispatch_elapsed_ms < timings.store_ms,
            "dispatching a batch to the store task must return well before its \
             write completes (dispatch={dispatch_elapsed_ms}ms, store={}ms); the \
             flush stage must not fully serialize embed-then-store",
            timings.store_ms
        );

        double_buffer
            .finish(&mut stats, &mut timings)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn successful_context_run_records_indexed_head_oid() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("lib.rs"), "pub fn alpha() {}\n").unwrap();

        let (_db, conn) = setup().await;
        let config = IndexingConfig::new(2, 1, 1).unwrap();
        let context = test_worktree_context(tmp.path(), tmp.path(), "ctx-head", "main");

        run_with_context_scope_setup_and_progress_root(
            &conn,
            &context,
            None,
            &RunScope::Full,
            &config,
            None,
            false,
            None,
            None,
            None,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(
            segments::get_worktree_context_head_oid(&conn, "ctx-head")
                .await
                .unwrap(),
            context.head_oid,
            "a successful run must record the head OID it indexed at"
        );

        let mut moved = context.clone();
        moved.head_oid = Some("f".repeat(40));
        run_with_context_scope_setup_and_progress_root(
            &conn,
            &moved,
            None,
            &RunScope::Full,
            &config,
            None,
            false,
            None,
            None,
            None,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(
            segments::get_worktree_context_head_oid(&conn, "ctx-head")
                .await
                .unwrap(),
            moved.head_oid,
            "a later run must replace the recorded head OID"
        );
    }

    /// Inserts a `worktree_contexts` row with an explicit `updated_at`,
    /// bypassing `upsert_worktree_context`'s `datetime('now')` default so tests
    /// can seed contexts of a specific age for the `SupersededSameSource`
    /// keep-count/age policy.
    async fn seed_worktree_context_row(
        conn: &Connection,
        context_id: &str,
        source_root: &Path,
        state_root: &Path,
        updated_at: &str,
    ) {
        conn.execute(
            "INSERT INTO worktree_contexts (\
                context_id, project_id, state_root, source_root, main_worktree_root, \
                worktree_role, branch_name, branch_ref, branch_status, head_oid, \
                git_dir, common_git_dir, updated_at\
            ) VALUES (?1, 'migration-hook-proj', ?2, ?3, ?3, 'main', NULL, NULL, 'unknown', NULL, NULL, NULL, ?4)",
            libsql::params![
                context_id.to_string(),
                state_root.to_string_lossy().into_owned(),
                source_root.to_string_lossy().into_owned(),
                updated_at.to_string(),
            ],
        )
        .await
        .unwrap();
    }

    /// Seeds two recent (within-keep_count) and one aged, beyond-keep_count
    /// same-source peer under fabricated `state_root`s, so the active
    /// context recorded by the run under test ranks 1st and the aged peer
    /// ranks 4th (beyond `GC_SUPERSEDED_SAME_SOURCE_KEEP_COUNT` = 3).
    async fn seed_superseded_same_source_candidate(conn: &Connection, source_root: &Path) {
        for id in ["kept-1", "kept-2"] {
            seed_worktree_context_row(
                conn,
                id,
                source_root,
                Path::new("/other-state"),
                "2026-06-25 00:00:00",
            )
            .await;
        }
        seed_worktree_context_row(
            conn,
            "superseded-1",
            source_root,
            Path::new("/other-state-old"),
            "2026-01-01 00:00:00",
        )
        .await;
    }

    /// The opt-in migration-time `SupersededSameSource` prune
    /// must never fire while [`GC_MIGRATION_PRUNE_ENV_VAR`] is unset (default
    /// OFF) — a context that would otherwise qualify stays recorded.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn migration_time_prune_is_disabled_by_default() {
        use crate::shared::constants::GC_MIGRATION_PRUNE_ENV_VAR;

        let _env_lock = crate::shared::fs::ENV_MUTEX
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let saved = std::env::var_os(GC_MIGRATION_PRUNE_ENV_VAR);
        std::env::remove_var(GC_MIGRATION_PRUNE_ENV_VAR);

        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("lib.rs"), "pub fn alpha() {}\n").unwrap();

        let (_db, conn) = setup().await;
        seed_superseded_same_source_candidate(&conn, tmp.path()).await;

        let config = IndexingConfig::new(2, 1, 1).unwrap();
        let context = test_worktree_context(tmp.path(), tmp.path(), "ctx-active", "main");
        run_with_context_scope_setup_and_progress_root(
            &conn,
            &context,
            None,
            &RunScope::Full,
            &config,
            None,
            false,
            None,
            None,
            None,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        let remaining: std::collections::HashSet<String> = segments::list_worktree_contexts(&conn)
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.context_id)
            .collect();
        assert!(
            remaining.contains("superseded-1"),
            "the switch is OFF by default, so a qualifying context must not be pruned at migration time"
        );

        match saved {
            Some(v) => std::env::set_var(GC_MIGRATION_PRUNE_ENV_VAR, v),
            None => std::env::remove_var(GC_MIGRATION_PRUNE_ENV_VAR),
        }
    }

    /// Enabling [`GC_MIGRATION_PRUNE_ENV_VAR`] prunes
    /// `SupersededSameSource` contexts at migration time via `delete_context`,
    /// with no inline VACUUM — rows beyond the keep-count/age policy are gone,
    /// contexts within the policy (and the active context) survive.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn migration_time_prune_removes_superseded_same_source_contexts_when_enabled() {
        use crate::shared::constants::GC_MIGRATION_PRUNE_ENV_VAR;

        let _env_lock = crate::shared::fs::ENV_MUTEX
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let saved = std::env::var_os(GC_MIGRATION_PRUNE_ENV_VAR);
        std::env::set_var(GC_MIGRATION_PRUNE_ENV_VAR, "1");

        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("lib.rs"), "pub fn alpha() {}\n").unwrap();

        let (_db, conn) = setup().await;
        seed_superseded_same_source_candidate(&conn, tmp.path()).await;

        let config = IndexingConfig::new(2, 1, 1).unwrap();
        let context = test_worktree_context(tmp.path(), tmp.path(), "ctx-active", "main");
        run_with_context_scope_setup_and_progress_root(
            &conn,
            &context,
            None,
            &RunScope::Full,
            &config,
            None,
            false,
            None,
            None,
            None,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        let remaining: std::collections::HashSet<String> = segments::list_worktree_contexts(&conn)
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.context_id)
            .collect();
        assert!(
            remaining.contains("ctx-active"),
            "the active context must survive"
        );
        assert!(
            remaining.contains("kept-1") && remaining.contains("kept-2"),
            "within-keep_count same-source peers must survive"
        );
        assert!(
            !remaining.contains("superseded-1"),
            "beyond-keep_count, aged same-source peer must be pruned once the switch is enabled"
        );

        match saved {
            Some(v) => std::env::set_var(GC_MIGRATION_PRUNE_ENV_VAR, v),
            None => std::env::remove_var(GC_MIGRATION_PRUNE_ENV_VAR),
        }
    }

    #[tokio::test]
    async fn pre_cancelled_pass_returns_cancelled_and_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..8 {
            fs::write(
                tmp.path().join(format!("mod_{i}.rs")),
                format!("pub fn item_{i}() -> usize {{ {i} }}\n"),
            )
            .unwrap();
        }

        let (_db, conn) = setup().await;
        let config = IndexingConfig::new(2, 1, 1).unwrap();
        let context = test_worktree_context(tmp.path(), tmp.path(), "ctx-cancel", "main");

        // A token cancelled before the pass starts must short-circuit at the very
        // first dispatch-boundary check and surface the distinct Cancelled
        // outcome — not a success and not a hard error.
        let token = CancellationToken::new();
        token.cancel();

        let err = run_with_context_scope_setup_and_progress_root(
            &conn,
            &context,
            None,
            &RunScope::Full,
            &config,
            None,
            false,
            None,
            None,
            None,
            &token,
        )
        .await
        .expect_err("a pre-cancelled pass must not report success");

        assert!(
            matches!(err, OneupError::Indexing(IndexingError::Cancelled)),
            "cancellation must surface the distinct Cancelled variant, got: {err:?}"
        );

        // The pass stopped before any flush, so the index is consistent and
        // simply empty (incomplete, not corrupt): the DB still validates and a
        // read returns zero rows for this context.
        schema::ensure_current(&conn, &schema::SchemaContext::unspecified())
            .await
            .expect("a cancelled pass must leave the schema valid");
        assert_eq!(
            segments::count_files_for_context(&conn, "ctx-cancel")
                .await
                .unwrap(),
            0,
            "a pass cancelled at the first boundary must not commit any files"
        );
    }

    #[tokio::test]
    async fn run_after_cancellation_completes_the_remaining_files() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..8 {
            fs::write(
                tmp.path().join(format!("mod_{i}.rs")),
                format!("pub fn item_{i}() -> usize {{ {i} }}\n"),
            )
            .unwrap();
        }

        let (_db, conn) = setup().await;
        let config = IndexingConfig::new(2, 1, 1).unwrap();
        let context = test_worktree_context(tmp.path(), tmp.path(), "ctx-resume", "main");

        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let _ = run_with_context_scope_setup_and_progress_root(
            &conn,
            &context,
            None,
            &RunScope::Full,
            &config,
            None,
            false,
            None,
            None,
            None,
            &cancelled,
        )
        .await;

        // A subsequent pass under a live (never-cancelled) token re-indexes the
        // remainder against the consistent-but-incomplete index and succeeds.
        let live = CancellationToken::new();
        let stats = run_with_context_scope_setup_and_progress_root(
            &conn,
            &context,
            None,
            &RunScope::Full,
            &config,
            None,
            false,
            None,
            None,
            None,
            &live,
        )
        .await
        .expect("a normal pass after cancellation must complete");

        assert_eq!(
            stats.files_indexed, 8,
            "the resumed pass must index every source file"
        );
        assert_eq!(
            segments::count_files_for_context(&conn, "ctx-resume")
                .await
                .unwrap(),
            8,
            "every file must be committed after the resumed pass"
        );
    }

    async fn count_context_file_segments(
        conn: &Connection,
        context_id: &str,
        file_path: &str,
    ) -> i64 {
        let mut rows = conn
            .query(
                "SELECT COUNT(*) FROM segments WHERE context_id = ?1 AND file_path = ?2",
                libsql::params![context_id, file_path],
            )
            .await
            .unwrap();
        rows.next().await.unwrap().unwrap().get(0).unwrap()
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
    async fn run_without_embedder_reports_reason_and_stored_vector_coverage() {
        // Defect C regression: an index run without a usable embedder must
        // report an explicit unavailable reason and the measured stored
        // coverage (zero vector rows over a positive embeddable count), both
        // in stats and in the persisted progress contract.
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("lib.rs"), "pub fn coverage_probe() {}\n").unwrap();

        let (_db, conn) = setup().await;
        let stats = run(&conn, tmp.path(), None).await.unwrap();

        assert!(!stats.embeddings_generated);
        assert!(
            stats
                .embedding_unavailable_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("unavailable")),
            "missing embedder must carry an explicit reason, got {:?}",
            stats.embedding_unavailable_reason
        );
        assert_eq!(stats.vector_rows, Some(0));
        assert!(stats.embeddable_segments.is_some_and(|count| count > 0));

        assert!(!stats.progress.embeddings_enabled);
        assert_eq!(
            stats.progress.embedding_unavailable_reason,
            stats.embedding_unavailable_reason
        );
        assert_eq!(stats.progress.vector_rows, Some(0));
        assert_eq!(
            stats.progress.embeddable_segments,
            stats.embeddable_segments
        );
    }

    #[tokio::test]
    async fn verify_stored_embedding_outcome_flips_dishonest_embedding_claims() {
        // Defect C regression: a run claiming embeddings while the context
        // stores zero vector rows for embeddable segments must be reported as
        // embeddings unavailable with an explicit reason.
        let (_db, conn) = setup().await;
        let context_id = "ctx-honesty";
        let insert = segments::SegmentInsert {
            id: segments::generate_segment_id(context_id, "src/a.rs", 1, 3),
            file_path: "src/a.rs".to_string(),
            language: "rust".to_string(),
            block_type: "function".to_string(),
            content: "fn honesty_probe() {}".to_string(),
            line_start: 1,
            line_end: 3,
            content_key: None,
            embedding_vec: None,
            breadcrumb: None,
            complexity: 1,
            role: "DEFINITION".to_string(),
            defined_symbols: "[\"honesty_probe\"]".to_string(),
            referenced_symbols: "[]".to_string(),
            referenced_relations: "[]".to_string(),
            called_symbols: "[]".to_string(),
            called_relations: "[]".to_string(),
            file_hash: "hash".to_string(),
        };
        segments::replace_file_segments_for_context_tx(&conn, context_id, "src/a.rs", &[insert])
            .await
            .unwrap();

        let mut stats = PipelineStats {
            embeddings_generated: true,
            ..PipelineStats::default()
        };
        verify_stored_embedding_outcome(&conn, context_id, &mut stats).await;

        assert!(
            !stats.embeddings_generated,
            "zero stored vectors with embeddable segments must not report embeddings as enabled"
        );
        assert!(stats
            .embedding_unavailable_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("no vector rows")));
        assert_eq!(stats.vector_rows, Some(0));
        assert!(stats.embeddable_segments.is_some_and(|count| count > 0));
    }

    #[test]
    fn parse_scanned_file_routes_markdown_to_doc_segmenter() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("README.md");
        fs::write(&path, "# Title\n\nintro\n\n## Install\n\nsteps\n").unwrap();

        let outcome = parse_scanned_file(ScannedWorkItem {
            sequence_id: 0,
            relative_path: "README.md".to_string(),
            path,
            extension: "md".to_string(),
            stored_hash: None,
            file_size: 0,
            modified_ns: 0,
        });

        let parsed = match outcome {
            ParseResultKind::Ready(parsed) => parsed,
            other => panic!("expected ready parse outcome, got {other:?}"),
        };
        assert!(!parsed.segments.is_empty());
        assert!(parsed
            .segments
            .iter()
            .all(|segment| segment.block_type == markdown::DOC_SECTION_BLOCK_TYPE));
        let breadcrumbs: Vec<_> = parsed
            .segments
            .iter()
            .map(|segment| segment.breadcrumb.as_deref())
            .collect();
        assert!(
            breadcrumbs.contains(&Some("README > Title > Install")),
            "expected file-stem-rooted heading breadcrumb; found {breadcrumbs:?}"
        );
    }

    #[test]
    fn parse_scanned_file_caps_dense_structural_output_at_the_segment_limit() {
        // H3: the per-file segment cap must apply to tree-sitter output too, not
        // only the fallback chunker. A source with far more top-level definitions
        // than the cap must be truncated to MAX_SEGMENTS_PER_FILE.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("dense.rs");
        let function_count = MAX_SEGMENTS_PER_FILE + 500;
        let mut source = String::with_capacity(function_count * 32);
        for i in 0..function_count {
            source.push_str(&format!("pub fn func_{i}() {{ let _ = {i}; }}\n"));
        }
        fs::write(&path, &source).unwrap();

        let outcome = parse_scanned_file(ScannedWorkItem {
            sequence_id: 0,
            relative_path: "dense.rs".to_string(),
            path,
            extension: "rs".to_string(),
            stored_hash: None,
            file_size: source.len() as u64,
            modified_ns: 0,
        });

        let parsed = match outcome {
            ParseResultKind::Ready(parsed) => parsed,
            other => panic!("expected ready parse outcome, got {other:?}"),
        };
        assert_eq!(
            parsed.segments.len(),
            MAX_SEGMENTS_PER_FILE,
            "structural parser output must be capped at {MAX_SEGMENTS_PER_FILE} segments"
        );
    }

    #[test]
    fn should_persist_progress_collapses_rapid_skips_into_one_write_per_window() {
        let start = Instant::now();
        let mut last_persisted_at: Option<Instant> = None;
        let mut persisted_at: Vec<Instant> = Vec::new();

        // 25 synthetic progress-skip events 10ms apart (0..=240ms), all
        // strictly inside the 250ms throttle window.
        for step in 0..25u32 {
            let now = start + Duration::from_millis(u64::from(step) * 10);
            if should_persist_progress(last_persisted_at, now, false) {
                persisted_at.push(now);
                last_persisted_at = Some(now);
            }
        }

        assert_eq!(
            persisted_at,
            vec![start],
            "many skips inside a single 250ms window must collapse to the \
             one opening persist_progress/atomic_replace call"
        );

        // An event that finally clears the window persists again.
        let now_past_window = start + Duration::from_millis(260);
        assert!(
            should_persist_progress(last_persisted_at, now_past_window, false),
            "a skip event past the throttle window must persist again"
        );
    }

    #[test]
    fn should_persist_progress_forces_terminal_complete_flush() {
        let last_persist = Instant::now();
        let now = last_persist + Duration::from_millis(10);

        assert!(
            !should_persist_progress(Some(last_persist), now, false),
            "sanity: a non-terminal refresh this soon after a persist must be throttled"
        );
        assert!(
            should_persist_progress(Some(last_persist), now, true),
            "the terminal Complete phase must force a flush even inside the throttle window"
        );
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
    async fn full_run_metadata_prefilter_skips_unchanged_content_reads() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a.rs"), "fn a() {}\n").unwrap();
        fs::write(tmp.path().join("b.rs"), "fn b() {}\n").unwrap();

        let (_db, conn) = setup().await;

        let stats1 = run(&conn, tmp.path(), None).await.unwrap();
        assert_eq!(stats1.files_scanned, 2);
        assert_eq!(stats1.progress.prefilter.as_ref().unwrap().discovered, 2);
        assert_eq!(stats1.progress.prefilter.as_ref().unwrap().content_read, 2);

        let stats2 = run(&conn, tmp.path(), None).await.unwrap();
        let prefilter = stats2.progress.prefilter.as_ref().unwrap();

        assert_eq!(stats2.files_scanned, 2);
        assert_eq!(stats2.files_processed, 0);
        assert_eq!(stats2.files_indexed, 0);
        assert_eq!(stats2.files_skipped, 2);
        assert_eq!(prefilter.discovered, 2);
        assert_eq!(prefilter.metadata_skipped, prefilter.discovered);
        assert_eq!(prefilter.content_read, 0);
        assert_eq!(prefilter.deleted, 0);
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
    async fn full_run_metadata_prefilter_reads_changed_metadata_only() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a.rs"), "fn a() {}\n").unwrap();
        fs::write(tmp.path().join("b.rs"), "fn b() {}\n").unwrap();

        let (_db, conn) = setup().await;

        run(&conn, tmp.path(), None).await.unwrap();
        fs::write(tmp.path().join("a.rs"), "fn a() {}\nfn a2() {}\n").unwrap();

        let stats = run(&conn, tmp.path(), None).await.unwrap();
        let prefilter = stats.progress.prefilter.as_ref().unwrap();

        assert_eq!(stats.files_scanned, 2);
        assert_eq!(stats.files_processed, 1);
        assert_eq!(stats.files_indexed, 1);
        assert_eq!(stats.files_skipped, 1);
        assert_eq!(prefilter.discovered, 2);
        assert_eq!(prefilter.metadata_skipped, 1);
        assert_eq!(prefilter.content_read, 1);
        assert_eq!(prefilter.deleted, 0);
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
        let config = IndexingConfig::from_sources(Some(2), Some(1), None).unwrap();
        assert!(config.write_batch_files > 1);
        assert_eq!(config.effective_write_batch_files(2), 2);
        assert_eq!(config.effective_write_batch_files(1), 1);

        let scope = RunScope::from_paths(["a.rs", "b.rs", "c.rs"].map(PathBuf::from)).unwrap();
        let stats = run_with_scope(&conn, tmp.path(), None, &scope, &config)
            .await
            .unwrap();

        assert_eq!(stats.files_scanned, 2);
        assert_eq!(stats.files_deleted, 1);
        assert_eq!(stats.files_indexed, 2);

        let paths = segments::get_all_file_paths(&conn).await.unwrap();
        assert_eq!(paths, vec!["a.rs", "c.rs", "keep.rs"]);
    }

    #[tokio::test]
    async fn context_scoped_runs_keep_same_relative_paths_independent() {
        let tmp = tempfile::tempdir().unwrap();
        let main_root = tmp.path().join("main");
        let linked_root = tmp.path().join("linked");
        fs::create_dir_all(&main_root).unwrap();
        fs::create_dir_all(&linked_root).unwrap();
        fs::write(main_root.join("shared.rs"), "pub fn main_branch() {}\n").unwrap();
        fs::write(linked_root.join("shared.rs"), "pub fn linked_branch() {}\n").unwrap();

        let (_db, conn) = setup().await;
        let config = IndexingConfig::new(2, 1, 1).unwrap();
        let main_context = test_worktree_context(&main_root, &main_root, "ctx-main", "main");
        let linked_context =
            test_worktree_context(&main_root, &linked_root, "ctx-linked", "feature");

        let main_stats = run_with_context_scope_setup_and_progress_root(
            &conn,
            &main_context,
            None,
            &RunScope::Full,
            &config,
            None,
            false,
            None,
            None,
            Some(&main_root),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let linked_stats = run_with_context_scope_setup_and_progress_root(
            &conn,
            &linked_context,
            None,
            &RunScope::Full,
            &config,
            None,
            false,
            None,
            None,
            Some(&main_root),
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(main_stats.progress.context_id.as_deref(), Some("ctx-main"));
        assert_eq!(
            linked_stats.progress.source_root.as_deref(),
            Some(linked_root.as_path())
        );
        assert_eq!(
            linked_stats.progress.branch_name.as_deref(),
            Some("feature")
        );
        assert_eq!(
            linked_stats.progress.branch_status,
            Some(BranchStatus::Named)
        );
        assert!(count_context_file_segments(&conn, "ctx-main", "shared.rs").await > 0);
        assert!(count_context_file_segments(&conn, "ctx-linked", "shared.rs").await > 0);

        fs::write(
            linked_root.join("shared.rs"),
            "pub fn linked_branch() {}\npub fn linked_only() {}\n",
        )
        .unwrap();
        let scope = RunScope::from_paths([PathBuf::from("shared.rs")]).unwrap();
        let linked_update = run_with_context_scope_setup_and_progress_root(
            &conn,
            &linked_context,
            None,
            &scope,
            &config,
            None,
            false,
            None,
            None,
            Some(&main_root),
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(linked_update.files_indexed, 1);
        assert!(count_context_file_segments(&conn, "ctx-main", "shared.rs").await > 0);
        assert!(count_context_file_segments(&conn, "ctx-linked", "shared.rs").await > 0);

        fs::remove_file(linked_root.join("shared.rs")).unwrap();
        let linked_delete = run_with_context_scope_setup_and_progress_root(
            &conn,
            &linked_context,
            None,
            &scope,
            &config,
            None,
            false,
            None,
            None,
            Some(&main_root),
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(linked_delete.files_deleted, 1);
        assert!(count_context_file_segments(&conn, "ctx-main", "shared.rs").await > 0);
        assert_eq!(
            count_context_file_segments(&conn, "ctx-linked", "shared.rs").await,
            0
        );
        assert!(
            segments::get_all_indexed_files_for_context(&conn, "ctx-main")
                .await
                .unwrap()
                .contains_key("shared.rs")
        );
        assert!(
            !segments::get_all_indexed_files_for_context(&conn, "ctx-linked")
                .await
                .unwrap()
                .contains_key("shared.rs")
        );
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
        // persist_progress writes via the secure-fs helper, which rejects
        // symlinked path components (macOS tmp dirs resolve through
        // /var -> /private/var); canonicalize so the progress file is written
        // and read back from the same real path.
        let project_root = tmp.path().canonicalize().unwrap();
        fs::write(project_root.join("lib.rs"), "pub fn alpha() {}\n").unwrap();

        let (_db, conn) = setup().await;
        let config = IndexingConfig::new(3, 2, 1).unwrap();
        let stats = run_with_config(&conn, &project_root, None, &config)
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

        let progress_path = index_progress_path(&project_root);
        let persisted: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(progress_path).unwrap()).unwrap();

        assert_eq!(persisted["files_processed"], 1);
        assert!(persisted["message"]
            .as_str()
            .unwrap()
            .contains("Processed 1 files"));
        assert_eq!(persisted["parallelism"]["jobs_configured"], 3);
        assert_eq!(persisted["parallelism"]["jobs_effective"], 1);
        assert_eq!(persisted["parallelism"]["embed_threads"], 0);
        assert!(
            persisted["timings"]["total_ms"].as_u64().unwrap()
                >= persisted["timings"]["scan_ms"].as_u64().unwrap()
        );
    }

    #[tokio::test]
    async fn progress_sender_emits_live_processed_counts() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a.rs"), "pub fn alpha() {}\n").unwrap();
        fs::write(tmp.path().join("b.rs"), "pub fn beta() {}\n").unwrap();

        let (_db, conn) = setup().await;
        let config = IndexingConfig::new(2, 1, 1).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();

        let stats = run_with_config_and_progress(&conn, tmp.path(), None, &config, Some(tx))
            .await
            .unwrap();
        let progress: Vec<IndexProgress> = rx.try_iter().collect();

        assert!(!progress.is_empty());
        assert!(progress.iter().any(|snapshot| {
            snapshot.state == IndexState::Running && snapshot.files_processed > 0
        }));
        assert_eq!(progress.last().unwrap().state, IndexState::Complete);
        assert_eq!(progress.last().unwrap().files_processed, 2);
        assert_eq!(stats.progress.files_processed, 2);
    }

    #[tokio::test]
    async fn persisted_progress_reports_embed_threads_when_embeddings_enabled() {
        // Pin the always-provisioned FP32 baseline so this embedding-path test
        // does not depend on the INT8 default artifact being present locally
        // (provisioned separately).
        let _variant = crate::indexer::embedder::Fp32VariantTestGuard::set();
        if !crate::indexer::embedder::is_model_available() {
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().canonicalize().unwrap();
        fs::write(project_root.join("lib.rs"), "pub fn alpha() {}\n").unwrap();

        let (_db, conn) = setup().await;
        let config = IndexingConfig::new(2, 2, 1).unwrap();
        let mut embedder = Embedder::new_with_threads(config.embed_threads)
            .await
            .unwrap();

        let stats = run_with_config(&conn, &project_root, Some(&mut embedder), &config)
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

        let progress_path = index_progress_path(&project_root);
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
        let id1 = segments::generate_segment_id(DEFAULT_INDEX_CONTEXT_ID, "src/main.rs", 1, 10);
        let id2 = segments::generate_segment_id(DEFAULT_INDEX_CONTEXT_ID, "src/main.rs", 1, 10);
        let id3 = segments::generate_segment_id(DEFAULT_INDEX_CONTEXT_ID, "src/main.rs", 1, 11);
        let id4 = segments::generate_segment_id("ctx-linked", "src/main.rs", 1, 10);

        assert_eq!(id1, id2);
        assert_ne!(id1, id3);
        assert_ne!(id1, id4);
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
            referenced_relations: Vec::new(),
            called_symbols: Vec::new(),
            called_relations: Vec::new(),
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

    fn embeddable_segment(content: &str) -> ParsedSegment {
        ParsedSegment {
            content: content.into(),
            block_type: "struct".into(),
            line_start: 159,
            line_end: 161,
            language: "rust".into(),
            breadcrumb: None,
            complexity: 0,
            role: crate::shared::types::SegmentRole::Definition,
            defined_symbols: Vec::new(),
            referenced_symbols: Vec::new(),
            referenced_relations: Vec::new(),
            called_symbols: Vec::new(),
            called_relations: Vec::new(),
        }
    }

    #[test]
    fn compose_embedding_text_prepends_language_stem_breadcrumb_and_symbols() {
        let mut segment = embeddable_segment(
            "pub struct ImpactHorizonEngine<'a> {\n    conn: &'a Connection,\n}",
        );
        segment.breadcrumb = Some("horizon expansion".into());
        segment.defined_symbols = vec!["ImpactHorizonEngine".into()];

        let text = compose_embedding_text("src/search/impact.rs", &segment);

        assert_eq!(
            text,
            "rust impact horizon expansion ImpactHorizonEngine\npub struct ImpactHorizonEngine<'a> {\n    conn: &'a Connection,\n}"
        );
    }

    #[test]
    fn compose_embedding_text_skips_missing_breadcrumb_and_symbols() {
        let mut segment = embeddable_segment("SUMMARY_JSON=\"$OUT_DIR/summary.json\"");
        segment.language = "shell".into();

        let text = compose_embedding_text("scripts/benchmark_parallel_indexing.sh", &segment);

        assert_eq!(
            text,
            "shell benchmark_parallel_indexing\nSUMMARY_JSON=\"$OUT_DIR/summary.json\""
        );
    }

    #[test]
    fn compose_embedding_text_clamps_header_and_total_length() {
        let mut segment = embeddable_segment(&"x".repeat(EMBEDDING_INPUT_MAX_CHARS * 2));
        segment.breadcrumb = Some("breadcrumb ".repeat(40));
        segment.defined_symbols = (0..40).map(|i| format!("Symbol{i}")).collect();

        let text = compose_embedding_text("src/lib.rs", &segment);
        let header = text.split('\n').next().unwrap();

        assert!(header.chars().count() <= EMBEDDING_HEADER_MAX_CHARS);
        assert!(text.chars().count() <= EMBEDDING_INPUT_MAX_CHARS);
        assert!(
            text.split_once('\n').unwrap().1.starts_with("xxx"),
            "content must follow the clamped header"
        );
    }

    #[test]
    fn compose_embedding_text_leaves_stored_segment_untouched() {
        let mut segment = embeddable_segment("pub struct ImpactHorizonEngine;");
        segment.defined_symbols = vec!["ImpactHorizonEngine".into()];

        let composed = compose_embedding_text("src/search/impact.rs", &segment);
        let insert = build_segment_insert(
            DEFAULT_INDEX_CONTEXT_ID,
            "src/search/impact.rs",
            "hash",
            &segment,
            None,
            None,
        );

        assert_ne!(composed, segment.content);
        assert_eq!(insert.content, segment.content);
        assert_eq!(
            insert.id,
            segments::generate_segment_id(
                DEFAULT_INDEX_CONTEXT_ID,
                "src/search/impact.rs",
                segment.line_start,
                segment.line_end
            )
        );
    }

    #[test]
    fn embedding_content_key_is_context_invariant_and_model_differentiated() {
        let mut segment = embeddable_segment("pub struct ImpactHorizonEngine;");
        segment.defined_symbols = vec!["ImpactHorizonEngine".into()];

        // The embed input is a deterministic function of repository-relative
        // path + content, so two different contexts (branches/worktrees) that
        // hold the same chunk produce the byte-identical embed input that any
        // context would, and therefore the same content key.
        let embed_input = compose_embedding_text("src/search/impact.rs", &segment);
        let key_ctx_a = embedding_content_key(
            HF_MODEL_REPO,
            EMBEDDING_DIM,
            EMBEDDING_MAX_TOKENS,
            &embed_input,
        );
        let key_ctx_b = embedding_content_key(
            HF_MODEL_REPO,
            EMBEDDING_DIM,
            EMBEDDING_MAX_TOKENS,
            &embed_input,
        );
        assert_eq!(
            key_ctx_a, key_ctx_b,
            "identical content must yield an identical key across contexts"
        );

        // A SHA-256 hex digest is 64 lowercase hex characters.
        assert_eq!(key_ctx_a.len(), 64);
        assert!(key_ctx_a.bytes().all(|b| b.is_ascii_hexdigit()));

        // Different content must yield a different key.
        let other_input = compose_embedding_text("src/search/impact.rs", &{
            let mut s = segment.clone();
            s.content = "pub struct DifferentEngine;".into();
            s
        });
        assert_ne!(
            embedding_content_key(
                HF_MODEL_REPO,
                EMBEDDING_DIM,
                EMBEDDING_MAX_TOKENS,
                &other_input
            ),
            key_ctx_a,
            "distinct content must not collide"
        );

        // Changing the model identity must invalidate reuse: a different model
        // id or embedding dimension yields a different key for the same input.
        assert_ne!(
            embedding_content_key(
                "org/other-model",
                EMBEDDING_DIM,
                EMBEDDING_MAX_TOKENS,
                &embed_input
            ),
            key_ctx_a,
            "a different model id must change the key"
        );
        assert_ne!(
            embedding_content_key(
                HF_MODEL_REPO,
                EMBEDDING_DIM + 1,
                EMBEDDING_MAX_TOKENS,
                &embed_input
            ),
            key_ctx_a,
            "a different embedding dimension must change the key"
        );

        // Changing the token window must invalidate reuse: the same content
        // embedded at a 128- vs 256-token window can diverge numerically once
        // its tail crosses 128 tokens, so the key must not alias across windows.
        assert_ne!(
            embedding_content_key(HF_MODEL_REPO, EMBEDDING_DIM, 128, &embed_input),
            embedding_content_key(HF_MODEL_REPO, EMBEDDING_DIM, 256, &embed_input),
            "a different max_tokens window must change the key"
        );

        // Fields are delimited so adjacent numeric/string fields cannot alias by
        // shifting a digit across a boundary (e.g. dim/max_tokens 1|2 vs 12|... ).
        assert_ne!(
            embedding_content_key("ab", 1, 2, &embed_input),
            embedding_content_key("a", 12, 2, &embed_input),
            "field boundaries must be unambiguous"
        );
        assert_ne!(
            embedding_content_key("m", 1, 23, &embed_input),
            embedding_content_key("m", 12, 3, &embed_input),
            "dim/max_tokens boundary must be unambiguous"
        );
    }

    #[test]
    fn plan_embedding_work_returns_only_deduped_in_order_misses() {
        fn seg(key: &str, input: &str) -> EmbeddableSegment {
            EmbeddableSegment {
                content_key: key.into(),
                embed_input: input.into(),
            }
        }

        let embeddable = vec![
            seg("k1", "input-1"),
            seg("k2", "input-2"),
            seg("k1", "input-1"), // same content as the first entry (within-batch dup)
            seg("k3", "input-3"),
        ];

        // k2 already lives in the pool, so it is reused rather than re-embedded;
        // the duplicate k1 is collapsed so identical content is embedded once;
        // ordering is preserved for deterministic embedding.
        let present: HashSet<String> = ["k2".to_string()].into_iter().collect();
        assert_eq!(
            plan_embedding_work(&embeddable, &present),
            vec![("k1", "input-1"), ("k3", "input-3")]
        );

        // Cold start (nothing pooled): every distinct chunk is embedded once, in
        // order — no regression for the no-overlap case.
        let cold = HashSet::new();
        assert_eq!(
            plan_embedding_work(&embeddable, &cold),
            vec![("k1", "input-1"), ("k2", "input-2"), ("k3", "input-3")]
        );

        // Nothing embeddable means nothing to embed.
        assert!(plan_embedding_work(&[], &cold).is_empty());
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
        // Pin the always-provisioned FP32 baseline so this embedding-path test
        // does not depend on the INT8 default artifact being present locally
        // (provisioned separately).
        let _variant = crate::indexer::embedder::Fp32VariantTestGuard::set();
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

    /// Fail-closed clamp on scope metadata loss.
    /// Test scenario: no scope, no coverage -> proceed normally.
    #[tokio::test]
    async fn clamp_deletion_on_scope_loss_no_scope_no_coverage() {
        let (_db, conn) = setup().await;
        let context_id = "ctx-no-scope";
        let requested = vec!["file1.rs".to_string(), "file2.rs".to_string()];

        let (clamped, warning) = clamp_deletion_on_scope_loss(&conn, context_id, &requested)
            .await
            .unwrap();

        assert_eq!(
            clamped, requested,
            "with no scope and no coverage, all requested deletes should proceed"
        );
        assert!(
            warning.is_none(),
            "no warning should be emitted when there is no coverage"
        );
    }

    /// Fail-closed clamp on scope metadata loss.
    /// Test scenario: scope present -> allow requested deletes (v1 logic).
    #[tokio::test]
    async fn clamp_deletion_on_scope_loss_with_scope_present() {
        let (_db, conn) = setup().await;
        let context_id = "ctx-with-scope";

        // Write scope metadata
        schema::write_scope_to_meta(&conn, &["services/auth".to_string()])
            .await
            .unwrap();

        let requested = vec!["file1.rs".to_string(), "file2.rs".to_string()];

        let (clamped, warning) = clamp_deletion_on_scope_loss(&conn, context_id, &requested)
            .await
            .unwrap();

        assert_eq!(
            clamped, requested,
            "with scope present, all requested deletes should proceed (v1 allows any recorded file)"
        );
        assert!(
            warning.is_none(),
            "no warning should be emitted when scope is present"
        );
    }

    /// Fail-closed clamp on scope metadata loss.
    /// Test scenario: scope lost, coverage exists -> clamp to recorded paths.
    #[tokio::test]
    async fn clamp_deletion_on_scope_loss_clamps_to_coverage() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("file1.rs"), "pub fn alpha() {}\n").unwrap();
        fs::write(tmp.path().join("file2.rs"), "pub fn beta() {}\n").unwrap();

        let (_db, conn) = setup().await;
        // Use default context_id which is what run_with_config uses
        let context_id = DEFAULT_INDEX_CONTEXT_ID;
        let config = IndexingConfig::new(2, 1, 1).unwrap();

        // Index files to create coverage
        run_with_config(&conn, tmp.path(), None, &config)
            .await
            .unwrap();

        // Verify coverage exists
        let recorded = segments::get_all_file_paths_for_context(&conn, context_id)
            .await
            .unwrap();
        assert!(
            !recorded.is_empty(),
            "files should be indexed before testing clamp: got {} recorded files",
            recorded.len()
        );

        // Simulate metadata loss: no scope set, but coverage exists
        // Request deletion of more paths than are recorded (includes ones not in index)
        let mut requested = recorded.clone();
        requested.push("file3.rs".to_string()); // not recorded
        requested.push("file4.rs".to_string()); // not recorded

        let (clamped, warning) = clamp_deletion_on_scope_loss(&conn, context_id, &requested)
            .await
            .unwrap();

        // Clamped should only include recorded paths
        assert!(
            clamped.len() < requested.len(),
            "clamp should reduce deletions to recorded coverage: clamped={}, requested={}",
            clamped.len(),
            requested.len()
        );
        assert!(
            warning.is_some(),
            "warning should be emitted when scope is lost but coverage exists"
        );

        let warning_text = warning.unwrap();
        assert!(
            warning_text.contains("Scope metadata lost"),
            "warning should mention scope metadata loss"
        );
    }

    /// Regression test for scope metadata re-stamping.
    /// Ensures that scope metadata is persisted correctly to staging DB
    /// before finalize_and_swap, so rebuilds cannot erase coverage truth.
    /// This test verifies the core persistence mechanism, not the full swap.
    #[tokio::test]
    async fn scope_metadata_persists_before_and_after_staging() {
        let (_db, conn) = setup().await;

        // Write initial scope metadata to live DB
        let initial_scope = vec!["services/auth".to_string()];
        schema::write_scope_to_meta(&conn, &initial_scope)
            .await
            .unwrap();

        // Verify it's in the live DB
        let read_scope = schema::read_scope_from_meta(&conn).await.unwrap();
        assert_eq!(
            read_scope,
            Some(initial_scope.clone()),
            "initial scope should be written to live DB"
        );

        // Simulate a rebuild: create a separate staging connection
        // (in real code, this is done via StagingRebuild::open)
        let staging_db = Db::open_memory().await.unwrap();
        let staging_conn = staging_db.connect().unwrap();

        // Initialize staging schema
        schema::initialize(&staging_conn).await.unwrap();

        // Now write the NEW scope to the staging DB before finalize_and_swap
        let new_scope = vec!["services/auth".to_string(), "libs/core".to_string()];
        schema::write_scope_to_meta(&staging_conn, &new_scope)
            .await
            .unwrap();

        // Verify scope is in staging DB
        let staged_scope = schema::read_scope_from_meta(&staging_conn).await.unwrap();
        assert_eq!(
            staged_scope,
            Some(new_scope.clone()),
            "scope should be stamped into staging DB before swap"
        );

        // The live DB should still have the old scope (not yet swapped)
        let live_scope = schema::read_scope_from_meta(&conn).await.unwrap();
        assert_eq!(
            live_scope,
            Some(initial_scope),
            "live DB should retain original scope until swap completes"
        );
    }
}
