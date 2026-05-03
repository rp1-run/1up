use chrono::{DateTime, Utc};
use colored::Colorize;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::shared::progress::{ProgressState, ProgressUi};
use crate::shared::types::{IndexPhase, IndexProgress, IndexState, OutputFormat};
use crate::shared::update::{InstallChannel, UpdateStatus};

/// Rendering contract for the maintenance command surface (`start`, `stop`,
/// `status`, `init`, `index`, `reindex`, `update`).
///
/// Core agent-facing commands (`search`, `get`, `symbol`, `impact`, `context`,
/// `structural`) bypass this trait entirely and render through
/// `crate::cli::lean` instead. The lean grammar is the single machine contract
/// for those commands, so they never participate in human/plain/json format
/// selection.
pub trait Formatter {
    fn format_message(&self, message: &str) -> String;
    fn format_start_result(&self, result: &StartResultInfo) -> String;
    fn format_index_summary(&self, message: &str, progress: &IndexProgress) -> String;
    fn format_index_watch_update(&self, progress: &IndexProgress) -> String;
    fn format_status(&self, status: &StatusInfo) -> String;
    fn format_stop_result(&self, result: &StopResultInfo) -> String;
    fn format_project_list(&self, projects: &ProjectListInfo) -> String;
    fn format_update_status(&self, _status: &UpdateStatusInfo) -> String {
        String::new()
    }
    fn format_update_result(&self, _result: &UpdateResult) -> String {
        String::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartStatus {
    Started,
    AlreadyRunning,
    StartupInProgress,
    IndexedAndStarted,
}

impl StartStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::AlreadyRunning => "already_running",
            Self::StartupInProgress => "startup_in_progress",
            Self::IndexedAndStarted => "indexed_and_started",
        }
    }
}

