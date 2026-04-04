# Interaction Model

**Project**: 1up
**Last Updated**: 2026-04-04

## Experience Principles

- **Human-first defaults, machine-readable on demand**: CLI defaults to colored human-readable output. Users opt into `json` or `plain` via `--format/-f` for scripting. Human format uses spinners on stderr; json provides structured data; plain emits tab-separated key-value pairs without ANSI.
- **Graceful degradation over hard failure**: When the embedding model is unavailable, the system warns and continues without embeddings. Search degrades to FtsOnly; indexing stores null vectors but retains full-text and symbol data.
- **Progressive automation with explicit escape hatches**: `1up start` auto-initializes, indexes, persists settings, and starts the daemon in one command. Individual steps (`init`, `index`, `reindex`, `stop`) remain available for granular control.
- **Configuration cascade**: Concurrency settings resolve through CLI flags > env vars (`ONEUP_INDEX_JOBS`, `ONEUP_EMBED_THREADS`) > registry persisted values > automatic defaults.
- **Idempotent lifecycle operations**: `start` is safe to re-run (refreshes registry + SIGHUP instead of spawning a second worker). `stop` deregisters the project and only sends SIGTERM when no projects remain.

## Actors & Surfaces

| Actor | Goals | Surface |
|-------|-------|---------|
| Developer | Semantic/keyword search, symbol lookup, context retrieval, structural queries, indexing | CLI |
| Script/tool integration | Consume structured data programmatically, automate lifecycle | CLI (json/plain modes) |
| Background daemon | Watch files, incrementally re-index, persist progress | Daemon process (surfaced via `status`) |

## CLI Entry Points

| Command | Purpose | Key Flags |
|---------|---------|-----------|
| `1up init [PATH]` | Create `.1up/project_id` | |
| `1up start [PATH]` | Auto-init + index + daemon start/refresh | `--jobs`, `--embed-threads` |
| `1up stop [PATH]` | Deregister project, SIGTERM if last | |
| `1up status [PATH]` | Daemon state, counts, indexing progress | |
| `1up search <QUERY>` | Hybrid semantic + FTS search | `--limit` |
| `1up symbol <NAME>` | Symbol definition/reference lookup | `--references` |
| `1up context <LOCATION>` | Enclosing scope context retrieval | `--expansion` |
| `1up structural <PATTERN>` | Tree-sitter S-expression queries | `--language` |
| `1up index [PATH]` | Incremental index | `--jobs`, `--embed-threads` |
| `1up reindex [PATH]` | Full rebuild from scratch | `--jobs`, `--embed-threads` |

Global flags: `--format (human|json|plain)` (default: human), `--verbose (-v/-vv)`

## User-Visible States

| State | Meaning | Human Display |
|-------|---------|---------------|
| IndexState::Idle | No indexing in progress | Dimmed "idle" |
| IndexState::Running | Indexing pass active | Yellow "running" |
| IndexState::Complete | Last pass finished successfully | Green "complete" |
| IndexPhase (Pending/Scanning/Parsing/Storing/Complete) | Current stage within a pass; Storing shows as "embedding & storing" when embeddings enabled | Cyan phase label |
| Daemon running/stopped | Background watcher active or not | Green "running" / red "stopped" |
| Embeddings enabled/disabled | Semantic vector search availability | Green "enabled" / yellow "disabled" |

## Feedback Loops

### Indexing Progress
- **Trigger**: `index`, `reindex`, or `start`
- **Spinner**: Phases shown on stderr ("Preparing database", "Loading embedding model") when TTY detected; resolves to success/warn/update
- **Completion summary**: Work counters (files indexed/skipped/deleted, segments stored), effective parallelism (workers, embed threads), per-stage timings (scan/parse/embed/store/total in ms), embedding availability

### Status Dashboard
- **Trigger**: `1up status`
- **Output**: Daemon state with PID, project ID, indexed file/segment counts, full IndexProgress snapshot (state, phase, work, parallelism, timings, embeddings, last update timestamp)

### Daemon Lifecycle Notification
- **Trigger**: `start` when daemon already running
- **Output**: Reports daemon notified via SIGHUP. Re-registration goes to stderr; new-project registration goes to stdout.

### Embedding Model Warnings
- **Trigger**: Model fails to load or download
- **Output**: Context-specific warning with hint to delete `.download_failed` sentinel to retry

### Search Results
- **Human mode**: Bold kind labels, cyan file:line locations, dimmed metadata (scope, symbols, complexity, score), content preview truncated to 10-12 lines with "..." indicator
- **Empty results**: "No results found." or "No symbols found."

## Output Routing

- **Stdout**: Final results, new-project registration notifications
- **Stderr**: Spinners, diagnostic messages, warnings, re-registration messages
- **Rationale**: Clean separation allows scripts to capture structured output via stdout while progress flows to stderr

## Output Format Semantics

| Mode | Encoding | Use Case |
|------|----------|----------|
| Human | ANSI colors (bold, cyan, dimmed, green, yellow, red), multiline labeled sections | Interactive terminal |
| JSON | Structured objects with nested progress/work/parallelism/timings | Programmatic consumption |
| Plain | Tab-separated `key:value` pairs, no ANSI | Simple text processing (grep, awk) |

All three formats carry the same information density for index summaries and status.

## Accessibility

- **TTY-aware spinners**: Only animate when stderr is a TTY; prevents garbled output in pipes, CI, editors
- **Plain output mode**: Strips all ANSI codes for assistive tools and text processors
- **Content truncation**: Search results truncated to 10-12 lines with visible "..." overflow indicator

## Cross-References
- **Architecture**: See [architecture.md](architecture.md) for system topology and data flows
- **Modules**: See [modules.md](modules.md) for component breakdown
- **Patterns**: See [patterns.md](patterns.md) for implementation conventions
