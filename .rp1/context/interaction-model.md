---
scope: kbRoot
path_pattern: "interaction-model.md"
producer: knowledge-base
type: document
description: "Cross-surface interaction semantics, UX principles, user-visible states, and accessibility constraints for a single-project codebase."
strictness: strict
---
# 1up - Interaction Model

**Project**: 1up
**Analysis Date**: 2026-06-15
**Surfaces**: MCP server (agent-facing), discovery CLI, maintenance CLI, MCP setup/onboarding, instruction-file cleanup, agent instruction surfaces

## Experience Principles

- **MCP-first agent discovery**: The supported agent surface is the local `oneup` MCP server with nine canonical `oneup_*` tools. Agents call MCP before raw grep/rg/find/broad file reads and do not shell out to `1up ...` for scored discovery. The server-injected instructions front-load "before raw grep" as the durable routing rule. (`src/mcp/server.rs`, `src/mcp/tools.rs`, `CLAUDE.md`)
- **Orientation before discovery for unfamiliar repos**: The first move in an unfamiliar repository is `oneup_overview`, a deterministic digest of stats, most-referenced types, module map, cross-module dependencies, and entry points. The digest then hands the agent its next move (inspect the top symbol, search the densest module). (`src/mcp/tools.rs:371-399,804-834`, `src/mcp/server.rs:11`)
- **Search-to-read-to-verify**: Discovery starts with ranked search; selected handles or precise locations are hydrated before any conclusion or edit. Symbol lookup is the completeness path for known symbols. Search is explicitly ranked discovery, not proof of absence. (`src/mcp/tools.rs`, `src/cli/get.rs`, `src/cli/symbol.rs`)
- **Explicit readiness before trust**: Agents and users get an explicit index readiness state (`ready`/`missing`/`indexing`/`stale`/`degraded`/`blocked`) before relying on search. Every non-ready state carries a recovery action, and a drifted HEAD adds a refresh action even when ready. (`src/mcp/tools.rs:556-606`, `src/mcp/ops.rs`, `src/cli/status.rs`)
- **Two output registers**: Core discovery commands default to human-readable output (capitalized labels, numbered matches) and expose `--plain` for one stable machine-parseable lean grammar; they reject `--format`. Maintenance commands keep human/plain/json renderers behind `--format/-f`. (`src/cli/discovery_output.rs`, `src/cli/lean.rs`, `src/cli/mod.rs:165-182`)
- **Advisory impact boundary**: Impact exploration is likely-impact guidance, not exact dependency truth. Primary results are higher confidence; contextual results are lower confidence and must be verified. CLI labels output "Likely impact (advisory)". (`src/cli/discovery_output.rs:118-123`, `src/cli/lean.rs:80-123`, `src/mcp/tools.rs:275-325`)
- **Refuse unsafe ambiguity**: Impact requires exactly one anchor and refuses zero/multiple/ambiguous anchors with narrowing hints; CLI and MCP both enforce single-anchor selection. (`src/cli/impact.rs:64-106`, `src/mcp/tools.rs:416-462`)
- **Local-only, user-owned, non-mutating by default**: MCP reads and indexes only the configured local repository through the `.1up` index. It does not edit files, run tests, execute arbitrary shell, mutate host config after setup, or index remote repos. Instruction-file edits happen only through the explicit opt-in `doctor` command. (`docs/mcp-installation.md:168-173`, `src/mcp/tools.rs` annotations)
- **1up never writes to your instruction files unless explicitly invoked**: No normal operation (start, indexing, search) ever creates or edits `AGENTS.md`/`CLAUDE.md`/`.github/copilot-instructions.md`. Cleanup is opt-in, default-OFF, preview-first, and even with `--apply` only removes a span 1up can prove it owns. (`src/cli/mod.rs:121-149`, `src/cli/doctor.rs`, `src/cli/hint_cleanup.rs`)
- **Progress and notices stay off protocol stdout**: Warnings, degradation notices, daemon version mismatches, and disambiguation hints go to stderr; stdout is reserved for parseable lean rows, JSON, and MCP stdio. MCP instructions are engineered to fit a 2KB host truncation budget so the core routing rule survives. (`src/cli/search.rs:58-67`, `src/cli/get.rs:132-137`, `src/mcp/server.rs:62-104`)

## Actors & Surfaces

