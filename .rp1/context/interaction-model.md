# 1up - Interaction Model

## Experience Principles

| Principle | Meaning | Evidence |
|---|---|---|
| MCP-first agent discovery | The supported agent surface is the `oneup` MCP server with canonical `oneup_*` tools. Agents should use MCP before broad raw search and should not shell out to `1up ...` for scored discovery workflows. | `README.md`, `docs/mcp-installation.md`, `src/mcp/server.rs`, `evals/README.md` |
| Search-to-read-to-verify | Discovery starts with ranked search, then selected handles or locations are hydrated before conclusions. Symbol lookup is the completeness path for known symbols. | `src/mcp/tools.rs`, `evals/suites/1up-search/prompt-1up.txt`, `src/cli/get.rs`, `src/cli/symbol.rs` |
| Explicit readiness before trust | Agents and users get explicit index readiness states before relying on search. Missing, stale, indexing, and degraded states include recovery actions. | `src/mcp/ops.rs`, `src/mcp/tools.rs`, `src/cli/status.rs`, `docs/mcp-installation.md` |
| Lean core CLI contract | Core discovery commands have one machine-parseable lean grammar and reject `--format`; maintenance commands retain human/plain/json renderers. | `src/cli/mod.rs`, `src/cli/lean.rs`, `src/cli/output.rs` |
| Advisory impact boundary | Impact exploration is likely-impact guidance, not exact dependency truth. Primary results are higher confidence; contextual results are lower confidence and must be verified. | `src/search/impact.rs`, `src/cli/lean.rs`, `src/mcp/tools.rs`, `evals/suites/1up-impact/prompt-1up.txt` |
| Refuse unsafe ambiguity | Impact requires exactly one anchor and refuses broad, ambiguous, missing, or out-of-scope anchors with narrowing hints. | `src/cli/impact.rs`, `src/search/impact.rs`, `src/mcp/tools.rs` |
| Local-only, user-owned setup | MCP reads and indexes only the configured local repository. It does not edit files, run tests, execute arbitrary shell commands, or mutate host config after setup. | `README.md`, `docs/mcp-installation.md`, `src/mcp/ops.rs` |
| Progress stays off protocol stdout | TTY progress uses stderr spinners/bars; non-TTY or structured modes stream parseable progress. MCP stdio and JSON maintenance output avoid passive notices on stdout. | `src/shared/progress.rs`, `src/cli/output.rs`, `src/main.rs`, `docs/mcp-installation.md` |

## Actors

| Actor | Goals | Primary Surfaces |
|---|---|---|
| AI agent / MCP host | Discover code, hydrate evidence, verify symbols, estimate likely impact without dumping broad raw search into context. | MCP server, repo instruction files, eval prompts |
| Human installer | Install/update 1up, configure the `oneup` server in a host, review scope and approve host trust prompts. | README setup prompt, `1up add-mcp`, manual MCP snippets, installation guide |
| Developer CLI user | Search, read, inspect context, explore impact, and manage local indexes from a terminal. | Lean CLI, maintenance CLI, status/progress output |
| Script / automation | Parse stable lean rows, maintenance JSON/plain, update status, and eval metadata. | CLI stdout contracts, MCP structured envelopes, eval assertions |
| Background daemon | Keep registered projects warm, refresh indexes, serve warm search where supported. | `1up start`, `1up status`, MCP auto-start, daemon registry/status files |
| Maintainer / release operator | Validate install readiness, adoption behavior, recall, benchmarks, and release evidence. | evals, release docs, smoke scripts, benchmark/recall harnesses |

## Surfaces

### MCP Server

