<p align="center">
  <img src="assets/logo.png" alt="1up" width="128" height="128" />
</p>

<h1 align="center">1up</h1>

<p align="center">
  <strong>Find the right code. Ground the answer in source.</strong>
</p>

<p align="center">
  <a href="https://github.com/rp1-run/1up/releases/latest"><img src="https://img.shields.io/github/v/release/rp1-run/1up?color=blue" alt="Latest release" /></a>
  &nbsp;
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache%202.0-blue" alt="License: Apache 2.0" /></a>
  &nbsp;
  <a href="https://github.com/rp1-run/1up/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/rp1-run/1up/ci.yml?branch=main&label=CI" alt="CI status" /></a>
</p>

<p align="center">
  <img src="assets/readme/icons/lobehub/codex.svg" alt="Codex" width="28" height="28" />
  &nbsp;&nbsp;
  <img src="assets/readme/icons/lobehub/claudecode.svg" alt="Claude Code" width="28" height="28" />
  &nbsp;&nbsp;
  <img src="assets/readme/icons/lobehub/cursor.svg" alt="Cursor" width="28" height="28" />
  &nbsp;&nbsp;
  <img src="assets/readme/icons/lobehub/githubcopilot.svg" alt="GitHub Copilot" width="28" height="28" />
  &nbsp;&nbsp;
  <img src="assets/readme/icons/lobehub/mcp.svg" alt="MCP" width="28" height="28" />
</p>

1up turns a repository or Git worktree into a local index ranked by meaning and text,
and serves it to coding agents over MCP. It is for developers whose agents open with
broad repository reads and burn tokens and time doing it: with 1up the agent starts from ranked,
source-grounded evidence, hydrates only the spans it needs, then verifies symbols and likely
impact — all from an index that never leaves your machine.

Use 1up if you want your agent's first move to be ranked, source-grounded discovery instead of a
broad file sweep, and you are comfortable treating its ranked search and impact as strong leads to
verify rather than exhaustive proofs.

## <img src="assets/readme/icons/heroicons-solid/bolt.svg" alt="" width="20" height="20"> Why 1up

- **Measured, not promised.** In one paired run — the same agent answering the same seven
  questions, with 1up versus without — 1up cut input tokens 44%, latency 42%, and cost 52%, and all
  seven cases were answered correctly both ways.
- **Ranked by meaning and text.** Hybrid vector, full-text, and symbol retrieval finds code by
  intent and returns exact, hydratable source spans, so the agent skips the opening file sweep.
- **Local and private.** The index is built and stored on your machine under `.1up`; only the
  evidence your agent selects is passed to its host.
- **Nine focused MCP tools.** One `oneup` server moves an agent from readiness to orientation,
  ranked discovery, exact evidence, symbol completeness, and advisory impact.
- **Bounded on large repositories.** Over-threshold monorepos are not indexed blindly; 1up reports
  facts and asks for a directory scope so the first index stays fast and predictable.

## <img src="assets/readme/icons/heroicons-solid/chart-bar-square.svg" alt="" width="20" height="20"> Measured Against Going Without

The warm-suite eval runs the same coding agent on the same seven code-comprehension and
impact-analysis questions twice — once with the `oneup` MCP tools, once with only raw file reads
and text search — and both must pass the same factual-accuracy and expected-file assertions.
Latest paired run (2026-07-14):

| Axis (7-case total) | Without 1up | With 1up | Delta |
|---|---|---|---|
| Input tokens | 3,838,805 | 2,142,698 | −44% |
| Latency | 861s | 498s | −42% |
| Cost | $1.32 | $0.63 | −52% |

Both variants answered all seven cases correctly; the with-1up agent averaged 8 tool calls per
case. These are single paired runs, not confidence intervals — the harness, cases, and
reproduction steps live in [evals/README.md](evals/README.md).

## <img src="assets/readme/icons/heroicons-solid/rocket-launch.svg" alt="" width="20" height="20"> Start Here

Two ways to get going. Both end with the `oneup` MCP server configured for your repository.

### Option 1: Let Your Agent Configure 1up

This is the fastest path to a useful result. Paste the prompt below into the MCP-capable coding
agent you use for this repository and let it configure the project-scoped server:

````markdown
# Configure 1up MCP for this repository.

1. If `1up` is not installed, run:
   `curl -fsSL https://github.com/rp1-run/1up/releases/latest/download/setup.sh | bash`
   Otherwise, run `1up update`.
2. Verify the install with `1up --version`.
3. Configure the `oneup` MCP server in project/workspace scope.
   - Server name: `oneup`
   - Command: `1up`
   - Args: `["mcp"]`
4. Use explicit `--path` config only when project/workspace config is not available.
5. Do not try to restart this active host or verify newly added MCP tools from it.
   If config changed, ask the user to restart/reload this host so it can load `oneup`, then approve
   or trust the server if prompted.