#[derive(Debug, Clone)]
pub struct StartResultInfo {
    pub status: StartStatus,
    pub project_id: Option<String>,
    pub project_root: Option<PathBuf>,
    pub source_root: Option<PathBuf>,
    pub registered: Option<bool>,
    pub index_status: Option<ProjectListIndexStatus>,
    pub pid: Option<u32>,
    pub message: String,
    pub progress: Option<IndexProgress>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleState {
    NotStarted,
    Indexing,
    Active,
    Registered,
    Stopped,
}

impl LifecycleState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotStarted => "not_started",
            Self::Indexing => "indexing",
            Self::Active => "active",
            Self::Registered => "registered",
            Self::Stopped => "stopped",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusInfo {
    pub lifecycle_state: LifecycleState,
    pub registered: bool,
    pub daemon_running: bool,
    pub pid: Option<u32>,
    pub project_initialized: bool,
    pub indexed_files: Option<u64>,
    pub total_segments: Option<u64>,
    pub project_id: Option<String>,
    pub project_root: PathBuf,
    pub source_root: PathBuf,
    pub index_present: bool,
    pub index_readable: bool,
    pub last_file_check_at: Option<DateTime<Utc>>,
    pub index_progress: Option<IndexProgress>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StopStatus {
    Stopped,
    NotRegistered,
    DaemonNotRunning,
    Unsupported,
}

impl StopStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::NotRegistered => "not_registered",
            Self::DaemonNotRunning => "daemon_not_running",
            Self::Unsupported => "unsupported",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct StopResultInfo {
    pub status: StopStatus,
    pub project_root: PathBuf,
    pub registered: bool,
    pub daemon_running: bool,
    pub pid: Option<u32>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectListInfo {
    pub projects: Vec<ProjectListItem>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectListItem {
    pub project_id: String,
    pub state: LifecycleState,
    pub project_root: PathBuf,
    pub source_root: PathBuf,
    pub registered_at: String,
    pub daemon_running: bool,
    pub index_status: ProjectListIndexStatus,
    pub files: Option<u64>,
    pub segments: Option<u64>,
    pub last_file_check_at: Option<DateTime<Utc>>,
    pub index_progress: Option<IndexProgress>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectListIndexStatus {
    Ready,
    NotBuilt,
    Unavailable,
}

impl ProjectListIndexStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::NotBuilt => "not_built",
            Self::Unavailable => "unavailable",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct WorkSummary {
    files_completed: usize,
    files_indexed: usize,
    files_skipped: usize,
    files_deleted: usize,
}

/// Data needed to render the output of `1up update --check` and `1up update --status`.
#[derive(Debug, Clone, Serialize)]
pub struct UpdateStatusInfo {
    pub current_version: String,
    pub cached: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_version: Option<String>,
    pub update_available: bool,
    #[serde(skip)]
    pub status: UpdateStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub install_channel: Option<InstallChannel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_age_secs: Option<i64>,
    pub yanked: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minimum_safe_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upgrade_instruction: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_message: Option<String>,
}

/// Outcome of running `1up update` (no flags).
#[derive(Debug, Clone)]
pub enum UpdateResult {
    UpToDate {
        current_version: String,
        latest_version: String,
    },
    ChannelManaged {
        current_version: String,
        latest_version: String,
        install_channel: InstallChannel,
        upgrade_instruction: String,
        status: UpdateStatus,
        message: Option<String>,
    },
    Updated {
        old_version: String,
        new_version: String,
    },
}

impl From<&IndexProgress> for WorkSummary {
    fn from(progress: &IndexProgress) -> Self {
        Self {
            files_completed: progress.files_indexed + progress.files_deleted,
            files_indexed: progress.files_indexed,
            files_skipped: progress.files_skipped,
            files_deleted: progress.files_deleted,
        }
    }
}

pub fn formatter_for(format: OutputFormat) -> Box<dyn Formatter> {
    match format {
        OutputFormat::Json => Box::new(JsonFormatter),
        OutputFormat::Human => Box::new(HumanFormatter),
        OutputFormat::Plain => Box::new(PlainFormatter),
    }
}

pub fn spawn_index_watch_renderer(format: OutputFormat) -> (Sender<IndexProgress>, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel();
    let handle = thread::spawn(move || render_watch_updates(format, rx));
    (tx, handle)
}

struct JsonFormatter;
struct HumanFormatter;
struct PlainFormatter;

const WATCH_RENDER_INTERVAL: Duration = Duration::from_millis(125);

#[derive(Debug, Clone, PartialEq, Eq)]
struct WatchRenderState {
    state: IndexState,
    phase: IndexPhase,
    message: Option<String>,
    files_processed: usize,
    files_total: usize,
    files_indexed: usize,
    files_skipped: usize,
    files_deleted: usize,
    segments_stored: usize,
    embeddings_enabled: bool,
    parallelism: Option<crate::shared::types::IndexParallelism>,
}

impl From<&IndexProgress> for WatchRenderState {
    fn from(progress: &IndexProgress) -> Self {
        Self {
            state: progress.state,
            phase: progress.phase,
            message: progress.message.clone(),
            files_processed: progress.files_processed,
            files_total: progress.files_total,
            files_indexed: progress.files_indexed,
            files_skipped: progress.files_skipped,
            files_deleted: progress.files_deleted,
            segments_stored: progress.segments_stored,
            embeddings_enabled: progress.embeddings_enabled,
            parallelism: progress.parallelism.clone(),
        }
    }
}

impl Formatter for JsonFormatter {
    fn format_message(&self, message: &str) -> String {
        to_json(&serde_json::json!({ "message": message }))
    }

    fn format_start_result(&self, result: &StartResultInfo) -> String {
        let mut payload = serde_json::json!({
            "status": result.status.as_str(),
            "project_id": &result.project_id,
            "project_root": &result.project_root,
            "source_root": &result.source_root,
            "registered": result.registered,
            "index_status": result.index_status.map(ProjectListIndexStatus::as_str),
            "pid": result.pid,
            "message": &result.message,
        });
        if let Some(progress) = &result.progress {
            payload["progress"] = serde_json::json!(progress);
            payload["work"] = serde_json::json!(WorkSummary::from(progress));
        }
        to_json(&payload)
    }

    fn format_index_summary(&self, message: &str, progress: &IndexProgress) -> String {
        let work = WorkSummary::from(progress);
        to_json(&serde_json::json!({
            "message": message,
            "progress": progress,
            "work": work,
        }))
    }

    fn format_index_watch_update(&self, progress: &IndexProgress) -> String {
        serde_json::to_string(&serde_json::json!({
            "event": "index_progress",
            "progress": progress,
            "work": WorkSummary::from(progress),
        }))
        .unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
    }

    fn format_status(&self, status: &StatusInfo) -> String {
        let index_work = status.index_progress.as_ref().map(WorkSummary::from);
        to_json(&serde_json::json!({
            "lifecycle_state": status.lifecycle_state.as_str(),
            "registered": status.registered,
            "daemon_running": status.daemon_running,
            "pid": status.pid,
            "project_initialized": status.project_initialized,
            "indexed_files": status.indexed_files,
            "total_segments": status.total_segments,
            "project_id": &status.project_id,
            "project_root": &status.project_root,
            "source_root": &status.source_root,
            "index_status": render_index_health_plain(status),
            "last_file_check_at": status.last_file_check_at,
            "index_progress": &status.index_progress,
            "index_work": index_work,
        }))
    }

    fn format_stop_result(&self, result: &StopResultInfo) -> String {
        to_json(result)
    }

    fn format_project_list(&self, projects: &ProjectListInfo) -> String {
        to_json(projects)
    }

    fn format_update_status(&self, info: &UpdateStatusInfo) -> String {
        if !info.cached {
            let message = info.status_message.as_deref().unwrap_or(
                "No cached update information. Run `1up update --check` to check for updates.",
            );
            return to_json(&serde_json::json!({
                "current_version": info.current_version,
                "cached": false,
                "message": message,
            }));
        }
        let mut obj = serde_json::json!({
            "current_version": info.current_version,
            "latest_version": info.latest_version,
            "update_available": info.update_available,
            "install_channel": info.install_channel,
            "yanked": info.yanked,
            "minimum_safe_version": info.minimum_safe_version,
            "message": info.message,
            "notes_url": info.notes_url,
            "upgrade_instruction": info.upgrade_instruction,
        });
        if let Some(checked_at) = &info.checked_at {
            obj["checked_at"] = serde_json::json!(checked_at.to_rfc3339());
        }
        if let Some(cache_age_secs) = info.cache_age_secs {
            obj["cache_age_secs"] = serde_json::json!(cache_age_secs);
            obj["cached"] = serde_json::json!(true);
        }
        to_json(&obj)
    }

    fn format_update_result(&self, result: &UpdateResult) -> String {
        match result {
            UpdateResult::UpToDate {
                current_version,
                latest_version,
            } => to_json(&serde_json::json!({
                "current_version": current_version,
                "latest_version": latest_version,
                "update_available": false,
                "message": "Already up to date.",
            })),
            UpdateResult::ChannelManaged {
                current_version,
                latest_version,
                install_channel,
                upgrade_instruction,
                status,
                message,
            } => to_json(&serde_json::json!({
                "current_version": current_version,
                "latest_version": latest_version,
                "update_available": true,
                "install_channel": install_channel,
                "managed": true,
                "status": render_update_status_label(status),
                "upgrade_instruction": upgrade_instruction,
                "message": message,
            })),
            UpdateResult::Updated {
                old_version,
                new_version,
            } => to_json(&serde_json::json!({
                "updated": true,
                "old_version": old_version,
                "new_version": new_version,
                "message": format!("Updated 1up from {old_version} to {new_version}."),
            })),
        }
    }
}

impl Formatter for HumanFormatter {
    fn format_message(&self, message: &str) -> String {
        message.to_string()
    }

    fn format_start_result(&self, result: &StartResultInfo) -> String {
        let mut out = match &result.progress {
            Some(progress) => self.format_index_summary(&result.message, progress),
            None => {
                let mut out = result.message.clone();
                out.push('\n');
                out
            }
        };
        append_start_fields_human(&mut out, result);
        out
    }

    fn format_index_summary(&self, message: &str, progress: &IndexProgress) -> String {
        let mut out = String::new();
        out.push_str(message);
        out.push('\n');
        let work = WorkSummary::from(progress);
        out.push_str(&format!(
            "Progress: {} ({}) | scanned {} of {} | indexed {} | skipped {} | deleted {} | segments {}\n",
            render_index_state_human(progress.state),
            render_index_phase_human(progress),
            progress.files_scanned,
            progress.files_total,
            progress.files_indexed,
            progress.files_skipped,
            progress.files_deleted,
            progress.segments_stored,
        ));
        out.push_str(&format!(
            "Work: completed {} ({} indexed, {} deleted) | skipped {}\n",
            work.files_completed, work.files_indexed, work.files_deleted, work.files_skipped,
        ));
        if let Some(parallelism) = &progress.parallelism {
            out.push_str(&format!(
                "Parallelism: workers {} effective / {} configured | embed threads {}\n",
                parallelism.jobs_effective, parallelism.jobs_configured, parallelism.embed_threads,
            ));
        }
        if let Some(timings) = &progress.timings {
            let mut timing_parts = Vec::new();
            if let Some(db_ms) = timings.db_prepare_ms {
                timing_parts.push(format!("db_prepare {}", render_duration_ms(db_ms)));
            }
            if let Some(model_ms) = timings.model_prepare_ms {
                timing_parts.push(format!("model_prepare {}", render_duration_ms(model_ms)));
            }
            if let Some(input_ms) = timings.input_prep_ms {
                timing_parts.push(format!("input_prep {}", render_duration_ms(input_ms)));
            }
            timing_parts.push(format!("scan {}", render_duration_ms(timings.scan_ms)));
            timing_parts.push(format!("parse {}", render_duration_ms(timings.parse_ms)));
            timing_parts.push(format!("embed {}", render_duration_ms(timings.embed_ms)));
            timing_parts.push(format!("store {}", render_duration_ms(timings.store_ms)));
            timing_parts.push(format!("total {}", render_duration_ms(timings.total_ms)));
            out.push_str(&format!("Timings: {}\n", timing_parts.join(" | ")));
        }
        if let Some(scope) = &progress.scope {
            let mut scope_str = format!(
                "Scope: requested {} | executed {}",
                scope.requested, scope.executed
            );
            if scope.changed_paths > 0 {
                scope_str.push_str(&format!(" | changed_paths {}", scope.changed_paths));
            }
            if let Some(reason) = &scope.fallback_reason {
                scope_str.push_str(&format!(" | fallback: {reason}"));
            }
            out.push_str(&scope_str);
            out.push('\n');
        }
        if let Some(prefilter) = &progress.prefilter {
            out.push_str(&format!(
                "Prefilter: discovered {} | metadata_skipped {} | content_read {} | deleted {}\n",
                prefilter.discovered,
                prefilter.metadata_skipped,
                prefilter.content_read,
                prefilter.deleted,
            ));
        }
        out.push_str(&format!(
            "Embeddings: {}\n",
            render_embeddings_human(progress.embeddings_enabled)
        ));
        out.push_str(&format!("Updated: {}\n", progress.updated_at.to_rfc3339()));
        out
    }

    fn format_index_watch_update(&self, progress: &IndexProgress) -> String {
        format!(
            "{} | {} ({}) | processed {} of {} | indexed {} | skipped {} | deleted {} | segments {}",
            render_index_watch_message(progress),
            render_index_state_human(progress.state),
            render_index_phase_human(progress),
            progress.files_processed,
            progress.files_total,
            progress.files_indexed,
            progress.files_skipped,
            progress.files_deleted,
            progress.segments_stored,
        )
    }

    fn format_status(&self, status: &StatusInfo) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "Lifecycle: {}\n",
            render_lifecycle_state_human(status.lifecycle_state)
        ));
        out.push_str(&format!(
            "Registered: {}\n",
            render_bool_human(status.registered)
        ));
        let daemon_state = if status.daemon_running {
            "running".green().to_string()
        } else {
            "stopped".red().to_string()
        };
        out.push_str(&format!("Daemon: {daemon_state}\n"));
        if let Some(pid) = status.pid {
            out.push_str(&format!("PID: {pid}\n"));
        }
        if let Some(id) = &status.project_id {
            out.push_str(&format!("Project ID: {id}\n"));
        } else if status.project_initialized {
            out.push_str("Project ID: initialized\n");
        } else {
            out.push_str(&format!("Project ID: {}\n", "not initialized".yellow()));
        }
        out.push_str(&format!(
            "Project root: {}\n",
            status.project_root.display()
        ));
        out.push_str(&format!("Source root: {}\n", status.source_root.display()));
        out.push_str(&format!("Index: {}\n", render_index_health_human(status)));
        if let Some(last_file_check_at) = &status.last_file_check_at {
            out.push_str(&format!(
                "Last file check: {} ({})\n",
                render_time_ago(last_file_check_at),
                last_file_check_at.to_rfc3339()
            ));
        } else {
            out.push_str(&format!("Last file check: {}\n", "never recorded".yellow()));
        }
        if let Some(files) = status.indexed_files {
            out.push_str(&format!("Indexed files: {files}\n"));
        }
        if let Some(segs) = status.total_segments {
            out.push_str(&format!("Total segments: {segs}\n"));
        }
        if let Some(progress) = &status.index_progress {
            let work = WorkSummary::from(progress);
            out.push_str(&format!(
                "Index status: {}\n",
                render_index_state_human(progress.state)
            ));
            out.push_str(&format!(
                "Index phase: {}\n",
                render_index_phase_human(progress)
            ));
            out.push_str(&format!(
                "Last index: scanned {} of {} files, indexed {}, skipped {}, deleted {}, segments {}\n",
                progress.files_scanned,
                progress.files_total,
                progress.files_indexed,
                progress.files_skipped,
                progress.files_deleted,
                progress.segments_stored,
            ));
            out.push_str(&format!(
                "Processed: {} of {}\n",
                progress.files_processed, progress.files_total
            ));
            if let Some(message) = progress.message.as_deref() {
                out.push_str(&format!("Index message: {message}\n"));
            }
            out.push_str(&format!(
                "Work: completed {} ({} indexed, {} deleted) | skipped {}\n",
                work.files_completed, work.files_indexed, work.files_deleted, work.files_skipped,
            ));
            if let Some(parallelism) = &progress.parallelism {
                out.push_str(&format!(
                    "Parallelism: workers {} effective / {} configured | embed threads {}\n",
                    parallelism.jobs_effective,
                    parallelism.jobs_configured,
                    parallelism.embed_threads,
                ));
            }
            if let Some(timings) = &progress.timings {
                let mut timing_parts = Vec::new();
                if let Some(db_ms) = timings.db_prepare_ms {
                    timing_parts.push(format!("db_prepare {}", render_duration_ms(db_ms)));
                }
                if let Some(model_ms) = timings.model_prepare_ms {
                    timing_parts.push(format!("model_prepare {}", render_duration_ms(model_ms)));
                }
                if let Some(input_ms) = timings.input_prep_ms {
                    timing_parts.push(format!("input_prep {}", render_duration_ms(input_ms)));
                }
                timing_parts.push(format!("scan {}", render_duration_ms(timings.scan_ms)));
                timing_parts.push(format!("parse {}", render_duration_ms(timings.parse_ms)));
                timing_parts.push(format!("embed {}", render_duration_ms(timings.embed_ms)));
                timing_parts.push(format!("store {}", render_duration_ms(timings.store_ms)));
                timing_parts.push(format!("total {}", render_duration_ms(timings.total_ms)));
                out.push_str(&format!("Timings: {}\n", timing_parts.join(" | ")));
            }
            if let Some(scope) = &progress.scope {
                let mut scope_str = format!(
                    "Scope: requested {} | executed {}",
                    scope.requested, scope.executed
                );
                if scope.changed_paths > 0 {
                    scope_str.push_str(&format!(" | changed_paths {}", scope.changed_paths));
                }
                if let Some(reason) = &scope.fallback_reason {
                    scope_str.push_str(&format!(" | fallback: {reason}"));
                }
                out.push_str(&scope_str);
                out.push('\n');
            }
            if let Some(prefilter) = &progress.prefilter {
                out.push_str(&format!(
                    "Prefilter: discovered {} | metadata_skipped {} | content_read {} | deleted {}\n",
                    prefilter.discovered, prefilter.metadata_skipped, prefilter.content_read, prefilter.deleted,
                ));
            }
            out.push_str(&format!(
                "Embeddings: {}\n",
                render_embeddings_human(progress.embeddings_enabled)
            ));
            out.push_str(&format!(
                "Updated: {} ({})\n",
                render_time_ago(&progress.updated_at),
                progress.updated_at.to_rfc3339()
            ));
        }
        out
    }

    fn format_stop_result(&self, result: &StopResultInfo) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "Status: {}\n",
            render_stop_status_human(result.status)
        ));
        out.push_str(&format!(
            "Project root: {}\n",
            result.project_root.display()
        ));
        out.push_str(&format!(
            "Registered: {}\n",
            render_bool_human(result.registered)
        ));
        out.push_str(&format!(
            "Daemon: {}\n",
            render_daemon_state_human(result.daemon_running)
        ));
        if let Some(pid) = result.pid {
            out.push_str(&format!("PID: {pid}\n"));
        }
        out.push_str(&format!("Message: {}\n", result.message));
        out
    }

    fn format_project_list(&self, projects: &ProjectListInfo) -> String {
        if projects.projects.is_empty() {
            return "No registered projects.\nRun `1up start` in a repository to register one.\n"
                .to_string();
        }

        let rows: Vec<ProjectListRow> = projects
            .projects
            .iter()
            .map(ProjectListRow::from_item)
            .collect();
        let widths = ProjectListWidths::from_rows(&rows);

        let mut out = String::from("Registered projects\n");
        out.push_str(&format_project_list_header(&widths));
        out.push_str(&format_project_list_separator(&widths));
        for row in rows {
            out.push_str(&format_project_list_row(&row, &widths));
        }
        out
    }

    fn format_update_status(&self, info: &UpdateStatusInfo) -> String {
        if !info.cached {
            let message = info.status_message.as_deref().unwrap_or(
                "No cached update information.\nRun `1up update --check` to check for updates.",
            );
            return format!(
                "Current version: {}\n{}\n",
                info.current_version.bold(),
                message
            );
        }
        let mut out = String::new();
        out.push_str(&format!(
            "Current version: {}\n",
            info.current_version.bold()
        ));
        if let Some(ref latest) = info.latest_version {
            out.push_str(&format!("Latest version:  {}\n", latest.bold()));
        }
        if let Some(ref channel) = info.install_channel {
            out.push_str(&format!("Install source:  {channel}\n"));
        }
        if let (Some(ref checked_at), Some(cache_age_secs)) =
            (&info.checked_at, info.cache_age_secs)
        {
            out.push_str(&format!(
                "Last checked:    {} ({})\n",
                checked_at.to_rfc3339(),
                render_cache_age(cache_age_secs),
            ));
        }
        match &info.status {
            UpdateStatus::UpToDate => {
                out.push_str(&format!("Status: {}\n", "up to date".green()));
            }
            UpdateStatus::UpdateAvailable { latest } => {
                out.push_str(&format!(
                    "Status: {}\n",
                    format!("update available ({latest})").yellow()
                ));
                if let Some(ref instruction) = info.upgrade_instruction {
                    out.push_str(&format!("Run: {instruction}\n"));
                }
            }
            UpdateStatus::Yanked { message, .. } => {
                out.push_str(&format!(
                    "Status: {}\n",
                    "YANKED -- upgrade immediately".red()
                ));
                if let Some(msg) = message {
                    out.push_str(&format!("Message: {msg}\n"));
                }
                if let Some(ref instruction) = info.upgrade_instruction {
                    out.push_str(&format!("Run: {instruction}\n"));
                }
            }
            UpdateStatus::BelowMinimumSafe {
                minimum_safe,
                message,
                ..
            } => {
                out.push_str(&format!(
                    "Status: {}\n",
                    format!("below minimum safe version ({minimum_safe})").red()
                ));
                if let Some(msg) = message {
                    out.push_str(&format!("Message: {msg}\n"));
                }
                if let Some(ref instruction) = info.upgrade_instruction {
                    out.push_str(&format!("Run: {instruction}\n"));
                }
            }
        }
        out
    }

    fn format_update_result(&self, result: &UpdateResult) -> String {
        match result {
            UpdateResult::UpToDate {
                current_version, ..
            } => {
                format!("Already up to date (version {current_version}).")
            }
            UpdateResult::ChannelManaged {
                latest_version,
                install_channel,
                upgrade_instruction,
                status,
                message,
                current_version,
            } => {
                let mut out = String::new();
                out.push_str(&format!(
                    "Update available: 1up {} (current: {})\n",
                    latest_version.bold(),
                    current_version
                ));
                match status {
                    UpdateStatus::Yanked { message, .. } => {
                        out.push_str(&format!(
                            "{}\n",
                            "WARNING: this version has been recalled. Upgrade immediately.".red()
                        ));
                        if let Some(msg) = message {
                            out.push_str(&format!("Message: {msg}\n"));
                        }
                    }
                    UpdateStatus::BelowMinimumSafe {
                        minimum_safe,
                        message,
                        ..
                    } => {
                        out.push_str(&format!(
                            "{}\n",
                            format!(
                                "WARNING: current version is below minimum safe version ({minimum_safe}). Upgrade immediately."
                            )
                            .red()
                        ));
                        if let Some(msg) = message {
                            out.push_str(&format!("Message: {msg}\n"));
                        }
                    }
                    _ => {}
                }
                if let Some(msg) = message {
                    if !matches!(
                        status,
                        UpdateStatus::Yanked { .. } | UpdateStatus::BelowMinimumSafe { .. }
                    ) {
                        out.push_str(&format!("Message: {msg}\n"));
                    }
                }
                out.push_str(&format!(
                    "1up is managed by {install_channel}. Run: {upgrade_instruction}\n"
                ));
                out
            }
            UpdateResult::Updated {
                old_version,
                new_version,
            } => {
                format!(
                    "Updated 1up from {} to {}.",
                    old_version,
                    new_version.green().bold()
                )
            }
        }
    }
}

