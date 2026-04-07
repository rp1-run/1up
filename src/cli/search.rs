use clap::Args;

use crate::cli::output::formatter_for;
use crate::daemon::lifecycle;
use crate::indexer::embedder::{EmbeddingLoadStatus, EmbeddingRuntime, EmbeddingUnavailableReason};
use crate::search::HybridSearchEngine;
use crate::shared::config::project_db_path;
use crate::shared::project;
use crate::shared::types::OutputFormat;
use crate::storage::db::Db;
use crate::storage::schema;

#[derive(Args)]
pub struct SearchArgs {
    /// Search query
    pub query: String,

    /// Maximum number of results
    #[arg(long, short = 'n', default_value = "20")]
    pub limit: usize,

    /// Project root directory (defaults to current directory)
    #[arg(long, default_value = ".")]
    pub path: String,
}

pub async fn exec(args: SearchArgs, format: OutputFormat) -> anyhow::Result<()> {
    let project_root = std::path::Path::new(&args.path).canonicalize()?;
    let db_path = project_db_path(&project_root);
    let fmt = formatter_for(format);

    if let Ok(pid) = project::read_project_id(&project_root) {
        if let Err(e) = lifecycle::ensure_daemon(&pid, &project_root) {
            tracing::debug!("auto-start daemon skipped: {e}");
        }
    }

    if !db_path.exists() {
        anyhow::bail!(
            "no current index found at {}. Run `1up reindex` to create a fresh schema-v5 index.",
            db_path.display()
        );
    }

    let db = Db::open_ro(&db_path).await?;
    let conn = db.connect()?;
    schema::ensure_current(&conn).await?;

    let mut runtime = EmbeddingRuntime::default();
    let status = runtime.prepare_for_search(1);
    match &status {
        EmbeddingLoadStatus::Warm | EmbeddingLoadStatus::Loaded => {}
        EmbeddingLoadStatus::Downloaded => {
            tracing::debug!("search runtime loaded a fresh embedder via download path");
        }
        EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::PreviousDownloadFailed) => {
            eprintln!("warning: embedding model download previously failed; search is degraded to FTS-only mode. Delete ~/.local/share/1up/models/all-MiniLM-L6-v2/.download_failed to retry");
        }
        EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::ModelMissing) => {
            eprintln!("warning: embedding model not found; search is degraded to FTS-only mode. Run `1up index` to download the model and enable semantic search");
        }
        EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::ModelDirUnavailable(err))
        | EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::LoadFailed(err))
        | EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::DownloadFailed(err)) => {
            eprintln!(
                "warning: embedding model failed to load ({err}); search is degraded to FTS-only mode (results may be less relevant)"
            );
        }
    }

    let results = if status.is_available() {
        let mut engine = HybridSearchEngine::new(&conn, runtime.current_embedder());
        engine.search(&args.query, args.limit).await?
    } else {
        let engine = HybridSearchEngine::new(&conn, None);
        engine.fts_only_search(&args.query, args.limit).await?
    };

    println!("{}", fmt.format_search_results(&results));
    Ok(())
}
