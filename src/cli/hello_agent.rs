use clap::Args;
use colored::Colorize;
use serde::Serialize;

use crate::shared::types::OutputFormat;

#[derive(Args)]
pub struct HelloAgentArgs {}

const AGENT_INSTRUCTION: &str = r#"# 1up — Agent Quick Reference

You have access to `1up`, a local code search and indexing CLI for the current repository.

## Commands

### Semantic + Full-Text Search
```
1up search "<query>" [--limit N] [--path <dir>]
```
Hybrid search combining vector similarity and keyword matching. Best for natural-language queries like "how does authentication work" or "error handling in the API layer".

### Symbol Lookup
```
1up symbol <name> [--references]
```
Find definitions (and optionally references) of functions, types, and variables by name. Supports fuzzy matching.

### Code Context
```
1up context <file>:<line>
```
Retrieve the enclosing scope (function, class, block) around a specific file location. Useful for understanding surrounding code after finding a search hit.

### Structural Search
```
1up structural "<tree-sitter-query>" [--language <lang>]
```
AST-pattern search using tree-sitter S-expression queries. Use for precise structural matches like "all functions returning Result" or "all impl blocks for a type".

## Global Flags

- `--format plain|json|human` — Output format (default: plain). Use `plain` for easy parsing, `json` for structured data.
- `-v` / `-vv` — Increase verbosity.

## Recommended Workflow

1. Start with `1up search` for broad exploration.
2. Narrow to `1up symbol` when you know the name.
3. Use `1up context` to read surrounding code.
4. Use `1up structural` for precise AST patterns.

## Tips

- The index updates automatically via a background daemon. Run `1up start` if not yet initialized.
- Plain output is tab-delimited and machine-friendly.
- Search results include file path, line range, block type, and relevance score.
"#;

#[derive(Debug, Serialize)]
struct HelloAgentOutput {
    instruction: String,
}

pub async fn exec(_args: HelloAgentArgs, format: OutputFormat) -> anyhow::Result<()> {
    match format {
        OutputFormat::Plain => {
            print!("{}", AGENT_INSTRUCTION);
        }
        OutputFormat::Json => {
            let output = HelloAgentOutput {
                instruction: AGENT_INSTRUCTION.to_string(),
            };
            println!(
                "{}",
                serde_json::to_string_pretty(&output)
                    .unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"))
            );
        }
        OutputFormat::Human => {
            println!("{}\n", "1up Agent Instructions".bold().underline());
            print!("{}", AGENT_INSTRUCTION);
        }
    }
    Ok(())
}
