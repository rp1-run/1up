use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use clap::Args;
use serde_json::json;
use tracing;

use crate::cli::project_status_files::prune_daemon_context_status;
use crate::daemon::lifecycle::acquire_rebuild_lock;
use crate::daemon::registry::Registry;
use crate::shared::config;
use crate::shared::constants::{
    GC_SUPERSEDED_SAME_SOURCE_KEEP_COUNT, GC_SUPERSEDED_SAME_SOURCE_MAX_AGE_DAYS,
};
use crate::shared::project::resolve_project_root;
use crate::shared::types::{OutputFormat, WorktreeContext};
use crate::storage::db::Db;
use crate::storage::segments::{self, ContextDeletionCounts, IndexedContextRow};
use std::io;

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
    /// A context sharing the active worktree's `source_root` but recorded under a
    /// *different* `state_root` (e.g. after a state-root resolution/relocation
    /// change) — ranked beyond the retention policy's `keep_count` most-recently-
    /// updated same-source peers and older than its `max_age`. Unlike
    /// `StaleBranchSnapshot` (same `state_root`, pruned unconditionally),
    /// same-source-different-`state_root` is a softer signal, so it is
    /// policy-gated rather than unconditional (governance: no invented numeric
    /// defaults; automatic-on-migration application stays opt-in, default OFF).
    SupersededSameSource,
}

impl PruneReason {
    fn as_str(self) -> &'static str {
        match self {
            PruneReason::SourceMissing => "source-missing",
            PruneReason::StaleBranchSnapshot => "stale-branch-snapshot",
            PruneReason::NestedSubdirContext => "nested-subdir",
            PruneReason::SupersededSameSource => "superseded-same-source",
        }
    }
}

/// Keep-count/age thresholds gating the `SupersededSameSource` prune reason.
///
/// PLACEHOLDER-conservative: sourced from [`GC_SUPERSEDED_SAME_SOURCE_KEEP_COUNT`]
/// and [`GC_SUPERSEDED_SAME_SOURCE_MAX_AGE_DAYS`], both explicitly flagged as
/// interim values pending planning-gate finalization, not invented final
/// defaults. Enforced against `1up gc --apply`'s real candidate set via
/// [`same_source_recency_ranks`] in `exec()`'s classification pass.
struct RetentionPolicy {
    keep_count: usize,
    max_age: Duration,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        RetentionPolicy {
            keep_count: GC_SUPERSEDED_SAME_SOURCE_KEEP_COUNT,
            max_age: Duration::days(GC_SUPERSEDED_SAME_SOURCE_MAX_AGE_DAYS),
        }
    }
}

/// Evaluation context for the `SupersededSameSource` policy: the retention
/// thresholds, this candidate's 1-based recency rank among all recorded
/// contexts sharing its `source_root` (1 = most-recently-updated), and the
/// reference time used to compute age from `updated_at`.
///
/// A `prune_reason` call passing `None` here means the policy is not
/// evaluated for that call, so `SupersededSameSource` never fires. `exec()`'s
/// classification pass always supplies `Some` for candidates sharing the
/// active `source_root`, computed via [`same_source_recency_ranks`].
struct SupersededSameSourceContext<'a> {
    policy: &'a RetentionPolicy,
    rank_among_same_source: usize,
    now: DateTime<Utc>,
}

