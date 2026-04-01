use std::collections::HashMap;
use std::path::PathBuf;
use tokio::signal::unix::{signal, SignalKind};
use tracing::{debug, error, info, warn};

use crate::daemon::lifecycle;
use crate::daemon::registry::Registry;
use crate::daemon::watcher::{self, FileWatcher};
use crate::indexer::embedder::{self, Embedder};
use crate::indexer::pipeline;
use crate::shared::config;
use crate::shared::constants::WATCHER_DEBOUNCE_MS;
use crate::shared::errors::OneupError;
use crate::storage::{db::Db, schema};

struct ProjectState {
    #[allow(dead_code)]
    project_root: PathBuf,
    db: Db,
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
    let mut embedder = match load_embedder() {
        Ok(emb) => Some(emb),
        Err(e) => {
            warn!("embedding model not available ({e}); daemon will index without embeddings (semantic search degraded to FTS-only)");
            None
        }
    };

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
                let changed = file_watcher.drain_events();
                if !changed.is_empty() {
                    let filtered = watcher::filter_changed_paths(changed);
                    if !filtered.is_empty() {
                        debug!("detected {} changed files", filtered.len());
                        for (root, state) in &projects {
                            let conn = match state.db.connect() {
                                Ok(c) => c,
                                Err(e) => {
                                    error!("db connect failed for {}: {e}", root.display());
                                    continue;
                                }
                            };

                            let relevant: Vec<&PathBuf> = filtered
                                .iter()
                                .filter(|p| p.starts_with(root))
                                .collect();

                            if relevant.is_empty() {
                                continue;
                            }

                            info!(
                                "re-indexing {} files in {}",
                                relevant.len(),
                                root.display()
                            );

                            match pipeline::run(&conn, root, embedder.as_mut()).await {
                                Ok(stats) => {
                                    info!(
                                        "re-index complete: {} indexed, {} skipped",
                                        stats.files_indexed, stats.files_skipped
                                    );
                                }
                                Err(e) => {
                                    error!("re-index failed for {}: {e}", root.display());
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    if let Err(e) = file_watcher.unwatch_all() {
        warn!("failed to unwatch on shutdown: {e}");
    }

    info!("daemon worker exiting");
    Ok(())
}

fn load_embedder() -> Result<Embedder, OneupError> {
    if !embedder::is_model_available() {
        return Err(crate::shared::errors::EmbeddingError::ModelNotAvailable(
            "model not downloaded".to_string(),
        )
        .into());
    }

    let dir = config::model_dir()?;
    Embedder::from_dir(&dir)
}

async fn load_and_watch_projects(
    watcher: &mut FileWatcher,
    projects: &mut HashMap<PathBuf, ProjectState>,
) -> Result<(), OneupError> {
    let registry = Registry::load()?;

    for entry in &registry.projects {
        if !entry.project_root.exists() {
            warn!(
                "skipping non-existent project: {}",
                entry.project_root.display()
            );
            continue;
        }

        let db_path = config::project_db_path(&entry.project_root);
        let db = Db::open_rw(&db_path).await?;
        let conn = db.connect()?;
        schema::migrate(&conn).await?;

        watcher.watch(&entry.project_root)?;

        projects.insert(
            entry.project_root.clone(),
            ProjectState {
                project_root: entry.project_root.clone(),
                db,
            },
        );

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
        if projects.contains_key(&entry.project_root) {
            continue;
        }

        if !entry.project_root.exists() {
            warn!(
                "skipping non-existent project: {}",
                entry.project_root.display()
            );
            continue;
        }

        let db_path = config::project_db_path(&entry.project_root);
        let db = Db::open_rw(&db_path).await?;
        let conn = db.connect()?;
        schema::migrate(&conn).await?;

        watcher.watch(&entry.project_root)?;

        projects.insert(
            entry.project_root.clone(),
            ProjectState {
                project_root: entry.project_root.clone(),
                db,
            },
        );

        info!("now watching project: {}", entry.project_root.display());
    }

    Ok(())
}
