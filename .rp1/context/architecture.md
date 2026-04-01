# Architecture

## Process Model

Two process types share no runtime state:

1. **CLI process** (per invocation): Parses args via clap, opens libSQL database, runs query/command, outputs results, exits. Read-only DB access for queries; read-write for indexing commands.

2. **Daemon process** (background): Spawned as a detached child via `1up __worker`. Watches registered projects for file changes using the `notify` crate, runs incremental re-indexing through the same pipeline code. Manages lifecycle via PID file and Unix signals.

Communication between CLI and daemon is exclusively through:
- The libSQL database (shared storage)
- The PID file (`~/.local/share/1up/daemon.pid`)
- The project registry (`~/.local/share/1up/projects.json`)
- Unix signals (SIGHUP for reload, SIGTERM for shutdown)

## Daemon Lifecycle

1. **Start**: `1up start` or auto-start on first query registers the project, spawns `1up __worker`, writes PID file.
2. **Reload**: SIGHUP causes the worker to reload the project registry and update watched directories.
3. **Stop**: `1up stop` deregisters the project. SIGTERM if no projects remain; SIGHUP otherwise.
4. **No idle timeout**: Daemon runs indefinitely until explicitly stopped via `1up stop` or SIGTERM.
5. **Crash recovery**: Stale PID files detected and cleaned on startup.

## Data Flow: Indexing Pipeline

```
Scanner (ignore crate, .gitignore-aware)
  -> Read file, compute SHA-256 hash
  -> Skip if hash unchanged (incremental)
  -> Language supported? 
     Yes -> Tree-sitter parse -> extract segments, symbols, complexity
     No  -> Text chunker (sliding window with overlap)
  -> Embedder (ONNX batch inference, 384-dim vectors)
  -> Store in libSQL (INSERT OR REPLACE in transaction)
```

Deleted files have their segments removed. The scanner uses the `ignore` crate (same library powering ripgrep) for .gitignore respect.

## Data Flow: Search

```
Query text
  -> Intent detection (DEFINITION, FLOW, USAGE, DOCS, GENERAL)
  -> Generate query embedding (384-dim via ONNX)
  -> Stage 1: int8 prefilter (embedding_q8, top-200 candidates)
  -> Stage 2: f32 rerank (full-precision embedding)
  -> FTS5 MATCH query (parallel with vector search)
  -> RRF fusion with intent-based boosting
  -> Dedup, per-file caps, penalties (test/doc/short segments)
  -> Return ranked results
```

Falls back to FTS-only when the embedding model is unavailable.

## Storage Schema

Single libSQL database per project at `.1up/index.db`:

- **segments**: Main table with file_path, language, block_type, content, line_start/end, embedding (F32_BLOB(384)), embedding_q8 (VECTOR8(384)), complexity, role, defined_symbols (JSON), referenced_symbols (JSON), file_hash
- **segments_fts**: FTS5 virtual table synced via triggers on content, file_path, block_type
- **meta**: Key-value store for schema_version and timestamps

## Storage Layout

```
~/.config/1up/                  # XDG config (reserved)
~/.local/share/1up/             # XDG data
  daemon.pid
  projects.json
  models/all-MiniLM-L6-v2/     # ONNX model + tokenizer (auto-downloaded)

<project>/.1up/
  project_id                    # UUID
  index.db                     # libSQL database
```

## Technology Stack

| Component | Crate |
|-----------|-------|
| Async runtime | `tokio` |
| CLI | `clap` (derive) |
| Database | `libsql` |
| Tree-sitter | `tree-sitter` + 9 language grammar crates |
| ONNX inference | `ort` |
| Tokenizer | `tokenizers` |
| File watching | `notify` |
| File scanning | `ignore` |
| Serialization | `serde`, `serde_json` |
| Error handling | `thiserror`, `anyhow` |
| HTTP | `reqwest` (model download) |
| Hashing | `sha2` |
| Terminal output | `colored` |
| UUID | `uuid` |
| XDG paths | `dirs` |
| Progress bars | `indicatif` |
| Logging | `tracing`, `tracing-subscriber` |
| Process/signal | `nix` |
| Timestamps | `chrono` |
