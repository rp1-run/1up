<p align="center">
  <img src="assets/logo.png" alt="1up" width="128" height="128" />
</p>

<p align="center">
  <strong>Local code search for coding agents.</strong>
</p>

<p align="center">
  Give Codex, Claude Code, Cursor, VS Code, and other MCP-capable agents a fast way to search, read, trace, and reason about your repository.
</p>

`1up` is built for agents. You install it once, connect it to the repository you want an agent to understand, and the agent gets a small set of `oneup_*` MCP tools for code discovery.

When the MCP server connects, `1up` registers the repository and starts or reloads the local background daemon where supported. The agent can then call `oneup_prepare` to check readiness and create or rebuild the local index when needed.

## Start Here

The easiest path is to paste the setup prompt into the agent you want to configure.

## Option 1: Paste This Prompt Into Your Agent

This is the fastest path. Open the repository in your agent host, paste this prompt, and let the agent do the setup.

```markdown
# Configure 1up MCP for this repository.

§RULES
- Configure MCP directly in the host's config. Do not run `1up add-mcp`.
- Use `MAIN_ROOT` for the MCP path and repository instruction file.
- If this checkout is a linked Git worktree, `MAIN_ROOT` is the main worktree root, not the linked worktree path.
- Do not try to restart this active host or verify newly added MCP tools from it.

§PROC
1. Resolve `MAIN_ROOT` as the main worktree root:
   - If Git is available: run `git rev-parse --path-format=absolute --git-common-dir`.
   - If the result ends with `/.git`, strip `/.git`; that path is `MAIN_ROOT`. Stop.
   - Otherwise use `git rev-parse --show-toplevel`.
   - If Git resolution fails, use `pwd -P`.
   - Do not substitute the current linked worktree path after `MAIN_ROOT` is resolved.
2. Install/update `1up`:
   - If `1up` is not installed, install it with: `curl -fsSL https://1up.rp1.run/setup.sh | bash`
   - If present: `1up update`
   - Verify: `1up --version`
3. Add/update MCP config:
   - Server name: `oneup`
   - Command: `1up`
   - Args: `["mcp", "--path", "<MAIN_ROOT>"]`
   - JSON shape: `{"mcpServers":{"oneup":{"command":"1up","args":["mcp","--path","<MAIN_ROOT>"]}}}`
   - For TOML hosts, create the equivalent `oneup` server entry.
4. Insert this minimal 1up hint into the repo instruction file under `MAIN_ROOT` only if equivalent guidance does not already exist (`AGENTS.md`, `CLAUDE.md`, `.github/copilot-instructions.md`, or host equivalent). Prefer an existing file; create the host's normal repo instruction file only if none exists. Do not duplicate the hint.

  ```markdown
  For code-discovery questions in this repo, use the `oneup` MCP tools before broad raw search. Use `oneup_prepare` when readiness is unknown, `oneup_search` for ranked discovery, `oneup_read` to hydrate returned handles or precise file locations, `oneup_symbol` for definitions/references, and `oneup_impact` for likely blast radius. Use `rg`, `grep`, or `find` first only for exact literals, regexes, non-code files, or when the MCP server is unavailable.
  ```

5. If MCP config was added or changed, ask the user to restart/reload this host so it can load `oneup`. The active host cannot restart itself. Ask the user to approve/trust `oneup` if the host prompts after restart.

