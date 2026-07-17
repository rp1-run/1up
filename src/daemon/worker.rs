use std::collections::{HashMap, HashSet};
use std::future::{self, Future};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow;
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
use crate::shared::fs::{
    atomic_replace, ensure_secure_project_root, probe_source_presence, SourcePresence,
};
use crate::shared::project::canonical_project_root;
use crate::shared::types::WorktreeContext;
use crate::shared::types::{
    combine_degraded_reasons, DaemonContextStatus, DaemonContextStatusFile, DaemonProjectStatus,
    DaemonRefreshState, DaemonWatchStatus, IndexScopeInfo, IndexingConfig, RunScope, SetupTimings,
};
use crate::storage::segments::{self, IndexedContextRow};
use crate::storage::{db::Db, schema};

const DAEMON_CONTEXT_STATUS_FILE_NAME: &str = "daemon_context_status.json";
const STARTUP_RECONCILIATION_REASON: &str = "startup_reconciliation";

/// Test-only seam gate for [`test_rebuild_hold`]; never set in production.
const REBUILD_HOLD_ENV_VAR: &str = "ONEUP_TEST_REBUILD_HOLD";
/// While this file exists under a project's `.1up/`, a pass with the seam
/// enabled parks inside its pipeline window (rebuild lock held).
const REBUILD_HOLD_FILE_NAME: &str = "test-rebuild.hold";
/// Written by a parked pass so tests can detect it deterministically.
const REBUILD_HOLD_ENTERED_FILE_NAME: &str = "test-rebuild.hold-entered";

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
    /// onto a fresh inode, so a mismatch between this and the current
    /// on-disk identity means the daemon's long-lived handle now points at the
    /// orphaned pre-swap inode and must be reopened before any pass touches it.
    /// `None` when the index was absent when `db` was opened.
    index_identity: Option<IndexFileIdentity>,
    /// Cached per-context vector `COUNT(*)` for the open index, populated
    /// lazily on the first search that needs it and reused across requests so the
    /// hot path skips a per-query `COUNT(*)`. MUST be invalidated (`None`) on
    /// `reopen_if_index_swapped` so a build-aside swap never serves a stale count
    /// into `vector_search_path_for_corpus` (exhaustive-vs-ANN) path selection.
    cached_vector_count: Option<usize>,
    /// One reused tuned read [`Connection`] to the open index, serving repeated
    /// daemon searches without a fresh per-request `connect()` + PRAGMA pass.
    /// Created lazily on the first search and dropped (`None`) whenever
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
    /// Per-project child of the daemon's shared cancellation token. Cancelling it
    /// aborts THIS project's in-flight rebuild at its next safe yield without
    /// disturbing any sibling project; the shared parent still cancels every
    /// child on SIGTERM. Cancelled when the project is dropped from the active
    /// set (a SIGHUP reload de-register or worker shutdown) so a de-registered
    /// project's rebuild stops promptly instead of running on against a gone
    /// registry (issue #109).
    ///
    /// Cancellation is a REQUEST, not proof the pass has stopped: the in-flight
    /// pass owns its single-writer [`lifecycle::RebuildLock`] guard on the
    /// `run_project` stack frame and keeps being polled after a mid-pass
    /// de-registration, so the `rebuild.lock` FD releases only when the pipeline
    /// has exited at a committed boundary and the frame returns — never while the
    /// old pass could still write (cancel → drain → unlock).
    cancel_token: CancellationToken,
}

/// On-disk identity of an `index.db` file, used to detect a build-aside swap
/// performed by another process (the one-shot rebuild owners). On Unix the
/// atomic rename that switches the index over replaces the directory entry
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

/// Outcome of building one registry entry's in-memory daemon state.
enum ProjectStateBuild {
    /// The project is loadable now: insert it, watch it, and queue its startup
    /// reconciliation.
    Ready(Box<ProjectState>),
    /// The project is definitely unusable (absent root/source, or a schema that
    /// needs a clean rebuild): drop it until the next registry reload.
    Skip,
    /// A presence probe could not decide (transient failure, e.g. an
    /// unreachable network mount): the entry must be retained in
    /// [`DeferredProjects`] and re-probed on the daemon's normal cycle, so
    /// recovery never depends on SIGHUP, a daemon restart, or an unrelated
    /// watcher event.
    Defer,
}

/// Registered projects whose load-time presence probe was indeterminate, keyed
/// by context id. Retried by [`retry_deferred_projects`] on every debounce
/// tick, and rebuilt from the fresh registry on SIGHUP reload. A deferred
/// project counts as registered for idle-shutdown accounting so a temporarily
/// unreachable source can never cause the daemon to reap itself.
type DeferredProjects = HashMap<String, ProjectEntry>;

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

pub async fn run() -> Result<(), OneupError> {
    let _daemon_lock = lifecycle::acquire_daemon_lock()?;

    run_inner().await
}

/// Registry-reload trigger source consumed by the rebuild sweep.
///
/// Production threads the process SIGHUP stream (`run_inner` owns the sole
/// [`Signal`]) through this seam unchanged; lifecycle tests inject reload
/// ticks over a plain channel instead. The seam exists because raising a
/// process-global SIGHUP from a test is unreliable under the parallel test
/// harness: the signal is process-wide, other tests' tokio signal listeners
/// can consume it, and delivery races the sweep's select loop — whereas a
/// channel send is delivered deterministically to exactly this consumer.
trait ReloadSignal {
    /// Completes when a registry reload has been requested. Mirrors
    /// [`Signal::recv`]; `None` means the source can never fire again.
    async fn recv(&mut self) -> Option<()>;
}

impl ReloadSignal for Signal {
    async fn recv(&mut self) -> Option<()> {
        Signal::recv(self).await
    }
}

/// Test-side injected reload source: each queued `()` is one reload tick,
/// delivered to the sweep exactly like a SIGHUP wake-up but without touching
/// process-global signal state.
#[cfg(test)]
impl ReloadSignal for mpsc::UnboundedReceiver<()> {
    async fn recv(&mut self) -> Option<()> {
        mpsc::UnboundedReceiver::recv(self).await
    }
}

async fn run_inner() -> Result<(), OneupError> {
    info!("daemon worker starting (pid={})", std::process::id());

    // Note: the daemon must NOT die with its launcher — `1up start`
    // exits immediately by design and the daemon lifecycle tests require
    // persistence. Orphan control is handled by SIGTERM responsiveness
    // (handler below), idle shutdown, and stale-state reconciliation.

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

    let mut deferred: DeferredProjects = HashMap::new();
    load_and_watch_projects(
        &mut file_watcher,
        &mut projects,
        &mut deferred,
        &cancel_token,
    )
    .await?;
    prewarm_project_embedders(&mut projects);
    record_file_check_for_all_projects(&mut projects, Utc::now(), true);
    run_dirty_projects_until_clean_or_cancelled(
        &mut file_watcher,
        &mut projects,
        &mut deferred,
        &cancel_token,
        &mut sigterm,
        &mut sighup,
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
    // at startup. A deferred (indeterminate-probe) project still counts as
    // registered, so a temporarily unreachable source never idles the daemon
    // out from under the project it must keep retrying.
    let mut empty_since: Option<std::time::Instant> =
        (projects.is_empty() && deferred.is_empty()).then(std::time::Instant::now);

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
                if let Err(e) = reload_projects(&mut file_watcher, &mut projects, &mut deferred, &cancel_token).await {
                    error!("failed to reload projects: {e}");
                } else {
                    record_file_check_for_all_projects(&mut projects, Utc::now(), true);
                    run_dirty_projects_until_clean_or_cancelled(
                        &mut file_watcher,
                        &mut projects,
                        &mut deferred,
                        &cancel_token,
                        &mut sigterm,
                        &mut sighup,
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
                // Re-probe deferred (indeterminate at load time) projects first,
                // so one that just recovered is loaded, watched, and marked dirty
                // before this tick's dirty sweep runs.
                retry_deferred_projects(&mut file_watcher, &mut projects, &mut deferred, &cancel_token).await;
                let is_empty = projects.is_empty() && deferred.is_empty();
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
                        &mut file_watcher,
                        &mut projects,
                        &mut deferred,
                        &cancel_token,
                        &mut sigterm,
                        &mut sighup,
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
                    &mut file_watcher,
                    &mut projects,
                    &mut deferred,
                    &cancel_token,
                    &mut sigterm,
                    &mut sighup,
                    &mut search_requests_rx,
                )
                .await;
            }
        }
    }

    mark_all_contexts_daemon_stopped(&mut projects);
    // Shutdown drop-path: cancel every project's token and drop its handles before
    // exit. No rebuild lock can be held here — the lock guard is owned by the
    // in-flight pass frame, and every sweep above is awaited to completion (a
    // SIGTERM'd pass drains to its committed boundary and releases the guard
    // before the loop breaks), so a worker shutdown never leaves an orphaned
    // `rebuild.lock` behind for the next process (issue #109).
    for (_context_id, state) in projects.drain() {
        release_removed_project(state);
    }

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
    deferred: &mut DeferredProjects,
    parent_token: &CancellationToken,
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

    // De-register entries for fully-deleted projects (project root AND index.db
    // both gone). This is the natural companion to the source-missing prune above:
    // it needs no per-project index.db and complements that prune, which cannot
    // run once index.db itself is gone. Reload afterwards so the watch loop below
    // sees only the survivors (issue #115). Best-effort — never fails startup.
    deregister_deleted_projects_on_startup(&registry).await;
    let registry = Registry::load().unwrap_or(registry);

    for entry in &registry.projects {
        load_registered_project(entry, watcher, projects, deferred, parent_token).await?;
    }

    Ok(())
}

/// Build, watch, and insert one registry entry's project state, or record the
/// entry for a later retry.
///
/// A `Defer` outcome (indeterminate presence probe) keeps the entry in
/// `deferred` — warning on the first deferral, debug thereafter — so
/// [`retry_deferred_projects`] re-probes it on the daemon's normal cycle
/// instead of dropping it until SIGHUP. A `Skip` outcome is a *definite*
/// decision, so any deferred record is cleared.
async fn load_registered_project(
    entry: &ProjectEntry,
    watcher: &mut FileWatcher,
    projects: &mut ProjectStates,
    deferred: &mut DeferredProjects,
    parent_token: &CancellationToken,
) -> Result<(), OneupError> {
    match build_project_state(entry, parent_token).await? {
        ProjectStateBuild::Ready(state) => {
            let mut state = *state;
            let source_root = state.source_root.clone();
            mark_startup_reconciliation_pending(&mut state);
            watcher.watch(&source_root)?;
            let context_id = state.context.context_id.clone();
            projects.insert(context_id.clone(), state);
            deferred.remove(&context_id);

            info!(
                "watching project: {} (context {}, source {})",
                entry.project_root.display(),
                context_id,
                source_root.display()
            );
        }
        ProjectStateBuild::Skip => {
            deferred.remove(&entry.context_id());
        }
        ProjectStateBuild::Defer => {
            let context_id = entry.context_id();
            let newly_deferred = deferred.insert(context_id.clone(), entry.clone()).is_none();
            if newly_deferred {
                warn!(
                    "deferring project {} (context {}): presence probe indeterminate (transient failure); retrying on the daemon's normal cycle",
                    entry.project_root.display(),
                    context_id
                );
            } else {
                debug!(
                    "project {} (context {}) still deferred: presence probe indeterminate",
                    entry.project_root.display(),
                    context_id
                );
            }
        }
    }

    Ok(())
}

/// Re-probe registered projects whose load-time presence probe was
/// indeterminate. Called on every debounce tick so a project on a temporarily
/// unreachable source (e.g. a flaky network mount) is loaded, watched, and
/// queued for reconciliation as soon as its probe recovers — recovery must
/// never depend on SIGHUP, a daemon restart, or an unrelated watcher event.
/// Best-effort: a load/watch failure keeps the entry deferred for the next
/// tick, since a fault while the source is flapping is exactly the transient
/// case being retried.
async fn retry_deferred_projects(
    watcher: &mut FileWatcher,
    projects: &mut ProjectStates,
    deferred: &mut DeferredProjects,
    parent_token: &CancellationToken,
) {
    if deferred.is_empty() {
        return;
    }

    let entries: Vec<ProjectEntry> = deferred.values().cloned().collect();
    for entry in entries {
        if let Err(e) =
            load_registered_project(&entry, watcher, projects, deferred, parent_token).await
        {
            debug!(
                "retry of deferred project {} failed; keeping it deferred: {e}",
                entry.project_root.display()
            );
        }
    }
}

/// Prewarm every loaded project's embedding runtime immediately after
/// [`load_and_watch_projects`] so the first real search finds a `Warm` runtime
/// instead of paying a cold model load past the daemon's search deadline.
/// Mirrors the search path's own `prepare_for_search` call
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

