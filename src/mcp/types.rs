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
pub const TOOL_OVERVIEW: &str = "oneup_overview";

pub const RETAINED_PUBLIC_TOOLS: [&str; 9] = [
    TOOL_STATUS,
    TOOL_START,
    TOOL_SEARCH,
    TOOL_GET,
    TOOL_SYMBOL,
    TOOL_CONTEXT,
    TOOL_IMPACT,
    TOOL_STRUCTURAL,
    TOOL_OVERVIEW,
];

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
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
pub struct OverviewInput {
    pub path: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StartInput {
    #[serde(default)]
    pub mode: StartMode,
    pub path: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Optional list of repo-relative directory cones to add to the indexed scope (union operation). Each path must be repo-relative and cannot contain `../` or absolute paths. Scope roots are persisted per state_root and survive branch switches and daemon restarts."
    )]
    pub scope_add: Option<Vec<String>>,
    #[serde(default)]
    #[schemars(
        description = "Optional list of repo-relative directory cones to narrow the indexed scope to. Must be a subset of the currently indexed scope. Narrowing triggers a full rebuild via atomic StagingRebuild and is never an implicit side effect."
    )]
    pub scope_narrow: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchInput {
    pub query: String,
    #[serde(default)]
    #[schemars(
        description = "Optional additional queries for a multi-part question. Pass every distinct aspect of the question in one call (up to four total, including `query`); 1up runs the hybrid search per aspect and merges the ranked results, so one call replaces several sequential searches. Omit for a single-aspect question."
    )]
    pub queries: Option<Vec<String>>,
    pub limit: Option<usize>,
    pub path: Option<String>,
    #[schemars(
        description = "Optional repo-relative directory prefix (e.g. \"src/foo\") to constrain results to that subtree. Distinct from `path`, which selects the repository root."
    )]
    pub path_prefix: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetInput {
    #[serde(default)]
    #[schemars(description = "Durable result handles returned by oneup_search or oneup_symbol.")]
    pub handles: Vec<String>,
    pub path: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Optional verbosity level: \"default\" omits symbol lists and redundant summaries; \"full\" includes detailed symbol metadata. Defaults to \"default\"."
    )]
    pub verbosity: Option<String>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "json_object_schema")]
    pub arguments: Option<Value>,
}

/// Directory statistics for monorepo facts envelope, showing file and vector counts per top-level directory.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DirectoryStats {
    /// Directory name relative to repo root (e.g., "services", "libs", "tools").
    pub directory: String,
    /// Number of files in this directory (recursive count).
    pub file_count: usize,
    /// Estimated vector count: file_count / 10 (conservative assumption of ~10 vectors per file).
    pub estimated_vectors: usize,
}

/// Facts envelope returned on first-run gate in large monorepos.
/// Provides directory statistics, workspace manifests, and sparse-checkout info so agents can make informed scope decisions.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FactsEnvelope {
    /// Per-directory file and vector statistics for the top-level directories.
    /// Excludes tool/editor dot-directories (.idea, .claude, .vscode, .1up, .agentdocs).
    pub per_directory_stats: Vec<DirectoryStats>,
    /// Paths to workspace manifest files detected (e.g., Cargo.toml, package.json).
    /// These are paths relative to the repo root where manifest-like files were found.
    pub workspace_manifests: Vec<String>,
    /// Output of `git sparse-checkout list` if sparse-checkout is active; None otherwise.
    pub sparse_checkout: Option<String>,
    /// Launch subdirectory before project root resolution (if 1up was started from a subdirectory).
    /// Included as a suggestion for the default scope.
    pub launch_subdir: Option<String>,
    /// Ranked suggestions for scope cones based on largest directories.
    /// Formatted without leading "Or" phrasing for coherent agent consumption.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suggestions: Vec<String>,
    /// Total file count across all directories (gitignore-aware tracked count).
    pub file_count_total: usize,
    /// Total estimated vector count based on measured language densities.
    pub vector_estimate_total: usize,
    /// Basis for the vector estimate explaining how it was computed and confidence level.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vector_estimate_basis: Option<String>,
    /// Conservative lower bound on vector count (15 segments/file × total files).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vector_estimate_low: Option<usize>,
    /// Pessimistic upper bound on vector count (40 segments/file × total files).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vector_estimate_high: Option<usize>,
}

fn json_object_schema(_: &mut SchemaGenerator) -> Schema {
    json_schema!({
        "type": "object"
    })
}
