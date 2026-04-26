use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PrepareMode {
    #[default]
    Check,
    IndexIfMissing,
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
    pub locations: Vec<ReadLocationInput>,
    pub path: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadLocationInput {
    pub path: String,
    pub line: usize,
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
    pub data: Value,
    pub next_actions: Vec<NextAction>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct NextAction {
    pub tool: String,
    pub reason: String,
    pub arguments: Value,
}
