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
- **Scope-first monorepo path** *(v0.1.13)* — over-threshold repos (`ONEUP_FILE_COUNT_THRESHOLD`, default 3000) refuse a first unscoped index and return a facts envelope (gitignore-aware per-directory stats, calibrated vector estimates with bounds, `launch_subdir` as first suggestion); indexing starts only after a scoped or explicitly confirmed start. Scope persists across branch switches and restarts.
- **`--scope` is a first-class CLI surface** *(v0.1.14)* — `1up start --scope <dir>` applies and persists scope, not just MCP `oneup_start {scope_add}`; the CLI start path enforces the monorepo gate itself before indexing — an over-threshold, unscoped, empty/absent first index does not count as ready, emits the facts envelope, and exits 1 — while an explicit `--scope` bypasses the gate by design. A small unscoped schema-current empty index may still count as ready so the daemon can index it in the background.
- **Non-blocking start with polling** *(v0.1.13)* — `oneup_start` spawns rebuilds and returns within `ONEUP_START_RESPONSE_BUDGET_MS` (2s default): fast ops return final readiness; longer rebuilds return Indexing + progress and agents poll `oneup_status`. Search during rebuild is bounded at 10s, degrading honestly.
- **Reads ride the schema-init window** *(v0.1.14)* — CLI `search/get/symbol/impact/list/structural` and the MCP warm-connection path validate via `ensure_current_tolerating_init`: when a read races direct initialization of a freshly served database, they retry only the transient "tables present, version row absent" shape for ten validation attempts with nine 50 ms sleeps (approximately 450 ms) instead of surfacing a spurious "reindex required"; a genuine version mismatch is a distinct shape and still fails fast on the first attempt. A completed build-aside swap installs a fully initialized database and does not cause this window.
- **Coverage disclosure over silent gaps** *(v0.1.13)* — `index_scope` (roots, indexed/total files, coverage) rides readiness and search payloads; an unscoped `index_scope` adds an `eligibility_note` *(v0.1.14)* explaining the indexed/total gap (unsupported/non-code files, lockfiles, or vendored files); out-of-cone `oneup_context` reads carry `out_of_scope_disclosure` instead of truncating; empty scoped searches suggest widening. Placeholder text never appears in machine-usable `next_actions` arguments (omitted when no real value exists). `oneup_get` takes a `verbosity` *(v0.1.14)* — `default` omits symbol lists, `full` includes detailed symbol metadata — while segment `summary` stays `None` unconditionally and a per-segment `symbol_hint` survives the gating so the `oneup_symbol` verification next_action stays available at default verbosity.
- **Graceful stop for deleted paths** *(v0.1.14)* — `1up stop <deleted-path>` lexically absolutizes and deregisters via a registry fallback (matching state or source root, so a deleted linked worktree resolves), then notifies a live daemon — SIGHUP if other projects remain, SIGTERM if none — rather than dead-ending or falsely reporting `daemon: false` while the worker keeps watching the gone root.
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

- **Readiness** (`oneup_status`): ready / missing / indexing / stale / degraded / blocked (+ `drifted`). `refuse_and_propose_scope` *(v0.1.13)*: first contact with an over-threshold unscoped repo returns the facts envelope with scope_add suggestions. A directly initialized fresh index's brief "tables present, version row absent" window is a transient `initializing` shape that reads retry through, not a failure. Stale schema (e.g. v18 index under a v19 binary) fails closed with an `oneup_start {mode: reindex}` next_action — agents self-serve the migration.
- **Operation** (search): ok / empty / partial / degraded. **Read** (`get`/`context`): found / not_found / ambiguous / rejected / error. **Impact**: expanded / expanded_scoped / empty / empty_scoped / refused.
- **Start**: started / already_running / startup_in_progress / indexed_and_started.
- **Stop** (`stop`): stopped / not_registered / daemon_not_running / unsupported; a deleted-path fallback *(v0.1.14)* still reports the true daemon state (SIGHUP-notified running vs SIGTERM-stopped) instead of a hardcoded `daemon: false`.
- **Lifecycle** (`status`/`list`): not_started / indexing / active / registered / stopped; **watch**: watching / daemon_stopped / source_missing / unsupported / unknown. A stale Running marker on an unregistered project resolves to stopped.
- **Doctor** (per file): clean / would_remove_fence / removed_fence / advise_unfenced.
- **Update**: up_to_date / update_available / yanked / below_minimum_safe.
- **Start schema-drift (JSON)**: schema_out_of_date / binary_out_of_date / index_unreadable (with found/expected/action/path); non-JSON prints a warning + `Run:` action.

## Feedback Loops

- **Orientation** — `oneup_overview` digest → "inspect top symbol / search densest module" (empty → readiness check).
- **Readiness** — `oneup_status` → next_actions steer to search / `oneup_start` mode / poll / retry.
- **Discovery** — `oneup_search` → pre-filled `oneup_get`/`oneup_context`/`oneup_symbol` next_actions.
- **Scope lifecycle** *(v0.1.13)* — facts envelope → `oneup_start {scope_add}` (non-blocking, detached polling) or `1up start --scope` (foreground, awaits initial index before daemon registration, then non-blocking) → poll `oneup_status` (index_scope visible during and after) → widen via further `scope_add` (incremental) or `scope_narrow` (atomic rebuild); scope carries across branch switches via shared DB meta.
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
- **Scope entry point** — MCP applies scope via `oneup_start {scope_add}`; CLI applies + persists it via `1up start --scope <dir>` (validated through the shared `ScopeRoots` guard: absolute paths, `../` escapes, and empty scopes are refused).
- **Outside-root access** — MCP rejects out-of-repo locations; CLI allows with `--allow-outside-root`.
- **Post-upgrade notices** — daemon drain/restart + version-handshake messages on stderr only, keeping JSON/lean stdout clean.

## Affordance / Accessibility Constraints

- Canonical `oneup_*` names are the only discoverable vocabulary; exactly nine retained tools; any `oneup_*` not in `RETAINED_PUBLIC_TOOLS` (legacy `oneup_prepare`/`oneup_read`) is treated as stale and does not exist.
- Every MCP envelope carries ≥1 concrete `next_action` (incl. a fallback when empty); annotations expose read-only/idempotent hints.
- Progress animation only on stderr TTYs; protocol stdout preserved; MCP instructions fit a 2KB budget with the routing rule front-loaded.
- 1-based line citations with `NNNN| ` gutters; setup docs emphasize absolute repo paths + "reload/approve" (the host cannot restart itself).
- Doctor in-scope paths stored forward-slash for cross-platform-identical reports; lean grammar preserves CRLF/byte-exactness.