6. After reload, call `oneup_status` and follow its next action only if indexing is needed.
   Then call `oneup_overview`, search for a concrete code question with `oneup_search`, and hydrate
   the best result with `oneup_get`.
````

### Option 2: Install It Yourself

Prefer to run each step? The script installer supports Apple Silicon macOS and Linux on arm64 or
x86_64:

```sh
curl -fsSL https://github.com/rp1-run/1up/releases/latest/download/setup.sh | bash
```

The installer places `1up` in `~/.local/bin`. If that directory is already on `PATH`, the command
is ready immediately; otherwise follow the printed instruction or open a new shell. Verify the
binary:

```sh
1up --version
```

Add this project/workspace MCP entry:

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

Reload the host, approve or trust `oneup` if prompted, then call `oneup_status` and follow its
next action.

For host-specific config shapes — Codex, Claude Code, Cursor, VS Code, Copilot, generic JSON
clients, and explicit-path setups — see the focused
[docs/mcp-installation.md](docs/mcp-installation.md) reference. Prefer project/workspace
configuration with `args = ["mcp"]`; use an absolute repository or worktree path only for
user-global or static hosts.

## <img src="assets/readme/icons/heroicons-solid/book-open.svg" alt="" width="20" height="20"> Going Deep

Everything below is reference detail for when you want it: the tool surface, large-repository
scoping, the data and security boundary, staying current, and fixes for common snags.

### <img src="assets/readme/icons/heroicons-solid/wrench-screwdriver.svg" alt="" width="18" height="18"> Nine Tools, One Discovery Loop

One `oneup` server exposes exactly nine focused tools. Together they move an agent from readiness
to orientation, ranked discovery, and exact evidence:

| Tool | Use it for |
|---|---|
| `oneup_status` | Check repository and index readiness without starting work |
| `oneup_start` | Explicitly create, refresh, rebuild, or change the indexed scope |
| `oneup_overview` | Get a bounded orientation digest for an unfamiliar repository |
| `oneup_search` | Find source by meaning and text and return ranked result handles |
| `oneup_get` | Hydrate selected search or symbol handles with indexed source evidence |
| `oneup_symbol` | Find definitions and references for completeness checks |
| `oneup_context` | Read bounded context at repository-contained file-line locations |
| `oneup_impact` | Explore advisory likely impact from a known handle, symbol, or file |
| `oneup_structural` | Run an explicit tree-sitter structural pattern search |

Start with `oneup_status`, follow the returned `oneup_start` action only when needed, and use
`oneup_overview` to get the shape of an unfamiliar repository. Keep raw file reads, `rg`, or `find`
for exact literal verification after `1up` has narrowed the scope.

### <img src="assets/readme/icons/heroicons-solid/arrows-pointing-out.svg" alt="" width="18" height="18"> Keep Large Repositories Bounded

Point `1up` at a large monorepo and it stops before doing expensive, unbounded work. On a first
index, repositories over the default 3,000-file threshold are not indexed in full without a scope.
Instead, `1up` reports repository facts and asks you to choose a repo-relative directory cone:

```sh
1up start --scope services/payments
```

The scope is persisted across branches and restarts, and includes cannot reach outside it. Once an
index already has a scope, let the agent follow the ranked scope suggestions returned by
`oneup_start` to widen or narrow it explicitly; the CLI `--scope` example above is for the first
index, not an implicit widening command.

### <img src="assets/readme/icons/heroicons-solid/shield-check.svg" alt="" width="18" height="18"> Know the Boundaries

The index stays on your machine; selected evidence can still pass to the configured agent host.
The full data, network, and security boundary is:

- Source-derived index data is stored locally under the repository's `.1up` state. Linked
  worktrees share the main worktree's physical index while keeping results scoped to the active
  worktree and branch context.
- MCP returns selected source evidence to the configured agent host. That host's own data handling
  and network policy still apply.
- Installation and updates contact GitHub. Most non-MCP CLI commands may also refresh a cached
  update check. The first explicit indexing action may download the
  [`all-MiniLM-L6-v2`](https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2) model from
  Hugging Face. If embeddings are unavailable, search can degrade to full-text results.
- Built-in secret-path globs reduce accidental indexing of common key, environment, cloud, SSH,
  and credential files. They are not a secret scanner or a data-loss-prevention guarantee.
- 1up makes a best-effort write of `.1up/.gitignore`; do not commit `.1up` index data.
- Every MCP tool except `oneup_start` is annotated read-only. The start tool updates local index
  state; none of the tools edits source files or executes arbitrary shell commands.
- When a release publishes checksums, the installer and updater require the matching archive
  checksum. Provenance verification also runs when a supported verifier can reach a verdict. An
  unavailable, offline, or inconclusive verifier falls back to the checksum with a notice; a
  failed provenance verdict aborts the operation.

### <img src="assets/readme/icons/heroicons-solid/arrow-path.svg" alt="" width="18" height="18"> Stay Current, Recover Cleanly

