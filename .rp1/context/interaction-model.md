# 1up — Interaction Model

## Experience Principles

| Principle | Meaning |
|---|---|
| Explicit search-to-impact escalation | Discovery and likely-impact exploration are separate steps. Search finds anchors; impact expands from one exact anchor. |
| Machine-readable first | Discovery and impact default to `plain`; lifecycle/status commands default to `human`. Machine modes carry stable follow-up handles and structured envelopes. |
| Refuse ambiguity, then teach the next step | Broad symbol impact requests return structured refusal plus narrowing hints instead of noisy speculative output. |
| Local impact reads preserve discovery behavior | `impact` reads the local index directly and does not alter daemon-backed search semantics. |

## Actors

| Actor | Goals | Primary Surfaces |
|---|---|---|
| Developer | Find relevant code, escalate from a concrete anchor, recover from ambiguity with guidance. | Discovery CLI, Impact CLI, human output |
| AI agent / host tool | Consume stable search results, capture `segment_id`, drive deterministic search-to-impact loops. | Plain output, JSON output, reminder surface |
| Script / automation | Parse stable stdout contracts and distinguish `expanded`, `expanded_scoped`, `empty`, `empty_scoped`, and `refused` outcomes. | Plain output, JSON output |
| Background daemon | Keep the index warm and serve discovery-oriented search requests. | Daemon process, status surface, search IPC |

## Surfaces

### Discovery CLI

- Entry points: `1up search`, `1up symbol`, `1up context`, `1up structural`
- Role: broad exploration and anchor finding
- Additive behavior: machine-readable modes may expose `segment_id`
- Follow-up rule: use `symbol -r` when exhaustive confirmation matters

### Impact CLI

- Entry points:
  - `1up impact --from-file <path[:line]>`
  - `1up impact --from-symbol <name>`
  - `1up impact --from-segment <segment_id>`
- Role: bounded probable-impact exploration from a known anchor
- Guarantees:
  - exactly one anchor is required
  - only confident non-ambiguous relation-backed likely impact stays in primary `results`; heuristic-only or demoted relation suggestions stay in `contextual_results`
  - no-primary cases return explicit `empty` or `empty_scoped` states instead of anchor echoes
  - output is advisory, not exact dependency truth
  - broad symbol requests are refused with narrowing guidance

### Readiness And Health Surface

- Entry points: `1up status`, `1up start`, `1up index`, `1up reindex`
- Role: check daemon heartbeat, index freshness, and schema readiness before discovery or impact work
- Impact-specific behavior: stale or missing schema v9 indexes fail early and direct the caller to `1up reindex`

### Agent Reminder Surface

- Source: `src/reminder.md`
- Role: teach a conservative workflow
- Core message:
  - check status first
  - search first
  - escalate to impact only from an anchor
  - verify exhaustiveness with `symbol -r`

### Developer Harness (justfile)

- Entry points: `just eval-recall`, `just bench-vector-index-size`
- Role: local-only developer evaluation and benchmarking for retrieval quality and index size
- Not part of the shipped CLI contract; does not appear in `1up --help` or the reminder surface
- Purpose: lets storage-format or ranker changes be validated cold against anchor-based gold and pinned size baselines before shipping

## User-Visible States

| State | Meaning | Signals |
|---|---|---|
| `ImpactExpanded` | Anchor resolved and bounded candidates were returned. | `status = expanded`, resolved anchor block, ranked results |
| `ImpactExpandedScoped` | Expansion happened under explicit or implied narrowing. | `status = expanded_scoped`, scope-aware resolved anchor |
| `ImpactEmptyScoped` | Anchor resolved under scope, but no relation-backed likely-impact candidates survived in that scope. | `status = empty_scoped`, resolved anchor block, optional contextual guidance, no refusal |
| `ImpactRefused` | Request was too broad or too ambiguous to expand safely. | `status = refused`, refusal reason, hint code, suggested scope or segment |
| `ImpactEmpty` | Anchor resolved but no likely-impact candidates were found. | zero-result impact envelope, no refusal |
| `ImpactAnchorValidationError` | User supplied zero or multiple anchors. | exact-one-anchor error mentioning accepted flags |
| `ImpactIndexUnavailable` | Local index is missing or stale for the command contract. | explicit guidance to run `1up reindex` |
| `ImpactContextualGuidance` | Lower-confidence same-file, test-only, or demoted relation suggestions that support follow-up but are not primary likely impact. | `contextual_results` in JSON, `context_result*` lines in plain, `Contextual Guidance` section in human output |
| `SearchResultWithFollowUpHandle` | Search hit is backed by an indexed segment and can feed `impact`. | `segment_id` in plain/json; omitted from concise human search output |

