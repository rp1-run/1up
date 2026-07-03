use anyhow::Context;
use clap::Args;
use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

use crate::cli::lean;
use crate::daemon::{lifecycle, search_service};
use crate::indexer::embedder::{
    download_failure_marker_hint, EmbeddingLoadStatus, EmbeddingRuntime, EmbeddingUnavailableReason,
};
use crate::search::{retrieval, HybridSearchEngine, SearchScope};
use crate::shared::config::project_db_path;
use crate::shared::constants::{NO_INDEXED_EMBEDDINGS_REASON, VERSION};
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

    /// Repo-relative directory prefix to constrain results to (e.g. "src/foo")
    #[arg(long)]
    pub path_prefix: Option<String>,
}

const DAEMON_SEARCH_TIMEOUT: Duration = Duration::from_millis(250);

pub async fn exec(args: SearchArgs) -> anyhow::Result<()> {
    let resolved = crate::shared::project::resolve_project_root(std::path::Path::new(&args.path))?;
    let project_root = resolved.state_root;
    let source_root = resolved.source_root;
    let mut search_scope = SearchScope::from_worktree_context(&resolved.worktree_context);
    if let Some(prefix) = args.path_prefix.as_deref() {
        search_scope = search_scope.with_path_prefix(prefix);
    }
    let path_prefix = search_scope.path_prefix().map(str::to_string);
    let db_path = project_db_path(&project_root);

    warn_if_degraded_branch_context(&search_scope);

    if let Ok(pid) = project::read_project_id(&project_root) {
        if let Err(e) = lifecycle::ensure_daemon(&pid, &project_root, &source_root) {
            tracing::debug!("auto-start daemon skipped: {e}");
        }
    }

    if let Some((results, daemon_version, degraded_reason)) = try_daemon_search(
        &project_root,
        &source_root,
        search_scope.context_id(),
        &args.query,
        args.limit,
        path_prefix.as_deref(),
    )
    .await
    {
        // REQ-001: classify by version BEFORE writing. A daemon still running the
        // previous binary stamps a mismatched `daemon_version`; its results must
        // never be served as authoritative, so the version check now gates the
        // write instead of trailing a soft warning after results were already
        // emitted (the headline write-then-warn bug).
        if daemon_response_is_authoritative(daemon_version.as_deref()) {
            serve_daemon_results(&results, degraded_reason)?;
            return Ok(());
        }

        // REQ-002: refuse the stale result, then drain the old daemon and restart
        // a fresh one under the current binary. On a detected mismatch this
        // always drains and restarts: per the recorded gating decision
        // (`DAEMON_AUTO_RESTART_GATING_ENABLED = false`, REQ-004/OQ-003) there is
        // no idle/size gating. The specific idle/size thresholds are an open
        // owner decision (OQ-003); a future owner introduces the gate here
        // without re-deriving the rationale.
        let stale_version = daemon_version.as_deref().unwrap_or("unknown");
        eprintln!(
            "warning: daemon is running a stale binary version ({stale_version}); CLI is ({VERSION}). Draining the stale daemon and restarting under the current binary."
        );

        match drain_and_restart_stale_daemon(&project_root, &source_root) {
            Ok(()) => {
                // Re-attempt against the fresh daemon and serve only if it now
                // reports a matching version. If it is not yet ready (or somehow
                // still mismatched), fall through to the local search below.
                if let Some((results, daemon_version, degraded_reason)) = try_daemon_search(
                    &project_root,
                    &source_root,
                    search_scope.context_id(),
                    &args.query,
                    args.limit,
                    path_prefix.as_deref(),
                )
                .await
                {
                    if daemon_response_is_authoritative(daemon_version.as_deref()) {
                        serve_daemon_results(&results, degraded_reason)?;
                        return Ok(());
                    }
                }
            }
            Err(err) => {
                // Drain exceeded its bound or the restart failed: surface the
                // actionable guidance (the drain-timeout error already says to
                // run `1up stop` then retry) and fall back to local search.
                eprintln!("warning: {err}");
            }
        }
        // Fall through to the local in-process search below: a version mismatch
        // is served from the current binary locally, never from the stale daemon.
    }

    if !db_path.exists() {
        anyhow::bail!(
            "no current index found at {}. Run `1up reindex` to create a fresh index.",
            db_path.display()
        );
    }

    let db = Db::open_ro(&db_path).await?;
    let conn = db.connect()?;
    schema::ensure_current(&conn, &schema::SchemaContext::new(&db_path, &source_root)).await?;

    let has_vectors = retrieval::has_indexed_embeddings(&conn, &search_scope).await?;
    let results = if has_vectors {
        let mut runtime = EmbeddingRuntime::default();
        let status = runtime.prepare_for_search(1)?;
        match &status {
            EmbeddingLoadStatus::Warm | EmbeddingLoadStatus::Loaded => {}
            EmbeddingLoadStatus::Downloaded => {
                tracing::debug!("search runtime loaded a fresh embedder via download path");
            }
            EmbeddingLoadStatus::Unavailable(
                EmbeddingUnavailableReason::PreviousDownloadFailed,
            ) => {
                // REQ-002: passive search never clears the marker (no network
                // hammering); it only prints the runtime-resolved recovery path.
                eprintln!(
                    "warning: embedding model download previously failed; search is degraded to FTS-only mode{}",
                    download_failure_marker_hint()
                );
            }
            EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::ModelMissing) => {
                eprintln!("warning: embedding model not found; search is degraded to FTS-only mode. Run `1up index` to download the model and enable semantic search");
            }
            EmbeddingLoadStatus::Unavailable(
                EmbeddingUnavailableReason::ArtifactsUnverifiable(err),
            ) => {
                eprintln!(
                    "warning: embedding model artifacts failed verification ({err}); search is degraded to FTS-only mode. Run `1up index` to re-download the model"
                );
            }
            EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::ModelDirUnavailable(
                err,
            ))
            | EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::LoadFailed(err))
            | EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::DownloadFailed(err)) => {
                eprintln!(
                    "warning: embedding model failed to load ({err}); search is degraded to FTS-only mode (results may be less relevant)"
                );
            }
        }

        if status.is_available() {
            let mut engine = HybridSearchEngine::new_scoped(
                &conn,
                runtime.current_embedder(),
                search_scope.clone(),
            )
            .with_has_vectors(has_vectors);
            engine.search(&args.query, args.limit).await?
        } else {
            let engine = HybridSearchEngine::new_scoped(&conn, None, search_scope.clone());
            engine.fts_only_search(&args.query, args.limit).await?
        }
    } else {
        eprintln!("warning: {NO_INDEXED_EMBEDDINGS_REASON}");
        let engine = HybridSearchEngine::new_scoped(&conn, None, search_scope.clone());
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

/// Writes authoritative daemon results to stdout and emits any degraded-search
/// notice to stderr, keeping the machine-readable result stream on stdout clean.
fn serve_daemon_results(
    results: &[SearchResult],
    degraded_reason: Option<String>,
) -> anyhow::Result<()> {
    write_results(results)?;
    if let Some(reason) = degraded_reason {
        eprintln!("warning: {reason}");
    }
    Ok(())
}

/// Whether a daemon search response may be served as authoritative.
///
/// A response whose `daemon_version` matches the running binary is authoritative.
/// A *known* mismatch is refused (REQ-001) so results produced by a daemon still
/// running the previous binary are never served; an absent version is treated as
/// authoritative to preserve the established no-version-info behavior.
fn daemon_response_is_authoritative(daemon_version: Option<&str>) -> bool {
    daemon_version.is_none_or(|dv| dv == VERSION)
}

/// Refuses a stale-binary daemon (REQ-002): drains the running daemon then
/// restarts a fresh one under the current binary so the retried search is served
/// by a matching-version daemon. Returns the actionable error on a drain timeout
/// or a restart failure so the caller surfaces it and falls back to local search
/// rather than serving stale results.
fn drain_and_restart_stale_daemon(project_root: &Path, source_root: &Path) -> anyhow::Result<()> {
    let project_id = project::read_project_id(project_root)
        .context("read project id while restarting the stale daemon")?;
    match lifecycle::is_daemon_running()? {
        Some(pid) => {
            lifecycle::drain_and_restart_daemon(pid, &project_id, project_root, source_root)?;
        }
        None => {
            // The stale daemon exited between the search and now; spawn a fresh
            // one under the current binary instead of draining a dead pid.
            lifecycle::ensure_daemon(&project_id, project_root, source_root)?;
        }
    }
    Ok(())
}

async fn try_daemon_search(
    project_root: &Path,
    source_root: &Path,
    context_id: &str,
    query: &str,
    limit: usize,
    path_prefix: Option<&str>,
) -> Option<(Vec<SearchResult>, Option<String>, Option<String>)> {
    let result = tokio::time::timeout(
        DAEMON_SEARCH_TIMEOUT,
        search_service::request_search(
            project_root,
            source_root,
            context_id,
            query,
            limit,
            path_prefix,
        ),
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

fn warn_if_degraded_branch_context(scope: &SearchScope) {
    if let Some(reason) = scope.degraded_reason() {
        eprintln!("warning: {reason}");
    }
}

#[cfg(test)]
mod tests {
    use super::{daemon_response_is_authoritative, SearchArgs};
    use crate::shared::constants::VERSION;
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

    /// REQ-001: a daemon response whose `daemon_version` differs from the running
    /// binary must be refused (non-authoritative) so the caller takes the
    /// drain/restart path instead of writing stale-binary results. A matching or
    /// absent version stays authoritative. This is the inverted-ordering guard;
    /// before the fix there was no version gate and stale results were written.
    #[test]
    fn daemon_response_authority_is_gated_by_version() {
        // Known mismatch -> refused, so the write is skipped and the mismatch
        // (drain+restart) path is taken.
        assert!(!daemon_response_is_authoritative(Some(
            "0.0.0-stale-binary"
        )));
        // Same binary -> authoritative, results may be served.
        assert!(daemon_response_is_authoritative(Some(VERSION)));
        // No version reported -> authoritative (preserves prior behavior; we
        // only refuse on a *known* mismatch).
        assert!(daemon_response_is_authoritative(None));
    }
}
