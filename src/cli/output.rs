use chrono::{DateTime, Utc};
use colored::Colorize;
use serde::Serialize;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::search::impact::{
    ImpactCandidate, ImpactReason, ImpactResultEnvelope, ImpactStatus, ResolvedImpactAnchor,
};
use crate::shared::progress::{ProgressState, ProgressUi};
use crate::shared::types::{
    ContextResult, IndexPhase, IndexProgress, IndexState, OutputFormat, SearchResult, SegmentRole,
    StructuralResult, SymbolResult,
};
use crate::shared::update::{InstallChannel, UpdateStatus};

pub trait Formatter {
    fn format_search_results(&self, results: &[SearchResult]) -> String;
    fn format_symbol_results(&self, results: &[SymbolResult]) -> String;
    fn format_context_result(&self, result: &ContextResult) -> String;
    fn format_structural_results(&self, results: &[StructuralResult]) -> String;
    fn format_impact_result(&self, result: &ImpactResultEnvelope) -> String;
    fn format_message(&self, message: &str) -> String;
    fn format_index_summary(&self, message: &str, progress: &IndexProgress) -> String;
    fn format_index_watch_update(&self, progress: &IndexProgress) -> String;
    fn format_status(&self, status: &StatusInfo) -> String;
    fn format_update_status(&self, _status: &UpdateStatusInfo) -> String {
        String::new()
    }
    fn format_update_result(&self, _result: &UpdateResult) -> String {
        String::new()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusInfo {
    pub daemon_running: bool,
    pub pid: Option<u32>,
    pub project_initialized: bool,
    pub indexed_files: Option<u64>,
    pub total_segments: Option<u64>,
    pub project_id: Option<String>,
    pub index_present: bool,
    pub index_readable: bool,
    pub last_file_check_at: Option<DateTime<Utc>>,
    pub index_progress: Option<IndexProgress>,
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
    fn format_search_results(&self, results: &[SearchResult]) -> String {
        to_json(results)
    }

    fn format_symbol_results(&self, results: &[SymbolResult]) -> String {
        to_json(results)
    }

    fn format_context_result(&self, result: &ContextResult) -> String {
        to_json(result)
    }

    fn format_structural_results(&self, results: &[StructuralResult]) -> String {
        to_json(results)
    }

    fn format_impact_result(&self, result: &ImpactResultEnvelope) -> String {
        to_json(result)
    }

    fn format_message(&self, message: &str) -> String {
        to_json(&serde_json::json!({ "message": message }))
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
            "daemon_running": status.daemon_running,
            "pid": status.pid,
            "project_initialized": status.project_initialized,
            "indexed_files": status.indexed_files,
            "total_segments": status.total_segments,
            "project_id": &status.project_id,
            "index_status": render_index_health_plain(status),
            "last_file_check_at": status.last_file_check_at,
            "index_progress": &status.index_progress,
            "index_work": index_work,
        }))
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
    fn format_search_results(&self, results: &[SearchResult]) -> String {
        if results.is_empty() {
            return "No results found.".to_string();
        }
        let mut out = String::new();
        for (i, r) in results.iter().enumerate() {
            if i > 0 {
                out.push_str("\n----------------------------------------\n\n");
            }
            out.push_str(&format!(
                "{} {}\n",
                r.display_kind().bold(),
                format!("{}:{}", r.file_path, r.line_number).cyan(),
            ));
            out.push_str(&format!("{}\n\n", render_search_metadata(r).dimmed()));
            for line in r.content.lines().take(12) {
                out.push_str(&format!("  {line}\n"));
            }
            if r.content.lines().count() > 12 {
                out.push_str(&format!("  {}\n", "...".dimmed()));
            }
        }
        out
    }

    fn format_symbol_results(&self, results: &[SymbolResult]) -> String {
        if results.is_empty() {
            return "No symbols found.".to_string();
        }
        let mut out = String::new();
        for (i, r) in results.iter().enumerate() {
            if i > 0 {
                out.push_str("\n----------------------------------------\n\n");
            }
            out.push_str(&format!(
                "{} {} {}\n",
                r.reference_kind.to_string().to_uppercase().bold(),
                r.name.bold(),
                format!("{}:{}", r.file_path, r.line_start).cyan(),
            ));
            out.push_str(&format!("{}\n\n", render_symbol_metadata(r).dimmed()));
            for line in r.content.lines().take(12) {
                out.push_str(&format!("  {line}\n"));
            }
            if r.content.lines().count() > 12 {
                out.push_str(&format!("  {}\n", "...".dimmed()));
            }
        }
        out
    }

