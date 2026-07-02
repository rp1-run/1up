use std::collections::HashSet;
use std::path::{Path, PathBuf};

use clap::Args;
use serde_json::json;

use crate::cli::project_status_files::prune_daemon_context_status;
use crate::daemon::lifecycle::acquire_rebuild_lock;
use crate::daemon::registry::Registry;
use crate::shared::config;
use crate::shared::project::resolve_project_root;
use crate::shared::types::{OutputFormat, WorktreeContext};
use crate::storage::db::Db;
use crate::storage::segments::{self, ContextDeletionCounts, IndexedContextRow};

/// Arguments for the `gc` command. The authoritative long-form help (preview-first
/// safety model, what is and is not prunable) is the `long_about` on the `Gc` arm in
/// `src/cli/mod.rs`, which `1up gc --help` renders.
#[derive(Args)]
pub struct GcArgs {
    /// Project root directory (defaults to current directory)
    #[arg(default_value = ".")]
    pub path: String,

    /// Prune the stale contexts and compact the index. Without this flag the command
    /// is a read-only preview that writes nothing
    #[arg(long)]
    pub apply: bool,

    /// Skip the post-prune VACUUM. Rows are deleted but `index.db` is not compacted,
    /// so freed space is reused by future indexing rather than returned to the
    /// filesystem. Use when a running daemon holds the exclusive VACUUM lock
    #[arg(long)]
    pub no_vacuum: bool,

    /// Output format override (defaults to human)
    #[arg(long, short = 'f')]
    pub format: Option<OutputFormat>,
}

/// Why a recorded context is safe to prune from the shared index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PruneReason {
    /// The worktree's source directory no longer exists on disk.
    SourceMissing,
    /// A stale snapshot of the current worktree under an older branch. `context_id`
    /// embeds the branch, so a same-worktree context that is not the active one is a
    /// leftover per-branch index that rebuilds on demand if that branch is revisited.
    StaleBranchSnapshot,
    /// A context whose `source_root` is a non-git-root subdirectory strictly inside the
    /// active worktree's `source_root`. The resolution clamp guarantees normal usage can
    /// no longer mint such a context, so any recorded one is a permanently unreachable
    /// orphan from the pre-clamp subdirectory bug.
    NestedSubdirContext,
}

impl PruneReason {
    fn as_str(self) -> &'static str {
        match self {
            PruneReason::SourceMissing => "source-missing",
            PruneReason::StaleBranchSnapshot => "stale-branch-snapshot",
            PruneReason::NestedSubdirContext => "nested-subdir",
        }
    }
}

/// Decide whether one recorded context is prunable relative to `active` (the live
/// worktree+branch this invocation resolved to). Pure and injected with
/// `source_exists` and `is_git_root` so it is deterministic and unit-testable.
///
/// The active context is never prunable. A context is pruned when its source
/// directory is gone, when it shares the active worktree's roots but not its
/// `context_id` (a stale per-branch snapshot), or when its source is a non-git-root
/// subdirectory strictly inside the active source (a nested-subdir orphan the
/// resolution clamp can no longer mint). Contexts belonging to other still-present
/// worktrees are left untouched, since this invocation cannot tell which of their
/// contexts is the live one (run `gc` from inside them instead).
fn prune_reason(
    active: &WorktreeContext,
    candidate: &IndexedContextRow,
    source_exists: &dyn Fn(&Path) -> bool,
    is_git_root: &dyn Fn(&Path) -> bool,
) -> Option<PruneReason> {
    if candidate.context_id == active.context_id {
        return None;
    }
    if !source_exists(&candidate.source_root) {
        return Some(PruneReason::SourceMissing);
    }
    if candidate.state_root == active.state_root && candidate.source_root == active.source_root {
        return Some(PruneReason::StaleBranchSnapshot);
    }
    if candidate.state_root == active.state_root
        && is_strict_descendant(&candidate.source_root, &active.source_root)
        && !is_git_root(&candidate.source_root)
    {
        return Some(PruneReason::NestedSubdirContext);
    }
    None
}

