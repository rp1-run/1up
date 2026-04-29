# MCP Installation

Use this guide to connect the local `1up` binary to an MCP-capable agent host. The supported server identity is `oneup`, and the server command is always:

```sh
1up mcp --path /absolute/path/to/repo
```

The wrapper-first path uses `1up add-mcp` to delegate setup to the external `add-mcp` CLI. Manual snippets are the fallback for locked-down environments, team-managed configuration, or hosts where `add-mcp` cannot complete.

## Ready-To-Run Agent Prompt

Paste this into the agent host you want to configure:

```text
Configure 1up MCP for this repository.

RULES
- Configure MCP directly in the host's config. Do not run `1up add-mcp`.
- Use `MAIN_ROOT` for the MCP path and repository instruction file.
- If this checkout is a linked Git worktree, `MAIN_ROOT` is the main worktree root, not the linked worktree path.
- Do not try to restart this active host or verify newly added MCP tools from it.

PROC
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

For code-discovery questions in this repo, use the `oneup` MCP tools before broad raw search. Use `oneup_prepare` when readiness is unknown, `oneup_search` for ranked discovery, `oneup_read` to hydrate returned handles or precise file locations, `oneup_symbol` for definitions/references, and `oneup_impact` for likely blast radius. Use `rg`, `grep`, or `find` first only for exact literals, regexes, non-code files, or when the MCP server is unavailable.

5. If MCP config was added or changed, ask the user to restart/reload this host so it can load `oneup`. The active host cannot restart itself. Ask the user to approve/trust `oneup` if the host prompts after restart.

OUT
- `MAIN_ROOT`
- `1up --version`
- MCP config file changed
- repo instruction file changed
- restart/approval message given to user, if needed
```

The prompt keeps host configuration mutation in host-owned config files. It only adds the small repository instruction that tells future agents when to use 1up.

## Human Quick Setup

1. Install or update `1up`, then verify it.

If `1up` is not installed:

```sh
curl -fsSL https://1up.rp1.run/setup.sh | bash
1up --version
```

If `1up` is already installed:

```sh
1up update
1up --version
```

2. Resolve the repository path:

```sh
cd /path/to/repo
pwd -P
```

3. Try wrapper-first setup for your host:

```sh
1up add-mcp --path /absolute/path/to/repo --agent codex
```

