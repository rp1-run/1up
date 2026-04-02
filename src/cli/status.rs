use clap::Args;

use crate::cli::output::{formatter_for, StatusInfo};
use crate::daemon::lifecycle;
use crate::shared::config;
use crate::shared::project;
use crate::shared::types::OutputFormat;
use crate::storage::db::Db;
use crate::storage::schema;
use crate::storage::segments;

#[derive(Args)]
pub struct StatusArgs {
    /// Project root directory (defaults to current directory)
    #[arg(default_value = ".")]
    pub path: String,
}

pub async fn exec(args: StatusArgs, format: OutputFormat) -> anyhow::Result<()> {
    let project_root = std::path::Path::new(&args.path).canonicalize()?;
    let fmt = formatter_for(format);

    let (daemon_running, pid) = match lifecycle::is_daemon_running()? {
        Some(pid) => (true, Some(pid)),
        None => (false, None),
    };

    let project_id = project::read_project_id(&project_root).ok();

    let (indexed_files, total_segments) = {
        let db_path = config::project_db_path(&project_root);
        if db_path.exists() {
            match Db::open_ro(&db_path).await {
                Ok(db) => match db.connect() {
                    Ok(conn) => {
                        if schema::ensure_current(&conn).await.is_ok() {
                            let files = segments::count_files(&conn).await.ok();
                            let segs = segments::count_segments(&conn).await.ok();
                            (files, segs)
                        } else {
                            (None, None)
                        }
                    }
                    Err(_) => (None, None),
                },
                Err(_) => (None, None),
            }
        } else {
            (None, None)
        }
    };

    let status = StatusInfo {
        daemon_running,
        pid,
        indexed_files,
        total_segments,
        project_id,
    };

    println!("{}", fmt.format_status(&status));
    Ok(())
}
