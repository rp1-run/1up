use std::path::Path;

use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content},
    tool, tool_router,
};
use serde::Serialize;
use serde_json::{json, Value};

use crate::mcp::ops::{
    self, McpProjectRoots, OperationStatus, OverviewPayload, ReadLocation, ReadPayload,
    ReadinessPayload, ReadinessStatus, SearchPayload, SymbolInclude, SymbolLookupRequest,
    SymbolPayload,
};
use crate::mcp::server::OneupMcpServer;
use crate::mcp::types::{
    ContextInput, GetInput, ImpactInput, NextAction, OverviewInput, ReadinessContextMetadata,
    SearchInput, StartInput, StartMode, StatusInput, StructuralInput, SymbolIncludeInput,
    SymbolInput, ToolEnvelope, RETAINED_PUBLIC_TOOLS, TOOL_CONTEXT, TOOL_GET, TOOL_IMPACT,
    TOOL_SEARCH, TOOL_START, TOOL_STATUS, TOOL_STRUCTURAL, TOOL_SYMBOL,
};
use crate::search::impact::{ImpactAnchor, ImpactRequest, ImpactResultEnvelope, ImpactStatus};
use crate::shared::constants::{
    HYDRATION_BATCH_MAX_HANDLES, MAX_RECOVERY_ACTIONS, MAX_SEARCH_QUERIES, MAX_SEARCH_RESULTS,
    SUMMARY_MAX_BYTES,
};
use crate::shared::types::{
    BranchStatus, DaemonRefreshState, DaemonWatchStatus, IndexState, StructuralResult,
    StructuralSearchReport, StructuralSearchStatus, WorktreeContext,
};

