use clap::Args;

use crate::cli::output::formatter_for;
use crate::shared::types::OutputFormat;

#[derive(Args)]
pub struct StopArgs {
    /// Project root directory (defaults to current directory)
    #[arg(default_value = ".")]
    pub path: String,
}

pub async fn exec(args: StopArgs, format: OutputFormat) -> anyhow::Result<()> {
    let _project_root = std::path::Path::new(&args.path).canonicalize()?;
    let fmt = formatter_for(format);
    println!("{}", fmt.format_message("stop: not yet implemented"));
    Ok(())
}