/// True when `descendant` lies strictly inside `ancestor` — a proper subdirectory,
/// never the directory itself. A linked worktree nested in the main worktree is
/// excluded separately by the `is_git_root` predicate at the call site.
fn is_strict_descendant(descendant: &Path, ancestor: &Path) -> bool {
    descendant != ancestor && descendant.starts_with(ancestor)
}

/// A context selected for pruning, plus the segment count that quantifies its cost.
struct GcCandidate {
    context_id: String,
    branch_name: Option<String>,
    source_root: PathBuf,
    reason: PruneReason,
    segments: u64,
}

/// Everything the renderer needs, decoupled from the apply/preview control flow.
struct GcReport<'a> {
    applied: bool,
    db_path: &'a Path,
    active_context_id: &'a str,
    total_contexts: usize,
    candidates: &'a [GcCandidate],
    deleted: ContextDeletionCounts,
    vacuumed: bool,
    size_before: u64,
    size_after: Option<u64>,
    warnings: &'a [String],
}

pub async fn exec(args: GcArgs, format: OutputFormat) -> anyhow::Result<()> {
    let path = Path::new(&args.path);
    let resolved = resolve_project_root(path)
        .map_err(|e| anyhow::anyhow!("failed to resolve project at {}: {e}", path.display()))?;
    let state_root = resolved.state_root.clone();
    let active = resolved.worktree_context;
    let db_path = config::project_db_path(&state_root);

    if !db_path.exists() {
        println!(
            "No 1up index found at {}. Nothing to prune.",
            state_root.display()
        );
        return Ok(());
    }

    // Read-only pass: list recorded contexts, classify, and count each candidate's
    // segments so the preview can quantify the prune.
    let (total_contexts, mut candidates) = {
        let db = Db::open_ro(&db_path).await?;
        let conn = db.connect()?;
        let contexts = segments::list_worktree_contexts(&conn).await?;
        let total = contexts.len();
        // A `.git` entry (dir for a main worktree, file for a linked one) marks a real
        // worktree root, so a nested candidate carrying one is never a subdir orphan.
        let is_git_root = |p: &Path| {
            let dot_git = p.join(".git");
            dot_git.is_dir() || dot_git.is_file()
        };
        let mut candidates = Vec::new();
        for ctx in &contexts {
            if let Some(reason) = prune_reason(&active, ctx, &|p: &Path| p.exists(), &is_git_root) {
                let segments = segments::count_segments_for_context(&conn, &ctx.context_id).await?;
                candidates.push(GcCandidate {
                    context_id: ctx.context_id.clone(),
                    branch_name: ctx.branch_name.clone(),
                    source_root: ctx.source_root.clone(),
                    reason,
                    segments,
                });
            }
        }
        (total, candidates)
    };

    // Stable output: grouped by reason string (alphabetical), then heaviest first.
    candidates.sort_by(|a, b| {
        a.reason
            .as_str()
            .cmp(b.reason.as_str())
            .then(b.segments.cmp(&a.segments))
            .then(a.context_id.cmp(&b.context_id))
    });

    let size_before = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);

    // Preview (default), or nothing to prune: write nothing, report what would happen.
    if !args.apply || candidates.is_empty() {
        render(
            format,
            &GcReport {
                applied: false,
                db_path: &db_path,
                active_context_id: &active.context_id,
                total_contexts,
                candidates: &candidates,
                deleted: ContextDeletionCounts::default(),
                vacuumed: false,
                size_before,
                size_after: None,
                warnings: &[],
            },
        );
        return Ok(());
    }

    // Apply: hold the single-writer rebuild lock while we delete rows and compact, so
    // no concurrent rebuild races the prune.
    let _rebuild_lock = acquire_rebuild_lock(&state_root)?;
    let mut deleted = ContextDeletionCounts::default();
    let mut vacuum_error: Option<String> = None;
    {
        let db = Db::open_rw(&db_path).await?;
        let conn = db.connect_tuned().await?;
        for candidate in &candidates {
            let counts = segments::delete_context(&conn, &candidate.context_id).await?;
            deleted.segments += counts.segments;
            deleted.relations += counts.relations;
            deleted.indexed_files += counts.indexed_files;
        }
        if !args.no_vacuum {
            if let Err(err) = segments::vacuum_database(&conn).await {
                vacuum_error = Some(err.to_string());
            }
        }
    }

    // Best-effort bookkeeping so pruned contexts do not reappear via the daemon. The
    // index rows are already gone; a hiccup here is a warning, never a hard failure.
    let pruned_ids: HashSet<String> = candidates.iter().map(|c| c.context_id.clone()).collect();
    let mut warnings: Vec<String> = Vec::new();
    if let Some(err) = &vacuum_error {
        warnings.push(format!(
            "VACUUM did not run ({err}); rows were pruned but index.db was not compacted. \
             Stop the daemon (`1up stop`) and re-run `1up gc --apply` to reclaim disk."
        ));
    }
    if let Err(err) = prune_daemon_context_status(&state_root, &pruned_ids) {
        warnings.push(format!("could not update daemon status file: {err}"));
    }
    if let Err(err) = Registry::load().and_then(|mut r| r.deregister_context_ids(&pruned_ids)) {
        warnings.push(format!("could not update project registry: {err}"));
    }

    let size_after = std::fs::metadata(&db_path).map(|m| m.len()).ok();
    render(
        format,
        &GcReport {
            applied: true,
            db_path: &db_path,
            active_context_id: &active.context_id,
            total_contexts,
            candidates: &candidates,
            deleted,
            vacuumed: !args.no_vacuum && vacuum_error.is_none(),
            size_before,
            size_after,
            warnings: &warnings,
        },
    );
    Ok(())
}

