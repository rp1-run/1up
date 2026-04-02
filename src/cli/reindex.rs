use clap::Args;
use nanospinner::Spinner;

use crate::cli::output::formatter_for;
use crate::indexer::embedder::{self, Embedder};
use crate::indexer::pipeline;
use crate::shared::config;
use crate::shared::types::OutputFormat;
use crate::storage::db::Db;
use crate::storage::schema;

#[derive(Args)]
pub struct ReindexArgs {
    /// Directory to re-index (defaults to current directory)
    #[arg(default_value = ".")]
    pub path: String,
}

fn spin(msg: impl Into<String>) -> nanospinner::SpinnerHandle {
    use std::io::IsTerminal;
    Spinner::with_writer_tty(msg, std::io::stderr(), std::io::stderr().is_terminal()).start()
}

pub async fn exec(args: ReindexArgs, format: OutputFormat) -> anyhow::Result<()> {
    let project_root = std::path::Path::new(&args.path).canonicalize()?;
    let db_path = config::project_db_path(&project_root);
    let fmt = formatter_for(format);

    let setup_spinner = spin("Rebuilding database");

    let db = Db::open_rw(&db_path).await?;
    let conn = db.connect()?;
    schema::rebuild(&conn).await?;
    setup_spinner.success_with("Rebuilt schema v5");

    let model_spinner = spin("Loading embedding model");

    let mut embedder_opt = if embedder::is_model_available() {
        match Embedder::from_dir(&config::model_dir()?) {
            Ok(e) => {
                model_spinner.success();
                Some(e)
            }
            Err(err) => {
                model_spinner.warn_with(format!("Embedding model failed to load ({err})"));
                None
            }
        }
    } else if embedder::is_download_failed() {
        model_spinner.warn_with("Embedding model unavailable (previous download failed)");
        None
    } else {
        model_spinner.update("Downloading embedding model");
        match Embedder::new().await {
            Ok(e) => {
                model_spinner.success_with("Embedding model downloaded");
                Some(e)
            }
            Err(err) => {
                model_spinner.warn_with(format!("Model download failed ({err})"));
                None
            }
        }
    };

    let stats = pipeline::run(&conn, &project_root, embedder_opt.as_mut()).await?;

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
    println!("{}", fmt.format_message(&msg));
    Ok(())
}