- Entry point: `1up mcp --path <repo-or-worktree>` under server identity `oneup`.
- Tools: `oneup_status`, `oneup_start`, `oneup_search`, `oneup_get`, `oneup_symbol`, `oneup_context`, `oneup_impact`, `oneup_structural`.
- Role: primary agent-facing status/start, search, get, symbol, context, impact, and structural workflow.
- Output: presentation-free structured `ToolEnvelope` with `status`, `summary`, `data`, and `next_actions`; text content mirrors the summary for host display without ANSI color, spinners, or terminal table formatting.
- Readiness/start modes: `oneup_status` checks without indexing; `oneup_start` accepts `index_if_needed`, `index_if_missing`, and `reindex`.
- Product terms map directly to retained tools: readiness/status uses `oneup_status`, allowed start/index behavior uses `oneup_start`, handle hydration uses `oneup_get`, and file-line context retrieval uses `oneup_context`.
- Safety: read-only tool annotations, single configured repository root, no arbitrary command execution.

### MCP Setup And Onboarding

- Entry points: README ready-to-run prompt, `docs/mcp-installation.md`, `1up add-mcp`, manual JSON/TOML snippets.
- The agent-pasted setup prompt intentionally edits host config directly and tells the active host not to restart or verify its own newly-added tools.
- Human quick setup may use `1up add-mcp`, which validates the repository path, chooses `bunx` before `npx`, delegates mutation to external `add-mcp`, and prints manual fallback guidance on failure.
- Users must reload/restart the host and approve/trust the `oneup` server when required.
- Historical reminder fences, `hello-agent`, portable skills, and digit-leading `1up_*` aliases are no longer current onboarding surfaces.

### Discovery CLI

- Help-visible retained discovery entry points: `get`, `symbol`, `context`, and `impact`; hidden lean compatibility commands include `search` and `structural`.
- `search`: hidden ranked semantic plus FTS discovery, default `-n 3`, auto-starts daemon when registered, falls back to local search if daemon search times out.
- `get`: hydrates one or more full or 12-char segment handles, accepting optional leading `:` and preserving request order.
- `symbol`: exact definitions by default, `--references` for usages, `--fuzzy` for approximate matching; empty exact results teach `--fuzzy`.
- `context`: reads a file:line scope, rejects absolute/outside-root paths unless `--allow-outside-root` is explicit.
- `impact`: expands likely impact from exactly one public file, symbol, or handle anchor with optional `--scope`, `--depth`, and `-n`; segment input is hidden compatibility.
- `structural`: hidden tree-sitter query search; MCP `oneup_structural` is the retained agent-facing structural surface.
- Retained commands default to human-readable discovery output and use `--plain` for stable lean output where supported; core discovery commands reject `--format`.

### Maintenance CLI

- Entry points: `init`, `start`, `stop`, `status`, `list`, `index`, `reindex`, `update`.
- Defaults: `start`, `stop`, `status`, and `update` default to human; `init`, `index`, and `reindex` default to plain; all accept `--format/-f`.
- `start` registers the resolved `WorktreeContext`, initializes when needed, indexes if no current context index exists, and reports stale/newer/unreadable schema recovery separately.
- `status` and `list` show source root, context id, main worktree, worktree role, branch, branch status, watch status, index status, and last update state where available.
- `index` and `reindex` expose `--watch`, `--jobs`, and `--embed-threads`; progress includes phase, work counts, embeddings, context id, source root, branch metadata, parallelism, timings, scope, and prefilter details.
- `update` has passive post-command notifications for normal commands, but suppresses them for MCP, worker, update itself, and JSON maintenance output.

### Agent Instruction And Eval Surfaces

- `AGENTS.md` and `CLAUDE.md` tell agents to use `oneup` MCP before broad raw search for code-discovery questions.
- Adoption evals start an MCP server with `1up mcp --path .` and grade provider tool-call metadata.
- Evals require canonical MCP use, get/context after search, symbol verification when completeness matters, impact use for blast-radius tasks, and explicit primary/contextual interpretation.
- Broad raw `grep`, `rg`, `find`, and `Glob` discovery fail the 1up variant; exact literal verification is allowed only after MCP narrows to precise files.

## User-Visible States

