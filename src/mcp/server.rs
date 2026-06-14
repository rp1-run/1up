use std::path::PathBuf;

use rmcp::{
    handler::server::router::tool::ToolRouter,
    model::{Implementation, ServerCapabilities, ServerInfo},
    tool_handler,
    transport::stdio,
    ServerHandler, ServiceExt,
};

const SERVER_GUIDANCE: &str = "Use 1up as the primary code-search interface for the configured repository. Call oneup_overview first when starting work on an unfamiliar repository to retrieve a deterministic orientation digest of repository statistics, most-referenced types, modules, and entry points. For questions about where behavior lives, how code works, implementation patterns, or symbol relationships, start with oneup_status when readiness is unknown, use oneup_start only when indexing or rebuilding is needed, then call oneup_search before raw grep, rg, find, or broad file reads. Hydrate selected search results with oneup_get handles before relying on them, and use oneup_context for file-line context. Use oneup_symbol for definitions, references, and completeness checks around a known symbol. Use oneup_impact only for explicit blast-radius questions after the core status/search/get/symbol/context loop has produced evidence, and use oneup_structural for explicit tree-sitter pattern searches. Use raw file reads, grep, rg, or find only after 1up narrows the scope, or for exact literal verification. oneup_search is ranked discovery, not exhaustive proof.";

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

#[cfg(test)]
mod tests {
    use super::*;

    /// Claude Code truncates the MCP `instructions` field at 2KB.
    const INSTRUCTIONS_BUDGET_BYTES: usize = 2048;

    /// Stable routing substring that must survive truncation so agents keep
    /// the "prefer 1up over raw search" rule even under an adverse 2KB cut.
    const ROUTING_GUIDANCE: &str = "before raw grep";

    /// Realistic long state/source roots so the measurement includes the
    /// path-dependent suffix `instructions()` appends, not just the static
    /// `SERVER_GUIDANCE` constant.
    fn server_with_realistic_long_paths() -> OneupMcpServer {
        let state_root = PathBuf::from(
            "/Users/some-developer/Development/workspaces/example-organization/example-monorepo-with-a-long-name",
        );
        let source_root = PathBuf::from(
            "/Users/some-developer/Development/workspaces/example-organization/example-monorepo-with-a-long-name/.worktrees/feature-branch-with-a-descriptive-name",
        );
        OneupMcpServer::new(state_root, source_root)
    }

    #[test]
    fn rendered_instructions_stay_within_2kb_budget() {
        let rendered = server_with_realistic_long_paths().instructions();
        assert!(
            rendered.len() <= INSTRUCTIONS_BUDGET_BYTES,
            "rendered MCP instructions are {} bytes, exceeding the {}-byte budget Claude Code truncates at",
            rendered.len(),
            INSTRUCTIONS_BUDGET_BYTES
        );
    }

    #[test]
    fn routing_guidance_survives_2kb_truncation() {
        let rendered = server_with_realistic_long_paths().instructions();
        let offset = rendered.find(ROUTING_GUIDANCE).unwrap_or_else(|| {
            panic!("routing guidance {ROUTING_GUIDANCE:?} must be present in rendered instructions")
        });
        let end = offset + ROUTING_GUIDANCE.len();
        assert!(
            end <= INSTRUCTIONS_BUDGET_BYTES,
            "routing guidance ends at byte {end} but must fall within the first {INSTRUCTIONS_BUDGET_BYTES} bytes to survive truncation"
        );
    }
}
