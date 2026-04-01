use clap::Args;

use crate::cli::output::formatter_for;
use crate::shared::types::OutputFormat;

#[derive(Args)]
pub struct ContextArgs {
    /// File location in file:line format
    pub location: String,

    /// Project root directory (defaults to current directory)
    #[arg(long, default_value = ".")]
    pub path: String,
}

pub async fn exec(args: ContextArgs, format: OutputFormat) -> anyhow::Result<()> {
    let _project_root = std::path::Path::new(&args.path).canonicalize()?;
    let fmt = formatter_for(format);
    println!(
        "{}",
        fmt.format_message(&format!(
            "context retrieval for '{}': not yet implemented",
            args.location
        ))
    );
    Ok(())
}
