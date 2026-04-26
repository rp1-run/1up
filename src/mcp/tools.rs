use std::path::Path;
use std::time::Instant;

use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content},
    tool, tool_router,
};
use serde::Serialize;
use serde_json::{json, Value};

use crate::daemon::registry::Registry;
use crate::indexer::embedder::EmbeddingRuntime;
use crate::indexer::pipeline;
use crate::mcp::ops::{
    self, McpProjectRoots, OperationStatus, ReadLocation, ReadPayload, ReadinessPayload,
    ReadinessStatus, SearchPayload, SymbolInclude, SymbolLookupRequest, SymbolPayload,
};
use crate::mcp::server::OneupMcpServer;
use crate::mcp::types::{
    ImpactInput, NextAction, PrepareInput, PrepareMode, ReadInput, SearchInput, SymbolIncludeInput,
    SymbolInput, ToolEnvelope,
};
use crate::search::impact::{ImpactAnchor, ImpactRequest, ImpactResultEnvelope, ImpactStatus};
use crate::shared::config;
use crate::shared::constants::MAX_SEARCH_RESULTS;
use crate::shared::project;
use crate::shared::types::{RunScope, SetupTimings};
use crate::storage::db::Db;
use crate::storage::schema;

const DEFAULT_SEARCH_LIMIT: usize = 5;

