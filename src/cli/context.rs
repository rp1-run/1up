use clap::Args;

use crate::cli::output::formatter_for;
use crate::search::context::{parse_location, ContextEngine};
use crate::shared::types::OutputFormat;

#[derive(Args)]
pub struct ContextArgs {
    /// File location in file:line format
    pub location: String,

    /// Project root directory (defaults to current directory)
    #[arg(long, default_value = ".")]
    pub path: String,

    /// Context expansion window in lines (used when tree-sitter scope unavailable)
    #[arg(long)]
    pub expansion: Option<usize>,
}

pub async fn exec(args: ContextArgs, format: OutputFormat) -> anyhow::Result<()> {
    let project_root = std::path::Path::new(&args.path).canonicalize()?;
    let fmt = formatter_for(format);

    let (file_str, line) = parse_location(&args.location)?;
    let file_path = project_root.join(&file_str);

    if !file_path.exists() {
        let abs = std::path::Path::new(&file_str);
        if abs.is_absolute() && abs.exists() {
            let result = ContextEngine::retrieve(abs, line, args.expansion)?;
            println!("{}", fmt.format_context_result(&result));
            return Ok(());
        }
        anyhow::bail!("file not found: {}", file_path.display());
    }

    let result = ContextEngine::retrieve(&file_path, line, args.expansion)?;
    println!("{}", fmt.format_context_result(&result));
    Ok(())
}
