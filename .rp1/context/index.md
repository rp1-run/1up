# 1up Knowledge Base Index

**Project**: 1up -- Unified search substrate for source repositories
**Language**: Rust (2021 edition)
**Binary**: `1up` (single crate, package name `oneup`)

## KB Files

| File | Contents |
|------|----------|
| `architecture.md` | System architecture, process model, data flow, storage layout |
| `modules.md` | Module hierarchy with descriptions and responsibilities |
| `patterns.md` | Key patterns, conventions, and coding standards |

## Loading Guide

| Task | Load |
|------|------|
| Code review | `patterns.md` |
| Bug investigation | `architecture.md`, `modules.md` |
| Feature work | `modules.md`, `patterns.md` |
| System-wide analysis | all files |

## Quick Orientation

- Single-binary CLI with clap derive subcommands
- Turso (formerly libSQL) for persistent storage with tantivy-backed FTS + vector columns
- Tree-sitter for multi-language AST parsing (9 languages compiled in)
- ONNX embeddings via `ort` crate (all-MiniLM-L6-v2, 384-dim)
- Background daemon for file watching and incremental re-indexing
- CLI and daemon share no runtime state -- communicate through the database and filesystem
- XDG-compliant paths for config and data storage
