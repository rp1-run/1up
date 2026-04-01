use clap::Args;

use crate::cli::output::formatter_for;
use crate::daemon::lifecycle;
use crate::indexer::embedder::{is_model_available, Embedder};
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
        eprintln!(
            "warning: no index found at {}. Run `1up index` first.",
            db_path.display()
        );
        println!("{}", fmt.format_search_results(&[]));
        return Ok(());
    }

    let db = Db::open_ro(&db_path).await?;
    let conn = db.connect()?;
    schema::migrate(&conn).await?;

    let mut embedder_opt = if is_model_available() {
        match Embedder::from_dir(&crate::shared::config::model_dir()?) {
            Ok(e) => Some(e),
            Err(err) => {
                eprintln!("warning: embedding model unavailable ({err}), falling back to FTS-only");
                None
            }
        }
    } else {
        eprintln!("warning: embedding model not downloaded, falling back to FTS-only search");
        None
    };

    let results = match &mut embedder_opt {
        Some(embedder) => {
            let mut engine = HybridSearchEngine::new(&conn, Some(embedder));
            engine.search(&args.query, args.limit).await?
        }
        None => {
            let engine = HybridSearchEngine::new(&conn, None);
            engine.fts_only_search(&args.query, args.limit).await?
        }
    };

    println!("{}", fmt.format_search_results(&results));
    Ok(())
}
