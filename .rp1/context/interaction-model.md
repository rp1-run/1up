# Interaction Model

**Project**: 1up
**Last Updated**: 2026-04-12

## Experience Principles

- **Agent-first defaults, human-readable on demand**: CLI defaults to `plain` output (tab-separated key-value pairs, no ANSI) for agent and scripting friendliness. Users opt into `human` via `--format/-f` for interactive use. Human format uses spinners on stderr; json provides structured data; plain emits tab-separated key-value pairs without ANSI.
- **Graceful degradation over hard failure**: When the embedding model is unavailable, the system warns and continues without embeddings. Search degrades to FtsOnly; indexing stores null vectors but retains full-text and symbol data. EmbeddingLoadStatus provides granular reasons (PreviousDownloadFailed, ModelMissing, LoadFailed, DownloadFailed) so feedback is context-specific.
- **Progressive automation with explicit escape hatches**: `1up start` auto-initializes, indexes, installs agent instruction fences, persists settings, and starts the daemon in one command. Individual steps (`init`, `index`, `reindex`, `stop`) remain available for granular control. `1up update` auto-refreshes stale cache then applies or advises; `--check` and `--status` give explicit control over the update lifecycle.
- **Configuration cascade**: Concurrency settings resolve through CLI flags > env vars (`ONEUP_INDEX_JOBS`, `ONEUP_EMBED_THREADS`) > registry persisted values > automatic defaults.
- **Idempotent lifecycle operations**: `start` is safe to re-run (refreshes registry + SIGHUP instead of spawning a second worker; fence install is idempotent via version check). `stop` deregisters the project and only sends SIGTERM when no projects remain.
- **Daemon-first with local fallback**: Search attempts daemon-served results first via Unix socket with a 250ms timeout, then falls back to a local in-process search runtime transparently. Search and context commands also auto-start the daemon if a project ID exists.
- **Agent instruction automation**: `start` auto-installs versioned, fenced agent reminder blocks into target files (e.g., CLAUDE.md). Fences use HTML comment markers (`<!-- 1up:start:VERSION -->`) for idempotent create/update/skip lifecycle. The `hello-agent` command outputs the condensed reminder directly for on-demand agent onboarding.
- **Non-intrusive update awareness**: Every non-JSON, non-Worker, non-Update command spawns a background cache refresh and shows a passive update notification on stderr after the primary command completes (with a 2-second timeout). This keeps users informed without interrupting workflows. The `update` command handles its own output, and JSON consumers are never polluted with update notices.

## Actors & Surfaces

| Actor | Goals | Surface |
|-------|-------|---------|
| Developer | Semantic/keyword search, symbol lookup, context retrieval, structural queries, indexing, checking for and applying updates | CLI (human format) |
| AI agent / host tool | Code exploration via ranked results, receiving condensed operational instructions, consuming machine-friendly output | CLI (plain/json), fenced instruction blocks in CLAUDE.md |
| Script / automation | Consume structured data programmatically, automate lifecycle | CLI (json/plain formats) |
| Background daemon | Watch files, incrementally re-index, serve search requests via Unix socket, persist progress, report daemon version to CLI | Daemon process (surfaced via `status`) |

## CLI Entry Points

| Command | Purpose | Key Flags |
|---------|---------|-----------|
| `1up init [PATH]` | Create `.1up/project_id` | |
| `1up start [PATH]` | Auto-init + install fences + index + daemon start/refresh | `--jobs`, `--embed-threads` |
| `1up stop [PATH]` | Deregister project, SIGTERM if last | |
| `1up status [PATH]` | Daemon state, project initialization, index health, file-watcher heartbeat, counts, indexing progress | |
| `1up search <QUERY>` | Hybrid semantic + FTS search (daemon-first, local fallback); version mismatch warning | `--limit/-n`, `--path` |
| `1up symbol <NAME>` | Symbol definition/reference lookup | `--references/-r` |
| `1up context <LOCATION>` | Enclosing scope context retrieval with access scope tracking | `--expansion`, `--allow-outside-root` |
| `1up structural <PATTERN>` | Tree-sitter S-expression queries | `--language/-l` |
| `1up index [PATH]` | Incremental index | `--jobs`, `--embed-threads` |
| `1up reindex [PATH]` | Full rebuild from scratch | `--jobs`, `--embed-threads` |
| `1up hello-agent` | Output condensed agent instruction reminder | |
| `1up update` | Refresh cache if stale, then self-update or show channel-specific upgrade instructions | `--check` (force fetch), `--status` (cached info) |

