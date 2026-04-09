<p align="center">
  <img src="assets/logo.png" alt="1up" width="128" height="128" />
</p>

# 1up

Unified search substrate for source repositories. A single CLI binary providing symbol lookup, reference search, context retrieval, and hybrid semantic + full-text search with machine-readable JSON output.

Built in Rust with tree-sitter for multi-language parsing, ONNX embeddings (all-MiniLM-L6-v2) for semantic search, and libSQL for persistent storage. A background daemon handles file watching and incremental re-indexing.

## Agent Skill

1up ships a portable [Agent Skill](https://agentskills.io/specification) that teaches AI coding agents to prefer `1up` over grep/rg for code search. Install it so your agent uses semantic search, symbol lookup, and context retrieval instead of raw text matching.

```sh
npx skills add rp1-run/1up
```

This auto-detects your installed agents (Claude Code, Cursor, Copilot, Cline, Windsurf, etc.) and configures the skill for each one.

## Installation

Build from source (requires Rust toolchain):

```sh
git clone https://github.com/prem/1up-rs.git
cd 1up-rs
cargo install --path .
```

The binary is named `1up`.

On the first indexing or search run, 1up may download `model.onnx` and `tokenizer.json` from
Hugging Face into `~/.local/share/1up/models/all-MiniLM-L6-v2/verified/<artifact-id>/`. Those
files only become active after both pass pinned SHA-256 verification and `current.json` is
updated. If a download fails, the last verified artifact stays active and 1up writes
`~/.local/share/1up/models/all-MiniLM-L6-v2/.download_failed`; remove that marker and rerun
`1up index` or `1up start` to retry semantic-search setup.

## Quick Start

```sh
# Initialize a project
cd /path/to/your/repo
1up init

# Index and start the background daemon
1up start

# Search for code
1up search "error handling"

# Look up a symbol
1up symbol MyFunction

# Get context around a line
1up context src/main.rs:42

# Stop the daemon
1up stop
```

## Usage

### Project Setup

Initialize a project to create a `.1up/` directory with a project identifier:

```sh
1up init [path]
```

Index the repository and start the background daemon for file watching. If the project has not been initialized yet, `1up start` will create `.1up/project_id` first:

```sh
1up start [path]
```

The daemon watches for file changes and incrementally re-indexes. If you skip `1up start`, the daemon auto-starts on the first query for initialized projects.

### Search

Hybrid semantic + full-text search with reciprocal rank fusion (RRF) ranking:

```sh
1up search "error handling for network requests"
1up search "database connection pool" -n 10
```

If the embedding model is unavailable, search degrades gracefully to full-text only. Missing or stale indexes are recovery cases: run `1up reindex`.

### Symbol Lookup

Find symbol definitions (functions, structs, traits, classes, types):

```sh
1up symbol MyFunction
1up symbol parse_config
```

Include references (usages) alongside definitions:

```sh
1up symbol -r MyType
1up symbol --references handle_request
```

Supports fuzzy and partial matching -- partial names return ranked candidates.

### Context Retrieval

Retrieve the enclosing scope (function, class, impl block) around a file location:

```sh
1up context src/main.rs:42
1up context src/lib.rs:100 --expansion 80
```

Uses tree-sitter to snap to structural boundaries. Falls back to a line window (+/- 50 lines) for
unsupported languages.

By default, `1up context` only reads files whose canonical path stays under the selected project
root. Absolute paths and outside-root targets are rejected unless you pass
`--allow-outside-root`, and outside-root reads are marked in the result payload.

### Structural Search

Search code using tree-sitter S-expression queries:

```sh
1up structural "(function_item name: (identifier) @name)"
1up structural "(call_expression function: (identifier) @fn)" -l rust
```

Falls back to direct filesystem scanning if no index exists.

### Indexing

Explicitly index a repository without starting the daemon:

```sh
1up index [path]
1up index --jobs 4 --embed-threads 2 [path]
```

Force a full re-index when recovering from stale or partial indexes:

```sh
1up reindex [path]
1up reindex --jobs 1 --embed-threads 1 [path]
```

### Daemon Management

Start or refresh daemon-managed indexing:

```sh
1up start [path]
1up start --jobs 6 --embed-threads 2 [path]
```

Check daemon and index status:

```sh
1up status
```

Stop the daemon for the current project:

```sh
1up stop
```

## CLI Reference

### Global Flags

| Flag | Short | Description | Default |
|------|-------|-------------|---------|
| `--format <FORMAT>` | `-f` | Output format: `plain`, `json`, `human` | `plain` |
| `--verbose` | `-v` | Increase logging verbosity (`-v` debug, `-vv` trace) | off |

### Subcommands

| Command | Description |
|---------|-------------|
| `init [PATH]` | Initialize a project for indexing |
| `start [PATH]` | Init if needed, index, and start/refresh daemon |
| `stop [PATH]` | Stop the daemon for this project |
| `status [PATH]` | Show daemon state, index counts, and indexing progress |
| `search <QUERY>` | Hybrid semantic + full-text search |
| `symbol <NAME>` | Look up symbol definitions and references |
| `context <LOCATION>` | Retrieve enclosing scope around `file:line` |
| `structural <PATTERN>` | Tree-sitter S-expression query search |
| `index [PATH]` | Incremental index without starting daemon |
| `reindex [PATH]` | Full re-index from scratch |

Indexing commands (`index`, `reindex`, `start`) accept `--jobs <N>` and `--embed-threads <N>` to control parallelism.

## Output Formats

Plain text output is the CLI default. Use human for interactive use and JSON when scripting:

```sh
# Plain text (default)
1up symbol parse_config

# Human-readable interactive output
1up symbol parse_config -f human

# JSON for scripts
1up symbol parse_config -f json
```

## Supported Languages

Tree-sitter grammars compiled into the binary:

Rust, Python, JavaScript, TypeScript, Go, Java, C, C++

Files in unsupported languages are indexed via text chunking (sliding window) and remain searchable through full-text and semantic search.

## Development

See [DEVELOPMENT.md](DEVELOPMENT.md) for architecture, daemon IPC, storage layout, internals, and
contributing. Before release work, run `just security-check`; it executes the repo security gate
and writes retained evidence to `target/security/security-check.json`.

## License

Apache 2.0. See [LICENSE](LICENSE).
