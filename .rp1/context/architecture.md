# System Architecture

**Project**: 1up
**Architecture Pattern**: Layered + Two-Process Model
**Last Updated**: 2026-04-13

## High-Level Architecture

```mermaid
graph TB
    User[User] -->|invokes| CLI[CLI Layer<br/>clap subcommands]
    CLI -->|resolve config| Config[Indexing Config<br/>flags -> env -> registry -> auto]
    CLI -->|start/stop| Daemon[Daemon Worker<br/>background process]
    CLI -->|index/reindex| Pipeline[Staged Indexing Orchestrator<br/>pipeline::run_with_config]
    CLI -->|search fallback| Search[Search Engine]
    CLI -->|search request| SearchSocket[Daemon Search Socket<br/>Framed JSON over UnixStream]
    CLI -->|impact| Impact[Impact Horizon Engine<br/>local advisory expansion]
    CLI -->|hello-agent| Reminder[Agent Reminder<br/>fenced versioned blocks]
    CLI -->|1up update| UpdateCLI[Update Command<br/>check / status / self-update]

    UpdateCLI -->|fetch manifest| UpdateManifest[Update Manifest<br/>raw.githubusercontent.com]
    UpdateCLI -->|read/write cache| UpdateCache[Update Cache<br/>update-check.json 24h TTL]
    UpdateCLI -->|download + verify| SelfUpdate[Self-Update<br/>SHA256-verified binary replace]
    CLI -.->|async startup| UpdateCache
    CLI -.->|passive notification| User

    Daemon -->|file events| Watcher[File Watcher<br/>notify crate]
    Daemon -->|re-index| Pipeline
    Daemon -->|serve query requests| SearchService[Search Service<br/>Framed SearchRequest/SearchResponse]
    Daemon -->|SIGHUP/SIGTERM| Lifecycle[Lifecycle Manager<br/>PID file + signals]
    Daemon -->|load/reload| Registry[Project Registry<br/>projects.json + indexing config]
    Daemon -->|schedule runs| RunState[Per-Project Run State<br/>one active run + queued follow-up]
    Daemon -->|heartbeat| DaemonStatus[Daemon Status<br/>daemon_status.json per project]
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
    Writer --> DB[(libSQL<br/>.1up/index.db<br/>segments + segment_relations)]

    SearchService --> Search
    Search -->|vector search| DB
    Search -->|FTS5 match| DB
    Search -->|hydrate ranked ids| DB
    Search -->|embed query| Embedder
    Impact -->|seed lookup + relation expansion| DB

    Scanner -->|walks| FS[Project Files]
    Watcher -->|monitors| FS

    Embedder -->|verified artifacts| ModelCache[Model Cache<br/>~/.local/share/1up/models/]
    Lifecycle -->|approved-root guards| FSHelpers[Secure FS Helpers<br/>shared/fs.rs]
    Registry -->|approved-root guards| FSHelpers
    Writer -->|approved-root guards| FSHelpers
    UpdateCache -->|approved-root guards| FSHelpers

    style CLI fill:#4A90D9,color:#fff
    style Daemon fill:#D94A4A,color:#fff
    style DB fill:#50C878,color:#fff
    style Embedder fill:#FFB347,color:#fff
    style FSHelpers fill:#9B59B6,color:#fff
    style UpdateCLI fill:#2ECC71,color:#fff
    style UpdateCache fill:#2ECC71,color:#fff
```

CLI and daemon runs converge on the same `IndexingConfig` resolution path before entering the
indexing pipeline. Search now has two entry points: the CLI can search locally, or it can send a
framed JSON request to the daemon over a Unix domain socket so repeated searches reuse the
daemon's warm embedding runtime. The daemon only serves same-UID peers, bounds frame sizes and
per-frame deadlines, and sheds excess load with safe fallback responses. Only file-local parse
work fans out; embeddings stay in one ONNX session and all database mutation flows through a
single transactional writer so replacement semantics remain deterministic. The update subsystem
runs independently: a background cache refresh on CLI startup and a passive notification after
command completion keep users informed without interrupting workflows. Impact Horizon is
intentionally separate: the CLI opens the same schema-validated index read-only, resolves exact
file, symbol, or `segment_id` anchors, expands through persisted `segment_relations` plus bounded
same-file and test heuristics, and returns advisory results without changing the daemon search
path.

## Architectural Patterns