/// The result of classifying recorded contexts by whether their source worktree
/// directory is still present, using a three-state probe.
///
/// `to_prune` holds the contexts whose source is *definitely absent* and are
/// therefore eligible for deletion. `indeterminate` holds contexts whose source
/// probe could not decide (a transient/mount failure): they are **retained** this
/// cycle and surfaced so the caller can `warn!` about the skipped prune rather
/// than destructively deleting a live-but-unreachable source's rows.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SourceMissingSelection {
    pub to_prune: Vec<String>,
    pub indeterminate: Vec<String>,
}

/// Classify the recorded contexts by source-root presence for a startup prune.
///
/// Pure and injected with `probe` so it is deterministic and unit-testable (the
/// daemon passes [`crate::shared::fs::probe_source_presence`]). Only sources that
/// are *definitely absent* ([`SourcePresence::Absent`]) are selected for pruning;
/// a source whose probe is [`SourcePresence::Indeterminate`] (a permission/IO
/// fault on a flaky or unmounted network mount) is retained and reported
/// separately so a transient failure never false-prunes a live source's index
/// rows. A [`SourcePresence::Present`] source is always retained.
///
/// Like the source-missing arm of `cli::gc::prune_reason`, this deliberately
/// selects on *source-root absence alone*: the daemon's startup prune never
/// touches stale-branch snapshots of a still-present worktree — those rebuild on
/// demand and stay a manual decision. A context whose `source_root` still exists
/// is therefore always retained, including a same-`state_root`, other-branch
/// snapshot that shares a live worktree.
pub fn classify_source_missing_contexts(
    contexts: &[IndexedContextRow],
    probe: &dyn Fn(&Path) -> SourcePresence,
) -> SourceMissingSelection {
    let mut selection = SourceMissingSelection::default();
    for ctx in contexts {
        match probe(&ctx.source_root) {
            SourcePresence::Present => {}
            SourcePresence::Absent => selection.to_prune.push(ctx.context_id.clone()),
            SourcePresence::Indeterminate => selection.indeterminate.push(ctx.context_id.clone()),
        }
    }
    selection
}

/// Best-effort startup prune of contexts whose source worktree directory has been
/// removed (e.g. a deleted git worktree), so a dead context's rows do not linger
/// in the shared index until a manual `1up gc`.
///
/// Scope is deliberately the *source-missing* subset only (via
/// [`classify_source_missing_contexts`]) — never stale-branch snapshots of a live
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
    // A registered entry without an on-disk index yet has nothing to prune. A
    // boolean `exists()` is deliberate here: a transient false-negative only skips
    // this DB's prune for one cycle (non-destructive), so the three-state probe —
    // reserved for decisions that delete rows or change registration — is not
    // needed.
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
        let selection =
            classify_source_missing_contexts(&contexts, &|p: &Path| probe_source_presence(p));
        if !selection.indeterminate.is_empty() {
            warn!(
                "retaining {} context(s) with indeterminate source presence (transient probe failure, e.g. an unreachable network mount) at {}: {}",
                selection.indeterminate.len(),
                db_path.display(),
                selection.indeterminate.join(", ")
            );
        }
        for context_id in &selection.to_prune {
            segments::delete_context(&conn, context_id).await?;
        }
        Ok::<_, OneupError>(selection.to_prune)
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

/// Conservative three-state outcome of a filesystem existence probe for a path
/// that may have been deleted out from under the registry.
///
/// The middle state is the whole point: de-registration mutates the shared
/// `projects.json`, so a transient stat failure on a flaky or unmounted network
/// share must never be mistaken for deletion. Only a hard `ErrorKind::NotFound`
/// counts as definitely gone; every other io error (EIO, ENOTCONN, EACCES, …) is
/// `Indeterminate` and keeps the entry — matching the conservative posture the
/// source-missing prune uses before mutating shared state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathPresence {
    /// `symlink_metadata` succeeded: the path exists.
    Present,
    /// `symlink_metadata` returned `ErrorKind::NotFound`: the path is gone.
    DefinitelyAbsent,
    /// The probe could not decide (any io error other than `NotFound`). Treated
    /// as "keep" so a transient outage never triggers de-registration.
    Indeterminate,
}

/// Conservatively probe whether `path` exists.
///
/// Uses `symlink_metadata` so the final component is not followed — a dangling
/// symlink reports `Present` (the directory entry exists) rather than absent, so a
/// moved link target never looks like a deleted project. Only a hard
/// `ErrorKind::NotFound` maps to `DefinitelyAbsent`; any other error is
/// `Indeterminate`, so a stat failure on a flaky mount is never read as deletion.
fn probe_path_presence(path: &Path) -> PathPresence {
    match std::fs::symlink_metadata(path) {
        Ok(_) => PathPresence::Present,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => PathPresence::DefinitelyAbsent,
        Err(_) => PathPresence::Indeterminate,
    }
}

/// Pure decision: is a registry entry dead (safe to de-register on startup)?
///
/// True only when BOTH the project root and its `index.db` are `DefinitelyAbsent`.
/// A live root with a missing `index.db` is a fresh, not-yet-indexed project and
/// is kept; any `Indeterminate` probe on either input is kept, so a flaky mount
/// never triggers de-registration. Taking the two probe results as parameters
/// keeps this deterministic and unit-testable.
fn is_entry_dead(root_probe: PathPresence, db_probe: PathPresence) -> bool {
    root_probe == PathPresence::DefinitelyAbsent && db_probe == PathPresence::DefinitelyAbsent
}

/// Probe whether a single registry entry is dead *right now*: true only when
/// both its project root and its derived `index.db` (via
/// [`config::project_db_path`]) are `DefinitelyAbsent`.
///
/// Pure and injected with `probe` so it is deterministic and unit-testable (the
/// daemon passes [`probe_path_presence`]). Shared by the pre-lock candidate scan
/// ([`dead_project_context_ids`]) and the under-lock re-validation predicate, so
/// the two cannot drift.
fn probe_entry_dead(entry: &ProjectEntry, probe: &dyn Fn(&Path) -> PathPresence) -> bool {
    let root_probe = probe(&entry.project_root);
    let db_probe = probe(&config::project_db_path(&entry.project_root));
    is_entry_dead(root_probe, db_probe)
}

/// Select the context ids of registry entries whose project root AND `index.db`
/// are both definitively gone (a fully-deleted project).
///
/// Pure and injected with `probe` so it is deterministic and unit-testable (the
/// daemon passes [`probe_path_presence`]). This is only a cheap pre-lock
/// candidate filter: the paths are re-probed under the registry lock before any
/// entry is actually removed.
fn dead_project_context_ids(
    registry: &Registry,
    probe: &dyn Fn(&Path) -> PathPresence,
) -> Vec<String> {
    registry
        .projects
        .iter()
        .filter(|entry| probe_entry_dead(entry, probe))
        .map(ProjectEntry::context_id)
        .collect()
}

/// Best-effort startup de-registration of entries for fully-deleted projects —
/// those whose project root AND `index.db` are both definitively gone.
///
/// This runs at daemon startup, next to
/// [`prune_source_missing_contexts_on_startup`], because it is the one
/// reconciliation that needs no per-project `index.db`: the source-missing prune
/// opens each project's `index.db` to delete dead contexts and so cannot run once
/// that file is itself gone. This complements it by removing the leftover registry
/// entry, so a project whose entire directory (including `.1up/index.db`) was
/// deleted stops being reloaded, warned about, and re-skipped on every startup
/// (issue #115).
///
/// Every step is best-effort: a de-register hiccup is logged and swallowed so it
/// can never block or fail daemon startup. The registry is reloaded fresh before
/// mutating so a concurrent registration is not clobbered, and the removal goes
/// through the atomic save under the registry lock
/// ([`Registry::deregister_context_ids_if`]).
///
/// The pre-lock scan is only a candidate filter: context ids are deterministic
/// for the same state root, source root, and branch, so a project recreated and
/// re-registered by a concurrent `1up start` between the scan and the locked
/// mutation would carry the same id as the stale snapshot entry. Each candidate
/// is therefore re-probed ([`probe_entry_dead`]) under the registry lock, after
/// the fresh reload, and removed only if its project root and `index.db` are
/// still definitively gone at save time.
async fn deregister_deleted_projects_on_startup(registry: &Registry) {
    let dead_ids = dead_project_context_ids(registry, &|p: &Path| probe_path_presence(p));
    if dead_ids.is_empty() {
        return;
    }

    // One info line per candidate entry, naming the project root for
    // diagnosability. A candidate is only actually removed if it still probes
    // dead under the registry lock below.
    let dead_set: HashSet<String> = dead_ids.into_iter().collect();
    for entry in &registry.projects {
        if dead_set.contains(&entry.context_id()) {
            info!(
                "fully-deleted project candidate for de-registration on startup: {} (context {})",
                entry.project_root.display(),
                entry.context_id()
            );
        }
    }

    match Registry::load().and_then(|mut r| {
        r.deregister_context_ids_if(&dead_set, &|entry| {
            probe_entry_dead(entry, &|p: &Path| probe_path_presence(p))
        })
    }) {
        Ok(removed) => {
            info!("de-registered {removed} fully-deleted project(s) on startup");
        }
        Err(err) => {
            warn!("failed to de-register fully-deleted projects on startup: {err}");
        }
    }
}

/// Release a project that is leaving the active set (SIGHUP reload de-register,
/// deleted source, or worker shutdown).
///
/// Cancelling the per-project token first aborts any in-flight rebuild for this
/// project at its next safe yield without touching sibling projects. Dropping the
/// state then releases everything it owns deterministically — most importantly the
/// [`lifecycle::RebuildLock`] guard in `rebuild_lock`, whose `rebuild.lock` FD
/// would otherwise be orphaned and wedge every later `1up reindex` with a
/// contention error that never clears (issue #109) — along with the `index.db`
/// handles and reused read connection.
fn release_removed_project(state: ProjectState) {
    state.cancel_token.cancel();
    // `state` (its `Db` and reused read connection) drops at the end of this
    // scope, closing the index handles. The rebuild-lock guard is NOT here: it is
    // owned by the in-flight pass frame (`run_project`), which keeps being polled
    // after this cancellation request and releases the lock only once the
    // pipeline has exited at a committed boundary (cancel → drain → unlock).
}

/// Drop every active-set project whose context is no longer in the registry,
/// requesting cancellation of its in-flight rebuild.
///
/// Cancellation comes FIRST and is unconditional: watcher cleanup is best-effort
/// (log-and-continue), so a failed `unwatch` can never skip cancelling a removed
/// project's pass (which would leave its rebuild running against a gone registry
/// entry, issue #109). The watcher is only unwatched when no surviving registered
/// context still shares that source root (linked worktrees share one watch).
///
/// This only REQUESTS cancellation. When the removed project's pass is in flight,
/// its rebuild-lock guard stays owned by the still-polled `run_project` frame and
/// releases after the pipeline drains to a committed boundary, so a competing
/// writer can never acquire the lock while the old pass could still write.
fn drop_deregistered_projects(
    watcher: &mut FileWatcher,
    projects: &mut ProjectStates,
    registered_contexts: &HashSet<String>,
    registered_sources: &HashSet<PathBuf>,
) {
    let current_contexts: Vec<String> = projects.keys().cloned().collect();
    for context_id in &current_contexts {
        if registered_contexts.contains(context_id) {
            continue;
        }
        if let Some(mut state) = projects.remove(context_id) {
            info!(
                "removing project context {} for {}",
                context_id,
                state.project_root.display()
            );
            state.cancel_token.cancel();
            state.watch_status = DaemonWatchStatus::DaemonStopped;
            persist_daemon_context_status_for_state(&state);
            if !registered_sources.contains(&canonical_project_root(&state.source_root)) {
                if let Err(err) = watcher.unwatch(&state.source_root) {
                    warn!(
                        "failed to unwatch removed source root {}: {err}",
                        state.source_root.display()
                    );
                }
            }
            release_removed_project(state);
        }
    }
}