| Actor | Surface | Goal | Entry Points |
|-------|---------|------|--------------|
| AI agent / MCP host | MCP server (nine `oneup_*` tools), server-injected instructions, repo instruction files | Orient in an unfamiliar repo, discover code by meaning, hydrate evidence, verify symbols, estimate likely blast radius without dumping raw search into context | `oneup_overview`, `oneup_status`, `oneup_search`, `oneup_get`, `oneup_context`, `oneup_symbol`, `oneup_impact`, `oneup_structural`, `oneup_start` |
| Human installer / operator | README setup, `1up add-mcp`, manual JSON/TOML snippets, `1up doctor --clean-hints` | Install 1up once globally, connect `oneup` per project, reload/trust the host, clean legacy pasted hints | `1up add-mcp`, manual MCP config, `1up doctor --clean-hints`, `docs/mcp-installation.md` |
| Developer CLI user | Discovery CLI (get/symbol/context/impact visible; search/structural hidden), maintenance CLI | Search, hydrate handles, read context, look up symbols, explore advisory impact, manage local indexes with readable output by default | `1up get`, `1up symbol`, `1up context`, `1up impact`, `1up status`, `1up start` |
| Script / automation | `--plain` lean grammar, maintenance `--format json`, MCP `ToolEnvelope` | Parse stable lean rows and JSON envelopes, assert on deterministic states | `1up <cmd> --plain`, `1up <cmd> --format json`, MCP structured envelope |
| Background daemon | `1up start`/`status`/`list`, auto-start on search/symbol/context, daemon context status files | Keep registered worktree contexts warm, refresh indexes, serve fast warm search with local fallback | `1up start`, daemon auto-start, watch-status files |

## Primary Actions

### MCP Server (oneup)
**Role**: Primary agent-facing discovery surface exposing nine read-mostly tools that return a presentation-free `ToolEnvelope` plus a text-mirror summary for host display.
**Primary actions**: `oneup_overview` (orientation digest), `oneup_status` (readiness without indexing), `oneup_start` (create/refresh/rebuild index), `oneup_search` (ranked discovery), `oneup_get` (hydrate handles), `oneup_context` (file-line context), `oneup_symbol` (definitions/references), `oneup_impact` (advisory blast radius), `oneup_structural` (tree-sitter pattern search).
**Intentional constraints**: Single configured repository root; read-only tool annotations on all read tools (only `oneup_start` mutates, non-idempotent); presentation-free envelope with no ANSI/spinners/tables; instructions fit a 2KB truncation budget.

### Discovery CLI
**Role**: Human-default terminal discovery with an opt-in lean grammar; agent-facing structural and hidden semantic search live behind `hide` flags.
**Primary actions**: `get` (hydrate handles, leading `:` tolerated, request order preserved), `symbol` (`--references`, `--fuzzy`), `context` (`--allow-outside-root` gate), `impact` (`--from-file`/`--from-symbol`/`--from-handle`, hidden `--from-segment`); hidden `search` (default `-n 3`, daemon auto-start + 250ms fallback) and `structural`.
**Intentional constraints**: Rejects `--format/-f` (use `--plain` for lean output); `context` rejects absolute/outside-root paths unless explicitly allowed.

### Maintenance CLI
**Role**: Manage local project lifecycle and indexes with human/plain/json renderers selectable via `--format/-f`.
**Primary actions**: Visible `start`, `status`, `list`, `stop`, `doctor`; hidden `init`, `index`, `reindex`, `update`, `add-mcp`, `mcp`, `__worker`. Index/reindex expose `--watch`/`--jobs`/`--embed-threads`.
**Intentional constraints**: `start`/`stop`/`status`/`update`/`doctor` default to human, `init`/`index`/`reindex` default to plain; `--plain` and `--format` are mutually exclusive on lifecycle commands.

### Instruction-File Cleanup (doctor)
**Role**: Opt-in, default-OFF, preview-first diagnostic that finds and conservatively removes legacy stale `oneup_*` hints from project instruction files.
**Primary actions**: `1up doctor --clean-hints` (read-only preview), `1up doctor --clean-hints --apply` (fence-only mutate). Scans `AGENTS.md`/`CLAUDE.md`/`.github/copilot-instructions.md`; reports per-file status and recommended action.
**Intentional constraints**: Fence-only auto-remove (only a `<!-- 1up:hint:begin -->`/`<!-- 1up:hint:end -->` span 1up owns); detect-and-advise for unfenced stale tokens, which are never auto-edited; staleness is any `oneup_*` token absent from the nine retained tools.

### MCP Setup And Onboarding
**Role**: Connect the local server to a host and verify readiness, preferring project/workspace config with bare `mcp` args and reserving `--path` for global/static config that may launch outside the repo.
**Primary actions**: `1up add-mcp` (chooses `bunx` before `npx`, delegates to external `add-mcp`), manual JSON/TOML snippets; register `oneup`, reload/trust host, list tools, call `oneup_status`.
**Intentional constraints**: Project scope emits bare `1up mcp`; `--path <repo>` is added only with `--global`; failures print manual-fallback text ending in "call `oneup_status`".

