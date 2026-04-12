# Domain Concepts & Terminology

**Project**: 1up
**Domain**: Code Search & Indexing

## Core Domain Concepts

### Segment
**Definition**: The fundamental indexing unit - a parsed block of source code (function, struct, chunk) with metadata including role, symbols, complexity, and embedding vector.
**Implementation**: `src/shared/types.rs`, `src/storage/segments.rs`
**Variants**:
- **ParsedSegment**: In-memory representation produced by parser/chunker with content, line range, language, role, complexity, and symbol references
- **StoredSegment**: Database-persisted row with all metadata columns including file hash for incremental indexing
- **SegmentRole**: Classification enum - Definition, Implementation, Orchestration, Import, or Docs

### SearchResult
**Definition**: A ranked code search result with file path, content, fused score, and optional symbol/role metadata.
**Implementation**: `src/shared/types.rs`
**Specialized Variants**:
- **SymbolResult**: Symbol lookup result distinguishing definitions from usages, with breadcrumb navigation context
- **StructuralResult**: AST pattern matching result with optional pattern name
- **ContextResult**: Context retrieval result including the enclosing scope (function, class, etc.) and ContextAccessScope

### ContextAccessScope
**Definition**: Distinguishes whether a context retrieval result comes from within the project root (ProjectRoot) or outside it (OutsideRoot), enabling security-aware scope reporting.
**Implementation**: `src/shared/types.rs`, `src/search/context.rs`

### Project
**Definition**: A registered codebase identified by a UUID stored in `.1up/project_id`, with a local index database at `.1up/index.db`. Project state files are created via atomic_replace with secure filesystem operations.
**Implementation**: `src/shared/project.rs`, `src/shared/config.rs`

### QueryIntent
**Definition**: Detected search intent from natural language queries: Definition, Flow, Usage, Docs, or General. Used to apply multiplicative score boosts to matching result types.
**Implementation**: `src/search/intent.rs`

### Embedder
**Definition**: ONNX-backed embedding engine using all-MiniLM-L6-v2 model, producing 384-dimensional L2-normalized vectors via mean pooling. Supports configurable intra-op thread count via `from_dir_with_threads`.
**Implementation**: `src/indexer/embedder.rs`

### EmbeddingRuntime
**Definition**: Warm-cache wrapper around Embedder that avoids redundant model loads. Uses EmbeddingCompatibilityKey (model dir + file fingerprints + thread count) to detect when the cached runtime can be reused. Provides separate paths for indexing (may download model) and search (never downloads).
**Implementation**: `src/indexer/embedder.rs`

### EmbeddingLoadStatus
**Definition**: Result of an EmbeddingRuntime preparation attempt: Warm (reused cached runtime), Loaded (fresh load from disk), Downloaded (fetched from HuggingFace then loaded), or Unavailable(reason). Drives degraded-mode decisions in both indexer and daemon search.
**Implementation**: `src/indexer/embedder.rs`

### VerifiedArtifactManifest
**Definition**: Manifest for a verified model artifact set, containing artifact_id, schema version, and per-file SHA-256 digests. Stored under `models/all-MiniLM-L6-v2/verified/<artifact_id>/manifest.json`. Enables staged download-verify-activate pipeline.
**Implementation**: `src/indexer/embedder.rs`

### ActiveArtifactPointer
**Definition**: JSON pointer file (`current.json`) that records which verified artifact set is the active model. Written atomically after successful download and verification.
**Implementation**: `src/indexer/embedder.rs`

### IndexingConfig
**Definition**: Resolved parallelism settings for an indexing run: `jobs` (parse worker count), `embed_threads` (ONNX intra-op threads), and `write_batch_files` (storage transaction batch size). Resolved via a priority chain: CLI flags > env vars > registry persisted values > auto defaults.
**Implementation**: `src/shared/types.rs`, `src/shared/config.rs`

### IndexProgress
**Definition**: Persisted indexing status for a project, tracking state (Idle/Running/Complete), phase (Pending/Scanning/Parsing/Storing/Complete), file counters, parallelism snapshot, and stage timings. Written to `.1up/index_status.json`.
**Implementation**: `src/shared/types.rs`, `src/indexer/pipeline.rs`

