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
license: MIT
compatibility: Requires 1up CLI on PATH. Works with any agent that can execute shell commands.
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
1up start    # initializes, indexes, and starts the daemon
```

After this, the index stays current automatically via the background daemon.

## Commands

### Semantic Search — `1up search`

Find code by meaning. Combines vector similarity, full-text matching, and symbol analysis with ranked fusion.

```sh
1up search "error handling"              # natural language query
1up search "database connection pool"    # conceptual search
1up search "retry logic with backoff"    # finds implementations even without exact wording
1up search "parse configuration" -n 5    # limit results
```

Use `-f json` for structured output when parsing results programmatically:

```sh
1up search "auth middleware" -f json
```

**When to use**: Exploring unfamiliar code, investigating how something works, finding implementations by concept rather than by name.

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

## Common Agent Workflows

**Bug investigation** — start broad, narrow down:

```sh
1up search "connection timeout handling"    # find relevant code
1up symbol -r ConnectionPool                # trace usage
1up context src/pool.rs:87                  # understand the specific code
```

**Understanding a module** — combine search and symbols:

```sh
1up search "what does the indexer do"       # conceptual overview
1up symbol Pipeline                         # find the core type
1up symbol -r run_with_config               # trace how it's called
```

**Finding where to make a change**:

```sh
1up search "user validation logic"          # find the area
1up symbol validate_user                    # find the exact function
1up context src/auth/validate.rs:55         # see full implementation
```