    fn format_context_result(&self, result: &ContextResult) -> String {
        let mut out = String::new();
        let access_scope = result
            .access_scope
            .map(|scope| format!(" {}", format!("[{}]", scope.as_str()).dimmed()))
            .unwrap_or_default();
        out.push_str(&format!(
            "{} {}{} (lines {}-{})\n\n",
            result.file_path.cyan(),
            format!("[{}]", result.scope_type).dimmed(),
            access_scope,
            result.line_start,
            result.line_end,
        ));
        for (i, line) in result.content.lines().enumerate() {
            let line_num = result.line_start + i;
            out.push_str(&format!("{} {line}\n", format!("{line_num:>4} |").dimmed()));
        }
        out
    }

    fn format_structural_results(&self, results: &[StructuralResult]) -> String {
        if results.is_empty() {
            return "No structural matches found.".to_string();
        }
        let mut out = String::new();
        for (i, r) in results.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            let label = r.pattern_name.as_deref().unwrap_or("match");
            out.push_str(&format!(
                "{} {} {}\n",
                format!("{}:{}-{}", r.file_path, r.line_start, r.line_end).cyan(),
                format!("[{}]", label).dimmed(),
                format!("({})", r.language).dimmed(),
            ));
            for line in r.content.lines().take(10) {
                out.push_str(&format!("  {line}\n"));
            }
            if r.content.lines().count() > 10 {
                out.push_str(&format!("  {}\n", "...".dimmed()));
            }
        }
        out
    }

    fn format_impact_result(&self, result: &ImpactResultEnvelope) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "{} {}\n",
            "Likely Impact".bold().underline(),
            format!("[{}]", render_impact_status_label(result.status)).dimmed()
        ));

        if let Some(anchor) = &result.resolved_anchor {
            out.push_str(&format!(
                "Resolved: {}\n",
                render_impact_anchor_head(anchor).cyan()
            ));
            if let Some(context) = render_impact_anchor_context_human(anchor) {
                out.push_str(&format!("{}\n", format!("Context: {context}").dimmed()));
            }
        }

        if let Some(refusal) = &result.refusal {
            out.push_str(&format!(
                "Refusal: {} {}\n",
                refusal.reason.yellow().bold(),
                refusal.message
            ));
        } else if result.results.is_empty() {
            out.push_str("No likely-impact candidates found.\n");
        } else {
            for (i, candidate) in result.results.iter().enumerate() {
                if i > 0 {
                    out.push('\n');
                }
                out.push_str(&format!(
                    "{}. {} {}\n",
                    i + 1,
                    format!(
                        "{}:{}-{}",
                        candidate.file_path, candidate.line_start, candidate.line_end
                    )
                    .cyan(),
                    format!("[{}]", candidate.block_type).dimmed()
                ));
                out.push_str(&format!(
                    "{}\n",
                    render_impact_candidate_metadata(candidate).dimmed()
                ));
                if let Some(breadcrumb) = &candidate.breadcrumb {
                    out.push_str(&format!(
                        "{}\n",
                        format!("Breadcrumb: {breadcrumb}").dimmed()
                    ));
                }
                if !candidate.reasons.is_empty() {
                    out.push_str(&format!(
                        "{}\n",
                        format!("Why: {}", render_impact_reasons_human(&candidate.reasons))
                            .dimmed()
                    ));
                }
            }
        }

        if let Some(hint) = &result.hint {
            out.push('\n');
            out.push_str(&format!(
                "Next step {} {}\n",
                format!("[{}]", hint.code).dimmed(),
                hint.message
            ));
            if let Some(scope) = &hint.suggested_scope {
                out.push_str(&format!("Suggested scope: {}\n", scope.cyan()));
            }
            if let Some(segment_id) = &hint.suggested_segment_id {
                out.push_str(&format!("Suggested segment: {}\n", segment_id.cyan()));
            }
        }

        out
    }

    fn format_message(&self, message: &str) -> String {
        message.to_string()
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
            out.push_str(&format!(
                "Timings: scan {} | parse {} | embed {} | store {} | total {}\n",
                render_duration_ms(timings.scan_ms),
                render_duration_ms(timings.parse_ms),
                render_duration_ms(timings.embed_ms),
                render_duration_ms(timings.store_ms),
                render_duration_ms(timings.total_ms),
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
        let state = if status.daemon_running {
            "running".green().to_string()
        } else {
            "stopped".red().to_string()
        };
        out.push_str(&format!("Daemon: {state}\n"));
        if let Some(pid) = status.pid {
            out.push_str(&format!("PID: {pid}\n"));
        }
        if let Some(id) = &status.project_id {
            out.push_str(&format!("Project: {id}\n"));
        } else if status.project_initialized {
            out.push_str("Project: initialized\n");
        } else {
            out.push_str(&format!("Project: {}\n", "not initialized".yellow()));
        }
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
                out.push_str(&format!(
                    "Timings: scan {} | parse {} | embed {} | store {} | total {}\n",
                    render_duration_ms(timings.scan_ms),
                    render_duration_ms(timings.parse_ms),
                    render_duration_ms(timings.embed_ms),
                    render_duration_ms(timings.store_ms),
                    render_duration_ms(timings.total_ms),
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
    fn format_search_results(&self, results: &[SearchResult]) -> String {
        if results.is_empty() {
            return "No results found.".to_string();
        }
        let mut out = String::new();
        for r in results {
            out.push_str(&format!(
                "{}:{}-{}\t{}\t{:.4}",
                r.file_path, r.line_number, r.line_end, r.block_type, r.score
            ));
            if let Some(segment_id) = r.segment_id.as_deref() {
                out.push_str(&format!("\tsegment={segment_id}"));
            }
            out.push('\n');
            out.push_str(&format!("{}\n", render_search_metadata(r)));
            out.push_str(&r.content);
            if !r.content.ends_with('\n') {
                out.push('\n');
            }
            out.push('\n');
        }
        out
    }

    fn format_symbol_results(&self, results: &[SymbolResult]) -> String {
        if results.is_empty() {
            return "No symbols found.".to_string();
        }
        let mut out = String::new();
        for r in results {
            out.push_str(&format!(
                "{}\t{}\t{}\t{}:{}-{}\n",
                r.name, r.reference_kind, r.kind, r.file_path, r.line_start, r.line_end
            ));
            out.push_str(&format!("{}\n", render_symbol_metadata(r)));
        }
        out
    }

    fn format_context_result(&self, result: &ContextResult) -> String {
        let mut out = String::new();
        match result.access_scope {
            Some(scope) => out.push_str(&format!(
                "{}:{}-{}\t{}\t{}\n",
                result.file_path,
                result.line_start,
                result.line_end,
                result.scope_type,
                scope.as_str()
            )),
            None => out.push_str(&format!(
                "{}:{}-{}\t{}\n",
                result.file_path, result.line_start, result.line_end, result.scope_type
            )),
        }
        out.push_str(&result.content);
        if !result.content.ends_with('\n') {
            out.push('\n');
        }
        out
    }

    fn format_structural_results(&self, results: &[StructuralResult]) -> String {
        if results.is_empty() {
            return "No structural matches found.".to_string();
        }
        let mut out = String::new();
        for r in results {
            let label = r.pattern_name.as_deref().unwrap_or("match");
            out.push_str(&format!(
                "{}:{}-{}\t{}\t{}\n",
                r.file_path, r.line_start, r.line_end, label, r.language
            ));
            out.push_str(&r.content);
            if !r.content.ends_with('\n') {
                out.push('\n');
            }
            out.push('\n');
        }
        out
    }

    fn format_impact_result(&self, result: &ImpactResultEnvelope) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "status\t{}\n",
            render_impact_status_label(result.status)
        ));

        if let Some(anchor) = &result.resolved_anchor {
            out.push_str(&format!("anchor\t{}\t{}\n", anchor.kind, anchor.value));
            for line in render_impact_anchor_context_plain(anchor) {
                out.push_str(&line);
                out.push('\n');
            }
        }

        if let Some(refusal) = &result.refusal {
            out.push_str(&format!(
                "refusal\t{}\t{}\n",
                refusal.reason, refusal.message
            ));
        }

        if let Some(hint) = &result.hint {
            out.push_str(&format!("hint\t{}\t{}\n", hint.code, hint.message));
            if let Some(scope) = &hint.suggested_scope {
                out.push_str(&format!("hint_scope\t{scope}\n"));
            }
            if let Some(segment_id) = &hint.suggested_segment_id {
                out.push_str(&format!("hint_segment\t{segment_id}\n"));
            }
        }

        for (i, candidate) in result.results.iter().enumerate() {
            out.push_str(&format!(
                "result\t{}\t{}:{}-{}\t{}\tlang={}\tscore={:.4}\thop={}\tsegment={}\n",
                i + 1,
                candidate.file_path,
                candidate.line_start,
                candidate.line_end,
                candidate.block_type,
                candidate.language,
                candidate.score,
                candidate.hop,
                candidate.segment_id
            ));
            out.push_str(&format!(
                "result_meta\t{}\n",
                render_impact_candidate_metadata(candidate)
            ));
            if let Some(breadcrumb) = &candidate.breadcrumb {
                out.push_str(&format!("result_breadcrumb\t{}\n", breadcrumb));
            }
            if !candidate.reasons.is_empty() {
                for reason in &candidate.reasons {
                    out.push_str(&format!(
                        "result_reason\t{}\t{}\n",
                        i + 1,
                        render_impact_reason_plain(reason)
                    ));
                }
            }
        }

        out
    }

    fn format_message(&self, message: &str) -> String {
        message.to_string()
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
        }
        out.push_str(&format!("\tupdated:{}\n", progress.updated_at.to_rfc3339()));
        out
    }

    fn format_status(&self, status: &StatusInfo) -> String {
        let state = if status.daemon_running {
            "running"
        } else {
            "stopped"
        };
        let mut out = format!("daemon:{state}");
        if let Some(pid) = status.pid {
            out.push_str(&format!("\tpid:{pid}"));
        }
        out.push_str(&format!(
            "\tproject_initialized:{}",
            status.project_initialized
        ));
        if let Some(id) = &status.project_id {
            out.push_str(&format!("\tproject:{id}"));
        }
        out.push_str(&format!("\tindex:{}", render_index_health_plain(status)));
        match &status.last_file_check_at {
            Some(last_file_check_at) => out.push_str(&format!(
                "\tlast_file_check:{}\tlast_file_check_ago:{}",
                last_file_check_at.to_rfc3339(),
                render_time_ago(last_file_check_at)
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
                out.push_str(&format!("\tindex_message:{message}"));
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
            }
            out.push_str(&format!(
                "\tupdated:{}\tago:{}",
                progress.updated_at.to_rfc3339(),
                render_time_ago(&progress.updated_at)
            ));
        }
        out.push('\n');
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

fn render_index_health_human(status: &StatusInfo) -> colored::ColoredString {
    match render_index_health_plain(status) {
        "ready" => "ready".green(),
        "not_built" => "not built".yellow(),
        _ => "unavailable".red(),
    }
}

fn render_index_health_plain(status: &StatusInfo) -> &'static str {
    if !status.index_present {
        "not_built"
    } else if status.index_readable {
        "ready"
    } else {
        "unavailable"
    }
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

fn render_search_metadata(result: &SearchResult) -> String {
    let mut parts = vec![format!("Kind: {}", result.block_type)];

    if let Some(breadcrumb) = &result.breadcrumb {
        parts.push(format!("Scope: {breadcrumb}"));
    }
    if let Some(defined) = &result.defined_symbols {
        if !defined.is_empty() {
            parts.push(format!("Defines: {}", truncate_items(defined, 5)));
        }
    }
    if let Some(calls) = &result.called_symbols {
        if !calls.is_empty() {
            parts.push(format!("Calls: {}", truncate_items(calls, 5)));
        }
    }
    if let Some(complexity) = result.complexity {
        if complexity > 0 {
            parts.push(format!("Complexity: {complexity}"));
        }
    }

    parts.push(format!("Score: {:.4}", result.score));
    parts.join(" | ")
}

fn render_symbol_metadata(result: &SymbolResult) -> String {
    let mut parts = vec![format!("Kind: {}", result.kind)];

    if let Some(breadcrumb) = &result.breadcrumb {
        parts.push(format!("Scope: {breadcrumb}"));
    }
    if let Some(defined) = &result.defined_symbols {
        if !defined.is_empty() {
            parts.push(format!("Defines: {}", truncate_items(defined, 5)));
        }
    }
    if let Some(calls) = &result.called_symbols {
        if !calls.is_empty() {
            parts.push(format!("Calls: {}", truncate_items(calls, 5)));
        }
    }
    if let Some(complexity) = result.complexity {
        if complexity > 0 {
            parts.push(format!("Complexity: {complexity}"));
        }
    }

    parts.join(" | ")
}

fn render_impact_status_label(status: ImpactStatus) -> &'static str {
    match status {
        ImpactStatus::Expanded => "expanded",
        ImpactStatus::ExpandedScoped => "expanded_scoped",
        ImpactStatus::Empty => "empty",
        ImpactStatus::EmptyScoped => "empty_scoped",
        ImpactStatus::Refused => "refused",
    }
}

fn render_impact_anchor_head(anchor: &ResolvedImpactAnchor) -> String {
    format!("{} {}", anchor.kind, anchor.value)
}

fn render_impact_anchor_context_human(anchor: &ResolvedImpactAnchor) -> Option<String> {
    let mut parts = Vec::new();

    if let Some(line) = anchor.line {
        parts.push(format!("line {line}"));
    }
    if let Some(scope) = &anchor.scope {
        parts.push(format!("scope {scope}"));
    }
    if !anchor.matched_files.is_empty() {
        parts.push(format!(
            "matched {}",
            truncate_items(&anchor.matched_files, 3)
        ));
    }
    if !anchor.seed_segment_ids.is_empty() {
        let seeds = anchor
            .seed_segment_ids
            .iter()
            .map(|segment_id| short_segment_id(segment_id))
            .collect::<Vec<_>>()
            .join(", ");
        parts.push(format!("seed segments {seeds}"));
    }

    (!parts.is_empty()).then(|| parts.join(" | "))
}

fn render_impact_anchor_context_plain(anchor: &ResolvedImpactAnchor) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(line) = anchor.line {
        lines.push(format!("anchor_line\t{line}"));
    }
    if let Some(scope) = &anchor.scope {
        lines.push(format!("anchor_scope\t{scope}"));
    }
    for matched_file in &anchor.matched_files {
        lines.push(format!("anchor_match\t{matched_file}"));
    }
    for seed_segment_id in &anchor.seed_segment_ids {
        lines.push(format!("anchor_seed_segment\t{seed_segment_id}"));
    }

    lines
}

fn render_impact_candidate_metadata(candidate: &ImpactCandidate) -> String {
    let mut parts = vec![
        format!("Language: {}", candidate.language),
        format!("Score: {:.4}", candidate.score),
        format!("Hop: {}", candidate.hop),
        format!("Segment: {}", candidate.segment_id),
    ];

    if let Some(role) = candidate.role {
        parts.push(format!("Role: {}", render_segment_role(role)));
    }
    if let Some(complexity) = candidate.complexity {
        if complexity > 0 {
            parts.push(format!("Complexity: {complexity}"));
        }
    }
    if let Some(defined_symbols) = &candidate.defined_symbols {
        if !defined_symbols.is_empty() {
            parts.push(format!("Defines: {}", truncate_items(defined_symbols, 5)));
        }
    }

    parts.join(" | ")
}

fn render_impact_reasons_human(reasons: &[ImpactReason]) -> String {
    reasons
        .iter()
        .map(render_impact_reason_human)
        .collect::<Vec<_>>()
        .join("; ")
}

fn render_impact_reason_human(reason: &ImpactReason) -> String {
    let mut label = reason.kind.clone();

    if let Some(symbol) = &reason.symbol {
        label.push('(');
        label.push_str(symbol);
        label.push(')');
    }

    if let Some(from_segment_id) = &reason.from_segment_id {
        label.push_str(" from ");
        label.push_str(&short_segment_id(from_segment_id));
    }

    label
}

fn render_impact_reason_plain(reason: &ImpactReason) -> String {
    let mut parts = vec![format!("kind={}", reason.kind)];

    if let Some(symbol) = &reason.symbol {
        parts.push(format!("symbol={symbol}"));
    }
    if let Some(from_segment_id) = &reason.from_segment_id {
        parts.push(format!("from_segment={from_segment_id}"));
    }

    parts.join("\t")
}

fn render_segment_role(role: SegmentRole) -> &'static str {
    match role {
        SegmentRole::Definition => "definition",
        SegmentRole::Implementation => "implementation",
        SegmentRole::Orchestration => "orchestration",
        SegmentRole::Import => "import",
        SegmentRole::Docs => "docs",
    }
}

