use std::io::{self, Write};

use clap::Args;

use crate::cli::{discovery_output, lean};
use crate::daemon::lifecycle;
use crate::search::{SearchScope, SymbolSearchEngine};
use crate::shared::config::project_db_path;
use crate::shared::project;
use crate::storage::db::Db;
use crate::storage::schema;

#[derive(Args)]
pub struct SymbolArgs {
    /// Symbol name to look up
    pub name: String,

    /// Include references (usages) in addition to definitions
    #[arg(long, short)]
    pub references: bool,

    /// Use fuzzy matching (substring, prefix, edit distance) when exact match fails
    #[arg(long)]
    pub fuzzy: bool,

    /// Project root directory (defaults to current directory)
    #[arg(long, default_value = ".")]
    pub path: String,

    /// Emit the stable lean output grammar instead of human-readable output
    #[arg(long)]
    pub plain: bool,
}

pub async fn exec(args: SymbolArgs) -> anyhow::Result<()> {
    let resolved = crate::shared::project::resolve_project_root(std::path::Path::new(&args.path))?;
    let project_root = resolved.state_root;
    let source_root = resolved.source_root;
    let search_scope = SearchScope::from_worktree_context(&resolved.worktree_context);
    let db_path = project_db_path(&project_root);

    warn_if_degraded_branch_context(&search_scope);

    if let Ok(pid) = project::read_project_id(&project_root) {
        if let Err(e) = lifecycle::ensure_daemon(&pid, &project_root, &source_root) {
            tracing::debug!("auto-start daemon skipped: {e}");
        }
    }

    if !db_path.exists() {
        anyhow::bail!(
            "no current index found at {}. Run `1up reindex` to create a fresh schema-v5 index.",
            db_path.display()
        );
    }

    let db = Db::open_ro(&db_path).await?;
    let conn = db.connect()?;
    schema::ensure_current(&conn).await?;

    let engine = SymbolSearchEngine::new_scoped(&conn, search_scope);
    let results = if args.references {
        engine.find_references(&args.name, args.fuzzy).await?
    } else {
        engine.find_definitions(&args.name, args.fuzzy).await?
    };

    if results.is_empty() && !args.fuzzy {
        eprintln!("No exact match found. Use --fuzzy for approximate matching.");
    }

    let mut stdout = io::stdout().lock();
    if args.plain {
        lean::render_symbol(&mut stdout, &results)?;
    } else {
        discovery_output::render_symbol(
            &mut stdout,
            &args.name,
            args.references,
            args.fuzzy,
            &results,
        )?;
    }
    stdout.flush()?;
    Ok(())
}

fn warn_if_degraded_branch_context(scope: &SearchScope) {
    if let Some(reason) = scope.degraded_reason() {
        eprintln!("warning: {reason}");
    }
}
