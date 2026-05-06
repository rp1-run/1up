use std::path::Path;

use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content},
    tool, tool_router,
};
use serde::Serialize;
use serde_json::{json, Value};

use crate::mcp::ops::{
    self, McpProjectRoots, OperationStatus, ReadLocation, ReadPayload, ReadinessPayload,
    ReadinessStatus, SearchPayload, SymbolInclude, SymbolLookupRequest, SymbolPayload,
};
use crate::mcp::server::OneupMcpServer;
use crate::mcp::types::{
    ContextInput, GetInput, ImpactInput, NextAction, ReadinessContextMetadata, SearchInput,
    StartInput, StatusInput, StructuralInput, SymbolIncludeInput, SymbolInput, ToolEnvelope,
    RETAINED_PUBLIC_TOOLS, TOOL_CONTEXT, TOOL_GET, TOOL_IMPACT, TOOL_SEARCH, TOOL_START,
    TOOL_STATUS, TOOL_STRUCTURAL, TOOL_SYMBOL,
};
use crate::search::impact::{ImpactAnchor, ImpactRequest, ImpactResultEnvelope, ImpactStatus};
use crate::shared::constants::MAX_SEARCH_RESULTS;
use crate::shared::types::{
    BranchStatus, DaemonRefreshState, DaemonWatchStatus, IndexState, StructuralResult,
    StructuralSearchReport, StructuralSearchStatus, WorktreeContext,
};

const DEFAULT_SEARCH_LIMIT: usize = 5;
const MCP_HANDLE_DISPLAY_LEN: usize = 12;
const MCP_FIELD_SEP: &str = "  ";
const MCP_PLACEHOLDER: &str = "-";

