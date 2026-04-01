use clap::Args;

use crate::cli::output::formatter_for;
use crate::shared::types::OutputFormat;

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
    let _project_root = std::path::Path::new(&args.path).canonicalize()?;
    let fmt = formatter_for(format);
    println!(
        "{}",
        fmt.format_message(&format!("symbol lookup for '{}': not yet implemented", args.name))
    );
    Ok(())
}
