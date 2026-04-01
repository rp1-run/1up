use clap::Args;

use crate::cli::output::formatter_for;
use crate::daemon::lifecycle;
use crate::search::StructuralSearchEngine;
use crate::shared::config::project_db_path;
use crate::shared::project;
use crate::shared::types::OutputFormat;
use crate::storage::db::Db;
use crate::storage::schema;

#[derive(Args)]
pub struct StructuralArgs {
    /// Tree-sitter query pattern (S-expression syntax)
    pub pattern: String,

    /// Filter to a specific language (e.g. rust, python, go)
    #[arg(long, short)]
    pub language: Option<String>,

    /// Project root directory (defaults to current directory)
    #[arg(long, default_value = ".")]
    pub path: String,
}

pub async fn exec(args: StructuralArgs, format: OutputFormat) -> anyhow::Result<()> {
    let project_root = std::path::Path::new(&args.path).canonicalize()?;
    let db_path = project_db_path(&project_root);
    let fmt = formatter_for(format);

    if let Ok(pid) = project::read_project_id(&project_root) {
        if let Err(e) = lifecycle::ensure_daemon(&pid, &project_root) {
            tracing::debug!("auto-start daemon skipped: {e}");
        }
    }

    let lang_filter = args.language.as_deref();

    if db_path.exists() {
        let db = Db::open_ro(&db_path).await?;
        let conn = db.connect()?;
        schema::migrate(&conn).await?;

        let engine = StructuralSearchEngine::new(&project_root, Some(&conn));
        let results = engine.search(&args.pattern, lang_filter).await?;
        println!("{}", fmt.format_structural_results(&results));
    } else {
        eprintln!(
            "warning: no index found at {}. Scanning files directly.",
            db_path.display()
        );
        let engine = StructuralSearchEngine::new(&project_root, None);
        let results = engine.search(&args.pattern, lang_filter).await?;
        println!("{}", fmt.format_structural_results(&results));
    }

    Ok(())
}