§OUT
- `MAIN_ROOT`
- `1up --version`
- MCP config file changed
- repo instruction file changed
- restart/approval message given to user, if needed
```

The full ready-to-run agent prompt, human quick setup path, host-specific examples, approval guidance, troubleshooting, and manual fallback setup are in [docs/mcp-installation.md](docs/mcp-installation.md).

## Option 2: Run Add-MCP Yourself

Use this human quick setup path when you want to configure the agent host from your terminal.

Install `1up`:

```sh
curl -fsSL https://1up.rp1.run/setup.sh | bash
```

The installer prints the shell rc file it updated. Source that file, or open a new shell, so `1up` is on your `PATH`:

```sh
source ~/.zshrc   # or ~/.bashrc, per the installer's final message
```

Verify the install:

```sh
1up --version
```

Then connect a repository to your agent host:

```sh
cd /path/to/repo
1up add-mcp --path "$(pwd -P)" --agent codex
```

Use the target for your host:

| Host | Target |
|---|---|
| Codex | `codex` |
| Claude Code | `claude-code` |
| Cursor | `cursor` |
| VS Code | `vscode` |
| GitHub Copilot CLI | `github-copilot-cli` |

After setup, reload the host if needed, approve or trust the `oneup` server, and ask the agent to call `oneup_prepare`. Connecting the server handles daemon startup where supported.

## Option 3: Fully Manual MCP Config

Manual setup is useful when a team wants to review config changes before applying them.

The repo path is the full filesystem path to the folder this MCP server entry should search. For example, if your project is in `/Users/alex/code/my-app`, use that path in the config. A full path is safer than a relative path because your agent host may launch the MCP server from a different directory.

The server identity is `oneup`. The command is `1up`. The args are `["mcp", "--path", "/Users/alex/code/my-app"]`.

```json
{
  "mcpServers": {
    "oneup": {
      "command": "1up",
      "args": ["mcp", "--path", "/Users/alex/code/my-app"]
    }
  }
}
```

For Codex project config, the same server looks like this:

```toml
[mcp_servers.oneup]
command = "1up"
args = ["mcp", "--path", "/Users/alex/code/my-app"]
```

See [docs/mcp-installation.md](docs/mcp-installation.md) for Claude Code, Cursor, VS Code, Copilot, generic MCP JSON clients, approval steps, and troubleshooting.

## Add The Agent Hint

Add this minimal agent-hint snippet for `AGENTS.md` or `CLAUDE.md` to the repository instruction file your host reads:

```text
For code-discovery questions in this repo, use the `oneup` MCP tools before broad raw search. Use `oneup_prepare` when readiness is unknown, `oneup_search` for ranked discovery, `oneup_read` to hydrate returned handles or precise file locations, `oneup_symbol` for definitions/references, and `oneup_impact` for likely blast radius. Use `rg`, `grep`, or `find` first only for exact literals, regexes, non-code files, or when the MCP server is unavailable.
```

Use the plain minimal instruction from the MCP installation guide. You do not need managed AGENTS/CLAUDE reminder fences, `hello-agent`, the old portable skill, or digit-leading `1up_*` MCP aliases.

## What The Agent Gets

Once connected, your agent gets one canonical MCP server named `oneup` and five tools:

| Agent need | MCP tool |
|---|---|
| Check whether the repository is ready | `oneup_prepare` |
| Search by meaning or intent | `oneup_search` |
| Read selected results or exact file locations | `oneup_read` |
| Find definitions and references | `oneup_symbol` |
| Explore likely blast radius | `oneup_impact` |

A good agent flow looks like this:

1. Call `oneup_prepare`.
2. Use `oneup_search` to find the right area of the codebase.
3. Use `oneup_read` to inspect selected results.
4. Use `oneup_symbol` when definitions or references must be complete.
5. Use `oneup_impact` when planning a change and checking likely follow-up files.

`oneup_search` is for discovery, not proof of completeness. Agents should switch to `oneup_symbol` for definition and reference completeness, and they should keep `rg`, `grep`, or `find` for exact literal checks after 1up has narrowed the scope.

## What 1up Does Locally

`1up` indexes the repository you configure and keeps that index local. The MCP server helps agents find relevant code without dumping huge raw search results into context.

It can:

- Build and refresh a local `.1up` index for the configured repository.
- Search by intent with semantic and keyword ranking.
- Return compact handles that agents can hydrate with `oneup_read`.
- Follow symbols and references when a ranked search is not enough.
- Suggest likely impact areas from a segment, symbol, or file anchor.

It does not:

- Edit source files.
- Refactor code.
- Run tests for the agent.
- Execute arbitrary shell commands through MCP.
- Mutate host MCP configuration after setup.

Host configuration remains owned by `add-mcp`, by the host itself, or by the user through manual config review.

## What To Expect

- The first semantic run may download verified `all-MiniLM-L6-v2` model artifacts.
- On macOS and Linux, one background daemon can watch all registered projects. Connecting an MCP server or running `1up start` in another repository registers that project and asks the existing daemon to reload.
- Windows currently focuses on local indexing workflows rather than daemon-backed `start`.
- If embeddings are unavailable, `1up` can fall back to full-text search instead of failing outright.
- If the agent cannot see the `1up` binary, use an absolute binary path in the host config or launch the host from an environment where `1up --version` works.

The install script targets macOS on Apple Silicon and Linux on arm64 or x86_64. Intel macOS and other platforms are not in the published release matrix yet.

## Update 1up

Run:

```sh
1up update
```

This downloads the latest release, verifies it, and replaces the installed binary in place. Re-running `1up update` when you are already current is a no-op and exits 0.

To pin a specific install version:

```sh
curl -fsSL https://1up.rp1.run/setup.sh | env 1UP_VERSION=v0.1.8 bash
```

## Product Proof

The public benchmark and eval corpus for this repo is the pinned `emdash` repository. Search comparisons use raw `rg` workflows as the baseline, not another semantic search tool.

```sh
just bench
just bench-parallel
just eval-parallel --summary
```

`just bench` runs the search comparison on pinned `emdash` checkouts and reports `1up` against raw `rg` command sequences for the same tasks. `just bench-parallel` runs the parallel indexing benchmark on the same pinned `emdash` corpus and reports release-built wall-clock medians for full index, mostly unchanged incremental, write-heavy incremental, and daemon refresh scenarios.

The current adoption evals score MCP tool calls and chains: `oneup_search`, `oneup_read`, `oneup_symbol`, and `oneup_impact`. They fail broad raw `grep`, `rg`, or `find` discovery in the 1up variant, while still allowing exact literal verification after MCP discovery narrows scope.

Archived result (Sonnet, 2026-04-19, lean CLI; both agents forbidden from sub-agent delegation for apples-to-apples comparison):

| Task | 1up | baseline | Winner (time) |
|------|:---:|:--------:|:------:|
| Search Stack | 61s / $0.37 | 108s / $0.55 | 1up |
| WordPress Import | 90s / $0.48 | 130s / $0.70 | 1up |
| Plugin Architecture | 82s / $0.41 | 126s / $0.73 | 1up |
| Live Content Query | 70s / $0.44 | 81s / $0.60 | 1up |
| FTSManager Impact | 54s / $0.36 | 54s / $0.28 | 1up (tie) |
| Schema Registry Impact | 96s / $0.55 | 113s / $0.43 | 1up |
| Plugin Runner Impact | 62s / $0.31 | 155s / $0.62 | 1up |
| **Total** | **515s / $2.93** | **768s / $3.91** | **1up** |

**1up vs baseline: -33% time, -25% cost.** 1up wins time on 6 of 7 tasks and ties the 7th. Quality average: 1up 0.787 vs baseline 0.705. Pass rate: 7/7 for 1up, 5/7 for baseline. Full results and cross-run history: [`evals/results/`](evals/results/).

## Project Docs

- MCP setup guide: [docs/mcp-installation.md](docs/mcp-installation.md)
- Release history: [CHANGELOG.md](CHANGELOG.md)
- Release runbook: [RELEASE.md](RELEASE.md)
- Contributor policy and merge expectations: [CONTRIBUTING.md](CONTRIBUTING.md)
- Source-build and engineering reference: [DEVELOPMENT.md](DEVELOPMENT.md)

## Building From Source

Build from source only if you are hacking on `1up` itself:

```sh
git clone https://github.com/rp1-run/1up.git
cd 1up
cargo install --path .
```

See [DEVELOPMENT.md](DEVELOPMENT.md) for the full contributor setup.

## License

Apache 2.0. See [LICENSE](LICENSE).