impl Formatter for PlainFormatter {
    fn format_message(&self, message: &str) -> String {
        message.to_string()
    }

    fn format_start_result(&self, result: &StartResultInfo) -> String {
        let out = format!(
            "status:{}\tproject_id:{}\tproject_root:{}\tsource_root:{}\tregistered:{}\tindex:{}\tpid:{}\tmessage:{}\n",
            result.status.as_str(),
            render_optional_plain(result.project_id.as_deref()),
            render_optional_path_plain(result.project_root.as_ref()),
            render_optional_path_plain(result.source_root.as_ref()),
            render_optional_bool_plain(result.registered),
            render_start_index_plain(result.index_status),
            render_optional_pid_plain(result.pid),
            plain_field(&result.message),
        );
        out
    }

    fn format_index_summary(&self, message: &str, progress: &IndexProgress) -> String {
        let mut out = String::new();
        let work = WorkSummary::from(progress);
        out.push_str(message);
        out.push('\n');
        out.push_str(&format!(
            "index_state:{}\tindex_phase:{}\tfiles_scanned:{}\tfiles_total:{}\tfiles_completed:{}\tfiles_indexed:{}\tfiles_skipped:{}\tfiles_deleted:{}\tsegments_stored:{}\tembeddings:{}",
            render_index_state_plain(progress.state),
            render_index_phase_plain(progress),
            progress.files_scanned,
            progress.files_total,
            work.files_completed,
            progress.files_indexed,
            progress.files_skipped,
            progress.files_deleted,
            progress.segments_stored,
            render_embeddings_plain(progress.embeddings_enabled),
        ));
        if let Some(parallelism) = &progress.parallelism {
            out.push_str(&format!(
                "\tjobs_configured:{}\tjobs_effective:{}\tembed_threads:{}",
                parallelism.jobs_configured, parallelism.jobs_effective, parallelism.embed_threads,
            ));
        }
        if let Some(timings) = &progress.timings {
            out.push_str(&format!(
                "\tscan_ms:{}\tparse_ms:{}\tembed_ms:{}\tstore_ms:{}\ttotal_ms:{}",
                timings.scan_ms,
                timings.parse_ms,
                timings.embed_ms,
                timings.store_ms,
                timings.total_ms,
            ));
            if let Some(db_ms) = timings.db_prepare_ms {
                out.push_str(&format!("\tdb_prepare_ms:{db_ms}"));
            }
            if let Some(model_ms) = timings.model_prepare_ms {
                out.push_str(&format!("\tmodel_prepare_ms:{model_ms}"));
            }
            if let Some(input_ms) = timings.input_prep_ms {
                out.push_str(&format!("\tinput_prep_ms:{input_ms}"));
            }
        }
        if let Some(scope) = &progress.scope {
            out.push_str(&format!(
                "\tscope_requested:{}\tscope_executed:{}\tscope_changed_paths:{}",
                scope.requested, scope.executed, scope.changed_paths,
            ));
            if let Some(reason) = &scope.fallback_reason {
                out.push_str(&format!("\tscope_fallback_reason:{reason}"));
            }
        }
        if let Some(prefilter) = &progress.prefilter {
            out.push_str(&format!(
                "\tprefilter_discovered:{}\tprefilter_metadata_skipped:{}\tprefilter_content_read:{}\tprefilter_deleted:{}",
                prefilter.discovered, prefilter.metadata_skipped, prefilter.content_read, prefilter.deleted,
            ));
        }
        out.push_str(&format!("\tupdated:{}\n", progress.updated_at.to_rfc3339()));
        out
    }

