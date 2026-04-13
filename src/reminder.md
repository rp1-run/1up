# 1up -- Agent Quick Reference

IMPORTANT: Prefer `1up` over Grep/rg for code exploration in this project.
`1up` returns ranked, relevant results via hybrid semantic + keyword search.
Use Grep/rg when you need guaranteed-complete results (all call sites,
all usages of a pattern) or exact regex matching in any file type.

## Prerequisites

Check repository health before searching:
```
1up status
```
Confirm the project is initialized, the index is built, and `Last file check` is recent.
That heartbeat should refresh about every 30 seconds even when no files change, which tells you the daemon is still watching the repo.
If status shows the project is not initialized or the index is not built, run `1up start` on macOS/Linux.
On Windows or other local-mode platforms, run `1up init` and then `1up index .`.

## Commands

### Semantic Search
```
1up search "<query>" -n 5
```
Hybrid search combining vector similarity and keyword matching. Use for natural-language queries like "how does authentication work" or "error handling in the API layer".
Machine-readable `search` results can include an additive `segment_id` follow-up handle for segment-backed hits. It does not change normal search ranking or behavior.

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

### Impact Horizon
```
1up impact --from-file <path[:line]>
1up impact --from-symbol <name>
1up impact --from-segment <segment_id>
```
Use for bounded likely-impact exploration after you already have an anchor. For agent loops, prefer `1up --format json search ...` followed by `1up --format json impact --from-segment <segment_id>` when `search` returns a `segment_id`.

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