### RunScope
**Definition**: Scope for an indexing run: Full (entire project) or Paths(BTreeSet<PathBuf>) for targeted re-indexing of specific changed files. Supports merge semantics where Paths+Paths unions and Paths+Full escalates to Full.
**Implementation**: `src/shared/types.rs`, `src/daemon/worker.rs`

### ProjectRunState
**Definition**: Daemon-internal per-project run state tracking: one active run + one queued follow-up. Collapses burst file changes into a single dirty flag with accumulated RunScope. mark_dirty merges scopes; start_run consumes the pending scope; finish_run clears running flag.
**Implementation**: `src/daemon/worker.rs`

### Registry
**Definition**: Global project registry persisted at `~/.local/share/1up/projects.json` via atomic_replace with secure file permissions. Each ProjectEntry stores project ID, root path, registration timestamp, and optional IndexingConfig. Supports SIGHUP-triggered reload in the daemon.
**Implementation**: `src/daemon/registry.rs`

### RetrievalBackend
**Definition**: Polymorphic retrieval strategy that selects between SqlVectorV2 (vector + FTS parallel retrieval) and FtsOnly based on whether a query embedding exists and the index contains vectors. Returns RetrievedCandidates with separate vector and FTS result lists.
**Implementation**: `src/search/retrieval.rs`

### CandidateRow
**Definition**: Lightweight search candidate extracted from the database before full hydration. Contains segment metadata (file_path, language, block_type, line range, symbols, role) without full content. Used as the common currency between retrieval, symbol search, and RRF ranking.
**Implementation**: `src/search/retrieval.rs`

### SearchRequest / SearchResponse
**Definition**: Daemon IPC message types for the Unix domain socket search service. SearchRequest contains project_root, query, and limit. SearchResponse is tagged-union: Results{results, daemon_version} or Unavailable{reason}. Transmitted as length-prefixed JSON frames. The optional `daemon_version` field enables CLI/daemon version mismatch detection.
**Implementation**: `src/daemon/search_service.rs`

### Fence
**Definition**: Versioned managed section in agent instruction files (AGENTS.md, CLAUDE.md). Delimited by `<!-- 1up:start:<version> -->` and `<!-- 1up:end:<version> -->` markers. apply_fence handles create, update, and idempotent no-op. Preserves other tools' fences (e.g., rp1).
**Implementation**: `src/shared/reminder.rs`, `src/shared/constants.rs`

### SecureFilesystem
**Definition**: Comprehensive filesystem safety layer in shared/fs.rs. Provides atomic_replace (temp-write + rename + fsync), ensure_secure_dir (symlink-rejecting recursive mkdir with mode enforcement), validate_regular_file_path (canonical parent + approved-root containment), clamp_canonical_path_to_root (symlink-resolving containment check), and remove helpers for regular files and sockets.
**Implementation**: `src/shared/fs.rs`

### SymbolSearchEngine
**Definition**: Exact-first canonical symbol lookup engine. Normalizes queries via normalize_symbolish (strip non-alphanumeric, lowercase), attempts exact canonical match, then falls back to prefix-seed and contains-based fuzzy search with Levenshtein distance. Returns definitions-first, ordered by block_type priority.
**Implementation**: `src/search/symbol.rs`, `src/shared/symbols.rs`

### IPC Frame Protocol
**Definition**: Length-prefixed JSON framing for daemon Unix domain socket communication. 4-byte big-endian length header + JSON payload. Bounded by MAX_DAEMON_REQUEST_BYTES (16KB) and MAX_DAEMON_RESPONSE_BYTES (2MB). All reads/writes have millisecond-level deadlines. Peer UID authorization via SO_PEERCRED.
**Implementation**: `src/daemon/ipc.rs`, `src/shared/constants.rs`

### UpdateManifest
**Definition**: Machine-readable update manifest fetched from a configured HTTPS endpoint. Contains version, git_tag, published_at, notes_url, per-platform artifacts (target triple, archive name, SHA-256, download URL), distribution channel metadata (GitHub release, Homebrew tap/formula, Scoop bucket/manifest), and safety signals (yanked flag, minimum_safe_version, operator message).
**Implementation**: `src/shared/update.rs`