    fn format_index_watch_update(&self, progress: &IndexProgress) -> String {
        let mut out = format!(
            "event:index_progress\tindex_state:{}\tindex_phase:{}\tmessage:{}\tfiles_processed:{}\tfiles_total:{}\tfiles_indexed:{}\tfiles_skipped:{}\tfiles_deleted:{}\tsegments_stored:{}\tembeddings:{}",
            render_index_state_plain(progress.state),
            render_index_phase_plain(progress),
            render_index_watch_message(progress),
            progress.files_processed,
            progress.files_total,
            progress.files_indexed,
            progress.files_skipped,
            progress.files_deleted,
            progress.segments_stored,
            render_embeddings_plain(progress.embeddings_enabled),
        );
        if let Some(parallelism) = &progress.parallelism {
            out.push_str(&format!(
                "\tjobs_configured:{}\tjobs_effective:{}\tembed_threads:{}",
                parallelism.jobs_configured, parallelism.jobs_effective, parallelism.embed_threads,
            ));
        }
        if let Some(timings) = &progress.timings {
            out.push_str(&format!(
                "\tscan_ms:{}\tparse_ms:{}\tembed_ms:{}\tstore_ms:{}\ttotal_ms:{}",
                timings.scan_ms,
                timings.parse_ms,
                timings.embed_ms,
                timings.store_ms,
                timings.total_ms,
            ));
            if let Some(db_ms) = timings.db_prepare_ms {
                out.push_str(&format!("\tdb_prepare_ms:{db_ms}"));
            }
            if let Some(model_ms) = timings.model_prepare_ms {
                out.push_str(&format!("\tmodel_prepare_ms:{model_ms}"));
            }
            if let Some(input_ms) = timings.input_prep_ms {
                out.push_str(&format!("\tinput_prep_ms:{input_ms}"));
            }
        }
        if let Some(scope) = &progress.scope {
            out.push_str(&format!(
                "\tscope_requested:{}\tscope_executed:{}",
                scope.requested, scope.executed,
            ));
            if let Some(reason) = &scope.fallback_reason {
                out.push_str(&format!("\tscope_fallback_reason:{reason}"));
            }
        }
        out.push_str(&format!("\tupdated:{}\n", progress.updated_at.to_rfc3339()));
        out
    }

    fn format_status(&self, status: &StatusInfo) -> String {
        let daemon_state = if status.daemon_running {
            "running"
        } else {
            "stopped"
        };
        let mut out = format!(
            "lifecycle:{}\tregistered:{}\tdaemon:{}\tpid:{}\tproject_initialized:{}\tproject_id:{}\tproject_root:{}\tsource_root:{}\tindex:{}",
            status.lifecycle_state.as_str(),
            status.registered,
            daemon_state,
            render_optional_pid_plain(status.pid),
            status.project_initialized,
            render_optional_plain(status.project_id.as_deref()),
            plain_field(&status.project_root.display().to_string()),
            plain_field(&status.source_root.display().to_string()),
            render_index_health_plain(status),
        );
        match &status.last_file_check_at {
            Some(last_file_check_at) => out.push_str(&format!(
                "\tlast_file_check:{}",
                last_file_check_at.to_rfc3339()
            )),
            None => out.push_str("\tlast_file_check:none"),
        }
        if let Some(files) = status.indexed_files {
            out.push_str(&format!("\tfiles:{files}"));
        }
        if let Some(segs) = status.total_segments {
            out.push_str(&format!("\tsegments:{segs}"));
        }
        if let Some(progress) = &status.index_progress {
            let work = WorkSummary::from(progress);
            out.push_str(&format!(
                "\tindex_state:{}\tindex_phase:{}\tlast_scanned:{}\tlast_total:{}\tlast_completed:{}\tlast_indexed:{}\tlast_skipped:{}\tlast_deleted:{}\tlast_segments:{}\tembeddings:{}",
                render_index_state_plain(progress.state),
                render_index_phase_plain(progress),
                progress.files_scanned,
                progress.files_total,
                work.files_completed,
                progress.files_indexed,
                progress.files_skipped,
                progress.files_deleted,
                progress.segments_stored,
                render_embeddings_plain(progress.embeddings_enabled),
            ));
            out.push_str(&format!("\tlast_processed:{}", progress.files_processed));
            if let Some(message) = progress.message.as_deref() {
                out.push_str(&format!("\tindex_message:{}", plain_field(message)));
            }
            if let Some(parallelism) = &progress.parallelism {
                out.push_str(&format!(
                    "\tjobs_configured:{}\tjobs_effective:{}\tembed_threads:{}",
                    parallelism.jobs_configured,
                    parallelism.jobs_effective,
                    parallelism.embed_threads,
                ));
            }
            if let Some(timings) = &progress.timings {
                out.push_str(&format!(
                    "\tscan_ms:{}\tparse_ms:{}\tembed_ms:{}\tstore_ms:{}\ttotal_ms:{}",
                    timings.scan_ms,
                    timings.parse_ms,
                    timings.embed_ms,
                    timings.store_ms,
                    timings.total_ms,
                ));
                if let Some(db_ms) = timings.db_prepare_ms {
                    out.push_str(&format!("\tdb_prepare_ms:{db_ms}"));
                }
                if let Some(model_ms) = timings.model_prepare_ms {
                    out.push_str(&format!("\tmodel_prepare_ms:{model_ms}"));
                }
                if let Some(input_ms) = timings.input_prep_ms {
                    out.push_str(&format!("\tinput_prep_ms:{input_ms}"));
                }
            }
            if let Some(scope) = &progress.scope {
                out.push_str(&format!(
                    "\tscope_requested:{}\tscope_executed:{}\tscope_changed_paths:{}",
                    scope.requested, scope.executed, scope.changed_paths,
                ));
                if let Some(reason) = &scope.fallback_reason {
                    out.push_str(&format!("\tscope_fallback_reason:{}", plain_field(reason)));
                }
            }
            if let Some(prefilter) = &progress.prefilter {
                out.push_str(&format!(
                    "\tprefilter_discovered:{}\tprefilter_metadata_skipped:{}\tprefilter_content_read:{}\tprefilter_deleted:{}",
                    prefilter.discovered, prefilter.metadata_skipped, prefilter.content_read, prefilter.deleted,
                ));
            }
            out.push_str(&format!("\tupdated:{}", progress.updated_at.to_rfc3339()));
        }
        out.push('\n');
        out
    }

    fn format_stop_result(&self, result: &StopResultInfo) -> String {
        let daemon_state = if result.daemon_running {
            "running"
        } else {
            "stopped"
        };
        format!(
            "status:{}\tproject_root:{}\tregistered:{}\tdaemon:{}\tpid:{}\tmessage:{}\n",
            result.status.as_str(),
            plain_field(&result.project_root.display().to_string()),
            result.registered,
            daemon_state,
            render_optional_pid_plain(result.pid),
            plain_field(&result.message),
        )
    }

    fn format_project_list(&self, projects: &ProjectListInfo) -> String {
        if projects.projects.is_empty() {
            return "projects:0\n".to_string();
        }

        let mut out = String::new();
        for project in &projects.projects {
            out.push_str(&format!(
                "project:{}\tstate:{}\tproject_root:{}\tsource_root:{}\tindex:{}\tfiles:{}\tsegments:{}\tlast_file_check:{}\tregistered_at:{}\n",
                plain_field(&project.project_id),
                project.state.as_str(),
                plain_field(&project.project_root.display().to_string()),
                plain_field(&project.source_root.display().to_string()),
                project.index_status.as_str(),
                render_optional_count(project.files),
                render_optional_count(project.segments),
                render_optional_time(project.last_file_check_at.as_ref()),
                project.registered_at,
            ));
        }
        out
    }

    fn format_update_status(&self, info: &UpdateStatusInfo) -> String {
        if !info.cached {
            let mut out = format!("current:{}\tcached:false", info.current_version);
            if let Some(message) = info.status_message.as_deref() {
                out.push_str(&format!("\tmessage:{message}"));
            }
            out.push('\n');
            return out;
        }
        let status_label = render_update_status_label(&info.status);
        let mut out = format!(
            "current:{}\tlatest:{}\tstatus:{}\tupdate_available:{}\tchannel:{}\tinstruction:{}",
            info.current_version,
            info.latest_version.as_deref().unwrap_or("unknown"),
            status_label,
            info.update_available,
            info.install_channel
                .map(|c| c.to_string())
                .unwrap_or_else(|| "unknown".to_string()),
            info.upgrade_instruction.as_deref().unwrap_or(""),
        );
        if let Some(ref checked_at) = info.checked_at {
            out.push_str(&format!("\tchecked_at:{}", checked_at.to_rfc3339()));
        }
        if let Some(cache_age_secs) = info.cache_age_secs {
            out.push_str(&format!("\tcache_age_secs:{cache_age_secs}"));
        }
        if info.yanked {
            out.push_str("\tyanked:true");
        }
        if let Some(ref min_safe) = info.minimum_safe_version {
            out.push_str(&format!("\tminimum_safe_version:{min_safe}"));
        }
        if let Some(ref msg) = info.message {
            out.push_str(&format!("\tmessage:{msg}"));
        }
        out.push('\n');
        out
    }

    fn format_update_result(&self, result: &UpdateResult) -> String {
        match result {
            UpdateResult::UpToDate {
                current_version,
                latest_version,
            } => {
                format!(
                    "current:{current_version}\tlatest:{latest_version}\tupdate_available:false\n"
                )
            }
            UpdateResult::ChannelManaged {
                current_version,
                latest_version,
                install_channel,
                upgrade_instruction,
                message,
                ..
            } => {
                let mut out = format!(
                    "current:{current_version}\tlatest:{latest_version}\tupdate_available:true\tchannel:{install_channel}\tmanaged:true\tinstruction:{upgrade_instruction}"
                );
                if let Some(ref msg) = message {
                    out.push_str(&format!("\tmessage:{msg}"));
                }
                out.push('\n');
                out
            }
            UpdateResult::Updated {
                old_version,
                new_version,
            } => {
                format!("updated:true\told_version:{old_version}\tnew_version:{new_version}\n")
            }
        }
    }
}

