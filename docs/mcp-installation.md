# MCP Installation Reference

Use this page when you need to edit MCP host config by hand. For the fastest setup, install command, and pasteable agent prompt, start with the [README](../README.md#start-here).

## Server Entry

Every host points at the same local stdio server:

```sh
1up mcp
```

Use:

- Server identity: `oneup`
- Command: `1up`
- Preferred project/workspace args: `["mcp"]`
- Explicit-path args, only when needed: `["mcp", "--path", "/absolute/path/to/repo"]`

Install `1up` once globally, then connect it from each project you want an agent to understand. Prefer project or workspace config with `args = ["mcp"]`. Use `--path` only for user-global or static config that may launch outside the repository.

If you need an explicit path, use the absolute path to the repository or linked worktree. Run `pwd -P` from that directory only if you need to discover the path. For linked worktrees, use the linked worktree path; `1up` keeps shared index state under the main worktree while indexing and searching the active worktree.

If a GUI host cannot find `1up`, replace `command = "1up"` or `"command": "1up"` with the absolute binary path from `command -v 1up`.

## Host Config Shapes

### Codex

Project-scoped `.codex/config.toml`:

```toml
[mcp_servers.oneup]
command = "1up"
args = ["mcp"]
```

For user-global `~/.codex/config.toml`, use explicit-path args unless the host guarantees it launches MCP servers from the active project directory.

### Claude Code, Cursor, And Generic MCP JSON Clients

Project config:

```json
{
  "mcpServers": {
    "oneup": {
      "command": "1up",
      "args": ["mcp"]
    }
  }
}
```

Common project locations are `.mcp.json` for Claude Code and `.cursor/mcp.json` for Cursor. Use explicit-path args for user-global settings.

### VS Code And Copilot

Workspace-scoped `.vscode/mcp.json`:

```json
{
  "servers": {
    "oneup": {
      "type": "stdio",
      "command": "1up",
      "args": ["mcp"]
    }
  }
}
```

Use explicit-path args for host-owned user settings.

For user-global or static config where the host does not launch from the project directory, use explicit args instead:

```toml
args = ["mcp", "--path", "/absolute/path/to/repo"]
```

```json
"args": ["mcp", "--path", "/absolute/path/to/repo"]
```

## After Saving Config

1. Reload or restart the host if tools do not appear immediately.
2. Approve or trust the `oneup` server if the host prompts.
3. Confirm the displayed command and detected repository match the intended project.
4. List MCP tools and call `oneup_status`.

Expected tools: `oneup_status`, `oneup_start`, `oneup_search`, `oneup_get`, `oneup_symbol`, `oneup_context`, `oneup_impact`, `oneup_structural`, and `oneup_overview`.

If `oneup_status` reports `missing` or `stale`, call `oneup_start` with the mode suggested in the response, then check readiness again. Once readiness is `ready`, use `oneup_search`, then hydrate evidence with `oneup_get` or `oneup_context`.

## Troubleshooting

### Host Cannot Start `oneup`

Check the binary from a terminal:

```sh
command -v 1up
1up --version
```

If this works in a terminal but not in the host, use the absolute binary path in the MCP config or launch the host from an environment with the same `PATH`.

### Tools Do Not Appear

- Reload or restart the host.
- Approve project-scoped or workspace-scoped servers when prompted.
- Check that the server identity is exactly `oneup`.
- Check that the config file is in the scope your host actually reads.

### Wrong Repository Or Empty Results

For project/workspace config, verify that the host is opening the intended workspace. For explicit-path config, verify the path:

```sh
test -d /absolute/path/to/repo
cd /absolute/path/to/repo
pwd -P
```

Avoid relative paths in explicit-path config. MCP hosts may launch from a home directory, app bundle, workspace root, or background service.

### Protocol Errors

MCP stdio expects protocol messages on stdout. If a host reports parse errors:

- Use `command = "1up"` plus args instead of a shell command string.
- Avoid shell startup files or wrappers that print banners to stdout.
- Try the absolute binary path from `command -v 1up`.
- Capture the host log, `1up --version`, OS, host version, and exact MCP config.

## Cleaning Legacy Pasted 1up Hints

Older 1up onboarding once asked users to paste a code-discovery hint block into their own project instruction files. Some of those pasted blocks reference legacy 1up tool names (the `oneup_*` tokens that 1up no longer provides), so an agent reading them is pointed at tools that do not exist. `1up doctor --clean-hints` is an opt-in helper for finding and cleaning that stale guidance.

This command is **opt-in and default-OFF**: 1up writes nothing to your instruction files unless you run it explicitly. Normal 1up operation (`1up start`, indexing, search) never creates or edits these files.

When you run it with `--clean-hints`, it inspects three files in the project root:

- `AGENTS.md`
- `CLAUDE.md`
- `.github/copilot-instructions.md`

It detects legacy stale 1up tool tokens, defined precisely as any `oneup_*` token that is not one of 1up's current real tools. Currently retained tools are never flagged.

There are two behaviors, and they are deliberately conservative:

- **Fence-only auto-remove**: the command removes only a span 1up can prove it owns — a fenced block delimited by `<!-- 1up:hint:begin -->` and `<!-- 1up:hint:end -->`. Everything outside that exact pair is preserved byte-for-byte. Because earlier guidance was pasted unfenced and 1up has never written such a fence, this path rarely matches existing files in practice.
- **Detect-and-advise**: for any stale token that is not inside a 1up-owned fence, the command reports what it found and recommends manual removal. It never edits unfenced content.

The safety model is preview-first:

```sh
# Read-only preview (writes nothing):
1up doctor --clean-hints

# Apply the fence-only removal (the only path that edits a file):
1up doctor --clean-hints --apply
```

Without `--apply` the command only reports. With `--apply` it removes a 1up-owned fenced span if one is present; unfenced stale tokens are always advised, never edited.

## Safety

`1up mcp` is a local code-discovery server for the detected or configured repository. It does not edit files, refactor code, run tests, execute arbitrary shell commands, mutate host config, or index remote repositories directly.

The server reads local repository contents through the `.1up` index. It may update `.1up` index artifacts only through explicit index lifecycle actions such as `oneup_start`.
