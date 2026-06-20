---
scope: kbRoot
path_pattern: "interaction-model.md"
producer: knowledge-base
type: document
description: "Cross-surface interaction semantics, UX principles, user-visible states, and accessibility constraints for a single-project codebase."
strictness: strict
---
# Interaction Model

How humans (CLI) and agents (MCP) drive 1up, the states they observe, and the output contracts across surfaces.

## Experience Principles

- **MCP-first agent discovery** — the supported agent surface is the `oneup` MCP server (nine `oneup_*` tools); agents call it before raw grep/rg/find. `SERVER_GUIDANCE` front-loads "before raw grep" so it survives 2KB truncation.
- **Orientation before discovery** — first move on an unfamiliar repo is `oneup_overview` (deterministic digest); it hands the agent its next move.
- **Search → read → verify** — ranked search yields handles, hydrated before any conclusion; `oneup_symbol` is the completeness path. Search is ranked discovery, not proof of absence.
- **Explicit readiness before trust** — every non-ready state carries a recovery action; a drifted HEAD appends an `oneup_start` (`index_if_needed`) action; an ambiguous branch downgrades Ready → Degraded with a reason.
- **Two output registers** — core discovery commands default human-readable and expose `--plain` (one stable lean grammar), rejecting `--format`; maintenance commands keep `--format human|plain|json`.
- **Advisory impact boundary** — primary (higher-confidence) vs contextual (lower-confidence) results never collapse; CLI labels it "Likely impact (advisory)".
- **Refuse unsafe ambiguity** — impact requires exactly one anchor; zero/multiple → refused with narrowing hints.
- **Local-only, user-owned, non-mutating by default** — MCP reads/indexes only the configured repo; only `oneup_start` mutates (every other tool is `read_only_hint=true`); host config stays owned by the host/user.
- **Never writes to instruction files unless invoked** — no normal op creates/edits `AGENTS.md`/`CLAUDE.md`; cleanup is opt-in, default-OFF, preview-first, fence-only.
- **Machine-clean stdout, diagnostics on stderr** — warnings, daemon version-handshake/drain notices, schema-drift banners, disambiguation hints all go to stderr; stdout is parseable rows/JSON/MCP stdio.

## Actors & Surfaces

- **AI agent / MCP host** → `oneup` MCP server (nine tools, server-injected instructions).
- **Human installer/operator** → README Start Here (paste-prompt / terminal install / manual config), `1up add-mcp`, `1up doctor --clean-hints`.
- **Developer CLI user** → streamlined human CLI: `start/status/list/stop/get/symbol/context/impact/doctor` visible; `search/structural/reindex/update/mcp/init/index/add-mcp/__worker` callable but hidden.
- **Script / automation** → `--plain` lean grammar, maintenance `--format json`, MCP `ToolEnvelope`.
- **Background daemon** → `start/status/list/stop`, auto-start on search, drain/restart on version handshake.

## User-Visible States

- **Readiness** (`oneup_status`): ready / missing / indexing / stale / degraded / blocked (+ `drifted`).
- **Operation** (search): ok / empty / partial / degraded. **Read** (`get`/`context`): found / not_found / ambiguous / rejected / error. **Impact**: expanded / expanded_scoped / empty / empty_scoped / refused.
- **Start**: started / already_running / startup_in_progress / indexed_and_started.
- **Lifecycle** (`status`/`list`): not_started / indexing / active / registered / stopped; **watch**: watching / daemon_stopped / source_missing / unsupported / unknown. A stale Running marker on an unregistered project resolves to stopped.
- **Doctor** (per file): clean / would_remove_fence / removed_fence / advise_unfenced.
- **Update**: up_to_date / update_available / yanked / below_minimum_safe.
- **Start schema-drift (JSON)**: schema_out_of_date / binary_out_of_date / index_unreadable (with found/expected/action/path); non-JSON prints a warning + `Run:` action.

## Feedback Loops

- **Orientation** — `oneup_overview` digest → "inspect top symbol / search densest module" (empty → readiness check).
- **Readiness** — `oneup_status` → next_actions steer to search / `oneup_start` mode / poll / retry.
- **Discovery** — `oneup_search` → pre-filled `oneup_get`/`oneup_context`/`oneup_symbol` next_actions.
- **Impact** — `oneup_impact` → read primary, fall back to contextual; refused/empty → narrower anchor/scope.
- **Daemon warm-search-or-fallback** — CLI auto-starts the daemon, tries warm search (250ms timeout); a stale-binary response is refused (stderr warning) → drain+restart; any miss → transparent local fallback (never stale, never blocking).
- **Cross-worktree schema-drift** — a second worktree on a different binary fails closed with a precise error naming the offending schema version + worktree path; remediate via `1up update` (older binary) or `1up reindex` (newer binary).
- **Update** — `update` drains the daemon, fetches a verified manifest, applies/refuses; `--check` forces a fresh fetch; yanked/below-minimum-safe warn to upgrade immediately.
- **Hint cleanup** — `doctor --clean-hints` previews; `--apply` removes only a 1up-owned fence; unfenced tokens are advised, never edited.

## Cross-Surface Deltas

- **Default format** — CLI human (needs `--plain`); MCP presentation-free envelope + text summary mirror.
- **Format flag** — discovery rejects `--format`; maintenance keeps it; lifecycle makes `--plain`/`--format` mutually exclusive.
- **Visibility** — human help hides `search/structural/mcp/init/index/reindex/update/add-mcp`; they stay callable.
- **MCP args by scope** — project: `args:["mcp"]`; global/static: `["mcp","--path","<repo>"]`.
- **Outside-root access** — MCP rejects out-of-repo locations; CLI allows with `--allow-outside-root`.
- **Post-upgrade notices** — daemon drain/restart + version-handshake messages on stderr only, keeping JSON/lean stdout clean.

## Affordance / Accessibility Constraints

- Canonical `oneup_*` names are the only discoverable vocabulary; exactly nine retained tools; any `oneup_*` not in `RETAINED_PUBLIC_TOOLS` (legacy `oneup_prepare`/`oneup_read`) is treated as stale and does not exist.
- Every MCP envelope carries ≥1 concrete `next_action` (incl. a fallback when empty); annotations expose read-only/idempotent hints.
- Progress animation only on stderr TTYs; protocol stdout preserved; MCP instructions fit a 2KB budget with the routing rule front-loaded.
- 1-based line citations with `NNNN| ` gutters; setup docs emphasize absolute repo paths + "reload/approve" (the host cannot restart itself).
- Doctor in-scope paths stored forward-slash for cross-platform-identical reports; lean grammar preserves CRLF/byte-exactness.
