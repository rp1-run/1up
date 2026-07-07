use std::collections::{HashMap, HashSet};
use std::future::{self, Future};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use libsql::Connection;
use tokio::net::UnixStream;
use tokio::signal::unix::{signal, Signal, SignalKind};
use tokio::sync::{mpsc, oneshot, OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::cli::project_status_files::{prune_daemon_context_status, read_index_progress};
use crate::daemon::lifecycle;
use crate::daemon::registry::{ProjectEntry, Registry};
use crate::daemon::search_service::{self, SearchRequest, SearchResponse};
use crate::daemon::watcher::{self, FileWatcher};
use crate::indexer::embedder::{EmbeddingLoadStatus, EmbeddingRuntime, EmbeddingUnavailableReason};
use crate::indexer::pipeline;
use crate::search::{retrieval, HybridSearchEngine, SearchScope};
use crate::shared::config;
use crate::shared::constants::{
    DAEMON_FILE_CHECK_PERSIST_INTERVAL_MS, DAEMON_IDLE_SHUTDOWN_ENV_VAR, DAEMON_IDLE_SHUTDOWN_SECS,
    MAX_DAEMON_IN_FLIGHT_REQUESTS, PROJECT_STATE_DIR_MODE, SECURE_STATE_FILE_MODE,
    STALE_REBUILD_REASON, VERSION, WATCHER_DEBOUNCE_MS,
};
use crate::shared::errors::OneupError;
use crate::shared::fs::{atomic_replace, ensure_secure_project_root};
use crate::shared::project::canonical_project_root;
use crate::shared::types::WorktreeContext;
use crate::shared::types::{
    combine_degraded_reasons, DaemonContextStatus, DaemonContextStatusFile, DaemonProjectStatus,
    DaemonRefreshState, DaemonWatchStatus, IndexingConfig, RunScope, SetupTimings,
};
use crate::storage::segments::{self, IndexedContextRow};
use crate::storage::{db::Db, schema};

const DAEMON_CONTEXT_STATUS_FILE_NAME: &str = "daemon_context_status.json";
const STARTUP_RECONCILIATION_REASON: &str = "startup_reconciliation";

#[derive(Debug, Default)]
struct ProjectRunState {
    running: bool,
    dirty: bool,
    pending_scope: Option<RunScope>,
    pending_fallback_reason: Option<String>,
}

impl ProjectRunState {
    fn mark_dirty(&mut self, scope: RunScope) {
        match self.pending_scope.as_mut() {
            Some(existing) => existing.merge(scope),
            None => self.pending_scope = Some(scope),
        }

        self.dirty = true;
    }

    fn mark_dirty_with_reason(&mut self, scope: RunScope, reason: String) {
        self.mark_dirty(scope);
        self.pending_fallback_reason = Some(reason);
    }

    fn start_run(&mut self) -> RunScope {
        debug_assert!(self.dirty, "only dirty projects should start a run");
        self.running = true;
        self.dirty = false;
        self.pending_scope
            .take()
            .expect("dirty project must have a pending scope")
    }

    fn finish_run(&mut self) {
        self.running = false;
    }
}

struct ProjectState {
    project_root: PathBuf,
    source_root: PathBuf,
    context: WorktreeContext,
    db: Db,
    /// Identity (device, inode) of the `index.db` file backing `db` at the time
    /// `db` was opened. A one-shot rebuild swaps the index via an atomic rename
    /// onto a fresh inode (T2), so a mismatch between this and the current
    /// on-disk identity means the daemon's long-lived handle now points at the
    /// orphaned pre-swap inode and must be reopened before any pass touches it
    /// (HYP-002). `None` when the index was absent when `db` was opened.
    index_identity: Option<IndexFileIdentity>,
    /// Cached per-context vector `COUNT(*)` for the open index (R-007), populated
    /// lazily on the first search that needs it and reused across requests so the
    /// hot path skips a per-query `COUNT(*)`. MUST be invalidated (`None`) on
    /// `reopen_if_index_swapped` so a build-aside swap never serves a stale count
    /// into `vector_search_path_for_corpus` (exhaustive-vs-ANN) path selection.
    cached_vector_count: Option<usize>,
    /// One reused tuned read [`Connection`] to the open index, serving repeated
    /// daemon searches without a fresh per-request `connect()` + PRAGMA pass
    /// (R-008). Created lazily on the first search and dropped (`None`) whenever
    /// the backing `db` is reopened on a build-aside swap
    /// (`reopen_if_index_swapped`), so a reused connection can never outlive the
    /// inode it was opened against and serve through an orphaned pre-swap handle.
    /// `libsql` caches prepared statements on the connection, so reusing it also
    /// reuses the prepared statements for the repeated search queries.
    read_conn: Option<Connection>,
    indexing: Option<IndexingConfig>,
    embedding_runtime: EmbeddingRuntime,
    run_state: ProjectRunState,
    watch_status: DaemonWatchStatus,
    last_refresh_state: DaemonRefreshState,
    last_refresh_started_at: Option<DateTime<Utc>>,
    last_refresh_completed_at: Option<DateTime<Utc>>,
    last_refresh_error: Option<String>,
    last_file_check_persisted_at: Option<DateTime<Utc>>,
}

/// On-disk identity of an `index.db` file, used to detect a build-aside swap
/// performed by another process (the one-shot rebuild owners — T4). On Unix the
/// atomic rename that switches the index over (T2) replaces the directory entry
/// with a different inode, so `(device, inode)` changes exactly when the file the
/// daemon's open handle refers to has been orphaned. `worker.rs` is compiled only
/// on Unix (`mod.rs` routes non-Unix to `worker_stub.rs`), so the Unix-specific
/// `MetadataExt` is always available here.
type IndexFileIdentity = (u64, u64);

struct QueuedSearchRequest {
    request: SearchRequest,
    respond_to: oneshot::Sender<SearchResponse>,
}

type ProjectStates = HashMap<String, ProjectState>;

/// Idle-shutdown grace as a `Duration`, honouring the runtime override
/// [`DAEMON_IDLE_SHUTDOWN_ENV_VAR`] and otherwise [`DAEMON_IDLE_SHUTDOWN_SECS`].
fn daemon_idle_shutdown_timeout() -> std::time::Duration {
    let secs = std::env::var(DAEMON_IDLE_SHUTDOWN_ENV_VAR)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .unwrap_or(DAEMON_IDLE_SHUTDOWN_SECS);
    std::time::Duration::from_secs(secs)
}

/// Whether an empty daemon has been idle long enough to self-exit.
///
/// A daemon that still has a registered project (`is_empty == false`) never
/// idles out. An empty one exits once it has been continuously empty for at
/// least `idle_timeout`, so a daemon left behind by `1up stop` (last project
/// deregistered) or orphaned by a crashed/ended parent reaps itself instead of
/// lingering until SIGTERM.
fn should_idle_shutdown(
    is_empty: bool,
    empty_for: Option<std::time::Duration>,
    idle_timeout: std::time::Duration,
) -> bool {
    is_empty && empty_for.is_some_and(|elapsed| elapsed >= idle_timeout)
}

/// REQ-011: Set up parent-death signaling to ensure workers don't outlive their parent.
/// On Unix platforms (Linux/macOS), this installs signal handlers or OS mechanisms
/// that will terminate the worker if its parent process dies.
/// This prevents orphaned __worker processes from consuming resources.
#[allow(dead_code)]
fn setup_parent_death_signal() {
    #[cfg(target_os = "linux")]
    {
        use nix::sys::prctl::{self, PrctlOption};

        // On Linux, use prctl(PR_SET_PDEATHSIG) to receive SIGTERM when parent dies
        match prctl::prctl(PrctlOption::SetPdeathsig(nix::sys::signal::Signal::SIGTERM)) {
            Ok(_) => debug!("parent-death signaling configured: SIGTERM on parent exit"),
            Err(e) => warn!("failed to set parent-death signal: {e}"),
        }
    }

    #[cfg(target_os = "macos")]
    {
        use nix::sys::signal::{signal, SigHandler, Signal};
        use nix::unistd;

        // On macOS, install a SIGTERM handler and spawn a monitor thread
        // to detect when parent dies (parent PID becomes 1 / init)
        let _parent_pid = unistd::getppid();

        // Register SIGTERM handler
        extern "C" fn handle_sigterm(_sig: i32) {
            std::process::exit(1);
        }

        unsafe {
            let _ = signal(Signal::SIGTERM, SigHandler::Handler(handle_sigterm));
        }

        // Spawn monitor thread to check if parent is still alive
        std::thread::spawn(|| {
            loop {
                std::thread::sleep(Duration::from_secs(1));

                // If parent PID becomes 1 (init), parent is dead
                let current_ppid = unistd::getppid().as_raw() as u32;
                if current_ppid == 1 {
                    std::process::exit(1);
                }
            }
        });

        debug!("parent-death signaling configured: monitor thread on macOS");
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        debug!("parent-death signaling not supported on this platform");
    }
}

pub async fn run() -> Result<(), OneupError> {
    let _daemon_lock = lifecycle::acquire_daemon_lock()?;

    run_inner().await
}

async fn run_inner() -> Result<(), OneupError> {
    info!("daemon worker starting (pid={})", std::process::id());

    // REQ-011: Set up parent-death signaling to prevent orphaned worker processes
    setup_parent_death_signal();

    let mut sighup = signal(SignalKind::hangup()).map_err(|e| {
        crate::shared::errors::DaemonError::SignalError(format!("SIGHUP handler: {e}"))
    })?;
    let mut sigterm = signal(SignalKind::terminate()).map_err(|e| {
        crate::shared::errors::DaemonError::SignalError(format!("SIGTERM handler: {e}"))
    })?;

    // A single cooperative-cancellation token shared by every dirty-run pass.
    // SIGTERM cancels it so an in-flight indexing pass stops at its next safe
    // unit boundary (see `pipeline`), making the bounded SIGTERM drain genuinely
    // interrupt indexing instead of waiting for the whole pass to finish.
    let cancel_token = CancellationToken::new();

    let mut file_watcher = FileWatcher::new()?;
    let mut projects: ProjectStates = HashMap::new();
    let request_limit = Arc::new(Semaphore::new(MAX_DAEMON_IN_FLIGHT_REQUESTS));
    let (search_requests_tx, mut search_requests_rx) =
        mpsc::channel::<QueuedSearchRequest>(MAX_DAEMON_IN_FLIGHT_REQUESTS);
    let search_listener = match search_service::bind_listener().await {
        Ok(listener) => Some(listener),
        Err(err) => {
            warn!("failed to start daemon search socket; search will fall back locally: {err}");
            None
        }
    };

    load_and_watch_projects(&mut file_watcher, &mut projects).await?;
    prewarm_project_embedders(&mut projects);
    record_file_check_for_all_projects(&mut projects, Utc::now(), true);
    run_dirty_projects_until_clean_or_cancelled(
        &file_watcher,
        &mut projects,
        &cancel_token,
        &mut sigterm,
        &mut search_requests_rx,
    )
    .await;

    let debounce = std::time::Duration::from_millis(WATCHER_DEBOUNCE_MS);
    let idle_timeout = daemon_idle_shutdown_timeout();
    // Reap an empty daemon: one with zero registered projects self-exits past
    // `idle_timeout` instead of lingering until SIGTERM, so a daemon left
    // behind by `1up stop` (last project deregistered) or orphaned by a
    // crashed/ended parent does not accumulate. Registration precedes daemon
    // spawn, so a fresh daemon loads a non-empty registry and never idles out
    // at startup.
    let mut empty_since: Option<std::time::Instant> =
        projects.is_empty().then(std::time::Instant::now);

    while !cancel_token.is_cancelled() {
        tokio::select! {
            request = async {
                match search_listener.as_ref() {
                    Some(listener) => Some(search_service::accept_connection(listener).await),
                    None => future::pending::<Option<Result<_, OneupError>>>().await,
                }
            } => {
                if let Some(request) = request {
                    match request {
                        Ok(Some(mut stream)) => {
                            let permit = match acquire_request_permit(&request_limit, &mut stream).await {
                                Ok(Some(permit)) => permit,
                                Ok(None) => continue,
                                Err(err) => {
                                    warn!("failed to respond to saturated daemon search request: {err}");
                                    continue;
                                }
                            };
                            let search_requests_tx = search_requests_tx.clone();
                            tokio::spawn(async move {
                                if let Err(err) = serve_search_connection(stream, permit, search_requests_tx).await {
                                    warn!("failed to serve daemon search connection: {err}");
                                }
                            });
                        }
                        Ok(None) => {}
                        Err(err) => {
                            warn!("failed to accept daemon search request: {err}");
                        }
                    }
                }
            }
            queued_request = search_requests_rx.recv() => {
                if let Some(queued_request) = queued_request {
                    let response = handle_search_request(&mut projects, queued_request.request).await;
                    let _ = queued_request.respond_to.send(response);
                } else if search_listener.is_some() {
                    warn!("daemon search request queue closed unexpectedly");
                }
            }
            _ = sighup.recv() => {
                info!("received SIGHUP, reloading project registry");
                if let Err(e) = reload_projects(&mut file_watcher, &mut projects).await {
                    error!("failed to reload projects: {e}");
                } else {
                    record_file_check_for_all_projects(&mut projects, Utc::now(), true);
                    run_dirty_projects_until_clean_or_cancelled(
                        &file_watcher,
                        &mut projects,
                        &cancel_token,
                        &mut sigterm,
                        &mut search_requests_rx,
                    )
                    .await;
                }
            }
            _ = sigterm.recv() => {
                info!("received SIGTERM, shutting down");
                // No pass is in flight on this arm; cancel so the loop guard and
                // any future pass observe the shutdown, then break.
                cancel_token.cancel();
                break;
            }
            _ = tokio::time::sleep(debounce) => {
                let is_empty = projects.is_empty();
                let empty_for = if is_empty {
                    Some(empty_since.get_or_insert_with(std::time::Instant::now).elapsed())
                } else {
                    empty_since = None;
                    None
                };
                if should_idle_shutdown(is_empty, empty_for, idle_timeout) {
                    info!(
                        "daemon has had no registered projects for >= {}s; self-exiting",
                        idle_timeout.as_secs()
                    );
                    cancel_token.cancel();
                    break;
                }
                let filtered =
                    watcher::filter_changed_paths(&file_watcher, file_watcher.drain_events());
                record_file_check_for_all_projects(&mut projects, Utc::now(), false);
                mark_branch_context_changes(&mut file_watcher, &mut projects);
                if filtered.is_empty() {
                    run_dirty_projects_until_clean_or_cancelled(
                        &file_watcher,
                        &mut projects,
                        &cancel_token,
                        &mut sigterm,
                        &mut search_requests_rx,
                    )
                    .await;
                    continue;
                }

                debug!(
                    "detected {} changed files and {} ambiguous paths",
                    filtered.file_paths.len(),
                    filtered.ambiguous_paths.len()
                );
                mark_changed_projects(&mut projects, &filtered);
                run_dirty_projects_until_clean_or_cancelled(
                    &file_watcher,
                    &mut projects,
                    &cancel_token,
                    &mut sigterm,
                    &mut search_requests_rx,
                )
                .await;
            }
        }
    }

    mark_all_contexts_daemon_stopped(&mut projects);

    if let Err(e) = file_watcher.unwatch_all() {
        warn!("failed to unwatch on shutdown: {e}");
    }
    if search_listener.is_some() {
        if let Err(err) = search_service::cleanup_socket_file() {
            warn!("failed to remove daemon search socket: {err}");
        }
    }

    info!("daemon worker exiting");
    Ok(())
}

async fn acquire_request_permit(
    request_limit: &Arc<Semaphore>,
    stream: &mut UnixStream,
) -> Result<Option<OwnedSemaphorePermit>, OneupError> {
    match request_limit.clone().try_acquire_owned() {
        Ok(permit) => Ok(Some(permit)),
        Err(_) => {
            search_service::send_busy_response(stream).await?;
            Ok(None)
        }
    }
}

async fn serve_search_connection(
    mut stream: UnixStream,
    _permit: OwnedSemaphorePermit,
    search_requests: mpsc::Sender<QueuedSearchRequest>,
) -> Result<(), OneupError> {
    let request = match search_service::read_request(&mut stream).await {
        Ok(request) => request,
        Err(err) => {
            debug!("rejecting daemon search request: {err}");
            let _ = search_service::send_unavailable_response(&mut stream).await;
            return Ok(());
        }
    };

    let (respond_to, response_rx) = oneshot::channel();
    if search_requests
        .send(QueuedSearchRequest {
            request,
            respond_to,
        })
        .await
        .is_err()
    {
        let _ = search_service::send_unavailable_response(&mut stream).await;
        return Ok(());
    }

    let response = match response_rx.await {
        Ok(response) => response,
        Err(_) => search_service::unavailable_response(),
    };

    search_service::send_response(&mut stream, &response).await
}

fn log_indexing_embedding_status(
    project_root: &Path,
    embed_threads: usize,
    status: &EmbeddingLoadStatus,
) {
    match status {
        EmbeddingLoadStatus::Warm => {
            debug!(
                "reused warm embedding runtime for {} (embed_threads={embed_threads})",
                project_root.display()
            );
        }
        EmbeddingLoadStatus::Loaded => {
            debug!(
                "loaded embedding model for {} (embed_threads={embed_threads})",
                project_root.display()
            );
        }
        EmbeddingLoadStatus::Downloaded => {
            info!(
                "downloaded embedding model for {} (embed_threads={embed_threads})",
                project_root.display()
            );
        }
        EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::PreviousDownloadFailed) => {
            warn!(
                "embedding model download previously failed; daemon will index {} without embeddings",
                project_root.display()
            );
        }
        EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::ModelMissing) => {
            debug!(
                "embedding model not available; daemon will index {} without embeddings",
                project_root.display()
            );
        }
        EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::ModelDirUnavailable(err))
        | EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::LoadFailed(err))
        | EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::DownloadFailed(err))
        | EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::ArtifactsUnverifiable(
            err,
        )) => {
            warn!(
                "failed to prepare embedding runtime for {} with embed_threads={embed_threads}: {err}; daemon will index without embeddings",
                project_root.display()
            );
        }
    }
}

