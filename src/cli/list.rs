use clap::Args;
use std::path::Path;

use crate::cli::output::{
    formatter_for, LifecycleState, ProjectListIndexStatus, ProjectListInfo, ProjectListItem,
};
use crate::cli::project_status_files::{read_daemon_status, read_index_progress};
use crate::daemon::lifecycle;
use crate::daemon::registry::{ProjectEntry, Registry};
use crate::shared::config;
use crate::shared::types::{IndexProgress, IndexState, OutputFormat};
use crate::storage::db::Db;
use crate::storage::schema;
use crate::storage::segments;

#[derive(Args)]
pub struct ListArgs {
    /// Print stable plain text output for simple scripts
    #[arg(long, conflicts_with = "format")]
    pub plain: bool,

    /// Output format override (defaults to human)
    #[arg(long, short = 'f', hide = true, conflicts_with = "plain")]
    pub format: Option<OutputFormat>,
}

pub async fn exec(_args: ListArgs, format: OutputFormat) -> anyhow::Result<()> {
    let fmt = formatter_for(format);
    let registry = Registry::load()?;
    let daemon_running = lifecycle::is_daemon_running()?.is_some();
    let mut projects = Vec::with_capacity(registry.projects.len());

    for entry in &registry.projects {
        projects.push(project_list_item(entry, daemon_running).await);
    }

    println!("{}", fmt.format_project_list(&ProjectListInfo { projects }));
    Ok(())
}

async fn project_list_item(entry: &ProjectEntry, daemon_running: bool) -> ProjectListItem {
    let index_progress = read_index_progress(&entry.project_root);
    let daemon_status = read_daemon_status(&entry.project_root);
    let (index_status, files, segments) = read_index_health(&entry.project_root).await;
    let files = files.or_else(|| {
        index_progress
            .as_ref()
            .map(|progress| progress.files_indexed as u64)
    });
    let segments = segments.or_else(|| {
        index_progress
            .as_ref()
            .map(|progress| progress.segments_stored as u64)
    });

    ProjectListItem {
        project_id: entry.project_id.clone(),
        state: project_state(daemon_running, index_progress.as_ref()),
        project_root: entry.project_root.clone(),
        source_root: entry.source_root().to_path_buf(),
        registered_at: entry.registered_at.clone(),
        daemon_running,
        index_status,
        files,
        segments,
        last_file_check_at: daemon_status.map(|status| status.last_file_check_at),
        index_progress,
    }
}

fn project_state(daemon_running: bool, progress: Option<&IndexProgress>) -> LifecycleState {
    if progress.is_some_and(|progress| progress.state == IndexState::Running) {
        LifecycleState::Indexing
    } else if daemon_running {
        LifecycleState::Active
    } else {
        LifecycleState::Registered
    }
}

async fn read_index_health(
    project_root: &Path,
) -> (ProjectListIndexStatus, Option<u64>, Option<u64>) {
    let db_path = config::project_db_path(project_root);
    if !db_path.exists() {
        return (ProjectListIndexStatus::NotBuilt, None, None);
    }

    let Ok(db) = Db::open_ro(&db_path).await else {
        return (ProjectListIndexStatus::Unavailable, None, None);
    };
    let Ok(conn) = db.connect() else {
        return (ProjectListIndexStatus::Unavailable, None, None);
    };
    if schema::ensure_current(&conn).await.is_err() {
        return (ProjectListIndexStatus::Unavailable, None, None);
    }

    let files = segments::count_files(&conn).await.ok();
    let segments = segments::count_segments(&conn).await.ok();
    (ProjectListIndexStatus::Ready, files, segments)
}
