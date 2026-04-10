use chrono::{DateTime, Utc};
use colored::Colorize;
use serde::Serialize;

use crate::shared::types::{
    ContextResult, IndexPhase, IndexProgress, IndexState, OutputFormat, SearchResult,
    StructuralResult, SymbolResult,
};

pub trait Formatter {
    fn format_search_results(&self, results: &[SearchResult]) -> String;
    fn format_symbol_results(&self, results: &[SymbolResult]) -> String;
    fn format_context_result(&self, result: &ContextResult) -> String;
    fn format_structural_results(&self, results: &[StructuralResult]) -> String;
    fn format_message(&self, message: &str) -> String;
    fn format_index_summary(&self, message: &str, progress: &IndexProgress) -> String;
    fn format_status(&self, status: &StatusInfo) -> String;
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

struct JsonFormatter;
struct HumanFormatter;
struct PlainFormatter;

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
}

impl Formatter for PlainFormatter {
    fn format_search_results(&self, results: &[SearchResult]) -> String {
        if results.is_empty() {
            return "No results found.".to_string();
        }
        let mut out = String::new();
        for r in results {
            out.push_str(&format!(
                "{}:{}-{}\t{}\t{:.4}\n",
                r.file_path, r.line_number, r.line_end, r.block_type, r.score
            ));
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
        IndexPhase::Scanning => "scanning",
        IndexPhase::Parsing => "parsing",
        IndexPhase::Storing if progress.embeddings_enabled => "embedding_and_storing",
        IndexPhase::Storing => "storing",
        IndexPhase::Complete => "complete",
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

    fn sample_progress() -> IndexProgress {
        IndexProgress {
            state: IndexState::Complete,
            phase: IndexPhase::Complete,
            files_total: 6,
            files_scanned: 6,
            files_indexed: 3,
            files_skipped: 2,
            files_deleted: 1,
            segments_stored: 14,
            embeddings_enabled: true,
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
}