## User-Visible States

| State | Meaning | Surface Signals |
|-------|---------|-----------------|
| `ready` / `missing` / `indexing` / `stale` / `degraded` / `blocked` | MCP readiness for the active worktree context: indexed and searchable / absent or unusable / a job is running / unreadable or schema-incompatible / search runs but embeddings or branch context are limited / repo could not be made ready | `ToolEnvelope.status` + `next_actions` steering to search, `oneup_start` mode, poll, reindex, or setup fix; degraded explains FTS-only or non-branch-filtered results |
| `drifted` | Repository HEAD moved after the last index, so even a ready index may be behind the working tree | Readiness appends an extra `oneup_start` action with mode `index_if_needed` |
| `ok` / `empty` / `partial` / `degraded` | Search/read operation outcome: full results / nothing matched / incomplete / lower-confidence (FTS-only) | Summary counts results and quality; `next_actions` suggest refine, hydrate, or fix |
| `found` / `not_found` / `ambiguous` / `rejected` / `error` | Per-record outcome for `oneup_get`/`oneup_context`: resolved / no segment / prefix matched multiple ids / location outside repo scope / failure | Records list outcomes; all-failed records flip the envelope to a structured error |
| `expanded` / `expanded_scoped` / `empty` / `empty_scoped` / `refused` | Impact outcomes: primary likely-impact returned (optionally scope-limited) / anchor resolved but nothing survived / expansion unsafe or ambiguous | CLI lean rows end `~P`/`~C`; `refused`/`empty` emit a terminal line + optional hint; MCP `refused` flips to structured error |
| `started` / `already_running` / `startup_in_progress` / `indexed_and_started` | Lifecycle start outcomes for the daemon/index | Human/plain/json start renderers include message, pid, optional index progress |
| `not_started` / `indexing` / `active` / `registered` / `stopped` | Project lifecycle: nothing set up / indexing / registered with running daemon / registered without daemon / artifacts exist but inactive | `status`/`list` render the state (human colorized) plus context id, worktree role, branch, watch status, last update metadata |
| `watching` / `daemon_stopped` / `source_missing` / `unsupported` / `unknown` | Per-context daemon watch state for a registered worktree | `status`/`list` include watch state alongside branch/worktree and last-refresh metadata |
| `clean` / `would_remove_fence` / `removed_fence` / `advise_unfenced` | Per-file doctor outcomes: no legacy hints / a 1up-owned fence would be removed (preview) / a fence was removed (`--apply`) / stale unfenced tokens reported but never auto-edited | Doctor report lists each file's status, recommended action, stale tokens with line numbers, and a `modified` flag true only when a fence was actually removed |

## Feedback Loops

- **MCP Orientation Loop**: Agent begins work on an unfamiliar repo -> `oneup_overview` returns a deterministic digest (stats, top types, module map, dependencies, entry points) -> non-empty digests suggest inspecting the top symbol then searching the densest module; empty digests fall back to a readiness check.
- **MCP Readiness Loop**: Readiness unknown for the configured repo/worktree -> `oneup_status` reports `ready`/`missing`/`indexing`/`stale`/`degraded`/`blocked` (plus `drifted`) with context-scoped counts and matching progress/heartbeat -> `next_actions` steer to search, `oneup_start` mode, poll, reindex, or setup fix.
- **MCP Discovery Loop**: Agent has a code-discovery question after readiness is `ready` -> `oneup_search` returns compact ranked handles -> the envelope's `next_actions` hand back pre-filled `oneup_get`/`oneup_context`/`oneup_symbol` calls so evidence is read before concluding.
- **MCP Impact Loop**: Explicit blast-radius question after evidence exists -> `oneup_impact` returns primary (likely targets) and contextual (lower-confidence) buckets -> `next_actions` push reading primary first, fall back to contextual, and on refused/empty suggest a narrower handle, scope, or search.
- **Lean CLI Handle Loop**: `search`/`symbol`/`impact` emit `:<12-char-handle>` lean rows -> user or script pastes handles (leading `:` tolerated, order preserved) into `1up get` -> `get` emits full segment records or `not_found<TAB><handle>` + `---` sentinels, with ambiguous-prefix disambiguation on stderr.
- **Index Progress Loop**: User runs `index`/`reindex`/`start` -> TTY human mode shows stderr progress while watch/plain/json/non-TTY stream parseable progress (phase, counts, embeddings, context id, branch, parallelism, timings, scope, prefilter) -> final summaries report results and updated timestamp.
- **Setup-And-Verify Loop**: User configures `oneup` via `add-mcp` or manual config -> verifies identity `oneup`, command `1up`, args (`mcp` for project, `mcp --path <repo>` for global), reloads/trusts host, lists tools -> calls `oneup_status`; failures print manual-fallback text ending in "call `oneup_status`".
- **Hint-Cleanup Preview Loop**: User suspects legacy pasted hints -> `1up doctor --clean-hints` previews per-file findings (`clean`/`would_remove_fence`/`advise_unfenced`) writing nothing -> re-running with `--apply` removes only a 1up-owned fenced span (idempotent on re-run); unfenced stale tokens are always advised, never edited.

