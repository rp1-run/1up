use clap::Args;

use crate::cli::output::formatter_for;
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

    /// Project root directory (defaults to current directory)
    #[arg(long, default_value = ".")]
    pub path: String,
}

pub async fn exec(args: SymbolArgs, format: OutputFormat) -> anyhow::Result<()> {
    let project_root = std::path::Path::new(&args.path).canonicalize()?;
    let db_path = project_db_path(&project_root);
    let fmt = formatter_for(format);

    if let Ok(pid) = project::read_project_id(&project_root) {
        if let Err(e) = lifecycle::ensure_daemon(&pid, &project_root) {
            tracing::debug!("auto-start daemon skipped: {e}");
        }
    }

    if !db_path.exists() {
        eprintln!(
            "warning: no index found at {}. Run `1up index` first.",
            db_path.display()
        );
        println!("{}", fmt.format_symbol_results(&[]));
        return Ok(());
    }

    let db = Db::open_ro(&db_path).await?;
    let conn = db.connect()?;
    schema::ensure_compatible(&conn).await?;

    let engine = SymbolSearchEngine::new(&conn);
    let results = if args.references {
        engine.find_references(&args.name).await?
    } else {
        engine.find_definitions(&args.name).await?
    };

    println!("{}", fmt.format_symbol_results(&results));
    Ok(())
}