fn log_search_embedding_status(
    project_root: &Path,
    embed_threads: usize,
    status: &EmbeddingLoadStatus,
) {
    match status {
        EmbeddingLoadStatus::Warm => {
            debug!(
                "reused warm daemon search runtime for {} (embed_threads={embed_threads})",
                project_root.display()
            );
        }
        EmbeddingLoadStatus::Loaded | EmbeddingLoadStatus::Downloaded => {
            debug!(
                "loaded daemon search runtime for {} (embed_threads={embed_threads})",
                project_root.display()
            );
        }
        EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::PreviousDownloadFailed) => {
            debug!(
                "embedding model download previously failed; daemon search for {} will use FTS-only mode",
                project_root.display()
            );
        }
        EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::ModelMissing) => {
            debug!(
                "embedding model not available; daemon search for {} will use FTS-only mode",
                project_root.display()
            );
        }
        EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::ModelDirUnavailable(err))
        | EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::LoadFailed(err))
        | EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::DownloadFailed(err))
        | EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::ArtifactsUnverifiable(
            err,
        )) => {
            debug!(
                "failed to prepare daemon search runtime for {} with embed_threads={embed_threads}: {err}; using FTS-only mode",
                project_root.display()
            );
        }
    }
}

async fn load_and_watch_projects(
    watcher: &mut FileWatcher,
    projects: &mut ProjectStates,
) -> Result<(), OneupError> {
    let registry = Registry::load()?;

    // Prune dead-worktree contexts (source directory gone) before building per-
    // context state and watching, so they are neither re-indexed nor re-watched.
    // Best-effort and non-blocking — it can never fail or block startup (see the
    // routine). It deregisters every context it prunes, so reload the registry
    // afterwards and watch only the survivors; contexts it did *not* prune (lock
    // contended, no index DB, or a deregister hiccup) remain and still flow through
    // `build_project_state`'s existing `SourceMissing` handling. Fall back to the
    // pre-prune snapshot if the reload itself hiccups so this introduces no new
    // startup failure.
    prune_source_missing_contexts_on_startup(&registry).await;
    let registry = Registry::load().unwrap_or(registry);

    for entry in &registry.projects {
        let Some(mut state) = build_project_state(entry).await? else {
            continue;
        };

        let source_root = state.source_root.clone();
        mark_startup_reconciliation_pending(&mut state);
        watcher.watch(&source_root)?;
        let context_id = state.context.context_id.clone();
        projects.insert(context_id.clone(), state);

        info!(
            "watching project: {} (context {}, source {})",
            entry.project_root.display(),
            context_id,
            source_root.display()
        );
    }

    Ok(())
}

/// Prewarm every loaded project's embedding runtime immediately after
/// [`load_and_watch_projects`] so the first real search finds a `Warm` runtime
/// instead of paying a cold model load past the daemon's search deadline
/// (REQ-004 D). Mirrors the search path's own `prepare_for_search` call
/// (`handle_search_request`) rather than the indexing path's
/// `prepare_for_indexing`, since prewarming is not itself an indexing pass and
/// must not trigger a model download — a project with no model available yet
/// simply stays `Unavailable` here exactly as it would on an unprewarmed first
/// search, degrading to FTS-only rather than blocking startup.
///
/// Best-effort and per-project: a resolution or load failure for one project
/// is logged and skipped, never propagated, so one misconfigured project can
/// never block another project's prewarm or daemon startup.
fn prewarm_project_embedders(projects: &mut ProjectStates) {
    for state in projects.values_mut() {
        let indexing_config = match config::resolve_indexing_config(
            None,
            None,
            state.indexing.as_ref(),
        ) {
            Ok(indexing_config) => indexing_config,
            Err(err) => {
                warn!(
                        "failed to resolve indexing configuration while prewarming embedder for {}: {err}",
                        state.project_root.display()
                    );
                continue;
            }
        };
        match state
            .embedding_runtime
            .prepare_for_search(indexing_config.embed_threads)
        {
            Ok(status) => {
                log_search_embedding_status(
                    &state.project_root,
                    indexing_config.embed_threads,
                    &status,
                );
            }
            Err(err) => {
                warn!(
                    "failed to prewarm embedding runtime for {}: {err}",
                    state.project_root.display()
                );
            }
        }
    }
}

/// Select the recorded contexts whose source worktree directory no longer exists.
///
/// Pure and injected with `source_exists` so it is deterministic and
/// unit-testable (the daemon passes `|p| p.exists()`). This mirrors the
/// source-missing arm of `cli::gc::prune_reason`, but deliberately selects on
/// *source-root absence alone*: unlike `1up gc`, the daemon's startup prune never
/// touches stale-branch snapshots of a still-present worktree — those rebuild on
/// demand and stay a manual decision. A context whose `source_root` still exists
/// is therefore always retained, including a same-`state_root`, other-branch
/// snapshot that shares a live worktree.
pub fn source_missing_context_ids(
    contexts: &[IndexedContextRow],
    source_exists: &dyn Fn(&Path) -> bool,
) -> Vec<String> {
    contexts
        .iter()
        .filter(|ctx| !source_exists(&ctx.source_root))
        .map(|ctx| ctx.context_id.clone())
        .collect()
}

/// Best-effort startup prune of contexts whose source worktree directory has been
/// removed (e.g. a deleted git worktree), so a dead context's rows do not linger
/// in the shared index until a manual `1up gc`.
///
/// Scope is deliberately the *source-missing* subset only (via
/// [`source_missing_context_ids`]) — never stale-branch snapshots of a live
/// worktree, which stay a manual decision. The safety boundaries are all enforced
/// here: the single-writer rebuild lock is taken **non-blocking** per index DB (a
/// contended or un-openable DB is skipped this cycle, never waited on), **no
/// `VACUUM`** runs on startup (deletes free pages for reuse without the exclusive
/// compaction), and every step is best-effort so any error is logged and swallowed
/// — a prune failure can never block or fail daemon startup. Registered entries
/// are grouped by their shared `index.db` path, since linked worktrees share one
/// index keyed by the main worktree's state root.
async fn prune_source_missing_contexts_on_startup(registry: &Registry) {
    // Linked worktrees share one `.1up/index.db` (keyed by the main worktree's
    // state root), so visit each distinct index DB once. The first entry seen for
    // a given DB path carries the state root used for its lock and bookkeeping.
    let mut seen_dbs: HashSet<PathBuf> = HashSet::new();
    let mut state_roots: Vec<PathBuf> = Vec::new();
    for entry in &registry.projects {
        if seen_dbs.insert(config::project_db_path(&entry.project_root)) {
            state_roots.push(entry.project_root.clone());
        }
    }

    for state_root in state_roots {
        let pruned = prune_source_missing_contexts_for_state_root(&state_root).await;
        if pruned.is_empty() {
            continue;
        }

        // Best-effort bookkeeping so pruned contexts neither linger in the daemon
        // status snapshot nor get re-watched. The index rows are already gone; a
        // hiccup here is a warning, never a startup failure (mirrors `1up gc`).
        // `deregister_context_ids` reloads the registry fresh so it does not clobber
        // a concurrent registration.
        let pruned_ids: HashSet<String> = pruned.iter().cloned().collect();
        if let Err(err) = prune_daemon_context_status(&state_root, &pruned_ids) {
            warn!(
                "failed to prune daemon status for source-missing contexts at {}: {err}",
                state_root.display()
            );
        }
        if let Err(err) = Registry::load().and_then(|mut r| r.deregister_context_ids(&pruned_ids)) {
            warn!(
                "failed to deregister source-missing contexts at {}: {err}",
                state_root.display()
            );
        }
        info!(
            "pruned {} source-missing context(s) from {} on startup: {}",
            pruned.len(),
            state_root.display(),
            pruned.join(", ")
        );
    }
}

/// Prune source-missing contexts from a single shared index DB, returning the ids
/// that were deleted.
///
/// The non-blocking rebuild lock is held only across the read + delete and dropped
/// when this function returns — before the caller does its registry/status
/// bookkeeping — so the exclusive hold is bounded to the mutation. Returns an empty
/// vec (never an error) when the DB is absent, the lock is contended, nothing is
/// source-missing, or any step fails: every failure is logged and swallowed so the
/// caller's startup path is never broken.
async fn prune_source_missing_contexts_for_state_root(state_root: &Path) -> Vec<String> {
    let db_path = config::project_db_path(state_root);
    // A registered entry without an on-disk index yet has nothing to prune.
    if !db_path.exists() {
        return Vec::new();
    }

    // Non-blocking: a contended lock means a one-shot rebuild or another writer is
    // active, so skip this DB this cycle rather than stalling startup. The guard
    // releases on drop / `?` unwind, bounding the hold to this scope.
    let _rebuild_lock = match lifecycle::try_acquire_rebuild_lock(state_root) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            debug!(
                "skipping source-missing prune for {}: rebuild lock held by another process",
                state_root.display()
            );
            return Vec::new();
        }
        Err(err) => {
            warn!(
                "skipping source-missing prune for {}: {err}",
                state_root.display()
            );
            return Vec::new();
        }
    };

    // Open RW, list contexts, and delete only the source-missing subset. NO
    // `vacuum_database` here: startup never runs the exclusive compaction, so the
    // deletes free pages for reuse without competing with the live index.
    let pruned = async {
        let db = Db::open_rw(&db_path).await?;
        let conn = db.connect_tuned().await?;
        let contexts = segments::list_worktree_contexts(&conn).await?;
        let pruned = source_missing_context_ids(&contexts, &|p: &Path| p.exists());
        for context_id in &pruned {
            segments::delete_context(&conn, context_id).await?;
        }
        Ok::<_, OneupError>(pruned)
    }
    .await;

    match pruned {
        Ok(pruned) => pruned,
        Err(err) => {
            warn!(
                "failed to prune source-missing contexts from {}: {err}",
                db_path.display()
            );
            Vec::new()
        }
    }
}

