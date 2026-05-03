use std::path::Path;

use crate::shared::config;
use crate::shared::types::{DaemonProjectStatus, IndexProgress};

const INDEX_PROGRESS_FILE_NAME: &str = "index_status.json";

pub(crate) fn read_index_progress(project_root: &Path) -> Option<IndexProgress> {
    let path = config::project_dot_dir(project_root).join(INDEX_PROGRESS_FILE_NAME);
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

pub(crate) fn read_daemon_status(project_root: &Path) -> Option<DaemonProjectStatus> {
    let path = config::project_daemon_status_path(project_root);
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}
