use clap::Args;

use crate::cli::output::formatter_for;
use crate::shared::types::OutputFormat;

#[derive(Args)]
pub struct InitArgs {
    /// Project root directory (defaults to current directory)
    #[arg(default_value = ".")]
    pub path: String,

    /// Output format override (defaults to plain)
    #[arg(long, short = 'f')]
    pub format: Option<OutputFormat>,
}

pub async fn exec(args: InitArgs, format: OutputFormat) -> anyhow::Result<()> {
    let project_root = std::path::Path::new(&args.path).canonicalize()?;
    let fmt = formatter_for(format);

    if crate::shared::project::is_initialized(&project_root) {
        let msg = format!("Project already initialized at {}", project_root.display());
        eprintln!("{}", fmt.format_message(&msg));
        return Ok(());
    }

    let (id, initialized_now) = crate::shared::project::ensure_project_id(&project_root)?;
    if !initialized_now {
        let msg = format!("Project already initialized at {}", project_root.display());
        eprintln!("{}", fmt.format_message(&msg));
        return Ok(());
    }

    let msg = format!("Initialized project {} at {}", id, project_root.display());
    println!("{}", fmt.format_message(&msg));
    Ok(())
}