## Feedback Loops

### Status-First Readiness Check

1. User or agent runs `1up status`.
2. Heartbeat freshness and index-built state act as trust signals.
3. Recovery is `start`, `index`, or `reindex` depending on platform and state.

### Search-To-Impact Handoff

1. Search returns a ranked hit with optional `segment_id`.
2. Caller passes that handle into `1up impact --from-segment`.
3. Discovery semantics stay unchanged while follow-up becomes exact.

### Refusal-Narrowing Loop

1. Impact receives a broad symbol or otherwise ambiguous request.
2. The command refuses expansion.
3. Output includes a code, explanation, and suggested scope or segment.
4. Caller reruns with a narrower anchor.

### Search-Then-Verify Loop

1. Ranked search returns promising results.
2. Reminder guidance warns that ranked search may omit lower-scored matches.
3. Caller uses `symbol -r` to verify all locations before acting.

### Retrieval-Quality Evaluation Loop

1. Developer changes a storage/index/retrieval parameter (e.g. vector quantization, prefilter K).
2. `just eval-recall` reindexes cold and reports recall@k against the anchor-based gold corpus.
3. `just bench-vector-index-size` reports on-disk footprint and median indexing time.
4. Developer weighs size vs recall vs REQ absolutes before shipping; gates fail the run on regression.

## Output Semantics

Core commands (`search`, `get`, `symbol`, `impact`, `context`, `structural`) emit a single lean line-oriented shape. They do not accept `--format`. Maintenance commands (`start`, `stop`, `status`, `init`, `index`, `reindex`, `update`) keep the three-renderer contract.

| Surface | Lean | Human | Plain | JSON |
|---|---|---|---|---|
| Search | `<score>  <path>:<l1>-<l2>  <kind>  <breadcrumb>::<symbol>  :<segment_id>` rows; default `-n 3` | n/a | n/a | n/a |
| Get | Segment header line + body, records separated by `---` | n/a | n/a | n/a |
| Symbol | Same row grammar as search; `<kind>` reflects reference vs definition | n/a | n/a | n/a |
| Impact | Primary rows (`~P`) then contextual rows (`~C`) under the same grammar; `empty`, `empty_scoped`, and `refused` envelopes emit a single status/hint line each | n/a | n/a | n/a |
| Context | Header line `<path>:<l1>-<l2>  context  <scope_type>` then numbered content lines | n/a | n/a | n/a |
| Structural | One row per match: `<path>:<l1>-<l2>  structural  <language>::<pattern_name>` + indented snippet | n/a | n/a | n/a |
| Status / lifecycle | n/a | Default mode | Available when requested | Available when requested |
| Index / reindex | n/a | Available when requested | Default mode | Available when requested |

## Cross-Surface Deltas

- Search is ranking-oriented discovery; impact is bounded, anchor-driven, and advisory.
- Search may expose `segment_id` only as an additive machine follow-up handle.
- Impact separates relation-backed likely impact from lower-confidence contextual guidance instead of mixing them into one ranked list.
- Empty impact outcomes are explicit machine-readable states, not padded success results.
- Human search output stays glanceable; human impact output intentionally includes more orientation because the user has already opted into deeper follow-up work.
- Impact does not depend on daemon IPC even though daemon-backed discovery remains the default warm-search path.
- Retrieval-quality and index-size measurement live in the justfile (developer-facing), not in the shipped `1up` CLI — storage-format changes like FLOAT32 -> FLOAT8 quantization can be validated without expanding the public surface.
