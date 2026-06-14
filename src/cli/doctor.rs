use std::io::ErrorKind;
use std::path::Path;

use clap::Args;

use crate::cli::hint_cleanup::{classify, FileReport};
use crate::cli::output::{
    formatter_for, DoctorFileReport, DoctorFileStatus, DoctorReport, StaleToken,
};
use crate::shared::fs::atomic_replace_within_project_root;
use crate::shared::types::OutputFormat;

/// User project instruction files, relative to the project root, that legacy 1up
/// hints may have been pasted into. Stored as forward-slash literals so report
/// output stays stable across platforms.
const IN_SCOPE_FILES: [&str; 3] = ["AGENTS.md", "CLAUDE.md", ".github/copilot-instructions.md"];

#[derive(Args)]
pub struct DoctorArgs {
    /// Project root directory (defaults to current directory)
    #[arg(default_value = ".")]
    pub path: String,

    /// Scan project instruction files for legacy 1up code-discovery hints
    #[arg(long)]
    pub clean_hints: bool,

    /// Apply the fence-removal edit (default is a read-only preview)
    #[arg(long)]
    pub apply: bool,

    /// Output format override (defaults to human)
    #[arg(long, short = 'f')]
    pub format: Option<OutputFormat>,
}

pub async fn exec(args: DoctorArgs, format: OutputFormat) -> anyhow::Result<()> {
    let fmt = formatter_for(format);

    if !args.clean_hints {
        println!(
            "{}",
            fmt.format_message(
                "No checks selected. Run `1up doctor --clean-hints` to scan project instruction files for legacy 1up hints."
            )
        );
        return Ok(());
    }

    let project_root = Path::new(&args.path).canonicalize()?;
    let mut files = Vec::new();

    for relative in IN_SCOPE_FILES {
        let absolute = project_root.join(relative);
        let content = match std::fs::read_to_string(&absolute) {
            Ok(content) => content,
            Err(err) if err.kind() == ErrorKind::NotFound => continue,
            Err(err) => {
                return Err(anyhow::anyhow!(
                    "failed to read {}: {err}",
                    absolute.display()
                ))
            }
        };

        let report = classify(&content);
        let applied = if args.apply && report.fenced_span.is_some() {
            atomic_replace_within_project_root(
                &absolute,
                report.cleaned.as_bytes(),
                &project_root,
            )?;
            true
        } else {
            false
        };

        files.push(build_file_report(relative, &report, applied));
    }

    println!("{}", fmt.format_doctor_report(&DoctorReport { files }));
    Ok(())
}

/// Map a pure [`FileReport`] plus whether a write was performed into the stable
/// per-file report. Status precedence is fence (the only destructive path) over
/// unfenced advisories over clean; `modified` is true only when a fence was
/// actually removed on disk.
fn build_file_report(relative: &str, report: &FileReport, applied: bool) -> DoctorFileReport {
    let stale_tokens: Vec<StaleToken> = report
        .unfenced_findings
        .iter()
        .map(|finding| StaleToken {
            token: finding.token.clone(),
            line: finding.line,
        })
        .collect();

    let (status, modified) = if report.fenced_span.is_some() {
        if applied {
            (DoctorFileStatus::RemovedFence, true)
        } else {
            (DoctorFileStatus::WouldRemoveFence, false)
        }
    } else if !stale_tokens.is_empty() {
        (DoctorFileStatus::AdviseUnfenced, false)
    } else {
        (DoctorFileStatus::Clean, false)
    };

    DoctorFileReport {
        file: relative.to_string(),
        recommended_action: recommended_action(status).to_string(),
        status,
        stale_tokens,
        modified,
    }
}

/// Fixed, deterministic guidance for each per-file outcome.
fn recommended_action(status: DoctorFileStatus) -> &'static str {
    match status {
        DoctorFileStatus::Clean => "No legacy 1up hints found; nothing to do.",
        DoctorFileStatus::WouldRemoveFence => {
            "Re-run with --apply to remove the 1up-owned hint fence."
        }
        DoctorFileStatus::RemovedFence => "Removed the 1up-owned hint fence.",
        DoctorFileStatus::AdviseUnfenced => {
            "Manually remove the stale 1up references; 1up will not edit unfenced content."
        }
    }
}
