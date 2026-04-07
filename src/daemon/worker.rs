use std::collections::HashMap;
use std::future;
use std::path::{Path, PathBuf};

use tokio::signal::unix::{signal, SignalKind};
use tracing::{debug, error, info, warn};

use crate::daemon::lifecycle;
use crate::daemon::registry::{ProjectEntry, Registry};
use crate::daemon::search_service::{self, SearchRequest, SearchResponse};
use crate::daemon::watcher::{self, FileWatcher};
use crate::indexer::embedder::{EmbeddingLoadStatus, EmbeddingRuntime, EmbeddingUnavailableReason};
use crate::indexer::pipeline;
use crate::search::HybridSearchEngine;
use crate::shared::config;
use crate::shared::constants::WATCHER_DEBOUNCE_MS;
use crate::shared::errors::OneupError;
use crate::shared::types::{IndexingConfig, RunScope};
use crate::storage::{db::Db, schema};

#[derive(Debug, Default)]
struct ProjectRunState {
    running: bool,
    dirty: bool,
    pending_scope: Option<RunScope>,
}

impl ProjectRunState {
    fn mark_dirty(&mut self, scope: RunScope) {
        match self.pending_scope.as_mut() {
            Some(existing) => existing.merge(scope),
            None => self.pending_scope = Some(scope),
        }

        self.dirty = true;
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
    db: Db,
    indexing: Option<IndexingConfig>,
    embedding_runtime: EmbeddingRuntime,
    run_state: ProjectRunState,
}

pub async fn run() -> Result<(), OneupError> {
    lifecycle::write_pid_file()?;

    let result = run_inner().await;

    if let Err(e) = lifecycle::remove_pid_file() {
        warn!("failed to clean up pid file: {e}");
    }

    result
}

async fn run_inner() -> Result<(), OneupError> {
    info!("daemon worker starting (pid={})", std::process::id());

    let mut sighup = signal(SignalKind::hangup()).map_err(|e| {
        crate::shared::errors::DaemonError::SignalError(format!("SIGHUP handler: {e}"))
    })?;
    let mut sigterm = signal(SignalKind::terminate()).map_err(|e| {
        crate::shared::errors::DaemonError::SignalError(format!("SIGTERM handler: {e}"))
    })?;

    let mut file_watcher = FileWatcher::new()?;
    let mut projects: HashMap<PathBuf, ProjectState> = HashMap::new();
    let search_listener = match search_service::bind_listener().await {
        Ok(listener) => Some(listener),
        Err(err) => {
            warn!("failed to start daemon search socket; search will fall back locally: {err}");
            None
        }
    };

    load_and_watch_projects(&mut file_watcher, &mut projects).await?;

    let debounce = std::time::Duration::from_millis(WATCHER_DEBOUNCE_MS);

    loop {
        tokio::select! {
            request = async {
                match search_listener.as_ref() {
                    Some(listener) => Some(search_service::accept_request(listener).await),
                    None => future::pending::<Option<Result<_, OneupError>>>().await,
                }
            } => {
                if let Some(request) = request {
                    match request {
                        Ok((mut stream, request)) => {
                            let response = handle_search_request(&mut projects, request).await;
                            if let Err(err) = search_service::send_response(&mut stream, &response).await {
                                warn!("failed to respond to daemon search request: {err}");
                            }
                        }
                        Err(err) => {
                            warn!("failed to accept daemon search request: {err}");
                        }
                    }
                }
            }
            _ = sighup.recv() => {
                info!("received SIGHUP, reloading project registry");
                if let Err(e) = reload_projects(&mut file_watcher, &mut projects).await {
                    error!("failed to reload projects: {e}");
                }
            }
            _ = sigterm.recv() => {
                info!("received SIGTERM, shutting down");
                break;
            }
            _ = tokio::time::sleep(debounce) => {
                let filtered = watcher::filter_changed_paths(file_watcher.drain_events());
                if filtered.is_empty() {
                    continue;
                }

                debug!(
                    "detected {} changed files and {} ambiguous paths",
                    filtered.file_paths.len(),
                    filtered.ambiguous_paths.len()
                );
                mark_changed_projects(&mut projects, &filtered);
                run_dirty_projects_until_clean(&file_watcher, &mut projects).await;
            }
        }
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
        | EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::DownloadFailed(err)) => {
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
        | EmbeddingLoadStatus::Unavailable(EmbeddingUnavailableReason::DownloadFailed(err)) => {
            debug!(
                "failed to prepare daemon search runtime for {} with embed_threads={embed_threads}: {err}; using FTS-only mode",
                project_root.display()
            );
        }
    }
}

async fn load_and_watch_projects(
    watcher: &mut FileWatcher,
    projects: &mut HashMap<PathBuf, ProjectState>,
) -> Result<(), OneupError> {
    let registry = Registry::load()?;

    for entry in &registry.projects {
        let Some(state) = build_project_state(entry).await? else {
            continue;
        };

        watcher.watch(&entry.project_root)?;
        projects.insert(entry.project_root.clone(), state);

        info!("watching project: {}", entry.project_root.display());
    }

    Ok(())
}

async fn reload_projects(
    watcher: &mut FileWatcher,
    projects: &mut HashMap<PathBuf, ProjectState>,
) -> Result<(), OneupError> {
    let registry = Registry::load()?;
    let registered_roots: std::collections::HashSet<PathBuf> = registry
        .projects
        .iter()
        .map(|p| p.project_root.clone())
        .collect();

    let current_roots: Vec<PathBuf> = projects.keys().cloned().collect();
    for root in &current_roots {
        if !registered_roots.contains(root) {
            info!("removing project: {}", root.display());
            watcher.unwatch(root)?;
            projects.remove(root);
        }
    }

    for entry in &registry.projects {
        if let Some(existing) = projects.get_mut(&entry.project_root) {
            if existing.indexing != entry.indexing {
                existing.indexing = entry.indexing.clone();
                info!(
                    "refreshed indexing settings for {}",
                    entry.project_root.display()
                );
            }
            continue;
        }

        let Some(state) = build_project_state(entry).await? else {
            continue;
        };

        watcher.watch(&entry.project_root)?;
        projects.insert(entry.project_root.clone(), state);

        info!("now watching project: {}", entry.project_root.display());
    }

    Ok(())
}

async fn build_project_state(entry: &ProjectEntry) -> Result<Option<ProjectState>, OneupError> {
    if !entry.project_root.exists() {
        warn!(
            "skipping non-existent project: {}",
            entry.project_root.display()
        );
        return Ok(None);
    }

    let db_path = config::project_db_path(&entry.project_root);
    let db = Db::open_rw(&db_path).await?;
    let conn = db.connect()?;
    if let Err(e) = schema::prepare_for_write(&conn).await {
        warn!(
            "skipping project {} until a clean rebuild succeeds: {e}",
            entry.project_root.display()
        );
        return Ok(None);
    }

    Ok(Some(ProjectState {
        project_root: entry.project_root.clone(),
        db,
        indexing: entry.indexing.clone(),
        embedding_runtime: EmbeddingRuntime::default(),
        run_state: ProjectRunState::default(),
    }))
}

fn normalize_relative_path(project_root: &Path, changed_path: &Path) -> Option<PathBuf> {
    let relative = changed_path.strip_prefix(project_root).ok()?;
    if relative.as_os_str().is_empty() {
        None
    } else {
        Some(relative.to_path_buf())
    }
}

fn mark_changed_projects(
    projects: &mut HashMap<PathBuf, ProjectState>,
    changes: &watcher::WatcherChanges,
) {
    for (root, state) in projects.iter_mut() {
        let scope = if changes.has_unscoped_error
            || changes
                .ambiguous_paths
                .iter()
                .any(|path| path.starts_with(root))
        {
            Some(RunScope::Full)
        } else {
            RunScope::from_paths(
                changes
                    .file_paths
                    .iter()
                    .filter(|path| path.starts_with(root))
                    .filter_map(|path| normalize_relative_path(root, path)),
            )
        };

        let Some(scope) = scope else {
            continue;
        };

        let relevant_count = changes
            .file_paths
            .iter()
            .filter(|path| path.starts_with(root))
            .count();

        let was_dirty = state.run_state.dirty;
        let was_running = state.run_state.running;
        state.run_state.mark_dirty(scope.clone());

        if was_running && !was_dirty {
            debug!(
                "project {} changed during an active run; queued one follow-up {}",
                root.display(),
                match scope {
                    RunScope::Full => "full re-index".to_string(),
                    RunScope::Paths(paths) => format!("run for {} changed paths", paths.len()),
                }
            );
        } else if !was_dirty {
            match scope {
                RunScope::Full => {
                    debug!("queued full re-index for {}", root.display());
                }
                RunScope::Paths(paths) => {
                    debug!(
                        "queued re-index for {} after {} changed paths",
                        root.display(),
                        paths.len().max(relevant_count)
                    );
                }
            }
        }
    }
}

fn next_dirty_project_root(
    projects: &HashMap<PathBuf, ProjectState>,
    preferred_root: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(preferred_root) = preferred_root {
        if projects
            .get(preferred_root)
            .is_some_and(|state| state.run_state.dirty && !state.run_state.running)
        {
            return Some(preferred_root.to_path_buf());
        }
    }

    let mut dirty_roots: Vec<PathBuf> = projects
        .iter()
        .filter(|(_, state)| state.run_state.dirty && !state.run_state.running)
        .map(|(root, _)| root.clone())
        .collect();
    dirty_roots.sort();
    dirty_roots.into_iter().next()
}

async fn run_dirty_projects_until_clean(
    watcher: &FileWatcher,
    projects: &mut HashMap<PathBuf, ProjectState>,
) {
    let mut preferred_root: Option<PathBuf> = None;

    while let Some(root) = next_dirty_project_root(projects, preferred_root.as_deref()) {
        preferred_root = None;

        let result = run_project(&root, projects).await;

        let filtered = watcher::filter_changed_paths(watcher.drain_events_nowait());
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
                info!(
                    "re-index complete for {}: {} indexed, {} skipped",
                    root.display(),
                    stats.files_indexed,
                    stats.files_skipped
                );

                if projects
                    .get(&root)
                    .is_some_and(|state| state.run_state.dirty)
                {
                    debug!(
                        "collapsed change burst for {} into one queued follow-up run",
                        root.display()
                    );
                    preferred_root = Some(root);
                }
            }
            Err(e) => {
                error!("re-index failed for {}: {e}", root.display());
            }
        }
    }
}

