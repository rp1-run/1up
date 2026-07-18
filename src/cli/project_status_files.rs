use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use serde::de::DeserializeOwned;

use crate::shared::config;
use crate::shared::constants::{STATUS_READ_RETRY_ATTEMPTS, STATUS_READ_RETRY_DELAY_MS};
use crate::shared::fs::atomic_replace_within_project_root;
use crate::shared::progress::{read_status_file, StatusFileRead};
use crate::shared::types::{
    DaemonContextStatus, DaemonContextStatusFile, DaemonProjectStatus, IndexProgress,
};

const INDEX_PROGRESS_FILE_NAME: &str = "index_status.json";
const DAEMON_CONTEXT_STATUS_FILE_NAME: &str = "daemon_context_status.json";

/// Read a JSON status file for a read-only display surface (`1up status` /
/// `1up list`).
///
/// Retry-or-propagate policy for this call-site class: an `Absent` file is the
/// legitimate not-yet-written state and resolves to `None` silently. An
/// `Unreadable` (torn/corrupt) file is retried up to
/// [`STATUS_READ_RETRY_ATTEMPTS`] times with a [`STATUS_READ_RETRY_DELAY_MS`]
/// pause between attempts (a torn write from a concurrent atomic replace settles
/// within one `rename(2)`); if it is still unparseable we `tracing::error!` and
/// return `None`. Returning `None` here means "no information" — the surface
/// renders as unavailable/indeterminate and never fabricates zero progress from
/// a corrupt file, and the error (visible at default verbosity) ensures the
/// corruption is not swallowed
/// silently. Sync (`std::thread::sleep`) because every display caller is sync
/// with respect to this read.
fn read_status_for_display<T: DeserializeOwned>(path: &Path, what: &str) -> Option<T> {
    for attempt in 1..=STATUS_READ_RETRY_ATTEMPTS {
        match read_status_file::<T>(path) {
            StatusFileRead::Absent => return None,
            StatusFileRead::Parsed(value) => return Some(value),
            StatusFileRead::Unreadable(err) => {
                if attempt == STATUS_READ_RETRY_ATTEMPTS {
                    tracing::error!(
                        "{what} file {} is unreadable after {STATUS_READ_RETRY_ATTEMPTS} attempts ({err}); reporting as unavailable, not empty progress",
                        path.display(),
                    );
                    return None;
                }
                std::thread::sleep(Duration::from_millis(STATUS_READ_RETRY_DELAY_MS));
            }
        }
    }
    None
}

pub(crate) fn read_index_progress(project_root: &Path) -> Option<IndexProgress> {
    let path = config::project_dot_dir(project_root).join(INDEX_PROGRESS_FILE_NAME);
    // Retry-or-propagate: Absent -> None (not yet written); Unreadable -> retry
    // then error + None (never rendered as valid empty progress).
    read_status_for_display(&path, "index_status.json")
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
    // Retry-or-propagate: Absent -> None (not yet written); Unreadable -> retry
    // then warn + None. `prune_daemon_context_status` treats `None` as a no-op,
    // which correctly leaves a corrupt file intact instead of clobbering it.
    read_status_for_display(&path, "daemon_context_status.json")
}

/// Remove the given context ids from `daemon_context_status.json` so pruned contexts
/// stop appearing in status snapshots. Used by `1up gc --apply`. Returns how many
/// entries were removed; a missing or unreadable file is treated as empty (no-op).
/// The write is clamped to the project root, matching the daemon's own secure write.
pub(crate) fn prune_daemon_context_status(
    project_root: &Path,
    context_ids: &HashSet<String>,
) -> anyhow::Result<usize> {
    let Some(mut file) = read_daemon_context_status_file(project_root) else {
        return Ok(0);
    };
    let before = file.contexts.len();
    file.contexts
        .retain(|context_id, _| !context_ids.contains(context_id));
    let removed = before - file.contexts.len();
    if removed == 0 {
        return Ok(0);
    }

    let path = config::project_dot_dir(project_root).join(DAEMON_CONTEXT_STATUS_FILE_NAME);
    let payload = serde_json::to_vec_pretty(&file)?;
    atomic_replace_within_project_root(&path, &payload, project_root)?;
    Ok(removed)
}

fn read_legacy_daemon_status(project_root: &Path) -> Option<DaemonProjectStatus> {
    let path = config::project_daemon_status_path(project_root);
    // Retry-or-propagate: Absent -> None (not yet written); Unreadable -> retry
    // then warn + None (never rendered as a valid check timestamp).
    read_status_for_display(&path, "daemon_status.json")
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