/// True when `updated_at` (a `datetime('now')`-formatted TEXT value,
/// `YYYY-MM-DD HH:MM:SS`, UTC — see `worktree_contexts.updated_at`) is at
/// least `min_age` old relative to `now`. Unparseable input degrades to
/// `false` (not old enough): a retention decision must never prune on
/// ambiguous data.
fn context_age_at_least(updated_at: &str, now: DateTime<Utc>, min_age: Duration) -> bool {
    match chrono::NaiveDateTime::parse_from_str(updated_at, "%Y-%m-%d %H:%M:%S") {
        Ok(parsed) => now - parsed.and_utc() >= min_age,
        Err(_) => false,
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
///
/// `retention` optionally gates a fourth, softer reason: a context sharing the
/// active `source_root` under a *different* `state_root`, ranked beyond the
/// policy's `keep_count` most-recently-updated same-source peers and older than
/// `max_age`. Passing `None` disables this check entirely (see
/// [`SupersededSameSourceContext`]).
fn prune_reason(
    active: &WorktreeContext,
    candidate: &IndexedContextRow,
    retention: Option<&SupersededSameSourceContext>,
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
    // Reached only when source_root == active.source_root implies state_root
    // differs (the equal-state-root case already returned above).
    if candidate.source_root == active.source_root {
        if let Some(ctx) = retention {
            if ctx.rank_among_same_source > ctx.policy.keep_count
                && context_age_at_least(&candidate.updated_at, ctx.now, ctx.policy.max_age)
            {
                return Some(PruneReason::SupersededSameSource);
            }
        }
    }
    None
}

/// True when `descendant` lies strictly inside `ancestor` — a proper subdirectory,
/// never the directory itself. A linked worktree nested in the main worktree is
/// excluded separately by the `is_git_root` predicate at the call site.
fn is_strict_descendant(descendant: &Path, ancestor: &Path) -> bool {
    descendant != ancestor && descendant.starts_with(ancestor)
}

/// 1-based recency rank (1 = most-recently-updated) of every recorded context
/// whose `source_root` matches `source_root`, keyed by `context_id`. The active
/// context is included in the ranked set — it is always present in `contexts`
/// and is typically the most recent after a fresh index run — so `keep_count`
/// naturally counts it as one of the retained peers. Ties in `updated_at` break
/// by `context_id` for a deterministic order. Contexts with a different
/// `source_root` are absent from the returned map entirely, matching
/// `prune_reason`'s gate (the policy only ever evaluates same-source
/// candidates).
fn same_source_recency_ranks(
    contexts: &[IndexedContextRow],
    source_root: &Path,
) -> HashMap<String, usize> {
    let mut same_source: Vec<&IndexedContextRow> = contexts
        .iter()
        .filter(|c| c.source_root == source_root)
        .collect();
    same_source.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then(a.context_id.cmp(&b.context_id))
    });
    same_source
        .into_iter()
        .enumerate()
        .map(|(index, ctx)| (ctx.context_id.clone(), index + 1))
        .collect()
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
        // A failed first-index (e.g. SIGKILL mid-rebuild) can leave orphan staging
        // DBs (index.db.rebuild-<uuid>) even though the main index.db was never
        // created. Under --apply, reclaim them — but only if `.1up` already exists
        // (so we never create it just to sweep) and while holding the rebuild lock
        // (so we never delete an in-progress first rebuild's staging DB) (M1).
        if args.apply && config::project_dot_dir(&state_root).exists() {
            match acquire_rebuild_lock(&state_root) {
                Ok(_lock) => {
                    if let Err(err) = sweep_staging_databases(&state_root) {
                        eprintln!("warning: could not sweep staging databases: {err}");
                    }
                }
                Err(err) => {
                    eprintln!(
                        "warning: skipped staging sweep (a rebuild may be in progress): {err}"
                    );
                }
            }
        }
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
        // `SupersededSameSource` enforcement against the real candidate set: rank
        // every same-source context by recency once, then gate each candidate's
        // reason lookup with the shared policy default.
        let retention_policy = RetentionPolicy::default();
        let same_source_ranks = same_source_recency_ranks(&contexts, &active.source_root);
        let now = Utc::now();
        let mut candidates = Vec::new();
        for ctx in &contexts {
            let retention =
                same_source_ranks
                    .get(&ctx.context_id)
                    .map(|&rank| SupersededSameSourceContext {
                        policy: &retention_policy,
                        rank_among_same_source: rank,
                        now,
                    });
            if let Some(reason) = prune_reason(
                &active,
                ctx,
                retention.as_ref(),
                &|p: &Path| p.exists(),
                &is_git_root,
            ) {
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

    // Preview (default): write nothing, report what would happen.
    if !args.apply {
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

    // Apply: hold the single-writer rebuild lock while we delete rows, compact, AND
    // sweep orphan staging DBs, so no concurrent rebuild races the prune. This runs
    // even when there are no context candidates so orphan staging databases (left by
    // a hard-killed or failed-first rebuild) are still reclaimed (M1) — previously
    // the whole apply path was skipped when `candidates` was empty.
    let _rebuild_lock = acquire_rebuild_lock(&state_root)?;
    let mut deleted = ContextDeletionCounts::default();
    let mut vacuum_error: Option<String> = None;
    if !candidates.is_empty() {
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
    if !pruned_ids.is_empty() {
        if let Err(err) = prune_daemon_context_status(&state_root, &pruned_ids) {
            warnings.push(format!("could not update daemon status file: {err}"));
        }
        if let Err(err) = Registry::load().and_then(|mut r| r.deregister_context_ids(&pruned_ids)) {
            warnings.push(format!("could not update project registry: {err}"));
        }
    }

    // Sweep orphaned staging databases (index.db.rebuild-<uuid>) left behind by hard
    // kills — always, even with no context candidates, under the rebuild lock held above.
    if let Err(err) = sweep_staging_databases(&state_root) {
        warnings.push(format!("could not sweep staging databases: {err}"));
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
            vacuumed: !args.no_vacuum && vacuum_error.is_none() && !candidates.is_empty(),
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

/// Sweep orphaned staging databases (index.db.rebuild-<uuid>) from the .1up directory.
/// These are left behind when a rebuild is hard-killed (e.g., SIGKILL) before finalization.
/// Returns Ok(()) on success (whether or not any files were found and deleted).
fn sweep_staging_databases(state_root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let dot_1up = config::project_dot_dir(state_root);
    if !dot_1up.exists() {
        return Ok(());
    }

    for entry in std::fs::read_dir(&dot_1up)? {
        let entry = entry?;
        let path = entry.path();
        let file_name = entry.file_name();
        let file_name_str = file_name.to_string_lossy();

        if file_name_str.starts_with(config::STAGING_INDEX_DB_PREFIX) {
            if let Err(err) = std::fs::remove_file(&path) {
                if err.kind() != io::ErrorKind::NotFound {
                    tracing::warn!(
                        "failed to remove staging database {}: {}",
                        path.display(),
                        err
                    );
                }
            }
        }
    }

    Ok(())
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
        row_updated_at(context_id, state_root, source_root, "2026-07-01 00:00:00")
    }

    fn row_updated_at(
        context_id: &str,
        state_root: &str,
        source_root: &str,
        updated_at: &str,
    ) -> IndexedContextRow {
        IndexedContextRow {
            context_id: context_id.to_string(),
            state_root: PathBuf::from(state_root),
            source_root: PathBuf::from(source_root),
            branch_name: None,
            updated_at: updated_at.to_string(),
        }
    }

    #[test]
    fn active_context_is_never_pruned() {
        let active = active_context();
        let candidate = row("active000000", "/repo", "/repo");
        assert_eq!(
            prune_reason(&active, &candidate, None, &|_| true, &|_| false),
            None
        );
    }

    #[test]
    fn missing_source_root_is_pruned_even_when_roots_differ() {
        let active = active_context();
        let candidate = row("other0000001", "/gone", "/gone");
        assert_eq!(
            prune_reason(
                &active,
                &candidate,
                None,
                &|p| p != Path::new("/gone"),
                &|_| false,
            ),
            Some(PruneReason::SourceMissing)
        );
    }

    #[test]
    fn same_worktree_other_branch_is_a_stale_snapshot() {
        let active = active_context();
        // Same roots as the active worktree, different id => an older branch's index.
        let candidate = row("oldbranch001", "/repo", "/repo");
        assert_eq!(
            prune_reason(&active, &candidate, None, &|_| true, &|_| false),
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
            prune_reason(&active, &candidate, None, &|_| true, &|_| false),
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
            prune_reason(&active, &candidate, None, &|_| true, &|_| false),
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
            prune_reason(&active, &candidate, None, &|_| true, &|p| p
                == Path::new("/repo/linked-wt"),),
            None
        );
    }

    fn old_policy() -> RetentionPolicy {
        RetentionPolicy {
            keep_count: 1,
            max_age: Duration::days(30),
        }
    }

    #[test]
    fn superseded_same_source_beyond_keep_count_and_max_age_is_pruned() {
        let active = active_context();
        // Same source_root as active, but a different state_root (e.g. recorded
        // before a state-root resolution change) and old.
        let candidate = row_updated_at(
            "reloc0000001",
            "/other-state",
            "/repo",
            "2026-01-01 00:00:00",
        );
        let policy = old_policy();
        let retention = SupersededSameSourceContext {
            policy: &policy,
            rank_among_same_source: 2, // beyond keep_count = 1
            now: DateTime::parse_from_rfc3339("2026-07-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        };
        assert_eq!(
            prune_reason(&active, &candidate, Some(&retention), &|_| true, &|_| {
                false
            }),
            Some(PruneReason::SupersededSameSource)
        );
    }

    #[test]
    fn superseded_same_source_within_keep_count_is_kept() {
        let active = active_context();
        let candidate = row_updated_at(
            "reloc0000002",
            "/other-state",
            "/repo",
            "2026-01-01 00:00:00",
        );
        let policy = old_policy();
        let retention = SupersededSameSourceContext {
            policy: &policy,
            rank_among_same_source: 1, // within keep_count = 1: always kept
            now: DateTime::parse_from_rfc3339("2026-07-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        };
        assert_eq!(
            prune_reason(&active, &candidate, Some(&retention), &|_| true, &|_| {
                false
            }),
            None
        );
    }

    #[test]
    fn superseded_same_source_too_recent_is_kept() {
        let active = active_context();
        // Beyond keep_count, but not yet older than max_age.
        let candidate = row_updated_at(
            "reloc0000003",
            "/other-state",
            "/repo",
            "2026-06-20 00:00:00",
        );
        let policy = old_policy();
        let retention = SupersededSameSourceContext {
            policy: &policy,
            rank_among_same_source: 2,
            now: DateTime::parse_from_rfc3339("2026-07-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        };
        assert_eq!(
            prune_reason(&active, &candidate, Some(&retention), &|_| true, &|_| {
                false
            }),
            None
        );
    }

    #[test]
    fn superseded_same_source_without_retention_context_is_kept() {
        let active = active_context();
        // Would qualify under the policy, but the caller passed `None`: the reason
        // must never fire without an explicit retention context.
        let candidate = row_updated_at(
            "reloc0000004",
            "/other-state",
            "/repo",
            "2020-01-01 00:00:00",
        );
        assert_eq!(
            prune_reason(&active, &candidate, None, &|_| true, &|_| false),
            None
        );
    }

    #[test]
    fn superseded_same_source_different_source_root_is_kept() {
        let active = active_context();
        // Different source_root than active: not evaluated by this policy at all,
        // even with a retention context that would otherwise qualify.
        let candidate = row_updated_at(
            "otherrepo001",
            "/other-state",
            "/other-repo",
            "2020-01-01 00:00:00",
        );
        let policy = old_policy();
        let retention = SupersededSameSourceContext {
            policy: &policy,
            rank_among_same_source: 99,
            now: DateTime::parse_from_rfc3339("2026-07-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        };
        assert_eq!(
            prune_reason(&active, &candidate, Some(&retention), &|_| true, &|_| {
                false
            }),
            None
        );
    }

    #[test]
    fn stale_branch_snapshot_takes_priority_over_superseded_same_source() {
        let active = active_context();
        // Same state_root AND source_root as active: the unconditional
        // StaleBranchSnapshot reason must win even when a retention context is
        // supplied that would also match the softer policy.
        let candidate = row_updated_at("oldbranch002", "/repo", "/repo", "2020-01-01 00:00:00");
        let policy = old_policy();
        let retention = SupersededSameSourceContext {
            policy: &policy,
            rank_among_same_source: 99,
            now: DateTime::parse_from_rfc3339("2026-07-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        };
        assert_eq!(
            prune_reason(&active, &candidate, Some(&retention), &|_| true, &|_| {
                false
            }),
            Some(PruneReason::StaleBranchSnapshot)
        );
    }

    #[test]
    fn context_age_at_least_true_when_older_than_min_age() {
        let now = DateTime::parse_from_rfc3339("2026-07-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert!(context_age_at_least(
            "2026-01-01 00:00:00",
            now,
            Duration::days(30)
        ));
    }

    #[test]
    fn context_age_at_least_false_when_younger_than_min_age() {
        let now = DateTime::parse_from_rfc3339("2026-07-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert!(!context_age_at_least(
            "2026-06-20 00:00:00",
            now,
            Duration::days(30)
        ));
    }

    #[test]
    fn context_age_at_least_false_on_unparseable_input() {
        let now = Utc::now();
        assert!(!context_age_at_least("not-a-date", now, Duration::days(0)));
    }

    #[test]
    fn human_bytes_scales_units() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MiB");
    }

    #[test]
    fn same_source_recency_ranks_orders_by_updated_at_descending() {
        let contexts = vec![
            row_updated_at("a", "/state-a", "/repo", "2026-01-01 00:00:00"),
            row_updated_at("b", "/state-b", "/repo", "2026-03-01 00:00:00"),
            row_updated_at("c", "/state-c", "/repo", "2026-02-01 00:00:00"),
            row_updated_at("other", "/state-d", "/other-repo", "2026-06-01 00:00:00"),
        ];
        let ranks = same_source_recency_ranks(&contexts, Path::new("/repo"));
        assert_eq!(
            ranks.get("b"),
            Some(&1),
            "most-recently-updated ranks first"
        );
        assert_eq!(ranks.get("c"), Some(&2));
        assert_eq!(
            ranks.get("a"),
            Some(&3),
            "oldest same-source peer ranks last"
        );
        assert_eq!(
            ranks.get("other"),
            None,
            "a different source_root is excluded from the ranked set entirely"
        );
    }

    /// Restores `HOME`/`XDG_DATA_HOME` on drop — even if an assertion panics — so a
    /// redirected data root never leaks into other tests in this binary. Mirrors
    /// `indexer::embedder`'s `DataRootGuard`.
    struct DataRootGuard {
        home: Option<std::ffi::OsString>,
        xdg_data: Option<std::ffi::OsString>,
    }

    impl DataRootGuard {
        fn redirect_to(dir: &Path) -> Self {
            let guard = DataRootGuard {
                home: std::env::var_os("HOME"),
                xdg_data: std::env::var_os("XDG_DATA_HOME"),
            };
            std::env::set_var("HOME", dir);
            std::env::set_var("XDG_DATA_HOME", dir.join(".local").join("share"));
            guard
        }
    }

    impl Drop for DataRootGuard {
        fn drop(&mut self) {
            fn restore(key: &str, value: Option<std::ffi::OsString>) {
                match value {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
            restore("HOME", self.home.take());
            restore("XDG_DATA_HOME", self.xdg_data.take());
        }
    }

    /// Inserts a `worktree_contexts` row with an explicit `updated_at`, bypassing
    /// `upsert_worktree_context`'s `datetime('now')` default so tests can seed
    /// contexts of a specific age for the `SupersededSameSource` keep-count/age
    /// policy.
    async fn insert_worktree_context_row(
        conn: &libsql::Connection,
        context_id: &str,
        source_root: &Path,
        state_root: &Path,
        updated_at: &str,
    ) {
        conn.execute(
            "INSERT INTO worktree_contexts (\
                context_id, project_id, state_root, source_root, main_worktree_root, \
                worktree_role, branch_name, branch_ref, branch_status, head_oid, \
                git_dir, common_git_dir, updated_at\
            ) VALUES (?1, 'gc-t4-proj', ?2, ?3, ?3, 'main', NULL, NULL, 'unknown', NULL, NULL, NULL, ?4)",
            libsql::params![
                context_id.to_string(),
                state_root.to_string_lossy().into_owned(),
                source_root.to_string_lossy().into_owned(),
                updated_at.to_string(),
            ],
        )
        .await
        .unwrap();
    }

    /// Integration test (REQ-003/T4): `1up gc --apply` must enforce the
    /// `SupersededSameSource` policy against its real candidate set — recency
    /// rank computed over every recorded context, not the always-`None`
    /// retention gate this loop used before T4 — then delete the qualifying
    /// rows via `delete_context` and VACUUM.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn gc_apply_prunes_superseded_same_source_contexts_beyond_policy_and_vacuums() {
        use crate::storage::db::Db;
        use crate::storage::schema;
        use crate::storage::segments::{IndexedFileMeta, SegmentInsert};

        // Every test in this binary that mutates HOME/XDG_DATA_HOME (`dirs::*`
        // reads them at call time) must serialize on this crate-wide lock, or a
        // concurrent mutation in another module corrupts this test's resolved
        // registry/data paths.
        let _env_lock = crate::shared::fs::ENV_MUTEX
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let home = tempfile::tempdir().unwrap();
        let _data_root_guard = DataRootGuard::redirect_to(home.path());

        let project = tempfile::tempdir().unwrap();
        let project_root = project.path().canonicalize().unwrap();
        std::fs::create_dir_all(project_root.join(".1up")).unwrap();
        let db_path = project_root.join(".1up").join("index.db");

        // Resolve what `exec()` will independently compute as the active
        // context, so seeded rows share its real `source_root`/`context_id`.
        let active = resolve_project_root(&project_root)
            .unwrap()
            .worktree_context;

        let shared_key = "gc-t4-shared-key".to_string();
        let shared_vector = serde_json::to_string(&vec![0.1_f32; 384]).unwrap();
        let segment_for = |context_id: &str| SegmentInsert {
            id: format!("{context_id}-seg"),
            file_path: format!("{context_id}.rs"),
            language: "rust".to_string(),
            block_type: "function".to_string(),
            content: format!("pub fn {context_id}() {{}}\n"),
            line_start: 1,
            line_end: 1,
            content_key: Some(shared_key.clone()),
            embedding_vec: Some(shared_vector.clone()),
            breadcrumb: None,
            complexity: 1,
            role: "DEFINITION".to_string(),
            defined_symbols: format!("[\"{context_id}\"]"),
            referenced_symbols: "[]".to_string(),
            referenced_relations: "[]".to_string(),
            called_symbols: "[]".to_string(),
            called_relations: "[]".to_string(),
            file_hash: format!("{context_id}-hash"),
        };
        let meta_for = |context_id: &str| IndexedFileMeta {
            extension: "rs".to_string(),
            file_hash: format!("{context_id}-hash"),
            file_size: 32,
            modified_ns: 1,
        };

        {
            let db = Db::open_rw(&db_path).await.unwrap();
            let conn = db.connect().unwrap();
            schema::initialize(&conn).await.unwrap();

            // Active context, recorded via the real upsert path (updated_at =
            // now), making it the most-recently-updated same-source peer.
            segments::upsert_worktree_context(&conn, &active, "gc-t4-proj")
                .await
                .unwrap();
            crate::storage::segments::replace_file_segments_for_context_tx_with_meta(
                &conn,
                &active.context_id,
                &format!("{}.rs", active.context_id),
                &[segment_for(&active.context_id)],
                Some(&meta_for(&active.context_id)),
            )
            .await
            .unwrap();

            // Two same-source peers under fabricated `state_root`s, within the
            // keep_count = 3 top-ranked slots (active + these two = 3 total):
            // kept regardless of age.
            for id in ["kept-1", "kept-2"] {
                insert_worktree_context_row(
                    &conn,
                    id,
                    &active.source_root,
                    Path::new("/other-state"),
                    "2026-06-25 00:00:00",
                )
                .await;
                crate::storage::segments::replace_file_segments_for_context_tx_with_meta(
                    &conn,
                    id,
                    &format!("{id}.rs"),
                    &[segment_for(id)],
                    Some(&meta_for(id)),
                )
                .await
                .unwrap();
            }

            // A fourth same-source peer: ranked beyond keep_count and older than
            // GC_SUPERSEDED_SAME_SOURCE_MAX_AGE_DAYS — the one candidate that
            // must be pruned by the SupersededSameSource policy.
            insert_worktree_context_row(
                &conn,
                "superseded-1",
                &active.source_root,
                Path::new("/other-state-old"),
                "2026-01-01 00:00:00",
            )
            .await;
            crate::storage::segments::replace_file_segments_for_context_tx_with_meta(
                &conn,
                "superseded-1",
                "superseded-1.rs",
                &[segment_for("superseded-1")],
                Some(&meta_for("superseded-1")),
            )
            .await
            .unwrap();

            let mut rows = conn
                .query(
                    "SELECT ref_count FROM embedding_pool WHERE content_key = ?1",
                    [shared_key.as_str()],
                )
                .await
                .unwrap();
            let ref_count_before: i64 = rows.next().await.unwrap().unwrap().get(0).unwrap();
            assert_eq!(
                ref_count_before, 4,
                "all four seeded segments must share one pooled embedding"
            );
        }

        let args = GcArgs {
            path: project_root.to_string_lossy().into_owned(),
            apply: true,
            no_vacuum: false,
            format: None,
        };
        exec(args, OutputFormat::Json)
            .await
            .expect("gc --apply must succeed");

        let db = Db::open_ro(&db_path).await.unwrap();
        let conn = db.connect().unwrap();
        let remaining = segments::list_worktree_contexts(&conn).await.unwrap();
        let remaining_ids: HashSet<String> =
            remaining.iter().map(|c| c.context_id.clone()).collect();
        assert!(
            remaining_ids.contains(&active.context_id),
            "active context must survive"
        );
        assert!(
            remaining_ids.contains("kept-1"),
            "within-keep_count same-source peer must survive"
        );
        assert!(
            remaining_ids.contains("kept-2"),
            "within-keep_count same-source peer must survive"
        );
        assert!(
            !remaining_ids.contains("superseded-1"),
            "beyond-keep_count, aged same-source peer must be pruned by gc --apply"
        );

        // Row/refcount assertions on `delete_context`: the pruned context's
        // segment is gone, and the shared pooled embedding's ref_count drops by
        // exactly one (still referenced by the three survivors) rather than
        // being reclaimed outright.
        assert_eq!(
            segments::count_segments_for_context(&conn, "superseded-1")
                .await
                .unwrap(),
            0
        );
        let mut rows = conn
            .query(
                "SELECT ref_count FROM embedding_pool WHERE content_key = ?1",
                [shared_key.as_str()],
            )
            .await
            .unwrap();
        let ref_count_after: i64 = rows.next().await.unwrap().unwrap().get(0).unwrap();
        assert_eq!(
            ref_count_after, 3,
            "delete_context must decrement, not zero out, a still-referenced pooled embedding"
        );

        // `runs vacuum_database`: a successful VACUUM compacts away every freed
        // page, so the freelist floor is back to zero after the apply.
        assert_eq!(
            segments::freelist_reclaimable_bytes(&conn).await.unwrap(),
            0,
            "gc --apply must VACUUM after pruning, leaving no freed-but-unreturned pages"
        );
    }

    #[test]
    fn sweep_staging_databases_removes_orphaned_rebuild_files() {
        let temp_dir = tempfile::tempdir().unwrap();
        let dot_1up = temp_dir.path().join(".1up");
        std::fs::create_dir(&dot_1up).unwrap();

        // Create some staging database files
        let staging_file_1 = dot_1up.join("index.db.rebuild-12345678-abcd-1234-abcd-123456789abc");
        let staging_file_2 = dot_1up.join("index.db.rebuild-87654321-dcba-4321-dcba-987654321def");
        let regular_file = dot_1up.join("index.db");

        std::fs::write(&staging_file_1, "staging data 1").unwrap();
        std::fs::write(&staging_file_2, "staging data 2").unwrap();
        std::fs::write(&regular_file, "regular db").unwrap();

        // Verify files exist before sweep
        assert!(staging_file_1.exists());
        assert!(staging_file_2.exists());
        assert!(regular_file.exists());

        // Run sweep
        sweep_staging_databases(temp_dir.path()).expect("sweep should succeed");

        // Verify staging files are gone but regular file remains
        assert!(
            !staging_file_1.exists(),
            "staging database should be removed"
        );
        assert!(
            !staging_file_2.exists(),
            "staging database should be removed"
        );
        assert!(regular_file.exists(), "regular index.db should remain");
    }

    #[test]
    fn sweep_staging_databases_handles_missing_directory() {
        let temp_dir = tempfile::tempdir().unwrap();
        // Don't create .1up directory
        let result = sweep_staging_databases(temp_dir.path());
        assert!(
            result.is_ok(),
            "sweep should succeed on missing .1up directory"
        );
    }
}