async fn run_project(
    root: &Path,
    projects: &mut HashMap<PathBuf, ProjectState>,
) -> Result<pipeline::PipelineStats, OneupError> {
    let (project_root, scope, setup) = {
        let state = projects
            .get_mut(root)
            .expect("dirty project must exist while running");
        let scope = state.run_state.start_run();
        let project_root = state.project_root.clone();
        let setup = (|| {
            let conn = state.db.connect()?;
            let indexing_config =
                config::resolve_indexing_config(None, None, state.indexing.as_ref())?;
            Ok((conn, indexing_config))
        })();

        (project_root, scope, setup)
    };

    let (conn, indexing_config) = match setup {
        Ok(values) => values,
        Err(e) => {
            projects
                .get_mut(root)
                .expect("dirty project must exist while finishing a failed setup")
                .run_state
                .finish_run();
            return Err(e);
        }
    };

    match &scope {
        RunScope::Full => {
            info!(
                "re-indexing full project {} (jobs={}, embed_threads={})",
                project_root.display(),
                indexing_config.jobs,
                indexing_config.embed_threads
            );
        }
        RunScope::Paths(paths) => {
            info!(
                "re-indexing {} changed files in {} (jobs={}, embed_threads={})",
                paths.len(),
                project_root.display(),
                indexing_config.jobs,
                indexing_config.embed_threads
            );
        }
    }

    let result = {
        let state = projects
            .get_mut(root)
            .expect("dirty project must exist while preparing embeddings");
        let status = state
            .embedding_runtime
            .prepare_for_indexing(indexing_config.embed_threads)
            .await;
        log_indexing_embedding_status(&project_root, indexing_config.embed_threads, &status);
        pipeline::run_with_scope(
            &conn,
            &project_root,
            state.embedding_runtime.current_embedder(),
            &scope,
            &indexing_config,
        )
        .await
    };

    projects
        .get_mut(root)
        .expect("dirty project must exist while finishing a run")
        .run_state
        .finish_run();

    result
}

