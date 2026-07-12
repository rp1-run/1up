use std::time::Instant;

use clap::Args;

use crate::cli::output::{formatter_for, spawn_index_watch_renderer};
use crate::daemon::registry::Registry;
use crate::indexer::embedder::{
    clear_download_failure, EmbeddingLoadStatus, EmbeddingRuntime, EmbeddingUnavailableReason,
};
use crate::indexer::pipeline;
use crate::shared::config;
use crate::shared::constants::SCHEMA_VERSION;
use crate::shared::progress::{ProgressState, ProgressUi};
use crate::shared::types::{
    IndexPhase, IndexProgress, IndexState, OutputFormat, SetupTimings, WorktreeContext,
};
use crate::storage::swap::StagingRebuild;

#[derive(Args)]
pub struct ReindexArgs {
    /// Directory to re-index (defaults to current directory)
    #[arg(default_value = ".")]
    pub path: String,

    /// Maximum concurrent parse workers; overrides ONEUP_INDEX_JOBS
    #[arg(long, value_name = "N", value_parser = crate::cli::parse_positive_usize)]
    pub jobs: Option<usize>,

    /// ONNX intra-op threads; overrides ONEUP_EMBED_THREADS
    #[arg(long, value_name = "N", value_parser = crate::cli::parse_positive_usize)]
    pub embed_threads: Option<usize>,

    /// Glob pattern to include (repeatable); overrides the persisted per-project config
    #[arg(long = "include-glob", value_name = "GLOB")]
    pub include_globs: Vec<String>,

    /// Glob pattern to exclude (repeatable); overrides the persisted per-project config
    #[arg(long = "exclude-glob", value_name = "GLOB")]
    pub exclude_globs: Vec<String>,

    /// Dotfile directory to index despite the default hidden-directory exclusion (repeatable)
    #[arg(long = "index-hidden-dir", value_name = "DIR")]
    pub index_hidden_dirs: Vec<String>,

    /// Stream live reindex progress updates until the run completes
    #[arg(long)]
    pub watch: bool,

    /// Output format override (defaults to plain)
    #[arg(long, short = 'f')]
    pub format: Option<OutputFormat>,
}

fn spin(msg: impl Into<String>, show_progress_ui: bool) -> ProgressUi {
    ProgressUi::stderr_if(ProgressState::spinner(msg), show_progress_ui)
}

/// Treats an empty repeated-flag `Vec` as "not supplied on the CLI" so
/// `resolve_indexing_config_with_globs` falls through to the persisted
/// registry value instead of clobbering it with an empty override.
fn cli_globs(globs: Vec<String>) -> Option<Vec<String>> {
    (!globs.is_empty()).then_some(globs)
}

fn should_use_direct_watch_progress_ui(format: OutputFormat) -> bool {
    use std::io::IsTerminal;

    format == OutputFormat::Human && std::io::stderr().is_terminal()
}

fn model_status_message(status: &EmbeddingLoadStatus) -> String {
    match status {
        EmbeddingLoadStatus::Warm | EmbeddingLoadStatus::Loaded => {
            "Embedding model ready".to_string()
        }
        EmbeddingLoadStatus::Downloaded => "Embedding model downloaded".to_string(),
        EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::PreviousDownloadFailed) => {
            "Embedding model unavailable (previous download failed)".to_string()
        }
        EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::DownloadFailed(err)) => {
            format!("Model download failed ({err})")
        }
        EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::ModelDirUnavailable(err))
        | EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::LoadFailed(err)) => {
            format!("Embedding model failed to load ({err})")
        }
        EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::ArtifactsUnverifiable(
            err,
        )) => {
            format!("Embedding model artifacts failed verification ({err})")
        }
        EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::ModelMissing) => {
            "Embedding model unavailable".to_string()
        }
    }
}

fn send_watch_progress(
    progress_tx: &pipeline::ProgressSender,
    phase: IndexPhase,
    message: impl Into<String>,
) {
    let _ = progress_tx.send(IndexProgress::watch(
        IndexState::Running,
        phase,
        message.into(),
    ));
}