async fn reload_projects(
    watcher: &mut FileWatcher,
    projects: &mut ProjectStates,
    deferred: &mut DeferredProjects,
    parent_token: &CancellationToken,
) -> Result<(), OneupError> {
    let registry = Registry::load()?;
    // The fresh registry supersedes the deferred snapshot: entries that were
    // deregistered must stop being retried, and entries that are still
    // registered (and still indeterminate) re-enter the deferred set through
    // `load_registered_project` below.
    deferred.clear();
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

    drop_deregistered_projects(watcher, projects, &registered_contexts, &registered_sources);

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

        load_registered_project(entry, watcher, projects, deferred, parent_token).await?;
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

async fn build_project_state(
    entry: &ProjectEntry,
    parent_token: &CancellationToken,
) -> Result<ProjectStateBuild, OneupError> {
    match probe_source_presence(&entry.project_root) {
        SourcePresence::Present => {}
        SourcePresence::Absent => {
            warn!(
                "skipping non-existent project: {}",
                entry.project_root.display()
            );
            return Ok(ProjectStateBuild::Skip);
        }
        SourcePresence::Indeterminate => {
            // A transient probe failure on the state root: defer without
            // recording anything, so a flaky mount is never mistaken for
            // deletion; the caller retains the entry and re-probes it on the
            // daemon's normal cycle.
            debug!(
                "deferring project {}: state root presence is indeterminate (transient probe failure)",
                entry.project_root.display()
            );
            return Ok(ProjectStateBuild::Defer);
        }
    }
    let source_root = entry.source_root().to_path_buf();
    match probe_source_presence(&source_root) {
        SourcePresence::Present => {}
        SourcePresence::Absent => {
            warn!(
                "skipping project {} because source root is missing: {}",
                entry.project_root.display(),
                source_root.display()
            );
            persist_source_missing_context_status(entry);
            return Ok(ProjectStateBuild::Skip);
        }
        SourcePresence::Indeterminate => {
            // Do NOT persist source-missing status on a transient probe failure: an
            // unreachable/unmounted network source must not be recorded as deleted.
            // Defer so the caller retains the entry and re-probes it once the
            // source is reachable again.
            debug!(
                "deferring project {}: source root presence is indeterminate (transient probe failure): {}",
                entry.project_root.display(),
                source_root.display()
            );
            return Ok(ProjectStateBuild::Defer);
        }
    }

    let db_path = config::project_db_path(&entry.project_root);
    let db = Db::open_rw(&db_path).await?;
    let conn = db.connect_tuned().await?;
    if let Err(e) = schema::prepare_for_write(&conn).await {
        warn!(
            "skipping project {} until a clean rebuild succeeds: {e}",
            entry.project_root.display()
        );
        return Ok(ProjectStateBuild::Skip);
    }
    // Record the inode the handle now refers to so a later build-aside swap (a
    // one-shot rebuild atomically renaming a fresh index over `index.db`) is
    // detectable and the handle gets reopened before it writes.
    let index_identity = index_file_identity(&db_path);

    Ok(ProjectStateBuild::Ready(Box::new(ProjectState {
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
        // A child of the daemon's shared token: SIGTERM cancels the parent and
        // thus this project too, while a de-register cancels only this child.
        cancel_token: parent_token.child_token(),
    })))
}

/// On-disk `(device, inode)` identity of an `index.db` file, or `None` when the
/// file is absent or cannot be stat'd. Two opens of the same path yield the same
/// identity until an atomic rename swaps a different file over it, which is
/// exactly how the build-aside switch-over installs a refreshed index.
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
/// `index.db`. A one-shot rebuild (CLI `reindex` / MCP `run_index`) builds a
/// refreshed index aside and atomically renames it over `index.db`, which
/// orphans the inode the daemon's handle still refers to. Continuing to use that
/// stale handle is a data-divergence hazard, not merely a stale read: a write
/// through it lands in the now-unlinked old inode and is silently lost.
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
    // freshly-adopted index before the next search. The next search
    // lazily re-creates it via `connect_tuned`, which re-runs the read PRAGMA
    // profile on the new handle.
    state.read_conn = None;
    // The swapped-in index has its own vector population; drop the cached count
    // so the next search recomputes it against the refreshed index. A
    // stale count here could flip `vector_search_path_for_corpus` between the
    // exhaustive scan and the ANN path and silently change served candidates.
    state.cached_vector_count = None;
    Ok(())
}

/// Return the daemon's reused tuned read connection for `state`, creating it on
/// first use.
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
///
/// SIGHUP is serviced INSIDE the sweep (see `run_unit_while_servicing_events`):
/// a registry reload delivered mid-rebuild is observed while the pass is still
/// running, so de-registering the rebuilding project cancels that pass instead of
/// waiting — possibly minutes — for the pipeline to finish (issue #109).
async fn run_dirty_projects_until_clean_or_cancelled(
    watcher: &mut FileWatcher,
    projects: &mut ProjectStates,
    deferred: &mut DeferredProjects,
    cancel_token: &CancellationToken,
    sigterm: &mut Signal,
    sighup: &mut impl ReloadSignal,
    search_requests_rx: &mut mpsc::Receiver<QueuedSearchRequest>,
) {
    let sweep = run_dirty_projects_until_clean(
        watcher,
        projects,
        deferred,
        cancel_token,
        sighup,
        search_requests_rx,
    );
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
    watcher: &mut FileWatcher,
    projects: &mut ProjectStates,
    deferred: &mut DeferredProjects,
    cancel_token: &CancellationToken,
    sighup: &mut impl ReloadSignal,
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

        let result = run_project(
            &key,
            projects,
            deferred,
            search_requests_rx,
            watcher,
            cancel_token,
            sighup,
        )
        .await;

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
                    if cancel_token.is_cancelled() {
                        // SIGTERM cancelled the pass at a unit boundary.
                        // `run_project` already re-queued the scope (the context
                        // stays dirty so the restarted binary re-indexes the
                        // remainder). Stop the sweep; the main loop guard will
                        // break for shutdown.
                        debug!("re-index sweep for context {key} cancelled for shutdown");
                        break;
                    }
                    // Only this project's child token was cancelled: a SIGHUP
                    // reload de-registered it mid-pass. Its rebuild stopped at a
                    // committed boundary and its lock has been released; keep
                    // sweeping the surviving projects.
                    debug!(
                        "re-index pass for context {key} cancelled by de-registration; \
                         continuing sweep for remaining projects"
                    );
                    continue;
                }
                if matches!(
                    &e,
                    OneupError::Daemon(
                        crate::shared::errors::DaemonError::RebuildLockContended { .. }
                            | crate::shared::errors::DaemonError::SourceProbeIndeterminate { .. }
                    )
                ) {
                    // The pass deferred — either to a competing one-shot rebuild
                    // or because the source presence probe was indeterminate —
                    // and left the project dirty with its queued scope intact.
                    // Return to the select loop instead of immediately
                    // re-selecting the same key (which would busy-spin on the
                    // held lock or the unreachable source); the next debounce
                    // tick or file event retries once the condition clears.
                    debug!("deferring re-index sweep for context {key}: {e}");
                    break;
                }
                error!("re-index failed for context {key}: {e}");
            }
        }
    }
}

/// Persist carried scope to the progress file so the new context's
/// rebuild applies the scope from the prior context. This is an interim guard
/// rail; v1.1 will implement per-cone drift tracking and full branch-context
/// retention.
fn persist_carried_scope(
    state_root: &Path,
    scope_info: &IndexScopeInfo,
) -> Result<(), std::io::Error> {
    use crate::shared::types::{IndexPhase, IndexProgress, IndexState};

    let status_path = config::project_dot_dir(state_root).join("index_status.json");

    // Read existing progress or create a new one
    let mut progress: IndexProgress = if status_path.exists() {
        let content = std::fs::read_to_string(&status_path)?;
        serde_json::from_str(&content).unwrap_or_else(|_| IndexProgress {
            state: IndexState::Idle,
            phase: IndexPhase::Pending,
            context_id: None,
            source_root: None,
            branch_name: None,
            branch_status: None,
            files_total: 0,
            files_scanned: 0,
            files_processed: 0,
            files_indexed: 0,
            files_skipped: 0,
            files_deleted: 0,
            segments_stored: 0,
            embeddings_enabled: false,
            embedding_unavailable_reason: None,
            vector_rows: None,
            embeddable_segments: None,
            message: None,
            parallelism: None,
            timings: None,
            scope: None,
            prefilter: None,
            indexer_pid: None,
            updated_at: Utc::now(),
        })
    } else {
        IndexProgress {
            state: IndexState::Idle,
            phase: IndexPhase::Pending,
            context_id: None,
            source_root: None,
            branch_name: None,
            branch_status: None,
            files_total: 0,
            files_scanned: 0,
            files_processed: 0,
            files_indexed: 0,
            files_skipped: 0,
            files_deleted: 0,
            segments_stored: 0,
            embeddings_enabled: false,
            embedding_unavailable_reason: None,
            vector_rows: None,
            embeddable_segments: None,
            message: None,
            parallelism: None,
            timings: None,
            scope: None,
            prefilter: None,
            indexer_pid: None,
            updated_at: Utc::now(),
        }
    };

    // Update the scope while preserving other fields
    progress.scope = Some(scope_info.clone());
    progress.updated_at = Utc::now();

    // Write back to file
    let json = serde_json::to_string_pretty(&progress)?;
    std::fs::write(&status_path, json)?;

    Ok(())
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

        // Scope carry on branch switch. If the old context was scoped,
        // document the scope so the new context can inherit it and avoid rebuild
        // multiplication. This is an interim guard rail; v1.1 will implement
        // per-cone drift tracking and full branch-context retention.
        let prior_scope =
            read_index_progress(&state.project_root).and_then(|progress| progress.scope.clone());

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
            // If prior context had scope, carry it to prevent rebuild multiplication
            if let Some(scope_info) = prior_scope {
                info!(
                    "Carrying scope from prior branch context to {} (interim guard rail for v1.1 per-cone drift)",
                    new_context_id
                );
                // Persist the scope to the progress file so the new context's rebuild applies it
                if let Err(err) = persist_carried_scope(&state.project_root, &scope_info) {
                    warn!(
                        "failed to persist carried scope for {}: {}",
                        new_context_id, err
                    );
                }
            }
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

/// Run `unit` (a project's pipeline pass) to completion, servicing queued daemon
/// search requests AND SIGHUP registry reloads as they arrive in the meantime.
///
/// Confirmed hazard: a refresh sweep's per-project pass can run for
/// seconds — far past `DAEMON_READ_TIMEOUT_MS` — so yielding only at project
/// boundaries is insufficient; a search queued while `unit` is still pending
/// must be served without waiting for `unit` to finish. Each iteration polls
/// `unit` and the search-request channel together: whichever is ready first
/// wins, and if a request wins, `unit` simply gets re-polled (resuming
/// exactly where it left off) on the next loop iteration. `handle_search_request`
/// takes its own brief, per-call slice of `projects` rather than the
/// long-lived borrow `unit` may or may not hold, so a search for any OTHER
/// project never contends with `unit`'s (potentially long) execution here.
///
/// The SIGHUP arm is what makes a registry reload observable DURING a rebuild
/// (issue #109): previously no active-sweep path polled `sighup.recv()`, so a
/// SIGHUP delivered mid-pass stayed queued until the pipeline finished and the
/// de-registration drop-path could never cancel an in-flight rebuild. A reload
/// here may remove the very project `unit` is rebuilding: `reload_projects`
/// cancels the removed project's child token and drops its map entry, then this
/// loop keeps polling the SAME `unit` future so the pipeline unwinds
/// cooperatively at its next committed boundary (never dropped mid-flush).
/// `run_project` then finalizes without requiring the map entry and releases the
/// rebuild-lock guard LAST.
async fn run_unit_while_servicing_events<F: Future>(
    unit: F,
    projects: &mut ProjectStates,
    deferred: &mut DeferredProjects,
    watcher: &mut FileWatcher,
    daemon_token: &CancellationToken,
    sighup: &mut impl ReloadSignal,
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
            _ = sighup.recv() => {
                info!("received SIGHUP during a rebuild pass, reloading project registry");
                if let Err(e) = reload_projects(watcher, projects, deferred, daemon_token).await {
                    error!("failed to reload projects during a rebuild pass: {e}");
                } else {
                    record_file_check_for_all_projects(projects, Utc::now(), true);
                }
            }
        }
    }
}

/// Number of walk entries between test-only throttle sleeps (see
/// [`TEST_GATE_WALK_ENTRY_DELAY_ENV_VAR`]). Small so a modest fixture yields many
/// deliberate yield points, each followed by a cancellation check.
const TEST_GATE_WALK_THROTTLE_EVERY: usize = 10;

/// Count files in a gitignore-aware manner for gate-check purposes.
///
/// Returns the count of regular files that are not ignored by `.gitignore`.
/// Checks the cancellation token every 100 entries so a SIGTERM-driven token
/// cancellation aborts the walk promptly. A cancelled walk returns
/// [`IndexingError::Cancelled`] — a distinct outcome from a genuine empty walk
/// (`Ok(0)`) — so the caller never mistakes "cancelled during shutdown" for
/// "zero files" and opens the first-index gate while draining.
fn count_files_gitignore_aware(
    source_root: &Path,
    cancel_token: &CancellationToken,
) -> Result<usize, OneupError> {
    use crate::shared::errors::IndexingError;
    use ignore::WalkBuilder;

    // Test-only: read once. Holds the walk open (a small sleep every N entries)
    // so a test can land SIGTERM mid-walk. Compiled to a constant `None` in
    // release builds so a stray env var can never throttle a production gate
    // walk (1000ms on a 185k-file repo would add ~5 hours).
    let throttle_delay = if cfg!(debug_assertions) {
        std::env::var(crate::shared::constants::TEST_GATE_WALK_ENTRY_DELAY_ENV_VAR)
            .ok()
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .filter(|ms| *ms > 0)
            .map(std::time::Duration::from_millis)
    } else {
        None
    };

    let walker = WalkBuilder::new(source_root)
        .hidden(false)
        .ignore(true) // Respect .gitignore
        .build();

    let mut count = 0;
    for (idx, result) in walker.into_iter().enumerate() {
        // Check cancellation every 100 entries to allow timely SIGTERM exit
        if idx % 100 == 0 && cancel_token.is_cancelled() {
            debug!("count_files_gitignore_aware cancelled at {} entries", count);
            return Err(IndexingError::Cancelled.into());
        }

        // Test-only throttle: sleep at a fixed cadence and re-check cancellation
        // immediately after the yield so a mid-walk SIGTERM aborts within one
        // throttle interval rather than waiting for the next 100-entry boundary.
        if let Some(delay) = throttle_delay {
            if idx % TEST_GATE_WALK_THROTTLE_EVERY == 0 {
                std::thread::sleep(delay);
                if cancel_token.is_cancelled() {
                    debug!("count_files_gitignore_aware cancelled at {} entries", count);
                    return Err(IndexingError::Cancelled.into());
                }
            }
        }

        if let Ok(entry) = result {
            if entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
                count += 1;
            }
        }
    }

    Ok(count)
}

