use schemars::{json_schema, JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

use crate::shared::types::{BranchStatus, DaemonRefreshState, DaemonWatchStatus, WorktreeRole};

pub const TOOL_STATUS: &str = "oneup_status";
pub const TOOL_START: &str = "oneup_start";
pub const TOOL_SEARCH: &str = "oneup_search";
pub const TOOL_GET: &str = "oneup_get";
pub const TOOL_SYMBOL: &str = "oneup_symbol";
pub const TOOL_CONTEXT: &str = "oneup_context";
pub const TOOL_IMPACT: &str = "oneup_impact";
pub const TOOL_STRUCTURAL: &str = "oneup_structural";

pub const RETAINED_PUBLIC_TOOLS: [&str; 8] = [
    TOOL_STATUS,
    TOOL_START,
    TOOL_SEARCH,
    TOOL_GET,
    TOOL_SYMBOL,
    TOOL_CONTEXT,
    TOOL_IMPACT,
    TOOL_STRUCTURAL,
];

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StartMode {
    #[default]
    #[serde(alias = "auto")]
    IndexIfNeeded,
    IndexIfMissing,
    Reindex,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StatusInput {
    pub path: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StartInput {
    #[serde(default)]
    pub mode: StartMode,
    pub path: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchInput {
    pub query: String,
    pub limit: Option<usize>,
    pub path: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetInput {
    #[serde(default)]
    #[schemars(description = "Durable result handles returned by oneup_search or oneup_symbol.")]
    pub handles: Vec<String>,
    pub path: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ContextInput {
    #[serde(default)]
    #[schemars(
        description = "Repository-scoped locations for file-line context retrieval. Use paths relative to the configured repository and 1-based line numbers."
    )]
    pub locations: Vec<ReadLocationInput>,
    pub path: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(description = "A repository-contained file-line location for oneup_context retrieval.")]
pub struct ReadLocationInput {
    #[schemars(
        description = "File path relative to the configured repository. Absolute paths are accepted only when they stay inside that repository."
    )]
    pub path: String,
    #[schemars(description = "1-based source line to retrieve bounded context around.")]
    pub line: usize,
    #[schemars(
        description = "Optional number of fallback lines to include around the requested line when no enclosing scope is found."
    )]
    pub expansion: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StructuralInput {
    #[schemars(description = "Tree-sitter query pattern in S-expression syntax.")]
    pub pattern: String,
    #[schemars(
        description = "Optional supported language filter such as rust, python, go, or typescript."
    )]
    pub language: Option<String>,
    pub limit: Option<usize>,
    pub path: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SymbolIncludeInput {
    #[default]
    Definitions,
    References,
    Both,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SymbolInput {
    pub name: String,
    #[serde(default)]
    pub include: SymbolIncludeInput,
    #[serde(default)]
    pub fuzzy: bool,
    pub path: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImpactInput {
    #[serde(alias = "segment_id")]
    #[schemars(
        description = "Result handle returned by oneup_search or oneup_symbol. A leading ':' is accepted. The older segment_id field name is accepted as a compatibility alias."
    )]
    pub handle: Option<String>,
    pub symbol: Option<String>,
    pub file: Option<String>,
    pub line: Option<usize>,
    pub scope: Option<String>,
    pub depth: Option<usize>,
    pub limit: Option<usize>,
    pub path: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ToolEnvelope {
    pub status: String,
    pub summary: String,
    #[schemars(schema_with = "json_object_schema")]
    pub data: Value,
    pub next_actions: Vec<NextAction>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReadinessContextMetadata {
    pub context_id: String,
    pub main_worktree_root: PathBuf,
    pub worktree_role: WorktreeRole,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_ref: Option<String>,
    pub branch_status: BranchStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_oid: Option<String>,
    pub watch_status: DaemonWatchStatus,
    pub last_update_state: DaemonRefreshState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_update_started_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_update_completed_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_update_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct NextAction {
    pub tool: String,
    pub reason: String,
    #[schemars(schema_with = "json_object_schema")]
    pub arguments: Value,
}

fn json_object_schema(_: &mut SchemaGenerator) -> Schema {
    json_schema!({
        "type": "object"
    })
}