#[tool_router(router = tool_router, vis = "pub(crate)")]
impl OneupMcpServer {
    #[tool(
        name = "oneup_status",
        description = "Check 1up index readiness for the configured repository without indexing. Call first when readiness is unknown, then follow the returned retained oneup action.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<ToolEnvelope>().unwrap(),
        annotations(title = "Check 1up Status", read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    pub async fn oneup_status(&self, Parameters(input): Parameters<StatusInput>) -> CallToolResult {
        let roots = match self.roots(input.path.as_deref()) {
            Ok(roots) => roots,
            Err(err) => {
                let path = input.path.as_deref().unwrap_or(".");
                let payload = ops::blocked_readiness_for_path(path, err.to_string());
                return result(readiness_result(payload, None));
            }
        };

        let mut payload = ops::check_status(&roots).await;
        apply_branch_readiness(&mut payload, &roots.worktree_context);
        let metadata = readiness_context_metadata(&roots, &payload);

        result(readiness_result(payload, Some(metadata)))
    }

    #[tool(
        name = "oneup_start",
        description = "Prepare the configured repository for 1up discovery by creating, refreshing, or rebuilding the local index when explicitly requested.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<ToolEnvelope>().unwrap(),
        annotations(title = "Start 1up", destructive_hint = false, idempotent_hint = false, open_world_hint = false)
    )]
    pub async fn oneup_start(&self, Parameters(input): Parameters<StartInput>) -> CallToolResult {
        let roots = match self.roots(input.path.as_deref()) {
            Ok(roots) => roots,
            Err(err) => {
                let path = input.path.as_deref().unwrap_or(".");
                let payload = ops::blocked_readiness_for_path(path, err.to_string());
                return result(readiness_result(payload, None));
            }
        };

        let mut payload = match ops::start(&roots, input.mode).await {
            Ok(payload) => payload,
            Err(err) => return indexed_tool_error(err.to_string()),
        };
        apply_branch_readiness(&mut payload, &roots.worktree_context);
        let metadata = readiness_context_metadata(&roots, &payload);

        result(readiness_result(payload, Some(metadata)))
    }

    #[tool(
        name = "oneup_search",
        description = "Search source code by meaning as the primary discovery path for code questions. Call before raw grep, rg, find, or broad file reads, then hydrate handles, inspect file-line context, or verify symbols from the returned actions.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<ToolEnvelope>().unwrap(),
        annotations(title = "Search Code", read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    pub async fn oneup_search(&self, Parameters(input): Parameters<SearchInput>) -> CallToolResult {
        if input.query.trim().is_empty() {
            return error_result(
                "error",
                "query cannot be empty",
                vec![action(
                    TOOL_SEARCH,
                    "Retry with a natural-language code discovery query.",
                    json!({ "query": "<code discovery query>" }),
                )],
            );
        }

        let roots = match self.roots(input.path.as_deref()) {
            Ok(roots) => roots,
            Err(err) => return error_result("error", err.to_string(), vec![]),
        };
        let limit = input
            .limit
            .unwrap_or(DEFAULT_SEARCH_LIMIT)
            .clamp(1, MAX_SEARCH_RESULTS);

        match ops::run_search(
            &roots.state_root,
            &roots.worktree_context,
            &input.query,
            limit,
        )
        .await
        {
            Ok(payload) => {
                let summary = search_summary(&payload, &input.query);
                let next_actions = search_next_actions(&payload);
                result(envelope(
                    status_string(&payload.status),
                    summary,
                    payload_value(&payload),
                    next_actions,
                ))
            }
            Err(err) => indexed_tool_error(err.to_string()),
        }
    }

    #[tool(
        name = "oneup_get",
        description = "Hydrate selected code segments from oneup_search or oneup_symbol handles. Use before answering, citing, or editing discovered code.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<ToolEnvelope>().unwrap(),
        annotations(title = "Get Code", read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    pub async fn oneup_get(&self, Parameters(input): Parameters<GetInput>) -> CallToolResult {
        if input.handles.is_empty() {
            return error_result(
                "error",
                "provide at least one handle",
                vec![action(
                    TOOL_SEARCH,
                    "Search first to obtain durable handles for oneup_get.",
                    json!({ "query": "<code discovery query>" }),
                )],
            );
        }
        let roots = match self.roots(input.path.as_deref()) {
            Ok(roots) => roots,
            Err(err) => return error_result("error", err.to_string(), vec![]),
        };

        match ops::get_handles(&roots.state_root, &input.handles).await {
            Ok(payload) => {
                let summary = read_summary(&payload);
                let next_actions = read_next_actions(&payload);
                call_result(
                    envelope(
                        status_string(&payload.status),
                        summary,
                        payload_value(&payload),
                        next_actions,
                    ),
                    all_read_records_failed(&payload),
                )
            }
            Err(err) => indexed_tool_error(err.to_string()),
        }
    }

    #[tool(
        name = "oneup_context",
        description = "Retrieve repository-scoped file-line context from precise source locations. Use after search, get, or symbol evidence identifies relevant lines.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<ToolEnvelope>().unwrap(),
        annotations(title = "Read Context", read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    pub async fn oneup_context(
        &self,
        Parameters(input): Parameters<ContextInput>,
    ) -> CallToolResult {
        if input.locations.is_empty() {
            return error_result(
                "error",
                "provide at least one precise location",
                vec![action(
                    TOOL_SEARCH,
                    "Search first to find relevant source locations for oneup_context.",
                    json!({ "query": "<code discovery query>" }),
                )],
            );
        }
        let roots = match self.roots(input.path.as_deref()) {
            Ok(roots) => roots,
            Err(err) => return error_result("error", err.to_string(), vec![]),
        };

        let locations = input
            .locations
            .iter()
            .map(|location| ReadLocation {
                path: location.path.clone(),
                line: location.line,
                expansion: location.expansion,
            })
            .collect::<Vec<_>>();

        match ops::read_context_locations(&roots.source_root, &locations) {
            Ok(payload) => {
                let summary = read_summary(&payload);
                let next_actions = read_next_actions(&payload);
                call_result(
                    envelope(
                        status_string(&payload.status),
                        summary,
                        payload_value(&payload),
                        next_actions,
                    ),
                    all_read_records_failed(&payload),
                )
            }
            Err(err) => error_result("error", err.to_string(), vec![]),
        }
    }

    #[tool(
        name = "oneup_symbol",
        description = "Find definitions and references for a known symbol. Use after search, get, or context when completeness matters, then hydrate returned handles or locations for evidence.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<ToolEnvelope>().unwrap(),
        annotations(title = "Verify Symbol", read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    pub async fn oneup_symbol(&self, Parameters(input): Parameters<SymbolInput>) -> CallToolResult {
        if input.name.trim().is_empty() {
            return error_result(
                "error",
                "symbol name cannot be empty",
                vec![action(
                    TOOL_SEARCH,
                    "Search for the behavior first, then verify a discovered symbol.",
                    json!({ "query": "<behavior or symbol intent>" }),
                )],
            );
        }

        let roots = match self.roots(input.path.as_deref()) {
            Ok(roots) => roots,
            Err(err) => return error_result("error", err.to_string(), vec![]),
        };
        let request = SymbolLookupRequest {
            name: input.name.clone(),
            include: symbol_include(input.include),
            fuzzy: input.fuzzy,
        };

        match ops::lookup_symbol(&roots.state_root, &roots.worktree_context, request).await {
            Ok(payload) => {
                let summary = symbol_summary(&payload, &input.name);
                let next_actions = symbol_next_actions(&payload, &input.name);
                result(envelope(
                    status_string(&payload.status),
                    summary,
                    payload_value(&payload),
                    next_actions,
                ))
            }
            Err(err) => indexed_tool_error(err.to_string()),
        }
    }

    #[tool(
        name = "oneup_impact",
        description = "Explore likely affected code from a result handle, symbol, or file anchor. Use for explicit blast-radius questions after the core status, search, get, symbol, and context loop.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<ToolEnvelope>().unwrap(),
        annotations(title = "Explore Impact", read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    pub async fn oneup_impact(&self, Parameters(input): Parameters<ImpactInput>) -> CallToolResult {
        let request = match impact_request(&input) {
            Ok(request) => request,
            Err(message) => {
                return error_result(
                    "error",
                    message,
                    vec![
                        action(
                            TOOL_SEARCH,
                            "Search to obtain a precise result handle for impact exploration.",
                            json!({ "query": "<code discovery query>" }),
                        ),
                        action(
                            TOOL_SYMBOL,
                            "Verify a known symbol before using it as an impact anchor.",
                            json!({ "name": "<symbol>", "include": "both" }),
                        ),
                    ],
                );
            }
        };

        let roots = match self.roots(input.path.as_deref()) {
            Ok(roots) => roots,
            Err(err) => return error_result("error", err.to_string(), vec![]),
        };

        match ops::explore_impact(&roots.state_root, &roots.worktree_context, request).await {
            Ok(payload) => {
                let summary = impact_summary(&payload);
                let next_actions = impact_next_actions(&payload);
                call_result(
                    envelope(
                        status_string(&payload.status),
                        summary,
                        payload_value(&payload),
                        next_actions,
                    ),
                    payload.status == ImpactStatus::Refused,
                )
            }
            Err(err) => indexed_tool_error(err.to_string()),
        }
    }

    #[tool(
        name = "oneup_structural",
        description = "Run a tree-sitter structural query against indexed source for supported languages. Returns structured matches or explicit diagnostics.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<ToolEnvelope>().unwrap(),
        annotations(title = "Structural Search", read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    pub async fn oneup_structural(
        &self,
        Parameters(input): Parameters<StructuralInput>,
    ) -> CallToolResult {
        if input.pattern.trim().is_empty() {
            return error_result(
                "error",
                "structural pattern cannot be empty",
                vec![action(
                    TOOL_STRUCTURAL,
                    "Retry with a tree-sitter query pattern.",
                    json!({ "pattern": "<tree-sitter query pattern>" }),
                )],
            );
        }

        let roots = match self.roots(input.path.as_deref()) {
            Ok(roots) => roots,
            Err(err) => return error_result("error", err.to_string(), vec![]),
        };

        match ops::search_structural(
            &roots.state_root,
            &roots.source_root,
            &roots.worktree_context,
            &input.pattern,
            input.language.as_deref(),
        )
        .await
        {
            Ok(mut payload) => {
                if let Some(limit) = input.limit {
                    payload.results.truncate(limit.clamp(1, MAX_SEARCH_RESULTS));
                }
                let summary = structural_summary(&payload, &input.pattern);
                let next_actions = structural_next_actions(&payload, &input.pattern);
                call_result(
                    envelope(
                        status_string(&payload.status),
                        summary,
                        payload_value(&payload),
                        next_actions,
                    ),
                    payload.status == StructuralSearchStatus::Error,
                )
            }
            Err(err) => indexed_tool_error(err.to_string()),
        }
    }

    fn roots(&self, path: Option<&str>) -> anyhow::Result<McpProjectRoots> {
        match path.filter(|path| !path.trim().is_empty()) {
            Some(path) => ops::resolve_project(Path::new(path)),
            None => Ok(McpProjectRoots {
                state_root: self.state_root.clone(),
                source_root: self.source_root.clone(),
                worktree_context: crate::daemon::registry::registration_context(
                    &self.state_root,
                    &self.source_root,
                ),
            }),
        }
    }
}

fn impact_request(input: &ImpactInput) -> Result<ImpactRequest, String> {
    let anchors = [
        input
            .handle
            .as_ref()
            .filter(|value| !value.trim().is_empty()),
        input
            .symbol
            .as_ref()
            .filter(|value| !value.trim().is_empty()),
        input.file.as_ref().filter(|value| !value.trim().is_empty()),
    ];
    let count = anchors.iter().filter(|anchor| anchor.is_some()).count();
    if count != 1 {
        return Err("provide exactly one impact anchor: handle, symbol, or file".to_string());
    }
    if input.line.is_some()
        && input
            .file
            .as_ref()
            .is_none_or(|file| file.trim().is_empty())
    {
        return Err("line can only be used with a file impact anchor".to_string());
    }

    let anchor = if let Some(handle) = input.handle.as_ref() {
        ImpactAnchor::Segment {
            id: normalize_handle(handle),
        }
    } else if let Some(symbol) = input.symbol.as_ref() {
        ImpactAnchor::Symbol {
            name: symbol.clone(),
        }
    } else {
        ImpactAnchor::File {
            path: input.file.clone().unwrap_or_default(),
            line: input.line,
        }
    };

    Ok(ImpactRequest {
        anchor,
        scope: input.scope.clone(),
        depth: input.depth.unwrap_or_default(),
        limit: input.limit.unwrap_or_default(),
    })
}

fn symbol_include(include: SymbolIncludeInput) -> SymbolInclude {
    match include {
        SymbolIncludeInput::Definitions => SymbolInclude::Definitions,
        SymbolIncludeInput::References => SymbolInclude::References,
        SymbolIncludeInput::Both => SymbolInclude::Both,
    }
}

fn normalize_handle(raw: &str) -> String {
    raw.trim()
        .strip_prefix(':')
        .unwrap_or(raw.trim())
        .to_string()
}

fn apply_branch_readiness(payload: &mut ReadinessPayload, context: &WorktreeContext) {
    if context.branch_status == BranchStatus::Named {
        return;
    }

    let branch_reason = format!(
        "branch_status is {}; search results may not be definitively branch-filtered",
        context.branch_status.as_str()
    );

    if payload.status == ReadinessStatus::Ready {
        payload.status = ReadinessStatus::Degraded;
        payload.summary =
            "The index is readable, but the active branch context is ambiguous.".to_string();
        payload.reason = Some(branch_reason);
    } else if payload.status == ReadinessStatus::Degraded {
        payload.reason = Some(match payload.reason.take() {
            Some(existing) if !existing.contains(&branch_reason) => {
                format!("{existing}; {branch_reason}")
            }
            Some(existing) => existing,
            None => branch_reason,
        });
    }
}

fn readiness_context_metadata(
    roots: &McpProjectRoots,
    payload: &ReadinessPayload,
) -> ReadinessContextMetadata {
    let context_status = crate::cli::project_status_files::read_daemon_context_status(
        &roots.state_root,
        &roots.worktree_context.context_id,
    );
    let progress_update_state = match payload
        .index_progress
        .as_ref()
        .map(|progress| progress.state)
    {
        Some(IndexState::Running) => Some(DaemonRefreshState::Running),
        Some(IndexState::Complete) => Some(DaemonRefreshState::Complete),
        _ => None,
    };
    let last_update_state = context_status
        .as_ref()
        .map(|status| match status.last_refresh_state {
            DaemonRefreshState::Unknown => {
                progress_update_state.unwrap_or(DaemonRefreshState::Unknown)
            }
            state => state,
        });
    let last_update_state = last_update_state
        .unwrap_or_else(|| progress_update_state.unwrap_or(DaemonRefreshState::Unknown));

    ReadinessContextMetadata {
        context_id: roots.worktree_context.context_id.clone(),
        main_worktree_root: roots.worktree_context.main_worktree_root.clone(),
        worktree_role: roots.worktree_context.worktree_role,
        branch_name: roots.worktree_context.branch_name.clone(),
        branch_ref: roots.worktree_context.branch_ref.clone(),
        branch_status: roots.worktree_context.branch_status,
        head_oid: roots.worktree_context.head_oid.clone(),
        watch_status: context_status
            .as_ref()
            .map(|status| status.watch_status)
            .unwrap_or(DaemonWatchStatus::Unknown),
        last_update_state,
        last_update_started_at: context_status
            .as_ref()
            .and_then(|status| status.last_refresh_started_at.as_ref().cloned()),
        last_update_completed_at: context_status
            .as_ref()
            .and_then(|status| status.last_refresh_completed_at.as_ref().cloned()),
        last_update_error: context_status.and_then(|status| status.last_refresh_error),
    }
}

fn readiness_next_actions(payload: &ReadinessPayload) -> Vec<NextAction> {
    match payload.status {
        ReadinessStatus::Ready => vec![action(
            TOOL_SEARCH,
            "Start discovery with a task-specific code search.",
            json!({ "query": "<code discovery query>" }),
        )],
        ReadinessStatus::Degraded => vec![
            action(
                TOOL_SEARCH,
                "Search is available, but results may be degraded.",
                json!({ "query": "<code discovery query>" }),
            ),
            action(
                TOOL_STATUS,
                "Refresh readiness after fixing the degraded index state.",
                json!({}),
            ),
        ],
        ReadinessStatus::Missing => vec![action(
            TOOL_START,
            "Create the local 1up index explicitly before searching.",
            json!({ "mode": "index_if_missing" }),
        )],
        ReadinessStatus::Indexing => vec![action(
            TOOL_STATUS,
            "Poll readiness until indexing completes.",
            json!({}),
        )],
        ReadinessStatus::Stale => vec![action(
            TOOL_START,
            "Rebuild the local index explicitly before searching.",
            json!({ "mode": "reindex" }),
        )],
        ReadinessStatus::Blocked => vec![action(
            TOOL_STATUS,
            "Retry readiness after correcting the local repository path or project state.",
            json!({}),
        )],
    }
}

fn search_next_actions(payload: &SearchPayload) -> Vec<NextAction> {
    let Some(first) = payload.results.first() else {
        return vec![action(
            TOOL_SEARCH,
            "Try a narrower or differently worded discovery query.",
            json!({ "query": "<refined code discovery query>" }),
        )];
    };

    let mut actions = vec![
        action(
            TOOL_GET,
            "Hydrate the top search result before editing or concluding.",
            json!({ "handles": [format!(":{}", first.handle)] }),
        ),
        action(
            TOOL_CONTEXT,
            "Retrieve file-line context around the top search result.",
            json!({ "locations": [location_argument(&first.path, first.line_start)] }),
        ),
    ];

    if let Some(symbol) = search_symbol_hint(first) {
        actions.push(action(
            TOOL_SYMBOL,
            "Verify definitions and references when completeness matters.",
            json!({ "name": symbol, "include": "both", "fuzzy": true }),
        ));
    }

    actions
}

fn read_next_actions(payload: &ReadPayload) -> Vec<NextAction> {
    if let Some(segment) = payload
        .records
        .iter()
        .filter_map(|record| record.segment.as_ref())
        .next()
    {
        let mut actions = vec![action(
            TOOL_CONTEXT,
            "Retrieve file-line context around the hydrated segment.",
            json!({ "locations": [location_argument(&segment.path, segment.line_start)] }),
        )];
        if let Some(symbol) = segment.defined_symbols.first() {
            actions.push(action(
                TOOL_SYMBOL,
                "Verify references for the symbol defined in this segment.",
                json!({ "name": symbol, "include": "both", "fuzzy": true }),
            ));
        }
        return actions;
    }

    if let Some(context) = payload
        .records
        .iter()
        .filter_map(|record| record.context.as_ref())
        .next()
    {
        return vec![action(
            TOOL_SEARCH,
            "Search indexed code if this file-line context needs more evidence.",
            json!({ "query": format!("{} {}", context.path, context.scope_type) }),
        )];
    }

    vec![action(
        TOOL_SEARCH,
        "Search again to obtain a valid handle or precise file location.",
        json!({ "query": "<code discovery query>" }),
    )]
}

fn symbol_next_actions(payload: &SymbolPayload, name: &str) -> Vec<NextAction> {
    let records = payload
        .definitions
        .iter()
        .chain(payload.references.iter())
        .take(3)
        .collect::<Vec<_>>();
    let handles = records
        .iter()
        .map(|record| format!(":{}", record.handle))
        .collect::<Vec<_>>();

    if handles.is_empty() {
        return vec![action(
            TOOL_SEARCH,
            "Search by behavior or context to find candidate symbols.",
            json!({ "query": name }),
        )];
    }

    let locations = records
        .iter()
        .map(|record| location_argument(&record.path, record.line_start))
        .collect::<Vec<_>>();

    vec![
        action(
            TOOL_GET,
            "Read the symbol matches before using them as evidence.",
            json!({ "handles": handles }),
        ),
        action(
            TOOL_CONTEXT,
            "Retrieve file-line context around the symbol matches.",
            json!({ "locations": locations }),
        ),
    ]
}

fn search_symbol_hint(hit: &ops::SearchHit) -> Option<&str> {
    hit.symbol
        .as_deref()
        .or_else(|| hit.defined_symbols.first().map(String::as_str))
}

fn location_argument(path: &str, line: usize) -> Value {
    json!({
        "path": path,
        "line": line,
        "expansion": 2
    })
}

fn structural_next_actions(payload: &StructuralSearchReport, pattern: &str) -> Vec<NextAction> {
    if let Some(first) = payload.results.first() {
        return vec![action(
            TOOL_CONTEXT,
            "Retrieve file-line context around the structural match.",
            json!({ "locations": [location_argument(&first.file_path, first.line_start)] }),
        )];
    }

    vec![
        action(
            TOOL_STRUCTURAL,
            "Retry with an adjusted tree-sitter pattern, language, or query scope.",
            json!({ "pattern": pattern, "language": "<supported language>" }),
        ),
        action(
            TOOL_SEARCH,
            "Use ranked search if a structural pattern is too narrow.",
            json!({ "query": "<code discovery query>" }),
        ),
    ]
}

fn impact_next_actions(payload: &ImpactResultEnvelope) -> Vec<NextAction> {
    if let Some(first) = payload.results.first() {
        return vec![action(
            TOOL_GET,
            "Read primary likely-impact results before making changes.",
            json!({ "handles": [format!(":{}", first.segment_id)] }),
        )];
    }

    if let Some(contextual) = payload
        .contextual_results
        .as_ref()
        .and_then(|results| results.first())
    {
        return vec![action(
            TOOL_GET,
            "Read contextual impact guidance when no primary result is available.",
            json!({ "handles": [format!(":{}", contextual.segment_id)] }),
        )];
    }

    if let Some(hint) = &payload.hint {
        if let Some(segment_id) = &hint.suggested_segment_id {
            return vec![action(
                TOOL_IMPACT,
                "Retry impact with the suggested narrower result handle.",
                json!({ "handle": format!(":{segment_id}") }),
            )];
        }
        if let Some(scope) = &hint.suggested_scope {
            return vec![action(
                TOOL_SEARCH,
                "Search within the suggested scope to find a narrower anchor.",
                json!({ "query": scope }),
            )];
        }
    }

    vec![action(
        TOOL_SEARCH,
        "Search for a narrower segment or symbol before retrying impact.",
        json!({ "query": "<narrower impact anchor>" }),
    )]
}

fn all_read_records_failed(payload: &ReadPayload) -> bool {
    !payload.records.is_empty()
        && payload.records.iter().all(|record| {
            matches!(
                record.status,
                ops::ReadStatus::NotFound
                    | ops::ReadStatus::Ambiguous
                    | ops::ReadStatus::Rejected
                    | ops::ReadStatus::Error
            )
        })
}

fn search_summary(payload: &SearchPayload, query: &str) -> String {
    let header = match payload.status {
        OperationStatus::Ok => format!(
            "Found {} ranked 1up search result(s) for \"{}\".",
            payload.results.len(),
            query
        ),
        OperationStatus::Degraded => format!(
            "Found {} degraded 1up search result(s) for \"{}\".",
            payload.results.len(),
            query
        ),
        OperationStatus::Empty => format!("No indexed code matched \"{}\".", query),
        OperationStatus::Partial => format!(
            "Found {} partial 1up search result(s) for \"{}\".",
            payload.results.len(),
            query
        ),
    };

    if payload.results.is_empty() {
        return header;
    }

    let rows = payload
        .results
        .iter()
        .map(format_search_hit_row)
        .collect::<Vec<_>>()
        .join("\n");

    format!("{header}\n\n{rows}")
}

fn format_search_hit_row(hit: &ops::SearchHit) -> String {
    let symbol = hit
        .symbol
        .as_deref()
        .or_else(|| hit.defined_symbols.first().map(String::as_str));
    let breadcrumb_symbol = format_breadcrumb_symbol(hit.breadcrumb.as_deref(), symbol);
    format!(
        "{}{MCP_FIELD_SEP}{}:{}-{}{MCP_FIELD_SEP}{}{MCP_FIELD_SEP}{}{MCP_FIELD_SEP}:{}",
        hit.score,
        hit.path,
        hit.line_start,
        hit.line_end,
        hit.kind,
        breadcrumb_symbol,
        short_handle(&hit.handle)
    )
}

fn format_breadcrumb_symbol(breadcrumb: Option<&str>, symbol: Option<&str>) -> String {
    let breadcrumb = breadcrumb
        .filter(|value| !value.is_empty())
        .unwrap_or(MCP_PLACEHOLDER);
    let symbol = symbol
        .filter(|value| !value.is_empty())
        .unwrap_or(MCP_PLACEHOLDER);
    format!("{breadcrumb}::{symbol}")
}

fn short_handle(handle: &str) -> String {
    handle.chars().take(MCP_HANDLE_DISPLAY_LEN).collect()
}

fn read_summary(payload: &ReadPayload) -> String {
    let record_label = read_record_label(payload);
    let header = format!(
        "Read {} {record_label} record(s); status is {}.",
        payload.records.len(),
        status_string(&payload.status)
    );

    if payload.records.is_empty() {
        return header;
    }

    let records = payload
        .records
        .iter()
        .map(format_read_record)
        .collect::<Vec<_>>()
        .join("\n");

    format!("{header}\n\n{records}")
}

fn read_record_label(payload: &ReadPayload) -> &'static str {
    if payload
        .records
        .iter()
        .all(|record| matches!(record.source, ops::ReadSource::Location { .. }))
    {
        return "file-line context";
    }

    if payload
        .records
        .iter()
        .all(|record| matches!(record.source, ops::ReadSource::Handle { .. }))
    {
        return "code segment";
    }

    "code and file-line context"
}

fn format_read_record(record: &ops::ReadRecord) -> String {
    if let Some(segment) = &record.segment {
        return format_segment_record(segment);
    }
    if let Some(context) = &record.context {
        return format_context_record(context);
    }

    format!(
        "{}\t{}\t{}\n---",
        status_string(&record.status),
        format_read_source(&record.source),
        record.message.as_deref().unwrap_or("")
    )
}

fn format_segment_record(segment: &ops::SegmentRecord) -> String {
    format!(
        "segment {}\npath\t{}\tlines\t{}-{}\tkind\t{}\tlanguage\t{}\tbreadcrumb\t{}\trole\t{}\tdefines\t{}\treferences\t{}\tcalls\t{}\n\n{}\n\n---",
        segment.handle,
        segment.path,
        segment.line_start,
        segment.line_end,
        segment.kind,
        segment.language,
        segment.breadcrumb.as_deref().unwrap_or(MCP_PLACEHOLDER),
        status_string(&segment.role),
        segment.defined_symbols.join(","),
        segment.referenced_symbols.join(","),
        segment.called_symbols.join(","),
        segment.content
    )
}

fn format_context_record(context: &ops::ContextRecord) -> String {
    format!(
        "context {}:{}-{}\nlanguage\t{}\tscope\t{}\n\n{}\n\n---",
        context.path,
        context.line_start,
        context.line_end,
        context.language,
        context.scope_type,
        context.content
    )
}

fn format_read_source(source: &ops::ReadSource) -> String {
    match source {
        ops::ReadSource::Handle { raw, .. } => raw.clone(),
        ops::ReadSource::Location { path, line } => format!("{path}:{line}"),
    }
}

fn symbol_summary(payload: &SymbolPayload, name: &str) -> String {
    format!(
        "Found {} definition(s) and {} reference(s) for symbol \"{}\".",
        payload.definitions.len(),
        payload.references.len(),
        name
    )
}

fn structural_summary(payload: &StructuralSearchReport, pattern: &str) -> String {
    let header = match payload.status {
        StructuralSearchStatus::Ok => format!(
            "Structural search returned {} match(es) for \"{}\".",
            payload.results.len(),
            pattern
        ),
        StructuralSearchStatus::Empty => {
            format!("Structural search found no matches for \"{}\".", pattern)
        }
        StructuralSearchStatus::Error => payload
            .diagnostics
            .first()
            .map(|diagnostic| diagnostic.message.clone())
            .unwrap_or_else(|| "Structural search could not compile the pattern.".to_string()),
    };

    if payload.results.is_empty() {
        return header;
    }

    let rows = payload
        .results
        .iter()
        .map(format_structural_result_row)
        .collect::<Vec<_>>()
        .join("\n");

    format!("{header}\n\n{rows}")
}

fn format_structural_result_row(result: &StructuralResult) -> String {
    let pattern_name = result.pattern_name.as_deref().unwrap_or(MCP_PLACEHOLDER);
    format!(
        "{}:{}-{}{MCP_FIELD_SEP}structural{MCP_FIELD_SEP}{}::{}",
        result.file_path, result.line_start, result.line_end, result.language, pattern_name
    )
}

fn impact_summary(payload: &ImpactResultEnvelope) -> String {
    let contextual_count = payload
        .contextual_results
        .as_ref()
        .map_or(0, std::vec::Vec::len);
    match payload.status {
        ImpactStatus::Expanded | ImpactStatus::ExpandedScoped => format!(
            "Impact exploration returned {} primary and {} contextual result(s).",
            payload.results.len(),
            contextual_count
        ),
        ImpactStatus::Empty | ImpactStatus::EmptyScoped => {
            "Impact exploration found no likely impacted segments.".to_string()
        }
        ImpactStatus::Refused => payload
            .refusal
            .as_ref()
            .map(|refusal| refusal.message.clone())
            .unwrap_or_else(|| "Impact exploration was refused.".to_string()),
    }
}

fn indexed_tool_error(message: String) -> CallToolResult {
    error_result(
        "error",
        message,
        vec![action(
            TOOL_STATUS,
            "Check readiness and index state before retrying this MCP call.",
            json!({}),
        )],
    )
}

fn readiness_result(
    payload: ReadinessPayload,
    metadata: Option<ReadinessContextMetadata>,
) -> ToolEnvelope {
    let status = status_string(&payload.status);
    let summary = payload.summary.clone();
    let mut data = payload_value(&payload);
    if let Some(metadata) = metadata {
        merge_object_fields(&mut data, payload_value(&metadata));
    }
    envelope(status, summary, data, readiness_next_actions(&payload))
}

fn result(envelope: ToolEnvelope) -> CallToolResult {
    call_result(envelope, false)
}

fn error_result(
    status: impl Into<String>,
    summary: impl Into<String>,
    next_actions: Vec<NextAction>,
) -> CallToolResult {
    call_result(
        envelope(
            status,
            summary.into(),
            json!({ "error": true }),
            next_actions,
        ),
        true,
    )
}

fn call_result(envelope: ToolEnvelope, is_error: bool) -> CallToolResult {
    let value = payload_value(&envelope);
    let mut result = if is_error {
        CallToolResult::structured_error(value)
    } else {
        CallToolResult::structured(value)
    };
    result.content = vec![Content::text(envelope.summary)];
    result
}

fn envelope(
    status: impl Into<String>,
    summary: impl Into<String>,
    data: Value,
    next_actions: Vec<NextAction>,
) -> ToolEnvelope {
    ToolEnvelope {
        status: status.into(),
        summary: summary.into(),
        data,
        next_actions,
    }
}

fn action(tool: &str, reason: impl Into<String>, arguments: Value) -> NextAction {
    debug_assert!(
        RETAINED_PUBLIC_TOOLS.contains(&tool),
        "next action points to non-retained MCP tool: {tool}"
    );
    NextAction {
        tool: tool.to_string(),
        reason: reason.into(),
        arguments,
    }
}

fn payload_value<T: Serialize>(payload: &T) -> Value {
    serde_json::to_value(payload).unwrap_or_else(|err| {
        json!({
            "serialization_error": err.to_string()
        })
    })
}

fn merge_object_fields(target: &mut Value, fields: Value) {
    let (Some(target), Some(fields)) = (target.as_object_mut(), fields.as_object()) else {
        return;
    };

    for (key, value) in fields {
        debug_assert!(
            !target.contains_key(key),
            "MCP readiness metadata should not overwrite payload key `{key}`"
        );
        target.entry(key.clone()).or_insert_with(|| value.clone());
    }
}

fn status_string<T: Serialize>(status: &T) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|value| value.as_str().map(ToString::to_string))
        .unwrap_or_else(|| "ok".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::impact::{ImpactAnchor, ImpactHint};

    fn impact_input() -> ImpactInput {
        ImpactInput {
            handle: None,
            symbol: None,
            file: None,
            line: None,
            scope: None,
            depth: None,
            limit: None,
            path: None,
        }
    }

    #[test]
    fn impact_request_accepts_public_handle_anchor() {
        let mut input = impact_input();
        input.handle = Some(":abcdef012345".to_string());
        input.depth = Some(2);
        input.limit = Some(7);

        let request = impact_request(&input).unwrap();

        assert_eq!(
            request.anchor,
            ImpactAnchor::Segment {
                id: "abcdef012345".to_string()
            }
        );
        assert_eq!(request.depth, 2);
        assert_eq!(request.limit, 7);
    }

    #[test]
    fn impact_request_accepts_segment_id_as_compatibility_alias() {
        let input: ImpactInput =
            serde_json::from_value(json!({ "segment_id": ":abcdef012345" })).unwrap();

        let request = impact_request(&input).unwrap();

        assert_eq!(
            request.anchor,
            ImpactAnchor::Segment {
                id: "abcdef012345".to_string()
            }
        );
    }

    #[test]
    fn impact_request_rejects_line_without_file_anchor() {
        let mut input = impact_input();
        input.handle = Some(":abcdef012345".to_string());
        input.line = Some(10);

        let message = impact_request(&input).unwrap_err();

        assert_eq!(message, "line can only be used with a file impact anchor");
    }

    #[test]
    fn impact_retry_next_action_uses_public_handle_argument() {
        let payload = ImpactResultEnvelope {
            status: ImpactStatus::Empty,
            resolved_anchor: None,
            results: Vec::new(),
            contextual_results: None,
            hint: Some(ImpactHint {
                code: "narrow".to_string(),
                message: "Retry with this handle.".to_string(),
                suggested_scope: None,
                suggested_segment_id: Some("abcdef012345".to_string()),
            }),
            refusal: None,
        };

        let actions = impact_next_actions(&payload);

        assert_eq!(actions[0].tool, TOOL_IMPACT);
        assert_eq!(actions[0].arguments, json!({ "handle": ":abcdef012345" }));
    }
}