### UpdateCheckCache
**Definition**: Locally persisted result of the most recent update check at `~/.local/share/1up/update-check.json`. Stores current_version, latest_version, checked_at timestamp, install_channel, yanked flag, minimum_safe_version, operator message, notes_url, and upgrade_instruction. Valid for 24 hours (UPDATE_CHECK_TTL_SECS). Version-pinned: cache written by a different binary version is silently discarded.
**Implementation**: `src/shared/update.rs`, `src/shared/config.rs`

### InstallChannel
**Definition**: How 1up was installed on the system: Homebrew, Scoop, Manual, or Unknown. Auto-detected by examining the canonicalized binary path for Cellar/homebrew (macOS/Linux) or scoop\apps (Windows) path segments. Determines the upgrade instruction shown to users.
**Implementation**: `src/shared/update.rs`

### UpdateStatus
**Definition**: Assessed update urgency relative to the running binary version: UpToDate, UpdateAvailable{latest}, Yanked{latest, message} (version recalled, upgrade immediately), or BelowMinimumSafe{latest, minimum_safe, message} (below operator-set floor). Yanked takes precedence over BelowMinimumSafe; both take precedence over UpdateAvailable.
**Implementation**: `src/shared/update.rs`

### DaemonProjectStatus
**Definition**: Persisted daemon heartbeat for a project, written to `.1up/daemon_status.json`. Contains a single `last_file_check_at` timestamp updated every 30 seconds (DAEMON_FILE_CHECK_PERSIST_INTERVAL_MS) to prove the daemon is still watching the repo. Surfaced in `1up status` output.
**Implementation**: `src/shared/types.rs`, `src/shared/config.rs`

### SelfUpdateResult
**Definition**: Outcome of a successful in-place binary replacement via `self_update`. Contains old_version and new_version. Only valid for Manual/Unknown install channels; managed installs (Homebrew, Scoop) use their respective package manager commands.
**Implementation**: `src/shared/update.rs`

## Technical Concepts

### RRF (Reciprocal Rank Fusion)
**Purpose**: Score fusion algorithm combining rankings from vector, FTS, and symbol search channels
**Formula**: `1/(k+rank)` with configurable per-channel weights (vector=1.5x, symbol=4x, FTS=1x)
**Implementation**: `src/search/ranking.rs`

### Result Quality Pipeline
**Purpose**: Sequential quality filters applied after fusion
**Stages**: RRF fusion -> intent boost -> query match boost -> content kind boost -> file path boost (test/vendor penalty) -> short segment penalty -> overlap deduplication -> per-file cap (3) -> limit

### Incremental Indexing
**Purpose**: SHA-256 file content hashing enables skip-if-unchanged logic during re-indexing
**Implementation**: `src/indexer/pipeline.rs`
**Related**: Daemon watches for file changes with 500ms debounce

### Schema Versioning
**Purpose**: Meta table tracks schema version; `prepare_for_write` validates or initializes; stale versions require explicit `1up reindex`
**Implementation**: `src/storage/schema.rs`

### Non-Fatal Update Checks
**Purpose**: Update check failures never block normal operation. Cache read/write/clear failures are logged at debug level but not propagated. Stale cache is returned on fetch failure. Permanent HTTP errors (4xx except 408/429) invalidate cache; transient errors preserve it.

### Version-Pinned Cache
**Purpose**: Update check cache is bound to the binary version that wrote it. `read_compatible_update_cache` discards and clears cache entries from different binary versions, preventing stale state from leaking across manual upgrades or downgrades.

### Download-Verify-Replace Pipeline
**Purpose**: `self_update` stages artifacts in a temp dir adjacent to the binary: download archive -> verify SHA-256 checksum -> extract binary from tar.gz (Unix) or zip (Windows) -> atomic rename replacement with platform-specific strategy.

## Terminology Glossary

