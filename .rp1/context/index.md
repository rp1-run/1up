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
| [concept_map.md](concept_map.md) | ~80 | Terminology, types, domain relationships, impact outcome vocabulary |
| [architecture.md](architecture.md) | ~120 | Topology, data flow, storage, daemon boundaries, impact outcome flow |
| [interaction-model.md](interaction-model.md) | ~120 | CLI states, output contracts, follow-up flows, machine-readable impact semantics |
| [modules.md](modules.md) | ~100 | Component ownership, dependencies, feature deltas, rollout evidence entry points |
| [patterns.md](patterns.md) | ~74 | Coding, storage, error, output, and test idioms |

## Task-Based Loading

| Task | Load Files |
|---|---|
| Code review | `patterns.md` |
| Bug investigation | `architecture.md`, `modules.md` |
| Feature work | `modules.md`, `patterns.md` |
| Search and ranking changes | `concept_map.md`, `architecture.md`, `interaction-model.md` |
| Impact or relation work | `concept_map.md`, `architecture.md`, `modules.md`, `interaction-model.md`, `patterns.md` |
| Strategic or system-wide analysis | All files |

## Recent Learnings

- `impact` now separates confident relation-backed likely-impact `results` from heuristic-only or demoted-relation `contextual_results`, and empty expansions return explicit `empty` or `empty_scoped` states instead of anchor-echo fallbacks.
- Search results expose additive machine-readable `segment_id` handles for exact impact follow-up without changing discovery ranking.
- Schema v9 persists relation lookup-target and qualifier-fingerprint evidence in `segment_relations`, enabling bounded structural-confidence scoring without changing the impact envelope.
- Rollout evidence now has dedicated entry points: `just impact-eval` for trust gating and `just impact-bench` for latency gating.

## How To Load

Agents load this KB automatically:
1. Read `index.md` first.
2. Load only the files needed for the current task.
3. Avoid loading the full KB unless the work is system-wide.