async fn exec_watch(args: ReindexArgs, format: OutputFormat) -> anyhow::Result<()> {
    let resolved = crate::shared::project::resolve_project_root_for_creation(
        std::path::Path::new(&args.path),
    )?;
    let project_root = resolved.state_root;
    let worktree_context = resolved.worktree_context;
    let fmt = formatter_for(format);
    let registry = Registry::load()?;
    let indexing_config = config::resolve_indexing_config_with_globs(
        args.jobs,
        args.embed_threads,
        cli_globs(args.include_globs),
        cli_globs(args.exclude_globs),
        cli_globs(args.index_hidden_dirs),
        registry.indexing_config_for_context(&worktree_context),
    )?;

    if should_use_direct_watch_progress_ui(format) {
        let stats = run_reindex_once(
            &worktree_context,
            &project_root,
            &indexing_config,
            true,
            None,
        )
        .await?;

        let msg = format!(
            "Re-indexed {} files ({} segments). Clean rebuild complete.{}",
            stats.files_indexed,
            stats.segments_stored,
            if stats.embeddings_generated {
                ""
            } else {
                " [no embeddings]"
            },
        );
        println!("{}", fmt.format_index_summary(&msg, &stats.progress));
        return Ok(());
    }

    let (progress_tx, progress_handle) = spawn_index_watch_renderer(format);
    send_watch_progress(&progress_tx, IndexPhase::Rebuilding, "Rebuilding database");

    let result = run_reindex_once(
        &worktree_context,
        &project_root,
        &indexing_config,
        false,
        Some(&progress_tx),
    )
    .await;

    drop(progress_tx);
    let _ = progress_handle.join();

    let stats = result?;
    if format != OutputFormat::Json {
        let msg = format!(
            "Re-indexed {} files ({} segments). Clean rebuild complete.{}",
            stats.files_indexed,
            stats.segments_stored,
            if stats.embeddings_generated {
                ""
            } else {
                " [no embeddings]"
            },
        );
        println!("{}", fmt.format_index_summary(&msg, &stats.progress));
    }

    Ok(())
}

pub async fn exec(args: ReindexArgs, format: OutputFormat) -> anyhow::Result<()> {
    if args.watch {
        return exec_watch(args, format).await;
    }

    let resolved = crate::shared::project::resolve_project_root_for_creation(
        std::path::Path::new(&args.path),
    )?;
    let project_root = resolved.state_root;
    let worktree_context = resolved.worktree_context;
    let fmt = formatter_for(format);
    let registry = Registry::load()?;
    let indexing_config = config::resolve_indexing_config_with_globs(
        args.jobs,
        args.embed_threads,
        cli_globs(args.include_globs),
        cli_globs(args.exclude_globs),
        cli_globs(args.index_hidden_dirs),
        registry.indexing_config_for_context(&worktree_context),
    )?;
    let show_progress_ui = format == OutputFormat::Human;

    // REQ-004/H2: `1up reindex` rebuilds `.1up/index.db` directly (it does not funnel
    // through `ensure_project_id`), so ensure `.1up/.gitignore` here too — otherwise
    // a reindex-first user on a fresh repo could commit their local index.db.
    // Best-effort: a gitignore failure must never block the rebuild.
    if let Err(err) = crate::shared::project::ensure_project_gitignore(&project_root) {
        tracing::warn!(
            "failed to ensure .1up/.gitignore at {}: {err}",
            project_root.display()
        );
    }

    let stats = run_reindex_once(
        &worktree_context,
        &project_root,
        &indexing_config,
        show_progress_ui,
        None,
    )
    .await?;

    let msg = format!(
        "Re-indexed {} files ({} segments). Clean rebuild complete.{}",
        stats.files_indexed,
        stats.segments_stored,
        if stats.embeddings_generated {
            ""
        } else {
            " [no embeddings]"
        },
    );
    println!("{}", fmt.format_index_summary(&msg, &stats.progress));
    Ok(())
}

