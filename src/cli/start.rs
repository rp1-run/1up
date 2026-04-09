use clap::Args;

use crate::cli::output::formatter_for;
use crate::daemon::lifecycle;
use crate::daemon::registry::Registry;
use crate::indexer::embedder::{EmbeddingLoadStatus, EmbeddingRuntime, EmbeddingUnavailableReason};
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

    /// Maximum concurrent parse workers; overrides ONEUP_INDEX_JOBS
    #[arg(long, value_name = "N", value_parser = crate::cli::parse_positive_usize)]
    pub jobs: Option<usize>,

    /// ONNX intra-op threads; overrides ONEUP_EMBED_THREADS
    #[arg(long, value_name = "N", value_parser = crate::cli::parse_positive_usize)]
    pub embed_threads: Option<usize>,
}

pub async fn exec(args: StartArgs, format: OutputFormat) -> anyhow::Result<()> {
    let project_root = std::path::Path::new(&args.path).canonicalize()?;
    let fmt = formatter_for(format);
    if !lifecycle::supports_daemon() {
        println!(
            "{}",
            fmt.format_message(
                "Background daemon workflows are not supported on this platform yet. Use `1up init` and `1up index`, then rerun `1up index` or `1up reindex` to refresh the local database."
            )
        );
        return Ok(());
    }

    let mut registry = Registry::load()?;
    let indexing_config = config::resolve_indexing_config(
        args.jobs,
        args.embed_threads,
        registry.indexing_config_for(&project_root),
    )?;

    let (project_id, initialized_now) = if project::is_initialized(&project_root) {
        (project::read_project_id(&project_root)?, false)
    } else {
        let id = project::write_project_id(&project_root)?;
        tracing::info!(
            "initialized project {} at {} during start",
            id,
            project_root.display()
        );
        (id, true)
    };
    let init_prefix = if initialized_now {
        format!("Initialized project {project_id}. ")
    } else {
        String::new()
    };

    if let Some(pid) = lifecycle::is_daemon_running()? {
        let already_registered = registry
            .projects
            .iter()
            .any(|p| p.project_root == project_root);

        registry.register(&project_id, &project_root, Some(indexing_config.clone()))?;
        lifecycle::send_sighup(pid)?;
        let msg = if already_registered {
            format!("{init_prefix}Daemon already running (pid={pid}); project settings refreshed.")
        } else {
            format!(
                "{init_prefix}Project registered. Daemon (pid={pid}) notified to watch {}.",
                project_root.display()
            )
        };
        if already_registered {
            eprintln!("{}", fmt.format_message(&msg));
        } else {
            println!("{}", fmt.format_message(&msg));
        }
        return Ok(());
    }

    let db_path = config::project_db_path(&project_root);
    let db = Db::open_rw(&db_path).await?;
    let conn = db.connect()?;
    schema::prepare_for_write(&conn).await?;

    let mut runtime = EmbeddingRuntime::default();
    let status = runtime
        .prepare_for_indexing(indexing_config.embed_threads)
        .await;
    match &status {
        EmbeddingLoadStatus::Warm | EmbeddingLoadStatus::Loaded => {}
        EmbeddingLoadStatus::Downloaded => {
            eprintln!("info: embedding model downloaded successfully");
        }
        EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::PreviousDownloadFailed) => {
            eprintln!("warning: embedding model download previously failed; indexing without embeddings (semantic search will be unavailable). Delete ~/.local/share/1up/models/all-MiniLM-L6-v2/.download_failed to retry");
        }
        EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::DownloadFailed(err)) => {
            eprintln!(
                "warning: embedding model download failed ({err}); indexing without embeddings (semantic search will be unavailable)"
            );
        }
        EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::ModelDirUnavailable(err))
        | EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::LoadFailed(err)) => {
            eprintln!(
                "warning: embedding model failed to load ({err}); indexing without embeddings (semantic search will be unavailable)"
            );
        }
        EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::ModelMissing) => {
            eprintln!(
                "warning: embedding model unavailable; indexing without embeddings (semantic search will be unavailable)"
            );
        }
    }

    let stats = pipeline::run_with_config(
        &conn,
        &project_root,
        runtime.current_embedder(),
        &indexing_config,
    )
    .await?;

    registry.register(&project_id, &project_root, Some(indexing_config))?;

    let binary = lifecycle::current_binary_path()?;
    let pid = lifecycle::spawn_daemon(&binary)?;

    let msg = format!(
        "{init_prefix}Indexed {} files ({} segments). Daemon started (pid={pid}).",
        stats.files_indexed, stats.segments_stored,
    );
    println!("{}", fmt.format_index_summary(&msg, &stats.progress));
    Ok(())
}
