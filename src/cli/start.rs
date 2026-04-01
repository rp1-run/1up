use clap::Args;

use crate::cli::output::formatter_for;
use crate::daemon::lifecycle;
use crate::daemon::registry::Registry;
use crate::indexer::embedder::{self, Embedder};
use crate::indexer::pipeline;
use crate::shared::config;
use crate::shared::project;
use crate::shared::types::OutputFormat;
use crate::storage::db::Db;
use crate::storage::schema;

#[derive(Args)]
pub struct StartArgs {
    /// Project root directory (defaults to current directory)
    #[arg(default_value = ".")]
    pub path: String,
}

pub async fn exec(args: StartArgs, format: OutputFormat) -> anyhow::Result<()> {
    let project_root = std::path::Path::new(&args.path).canonicalize()?;
    let fmt = formatter_for(format);

    if !project::is_initialized(&project_root) {
        anyhow::bail!(
            "Project not initialized at {}. Run `1up init` first.",
            project_root.display()
        );
    }

    let project_id = project::read_project_id(&project_root)?;

    if let Some(pid) = lifecycle::is_daemon_running()? {
        let mut registry = Registry::load()?;
        let already_registered = registry
            .projects
            .iter()
            .any(|p| p.project_root == project_root);

        if already_registered {
            let msg = format!("Daemon already running (pid={pid}) and project already registered.");
            eprintln!("{}", fmt.format_message(&msg));
            return Ok(());
        }

        registry.register(&project_id, &project_root)?;
        lifecycle::send_sighup(pid)?;
        let msg = format!(
            "Project registered. Daemon (pid={pid}) notified to watch {}.",
            project_root.display()
        );
        println!("{}", fmt.format_message(&msg));
        return Ok(());
    }

    let db_path = config::project_db_path(&project_root);
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

    let mut registry = Registry::load()?;
    registry.register(&project_id, &project_root)?;

    let binary = lifecycle::current_binary_path()?;
    let pid = lifecycle::spawn_daemon(&binary)?;

    let msg = format!(
        "Indexed {} files ({} segments). Daemon started (pid={pid}).",
        stats.files_indexed, stats.segments_stored,
    );
    println!("{}", fmt.format_message(&msg));
    Ok(())
}
