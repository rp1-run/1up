use clap::Args;

use crate::cli::output::{formatter_for, spawn_index_watch_renderer};
use crate::daemon::registry::Registry;
use crate::indexer::embedder::{EmbeddingLoadStatus, EmbeddingRuntime, EmbeddingUnavailableReason};
use crate::indexer::pipeline;
use crate::shared::config;
use crate::shared::constants::SCHEMA_VERSION;
use crate::shared::progress::{ProgressState, ProgressUi};
use crate::shared::types::{IndexPhase, IndexProgress, IndexState, OutputFormat};
use crate::storage::db::Db;
use crate::storage::schema;

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

    /// Stream live reindex progress updates until the run completes
    #[arg(long)]
    pub watch: bool,
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

async fn exec_watch(args: ReindexArgs, format: OutputFormat) -> anyhow::Result<()> {
    let project_root = std::path::Path::new(&args.path).canonicalize()?;
    let db_path = config::project_db_path(&project_root);
    let fmt = formatter_for(format);
    let registry = Registry::load()?;
    let indexing_config = config::resolve_indexing_config(
        args.jobs,
        args.embed_threads,
        registry.indexing_config_for(&project_root),
    )?;

    if should_use_direct_watch_progress_ui(format) {
        let stats = run_reindex_once(&db_path, &project_root, &indexing_config, true, None).await?;

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
        &db_path,
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

    let project_root = std::path::Path::new(&args.path).canonicalize()?;
    let db_path = config::project_db_path(&project_root);
    let fmt = formatter_for(format);
    let registry = Registry::load()?;
    let indexing_config = config::resolve_indexing_config(
        args.jobs,
        args.embed_threads,
        registry.indexing_config_for(&project_root),
    )?;
    let show_progress_ui = format == OutputFormat::Human;
    let stats = run_reindex_once(
        &db_path,
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
    db_path: &std::path::Path,
    project_root: &std::path::Path,
    indexing_config: &crate::shared::types::IndexingConfig,
    show_progress_ui: bool,
    progress_tx: Option<&pipeline::ProgressSender>,
) -> anyhow::Result<pipeline::PipelineStats> {
    let mut setup_spinner = spin("Rebuilding database", show_progress_ui);

    let db = Db::open_rw(db_path).await?;
    let conn = db.connect_tuned().await?;
    schema::rebuild(&conn).await?;
    setup_spinner.success_with(&format!("Rebuilt schema v{SCHEMA_VERSION}"));

    if let Some(progress_tx) = progress_tx {
        send_watch_progress(
            progress_tx,
            IndexPhase::Rebuilding,
            &format!("Rebuilt schema v{SCHEMA_VERSION}"),
        );
        send_watch_progress(
            progress_tx,
            IndexPhase::LoadingModel,
            "Loading embedding model",
        );
    }

    let mut model_spinner = spin("Loading embedding model", show_progress_ui);

    let mut runtime = EmbeddingRuntime::default();
    let status = runtime
        .prepare_for_indexing_with_progress(indexing_config.embed_threads, show_progress_ui)
        .await;
    let status_message = model_status_message(&status);
    match &status {
        EmbeddingLoadStatus::Warm | EmbeddingLoadStatus::Loaded => model_spinner.success(),
        EmbeddingLoadStatus::Downloaded => model_spinner.success_with(status_message.clone()),
        EmbeddingLoadStatus::Unavailable(_) => model_spinner.warn_with(status_message.clone()),
    }

    if let Some(progress_tx) = progress_tx {
        send_watch_progress(progress_tx, IndexPhase::LoadingModel, status_message);
    }

    pipeline::run_with_config_and_progress_ui(
        &conn,
        project_root,
        runtime.current_embedder(),
        indexing_config,
        progress_tx.cloned(),
        show_progress_ui,
    )
    .await
    .map_err(Into::into)
}