Global flags: `--format (plain|json|human)` (default: plain), `--verbose (-v/-vv)`

## User-Visible States

| State | Meaning | Human Display |
|-------|---------|---------------|
| IndexState::Idle | No indexing in progress | Dimmed "idle" |
| IndexState::Running | Indexing pass active | Yellow "running" |
| IndexState::Complete | Last pass finished successfully | Green "complete" |
| IndexPhase (Pending/Scanning/Parsing/Storing/Complete) | Current stage within a pass; Storing shows as "embedding & storing" when embeddings enabled | Cyan phase label |
| Daemon running/stopped | Background watcher active or not | Green "running" / red "stopped" |
| Project initialized / not initialized | Whether `.1up/project_id` exists | Project ID or yellow "not initialized" |
| Index health: ready / not_built / unavailable | Index database exists and is readable with current schema | Green "ready" / yellow "not built" / red "unavailable" |
| Last file check timestamp | When daemon last checked for file changes from `.1up/daemon_status.json` | Relative "time ago" with ISO-8601 or yellow "never recorded" |
| Embeddings enabled/disabled | Semantic vector search availability | Green "enabled" / yellow "disabled" |
| EmbeddingLoadStatus | Granular model readiness: Warm, Loaded, Downloaded, Unavailable(reason) | Spinner resolves to success or warn with specific message |
| ContextAccessScope | Whether context target is within project root or outside it | Dimmed "[outside_root]" label after scope type |
| FenceAction | Outcome of agent instruction fence installation | stderr: "Created..." / "Updated..." / silent for AlreadyCurrent |
| UpdateStatus::UpToDate | Current version matches or exceeds latest | Green "up to date" |
| UpdateStatus::UpdateAvailable | Newer version exists | Yellow "update available ({version})" with upgrade instruction |
| UpdateStatus::Yanked | Current version recalled | Red "YANKED -- upgrade immediately" with message |
| UpdateStatus::BelowMinimumSafe | Below operator-set version floor | Red "below minimum safe version ({version})" with message |
| UpdateResult::Updated | Self-update completed | "Updated 1up from X to Y" with green bold new version |
| UpdateResult::ChannelManaged | Update available but managed by package manager | "managed by {channel}. Run: {instruction}" |
| InstallChannel | How 1up was installed (Manual/Homebrew/Scoop/Unknown) | Shown as "Install source: {channel}" in update status |

## Feedback Loops

### Indexing Progress
- **Trigger**: `index`, `reindex`, or `start`
- **Spinner**: Phases shown on stderr ("Preparing database", "Loading embedding model") when TTY detected; resolves to success/warn/update
- **Completion summary**: Work counters (files indexed/skipped/deleted, segments stored), effective parallelism (workers, embed threads), per-stage timings (scan/parse/embed/store/total in ms), embedding availability

### Status Dashboard
- **Trigger**: `1up status`
- **Output**: Daemon state with PID, project initialization status, index health (ready/not_built/unavailable), last file check timestamp with relative "time ago" from daemon_status.json, indexed file/segment counts, full IndexProgress snapshot (state, phase, work, parallelism, timings, embeddings). Updated timestamp shown with relative "time ago" alongside ISO-8601.

### Daemon Lifecycle Notification
- **Trigger**: `start` when daemon already running
- **Output**: Reports daemon notified via SIGHUP. Re-registration goes to stderr; new-project registration goes to stdout.

### Embedding Model Status
- **Trigger**: Model load during index, reindex, search, or start
- **Output**: Granular feedback per EmbeddingLoadStatus variant: Warm/Loaded are silent success; Downloaded gets info message; Unavailable variants produce context-specific warnings with actionable hints.

