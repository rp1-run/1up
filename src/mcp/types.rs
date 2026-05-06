use schemars::{json_schema, JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

use crate::shared::types::{BranchStatus, DaemonRefreshState, DaemonWatchStatus, WorktreeRole};

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PrepareMode {
    #[default]
    #[serde(alias = "default")]
    #[serde(alias = "read")]
    Check,
    IndexIfMissing,
    #[serde(alias = "auto")]
    IndexIfNeeded,
    Reindex,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PrepareInput {
    #[serde(default)]
    pub mode: PrepareMode,
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
pub struct ReadInput {
    #[serde(default)]
    pub handles: Vec<String>,
    #[serde(default)]
    #[schemars(
        description = "Repository-scoped locations for file-line context retrieval. Use paths relative to the configured repository and 1-based line numbers."
    )]
    pub locations: Vec<ReadLocationInput>,
    pub path: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(
    description = "A repository-contained file-line location for oneup_read context retrieval."
)]
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
    pub segment_id: Option<String>,
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