Keep the binary and its shared worktree index aligned. For the recommended script install, apply
the version advertised on the stable update channel in place, whether the binary lives in the new
`~/.local/bin` default or the legacy `~/.1up/bin` location:

```sh
1up update
```

Force an immediate remote check before applying:

```sh
1up update --check && 1up update
```

All linked worktrees that share an index must use the same 1up version. After upgrading them, use
`1up reindex` only when the CLI reports an old, unreadable, or incompatible index schema; routine
refresh belongs to `1up start` or the action returned by the MCP server.

### <img src="assets/readme/icons/heroicons-solid/information-circle.svg" alt="" width="18" height="18"> Troubleshooting

Most setup failures come down to `PATH`, host reload, repository scope, or version alignment.

<details>
<summary><strong>The host cannot find 1up</strong></summary>

```sh
command -v 1up
1up --version
```

The recommended installer writes `~/.local/bin/1up`. Most shells already include `~/.local/bin`;
when yours does not, the installer prints the rc-file reload step it added. Rerunning the installer
migrates its legacy managed `~/.1up/bin` PATH block to the new default without disturbing the rest
of your rc file. If the commands work in a terminal but not in a GUI host, use `~/.local/bin/1up`
as the absolute binary path in MCP config or launch the host with the same `PATH`.
</details>

<details>
<summary><strong>The tools do not appear</strong></summary>

Reload or restart the host, approve the project/workspace server if prompted, and confirm its name
is `oneup`.
</details>

<details>
<summary><strong>The wrong repository opens, or results are empty</strong></summary>

Confirm the host opened the intended workspace. For static or user-global config, use a canonical
absolute path to the repository or linked worktree; do not rely on the host's working directory.
</details>

<details>
<summary><strong>Indexing is blocked in a large repository</strong></summary>

Choose a repo-relative scope from the returned suggestions. Do not bypass the first-index gate with
a broad include pattern.
</details>

<details>
<summary><strong>The index schema is incompatible</strong></summary>

Update 1up in every linked worktree first. If the current binary says the index is older or
unreadable, run `1up reindex` from the intended worktree.
</details>

<details>
<summary><strong>Windows lifecycle commands say the daemon is unsupported</strong></summary>

This is expected. Use the manual Windows release for local MCP/index/search workflows; background
daemon lifecycle remains unavailable on Windows.
</details>

## <img src="assets/readme/icons/heroicons-solid/command-line.svg" alt="" width="20" height="20"> From the Terminal

The CLI exists for scripts, automation, and index debugging. Agents are the primary interface
through the `oneup_*` MCP tools, and most humans only ever run the installer and `1up update`. When
you do want the shell, the same local index powers a compact lifecycle on macOS and Linux.

<details>
<summary>Terminal command tour</summary>

Lifecycle:

```sh
1up start
1up status
1up list
1up stop
```

Discovery loop:

```sh
1up search "where is authentication configured?"
1up get :RESULT_HANDLE
1up symbol AuthService --references
1up context src/auth.rs:120
1up impact --from-symbol AuthService
```

`1up impact` returns likely follow-up areas, not an exhaustive dependency graph. The lifecycle
commands and `get`, `symbol`, `context`, and `impact` accept `--plain`; search already emits a
stable lean result grammar. `--plain` is only for shell scripts and terminal automation — agents
should use the `oneup_*` MCP tools through the configured server.

Disk cleanup is preview-first:

```sh
1up gc
1up gc --apply
```

The preview reports stale worktree or branch contexts that can be rebuilt later. Use `--apply` only
after reviewing it.
</details>

## <img src="assets/readme/icons/heroicons-solid/cpu-chip.svg" alt="" width="20" height="20"> Platform Support

Use the installer on its published targets; use the Windows archive directly.

| Platform | Install path | Notes |
|---|---|---|
| macOS, Apple Silicon | Script installer | Published as `aarch64-apple-darwin` |
| Linux, arm64 | Script installer | Published as `aarch64-unknown-linux-gnu` |
| Linux, x86_64 | Script installer | Published as `x86_64-unknown-linux-gnu` |
| Windows, x86_64 | Manual zip from [GitHub Releases](https://github.com/rp1-run/1up/releases/latest) | Keep `onnxruntime.dll` beside `1up.exe`, put their directory on `PATH`; no background daemon yet |

There is no published Intel macOS build. On Windows the local binary supports indexing, search, and
MCP use, but the background `start` / `status` / `list` / `stop` daemon workflow is Unix-only.

## <img src="assets/readme/icons/heroicons-solid/document-text.svg" alt="" width="20" height="20"> Documentation

- [MCP installation reference](docs/mcp-installation.md)
- [GitHub Releases](https://github.com/rp1-run/1up/releases)
- [Release history](CHANGELOG.md)
- [Contributing](CONTRIBUTING.md)
- [Building from source](DEVELOPMENT.md)
- [License](LICENSE)

## <img src="assets/readme/icons/heroicons-solid/scale.svg" alt="" width="20" height="20"> License

Apache 2.0. See [LICENSE](LICENSE).