fn render(format: OutputFormat, report: &GcReport) {
    match format {
        OutputFormat::Json => println!("{}", render_json(report)),
        OutputFormat::Human | OutputFormat::Plain => println!("{}", render_human(report)),
    }
}

fn render_json(report: &GcReport) -> String {
    let candidates: Vec<_> = report
        .candidates
        .iter()
        .map(|c| {
            json!({
                "context_id": c.context_id,
                "branch_name": c.branch_name,
                "source_root": c.source_root,
                "reason": c.reason.as_str(),
                "segments": c.segments,
            })
        })
        .collect();

    let mut value = json!({
        "applied": report.applied,
        "db_path": report.db_path,
        "active_context_id": report.active_context_id,
        "total_contexts": report.total_contexts,
        "prunable_contexts": report.candidates.len(),
        "candidates": candidates,
        "size_bytes": report.size_before,
        "warnings": report.warnings,
    });
    if report.applied {
        value["deleted"] = json!({
            "segments": report.deleted.segments,
            "relations": report.deleted.relations,
            "indexed_files": report.deleted.indexed_files,
        });
        value["vacuumed"] = json!(report.vacuumed);
        value["size_bytes_after"] = json!(report.size_after);
    }
    serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string())
}

fn render_human(report: &GcReport) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "1up gc — shared index at {}\n",
        report.db_path.display()
    ));
    out.push_str(&format!(
        "active context {} is kept; {} context(s) recorded\n",
        short_id(report.active_context_id),
        report.total_contexts,
    ));

    if report.candidates.is_empty() {
        out.push_str("No stale contexts to prune; all recorded contexts are live.");
        return out;
    }

    for c in report.candidates {
        out.push_str(&format!(
            "  [{:<21}] {}  {:<24}  {:>6} segments  {}\n",
            c.reason.as_str(),
            short_id(&c.context_id),
            c.branch_name.as_deref().unwrap_or("(no branch)"),
            c.segments,
            c.source_root.display(),
        ));
    }

    if report.applied {
        out.push_str(&format!(
            "\nPruned {} context(s): {} segments, {} relations, {} files.\n",
            report.candidates.len(),
            report.deleted.segments,
            report.deleted.relations,
            report.deleted.indexed_files,
        ));
        match (report.vacuumed, report.size_after) {
            (true, Some(after)) => out.push_str(&format!(
                "index.db: {} → {} (freed {}).",
                human_bytes(report.size_before),
                human_bytes(after),
                human_bytes(report.size_before.saturating_sub(after)),
            )),
            _ => out.push_str(&format!(
                "index.db: {} (not compacted; freed space is reused by future indexing).",
                human_bytes(report.size_before),
            )),
        }
    } else {
        let total_segments: u64 = report.candidates.iter().map(|c| c.segments).sum();
        out.push_str(&format!(
            "\nWould prune {} context(s) (~{} segments) from a {} index. \
             Re-run with --apply to prune and compact.",
            report.candidates.len(),
            total_segments,
            human_bytes(report.size_before),
        ));
    }

    for warning in report.warnings {
        out.push_str(&format!("\n! {warning}"));
    }
    out
}

