use clap::Args;
use nanospinner::Spinner;

use crate::cli::output::formatter_for;
use crate::daemon::registry::Registry;
use crate::indexer::embedder::{EmbeddingLoadStatus, EmbeddingRuntime, EmbeddingUnavailableReason};
use crate::indexer::pipeline;
use crate::shared::config;
use crate::shared::types::OutputFormat;
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
}

fn spin(msg: impl Into<String>) -> nanospinner::SpinnerHandle {
    use std::io::IsTerminal;
    Spinner::with_writer_tty(msg, std::io::stderr(), std::io::stderr().is_terminal()).start()
}

pub async fn exec(args: IndexArgs, format: OutputFormat) -> anyhow::Result<()> {
    let project_root = std::path::Path::new(&args.path).canonicalize()?;
    let db_path = config::project_db_path(&project_root);
    let fmt = formatter_for(format);
    let registry = Registry::load()?;
    let indexing_config = config::resolve_indexing_config(
        args.jobs,
        args.embed_threads,
        registry.indexing_config_for(&project_root),
    )?;

    let dot_dir = config::project_dot_dir(&project_root);
    if !dot_dir.exists() {
        std::fs::create_dir_all(&dot_dir)?;
    }

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
    match &status {
        EmbeddingLoadStatus::Warm | EmbeddingLoadStatus::Loaded => {
            model_spinner.success();
        }
        EmbeddingLoadStatus::Downloaded => {
            model_spinner.success_with("Embedding model downloaded");
        }
        EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::PreviousDownloadFailed) => {
            model_spinner.warn_with("Embedding model unavailable (previous download failed)");
        }
        EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::DownloadFailed(err)) => {
            model_spinner.warn_with(format!("Model download failed ({err})"));
        }
        EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::ModelDirUnavailable(err))
        | EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::LoadFailed(err)) => {
            model_spinner.warn_with(format!("Embedding model failed to load ({err})"));
        }
        EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::ModelMissing) => {
            model_spinner.warn_with("Embedding model unavailable");
        }
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