Use `claude-code`, `cursor`, `vscode`, or `github-copilot-cli` instead of `codex` when configuring that host. If the wrapper cannot complete, use the host-specific manual snippets in [Manual Fallback Setup](#7-manual-fallback-setup).

4. Paste this single minimal agent hint into the repository instruction file your host reads, such as `AGENTS.md`, `CLAUDE.md`, `.github/copilot-instructions.md`, or the host-equivalent file:

```text
For code-discovery questions in this repo, use the `oneup` MCP tools before broad raw search. Use `oneup_prepare` when readiness is unknown, `oneup_search` for ranked discovery, `oneup_read` to hydrate returned handles or precise file locations, `oneup_symbol` for definitions/references, and `oneup_impact` for likely blast radius. Use `rg`, `grep`, or `find` first only for exact literals, regexes, non-code files, or when the MCP server is unavailable.
```

5. Reload or restart the host if needed, approve or trust the `oneup` server, list MCP tools, and call `oneup_prepare`.

## 1. Install Or Update 1up

Install `1up` and verify that the installed binary is on the same `PATH` your agent host can use:

```sh
curl -fsSL https://1up.rp1.run/setup.sh | bash
1up --version
```

Update an existing install before configuring a host:

```sh
1up update
1up --version
```

If a GUI host cannot find `1up`, use the absolute path to the installed binary in manual configuration, or adjust the host launch environment so it can resolve the same `1up` you verified in the terminal.

## 2. Choose Repository Path, Host, And Scope

Pick the repository the agent should search and prefer a canonical absolute path:

```sh
cd /path/to/repo
pwd -P
```

Use the printed path as `/absolute/path/to/repo`. Absolute paths are safest because agent hosts may launch MCP servers from a home directory, app bundle, workspace root, or background service rather than from your current shell directory.

Choose the host target and scope before setup:

| Host | Wrapper target | Common scope |
|---|---|---|
| Codex | `--agent codex` | Project or user-global |
| Claude Code | `--agent claude-code` | Project or user-global |
| Cursor | `--agent cursor` | Project or user-global |
| VS Code/Copilot | `--agent vscode` or `--agent github-copilot-cli` | Workspace or user-global |
| Generic MCP client | Manual JSON | Client-specific |

Project or workspace scope is usually easier to review with a team. User-global scope is convenient when you repeatedly use the same host, but it still points at a specific repository path for this server entry.

## 3. Run Wrapper-First Setup

From any directory, run `1up add-mcp` with the repository path and host target:

```sh
1up add-mcp --path /absolute/path/to/repo --agent codex
1up add-mcp --path /absolute/path/to/repo --agent claude-code
1up add-mcp --path /absolute/path/to/repo --agent cursor
1up add-mcp --path /absolute/path/to/repo --agent vscode
```

Use `--global` when the host and `add-mcp` support a user-global installation:

```sh
1up add-mcp --path /absolute/path/to/repo --agent codex --global
```

Use `--yes` only when you intentionally want non-interactive `add-mcp` confirmation:

```sh
1up add-mcp --path /absolute/path/to/repo --agent codex --yes
```

The wrapper behavior is intentionally narrow:

- It validates and canonicalizes the repository path.
- It detects `bunx` first, then `npx`, unless `--runner bunx` or `--runner npx` is provided.
- It invokes the external `add-mcp` package with server identity `oneup`.
- It passes one server source argument equivalent to `1up mcp --path /absolute/path/to/repo`.
- It forwards selected `--agent`, `--global`, and `--yes` options.

`1up add-mcp` does not parse, generate, or patch Codex, Claude Code, Cursor, VS Code, Copilot, or generic client configuration files. Host configuration mutation remains owned by `add-mcp` or by the user through manual setup.

## 4. Review Add-MCP Or Host Confirmation

Before accepting an `add-mcp` prompt, host approval prompt, or generated configuration diff, review:

- Server identity: `oneup`
- Command: `1up mcp --path /absolute/path/to/repo`
- Repository path: the intended local repository, not a parent directory or unrelated checkout
- Scope: project/workspace versus user-global
- Host target: the host you intended to configure

`--yes` can approve the external `add-mcp` flow when supported, but it does not replace host trust, workspace trust, or project approval steps that the host enforces later.

## 5. Approve Or Trust The Server

Some hosts require a reload, workspace trust confirmation, or project-scoped server approval before tools appear. After setup:

1. Restart or reload the host if it does not pick up MCP changes immediately.
2. Approve or trust the `oneup` server when the host asks.
3. Confirm the displayed command and repository path still match the values above.

Do not approve a server entry that uses a different command, a different repository path, or an unexpected global scope.

## 6. Verify Tool Listing And Readiness

In the configured host, list MCP tools for the `oneup` server. The expected tools are:

| Tool | Purpose |
|---|---|
| `oneup_prepare` | Check readiness, missing index, stale schema, indexing, or degraded state |
| `oneup_search` | Search local code by meaning or intent |
| `oneup_read` | Hydrate returned handles or precise file locations |
| `oneup_symbol` | Find definitions and references |
| `oneup_impact` | Explore likely impact from a segment, symbol, or file anchor |

Then call `oneup_prepare` in its default check mode. A clear `ready`, `indexing`, `missing`, `stale`, or `degraded` readiness state is acceptable when it includes actionable guidance. Typical next steps are:

| Readiness | User action |
|---|---|
| `ready` | Start discovery with `oneup_search`. |
| `indexing` | Wait for the current index run to finish, then check readiness again. |
| `missing` | Run `1up start` from the repository, or use an explicit indexing mode if your host workflow supports it. |
| `stale` | Run `1up reindex` for the repository, then check readiness again. |
| `degraded` | Read the diagnostic, fix the environment issue, and retry readiness. |

For a discovery smoke, ask the host to search with `oneup_search`, hydrate a selected handle with `oneup_read`, and use `oneup_symbol` when it needs definition or reference completeness.

## 7. Manual Fallback Setup

Use manual setup when no package runner is available, `add-mcp` is blocked, host approvals need review before mutation, or team policy requires committed configuration.

All examples below use server identity `oneup`, command `1up`, and args `["mcp", "--path", "/absolute/path/to/repo"]`. Replace the path with the absolute repository path from step 2.

### Codex

Project-scoped `.codex/config.toml`:

```toml
[mcp_servers.oneup]
command = "1up"
args = ["mcp", "--path", "/absolute/path/to/repo"]
```

User-global `~/.codex/config.toml` uses the same server block. Prefer project scope when a repository-specific server should be reviewed with the project.

### Claude Code

Project-scoped `.mcp.json`:

```json
{
  "mcpServers": {
    "oneup": {
      "command": "1up",
      "args": ["mcp", "--path", "/absolute/path/to/repo"]
    }
  }
}
```

For user-global setup, use the host-owned Claude Code MCP setup flow with the same identity, command, and args. Project-scoped servers may require explicit approval before tools are available.

### Cursor

Project-scoped `.cursor/mcp.json`:

```json
{
  "mcpServers": {
    "oneup": {
      "command": "1up",
      "args": ["mcp", "--path", "/absolute/path/to/repo"]
    }
  }
}
```

Cursor user configuration uses the same `mcpServers.oneup` server shape when user-global setup is preferred. Fresh setups can require enabling or trusting the server before tool listing works.

### VS Code And Copilot

Workspace-scoped `.vscode/mcp.json`:

```json
{
  "servers": {
    "oneup": {
      "type": "stdio",
      "command": "1up",
      "args": ["mcp", "--path", "/absolute/path/to/repo"]
    }
  }
}
```

Use the host-owned user setting for user-global setup with the same `servers.oneup` entry. Workspace MCP servers can require workspace trust confirmation.

### Generic MCP JSON Client

Use the standard MCP server entry expected by your client:

```json
{
  "mcpServers": {
    "oneup": {
      "command": "1up",
      "args": ["mcp", "--path", "/absolute/path/to/repo"]
    }
  }
}
```

If your client uses `servers` instead of `mcpServers`, keep the same server identity, command, args, and stdio transport.

After saving manual configuration, reload the host, list tools, and call `oneup_prepare`.

## 8. Troubleshooting

### Host Cannot Start The Server

Check the binary and host launch environment:

```sh
command -v 1up
1up --version
```

If the terminal can find `1up` but the host cannot, configure the absolute binary path or launch the host from an environment that inherits the updated `PATH`.

Check runner availability for wrapper setup:

```sh
command -v bunx || command -v npx
```

If neither runner is available, use the manual fallback. If a selected runner is unavailable, retry with `--runner auto` or the runner that exists on `PATH`.

### Repository Path Problems

Verify the configured path exists and is the repository you intended:

```sh
test -d /absolute/path/to/repo
cd /absolute/path/to/repo
pwd -P
```

Avoid relative paths in host configuration. They are resolved from the host's launch directory, which may not be the repository.

### Protocol Errors Or Non-JSON Stdout

MCP stdio requires protocol messages on stdout. User-facing diagnostics should go to stderr. If the host reports protocol parse errors:

- Confirm the command is `1up` with args `["mcp", "--path", "/absolute/path/to/repo"]`.
- Confirm shell aliases, wrapper scripts, or shell startup files are not printing banners to stdout.
- Retry with the installed `1up` binary path rather than a shell command string.
- Capture the host error log, `1up --version`, OS, host version, and the exact server configuration.

### Missing Or Stale Index

`oneup_prepare` reports index state before discovery. If the index is missing, run:

```sh
cd /absolute/path/to/repo
1up start
```

If the schema is stale or incompatible, run:

```sh
cd /absolute/path/to/repo
1up reindex
```

Then call `oneup_prepare` again from the host.

### Reporting An Unresolved Setup Issue

Include this information in a maintainer report:

- `1up --version`
- OS and architecture
- Host name and version
- Setup mode: wrapper, direct `add-mcp`, or manual
- Server identity, command, args, and scope
- Repository path used in configuration
- Tool listing result
- `oneup_prepare` readiness result or host protocol error

## 9. Safety And Permissions

`1up mcp` is a local code-discovery server for the configured repository. It does not:

- Edit source files
- Refactor code
- Run tests
- Execute arbitrary shell commands
- Mutate host MCP configuration
- Index remote repositories directly

The server reads local repository contents through the 1up index and may update `.1up` local index artifacts only through explicit index lifecycle actions. `.1up` is 1up-managed state; it is separate from your source files.

The wrapper delegates host configuration changes to external `add-mcp`. Manual fallback changes are performed by you in host-owned configuration files. In both cases, review the server identity, repository path, command, and scope before approval.
