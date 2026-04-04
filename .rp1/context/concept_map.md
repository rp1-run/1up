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
- **ContextResult**: Context retrieval result including the enclosing scope (function, class, etc.)

### Project
**Definition**: A registered codebase identified by a UUID stored in `.1up/project_id`, with a local index database at `.1up/index.db`.
**Implementation**: `src/shared/project.rs`, `src/shared/config.rs`

### QueryIntent
**Definition**: Detected search intent from natural language queries: Definition, Flow, Usage, Docs, or General. Used to apply multiplicative score boosts to matching result types.
**Implementation**: `src/search/intent.rs`

### Embedder
**Definition**: ONNX-backed embedding engine using all-MiniLM-L6-v2 model, producing 384-dimensional L2-normalized vectors via mean pooling. Supports configurable intra-op thread count via `from_dir_with_threads`.
**Implementation**: `src/indexer/embedder.rs`

### IndexingConfig
**Definition**: Resolved parallelism settings for an indexing run: `jobs` (parse worker count), `embed_threads` (ONNX intra-op threads), and `write_batch_files` (storage transaction batch size). Resolved via a priority chain: CLI flags > env vars > registry persisted values > auto defaults.
**Implementation**: `src/shared/types.rs`, `src/shared/config.rs`

### IndexProgress
**Definition**: Persisted indexing status for a project, tracking state (Idle/Running/Complete), phase (Pending/Scanning/Parsing/Storing/Complete), file counters, parallelism snapshot, and stage timings. Written to `.1up/index_status.json`.
**Implementation**: `src/shared/types.rs`, `src/indexer/pipeline.rs`

### ProjectRunState
**Definition**: Daemon-internal per-project run state tracking: one active run + one queued follow-up. Collapses burst file changes into a single dirty flag with accumulated change count.
**Implementation**: `src/daemon/worker.rs`

### Registry
**Definition**: Global project registry persisted at `~/.local/share/1up/projects.json`. Each ProjectEntry stores project ID, root path, and optional IndexingConfig. Supports SIGHUP-triggered reload in the daemon.
**Implementation**: `src/daemon/registry.rs`

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

## Terminology Glossary

### Search Terms
- **Breadcrumb**: Hierarchical scope path for a code segment (e.g., `module::class::method`) providing navigation context
- **Intent Detection**: Signal-based classification of search queries into Definition, Flow, Usage, Docs, or General categories
- **Vector Prefilter**: First-stage candidate selection using int8-quantized vector similarity (top-200) before full ranking
- **Per-File Cap**: Deduplication strategy limiting results to 3 per source file (`MAX_RESULTS_PER_FILE`)
- **Query Match Boost**: Multiplicative ranking signal based on overlap between query terms and result content/symbols/path, with bonuses for full-term coverage and phrase matches
- **Content Kind Boost**: Ranking adjustment that penalizes markdown results for non-Docs queries (0.72x) and boosts them for Docs queries (1.15x)

### Indexing Terms
- **Sliding Window Chunker**: Fallback text segmentation for files without tree-sitter support (60-line window, 10-line overlap)
- **Embedding Vector**: 384-dim L2-normalized float32 vector for semantic similarity search
- **File Hash**: SHA-256 content hash stored per segment enabling incremental re-indexing
- **Download Failure Marker**: Sentinel file (`.download_failed`) preventing repeated model download attempts
- **SupportedLanguage**: Enum of 16 languages with tree-sitter grammar support
- **Reorder Buffer**: BTreeMap-based deterministic ordering mechanism that reassembles out-of-order parallel parse results by sequence ID before flushing to storage
- **Sequence ID**: Monotonic index assigned to each scanned file before parallel dispatch, ensuring deterministic output order regardless of parse worker completion order
- **Write Batch**: Configurable number of parsed files grouped into a single storage transaction (`write_batch_files`), balancing throughput vs transaction size

### Infrastructure Terms
- **Daemon**: Background file watcher process that triggers incremental re-indexing on file changes. Supports SIGHUP for registry reload and SIGTERM for graceful shutdown.
- **XDG-Compliant Storage**: Global config in `~/.config/1up/`, data in `~/.local/share/1up/` (models, registry); per-project data in `.1up/`
- **IndexingConfig Resolution Chain**: Priority-ordered config resolution: CLI flags (`--jobs`, `--embed-threads`) > env vars (`ONEUP_INDEX_JOBS`, `ONEUP_EMBED_THREADS`) > registry persisted values > auto defaults (cores-1 jobs, clamped embed threads)

## Concept Boundaries

| Context | Scope | Key Concepts |
|---------|-------|-------------|
| Indexing | `src/indexer/` | Parser, Chunker, Embedder, Pipeline, Scanner, Parallel Worker Pool, Reorder Buffer, IndexingConfig |
| Search | `src/search/` | HybridSearchEngine, SymbolSearchEngine, StructuralSearchEngine, QueryIntent, RRF Ranking, Multi-Signal Boosting |
| Storage | `src/storage/` | Schema, Segments CRUD, FTS Virtual Table, Vector Index, Meta Table, Transactional Batch Writes |
| Shared | `src/shared/` | Types, Config, Errors, Constants, Project, IndexingConfig Resolution |
| Daemon | `src/daemon/` | Registry, Worker, FileWatcher, ProjectRunState, SIGHUP Reload, Run Scheduling |

## Cross-Cutting Concerns

- **Error Handling**: Typed hierarchy with `OneupError` wrapping domain-specific enums (StorageError, IndexingError, SearchError, etc.) via thiserror
- **Output Formatting**: Three modes (JSON, Human, Plain) selectable via CLI flag
- **Model Management**: Auto-download from HuggingFace with failure markers to prevent retry storms
- **Incremental Updates**: File hashing + daemon filesystem monitoring with debounce + deleted file detection via set difference
- **Parallelism Configuration**: IndexingConfig resolution chain (CLI > env > registry > auto) with per-project persistence; daemon reloads on SIGHUP
- **Observability**: IndexProgress with IndexParallelism and IndexStageTimings persisted to `.1up/index_status.json`; phase-granular progress updates; rendered in status output

## Cross-References
- **Architecture**: See [architecture.md](architecture.md) for system layers and data flows
- **Interaction Model**: See [interaction-model.md](interaction-model.md) for CLI surfaces and feedback loops
- **Modules**: See [modules.md](modules.md) for component breakdown
- **Patterns**: See [patterns.md](patterns.md) for implementation conventions
