<p align="center">
  <img src="assets/logo.png" alt="1up" width="128" height="128" />
</p>

# 1up

Unified search substrate for source repositories. A single CLI binary providing symbol lookup, reference search, context retrieval, and hybrid semantic + full-text search with machine-readable JSON output.

Built in Rust with tree-sitter for multi-language parsing, ONNX embeddings (all-MiniLM-L6-v2) for semantic search, and libSQL for persistent storage. A background daemon handles file watching and incremental re-indexing.

## Installation

Build from source (requires Rust toolchain):

```sh
git clone https://github.com/prem/1up-rs.git
cd 1up-rs
cargo install --path .
```

The binary is named `1up`. On first use, the embedding model (~80MB) is auto-downloaded from Hugging Face.

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

Uses tree-sitter to snap to structural boundaries. Falls back to a line window (+/- 50 lines) for unsupported languages.

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
| `--format <FORMAT>` | `-f` | Output format: `human`, `json`, `plain` | `human` |
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

Human-readable output is the CLI default. Use JSON when scripting:

```sh
# Human-readable (default)
1up symbol parse_config

# JSON for scripts
1up symbol parse_config -f json

# Plain text (no colors, no JSON)
1up symbol parse_config -f plain
```

## Supported Languages

Tree-sitter grammars compiled into the binary:

Rust, Python, JavaScript, TypeScript, Go, Java, C, C++

Files in unsupported languages are indexed via text chunking (sliding window) and remain searchable through full-text and semantic search.

## Development

See [DEVELOPMENT.md](DEVELOPMENT.md) for architecture, storage layout, internals, and contributing.

## License

MIT
