use clap::Args;

use std::time::Instant;

use crate::cli::output::{formatter_for, spawn_index_watch_renderer};
use crate::daemon::registry::Registry;
use crate::indexer::embedder::{EmbeddingLoadStatus, EmbeddingRuntime, EmbeddingUnavailableReason};
use crate::indexer::pipeline;
use crate::shared::config;
use crate::shared::fs::ensure_secure_project_root;
use crate::shared::progress::{ProgressState, ProgressUi};
use crate::shared::types::{IndexPhase, IndexProgress, IndexState, OutputFormat, SetupTimings};
use crate::storage::db::Db;
use crate::storage::schema;

#[derive(Args)]
pub struct IndexArgs {
    /// Directory to index (defaults to current directory)
    #[arg(default_value = ".")]
    pub path: String,

    /// Maximum concurrent parse workers; overrides ONEUP_INDEX_JOBS
    #[arg(long, value_name = "N", value_parser = crate::cli::parse_positive_usize)]
    pub jobs: Option<usize>,

    /// ONNX intra-op threads; overrides ONEUP_EMBED_THREADS
    #[arg(long, value_name = "N", value_parser = crate::cli::parse_positive_usize)]
    pub embed_threads: Option<usize>,

    /// Stream live index progress updates until the run completes
    #[arg(long)]
    pub watch: bool,

    /// Output format override (defaults to plain)
    #[arg(long, short = 'f')]
    pub format: Option<OutputFormat>,
}

fn spin(msg: impl Into<String>, show_progress_ui: bool) -> ProgressUi {
    ProgressUi::stderr_if(ProgressState::spinner(msg), show_progress_ui)
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

async fn exec_watch(args: IndexArgs, format: OutputFormat) -> anyhow::Result<()> {
    let resolved = crate::shared::project::resolve_project_root(std::path::Path::new(&args.path))?;
    let project_root = resolved.state_root;
    let source_root = resolved.source_root;
    let db_path = config::project_db_path(&project_root);
    let fmt = formatter_for(format);
    let registry = Registry::load()?;
    let indexing_config = config::resolve_indexing_config(
        args.jobs,
        args.embed_threads,
        registry.indexing_config_for(&project_root),
    )?;

    ensure_secure_project_root(&project_root)?;

    if should_use_direct_watch_progress_ui(format) {
        let stats = run_index_once(
            &db_path,
            &source_root,
            Some(&project_root),
            &indexing_config,
            true,
            None,
        )
        .await?;

        let msg = format!(
            "Indexed {} files ({} segments). {} skipped, {} deleted.{}",
            stats.files_indexed,
            stats.segments_stored,
            stats.files_skipped,
            stats.files_deleted,
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
    send_watch_progress(&progress_tx, IndexPhase::Preparing, "Preparing database");

    let result = run_index_once(
        &db_path,
        &source_root,
        Some(&project_root),
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
            "Indexed {} files ({} segments). {} skipped, {} deleted.{}",
            stats.files_indexed,
            stats.segments_stored,
            stats.files_skipped,
            stats.files_deleted,
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

pub async fn exec(args: IndexArgs, format: OutputFormat) -> anyhow::Result<()> {
    if args.watch {
        return exec_watch(args, format).await;
    }

    let resolved = crate::shared::project::resolve_project_root(std::path::Path::new(&args.path))?;
    let project_root = resolved.state_root;
    let source_root = resolved.source_root;
    let db_path = config::project_db_path(&project_root);
    let fmt = formatter_for(format);
    let registry = Registry::load()?;
    let indexing_config = config::resolve_indexing_config(
        args.jobs,
        args.embed_threads,
        registry.indexing_config_for(&project_root),
    )?;
    let show_progress_ui = format == OutputFormat::Human;

    ensure_secure_project_root(&project_root)?;

    let stats = run_index_once(
        &db_path,
        &source_root,
        Some(&project_root),
        &indexing_config,
        show_progress_ui,
        None,
    )
    .await?;

    let msg = format!(
        "Indexed {} files ({} segments). {} skipped, {} deleted.{}",
        stats.files_indexed,
        stats.segments_stored,
        stats.files_skipped,
        stats.files_deleted,
        if stats.embeddings_generated {
            ""
        } else {
            " [no embeddings]"
        },
    );
    println!("{}", fmt.format_index_summary(&msg, &stats.progress));
    Ok(())
}

async fn run_index_once(
    db_path: &std::path::Path,
    project_root: &std::path::Path,
    state_root: Option<&std::path::Path>,
    indexing_config: &crate::shared::types::IndexingConfig,
    show_progress_ui: bool,
    progress_tx: Option<&pipeline::ProgressSender>,
) -> anyhow::Result<pipeline::PipelineStats> {
    let mut setup = SetupTimings::new(Instant::now());
    let mut setup_spinner = spin("Preparing database", show_progress_ui);

    let db_start = Instant::now();
    let db = Db::open_rw(db_path).await?;
    let conn = db.connect_tuned().await?;
    schema::prepare_for_write(&conn).await?;
    setup.db_prepare_ms = db_start.elapsed().as_millis();

    setup_spinner.success();

    if let Some(progress_tx) = progress_tx {
        send_watch_progress(
            progress_tx,
            IndexPhase::LoadingModel,
            "Loading embedding model",
        );
    }

    let mut model_spinner = spin("Loading embedding model", show_progress_ui);

    let model_start = Instant::now();
    let mut runtime = EmbeddingRuntime::default();
    let status = runtime
        .prepare_for_indexing_with_progress(indexing_config.embed_threads, show_progress_ui)
        .await;
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

    pipeline::run_with_scope_setup_and_progress_root(
        &conn,
        project_root,
        runtime.current_embedder(),
        &crate::shared::types::RunScope::Full,
        indexing_config,
        progress_tx.cloned(),
        show_progress_ui,
        Some(setup),
        None,
        state_root,
    )
    .await
    .map_err(Into::into)
}
