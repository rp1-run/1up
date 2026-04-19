<!-- rp1:start:v0.7.1 -->
## rp1 Knowledge Base

**Use Progressive Disclosure Pattern**

Location: `.rp1/context/`

Files:
- index.md (always load first)
- architecture.md
- modules.md
- patterns.md
- concept_map.md

Loading rules:
1. Always read index.md first.
2. Then load based on task type:
   - Code review: patterns.md
   - Bug investigation: architecture.md, modules.md
   - Feature work: modules.md, patterns.md
   - Strategic or system-wide analysis: all files

## rp1 Skill Awareness

You have access to rp1 skills. When you notice the user working on a task
that an rp1 skill addresses, briefly suggest it.

### Skill Categories
| Category | Skills | Suggest When |
|----------|--------|--------------|
| Development | /build, /build-fast, /speedrun | User starts new feature or describes a change |
| Investigation | /code-investigate | User is debugging or examining errors |
| Quality | /code-check, /code-audit, /code-clean-comments | User finishes implementation |
| Review | /pr-review, /pr-visual, /address-pr-feedback | User prepares or responds to PR |
| Documentation | /write-content, /generate-user-docs | User writes or updates docs |
| Knowledge | /knowledge-build | User needs codebase context or KB is stale |
| Strategy | /strategize, /deep-research, /analyse-security | User faces architectural or security decisions |
| Planning | /blueprint, /blueprint-audit | User plans a project or audits progress |
| Prompt | /prompt-writer, /tersify-prompt | User authors or rewrites prompts |

### Suggestion Rules
- Limit to 1 suggestion per turn. Format: skill name, one sentence why, offer to run.
- Do not re-suggest a skill the user declined this session.
- Do not suggest while an rp1 workflow is already running.
- Only suggest when there is a clear match to the user's current activity.
- For deeper questions about rp1, suggest the user invoke /guide.
<!-- rp1:end:v0.7.1 -->

<!-- 1up:start:0.1.9 -->
# 1up — Agent Quick Reference

## Tool Selection

Pick by what you know, not by habit:

| You know... | Use | Reason |
|---|---|---|
| Concept, not files/keywords | `1up search` | Semantic search needs no vocabulary |
| Symbol/keyword name | `1up symbol -r` or `grep` | Literal match = precise, low noise |
| Keyword, need ALL instances | `grep` | Guaranteed-complete, best signal:noise |
| Nothing (new codebase) | `1up search` | Works w/o prior knowledge |

**Sub-agents**: Include this section in sub-agent prompts.

## Setup

`1up status` before searching. Confirm index built + `Last file check` recent (~30s refresh). If not initialized: `1up start` (macOS/Linux) or `1up init && 1up index .`.

## Core Commands (lean, one shape)

```
1up search "<query>" [-n 3]            # semantic + keyword hybrid; default 3 hits
1up get <id|prefix> [<id>...]          # hydrate full segment body by handle
1up symbol <name> [-r]                 # def lookup; -r = all references
1up context <file>:<line>              # enclosing scope at location
1up impact --from-symbol <name>        # blast radius from symbol
1up impact --from-file <path>          # blast radius from file
1up impact --from-segment <id>         # blast radius from segment
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
- `<kind>`: segment block type (e.g. `FUNCTION`, `STRUCT`, `IMPL`)
- `<breadcrumb>::<symbol>`: enclosing scope path, `::` separated
- `:<segment_id>`: 12-char hex handle; feed directly into `1up get` or `1up impact --from-segment` — both accept the prefix and disambiguate if it matches more than one segment
- `~<channel>`: impact-only trailing tag, `~P` (primary) or `~C` (contextual)
- Fields are separated by **two ASCII spaces**

`context` and `structural` omit `<score>` and `:<segment_id>` because they are read-after-pick, not discovery.

## Maintenance Commands (keep `--format`)

`start`, `stop`, `status`, `init`, `index`, `reindex`, `update`, `hello-agent` still accept `--format plain|json|human` (default: `human` for lifecycle, `plain` for `index`/`reindex`).

## Workflow

1. `search` — explore by meaning/intent, capture `:<segment_id>`
2. `get` — hydrate the full body of 1-2 promising ids before reading files
3. `symbol` — known or partial name lookup
4. `context` — enclosing scope at a file:line pick
5. `impact` — blast-radius analysis from an anchor
6. `structural` — AST-level pattern matching

## Search-then-Verify

Semantic search ranks by relevance; may omit matches. Never conclude "only N callers" from search alone. Verify completeness w/ `1up symbol -r <name>` or `grep`.
<!-- 1up:end:0.1.9 -->
