use clap::Args;
use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

use crate::cli::lean;
use crate::daemon::{lifecycle, search_service};
use crate::indexer::embedder::{EmbeddingLoadStatus, EmbeddingRuntime, EmbeddingUnavailableReason};
use crate::search::HybridSearchEngine;
use crate::shared::config::project_db_path;
use crate::shared::constants::VERSION;
use crate::shared::project;
use crate::shared::types::SearchResult;
use crate::storage::db::Db;
use crate::storage::schema;

#[derive(Args)]
pub struct SearchArgs {
    /// Search query
    pub query: String,

    /// Maximum number of results
    #[arg(long, short = 'n', default_value = "3")]
    pub limit: usize,

    /// Project root directory (defaults to current directory)
    #[arg(long, default_value = ".")]
    pub path: String,
}

const DAEMON_SEARCH_TIMEOUT: Duration = Duration::from_millis(250);

pub async fn exec(args: SearchArgs) -> anyhow::Result<()> {
    let resolved = crate::shared::project::resolve_project_root(std::path::Path::new(&args.path))?;
    let project_root = resolved.state_root;
    let source_root = resolved.source_root;
    let db_path = project_db_path(&project_root);

    if let Ok(pid) = project::read_project_id(&project_root) {
        if let Err(e) = lifecycle::ensure_daemon(&pid, &project_root, &source_root) {
            tracing::debug!("auto-start daemon skipped: {e}");
        }
    }

    if let Some((results, daemon_version)) =
        try_daemon_search(&project_root, &args.query, args.limit).await
    {
        write_results(&results)?;
        if let Some(ref dv) = daemon_version {
            if dv != VERSION {
                eprintln!(
                    "warning: CLI version ({VERSION}) differs from daemon version ({dv}). Run `1up stop` and re-run your command to restart the daemon under the current binary."
                );
            }
        }
        return Ok(());
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

    write_results(&results)?;
    Ok(())
}

/// Emit lean search rows through a locked stdout handle so the renderer writes
/// once per call without buffering the entire result set into a `String`.
fn write_results(results: &[SearchResult]) -> anyhow::Result<()> {
    let mut stdout = io::stdout().lock();
    lean::render_search(&mut stdout, results)?;
    stdout.flush()?;
    Ok(())
}

async fn try_daemon_search(
    project_root: &Path,
    query: &str,
    limit: usize,
) -> Option<(Vec<SearchResult>, Option<String>)> {
    let result = tokio::time::timeout(
        DAEMON_SEARCH_TIMEOUT,
        search_service::request_search(project_root, query, limit),
    )
    .await;

    match result {
        Ok(Ok(Some(response))) => Some(response),
        Ok(Ok(None)) => {
            tracing::debug!("daemon search unavailable; falling back to local runtime");
            None
        }
        Ok(Err(err)) => {
            tracing::debug!("daemon search request failed; falling back to local runtime: {err}");
            None
        }
        Err(_) => {
            tracing::debug!("daemon search timed out; falling back to local runtime");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SearchArgs;
    use clap::Parser;

    #[derive(Parser)]
    struct TestCli {
        #[command(flatten)]
        args: SearchArgs,
    }

    #[test]
    fn search_default_limit_is_three() {
        let cli = TestCli::parse_from(["test", "needle"]);
        assert_eq!(cli.args.limit, 3);
    }

    #[test]
    fn search_limit_override_is_respected() {
        let cli = TestCli::parse_from(["test", "needle", "-n", "7"]);
        assert_eq!(cli.args.limit, 7);
    }
}