async fn reload_projects(
    watcher: &mut FileWatcher,
    projects: &mut ProjectStates,
) -> Result<(), OneupError> {
    let registry = Registry::load()?;
    let registered_contexts: HashSet<String> = registry
        .projects
        .iter()
        .map(ProjectEntry::context_id)
        .collect();
    let registered_sources: HashSet<PathBuf> = registry
        .projects
        .iter()
        .map(|entry| canonical_project_root(entry.source_root()))
        .collect();

    let current_contexts: Vec<String> = projects.keys().cloned().collect();
    for context_id in &current_contexts {
        if !registered_contexts.contains(context_id) {
            if let Some(mut state) = projects.remove(context_id) {
                info!(
                    "removing project context {} for {}",
                    context_id,
                    state.project_root.display()
                );
                state.watch_status = DaemonWatchStatus::DaemonStopped;
                persist_daemon_context_status_for_state(&state);
                if !registered_sources.contains(&canonical_project_root(&state.source_root)) {
                    watcher.unwatch(&state.source_root)?;
                }
            }
        }
    }

    for entry in &registry.projects {
        let context_id = entry.context_id();
        if let Some(existing) = projects.get_mut(&context_id) {
            let entry_context = context_from_entry(entry);
            let branch_changed = branch_context_changed(&existing.context, &entry_context);
            if branch_changed {
                existing.context = entry_context;
                mark_refresh_pending(
                    existing,
                    RunScope::Full,
                    Some("branch_context_changed".to_string()),
                );
                info!(
                    "queued full context refresh for {} ({}) after branch context changed",
                    existing.project_root.display(),
                    context_id
                );
            }
            if existing.indexing != entry.indexing {
                existing.indexing = entry.indexing.clone();
                info!(
                    "refreshed indexing settings for {}",
                    entry.project_root.display()
                );
            }
            if !branch_changed {
                mark_startup_reconciliation_pending(existing);
                info!(
                    "queued startup reconciliation for {} ({})",
                    existing.project_root.display(),
                    context_id
                );
            }
            continue;
        }

        let Some(mut state) = build_project_state(entry).await? else {
            continue;
        };

        let entry_source_root = state.source_root.clone();
        let context_id = state.context.context_id.clone();
        mark_startup_reconciliation_pending(&mut state);
        watcher.watch(&entry_source_root)?;
        projects.insert(context_id.clone(), state);

        info!(
            "now watching project: {} (context {}, source {})",
            entry.project_root.display(),
            context_id,
            entry_source_root.display()
        );
    }

    Ok(())
}

fn mark_startup_reconciliation_pending(state: &mut ProjectState) {
    mark_refresh_pending(
        state,
        RunScope::Full,
        Some(STARTUP_RECONCILIATION_REASON.to_string()),
    );
}

async fn build_project_state(entry: &ProjectEntry) -> Result<Option<ProjectState>, OneupError> {
    if !entry.project_root.exists() {
        warn!(
            "skipping non-existent project: {}",
            entry.project_root.display()
        );
        return Ok(None);
    }
    let source_root = entry.source_root().to_path_buf();
    if !source_root.exists() {
        warn!(
            "skipping project {} because source root is missing: {}",
            entry.project_root.display(),
            source_root.display()
        );
        persist_source_missing_context_status(entry);
        return Ok(None);
    }

    let db_path = config::project_db_path(&entry.project_root);
    let db = Db::open_rw(&db_path).await?;
    let conn = db.connect_tuned().await?;
    if let Err(e) = schema::prepare_for_write(&conn).await {
        warn!(
            "skipping project {} until a clean rebuild succeeds: {e}",
            entry.project_root.display()
        );
        return Ok(None);
    }
    // Record the inode the handle now refers to so a later build-aside swap (a
    // one-shot rebuild atomically renaming a fresh index over `index.db`) is
    // detectable and the handle gets reopened before it writes (HYP-002).
    let index_identity = index_file_identity(&db_path);

    Ok(Some(ProjectState {
        project_root: entry.project_root.clone(),
        source_root,
        context: context_from_entry(entry),
        db,
        index_identity,
        cached_vector_count: None,
        read_conn: None,
        indexing: entry.indexing.clone(),
        embedding_runtime: EmbeddingRuntime::default(),
        run_state: ProjectRunState::default(),
        watch_status: DaemonWatchStatus::Watching,
        last_refresh_state: DaemonRefreshState::Unknown,
        last_refresh_started_at: None,
        last_refresh_completed_at: None,
        last_refresh_error: None,
        last_file_check_persisted_at: None,
    }))
}

/// On-disk `(device, inode)` identity of an `index.db` file, or `None` when the
/// file is absent or cannot be stat'd. Two opens of the same path yield the same
/// identity until an atomic rename swaps a different file over it, which is
/// exactly how the build-aside switch-over (T2) installs a refreshed index.
fn index_file_identity(index_path: &Path) -> Option<IndexFileIdentity> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(index_path)
        .ok()
        .map(|meta| (meta.dev(), meta.ino()))
}

/// Reopen the daemon's long-lived `ProjectState.db` if a build-aside rebuild has
/// swapped `index.db` onto a fresh inode since the handle was opened.
///
/// The daemon is the sole cross-process holder of a long-lived RW handle to
/// `index.db`. A one-shot rebuild (CLI `reindex` / MCP `run_index` — T4) builds a
/// refreshed index aside and atomically renames it over `index.db` (T2), which
/// orphans the inode the daemon's handle still refers to. Continuing to use that
/// stale handle is a data-divergence hazard, not merely a stale read: a write
/// through it lands in the now-unlinked old inode and is silently lost (HYP-002).
///
/// This compares the recorded open-time identity against the current on-disk
/// identity. On a match (the common case — no swap) it is a cheap no-op that
/// keeps the warm handle. On a mismatch it closes the stale handle, reopens
/// `index.db` (picking up the new inode), and re-validates the schema via
/// `prepare_for_write` before the caller proceeds — so no refresh pass or search
/// ever runs against a pre-swap handle. Because the switch-over is a single
/// atomic rename, the file is always a complete index; reopening therefore lands
/// on a fully-built, finalized generation, never a partial one.
///
/// Closing and reopening rather than holding the handle open across the rename is
/// also what keeps the swap unblocked on Windows, where a rename cannot replace a
/// file that another handle holds open (`ERROR_SHARING_VIOLATION`): the daemon
/// never keeps the orphaned handle once it observes the swap.
async fn reopen_if_index_swapped(state: &mut ProjectState) -> Result<(), OneupError> {
    let db_path = config::project_db_path(&state.project_root);
    let current_identity = index_file_identity(&db_path);

    // No swap: identities match (or the index is still absent on both sides).
    // Keep the warm handle untouched so steady-state passes never thrash it.
    if current_identity == state.index_identity {
        return Ok(());
    }

    info!(
        "index for {} was swapped underneath the daemon; reopening handle to adopt the refreshed index",
        state.project_root.display()
    );

    let db = Db::open_rw(&db_path).await?;
    let conn = db.connect_tuned().await?;
    // Re-validate the freshly-renamed index before adopting it, so a swap that
    // produced an unreadable/incompatible index fails loud here instead of
    // surfacing later as a confusing write/search failure.
    schema::prepare_for_write(&conn).await?;
    drop(conn);

    state.db = db;
    state.index_identity = current_identity;
    // Drop the reused read connection: it (and its cached prepared statements)
    // was bound to the pre-swap `db`/inode, so it must be reopened against the
    // freshly-adopted index before the next search (R-008). The next search
    // lazily re-creates it via `connect_tuned`, which re-runs the read PRAGMA
    // profile on the new handle.
    state.read_conn = None;
    // The swapped-in index has its own vector population; drop the cached count
    // so the next search recomputes it against the refreshed index (R-007). A
    // stale count here could flip `vector_search_path_for_corpus` between the
    // exhaustive scan and the ANN path and silently change served candidates.
    state.cached_vector_count = None;
    Ok(())
}

/// Return the daemon's reused tuned read connection for `state`, creating it on
/// first use (R-008).
///
/// Repeated daemon searches share one [`Connection`] (and its libSQL prepared-
/// statement cache) instead of opening a fresh connection and re-running the read
/// PRAGMA profile per request. The connection is bound to the currently-open
/// `state.db`; `reopen_if_index_swapped` drops it (`read_conn = None`) on a
/// build-aside swap, so the next call here re-creates it against the adopted
/// index — a reused connection can never outlive the inode it was opened against.
///
/// The returned value is a clone of the cached handle. `libsql::Connection` is an
/// `Arc`-backed handle, so the clone shares the same underlying connection and
/// statement cache; cloning only lets the caller use the connection without
/// holding a borrow on `state` across the later `&mut state` accesses (cached
/// count, embedding runtime).
async fn ensure_read_conn(state: &mut ProjectState) -> Result<Connection, OneupError> {
    if state.read_conn.is_none() {
        state.read_conn = Some(state.db.connect_tuned().await?);
    }
    // Safe: just populated above when absent.
    Ok(state
        .read_conn
        .as_ref()
        .expect("read_conn populated")
        .clone())
}

fn context_from_entry(entry: &ProjectEntry) -> WorktreeContext {
    WorktreeContext {
        context_id: entry.context_id(),
        state_root: entry.project_root.clone(),
        source_root: entry.source_root().to_path_buf(),
        main_worktree_root: entry.main_worktree_root().to_path_buf(),
        worktree_role: entry.worktree_role(),
        git_dir: None,
        common_git_dir: None,
        branch_name: entry.branch_name.clone(),
        branch_ref: entry.branch_ref.clone(),
        head_oid: entry.head_oid.clone(),
        branch_status: entry.branch_status(),
    }
}

fn current_context_for_state(state: &ProjectState) -> WorktreeContext {
    crate::daemon::registry::registration_context(&state.project_root, &state.source_root)
}

fn branch_context_changed(old: &WorktreeContext, new: &WorktreeContext) -> bool {
    old.context_id != new.context_id
        || old.branch_ref != new.branch_ref
        || old.head_oid != new.head_oid
        || old.branch_status != new.branch_status
        || canonical_project_root(&old.source_root) != canonical_project_root(&new.source_root)
}

fn record_file_check_for_all_projects(
    projects: &mut ProjectStates,
    checked_at: DateTime<Utc>,
    force: bool,
) {
    for state in projects.values_mut() {
        record_file_check(state, checked_at, force);
    }
}

fn record_file_check(state: &mut ProjectState, checked_at: DateTime<Utc>, force: bool) {
    if !force
        && state
            .last_file_check_persisted_at
            .is_some_and(|last_persisted_at| {
                checked_at
                    .signed_duration_since(last_persisted_at)
                    .num_milliseconds()
                    < DAEMON_FILE_CHECK_PERSIST_INTERVAL_MS as i64
            })
    {
        return;
    }

    let status = DaemonProjectStatus {
        last_file_check_at: checked_at,
    };
    persist_daemon_project_status(&state.project_root, &status);
    state.last_file_check_persisted_at = Some(checked_at);
    persist_daemon_context_status_for_state(state);
}

fn persist_daemon_project_status(project_root: &Path, status: &DaemonProjectStatus) {
    let secure_root = match ensure_secure_project_root(project_root) {
        Ok(root) => root,
        Err(err) => {
            debug!(
                "failed to prepare secure project root for daemon heartbeat {}: {err}",
                project_root.display()
            );
            return;
        }
    };

    let payload = match serde_json::to_vec_pretty(status) {
        Ok(payload) => payload,
        Err(err) => {
            debug!(
                "failed to serialize daemon heartbeat for {}: {err}",
                project_root.display()
            );
            return;
        }
    };

    let path = config::project_daemon_status_path(project_root);
    if let Err(err) = atomic_replace(
        &path,
        &payload,
        &secure_root,
        PROJECT_STATE_DIR_MODE,
        SECURE_STATE_FILE_MODE,
    ) {
        debug!(
            "failed to persist daemon heartbeat for {}: {err}",
            project_root.display()
        );
    }
}

fn daemon_context_status_path(project_root: &Path) -> PathBuf {
    config::project_dot_dir(project_root).join(DAEMON_CONTEXT_STATUS_FILE_NAME)
}

fn context_status_for_state(state: &ProjectState) -> DaemonContextStatus {
    DaemonContextStatus {
        context_id: state.context.context_id.clone(),
        source_root: Some(state.source_root.clone()),
        watch_status: state.watch_status,
        last_file_check_at: state.last_file_check_persisted_at,
        last_refresh_state: state.last_refresh_state,
        last_refresh_started_at: state.last_refresh_started_at,
        last_refresh_completed_at: state.last_refresh_completed_at,
        last_refresh_error: state.last_refresh_error.clone(),
        branch_name: state.context.branch_name.clone(),
        branch_status: state.context.branch_status,
    }
}

fn context_status_for_entry(
    entry: &ProjectEntry,
    watch_status: DaemonWatchStatus,
) -> DaemonContextStatus {
    let context = context_from_entry(entry);
    DaemonContextStatus {
        context_id: context.context_id.clone(),
        source_root: Some(context.source_root.clone()),
        watch_status,
        last_file_check_at: None,
        last_refresh_state: DaemonRefreshState::Unknown,
        last_refresh_started_at: None,
        last_refresh_completed_at: None,
        last_refresh_error: None,
        branch_name: context.branch_name,
        branch_status: context.branch_status,
    }
}

fn persist_source_missing_context_status(entry: &ProjectEntry) {
    let status = context_status_for_entry(entry, DaemonWatchStatus::SourceMissing);
    persist_daemon_context_status(&entry.project_root, &status);
}

fn persist_daemon_context_status_for_state(state: &ProjectState) {
    let status = context_status_for_state(state);
    persist_daemon_context_status(&state.project_root, &status);
}

fn persist_daemon_context_status(project_root: &Path, status: &DaemonContextStatus) {
    let secure_root = match ensure_secure_project_root(project_root) {
        Ok(root) => root,
        Err(err) => {
            debug!(
                "failed to prepare secure project root for daemon context status {}: {err}",
                project_root.display()
            );
            return;
        }
    };

    let path = daemon_context_status_path(project_root);
    let mut file = std::fs::read_to_string(&path)
        .ok()
        .and_then(|content| serde_json::from_str::<DaemonContextStatusFile>(&content).ok())
        .unwrap_or_default();
    file.contexts
        .insert(status.context_id.clone(), status.clone());

    let payload = match serde_json::to_vec_pretty(&file) {
        Ok(payload) => payload,
        Err(err) => {
            debug!(
                "failed to serialize daemon context status for {}: {err}",
                project_root.display()
            );
            return;
        }
    };

    if let Err(err) = atomic_replace(
        &path,
        &payload,
        &secure_root,
        PROJECT_STATE_DIR_MODE,
        SECURE_STATE_FILE_MODE,
    ) {
        debug!(
            "failed to persist daemon context status for {}: {err}",
            project_root.display()
        );
    }
}

fn mark_refresh_pending(
    state: &mut ProjectState,
    scope: RunScope,
    fallback_reason: Option<String>,
) {
    if let Some(reason) = fallback_reason {
        state.run_state.mark_dirty_with_reason(scope, reason);
    } else {
        state.run_state.mark_dirty(scope);
    }
    state.last_refresh_state = DaemonRefreshState::Pending;
    state.last_refresh_error = None;
    persist_daemon_context_status_for_state(state);
}