async fn handle_search_request(
    projects: &mut HashMap<PathBuf, ProjectState>,
    request: SearchRequest,
) -> SearchResponse {
    let Some(state) = projects.get_mut(&request.project_root) else {
        return SearchResponse::Unavailable {
            reason: format!(
                "project {} is not registered with the daemon",
                request.project_root.display()
            ),
        };
    };

    let indexing_config = match config::resolve_indexing_config(None, None, state.indexing.as_ref())
    {
        Ok(indexing_config) => indexing_config,
        Err(err) => {
            return SearchResponse::Unavailable {
                reason: format!("failed to resolve search configuration: {err}"),
            };
        }
    };

    let conn = match state.db.connect() {
        Ok(conn) => conn,
        Err(err) => {
            return SearchResponse::Unavailable {
                reason: format!("failed to open search connection: {err}"),
            };
        }
    };

    if let Err(err) = schema::ensure_current(&conn).await {
        return SearchResponse::Unavailable {
            reason: format!("search index is unavailable: {err}"),
        };
    }

    let status = state
        .embedding_runtime
        .prepare_for_search(indexing_config.embed_threads);
    log_search_embedding_status(&state.project_root, indexing_config.embed_threads, &status);

    let results = if status.is_available() {
        let mut engine = HybridSearchEngine::new(&conn, state.embedding_runtime.current_embedder());
        engine.search(&request.query, request.limit).await
    } else {
        let engine = HybridSearchEngine::new(&conn, None);
        engine.fts_only_search(&request.query, request.limit).await
    };

    match results {
        Ok(results) => SearchResponse::Results { results },
        Err(err) => SearchResponse::Unavailable {
            reason: format!("daemon search failed: {err}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        projects.insert(
            alpha_root.clone(),
            ProjectState {
                project_root: alpha_root.clone(),
                db: alpha_db,
                indexing: None,
                embedding_runtime: EmbeddingRuntime::default(),
                run_state: ProjectRunState {
                    running: true,
                    dirty: false,
                    pending_scope: None,
                },
            },
        );
        projects.insert(
            beta_root.clone(),
            ProjectState {
                project_root: beta_root.clone(),
                db: beta_db,
                indexing: None,
                embedding_runtime: EmbeddingRuntime::default(),
                run_state: ProjectRunState::default(),
            },
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

        let alpha = &projects.get(&alpha_root).unwrap().run_state;
        assert!(alpha.running);
        assert!(alpha.dirty);
        assert_eq!(
            alpha.pending_scope,
            RunScope::from_paths([PathBuf::from("README.md"), PathBuf::from("src/lib.rs")])
        );

        let beta = &projects.get(&beta_root).unwrap().run_state;
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
        projects.insert(
            alpha_root.clone(),
            ProjectState {
                project_root: alpha_root.clone(),
                db: alpha_db,
                indexing: None,
                embedding_runtime: EmbeddingRuntime::default(),
                run_state: ProjectRunState::default(),
            },
        );
        projects.insert(
            beta_root.clone(),
            ProjectState {
                project_root: beta_root.clone(),
                db: beta_db,
                indexing: None,
                embedding_runtime: EmbeddingRuntime::default(),
                run_state: ProjectRunState::default(),
            },
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
            projects.get(&alpha_root).unwrap().run_state.pending_scope,
            Some(RunScope::Full)
        );
        assert!(projects
            .get(&beta_root)
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
            projects.get(&alpha_root).unwrap().run_state.pending_scope,
            Some(RunScope::Full)
        );
        assert_eq!(
            projects.get(&beta_root).unwrap().run_state.pending_scope,
            Some(RunScope::Full)
        );
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
        projects.insert(
            alpha_root.clone(),
            ProjectState {
                project_root: alpha_root.clone(),
                db: alpha_db,
                indexing: None,
                embedding_runtime: EmbeddingRuntime::default(),
                run_state: ProjectRunState {
                    running: false,
                    dirty: true,
                    pending_scope: Some(
                        RunScope::from_paths([PathBuf::from("src/lib.rs")]).unwrap(),
                    ),
                },
            },
        );
        projects.insert(
            beta_root.clone(),
            ProjectState {
                project_root: beta_root.clone(),
                db: beta_db,
                indexing: None,
                embedding_runtime: EmbeddingRuntime::default(),
                run_state: ProjectRunState {
                    running: false,
                    dirty: true,
                    pending_scope: Some(
                        RunScope::from_paths([PathBuf::from("src/mod.rs")]).unwrap(),
                    ),
                },
            },
        );

        let preferred = next_dirty_project_root(&projects, Some(beta_root.as_path()));
        assert_eq!(preferred, Some(beta_root.clone()));

        projects.get_mut(&beta_root).unwrap().run_state.dirty = false;
        let fallback = next_dirty_project_root(&projects, Some(beta_root.as_path()));
        assert_eq!(fallback, Some(alpha_root));
    }
}
