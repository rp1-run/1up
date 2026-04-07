# System Architecture

**Project**: 1up
**Architecture Pattern**: Layered + Two-Process Model
**Last Updated**: 2026-04-07

## High-Level Architecture

```mermaid
graph TB
    User[User] -->|invokes| CLI[CLI Layer<br/>clap subcommands]
    CLI -->|resolve config| Config[Indexing Config<br/>flags -> env -> registry -> auto]
    CLI -->|start/stop| Daemon[Daemon Worker<br/>background process]
    CLI -->|index/reindex| Pipeline[Staged Indexing Orchestrator<br/>pipeline::run_with_config]
    CLI -->|search fallback| Search[Search Engine]
    CLI -->|search request| SearchSocket[Daemon Search Socket<br/>JSON over UnixStream]

    Daemon -->|file events| Watcher[File Watcher<br/>notify crate]
    Daemon -->|re-index| Pipeline
    Daemon -->|serve query requests| SearchService[Search Service<br/>SearchRequest/SearchResponse]
    Daemon -->|SIGHUP/SIGTERM| Lifecycle[Lifecycle Manager<br/>PID file + signals]
    Daemon -->|load/reload| Registry[Project Registry<br/>projects.json + indexing config]
    Daemon -->|schedule runs| RunState[Per-Project Run State<br/>one active run + queued follow-up]
    SearchSocket --> SearchService

    Config --> Pipeline
    Registry --> Config
    RunState --> Pipeline

    Pipeline -->|scope plan + scan| Scanner[Scanner<br/>ignore crate]
    Pipeline -->|bounded parse| Parser[Parse Worker Pool<br/>spawn_blocking x jobs]
    Parser -->|ordered flush| Reorder[Deterministic Reorder Buffer]
    Reorder -->|fallback chunking| Chunker[Text Chunker]
    Reorder -->|embed batches| Embedder[Single Embed Session<br/>ONNX Runtime]
    Embedder -->|single writer| Writer[Transactional Writer<br/>batch replace]
    Writer --> DB[(libSQL<br/>.1up/index.db)]

    SearchService --> Search
    Search -->|vector search| DB
    Search -->|FTS5 match| DB
    Search -->|hydrate ranked ids| DB
    Search -->|embed query| Embedder

    Scanner -->|walks| FS[Project Files]
    Watcher -->|monitors| FS

    style CLI fill:#4A90D9,color:#fff
    style Daemon fill:#D94A4A,color:#fff
    style DB fill:#50C878,color:#fff
    style Embedder fill:#FFB347,color:#fff
```

CLI and daemon runs converge on the same `IndexingConfig` resolution path before entering the
indexing pipeline. Search now has two entry points: the CLI can search locally, or it can send a
JSON request to the daemon over a Unix domain socket so repeated searches reuse the daemon's warm
embedding runtime. Only file-local parse work fans out; embeddings stay in one ONNX session and
all database mutation flows through a single transactional writer so replacement semantics remain
deterministic.

## Architectural Patterns

### Two-Process Model
CLI process (per-invocation) and detached daemon worker (background) share no runtime state. Communication is exclusively through the libSQL database, PID file, project registry (JSON), and Unix signals (SIGHUP/SIGTERM).

### Layered Architecture
Clear separation: CLI (presentation) -> Indexer/Search (processing) -> Storage (persistence), with daemon as a parallel entry point into the same pipeline.

### Staged Single-Writer Pipeline
Indexing is split into scan, delete cleanup, bounded parse, embed, and write phases. Parse workers run concurrently, but completed files are reordered by sequence ID before any storage mutation so one writer owns all segment replacement work. Write batch size is configurable via `write_batch_files` to tune transaction granularity.

### Scoped Incremental Scheduling
Daemon watcher events accumulate into `RunScope::Paths` so follow-up runs scan only changed files
and known deletions. When a changed path can alter ignore semantics or cannot be reconciled
precisely, the pipeline falls back to a full scan instead of risking stale state.

### Incremental Processing
SHA-256 file hashing in pipeline; skip if hash unchanged; deleted file detection via set difference.

### Exact-First Symbol Index
Parsed definition and usage symbols are persisted into `segment_symbols` with normalized
`canonical_symbol` values. Search checks exact canonical matches first, then widens into
prefix/contains candidate loads and fuzzy matching only when needed.

### Candidate-First Search Hydration
Hybrid search ranks lightweight candidate rows from vector, FTS, and symbol retrieval before
hydrating final segment bodies by ID. This keeps the hot path focused on a small candidate set
instead of materializing every possible match up front.

### Warm Search Runtime Reuse
The daemon keeps one `EmbeddingRuntime` per project and can reuse it for both indexing and search
requests. When the model files and `embed_threads` value are unchanged, repeated searches stay on a
warm path instead of rebuilding the ONNX session.

### Graceful Degradation
Embedder is `Optional<&mut Embedder>`; missing model degrades to FTS-only; `SqlVectorV2` falls back to `FtsOnly` on failure.

### Schema-Gated Access
`schema::ensure_current()` validates version + required objects before any read/write; stale schemas require explicit `1up reindex`.

### Shared Config Resolution
Indexing settings (jobs, embed_threads, write_batch_files) resolve in one chain: CLI flags -> environment variables (`ONEUP_INDEX_JOBS`, `ONEUP_EMBED_THREADS`, `ONEUP_INDEX_WRITE_BATCH_FILES`) -> persisted registry config -> automatic defaults. Manual and daemon-triggered runs share the same concurrency model.

