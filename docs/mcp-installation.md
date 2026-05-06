# MCP Installation

Use this guide to connect the local `1up` binary to an MCP-capable agent host. The supported server identity is `oneup`, and the server command is:

```sh
1up mcp --path /absolute/path/to/repo
```

All setup below uses manual host configuration. The install script remains the supported binary install path:

```sh
curl -fsSL https://1up.rp1.run/setup.sh | bash
```

## Ready-To-Run Agent Prompt

Paste this into the agent host you want to configure:

```text
Configure 1up MCP for this repository.

RULES
- Configure MCP directly in the host's config.
- Use `SOURCE_ROOT` for the MCP path and repository instruction file.
- If this checkout is a linked Git worktree, keep the linked worktree path as `SOURCE_ROOT`.
- `1up` stores shared index state in the main worktree automatically.
- Do not try to restart this active host or verify newly added MCP tools from it.

PROC
1. Resolve `SOURCE_ROOT` as the active checkout root:
   - If Git is available: run `git rev-parse --show-toplevel`.
   - If Git resolution fails, use `pwd -P`.
   - Do not replace a linked worktree path with the main worktree path for MCP config.
2. Install/update `1up`:
   - If `1up` is not installed, install it with: `curl -fsSL https://1up.rp1.run/setup.sh | bash`
   - If present: `1up update`
   - Verify: `1up --version`
3. Add/update MCP config:
   - Server name: `oneup`
   - Command: `1up`
   - Args: `["mcp", "--path", "<SOURCE_ROOT>"]`
   - JSON shape: `{"mcpServers":{"oneup":{"command":"1up","args":["mcp","--path","<SOURCE_ROOT>"]}}}`
   - For TOML hosts, create the equivalent `oneup` server entry.
4. Insert this minimal 1up hint into the repo instruction file under `SOURCE_ROOT` only if equivalent guidance does not already exist (`AGENTS.md`, `CLAUDE.md`, `.github/copilot-instructions.md`, or host equivalent). Prefer an existing file; create the host's normal repo instruction file only if none exists. Do not duplicate the hint.

For code-discovery questions in this repo, use the `oneup` MCP tools before broad raw search. Use `oneup_status` when readiness is unknown, `oneup_start` only when indexing or rebuilding is needed, `oneup_search` for ranked discovery, `oneup_get` to hydrate result handles, `oneup_context` for precise file-line context, `oneup_symbol` for definitions/references, `oneup_impact` for likely blast radius, and `oneup_structural` for tree-sitter pattern searches. Use `rg`, `grep`, or `find` first only for exact literals, regexes, non-code files, or when the MCP server is unavailable.

5. If MCP config was added or changed, ask the user to restart/reload this host so it can load `oneup`. The active host cannot restart itself. Ask the user to approve/trust `oneup` if the host prompts after restart. After reload, list tools and call `oneup_status`.