### Search Results
- **Human mode**: Bold kind labels, cyan file:line locations, dimmed metadata (scope, symbols, complexity, score), content preview truncated to 12 lines with "..." indicator
- **Empty results**: "No results found." or "No symbols found."

### Search Daemon Fallback
- **Trigger**: Daemon socket unavailable or times out
- **Output**: Transparent fallback to local in-process runtime. All fallback paths are logged at debug level only; user sees no difference in output.

### Version Mismatch Warning
- **Trigger**: Search via daemon when CLI version differs from daemon version
- **Output**: Warning on stderr: "CLI version ({VERSION}) differs from daemon version ({dv}). Run `1up stop` and re-run your command to restart the daemon under the current binary." Suppressed in JSON output format.

### Agent Instruction Fence Installation
- **Trigger**: `1up start` (runs before indexing)
- **Output**: For each target file: Created -> stderr with filename and version; Updated -> stderr with old->new version; AlreadyCurrent -> silent. Write failures produce warnings but do not abort start.

### Context Access Scope
- **Trigger**: `1up context` with path outside project root
- **Output**: Absolute paths without `--allow-outside-root` produce an error with actionable message. When flag is granted, output includes ContextAccessScope label.

### Update Check
- **Trigger**: `1up update --check`
- **Output**: Forces a fresh manifest fetch, writes result to cache, displays UpdateStatusInfo with current/latest versions, install channel, update status (colored by severity), cache age, and upgrade instructions.

### Update Status Display
- **Trigger**: `1up update --status`
- **Output**: Displays cached update information without network access. Shows cache age. When updates are disabled, shows "Updates are disabled for this build." When no cache exists, prompts "Run `1up update --check`."

### Self-Update
- **Trigger**: `1up update` (no flags)
- **Output**: Refreshes cache if stale, then branches by install channel: Manual installs stop daemon ("Stopping daemon..."), download and replace binary, report "Updated 1up from X to Y". Homebrew/Scoop show channel-specific upgrade instructions. Unknown directs to GitHub releases. Already up-to-date shows "Already up to date (version X)."

### Passive Update Notification
- **Trigger**: Any CLI command except `update`, `__worker`, and any command using JSON output format
- **Output**: Background async cache refresh spawned at command start. After primary command completes, notification displayed on stderr with 2-second timeout. Entirely non-blocking: if refresh does not complete in time, it is silently abandoned.

## Output Routing

- **Stdout**: Final results, new-project registration notifications
- **Stderr**: Spinners, diagnostic messages, warnings, re-registration messages, update notifications, version mismatch warnings, tracing logs
- **Rationale**: Clean separation allows scripts to capture structured output via stdout while progress flows to stderr

## Output Format Semantics

| Mode | Encoding | Use Case |
|------|----------|----------|
| Plain | Tab-separated `key:value` pairs, no ANSI (default) | Agent consumption, simple text processing (grep, awk) |
| Human | ANSI colors (bold, cyan, dimmed, green, yellow, red), multiline labeled sections | Interactive terminal |
| JSON | Structured objects with nested progress/work/parallelism/timings | Programmatic consumption |

All three formats carry the same information density for index summaries, status, and update output.

## Accessibility

- **TTY-aware spinners**: Only animate when stderr is a TTY; prevents garbled output in pipes, CI, editors
- **Plain output mode**: Strips all ANSI codes for assistive tools and text processors; is the default format
- **Content truncation**: Search results truncated to 12 lines with visible "..." overflow indicator in human mode; plain mode emits full content
- **Stderr-only logging and notifications**: Tracing output, update notifications, and version mismatch warnings always routed to stderr so stdout remains clean for structured output; verbosity: 0=error, 1=warn, 2=info+debug, 3+=trace
- **Relative timestamps**: Status and cache-age timestamps shown as human-friendly durations ("5m ago") alongside absolute ISO-8601 for both human comprehension and machine parsing

## Cross-References
- **Architecture**: See [architecture.md](architecture.md) for system topology and data flows
- **Modules**: See [modules.md](modules.md) for component breakdown
- **Patterns**: See [patterns.md](patterns.md) for implementation conventions