#[tool_router(router = tool_router, vis = "pub(crate)")]
impl OneupMcpServer {
    #[tool(
        name = "oneup_prepare",
        description = "Check whether the local repository is ready for 1up MCP search. Use before discovery when index state is unknown.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<ToolEnvelope>().unwrap(),
        annotations(title = "Prepare 1up", destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    pub async fn oneup_prepare(
        &self,
        Parameters(input): Parameters<PrepareInput>,
    ) -> CallToolResult {
        let roots = match self.roots(input.path.as_deref()) {
            Ok(roots) => roots,
            Err(err) => return error_result("error", err.to_string(), vec![]),
        };

        let payload = match prepare(&roots, input.mode).await {
            Ok(payload) => payload,
            Err(err) => return indexed_tool_error(err.to_string()),
        };

        let status = status_string(&payload.status);
        let summary = payload.summary.clone();
        result(envelope(
            status,
            summary,
            payload_value(&payload),
            readiness_next_actions(&payload),
        ))
    }

    #[tool(
        name = "oneup_search",
        description = "Search source code by meaning; use for discovery, not proof of completeness. Follow returned handles with read, symbol, or impact tools.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<ToolEnvelope>().unwrap(),
        annotations(title = "Search Code", read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    pub async fn oneup_search(&self, Parameters(input): Parameters<SearchInput>) -> CallToolResult {
        if input.query.trim().is_empty() {
            return error_result(
                "error",
                "query cannot be empty",
                vec![action(
                    "oneup_search",
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

        match ops::run_search(&roots.state_root, &input.query, limit).await {
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
        name = "oneup_read",
        description = "Read code from 1up handles or precise file locations after search, diagnostics, or review comments. Use this to hydrate selected results.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<ToolEnvelope>().unwrap(),
        annotations(title = "Read Code", read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    pub async fn oneup_read(&self, Parameters(input): Parameters<ReadInput>) -> CallToolResult {
        if input.handles.is_empty() && input.locations.is_empty() {
            return error_result(
                "error",
                "provide at least one handle or precise location",
                vec![action(
                    "oneup_search",
                    "Search first to obtain durable handles for oneup_read.",
                    json!({ "query": "<code discovery query>" }),
                )],
            );
        }
        if let Some(location) = input.locations.iter().find(|location| location.line == 0) {
            return error_result(
                "error",
                format!("line must be 1-based for {}", location.path),
                vec![],
            );
        }

        let roots = match self.roots(input.path.as_deref()) {
            Ok(roots) => roots,
            Err(err) => return error_result("error", err.to_string(), vec![]),
        };

        let mut records = Vec::new();
        if !input.handles.is_empty() {
            match ops::read_handles(&roots.state_root, &input.handles).await {
                Ok(payload) => records.extend(payload.records),
                Err(err) => return indexed_tool_error(err.to_string()),
            }
        }
        if !input.locations.is_empty() {
            let locations = input
                .locations
                .iter()
                .map(|location| ReadLocation {
                    path: location.path.clone(),
                    line: location.line,
                    expansion: location.expansion,
                })
                .collect::<Vec<_>>();
            match ops::read_locations(&roots.source_root, &locations) {
                Ok(payload) => records.extend(payload.records),
                Err(err) => return error_result("error", err.to_string(), vec![]),
            }
        }

        let payload = ReadPayload {
            status: aggregate_read_status(&records),
            records,
        };
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

    #[tool(
        name = "oneup_symbol",
        description = "Find definitions and optionally references for a symbol; use after search when completeness matters. Results are grouped by definition and reference.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<ToolEnvelope>().unwrap(),
        annotations(title = "Verify Symbol", read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    pub async fn oneup_symbol(&self, Parameters(input): Parameters<SymbolInput>) -> CallToolResult {
        if input.name.trim().is_empty() {
            return error_result(
                "error",
                "symbol name cannot be empty",
                vec![action(
                    "oneup_search",
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

        match ops::lookup_symbol(&roots.state_root, request).await {
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
        description = "Explore likely impact from a segment, symbol, or file anchor. Use after search or read when you need advisory follow-up targets.",
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
                            "oneup_search",
                            "Search to obtain a precise segment handle for impact exploration.",
                            json!({ "query": "<code discovery query>" }),
                        ),
                        action(
                            "oneup_symbol",
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

        match ops::explore_impact(&roots.state_root, request).await {
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

    fn roots(&self, path: Option<&str>) -> anyhow::Result<McpProjectRoots> {
        match path.filter(|path| !path.trim().is_empty()) {
            Some(path) => ops::resolve_project(Path::new(path)),
            None => Ok(McpProjectRoots {
                state_root: self.state_root.clone(),
                source_root: self.source_root.clone(),
            }),
        }
    }
}

async fn prepare(roots: &McpProjectRoots, mode: PrepareMode) -> anyhow::Result<ReadinessPayload> {
    let readiness = ops::classify_readiness(&roots.state_root, &roots.source_root).await;
    match mode {
        PrepareMode::Check => Ok(readiness),
        PrepareMode::IndexIfMissing if readiness.status == ReadinessStatus::Missing => {
            run_index(roots, false).await?;
            Ok(ops::classify_readiness(&roots.state_root, &roots.source_root).await)
        }
        PrepareMode::IndexIfNeeded
            if matches!(
                readiness.status,
                ReadinessStatus::Missing | ReadinessStatus::Degraded
            ) =>
        {
            run_index(roots, false).await?;
            Ok(ops::classify_readiness(&roots.state_root, &roots.source_root).await)
        }
        PrepareMode::Reindex => {
            run_index(roots, true).await?;
            Ok(ops::classify_readiness(&roots.state_root, &roots.source_root).await)
        }
        _ => Ok(readiness),
    }
}

async fn run_index(
    roots: &McpProjectRoots,
    rebuild: bool,
) -> anyhow::Result<pipeline::PipelineStats> {
    if project::read_project_id(&roots.state_root).is_err() {
        project::write_project_id(&roots.state_root)?;
    }

    let db_path = config::project_db_path(&roots.state_root);
    let registry = Registry::load()?;
    let indexing_config = config::resolve_indexing_config(
        None,
        None,
        registry.indexing_config_for(&roots.state_root),
    )?;
    let mut setup = SetupTimings::new(Instant::now());

    let db_start = Instant::now();
    let db = Db::open_rw(&db_path).await?;
    let conn = db.connect_tuned().await?;
    if rebuild {
        schema::rebuild(&conn).await?;
    } else {
        schema::prepare_for_write(&conn).await?;
    }
    setup.db_prepare_ms = db_start.elapsed().as_millis();

    let model_start = Instant::now();
    let mut runtime = EmbeddingRuntime::default();
    runtime
        .prepare_for_indexing_with_progress(indexing_config.embed_threads, false)
        .await;
    setup.model_prepare_ms = model_start.elapsed().as_millis();

    pipeline::run_with_scope_setup_and_progress_root(
        &conn,
        &roots.source_root,
        runtime.current_embedder(),
        &RunScope::Full,
        &indexing_config,
        None,
        false,
        Some(setup),
        None,
        Some(&roots.state_root),
    )
    .await
    .map_err(Into::into)
}

fn impact_request(input: &ImpactInput) -> Result<ImpactRequest, String> {
    let anchors = [
        input
            .segment_id
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
        return Err("provide exactly one impact anchor: segment_id, symbol, or file".to_string());
    }
    if input.line.is_some()
        && input
            .file
            .as_ref()
            .is_none_or(|file| file.trim().is_empty())
    {
        return Err("line can only be used with a file impact anchor".to_string());
    }

    let anchor = if let Some(segment_id) = input.segment_id.as_ref() {
        ImpactAnchor::Segment {
            id: segment_id.clone(),
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

fn readiness_next_actions(payload: &ReadinessPayload) -> Vec<NextAction> {
    match payload.status {
        ReadinessStatus::Ready => vec![action(
            "oneup_search",
            "Start discovery with a task-specific code search.",
            json!({ "query": "<code discovery query>" }),
        )],
        ReadinessStatus::Degraded => vec![
            action(
                "oneup_search",
                "Search is available, but results may be degraded.",
                json!({ "query": "<code discovery query>" }),
            ),
            action(
                "oneup_prepare",
                "Refresh readiness after fixing the degraded index state.",
                json!({ "mode": "check" }),
            ),
        ],
        ReadinessStatus::Missing => vec![action(
            "oneup_prepare",
            "Create the local 1up index explicitly before searching.",
            json!({ "mode": "index_if_missing" }),
        )],
        ReadinessStatus::Indexing => vec![action(
            "oneup_prepare",
            "Poll readiness until indexing completes.",
            json!({ "mode": "check" }),
        )],
        ReadinessStatus::Stale => vec![action(
            "oneup_prepare",
            "Rebuild the local index explicitly before searching.",
            json!({ "mode": "reindex" }),
        )],
    }
}

fn search_next_actions(payload: &SearchPayload) -> Vec<NextAction> {
    let Some(first) = payload.results.first() else {
        return vec![action(
            "oneup_search",
            "Try a narrower or differently worded discovery query.",
            json!({ "query": "<refined code discovery query>" }),
        )];
    };

    let mut actions = vec![
        action(
            "oneup_read",
            "Hydrate the top search result before editing or concluding.",
            json!({ "handles": [format!(":{}", first.handle)] }),
        ),
        action(
            "oneup_impact",
            "Explore likely follow-up targets from the selected result.",
            json!({ "segment_id": first.handle }),
        ),
    ];

    if let Some(symbol) = &first.symbol {
        actions.push(action(
            "oneup_symbol",
            "Verify definitions and references when completeness matters.",
            json!({ "name": symbol, "include": "both", "fuzzy": true }),
        ));
    }

    actions
}

fn read_next_actions(payload: &ReadPayload) -> Vec<NextAction> {
    let Some(segment) = payload
        .records
        .iter()
        .filter_map(|record| record.segment.as_ref())
        .next()
    else {
        return vec![action(
            "oneup_search",
            "Search again to obtain a valid handle or precise file location.",
            json!({ "query": "<code discovery query>" }),
        )];
    };

    let mut actions = vec![action(
        "oneup_impact",
        "Explore likely impact from the hydrated segment.",
        json!({ "segment_id": segment.handle }),
    )];
    if let Some(symbol) = segment.defined_symbols.first() {
        actions.push(action(
            "oneup_symbol",
            "Verify references for the symbol defined in this segment.",
            json!({ "name": symbol, "include": "both", "fuzzy": true }),
        ));
    }
    actions
}

fn symbol_next_actions(payload: &SymbolPayload, name: &str) -> Vec<NextAction> {
    let handles = payload
        .definitions
        .iter()
        .chain(payload.references.iter())
        .take(3)
        .map(|record| format!(":{}", record.handle))
        .collect::<Vec<_>>();

    if handles.is_empty() {
        return vec![action(
            "oneup_search",
            "Search by behavior or context to find candidate symbols.",
            json!({ "query": name }),
        )];
    }

    vec![
        action(
            "oneup_read",
            "Read the symbol matches before using them as evidence.",
            json!({ "handles": handles }),
        ),
        action(
            "oneup_impact",
            "Explore likely impact from this symbol anchor.",
            json!({ "symbol": name }),
        ),
    ]
}

fn impact_next_actions(payload: &ImpactResultEnvelope) -> Vec<NextAction> {
    if let Some(first) = payload.results.first() {
        return vec![action(
            "oneup_read",
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
            "oneup_read",
            "Read contextual impact guidance when no primary result is available.",
            json!({ "handles": [format!(":{}", contextual.segment_id)] }),
        )];
    }

    if let Some(hint) = &payload.hint {
        if let Some(segment_id) = &hint.suggested_segment_id {
            return vec![action(
                "oneup_impact",
                "Retry impact with the suggested narrower segment anchor.",
                json!({ "segment_id": segment_id }),
            )];
        }
        if let Some(scope) = &hint.suggested_scope {
            return vec![action(
                "oneup_search",
                "Search within the suggested scope to find a narrower anchor.",
                json!({ "query": scope }),
            )];
        }
    }

    vec![action(
        "oneup_search",
        "Search for a narrower segment or symbol before retrying impact.",
        json!({ "query": "<narrower impact anchor>" }),
    )]
}

fn aggregate_read_status(records: &[ops::ReadRecord]) -> OperationStatus {
    if records.is_empty() {
        return OperationStatus::Empty;
    }
    if records
        .iter()
        .all(|record| record.status == ops::ReadStatus::Found)
    {
        OperationStatus::Ok
    } else if records
        .iter()
        .any(|record| record.status == ops::ReadStatus::Found)
    {
        OperationStatus::Partial
    } else {
        OperationStatus::Empty
    }
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
    match payload.status {
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
    }
}

fn read_summary(payload: &ReadPayload) -> String {
    format!(
        "Read {} code record(s); status is {}.",
        payload.records.len(),
        status_string(&payload.status)
    )
}

fn symbol_summary(payload: &SymbolPayload, name: &str) -> String {
    format!(
        "Found {} definition(s) and {} reference(s) for symbol \"{}\".",
        payload.definitions.len(),
        payload.references.len(),
        name
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
            "oneup_prepare",
            "Check readiness and index state before retrying this MCP call.",
            json!({ "mode": "check" }),
        )],
    )
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

fn status_string<T: Serialize>(status: &T) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|value| value.as_str().map(ToString::to_string))
        .unwrap_or_else(|| "ok".to_string())
}
