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
1up --format human hello-agent
```

## Get Started

From the root of the repository you want to search:

### macOS and Linux

```sh
cd /path/to/repo
1up --format human start
```

### Windows

```powershell
cd C:\path\to\repo
1up --format human init
1up --format human index .
```

For interactive use in a terminal, the examples below use `--format human` so results render nicely instead of the default agent-friendly `plain` format.

Try a few common workflows:

```sh
1up --format human search "authentication flow" -n 5
1up --format human symbol -r AuthManager
1up --format human context src/auth/manager.rs:84
1up --format human structural "(function_item name: (identifier) @name)"
```

The first semantic run may download verified `all-MiniLM-L6-v2` model artifacts. On macOS and Linux, the daemon keeps the index current after `1up start`.

## Choose the Right Command

| If you need to... | Use | Why |
|---|---|---|
| Explore unfamiliar code by meaning | `1up --format human search "retry logic with backoff" -n 5` | Ranked semantic and keyword search for discovery |
| Jump to a definition and all callers | `1up --format human symbol -r validate_token` | Exact-first symbol lookup with reference search |
| Understand code at a specific file and line | `1up --format human context src/auth.rs:87` | Snaps to the enclosing function, impl, or scope |
| Inspect likely blast radius from an exact anchor | `1up --format human impact --from-file src/auth.rs:87` | Bounded likely-impact follow-up without changing normal search behavior |
| Match code structure instead of text | `1up --format human structural "(function_item name: (identifier) @name)"` | Tree-sitter AST search |

All commands support `--format plain|json|human`. Use `--format human` for interactive terminal use and `--format json` when an agent or script needs structured output.

For agent handoff loops, `1up --format json search ...` exposes `segment_id` on segment-backed hits. Reuse that exact handle with `1up --format json impact --from-segment <segment_id>` when you want bounded likely-impact follow-up without reconstructing a `file:line` anchor.

## Recommended Workflow

Use semantic search for discovery, then switch to symbol lookup for completeness:

```sh
1up --format human search "rate limit handling" -n 5
1up --format human symbol -r RateLimiter
1up --format human context src/rate_limit.rs:87
```

That pattern is important. Semantic search is ranked and intentionally selective. It is excellent for finding the right place to look, but `1up symbol -r` is the safer follow-up when you need all definitions and references for a symbol.

When an agent needs the next likely inspection targets after discovery, prefer the explicit handoff:

```sh
1up --format json search "load auth config" -n 5
1up --format json impact --from-segment <segment_id>
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

`just bench` runs the search comparison on pinned `emdash` checkouts and reports `1up` against raw `rg` command sequences for the same tasks. `just bench-parallel` runs the parallel indexing benchmark on the same pinned `emdash` corpus.

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
