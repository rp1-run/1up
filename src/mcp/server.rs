use std::path::PathBuf;

use rmcp::{
    model::{Implementation, ServerCapabilities, ServerInfo},
    transport::stdio,
    ServerHandler, ServiceExt,
};

const SERVER_GUIDANCE: &str = "Use 1up for local code discovery, reading selected code, symbol verification, and likely-impact exploration in the configured repository. Use search for discovery, read to hydrate handles or file locations, symbol when completeness matters, and impact for advisory follow-up targets.";

#[derive(Debug)]
struct OneupMcpServer {
    state_root: PathBuf,
    source_root: PathBuf,
}

impl OneupMcpServer {
    fn new(state_root: PathBuf, source_root: PathBuf) -> Self {
        Self {
            state_root,
            source_root,
        }
    }

    fn instructions(&self) -> String {
        format!(
            "{SERVER_GUIDANCE} Configured repository: {}. Local index state: {}.",
            self.source_root.display(),
            self.state_root.display()
        )
    }
}

impl ServerHandler for OneupMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                Implementation::new("1up", env!("CARGO_PKG_VERSION"))
                    .with_title("1up MCP")
                    .with_description("Local code discovery MCP server"),
            )
            .with_instructions(self.instructions())
    }
}

pub async fn serve_stdio(state_root: PathBuf, source_root: PathBuf) -> anyhow::Result<()> {
    let service = OneupMcpServer::new(state_root, source_root);
    let running = service.serve(stdio()).await?;
    let _quit_reason = running.waiting().await?;
    Ok(())
}
