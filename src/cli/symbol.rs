use std::io::{self, Write};

use clap::Args;

use crate::cli::lean;
use crate::daemon::lifecycle;
use crate::search::SymbolSearchEngine;
use crate::shared::config::project_db_path;
use crate::shared::project;
use crate::shared::types::OutputFormat;
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
}

pub async fn exec(args: SymbolArgs, _format: OutputFormat) -> anyhow::Result<()> {
    let resolved = crate::shared::project::resolve_project_root(std::path::Path::new(&args.path))?;
    let project_root = resolved.state_root;
    let db_path = project_db_path(&project_root);

    if let Ok(pid) = project::read_project_id(&project_root) {
        if let Err(e) = lifecycle::ensure_daemon(&pid, &project_root) {
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

    let engine = SymbolSearchEngine::new(&conn);
    let results = if args.references {
        engine.find_references(&args.name, args.fuzzy).await?
    } else {
        engine.find_definitions(&args.name, args.fuzzy).await?
    };

    if results.is_empty() && !args.fuzzy {
        eprintln!("No exact match found. Use --fuzzy for approximate matching.");
    }

    let mut stdout = io::stdout().lock();
    lean::render_symbol(&mut stdout, &results)?;
    stdout.flush()?;
    Ok(())
}