### Two-Process Model
Short-lived CLI commands and a long-lived daemon worker share no in-process state. Communication is exclusively through the libSQL database, PID file, project registry (JSON), Unix domain socket IPC, and Unix signals (SIGHUP/SIGTERM).

### Layered Architecture
Presentation (CLI), processing (Indexer/Search), persistence (Storage), and cross-cutting concerns (Shared) form distinct layers with one-directional dependencies. Daemon acts as a parallel entry point into the same pipeline.

### Staged Single-Writer Pipeline
Indexing is split into scan, delete cleanup, bounded parse, embed, and write phases. Parse workers run concurrently, but completed files are reordered by sequence ID before any storage mutation so one writer owns all segment replacement work. Write batch size is configurable via `write_batch_files` to tune transaction granularity.

### Scoped Incremental Scheduling
Daemon watcher events accumulate into `RunScope::Paths` so follow-up runs scan only changed files and known deletions. When a changed path can alter ignore semantics or cannot be reconciled precisely, the pipeline falls back to a full scan. `ProjectRunState` collapses change bursts via `mark_dirty` + `pending_scope` merge, ensuring at most one active run and one queued follow-up per project.

### Incremental Processing
SHA-256 file hashing in pipeline; skip if hash unchanged; deleted file detection via set difference.

### Exact-First Symbol Index
Parsed definition and usage symbols are persisted into `segment_symbols` with normalized `canonical_symbol` values. Search checks exact canonical matches first, then widens into prefix/contains candidate loads and fuzzy matching only when needed.

### Candidate-First Search Hydration
Search ranks lightweight candidate IDs across three retrieval channels (vector, FTS, symbol) before hydrating only the top-ranked segments, avoiding expensive full-row reads for discarded results.

### Local-Only Impact Read Path
`1up impact` bypasses daemon IPC in v1 and reads the index directly from the CLI. This keeps
likely-impact exploration explicitly opt-in, avoids transport and warm-runtime changes, and
preserves the existing daemon-backed `search` path.

### Relation-Backed Advisory Expansion
The storage layer persists unresolved outbound call/reference rows in `segment_relations`. Impact
Horizon resolves those relations against current segment definitions at query time, applies hop
limits plus same-file and test heuristics, and emits bounded advisory rankings rather than exact
dependency claims.

### Warm Search Runtime Reuse
The daemon maintains a per-project `EmbeddingRuntime` that stays warm across indexing and search requests. `EmbeddingLoadStatus::Warm` indicates the ONNX session was reused without rebuilding when model fingerprint and thread count are unchanged.

### Graceful Degradation
Every optional component (embeddings, daemon socket, model download, update check) has a defined fallback path so the system continues operating at reduced capability rather than failing. Embedding failures degrade to FTS-only; daemon unavailability triggers local search; update check failures preserve stale cache or silently skip.

### Schema-Gated Access
`schema::ensure_current()` validates schema version plus the required tables, indexes, triggers, and the `embedding_vec` column before any read/write; stale schemas require explicit `1up reindex`. `check_embedding_model_compatible` validates model provenance before mixing embeddings.

### Shared Config Resolution
Indexing settings (jobs, embed_threads, write_batch_files) resolve in one chain: CLI flags -> environment variables -> persisted registry config -> automatic defaults. Manual and daemon-triggered runs share the same concurrency model.

### Transient Failure Retry
Database lock contention is handled with bounded retries (10 attempts, 50ms delay) rather than failing immediately, supporting concurrent CLI and daemon access to the same database.

### Approved-Root Filesystem Guards
`shared/fs.rs` implements secure filesystem operations: all operations reject symlink components, enforce root clamping, and set owner-only permissions (0o700 dirs, 0o600 files/sockets). This prevents symlink-escape and TOCTOU attacks on daemon state.

### Framed IPC with Load Shedding
Daemon search uses length-prefixed JSON over a same-UID-authenticated Unix socket with bounded sizes (16 KiB request, 4 KiB query, 2 MiB response), 250ms deadlines, and semaphore-based load shedding (8 concurrent slots).

### Platform-Conditional Compilation
`daemon/mod.rs` uses `#[cfg(unix)]` and `#[cfg(not(unix))]` to swap between full implementations and stub modules that return unsupported-platform errors. ONNX Runtime links statically on Unix and loads dynamically on Windows. Self-update uses platform-conditional binary replacement (atomic rename on Unix, rename-aside-and-replace on Windows) and platform-conditional archive extraction (tar.gz on Unix, zip on Windows).