### Search Terms
- **Breadcrumb**: Hierarchical scope path for a code segment (e.g., `module::class::method`) providing navigation context
- **Intent Detection**: Signal-based classification of search queries into Definition, Flow, Usage, Docs, or General categories
- **Vector Prefilter**: First-stage candidate selection using vector similarity via libsql vector_top_k (top-200) before full RRF ranking
- **Per-File Cap**: Deduplication strategy limiting results to 3 per source file (`MAX_RESULTS_PER_FILE`)
- **Query Match Boost**: Multiplicative ranking signal based on overlap between query terms and result content/symbols/path, with bonuses for full-term coverage and phrase matches
- **Content Kind Boost**: Ranking adjustment that penalizes markdown results for non-Docs queries (0.72x) and boosts them for Docs queries (1.15x)
- **File Path Boost**: Ranking adjustment that penalizes test/spec (0.7x), doc/readme (0.8x), and vendor/node_modules (0.5x) paths; includes query-path overlap boost for terms matching path components
- **Short Segment Penalty**: Ranking penalty for very short segments: 0.6x for <=2 lines, 0.85x for <=5 lines, 1.0x otherwise
- **Symbol Canonicalization**: Normalization of symbol names by stripping non-alphanumeric characters and lowercasing, enabling cross-convention matching (e.g., ConfigLoader == config_loader)

### Indexing Terms
- **Sliding Window Chunker**: Fallback text segmentation for files without tree-sitter support (60-line window, 10-line overlap)
- **Embedding Vector**: 384-dim L2-normalized float32 vector for semantic similarity search, stored in segment_vectors table
- **File Hash**: SHA-256 content hash stored per segment enabling incremental re-indexing
- **Download Failure Marker**: Sentinel file (`.download_failed`) preventing repeated model download attempts; cleared on success
- **Warm Cache**: EmbeddingRuntime's ability to reuse a previously loaded ONNX session when model directory, file fingerprints (size + mtime), and thread count are unchanged
- **Staged Model Management**: Three-phase model artifact pipeline: download to .staging -> verify SHA-256 digests -> move to verified/<artifact_id>/ -> atomically write current.json pointer
- **SupportedLanguage**: Enum of 16 languages with tree-sitter grammar support
- **Reorder Buffer**: BTreeMap-based deterministic ordering mechanism that reassembles out-of-order parallel parse results by sequence ID before flushing to storage
- **Sequence ID**: Monotonic index assigned to each scanned file before parallel dispatch, ensuring deterministic output order
- **Write Batch**: Configurable number of parsed files grouped into a single storage transaction (`write_batch_files`)

### Update Terms
- **Update Manifest URL**: HTTPS endpoint serving the UpdateManifest JSON. Set at compile time for release builds via ONEUP_UPDATE_MANIFEST_URL env var; runtime value overrides baked value; empty runtime value disables updates for the process
- **Update Check TTL**: Time-to-live for the local update-check cache: 24 hours (86400 seconds). Stale cache triggers a background manifest re-fetch
- **Target Triple**: Rust target triple (e.g., aarch64-apple-darwin) used to match the current platform against release artifacts. Supports 5 platforms: macOS ARM/x86, Linux ARM/x86, Windows x86
- **Yanked Version**: A released version marked as recalled in the update manifest. Yanked status takes precedence over all other update assessments and triggers an urgent upgrade warning
- **Minimum Safe Version**: Operator-set version floor in the update manifest. Binaries below this version receive urgent upgrade warnings with the BelowMinimumSafe status

### Infrastructure Terms
- **Daemon**: Background file watcher process that triggers incremental re-indexing on file changes and serves search requests via Unix domain socket. Supports SIGHUP for registry reload, SIGTERM for graceful shutdown, and semaphore-bounded concurrent search handling
- **Daemon Heartbeat**: Periodic timestamp (`last_file_check_at`) persisted to `.1up/daemon_status.json` every 30 seconds, proving the daemon is actively watching the project. Surfaced in `1up status` output for health checking
- **XDG-Compliant Storage**: Global config in `~/.config/1up/`, data in `~/.local/share/1up/` (models, registry, daemon.pid, daemon.sock, update-check.json); per-project data in `.1up/`
- **IndexingConfig Resolution Chain**: Priority-ordered config resolution: CLI flags > env vars (ONEUP_INDEX_JOBS, ONEUP_EMBED_THREADS, ONEUP_INDEX_WRITE_BATCH_FILES) > registry persisted values > auto defaults
- **Scope Escalation**: Ambiguous watcher events (directory changes, pathless errors) escalate RunScope from Paths to Full for the affected project
- **Peer UID Authorization**: Unix domain socket security check verifying connecting client runs under same UID as daemon
- **Semaphore-Bounded Request Handling**: Daemon limits concurrent in-flight search requests to MAX_DAEMON_IN_FLIGHT_REQUESTS (8); excess returns 'daemon busy'

