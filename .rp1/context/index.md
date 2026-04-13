# 1up — Knowledge Base Index

**What**: A semantic code search engine that indexes local repositories with tree-sitter parsing and ONNX embeddings, then serves hybrid search, symbol lookup, structural queries, context retrieval, and bounded impact exploration through a CLI with an optional background daemon.

**Why**: It gives developers and agents fast code discovery by meaning and structure while keeping indexing local, incremental, and stable enough for interactive workflows.

## Quick Reference

| Attribute | Value |
|---|---|
| Entry point | `src/main.rs` -> `src/cli/mod.rs` |
| Key patterns | Layered CLI + daemon model, candidate-first retrieval, local-only impact analysis |
| Tech stack | Rust, Tokio, libSQL, ONNX Runtime, tree-sitter, clap |
| Distribution | Homebrew, Scoop, GitHub Releases, built-in self-update |

## KB File Manifest

| File | Lines | Load For |
|---|---:|---|
| [concept_map.md](concept_map.md) | ~73 | Terminology, types, domain relationships |
| [architecture.md](architecture.md) | ~114 | Topology, data flow, storage, daemon boundaries |
| [interaction-model.md](interaction-model.md) | ~110 | CLI states, output contracts, follow-up flows |
| [modules.md](modules.md) | ~90 | Component ownership, dependencies, feature deltas |
| [patterns.md](patterns.md) | ~74 | Coding, storage, error, output, and test idioms |

## Task-Based Loading

| Task | Load Files |
|---|---|
| Code review | `patterns.md` |
| Bug investigation | `architecture.md`, `modules.md` |
| Feature work | `modules.md`, `patterns.md` |
| Search and ranking changes | `concept_map.md`, `architecture.md`, `interaction-model.md` |
| Impact or relation work | `concept_map.md`, `architecture.md`, `modules.md`, `interaction-model.md` |
| Strategic or system-wide analysis | All files |

## Recent Learnings

- `impact-horizon` added an explicit local-only `1up impact` path instead of extending daemon IPC.
- Search results can now expose additive machine-readable `segment_id` handles for exact impact follow-up.
- Schema v8 introduces persisted `segment_relations`, with bounded expansion and refusal semantics to protect interactivity.
- Benchmarks and black-box tests now guard against regressions to core search behavior while impact support evolves.

## How To Load

Agents load this KB automatically:
1. Read `index.md` first.
2. Load only the files needed for the current task.
3. Avoid loading the full KB unless the work is system-wide.