async fn run_reindex_once(
    context: &WorktreeContext,
    state_root: &std::path::Path,
    indexing_config: &crate::shared::types::IndexingConfig,
    show_progress_ui: bool,
    progress_tx: Option<&pipeline::ProgressSender>,
) -> anyhow::Result<pipeline::PipelineStats> {
    let mut setup = SetupTimings::new(Instant::now());
    let mut setup_spinner = spin("Rebuilding database", show_progress_ui);

    // Single-writer rebuild lock: ALWAYS held across the staged build + the atomic
    // switch-over so exactly one process owns the rebuild (single-writer
    // guarantee) and the switch runs under the lock (HYP-001/HYP-002). `state_root`
    // is non-optional precisely so this lock can never be silently bypassed.
    // Released when this function returns (RAII).
    let _rebuild_lock = crate::daemon::lifecycle::acquire_rebuild_lock(state_root)?;

    // Build the refreshed index aside into a staging file rather than dropping the
    // live index in place: the served `index.db` keeps answering (stale-but-
    // available) throughout the rebuild and is replaced by a single atomic switch
    // only once the refreshed index is complete and valid. If this function returns
    // early (error/cancellation) before the switch, the guard drops and the prior
    // index is left intact and served.
    let db_start = Instant::now();
    let staged = StagingRebuild::open(state_root).await?;
    setup.db_prepare_ms = db_start.elapsed().as_millis();
    setup_spinner.success_with(format!("Rebuilt schema v{SCHEMA_VERSION}"));

    if let Some(progress_tx) = progress_tx {
        send_watch_progress(
            progress_tx,
            IndexPhase::Rebuilding,
            format!("Rebuilt schema v{SCHEMA_VERSION}"),
        );
        send_watch_progress(
            progress_tx,
            IndexPhase::LoadingModel,
            "Loading embedding model",
        );
    }

    let mut model_spinner = spin("Loading embedding model", show_progress_ui);

    let model_start = Instant::now();
    // REQ-002: `1up reindex` is an explicit, deliberate retry signal, so clear
    // any prior download-failure marker before the model prepare's
    // `is_download_failed()` guard runs. Passive search (cli/search.rs) never
    // does this, so it stays FTS-only until an explicit index/reindex clears
    // the marker.
    clear_download_failure();
    let mut runtime = EmbeddingRuntime::default();
    let status = runtime
        .prepare_for_indexing_with_progress(indexing_config.embed_threads, show_progress_ui)
        .await?;
    setup.model_prepare_ms = model_start.elapsed().as_millis();
    let status_message = model_status_message(&status);
    match &status {
        EmbeddingLoadStatus::Warm | EmbeddingLoadStatus::Loaded => model_spinner.success(),
        EmbeddingLoadStatus::Downloaded => model_spinner.success_with(status_message.clone()),
        EmbeddingLoadStatus::Unavailable(_) => model_spinner.warn_with(status_message.clone()),
    }

    if let Some(progress_tx) = progress_tx {
        send_watch_progress(progress_tx, IndexPhase::LoadingModel, status_message);
    }

    // One-shot CLI reindex: not subject to the daemon's SIGTERM drain, so it
    // runs under a fresh token that is never cancelled.
    let stats = pipeline::run_with_context_scope_setup_and_progress_root(
        staged.connection(),
        context,
        runtime.current_embedder(),
        &crate::shared::types::RunScope::Full,
        indexing_config,
        progress_tx.cloned(),
        show_progress_ui,
        Some(setup),
        None,
        Some(state_root),
        &tokio_util::sync::CancellationToken::new(),
    )
    .await?;

    // Switch the freshly-built staging index over the served `index.db` in a single
    // atomic rename, still under the held rebuild lock.
    staged.finalize_and_swap().await?;

    Ok(stats)
}
