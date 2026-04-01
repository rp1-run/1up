use clap::Args;

use crate::cli::output::formatter_for;
use crate::indexer::embedder::{self, Embedder};
use crate::indexer::pipeline;
use crate::shared::config;
use crate::shared::types::OutputFormat;
use crate::storage::db::Db;
use crate::storage::schema;
use crate::storage::segments;

#[derive(Args)]
pub struct ReindexArgs {
    /// Directory to re-index (defaults to current directory)
    #[arg(default_value = ".")]
    pub path: String,
}

pub async fn exec(args: ReindexArgs, format: OutputFormat) -> anyhow::Result<()> {
    let project_root = std::path::Path::new(&args.path).canonicalize()?;
    let db_path = config::project_db_path(&project_root);
    let fmt = formatter_for(format);

    if !db_path.exists() {
        anyhow::bail!(
            "No index found at {}. Run `1up index` first.",
            db_path.display()
        );
    }

    let db = Db::open_rw(&db_path).await?;
    let conn = db.connect()?;
    schema::migrate(&conn).await?;

    let all_paths = segments::get_all_file_paths(&conn).await?;
    for path in &all_paths {
        segments::delete_segments_by_file(&conn, path).await?;
    }

    let mut embedder_opt = if embedder::is_model_available() {
        match Embedder::from_dir(&config::model_dir()?) {
            Ok(e) => Some(e),
            Err(err) => {
                eprintln!(
                    "warning: embedding model failed to load ({err}); re-indexing without embeddings (semantic search will be unavailable)"
                );
                None
            }
        }
    } else if embedder::is_download_failed() {
        eprintln!("warning: embedding model download previously failed; re-indexing without embeddings (semantic search will be unavailable). Delete ~/.local/share/1up/models/all-MiniLM-L6-v2/.download_failed to retry");
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
                    "warning: embedding model download failed ({err}); re-indexing without embeddings (semantic search will be unavailable)"
                );
                None
            }
        }
    };

    let stats = pipeline::run(&conn, &project_root, embedder_opt.as_mut()).await?;

    let msg = format!(
        "Re-indexed {} files ({} segments). {} deleted.{}",
        stats.files_indexed,
        stats.segments_stored,
        all_paths.len(),
        if stats.embeddings_generated {
            ""
        } else {
            " [no embeddings -- semantic search unavailable]"
        },
    );
    println!("{}", fmt.format_message(&msg));
    Ok(())
}