| State | Meaning | Signals / Recovery |
|---|---|---|
| `ready` | The active worktree context is indexed and semantic search is available. | `oneup_status` suggests `oneup_search`; counts are context-scoped. |
| `missing` | Project, index, or active context rows are absent, empty, or not usable. | `oneup_status` suggests explicit `oneup_start` indexing mode. |
| `indexing` | Index progress file reports a running job for the active context. | Poll `oneup_status` or `1up status` from the same worktree. |
| `stale` | Index exists but is unreadable, stale, or schema-incompatible. | Rebuild with `oneup_start` mode `reindex` or `1up reindex`. |
| `degraded` | Search can run, but embeddings are unavailable, latest index lacks embeddings, or branch context is unknown/unreadable. | Results may be FTS-only or only context-scoped rather than definitively branch-filtered; fix model/index/git state then recheck. |
| `blocked` | MCP status/start could not make the repository ready. | Structured data explains the blocking condition and suggests corrective start or setup actions. |
| `ok` / `empty` / `partial` / `degraded` | MCP operation status for search/get/context/symbol/structural. | `summary`, `data`, and `next_actions` explain next step. |
| `found` / `not_found` / `ambiguous` / `rejected` / `error` | Per-record `oneup_get` or `oneup_context` outcomes. | Ambiguous handles list matching IDs; rejected locations identify path-scope violations. |
| `expanded` / `expanded_scoped` | Impact returned primary likely-impact rows. | CLI rows end `~P`; MCP data contains primary `results`. |
| `empty` / `empty_scoped` | Impact anchor resolved but no primary likely impact survived. | May include contextual guidance and a hint. |
| `refused` | Impact expansion was unsafe or ambiguous. | Reason and hint suggest scope, segment, search, or reindex. |
| `started` / `already_running` / `startup_in_progress` / `indexed_and_started` | Start lifecycle result. | Human/plain/json start result includes message, pid, and optional index progress. |
| `idle` / `running` / `complete` with phases | Index lifecycle state. | Phases include pending, preparing, rebuilding, loading_model, scanning, parsing, storing/embedding_and_storing, complete. |
| `watching` / `daemon_stopped` / `source_missing` / `unsupported` / `unknown` | Daemon watch state for a registered context. | `status` and `list` include watch state plus branch/worktree and last refresh metadata. |
| `up_to_date` / `update_available` / `yanked` / `below_minimum_safe` | Update status. | Human/plain/json update renderers include install channel and upgrade instruction when needed. |

## Feedback Loops

### MCP Readiness Loop

1. Agent calls `oneup_status` when readiness is unknown for the configured repo or worktree path.
2. The tool resolves `WorktreeContext` and reports `ready`, `missing`, `indexing`, `stale`, `degraded`, or `blocked` for that `context_id`.
3. Structured data includes state root, source root, context-scoped counts, matching progress, and matching daemon heartbeat when available.
4. `next_actions` steer to search, explicit indexing, polling, reindexing, or setup correction.
5. If indexing or rebuilding is needed, the agent calls `oneup_start` with the suggested mode and then checks `oneup_status` again.

### MCP Discovery Loop

1. Agent calls `oneup_search` with a task-specific intent query.
2. Search returns compact handles and ranked summaries.
3. Agent calls `oneup_get` on selected handles to hydrate source evidence.
4. Agent calls `oneup_context` for bounded file-line context around a relevant path and line.
5. Agent calls `oneup_symbol` for definition/reference completeness when a symbol matters.
6. Agent answers from inspected code, not from ranked search alone.

### MCP Impact Loop

1. Agent calls `oneup_impact` directly for a clear symbol/file anchor or after search with a segment handle.
2. Primary results are treated as likely follow-up targets; contextual results are lower-confidence guidance.
3. Agent reads important results and verifies direct references before acting.
4. If refused, the agent narrows with a suggested scope, file, symbol, or segment.

### Lean CLI Handle Loop

1. `search`, `symbol`, or `impact` emits lean rows with `:<12-char-handle>`.
2. User or script passes handles directly to `1up get`, with or without the leading colon.
3. `get` emits full segment records or `not_found` records with `---` sentinels.

