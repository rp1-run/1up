<p align="center">
  <img src="assets/logo.png" alt="1up" width="128" height="128" />
</p>

<p align="center">
  <strong>Semantic code search for agents and developers.</strong>
</p>

<p align="center">
  <code>1up</code> combines ranked semantic search, exact-first symbol lookup, file:line context retrieval,
  and structural AST search in one CLI so agents can find the right code path fast.
</p>

`1up` is built for code exploration, not raw text dumping. Use it when you want to understand how a system works, jump to the right symbol, or inspect surrounding code with minimal noise. Keep `rg` for exact strings, logs, config keys, and any search where you need guaranteed-complete text matches.

## Why 1up

- Search by intent, not just keywords.
- Move from discovery to verification with `search`, `symbol -r`, and `context`.
- Return compact, ranked results that fit agent workflows and context windows.
- Emit `plain`, `human`, or `json` output for terminals, scripts, and coding agents.
- Keep the index warm on macOS and Linux with a background daemon.

## Install

Public installs are intended to come from tagged GitHub releases and first-party package definitions. The distributed executable is always named `1up`.

| Channel | Platforms | Command |
|---|---|---|
| Homebrew | macOS arm64, Linux | `brew install rp1-run/tap/1up` |
| Scoop | Windows | `scoop install https://github.com/rp1-run/scoop-bucket/raw/main/bucket/1up.json` |
| Direct release asset | macOS arm64, Linux arm64, Linux amd64, Windows amd64 | Download the matching archive from [GitHub Releases](https://github.com/rp1-run/1up/releases) |

Verify the install:

```sh
1up --version
1up --help
```

If you downloaded a release archive directly, download the matching `SHA256SUMS` file from the same GitHub Release and verify the archive before unpacking it.

## Strongly Recommended: Install the Agent Skill

After you install `1up`, the most effective setup for agentic work is to install the portable agent skill once:

```sh
npx skills add rp1-run/1up
```

This teaches supported agents when to use `1up search`, `1up symbol`, `1up context`, and `1up structural`, and when `rg` is still the better choice.

On macOS and Linux, `1up start` also creates or updates versioned 1up reminder fences in `AGENTS.md` and `CLAUDE.md` inside the repo. You can preview the injected reminder with:

```sh
1up hello-agent --format human
```

## Get Started

From the root of the repository you want to search:

### macOS and Linux

```sh
cd /path/to/repo
1up start --format human
```

### Windows

```powershell
cd C:\path\to\repo
1up init --format human
1up index . --format human
```

Core agent-facing commands (`search`, `symbol`, `impact`, `context`, `structural`, `get`) emit a single lean row grammar and do not accept `--format`. Maintenance commands (`start`, `stop`, `status`, `init`, `index`, `reindex`, `update`, `hello-agent`) still accept `--format plain|json|human` — use `--format human` for interactive terminal output.

Try a few common workflows:

```sh
1up search "authentication flow" -n 5
1up symbol -r AuthManager
1up context src/auth/manager.rs:84
1up structural "(function_item name: (identifier) @name)"
```

The first semantic run may download verified `all-MiniLM-L6-v2` model artifacts. On macOS and Linux, the daemon keeps the index current after `1up start`.

After indexing, `1up status` shows end-to-end timing (including DB, model, and input preparation), scope info (requested vs executed scope and fallback reasons), and prefilter counters (files discovered, metadata-skipped, content-read, and deleted). Use `1up status --format json` to consume these fields programmatically.

## Choose the Right Command

| If you need to... | Use | Why |
|---|---|---|
| Explore unfamiliar code by meaning | `1up search "retry logic with backoff" -n 5` | Ranked semantic and keyword search for discovery |
| Hydrate a segment body from a handle | `1up get :<segment_id>` | Full content + metadata for the picked handle |
| Jump to a definition and all callers | `1up symbol -r validate_token` | Exact-first symbol lookup with reference search |
| Understand code at a specific file and line | `1up context src/auth.rs:87` | Snaps to the enclosing function, impl, or scope |
| Inspect likely blast radius from an exact anchor | `1up impact --from-file src/auth.rs:87` | Opt-in, local likely-impact follow-up that keeps normal search behavior unchanged |
| Match code structure instead of text | `1up structural "(function_item name: (identifier) @name)"` | Tree-sitter AST search |
| Check indexing timing and scope details | `1up status --format human` | Shows end-to-end timing, scope fallback reasons, and prefilter counters |

Each discovery row emitted by the core commands ends with a `:<segment_id>` handle (12 hex chars). Pass that handle back to `1up get` to pull the full body, or to `1up impact --from-segment <handle>` for bounded likely-impact follow-up — no `file:line` reconstruction required.

## Recommended Workflow

Use semantic search for discovery, then switch to symbol lookup for completeness:

```sh
1up search "rate limit handling" -n 5
1up symbol -r RateLimiter
1up context src/rate_limit.rs:87
```

That pattern is important. Semantic search is ranked and intentionally selective. It is excellent for finding the right place to look, but `1up symbol -r` is the safer follow-up when you need all definitions and references for a symbol.

When an agent needs the next likely inspection targets after discovery, capture a handle from `search` and hand it off directly:

```sh
1up search "load auth config" -n 5
1up impact --from-segment <segment_id>
```

## A Few Honest Notes

- Semantic search is a ranked discovery tool, not proof of completeness. Verify important findings with `1up symbol -r`.
- The first semantic run may download verified model artifacts.
- If embeddings are unavailable, `1up` can still fall back to full-text search instead of failing outright.
- Windows currently focuses on local-mode workflows rather than daemon-backed `start`.

## Benchmarking

The public benchmark and eval corpus for this repo is the pinned `emdash` repository. Search comparisons use raw `rg` workflows as the baseline, not another semantic search tool.

```sh
just bench
just bench-parallel
```

`just bench` runs the search comparison on pinned `emdash` checkouts and reports `1up` against raw `rg` command sequences for the same tasks. `just bench-parallel` runs the parallel indexing benchmark on the same pinned `emdash` corpus and reports release-built wall-clock medians for full index, mostly unchanged incremental, write-heavy incremental, and daemon refresh scenarios. The summary includes scope evidence (fallback, scoped, and full execution counts) and per-run telemetry with timing, scope, and prefilter breakdowns.

The Criterion bench suite also covers `impact_file_anchor`, `impact_symbol_anchor_narrow`, and `impact_symbol_anchor_refused` while keeping the existing search benches as the non-regression guardrail for core discovery commands.

## Agent Eval Results

The eval suite runs Claude agents with and without `1up` on traced-flow tasks across the pinned [emdash](https://github.com/emdash-cms/emdash) monorepo (1,362 files). Each task asks the agent to trace a multi-file flow or identify blast radius — the kind of exploration where semantic search should outperform keyword matching.

```sh
just eval-parallel --summary
```

Latest results (Sonnet, 2026-04-16):

| Task | 1up | baseline | Winner |
|------|:---:|:--------:|:------:|
| Search Stack | 79s / $0.27 | 137s / $0.54 | 1up |
| WordPress Import | 112s / $0.37 | 118s / $0.34 | 1up |
| Plugin Architecture | 116s / $0.40 | 125s / $0.46 | 1up |
| Live Content Query | 131s / $0.40 | 175s / $0.85 | 1up |
| FTS Impact | 152s / $0.40 | 91s / $0.37 | baseline |
| Registry Impact | 98s / $0.28 | 112s / $0.36 | 1up |
| Runner Impact | 145s / $0.42 | 191s / $0.80 | 1up |
| **Total** | **833s / $2.54** | **949s / $3.72** | **1up** |

**1up vs baseline: -12% time, -32% cost.** 6/7 wins for 1up. FTS Impact is the one baseline win — the task names a specific class, so grep is competitive. Full results: [`evals/results/`](evals/results/).

## Upgrade

Use the same channel you installed from:

```sh
brew upgrade 1up
scoop update 1up
```

For direct release assets, download the newer archive from [GitHub Releases](https://github.com/rp1-run/1up/releases), verify it against `SHA256SUMS`, and replace the existing binary.

## Source Builds

Package installs and release assets are the supported onboarding path. If you need a development build instead:

```sh
git clone https://github.com/rp1-run/1up.git
cd 1up
cargo install --path .
```

## Project Docs

- Release history: [CHANGELOG.md](CHANGELOG.md)
- Release runbook: [RELEASE.md](RELEASE.md)
- Contributor policy and merge expectations: [CONTRIBUTING.md](CONTRIBUTING.md)
- Source-build and engineering reference: [DEVELOPMENT.md](DEVELOPMENT.md)

## License

Apache 2.0. See [LICENSE](LICENSE).
