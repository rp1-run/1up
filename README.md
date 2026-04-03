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

Search requires a current local schema-v5 index. On rebuilt/current indexes, 1up combines symbol matches, native-vector retrieval over `segments.embedding_vec`, and FTS5 keyword matches. If the embedding model is unavailable, fails to load, or a vector query fails for this invocation, search warns and degrades to `FtsOnly`. Missing, stale, or partial indexes are recovery cases: run `1up reindex`.

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

### Indexing

Explicitly index a repository without starting the daemon:

```sh
1up index [path]
```

`1up index` is the incremental updater for already-current indexes. On an empty project it creates the current schema-v5 layout; if embeddings are unavailable it still stores searchable content with null vectors.

The command also records a final progress snapshot in `.1up/index_status.json`. JSON output keeps the existing `message` field and adds a `progress` object with the last phase, counters, embedding availability, and timestamp.

Force a full re-index when adopting the rewrite or recovering from stale/partial local indexes:

```sh
1up reindex [path]
```

`1up reindex` treats pre-rewrite local indexes as disposable cache, rebuilds schema v5 from scratch, and repopulates `segments`, `segments_fts`, and `segments.embedding_vec`.

### Daemon Management

Check daemon and index status:

```sh
1up status
```

`1up status` reports the latest recorded indexing progress in addition to daemon state and index counts.

Stop the daemon for the current project:

```sh
1up stop
```

If other projects are still registered, the daemon keeps running. It shuts down when no projects remain or after an idle timeout (default: 30 minutes).

## CLI Reference

### Global Flags

| Flag | Short | Description | Default |
|------|-------|-------------|---------|
| `--format <FORMAT>` | `-f` | Output format: `json`, `human`, `plain` | `json` |
| `--verbose` | `-v` | Increase logging verbosity (`-v` debug, `-vv` trace) | off |

### Subcommands

#### `1up init [PATH]`

Initialize a project for 1up indexing. Creates `.1up/project_id` in the project root.

| Argument | Description | Default |
|----------|-------------|---------|
| `PATH` | Project root directory | `.` |

#### `1up start [PATH]`

Initialize the project if needed, index the repository, and start the background daemon with file watching.

| Argument | Description | Default |
|----------|-------------|---------|
| `PATH` | Project root directory | `.` |

#### `1up stop [PATH]`

Stop the background daemon for the current project. Sends SIGTERM if no projects remain registered, SIGHUP otherwise.

| Argument | Description | Default |
|----------|-------------|---------|
| `PATH` | Project root directory | `.` |

#### `1up status [PATH]`

Show daemon running state, project ID, indexed file count, segment count, and the latest recorded indexing progress.

| Argument | Description | Default |
|----------|-------------|---------|
| `PATH` | Project root directory | `.` |

#### `1up symbol <NAME>`

Look up symbol definitions and optionally references.

| Argument/Flag | Description | Default |
|---------------|-------------|---------|
| `NAME` | Symbol name to look up | required |
| `--references`, `-r` | Include usages in addition to definitions | `false` |
| `--path <PATH>` | Project root directory | `.` |

#### `1up search <QUERY>`

Hybrid semantic + full-text search.

| Argument/Flag | Description | Default |
|---------------|-------------|---------|
| `QUERY` | Search query | required |
| `--limit`, `-n` | Maximum number of results | `20` |
| `--path <PATH>` | Project root directory | `.` |

#### `1up context <LOCATION>`

Retrieve code context around a file location.

| Argument/Flag | Description | Default |
|---------------|-------------|---------|
| `LOCATION` | File location in `file:line` format | required |
| `--path <PATH>` | Project root directory | `.` |
| `--expansion <N>` | Context window in lines (fallback mode) | `50` |

#### `1up index [PATH]`

Index a repository. Downloads the embedding model on first use and emits a final progress snapshot.

| Argument | Description | Default |
|----------|-------------|---------|
| `PATH` | Directory to index | `.` |

#### `1up reindex [PATH]`

Force a full re-index by rebuilding the local schema-v5 search index from scratch. Use this to adopt the rewrite or recover from stale v4 / partial v5 indexes.

| Argument | Description | Default |
|----------|-------------|---------|
| `PATH` | Directory to re-index | `.` |

## Output Formats

JSON is the default format, designed for machine consumption:

```sh
# JSON (default)
1up symbol parse_config

# Human-readable with colors
1up symbol parse_config -f human

# Plain text (no colors, no JSON)
1up symbol parse_config -f plain
```

## Storage Layout

```
~/.config/1up/                  # XDG config (reserved for future use)
~/.local/share/1up/             # XDG data
  daemon.pid                    # Daemon PID file
  projects.json                 # Global project registry
  models/
    all-MiniLM-L6-v2/
      model.onnx                # ONNX embedding model (auto-downloaded)
      tokenizer.json            # WordPiece tokenizer (auto-downloaded)

<project-root>/
  .1up/
    project_id                  # UUID identifying this project
    index.db                    # libSQL database (segments, FTS, vectors)
```

## Supported Languages

Tree-sitter grammars compiled into the binary:

Rust, Python, JavaScript, TypeScript, Go, Java, C, C++

Files in unsupported languages are indexed via text chunking (sliding window) and remain searchable through full-text and semantic search.

## License

MIT
