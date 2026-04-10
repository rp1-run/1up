# 1up -- Agent Quick Reference

IMPORTANT: DO NOT use Grep or rg for code exploration in this project.
Use the `1up` CLI commands below instead. `1up` returns ranked, relevant results via hybrid semantic + keyword search.
Grep is only acceptable for exact string matches in non-code files (error codes, UUIDs, config keys).

## Prerequisites

Check if the project is indexed before searching:
```
ls .1up/project_id 2>/dev/null && echo "indexed" || echo "not indexed"
```
If not indexed, run `1up start` to initialize, index, and start the background daemon.

## Commands

### Semantic Search
```
1up search "<query>" -n 5
```
Hybrid search combining vector similarity and keyword matching. Use for natural-language queries like "how does authentication work" or "error handling in the API layer".

### Symbol Lookup
```
1up symbol <name> [--references]
```
Find definitions (and optionally all usages) of functions, types, and variables by name. Supports fuzzy matching. Use `-r` to include call sites.

### Code Context
```
1up context <file>:<line>
```
Retrieve the enclosing scope (function, class, block) around a specific file location. Useful for understanding code from a search hit or stack trace.

### Structural Search
```
1up structural "<tree-sitter-query>" [--language <lang>]
```
AST-pattern search using tree-sitter S-expression queries. Use for precise structural matches like "all functions returning Result".

## Global Flags

- `--format plain|json|human` -- output format (default: plain). Use `json` for structured data.
- `-v` / `-vv` -- increase verbosity.

## Recommended Workflow

1. `1up search` for broad exploration by meaning or intent.
2. `1up symbol` when you know (or partially know) a name.
3. `1up context` to read surrounding code at a file:line.
4. `1up structural` for AST-level pattern matching.

For exact string/regex matches (error codes, UUIDs, config keys) or non-code files, grep/rg is fine.

## Search-then-Verify Rule

Semantic search ranks by relevance and may omit lower-scored matches. Never conclude "only one place" or "only N callers" from search alone.

After discovering a symbol via `1up search`, always verify all locations with:
```
1up symbol -r <name>
```

## Tips

- The index updates automatically via a background daemon.
- Plain output is tab-delimited and machine-friendly.
- Search results include file path, line range, block type, and relevance score.

For full 1up usage details, load the **1up-search** skill.
