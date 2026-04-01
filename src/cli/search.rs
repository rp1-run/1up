use clap::Args;

use crate::cli::output::formatter_for;
use crate::shared::types::OutputFormat;

#[derive(Args)]
pub struct SearchArgs {
    /// Search query
    pub query: String,

    /// Maximum number of results
    #[arg(long, short = 'n', default_value = "20")]
    pub limit: usize,

    /// Project root directory (defaults to current directory)
    #[arg(long, default_value = ".")]
    pub path: String,
}

pub async fn exec(args: SearchArgs, format: OutputFormat) -> anyhow::Result<()> {
    let _project_root = std::path::Path::new(&args.path).canonicalize()?;
    let fmt = formatter_for(format);
    println!(
        "{}",
        fmt.format_message(&format!("search for '{}': not yet implemented", args.query))
    );
    Ok(())
}
