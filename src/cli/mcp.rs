use std::path::Path;

use clap::Args;

use crate::shared::project;

#[derive(Args)]
pub struct McpArgs {
    /// Project root directory (defaults to current directory)
    #[arg(long, default_value = ".")]
    pub path: String,
}

pub async fn exec(args: McpArgs) -> anyhow::Result<()> {
    let resolved = project::resolve_project_root(Path::new(&args.path))?;
    crate::mcp::server::serve_stdio(resolved.state_root, resolved.source_root).await
}
