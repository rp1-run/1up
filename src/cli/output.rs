use colored::Colorize;
use serde::Serialize;

use crate::shared::types::{
    ContextResult, OutputFormat, SearchResult, StructuralResult, SymbolResult,
};

pub trait Formatter {
    fn format_search_results(&self, results: &[SearchResult]) -> String;
    fn format_symbol_results(&self, results: &[SymbolResult]) -> String;
    fn format_context_result(&self, result: &ContextResult) -> String;
    fn format_structural_results(&self, results: &[StructuralResult]) -> String;
    fn format_message(&self, message: &str) -> String;
    fn format_status(&self, status: &StatusInfo) -> String;
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusInfo {
    pub daemon_running: bool,
    pub pid: Option<u32>,
    pub indexed_files: Option<u64>,
    pub total_segments: Option<u64>,
    pub project_id: Option<String>,
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
                out.push('\n');
            }
            out.push_str(&format!(
                "{} {} (score: {:.4})\n",
                format!("{}:{}", r.file_path, r.line_number).cyan(),
                format!("[{}]", r.block_type).dimmed(),
                r.score
            ));
            for line in r.content.lines().take(5) {
                out.push_str(&format!("  {line}\n"));
            }
            if r.content.lines().count() > 5 {
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
                out.push('\n');
            }
            let kind_label = format!("[{}: {}]", r.reference_kind, r.kind);
            out.push_str(&format!(
                "{} {} {}\n",
                r.name.bold(),
                kind_label.dimmed(),
                format!("{}:{}-{}", r.file_path, r.line_start, r.line_end).cyan(),
            ));
            for line in r.content.lines().take(5) {
                out.push_str(&format!("  {line}\n"));
            }
            if r.content.lines().count() > 5 {
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
                "{}:{}\t{}\t{:.4}\n",
                r.file_path, r.line_number, r.block_type, r.score
            ));
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
        out.push('\n');
        out
    }
}

fn to_json<T: Serialize + ?Sized>(value: &T) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"))
}
