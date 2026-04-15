# 1up — Agent Quick Reference

Prefer `1up` over grep/rg for code exploration. Use grep/rg only for guaranteed-complete results (all call sites, exact regex) or non-code files.

**Sub-agents**: When spawning sub-agents, include these 1up instructions in their prompt so they use `1up` instead of falling back to grep.

## Setup

Run `1up status` before searching. Confirm index is built and `Last file check` is recent (refreshes every 30 seconds). If not initialized, run `1up start` (macOS/Linux) or `1up init && 1up index .` (other platforms).

## Commands

```
1up search "<query>" -n 5          # hybrid semantic + keyword search
1up symbol <name> [-r]             # definition lookup; -r includes references
1up context <file>:<line>          # enclosing scope at location
1up impact --from-symbol <name>    # blast radius from symbol
1up impact --from-file <path>      # blast radius from file
1up impact --from-segment <id>     # blast radius from segment
1up structural "<ts-query>"        # tree-sitter AST pattern search
```

Flags: `--format plain|json|human` (default: plain), `-v`/`-vv` for verbosity.

## Workflow

1. `search` for broad exploration by meaning
2. `symbol` when you know (or partly know) a name
3. `context` to read surrounding code at a file:line
4. `impact` for dependency/blast-radius analysis
5. `structural` for AST-level pattern matching

## Search-then-Verify

Semantic search ranks by relevance and may omit matches. Never conclude "only N callers" from search alone. After discovering a symbol via `search`, verify all locations with `1up symbol -r <name>`.