fn mark_refresh_running(state: &mut ProjectState, started_at: DateTime<Utc>) {
    state.last_refresh_state = DaemonRefreshState::Running;
    state.last_refresh_started_at = Some(started_at);
    state.last_refresh_completed_at = None;
    state.last_refresh_error = None;
    persist_daemon_context_status_for_state(state);
}

fn mark_refresh_finished(
    state: &mut ProjectState,
    finished_at: DateTime<Utc>,
    result: Result<(), &OneupError>,
) {
    state.last_refresh_completed_at = Some(finished_at);
    match result {
        Ok(()) => {
            state.last_refresh_state = DaemonRefreshState::Complete;
            state.last_refresh_error = None;
        }
        Err(err) => {
            state.last_refresh_state = DaemonRefreshState::Failed;
            state.last_refresh_error = Some(err.to_string());
        }
    }
    persist_daemon_context_status_for_state(state);
}

fn normalize_relative_path(project_root: &Path, changed_path: &Path) -> Option<PathBuf> {
    let relative = changed_path.strip_prefix(project_root).ok()?;
    if relative.as_os_str().is_empty() {
        None
    } else {
        Some(relative.to_path_buf())
    }
}

fn mark_changed_projects(projects: &mut ProjectStates, changes: &watcher::WatcherChanges) {
    for state in projects.values_mut() {
        let source_root = &state.source_root;
        let has_ambiguous = changes
            .ambiguous_paths
            .iter()
            .any(|path| path.starts_with(source_root));
        let (scope, promotion_reason) = if changes.has_unscoped_error || has_ambiguous {
            let reason = if changes.has_unscoped_error {
                "has_unscoped_error".to_string()
            } else {
                "ambiguous_paths".to_string()
            };
            (Some(RunScope::Full), Some(reason))
        } else {
            (
                RunScope::from_paths(
                    changes
                        .file_paths
                        .iter()
                        .filter(|path| path.starts_with(source_root))
                        .filter_map(|path| normalize_relative_path(source_root, path)),
                ),
                None,
            )
        };

        let Some(scope) = scope else {
            continue;
        };

        let relevant_count = changes
            .file_paths
            .iter()
            .filter(|path| path.starts_with(source_root))
            .count();

        let was_dirty = state.run_state.dirty;
        let was_running = state.run_state.running;
        mark_refresh_pending(state, scope.clone(), promotion_reason);

        if was_running && !was_dirty {
            debug!(
                "project {} changed during an active run; queued one follow-up {}",
                state.project_root.display(),
                match scope {
                    RunScope::Full => "full re-index".to_string(),
                    RunScope::Paths(paths) => format!("run for {} changed paths", paths.len()),
                }
            );
        } else if !was_dirty {
            match scope {
                RunScope::Full => {
                    debug!("queued full re-index for {}", state.project_root.display());
                }
                RunScope::Paths(paths) => {
                    debug!(
                        "queued re-index for {} after {} changed paths",
                        state.project_root.display(),
                        paths.len().max(relevant_count)
                    );
                }
            }
        }
    }
}

fn next_dirty_project_key(projects: &ProjectStates, preferred_key: Option<&str>) -> Option<String> {
    if let Some(preferred_key) = preferred_key {
        if projects
            .get(preferred_key)
            .is_some_and(|state| state.run_state.dirty && !state.run_state.running)
        {
            return Some(preferred_key.to_string());
        }
    }

    let mut dirty_keys: Vec<String> = projects
        .iter()
        .filter(|(_, state)| state.run_state.dirty && !state.run_state.running)
        .map(|(key, _)| key.clone())
        .collect();
    dirty_keys.sort();
    dirty_keys.into_iter().next()
}

/// Run dirty projects to completion, but race the whole sweep against SIGTERM so
/// the daemon can honour its bounded drain mid-pass.
///
/// On SIGTERM the token is cancelled and the in-flight sweep is **resumed**
/// (never dropped) until it reaches its next safe unit boundary and returns. The
/// pinned future is awaited to completion rather than dropped at an arbitrary
/// `.await`, which is what keeps an interrupted flush from being torn mid-write.
/// SIGTERM is consumed here, so callers detect shutdown via
/// `cancel_token.is_cancelled()` (the main loop guard) rather than a second
/// `sigterm.recv()`.
async fn run_dirty_projects_until_clean_or_cancelled(
    watcher: &FileWatcher,
    projects: &mut ProjectStates,
    cancel_token: &CancellationToken,
    sigterm: &mut Signal,
    search_requests_rx: &mut mpsc::Receiver<QueuedSearchRequest>,
) {
    let sweep = run_dirty_projects_until_clean(watcher, projects, cancel_token, search_requests_rx);
    tokio::pin!(sweep);

    tokio::select! {
        _ = &mut sweep => {}
        _ = sigterm.recv() => {
            info!("received SIGTERM during indexing; cancelling in-flight pass at next safe point");
            cancel_token.cancel();
            // Resume the same pinned future so it unwinds cooperatively at a
            // committed boundary instead of being dropped mid-flush.
            sweep.await;
        }
    }
}

async fn run_dirty_projects_until_clean(
    watcher: &FileWatcher,
    projects: &mut ProjectStates,
    cancel_token: &CancellationToken,
    search_requests_rx: &mut mpsc::Receiver<QueuedSearchRequest>,
) {
    let mut preferred_key: Option<String> = None;

    while let Some(key) = next_dirty_project_key(projects, preferred_key.as_deref()) {
        // Stop starting new project passes once cancelled; the current process is
        // draining for shutdown and any started pass would immediately re-cancel.
        if cancel_token.is_cancelled() {
            debug!("skipping further re-index sweeps: shutdown cancellation requested");
            break;
        }
        preferred_key = None;

        let result = run_project(&key, projects, cancel_token, search_requests_rx).await;

        let filtered = watcher::filter_changed_paths(watcher, watcher.drain_events_nowait());
        record_file_check_for_all_projects(projects, Utc::now(), false);
        if !filtered.is_empty() {
            debug!(
                "detected {} changed files and {} ambiguous paths while re-indexing",
                filtered.file_paths.len(),
                filtered.ambiguous_paths.len()
            );
            mark_changed_projects(projects, &filtered);
        }

        match result {
            Ok(stats) => {
                let project_root = projects
                    .get(&key)
                    .map(|state| state.project_root.clone())
                    .unwrap_or_else(|| PathBuf::from(&key));
                info!(
                    "re-index complete for {}: {} indexed, {} skipped",
                    project_root.display(),
                    stats.files_indexed,
                    stats.files_skipped
                );

                if projects
                    .get(&key)
                    .is_some_and(|state| state.run_state.dirty)
                {
                    debug!(
                        "collapsed change burst for context {} into one queued follow-up run",
                        key
                    );
                    preferred_key = Some(key);
                }
            }
            Err(e) => {
                if matches!(
                    &e,
                    OneupError::Indexing(crate::shared::errors::IndexingError::Cancelled)
                ) {
                    // SIGTERM cancelled the pass at a unit boundary. `run_project`
                    // already re-queued the scope (the context stays dirty so the
                    // restarted binary re-indexes the remainder). Stop the sweep;
                    // the main loop guard will break for shutdown.
                    debug!("re-index sweep for context {key} cancelled for shutdown");
                    break;
                }
                if matches!(
                    &e,
                    OneupError::Daemon(
                        crate::shared::errors::DaemonError::RebuildLockContended { .. }
                    )
                ) {
                    // The pass deferred to a competing one-shot rebuild and left
                    // the project dirty. Return to the select loop instead of
                    // immediately re-selecting the same key (which would
                    // busy-spin on the held lock); the next debounce tick or
                    // file event retries once the other writer releases it.
                    debug!("deferring re-index sweep for context {key}: {e}");
                    break;
                }
                error!("re-index failed for context {key}: {e}");
            }
        }
    }
}

fn mark_branch_context_changes(watcher: &mut FileWatcher, projects: &mut ProjectStates) {
    let changed_contexts: Vec<(String, WorktreeContext)> = projects
        .iter()
        .filter_map(|(context_id, state)| {
            let current_context = current_context_for_state(state);
            branch_context_changed(&state.context, &current_context)
                .then(|| (context_id.clone(), current_context))
        })
        .collect();

    for (old_context_id, current_context) in changed_contexts {
        let Some(mut state) = projects.remove(&old_context_id) else {
            continue;
        };
        let new_context_id = current_context.context_id.clone();
        let old_source_root = state.source_root.clone();
        state.watch_status = DaemonWatchStatus::DaemonStopped;
        persist_daemon_context_status_for_state(&state);
        state.watch_status = DaemonWatchStatus::Watching;
        state.context = current_context;
        mark_refresh_pending(
            &mut state,
            RunScope::Full,
            Some("branch_context_changed".to_string()),
        );
        info!(
            "queued full re-index for {} after branch context changed",
            state.project_root.display()
        );

        if let Some(existing) = projects.get_mut(&new_context_id) {
            mark_refresh_pending(
                existing,
                RunScope::Full,
                Some("branch_context_changed".to_string()),
            );
            if !source_root_is_still_tracked(projects, &old_source_root) {
                if let Err(err) = watcher.unwatch(&old_source_root) {
                    warn!(
                        "failed to unwatch merged branch context source {}: {err}",
                        old_source_root.display()
                    );
                }
            }
        } else {
            projects.insert(new_context_id, state);
        }
    }
}

fn source_root_is_still_tracked(projects: &ProjectStates, source_root: &Path) -> bool {
    let canonical_source_root = canonical_project_root(source_root);
    projects
        .values()
        .any(|state| canonical_project_root(&state.source_root) == canonical_source_root)
}

fn mark_all_contexts_daemon_stopped(projects: &mut ProjectStates) {
    for state in projects.values_mut() {
        state.watch_status = DaemonWatchStatus::DaemonStopped;
        persist_daemon_context_status_for_state(state);
    }
}

/// Run `unit` to completion, servicing queued daemon search requests as they
/// arrive in the meantime.
///
/// HYP-002 (CONFIRMED): a refresh sweep's per-project pass can run for
/// seconds — far past `DAEMON_READ_TIMEOUT_MS` — so yielding only at project
/// boundaries is insufficient; a search queued while `unit` is still pending
/// must be served without waiting for `unit` to finish. Each iteration polls
/// `unit` and the search-request channel together: whichever is ready first
/// wins, and if a request wins, `unit` simply gets re-polled (resuming
/// exactly where it left off) on the next loop iteration. `handle_search_request`
/// takes its own brief, per-call slice of `projects` (T8) rather than the
/// long-lived borrow `unit` may or may not hold, so a search for any OTHER
/// project never contends with `unit`'s (potentially long) execution here.
async fn run_unit_while_servicing_search<F: Future>(
    unit: F,
    projects: &mut ProjectStates,
    search_requests_rx: &mut mpsc::Receiver<QueuedSearchRequest>,
) -> F::Output {
    tokio::pin!(unit);
    loop {
        tokio::select! {
            result = &mut unit => return result,
            Some(queued) = search_requests_rx.recv() => {
                let response = handle_search_request(projects, queued.request).await;
                let _ = queued.respond_to.send(response);
            }
        }
    }
}

/// Count files in a gitignore-aware manner for gate-check purposes.
/// Returns the count of regular files that are not ignored by .gitignore.
fn count_files_gitignore_aware(source_root: &Path) -> Result<usize, OneupError> {
    use ignore::WalkBuilder;

    let walker = WalkBuilder::new(source_root)
        .hidden(false)
        .ignore(true) // Respect .gitignore
        .build();

    let count = walker
        .into_iter()
        .filter_map(|result| result.ok())
        .filter(|entry| entry.file_type().map(|ft| ft.is_file()).unwrap_or(false))
        .count();

    Ok(count)
}