### Index Progress Loop

1. User runs `index`, `reindex`, or `start`.
2. TTY human mode shows stderr progress; watch/plain/json/non-TTY modes stream parseable progress updates.
3. Final summaries report counts, timings, embeddings, scope, prefilter, and updated timestamp.

### Setup Review Loop

1. User chooses setup prompt, `add-mcp`, or manual config.
2. User verifies server identity `oneup`, command `1up`, args `mcp --path <repo-or-worktree>`, repository/worktree path, and scope.
3. User reloads/trusts the host, lists tools, and calls `oneup_status`.

## Output Semantics

| Surface | Contract |
|---|---|
| MCP tools | Presentation-free structured envelope: `status`, `summary`, `data`, `next_actions`; errors are structured errors with recovery actions. |
| `search` | `<score>  <path>:<l1>-<l2>  <kind>  <breadcrumb>::<symbol>  :<segment_id>` rows. |
| `symbol` | Same lean row grammar; `<kind>` is prefixed with `def:` or `usage:`. |
| `impact` | Primary rows use `~P`, contextual rows use `~C`; terminal states emit `refused`, `empty`, or `empty_scoped` plus optional hint. |
| `get` | `segment <id>`, tab metadata, blank line, body, blank line, `---`; unresolved handles emit `not_found<TAB><handle>` and `---`. |
| `context` | `<path>:<l1>-<l2>  context  <scope_type>`, blank line, then numbered gutter body. |
| `structural` | `<path>:<l1>-<l2>  structural  <language>::<pattern_name>` plus indented snippet. |
| Maintenance CLI | Human/plain/json renderers; JSON is pretty, plain is tab/key oriented, human is colored and explanatory. |

## Cross-Surface Deltas

- MCP is the primary agent integration; shell `1up ...` remains a human/manual CLI path and is not the scored agent workflow.
- MCP summaries are host-friendly and paired with structured data; retained CLI discovery commands default to human-readable output and expose lean `--plain` output.
- Core discovery commands reject `--format`; maintenance commands keep `--format/-f` for scripting compatibility.
- `oneup_context` rejects locations outside the configured repository; CLI `context` can read outside only with explicit `--allow-outside-root`.
- Search is ranked discovery, not proof of absence. Symbol lookup is the completeness-oriented surface.
- Impact is advisory and trust-bounded. Primary and contextual buckets must not be collapsed in user answers.
- Raw file tools are permitted for exact literal verification after 1up narrows scope, not as first-pass discovery.
- macOS/Linux support daemon-backed workflows; Windows documentation frames local indexing workflows as the current boundary.

## Accessibility And Discoverability Constraints

- Tool names are canonical `oneup_*`; digit-leading aliases are invalid in current guidance and evals.
- MCP tool annotations expose read-only/destructive/idempotent hints and human titles to capable hosts.
- `next_actions` make follow-up commands discoverable without requiring users to memorize workflows.
- Progress animation appears only on stderr TTYs; parseable stdout is preserved for MCP, JSON, and lean row contracts.
- Passive update notifications go to stderr and are suppressed where they would corrupt protocol or JSON consumers.
- File locations are 1-based, and context output prints gutters so users can cite or verify line ranges.
- Setup docs emphasize full absolute repo paths because hosts may launch MCP servers outside the repository working directory.
- Installation and fallback text tells users to reload/restart hosts and review trust prompts rather than assuming setup is immediately active.

## Reconciliation Notes

- Prior search-to-impact semantics remain valid, but the agent reminder surface moved from deleted reminder/skill files to MCP server instructions, README/docs setup guidance, and repo instruction files.
- Prior machine-readable plain/json distinctions for core commands are replaced by a single lean CLI grammar; structured JSON now belongs to maintenance commands and MCP envelopes.
- The `get`/`oneup_get` hydration step is central: search is intentionally compact, and reading selected handles is now an explicit interaction step.