## Concept Boundaries

| Context | Scope | Key Concepts |
|---------|-------|-------------|
| Indexing | `src/indexer/` | Parser, Chunker, Embedder, EmbeddingRuntime, Pipeline, Scanner, Parallel Worker Pool, Reorder Buffer, IndexingConfig, Staged Model Management, Verified Artifacts |
| Search | `src/search/` | HybridSearchEngine, SymbolSearchEngine, StructuralSearchEngine, ContextEngine, QueryIntent, RetrievalBackend, CandidateRow, RRF Ranking, Multi-Signal Boosting |
| Storage | `src/storage/` | Schema, Segments CRUD, FTS Virtual Table, Vector Index, Segment Symbols Table, Meta Table, Transactional Batch Writes |
| Shared | `src/shared/` | Types, Config, Errors, Constants, Project, Secure Filesystem, Symbol Canonicalization, Fence/Reminder, IndexingConfig Resolution, UpdateManifest, UpdateCheckCache, InstallChannel, UpdateStatus, DaemonProjectStatus, SelfUpdateResult |
| Daemon | `src/daemon/` | Registry, Worker, FileWatcher, ProjectRunState, RunScope, SearchService, IPC Frame Protocol, Lifecycle, SIGHUP Reload, Semaphore-Bounded Search, DaemonProjectStatus Heartbeat |

## Cross-Cutting Concerns

- **Error Handling**: Typed hierarchy with `OneupError` wrapping domain-specific enums (StorageError, IndexingError, SearchError, EmbeddingError, ParserError, DaemonError, ConfigError, FilesystemError, ProjectError, FenceError, UpdateError) via thiserror
- **Filesystem Security**: All state file operations go through shared/fs.rs helpers that reject symlinks, enforce owner-only permissions (0o700 dirs, 0o600 files/sockets), use atomic writes, and validate paths against approved roots
- **Output Formatting**: Three modes (JSON, Human, Plain) selectable via CLI flag; plain is default
- **Model Management**: Staged artifact pipeline with SHA-256 verification, warm cache via EmbeddingRuntime, download failure markers to prevent retry storms
- **Incremental Updates**: File hashing + daemon filesystem monitoring with debounce + deleted file detection via set difference + RunScope for targeted re-indexing
- **Parallelism Configuration**: IndexingConfig resolution chain (CLI > env > registry > auto) with per-project persistence; daemon reloads on SIGHUP
- **Observability**: IndexProgress with IndexParallelism and IndexStageTimings persisted to `.1up/index_status.json`; DaemonProjectStatus heartbeat persisted to `.1up/daemon_status.json` every 30 seconds; phase-granular progress updates
- **Platform Portability**: Daemon modules are Unix-only with conditional compilation; non-Unix platforms get stub implementations; Windows uses conditional ort configuration; self-update uses tar.gz on Unix and zip on Windows with platform-specific binary replacement strategy
- **Self-Update System**: Non-blocking 24-hour TTL cache with version-pinned validity, manifest fetch with bounded timeouts (5s request / 3s connect), passive update notifications on CLI invocation, in-place binary replacement for manual installs, package manager delegation for managed installs (Homebrew/Scoop). UpdateError.should_invalidate_cache() distinguishes permanent from transient failures

## Cross-References
- **Architecture**: See [architecture.md](architecture.md) for system layers and data flows
- **Interaction Model**: See [interaction-model.md](interaction-model.md) for CLI surfaces and feedback loops
- **Modules**: See [modules.md](modules.md) for component breakdown
- **Patterns**: See [patterns.md](patterns.md) for implementation conventions