fn render_watch_updates(format: OutputFormat, rx: Receiver<IndexProgress>) {
    use std::io::IsTerminal;

    let formatter = formatter_for(format);
    let mut progress_ui = start_watch_progress_ui(format);
    let stream_updates = should_stream_watch_updates(format, std::io::stderr().is_terminal());
    let mut last_rendered_state: Option<WatchRenderState> = None;
    let mut last_rendered_at: Option<Instant> = None;
    let mut disconnected = false;

    while !disconnected {
        let mut progress = match rx.recv() {
            Ok(progress) => progress,
            Err(_) => break,
        };

        loop {
            let current_state = WatchRenderState::from(&progress);
            if should_render_watch_update(
                last_rendered_state.as_ref(),
                &current_state,
                last_rendered_at,
            ) {
                break;
            }

            let Some(last_rendered_at) = last_rendered_at else {
                break;
            };

            let wait_for = WATCH_RENDER_INTERVAL.saturating_sub(last_rendered_at.elapsed());
            match rx.recv_timeout(wait_for) {
                Ok(next_progress) => progress = next_progress,
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }

        let current_state = WatchRenderState::from(&progress);
        if last_rendered_state.as_ref() == Some(&current_state) {
            if progress.state == IndexState::Complete {
                let spinner_label = render_index_watch_message(&progress);
                if let Some(mut progress_ui) = progress_ui.take() {
                    progress_ui.success_with(spinner_label);
                }
            }
            if progress.state == IndexState::Complete {
                break;
            }
            continue;
        }

        let spinner_label = render_index_watch_message(&progress);
        if let Some(progress_ui) = progress_ui.as_mut() {
            progress_ui.set_state(watch_progress_ui_state(&progress));
        }

        if stream_updates {
            let rendered = formatter.format_index_watch_update(&progress);
            print_watch_output(&rendered);
        }

        if progress.state == IndexState::Complete {
            if let Some(mut progress_ui) = progress_ui.take() {
                progress_ui.success_with(spinner_label);
            }
        }

        last_rendered_state = Some(current_state);
        last_rendered_at = Some(Instant::now());

        if progress.state == IndexState::Complete {
            break;
        }
    }
}

fn start_watch_progress_ui(format: OutputFormat) -> Option<ProgressUi> {
    if format != OutputFormat::Human {
        return None;
    }

    Some(ProgressUi::stderr_if(
        ProgressState::spinner("Watching index progress"),
        true,
    ))
}

fn should_stream_watch_updates(format: OutputFormat, stderr_is_terminal: bool) -> bool {
    match format {
        OutputFormat::Human => !stderr_is_terminal,
        OutputFormat::Json | OutputFormat::Plain => true,
    }
}

fn should_render_watch_update(
    last_rendered_state: Option<&WatchRenderState>,
    current_state: &WatchRenderState,
    last_rendered_at: Option<Instant>,
) -> bool {
    let Some(last_rendered_state) = last_rendered_state else {
        return true;
    };

    if current_state == last_rendered_state {
        return false;
    }

    if current_state.state == IndexState::Complete
        || current_state.state != last_rendered_state.state
        || current_state.phase != last_rendered_state.phase
        || (same_progress_counters(last_rendered_state, current_state)
            && current_state.message != last_rendered_state.message)
    {
        return true;
    }

    last_rendered_at
        .is_none_or(|last_rendered_at| last_rendered_at.elapsed() >= WATCH_RENDER_INTERVAL)
}

fn same_progress_counters(left: &WatchRenderState, right: &WatchRenderState) -> bool {
    left.files_processed == right.files_processed
        && left.files_total == right.files_total
        && left.files_indexed == right.files_indexed
        && left.files_skipped == right.files_skipped
        && left.files_deleted == right.files_deleted
        && left.segments_stored == right.segments_stored
        && left.embeddings_enabled == right.embeddings_enabled
        && left.parallelism == right.parallelism
}

fn watch_progress_ui_state(progress: &IndexProgress) -> ProgressState {
    let message = render_index_watch_message(progress);
    match progress.phase {
        IndexPhase::Parsing | IndexPhase::Storing if progress.files_total > 0 => {
            ProgressState::items(
                message,
                progress.files_processed as u64,
                progress.files_total as u64,
            )
        }
        _ => ProgressState::spinner(message),
    }
}

fn print_watch_output(rendered: &str) {
    if rendered.ends_with('\n') {
        print!("{rendered}");
    } else {
        println!("{rendered}");
    }
}

fn to_json<T: Serialize + ?Sized>(value: &T) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"))
}

fn render_lifecycle_state_human(state: LifecycleState) -> String {
    match state {
        LifecycleState::NotStarted => "not started".yellow().to_string(),
        LifecycleState::Indexing => "indexing".yellow().to_string(),
        LifecycleState::Active => "active".green().to_string(),
        LifecycleState::Registered => "registered".cyan().to_string(),
        LifecycleState::Stopped => "stopped".red().to_string(),
    }
}

fn render_stop_status_human(status: StopStatus) -> String {
    match status {
        StopStatus::Stopped => "stopped".green().to_string(),
        StopStatus::NotRegistered => "not registered".yellow().to_string(),
        StopStatus::DaemonNotRunning => "daemon not running".yellow().to_string(),
        StopStatus::Unsupported => "unsupported".red().to_string(),
    }
}

fn render_daemon_state_human(running: bool) -> String {
    if running {
        "running".green().to_string()
    } else {
        "stopped".red().to_string()
    }
}

fn render_bool_human(value: bool) -> String {
    if value {
        "yes".green().to_string()
    } else {
        "no".yellow().to_string()
    }
}

fn append_start_fields_human(out: &mut String, result: &StartResultInfo) {
    out.push_str(&format!(
        "Status: {}\n",
        render_start_status_human(result.status)
    ));
    if let Some(project_id) = &result.project_id {
        out.push_str(&format!("Project ID: {project_id}\n"));
    }
    if let Some(project_root) = &result.project_root {
        out.push_str(&format!("Project root: {}\n", project_root.display()));
    }
    if let Some(source_root) = &result.source_root {
        out.push_str(&format!("Source root: {}\n", source_root.display()));
    }
    match result.registered {
        Some(registered) => {
            out.push_str(&format!("Registered: {}\n", render_bool_human(registered)))
        }
        None => out.push_str(&format!("Registered: {}\n", "unknown".yellow())),
    }
    out.push_str(&format!(
        "Index: {}\n",
        render_start_index_human(result.index_status)
    ));
    if let Some(pid) = result.pid {
        out.push_str(&format!("PID: {pid}\n"));
    }
}

fn render_start_status_human(status: StartStatus) -> String {
    match status {
        StartStatus::Started => "started".green().to_string(),
        StartStatus::IndexedAndStarted => "indexed and started".green().to_string(),
        StartStatus::AlreadyRunning => "already running".cyan().to_string(),
        StartStatus::StartupInProgress => "startup in progress".yellow().to_string(),
    }
}

fn render_start_index_human(status: Option<ProjectListIndexStatus>) -> String {
    match status {
        Some(ProjectListIndexStatus::Ready) => "ready".green().to_string(),
        Some(ProjectListIndexStatus::NotBuilt) => "not built".yellow().to_string(),
        Some(ProjectListIndexStatus::Unavailable) => "unavailable".red().to_string(),
        None => "unknown".yellow().to_string(),
    }
}

fn render_index_state_human(state: IndexState) -> String {
    match state {
        IndexState::Idle => "idle".dimmed().to_string(),
        IndexState::Running => "running".yellow().to_string(),
        IndexState::Complete => "complete".green().to_string(),
    }
}

fn render_index_phase_human(progress: &IndexProgress) -> String {
    let label = match progress.phase {
        IndexPhase::Pending => "pending",
        IndexPhase::Preparing => "preparing",
        IndexPhase::Rebuilding => "rebuilding",
        IndexPhase::LoadingModel => "loading model",
        IndexPhase::Scanning => "scanning",
        IndexPhase::Parsing => "parsing",
        IndexPhase::Storing if progress.embeddings_enabled => "embedding & storing",
        IndexPhase::Storing => "storing",
        IndexPhase::Complete => "complete",
    };
    label.cyan().to_string()
}

fn render_embeddings_human(enabled: bool) -> String {
    if enabled {
        "enabled".green().to_string()
    } else {
        "disabled".yellow().to_string()
    }
}

fn render_duration_ms(duration_ms: u128) -> String {
    format!("{duration_ms}ms")
}

fn render_index_watch_message(progress: &IndexProgress) -> String {
    progress
        .message
        .clone()
        .unwrap_or_else(|| match progress.phase {
            IndexPhase::Pending => "Waiting for indexing".to_string(),
            IndexPhase::Preparing => "Preparing database".to_string(),
            IndexPhase::Rebuilding => "Rebuilding database".to_string(),
            IndexPhase::LoadingModel => "Loading embedding model".to_string(),
            IndexPhase::Scanning => {
                if progress.files_total == 0 {
                    "Scanning files".to_string()
                } else {
                    format!("Scanning {} files", progress.files_total)
                }
            }
            IndexPhase::Parsing | IndexPhase::Storing => {
                format!(
                    "Processing files ({}/{})",
                    progress.files_processed, progress.files_total
                )
            }
            IndexPhase::Complete => "Index complete".to_string(),
        })
}

fn render_time_ago(ts: &chrono::DateTime<Utc>) -> String {
    let secs = Utc::now().signed_duration_since(*ts).num_seconds().max(0);
    match secs {
        0..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}

fn render_index_state_plain(state: IndexState) -> &'static str {
    match state {
        IndexState::Idle => "idle",
        IndexState::Running => "running",
        IndexState::Complete => "complete",
    }
}

fn render_index_phase_plain(progress: &IndexProgress) -> &'static str {
    render_index_phase(progress)
}

fn render_embeddings_plain(enabled: bool) -> &'static str {
    if enabled {
        "enabled"
    } else {
        "disabled"
    }
}

fn render_optional_pid_plain(pid: Option<u32>) -> String {
    pid.map(|pid| pid.to_string())
        .unwrap_or_else(|| "none".to_string())
}

fn render_optional_bool_plain(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "true",
        Some(false) => "false",
        None => "unknown",
    }
}

fn render_optional_plain(value: Option<&str>) -> String {
    plain_field(value.unwrap_or("none"))
}

fn render_optional_path_plain(value: Option<&PathBuf>) -> String {
    value
        .map(|path| plain_field(&path.display().to_string()))
        .unwrap_or_else(|| "none".to_string())
}

fn render_start_index_plain(status: Option<ProjectListIndexStatus>) -> &'static str {
    status
        .map(ProjectListIndexStatus::as_str)
        .unwrap_or("unknown")
}

fn plain_field(value: &str) -> String {
    value
        .chars()
        .map(|ch| match ch {
            '\t' | '\n' | '\r' => ' ',
            other => other,
        })
        .collect()
}