### Fenced Agent Reminders
`shared/reminder.rs` manages versioned fence markers in AGENTS.md and CLAUDE.md files. The `hello_agent` CLI subcommand outputs a condensed agent instruction compiled from `src/reminder.md`. `apply_fence` handles create, update, and idempotent no-op cases.

### Evidence-Based Release Pipeline
Releases follow an evidence-accumulation pipeline: CI merge gates -> release-please versioning -> cross-platform build (4 targets) -> draft release -> update-manifest generation -> package publication (Homebrew/Scoop) -> update-manifest commit to main with verification -> release evidence bundle with security, archive verification, eval, and benchmark attestations.

### Channel-Aware Self-Update System
Full self-update lifecycle with install channel detection (Homebrew/Scoop/Manual/Unknown), cached manifest checks (24h TTL), passive update notifications on every CLI invocation, and channel-appropriate upgrade paths. Homebrew/Scoop users receive upgrade instructions; manual installs get SHA256-verified binary download with atomic replacement. Daemon is stopped before self-update. Compile-time `ONEUP_UPDATE_MANIFEST_URL` bakes the update endpoint into release builds; runtime env override enables testing and operator canaries.

### Daemon Heartbeat
The daemon persists a `DaemonProjectStatus` with `last_file_check_at` to `<project>/.1up/daemon_status.json`, throttled to at most once per 30 seconds. Force-written on project load and SIGHUP reload. The CLI status command reads this file to report daemon liveness with relative timestamps, enabling external tools to detect a stalled daemon without PID-based probing.

## Layer Details

| Layer | Purpose | Key Files |
|-------|---------|-----------|
| CLI | User-facing command parsing, output formatting, agent reminder management, passive update notifications, and local-only Impact Horizon dispatch | `src/main.rs`, `src/cli/` |
| Daemon | Background file watching, framed search IPC, registry management, auto re-indexing, heartbeat persistence | `src/daemon/` |
| Indexer | File scanning, parsing, chunking, embedding, pipeline orchestration | `src/indexer/` |
| Search | Query execution, intent detection, RRF fusion, result ranking, and advisory impact expansion | `src/search/` |
| Storage | Database lifecycle, schema management, segment CRUD, relation persistence, and queries | `src/storage/` |
| Shared | Cross-cutting: config paths, secure filesystem helpers, constants, error types, data types, agent reminders, update system | `src/shared/` |

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
  -> Replace file segments through single-writer transactional batch helpers (write_batch_files adapts to run size)
  -> Persist final progress with work counters, parallelism, and stage timings to .1up/index_status.json
```

### Search Query
```
CLI canonicalizes --path and auto-starts the daemon when the project is already initialized
  -> Send one length-prefixed JSON SearchRequest frame over ~/.local/share/1up/daemon.sock (250ms client timeout)
  -> Daemon accepts only same-UID peers; unreadable or mismatched peer credentials get SearchResponse::Unavailable
  -> Daemon enforces a 16 KiB request cap, 4 KiB query cap, 2 MiB response cap, and 250ms read/write deadlines
  -> Requests enter an 8-slot semaphore-backed queue; saturation returns SearchResponse::Unavailable { reason: "daemon busy" }
  -> Daemon validates registry entry + schema, then reuses or loads a warm EmbeddingRuntime
  -> Detect intent (DEFINITION, FLOW, USAGE, DOCS, GENERAL)
  -> Build symbol variants and run exact-first canonical symbol lookup
  -> Fetch vector and FTS candidate rows concurrently when embeddings are available
  -> Rank candidate IDs with RRF + intent/query/path/content boosts + per-file caps
  -> Hydrate only the final ranked segment IDs from storage
  -> Expose additive `segment_id` follow-up handles in plain/json output when a result is backed by an indexed segment
  -> Return SearchResponse::Results with optional daemon_version field; CLI warns on version mismatch and falls back to local search if daemon search is unavailable, busy, rejected, or timed out