fn short_segment_id(segment_id: &str) -> String {
    segment_id.chars().take(12).collect()
}

fn truncate_items(items: &[String], limit: usize) -> String {
    if items.len() <= limit {
        return items.join(", ");
    }

    let mut preview = items.iter().take(limit).cloned().collect::<Vec<_>>();
    preview.push(format!("+{}", items.len() - limit));
    preview.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::types::{IndexParallelism, IndexStageTimings};

    fn sample_search_result() -> SearchResult {
        SearchResult {
            file_path: "src/auth/builder.rs".to_string(),
            language: "rust".to_string(),
            block_type: "function".to_string(),
            content: "fn build_auth() {\n    apply_config();\n}".to_string(),
            score: 0.945,
            line_number: 21,
            line_end: 23,
            segment_id: Some("candidate-segment-abcdef123456".to_string()),
            breadcrumb: Some("AuthConfig::build".to_string()),
            complexity: Some(4),
            role: Some(SegmentRole::Orchestration),
            defined_symbols: Some(vec!["build_auth".to_string()]),
            referenced_symbols: None,
            called_symbols: Some(vec!["apply_config".to_string()]),
        }
    }

    fn sample_impact_result() -> ImpactResultEnvelope {
        ImpactResultEnvelope {
            status: ImpactStatus::ExpandedScoped,
            resolved_anchor: Some(ResolvedImpactAnchor {
                kind: "symbol".to_string(),
                value: "Config".to_string(),
                line: Some(14),
                scope: Some("src/auth".to_string()),
                seed_segment_ids: vec!["seed-segment-1234567890".to_string()],
                matched_files: vec![
                    "src/auth/config.rs".to_string(),
                    "src/auth/mod.rs".to_string(),
                ],
            }),
            results: vec![ImpactCandidate {
                segment_id: "candidate-segment-abcdef123456".to_string(),
                file_path: "src/auth/builder.rs".to_string(),
                language: "rust".to_string(),
                block_type: "function".to_string(),
                line_start: 21,
                line_end: 38,
                score: 0.945,
                hop: 1,
                reasons: vec![
                    ImpactReason {
                        kind: "called_by".to_string(),
                        symbol: Some("Config".to_string()),
                        from_segment_id: Some("seed-segment-1234567890".to_string()),
                    },
                    ImpactReason {
                        kind: "same_file".to_string(),
                        symbol: None,
                        from_segment_id: None,
                    },
                ],
                breadcrumb: Some("AuthConfig::build".to_string()),
                complexity: Some(4),
                role: Some(SegmentRole::Orchestration),
                defined_symbols: Some(vec!["build_auth".to_string()]),
            }],
            contextual_results: None,
            hint: Some(crate::search::impact::ImpactHint {
                code: "inspect_candidate".to_string(),
                message: "Inspect `src/auth/builder.rs` next.".to_string(),
                suggested_scope: Some("src/auth".to_string()),
                suggested_segment_id: Some("candidate-segment-abcdef123456".to_string()),
            }),
            refusal: None,
        }
    }

    fn sample_refused_impact_result() -> ImpactResultEnvelope {
        ImpactResultEnvelope {
            status: ImpactStatus::Refused,
            resolved_anchor: None,
            results: Vec::new(),
            contextual_results: None,
            hint: Some(crate::search::impact::ImpactHint {
                code: "narrow_with_scope".to_string(),
                message: "Pass `--scope src/auth` or reuse an exact segment anchor.".to_string(),
                suggested_scope: Some("src/auth".to_string()),
                suggested_segment_id: Some("seed-segment-1234567890".to_string()),
            }),
            refusal: Some(crate::search::impact::ImpactRefusal {
                reason: "symbol_too_broad".to_string(),
                message: "Symbol `Config` matched too many unrelated definitions.".to_string(),
            }),
        }
    }

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
            }),
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
    fn json_search_results_include_segment_id_when_available() {
        let formatter = JsonFormatter;
        let rendered = formatter.format_search_results(&[sample_search_result()]);
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();

        assert_eq!(
            value[0]["segment_id"],
            serde_json::Value::String("candidate-segment-abcdef123456".to_string())
        );
    }

    #[test]
    fn plain_search_results_append_segment_field() {
        let formatter = PlainFormatter;
        let rendered = formatter.format_search_results(&[sample_search_result()]);
        let first_line = rendered.lines().next().unwrap();

        assert_eq!(
            first_line,
            "src/auth/builder.rs:21-23\tfunction\t0.9450\tsegment=candidate-segment-abcdef123456"
        );
    }

    #[test]
    fn plain_search_results_preserve_legacy_shape_without_segment_field() {
        let formatter = PlainFormatter;
        let mut result = sample_search_result();
        result.segment_id = None;

        let rendered = formatter.format_search_results(&[result]);
        let first_line = rendered.lines().next().unwrap();

        assert_eq!(first_line, "src/auth/builder.rs:21-23\tfunction\t0.9450");
    }

    #[test]
    fn human_search_results_remain_concise_without_full_segment_id() {
        let formatter = HumanFormatter;
        let rendered = formatter.format_search_results(&[sample_search_result()]);

        assert!(!rendered.contains("candidate-segment-abcdef123456"));
        assert!(rendered.contains("src/auth/builder.rs:21"));
        assert!(rendered.contains("Kind: function | Scope: AuthConfig::build"));
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
    fn json_impact_result_keeps_nested_expanded_fields() {
        let formatter = JsonFormatter;
        let rendered = formatter.format_impact_result(&sample_impact_result());
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();

        assert_eq!(value["status"], "expanded_scoped");
        assert_eq!(value["resolved_anchor"]["scope"], "src/auth");
        assert!(value["resolved_anchor"]["seed_segment_ids"].is_array());
        assert_eq!(
            value["results"][0]["reasons"][0]["from_segment_id"],
            "seed-segment-1234567890"
        );
        assert_eq!(
            value["hint"]["suggested_segment_id"],
            "candidate-segment-abcdef123456"
        );
    }

    #[test]
    fn json_impact_refusal_keeps_structured_hint_and_refusal() {
        let formatter = JsonFormatter;
        let rendered = formatter.format_impact_result(&sample_refused_impact_result());
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();

        assert_eq!(value["status"], "refused");
        assert_eq!(value["refusal"]["reason"], "symbol_too_broad");
        assert_eq!(value["hint"]["code"], "narrow_with_scope");
        assert_eq!(value["hint"]["suggested_scope"], "src/auth");
    }

    #[test]
    fn human_impact_result_renders_context_breadcrumb_and_next_step() {
        let formatter = HumanFormatter;
        let rendered = formatter.format_impact_result(&sample_impact_result());

        assert!(rendered.contains("Likely Impact"));
        assert!(rendered.contains("Resolved: symbol Config"));
        assert!(rendered.contains("Context: line 14 | scope src/auth"));
        assert!(rendered.contains("Language: rust"));
        assert!(rendered.contains("Breadcrumb: AuthConfig::build"));
        assert!(rendered.contains("Why: called_by(Config) from seed-segment; same_file"));
        assert!(
            rendered.contains("Next step [inspect_candidate] Inspect `src/auth/builder.rs` next.")
        );
        assert!(rendered.contains("Suggested segment: candidate-segment-abcdef123456"));
    }

    #[test]
    fn human_impact_refusal_renders_actionable_guidance() {
        let formatter = HumanFormatter;
        let rendered = formatter.format_impact_result(&sample_refused_impact_result());

        assert!(rendered.contains("Refusal: symbol_too_broad"));
        assert!(rendered.contains("Next step [narrow_with_scope]"));
        assert!(rendered.contains("Suggested scope: src/auth"));
        assert!(rendered.contains("Suggested segment: seed-segment-1234567890"));
    }

    #[test]
    fn plain_impact_result_renders_full_anchor_context_and_reason_fields() {
        let formatter = PlainFormatter;
        let rendered = formatter.format_impact_result(&sample_impact_result());

        assert!(rendered.contains("status\texpanded_scoped"));
        assert!(rendered.contains("anchor\tsymbol\tConfig"));
        assert!(rendered.contains("anchor_line\t14"));
        assert!(rendered.contains("anchor_scope\tsrc/auth"));
        assert!(rendered.contains("anchor_match\tsrc/auth/config.rs"));
        assert!(rendered.contains("anchor_seed_segment\tseed-segment-1234567890"));
        assert!(rendered.contains(
            "result\t1\tsrc/auth/builder.rs:21-38\tfunction\tlang=rust\tscore=0.9450\thop=1\tsegment=candidate-segment-abcdef123456"
        ));
        assert!(rendered.contains("result_breadcrumb\tAuthConfig::build"));
        assert!(rendered.contains(
            "result_reason\t1\tkind=called_by\tsymbol=Config\tfrom_segment=seed-segment-1234567890"
        ));
        assert!(rendered.contains("result_reason\t1\tkind=same_file"));
    }

    #[test]
    fn plain_impact_refusal_renders_structured_guidance() {
        let formatter = PlainFormatter;
        let rendered = formatter.format_impact_result(&sample_refused_impact_result());

        assert!(rendered.contains("status\trefused"));
        assert!(rendered.contains(
            "refusal\tsymbol_too_broad\tSymbol `Config` matched too many unrelated definitions."
        ));
        assert!(rendered.contains(
            "hint\tnarrow_with_scope\tPass `--scope src/auth` or reuse an exact segment anchor."
        ));
        assert!(rendered.contains("hint_scope\tsrc/auth"));
        assert!(rendered.contains("hint_segment\tseed-segment-1234567890"));
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
            daemon_running: true,
            pid: Some(42),
            project_initialized: true,
            indexed_files: Some(3),
            total_segments: Some(14),
            project_id: Some("project-123".to_string()),
            index_present: true,
            index_readable: true,
            last_file_check_at: Some(sample_progress().updated_at),
            index_progress: Some(sample_progress()),
        });

        assert!(rendered.contains("last_completed:4"));
        assert!(rendered.contains("last_processed:6"));
        assert!(rendered.contains("index_message:Processed 6 files"));
        assert!(rendered.contains("jobs_effective:3"));
        assert!(rendered.contains("total_ms:41"));
    }

    #[test]
    fn human_status_reports_uninitialized_project_and_missing_index() {
        let formatter = HumanFormatter;
        let rendered = formatter.format_status(&StatusInfo {
            daemon_running: false,
            pid: None,
            project_initialized: false,
            indexed_files: None,
            total_segments: None,
            project_id: None,
            index_present: false,
            index_readable: false,
            last_file_check_at: None,
            index_progress: None,
        });

        assert!(rendered.contains("Project: not initialized"));
        assert!(rendered.contains("Index: not built"));
        assert!(rendered.contains("Last file check: never recorded"));
    }

    #[test]
    fn plain_status_reports_uninitialized_project_and_missing_index() {
        let formatter = PlainFormatter;
        let rendered = formatter.format_status(&StatusInfo {
            daemon_running: false,
            pid: None,
            project_initialized: false,
            indexed_files: None,
            total_segments: None,
            project_id: None,
            index_present: false,
            index_readable: false,
            last_file_check_at: None,
            index_progress: None,
        });

        assert!(rendered.contains("project_initialized:false"));
        assert!(rendered.contains("index:not_built"));
        assert!(rendered.contains("last_file_check:none"));
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
