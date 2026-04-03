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

Search requires a current local index schema. On rebuilt/current indexes, 1up combines symbol matches, native-vector retrieval, and FTS5 keyword matches. If the embedding model is unavailable, fails to load, or a vector query fails for this invocation, search warns and degrades to `FtsOnly`. Missing, stale, or partial indexes are recovery cases: run `1up reindex`.

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
1up index --jobs 4 --embed-threads 2 [path]
```

All indexing entry points (`index`, `reindex`, and `start`) resolve the same concurrency settings: CLI flags first, then `ONEUP_INDEX_JOBS` and `ONEUP_EMBED_THREADS`, then any settings persisted for the project in the daemon registry, then automatic defaults.

`1up index` is the incremental updater for already-current indexes. The pipeline scans and hashes files, fans out file-local parse work through a bounded worker pool, batches embeddings through a single ONNX session, and keeps database replacement work serialized through transactional writes. If embeddings are unavailable it still stores searchable content with null vectors.

Each run records `.1up/index_status.json` with the latest state, phase, work counters, embedding availability, effective parallelism, per-stage timings, and update timestamp. `1up status` renders the same snapshot in human, JSON, and plain output formats.

Force a full re-index when adopting the rewrite or recovering from stale/partial local indexes:

```sh
1up reindex [path]
1up reindex --jobs 1 --embed-threads 1 [path]
```

`1up reindex` treats pre-rewrite local indexes as disposable cache, rebuilds the local schema from scratch, and repopulates the segment, FTS, and vector data.

### Daemon Management

Start or refresh daemon-managed indexing for a project:

```sh
1up start [path]
1up start --jobs 6 --embed-threads 2 [path]
```

`1up start` persists the resolved indexing settings for that project. If the worker is already running, the command refreshes the registry entry and signals the daemon to reload settings instead of starting a second worker.

Check daemon and index status:

```sh
1up status
```

`1up status` reports daemon state, index counts, and the latest recorded indexing progress, including completed versus skipped work, effective worker count, embed threads, and scan/parse/embed/store/total timings.

Stop the daemon for the current project:

```sh
1up stop
```

If other projects are still registered, the daemon keeps running and reloads its registry via `SIGHUP`. If no projects remain, `1up stop` sends `SIGTERM` and shuts the worker down. While the daemon is running, each project allows only one active indexing pass at a time; file-change bursts collapse into at most one queued follow-up run.

## CLI Reference

### Global Flags

| Flag | Short | Description | Default |
|------|-------|-------------|---------|
| `--format <FORMAT>` | `-f` | Output format: `human`, `json`, `plain` | `human` |
| `--verbose` | `-v` | Increase logging verbosity (`-v` debug, `-vv` trace) | off |

### Subcommands

#### `1up init [PATH]`

Initialize a project for 1up indexing. Creates `.1up/project_id` in the project root.

| Argument | Description | Default |
|----------|-------------|---------|
| `PATH` | Project root directory | `.` |

#### `1up start [PATH]`

Initialize the project if needed, index the repository, persist indexing settings, and start or refresh the background daemon.

| Argument/Flag | Description | Default |
|---------------|-------------|---------|
| `PATH` | Project root directory | `.` |
| `--jobs <N>` | Maximum concurrent parse workers | auto |
| `--embed-threads <N>` | ONNX intra-op threads | auto |

#### `1up stop [PATH]`

Stop the background daemon for the current project. Sends SIGTERM if no projects remain registered, SIGHUP otherwise.

| Argument | Description | Default |
|----------|-------------|---------|
| `PATH` | Project root directory | `.` |

#### `1up status [PATH]`

Show daemon running state, project ID, indexed file count, segment count, and the latest recorded indexing progress summary.

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

#### `1up structural <PATTERN>`

Search indexed code with tree-sitter S-expression queries. Falls back to direct filesystem scanning if no index exists.

| Argument/Flag | Description | Default |
|---------------|-------------|---------|
| `PATTERN` | Tree-sitter query pattern | required |
| `--language <LANG>`, `-l <LANG>` | Restrict matches to one language | unset |
| `--path <PATH>` | Project root directory | `.` |

#### `1up index [PATH]`

Index a repository incrementally. Downloads the embedding model on first use and emits a final progress snapshot with work, parallelism, and timings.

| Argument/Flag | Description | Default |
|---------------|-------------|---------|
| `PATH` | Directory to index | `.` |
| `--jobs <N>` | Maximum concurrent parse workers | auto |
| `--embed-threads <N>` | ONNX intra-op threads | auto |

#### `1up reindex [PATH]`

Force a full re-index by rebuilding the local search index from scratch. Use this to adopt the rewrite or recover from stale or partial indexes.

| Argument/Flag | Description | Default |
|---------------|-------------|---------|
| `PATH` | Directory to re-index | `.` |
| `--jobs <N>` | Maximum concurrent parse workers | auto |
| `--embed-threads <N>` | ONNX intra-op threads | auto |

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
