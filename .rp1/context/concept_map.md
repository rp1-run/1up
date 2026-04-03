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
**Definition**: ONNX-backed embedding engine using all-MiniLM-L6-v2 model, producing 384-dimensional L2-normalized vectors via mean pooling.
**Implementation**: `src/indexer/embedder.rs`

## Technical Concepts

### RRF (Reciprocal Rank Fusion)
**Purpose**: Score fusion algorithm combining rankings from vector, FTS, and symbol search channels
**Formula**: `1/(k+rank)` with configurable per-channel weights (vector=1.5x, symbol=4x, FTS=1x)
**Implementation**: `src/search/ranking.rs`

### Result Quality Pipeline
**Purpose**: Sequential quality filters applied after fusion
**Stages**: RRF fusion -> intent boost -> file path boost (test/vendor penalty) -> short segment penalty -> overlap deduplication -> per-file cap (3) -> limit

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

### Indexing Terms
- **Sliding Window Chunker**: Fallback text segmentation for files without tree-sitter support (60-line window, 10-line overlap)
- **Embedding Vector**: 384-dim L2-normalized float32 vector for semantic similarity search
- **File Hash**: SHA-256 content hash stored per segment enabling incremental re-indexing
- **Download Failure Marker**: Sentinel file (`.download_failed`) preventing repeated model download attempts
- **SupportedLanguage**: Enum of 16 languages with tree-sitter grammar support

### Infrastructure Terms
- **Daemon**: Background file watcher process that triggers incremental re-indexing on file changes
- **XDG-Compliant Storage**: Global config in `~/.config/1up/`, data in `~/.local/share/1up/` (models, registry); per-project data in `.1up/`

## Concept Boundaries

| Context | Scope | Key Concepts |
|---------|-------|-------------|
| Indexing | `src/indexer/` | Parser, Chunker, Embedder, Pipeline, Scanner |
| Search | `src/search/` | HybridSearchEngine, SymbolSearchEngine, StructuralSearchEngine, QueryIntent, RRF Ranking |
| Storage | `src/storage/` | Schema, Segments CRUD, FTS Virtual Table, Vector Index, Meta Table |
| Shared | `src/shared/` | Types, Config, Errors, Constants, Project |

## Cross-Cutting Concerns

- **Error Handling**: Typed hierarchy with `OneupError` wrapping domain-specific enums (StorageError, IndexingError, SearchError, etc.) via thiserror
- **Output Formatting**: Three modes (JSON, Human, Plain) selectable via CLI flag
- **Model Management**: Auto-download from HuggingFace with failure markers to prevent retry storms
- **Incremental Updates**: File hashing + daemon filesystem monitoring with debounce

## Cross-References
- **Architecture**: See [architecture.md](architecture.md) for system layers and data flows
- **Modules**: See [modules.md](modules.md) for component breakdown
- **Patterns**: See [patterns.md](patterns.md) for implementation conventions