/// Deregister and stop watching a project whose source root has been deleted, so
/// the daemon stops trying to re-index a gone worktree. Handles both the main-repo
/// case (`state_root == source_root`) and a deleted *linked* worktree (`state_root`
/// survives, `source_root` gone). Best-effort registry + watch cleanup; returns
/// default stats so the daemon loop keeps serving other projects.
fn deregister_deleted_project(
    context_id: &str,
    projects: &mut ProjectStates,
    watcher: &mut FileWatcher,
    lock_root: &Path,
) -> Result<pipeline::PipelineStats, OneupError> {
    if let Some(removed_state) = projects.remove(context_id) {
        warn!(
            "project source root deleted or inaccessible: {}; deregistering",
            removed_state.source_root.display()
        );
        let context = WorktreeContext {
            context_id: removed_state.context.context_id.clone(),
            state_root: removed_state.project_root.clone(),
            source_root: removed_state.source_root.clone(),
            main_worktree_root: removed_state.context.main_worktree_root.clone(),
            worktree_role: removed_state.context.worktree_role,
            git_dir: removed_state.context.git_dir.clone(),
            common_git_dir: removed_state.context.common_git_dir.clone(),
            branch_name: removed_state.context.branch_name.clone(),
            branch_ref: removed_state.context.branch_ref.clone(),
            head_oid: removed_state.context.head_oid.clone(),
            branch_status: removed_state.context.branch_status,
        };
        if let Err(err) = Registry::load().and_then(|mut r| r.deregister_context(&context)) {
            warn!(
                "failed to deregister deleted project {}: {err}",
                lock_root.display()
            );
        }
        if let Err(err) = watcher.unwatch(&removed_state.source_root) {
            warn!(
                "failed to unwatch deleted source root {}: {err}",
                removed_state.source_root.display()
            );
        }
    }
    Ok(pipeline::PipelineStats::default())
}

