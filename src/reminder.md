# 1up — Agent Quick Reference

`1up` is a lean, agent-first code-search CLI. Prefer it over `find`, `grep`, `rg`, or `ls` for discovery. Only reach for those tools once 1up has narrowed the scope to one or two files.

## Tool Selection

| You know… | Use |
|---|---|
| Concept, not files/keywords | `1up search "<query>" -n 3` |
| Exact symbol name | `1up symbol <name> [-r]` (add `-r` for references) |
| A `:<segment_id>` handle from a prior row | `1up get <handle>` (body) or `1up impact --from-segment <handle>` (blast radius) |
| A specific file:line | `1up context <file>:<line>` |
| Blast radius from a symbol or file | `1up impact --from-symbol <name>` / `--from-file <path>[:<line>]` |
| Nothing (new codebase) | start with `1up search` |

`grep` remains the right tool only when you need **all** literal occurrences across the repo for completeness, after `1up` has identified the candidate set.

**Sub-agents**: Include this section in sub-agent prompts.

## Setup

`1up status` before searching. Confirm index built + `Last file check` recent (~30s refresh). If not initialized: `1up start` (macOS/Linux) or `1up init && 1up index .`.

## Core Commands (lean, one shape)

```
1up search "<query>" [-n 3]            # semantic + keyword hybrid; default 3 hits
1up get <id|prefix> [<id>...]          # hydrate full segment body by handle
1up symbol <name> [-r] [--fuzzy]       # def lookup; -r adds references; --fuzzy for approximate
1up context <file>:<line>              # enclosing scope at location
1up impact --from-symbol <name>        # blast radius from symbol
1up impact --from-file <path>          # blast radius from file
1up impact --from-segment <id>         # blast radius from segment handle
1up structural "<ts-query>"            # tree-sitter AST pattern search
```

Core commands emit **one machine-parseable shape**. They do not accept `--format`, `-f`, `--full`, `--brief`, `--human`, or `--verbose-fields`.

## Lean Row Grammar

Every discovery row matches:

```
<score>  <path>:<l1>-<l2>  <kind>  <breadcrumb>::<symbol>  :<segment_id>[  ~<channel>]
```

- `<score>`: integer 0-100, monotonic with ranking
- `<path>:<l1>-<l2>`: file path plus 1-based inclusive line span
- `<kind>`: segment block type (e.g. `FUNCTION`, `STRUCT`, `IMPL`); `symbol` rows prefix with `def:` or `usage:`
- `<breadcrumb>::<symbol>`: enclosing scope path, `::` separated
- `:<segment_id>`: 12-char hex handle; feed directly into `1up get` or `1up impact --from-segment` — both accept the prefix and disambiguate if it matches more than one segment
- `~<channel>`: impact-only trailing tag, `~P` (primary) or `~C` (contextual)
- Fields are separated by **two ASCII spaces**

`context` and `structural` omit `<score>` and `:<segment_id>` because they are read-after-pick, not discovery.

## Example handoff

```
1up search "authentication middleware" -n 3
# => 92  src/auth/middleware.ts:10-48  function  Auth::verify  :a4f2c1d9e0b3
1up get :a4f2c1d9e0b3                     # hydrate the full body
1up impact --from-segment a4f2c1d9e0b3    # blast radius from the same anchor
```

## Maintenance Commands (keep `--format`)

`start`, `stop`, `status`, `init`, `index`, `reindex`, `update`, `hello-agent` still accept `--format plain|json|human` (default: `human` for lifecycle, `plain` for `index`/`reindex`).

## Workflow

1. `search` — explore by meaning/intent, capture a `:<segment_id>` handle.
2. `get` — hydrate 1-2 promising handles before opening files.
3. `symbol` — known or partial name lookup (add `-r` for references).
4. `context` — enclosing scope at a file:line pick.
5. `impact` — blast-radius analysis; prefer `--from-segment <handle>` when you already have a hit.
6. `structural` — AST-level pattern matching.

## Search-then-Verify

Semantic search ranks by relevance; may omit matches. Never conclude "only N callers" from search alone. Verify completeness with `1up symbol -r <name>` or `grep` **after** 1up has pointed at the files.
