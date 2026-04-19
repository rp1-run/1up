use std::io::{self, Write};

use clap::Args;

use crate::cli::lean;
use crate::daemon::lifecycle;
use crate::search::StructuralSearchEngine;
use crate::shared::config::project_db_path;
use crate::shared::project;
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

pub async fn exec(args: StructuralArgs) -> anyhow::Result<()> {
    let resolved = crate::shared::project::resolve_project_root(std::path::Path::new(&args.path))?;
    let state_root = &resolved.state_root;
    let source_root = &resolved.source_root;
    let db_path = project_db_path(state_root);

    if let Ok(pid) = project::read_project_id(state_root) {
        if let Err(e) = lifecycle::ensure_daemon(&pid, state_root) {
            tracing::debug!("auto-start daemon skipped: {e}");
        }
    }

    let lang_filter = args.language.as_deref();

    let results = if db_path.exists() {
        let db = Db::open_ro(&db_path).await?;
        let conn = db.connect()?;
        schema::ensure_current(&conn).await?;

        let engine = StructuralSearchEngine::new(source_root, Some(&conn));
        engine.search(&args.pattern, lang_filter).await?
    } else {
        eprintln!(
            "warning: no index found at {}. Scanning files directly.",
            db_path.display()
        );
        let engine = StructuralSearchEngine::new(source_root, None);
        engine.search(&args.pattern, lang_filter).await?
    };

    let mut stdout = io::stdout().lock();
    lean::render_structural(&mut stdout, &results)?;
    stdout.flush()?;

    Ok(())
}
