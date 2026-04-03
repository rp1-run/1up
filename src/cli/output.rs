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
    pub indexed_files: Option<u64>,
    pub total_segments: Option<u64>,
    pub project_id: Option<String>,
    pub index_progress: Option<IndexProgress>,
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
        to_json(&serde_json::json!({
            "message": message,
            "progress": progress,
        }))
    }

    fn format_status(&self, status: &StatusInfo) -> String {
        to_json(status)
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
        out.push_str(&format!(
            "{} {} (lines {}-{})\n\n",
            result.file_path.cyan(),
            format!("[{}]", result.scope_type).dimmed(),
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
        }
        if let Some(files) = status.indexed_files {
            out.push_str(&format!("Indexed files: {files}\n"));
        }
        if let Some(segs) = status.total_segments {
            out.push_str(&format!("Total segments: {segs}\n"));
        }
        if let Some(progress) = &status.index_progress {
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
                "Embeddings: {}\n",
                render_embeddings_human(progress.embeddings_enabled)
            ));
            out.push_str(&format!("Updated: {}\n", progress.updated_at.to_rfc3339()));
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
        out.push_str(&format!(
            "{}:{}-{}\t{}\n",
            result.file_path, result.line_start, result.line_end, result.scope_type
        ));
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
        out.push_str(message);
        out.push('\n');
        out.push_str(&format!(
            "index_state:{}\tindex_phase:{}\tfiles_scanned:{}\tfiles_total:{}\tfiles_indexed:{}\tfiles_skipped:{}\tfiles_deleted:{}\tsegments_stored:{}\tembeddings:{}\tupdated:{}\n",
            render_index_state_plain(progress.state),
            render_index_phase_plain(progress),
            progress.files_scanned,
            progress.files_total,
            progress.files_indexed,
            progress.files_skipped,
            progress.files_deleted,
            progress.segments_stored,
            render_embeddings_plain(progress.embeddings_enabled),
            progress.updated_at.to_rfc3339(),
        ));
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
        if let Some(id) = &status.project_id {
            out.push_str(&format!("\tproject:{id}"));
        }
        if let Some(files) = status.indexed_files {
            out.push_str(&format!("\tfiles:{files}"));
        }
        if let Some(segs) = status.total_segments {
            out.push_str(&format!("\tsegments:{segs}"));
        }
        if let Some(progress) = &status.index_progress {
            out.push_str(&format!(
                "\tindex_state:{}\tindex_phase:{}\tlast_scanned:{}\tlast_total:{}\tlast_indexed:{}\tlast_skipped:{}\tlast_deleted:{}\tlast_segments:{}\tembeddings:{}\tupdated:{}",
                render_index_state_plain(progress.state),
                render_index_phase_plain(progress),
                progress.files_scanned,
                progress.files_total,
                progress.files_indexed,
                progress.files_skipped,
                progress.files_deleted,
                progress.segments_stored,
                render_embeddings_plain(progress.embeddings_enabled),
                progress.updated_at.to_rfc3339(),
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
