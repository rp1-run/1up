use std::path::Path;

use crate::shared::config;
use crate::shared::types::{
    DaemonContextStatus, DaemonContextStatusFile, DaemonProjectStatus, IndexProgress,
};

const INDEX_PROGRESS_FILE_NAME: &str = "index_status.json";
const DAEMON_CONTEXT_STATUS_FILE_NAME: &str = "daemon_context_status.json";

pub(crate) fn read_index_progress(project_root: &Path) -> Option<IndexProgress> {
    let path = config::project_dot_dir(project_root).join(INDEX_PROGRESS_FILE_NAME);
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

pub(crate) fn read_daemon_status(project_root: &Path) -> Option<DaemonProjectStatus> {
    read_legacy_daemon_status(project_root)
}

pub(crate) fn read_daemon_status_for_context(
    project_root: &Path,
    context_id: &str,
) -> Option<DaemonProjectStatus> {
    read_daemon_context_status(project_root, context_id)
        .and_then(|status| {
            status
                .last_file_check_at
                .map(|last_file_check_at| DaemonProjectStatus { last_file_check_at })
        })
        .or_else(|| read_daemon_status(project_root))
}

pub(crate) fn read_daemon_context_status(
    project_root: &Path,
    context_id: &str,
) -> Option<DaemonContextStatus> {
    read_daemon_context_status_file(project_root)
        .and_then(|file| file.contexts.get(context_id).cloned())
}

fn read_daemon_context_status_file(project_root: &Path) -> Option<DaemonContextStatusFile> {
    let path = config::project_dot_dir(project_root).join(DAEMON_CONTEXT_STATUS_FILE_NAME);
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn read_legacy_daemon_status(project_root: &Path) -> Option<DaemonProjectStatus> {
    let path = config::project_daemon_status_path(project_root);
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::types::{
        BranchStatus, DaemonContextStatus, DaemonContextStatusFile, DaemonRefreshState,
        DaemonWatchStatus,
    };
    use chrono::Utc;
    use std::collections::BTreeMap;

    #[test]
    fn read_daemon_status_for_context_prefers_matching_context_status() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path();
        std::fs::create_dir_all(config::project_dot_dir(project_root)).unwrap();

        let legacy_check = Utc::now();
        std::fs::write(
            config::project_daemon_status_path(project_root),
            serde_json::to_string(&DaemonProjectStatus {
                last_file_check_at: legacy_check,
            })
            .unwrap(),
        )
        .unwrap();

        let context_check = legacy_check + chrono::Duration::seconds(5);
        let file = DaemonContextStatusFile {
            contexts: BTreeMap::from([(
                "ctx".to_string(),
                DaemonContextStatus {
                    context_id: "ctx".to_string(),
                    source_root: Some(project_root.to_path_buf()),
                    watch_status: DaemonWatchStatus::Watching,
                    last_file_check_at: Some(context_check),
                    last_refresh_state: DaemonRefreshState::Complete,
                    last_refresh_started_at: None,
                    last_refresh_completed_at: Some(context_check),
                    last_refresh_error: None,
                    branch_name: Some("main".to_string()),
                    branch_status: BranchStatus::Named,
                },
            )]),
        };
        std::fs::write(
            config::project_dot_dir(project_root).join(DAEMON_CONTEXT_STATUS_FILE_NAME),
            serde_json::to_string(&file).unwrap(),
        )
        .unwrap();

        let status = read_daemon_status_for_context(project_root, "ctx").unwrap();
        assert_eq!(status.last_file_check_at, context_check);
    }

    #[test]
    fn read_daemon_status_for_context_ignores_other_context_status() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path();
        std::fs::create_dir_all(config::project_dot_dir(project_root)).unwrap();
        let legacy_check = Utc::now();
        std::fs::write(
            config::project_daemon_status_path(project_root),
            serde_json::to_string(&DaemonProjectStatus {
                last_file_check_at: legacy_check,
            })
            .unwrap(),
        )
        .unwrap();

        let other_context_check = legacy_check + chrono::Duration::seconds(5);
        let file = DaemonContextStatusFile {
            contexts: BTreeMap::from([(
                "other".to_string(),
                DaemonContextStatus {
                    context_id: "other".to_string(),
                    source_root: Some(project_root.to_path_buf()),
                    watch_status: DaemonWatchStatus::Watching,
                    last_file_check_at: Some(other_context_check),
                    last_refresh_state: DaemonRefreshState::Complete,
                    last_refresh_started_at: None,
                    last_refresh_completed_at: Some(other_context_check),
                    last_refresh_error: None,
                    branch_name: Some("main".to_string()),
                    branch_status: BranchStatus::Named,
                },
            )]),
        };
        std::fs::write(
            config::project_dot_dir(project_root).join(DAEMON_CONTEXT_STATUS_FILE_NAME),
            serde_json::to_string(&file).unwrap(),
        )
        .unwrap();

        let status = read_daemon_status_for_context(project_root, "missing").unwrap();
        assert_eq!(status.last_file_check_at, legacy_check);
    }
}