```

### Impact Horizon Query
```
CLI validates exactly one of --from-file, --from-symbol, or --from-segment
  -> Open the current index read-only and require schema v8 compatibility
  -> Resolve the anchor into a bounded seed set or refuse broad symbol requests with narrowing hints
  -> Traverse persisted `segment_relations` plus same-file and test heuristics within depth and per-file caps
  -> Apply hop decay, scope boost, and role boost to produce advisory candidate scores
  -> Return one structured envelope with status = expanded, expanded_scoped, or refused
  -> For search handoff loops, the same path accepts the additive `segment_id` emitted by plain/json search output
```

### Daemon File Watch Loop
```
Worker loads project registry and persisted indexing settings, binds search socket, then watches directories
  -> tokio::select! multiplexes: search connections, queued search requests, SIGHUP (reload), SIGTERM (shutdown), timer (drain events)
  -> On each timer tick: record daemon heartbeat to <project>/.1up/daemon_status.json (throttled to 30s intervals)
  -> Drain + filter changed paths and mark each owning project dirty with RunScope::Paths
  -> Ambiguous or unscoped watcher events escalate that project to RunScope::Full
  -> If project idle: start one indexing run with resolved config and current scope
  -> If changes arrive during a run: accumulate them and queue one follow-up pass
  -> After each run: rerun once if still dirty, otherwise return to idle
  -> On SIGHUP: reload registry, add/remove watchers, refresh indexing settings, force heartbeat persist
  -> On SIGTERM: unwatch all, clean up PID file and socket, exit
```

### Daemon Lifecycle
```
CLI start or auto-start validates approved roots, then registers the project in projects.json with optional indexing settings
  -> If worker already runs: send SIGHUP so it reloads project list and settings
  -> Else spawn detached 1up __worker child process (setsid for session leader)
  -> Worker writes daemon.pid under the secure XDG root, binds daemon.sock with owner-only permissions, and enters the event loop
  -> CLI stop deregisters project; sends SIGTERM if no projects remain, SIGHUP otherwise
  -> Stale PID files and stale sockets are cleaned only when the expected file type exists under the approved root
```

### Model Artifact Lifecycle
```
Download to .staging/<artifact_id>/ directory
  -> Verify SHA-256 digests against pinned constants for each artifact file
  -> Write VerifiedArtifactManifest to verified/<artifact_id>/manifest.json
  -> Move verified files from staging to verified/<artifact_id>/
  -> Atomically update current.json (ActiveArtifactPointer) to reference new artifact_id
  -> EmbeddingRuntime loads from verified path, caches by compatibility key
```

### Update Check
```
On CLI startup (non-JSON, non-Worker, non-Update commands): spawn async cache refresh task
  -> Read cached update-check.json from ~/.local/share/1up/; discard if current_version mismatches running binary
  -> If cache is valid (same version, within 24h TTL): return cached result
  -> Otherwise: fetch update-manifest.json from compiled-in ONEUP_UPDATE_MANIFEST_URL (3s connect, 5s request timeout)
  -> Build UpdateCheckCache from manifest with detected install channel and upgrade instruction
  -> Write cache atomically via approved-root filesystem helpers
  -> After CLI command completes: read cache and format passive notification if update available, yanked, or below minimum safe version
  -> On permanent HTTP errors (4xx except 408/429): invalidate cache
```

### Self-Update
```
User invokes 1up update; ensure updates are enabled (manifest URL configured)
  -> Refresh cache if stale; assess update status via semver comparison
  -> Detect install channel from resolved binary path (Homebrew Cellar path / Scoop apps path / Manual)
  -> If Homebrew or Scoop: print channel-specific upgrade instruction and exit
  -> If Unknown: print manual download link and exit
  -> If Manual: stop daemon (SIGTERM with 3s poll timeout), then proceed
  -> Fetch update manifest; find artifact matching current platform triple
  -> Download release archive to tempdir adjacent to current binary (10s connect, 300s request timeout)
  -> Verify SHA-256 checksum of downloaded archive against manifest
  -> Extract 1up binary from archive (tar.gz on Unix, zip on Windows)
  -> Replace running binary atomically (rename on Unix; rename-aside-and-replace on Windows)
  -> Report old and new versions