async fn run_project(
    context_id: &str,
    projects: &mut ProjectStates,
    cancel_token: &CancellationToken,
    search_requests_rx: &mut mpsc::Receiver<QueuedSearchRequest>,
) -> Result<pipeline::PipelineStats, OneupError> {
    // Acquire the single-writer rebuild lock BEFORE `start_run` consumes the
    // pending scope, so a contended pass leaves the project dirty (its queued
    // paths intact) for a later retry instead of racing a competing rebuild and
    // dropping the changes. Non-blocking: the daemon defers rather than stalling
    // its event loop while a one-shot rebuild holds the lock. The guard releases
    // on drop — including when an in-flight pass is cancelled and this frame
    // unwinds — freeing the lock for the restarted binary.
    let lock_root = projects
        .get(context_id)
        .expect("dirty project must exist while running")
        .project_root
        .clone();
    let _rebuild_lock = match lifecycle::try_acquire_rebuild_lock(&lock_root) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            debug!(
                "deferring re-index for {}: rebuild lock held by another process",
                lock_root.display()
            );
            return Err(crate::shared::errors::DaemonError::RebuildLockContended {
                state_root: lock_root.display().to_string(),
            }
            .into());
        }
        Err(e) => return Err(e),
    };

    // Holding the rebuild lock means any one-shot rebuild has finished and
    // released it, so its atomic switch-over is complete. If that rebuild swapped
    // the index onto a fresh inode, the daemon's long-lived handle now points at
    // the orphaned pre-swap inode; reopen it here — before `start_run` consumes
    // the pending scope and before any write — so this pass writes into the
    // refreshed index, never the lost old one (HYP-002). Doing this before
    // `start_run` keeps the project dirty for a clean retry if the reopen fails.
    {
        let state = projects
            .get_mut(context_id)
            .expect("dirty project must exist while running");
        if let Err(e) = reopen_if_index_swapped(state).await {
            warn!(
                "failed to reopen swapped index for {} before re-index: {e}",
                state.project_root.display()
            );
            mark_refresh_finished(state, Utc::now(), Err(&e));
            return Err(e);
        }
    }

    // REQ-001: Gate check for first-time large monorepo indexing.
    // Before starting a first index, check if file count is over threshold without scope.
    // If so, stay idle and let the MCP oneup_start path handle the gate.
    {
        let state = projects
            .get(context_id)
            .expect("dirty project must exist while gating");
        let state_root = &state.project_root;
        let source_root = &state.source_root;

        // Check if this is a first index (no index.db exists)
        let index_path = config::project_db_path(state_root);
        if !index_path.exists() {
            // First index: check the gate
            let threshold = std::env::var(crate::shared::constants::FILE_COUNT_THRESHOLD_ENV_VAR)
                .ok()
                .and_then(|raw| raw.trim().parse::<usize>().ok())
                .unwrap_or(crate::shared::constants::FILE_COUNT_THRESHOLD);

            let file_count = count_files_gitignore_aware(source_root).unwrap_or(0);

            // Check if scope is recorded in the progress file
            let scope_recorded = read_index_progress(state_root)
                .and_then(|progress| progress.scope)
                .is_some();

            // Check the gate
            if !lifecycle::should_start_first_index(
                state_root,
                file_count,
                threshold,
                scope_recorded,
            )? {
                debug!(
                    "daemon gate fired for {}: over-threshold ({} > {}) without scope; staying idle",
                    state_root.display(),
                    file_count,
                    threshold
                );
                // Re-mark the project as dirty so it stays queued for a later retry
                let state = projects.get_mut(context_id).expect("must exist");
                mark_refresh_pending(state, RunScope::Full, None);
                mark_refresh_finished(state, Utc::now(), Ok(()));
                return Ok(pipeline::PipelineStats::default());
            }
        }
    }

    let mut setup = SetupTimings::new(std::time::Instant::now());
    let (project_root, source_root, context, scope, daemon_fallback_reason, conn_setup) = {
        let state = projects
            .get_mut(context_id)
            .expect("dirty project must exist while running");
        let daemon_fallback_reason = state.run_state.pending_fallback_reason.take();
        let scope = state.run_state.start_run();
        mark_refresh_running(state, Utc::now());
        let project_root = state.project_root.clone();
        let source_root = state.source_root.clone();
        let context = state.context.clone();
        let db_start = std::time::Instant::now();
        let conn_setup = async {
            let conn = state.db.connect_tuned().await?;
            let mut indexing_config =
                config::resolve_indexing_config(None, None, state.indexing.as_ref())?;

            // REQ-002: Daemon path must apply recorded scope identically to MCP path.
            // Load scope from meta table and apply as include_globs so the file walk
            // respects the scoped boundaries. This ensures both daemon and in-process
            // paths produce identical scope behavior.
            if let Ok(Some(scope_roots)) = crate::storage::schema::read_scope_from_meta(&conn).await
            {
                let scope_globs: Vec<String> = scope_roots
                    .iter()
                    .map(|root| format!("{}/**", root))
                    .collect();
                indexing_config.include_globs = scope_globs;
            }

            Ok::<_, OneupError>((conn, indexing_config))
        }
        .await;
        setup.db_prepare_ms = db_start.elapsed().as_millis();

        (
            project_root,
            source_root,
            context,
            scope,
            daemon_fallback_reason,
            conn_setup,
        )
    };

    let (conn, indexing_config) = match conn_setup {
        Ok(values) => values,
        Err(e) => {
            projects
                .get_mut(context_id)
                .expect("dirty project must exist while finishing a failed setup")
                .run_state
                .finish_run();
            let state = projects
                .get_mut(context_id)
                .expect("dirty project must exist while recording failed setup");
            mark_refresh_finished(state, Utc::now(), Err(&e));
            return Err(e);
        }
    };

    match &scope {
        RunScope::Full => {
            info!(
                "re-indexing full project {} from {} (jobs={}, embed_threads={})",
                project_root.display(),
                source_root.display(),
                indexing_config.jobs,
                indexing_config.embed_threads
            );
        }
        RunScope::Paths(paths) => {
            info!(
                "re-indexing {} changed files in {} from {} (jobs={}, embed_threads={})",
                paths.len(),
                project_root.display(),
                source_root.display(),
                indexing_config.jobs,
                indexing_config.embed_threads
            );
        }
    }

    // Take the embedding runtime OUT of the map-borrowed state before the
    // (potentially multi-second) prepare+pipeline pass, so `projects` is never
    // held across it (HYP-002/T8): a queued daemon search for ANY project runs
    // via `run_unit_while_servicing_search` below without contending with this
    // pass for the whole-map borrow. `EmbeddingRuntime` is cheap to move
    // (`Default`-backed cache), so this is a pointer-swap, not a reload.
    let mut embedding_runtime = {
        let state = projects
            .get_mut(context_id)
            .expect("dirty project must exist while preparing embeddings");
        std::mem::take(&mut state.embedding_runtime)
    };
    let model_start = std::time::Instant::now();
    let prepare_status = embedding_runtime
        .prepare_for_indexing(indexing_config.embed_threads)
        .await;
    setup.model_prepare_ms = model_start.elapsed().as_millis();

    let status = match prepare_status {
        Ok(status) => status,
        Err(e) => {
            let state = projects
                .get_mut(context_id)
                .expect("dirty project must exist while restoring a failed embedding prepare");
            state.embedding_runtime = embedding_runtime;
            state.run_state.finish_run();
            mark_refresh_finished(state, Utc::now(), Err(&e));
            return Err(e);
        }
    };
    log_indexing_embedding_status(&project_root, indexing_config.embed_threads, &status);

    let pipeline_unit = pipeline::run_with_context_scope_setup_and_progress_root(
        &conn,
        &context,
        embedding_runtime.current_embedder(),
        &scope,
        &indexing_config,
        None,
        true,
        Some(setup),
        daemon_fallback_reason,
        Some(&project_root),
        cancel_token,
    );
    let result = run_unit_while_servicing_search(pipeline_unit, projects, search_requests_rx).await;

    let state = projects
        .get_mut(context_id)
        .expect("dirty project must exist while finishing a run");
    state.embedding_runtime = embedding_runtime;
    state.run_state.finish_run();

    if matches!(
        &result,
        Err(OneupError::Indexing(
            crate::shared::errors::IndexingError::Cancelled
        ))
    ) {
        // A cancelled pass is neither complete nor failed: it stopped at a
        // committed boundary with the remainder unindexed. Re-queue the scope so
        // the context stays dirty (refresh state -> Pending) and the remaining
        // files re-index on the next pass — here if the daemon survives, or on
        // the restarted binary's startup reconciliation after a SIGTERM drain.
        mark_refresh_pending(state, scope, Some("cancelled".to_string()));
        return result;
    }

    mark_refresh_finished(state, Utc::now(), result.as_ref().map(|_| ()));

    result
}

async fn handle_search_request(
    projects: &mut ProjectStates,
    request: SearchRequest,
) -> SearchResponse {
    let Some(state) = projects.get_mut(&request.context_id) else {
        debug!(
            "daemon search requested for unregistered context {} on project {}",
            request.context_id,
            request.project_root.display()
        );
        return search_service::unavailable_response();
    };
    if canonical_project_root(&state.project_root) != canonical_project_root(&request.project_root)
        || canonical_project_root(&state.source_root)
            != canonical_project_root(&request.source_root)
    {
        debug!(
            "daemon search requested for mismatched context {} source {} on project {}",
            request.context_id,
            request.source_root.display(),
            request.project_root.display()
        );
        return search_service::unavailable_response();
    }

    // Adopt a freshly-swapped index before serving: if a one-shot rebuild
    // atomically switched `index.db` onto a new inode (T2), reopen the handle so
    // this search is served by the refreshed index automatically, with no manual
    // step (REQ-002 AC3). The switch-over is atomic, so the handle always lands
    // on a complete index — never a partial one. A reopen failure falls back to
    // the standard unavailable response (the CLI then searches locally) rather
    // than serving through the orphaned pre-swap handle.
    if let Err(err) = reopen_if_index_swapped(state).await {
        warn!(
            "failed to reopen swapped index for {} before daemon search: {err}",
            state.project_root.display()
        );
        return search_service::unavailable_response();
    }

    let mut search_scope = SearchScope::from_worktree_context(&state.context);
    if let Some(prefix) = request.path_prefix.as_deref() {
        search_scope = search_scope.with_path_prefix(prefix);
    }
    // Stale-but-available: the daemon detects a rebuild/refresh from its own
    // in-memory refresh state (not the MCP out-of-process detector, so the
    // daemon keeps no dependency on the MCP layer). When a pass is in flight
    // the served results are flagged possibly-stale via `degraded_reason`.
    let rebuild_in_progress = state.last_refresh_state.is_in_flight();

    let indexing_config = match config::resolve_indexing_config(None, None, state.indexing.as_ref())
    {
        Ok(indexing_config) => indexing_config,
        Err(err) => {
            warn!(
                "failed to resolve daemon search configuration for {}: {err}",
                state.project_root.display()
            );
            return search_service::unavailable_response();
        }
    };

    // Reuse one tuned read connection across requests (R-008). The schema is NOT
    // re-validated here: `ensure_current` is authoritative at index-open
    // (`load_project_state` -> `prepare_for_write`) and on every build-aside swap
    // (`reopen_if_index_swapped` -> `prepare_for_write`, called just above), and
    // `read_conn` is dropped on swap, so a served connection always belongs to an
    // index whose schema was validated when it was adopted. Hoisting the gate out
    // of the per-request path is sound only because every (re)open re-runs it.
    let conn = match ensure_read_conn(state).await {
        Ok(conn) => conn,
        Err(err) => {
            warn!(
                "failed to open daemon search connection for {}: {err}",
                state.project_root.display()
            );
            return search_service::unavailable_response();
        }
    };

    let has_embeddings = match retrieval::has_indexed_embeddings(&conn, &search_scope).await {
        Ok(has_embeddings) => has_embeddings,
        Err(err) => {
            warn!(
                "failed to inspect daemon search embeddings for {}: {err}",
                state.project_root.display()
            );
            return search_service::unavailable_response();
        }
    };

    // Populate (once) and reuse the per-context vector count so the hot path
    // skips a per-query `COUNT(*)` (R-007). The cache is invalidated on index
    // swap by `reopen_if_index_swapped`, so a populated value always reflects the
    // currently-open index. Only meaningful when embeddings exist.
    let cached_vector_count = if has_embeddings {
        match state.cached_vector_count {
            Some(count) => Some(count),
            None => match retrieval::count_vector_rows_for_context(&conn, &search_scope).await {
                Ok(count) => {
                    state.cached_vector_count = Some(count);
                    Some(count)
                }
                Err(err) => {
                    warn!(
                        "failed to count daemon search vectors for {}: {err}",
                        state.project_root.display()
                    );
                    return search_service::unavailable_response();
                }
            },
        }
    } else {
        None
    };

    // This context's own embedding runtime is temporarily checked out by an
    // in-flight refresh sweep exactly while `last_refresh_state == Running`
    // (`run_project`'s `std::mem::take`, HYP-002/T8): `Pending` (dirty but not
    // yet started) leaves the runtime untouched, so only `Running` must divert
    // away from it.
    let embedder_checked_out_by_sweep = state.last_refresh_state == DaemonRefreshState::Running;

    let mut degraded_reason = None;
    let results = if !has_embeddings {
        degraded_reason = Some(daemon_search_degraded_reason());
        let engine = HybridSearchEngine::new_scoped(&conn, None, search_scope);
        engine.fts_only_search(&request.query, request.limit).await
    } else if embedder_checked_out_by_sweep {
        // Touching `state.embedding_runtime` here would either race the
        // sweep's live embedder or force a redundant cold reload into the
        // `Default` placeholder `run_project` left behind. Degrade to
        // FTS-only; `STALE_REBUILD_REASON` (folded in below) already tells the
        // caller a pass for this context is in flight.
        let engine = HybridSearchEngine::new_scoped(&conn, None, search_scope);
        engine.fts_only_search(&request.query, request.limit).await
    } else {
        // Take the embedding runtime OUT of the map-borrowed state so the
        // heavy hybrid search below runs without holding `&mut projects`
        // (HYP-002/T8): a concurrent refresh sweep for a DIFFERENT project
        // never contends with this call for the whole-map borrow, and WAL's
        // concurrent-reader semantics (HYP-001) make the shared connection
        // safe to use unlocked. Every exit path below restores it before
        // returning, so a warm model is never dropped.
        let mut embedding_runtime = std::mem::take(&mut state.embedding_runtime);
        let status = match embedding_runtime.prepare_for_search(indexing_config.embed_threads) {
            Ok(status) => status,
            Err(err) => {
                if let Some(state) = projects.get_mut(&request.context_id) {
                    state.embedding_runtime = embedding_runtime;
                }
                // An invalid ONEUP_MODEL_VARIANT override is a hard config error,
                // not a degrade: refuse the request rather than silently serving
                // FTS-only results from the wrong (or no) variant (T1).
                warn!(
                    "daemon search embedding preparation failed for {}: {err}",
                    request.project_root.display()
                );
                return search_service::unavailable_response();
            }
        };
        log_search_embedding_status(
            &request.project_root,
            indexing_config.embed_threads,
            &status,
        );

        let results = if status.is_available() {
            let mut engine = HybridSearchEngine::new_scoped(
                &conn,
                embedding_runtime.current_embedder(),
                search_scope.clone(),
            )
            .with_has_vectors(has_embeddings);
            if let Some(count) = cached_vector_count {
                engine = engine.with_vector_count(count);
            }
            engine.search(&request.query, request.limit).await
        } else {
            degraded_reason = Some(daemon_search_degraded_reason());
            let engine = HybridSearchEngine::new_scoped(&conn, None, search_scope);
            engine.fts_only_search(&request.query, request.limit).await
        };

        if let Some(state) = projects.get_mut(&request.context_id) {
            state.embedding_runtime = embedding_runtime;
        }
        results
    };

    // Fold the stale-rebuild notice in through the shared combiner so it
    // coexists with any embeddings-degraded reason (joined by "; ", neither
    // dropped) and rides only in `degraded_reason`.
    let degraded_reason = combine_degraded_reasons(
        rebuild_in_progress.then(|| STALE_REBUILD_REASON.to_string()),
        degraded_reason,
    );

    match results {
        Ok(results) => SearchResponse::Results {
            results,
            daemon_version: Some(VERSION.to_string()),
            degraded_reason,
        },
        Err(err) => {
            warn!(
                "daemon search failed for {}: {err}",
                request.project_root.display()
            );
            search_service::unavailable_response()
        }
    }
}