### Transient Failure Retry
Database lock contention is handled with bounded retries (10 attempts, 50ms delay) rather than failing immediately, supporting concurrent CLI and daemon access to the same database.

## Layer Details

| Layer | Purpose | Key Files |
|-------|---------|-----------|
| CLI | User-facing command parsing and output formatting | `src/main.rs`, `src/cli/` |
| Daemon | Background file watching, registry management, auto re-indexing | `src/daemon/` |
| Indexer | File scanning, parsing, chunking, embedding, pipeline orchestration | `src/indexer/` |
| Search | Query execution, intent detection, RRF fusion, result ranking | `src/search/` |
| Storage | Database lifecycle, schema management, segment CRUD, queries | `src/storage/` |
| Shared | Cross-cutting: config paths, constants, error types, data types | `src/shared/` |

## Data Flows

### Indexing Pipeline
```
Resolve indexing config (CLI flags -> env vars -> registry -> auto defaults)
  -> Resolve scope: full scan or changed-path follow-up
  -> For scoped runs, scan only requested paths plus indexed deletions; fall back to a full scan if ignore semantics changed
  -> Initialize progress snapshot
  -> Delete segments for removed files before new work begins
  -> Dispatch changed files to bounded spawn_blocking parse pool with sequence IDs
  -> Reorder completed parse results to preserve deterministic file ordering
  -> Generate embeddings in batches through one ONNX session when available
  -> Replace file segments through single-writer transactional batch helpers (`write_batch_files` adapts to run size)
  -> Persist final progress with work counters, parallelism, and stage timings to .1up/index_status.json
```

### Search Query
```
CLI canonicalizes --path and auto-starts the daemon when the project is already initialized
  -> Send SearchRequest { project_root, query, limit } over ~/.local/share/1up/daemon.sock (250ms timeout)
  -> Daemon validates registry entry + schema, then reuses or loads a warm EmbeddingRuntime
  -> Detect intent (DEFINITION, FLOW, USAGE, DOCS, GENERAL)
  -> Build symbol variants and run exact-first canonical symbol lookup
  -> Fetch vector and FTS candidate rows concurrently when embeddings are available
  -> Rank candidate IDs with RRF + intent/query/path/content boosts + per-file caps
  -> Hydrate only the final ranked segment IDs from storage
  -> Return SearchResponse::Results; CLI falls back to the same local search stack if daemon search is unavailable
```

### Daemon File Watch Loop
```
Worker loads project registry and persisted indexing settings, then watches directories
  -> tokio::select! multiplexes: SIGHUP (reload), SIGTERM (shutdown), timer (drain events)
  -> Drain + filter changed paths and mark each owning project dirty with RunScope::Paths
  -> Ambiguous or unscoped watcher events escalate that project to RunScope::Full
  -> If project idle: start one indexing run with resolved config and current scope
  -> If changes arrive during a run: accumulate them and queue one follow-up pass
  -> After each run: rerun once if still dirty, otherwise return to idle
  -> On SIGHUP: reload registry, add/remove watchers, refresh indexing settings
  -> On SIGTERM: unwatch all, clean up PID file, exit
```

### Daemon Lifecycle
```
CLI `start` or auto-start registers project in projects.json with optional indexing settings
  -> If worker already runs: send SIGHUP so it reloads project list and settings
  -> Else spawn detached `1up __worker` child process (setsid for session leader)
  -> Worker writes PID file, enters event loop
  -> CLI `stop` deregisters project; sends SIGTERM if no projects remain, SIGHUP otherwise
  -> Stale PID files detected and cleaned on next startup
```

## Integration Points

| Integration | Purpose | Type |
|-------------|---------|------|
| libSQL (Turso) | Segment storage, FTS5 search, native vector search with 384-dim embeddings | Embedded database |
| ONNX Runtime (ort) | Local ML inference for 384-dim sentence embeddings (all-MiniLM-L6-v2) | Embedded inference |
| Tree-sitter | Multi-language AST parsing (16 language grammars compiled in) | Compiled-in library |
| hyperfine | Parallel indexing performance benchmarking (serial vs auto vs constrained) | Dev tooling |

## State Management

- **PID file**: `~/.local/share/1up/daemon.pid`
- **Daemon search socket**: `~/.local/share/1up/daemon.sock`
- **Project registry**: `~/.local/share/1up/projects.json` (includes per-project IndexingConfig)
- **Per-project DB**: `<project>/.1up/index.db`
- **Index progress**: `<project>/.1up/index_status.json` (IndexProgress with parallelism + stage timings)
- **Model cache**: `~/.local/share/1up/models/`
- **In-memory daemon state**: per-project `ProjectRunState` (`running`, `dirty`, `pending_scope`) plus a warm `EmbeddingRuntime`

## Deployment

- **Type**: Single binary CLI with background daemon
- **Environment**: Local developer machine (macOS/Linux)
- **Distribution**: `cargo build --release`, installed to `~/.local/bin/1up` with codesign on macOS
- **Installation**: `just install` (builds release, copies to `~/.local/bin`, codesigns)
- **Dev tooling**: `just bench-parallel` for indexing benchmarks, `scripts/benchmark_emdash.sh` for comparative warm-query benchmarks
