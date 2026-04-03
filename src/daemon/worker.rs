use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tokio::signal::unix::{signal, SignalKind};
use tracing::{debug, error, info, warn};

use crate::daemon::lifecycle;
use crate::daemon::registry::{ProjectEntry, Registry};
use crate::daemon::watcher::{self, FileWatcher};
use crate::indexer::embedder::{self, Embedder};
use crate::indexer::pipeline;
use crate::shared::config;
use crate::shared::constants::WATCHER_DEBOUNCE_MS;
use crate::shared::errors::OneupError;
use crate::shared::types::IndexingConfig;
use crate::storage::{db::Db, schema};

#[derive(Debug, Default)]
struct ProjectRunState {
    running: bool,
    dirty: bool,
    pending_change_count: usize,
}

impl ProjectRunState {
    fn mark_dirty(&mut self, change_count: usize) {
        if change_count == 0 {
            return;
        }

        self.dirty = true;
        self.pending_change_count = self.pending_change_count.saturating_add(change_count);
    }

    fn start_run(&mut self) -> usize {
        debug_assert!(self.dirty, "only dirty projects should start a run");
        self.running = true;
        self.dirty = false;
        let pending_change_count = self.pending_change_count;
        self.pending_change_count = 0;
        pending_change_count
    }

    fn finish_run(&mut self) {
        self.running = false;
    }
}

struct ProjectState {
    project_root: PathBuf,
    db: Db,
    indexing: Option<IndexingConfig>,
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

    load_and_watch_projects(&mut file_watcher, &mut projects).await?;

    let debounce = std::time::Duration::from_millis(WATCHER_DEBOUNCE_MS);

    loop {
        tokio::select! {
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

                debug!("detected {} changed files", filtered.len());
                mark_changed_projects(&mut projects, &filtered);
                run_dirty_projects_until_clean(&file_watcher, &mut projects).await;
            }
        }
    }

    if let Err(e) = file_watcher.unwatch_all() {
        warn!("failed to unwatch on shutdown: {e}");
    }

    info!("daemon worker exiting");
    Ok(())
}

fn load_embedder(embed_threads: usize) -> Option<Embedder> {
    if !embedder::is_model_available() {
        if embedder::is_download_failed() {
            warn!(
                "embedding model download previously failed; daemon will index without embeddings"
            );
        } else {
            debug!("embedding model not available; daemon will index without embeddings");
        }
        return None;
    }

    let dir = match config::model_dir() {
        Ok(dir) => dir,
        Err(e) => {
            warn!("failed to resolve embedding model dir: {e}");
            return None;
        }
    };

    match Embedder::from_dir_with_threads(&dir, embed_threads) {
        Ok(embedder) => Some(embedder),
        Err(e) => {
            warn!(
                "failed to load embedding model with embed_threads={embed_threads}: {e}; daemon will index without embeddings"
            );
            None
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
        run_state: ProjectRunState::default(),
    }))
}

fn mark_changed_projects(projects: &mut HashMap<PathBuf, ProjectState>, changed_paths: &[PathBuf]) {
    for (root, state) in projects.iter_mut() {
        let relevant_count = changed_paths
            .iter()
            .filter(|path| path.starts_with(root))
            .count();

        if relevant_count == 0 {
            continue;
        }

        let was_dirty = state.run_state.dirty;
        let was_running = state.run_state.running;
        state.run_state.mark_dirty(relevant_count);

        if was_running && !was_dirty {
            debug!(
                "project {} changed during an active run; queued one follow-up pass",
                root.display()
            );
        } else if !was_dirty {
            debug!(
                "queued re-index for {} after {} changed paths",
                root.display(),
                relevant_count
            );
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
                "detected {} changed files while re-indexing",
                filtered.len()
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
    let (project_root, pending_change_count, setup) = {
        let state = projects
            .get_mut(root)
            .expect("dirty project must exist while running");
        let pending_change_count = state.run_state.start_run();
        let project_root = state.project_root.clone();
        let setup = (|| {
            let conn = state.db.connect()?;
            let indexing_config =
                config::resolve_indexing_config(None, None, state.indexing.as_ref())?;
            Ok((conn, indexing_config))
        })();

        (project_root, pending_change_count, setup)
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

    info!(
        "re-indexing {} changed files in {} (jobs={}, embed_threads={})",
        pending_change_count,
        project_root.display(),
        indexing_config.jobs,
        indexing_config.embed_threads
    );

    let mut embedder = load_embedder(indexing_config.embed_threads);
    let result =
        pipeline::run_with_config(&conn, &project_root, embedder.as_mut(), &indexing_config).await;

    projects
        .get_mut(root)
        .expect("dirty project must exist while finishing a run")
        .run_state
        .finish_run();

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_state_collapses_bursts_into_follow_up() {
        let mut state = ProjectRunState::default();
        state.mark_dirty(2);
        state.mark_dirty(3);

        assert!(state.dirty);
        assert_eq!(state.pending_change_count, 5);

        let pending = state.start_run();
        assert_eq!(pending, 5);
        assert!(state.running);
        assert!(!state.dirty);
        assert_eq!(state.pending_change_count, 0);

        state.mark_dirty(4);
        assert!(state.dirty);
        assert_eq!(state.pending_change_count, 4);

        state.finish_run();
        assert!(!state.running);
        assert!(state.dirty);
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
                run_state: ProjectRunState {
                    running: false,
                    dirty: true,
                    pending_change_count: 1,
                },
            },
        );
        projects.insert(
            beta_root.clone(),
            ProjectState {
                project_root: beta_root.clone(),
                db: beta_db,
                indexing: None,
                run_state: ProjectRunState {
                    running: false,
                    dirty: true,
                    pending_change_count: 1,
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