## Accessibility & Discoverability

- **Keyboard / touch / voice rules**: 1up is a terminal- and protocol-first product; affordances are stable command and tool grammars rather than pointer interactions. Canonical `oneup_*` tool names are the single discoverable vocabulary across agents, evals, and hint-staleness detection — there are nine retained tools, and any `oneup_*` token not in `RETAINED_PUBLIC_TOOLS` (e.g. legacy `oneup_prepare`/`oneup_read`) is treated as stale and does not exist.
- **Focus / announcement behavior**: Every MCP envelope carries at least one concrete, argument-filled `next_action` (including a fallback when results are empty), so agents do not need to memorize the workflow. Read-only / destructive / idempotent tool annotations and human titles let capable hosts surface safety hints; all read tools set `read_only_hint=true` and only `oneup_start` is non-idempotent.
- **Reduced motion / sensory load**: Progress animation appears only on stderr TTYs; parseable stdout is preserved for MCP stdio, JSON, and lean rows. Warnings, degradation notices, and disambiguation hints are kept off protocol stdout. MCP instructions are engineered to fit a 2KB host truncation budget with the "before raw grep" routing guidance front-loaded so it survives an adverse cut.
- **Citation and verification affordances**: File locations are 1-based and `context` output prints `NNNN| <line>` gutters so users and agents can cite or verify exact line ranges. Setup docs emphasize absolute repo/worktree paths because hosts may launch MCP servers from a home dir, app bundle, or background service, and consistently tell users to reload/restart and approve `oneup` before assuming setup is active.
- **Cross-platform stability**: Doctor in-scope file paths are stored as forward-slash literals so report output is identical across platforms, and the lean grammar preserves CRLF and missing-final-newline byte-exactness.

## Cross-Surface Deltas

| Behavior | Surfaces | Delta | Reason |
|----------|----------|-------|--------|
| Default discovery output format | Discovery CLI vs MCP | CLI defaults to human-readable (capitalized labels, numbered matches), requiring `--plain` for the lean grammar; MCP returns a presentation-free envelope + plain-text summary mirror (no ANSI/spinners/tables) | Terminals want readable output; agents and scripts want structured contracts |
| Format flag availability | Discovery CLI vs Maintenance CLI | Core discovery commands reject `--format/-f` (use `--plain`); maintenance commands keep `--format/-f` for scripting/JSON | Discovery has one canonical lean grammar; maintenance preserves existing JSON-consuming integrations |
| MCP server args by config scope | MCP setup (project vs global) | Project/workspace config uses bare `args = ["mcp"]`; user-global/static config uses `["mcp", "--path", "<absolute-repo>"]`. `add-mcp` emits bare `1up mcp` for project scope, `1up mcp --path <repo>` only with `--global` | Project hosts launch from the repo; global/static hosts may launch elsewhere and need an explicit path |
| Outside-root file access | `oneup_context` vs `1up context` | MCP rejects any location outside the configured repository; CLI rejects absolute/outside paths by default but allows them with `--allow-outside-root` | The MCP boundary is strictly repo-scoped for safety; the local human operator may deliberately opt out |
| Structural search exposure | MCP vs CLI structural | `oneup_structural` is a retained, advertised agent tool; CLI `structural` is hidden compatibility (lean structural row grammar still exists) | Structural search is valuable to agents but not a primary human terminal entry point |
| Impact primary vs contextual buckets | `oneup_impact` and `1up impact` | Both keep primary (likely target) and contextual (lower-confidence, verify) buckets distinct and must not collapse them | Conflating confidence levels would misrepresent advisory impact as dependency truth |
| Warm search vs local fallback | Discovery CLI (daemon vs local) | `search`/`symbol`/`context` auto-start the daemon and try warm search with a 250ms timeout, then transparently fall back to local in-process search; version skew warns to restart the daemon | Warm search is faster when available, but discovery must never block or fail when the daemon is unavailable |

## Related KB Links

- **System topology**: See [architecture.md](architecture.md)
- **Component inventory**: See [modules.md](modules.md)
- **Terminology**: See [concept_map.md](concept_map.md)
- **Implementation details**: See [patterns.md](patterns.md)