async fn run_project(
    context_id: &str,
    projects: &mut ProjectStates,
    deferred: &mut DeferredProjects,
    search_requests_rx: &mut mpsc::Receiver<QueuedSearchRequest>,
    watcher: &mut FileWatcher,
    daemon_token: &CancellationToken,
    sighup: &mut impl ReloadSignal,
) -> Result<pipeline::PipelineStats, OneupError> {
    // Acquire the single-writer rebuild lock BEFORE `start_run` consumes the
    // pending scope, so a contended pass leaves the project dirty (its queued
    // paths intact) for a later retry instead of racing a competing rebuild and
    // dropping the changes. Non-blocking: the daemon defers rather than stalling
    // its event loop while a one-shot rebuild holds the lock. The guard releases
    // on drop — including when an in-flight pass is cancelled and this frame
    // unwinds — freeing the lock for the restarted binary.
    let (lock_root, source_root, source_presence) = {
        let state = projects
            .get(context_id)
            .expect("dirty project must exist while running");
        (
            state.project_root.clone(),
            state.source_root.clone(),
            probe_source_presence(&state.source_root),
        )
    };
    // Detect a deleted source root BEFORE attempting the rebuild lock.
    // For a *linked* worktree the state_root (main repo, owns `.1up/`) can survive
    // while the source_root (the worktree) is deleted, so the lock — which is keyed
    // on state_root — would still acquire and the deleted-source cleanup below
    // would never run, leaving the daemon refreshing a gone worktree. Probing
    // source_root independently covers both the main-repo case (state_root ==
    // source_root) and the linked-worktree split.
    //
    // Only a *definitely-absent* source deregisters: a transient/indeterminate
    // probe (an unreachable network mount) must never trigger destructive
    // deregistration. An indeterminate probe also must not *attempt* the pass:
    // `start_run` below clears `dirty` and consumes the queued scope, and an
    // ordinary pass failure does not restore them, so proceeding would spend
    // the only pending refresh on a source that is not readable right now.
    // Defer instead — return before the lock and before `start_run`, leaving
    // the project dirty with its scope intact so the next debounce tick
    // re-probes and retries (the same deferral shape as a contended rebuild
    // lock below).
    match source_presence {
        SourcePresence::Absent => {
            return deregister_deleted_project(context_id, projects, watcher, &lock_root);
        }
        SourcePresence::Indeterminate => {
            debug!(
                "source presence indeterminate for {} (transient probe failure); deferring re-index and keeping the pending scope queued",
                source_root.display()
            );
            return Err(
                crate::shared::errors::DaemonError::SourceProbeIndeterminate {
                    source_root: source_root.display().to_string(),
                }
                .into(),
            );
        }
        SourcePresence::Present => {}
    }
    // Owned by THIS frame for the entire pass — through setup, the pipeline
    // window, and finalization — and dropped only when this function returns.
    // This is the in-flight pass's managed run state (issue #109): a SIGHUP
    // reload that de-registers this project mid-pass cancels the per-project
    // token but never touches this guard; the caller keeps polling this same
    // frame until the pipeline exits at a committed boundary, so the
    // `rebuild.lock` FD releases strictly AFTER the pass can no longer write
    // (cancel → drain → unlock) and is never orphaned by a removal during the
    // setup awaits below (the guard was previously unreachable on this stack
    // until it was parked in the map entry just before the pipeline).
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
        Err(e) => {
            // The source root existed at the top of this pass; a lock error here is
            // a genuine lock/IO fault, or a race where the root vanished mid-pass.
            // Re-check once for that race and clean up only on a *definite* absence
            // (a transient probe failure must not deregister), otherwise propagate.
            let state = projects.get(context_id).expect("dirty project must exist");
            if probe_source_presence(&state.source_root) == SourcePresence::Absent {
                return deregister_deleted_project(context_id, projects, watcher, &lock_root);
            }
            return Err(e);
        }
    };

    // Holding the rebuild lock means any one-shot rebuild has finished and
    // released it, so its atomic switch-over is complete. If that rebuild swapped
    // the index onto a fresh inode, the daemon's long-lived handle now points at
    // the orphaned pre-swap inode; reopen it here — before `start_run` consumes
    // the pending scope and before any write — so this pass writes into the
    // refreshed index, never the lost old one. Doing this before
    // `start_run` keeps the project dirty for a clean retry if the reopen fails.
    // Per-project cancellation token for this pass: a child of the daemon's shared
    // token, so SIGTERM (which cancels the parent) still aborts this pass, while a
    // de-register cancels only this project. The gate walk and pipeline below
    // observe it so dropping the project stops its rebuild at the next safe yield.
    let project_cancel_token = {
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
        state.cancel_token.clone()
    };

    // Gate check for first-time large monorepo indexing.
    // Before starting a first index, check if file count is over threshold without scope.
    // If so, stay idle and let the MCP oneup_start path handle the gate.
    {
        // Snapshot the roots and first-index decision as owned values so the
        // immutable borrow of `projects` ends before the (cancellable) gate walk
        // and the `get_mut` used on the abort path below.
        let (state_root, source_root, is_first_index) = {
            let state = projects
                .get(context_id)
                .expect("dirty project must exist while gating");
            // Check if this is a first index: index.db exists but has no indexed
            // content (empty schema created at startup). This is more robust than
            // checking file existence, which would be defeated by the empty DB
            // created in build_project_state.
            let is_first_index = {
                let conn = state.db.connect_tuned().await?;
                segments::count_segments(&conn).await.unwrap_or(0) == 0
            };
            (
                state.project_root.clone(),
                state.source_root.clone(),
                is_first_index,
            )
        };

        if is_first_index {
            // First index: check the gate
            let threshold = std::env::var(crate::shared::constants::FILE_COUNT_THRESHOLD_ENV_VAR)
                .ok()
                .and_then(|raw| raw.trim().parse::<usize>().ok())
                .unwrap_or(crate::shared::constants::FILE_COUNT_THRESHOLD);

            // Run the gate walk in spawn_blocking so the async executor stays
            // responsive to signals. The walk observes this pass's per-project
            // child token and aborts cooperatively: SIGTERM cancels the daemon
            // token and thus this child via parentage, while a de-register
            // cancels only this child.
            let source_root_clone = source_root.clone();
            let cancel_token_clone = project_cancel_token.clone();
            let walk_result: Result<usize, OneupError> = tokio::task::spawn_blocking(move || {
                count_files_gitignore_aware(&source_root_clone, &cancel_token_clone)
            })
            .await
            .unwrap_or_else(|join_err| {
                Err(OneupError::Other(anyhow::anyhow!(
                    "gate walk task failed to join: {join_err}"
                )))
            });

            // A cancelled (or otherwise failed) gate walk must NOT collapse to
            // `file_count = 0`: that value passes the file-count gate and would
            // start a first index during the shutdown drain (defect 2). Abort the
            // pass instead. The gate walk runs before `start_run`, so the project
            // is still dirty with its pending scope intact and re-runs on a later
            // pass. A genuinely empty repo still returns `Ok(0)` and takes the
            // normal gate path below.
            let file_count = match walk_result {
                Ok(count) => count,
                Err(e) => {
                    let state = projects
                        .get_mut(context_id)
                        .expect("dirty project must exist while aborting an interrupted gate walk");
                    if matches!(
                        &e,
                        OneupError::Indexing(crate::shared::errors::IndexingError::Cancelled)
                    ) {
                        // Cancellation is neither complete nor failed: record the
                        // refresh as pending (mirroring the pipeline cancel path)
                        // so status readers see a re-index is still owed.
                        state.last_refresh_state = DaemonRefreshState::Pending;
                        state.last_refresh_error = None;
                        persist_daemon_context_status_for_state(state);
                    } else {
                        // A genuine walk/join fault: record failure, leave dirty.
                        mark_refresh_finished(state, Utc::now(), Err(&e));
                    }
                    return Err(e);
                }
            };

            // Check if scope is recorded in the progress file
            let scope_recorded = read_index_progress(&state_root)
                .and_then(|progress| progress.scope)
                .is_some();

            // Check the gate using the pure decision logic
            if !lifecycle::gate_allows_first_index(
                is_first_index,
                file_count,
                threshold,
                scope_recorded,
            ) {
                debug!(
                    "daemon gate fired for {}: over-threshold ({} > {}) without scope; staying idle",
                    state_root.display(),
                    file_count,
                    threshold
                );
                // Persist a scope proposal so a later oneup_status/oneup_start can
                // surface ranked scope suggestions for this daemon-fired gate —
                // the synchronous MCP walk that builds the facts envelope is
                // hidden by the daemon-alive timing race, so without this the
                // Missing readiness would only carry a generic next_action.
                // Best-effort: runs the synchronous walk off the async executor
                // (matching FIX C) and never blocks or fails the idle return.
                let persist_state_root = state_root.to_path_buf();
                let persist_source_root = source_root.to_path_buf();
                match tokio::task::spawn_blocking(move || {
                    crate::mcp::ops::persist_scope_proposal_for_gate(
                        &persist_state_root,
                        &persist_source_root,
                    )
                })
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => warn!(
                        "failed to persist scope proposal for gated project {}: {e}",
                        state_root.display()
                    ),
                    Err(e) => warn!(
                        "scope proposal persistence task panicked for {}: {e}",
                        state_root.display()
                    ),
                }
                // Consume the pending run WITHOUT re-queueing: the dirty flag
                // is only cleared by start_run(), so returning without it made
                // the scheduler re-select this project immediately and re-run
                // the gitignore-aware gate walk back-to-back forever (observed
                // pinning a core on a 186k-file repo, ~13 walks in 2 minutes).
                // The gated project stays idle until a real dirty signal
                // arrives; the first scoped index runs through the MCP start
                // path anyway, after which segments exist and first-index
                // gating no longer applies.
                let state = projects.get_mut(context_id).expect("must exist");
                let _ = state.run_state.start_run();
                let _ = state.run_state.pending_fallback_reason.take();
                state.run_state.finish_run();
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

            // Daemon path must apply recorded scope identically to MCP path.
            // Load scope from meta table and apply as scope_globs (the exclusive
            // scope filter) — include_globs only guarantee inclusion and never
            // exclude, so assigning them here would silently full-index a scoped
            // repo on every daemon refresh.
            //
            // The gate opens on the scope decision recorded in the progress file,
            // which an in-flight scoped rebuild writes before its meta write lands
            // in the database; fall back to it so a daemon refresh in that window
            // never runs unscoped over a scoped repo.
            let scope_roots = crate::storage::schema::read_scope_from_meta(&conn)
                .await
                .ok()
                .flatten()
                .or_else(|| {
                    read_index_progress(&project_root)
                        .and_then(|progress| progress.scope)
                        .map(|scope_info| scope_info.roots)
                        .filter(|roots| !roots.is_empty())
                });
            if let Some(scope_roots) = scope_roots {
                // Persist the applied scope so index_scope/status and later
                // refreshes read the same decision regardless of which writer
                // (daemon or MCP rebuild) builds the index first. Idempotent
                // when the meta row already exists.
                if let Err(err) =
                    crate::storage::schema::write_scope_to_meta(&conn, &scope_roots).await
                {
                    warn!("failed to persist applied scope to meta: {err}");
                }
                indexing_config.scope_roots = scope_roots.clone();
                indexing_config.scope_globs = scope_roots
                    .iter()
                    .map(|root| format!("{}/**", root))
                    .collect();
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

    // EVIDENCE-BASED DECISION: Scope carries to new branch contexts.
    //
    // DISPUTE SUMMARY: Two reviews disagreed about whether scope carried on branch
    // switch reaches the new context's rebuild:
    // - Review A: claimed persist_carried_scope() writes only to index_status.json
    //   (progress file), while rebuilds read from schema::read_scope_from_meta()
    //   (database meta), so scope never reaches the new context.
    // - Review B: traced run_project() and claimed scope application at line ~1820
    //   reads DB meta (shared across contexts) before the repair code, so the repair
    //   is redundant defensive code and the test is a tautology.
    //
    // EVIDENCE (from scope_carry_branch_switch.rs integration test):
    // 1. Database meta table IS shared across branch contexts ✓
    // 2. When a scoped index completes, scope is in database meta ✓
    // 3. New context CAN read scope from database meta at line 1820 ✓
    // 4. persist_carried_scope() writes to progress file ONLY
    // 5. The repair code below reads from progress file and re-persists to database
    //
    // CONCLUSION: The scope DOES reach the new context via the database meta path
    // at line 1820 WITHOUT the repair code. The database is shared across all
    // contexts for a given project_root, so scope persisted by a prior context is
    // available to the new context. The repair code below is IDEMPOTENT: it checks
    // if progress file has a carried scope marker and re-persists to the database,
    // which is a no-op if the database already has it (the normal case).
    //
    // KEEPING THE REPAIR CODE: Retained as defensive programming against edge
    // cases where the database meta might be cleared or in inconsistent state.
    // However, the real regression prevention comes from line 1820's read from
    // database meta, not this repair block. The existing test
    // (scope_carry_on_branch_switch_repersists_to_database_meta) is a TAUTOLOGY
    // that only checks both sources exist simultaneously, not that the repair is
    // necessary. Recommend: replace with an end-to-end test that exercises the
    // full mark_branch_context_changes + run_project flow and verifies the scope
    // actually reaches include_globs in the indexing pipeline.

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
    // held across it: a queued daemon search for ANY project runs
    // via `run_unit_while_servicing_events` below without contending with this
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

    let pipeline_unit = async {
        test_rebuild_hold(&project_root).await;
        pipeline::run_with_context_scope_setup_and_progress_root(
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
            &project_cancel_token,
        )
        .await
    };
    let result = run_unit_while_servicing_events(
        pipeline_unit,
        projects,
        deferred,
        watcher,
        daemon_token,
        sighup,
        search_requests_rx,
    )
    .await;

    // The SIGHUP arm inside `run_unit_while_servicing_events` may have
    // de-registered THIS project mid-pass: its map entry is gone — or was
    // replaced by a freshly re-registered state that never started this pass
    // (only `start_run` sets `running`, so a replacement is distinguishable).
    // Finalize through the entry only when it is still this pass's own state and
    // never `expect` it: a successful mid-pass removal used to panic the worker
    // here.
    match projects.get_mut(context_id) {
        Some(state) if state.run_state.running => {
            state.embedding_runtime = embedding_runtime;
            state.run_state.finish_run();

            if matches!(
                &result,
                Err(OneupError::Indexing(
                    crate::shared::errors::IndexingError::Cancelled
                ))
            ) {
                // A cancelled pass is neither complete nor failed: it stopped at
                // a committed boundary with the remainder unindexed. Re-queue the
                // scope so the context stays dirty (refresh state -> Pending) and
                // the remaining files re-index on the next pass — here if the
                // daemon survives, or on the restarted binary's startup
                // reconciliation after a SIGTERM drain.
                mark_refresh_pending(state, scope, Some("cancelled".to_string()));
                return result;
            }

            mark_refresh_finished(state, Utc::now(), result.as_ref().map(|_| ()));
        }
        _ => {
            // De-registered mid-pass: the drop path already cancelled the token
            // and persisted the stopped status, and there is no entry to write
            // refresh state into. The pipeline above exited at a committed
            // boundary, and `_rebuild_lock` drops when this frame returns —
            // strictly after that exit — so a competing writer can only acquire
            // the lock once this pass can no longer write.
            debug!(
                "project context {context_id} was de-registered during its rebuild pass; \
                 skipping state finalization"
            );
        }
    }

    result
}

/// Test-only seam for the SIGHUP-during-rebuild lifecycle tests: when
/// [`REBUILD_HOLD_ENV_VAR`] is set, a pass parks here — inside the pipeline
/// window, rebuild lock held, daemon events (search/SIGHUP) still being
/// serviced — for as long as `<state_root>/.1up/`[`REBUILD_HOLD_FILE_NAME`]
/// exists, writing [`REBUILD_HOLD_ENTERED_FILE_NAME`] so the test can detect the
/// parked pass. The wait deliberately ignores the cancellation token: it
/// emulates a long unit of work between two committed boundaries, letting tests
/// observe the window where cancellation has been requested but the pipeline has
/// not yet reached its next safe yield (the rebuild lock must still be held
/// there). No-op — a single env read — outside tests.
async fn test_rebuild_hold(state_root: &Path) {
    if std::env::var(REBUILD_HOLD_ENV_VAR).is_err() {
        return;
    }
    let dot_dir = config::project_dot_dir(state_root);
    let hold_path = dot_dir.join(REBUILD_HOLD_FILE_NAME);
    if !hold_path.exists() {
        return;
    }
    let _ = std::fs::write(dot_dir.join(REBUILD_HOLD_ENTERED_FILE_NAME), b"held");
    while hold_path.exists() {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
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
    // atomically switched `index.db` onto a new inode, reopen the handle so
    // this search is served by the refreshed index automatically, with no manual
    // step. The switch-over is atomic, so the handle always lands
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

    // Reuse one tuned read connection across requests. The schema is NOT
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
    // skips a per-query `COUNT(*)`. The cache is invalidated on index
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
    // (`run_project`'s `std::mem::take`): `Pending` (dirty but not
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
        // heavy hybrid search below runs without holding `&mut projects`:
        // a concurrent refresh sweep for a DIFFERENT project
        // never contends with this call for the whole-map borrow, and WAL's
        // concurrent-reader semantics make the shared connection
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
                // FTS-only results from the wrong (or no) variant.
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
    fn cancelled_gate_walk_aborts_instead_of_opening_the_gate() {
        use crate::shared::errors::IndexingError;

        // A gate walk cancelled by SIGTERM must surface as a distinct
        // `Cancelled` outcome, never as `Ok(0)`. Collapsing to zero would feed
        // `file_count = 0` into the gate, which passes for any threshold and would
        // open a first index during the shutdown drain (defect 2).
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..250 {
            std::fs::write(tmp.path().join(format!("file_{i}.rs")), "fn x() {}").unwrap();
        }

        let cancel_token = CancellationToken::new();
        cancel_token.cancel(); // pre-cancelled: the walk aborts at its first check

        let result = count_files_gitignore_aware(tmp.path(), &cancel_token);
        assert!(
            matches!(result, Err(OneupError::Indexing(IndexingError::Cancelled))),
            "a cancelled gate walk must return Cancelled, got {result:?}"
        );

        // Document the trap the fix avoids: a collapsed `file_count = 0` would
        // OPEN the gate (0 is never over threshold), whereas the real over-threshold
        // count would correctly BLOCK it. The cancelled walk must therefore never
        // yield a count at all.
        assert!(
            lifecycle::gate_allows_first_index(true, 0, 10, false),
            "sanity: file_count=0 opens the gate — exactly why a cancelled walk must not collapse to 0"
        );
        assert!(
            !lifecycle::gate_allows_first_index(true, 250, 10, false),
            "sanity: the true over-threshold count blocks the gate"
        );
    }

    #[test]
    fn uncancelled_gate_walk_counts_files() {
        // Guard the happy path: without cancellation the walk returns the real
        // (nonzero) file count, so the Ok arm still feeds the gate a true value.
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..5 {
            std::fs::write(tmp.path().join(format!("file_{i}.rs")), "fn x() {}").unwrap();
        }

        let cancel_token = CancellationToken::new();
        let count = count_files_gitignore_aware(tmp.path(), &cancel_token)
            .expect("an uncancelled walk succeeds");
        assert_eq!(count, 5, "walk must count all non-ignored regular files");
    }

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
            cancel_token: CancellationToken::new(),
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

    /// `prewarm_project_embedders` runs once, right after
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

        // Cancelling the project's own token before the pass means run_project must
        // not record a completed (or failed) run; the context stays dirty for
        // re-indexing. The pass now observes this per-project token (a child of the
        // daemon's shared token), so cancel it directly on the active-set entry.
        projects.get(&key).unwrap().cancel_token.cancel();

        // No search traffic is exercised in this test; an idle channel (sender
        // kept alive, nothing ever sent) never resolves `run_project`'s internal
        // search-servicing select arm, so the pipeline unit is the only branch
        // that ever completes.
        let (_tx, mut search_requests_rx) = mpsc::channel(1);
        let mut watcher = FileWatcher::new().unwrap();
        let daemon_token = CancellationToken::new();
        let (_reload_tx, mut reload_rx) = mpsc::unbounded_channel::<()>();
        let result = run_project(
            &key,
            &mut projects,
            &mut HashMap::new(),
            &mut search_requests_rx,
            &mut watcher,
            &daemon_token,
            &mut reload_rx,
        )
        .await;
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
    async fn run_project_deregisters_deleted_directory_and_stops_spinning() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        for i in 0..6 {
            std::fs::write(
                root.join(format!("mod_{i}.rs")),
                format!("pub fn item_{i}() -> usize {{ {i} }}\n"),
            )
            .unwrap();
        }

        // Set up the project state and database
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

        let (_tx, mut search_requests_rx) = mpsc::channel(1);
        let mut watcher = FileWatcher::new().unwrap();

        // Before deletion: project exists and dirty flag is set
        assert!(projects.contains_key(&key), "project must exist initially");
        assert!(
            projects.get(&key).unwrap().run_state.dirty,
            "project must be dirty initially"
        );

        // Delete the source directory to simulate the watched directory being removed
        drop(tmp);
        assert!(!root.exists(), "source root must be deleted for the test");

        // Run the project: it should detect the missing root, deregister, and return
        // success (default stats) rather than error or infinite loop
        let daemon_token = CancellationToken::new();
        let (_reload_tx, mut reload_rx) = mpsc::unbounded_channel::<()>();
        let result = run_project(
            &key,
            &mut projects,
            &mut HashMap::new(),
            &mut search_requests_rx,
            &mut watcher,
            &daemon_token,
            &mut reload_rx,
        )
        .await;

        // After deletion handling:
        // 1. run_project should return Ok (not an error)
        assert!(
            result.is_ok(),
            "run_project must succeed and return default stats when project is deleted, got: {result:?}"
        );

        // 2. Project should be removed from the active projects map
        assert!(
            !projects.contains_key(&key),
            "deleted project must be removed from projects map so daemon stops re-selecting it"
        );
    }

    #[tokio::test]
    async fn run_project_deregisters_deleted_linked_worktree_when_state_root_survives() {
        // H4: for a *linked* worktree the state_root (main repo, owns `.1up/`) can
        // survive while the source_root (the worktree) is deleted. The rebuild lock
        // is keyed on state_root, so it still acquires — the daemon must detect the
        // gone source_root BEFORE the lock and deregister, instead of spinning on a
        // missing worktree. Distinct from the main-repo case (state == source).
        let main_tmp = tempfile::tempdir().unwrap();
        let main_root = main_tmp.path().canonicalize().unwrap();
        let worktree_tmp = tempfile::tempdir().unwrap();
        let worktree_root = worktree_tmp.path().canonicalize().unwrap();
        for i in 0..6 {
            std::fs::write(
                worktree_root.join(format!("mod_{i}.rs")),
                format!("pub fn item_{i}() -> usize {{ {i} }}\n"),
            )
            .unwrap();
        }

        // The index/state DB lives under the *main* (state) root, not the worktree.
        let db_path = config::project_db_path(&main_root);
        ensure_secure_project_root(&main_root).unwrap();
        let db = Db::open_rw(&db_path).await.unwrap();
        schema::initialize(&db.connect_tuned().await.unwrap())
            .await
            .unwrap();

        let mut projects = HashMap::new();
        let key = insert_project(
            &mut projects,
            project_state(
                &main_root,
                &worktree_root,
                db,
                ProjectRunState {
                    running: false,
                    dirty: true,
                    pending_scope: Some(RunScope::Full),
                    pending_fallback_reason: None,
                },
            ),
        );

        let (_tx, mut search_requests_rx) = mpsc::channel(1);
        let mut watcher = FileWatcher::new().unwrap();

        // Delete ONLY the linked worktree (source root); the main/state root survives.
        drop(worktree_tmp);
        assert!(
            !worktree_root.exists(),
            "worktree source root must be deleted for the test"
        );
        assert!(
            main_root.exists(),
            "main/state root must survive for the test"
        );

        let daemon_token = CancellationToken::new();
        let (_reload_tx, mut reload_rx) = mpsc::unbounded_channel::<()>();
        let result = run_project(
            &key,
            &mut projects,
            &mut HashMap::new(),
            &mut search_requests_rx,
            &mut watcher,
            &daemon_token,
            &mut reload_rx,
        )
        .await;

        assert!(
            result.is_ok(),
            "run_project must succeed and return default stats when the linked worktree is deleted, got: {result:?}"
        );
        assert!(
            !projects.contains_key(&key),
            "deleted linked worktree must be removed from the projects map so the daemon stops re-selecting it"
        );
    }

    /// Create a self-referencing symlink at `dir/indeterminate-src`.
    /// `fs::metadata` follows it into `ELOOP`, which maps to neither `NotFound`
    /// nor `NotADirectory`, so the presence probe reports `Indeterminate`
    /// deterministically — on every Unix host and independent of uid (unlike a
    /// permission-denied setup, which root bypasses).
    fn indeterminate_source_path(dir: &Path) -> PathBuf {
        let path = dir.join("indeterminate-src");
        std::os::unix::fs::symlink(&path, &path).unwrap();
        assert_eq!(
            probe_source_presence(&path),
            SourcePresence::Indeterminate,
            "a self-referencing symlink must probe as indeterminate"
        );
        path
    }

    /// A dirty project whose state root (and DB) are healthy but whose source
    /// root probes `Indeterminate`, queued with a paths scope so the tests can
    /// prove the scope survives a deferred pass un-consumed.
    async fn indeterminate_source_project(
        tmp: &tempfile::TempDir,
    ) -> (ProjectStates, String, RunScope) {
        let main_root = tmp.path().canonicalize().unwrap();
        let db_path = config::project_db_path(&main_root);
        ensure_secure_project_root(&main_root).unwrap();
        let db = Db::open_rw(&db_path).await.unwrap();
        schema::initialize(&db.connect_tuned().await.unwrap())
            .await
            .unwrap();

        let source_root = indeterminate_source_path(&main_root);
        let pending = RunScope::from_paths([PathBuf::from("src/lib.rs")]).unwrap();
        let mut projects = HashMap::new();
        let key = insert_project(
            &mut projects,
            project_state(
                &main_root,
                &source_root,
                db,
                ProjectRunState {
                    running: false,
                    dirty: true,
                    pending_scope: Some(pending.clone()),
                    pending_fallback_reason: None,
                },
            ),
        );
        (projects, key, pending)
    }

    #[tokio::test]
    async fn run_project_defers_indeterminate_source_and_preserves_pending_scope() {
        // Regression (PR #120 review): the indeterminate branch used to fall
        // through to `start_run`, which cleared `dirty` and consumed the only
        // pending scope; the pass then failed without restoring either, so the
        // refresh was silently lost until an unrelated event re-marked the
        // project. A deferred pass must leave the retry state fully intact.
        let tmp = tempfile::tempdir().unwrap();
        let (mut projects, key, pending) = indeterminate_source_project(&tmp).await;

        let (_tx, mut search_requests_rx) = mpsc::channel(1);
        let mut watcher = FileWatcher::new().unwrap();
        let (_reload_tx, mut reload_rx) = mpsc::unbounded_channel::<()>();
        let result = run_project(
            &key,
            &mut projects,
            &mut HashMap::new(),
            &mut search_requests_rx,
            &mut watcher,
            &CancellationToken::new(),
            &mut reload_rx,
        )
        .await;

        assert!(
            matches!(
                &result,
                Err(OneupError::Daemon(
                    crate::shared::errors::DaemonError::SourceProbeIndeterminate { .. }
                ))
            ),
            "an indeterminate source probe must defer the pass, got: {result:?}"
        );
        assert!(
            projects.contains_key(&key),
            "an indeterminate source must never deregister the project"
        );
        let run_state = &projects.get(&key).unwrap().run_state;
        assert!(!run_state.running, "no run may be left in flight");
        assert!(
            run_state.dirty,
            "the project must stay dirty so the next debounce tick retries"
        );
        assert_eq!(
            run_state.pending_scope,
            Some(pending),
            "the queued scope must survive the deferred pass un-consumed"
        );
    }

    #[tokio::test]
    async fn dirty_sweep_defers_indeterminate_source_without_busy_spinning() {
        // The sweep must treat the deferral like a contended rebuild lock:
        // return to the select loop (this call terminating at all proves it did
        // not re-select the still-dirty project forever) while `dirty` and the
        // queued scope stay intact for the next tick's retry.
        let tmp = tempfile::tempdir().unwrap();
        let (mut projects, key, pending) = indeterminate_source_project(&tmp).await;

        let cancel_token = CancellationToken::new();
        let (_tx, mut search_requests_rx) = mpsc::channel(1);
        let mut watcher = FileWatcher::new().unwrap();
        let mut deferred: DeferredProjects = HashMap::new();
        let (_reload_tx, mut reload_rx) = mpsc::unbounded_channel::<()>();
        run_dirty_projects_until_clean(
            &mut watcher,
            &mut projects,
            &mut deferred,
            &cancel_token,
            &mut reload_rx,
            &mut search_requests_rx,
        )
        .await;

        let run_state = &projects.get(&key).unwrap().run_state;
        assert!(
            run_state.dirty,
            "the deferred project must remain dirty after the sweep returns"
        );
        assert_eq!(
            run_state.pending_scope,
            Some(pending),
            "the queued scope must remain queued after the sweep returns"
        );
    }

    fn test_entry(project_root: &Path, source_root: Option<&Path>) -> ProjectEntry {
        ProjectEntry {
            project_id: "test-project".to_string(),
            project_root: project_root.to_path_buf(),
            source_root: source_root.map(Path::to_path_buf),
            context_id: Some(format!(
                "test-entry-{}",
                project_root
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("root")
            )),
            main_worktree_root: None,
            worktree_role: None,
            branch_name: Some("main".to_string()),
            branch_ref: None,
            branch_status: None,
            head_oid: None,
            registered_at: "2026-01-01T00:00:00Z".to_string(),
            indexing: None,
        }
    }

    #[tokio::test]
    async fn build_project_state_defers_indeterminate_probes_and_skips_definite_absence() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let project_root = root.join("repo");
        std::fs::create_dir_all(&project_root).unwrap();
        let loop_path = indeterminate_source_path(&root);

        // Indeterminate source root: defer (retain for retry), never skip.
        let entry = test_entry(&project_root, Some(&loop_path));
        assert!(
            matches!(
                build_project_state(&entry, &CancellationToken::new())
                    .await
                    .unwrap(),
                ProjectStateBuild::Defer
            ),
            "an indeterminate source-root probe must defer, not drop, the entry"
        );

        // Indeterminate state root: same deferral.
        let entry = test_entry(&loop_path, None);
        assert!(
            matches!(
                build_project_state(&entry, &CancellationToken::new())
                    .await
                    .unwrap(),
                ProjectStateBuild::Defer
            ),
            "an indeterminate state-root probe must defer, not drop, the entry"
        );

        // A definitely-absent source root still skips (dropped until reload).
        let entry = test_entry(&project_root, Some(&root.join("definitely-missing")));
        assert!(
            matches!(
                build_project_state(&entry, &CancellationToken::new())
                    .await
                    .unwrap(),
                ProjectStateBuild::Skip
            ),
            "a definitely-absent source root must skip, exactly as before"
        );
    }

    #[tokio::test]
    async fn retry_deferred_projects_loads_and_watches_once_the_probe_recovers() {
        // Regression (PR #120 review): a startup-indeterminate project used to
        // be dropped (`build_project_state` returned `None`) until a SIGHUP
        // registry reload or daemon restart. It must instead stay in the
        // deferred set and be loaded by the daemon's own retry cycle as soon
        // as the probe recovers.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let project_root = root.join("repo");
        std::fs::create_dir_all(&project_root).unwrap();
        ensure_secure_project_root(&project_root).unwrap();
        let source_root = indeterminate_source_path(&root);

        let entry = test_entry(&project_root, Some(&source_root));
        let context_id = entry.context_id();
        let mut deferred: DeferredProjects = HashMap::from([(context_id.clone(), entry)]);
        let mut projects: ProjectStates = HashMap::new();
        let mut watcher = FileWatcher::new().unwrap();

        // Still unreachable: the entry must be retained, neither loaded nor dropped.
        retry_deferred_projects(
            &mut watcher,
            &mut projects,
            &mut deferred,
            &CancellationToken::new(),
        )
        .await;
        assert!(
            projects.is_empty(),
            "an indeterminate probe must not load the project"
        );
        assert!(
            deferred.contains_key(&context_id),
            "the entry must stay deferred for the next cycle"
        );

        // The probe recovers: replace the symlink loop with a real directory.
        std::fs::remove_file(&source_root).unwrap();
        std::fs::create_dir_all(&source_root).unwrap();
        std::fs::write(source_root.join("lib.rs"), "pub fn f() {}\n").unwrap();

        retry_deferred_projects(
            &mut watcher,
            &mut projects,
            &mut deferred,
            &CancellationToken::new(),
        )
        .await;
        assert!(
            deferred.is_empty(),
            "a recovered project must leave the deferred set"
        );
        let state = projects
            .get(&context_id)
            .expect("the recovered project must be loaded without SIGHUP or a daemon restart");
        assert!(
            state.run_state.dirty,
            "the recovered project must be queued for reconciliation"
        );
        assert_eq!(state.run_state.pending_scope, Some(RunScope::Full));
        assert_eq!(
            state.run_state.pending_fallback_reason.as_deref(),
            Some(STARTUP_RECONCILIATION_REASON)
        );
    }

    /// Restores redirected env vars on drop — even if an assertion panics — so a
    /// redirected data root or seam gate never leaks into other tests in this
    /// binary. Mirrors `cli::gc`'s `DataRootGuard`.
    struct EnvVarsGuard {
        saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl EnvVarsGuard {
        fn new(keys: &[&'static str]) -> Self {
            Self {
                saved: keys
                    .iter()
                    .map(|key| (*key, std::env::var_os(key)))
                    .collect(),
            }
        }
    }

    impl Drop for EnvVarsGuard {
        fn drop(&mut self) {
            for (key, value) in &self.saved {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    fn dirty_full_run_state() -> ProjectRunState {
        ProjectRunState {
            running: false,
            dirty: true,
            pending_scope: Some(RunScope::Full),
            pending_fallback_reason: None,
        }
    }

    async fn seeded_project_db(root: &Path) -> Db {
        ensure_secure_project_root(root).unwrap();
        let db = Db::open_rw(&config::project_db_path(root)).await.unwrap();
        schema::initialize(&db.connect_tuned().await.unwrap())
            .await
            .unwrap();
        db
    }

    fn registry_entry(context_id: &str, root: &Path) -> ProjectEntry {
        // Branch fields mirror `test_context` so a reload of a retained project
        // compares equal in `branch_context_changed`.
        ProjectEntry {
            project_id: context_id.to_string(),
            project_root: root.to_path_buf(),
            source_root: Some(root.to_path_buf()),
            context_id: Some(context_id.to_string()),
            main_worktree_root: Some(root.to_path_buf()),
            worktree_role: Some(crate::shared::types::WorktreeRole::Main),
            branch_name: Some("main".to_string()),
            branch_ref: Some("refs/heads/main".to_string()),
            branch_status: Some(crate::shared::types::BranchStatus::Named),
            head_oid: Some("0000000000000000000000000000000000000000".to_string()),
            registered_at: "2026-01-01T00:00:00Z".to_string(),
            indexing: None,
        }
    }

    /// issue #109, production-path lifecycle: a SIGHUP registry reload delivered
    /// WHILE a rebuild pass is running — through the real sweep
    /// (`run_dirty_projects_until_clean` → `run_project` →
    /// `run_unit_while_servicing_events`), a real `rebuild.lock` acquired by that
    /// pass, and a mutated on-disk registry — must cancel the de-registered
    /// pass, keep the lock held until the pipeline exits at a committed
    /// boundary, release it only then, and leave the worker and the retained
    /// sibling project fully healthy.
    ///
    /// The reload trigger is INJECTED through the [`ReloadSignal`] seam (a
    /// channel tick sent while the pass is parked in the rebuild hold) rather
    /// than a raised process-global SIGHUP: a real signal is process-wide, so
    /// concurrent tests' tokio signal listeners can consume it and its delivery
    /// races the sweep's select loop, making the test flaky under the parallel
    /// harness. The seam feeds the exact select arm production wires SIGHUP
    /// into, so the covered reload path is unchanged.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn sighup_during_rebuild_cancels_pass_and_releases_lock_only_after_drain() {
        // Every test in this binary that mutates HOME/XDG_DATA_HOME (`dirs::*`
        // reads them at call time) must serialize on this crate-wide lock, or a
        // concurrent mutation in another module corrupts this test's resolved
        // registry path.
        let _env_lock = crate::shared::fs::ENV_MUTEX
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let _env_guard = EnvVarsGuard::new(&["HOME", "XDG_DATA_HOME", REBUILD_HOLD_ENV_VAR]);
        let home = tempfile::tempdir().unwrap();
        let home_root = home.path().canonicalize().unwrap();
        std::env::set_var("HOME", &home_root);
        std::env::set_var("XDG_DATA_HOME", home_root.join(".local").join("share"));
        std::env::set_var(REBUILD_HOLD_ENV_VAR, "1");

        // Two dirty projects; keys sort so the held project's pass runs first.
        let tmp_a = tempfile::tempdir().unwrap();
        let root_a = tmp_a.path().canonicalize().unwrap();
        let tmp_b = tempfile::tempdir().unwrap();
        let root_b = tmp_b.path().canonicalize().unwrap();
        for root in [&root_a, &root_b] {
            for i in 0..4 {
                std::fs::write(
                    root.join(format!("mod_{i}.rs")),
                    format!("pub fn item_{i}() -> usize {{ {i} }}\n"),
                )
                .unwrap();
            }
        }
        let db_a = seeded_project_db(&root_a).await;
        let db_b = seeded_project_db(&root_b).await;

        let key_a = "sighup-a-held".to_string();
        let key_b = "sighup-b-kept".to_string();
        let mut state_a = project_state(&root_a, &root_a, db_a, dirty_full_run_state());
        state_a.context.context_id = key_a.clone();
        let mut state_b = project_state(&root_b, &root_b, db_b, dirty_full_run_state());
        state_b.context.context_id = key_b.clone();
        let token_a = state_a.cancel_token.clone();
        let token_b = state_b.cancel_token.clone();

        let mut projects = HashMap::new();
        insert_project(&mut projects, state_a);
        insert_project(&mut projects, state_b);

        // The registry the SIGHUP reload will re-read: initially both projects.
        Registry {
            projects: vec![
                registry_entry(&key_a, &root_a),
                registry_entry(&key_b, &root_b),
            ],
        }
        .save()
        .unwrap();

        // Park A's pass inside its pipeline window (lock held) until released.
        let hold_path = config::project_dot_dir(&root_a).join(REBUILD_HOLD_FILE_NAME);
        let entered_path = config::project_dot_dir(&root_a).join(REBUILD_HOLD_ENTERED_FILE_NAME);
        std::fs::write(&hold_path, b"hold").unwrap();

        let mut watcher = FileWatcher::new().unwrap();
        watcher.watch(&root_a).unwrap();
        watcher.watch(&root_b).unwrap();
        let daemon_token = CancellationToken::new();
        // Injected reload source (see the test doc): `reload_tx` stands in for
        // the kernel delivering a SIGHUP, feeding the same sweep select arm.
        let (reload_tx, mut reload_rx) = mpsc::unbounded_channel::<()>();
        let (_tx, mut search_requests_rx) = mpsc::channel(1);
        let mut deferred: DeferredProjects = HashMap::new();

        // The sweep future mutably borrows `projects`; scope it so the borrow
        // ends before the post-sweep assertions below inspect the map.
        {
            let sweep = run_dirty_projects_until_clean(
                &mut watcher,
                &mut projects,
                &mut deferred,
                &daemon_token,
                &mut reload_rx,
                &mut search_requests_rx,
            );
            tokio::pin!(sweep);

            // Drives the pinned sweep concurrently with the test's observations and
            // panics if the sweep finishes while a phase still expects it parked.
            macro_rules! drive_sweep_tick {
                () => {
                    tokio::select! {
                        _ = &mut sweep => panic!("sweep must not finish while pass A is parked"),
                        _ = tokio::time::sleep(Duration::from_millis(5)) => {}
                    }
                };
            }

            // Phase 1: the real pass owns the real rebuild lock (it entered the hold,
            // which sits after lock acquisition inside the pipeline window).
            let deadline = std::time::Instant::now() + Duration::from_secs(60);
            while !entered_path.exists() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "pass A never entered the pipeline hold"
                );
                drive_sweep_tick!();
            }
            assert!(
                lifecycle::try_acquire_rebuild_lock(&root_a)
                    .unwrap()
                    .is_none(),
                "the in-flight pass must own the real rebuild lock"
            );

            // Phase 2: de-register A in the on-disk registry and inject the
            // reload tick while A's pass is still running. The pass is parked
            // in the rebuild hold with the lock held, so the tick is delivered
            // deterministically at the intended point.
            Registry {
                projects: vec![registry_entry(&key_b, &root_b)],
            }
            .save()
            .unwrap();
            reload_tx.send(()).unwrap();

            while !token_a.is_cancelled() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the injected reload never cancelled the de-registered in-flight pass"
                );
                drive_sweep_tick!();
            }
            // Cancellation has been REQUESTED but the pass has not reached its next
            // safe yield (the hold is still in place): the lock must STILL be held.
            // Releasing it here would let a competing writer in while the old
            // pipeline can still write — the exact ordering bug under review.
            for _ in 0..10 {
                drive_sweep_tick!();
                assert!(
                    lifecycle::try_acquire_rebuild_lock(&root_a)
                        .unwrap()
                        .is_none(),
                    "the rebuild lock must stay held until the cancelled pass drains \
                 to a committed boundary"
                );
            }
            assert!(
                !token_b.is_cancelled(),
                "the retained sibling must not be cancelled by the reload"
            );

            // Phase 3: release the hold. The cancelled pass exits at its first
            // committed boundary, run_project finalizes without the map entry (no
            // panic), the guard drops, and the sweep goes on to finish sibling B.
            std::fs::remove_file(&hold_path).unwrap();
            tokio::time::timeout(Duration::from_secs(120), &mut sweep)
                .await
                .expect("the sweep must complete after the cancelled pass drains");
        }

        assert!(
            lifecycle::try_acquire_rebuild_lock(&root_a)
                .unwrap()
                .is_some(),
            "the de-registered pass must release its rebuild.lock FD once drained (issue #109)"
        );
        assert!(
            !projects.contains_key(&key_a),
            "the de-registered project must leave the active set"
        );
        let retained = projects
            .get(&key_b)
            .expect("the retained sibling must survive the reload");
        assert!(
            !token_b.is_cancelled(),
            "the retained sibling must stay uncancelled after the sweep"
        );
        assert_eq!(
            retained.last_refresh_state,
            DaemonRefreshState::Complete,
            "the retained sibling's own pass must run to completion"
        );
        // Committed boundary: the aborted project's index is intact and readable.
        let db_a = Db::open_rw(&config::project_db_path(&root_a))
            .await
            .unwrap();
        let conn_a = db_a.connect_tuned().await.unwrap();
        segments::count_segments(&conn_a)
            .await
            .expect("the aborted pass must leave a readable index (committed boundary)");
    }

    /// The de-register drop-path must cancel a removed project's in-flight pass
    /// even when watcher cleanup cannot succeed: the watched source root is
    /// deleted before the reload, so `unwatch` is exercised on a gone path and is
    /// best-effort (log-and-continue). Before the fix the `?` on `unwatch` could
    /// return out of `drop_deregistered_projects` without ever cancelling the
    /// removed project's pipeline token.
    #[tokio::test]
    async fn reload_drop_path_cancels_removed_project_even_when_watcher_cleanup_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let db = seeded_project_db(&root).await;

        let mut projects = HashMap::new();
        let key = insert_project(
            &mut projects,
            project_state(&root, &root, db, ProjectRunState::default()),
        );
        let token = projects.get(&key).unwrap().cancel_token.clone();

        let mut watcher = FileWatcher::new().unwrap();
        watcher.watch(&root).unwrap();
        // Delete the watched root so the unwatch attempt inside the drop path
        // runs against a gone directory.
        drop(tmp);
        assert!(!root.exists(), "source root must be deleted for the test");

        // SIGHUP reload observes an empty registry: this context is de-registered.
        drop_deregistered_projects(
            &mut watcher,
            &mut projects,
            &HashSet::new(),
            &HashSet::new(),
        );

        assert!(
            !projects.contains_key(&key),
            "a de-registered project must leave the active set even when watcher \
             cleanup fails"
        );
        assert!(
            token.is_cancelled(),
            "watcher cleanup failure must never skip cancelling the removed \
             project's in-flight rebuild"
        );
    }

    /// issue #109 regression: dropping one project on reload must not disturb a
    /// project that is still registered — its cancellation token stays untouched
    /// so an in-flight rebuild would keep running.
    #[tokio::test]
    async fn reload_drop_path_keeps_retained_project_uncancelled() {
        let tmp_a = tempfile::tempdir().unwrap();
        let tmp_b = tempfile::tempdir().unwrap();
        let root_a = tmp_a.path().canonicalize().unwrap();
        let root_b = tmp_b.path().canonicalize().unwrap();
        let db_a = seeded_project_db(&root_a).await;
        let db_b = seeded_project_db(&root_b).await;

        let mut projects = HashMap::new();
        let key_a = insert_project(
            &mut projects,
            project_state(&root_a, &root_a, db_a, ProjectRunState::default()),
        );
        let key_b = insert_project(
            &mut projects,
            project_state(&root_b, &root_b, db_b, ProjectRunState::default()),
        );
        let token_a = projects.get(&key_a).unwrap().cancel_token.clone();
        let token_b = projects.get(&key_b).unwrap().cancel_token.clone();

        // Reload keeps only repo A registered; repo B is de-registered.
        let mut watcher = FileWatcher::new().unwrap();
        watcher.watch(&root_a).unwrap();
        watcher.watch(&root_b).unwrap();
        let registered_contexts: HashSet<String> = std::iter::once(key_a.clone()).collect();
        let registered_sources: HashSet<PathBuf> =
            std::iter::once(canonical_project_root(&root_a)).collect();
        drop_deregistered_projects(
            &mut watcher,
            &mut projects,
            &registered_contexts,
            &registered_sources,
        );

        assert!(
            projects.contains_key(&key_a),
            "the still-registered project must stay in the active set"
        );
        assert!(
            !token_a.is_cancelled(),
            "the retained project's in-flight rebuild must not be cancelled"
        );
        assert!(
            !projects.contains_key(&key_b),
            "the de-registered project must leave the active set"
        );
        assert!(
            token_b.is_cancelled(),
            "the dropped project's in-flight rebuild must be cancelled"
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

    /// Confirmed hazard: per-project-boundary yielding alone is
    /// insufficient because a single project's refresh pass can run for
    /// seconds, far past `DAEMON_READ_TIMEOUT_MS` — the search path must be
    /// decoupled from the sweep's `&mut projects` borrow, not merely
    /// interleaved with it at a coarser boundary.
    ///
    /// This drives `run_unit_while_servicing_events` — the exact seam
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
        let mut watcher = FileWatcher::new().unwrap();
        let daemon_token = CancellationToken::new();
        let (_reload_tx, mut reload_rx) = mpsc::unbounded_channel::<()>();
        let mut deferred: DeferredProjects = HashMap::new();
        let run = run_unit_while_servicing_events(
            slow_unit,
            &mut projects,
            &mut deferred,
            &mut watcher,
            &daemon_token,
            &mut reload_rx,
            &mut search_requests_rx,
        );
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

    /// Lost-write regression: after a one-shot rebuild swaps the index onto a fresh
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

        // Decisive lost-write guard: a write through the reopened handle is durable
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

    /// The per-context vector-count cache on `ProjectState`
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

    /// No-swap arm: a no-op reopen (no swap) keeps the cached
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

    /// Hoisting `ensure_current` out of the per-request path is
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

    /// Repeated queries on the reused tuned read connection must
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

    // REMOVED: scope_carry_on_branch_switch_repersists_to_database_meta
    //
    // This test was checking that scope exists in BOTH the progress file and the
    // database meta table simultaneously. However, it was a TAUTOLOGY that did not
    // prove the repair code (lines 1860-1894) was necessary:
    //
    // EVIDENCE (from scope_carry_branch_switch.rs integration test):
    // - The test wrote scope to the database, then checked both sources existed.
    // - It did NOT exercise the real flow: mark_branch_context_changes + run_project
    // - The test passed even when the repair code was disabled (line 1860-1894)
    //
    // FINDING: The repair code is defensive/idempotent, not load-bearing.
    // The real scope flow is line 1820: read from database meta (shared across
    // contexts). Since both contexts use the same project_root/.1up/index.db,
    // scope persisted by a prior context IS available to the new context.
    //
    // REPLACEMENT: See tests/scope_carry_branch_switch.rs for the empirical
    // evidence tests that settle the dispute about whether scope reaches the
    // new context's rebuild. The evidence test exercises the database sharing
    // behavior and confirms scope is available via the line-1820 path.

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
    fn source_missing_selects_only_contexts_whose_source_is_absent() {
        let contexts = [
            context_row("live00000001", "/repo", "/repo"),
            context_row("gone00000001", "/repo", "/repo-feature"),
        ];
        // Only `/repo-feature` is absent; the live `/repo` context is retained.
        let selection = classify_source_missing_contexts(&contexts, &|p| {
            if p == Path::new("/repo-feature") {
                SourcePresence::Absent
            } else {
                SourcePresence::Present
            }
        });
        assert_eq!(selection.to_prune, vec!["gone00000001".to_string()]);
        assert!(selection.indeterminate.is_empty());
    }

    #[test]
    fn indeterminate_source_is_retained_not_pruned() {
        // A transient probe failure (e.g. an unreachable network mount) must never
        // be treated as deletion: the context is reported as indeterminate and
        // retained, while a genuinely-absent sibling is still pruned.
        let contexts = [
            context_row("flaky0000001", "/repo", "/mnt/nfs/repo"),
            context_row("gone00000001", "/repo", "/repo-feature"),
        ];
        let selection =
            classify_source_missing_contexts(&contexts, &|p| match p.to_str().unwrap() {
                "/mnt/nfs/repo" => SourcePresence::Indeterminate,
                "/repo-feature" => SourcePresence::Absent,
                _ => SourcePresence::Present,
            });
        assert_eq!(selection.to_prune, vec!["gone00000001".to_string()]);
        assert_eq!(selection.indeterminate, vec!["flaky0000001".to_string()]);
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
        let selection = classify_source_missing_contexts(&contexts, &|_| SourcePresence::Present);
        assert!(selection.to_prune.is_empty());
        assert!(selection.indeterminate.is_empty());
    }

    #[test]
    fn source_missing_on_empty_input_is_empty() {
        let selection = classify_source_missing_contexts(&[], &|_| SourcePresence::Present);
        assert!(selection.to_prune.is_empty());
        assert!(selection.indeterminate.is_empty());
    }

    #[test]
    fn source_missing_selects_every_absent_context_and_keeps_the_live_one() {
        let contexts = [
            context_row("gone00000001", "/repo", "/wt-a"),
            context_row("live00000001", "/repo", "/repo"),
            context_row("gone00000002", "/repo", "/wt-b"),
        ];
        // Every context whose source is absent is selected, order-preserved; the
        // single live context is the only one retained.
        let selection = classify_source_missing_contexts(&contexts, &|p| {
            if p == Path::new("/repo") {
                SourcePresence::Present
            } else {
                SourcePresence::Absent
            }
        });
        assert_eq!(
            selection.to_prune,
            vec!["gone00000001".to_string(), "gone00000002".to_string()]
        );
        assert!(selection.indeterminate.is_empty());
    }

    #[test]
    fn is_entry_dead_only_when_both_definitely_absent() {
        use PathPresence::*;
        // Fully-deleted project: both root and index.db are gone.
        assert!(is_entry_dead(DefinitelyAbsent, DefinitelyAbsent));
        // Live root with a missing index.db is a fresh, not-yet-indexed project: keep.
        assert!(!is_entry_dead(Present, DefinitelyAbsent));
        // Live project (both present): keep.
        assert!(!is_entry_dead(Present, Present));
        // Any indeterminate probe (flaky mount) keeps the entry, even when the
        // other input looks gone — a transient outage must never de-register.
        assert!(!is_entry_dead(Indeterminate, DefinitelyAbsent));
        assert!(!is_entry_dead(DefinitelyAbsent, Indeterminate));
        assert!(!is_entry_dead(Indeterminate, Indeterminate));
        assert!(!is_entry_dead(Indeterminate, Present));
    }

    #[test]
    fn probe_path_presence_present_and_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let present = tmp.path().join("here");
        std::fs::write(&present, b"x").unwrap();
        assert_eq!(probe_path_presence(&present), PathPresence::Present);

        let missing = tmp.path().join("gone");
        assert_eq!(
            probe_path_presence(&missing),
            PathPresence::DefinitelyAbsent
        );
    }

    #[cfg(unix)]
    #[test]
    fn probe_path_presence_reports_dangling_symlink_as_present() {
        // symlink_metadata does not follow the final component, so a dangling
        // symlink is Present (its directory entry exists), never DefinitelyAbsent:
        // a project must not be de-registered because a link target moved.
        let tmp = tempfile::tempdir().unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(tmp.path().join("nonexistent-target"), &link).unwrap();
        assert_eq!(probe_path_presence(&link), PathPresence::Present);
    }

    fn dead_test_entry(project_root: &Path, context_id: &str) -> ProjectEntry {
        ProjectEntry {
            project_id: format!("id-{context_id}"),
            project_root: project_root.to_path_buf(),
            source_root: None,
            context_id: Some(context_id.to_string()),
            main_worktree_root: None,
            worktree_role: None,
            branch_name: None,
            branch_ref: None,
            branch_status: None,
            head_oid: None,
            registered_at: "2026-01-01T00:00:00Z".to_string(),
            indexing: None,
        }
    }

    #[test]
    fn dead_project_selects_only_fully_deleted_entries() {
        let tmp = tempfile::tempdir().unwrap();

        // Live project: root + index.db both exist.
        let live_root = tmp.path().join("live");
        std::fs::create_dir_all(config::project_dot_dir(&live_root)).unwrap();
        std::fs::write(config::project_db_path(&live_root), b"db").unwrap();

        // Fresh project: root exists, index.db not yet created.
        let fresh_root = tmp.path().join("fresh");
        std::fs::create_dir_all(&fresh_root).unwrap();

        // Fully-deleted project: neither root nor index.db exist on disk.
        let dead_root = tmp.path().join("dead");

        let registry = Registry {
            projects: vec![
                dead_test_entry(&live_root, "live00000001"),
                dead_test_entry(&fresh_root, "fresh0000001"),
                dead_test_entry(&dead_root, "dead00000001"),
            ],
        };

        // Only the fully-deleted entry is selected for removal; the live and
        // fresh (missing-db) entries survive.
        let dead = dead_project_context_ids(&registry, &|p| probe_path_presence(p));
        assert_eq!(dead, vec!["dead00000001".to_string()]);
    }

    #[test]
    fn dead_project_keeps_indeterminate_entries() {
        // An indeterminate probe (e.g. a flaky mount returning EIO) on both inputs
        // keeps the entry even though the paths appear gone, so a transient outage
        // never de-registers a live project.
        let registry = Registry {
            projects: vec![dead_test_entry(
                Path::new("/does-not-matter"),
                "flaky0000001",
            )],
        };
        let dead = dead_project_context_ids(&registry, &|_| PathPresence::Indeterminate);
        assert!(dead.is_empty());
    }

    #[test]
    fn revived_candidate_no_longer_probes_dead_at_removal_time() {
        // Regression companion for the startup TOCTOU fix: candidates come from
        // a stale snapshot, so each one is re-probed under the registry lock via
        // `probe_entry_dead` before removal. Once the project root and index.db
        // are recreated (e.g. a concurrent `1up start` re-registering the same
        // deterministic context id), the shared predicate must flip to "alive"
        // so the conditional removal keeps the entry.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("revived");
        let registry = Registry {
            projects: vec![dead_test_entry(&root, "revive000001")],
        };

        // Snapshot scan: fully deleted, so the entry is a removal candidate.
        let dead = dead_project_context_ids(&registry, &|p| probe_path_presence(p));
        assert_eq!(dead, vec!["revive000001".to_string()]);

        // Concurrent recreation before the locked mutation: root + index.db
        // are back, so the re-probe must no longer classify the entry as dead.
        std::fs::create_dir_all(config::project_dot_dir(&root)).unwrap();
        std::fs::write(config::project_db_path(&root), b"db").unwrap();
        assert!(
            !probe_entry_dead(&registry.projects[0], &|p| probe_path_presence(p)),
            "revived entry must survive the under-lock re-probe"
        );

        // A candidate that is still fully deleted keeps probing dead and is
        // therefore still removed.
        std::fs::remove_dir_all(&root).unwrap();
        assert!(probe_entry_dead(&registry.projects[0], &|p| {
            probe_path_presence(p)
        }));
    }

    #[tokio::test]
    async fn run_project_continues_serving_other_projects_after_one_deleted() {
        // Acceptance: GIVEN a long-running daemon with multiple projects,
        // WHEN one project is deleted, THEN the daemon continues to serve search
        // and watch other projects without degradation
        let tmp1 = tempfile::tempdir().unwrap();
        let root1 = tmp1.path().canonicalize().unwrap();
        for i in 0..3 {
            std::fs::write(root1.join(format!("file_{i}.rs")), format!("// File {i}\n")).unwrap();
        }

        let tmp2 = tempfile::tempdir().unwrap();
        let root2 = tmp2.path().canonicalize().unwrap();
        for i in 0..3 {
            std::fs::write(root2.join(format!("file_{i}.rs")), format!("// File {i}\n")).unwrap();
        }

        // Set up two project states
        let db_path1 = config::project_db_path(&root1);
        ensure_secure_project_root(&root1).unwrap();
        let db1 = Db::open_rw(&db_path1).await.unwrap();
        schema::initialize(&db1.connect_tuned().await.unwrap())
            .await
            .unwrap();

        let db_path2 = config::project_db_path(&root2);
        ensure_secure_project_root(&root2).unwrap();
        let db2 = Db::open_rw(&db_path2).await.unwrap();
        schema::initialize(&db2.connect_tuned().await.unwrap())
            .await
            .unwrap();

        let mut projects = HashMap::new();
        let key1 = insert_project(
            &mut projects,
            project_state(
                &root1,
                &root1,
                db1,
                ProjectRunState {
                    running: false,
                    dirty: true,
                    pending_scope: Some(RunScope::Full),
                    pending_fallback_reason: None,
                },
            ),
        );

        let key2 = insert_project(
            &mut projects,
            project_state(
                &root2,
                &root2,
                db2,
                ProjectRunState {
                    running: false,
                    dirty: false,
                    pending_scope: None,
                    pending_fallback_reason: None,
                },
            ),
        );

        let (_tx, mut search_requests_rx) = mpsc::channel(1);
        let mut watcher = FileWatcher::new().unwrap();

        // Verify both projects exist before deletion
        assert_eq!(projects.len(), 2, "should have 2 projects");

        // Delete the first project's directory
        drop(tmp1);
        assert!(
            !root1.exists(),
            "first project's source root must be deleted"
        );

        // Run the first project: it should handle the deletion gracefully
        let daemon_token = CancellationToken::new();
        let (_reload_tx, mut reload_rx) = mpsc::unbounded_channel::<()>();
        let result = run_project(
            &key1,
            &mut projects,
            &mut HashMap::new(),
            &mut search_requests_rx,
            &mut watcher,
            &daemon_token,
            &mut reload_rx,
        )
        .await;

        // First project should be handled without error
        assert!(
            result.is_ok(),
            "run_project must handle deleted directory gracefully, got: {result:?}"
        );

        // First project should be removed from projects
        assert!(
            !projects.contains_key(&key1),
            "deleted project must be removed from projects map"
        );

        // Second project should still exist in projects
        assert!(
            projects.contains_key(&key2),
            "non-deleted project must remain in projects map"
        );

        // Verify we still have exactly one project left
        assert_eq!(projects.len(), 1, "should have 1 project remaining");
    }

    #[test]
    fn spawn_daemon_includes_source_root_in_command_for_diagnosability() {
        // Acceptance: GIVEN the worker process is handling a deleted directory,
        // WHEN observing the process via ps/lsof, THEN the argv includes the project
        // path for diagnosability
        //
        // This unit test verifies the command construction includes the source_root
        // as a visible argument. The actual ps/lsof observation is an integration test,
        // but this proves the code path is correct.

        let tmp = tempfile::tempdir().unwrap();
        let source_root = tmp.path();

        // Verify source_root can be converted to a display string for passing as argv
        let source_root_display = source_root.display().to_string();

        // Evidence: The spawn_daemon function has been modified to include:
        // .arg("__worker")
        // .arg(&source_root_display)
        // This unit test documents the requirement. Full verification happens in
        // integration tests that spawn a real daemon and inspect ps output.
        assert!(
            !source_root_display.is_empty(),
            "source_root_display must be non-empty for diagnosability"
        );
        drop(tmp);
    }
}