fn render_index_health_human(status: &StatusInfo) -> colored::ColoredString {
    match render_index_health_plain(status) {
        "ready" => "ready".green(),
        "indexing" => "indexing".yellow(),
        "not_built" => "not built".yellow(),
        _ => "unavailable".red(),
    }
}

fn render_index_health_plain(status: &StatusInfo) -> &'static str {
    if status
        .index_progress
        .as_ref()
        .is_some_and(|progress| progress.state == IndexState::Running)
    {
        "indexing"
    } else if !status.index_present {
        "not_built"
    } else if status.index_readable {
        "ready"
    } else {
        "unavailable"
    }
}

#[derive(Debug, Clone)]
struct ProjectListRow {
    project: String,
    state: LifecycleState,
    index: ProjectListIndexStatus,
    files: String,
    segments: String,
    path: String,
    checked: String,
}

impl ProjectListRow {
    fn from_item(item: &ProjectListItem) -> Self {
        Self {
            project: compact_project_id(&item.project_id),
            state: item.state,
            index: item.index_status,
            files: render_optional_count(item.files),
            segments: render_optional_count(item.segments),
            path: render_project_list_path(&item.project_root, &item.source_root),
            checked: item
                .last_file_check_at
                .as_ref()
                .map(render_time_ago)
                .unwrap_or_else(|| "none".to_string()),
        }
    }
}

#[derive(Debug, Clone)]
struct ProjectListWidths {
    project: usize,
    state: usize,
    index: usize,
    files: usize,
    segments: usize,
    path: usize,
    checked: usize,
}

impl ProjectListWidths {
    fn from_rows(rows: &[ProjectListRow]) -> Self {
        let mut widths = Self {
            project: "Project".len(),
            state: "State".len(),
            index: "Index".len(),
            files: "Files".len(),
            segments: "Segments".len(),
            path: "Path".len(),
            checked: "Checked".len(),
        };

        for row in rows {
            widths.project = widths.project.max(row.project.len());
            widths.state = widths.state.max(row.state.as_str().len());
            widths.index = widths.index.max(row.index.as_str().len());
            widths.files = widths.files.max(row.files.len());
            widths.segments = widths.segments.max(row.segments.len());
            widths.path = widths.path.max(row.path.len());
            widths.checked = widths.checked.max(row.checked.len());
        }

        widths
    }
}

fn format_project_list_header(widths: &ProjectListWidths) -> String {
    format!(
        "{:<project$}  {:<state$}  {:<index$}  {:>files$}  {:>segments$}  {:<path$}  {:<checked$}\n",
        "Project",
        "State",
        "Index",
        "Files",
        "Segments",
        "Path",
        "Checked",
        project = widths.project,
        state = widths.state,
        index = widths.index,
        files = widths.files,
        segments = widths.segments,
        path = widths.path,
        checked = widths.checked,
    )
}

fn format_project_list_separator(widths: &ProjectListWidths) -> String {
    format!(
        "{}  {}  {}  {}  {}  {}  {}\n",
        "-".repeat(widths.project),
        "-".repeat(widths.state),
        "-".repeat(widths.index),
        "-".repeat(widths.files),
        "-".repeat(widths.segments),
        "-".repeat(widths.path),
        "-".repeat(widths.checked),
    )
}

fn format_project_list_row(row: &ProjectListRow, widths: &ProjectListWidths) -> String {
    format!(
        "{:<project$}  {}  {}  {:>files$}  {:>segments$}  {:<path$}  {:<checked$}\n",
        row.project,
        render_project_list_state_cell(row.state, widths.state),
        render_project_list_index_cell(row.index, widths.index),
        row.files,
        row.segments,
        row.path,
        row.checked,
        project = widths.project,
        files = widths.files,
        segments = widths.segments,
        path = widths.path,
        checked = widths.checked,
    )
}

fn render_project_list_state_cell(state: LifecycleState, width: usize) -> String {
    let padded = format!("{:<width$}", state.as_str(), width = width);
    match state {
        LifecycleState::NotStarted => padded.yellow().to_string(),
        LifecycleState::Indexing => padded.yellow().to_string(),
        LifecycleState::Active => padded.green().to_string(),
        LifecycleState::Registered => padded.cyan().to_string(),
        LifecycleState::Stopped => padded.red().to_string(),
    }
}

fn render_project_list_index_cell(status: ProjectListIndexStatus, width: usize) -> String {
    let padded = format!("{:<width$}", status.as_str(), width = width);
    match status {
        ProjectListIndexStatus::Ready => padded.green().to_string(),
        ProjectListIndexStatus::NotBuilt => padded.yellow().to_string(),
        ProjectListIndexStatus::Unavailable => padded.red().to_string(),
    }
}

fn render_optional_count(value: Option<u64>) -> String {
    match value {
        Some(count) => count.to_string(),
        None => "unknown".to_string(),
    }
}

fn compact_project_id(project_id: &str) -> String {
    if is_uuid_like(project_id) {
        project_id.chars().take(8).collect()
    } else {
        project_id.to_string()
    }
}

fn is_uuid_like(value: &str) -> bool {
    value.len() == 36
        && value.chars().enumerate().all(|(idx, ch)| {
            if matches!(idx, 8 | 13 | 18 | 23) {
                ch == '-'
            } else {
                ch.is_ascii_hexdigit()
            }
        })
}

fn render_project_list_path(project_root: &Path, source_root: &Path) -> String {
    const MAX_PATH_WIDTH: usize = 56;

    let root = compact_display_path(project_root);
    let source = compact_display_path(source_root);
    let path = if project_root == source_root {
        root
    } else {
        format!("{root} -> {source}")
    };

    ellipsize_middle(&path, MAX_PATH_WIDTH)
}

fn compact_display_path(path: &Path) -> String {
    if let Some(home) = dirs::home_dir() {
        if path == home {
            return "~".to_string();
        }
        if let Ok(rest) = path.strip_prefix(&home) {
            return PathBuf::from("~").join(rest).display().to_string();
        }
    }
    path.display().to_string()
}

fn ellipsize_middle(value: &str, max_width: usize) -> String {
    let len = value.chars().count();
    if len <= max_width {
        return value.to_string();
    }

    if max_width <= 3 {
        return ".".repeat(max_width);
    }

    let keep = max_width - 3;
    let left = keep / 2;
    let right = keep - left;
    let prefix: String = value.chars().take(left).collect();
    let suffix: String = value
        .chars()
        .rev()
        .take(right)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{prefix}...{suffix}")
}

fn render_optional_time(value: Option<&DateTime<Utc>>) -> String {
    value
        .map(|ts| ts.to_rfc3339())
        .unwrap_or_else(|| "none".to_string())
}

fn render_index_phase(progress: &IndexProgress) -> &'static str {
    match progress.phase {
        IndexPhase::Pending => "pending",
        IndexPhase::Preparing => "preparing",
        IndexPhase::Rebuilding => "rebuilding",
        IndexPhase::LoadingModel => "loading_model",
        IndexPhase::Scanning => "scanning",
        IndexPhase::Parsing => "parsing",
        IndexPhase::Storing if progress.embeddings_enabled => "embedding_and_storing",
        IndexPhase::Storing => "storing",
        IndexPhase::Complete => "complete",
    }
}

fn render_update_status_label(status: &UpdateStatus) -> &'static str {
    match status {
        UpdateStatus::UpToDate => "up_to_date",
        UpdateStatus::UpdateAvailable { .. } => "update_available",
        UpdateStatus::Yanked { .. } => "yanked",
        UpdateStatus::BelowMinimumSafe { .. } => "below_minimum_safe",
    }
}

