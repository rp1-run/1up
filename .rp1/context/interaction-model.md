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
- **Search → read → verify** — ranked search yields handles, hydrated before any conclusion; `oneup_symbol` is the completeness path. Search is ranked discovery, not proof of absence. One `oneup_search` call may carry multiple `queries` (fused via RRF) to cover several aspects of a question.
- **Explicit readiness before trust** — every non-ready state carries a recovery action; a drifted HEAD appends an `oneup_start` (`index_if_needed`) action; an ambiguous branch downgrades Ready → Degraded with a reason; an exact detached commit proven un-drifted stays Ready (pinned checkout).
- **Build-identity gates daemon trust, not bare semver** *(new)* — the daemon-authoritative check on `1up search` compares the full `BUILD_IDENTITY` stamp (`{semver}+{git}[.dirty[.digest]]`), not `VERSION`. A daemon reporting the same semver but a different git build — or no identity at all (an unstamped, pre-handshake daemon) — is refused as non-authoritative and drained+restarted, flipping the prior behavior where an absent version was trusted. Closes the same-semver-different-build hazard (issue #108).
- **Indeterminate source presence is never pruned** *(new)* — `1up gc` classifies a candidate's source dir Present/Absent/Indeterminate (`probe_source_presence`) instead of boolean `exists()`. Only a definite `Absent` prunes as `SourceMissing`; `Indeterminate` (transient probe failure, e.g. unreachable network mount) is always retained this cycle with a `tracing::warn!` — re-run once the source is reachable.
- **Torn status-file reads retry before degrading** *(new)* — display-surface status reads classify Absent (not-yet-written, silently None) / Parsed / Unreadable (torn/corrupt). Unreadable retries up to `STATUS_READ_RETRY_ATTEMPTS` (3 × 50ms — a concurrent atomic-replace settling), and only after exhausting retries error-logs and returns None — never fabricating zero/empty progress from a partial write.
- **Stale-branch accumulation is disclosed, not just cleanable** *(new)* — `1up status`/`list` surface a one-line advisory: "N stale branch context(s) (~X reclaimable) — run 1up gc", threshold-gated (`DISCLOSURE_STALE_CONTEXT_COUNT_FLOOR`/`DISCLOSURE_RECLAIMABLE_BYTES_FLOOR`) so expected small accumulation never nags; deletion stays a manual `1up gc` decision. Identical string across human (yellow)/plain (`disclosure_hint:`)/JSON (`disclosure_hint`).
- **Lock files self-heal against reaper races** *(new)* — the MCP instance lock and `1up start` startup guard opportunistically sweep abandoned per-project lock files at mint time (`reap_stale_locks`, best-effort, never delays), and after a successful flock verify the descriptor's pathname still names the locked inode (`flock_still_names_path`) — a concurrent reaper could unlink between open and flock. Orphans are dropped and re-acquired (bounded by `LOCK_ACQUIRE_IDENTITY_RETRIES`). User-visible effect: fewer spurious "another instance is already running" false positives.
- **Two output registers** — core discovery commands default human-readable and expose `--plain` (one stable lean grammar), rejecting `--format`; maintenance commands keep `--format human|plain|json`.
- **Advisory impact boundary** — primary vs contextual results never collapse; CLI labels it "Likely impact (advisory)". Impact requires exactly one anchor; zero/multiple → refused with narrowing hints.
- **Scope-first monorepo path** *(v0.1.13)* — over-threshold repos refuse a first unscoped index and return a facts envelope; indexing starts only after a scoped or explicitly confirmed start. Scope persists across branch switches and restarts. `1up start --scope <dir>` is the first-class CLI surface; the CLI gate enforces before indexing and exits 1 with the facts envelope on fire.
- **Non-blocking start with polling** — `oneup_start` spawns rebuilds and returns within `ONEUP_START_RESPONSE_BUDGET_MS` (2s); longer rebuilds return Indexing + progress and agents poll `oneup_status`. Search during rebuild is bounded (10s), degrading honestly.
- **Reads ride the schema-init window** *(refined)* — CLI read commands and the MCP warm-connection path validate via `ensure_current_tolerating_init`: the transient "tables present, version row absent" shape is retried on its own budget (`SCHEMA_INIT_WAIT_ATTEMPTS`=50 × 100ms ≈ 5s, no longer borrowing the ~450ms DB-lock budget) instead of surfacing a spurious "reindex required"; a genuine version mismatch still fails fast.
- **Coverage disclosure over silent gaps** — `index_scope` (roots, indexed/total, coverage, `eligibility_note`) rides readiness and search payloads; out-of-cone `oneup_context` reads carry `out_of_scope_disclosure`; empty scoped searches suggest widening. Envelope compaction *(v0.1.16)*: authoritative content lives exactly once in `data.records[].{segment,context}.content`, never mirrored into the constant-sized summary grammar. Context reads are scope-size-aware (whole scope ≤ `MAX_WHOLE_SCOPE_LINES`=101 returned untruncated; larger windows clamp to `MAX_CONTEXT_EXPANSION_LINES`=500/side); `verbosity: "full"` symbol lists cap at `MAX_SYMBOLS_PER_LIST`=20. Any bounding is load-bearing disclosure: a clipped record carries a `TruncationNote` + a ready-to-issue recovery call; recovery next_actions are prepended, deduped per path, capped at `MAX_RECOVERY_ACTIONS`=3. `oneup_get` batches are hard-capped (`MAX_GET_HANDLES_PER_CALL`=50, 16KiB handle bytes, 2MiB response budget) with structured over-cap/over-budget outcomes.
- **Graceful stop for deleted paths** — `1up stop <deleted-path>` lexically absolutizes and deregisters via a registry fallback, then notifies a live daemon (SIGHUP if other projects remain, SIGTERM if none) rather than falsely reporting `daemon: false`.
- **Local-only, user-owned, non-mutating by default** — MCP reads/indexes only the configured repo; only `oneup_start` mutates. No normal op creates/edits `AGENTS.md`/`CLAUDE.md`; cleanup is opt-in, preview-first, fence-only.
- **Machine-clean stdout, diagnostics on stderr** — warnings, handshake/drain notices, schema-drift banners, disambiguation hints go to stderr; stdout is parseable rows/JSON/MCP stdio.

## Actors & Surfaces

- **AI agent / MCP host** → `oneup` MCP server (nine tools, server-injected instructions).
- **Human installer/operator** → README Start Here, `1up add-mcp`, `1up doctor --clean-hints`; `setup.sh` installs to `$HOME/.local/bin`, failing closed on unverifiable checksums unless `ONEUP_SKIP_CHECKSUM=1`.
- **Developer CLI user** → streamlined human CLI: `start/status/list/stop/get/symbol/context/impact/doctor` visible; `search/structural/reindex/update/mcp/init/index/add-mcp/__worker` callable but hidden.
- **Script / automation** → `--plain` lean grammar, maintenance `--format json`, MCP `ToolEnvelope`.
- **Background daemon** → `start/status/list/stop`, auto-start on search, drain/restart on build-identity handshake.

## User-Visible States

- **Readiness** (`oneup_status`): ready / missing / indexing / stale / degraded / blocked (+ `drifted`); `refuse_and_propose_scope` on first contact with an over-threshold unscoped repo. Stale schema (e.g. v19 index under a v20 binary) fails closed with an `oneup_start {mode: reindex}` next_action.
- **Index lifecycle** *(new)*: `IndexState::Failed` renders as "failed" (red in human output, plain string in `--plain`) alongside idle/running/complete — a distinct terminal failure state rather than silently reverting to idle.
- **Source presence (gc)** *(new)*: Present / Absent / Indeterminate — only Absent prunes as `SourceMissing`; Indeterminate retains with a warning.
- **Status-file read** *(new)*: Absent / Parsed / Unreadable — replaces "best-effort read, silently None on any failure".
- **Operation** (search): ok / empty / partial / degraded. **Read** (`get`/`context`): found / not_found / ambiguous / rejected / error — a mistyped handle with a unique ≥8-hex-char prefix in the active context recovers to `found` with `recovered_from`; a bounded per-session retry memory refuses an identical already-failed handle without re-query, steering to disambiguating next-actions. **Impact**: expanded / expanded_scoped / empty / empty_scoped / refused.
- **Start**: started / already_running / startup_in_progress / indexed_and_started. **Stop**: stopped / not_registered / daemon_not_running / unsupported; deleted-path fallback reports the true daemon state.
- **Lifecycle** (`status`/`list`): not_started / indexing / active / registered / stopped; **watch**: watching / daemon_stopped / source_missing / unsupported / unknown; plus the advisory stale-branch disclosure hint above the floors.
- **Doctor** (per file): clean / would_remove_fence / removed_fence / advise_unfenced. **Update**: up_to_date / update_available / yanked / below_minimum_safe.
- **Start schema-drift (JSON)**: schema_out_of_date / binary_out_of_date / index_unreadable (with found/expected/action/path).

## Feedback Loops

- **Orientation** — `oneup_overview` digest → "inspect top symbol / search densest module" (empty → readiness check).
- **Readiness** — `oneup_status` → next_actions steer to search / `oneup_start` mode / poll / retry.
- **Discovery** — `oneup_search` (single or multi-query) → pre-filled `oneup_get`/`oneup_context`/`oneup_symbol` next_actions; failed handles get recovery or a structured refusal.
- **Scope lifecycle** — facts envelope → `oneup_start {scope_add}` or `1up start --scope` → poll `oneup_status` (index_scope visible) → widen via `scope_add` (incremental) or `scope_narrow` (atomic rebuild); scope carries across branch switches via DB meta.
- **Impact** — `oneup_impact` → read primary, fall back to contextual; refused/empty → narrower anchor/scope.
- **Daemon warm-search-or-fallback (revised authority)** *(refined)* — CLI auto-starts the daemon, tries warm search; a response is served only when the stamped `daemon_version` equals this binary's full `BUILD_IDENTITY` exactly; any mismatch (same-semver-different-build, or absent stamp) is refused with a stderr warning naming both identities, then the stale daemon is drained and restarted; any miss → transparent local fallback.
- **Stale-branch disclosure** *(new)* — `status`/`list` compute `segments::disclosure_stats` for the active worktree; above a floor, the one-line "run 1up gc" advisory appears (identical across formats); below, silence.
- **gc retention on ambiguous reachability** *(new)* — `1up gc` probes each candidate's source root; definite-absent prunes, Indeterminate warns and retains untouched this cycle, re-evaluated next run.
- **Cross-worktree schema-drift** — a second worktree on a different binary fails closed naming the offending schema version + worktree path; remediate via `1up update` or `1up reindex`.
- **Update** — `update` drains the daemon, fetches a verified manifest, applies/refuses; yanked/below-minimum-safe warn to upgrade immediately.
- **Hint cleanup** — `doctor --clean-hints` previews; `--apply` removes only a 1up-owned fence; unfenced tokens are advised, never edited.

## Cross-Surface Deltas

- **Default format** — CLI human (needs `--plain`); MCP presentation-free envelope + text summary mirror.
- **Format flag** — discovery rejects `--format`; maintenance keeps it; lifecycle makes `--plain`/`--format` mutually exclusive.
- **Visibility** — human help hides `search/structural/mcp/init/index/reindex/update/add-mcp`; they stay callable.
- **MCP args by scope** — project: `args:["mcp"]`; global/static: `["mcp","--path","<repo>"]`.
- **Scope entry point** — MCP applies scope via `oneup_start {scope_add}`; CLI applies + persists it via `1up start --scope <dir>` (validated through the shared `ScopeRoots` guard).
- **Outside-root access** — MCP rejects out-of-repo locations; CLI allows with `--allow-outside-root`.
- **Post-upgrade notices** — daemon drain/restart + build-identity handshake messages on stderr only, keeping JSON/lean stdout clean.

## Affordance / Accessibility Constraints

- Canonical `oneup_*` names are the only discoverable vocabulary; exactly nine retained tools; any `oneup_*` not in `RETAINED_PUBLIC_TOOLS` is treated as stale and does not exist.
- Every MCP envelope carries ≥1 concrete `next_action` (incl. a fallback when empty); annotations expose read-only/idempotent hints.
- Progress animation only on stderr TTYs; protocol stdout preserved; MCP instructions fit a 2KB budget with the routing rule front-loaded.
- Diagnostics-on-stderr extends to torn status files *(new)* — a torn/corrupt status file retries then logs via `tracing::error!`/`warn!` before returning None, so operators can distinguish "not yet written" from "corrupted" without stdout ever carrying a fabricated empty-progress value.
- 1-based line citations with `NNNN| ` gutters; setup docs emphasize absolute repo paths + "reload/approve" (the host cannot restart itself).
- Doctor in-scope paths stored forward-slash for cross-platform-identical reports; lean grammar preserves CRLF/byte-exactness.
