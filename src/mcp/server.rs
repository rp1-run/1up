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
        // The routing rule must not merely fit within the budget — it must be
        // *front-loaded with margin* so it survives a 2KB truncation even as the
        // guidance grows. We assert it lands within the first HALF of the budget
        // rather than `<= INSTRUCTIONS_BUDGET_BYTES`: the full-budget bound is
        // already implied by `rendered_instructions_stay_within_2kb_budget` (the
        // phrase end can't exceed the total length, which that test caps at 2048),
        // so it could never fail on its own. A half-budget margin gives this test
        // real, independent teeth — reordering SERVER_GUIDANCE so the routing rule
        // drifts past the midpoint fails here, long before growth would make a
        // truncation actually cut the rule off.
        const FRONT_LOAD_BUDGET_BYTES: usize = INSTRUCTIONS_BUDGET_BYTES / 2;

        // Measure against the REAL rendered instructions (guidance + path suffix),
        // not the static constant, so the offset reflects what a host receives.
        let rendered = server_with_realistic_long_paths().instructions();
        let offset = rendered.find(ROUTING_GUIDANCE).unwrap_or_else(|| {
            panic!("routing guidance {ROUTING_GUIDANCE:?} must be present in rendered instructions")
        });
        let routing_end = offset + ROUTING_GUIDANCE.len();
        assert!(
            routing_end <= FRONT_LOAD_BUDGET_BYTES,
            "routing guidance {ROUTING_GUIDANCE:?} ends at byte {routing_end} of the rendered \
             instructions, past the {FRONT_LOAD_BUDGET_BYTES}-byte front-load margin (half of the \
             {INSTRUCTIONS_BUDGET_BYTES}-byte truncation budget); keep the routing rule near the \
             top of SERVER_GUIDANCE so it survives a 2KB cut even as guidance grows"
        );
    }

    /// Drift guard for the agent-facing MCP guidance: every `oneup_*` tool token
    /// referenced in the shipped instructions must be a currently-retained tool.
    /// `SERVER_GUIDANCE` is interpolated into `instructions()` and sent to agents,
    /// but the repo-wide doc-token pin test in `tests/release_assets_tests.rs`
    /// only scans markdown docs, so without this a future stale token here (e.g.
    /// the removed `oneup_prepare`/`oneup_read`) would ship uncaught. Mirrors that
    /// test's plain byte scan (literal `oneup_` prefix followed by a maximal
    /// `[a-z_]` run) to avoid adding a `regex` dependency for one guard.
    #[test]
    fn server_guidance_tokens_match_retained_public_tools() {
        use crate::mcp::types::RETAINED_PUBLIC_TOOLS;

        // Scan both the raw constant and the fully rendered instructions so any
        // token introduced via interpolation is covered too.
        let rendered = server_with_realistic_long_paths().instructions();
        for source in [SERVER_GUIDANCE, rendered.as_str()] {
            for token in extract_oneup_tokens(source) {
                assert!(
                    RETAINED_PUBLIC_TOOLS.contains(&token.as_str()),
                    "MCP guidance references unknown tool `{token}` not in \
                     RETAINED_PUBLIC_TOOLS (authority: src/mcp/types.rs); correct the \
                     guidance string or update the retained tool set"
                );
            }
        }
    }

    /// Extract every `oneup_[a-z_]+` token from `content`: a literal `oneup_`
    /// prefix followed by a maximal run of `[a-z_]`, scanned left-to-right with
    /// non-overlapping matches. Kept as a plain byte scan (no `regex` dependency)
    /// and matches the extractor used by the documentation pin test in
    /// `tests/release_assets_tests.rs`.
    fn extract_oneup_tokens(content: &str) -> Vec<String> {
        const PREFIX: &str = "oneup_";
        let bytes = content.as_bytes();
        let mut tokens = Vec::new();
        let mut search_from = 0;
        while let Some(rel) = content[search_from..].find(PREFIX) {
            let start = search_from + rel;
            let mut end = start + PREFIX.len();
            while end < bytes.len() && (bytes[end].is_ascii_lowercase() || bytes[end] == b'_') {
                end += 1;
            }
            if end > start + PREFIX.len() {
                tokens.push(content[start..end].to_string());
                search_from = end;
            } else {
                search_from = start + PREFIX.len();
            }
        }
        tokens
    }
}
