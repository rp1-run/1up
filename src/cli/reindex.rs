use clap::Args;
use zenity::spinner::{Frames, MultiSpinner};

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

    let spinner = MultiSpinner::new();
    spinner.clear(None);
    let sp = spinner.add(Frames::default());
    spinner.set_text(&sp, " Preparing database".to_string());
    spinner.run_all();

    let db = Db::open_rw(&db_path).await?;
    let conn = db.connect()?;
    schema::migrate(&conn).await?;

    let all_paths = segments::get_all_file_paths(&conn).await?;
    spinner.set_text(&sp, format!(" Clearing {} files from index", all_paths.len()));
    for path in &all_paths {
        segments::delete_segments_by_file(&conn, path).await?;
    }

    spinner.set_text(&sp, " Loading embedding model".to_string());

    let mut embedder_opt = if embedder::is_model_available() {
        match Embedder::from_dir(&config::model_dir()?) {
            Ok(e) => Some(e),
            Err(err) => {
                spinner.set_text(&sp, format!(" Embedding model failed to load ({err})"));
                None
            }
        }
    } else if embedder::is_download_failed() {
        spinner.set_text(&sp, " Embedding model unavailable".to_string());
        None
    } else {
        spinner.set_text(&sp, " Downloading embedding model".to_string());
        match Embedder::new().await {
            Ok(e) => Some(e),
            Err(err) => {
                spinner.set_text(&sp, format!(" Model download failed ({err})"));
                None
            }
        }
    };

    spinner.stop(&sp);
    drop(spinner);

    let stats = pipeline::run(&conn, &project_root, embedder_opt.as_mut()).await?;

    let msg = format!(
        "Re-indexed {} files ({} segments). {} deleted.{}",
        stats.files_indexed,
        stats.segments_stored,
        all_paths.len(),
        if stats.embeddings_generated {
            ""
        } else {
            " [no embeddings]"
        },
    );
    println!("{}", fmt.format_message(&msg));
    Ok(())
}