```

### Release Pipeline
```
Conventional-commit PR titles enforced by pr-title.yml
  -> CI merge gates: security-check (cargo-audit), release-smoke on macOS/Linux/Windows, release-consistency
  -> release-please creates/maintains rolling release PR with version bump and changelog
  -> On release PR merge: build 4 targets with ONEUP_UPDATE_MANIFEST_URL baked in, generate SHA256SUMS and release-manifest.json, create draft GitHub release
  -> On release publish: render and push Homebrew formula and Scoop manifest to tap/bucket repos
  -> Generate update-manifest.json from release-manifest.json, commit to main branch
  -> Verify stable update-manifest.json is accessible at raw.githubusercontent.com URL
  -> release-evidence.yml verifies archives on native runners, assembles release-evidence.json bundle
```

## Integration Points

| Integration | Purpose | Type |
|-------------|---------|------|
| libSQL (Turso) | Segment storage, FTS5 search, native vector search with 384-dim embeddings | Embedded database |
| ONNX Runtime (ort) | Local ML inference for 384-dim sentence embeddings (all-MiniLM-L6-v2) with verified artifact activation | Embedded inference |
| Tree-sitter | Multi-language AST parsing (16 language grammars compiled in) | Compiled-in library |
| GitHub Actions CI/CD | Merge gates, release automation, cross-platform builds, evidence generation, package publication, update manifest publication | Workflow automation |
| Homebrew / Scoop | Package manager distribution channels for macOS/Linux and Windows | Package distribution |
| release-please | Automated semantic versioning and changelog from conventional commits | Release automation |
| reqwest | HTTP client for model downloads, update manifest fetches, and release archive downloads | HTTP library |
| promptfoo | Search quality evaluation framework with TypeScript assertion suites | Dev tooling |
| cargo-audit | Supply chain security scanning with repo-local advisory policy | Security tooling |
| hyperfine | Parallel indexing performance benchmarking | Dev tooling |
| lefthook | Git hooks manager for pre-push guards (main branch protection, rustfmt check) | Dev tooling |

## State Management

- **Secure roots**: `~/.local/share/1up/` and each `<project>/.1up/` directory are created with
  owner-only `0700` permissions by shared filesystem helpers before any state mutation.
- **PID file**: `~/.local/share/1up/daemon.pid` is written with atomic replace and `0600`
  permissions.
- **Daemon search socket**: `~/.local/share/1up/daemon.sock` is bound with `0600` permissions and
  cleaned up only if the path resolves to a socket under the approved root.
- **Project registry**: `~/.local/share/1up/projects.json` stores canonicalized project roots and
  optional per-project `IndexingConfig`; writes reject symlinked and outside-root paths.
- **Update cache**: `~/.local/share/1up/update-check.json` stores the latest update check result
  with 24h TTL, version-pinned to the running binary. Written atomically with secure permissions.
- **Per-project state**: `<project>/.1up/project_id`, `<project>/.1up/index.db`, and
  `<project>/.1up/index_status.json` stay inside the canonical project root and inherit the same
  approved-root checks. `index.db` now carries the `segment_relations` table in schema v8, so
  stale pre-v8 indexes require `1up reindex`.
- **Daemon heartbeat**: `<project>/.1up/daemon_status.json` stores `last_file_check_at` timestamp
  updated every 30 seconds by the daemon worker. Read by the status command for liveness display.
- **Model cache**: `~/.local/share/1up/models/all-MiniLM-L6-v2/` contains `current.json`, a
  `.download_failed` retry marker, `verified/<artifact-id>/` directories with `manifest.json`,
  `model.onnx`, and `tokenizer.json`, plus a transient `.staging/<artifact-id>/` tree used before
  activation.
- **In-memory daemon state**: per-project `ProjectRunState` (`running`, `dirty`, `pending_scope`)
  plus a warm `EmbeddingRuntime` and `last_file_check_persisted_at` throttle timestamp

## Deployment

- **Type**: Single binary CLI with background daemon and built-in self-update
- **Environment**: Local developer machine (macOS, Linux, Windows)
- **Distribution**: GitHub Releases with semantic versioning; Homebrew tap (`rp1-run/homebrew-tap`); Scoop bucket (`rp1-run/scoop-bucket`); 4 target triples: aarch64-apple-darwin, x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu, x86_64-pc-windows-msvc
- **Installation**: `brew install rp1-run/tap/1up` | `scoop install 1up` | GitHub Releases | `just install` (local dev build with codesign on macOS)
- **Self-update**: Built-in via `1up update` with SHA256-verified binary replacement for manual installs; package-managed installs receive channel-specific upgrade instructions; passive update notifications on every CLI invocation with 24h-cached manifest checks
- **License**: Apache-2.0
