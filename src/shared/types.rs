use serde::{Deserialize, Serialize};

/// Role classification for a parsed code segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SegmentRole {
    Definition,
    Implementation,
    Orchestration,
    Import,
    Docs,
}

/// A parsed segment extracted from source code by tree-sitter or the text chunker.
#[derive(Debug, Clone)]
pub struct ParsedSegment {
    pub content: String,
    pub block_type: String,
    pub line_start: usize,
    pub line_end: usize,
    pub language: String,
    pub breadcrumb: Option<String>,
    pub complexity: u32,
    pub role: SegmentRole,
    pub defined_symbols: Vec<String>,
    pub referenced_symbols: Vec<String>,
}

/// A search result returned by hybrid or FTS-only search.
#[derive(Debug, Clone, Serialize)]
pub struct SearchResult {
    pub file_path: String,
    pub language: String,
    pub block_type: String,
    pub content: String,
    pub score: f64,
    pub line_number: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<SegmentRole>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub defined_symbols: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub referenced_symbols: Option<Vec<String>>,
}

/// Distinguishes between a symbol definition and a usage reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReferenceKind {
    Definition,
    Usage,
}

impl std::fmt::Display for ReferenceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReferenceKind::Definition => write!(f, "definition"),
            ReferenceKind::Usage => write!(f, "usage"),
        }
    }
}

/// A symbol lookup result.
#[derive(Debug, Clone, Serialize)]
pub struct SymbolResult {
    pub name: String,
    pub kind: String,
    pub file_path: String,
    pub line_start: usize,
    pub line_end: usize,
    pub content: String,
    pub reference_kind: ReferenceKind,
}

/// A structural search result from AST pattern matching.
#[derive(Debug, Clone, Serialize)]
pub struct StructuralResult {
    pub file_path: String,
    pub language: String,
    pub pattern_name: Option<String>,
    pub content: String,
    pub line_start: usize,
    pub line_end: usize,
}

/// A context retrieval result with the enclosing scope.
#[derive(Debug, Clone, Serialize)]
pub struct ContextResult {
    pub file_path: String,
    pub language: String,
    pub content: String,
    pub line_start: usize,
    pub line_end: usize,
    pub scope_type: String,
}

/// Output format for CLI results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputFormat {
    #[default]
    Json,
    Human,
    Plain,
}

impl std::fmt::Display for OutputFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OutputFormat::Json => write!(f, "json"),
            OutputFormat::Human => write!(f, "human"),
            OutputFormat::Plain => write!(f, "plain"),
        }
    }
}

impl std::str::FromStr for OutputFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "json" => Ok(OutputFormat::Json),
            "human" => Ok(OutputFormat::Human),
            "plain" => Ok(OutputFormat::Plain),
            other => Err(format!("unknown output format: {other}")),
        }
    }
}