fn daemon_search_degraded_reason() -> String {
    "semantic embeddings unavailable; search is degraded to FTS-only mode".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    use crate::shared::constants::DAEMON_READ_TIMEOUT_MS;

    #[test]
    fn should_idle_shutdown_only_when_empty_past_timeout() {
        let timeout = Duration::from_secs(60);
        // A daemon that still owns a project never idles out.
        assert!(!should_idle_shutdown(false, None, timeout));
        assert!(!should_idle_shutdown(
            false,
            Some(Duration::from_secs(10_000)),
            timeout
        ));
        // Empty but inside the grace window: keep running.
        assert!(!should_idle_shutdown(true, None, timeout));
        assert!(!should_idle_shutdown(
            true,
            Some(Duration::from_secs(59)),
            timeout
        ));
        // Empty at/past the grace window: self-exit.
        assert!(should_idle_shutdown(
            true,
            Some(Duration::from_secs(60)),
            timeout
        ));
        assert!(should_idle_shutdown(
            true,
            Some(Duration::from_secs(61)),
            timeout
        ));
        // A zero timeout exits as soon as the daemon first observes itself empty.
        assert!(should_idle_shutdown(
            true,
            Some(Duration::ZERO),
            Duration::ZERO
        ));
    }

    fn test_context(project_root: &Path, source_root: &Path) -> WorktreeContext {
        WorktreeContext {
            context_id: format!(
                "test-{}",
                source_root
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("root")
            ),
            state_root: project_root.to_path_buf(),
            source_root: source_root.to_path_buf(),
            main_worktree_root: project_root.to_path_buf(),
            worktree_role: if project_root == source_root {
                crate::shared::types::WorktreeRole::Main
            } else {
                crate::shared::types::WorktreeRole::Linked
            },
            git_dir: None,
            common_git_dir: None,
            branch_name: Some("main".to_string()),
            branch_ref: Some("refs/heads/main".to_string()),
            head_oid: Some("0000000000000000000000000000000000000000".to_string()),
            branch_status: crate::shared::types::BranchStatus::Named,
        }
    }

    fn project_state(
        project_root: &Path,
        source_root: &Path,
        db: Db,
        run_state: ProjectRunState,
    ) -> ProjectState {
        ProjectState {
            project_root: project_root.to_path_buf(),
            source_root: source_root.to_path_buf(),
            context: test_context(project_root, source_root),
            db,
            index_identity: index_file_identity(&config::project_db_path(project_root)),
            cached_vector_count: None,
            read_conn: None,
            indexing: None,
            embedding_runtime: EmbeddingRuntime::default(),
            run_state,
            watch_status: DaemonWatchStatus::Watching,
            last_refresh_state: DaemonRefreshState::Unknown,
            last_refresh_started_at: None,
            last_refresh_completed_at: None,
            last_refresh_error: None,
            last_file_check_persisted_at: None,
        }
    }

    fn insert_project(projects: &mut ProjectStates, state: ProjectState) -> String {
        let context_id = state.context.context_id.clone();
        projects.insert(context_id.clone(), state);
        context_id
    }

    #[test]
    fn run_state_collapses_bursts_into_follow_up() {
        let mut state = ProjectRunState::default();
        state.mark_dirty(RunScope::from_paths([PathBuf::from("src/lib.rs")]).unwrap());
        state.mark_dirty(RunScope::from_paths([PathBuf::from("README.md")]).unwrap());

        assert!(state.dirty);
        assert_eq!(
            state.pending_scope,
            RunScope::from_paths([PathBuf::from("README.md"), PathBuf::from("src/lib.rs")])
        );

        let pending = state.start_run();
        assert_eq!(
            pending,
            RunScope::from_paths([PathBuf::from("README.md"), PathBuf::from("src/lib.rs")])
                .unwrap()
        );
        assert!(state.running);
        assert!(!state.dirty);
        assert!(state.pending_scope.is_none());

        state.mark_dirty(RunScope::Full);
        assert!(state.dirty);
        assert_eq!(state.pending_scope, Some(RunScope::Full));

        state.finish_run();
        assert!(!state.running);
        assert!(state.dirty);
    }

    #[test]
    fn startup_reconciliation_marks_full_refresh_with_reason() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join("repo");
        std::fs::create_dir_all(&project_root).unwrap();

        let runtime = tokio::runtime::Runtime::new().unwrap();
        let db = runtime.block_on(Db::open_memory()).unwrap();
        let mut state = project_state(&project_root, &project_root, db, ProjectRunState::default());

        mark_startup_reconciliation_pending(&mut state);

        assert!(state.run_state.dirty);
        assert_eq!(state.run_state.pending_scope, Some(RunScope::Full));
        assert_eq!(
            state.run_state.pending_fallback_reason.as_deref(),
            Some(STARTUP_RECONCILIATION_REASON)
        );
        assert_eq!(state.last_refresh_state, DaemonRefreshState::Pending);
    }

    /// REQ-004 D / T9: `prewarm_project_embedders` runs once, right after
    /// `load_and_watch_projects`, so a project's embedding runtime is already
    /// loaded before the first real search arrives. Modeled on
    /// `prepare_for_search_reuses_warm_runtime_when_model_is_unchanged`: proving
    /// a post-prewarm `prepare_for_search` call returns `Warm` (a cache hit,
    /// not a fresh load) is exactly what a first real search would observe.
    /// Gated on model availability like the other real-inference tests
    /// (hermetic CI disables model downloads).
    #[test]
    fn startup_prewarm_leaves_embedding_runtime_warm_before_first_search() {
        use crate::indexer::embedder::{is_model_available, Fp32VariantTestGuard};

        // Pin the always-provisioned FP32 baseline so this test runs on any
        // host with the FP32 model present, independent of whether the INT8
        // default variant's artifact has been downloaded.
        let _variant = Fp32VariantTestGuard::set();
        if !is_model_available() {
            eprintln!("skipping: model not available");
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join("repo");
        std::fs::create_dir_all(&project_root).unwrap();

        let runtime = tokio::runtime::Runtime::new().unwrap();
        let db = runtime.block_on(Db::open_memory()).unwrap();
        let mut projects: ProjectStates = HashMap::new();
        let state = project_state(&project_root, &project_root, db, ProjectRunState::default());
        let context_id = insert_project(&mut projects, state);

        prewarm_project_embedders(&mut projects);

        let state = projects.get_mut(&context_id).unwrap();
        assert!(
            state.embedding_runtime.current_embedder().is_some(),
            "prewarm should leave the embedder loaded"
        );
        // Resolve `embed_threads` exactly as `handle_search_request` would for
        // the first real search, so the compatibility key matches the one the
        // prewarm pass used.
        let indexing_config = config::resolve_indexing_config(None, None, state.indexing.as_ref())
            .expect("default indexing config resolves");
        let status = state
            .embedding_runtime
            .prepare_for_search(indexing_config.embed_threads)
            .unwrap();
        assert_eq!(
            status,
            EmbeddingLoadStatus::Warm,
            "the first real search's prepare_for_search call should reuse the \
             prewarmed runtime instead of loading it cold"
        );
    }

    #[test]
    fn mark_changed_projects_only_queues_matching_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let alpha_root = tmp.path().join("alpha");
        let beta_root = tmp.path().join("beta");
        std::fs::create_dir_all(alpha_root.join("src")).unwrap();
        std::fs::create_dir_all(beta_root.join("src")).unwrap();

        let alpha_db = Db::open_memory();
        let beta_db = Db::open_memory();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let alpha_db = runtime.block_on(alpha_db).unwrap();
        let beta_db = runtime.block_on(beta_db).unwrap();

        let mut projects = HashMap::new();
        let alpha_key = insert_project(
            &mut projects,
            project_state(
                &alpha_root,
                &alpha_root,
                alpha_db,
                ProjectRunState {
                    running: true,
                    dirty: false,
                    pending_scope: None,
                    pending_fallback_reason: None,
                },
            ),
        );
        let beta_key = insert_project(
            &mut projects,
            project_state(&beta_root, &beta_root, beta_db, ProjectRunState::default()),
        );

        let changes = watcher::WatcherChanges {
            file_paths: std::collections::BTreeSet::from([
                alpha_root.join("src").join("lib.rs"),
                alpha_root.join("README.md"),
                beta_root.join("src").join("mod.rs"),
                tmp.path().join("outside.txt"),
            ]),
            ambiguous_paths: std::collections::BTreeSet::new(),
            has_unscoped_error: false,
        };

        mark_changed_projects(&mut projects, &changes);

        let alpha = &projects.get(&alpha_key).unwrap().run_state;
        assert!(alpha.running);
        assert!(alpha.dirty);
        assert_eq!(
            alpha.pending_scope,
            RunScope::from_paths([PathBuf::from("README.md"), PathBuf::from("src/lib.rs")])
        );

        let beta = &projects.get(&beta_key).unwrap().run_state;
        assert!(!beta.running);
        assert!(beta.dirty);
        assert_eq!(
            beta.pending_scope,
            RunScope::from_paths([PathBuf::from("src/mod.rs")])
        );
    }

    #[test]
    fn mark_changed_projects_escalates_ambiguous_and_unscoped_events() {
        let tmp = tempfile::tempdir().unwrap();
        let alpha_root = tmp.path().join("alpha");
        let beta_root = tmp.path().join("beta");
        std::fs::create_dir_all(alpha_root.join("src")).unwrap();
        std::fs::create_dir_all(beta_root.join("src")).unwrap();

        let alpha_db = Db::open_memory();
        let beta_db = Db::open_memory();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let alpha_db = runtime.block_on(alpha_db).unwrap();
        let beta_db = runtime.block_on(beta_db).unwrap();

        let mut projects = HashMap::new();
        let alpha_key = insert_project(
            &mut projects,
            project_state(
                &alpha_root,
                &alpha_root,
                alpha_db,
                ProjectRunState::default(),
            ),
        );
        let beta_key = insert_project(
            &mut projects,
            project_state(&beta_root, &beta_root, beta_db, ProjectRunState::default()),
        );

        mark_changed_projects(
            &mut projects,
            &watcher::WatcherChanges {
                file_paths: std::collections::BTreeSet::new(),
                ambiguous_paths: std::collections::BTreeSet::from([alpha_root.join("src")]),
                has_unscoped_error: false,
            },
        );
        assert_eq!(
            projects.get(&alpha_key).unwrap().run_state.pending_scope,
            Some(RunScope::Full)
        );
        assert!(projects
            .get(&beta_key)
            .unwrap()
            .run_state
            .pending_scope
            .is_none());

        mark_changed_projects(
            &mut projects,
            &watcher::WatcherChanges {
                file_paths: std::collections::BTreeSet::new(),
                ambiguous_paths: std::collections::BTreeSet::new(),
                has_unscoped_error: true,
            },
        );
        assert_eq!(
            projects.get(&alpha_key).unwrap().run_state.pending_scope,
            Some(RunScope::Full)
        );
        assert_eq!(
            projects.get(&beta_key).unwrap().run_state.pending_scope,
            Some(RunScope::Full)
        );
    }

    #[test]
    fn mark_changed_projects_matches_source_root_when_state_root_differs() {
        let tmp = tempfile::tempdir().unwrap();
        let state_root = tmp.path().join("main");
        let source_root = tmp.path().join("worktree");
        std::fs::create_dir_all(source_root.join("src")).unwrap();
        std::fs::create_dir_all(&state_root).unwrap();

        let runtime = tokio::runtime::Runtime::new().unwrap();
        let db = runtime.block_on(Db::open_memory()).unwrap();

        let mut projects = HashMap::new();
        let state_key = insert_project(
            &mut projects,
            project_state(&state_root, &source_root, db, ProjectRunState::default()),
        );

        let changes = watcher::WatcherChanges {
            file_paths: std::collections::BTreeSet::from([source_root.join("src").join("lib.rs")]),
            ambiguous_paths: std::collections::BTreeSet::new(),
            has_unscoped_error: false,
        };

        mark_changed_projects(&mut projects, &changes);

        assert_eq!(
            projects.get(&state_key).unwrap().run_state.pending_scope,
            RunScope::from_paths([PathBuf::from("src/lib.rs")])
        );
    }

    #[test]
    fn mark_changed_projects_keeps_shared_state_root_contexts_independent() {
        let tmp = tempfile::tempdir().unwrap();
        let state_root = tmp.path().join("main");
        let alpha_source = tmp.path().join("alpha-worktree");
        let beta_source = tmp.path().join("beta-worktree");
        std::fs::create_dir_all(alpha_source.join("src")).unwrap();
        std::fs::create_dir_all(beta_source.join("src")).unwrap();
        std::fs::create_dir_all(&state_root).unwrap();

        let runtime = tokio::runtime::Runtime::new().unwrap();
        let alpha_db = runtime.block_on(Db::open_memory()).unwrap();
        let beta_db = runtime.block_on(Db::open_memory()).unwrap();

        let mut projects = HashMap::new();
        let alpha_key = insert_project(
            &mut projects,
            project_state(
                &state_root,
                &alpha_source,
                alpha_db,
                ProjectRunState::default(),
            ),
        );
        let beta_key = insert_project(
            &mut projects,
            project_state(
                &state_root,
                &beta_source,
                beta_db,
                ProjectRunState::default(),
            ),
        );

        mark_changed_projects(
            &mut projects,
            &watcher::WatcherChanges {
                file_paths: std::collections::BTreeSet::from([alpha_source
                    .join("src")
                    .join("lib.rs")]),
                ambiguous_paths: std::collections::BTreeSet::new(),
                has_unscoped_error: false,
            },
        );

        assert_eq!(
            projects.get(&alpha_key).unwrap().run_state.pending_scope,
            RunScope::from_paths([PathBuf::from("src/lib.rs")])
        );
        assert!(projects
            .get(&beta_key)
            .unwrap()
            .run_state
            .pending_scope
            .is_none());
    }

    #[test]
    fn mark_branch_context_changes_marks_old_context_stopped() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join("repo");
        std::fs::create_dir_all(&project_root).unwrap();
        let project_root = project_root.canonicalize().unwrap();

        let runtime = tokio::runtime::Runtime::new().unwrap();
        let db = runtime.block_on(Db::open_memory()).unwrap();
        let state = project_state(&project_root, &project_root, db, ProjectRunState::default());
        let old_context_id = state.context.context_id.clone();

        let mut projects = HashMap::new();
        projects.insert(old_context_id.clone(), state);

        let mut watcher = FileWatcher::new().unwrap();
        watcher.watch(&project_root).unwrap();

        mark_branch_context_changes(&mut watcher, &mut projects);

        assert!(!projects.contains_key(&old_context_id));
        assert_eq!(projects.len(), 1);
        assert_eq!(
            projects.values().next().unwrap().run_state.pending_scope,
            Some(RunScope::Full)
        );

        let context_status_path = daemon_context_status_path(&project_root);
        let context_status: DaemonContextStatusFile =
            serde_json::from_str(&std::fs::read_to_string(context_status_path).unwrap()).unwrap();
        let old_entry = context_status.contexts.get(&old_context_id).unwrap();
        assert_eq!(old_entry.watch_status, DaemonWatchStatus::DaemonStopped);
    }

    #[test]
    fn next_dirty_project_prefers_follow_up_root() {
        let tmp = tempfile::tempdir().unwrap();
        let alpha_root = tmp.path().join("alpha");
        let beta_root = tmp.path().join("beta");
        std::fs::create_dir_all(&alpha_root).unwrap();
        std::fs::create_dir_all(&beta_root).unwrap();

        let alpha_db = Db::open_memory();
        let beta_db = Db::open_memory();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let alpha_db = runtime.block_on(alpha_db).unwrap();
        let beta_db = runtime.block_on(beta_db).unwrap();

        let mut projects = HashMap::new();
        let alpha_key = insert_project(
            &mut projects,
            project_state(
                &alpha_root,
                &alpha_root,
                alpha_db,
                ProjectRunState {
                    running: false,
                    dirty: true,
                    pending_scope: Some(
                        RunScope::from_paths([PathBuf::from("src/lib.rs")]).unwrap(),
                    ),
                    pending_fallback_reason: None,
                },
            ),
        );
        let beta_key = insert_project(
            &mut projects,
            project_state(
                &beta_root,
                &beta_root,
                beta_db,
                ProjectRunState {
                    running: false,
                    dirty: true,
                    pending_scope: Some(
                        RunScope::from_paths([PathBuf::from("src/mod.rs")]).unwrap(),
                    ),
                    pending_fallback_reason: None,
                },
            ),
        );

        let preferred = next_dirty_project_key(&projects, Some(&beta_key));
        assert_eq!(preferred, Some(beta_key.clone()));

        projects.get_mut(&beta_key).unwrap().run_state.dirty = false;
        let fallback = next_dirty_project_key(&projects, Some(&beta_key));
        assert_eq!(fallback, Some(alpha_key));
    }

    #[tokio::test]
    async fn run_project_leaves_context_dirty_when_cancelled() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        for i in 0..6 {
            std::fs::write(
                root.join(format!("mod_{i}.rs")),
                format!("pub fn item_{i}() -> usize {{ {i} }}\n"),
            )
            .unwrap();
        }

        // A file-backed DB so the connection `run_project` opens shares the
        // initialized schema (separate `:memory:` connections would not), letting
        // the pass reach the hot-loop cancel check.
        let db_path = config::project_db_path(&root);
        ensure_secure_project_root(&root).unwrap();
        let db = Db::open_rw(&db_path).await.unwrap();
        schema::initialize(&db.connect_tuned().await.unwrap())
            .await
            .unwrap();

        let mut projects = HashMap::new();
        let key = insert_project(
            &mut projects,
            project_state(
                &root,
                &root,
                db,
                ProjectRunState {
                    running: false,
                    dirty: true,
                    pending_scope: Some(RunScope::Full),
                    pending_fallback_reason: None,
                },
            ),
        );

        // A token cancelled before the pass means run_project must not record a
        // completed (or failed) run; the context stays dirty for re-indexing.
        let cancel_token = CancellationToken::new();
        cancel_token.cancel();

        // No search traffic is exercised in this test; an idle channel (sender
        // kept alive, nothing ever sent) never resolves `run_project`'s internal
        // search-servicing select arm, so the pipeline unit is the only branch
        // that ever completes.
        let (_tx, mut search_requests_rx) = mpsc::channel(1);
        let result = run_project(&key, &mut projects, &cancel_token, &mut search_requests_rx).await;
        assert!(
            matches!(
                result,
                Err(OneupError::Indexing(
                    crate::shared::errors::IndexingError::Cancelled
                ))
            ),
            "a cancelled pass must surface the Cancelled outcome, got: {result:?}"
        );

        let run_state = &projects.get(&key).unwrap().run_state;
        assert!(!run_state.running, "the cancelled run must be finished");
        assert!(
            run_state.dirty,
            "a cancelled context must stay dirty so the remainder re-indexes"
        );
        assert_eq!(
            run_state.pending_scope,
            Some(RunScope::Full),
            "the un-indexed scope must be re-queued for the next pass"
        );
        assert_eq!(
            projects.get(&key).unwrap().last_refresh_state,
            DaemonRefreshState::Pending,
            "a cancelled pass is pending re-index, neither complete nor failed"
        );
    }

    #[tokio::test]
    async fn handle_search_request_uses_safe_unavailable_reason_for_missing_project() {
        let mut projects = HashMap::new();
        let response = handle_search_request(
            &mut projects,
            SearchRequest {
                project_root: PathBuf::from("/tmp/missing-project"),
                source_root: PathBuf::from("/tmp/missing-project"),
                context_id: "missing-context".to_string(),
                query: "needle".to_string(),
                limit: 3,
                path_prefix: None,
            },
        )
        .await;

        assert!(matches!(
            response,
            SearchResponse::Unavailable { ref reason } if reason == "daemon unavailable"
        ));
    }

    /// HYP-002 (CONFIRMED) / T8: per-project-boundary yielding alone is
    /// insufficient because a single project's refresh pass can run for
    /// seconds, far past `DAEMON_READ_TIMEOUT_MS` — the search path must be
    /// decoupled from the sweep's `&mut projects` borrow, not merely
    /// interleaved with it at a coarser boundary.
    ///
    /// This drives `run_unit_while_servicing_search` — the exact seam
    /// `run_project` uses to run its (potentially multi-second) pipeline pass
    /// — with an injected slow unit standing in for that pass (pipeline.rs is
    /// out of scope to modify directly), and asserts a search queued while the
    /// unit is still in flight is served well within `DAEMON_READ_TIMEOUT_MS`.
    /// No embeddings are seeded, so the served response is FTS-only and the
    /// test needs no ONNX model (hermetic, deterministic, no benchmark).
    #[tokio::test]
    async fn search_is_served_while_a_slow_sweep_unit_is_still_in_progress() {
        let tmp = tempfile::tempdir().unwrap();
        let state = file_backed_state(&tmp, &["seg1"]).await;
        let context_id = state.context.context_id.clone();
        let project_root = state.project_root.clone();
        let source_root = state.source_root.clone();
        let mut projects = HashMap::new();
        projects.insert(context_id.clone(), state);

        let (tx, mut search_requests_rx) = mpsc::channel(1);

        // Stands in for `run_project`'s pipeline pass: far longer than
        // `DAEMON_READ_TIMEOUT_MS`, so a search starved until it completes
        // would fail the assertion below.
        let slow_unit = tokio::time::sleep(Duration::from_millis(DAEMON_READ_TIMEOUT_MS * 4));
        let run =
            run_unit_while_servicing_search(slow_unit, &mut projects, &mut search_requests_rx);
        tokio::pin!(run);

        let (respond_to, response_rx) = oneshot::channel();
        tx.send(QueuedSearchRequest {
            request: SearchRequest {
                project_root,
                source_root,
                context_id,
                query: "fn".to_string(),
                limit: 10,
                path_prefix: None,
            },
            respond_to,
        })
        .await
        .unwrap();

        let started = std::time::Instant::now();
        let response = tokio::select! {
            response = response_rx => response.unwrap(),
            _ = &mut run => panic!(
                "the injected slow unit must not resolve before the concurrently \
                 queued search response — search was starved until the sweep finished"
            ),
        };
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_millis(DAEMON_READ_TIMEOUT_MS),
            "search must be served within DAEMON_READ_TIMEOUT_MS while the sweep unit is \
             still in progress, took {elapsed:?}"
        );
        assert!(
            matches!(response, SearchResponse::Results { .. }),
            "expected a served response, got {response:?}"
        );

        // Let the still-pending unit finish so nothing is left dangling.
        run.await;
    }

    #[tokio::test]
    async fn acquire_request_permit_returns_busy_response_when_saturated() {
        let request_limit = Arc::new(Semaphore::new(0));
        let (mut server, mut client) = UnixStream::pair().unwrap();

        let permit = acquire_request_permit(&request_limit, &mut server)
            .await
            .unwrap();
        let response: SearchResponse = crate::daemon::ipc::read_json_frame(
            &mut client,
            crate::shared::constants::MAX_DAEMON_RESPONSE_BYTES,
            Duration::from_millis(250),
        )
        .await
        .unwrap();

        assert!(permit.is_none());
        assert!(matches!(
            response,
            SearchResponse::Unavailable { ref reason } if reason == "daemon busy"
        ));
    }

    #[test]
    fn record_file_check_persists_immediately_and_then_throttles() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join("alpha");
        std::fs::create_dir_all(&project_root).unwrap();
        let project_root = project_root.canonicalize().unwrap();

        let runtime = tokio::runtime::Runtime::new().unwrap();
        let db = runtime.block_on(Db::open_memory()).unwrap();
        let mut state = project_state(&project_root, &project_root, db, ProjectRunState::default());

        let first_check = Utc::now();
        record_file_check(&mut state, first_check, false);
        let status_path = config::project_daemon_status_path(&project_root);
        let first_status: DaemonProjectStatus =
            serde_json::from_str(&std::fs::read_to_string(&status_path).unwrap()).unwrap();
        assert_eq!(first_status.last_file_check_at, first_check);
        assert_eq!(state.last_file_check_persisted_at, Some(first_check));

        let throttled_check = first_check + chrono::Duration::seconds(10);
        record_file_check(&mut state, throttled_check, false);
        let throttled_status: DaemonProjectStatus =
            serde_json::from_str(&std::fs::read_to_string(&status_path).unwrap()).unwrap();
        assert_eq!(throttled_status.last_file_check_at, first_check);
        assert_eq!(state.last_file_check_persisted_at, Some(first_check));

        let next_check = first_check + chrono::Duration::seconds(31);
        record_file_check(&mut state, next_check, false);
        let next_status: DaemonProjectStatus =
            serde_json::from_str(&std::fs::read_to_string(&status_path).unwrap()).unwrap();
        assert_eq!(next_status.last_file_check_at, next_check);
        assert_eq!(state.last_file_check_persisted_at, Some(next_check));

        let context_status_path = daemon_context_status_path(&project_root);
        let context_status: DaemonContextStatusFile =
            serde_json::from_str(&std::fs::read_to_string(context_status_path).unwrap()).unwrap();
        let entry = context_status
            .contexts
            .get(&state.context.context_id)
            .unwrap();
        assert_eq!(entry.watch_status, DaemonWatchStatus::Watching);
        assert_eq!(entry.last_file_check_at, Some(next_check));
    }

    #[test]
    fn record_file_check_force_bypasses_throttle() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join("alpha");
        std::fs::create_dir_all(&project_root).unwrap();
        let project_root = project_root.canonicalize().unwrap();

        let runtime = tokio::runtime::Runtime::new().unwrap();
        let db = runtime.block_on(Db::open_memory()).unwrap();
        let mut state = project_state(&project_root, &project_root, db, ProjectRunState::default());

        let first_check = Utc::now();
        let forced_check = first_check + chrono::Duration::seconds(1);
        record_file_check(&mut state, first_check, false);
        record_file_check(&mut state, forced_check, true);

        let status_path = config::project_daemon_status_path(&project_root);
        let status: DaemonProjectStatus =
            serde_json::from_str(&std::fs::read_to_string(&status_path).unwrap()).unwrap();
        assert_eq!(status.last_file_check_at, forced_check);
        assert_eq!(state.last_file_check_persisted_at, Some(forced_check));
    }

    #[test]
    fn refresh_status_persists_running_and_failed_states() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join("alpha");
        std::fs::create_dir_all(&project_root).unwrap();
        let project_root = project_root.canonicalize().unwrap();

        let runtime = tokio::runtime::Runtime::new().unwrap();
        let db = runtime.block_on(Db::open_memory()).unwrap();
        let mut state = project_state(&project_root, &project_root, db, ProjectRunState::default());

        let started_at = Utc::now();
        mark_refresh_running(&mut state, started_at);
        let status_path = daemon_context_status_path(&project_root);
        let running_status: DaemonContextStatusFile =
            serde_json::from_str(&std::fs::read_to_string(&status_path).unwrap()).unwrap();
        let entry = running_status
            .contexts
            .get(&state.context.context_id)
            .unwrap();
        assert_eq!(entry.last_refresh_state, DaemonRefreshState::Running);
        assert_eq!(entry.last_refresh_started_at, Some(started_at));

        let err: OneupError =
            crate::shared::errors::DaemonError::WatcherError("boom".to_string()).into();
        let finished_at = started_at + chrono::Duration::seconds(1);
        mark_refresh_finished(&mut state, finished_at, Err(&err));
        let failed_status: DaemonContextStatusFile =
            serde_json::from_str(&std::fs::read_to_string(&status_path).unwrap()).unwrap();
        let entry = failed_status
            .contexts
            .get(&state.context.context_id)
            .unwrap();
        assert_eq!(entry.last_refresh_state, DaemonRefreshState::Failed);
        assert_eq!(entry.last_refresh_completed_at, Some(finished_at));
        assert_eq!(
            entry.last_refresh_error.as_deref(),
            Some("daemon error: watcher error: boom")
        );
    }

    use crate::storage::{queries, swap};

    /// Open a file-backed `ProjectState` whose `index.db` holds `segment_ids`,
    /// recording the inode identity exactly as the daemon does on startup. The
    /// project root is canonicalized so the secure-fs path checks and the `flock`
    /// probe in the swap primitive take their real paths on macOS (`/var` ->
    /// `/private/var`).
    async fn file_backed_state(tmp: &tempfile::TempDir, segment_ids: &[&str]) -> ProjectState {
        let root = tmp.path().canonicalize().unwrap().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let db_path = config::project_db_path(&root);
        ensure_secure_project_root(&root).unwrap();

        let db = Db::open_rw(&db_path).await.unwrap();
        let conn = db.connect_tuned().await.unwrap();
        schema::initialize(&conn).await.unwrap();
        for id in segment_ids {
            insert_test_segment(&conn, id).await;
        }
        drop(conn);

        project_state(&root, &root, db, ProjectRunState::default())
    }

    /// Insert a minimal segment row (mirrors the storage/swap test fixture's
    /// NOT-NULL columns) so a generation can be distinguished by its row set.
    async fn insert_test_segment(conn: &libsql::Connection, id: &str) {
        conn.execute(
            "INSERT INTO segments (id, file_path, language, block_type, content, line_start, line_end, complexity, file_hash) \
             VALUES (?1, 'f.rs', 'rust', 'function', 'fn f(){}', 1, 1, 0, 'abc')",
            [id],
        )
        .await
        .unwrap();
    }

    /// Count segments through a connection from `db` (what a daemon pass/search
    /// would observe through its long-lived handle).
    async fn segment_count_via(db: &Db) -> i64 {
        let conn = db.connect().unwrap();
        let mut rows = conn.query(queries::COUNT_SEGMENTS, ()).await.unwrap();
        rows.next().await.unwrap().unwrap().get::<i64>(0).unwrap()
    }

    /// Count segments via an independent fresh read-only open of `index.db` —
    /// i.e. what survives on the *current* inode, independent of any stale handle.
    async fn segment_count_on_disk(index_path: &Path) -> i64 {
        let ro = Db::open_ro(index_path).await.unwrap();
        let conn = ro.connect().unwrap();
        let mut rows = conn.query(queries::COUNT_SEGMENTS, ()).await.unwrap();
        rows.next().await.unwrap().unwrap().get::<i64>(0).unwrap()
    }

    /// Build a finalized, self-contained staged index holding `segment_ids` at
    /// `state_root`'s uuid staging path, ready for `swap::swap_index_into_place`.
    /// The staged file is built in a scratch project (its name is not `index.db`,
    /// which `Db::open_rw` requires) and its single self-contained file is moved
    /// into the staging slot — the same approach the storage/swap tests use.
    async fn staged_index(state_root: &Path, segment_ids: &[&str]) -> PathBuf {
        let scratch = tempfile::tempdir().unwrap();
        let scratch_root = scratch.path().canonicalize().unwrap().join("scratch");
        std::fs::create_dir_all(&scratch_root).unwrap();
        let scratch_index = config::project_db_path(&scratch_root);
        ensure_secure_project_root(&scratch_root).unwrap();

        let db = Db::open_rw(&scratch_index).await.unwrap();
        let conn = db.connect_tuned().await.unwrap();
        schema::initialize(&conn).await.unwrap();
        for id in segment_ids {
            insert_test_segment(&conn, id).await;
        }
        drop(conn);
        swap::finalize_staged_db(db, &scratch_index).await.unwrap();

        let staging = config::project_staging_db_path(state_root);
        std::fs::rename(&scratch_index, &staging).unwrap();
        staging
    }

    /// HYP-002 regression: after a one-shot rebuild swaps the index onto a fresh
    /// inode, the daemon's reopen gate must adopt the new inode so a subsequent
    /// write lands in the refreshed index and is durable — never silently lost
    /// into the orphaned pre-swap inode.
    ///
    /// The decisive assertion is the post-reopen WRITE: with a stale handle the
    /// insert would land in the unlinked old inode and an independent open of
    /// `index.db` would not see it. Through the reopened handle the row is visible
    /// on disk, proving the daemon writes the live index after a swap.
    #[cfg(unix)]
    #[tokio::test]
    async fn reopen_adopts_swapped_index_so_writes_land_in_the_new_inode() {
        let tmp = tempfile::tempdir().unwrap();
        // Prior generation = 2 rows; the daemon holds an open handle to it.
        let mut state = file_backed_state(&tmp, &["old1", "old2"]).await;
        let root = state.project_root.clone();
        let index_path = config::project_db_path(&root);
        let pre_swap_identity = state.index_identity;
        assert_eq!(segment_count_via(&state.db).await, 2);

        // Simulate the one-shot rebuild: a new generation (3 rows) built aside and
        // atomically switched over while holding the single-writer rebuild lock.
        let staging = staged_index(&root, &["new1", "new2", "new3"]).await;
        {
            let _lock = lifecycle::acquire_rebuild_lock(&root).unwrap();
            swap::swap_index_into_place(&root, &staging).await.unwrap();
        }

        // The daemon adopts the swap: the recorded identity changes to the new
        // inode and the handle now reads the new generation.
        reopen_if_index_swapped(&mut state).await.unwrap();
        assert_ne!(
            state.index_identity, pre_swap_identity,
            "reopen must record the new inode identity after a swap"
        );
        assert_eq!(
            segment_count_via(&state.db).await,
            3,
            "the reopened handle must read the swapped-in (new) generation"
        );

        // Decisive HYP-002 guard: a write through the reopened handle is durable
        // on the live inode. A stale handle would write into the orphaned old
        // inode and this independent on-disk read would still see only 3 rows.
        let conn = state.db.connect_tuned().await.unwrap();
        insert_test_segment(&conn, "daemon_write").await;
        drop(conn);
        assert_eq!(
            segment_count_on_disk(&index_path).await,
            4,
            "a post-reopen daemon write must land in the live index, not the orphaned inode"
        );
    }

    /// REQ-006 / T6 AC3: the per-context vector-count cache on `ProjectState`
    /// MUST be invalidated when a build-aside rebuild swaps the index. A stale
    /// count surviving the swap could flip `vector_search_path_for_corpus`
    /// between the exhaustive scan and the ANN path and silently change served
    /// candidates, so `reopen_if_index_swapped` clears it; the next search then
    /// recomputes against the refreshed index.
    #[cfg(unix)]
    #[tokio::test]
    async fn reopen_invalidates_cached_vector_count_after_swap() {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = file_backed_state(&tmp, &["old1", "old2"]).await;
        let root = state.project_root.clone();

        // Prime the cache with a stale count from the pre-swap generation.
        state.cached_vector_count = Some(2);

        let staging = staged_index(&root, &["new1", "new2", "new3"]).await;
        {
            let _lock = lifecycle::acquire_rebuild_lock(&root).unwrap();
            swap::swap_index_into_place(&root, &staging).await.unwrap();
        }

        reopen_if_index_swapped(&mut state).await.unwrap();

        assert_eq!(
            state.cached_vector_count, None,
            "a build-aside swap must invalidate the cached vector count"
        );
    }

    /// REQ-006 / T6 AC3 (no-swap arm): a no-op reopen (no swap) keeps the cached
    /// count untouched so steady-state searches reuse it and never re-`COUNT(*)`.
    #[cfg(unix)]
    #[tokio::test]
    async fn reopen_no_swap_keeps_cached_vector_count() {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = file_backed_state(&tmp, &["old1", "old2"]).await;

        state.cached_vector_count = Some(7);
        reopen_if_index_swapped(&mut state).await.unwrap();

        assert_eq!(
            state.cached_vector_count,
            Some(7),
            "without a swap the cached count must survive a reopen no-op"
        );
    }

    /// Without a swap the reopen gate is a cheap no-op that keeps the warm handle:
    /// the recorded identity is unchanged and the handle is not reopened (a needless
    /// reopen every pass would thrash the connection and drop the warm cache).
    #[cfg(unix)]
    #[tokio::test]
    async fn reopen_is_a_noop_when_the_index_was_not_swapped() {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = file_backed_state(&tmp, &["only1"]).await;
        let identity_before = state.index_identity;

        reopen_if_index_swapped(&mut state).await.unwrap();

        assert_eq!(
            state.index_identity, identity_before,
            "an unswapped index must leave the recorded identity untouched"
        );
        assert_eq!(
            segment_count_via(&state.db).await,
            1,
            "the warm handle must keep serving the unchanged index"
        );
    }

    /// Build a finalized staged index whose recorded schema version is newer than
    /// this binary supports (`SCHEMA_VERSION + 1`), so adopting it must fail the
    /// `ensure_current` gate. Mirrors `staged_index` but rewrites the `meta`
    /// schema-version row before finalizing.
    async fn incompatible_staged_index(state_root: &Path) -> PathBuf {
        let scratch = tempfile::tempdir().unwrap();
        let scratch_root = scratch.path().canonicalize().unwrap().join("scratch");
        std::fs::create_dir_all(&scratch_root).unwrap();
        let scratch_index = config::project_db_path(&scratch_root);
        ensure_secure_project_root(&scratch_root).unwrap();

        let db = Db::open_rw(&scratch_index).await.unwrap();
        let conn = db.connect_tuned().await.unwrap();
        schema::initialize(&conn).await.unwrap();
        // Stamp a version the binary cannot read so `ensure_current` rejects it as
        // "newer than this binary supports".
        conn.execute(
            "UPDATE meta SET value = ?1 WHERE key = 'schema_version'",
            [(crate::shared::constants::SCHEMA_VERSION + 1).to_string()],
        )
        .await
        .unwrap();
        drop(conn);
        swap::finalize_staged_db(db, &scratch_index).await.unwrap();

        let staging = config::project_staging_db_path(state_root);
        std::fs::rename(&scratch_index, &staging).unwrap();
        staging
    }

    /// T7 AC2 / REQ-007: hoisting `ensure_current` out of the per-request path is
    /// sound only because every index (re)open re-runs it. A build-aside swap to
    /// an index whose schema this binary cannot serve MUST fail closed at the
    /// reopen gate (`reopen_if_index_swapped` -> `prepare_for_write` ->
    /// `ensure_current`), before any search is served through the adopted handle.
    #[cfg(unix)]
    #[tokio::test]
    async fn reopen_fails_closed_on_swap_to_incompatible_schema() {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = file_backed_state(&tmp, &["old1", "old2"]).await;
        let root = state.project_root.clone();
        let identity_before = state.index_identity;

        let staging = incompatible_staged_index(&root).await;
        {
            let _lock = lifecycle::acquire_rebuild_lock(&root).unwrap();
            swap::swap_index_into_place(&root, &staging).await.unwrap();
        }

        let result = reopen_if_index_swapped(&mut state).await;
        assert!(
            result.is_err(),
            "adopting a swapped-in index with an unsupported schema must fail closed at the reopen gate"
        );
        // The schema gate is authoritative on swap; nothing is served through the
        // incompatible index.
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("newer than this binary supports"),
            "the reopen failure must be the schema-version rejection, got: {err}"
        );
        // The reopen failure leaves no reused connection bound to the rejected
        // index for a later request to serve through.
        assert!(
            state.read_conn.is_none(),
            "a failed reopen must not leave a reused read connection on the rejected index"
        );
        // `identity_before` is the pre-swap inode; the gate failed before adopting
        // the new one, so callers re-attempt rather than serve a bad index.
        let _ = identity_before;
    }

    /// T7 AC3 / REQ-007: repeated queries on the reused tuned read connection must
    /// return identical results to a fresh per-request connection. Seeds an index,
    /// then asserts that the same query run twice through `ensure_read_conn`'s
    /// reused handle matches the result from an independent fresh `connect_tuned`.
    #[cfg(unix)]
    #[tokio::test]
    async fn reused_read_conn_returns_identical_results_to_per_request_conn() {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = file_backed_state(&tmp, &["seg1", "seg2", "seg3"]).await;
        let scope = SearchScope::from_worktree_context(&state.context);

        // Per-request baseline: a fresh tuned connection, FTS-only (no embeddings
        // seeded), exactly what the prior `state.db.connect()` path produced.
        let fresh = state.db.connect_tuned().await.unwrap();
        let baseline = HybridSearchEngine::new_scoped(&fresh, None, scope.clone())
            .fts_only_search("fn", 10)
            .await
            .unwrap();

        // First reused-connection query lazily creates `read_conn`.
        let conn1 = ensure_read_conn(&mut state).await.unwrap();
        assert!(
            state.read_conn.is_some(),
            "the first search must populate the reused read connection"
        );
        let first = HybridSearchEngine::new_scoped(&conn1, None, scope.clone())
            .fts_only_search("fn", 10)
            .await
            .unwrap();

        // Second reused-connection query must reuse the same handle, not reopen.
        let identity_ptr = state.read_conn.as_ref().map(|c| c as *const _);
        let conn2 = ensure_read_conn(&mut state).await.unwrap();
        assert_eq!(
            state.read_conn.as_ref().map(|c| c as *const _),
            identity_ptr,
            "the second search must reuse the cached connection, not replace it"
        );
        let second = HybridSearchEngine::new_scoped(&conn2, None, scope.clone())
            .fts_only_search("fn", 10)
            .await
            .unwrap();

        let ids = |rows: &[crate::shared::types::SearchResult]| {
            rows.iter()
                .map(|r| (r.segment_id.clone(), r.score))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            ids(&first),
            ids(&baseline),
            "reused-connection results must equal the per-request connection results"
        );
        assert_eq!(
            ids(&second),
            ids(&baseline),
            "repeated reused-connection queries must stay identical to the per-request results"
        );
    }

    fn context_row(context_id: &str, state_root: &str, source_root: &str) -> IndexedContextRow {
        IndexedContextRow {
            context_id: context_id.to_string(),
            state_root: PathBuf::from(state_root),
            source_root: PathBuf::from(source_root),
            branch_name: None,
            updated_at: "2026-01-01 00:00:00".to_string(),
        }
    }

    #[test]
    fn source_missing_selects_only_contexts_whose_source_is_gone() {
        let contexts = [
            context_row("live00000001", "/repo", "/repo"),
            context_row("gone00000001", "/repo", "/repo-feature"),
        ];
        // Only `/repo-feature` is gone; the live `/repo` context is retained.
        let pruned = source_missing_context_ids(&contexts, &|p| p != Path::new("/repo-feature"));
        assert_eq!(pruned, vec!["gone00000001".to_string()]);
    }

    #[test]
    fn source_present_contexts_are_never_selected() {
        // A live worktree plus a same-state_root, other-branch snapshot of it: both
        // have a present source, so the startup prune leaves both alone (unlike
        // `1up gc`, which would treat the snapshot as stale).
        let contexts = [
            context_row("active000001", "/repo", "/repo"),
            context_row("oldbranch001", "/repo", "/repo"),
        ];
        assert!(source_missing_context_ids(&contexts, &|_| true).is_empty());
    }

    #[test]
    fn source_missing_on_empty_input_is_empty() {
        assert!(source_missing_context_ids(&[], &|_| true).is_empty());
        assert!(source_missing_context_ids(&[], &|_| false).is_empty());
    }

    #[test]
    fn source_missing_selects_every_gone_context_and_keeps_the_live_one() {
        let contexts = [
            context_row("gone00000001", "/repo", "/wt-a"),
            context_row("live00000001", "/repo", "/repo"),
            context_row("gone00000002", "/repo", "/wt-b"),
        ];
        // Every context whose source is absent is selected, order-preserved; the
        // single live context is the only one retained.
        let pruned = source_missing_context_ids(&contexts, &|p| p == Path::new("/repo"));
        assert_eq!(
            pruned,
            vec!["gone00000001".to_string(), "gone00000002".to_string()]
        );
    }
}
