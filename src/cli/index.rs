use clap::Args;
use nanospinner::Spinner;

use crate::cli::output::{formatter_for, spawn_index_watch_renderer};
use crate::daemon::registry::Registry;
use crate::indexer::embedder::{EmbeddingLoadStatus, EmbeddingRuntime, EmbeddingUnavailableReason};
use crate::indexer::pipeline;
use crate::shared::config;
use crate::shared::fs::ensure_secure_project_root;
use crate::shared::types::{IndexPhase, IndexProgress, IndexState, OutputFormat};
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
}

fn spin(msg: impl Into<String>) -> nanospinner::SpinnerHandle {
    use std::io::IsTerminal;
    Spinner::with_writer_tty(msg, std::io::stderr(), std::io::stderr().is_terminal()).start()
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
    let project_root = std::path::Path::new(&args.path).canonicalize()?;
    let db_path = config::project_db_path(&project_root);
    let fmt = formatter_for(format);
    let registry = Registry::load()?;
    let indexing_config = config::resolve_indexing_config(
        args.jobs,
        args.embed_threads,
        registry.indexing_config_for(&project_root),
    )?;

    ensure_secure_project_root(&project_root)?;

    let (progress_tx, progress_handle) = spawn_index_watch_renderer(format);
    send_watch_progress(&progress_tx, IndexPhase::Preparing, "Preparing database");

    let result = async {
        let db = Db::open_rw(&db_path).await?;
        let conn = db.connect()?;
        schema::prepare_for_write(&conn).await?;

        send_watch_progress(
            &progress_tx,
            IndexPhase::LoadingModel,
            "Loading embedding model",
        );

        let mut runtime = EmbeddingRuntime::default();
        let status = runtime
            .prepare_for_indexing(indexing_config.embed_threads)
            .await;
        send_watch_progress(
            &progress_tx,
            IndexPhase::LoadingModel,
            model_status_message(&status),
        );

        pipeline::run_with_config_and_progress(
            &conn,
            &project_root,
            runtime.current_embedder(),
            &indexing_config,
            Some(progress_tx.clone()),
        )
        .await
    }
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

    let project_root = std::path::Path::new(&args.path).canonicalize()?;
    let db_path = config::project_db_path(&project_root);
    let fmt = formatter_for(format);
    let registry = Registry::load()?;
    let indexing_config = config::resolve_indexing_config(
        args.jobs,
        args.embed_threads,
        registry.indexing_config_for(&project_root),
    )?;

    ensure_secure_project_root(&project_root)?;

    let setup_spinner = spin("Preparing database");

    let db = Db::open_rw(&db_path).await?;
    let conn = db.connect()?;
    schema::prepare_for_write(&conn).await?;

    setup_spinner.success();

    let model_spinner = spin("Loading embedding model");

    let mut runtime = EmbeddingRuntime::default();
    let status = runtime
        .prepare_for_indexing(indexing_config.embed_threads)
        .await;
    let status_message = model_status_message(&status);
    match &status {
        EmbeddingLoadStatus::Warm | EmbeddingLoadStatus::Loaded => model_spinner.success(),
        EmbeddingLoadStatus::Downloaded => model_spinner.success_with(status_message),
        EmbeddingLoadStatus::Unavailable(_) => model_spinner.warn_with(status_message),
    }

    let stats = pipeline::run_with_config(
        &conn,
        &project_root,
        runtime.current_embedder(),
        &indexing_config,
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
