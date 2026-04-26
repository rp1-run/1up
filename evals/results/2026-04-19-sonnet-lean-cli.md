# Eval Results: 1up Lean CLI (PR #32)

**Date:** 2026-04-19
**Model:** claude-sonnet-4-6 (both agents)
**Grading model:** claude-haiku-4-5-20251001
**Corpus:** emdash (pinned at 5beb0dd) — 1,362 files, Astro/TypeScript monorepo
**Runner:** promptfoo via `just eval-parallel --summary`
**Branch:** lean-form (PR #32)

## Changes Under Test

1. Lean, agent-first CLI surface: `search`/`symbol`/`impact`/`context`/`structural`/`get` emit one machine-parseable row grammar; `--format` removed from core commands (retained on maintenance commands).
2. New `1up get <handle>` to hydrate full segment bodies by 12-char handle. `impact --from-segment` accepts the same prefix.
3. Agent hints rewritten around the lean grammar; eval prompts and production reminder teach the `search → get → impact` handoff.
4. Both eval prompts (1up + baseline) forbid sub-agent delegation (`Agent`/`Task`) so the comparison measures direct-exploration behaviour on equal footing. Prior runs showed baseline hiding its real exploration cost inside a single `Agent(Explore)` delegation.

## Results

| Task | 1up time | 1up cost | baseline time | baseline cost | Winner (time) |
|------|:--------:|:--------:|:-------------:|:------------:|:------:|
| Search Stack | 61s | $0.37 | 108s | $0.55 | 1up |
| WordPress Import | 90s | $0.48 | 130s | $0.70 | 1up |
| Plugin Architecture | 82s | $0.41 | 126s | $0.73 | 1up |
| Live Content Query | 70s | $0.44 | 81s | $0.60 | 1up |
| FTSManager Impact | 54s | $0.36 | 54s | $0.28 | 1up (tie) |
| Schema Registry Impact | 96s | $0.55 | 113s | $0.43 | 1up |
| Plugin Runner Impact | 62s | $0.31 | 155s | $0.62 | 1up |
| **Total** | **515s** | **$2.93** | **768s** | **$3.91** | **1up** |

**1up vs baseline: -33% time, -25% cost.** 1up wins time on 6 of 7 tasks and ties the 7th (FTSManager Impact, where baseline is cheaper by $0.08).

## Quality and Pass Rate

| Task | 1up score | baseline score | Status |
|------|:---------:|:--------------:|:------:|
| Search Stack | 0.812 | 0.567 | 1up pass, baseline fail |
| WordPress Import | 0.760 | 0.750 | both pass |
| Plugin Architecture | 0.743 | 0.475 | 1up pass, baseline fail |
| Live Content Query | 0.777 | 0.750 | both pass |
| FTSManager Impact | 0.820 | 0.858 | both pass |
| Schema Registry Impact | 0.750 | 0.785 | both pass |
| Plugin Runner Impact | 0.843 | 0.750 | both pass |
| **Average** | **0.787** | **0.705** | **1up +0.08** |

**7/7 passing for 1up, 5/7 for baseline.** Baseline fails Search Stack and Plugin Architecture on the LLM rubric thresholds — both tasks require multi-file trace synthesis that its direct `find`/`grep`/`cat` path cannot cover inside the rubric budget.

## Methodology

- **Search tests (4):** Trace multi-file flows across the emdash monorepo. Tests conceptual exploration where the agent does not know file paths upfront.
- **Impact tests (3):** Identify blast radius of changing a specific file/symbol. Tests dependency tracing through call sites and type references.
- **1up agent tools:** `Bash` (restricted: `1up ...`, `grep` for verification only), `Read`. Prompt forbids `Agent`/`Task`.
- **Baseline agent tools:** `Bash`, `Read`, `Glob`, `Grep`. Prompt forbids `Agent`/`Task`.
- **Assertions:** File-matching (expected files in output), LLM rubric (accuracy + completeness scored by Haiku), efficiency reporting (time, cost, turns).
- **Fixture:** Each test gets a fresh copy of the emdash repo at a pinned commit with a pre-built 1up index.

## Cross-Run Context

Prior eval runs on the same corpus under different prompt postures:

| Run | Sub-agent policy | 1up time | 1up cost | 1up vs baseline (time/cost) |
|-----|-----------------|---------:|---------:|:---------------------------:|
| 2026-04-16 (v0.1.7) | Both sides permitted | 833s | $2.54 | −12% / −32% |
| 2026-04-19 run 1 | Both sides permitted | 770s | $2.46 | −16% / −28% |
| 2026-04-19 run 2 | 1up forbidden, baseline permitted | 603s | $3.45 | −40% / +11% |
| 2026-04-19 run 3 | 1up allowed w/ inheritance, baseline permitted | 592s | $2.57 | −32% / −16% |
| **2026-04-19 run 4 (this result)** | **Both sides forbidden** | **515s** | **$2.93** | **−33% / −25%** |

Run 4 isolates the CLI's contribution by removing differential sub-agent delegation. The `−25%` cost delta is the first clean measurement — earlier numbers were confounded by baseline batching its work inside a single `Agent(Explore)` call while 1up did the same work in visible turns.

## Notes

- Pass rate for the 1up agent held at 7/7 across runs 3 and 4. Its quality average dipped slightly when delegation was forbidden (0.816 → 0.787) on wide-scope impact tasks. The trade — faster wall-clock, cheaper, fewer external dependencies — remains net positive.
- Baseline's quality drops from 0.757 → 0.705 when delegation is forbidden and two tests flip pass → fail. The prior `2t` SDK num_turns counter for baseline was masking this work inside opaque `Agent` delegations.
- Single-run results per posture. Sonnet shows lower variance than Haiku but individual task scores may shift ±0.03 between runs.
