use clap::Args;
use std::path::Path;

use crate::cli::output::{
    formatter_for, LifecycleState, ProjectListIndexStatus, ProjectListInfo, ProjectListItem,
};
use crate::cli::project_status_files::{
    read_daemon_context_status, read_daemon_status_for_context, read_index_progress,
};
use crate::daemon::lifecycle;
use crate::daemon::registry::{ProjectEntry, Registry};
use crate::shared::config;
use crate::shared::types::{
    DaemonContextStatus, DaemonRefreshState, DaemonWatchStatus, IndexProgress, IndexState,
    OutputFormat,
};
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
    let context_id = entry.context_id();
    let index_progress = read_index_progress(&entry.project_root).filter(|progress| {
        progress
            .context_id
            .as_deref()
            .is_none_or(|progress_context_id| progress_context_id == context_id.as_str())
    });
    let daemon_context_status = read_daemon_context_status(&entry.project_root, &context_id);
    let daemon_status = read_daemon_status_for_context(&entry.project_root, &context_id);
    let (index_status, files, segments) =
        read_index_health(&entry.project_root, Some(context_id.as_str())).await;
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
        context_id,
        main_worktree_root: entry.main_worktree_root().to_path_buf(),
        worktree_role: entry.worktree_role(),
        branch_name: entry.branch_name.clone(),
        branch_ref: entry.branch_ref.clone(),
        branch_status: entry.branch_status(),
        head_oid: entry.head_oid.clone(),
        watch_status: watch_status(daemon_running, daemon_context_status.as_ref()),
        last_update_state: last_update_state(
            daemon_context_status.as_ref(),
            index_progress.as_ref(),
        ),
        last_update_started_at: daemon_context_status
            .as_ref()
            .and_then(|status| status.last_refresh_started_at.as_ref().cloned()),
        last_update_completed_at: daemon_context_status
            .as_ref()
            .and_then(|status| status.last_refresh_completed_at.as_ref().cloned()),
        last_update_error: daemon_context_status
            .as_ref()
            .and_then(|status| status.last_refresh_error.clone()),
        registered_at: entry.registered_at.clone(),
        daemon_running,
        index_status,
        files,
        segments,
        last_file_check_at: daemon_status.map(|status| status.last_file_check_at),
        index_progress,
    }
}

fn watch_status(daemon_running: bool, status: Option<&DaemonContextStatus>) -> DaemonWatchStatus {
    status.map(|status| status.watch_status).unwrap_or_else(|| {
        if daemon_running {
            DaemonWatchStatus::Unknown
        } else {
            DaemonWatchStatus::DaemonStopped
        }
    })
}

fn last_update_state(
    status: Option<&DaemonContextStatus>,
    progress: Option<&IndexProgress>,
) -> DaemonRefreshState {
    status
        .map(|status| status.last_refresh_state)
        .unwrap_or_else(|| match progress.map(|progress| progress.state) {
            Some(IndexState::Running) => DaemonRefreshState::Running,
            Some(IndexState::Complete) => DaemonRefreshState::Complete,
            _ => DaemonRefreshState::Unknown,
        })
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
    context_id: Option<&str>,
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

    let files = match context_id {
        Some(context_id) => segments::count_files_for_context(&conn, context_id)
            .await
            .ok(),
        None => segments::count_files(&conn).await.ok(),
    };
    let segments = match context_id {
        Some(context_id) => segments::count_segments_for_context(&conn, context_id)
            .await
            .ok(),
        None => segments::count_segments(&conn).await.ok(),
    };
    let status = if segments == Some(0) {
        ProjectListIndexStatus::NotBuilt
    } else {
        ProjectListIndexStatus::Ready
    };
    (status, files, segments)
}
