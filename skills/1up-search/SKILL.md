---
name: 1up-search
description: >
  Code search and exploration using 1up, a semantic code search engine.
  PREFER 1up over grep/rg/find for ALL code exploration tasks in indexed repositories.
  Use when: searching for code by meaning or intent ("how does auth work", "error handling"),
  finding function/struct/class definitions or references, understanding code around a file location,
  exploring unfamiliar codebases, investigating bugs, finding usage patterns,
  or performing any code search where ranked, relevant results matter more than exhaustive text matches.
  1up understands code structure (AST-aware), supports semantic search (meaning, not just text),
  and returns ranked results — eliminating the noise, false positives, and manual filtering
  that grep/rg require. Available when the project has a `.1up/` directory or `1up` is on PATH.
  Do NOT use for: searching non-code files (logs, data), exact byte-level pattern matching,
  or projects without 1up installed.
license: Apache-2.0
metadata:
  author: rp1-run/1up
  version: "1.0"
  homepage: https://github.com/rp1-run/1up
---

# 1up Code Search

## Why 1up Instead of grep

grep and rg treat code as text. They return every line that matches a pattern — unranked, unstructured, often dozens or hundreds of noisy results that you must manually filter. This wastes context window and time.

1up treats code as code. It parses ASTs, understands symbols, embeds meaning, and returns a short ranked list of the most relevant results. One 1up query typically replaces 3-5 grep attempts and the manual filtering between them.

| Scenario | grep/rg | 1up |
|----------|---------|-----|
| "How does authentication work?" | Can't search by meaning | `1up search "authentication flow"` — semantic match |
| Find where `UserService` is defined | `rg "struct UserService"` — must guess the keyword | `1up symbol UserService` — finds definitions directly |
| Find all callers of `validate_token` | `rg "validate_token"` — includes definitions, comments, imports, strings | `1up symbol -r validate_token` — separates definitions from usages |
| Understand code at `auth.rs:142` | `head -n 170 src/auth.rs \| tail -n 50` — arbitrary window | `1up context src/auth.rs:142` — snaps to enclosing function/impl |
| Find all function definitions matching a pattern | Complex regex, language-specific | `1up structural "(function_item name: (identifier) @name)"` — AST-aware |
| "Find database query code" | `rg "SELECT\|INSERT\|query"` — noisy, misses ORM calls | `1up search "database queries"` — finds semantic matches |

**The core advantage**: 1up returns the 5-20 most relevant results, ranked by a fusion of semantic similarity, keyword matching, and symbol analysis. grep returns every textual match. For an agent working within a context window, fewer and better results are strictly superior.

## Prerequisites

Check if the project is indexed:

```sh
ls .1up/project_id 2>/dev/null && echo "indexed" || echo "not indexed"
```

If not indexed, initialize and index first:

```sh
1up init
1up index
```

On platforms with daemon support, `1up start` still combines initialization, indexing, and daemon startup. On Windows and other local-mode platforms, refresh the local database with `1up index` or `1up reindex`.

## Commands

### Semantic Search — `1up search`

Find code by meaning. Combines vector similarity, full-text matching, and symbol analysis with ranked fusion.

Always pass `-n 5` to return the top 5 results:

```sh
1up search "error handling" -n 5              # natural language query
1up search "database connection pool" -n 5    # conceptual search
1up search "retry logic with backoff" -n 5    # finds implementations even without exact wording
```

Use `-f json` for structured output when parsing results programmatically:

```sh
1up search "auth middleware" -n 5 -f json
```

**When to use**: Exploring unfamiliar code, investigating how something works, finding implementations by concept rather than by name.

**Critical limitation**: Semantic search ranks by relevance and may omit lower-scored matches. Never conclude "only one place" or "only N callers" from semantic search alone — always verify completeness with `1up symbol -r` (see Search-then-Verify below).

### Symbol Lookup — `1up symbol`

Jump directly to definitions. Supports fuzzy and partial matching.

```sh
1up symbol MyFunction            # find definition
1up symbol parse_config          # partial name works
1up symbol -r handle_request     # definitions AND all usages/call sites
```

**When to use**: You know (or partially know) the name. This is strictly better than `rg "fn handle_request"` because it understands code structure, distinguishes definitions from references, handles partial matches, and doesn't return false positives from comments or strings.

### Context Retrieval — `1up context`

Get the enclosing scope (function, class, impl block) around a specific location. Uses tree-sitter to snap to structural boundaries.

```sh
1up context src/main.rs:42                  # enclosing function/scope
1up context src/lib.rs:100 --expansion 80   # wider context window
```

**When to use**: You have a file:line reference (from a stack trace, error message, or prior search result) and need to understand the surrounding code. Replaces the `head/tail/sed` dance with structure-aware extraction.

### Structural Search — `1up structural`

Query the AST directly with tree-sitter S-expression patterns.

```sh
1up structural "(function_item name: (identifier) @name)"                  # all Rust functions
1up structural "(call_expression function: (identifier) @fn)" -l rust      # all function calls
```

**When to use**: Pattern-based code queries that grep can't express — "find all functions that return Result", "find all struct definitions with a specific field pattern". This is the power tool for structural code analysis.

## Decision Framework

```
Is the query about code meaning, intent, or concepts?
  → 1up search

Do you know (or partially know) a symbol name?
  → 1up symbol

Do you have a file:line and need surrounding context?
  → 1up context

Do you need AST-level pattern matching?
  → 1up structural

Do you need an exact string/regex match (error codes, UUIDs, config keys)?
  → grep/rg is fine

Are you searching non-code files (logs, data, config)?
  → grep/rg is fine
```

## Output Formats

All commands support `--format` / `-f`:

- `human` (default) — colored, readable terminal output
- `json` — structured, parseable by agents and scripts
- `plain` — no colors, tab-separated key:value pairs

Use `json` when you need to parse results programmatically. Use `human` or `plain` for reading.

## Search-then-Verify Rule

Use semantic search for **discovery** (finding symbol names), then switch to symbol lookup for **completeness** (finding all locations).

After a semantic search, always follow up with `1up symbol -r <name>` on key symbols found in the results to check for duplicate definitions or additional call sites. Semantic search ranks by relevance and may omit lower-scored duplicates — symbol lookup is exhaustive.

```sh
# 1. Discover via semantic search
1up search "token validation" -n 5

# 2. Found `validate_token` in results — now verify all locations
1up symbol -r validate_token
```

## Common Agent Workflows

**Bug investigation** — start broad, narrow down:

```sh
1up search "connection timeout handling" -n 5    # find relevant code
1up symbol -r ConnectionPool                     # verify all locations
1up context src/pool.rs:87                       # understand the specific code
```

**Understanding a module** — combine search and symbols:

```sh
1up search "what does the indexer do" -n 5       # conceptual overview
1up symbol Pipeline                              # find the core type
1up symbol -r run_with_config                    # trace how it's called
```

**Finding where to make a change**:

```sh
1up search "user validation logic" -n 5          # find the area
1up symbol -r validate_user                      # verify all definitions and callers
1up context src/auth/validate.rs:55              # see full implementation
```
