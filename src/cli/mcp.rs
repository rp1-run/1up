use std::path::Path;

use clap::Args;

use crate::daemon::lifecycle;
use crate::shared::project;

#[derive(Args)]
pub struct McpArgs {
    /// Project root directory (defaults to current directory)
    #[arg(long, default_value = ".")]
    pub path: String,
}

pub async fn exec(args: McpArgs) -> anyhow::Result<()> {
    let resolved = project::resolve_project_root(Path::new(&args.path))?;
    ensure_daemon_for_mcp(&resolved.state_root);
    crate::mcp::server::serve_stdio(resolved.state_root, resolved.source_root).await
}

fn ensure_daemon_for_mcp(project_root: &Path) {
    if !lifecycle::supports_daemon() {
        return;
    }

    let project_id = match project::read_project_id(project_root) {
        Ok(project_id) => project_id,
        Err(_) => match project::write_project_id(project_root) {
            Ok(project_id) => project_id,
            Err(err) => {
                tracing::debug!(
                    "MCP daemon auto-start skipped; failed to initialize project: {err}"
                );
                return;
            }
        },
    };

    if let Err(err) = lifecycle::ensure_daemon(&project_id, project_root) {
        tracing::debug!("MCP daemon auto-start skipped: {err}");
    }
}
