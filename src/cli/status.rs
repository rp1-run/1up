use clap::Args;

use std::path::Path;

use crate::cli::output::{formatter_for, LifecycleState, StatusInfo};
use crate::cli::project_status_files::{
    read_daemon_context_status, read_daemon_status_for_context, read_index_progress,
};
use crate::daemon::lifecycle;
use crate::daemon::registry::{ProjectEntry, Registry};
use crate::shared::config;
use crate::shared::project;
use crate::shared::types::{
    DaemonContextStatus, DaemonRefreshState, DaemonWatchStatus, IndexProgress, IndexState,
    OutputFormat, WorktreeContext,
};
use crate::storage::db::Db;
use crate::storage::schema;
use crate::storage::segments;

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

pub async fn exec(args: StatusArgs, format: OutputFormat) -> anyhow::Result<()> {
    let resolved = crate::shared::project::resolve_project_root(Path::new(&args.path))?;
    let project_root = resolved.state_root;
    let resolved_source_root = resolved.source_root;
    let worktree_context = resolved.worktree_context;
    let fmt = formatter_for(format);
    let registry = Registry::load()?;
    let registry_entry = find_registered_project(&registry.projects, &worktree_context);
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
    let daemon_context_status =
        read_daemon_context_status(&project_root, &worktree_context.context_id);
    let daemon_status = read_daemon_status_for_context(&project_root, &worktree_context.context_id);

    let (indexed_files, total_segments) = {
        if db_path.exists() {
            match Db::open_ro(&db_path).await {
                Ok(db) => match db.connect() {
                    Ok(conn) => {
                        if schema::ensure_current(&conn).await.is_ok() {
                            index_readable = true;
                            let files = segments::count_files_for_context(
                                &conn,
                                &worktree_context.context_id,
                            )
                            .await
                            .ok();
                            let segs = segments::count_segments_for_context(
                                &conn,
                                &worktree_context.context_id,
                            )
                            .await
                            .ok();
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

    let index_progress = read_index_progress(&project_root).filter(|progress| {
        progress
            .context_id
            .as_deref()
            .is_none_or(|context_id| context_id == worktree_context.context_id.as_str())
    });
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
        context_id: worktree_context.context_id.clone(),
        main_worktree_root: worktree_context.main_worktree_root.clone(),
        worktree_role: worktree_context.worktree_role,
        branch_name: worktree_context.branch_name.clone(),
        branch_ref: worktree_context.branch_ref.clone(),
        branch_status: worktree_context.branch_status,
        head_oid: worktree_context.head_oid.clone(),
        watch_status: watch_status(registered, daemon_running, daemon_context_status.as_ref()),
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
    context: &WorktreeContext,
) -> Option<&'a ProjectEntry> {
    let canonical = project::canonical_project_root(&context.state_root);
    let canonical_source = project::canonical_project_root(&context.source_root);
    projects
        .iter()
        .find(|project| project.context_id.as_deref() == Some(context.context_id.as_str()))
        .or_else(|| {
            projects.iter().find(|project| {
                project.project_root == canonical
                    && project.source_root() == canonical_source.as_path()
                    && project.branch_ref.as_deref() == context.branch_ref.as_deref()
                    && project.branch_status() == context.branch_status
            })
        })
        .or_else(|| {
            projects
                .iter()
                .find(|project| project.project_root == canonical)
        })
}

fn watch_status(
    registered: bool,
    daemon_running: bool,
    status: Option<&DaemonContextStatus>,
) -> DaemonWatchStatus {
    status.map(|status| status.watch_status).unwrap_or_else(|| {
        if registered && !daemon_running {
            DaemonWatchStatus::DaemonStopped
        } else {
            DaemonWatchStatus::Unknown
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

    if !project_initialized
        && !index_readable
        && !index_present
        && index_progress.is_none()
        && !registered
    {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::types::IndexPhase;

    #[test]
    fn lifecycle_is_stopped_when_index_artifact_exists_without_project_id() {
        let state = derive_lifecycle_state(false, false, false, true, false, None);

        assert_eq!(state, LifecycleState::Stopped);
    }

    #[test]
    fn lifecycle_is_stopped_when_progress_artifact_exists_without_project_id() {
        let progress = IndexProgress::watch(IndexState::Complete, IndexPhase::Complete, "done");
        let state = derive_lifecycle_state(false, false, false, false, false, Some(&progress));

        assert_eq!(state, LifecycleState::Stopped);
    }

    #[test]
    fn lifecycle_is_not_started_when_no_project_or_index_artifacts_exist() {
        let state = derive_lifecycle_state(false, false, false, false, false, None);

        assert_eq!(state, LifecycleState::NotStarted);
    }
}
