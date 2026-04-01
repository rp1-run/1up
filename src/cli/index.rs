use clap::Args;

use crate::cli::output::formatter_for;
use crate::indexer::embedder::{self, Embedder};
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
}

pub async fn exec(args: IndexArgs, format: OutputFormat) -> anyhow::Result<()> {
    let project_root = std::path::Path::new(&args.path).canonicalize()?;
    let db_path = config::project_db_path(&project_root);
    let fmt = formatter_for(format);

    let dot_dir = config::project_dot_dir(&project_root);
    if !dot_dir.exists() {
        std::fs::create_dir_all(&dot_dir)?;
    }

    let db = Db::open_rw(&db_path).await?;
    let conn = db.connect()?;
    schema::migrate(&conn).await?;

    let mut embedder_opt = if embedder::is_model_available() {
        match Embedder::from_dir(&config::model_dir()?) {
            Ok(e) => Some(e),
            Err(err) => {
                eprintln!(
                    "warning: embedding model failed to load ({err}); indexing without embeddings (semantic search will be unavailable)"
                );
                None
            }
        }
    } else if embedder::is_download_failed() {
        eprintln!("warning: embedding model download previously failed; indexing without embeddings (semantic search will be unavailable). Delete ~/.local/share/1up/models/all-MiniLM-L6-v2/.download_failed to retry");
        None
    } else {
        eprintln!("info: embedding model not found, attempting download...");
        match Embedder::new().await {
            Ok(e) => {
                eprintln!("info: embedding model downloaded successfully");
                Some(e)
            }
            Err(err) => {
                eprintln!(
                    "warning: embedding model download failed ({err}); indexing without embeddings (semantic search will be unavailable)"
                );
                None
            }
        }
    };

    let stats = pipeline::run(&conn, &project_root, embedder_opt.as_mut()).await?;

    let msg = format!(
        "Indexed {} files ({} segments). {} skipped, {} deleted.{}",
        stats.files_indexed,
        stats.segments_stored,
        stats.files_skipped,
        stats.files_deleted,
        if stats.embeddings_generated {
            ""
        } else {
            " [no embeddings -- semantic search unavailable]"
        },
    );
    println!("{}", fmt.format_message(&msg));
    Ok(())
}