OUT
- `SOURCE_ROOT`
- `1up --version`
- MCP config file changed
- repo instruction file changed
- restart/approval message given to user, if needed
```

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

3. Add a manual MCP server entry to the host configuration. Use server identity `oneup`, command `1up`, and args `["mcp", "--path", "/absolute/path/to/repo"]`. Host examples are in [Manual Host Config](#manual-host-config).

4. Paste this single minimal agent hint into the repository instruction file your host reads, such as `AGENTS.md`, `CLAUDE.md`, `.github/copilot-instructions.md`, or the host-equivalent file:

```text
For code-discovery questions in this repo, use the `oneup` MCP tools before broad raw search. Use `oneup_status` when readiness is unknown, `oneup_start` only when indexing or rebuilding is needed, `oneup_search` for ranked discovery, `oneup_get` to hydrate result handles, `oneup_context` for precise file-line context, `oneup_symbol` for definitions/references, `oneup_impact` for likely blast radius, and `oneup_structural` for tree-sitter pattern searches. Use `rg`, `grep`, or `find` first only for exact literals, regexes, non-code files, or when the MCP server is unavailable.
```

5. Reload or restart the host if needed, approve or trust the `oneup` server, list MCP tools, and call `oneup_status`.

## Install Or Update 1up

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

## Choose Repository Path And Scope

Pick the repository the agent should search and prefer a canonical absolute path:

```sh
cd /path/to/repo
pwd -P
```

Use the printed path as `/absolute/path/to/repo`. Absolute paths are safest because agent hosts may launch MCP servers from a home directory, app bundle, workspace root, or background service rather than from your current shell directory.

Choose the host and scope before editing configuration:

| Host | Config shape | Common scope |
|---|---|---|
| Codex | TOML `mcp_servers.oneup` | Project or user-global |
| Claude Code | JSON `mcpServers.oneup` | Project or user-global |
| Cursor | JSON `mcpServers.oneup` | Project or user-global |
| VS Code/Copilot | JSON `servers.oneup` | Workspace or user-global |
| Generic MCP client | Client-specific JSON | Client-specific |

Project or workspace scope is usually easier to review with a team. User-global scope is convenient when you repeatedly use the same host, but it still points at a specific repository path for this server entry.

## Manual Host Config

All examples below use server identity `oneup`, command `1up`, and args `["mcp", "--path", "/absolute/path/to/repo"]`. Replace the path with the absolute repository path from the previous step.

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

After saving manual configuration, reload the host, list tools, and call `oneup_status`.

## Approve Or Trust The Server

Some hosts require a reload, workspace trust confirmation, or project-scoped server approval before tools appear. After setup:

1. Restart or reload the host if it does not pick up MCP changes immediately.
2. Approve or trust the `oneup` server when the host asks.
3. Confirm the displayed command and repository path still match the values above.

Do not approve a server entry that uses a different command, a different repository path, or an unexpected global scope.

## Verify Tool Listing And Readiness

In the configured host, list MCP tools for the `oneup` server. The expected tools are:

| Tool | Purpose |
|---|---|
| `oneup_status` | Check readiness, missing index, stale schema, indexing, or degraded state without indexing |
| `oneup_start` | Create, refresh, or rebuild the local index when explicitly requested |
| `oneup_search` | Search local code by meaning or intent |
| `oneup_get` | Hydrate returned result handles |
| `oneup_symbol` | Find definitions and references |
| `oneup_context` | Retrieve precise repository-scoped file-line context |
| `oneup_impact` | Explore likely impact from a result handle, symbol, or file anchor |
| `oneup_structural` | Run tree-sitter structural pattern searches for supported languages |

Then call `oneup_status`. A clear `ready`, `indexing`, `missing`, `stale`, or `degraded` readiness state is acceptable when it includes actionable guidance. Typical next steps are:

| Readiness | User action |
|---|---|
| `ready` | Start discovery with `oneup_search`. |
| `indexing` | Wait for the current index run to finish, then check readiness again. |
| `missing` | Call `oneup_start` with `{"mode":"index_if_missing"}`, then check readiness again. |
| `stale` | Call `oneup_start` with `{"mode":"reindex"}`, then check readiness again. |
| `degraded` | Read the diagnostic, fix the environment issue, and retry readiness. |

For a discovery smoke, ask the host to search with `oneup_search`, hydrate a selected handle with `oneup_get`, retrieve file-line context with `oneup_context`, use `oneup_symbol` when it needs definition or reference completeness, use `oneup_impact` for explicit likely-impact questions, and use `oneup_structural` for explicit tree-sitter patterns.

## Human Project Lifecycle

Manual MCP setup and the terminal lifecycle are separate surfaces. For human project management, use:

```sh
1up start
1up status
1up list
1up stop
```

Default lifecycle output is human-readable. Add `--plain` when a local script needs stable text without terminal presentation:

```sh
1up status --plain
1up list --plain
```

`--plain` is not the agent protocol. Agents should use the `oneup_*` MCP tools.

## Troubleshooting

### Host Cannot Start The Server

Check the binary and host launch environment:

```sh
command -v 1up
1up --version
```

If the terminal can find `1up` but the host cannot, configure the absolute binary path or launch the host from an environment that inherits the updated `PATH`.

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
- Confirm shell startup files or shell functions are not printing banners to stdout.
- Retry with the installed `1up` binary path rather than a shell command string.
- Capture the host error log, `1up --version`, OS, host version, and the exact server configuration.

### Missing Or Stale Index

`oneup_status` reports index state before discovery. If the index is missing, run:

```sh
cd /absolute/path/to/repo
1up start
```

Then call `oneup_status` again from the host. If readiness still reports stale or degraded state, follow the specific action in the `oneup_status` response, usually `oneup_start` with the returned mode.

### Reporting An Unresolved Setup Issue

Include this information in a maintainer report:

- `1up --version`
- OS and architecture
- Host name and version
- Setup path: manual host config
- Server identity, command, args, and scope
- Repository path used in configuration
- Tool listing result
- `oneup_status` readiness result or host protocol error

## Safety And Permissions

`1up mcp` is a local code-discovery server for the configured repository. It does not:

- Edit source files
- Refactor code
- Run tests
- Execute arbitrary shell commands
- Mutate host MCP configuration
- Index remote repositories directly

The server reads local repository contents through the 1up index and may update `.1up` local index artifacts only through explicit index lifecycle actions. `.1up` is 1up-managed state; it is separate from your source files.

Manual setup changes are performed by you in host-owned configuration files. Review the server identity, repository path, command, and scope before approval.