fn render_cache_age(secs: i64) -> String {
    match secs {
        0..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::types::{IndexParallelism, IndexStageTimings};

    fn sample_progress() -> IndexProgress {
        IndexProgress {
            state: IndexState::Complete,
            phase: IndexPhase::Complete,
            files_total: 6,
            files_scanned: 6,
            files_processed: 6,
            files_indexed: 3,
            files_skipped: 2,
            files_deleted: 1,
            segments_stored: 14,
            embeddings_enabled: true,
            message: Some("Processed 6 files".to_string()),
            parallelism: Some(IndexParallelism {
                jobs_configured: 4,
                jobs_effective: 3,
                embed_threads: 2,
            }),
            timings: Some(IndexStageTimings {
                scan_ms: 11,
                parse_ms: 17,
                embed_ms: 23,
                store_ms: 5,
                total_ms: 41,
                db_prepare_ms: None,
                model_prepare_ms: None,
                input_prep_ms: None,
            }),
            scope: None,
            prefilter: None,
            updated_at: chrono::DateTime::parse_from_rfc3339("2026-04-03T06:07:08Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        }
    }

    #[test]
    fn json_index_summary_includes_work_parallelism_and_timings() {
        let formatter = JsonFormatter;
        let rendered = formatter.format_index_summary("indexed", &sample_progress());
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();

        assert_eq!(value["message"], "indexed");
        assert_eq!(value["work"]["files_completed"], 4);
        assert_eq!(value["work"]["files_skipped"], 2);
        assert_eq!(value["progress"]["parallelism"]["jobs_effective"], 3);
        assert_eq!(value["progress"]["timings"]["total_ms"], 41);
    }

    #[test]
    fn json_start_result_without_progress_omits_index_work() {
        let formatter = JsonFormatter;
        let rendered = formatter.format_start_result(&StartResultInfo {
            status: StartStatus::Started,
            project_id: Some("project-123".to_string()),
            project_root: Some(PathBuf::from("/repo")),
            source_root: Some(PathBuf::from("/repo")),
            registered: Some(true),
            index_status: Some(ProjectListIndexStatus::Ready),
            pid: Some(42),
            message: "Daemon started.".to_string(),
            progress: None,
        });
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();

        assert_eq!(value["status"], "started");
        assert_eq!(value["project_id"], "project-123");
        assert_eq!(value["registered"], true);
        assert_eq!(value["index_status"], "ready");
        assert_eq!(value["pid"], 42);
        assert_eq!(value["message"], "Daemon started.");
        assert!(value.get("progress").is_none());
        assert!(value.get("work").is_none());
    }

    #[test]
    fn json_start_result_with_progress_includes_index_work() {
        let formatter = JsonFormatter;
        let rendered = formatter.format_start_result(&StartResultInfo {
            status: StartStatus::IndexedAndStarted,
            project_id: Some("project-123".to_string()),
            project_root: Some(PathBuf::from("/repo")),
            source_root: Some(PathBuf::from("/repo")),
            registered: Some(true),
            index_status: Some(ProjectListIndexStatus::Ready),
            pid: Some(42),
            message: "Indexed and started.".to_string(),
            progress: Some(sample_progress()),
        });
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();

        assert_eq!(value["status"], "indexed_and_started");
        assert_eq!(value["pid"], 42);
        assert_eq!(value["progress"]["files_indexed"], 3);
        assert_eq!(value["work"]["files_completed"], 4);
    }

    #[test]
    fn plain_start_result_uses_stable_lifecycle_field_order() {
        let formatter = PlainFormatter;
        let rendered = formatter.format_start_result(&StartResultInfo {
            status: StartStatus::IndexedAndStarted,
            project_id: Some("project-123".to_string()),
            project_root: Some(PathBuf::from("/repo")),
            source_root: Some(PathBuf::from("/repo/src")),
            registered: Some(true),
            index_status: Some(ProjectListIndexStatus::Ready),
            pid: Some(42),
            message: "Indexed and started.".to_string(),
            progress: Some(sample_progress()),
        });

        assert_eq!(
            rendered,
            "status:indexed_and_started\tproject_id:project-123\tproject_root:/repo\tsource_root:/repo/src\tregistered:true\tindex:ready\tpid:42\tmessage:Indexed and started.\n"
        );
        assert!(!rendered.contains('\u{1b}'));
    }

    #[test]
    fn human_index_summary_renders_work_parallelism_and_timings() {
        let formatter = HumanFormatter;
        let rendered = formatter.format_index_summary("indexed", &sample_progress());

        assert!(rendered.contains("Work: completed 4 (3 indexed, 1 deleted) | skipped 2"));
        assert!(
            rendered.contains("Parallelism: workers 3 effective / 4 configured | embed threads 2")
        );
        assert!(rendered
            .contains("Timings: scan 11ms | parse 17ms | embed 23ms | store 5ms | total 41ms"));
    }

    #[test]
    fn json_watch_update_is_compact_and_includes_progress_event() {
        let formatter = JsonFormatter;
        let rendered = formatter.format_index_watch_update(&sample_progress());
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();

        assert_eq!(value["event"], "index_progress");
        assert_eq!(value["progress"]["files_processed"], 6);
        assert_eq!(value["progress"]["message"], "Processed 6 files");
    }

    #[test]
    fn plain_watch_update_includes_processed_and_message() {
        let formatter = PlainFormatter;
        let rendered = formatter.format_index_watch_update(&sample_progress());

        assert!(rendered.contains("event:index_progress"));
        assert!(rendered.contains("files_processed:6"));
        assert!(rendered.contains("message:Processed 6 files"));
    }

    #[test]
    fn human_watch_updates_use_in_place_rendering_on_ttys() {
        assert!(!should_stream_watch_updates(OutputFormat::Human, true));
        assert!(should_stream_watch_updates(OutputFormat::Human, false));
    }

    #[test]
    fn plain_and_json_watch_updates_always_stream() {
        assert!(should_stream_watch_updates(OutputFormat::Plain, true));
        assert!(should_stream_watch_updates(OutputFormat::Plain, false));
        assert!(should_stream_watch_updates(OutputFormat::Json, true));
        assert!(should_stream_watch_updates(OutputFormat::Json, false));
    }

    #[test]
    fn watch_render_state_ignores_timing_and_timestamp_noise() {
        let baseline = WatchRenderState::from(&sample_progress());
        let mut noisy = sample_progress();
        noisy.timings = Some(IndexStageTimings {
            scan_ms: 99,
            parse_ms: 101,
            embed_ms: 103,
            store_ms: 105,
            total_ms: 407,
            db_prepare_ms: None,
            model_prepare_ms: None,
            input_prep_ms: None,
        });
        noisy.updated_at = chrono::DateTime::parse_from_rfc3339("2026-04-03T06:07:09Z")
            .unwrap()
            .with_timezone(&chrono::Utc);

        assert_eq!(baseline, WatchRenderState::from(&noisy));
    }

    #[test]
    fn watch_render_changes_are_throttled_within_the_same_phase() {
        let mut previous_progress = sample_progress();
        previous_progress.state = IndexState::Running;
        previous_progress.phase = IndexPhase::Storing;
        let previous = WatchRenderState::from(&previous_progress);
        let mut next = previous_progress;
        next.files_processed += 1;
        next.message = Some("Processed 7 files".to_string());
        let next_state = WatchRenderState::from(&next);

        assert!(!should_render_watch_update(
            Some(&previous),
            &next_state,
            Some(Instant::now())
        ));
    }

    #[test]
    fn watch_render_phase_changes_are_emitted_immediately() {
        let previous = WatchRenderState::from(&sample_progress());
        let mut next = sample_progress();
        next.phase = IndexPhase::LoadingModel;
        next.message = Some("Embedding model ready".to_string());
        let next_state = WatchRenderState::from(&next);

        assert!(should_render_watch_update(
            Some(&previous),
            &next_state,
            Some(Instant::now())
        ));
    }

    #[test]
    fn watch_progress_ui_state_uses_bar_for_file_processing() {
        let mut progress = sample_progress();
        progress.state = IndexState::Running;
        progress.phase = IndexPhase::Parsing;
        progress.files_processed = 3;
        progress.files_total = 6;
        progress.message = Some("Processing files".to_string());

        assert_eq!(
            watch_progress_ui_state(&progress),
            ProgressState::items("Processing files", 3, 6)
        );
    }

    #[test]
    fn watch_progress_ui_state_keeps_spinner_for_unbounded_phases() {
        let mut progress = sample_progress();
        progress.state = IndexState::Running;
        progress.phase = IndexPhase::LoadingModel;
        progress.message = Some("Loading embedding model".to_string());

        assert_eq!(
            watch_progress_ui_state(&progress),
            ProgressState::spinner("Loading embedding model")
        );
    }

    #[test]
    fn plain_status_renders_last_work_and_total_duration() {
        let formatter = PlainFormatter;
        let rendered = formatter.format_status(&StatusInfo {
            lifecycle_state: LifecycleState::Active,
            registered: true,
            daemon_running: true,
            pid: Some(42),
            project_initialized: true,
            indexed_files: Some(3),
            total_segments: Some(14),
            project_id: Some("project-123".to_string()),
            project_root: PathBuf::from("/repo"),
            source_root: PathBuf::from("/repo"),
            index_present: true,
            index_readable: true,
            last_file_check_at: Some(sample_progress().updated_at),
            index_progress: Some(sample_progress()),
        });

        assert!(rendered.starts_with("lifecycle:active\tregistered:true\tdaemon:running"));
        assert!(rendered.contains("project_root:/repo"));
        assert!(rendered.contains("last_completed:4"));
        assert!(rendered.contains("last_processed:6"));
        assert!(rendered.contains("index_message:Processed 6 files"));
        assert!(rendered.contains("jobs_effective:3"));
        assert!(rendered.contains("total_ms:41"));
        assert!(!rendered.contains("last_file_check_ago"));
        assert!(!rendered.contains("\tago:"));
        assert!(!rendered.contains('\u{1b}'));
    }

    #[test]
    fn human_status_reports_uninitialized_project_and_missing_index() {
        let formatter = HumanFormatter;
        let rendered = formatter.format_status(&StatusInfo {
            lifecycle_state: LifecycleState::NotStarted,
            registered: false,
            daemon_running: false,
            pid: None,
            project_initialized: false,
            indexed_files: None,
            total_segments: None,
            project_id: None,
            project_root: PathBuf::from("/repo"),
            source_root: PathBuf::from("/repo"),
            index_present: false,
            index_readable: false,
            last_file_check_at: None,
            index_progress: None,
        });

        assert!(rendered.contains("Lifecycle: not started"));
        assert!(rendered.contains("Registered: no"));
        assert!(rendered.contains("Project ID: not initialized"));
        assert!(rendered.contains("Index: not built"));
        assert!(rendered.contains("Last file check: never recorded"));
    }

    #[test]
    fn plain_status_reports_uninitialized_project_and_missing_index() {
        let formatter = PlainFormatter;
        let rendered = formatter.format_status(&StatusInfo {
            lifecycle_state: LifecycleState::NotStarted,
            registered: false,
            daemon_running: false,
            pid: None,
            project_initialized: false,
            indexed_files: None,
            total_segments: None,
            project_id: None,
            project_root: PathBuf::from("/repo"),
            source_root: PathBuf::from("/repo"),
            index_present: false,
            index_readable: false,
            last_file_check_at: None,
            index_progress: None,
        });

        assert!(rendered.starts_with("lifecycle:not_started\tregistered:false\tdaemon:stopped"));
        assert!(rendered.contains("project_initialized:false"));
        assert!(rendered.contains("project_id:none"));
        assert!(rendered.contains("index:not_built"));
        assert!(rendered.contains("last_file_check:none"));
        assert!(!rendered.contains('\u{1b}'));
    }

    #[test]
    fn plain_stop_result_renders_structured_fields() {
        let formatter = PlainFormatter;
        let rendered = formatter.format_stop_result(&StopResultInfo {
            status: StopStatus::DaemonNotRunning,
            project_root: PathBuf::from("/repo"),
            registered: false,
            daemon_running: false,
            pid: None,
            message: "Project deregistered. No daemon is currently running.".to_string(),
        });

        assert_eq!(
            rendered,
            "status:daemon_not_running\tproject_root:/repo\tregistered:false\tdaemon:stopped\tpid:none\tmessage:Project deregistered. No daemon is currently running.\n"
        );
        assert!(!rendered.contains('\u{1b}'));
    }

    #[test]
    fn plain_project_list_uses_stable_lifecycle_field_order() {
        let formatter = PlainFormatter;
        let rendered = formatter.format_project_list(&ProjectListInfo {
            projects: vec![ProjectListItem {
                project_id: "project-123".to_string(),
                state: LifecycleState::Active,
                project_root: PathBuf::from("/repo"),
                source_root: PathBuf::from("/repo/src"),
                registered_at: "2026-05-01T00:00:00Z".to_string(),
                daemon_running: true,
                index_status: ProjectListIndexStatus::Ready,
                files: Some(3),
                segments: Some(14),
                last_file_check_at: Some(sample_progress().updated_at),
                index_progress: None,
            }],
        });

        assert_eq!(
            rendered,
            "project:project-123\tstate:active\tproject_root:/repo\tsource_root:/repo/src\tindex:ready\tfiles:3\tsegments:14\tlast_file_check:2026-04-03T06:07:08+00:00\tregistered_at:2026-05-01T00:00:00Z\n"
        );
        assert!(!rendered.contains('\u{1b}'));
    }

    #[test]
    fn human_project_list_renders_clear_empty_state() {
        let formatter = HumanFormatter;
        let rendered = formatter.format_project_list(&ProjectListInfo { projects: vec![] });

        assert_eq!(
            rendered,
            "No registered projects.\nRun `1up start` in a repository to register one.\n"
        );
    }

    #[test]
    fn human_project_list_renders_table_for_registered_projects() {
        let formatter = HumanFormatter;
        let rendered = formatter.format_project_list(&ProjectListInfo {
            projects: vec![ProjectListItem {
                project_id: "project-123".to_string(),
                state: LifecycleState::Registered,
                project_root: PathBuf::from("/repo"),
                source_root: PathBuf::from("/repo/src"),
                registered_at: "2026-05-01T00:00:00Z".to_string(),
                daemon_running: false,
                index_status: ProjectListIndexStatus::NotBuilt,
                files: None,
                segments: None,
                last_file_check_at: None,
                index_progress: None,
            }],
        });

        assert!(rendered.starts_with("Registered projects\n"));
        assert!(rendered.contains("Project"));
        assert!(rendered.contains("State"));
        assert!(rendered.contains("Index"));
        assert!(rendered.contains("Path"));
        assert!(rendered.contains("Checked"));
        assert!(!rendered.contains("Project root"));
        assert!(!rendered.contains("Source root"));
        assert!(rendered.contains("project-123"));
        assert!(rendered.contains("registered"));
        assert!(rendered.contains("not_built"));
        assert!(rendered.contains("/repo"));
        assert!(rendered.contains("/repo/src"));
        assert!(rendered.contains("unknown"));
        assert!(rendered.contains("none"));
    }

    #[test]
    fn human_project_list_compacts_wide_values_for_readability() {
        let formatter = HumanFormatter;
        let rendered = formatter.format_project_list(&ProjectListInfo {
            projects: vec![ProjectListItem {
                project_id: "123e4567-e89b-12d3-a456-426614174000".to_string(),
                state: LifecycleState::Active,
                project_root: PathBuf::from(
                    "/Users/prem/Development/some-very-long-project-name-with-extra-segments",
                ),
                source_root: PathBuf::from(
                    "/Users/prem/Development/some-very-long-project-name-with-extra-segments/worktree-feature-branch",
                ),
                registered_at: "2026-05-01T00:00:00Z".to_string(),
                daemon_running: true,
                index_status: ProjectListIndexStatus::Ready,
                files: Some(132),
                segments: Some(3732),
                last_file_check_at: Some(sample_progress().updated_at),
                index_progress: None,
            }],
        });

        assert!(rendered.contains("123e4567"));
        assert!(!rendered.contains("123e4567-e89b-12d3-a456-426614174000"));
        assert!(rendered.contains("..."));
        assert!(rendered.contains(" ago"));
        assert!(rendered
            .lines()
            .filter(|line| !line.is_empty())
            .all(|line| line.len() <= 130));
    }

    fn sample_update_status_info(status: UpdateStatus) -> UpdateStatusInfo {
        let update_available = !matches!(status, UpdateStatus::UpToDate);
        UpdateStatusInfo {
            current_version: "0.1.0".to_string(),
            cached: true,
            latest_version: Some("0.2.0".to_string()),
            update_available,
            status,
            install_channel: Some(InstallChannel::Manual),
            checked_at: Some(
                chrono::DateTime::parse_from_rfc3339("2026-04-10T12:00:00Z")
                    .unwrap()
                    .with_timezone(&chrono::Utc),
            ),
            cache_age_secs: Some(3600),
            yanked: false,
            minimum_safe_version: None,
            message: None,
            notes_url: Some("https://github.com/rp1-run/1up/releases/v0.2.0".to_string()),
            upgrade_instruction: Some("1up update".to_string()),
            status_message: None,
        }
    }

    #[test]
    fn json_update_status_with_cache_includes_required_fields() {
        let formatter = JsonFormatter;
        let info = sample_update_status_info(UpdateStatus::UpdateAvailable {
            latest: "0.2.0".to_string(),
        });
        let rendered = formatter.format_update_status(&info);
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();

        assert_eq!(value["current_version"], "0.1.0");
        assert_eq!(value["latest_version"], "0.2.0");
        assert_eq!(value["update_available"], true);
        assert_eq!(value["install_channel"], "manual");
        assert_eq!(value["upgrade_instruction"], "1up update");
        assert!(value["cache_age_secs"].is_number());
        assert!(value["checked_at"].is_string());
    }

    #[test]
    fn json_update_status_without_cache_shows_cached_false() {
        let formatter = JsonFormatter;
        let info = UpdateStatusInfo {
            current_version: "0.1.0".to_string(),
            cached: false,
            latest_version: None,
            update_available: false,
            status: UpdateStatus::UpToDate,
            install_channel: None,
            checked_at: None,
            cache_age_secs: None,
            yanked: false,
            minimum_safe_version: None,
            message: None,
            notes_url: None,
            upgrade_instruction: None,
            status_message: None,
        };
        let rendered = formatter.format_update_status(&info);
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();

        assert_eq!(value["cached"], false);
        assert_eq!(value["current_version"], "0.1.0");
    }

    #[test]
    fn human_update_status_without_cache_uses_custom_status_message() {
        let formatter = HumanFormatter;
        let info = UpdateStatusInfo {
            current_version: "0.1.0".to_string(),
            cached: false,
            latest_version: None,
            update_available: false,
            status: UpdateStatus::UpToDate,
            install_channel: None,
            checked_at: None,
            cache_age_secs: None,
            yanked: false,
            minimum_safe_version: None,
            message: None,
            notes_url: None,
            upgrade_instruction: None,
            status_message: Some("Updates are disabled for this build.".to_string()),
        };
        let rendered = formatter.format_update_status(&info);

        assert!(rendered.contains("Updates are disabled for this build."));
        assert!(!rendered.contains("Run `1up update --check`"));
    }

    #[test]
    fn human_update_status_shows_version_and_status() {
        let formatter = HumanFormatter;
        let info = sample_update_status_info(UpdateStatus::UpdateAvailable {
            latest: "0.2.0".to_string(),
        });
        let rendered = formatter.format_update_status(&info);

        assert!(rendered.contains("Current version:"));
        assert!(rendered.contains("0.1.0"));
        assert!(rendered.contains("Latest version:"));
        assert!(rendered.contains("0.2.0"));
        assert!(rendered.contains("update available"));
        assert!(rendered.contains("Run: 1up update"));
    }

    #[test]
    fn plain_update_status_produces_tab_delimited_output() {
        let formatter = PlainFormatter;
        let info = sample_update_status_info(UpdateStatus::UpdateAvailable {
            latest: "0.2.0".to_string(),
        });
        let rendered = formatter.format_update_status(&info);

        assert!(rendered.contains("current:0.1.0"));
        assert!(rendered.contains("\tlatest:0.2.0"));
        assert!(rendered.contains("\tstatus:update_available"));
        assert!(rendered.contains("\tupdate_available:true"));
        assert!(rendered.contains("\tchannel:manual"));
        assert!(rendered.contains("\tinstruction:1up update"));
    }

    #[test]
    fn json_update_result_updated_has_version_fields() {
        let formatter = JsonFormatter;
        let result = UpdateResult::Updated {
            old_version: "0.1.0".to_string(),
            new_version: "0.2.0".to_string(),
        };
        let rendered = formatter.format_update_result(&result);
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();

        assert_eq!(value["updated"], true);
        assert_eq!(value["old_version"], "0.1.0");
        assert_eq!(value["new_version"], "0.2.0");
    }

    #[test]
    fn plain_update_result_up_to_date() {
        let formatter = PlainFormatter;
        let result = UpdateResult::UpToDate {
            current_version: "0.1.0".to_string(),
            latest_version: "0.1.0".to_string(),
        };
        let rendered = formatter.format_update_result(&result);

        assert!(rendered.contains("current:0.1.0"));
        assert!(rendered.contains("update_available:false"));
    }

    #[test]
    fn human_update_result_channel_managed_shows_instruction() {
        let formatter = HumanFormatter;
        let result = UpdateResult::ChannelManaged {
            current_version: "0.1.0".to_string(),
            latest_version: "0.2.0".to_string(),
            install_channel: InstallChannel::Homebrew,
            upgrade_instruction: "brew upgrade rp1-run/tap/1up".to_string(),
            status: UpdateStatus::UpdateAvailable {
                latest: "0.2.0".to_string(),
            },
            message: None,
        };
        let rendered = formatter.format_update_result(&result);

        assert!(rendered.contains("Update available: 1up"));
        assert!(rendered.contains("0.2.0"));
        assert!(rendered.contains("managed by homebrew"));
        assert!(rendered.contains("brew upgrade rp1-run/tap/1up"));
    }

    #[test]
    fn human_update_result_channel_managed_yanked_shows_warning() {
        let formatter = HumanFormatter;
        let result = UpdateResult::ChannelManaged {
            current_version: "0.1.0".to_string(),
            latest_version: "0.2.0".to_string(),
            install_channel: InstallChannel::Homebrew,
            upgrade_instruction: "brew upgrade rp1-run/tap/1up".to_string(),
            status: UpdateStatus::Yanked {
                latest: "0.2.0".to_string(),
                message: Some("Critical bug fix".to_string()),
            },
            message: None,
        };
        let rendered = formatter.format_update_result(&result);

        assert!(rendered.contains("WARNING"));
        assert!(rendered.contains("recalled"));
        assert!(rendered.contains("Critical bug fix"));
    }
}