/// Short, stable display form of a 1up context id (32 hex chars).
fn short_id(context_id: &str) -> &str {
    context_id.get(..12).unwrap_or(context_id)
}

/// Compact human byte size (binary units) for index.db reporting.
fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::types::{BranchStatus, WorktreeRole};

    fn active_context() -> WorktreeContext {
        WorktreeContext {
            context_id: "active000000".to_string(),
            state_root: PathBuf::from("/repo"),
            source_root: PathBuf::from("/repo"),
            main_worktree_root: PathBuf::from("/repo"),
            worktree_role: WorktreeRole::Main,
            git_dir: None,
            common_git_dir: None,
            branch_name: Some("main".to_string()),
            branch_ref: Some("refs/heads/main".to_string()),
            head_oid: None,
            branch_status: BranchStatus::Named,
        }
    }

    fn row(context_id: &str, state_root: &str, source_root: &str) -> IndexedContextRow {
        IndexedContextRow {
            context_id: context_id.to_string(),
            state_root: PathBuf::from(state_root),
            source_root: PathBuf::from(source_root),
            branch_name: None,
        }
    }

    #[test]
    fn active_context_is_never_pruned() {
        let active = active_context();
        let candidate = row("active000000", "/repo", "/repo");
        assert_eq!(
            prune_reason(&active, &candidate, &|_| true, &|_| false),
            None
        );
    }

    #[test]
    fn missing_source_root_is_pruned_even_when_roots_differ() {
        let active = active_context();
        let candidate = row("other0000001", "/gone", "/gone");
        assert_eq!(
            prune_reason(&active, &candidate, &|p| p != Path::new("/gone"), &|_| {
                false
            }),
            Some(PruneReason::SourceMissing)
        );
    }

    #[test]
    fn same_worktree_other_branch_is_a_stale_snapshot() {
        let active = active_context();
        // Same roots as the active worktree, different id => an older branch's index.
        let candidate = row("oldbranch001", "/repo", "/repo");
        assert_eq!(
            prune_reason(&active, &candidate, &|_| true, &|_| false),
            Some(PruneReason::StaleBranchSnapshot)
        );
    }

    #[test]
    fn other_live_worktree_is_left_untouched() {
        let active = active_context();
        // A different worktree whose source still exists and is not nested inside the
        // active source: not ours to judge.
        let candidate = row("linked000001", "/repo", "/repo-feature");
        assert_eq!(
            prune_reason(&active, &candidate, &|_| true, &|_| false),
            None
        );
    }

    #[test]
    fn nested_subdir_context_is_pruned() {
        let active = active_context();
        // A pre-clamp orphan: same state root, source strictly inside the active source,
        // and not itself a git root.
        let candidate = row("nested000001", "/repo", "/repo/src/search");
        assert_eq!(
            prune_reason(&active, &candidate, &|_| true, &|_| false),
            Some(PruneReason::NestedSubdirContext)
        );
    }

    #[test]
    fn nested_linked_worktree_is_kept() {
        let active = active_context();
        // A linked worktree created inside the main worktree: nested, but its source root
        // carries a `.git` file, so the git-root predicate protects it from the rule.
        let candidate = row("nestedwt0001", "/repo", "/repo/linked-wt");
        assert_eq!(
            prune_reason(&active, &candidate, &|_| true, &|p| p
                == Path::new("/repo/linked-wt"),),
            None
        );
    }

    #[test]
    fn human_bytes_scales_units() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MiB");
    }
}
