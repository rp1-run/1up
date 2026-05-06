use std::path::PathBuf;

use rmcp::{
    handler::server::router::tool::ToolRouter,
    model::{Implementation, ServerCapabilities, ServerInfo},
    tool_handler,
    transport::stdio,
    ServerHandler, ServiceExt,
};

const SERVER_GUIDANCE: &str = "Use 1up as the primary code-search interface for the configured repository. For questions about where behavior lives, how code works, implementation patterns, or symbol relationships, start with oneup_status when readiness is unknown, use oneup_start only when indexing or rebuilding is needed, then call oneup_search before raw grep, rg, find, or broad file reads. Hydrate selected search results with oneup_get handles before relying on them, and use oneup_context for file-line context. Use oneup_symbol for definitions, references, and completeness checks around a known symbol. Use oneup_impact only for explicit blast-radius questions after the core status/search/get/symbol/context loop has produced evidence, and use oneup_structural for explicit tree-sitter pattern searches. Use raw file reads, grep, rg, or find only after 1up narrows the scope, or for exact literal verification. oneup_search is ranked discovery, not exhaustive proof.";

#[derive(Debug, Clone)]
pub(crate) struct OneupMcpServer {
    pub(crate) state_root: PathBuf,
    pub(crate) source_root: PathBuf,
    pub(crate) tool_router: ToolRouter<Self>,
}

impl OneupMcpServer {
    fn new(state_root: PathBuf, source_root: PathBuf) -> Self {
        Self {
            state_root,
            source_root,
            tool_router: Self::tool_router(),
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

#[tool_handler(router = self.tool_router)]
impl ServerHandler for OneupMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                Implementation::new("1up", env!("CARGO_PKG_VERSION"))
                    .with_title("1up MCP")
                    .with_description("Primary local code search and discovery MCP server"),
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
