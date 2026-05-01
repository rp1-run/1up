use clap::Args;

use std::path::{Path, PathBuf};

use crate::cli::output::{formatter_for, LifecycleState, StatusInfo};
use crate::daemon::lifecycle;
use crate::daemon::registry::{ProjectEntry, Registry};
use crate::shared::config;
use crate::shared::project;
use crate::shared::types::{DaemonProjectStatus, IndexProgress, IndexState, OutputFormat};
use crate::storage::db::Db;
use crate::storage::schema;
use crate::storage::segments;

const INDEX_PROGRESS_FILE_NAME: &str = "index_status.json";

#[derive(Args)]
pub struct StatusArgs {
    /// Project root to inspect (defaults to current directory)
    #[arg(default_value = ".")]
    pub path: String,

    /// Print stable plain text output for simple scripts
    #[arg(long, conflicts_with = "format")]
    pub plain: bool,

    /// Output format override (defaults to human)
    #[arg(long, short = 'f', hide = true, conflicts_with = "plain")]
    pub format: Option<OutputFormat>,
}

fn read_index_progress(project_root: &std::path::Path) -> Option<IndexProgress> {
    let path = config::project_dot_dir(project_root).join(INDEX_PROGRESS_FILE_NAME);
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn read_daemon_status(project_root: &std::path::Path) -> Option<DaemonProjectStatus> {
    let path = config::project_daemon_status_path(project_root);
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

pub async fn exec(args: StatusArgs, format: OutputFormat) -> anyhow::Result<()> {
    let resolved = crate::shared::project::resolve_project_root(Path::new(&args.path))?;
    let project_root = resolved.state_root;
    let resolved_source_root = resolved.source_root;
    let fmt = formatter_for(format);
    let registry = Registry::load()?;
    let registry_entry = find_registered_project(&registry.projects, &project_root);
    let registered = registry_entry.is_some();
    let source_root = registry_entry
        .map(|entry| entry.source_root().to_path_buf())
        .unwrap_or(resolved_source_root);

    let (daemon_running, pid) = match lifecycle::is_daemon_running()? {
        Some(pid) => (true, Some(pid)),
        None => (false, None),
    };

    let project_id = project::read_project_id(&project_root).ok();
    let project_initialized = project_id.is_some();
    let db_path = config::project_db_path(&project_root);
    let index_present = db_path.exists();
    let mut index_readable = false;
    let daemon_status = read_daemon_status(&project_root);

    let (indexed_files, total_segments) = {
        if db_path.exists() {
            match Db::open_ro(&db_path).await {
                Ok(db) => match db.connect() {
                    Ok(conn) => {
                        if schema::ensure_current(&conn).await.is_ok() {
                            index_readable = true;
                            let files = segments::count_files(&conn).await.ok();
                            let segs = segments::count_segments(&conn).await.ok();
                            (files, segs)
                        } else {
                            (None, None)
                        }
                    }
                    Err(_) => (None, None),
                },
                Err(_) => (None, None),
            }
        } else {
            (None, None)
        }
    };

    let index_progress = read_index_progress(&project_root);
    let lifecycle_state = derive_lifecycle_state(
        registered,
        daemon_running,
        project_initialized,
        index_present,
        index_readable,
        index_progress.as_ref(),
    );

    let status = StatusInfo {
        lifecycle_state,
        registered,
        daemon_running,
        pid,
        project_initialized,
        indexed_files,
        total_segments,
        project_id,
        project_root,
        source_root,
        index_present,
        index_readable,
        last_file_check_at: daemon_status.map(|status| status.last_file_check_at),
        index_progress,
    };

    println!("{}", fmt.format_status(&status));
    Ok(())
}

fn find_registered_project<'a>(
    projects: &'a [ProjectEntry],
    project_root: &Path,
) -> Option<&'a ProjectEntry> {
    let canonical = canonical_project_root(project_root);
    projects
        .iter()
        .find(|project| project.project_root == canonical)
}

fn canonical_project_root(project_root: &Path) -> PathBuf {
    project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf())
}

fn derive_lifecycle_state(
    registered: bool,
    daemon_running: bool,
    project_initialized: bool,
    index_present: bool,
    index_readable: bool,
    index_progress: Option<&IndexProgress>,
) -> LifecycleState {
    if index_progress.is_some_and(|progress| progress.state == IndexState::Running) {
        return LifecycleState::Indexing;
    }

    if !project_initialized && !index_readable && !registered {
        return LifecycleState::NotStarted;
    }

    if registered && daemon_running {
        return LifecycleState::Active;
    }

    if registered {
        return LifecycleState::Registered;
    }

    if project_initialized || index_present || index_progress.is_some() {
        return LifecycleState::Stopped;
    }

    LifecycleState::NotStarted
}