const DEFAULT_SEARCH_LIMIT: usize = 5;
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

        // Check if we should return facts envelope for a large monorepo.
        // Only return facts if NO scope has been provided yet (user hasn't decided).
        // If scope_add or scope_narrow is provided, proceed with indexing.
        // CRITICAL: Check this BEFORE calling ops::start() to prevent background
        // spawning when gate fires.
        let readiness = ops::check_status(&roots).await;
        if input.mode == StartMode::IndexIfMissing || input.mode == StartMode::IndexIfNeeded {
            // Skip facts envelope if scope has already been provided
            if input.scope_add.is_none() && input.scope_narrow.is_none() {
                if let Ok(should_return_facts) = ops::should_return_facts_envelope(
                    &roots.state_root,
                    &roots.source_root,
                    &readiness,
                )
                .await
                {
                    if should_return_facts {
                        // Generate and return facts envelope instead of indexing
                        // Gate fires before ops::start to prevent background spawning
                        let facts = match ops::generate_facts_envelope(
                            &roots.state_root,
                            &roots.source_root,
                            roots.launch_subdir.clone(),
                        )
                        .await
                        {
                            Ok(facts) => facts,
                            Err(err) => return indexed_tool_error(err.to_string()),
                        };

                        let next_actions = facts_next_actions(&facts);

                        let env = envelope(
                            "refuse_and_propose_scope",
                            format!(
                                "Large repository ({} files) requires scope selection before indexing. \
                                 Review available directories and call oneup_start with scope_add.",
                                facts.file_count_total
                            ),
                            serde_json::to_value(&facts).unwrap_or_else(|_| json!({})),
                            next_actions,
                        );
                        return result(env);
                    }
                }
            }
        }

        // Only call ops::start() if gate did not fire (i.e., we reach this point)
        let mut payload =
            match ops::start(&roots, input.mode, input.scope_add, input.scope_narrow).await {
                Ok(payload) => payload,
                Err(err) => return indexed_tool_error(err.to_string()),
            };
        apply_branch_readiness(&mut payload, &roots.worktree_context);
        let metadata = readiness_context_metadata(&roots, &payload);

        result(readiness_result(payload, Some(metadata)))
    }

    #[tool(
        name = "oneup_search",
        description = "Search source code by meaning as the primary discovery path for code questions. Call before raw grep, rg, find, or broad file reads, then hydrate a small selected batch of handles in one oneup_get call, inspect file-line context, or verify symbols from the returned actions. For a multi-part question, pass every distinct aspect in one call via the queries array instead of issuing separate searches; 1up fuses the per-aspect results into one ranked list.",
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
                    None,
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

        // Effective query set for a (possibly multi-part) search: the primary
        // `query` first, then each distinct non-empty extra aspect, deduped and
        // capped at MAX_SEARCH_QUERIES.
        let mut queries = vec![input.query.clone()];
        if let Some(extra) = input.queries.as_ref() {
            for candidate in extra {
                let trimmed = candidate.trim();
                if trimmed.is_empty() || queries.iter().any(|existing| existing == trimmed) {
                    continue;
                }
                queries.push(trimmed.to_string());
                if queries.len() >= MAX_SEARCH_QUERIES {
                    break;
                }
            }
        }

        // A whole-repo sentinel in `path_prefix` means "no cone", so it is
        // normalized away rather than validated as a literal subtree prefix.
        let path_prefix = normalize_repo_scope(input.path_prefix.as_deref());
        match ops::run_search(
            &roots.state_root,
            &roots.worktree_context,
            &queries,
            limit,
            path_prefix.as_deref(),
        )
        .await
        {
            Ok(payload) => {
                let summary = search_summary(&payload);
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
        description = "Hydrate selected code segments from oneup_search or oneup_symbol handles. Passing 2-4 selected handles per call is the norm: each handle reports an independent, ordered outcome so one bad handle never fails the rest. Use before answering, citing, or editing discovered code. Reading code needs no verbosity argument: the default returns the complete source once in structured data, not mirrored in the text summary, alongside constant-size symbol counts and a ready-to-issue oneup_symbol call when you need the full symbol lists.",
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
                    None,
                )],
            );
        }
        let roots = match self.roots(input.path.as_deref()) {
            Ok(roots) => roots,
            Err(err) => return error_result("error", err.to_string(), vec![]),
        };

        match ops::get_handles(
            &roots.state_root,
            &roots.worktree_context,
            &input.handles,
            input.verbosity.as_deref(),
        )
        .await
        {
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
        description = "Retrieve repository-scoped file-line context from precise source locations. Use after search, get, or symbol evidence identifies relevant lines. A small enclosing scope is returned whole; a large scope is windowed around the requested line. When windowed, the record carries a truncation note whose ready-to-issue oneup_context recovery call fetches the omitted remainder, prepended first in next_actions to follow before answering.",
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
                    None,
                )],
            );
        }
        let roots = match self.roots(input.path.as_deref()) {
            Ok(roots) => roots,
            Err(err) => return error_result("error", err.to_string(), vec![]),
        };
        let scan_filter = match ops::resolve_context_scan_filter(&roots.worktree_context) {
            Ok(scan_filter) => scan_filter,
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

        match ops::read_context_locations(
            &self.state_root,
            &roots.source_root,
            &scan_filter,
            &locations,
        )
        .await
        {
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
                    None,
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
                // Prepend a ready-to-issue corrected impact call when the caller
                // supplied a usable anchor in the wrong shape, so a bad call is
                // one retry away instead of a fresh search/symbol round trip.
                let mut actions = Vec::new();
                if let Some(corrected) = corrected_impact_call(&input) {
                    actions.push(action(
                        TOOL_IMPACT,
                        "Retry impact with a single corrected anchor.",
                        Some(corrected),
                    ));
                }
                actions.push(action(
                    TOOL_SEARCH,
                    "Search to obtain a precise result handle for impact exploration.",
                    None,
                ));
                actions.push(action(
                    TOOL_SYMBOL,
                    "Verify a known symbol before using it as an impact anchor.",
                    None,
                ));
                return error_result("error", message, actions);
            }
        };

        // When `path` was consumed as a relative file anchor, it does not name
        // the project root, so resolve roots from the ambient project instead.
        let root_selector = if impact_path_as_file_anchor(&input).is_some() {
            None
        } else {
            input.path.as_deref()
        };
        let roots = match self.roots(root_selector) {
            Ok(roots) => roots,
            Err(err) => return error_result("error", err.to_string(), vec![]),
        };

        match ops::explore_impact(&roots.state_root, &roots.worktree_context, request).await {
            Ok(payload) => {
                let summary = impact_summary(&payload);
                let mut next_actions = impact_next_actions(&payload);
                // A remaining scope-exclusion refusal means a real requested
                // cone blocked the anchor: either it resolved outside the scope
                // (file/handle) or a scoped symbol lookup found nothing inside
                // the cone. Prepend the same anchor with no scope as a
                // ready-to-issue retry that searches the whole repo.
                let scope_requested = normalize_repo_scope(input.scope.as_deref()).is_some();
                let scope_excluded_anchor = payload.refusal.as_ref().is_some_and(|refusal| {
                    refusal.reason == "anchor_out_of_scope"
                        || (scope_requested && refusal.reason == "anchor_not_found")
                });
                if scope_excluded_anchor {
                    if let Some(anchor) = corrected_impact_call(&input) {
                        next_actions.insert(
                            0,
                            action(
                                TOOL_IMPACT,
                                "Retry impact without a scope to search the whole repository.",
                                Some(anchor),
                            ),
                        );
                    }
                }
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

    #[tool(
        name = "oneup_overview",
        description = "Retrieve a deterministic orientation digest of the indexed repository: statistics, most-referenced types, module map, cross-module dependencies, and entry points. Call first when starting work on an unfamiliar repository, then follow the returned actions into targeted discovery.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<ToolEnvelope>().unwrap(),
        annotations(title = "Repository Overview", read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    pub async fn oneup_overview(
        &self,
        Parameters(input): Parameters<OverviewInput>,
    ) -> CallToolResult {
        let roots = match self.roots(input.path.as_deref()) {
            Ok(roots) => roots,
            Err(err) => return error_result("error", err.to_string(), vec![]),
        };

        match ops::compute_overview(&roots.state_root, &roots.worktree_context).await {
            Ok(payload) => {
                let summary = overview_summary(&payload);
                let next_actions = overview_next_actions(&payload);
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
                launch_subdir: self.launch_subdir.clone(),
            }),
        }
    }
}

/// Whether an impact anchor value looks like a repo-relative file path rather
/// than a symbol name: it carries a path separator and does not end with one
/// (a trailing separator is a directory, not a file). Symbol names never
/// contain a path separator, so this promotes a file path an agent mistakenly
/// supplied in the `symbol` slot to a File anchor without misreading a dotted
/// symbol name.
fn looks_like_file_path(value: &str) -> bool {
    let trimmed = value.trim();
    trimmed.contains('/') && !trimmed.ends_with('/')
}

/// Whole-repo scope sentinels an agent may pass to mean "no cone" (case-
/// insensitive, trimmed). The empty string is handled separately by the trim.
const WHOLE_REPO_SCOPE_SENTINELS: [&str; 5] = ["all", ".", "/", "*", "**"];

/// Normalizes a scope / path-prefix argument before it is validated as a real
/// directory cone: a blank value or a whole-repo sentinel becomes `None` (no
/// cone), so a whole-repo request is never misread as a literal path prefix.
/// Any other value is trimmed and preserved.
fn normalize_repo_scope(value: Option<&str>) -> Option<String> {
    let trimmed = value.map(str::trim).filter(|value| !value.is_empty())?;
    if WHOLE_REPO_SCOPE_SENTINELS
        .iter()
        .any(|sentinel| trimmed.eq_ignore_ascii_case(sentinel))
    {
        return None;
    }
    Some(trimmed.to_string())
}

/// Whether a value looks like a repo-relative file path suitable for promotion
/// to a File impact anchor from the project-root `path` slot: it is relative
/// (no leading `/`), carries a path separator, and does not end with one.
/// Absolute paths retain their project-root selector meaning and are never
/// promoted.
fn looks_like_relative_file_path(value: &str) -> bool {
    let trimmed = value.trim();
    !trimmed.starts_with('/') && looks_like_file_path(trimmed)
}

/// The repo-relative file path an impact call supplied in the project-root
/// `path` slot as its only anchor, or `None` when `path` is absent, absolute,
/// not file-looking, or another anchor (handle/symbol/file) is present. When
/// this returns `Some`, `path` names a file anchor rather than the project
/// root, so the handler must resolve roots without it.
fn impact_path_as_file_anchor(input: &ImpactInput) -> Option<String> {
    let has_other_anchor = [&input.handle, &input.symbol, &input.file]
        .into_iter()
        .any(|value| {
            value
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
        });
    if has_other_anchor {
        return None;
    }
    input
        .path
        .as_deref()
        .map(str::trim)
        .filter(|value| looks_like_relative_file_path(value))
        .map(str::to_string)
}

fn impact_request(input: &ImpactInput) -> Result<ImpactRequest, String> {
    let handle = input
        .handle
        .as_ref()
        .filter(|value| !value.trim().is_empty());
    let symbol = input
        .symbol
        .as_ref()
        .filter(|value| !value.trim().is_empty());
    let file = input.file.as_ref().filter(|value| !value.trim().is_empty());

    let count = [handle.is_some(), symbol.is_some(), file.is_some()]
        .iter()
        .filter(|present| **present)
        .count();
    if count > 1 {
        return Err("provide exactly one impact anchor: handle, symbol, or file".to_string());
    }

    // A repo-relative file path supplied in the project-root `path` slot with no
    // other anchor is promoted to a File anchor. Absolute paths keep their
    // project-root meaning.
    let path_as_file = impact_path_as_file_anchor(input);
    if count == 0 && path_as_file.is_none() {
        return Err("provide exactly one impact anchor: handle, symbol, or file".to_string());
    }

    // A file path supplied in the `symbol` slot is likewise promoted, so `line`
    // pins it just like an explicit `file` anchor.
    let symbol_as_file = symbol.filter(|value| looks_like_file_path(value));
    let file_anchor: Option<String> = file
        .cloned()
        .or_else(|| symbol_as_file.cloned())
        .or(path_as_file);
    if input.line.is_some() && file_anchor.is_none() {
        return Err("line can only be used with a file impact anchor".to_string());
    }

    let anchor = if let Some(handle) = handle {
        ImpactAnchor::Segment {
            id: normalize_handle(handle),
        }
    } else if let Some(path) = file_anchor {
        ImpactAnchor::File {
            path,
            line: input.line,
        }
    } else {
        ImpactAnchor::Symbol {
            name: symbol.cloned().unwrap_or_default(),
        }
    };

    Ok(ImpactRequest {
        anchor,
        scope: normalize_repo_scope(input.scope.as_deref()),
        depth: input.depth.unwrap_or_default(),
        limit: input.limit.unwrap_or_default(),
    })
}

/// Best-effort corrected single-anchor `oneup_impact` call for an
/// anchor-validation error: it keeps the most precise anchor the caller already
/// supplied (handle, then a file / file-looking symbol, then a plain symbol) so
/// a malformed call is one ready-to-issue retry away rather than a fresh
/// discovery round trip. Returns `None` when no usable anchor was supplied.
fn corrected_impact_call(input: &ImpactInput) -> Option<Value> {
    let trimmed = |value: &Option<String>| {
        value
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    let handle = trimmed(&input.handle);
    let symbol = trimmed(&input.symbol);
    let file = trimmed(&input.file);

    if let Some(handle) = handle {
        return Some(json!({ "handle": format!(":{}", normalize_handle(&handle)) }));
    }

    // A file path (explicit, in the symbol slot, or a relative path in the
    // project-root `path` slot) keeps its line.
    let file_path = file
        .or_else(|| symbol.clone().filter(|value| looks_like_file_path(value)))
        .or_else(|| impact_path_as_file_anchor(input));
    if let Some(path) = file_path {
        let mut arguments = serde_json::Map::new();
        arguments.insert("file".to_string(), Value::from(path));
        if let Some(line) = input.line {
            arguments.insert("line".to_string(), Value::from(line));
        }
        return Some(Value::Object(arguments));
    }

    symbol.map(|symbol| json!({ "symbol": symbol }))
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

    // An exact detached commit that matches the indexed state is pinned, not
    // ambiguous, so it must not be downgraded. This aligns the readiness path
    // with the search path, which already treats `Detached` as non-degraded
    // (`SearchScope::degraded_reason`, src/search/scope.rs). Exempt it only when
    // HEAD is proven un-drifted (`Some(false)`); `Some(true)` (drifted) and
    // `None` (unprovable) keep the degraded caveat.
    if context.branch_status == BranchStatus::Detached && payload.drifted == Some(false) {
        return;
    }

    let branch_reason = context.branch_status.branch_scope_caveat();

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

/// Builds the Missing-readiness next_actions. When the daemon gate persisted a
/// fresh scope proposal (over-threshold unscoped repo), surface ranked
/// `scope_add` cones — the same actionable choices the synchronous facts
/// envelope offers — so the refusal is durably actionable through `oneup_status`
/// and a follow-up unscoped `oneup_start`. Absent a proposal, fall back to the
/// generic "create the index" action.
fn missing_readiness_next_actions(payload: &ReadinessPayload) -> Vec<NextAction> {
    let Some(proposal) = payload.scope_proposal.as_ref() else {
        return vec![action(
            TOOL_START,
            "Create the local 1up index explicitly before searching.",
            Some(json!({ "mode": "index_if_missing" })),
        )];
    };

    let mut actions = Vec::new();
    if let Some(launch_subdir) = proposal.launch_subdir.as_ref() {
        actions.push(action(
            TOOL_START,
            format!("Index the launch subdirectory first: {}", launch_subdir),
            Some(json!({ "mode": "index_if_needed", "scope_add": [launch_subdir] })),
        ));
    }

    // Reuse the rank-aligned suggestion strings as the scope_add reasons:
    // `suggestions[i]` describes `scope_candidates[i]` by construction (both
    // derive from the same ranked generator in `attach_scope_proposal_if_fresh`),
    // so this stays a single phrasing source with the synchronous facts
    // envelope. Agents consume next_actions reasons individually (issue #88),
    // so every reason must read standalone — the generator guarantees the
    // first entry is a primary imperative exactly when no launch_subdir action
    // precedes it, phrases the rest as "Alternatively, …", and never emits a
    // reason beginning with "Or ". The leading action additionally carries the
    // over-threshold file count so the refusal stays legible on its own.
    for (candidate, suggestion) in proposal
        .scope_candidates
        .iter()
        .zip(proposal.suggestions.iter())
    {
        let reason = if actions.is_empty() {
            format!(
                "Large repository ({} files) needs a scope. {}",
                proposal.file_count_total, suggestion
            )
        } else {
            suggestion.clone()
        };
        actions.push(action(
            TOOL_START,
            reason,
            Some(json!({ "mode": "index_if_needed", "scope_add": [candidate] })),
        ));
    }

    // Defensive: a proposal with no usable cones still needs an actionable step.
    if actions.is_empty() {
        actions.push(action(
            TOOL_START,
            "Create the local 1up index explicitly before searching.",
            Some(json!({ "mode": "index_if_missing" })),
        ));
    }
    actions
}

fn readiness_next_actions(payload: &ReadinessPayload) -> Vec<NextAction> {
    let mut actions = match payload.status {
        ReadinessStatus::Ready => vec![action(
            TOOL_SEARCH,
            "Start discovery with a task-specific code search.",
            None,
        )],
        ReadinessStatus::Degraded => vec![
            action(
                TOOL_SEARCH,
                "Search is available, but results may be degraded.",
                None,
            ),
            action(
                TOOL_STATUS,
                "Refresh readiness after fixing the degraded index state.",
                Some(json!({})),
            ),
        ],
        ReadinessStatus::Missing => missing_readiness_next_actions(payload),
        ReadinessStatus::Indexing => vec![action(
            TOOL_STATUS,
            "Poll readiness until indexing completes.",
            Some(json!({})),
        )],
        ReadinessStatus::Stale => vec![action(
            TOOL_START,
            "Rebuild the local index explicitly before searching.",
            Some(json!({ "mode": "reindex" })),
        )],
        ReadinessStatus::Blocked => vec![action(
            TOOL_STATUS,
            "Retry readiness after correcting the local repository path or project state.",
            Some(json!({})),
        )],
    };

    if payload.drifted == Some(true) {
        actions.push(action(
            TOOL_START,
            "The repository HEAD moved after the last index; refresh the index to pick up the changes.",
            Some(json!({ "mode": "index_if_needed" })),
        ));
    }

    actions
}

/// Selects the bounded, relevance-ordered batch of result handles the default
/// post-search `oneup_get` next action recommends hydrating.
///
/// `results` are already RRF-ranked, so taking the leading
/// `min(len, HYDRATION_BATCH_MAX_HANDLES)` yields the most relevant handles in
/// ranked order (not an arbitrary subset). Each handle is emitted as a
/// `:`-prefixed full handle; the caller handles the empty-results branch before
/// reaching this gate.
fn select_hydration_handles(results: &[ops::SearchHit]) -> Vec<String> {
    results
        .iter()
        .take(HYDRATION_BATCH_MAX_HANDLES)
        .map(|hit| format!(":{}", hit.handle))
        .collect()
}

fn search_next_actions(payload: &SearchPayload) -> Vec<NextAction> {
    let Some(first) = payload.results.first() else {
        // If empty search results with scope, suggest widening scope
        if let Some(scope) = &payload.index_scope {
            if !scope.roots.is_empty() {
                // Placeholder-free output. When results are empty and scoped,
                // suggest widening scope but omit arguments for the search action
                // since we cannot synthesize a real refined query.
                let actions = vec![
                    action(
                        TOOL_START,
                        "Expand the indexed scope to include more code.",
                        None,
                    ),
                    action(
                        TOOL_SEARCH,
                        "Try a narrower or differently worded discovery query.",
                        None,
                    ),
                ];
                return actions;
            }
        }
        // Empty results, unscoped. Omit arguments since we cannot synthesize a real query.
        return vec![action(
            TOOL_SEARCH,
            "Try a narrower or differently worded discovery query.",
            None,
        )];
    };

    let handles = select_hydration_handles(&payload.results);
    let mut actions = vec![
        action(
            TOOL_GET,
            format!(
                "Hydrate the top {} result{} in one batched oneup_get call before editing or concluding; each handle reports independently, so one bad handle never fails the rest.",
                handles.len(),
                if handles.len() == 1 { "" } else { "s" },
            ),
            Some(json!({ "handles": handles })),
        ),
        action(
            TOOL_CONTEXT,
            "Retrieve file-line context around the top search result.",
            Some(json!({ "locations": [location_argument(&first.path, first.line_start)] })),
        ),
    ];

    if let Some(symbol) = search_symbol_hint(first) {
        actions.push(action(
            TOOL_SYMBOL,
            "Verify definitions and references when completeness matters.",
            Some(json!({ "name": symbol, "include": "both", "fuzzy": true })),
        ));
    }

    actions
}

/// Envelope next_actions for get/context. Recovery actions for any bounded
/// record are **prepended** (first, ahead of generic follow-ups) so an agent
/// sees how to fetch the omitted content before anything else
/// (load-bearing). They are deduped per path and capped at
/// [`MAX_RECOVERY_ACTIONS`] so next_actions can never become the new unbounded
/// payload the compaction is meant to remove.
fn read_next_actions(payload: &ReadPayload) -> Vec<NextAction> {
    let mut actions = recovery_next_actions(payload);
    actions.extend(read_follow_up_actions(payload));
    actions
}

/// Prepended recovery actions copied verbatim from each bounded record's
/// `TruncationNote.recovery` (structured, ready-to-issue). The description names
/// the clipped scope, its full range, and the omitted line counts so the agent
/// can act without re-deriving them. Deduped per path, capped at
/// [`MAX_RECOVERY_ACTIONS`].
fn recovery_next_actions(payload: &ReadPayload) -> Vec<NextAction> {
    let mut actions = Vec::new();
    let mut seen_paths: Vec<&str> = Vec::new();

    for record in &payload.records {
        let (path, truncation) = if let Some(segment) = &record.segment {
            (segment.path.as_str(), segment.truncation.as_ref())
        } else if let Some(context) = &record.context {
            (context.path.as_str(), context.truncation.as_ref())
        } else {
            continue;
        };

        let Some(note) = truncation else { continue };
        if seen_paths.contains(&path) {
            continue;
        }
        seen_paths.push(path);

        actions.push(action(
            note.recovery.tool.as_str(),
            recovery_reason(path, note),
            Some(note.recovery.arguments.clone()),
        ));

        if actions.len() >= MAX_RECOVERY_ACTIONS {
            break;
        }
    }

    actions
}

/// Human-facing reason for a recovery action: names the clipped scope, its full
/// line range, and the omitted counts (e.g. "Retrieve the omitted remainder of
/// scope `loadPlugins` (manager.ts:71-588; 13 lines above / 498 below
/// omitted).") for scope clips, or the omitted symbol count for symbol clips.
fn recovery_reason(path: &str, note: &ops::TruncationNote) -> String {
    if let (Some(start), Some(end)) = (note.full_line_start, note.full_line_end) {
        let scope = note
            .scope_name
            .as_deref()
            .map(|name| format!("scope `{name}`"))
            .unwrap_or_else(|| {
                note.scope_type
                    .as_deref()
                    .map(|scope_type| format!("{scope_type} scope"))
                    .unwrap_or_else(|| "the enclosing scope".to_string())
            });
        return format!(
            "Retrieve the omitted remainder of {scope} ({path}:{start}-{end}; {} lines above / {} below omitted).",
            note.omitted_above.unwrap_or(0),
            note.omitted_below.unwrap_or(0),
        );
    }

    if let Some(omitted) = note.omitted_symbols {
        return format!("Retrieve the {omitted} omitted symbol(s) for the capped list in {path}.");
    }

    format!("Retrieve the content omitted from {path}.")
}

fn read_follow_up_actions(payload: &ReadPayload) -> Vec<NextAction> {
    if let Some(segment) = payload
        .records
        .iter()
        .filter_map(|record| record.segment.as_ref())
        .next()
    {
        let mut actions = vec![action(
            TOOL_CONTEXT,
            "Retrieve file-line context around the hydrated segment.",
            Some(json!({ "locations": [location_argument(&segment.path, segment.line_start)] })),
        )];
        // Use the pre-gating symbol hint: defined_symbols is emptied at default
        // verbosity (payload cleanup), but defining segments must still
        // offer symbol verification.
        if let Some(symbol) = segment.symbol_hint.as_deref() {
            actions.push(action(
                TOOL_SYMBOL,
                "Verify references for the symbol defined in this segment.",
                Some(json!({ "name": symbol, "include": "both", "fuzzy": true })),
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
            Some(json!({ "query": format!("{} {}", context.path, context.scope_type) })),
        )];
    }

    // No content hydrated: give status-aware guidance for the failed handle
    // records so an agent can disambiguate or refine instead of blindly
    // repeating the call.
    handle_failure_next_actions(payload)
}

/// Next_actions for a get call that hydrated no content. A
/// failure carrying candidate ids — an ambiguous prefix or a rejected identical
/// retry whose original failure was ambiguous — prepends a ready-to-issue
/// `oneup_get` prefilled with the real candidate ids from `matching_handles`
/// (never placeholders) so the agent can pick one unambiguous handle ahead of
/// the generic search fallback. The trailing search reason is the most specific
/// applicable: a rejected identical retry is steered to a refined query instead
/// of repeating the call; an absent handle notes it is not in the active
/// context. The placeholder-free search fallback always trails so
/// there is always a forward action that differs from repeating the call.
fn handle_failure_next_actions(payload: &ReadPayload) -> Vec<NextAction> {
    let mut actions = Vec::new();

    if let Some(candidates) = payload
        .records
        .iter()
        .find(|record| {
            matches!(
                record.status,
                ops::ReadStatus::Ambiguous | ops::ReadStatus::Rejected
            ) && !record.matching_handles.is_empty()
        })
        .map(|record| record.matching_handles.clone())
    {
        actions.push(action(
            TOOL_GET,
            "Hydrate the listed candidate handles to choose the one you meant.",
            Some(json!({ "handles": candidates })),
        ));
    }

    let rejected_identical_retry = payload
        .records
        .iter()
        .any(|record| record.status == ops::ReadStatus::Rejected);
    let absent_from_context = payload
        .records
        .iter()
        .any(|record| record.status == ops::ReadStatus::NotFound);
    let search_reason = if rejected_identical_retry {
        "This identical handle already failed in the active context this session; search with a refined query instead of repeating the call."
    } else if absent_from_context {
        "The handle is not present in the active context; search to obtain a valid handle."
    } else {
        "Search again to obtain a valid handle or precise file location."
    };
    actions.push(action(TOOL_SEARCH, search_reason, None));

    actions
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
            Some(json!({ "query": name })),
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
            Some(json!({ "handles": handles })),
        ),
        action(
            TOOL_CONTEXT,
            "Retrieve file-line context around the symbol matches.",
            Some(json!({ "locations": locations })),
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
            Some(json!({ "locations": [location_argument(&first.file_path, first.line_start)] })),
        )];
    }

    // Placeholder-free output. When results are empty, suggest alternatives
    // without placeholder arguments.
    vec![
        action(
            TOOL_STRUCTURAL,
            "Retry with an adjusted tree-sitter pattern, language, or query scope.",
            Some(json!({ "pattern": pattern })),
        ),
        action(
            TOOL_SEARCH,
            "Use ranked search if a structural pattern is too narrow.",
            None,
        ),
    ]
}

fn impact_next_actions(payload: &ImpactResultEnvelope) -> Vec<NextAction> {
    if let Some(first) = payload.results.first() {
        return vec![action(
            TOOL_GET,
            "Read primary likely-impact results before making changes.",
            Some(json!({ "handles": [format!(":{}", first.segment_id)] })),
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
            Some(json!({ "handles": [format!(":{}", contextual.segment_id)] })),
        )];
    }

    if let Some(hint) = &payload.hint {
        if let Some(segment_id) = &hint.suggested_segment_id {
            return vec![action(
                TOOL_IMPACT,
                "Retry impact with the suggested narrower result handle.",
                Some(json!({ "handle": format!(":{segment_id}") })),
            )];
        }
        if let Some(scope) = &hint.suggested_scope {
            return vec![action(
                TOOL_SEARCH,
                "Search within the suggested scope to find a narrower anchor.",
                Some(json!({ "query": scope })),
            )];
        }
    }

    // Placeholder-free output. Omit arguments when we cannot synthesize a real narrower anchor.
    vec![action(
        TOOL_SEARCH,
        "Search for a narrower segment or symbol before retrying impact.",
        None,
    )]
}

/// Non-empty digests hand the agent its next move: verify the top
/// most-referenced type, then search the densest module. An empty digest
/// falls back to a readiness check so the envelope always carries at least
/// one canonical action.
fn overview_next_actions(payload: &OverviewPayload) -> Vec<NextAction> {
    let mut actions = Vec::new();

    if let Some(top) = payload.top_symbols.first() {
        actions.push(action(
            TOOL_SYMBOL,
            "Inspect the definition and references of the most-referenced type.",
            Some(json!({ "name": top.name })),
        ));
    }
    if let Some(densest) = payload.modules.first() {
        actions.push(action(
            TOOL_SEARCH,
            "Start targeted discovery inside the densest module.",
            Some(json!({ "query": format!("{} module responsibilities", densest.module) })),
        ));
    }
    if actions.is_empty() {
        actions.push(action(
            TOOL_STATUS,
            "Check readiness and indexing options before retrying the overview.",
            Some(json!({})),
        ));
    }

    actions
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

/// Content-free search summary (mirrors the `read_summary`/`oneup_context`
/// grammar): a constant-shaped orientation line whose length is independent of
/// the query text and the result set. The ranked results are the single source
/// of truth in `structuredContent`, so the text echoes neither the query, the
/// per-result rows, nor any (truncated) handle — that mirror is what re-inflated
/// the search envelope. Bounded at [`SUMMARY_MAX_BYTES`] so even a many-root
/// empty-scope notice cannot grow the text block.
fn search_summary(payload: &SearchPayload) -> String {
    let count = payload.results.len();
    let summary = match payload.status {
        OperationStatus::Ok => {
            format!("Found {count} ranked 1up search result(s); details in structuredContent.")
        }
        OperationStatus::Degraded => {
            format!("Found {count} degraded 1up search result(s); details in structuredContent.")
        }
        OperationStatus::Partial => {
            format!("Found {count} partial 1up search result(s); details in structuredContent.")
        }
        OperationStatus::Empty => match payload
            .index_scope
            .as_ref()
            .filter(|scope| !scope.roots.is_empty())
        {
            Some(scope) => format!(
                "No indexed code matched in the configured scope. {}",
                scope.coverage_description()
            ),
            None => "No indexed code matched the search.".to_string(),
        },
    };

    clamp_summary_bytes(summary)
}

/// Clamp a model-facing summary to [`SUMMARY_MAX_BYTES`], truncating on a UTF-8
/// character boundary so the text block can never exceed the byte budget.
fn clamp_summary_bytes(mut summary: String) -> String {
    if summary.len() <= SUMMARY_MAX_BYTES {
        return summary;
    }
    let mut end = SUMMARY_MAX_BYTES;
    while end > 0 && !summary.is_char_boundary(end) {
        end -= 1;
    }
    summary.truncate(end);
    summary
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
    let status = status_string(&record.status);
    if let Some(segment) = &record.segment {
        return format_segment_record(&status, segment);
    }
    if let Some(context) = &record.context {
        return format_context_record(&status, context);
    }

    format!(
        "{status}\t{}\t{}",
        format_read_source(&record.source),
        record.message.as_deref().unwrap_or("")
    )
}

/// Content-free segment line: the authoritative source stays only in
/// `structuredContent`; the text summary carries a constant-sized orientation
/// line whose length is independent of the segment body. A symbol-list clip
/// appends a bounded `truncated: {reason}` marker so the omission is visible in
/// the model-facing text, not just in structured data.
fn format_segment_record(status: &str, segment: &ops::SegmentRecord) -> String {
    let mut line = format!(
        "{status}\t{}:{}-{}\t{}\tsegment {}",
        segment.path, segment.line_start, segment.line_end, segment.language, segment.handle,
    );
    if let Some(note) = &segment.truncation {
        line.push_str(&format!("\ttruncated: {}", note.reason));
    }
    line
}

/// Content-free context line. A windowed scope appends a bounded
/// `truncated: {reason} +{above}/-{below} lines` marker (constant-sized: a
/// reason constant plus two bounded numerals) so the omitted line counts are
/// visible in the text block as well as in the structured `TruncationNote`.
fn format_context_record(status: &str, context: &ops::ContextRecord) -> String {
    let mut line = format!(
        "{status}\t{}:{}-{}\t{}\tscope {}",
        context.path, context.line_start, context.line_end, context.language, context.scope_type,
    );
    if let Some(note) = &context.truncation {
        line.push_str(&format!(
            "\ttruncated: {} +{}/-{} lines",
            note.reason,
            note.omitted_above.unwrap_or(0),
            note.omitted_below.unwrap_or(0),
        ));
    }
    line
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

fn overview_summary(payload: &OverviewPayload) -> String {
    if payload.status == OperationStatus::Empty {
        return "The 1up index is ready but contains no indexed code for this context.".to_string();
    }

    let mut summary = format!(
        "Indexed {} file(s) and {} segment(s) across {} language(s)",
        payload.stats.indexed_files,
        payload.stats.total_segments,
        payload.stats.languages.len()
    );
    if let Some(module) = payload.modules.first() {
        summary.push_str(&format!("; densest module is {}", module.module));
    }
    if let Some(top) = payload.top_symbols.first() {
        summary.push_str(&format!("; most-referenced type is {}", top.name));
    }
    summary.push('.');
    summary
}

fn indexed_tool_error(message: String) -> CallToolResult {
    error_result(
        "error",
        message,
        vec![action(
            TOOL_STATUS,
            "Check readiness and index state before retrying this MCP call.",
            Some(json!({})),
        )],
    )
}

/// Once the index phase has reached a terminal ready/complete state, drop
/// build-time telemetry that no longer informs a readiness decision: the
/// index_progress prefilter/parallelism/message internals plus the
/// index_progress source_root that only duplicates the envelope's top-level
/// source_root. Running or not-yet-ready phases keep their full progress so
/// live indexing stays observable.
fn lean_ready_status(payload: &mut ReadinessPayload) {
    let ready = payload.status == ReadinessStatus::Ready;
    let Some(progress) = payload.index_progress.as_mut() else {
        return;
    };
    if !(ready || matches!(progress.state, IndexState::Complete)) {
        return;
    }
    progress.prefilter = None;
    progress.parallelism = None;
    progress.message = None;
    progress.source_root = None;
}

fn readiness_result(
    mut payload: ReadinessPayload,
    metadata: Option<ReadinessContextMetadata>,
) -> ToolEnvelope {
    lean_ready_status(&mut payload);
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

/// Builds the facts-envelope `next_actions` for a large-monorepo scope refusal.
///
/// Keeps the `launch_subdir` action first when present, then emits the top-N
/// ranked `scope_add` actions from the same source of truth as
/// `facts.suggestions` (`ops::ranked_scope_suggestions`). Reasons are coherent
/// standalone imperatives — the first is a primary imperative unless a
/// `launch_subdir` action precedes it, in which case every scope suggestion
/// reads as an alternative so none is a dangling primary and none begins with
/// "Or ". The envelope shape stays additive: only `next_actions` grows.
fn facts_next_actions(facts: &crate::mcp::types::FactsEnvelope) -> Vec<NextAction> {
    let mut next_actions = Vec::new();

    if let Some(launch_subdir) = facts.launch_subdir.as_ref() {
        next_actions.push(action(
            TOOL_START,
            format!("Index the launch subdirectory first: {}", launch_subdir),
            Some(json!({
                "mode": "index_if_needed",
                "scope_add": [launch_subdir]
            })),
        ));
    }

    for suggestion in ops::ranked_scope_suggestions(facts) {
        next_actions.push(action(
            TOOL_START,
            suggestion.reason,
            Some(json!({
                "mode": "index_if_needed",
                "scope_add": [suggestion.directory]
            })),
        ));
    }

    next_actions
}

fn action(tool: &str, reason: impl Into<String>, arguments: Option<Value>) -> NextAction {
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

    #[test]
    fn search_summary_is_content_free_and_within_budget() {
        let payload = SearchPayload {
            status: OperationStatus::Ok,
            results: vec![ops::SearchHit {
                handle: "abcdef0123456789".to_string(),
                path: "src/very/deep/module/path.rs".to_string(),
                language: "rust".to_string(),
                kind: "function".to_string(),
                score: 42,
                line_start: 10,
                line_end: 20,
                breadcrumb: Some("Module::Type".to_string()),
                symbol: Some("SecretSymbolName".to_string()),
                defined_symbols: vec!["SecretSymbolName".to_string()],
            }],
            degraded_reason: None,
            index_scope: None,
        };

        let summary = search_summary(&payload);

        assert!(
            summary.contains("1 ranked"),
            "summary should report the ranked count: {summary}"
        );
        assert!(
            !summary.contains("abcdef0123456789"),
            "summary must not leak a result handle: {summary}"
        );
        assert!(
            !summary.contains("src/very/deep"),
            "summary must not enumerate result paths: {summary}"
        );
        assert!(
            !summary.contains("SecretSymbolName"),
            "summary must not enumerate per-result symbols: {summary}"
        );
        assert!(
            summary.len() <= SUMMARY_MAX_BYTES,
            "summary must stay within budget; got {} bytes",
            summary.len()
        );
    }

    #[test]
    fn clamp_summary_bytes_truncates_on_char_boundary() {
        let long = "x".repeat(SUMMARY_MAX_BYTES + 50);
        assert_eq!(clamp_summary_bytes(long).len(), SUMMARY_MAX_BYTES);

        // A run of multi-byte characters is truncated on a boundary, never mid
        // codepoint, so the clamped string stays valid UTF-8 within budget.
        let multibyte = "é".repeat(SUMMARY_MAX_BYTES);
        let clamped = clamp_summary_bytes(multibyte);
        assert!(clamped.len() <= SUMMARY_MAX_BYTES);
        assert!(clamped.chars().all(|c| c == 'é'));
    }

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
    fn readiness_next_actions_append_index_if_needed_start_when_drifted() {
        let mut payload = ops::blocked_readiness_for_path("repo", "fixture");
        payload.status = ReadinessStatus::Ready;

        let clean_actions = readiness_next_actions(&payload);
        assert!(
            !clean_actions.iter().any(|action| action.tool == TOOL_START),
            "non-drifted ready payload must not suggest a start action"
        );

        payload.drifted = Some(true);
        let drift_actions = readiness_next_actions(&payload);
        assert_eq!(drift_actions.len(), clean_actions.len() + 1);
        let appended = drift_actions.last().unwrap();
        assert_eq!(appended.tool, TOOL_START);
        if let Some(args) = &appended.arguments {
            assert_eq!(args["mode"], "index_if_needed");
        } else {
            panic!("expected arguments to be Some");
        }
        assert_eq!(drift_actions[0].tool, TOOL_SEARCH);
    }

    #[test]
    fn missing_readiness_without_proposal_falls_back_to_generic_start() {
        let mut payload = ops::blocked_readiness_for_path("repo", "fixture");
        payload.status = ReadinessStatus::Missing;

        let actions = missing_readiness_next_actions(&payload);
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].tool, TOOL_START);
        assert_eq!(
            actions[0].arguments.as_ref().unwrap()["mode"],
            "index_if_missing"
        );
    }

    #[test]
    fn missing_readiness_with_fresh_proposal_surfaces_ranked_scope_add_actions() {
        // Suggestions as `attach_scope_proposal_if_fresh` derives them:
        // rank-aligned with scope_candidates and phrased by
        // `generate_ranked_scope_suggestions` (primary imperative first, then
        // standalone alternatives).
        let mut payload = ops::blocked_readiness_for_path("repo", "fixture");
        payload.status = ReadinessStatus::Missing;
        payload.scope_proposal = Some(ops::ScopeProposalSummary {
            file_count_total: 5000,
            launch_subdir: None,
            suggestions: vec![
                "Index the largest directory: services".to_string(),
                "Alternatively, index the 2nd largest directory: libs".to_string(),
            ],
            scope_candidates: vec!["services".to_string(), "libs".to_string()],
        });

        let actions = missing_readiness_next_actions(&payload);
        assert_eq!(actions.len(), 2, "one scope_add action per ranked cone");
        assert!(actions.iter().all(|a| a.tool == TOOL_START));
        assert_eq!(
            actions[0].arguments.as_ref().unwrap()["scope_add"],
            serde_json::json!(["services"])
        );
        assert_eq!(
            actions[1].arguments.as_ref().unwrap()["scope_add"],
            serde_json::json!(["libs"])
        );
        // The over-threshold file count is surfaced so the refusal is legible,
        // and the reason embeds the rank-aligned suggestion string verbatim
        // rather than re-deriving its own wording.
        assert_eq!(
            actions[0].reason,
            "Large repository (5000 files) needs a scope. Index the largest directory: services"
        );
        assert_eq!(
            actions[1].reason,
            "Alternatively, index the 2nd largest directory: libs"
        );
        // Issue #88 regression guard: agents consume next_actions reasons
        // individually, so no reason may lean on a preceding sibling.
        assert!(
            actions.iter().all(|a| !a.reason.starts_with("Or ")),
            "no next_action reason may begin with \"Or \": {:?}",
            actions.iter().map(|a| &a.reason).collect::<Vec<_>>()
        );
    }

    #[test]
    fn missing_readiness_with_launch_subdir_leads_with_it_and_keeps_reasons_standalone() {
        // A launch_subdir cone gets the dedicated leading action; the ranked
        // suggestions (which the generator already de-duplicated against the
        // launch_subdir) follow as standalone alternatives — never a dangling
        // primary and never an "Or "-prefixed fragment.
        let mut payload = ops::blocked_readiness_for_path("repo", "fixture");
        payload.status = ReadinessStatus::Missing;
        payload.scope_proposal = Some(ops::ScopeProposalSummary {
            file_count_total: 5000,
            launch_subdir: Some("services".to_string()),
            suggestions: vec![
                "Alternatively, index the 2nd largest directory: libs".to_string(),
                "Alternatively, index the 3rd largest directory: tools".to_string(),
            ],
            scope_candidates: vec!["libs".to_string(), "tools".to_string()],
        });

        let actions = missing_readiness_next_actions(&payload);
        assert_eq!(actions.len(), 3, "launch_subdir action plus one per cone");
        assert_eq!(
            actions[0].arguments.as_ref().unwrap()["scope_add"],
            serde_json::json!(["services"])
        );
        assert!(actions[0].reason.contains("launch subdirectory"));
        // The launch_subdir cone appears exactly once: the ranked candidates
        // were de-duplicated at derivation, not re-filtered here.
        assert_eq!(
            actions[1].arguments.as_ref().unwrap()["scope_add"],
            serde_json::json!(["libs"])
        );
        assert_eq!(
            actions[1].reason,
            "Alternatively, index the 2nd largest directory: libs"
        );
        assert_eq!(
            actions[2].arguments.as_ref().unwrap()["scope_add"],
            serde_json::json!(["tools"])
        );
        assert!(
            actions.iter().all(|a| !a.reason.starts_with("Or ")),
            "no next_action reason may begin with \"Or \": {:?}",
            actions.iter().map(|a| &a.reason).collect::<Vec<_>>()
        );
    }

    #[test]
    fn lean_ready_status_strips_build_telemetry_from_terminal_index_progress() {
        use crate::shared::types::{
            IndexParallelism, IndexPhase, IndexPrefilterInfo, IndexProgress, IndexState,
        };
        use std::path::PathBuf;

        let mut progress = IndexProgress::pending();
        progress.state = IndexState::Complete;
        progress.phase = IndexPhase::Complete;
        progress.source_root = Some(PathBuf::from("/repo"));
        progress.message = Some("indexing complete".to_string());
        progress.parallelism = Some(IndexParallelism {
            jobs_configured: 8,
            jobs_effective: 8,
            embed_threads: 4,
        });
        progress.prefilter = Some(IndexPrefilterInfo {
            discovered: 100,
            metadata_skipped: 10,
            content_read: 90,
            deleted: 0,
        });

        let mut payload = ready_payload(Some(false));
        payload.source_root = "/repo".to_string();
        payload.index_progress = Some(progress);

        lean_ready_status(&mut payload);

        let leaned = payload.index_progress.as_ref().unwrap();
        assert!(leaned.prefilter.is_none(), "prefilter must be stripped");
        assert!(leaned.parallelism.is_none(), "parallelism must be stripped");
        assert!(leaned.message.is_none(), "message must be stripped");
        assert!(
            leaned.source_root.is_none(),
            "duplicate index_progress source_root must be stripped"
        );
        // Readiness essentials survive: the terminal state itself is retained.
        assert!(matches!(leaned.state, IndexState::Complete));
    }

    #[test]
    fn lean_ready_status_preserves_running_index_progress() {
        use crate::shared::types::{IndexParallelism, IndexPhase, IndexProgress, IndexState};

        let mut progress = IndexProgress::pending();
        progress.state = IndexState::Running;
        progress.phase = IndexPhase::Storing;
        progress.message = Some("storing segments".to_string());
        progress.parallelism = Some(IndexParallelism {
            jobs_configured: 8,
            jobs_effective: 8,
            embed_threads: 4,
        });

        // A not-yet-ready payload with a running index keeps its live progress
        // telemetry so indexing stays observable.
        let mut payload = ops::blocked_readiness_for_path("repo", "indexing");
        assert_ne!(payload.status, ReadinessStatus::Ready);
        payload.index_progress = Some(progress);

        lean_ready_status(&mut payload);

        let kept = payload.index_progress.as_ref().unwrap();
        assert!(
            kept.parallelism.is_some(),
            "running parallelism must be kept"
        );
        assert!(kept.message.is_some(), "running message must be kept");
    }

    fn detached_context(branch_status: BranchStatus) -> WorktreeContext {
        use std::path::PathBuf;
        WorktreeContext {
            context_id: "ctx".to_string(),
            state_root: PathBuf::from("/repo"),
            source_root: PathBuf::from("/repo"),
            main_worktree_root: PathBuf::from("/repo"),
            worktree_role: crate::shared::types::WorktreeRole::Main,
            git_dir: None,
            common_git_dir: None,
            branch_name: None,
            branch_ref: None,
            head_oid: None,
            branch_status,
        }
    }

    fn ready_payload(drifted: Option<bool>) -> ReadinessPayload {
        let mut payload = ops::blocked_readiness_for_path("repo", "fixture");
        payload.status = ReadinessStatus::Ready;
        payload.summary = "The index is ready.".to_string();
        payload.reason = None;
        payload.drifted = drifted;
        payload
    }

    #[test]
    fn apply_branch_readiness_pins_exact_detached_commit() {
        let mut named = ready_payload(Some(false));
        apply_branch_readiness(&mut named, &detached_context(BranchStatus::Named));
        assert_eq!(named.status, ReadinessStatus::Ready);
        assert!(named.reason.is_none());

        let mut pinned = ready_payload(Some(false));
        apply_branch_readiness(&mut pinned, &detached_context(BranchStatus::Detached));
        assert_eq!(pinned.status, ReadinessStatus::Ready);
        assert!(
            pinned.reason.is_none(),
            "pinned detached commit must not carry a branch-ambiguity reason"
        );
    }

    #[test]
    fn apply_branch_readiness_degrades_unprovable_or_ambiguous_branches() {
        let detached_caveat = BranchStatus::Detached.branch_scope_caveat();
        for drifted in [Some(true), None] {
            let mut payload = ready_payload(drifted);
            apply_branch_readiness(&mut payload, &detached_context(BranchStatus::Detached));
            assert_eq!(
                payload.status,
                ReadinessStatus::Degraded,
                "detached with drifted={drifted:?} must degrade"
            );
            assert_eq!(payload.reason.as_deref(), Some(detached_caveat.as_str()));
        }

        for status in [BranchStatus::Unreadable, BranchStatus::Unknown] {
            let mut payload = ready_payload(Some(false));
            apply_branch_readiness(&mut payload, &detached_context(status));
            assert_eq!(
                payload.status,
                ReadinessStatus::Degraded,
                "{status:?} must degrade even when un-drifted"
            );
            assert_eq!(
                payload.reason.as_deref(),
                Some(status.branch_scope_caveat().as_str())
            );
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
    fn impact_request_promotes_file_looking_symbol_to_file_anchor() {
        let mut input = impact_input();
        input.symbol = Some("src/mcp/tools.rs".to_string());

        let request = impact_request(&input).unwrap();

        assert_eq!(
            request.anchor,
            ImpactAnchor::File {
                path: "src/mcp/tools.rs".to_string(),
                line: None,
            }
        );
    }

    #[test]
    fn impact_request_promoted_symbol_file_anchor_accepts_line() {
        let mut input = impact_input();
        input.symbol = Some("src/mcp/tools.rs".to_string());
        input.line = Some(42);

        let request = impact_request(&input).unwrap();

        assert_eq!(
            request.anchor,
            ImpactAnchor::File {
                path: "src/mcp/tools.rs".to_string(),
                line: Some(42),
            }
        );
    }

    #[test]
    fn impact_request_keeps_plain_symbol_anchor() {
        let mut input = impact_input();
        input.symbol = Some("PolicyRuleValidator".to_string());

        let request = impact_request(&input).unwrap();

        assert_eq!(
            request.anchor,
            ImpactAnchor::Symbol {
                name: "PolicyRuleValidator".to_string(),
            }
        );
    }

    #[test]
    fn corrected_impact_call_prefers_handle_over_other_anchors() {
        // Two anchors fail validation; the corrected retry keeps the most
        // precise one (the handle) as a single ready-to-issue call.
        let mut input = impact_input();
        input.handle = Some("abcdef012345".to_string());
        input.symbol = Some("PolicyRuleValidator".to_string());

        assert!(impact_request(&input).is_err());
        assert_eq!(
            corrected_impact_call(&input),
            Some(json!({ "handle": ":abcdef012345" }))
        );
    }

    #[test]
    fn corrected_impact_call_promotes_file_looking_symbol_with_line() {
        let mut input = impact_input();
        input.symbol = Some("src/mcp/tools.rs".to_string());
        input.file = Some("src/other.rs".to_string());
        input.line = Some(7);

        // Two file-ish anchors fail validation; the explicit file wins and
        // keeps the line.
        assert!(impact_request(&input).is_err());
        assert_eq!(
            corrected_impact_call(&input),
            Some(json!({ "file": "src/other.rs", "line": 7 }))
        );
    }

    #[test]
    fn corrected_impact_call_is_none_without_any_anchor() {
        assert_eq!(corrected_impact_call(&impact_input()), None);
    }

    #[test]
    fn impact_request_promotes_relative_path_slot_to_file_anchor() {
        // A relative file path supplied in the project-root `path` slot with no
        // other anchor resolves to a File anchor.
        let mut input = impact_input();
        input.path = Some("packages/cloudflare/src/sandbox/runner.ts".to_string());

        let request = impact_request(&input).unwrap();

        assert_eq!(
            request.anchor,
            ImpactAnchor::File {
                path: "packages/cloudflare/src/sandbox/runner.ts".to_string(),
                line: None,
            }
        );
    }

    #[test]
    fn impact_request_promoted_path_slot_anchor_accepts_line() {
        // {"path": "...runner.ts", "line": 111} with no other anchor resolves to
        // a file anchor pinned at the line.
        let mut input = impact_input();
        input.path = Some("packages/cloudflare/src/sandbox/runner.ts".to_string());
        input.line = Some(111);

        let request = impact_request(&input).unwrap();

        assert_eq!(
            request.anchor,
            ImpactAnchor::File {
                path: "packages/cloudflare/src/sandbox/runner.ts".to_string(),
                line: Some(111),
            }
        );
    }

    #[test]
    fn impact_request_does_not_promote_absolute_path_slot() {
        // An absolute path keeps its project-root selector meaning and is never
        // promoted, so a call with no real anchor still errors -- with or
        // without a line, since the absent-anchor check precedes the line check.
        let mut input = impact_input();
        input.path = Some("/Users/dev/project".to_string());
        assert_eq!(
            impact_request(&input).unwrap_err(),
            "provide exactly one impact anchor: handle, symbol, or file"
        );

        input.line = Some(9);
        assert_eq!(
            impact_request(&input).unwrap_err(),
            "provide exactly one impact anchor: handle, symbol, or file"
        );
    }

    #[test]
    fn corrected_impact_call_promotes_relative_path_slot_with_line() {
        let mut input = impact_input();
        input.path = Some("packages/cloudflare/src/sandbox/runner.ts".to_string());
        input.line = Some(111);

        assert_eq!(
            corrected_impact_call(&input),
            Some(json!({
                "file": "packages/cloudflare/src/sandbox/runner.ts",
                "line": 111
            }))
        );
    }

    #[test]
    fn corrected_impact_call_ignores_absolute_path_slot() {
        // An absolute path is a project-root selector, not a file anchor, so the
        // corrected-call builder synthesizes nothing from it -- the error
        // envelope falls back to the generic search/symbol next actions.
        let mut input = impact_input();
        input.path = Some("/Users/dev/project".to_string());
        input.line = Some(9);

        assert_eq!(corrected_impact_call(&input), None);
    }

    #[test]
    fn impact_path_slot_stays_repo_root_when_an_explicit_anchor_is_present() {
        // With a real anchor present, a file-looking `path` retains its
        // project-root selector meaning: it is never consumed as a file anchor,
        // so the handler passes it through to root resolution unchanged.
        let mut input = impact_input();
        input.handle = Some(":abcdef012345".to_string());
        input.path = Some("packages/cloudflare/src/sandbox/runner.ts".to_string());

        assert_eq!(impact_path_as_file_anchor(&input), None);

        let request = impact_request(&input).unwrap();
        assert_eq!(
            request.anchor,
            ImpactAnchor::Segment {
                id: "abcdef012345".to_string()
            }
        );
    }

    #[test]
    fn normalize_repo_scope_maps_whole_repo_sentinels_to_none() {
        for sentinel in ["all", "ALL", " . ", "/", "*", "**", "", "   "] {
            assert_eq!(
                normalize_repo_scope(Some(sentinel)),
                None,
                "whole-repo sentinel {sentinel:?} must normalize to no scope"
            );
        }
        assert_eq!(normalize_repo_scope(None), None);
        // A real directory cone is preserved (and trimmed).
        assert_eq!(
            normalize_repo_scope(Some("  packages/core  ")),
            Some("packages/core".to_string())
        );
    }

    #[test]
    fn impact_request_drops_whole_repo_scope_sentinel_but_keeps_real_cone() {
        let mut input = impact_input();
        input.symbol = Some("load_auth_config".to_string());

        input.scope = Some("all".to_string());
        assert_eq!(
            impact_request(&input).unwrap().scope,
            None,
            "scope:'all' must be normalized to no cone"
        );

        input.scope = Some("packages/core".to_string());
        assert_eq!(
            impact_request(&input).unwrap().scope,
            Some("packages/core".to_string()),
            "a real directory cone must still scope the request"
        );
    }

    #[test]
    fn corrected_impact_call_drops_scope_for_no_scope_retry() {
        // The retry offered for an outside-scope error keeps the anchor but
        // never carries the scope that excluded it.
        let mut input = impact_input();
        input.symbol = Some("load_auth_config".to_string());
        input.scope = Some("packages/core".to_string());

        let corrected = corrected_impact_call(&input).unwrap();
        assert_eq!(corrected["symbol"], "load_auth_config");
        assert!(
            corrected.get("scope").is_none(),
            "corrected retry must not carry a scope: {corrected:?}"
        );
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
        assert_eq!(
            actions[0].arguments,
            Some(json!({ "handle": ":abcdef012345" }))
        );
    }

    fn overview_payload_fixture(
        status: OperationStatus,
        top_symbols: Vec<ops::OverviewTopSymbol>,
        modules: Vec<ops::OverviewModule>,
    ) -> OverviewPayload {
        OverviewPayload {
            status,
            stats: ops::OverviewStats {
                indexed_files: 0,
                total_segments: 0,
                languages: Vec::new(),
            },
            top_symbols,
            modules,
            module_dependencies: Vec::new(),
            entry_points: Vec::new(),
        }
    }

    fn top_symbol(name: &str) -> ops::OverviewTopSymbol {
        ops::OverviewTopSymbol {
            name: name.to_string(),
            handle: "abcdef012345".to_string(),
            path: "src/storage/db.rs".to_string(),
            line_start: 10,
            line_end: 40,
            referencing_files: 27,
            definition_count: 1,
        }
    }

    fn module(name: &str, segments: u64) -> ops::OverviewModule {
        ops::OverviewModule {
            module: name.to_string(),
            segments,
        }
    }

    #[test]
    fn overview_next_actions_suggest_top_symbol_then_densest_module() {
        let payload = overview_payload_fixture(
            OperationStatus::Ok,
            vec![top_symbol("Db")],
            vec![module("src/cli", 900), module("src/storage", 700)],
        );

        let actions = overview_next_actions(&payload);

        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0].tool, TOOL_SYMBOL);
        assert_eq!(actions[0].arguments, Some(json!({ "name": "Db" })));
        assert_eq!(actions[1].tool, TOOL_SEARCH);
        assert_eq!(
            actions[1].arguments,
            Some(json!({ "query": "src/cli module responsibilities" }))
        );
    }

    #[test]
    fn overview_next_actions_fall_back_to_status_for_empty_digest() {
        let payload = overview_payload_fixture(OperationStatus::Empty, Vec::new(), Vec::new());

        let actions = overview_next_actions(&payload);

        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].tool, TOOL_STATUS);
        assert_eq!(actions[0].arguments, Some(json!({})));
    }

    #[test]
    fn overview_next_actions_keep_module_search_without_qualifying_types() {
        let payload =
            overview_payload_fixture(OperationStatus::Ok, Vec::new(), vec![module("scripts", 12)]);

        let actions = overview_next_actions(&payload);

        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].tool, TOOL_SEARCH);
        assert_eq!(
            actions[0].arguments,
            Some(json!({ "query": "scripts module responsibilities" }))
        );
    }

    fn scope_recovery_note(
        path: &str,
        line: usize,
        above: usize,
        below: usize,
    ) -> ops::TruncationNote {
        ops::TruncationNote {
            reason: crate::shared::constants::SCOPE_TRUNCATION_REASON.to_string(),
            scope_name: Some("loadPlugins".to_string()),
            scope_type: Some("function".to_string()),
            full_line_start: Some(71),
            full_line_end: Some(588),
            omitted_above: Some(above),
            omitted_below: Some(below),
            omitted_symbols: None,
            recovery: ops::RecoveryCall {
                tool: TOOL_CONTEXT.to_string(),
                arguments: json!({ "path": path, "line": line, "expansion": 500 }),
            },
        }
    }

    fn context_record(path: &str, truncation: Option<ops::TruncationNote>) -> ops::ContextRecord {
        ops::ContextRecord {
            path: path.to_string(),
            language: "rust".to_string(),
            scope_type: "function".to_string(),
            content: "fn load() { /* body */ }".to_string(),
            line_start: 84,
            line_end: 90,
            out_of_scope_disclosure: None,
            truncation,
        }
    }

    fn context_read_payload(records: Vec<ops::ContextRecord>) -> ReadPayload {
        ReadPayload {
            status: OperationStatus::Ok,
            records: records
                .into_iter()
                .map(|context| ops::ReadRecord {
                    status: ops::ReadStatus::Found,
                    source: ops::ReadSource::Location {
                        path: context.path.clone(),
                        line: context.line_start,
                    },
                    segment: None,
                    context: Some(context),
                    matching_handles: Vec::new(),
                    recovered_from: None,
                    message: None,
                })
                .collect(),
        }
    }

    #[test]
    fn context_summary_line_is_content_free_with_truncation_marker() {
        let payload = context_read_payload(vec![context_record(
            "manager.ts",
            Some(scope_recovery_note("manager.ts", 87, 13, 498)),
        )]);

        let summary = read_summary(&payload);

        assert!(
            !summary.contains("fn load()"),
            "summary must not mirror source content: {summary}"
        );
        assert!(
            summary.contains(&format!(
                "manager.ts:84-90\trust\tscope function\ttruncated: {} +13/-498 lines",
                crate::shared::constants::SCOPE_TRUNCATION_REASON
            )),
            "summary missing constant-sized context line with truncation marker: {summary}"
        );
    }

    #[test]
    fn context_summary_omits_marker_when_not_truncated() {
        let payload = context_read_payload(vec![context_record("manager.ts", None)]);

        let summary = read_summary(&payload);

        assert!(
            !summary.contains("truncated:"),
            "unbounded record must not carry a truncation marker: {summary}"
        );
    }

    #[test]
    fn recovery_action_is_prepended_first_and_carries_verbatim_arguments() {
        let payload = context_read_payload(vec![context_record(
            "manager.ts",
            Some(scope_recovery_note("manager.ts", 87, 13, 498)),
        )]);

        let actions = read_next_actions(&payload);

        assert_eq!(actions[0].tool, TOOL_CONTEXT);
        assert_eq!(
            actions[0].arguments,
            Some(json!({ "path": "manager.ts", "line": 87, "expansion": 500 })),
            "recovery arguments must be copied verbatim from the note"
        );
        assert!(
            actions[0].reason.contains("loadPlugins")
                && actions[0].reason.contains("manager.ts:71-588")
                && actions[0].reason.contains("13 lines above / 498 below"),
            "recovery reason must name the scope, full range, and omitted counts: {}",
            actions[0].reason
        );
    }

    #[test]
    fn recovery_actions_dedupe_per_path_and_cap_at_limit() {
        let mut records = Vec::new();
        for index in 0..(MAX_RECOVERY_ACTIONS + 3) {
            let path = format!("file{index}.ts");
            // Two truncated records for the same path must yield one action.
            records.push(context_record(
                &path,
                Some(scope_recovery_note(&path, 87, 1, 2)),
            ));
            records.push(context_record(
                &path,
                Some(scope_recovery_note(&path, 87, 1, 2)),
            ));
        }
        let payload = context_read_payload(records);

        let recovery = recovery_next_actions(&payload);

        assert_eq!(
            recovery.len(),
            MAX_RECOVERY_ACTIONS,
            "recovery actions must be capped at MAX_RECOVERY_ACTIONS"
        );
        let mut paths: Vec<String> = recovery
            .iter()
            .map(|action| {
                action.arguments.as_ref().unwrap()["path"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        paths.sort();
        paths.dedup();
        assert_eq!(
            paths.len(),
            MAX_RECOVERY_ACTIONS,
            "recovery actions must be deduped per path"
        );
    }

    fn handle_record(status: ops::ReadStatus, matching_handles: Vec<String>) -> ops::ReadRecord {
        ops::ReadRecord {
            status,
            source: ops::ReadSource::Handle {
                raw: ":abc".to_string(),
                normalized: "abc".to_string(),
            },
            segment: None,
            context: None,
            matching_handles,
            recovered_from: None,
            message: None,
        }
    }

    #[test]
    fn ambiguous_record_prefills_oneup_get_with_real_candidates_before_search() {
        let candidates = vec![
            "0b25cc46a316205a1afe69ccd11337e2".to_string(),
            "0b25cc46a316205a1afe69ccd1144abc".to_string(),
        ];
        let payload = ReadPayload {
            status: OperationStatus::Empty,
            records: vec![handle_record(
                ops::ReadStatus::Ambiguous,
                candidates.clone(),
            )],
        };

        let actions = read_next_actions(&payload);

        assert_eq!(actions[0].tool, TOOL_GET);
        assert_eq!(
            actions[0].arguments,
            Some(json!({ "handles": candidates })),
            "disambiguation action must prefill the real candidate ids"
        );
        assert!(
            actions.iter().any(|action| action.tool == TOOL_SEARCH),
            "generic search fallback must trail the disambiguation action"
        );
    }

    #[test]
    fn not_found_record_notes_absence_from_active_context() {
        let payload = ReadPayload {
            status: OperationStatus::Empty,
            records: vec![handle_record(ops::ReadStatus::NotFound, Vec::new())],
        };

        let actions = read_next_actions(&payload);

        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].tool, TOOL_SEARCH);
        assert!(actions[0].arguments.is_none());
        assert!(
            actions[0].reason.contains("active context"),
            "not_found guidance must note the handle is absent from the active context: {}",
            actions[0].reason
        );
    }

    #[test]
    fn rejected_record_offers_candidates_and_a_refined_search_not_a_repeat() {
        // A rejected identical retry whose original failure was ambiguous keeps
        // the cached candidate ids, so the follow-up prefills a disambiguating
        // oneup_get and steers the trailing search away from repeating the call.
        let candidates = vec![
            "0b25cc46a316205a1afe69ccd11337e2".to_string(),
            "0b25cc46a316205a1afe69ccd1144abc".to_string(),
        ];
        let payload = ReadPayload {
            status: OperationStatus::Empty,
            records: vec![handle_record(ops::ReadStatus::Rejected, candidates.clone())],
        };

        let actions = read_next_actions(&payload);

        assert_eq!(actions[0].tool, TOOL_GET);
        assert_eq!(
            actions[0].arguments,
            Some(json!({ "handles": candidates })),
            "a rejected retry with cached candidates prefills oneup_get with the real ids"
        );
        let search = actions
            .iter()
            .find(|action| action.tool == TOOL_SEARCH)
            .expect("a rejected retry must still trail a search fallback");
        assert!(
            search.reason.contains("refined query"),
            "the rejection must steer toward a refined query, not a repeat: {}",
            search.reason
        );
    }

    #[test]
    fn rejected_record_without_candidates_only_offers_a_refined_search() {
        // A rejected retry whose original failure was a plain not-found carries
        // no candidates, so only the refined-query search fallback is offered.
        let payload = ReadPayload {
            status: OperationStatus::Empty,
            records: vec![handle_record(ops::ReadStatus::Rejected, Vec::new())],
        };

        let actions = read_next_actions(&payload);

        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].tool, TOOL_SEARCH);
        assert!(actions[0].reason.contains("refined query"));
    }

    fn search_hit(handle: &str) -> ops::SearchHit {
        ops::SearchHit {
            handle: handle.to_string(),
            path: "src/lib.rs".to_string(),
            language: "rust".to_string(),
            kind: "function".to_string(),
            score: 100,
            line_start: 1,
            line_end: 10,
            breadcrumb: None,
            symbol: None,
            defined_symbols: Vec::new(),
        }
    }

    fn search_payload(hits: Vec<ops::SearchHit>) -> SearchPayload {
        SearchPayload {
            status: OperationStatus::Ok,
            results: hits,
            degraded_reason: None,
            index_scope: None,
        }
    }

    #[test]
    fn select_hydration_handles_empty_input_yields_empty_output() {
        assert!(select_hydration_handles(&[]).is_empty());
    }

    #[test]
    fn select_hydration_handles_single_result_returns_that_one_handle() {
        let handles = select_hydration_handles(&[search_hit("aaa")]);
        assert_eq!(handles, vec![":aaa".to_string()]);
    }

    #[test]
    fn select_hydration_handles_two_to_four_results_returns_all_of_them() {
        for count in 2..=HYDRATION_BATCH_MAX_HANDLES {
            let hits: Vec<_> = (0..count).map(|i| search_hit(&format!("h{i}"))).collect();
            let handles = select_hydration_handles(&hits);
            assert_eq!(
                handles.len(),
                count,
                "{count} results must all be recommended"
            );
            let expected: Vec<String> = (0..count).map(|i| format!(":h{i}")).collect();
            assert_eq!(handles, expected);
        }
    }

    #[test]
    fn select_hydration_handles_bounds_many_results_to_top_four_in_ranked_order() {
        let hits: Vec<_> = (0..8).map(|i| search_hit(&format!("h{i}"))).collect();

        let handles = select_hydration_handles(&hits);

        assert_eq!(
            handles.len(),
            HYDRATION_BATCH_MAX_HANDLES,
            "a many-result set must never recommend more than the cap"
        );
        assert_eq!(
            handles,
            vec![
                ":h0".to_string(),
                ":h1".to_string(),
                ":h2".to_string(),
                ":h3".to_string(),
            ],
            "handles must be the top HYDRATION_BATCH_MAX_HANDLES in ranked order"
        );
    }

    #[test]
    fn select_hydration_handles_carry_the_colon_prefix() {
        let handles = select_hydration_handles(&[search_hit("deadbeef")]);
        assert!(
            handles.iter().all(|handle| handle.starts_with(':')),
            "every recommended handle must carry the ':' prefix: {handles:?}"
        );
    }

    #[test]
    fn search_next_actions_default_get_recommends_the_selected_batch() {
        let payload = search_payload(vec![
            search_hit("h0"),
            search_hit("h1"),
            search_hit("h2"),
            search_hit("h3"),
            search_hit("h4"),
        ]);

        let actions = search_next_actions(&payload);

        let get = actions
            .iter()
            .find(|action| action.tool == TOOL_GET)
            .expect("a multi-result search must recommend a batched oneup_get");
        assert_eq!(
            get.arguments,
            Some(json!({
                "handles": [":h0", ":h1", ":h2", ":h3"]
            })),
            "the default get action must prefill the bounded selected batch"
        );
    }

    #[test]
    fn search_next_actions_single_result_recommends_exactly_one_handle() {
        let payload = search_payload(vec![search_hit("only")]);

        let actions = search_next_actions(&payload);

        let get = actions
            .iter()
            .find(|action| action.tool == TOOL_GET)
            .expect("a single-result search must still recommend oneup_get");
        assert_eq!(
            get.arguments,
            Some(json!({ "handles": [":only"] })),
            "a single result recommends exactly that one handle"
        );
    }

    fn facts_fixture(
        launch_subdir: Option<&str>,
        directories: &[&str],
    ) -> crate::mcp::types::FactsEnvelope {
        use crate::mcp::types::{DirectoryStats, FactsEnvelope};

        let per_directory_stats = directories
            .iter()
            .enumerate()
            .map(|(idx, directory)| DirectoryStats {
                directory: directory.to_string(),
                file_count: 100 - idx * 10,
                estimated_vectors: (100 - idx * 10) * 10,
            })
            .collect();

        FactsEnvelope {
            per_directory_stats,
            workspace_manifests: vec![],
            sparse_checkout: None,
            launch_subdir: launch_subdir.map(|s| s.to_string()),
            suggestions: vec![],
            file_count_total: 5000,
            vector_estimate_total: 50000,
            vector_estimate_basis: None,
            vector_estimate_low: None,
            vector_estimate_high: None,
        }
    }

    fn scope_add_dirs(action: &NextAction) -> Vec<String> {
        action.arguments.as_ref().unwrap()["scope_add"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn facts_next_actions_repo_root_emits_multiple_actions_without_dangling_or() {
        // Repo-root path (no launch_subdir): first action is a primary
        // imperative, followed by ranked alternatives. Multiple actions, all
        // carrying scope_add, none beginning with "Or ".
        let facts = facts_fixture(None, &["services", "libs", "tools"]);

        let actions = facts_next_actions(&facts);

        assert!(
            actions.len() >= 2,
            "repo-root facts must emit multiple ranked actions; got {}",
            actions.len()
        );
        for action in &actions {
            assert_eq!(action.tool, TOOL_START);
            assert!(
                !action.reason.starts_with("Or "),
                "no next_action reason may begin with 'Or '; got {:?}",
                action.reason
            );
            assert!(
                !scope_add_dirs(action).is_empty(),
                "every scope action must carry a scope_add directory"
            );
        }
        assert_eq!(actions[0].reason, "Index the largest directory: services");
        assert_eq!(scope_add_dirs(&actions[0]), vec!["services".to_string()]);
    }

    #[test]
    fn facts_next_actions_launch_subdir_first_then_alternatives_without_dangling_or() {
        // launch_subdir path: the launch action is first, then ranked scope
        // alternatives (the launch directory is skipped). Multiple actions, all
        // carrying scope_add, none beginning with "Or ".
        let facts = facts_fixture(Some("services"), &["services", "libs", "tools"]);

        let actions = facts_next_actions(&facts);

        assert!(
            actions.len() >= 2,
            "launch_subdir facts must emit multiple actions; got {}",
            actions.len()
        );
        assert_eq!(
            actions[0].reason,
            "Index the launch subdirectory first: services"
        );
        assert_eq!(scope_add_dirs(&actions[0]), vec!["services".to_string()]);

        for action in &actions {
            assert_eq!(action.tool, TOOL_START);
            assert!(
                !action.reason.starts_with("Or "),
                "no next_action reason may begin with 'Or '; got {:?}",
                action.reason
            );
            assert!(
                !scope_add_dirs(action).is_empty(),
                "every scope action must carry a scope_add directory"
            );
        }

        // The launch directory is not re-suggested as a scope alternative.
        let scope_dirs: Vec<String> = actions[1..].iter().flat_map(scope_add_dirs).collect();
        assert!(
            !scope_dirs.contains(&"services".to_string()),
            "launch_subdir must not be duplicated as a scope alternative; got {:?}",
            scope_dirs
        );
        assert_eq!(
            actions[1].reason,
            "Alternatively, index the 2nd largest directory: libs"
        );
    }
}
