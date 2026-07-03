# 1up evals

Deterministic quality harnesses for 1up search and agent adoption.

## MCP adoption harness

The 1up agent variants in `suites/1up-search/evals.yaml` and `suites/1up-impact/evals.yaml` run the local MCP server with command `1up` and args `["mcp", "--path", "."]`. Prompts instruct agents to use canonical retained MCP tools: `oneup_status`, `oneup_start`, `oneup_search`, `oneup_get`, `oneup_symbol`, `oneup_context`, `oneup_impact`, and `oneup_structural` instead of shelling out to `1up ...`.

The shared assertions inspect provider MCP tool-call metadata. They require `oneup_status` before discovery, optional `oneup_start` only after status, MCP search before code discovery, handle hydration with `oneup_get` or file-line context with `oneup_context`, symbol verification with `oneup_symbol` when completeness matters, and `oneup_impact` plus primary/contextual interpretation for impact tasks. When provider metadata includes tool outputs, the assertions validate the retained structured envelope shape: `status`, `summary`, `data`, and canonical `next_actions`.

Broad raw `grep`, `rg`, and `find` usage is a failure in the 1up variant; exact literal `grep` or `rg` verification is allowed only after MCP discovery narrows scope to precise files. Terminal presentation such as ANSI color, spinners, or tables is not part of the MCP eval contract.

Release readiness uses these existing suites as the MCP adoption evidence source. `scripts/release/generate_release_evidence.sh` records a retained summary JSON when one is available, or an explicit skipped reason when provider credentials, host access, or artifact retention are unavailable; no separate installation-readiness eval harness is required.

Useful checks:

```sh
npm run lint
npm test
npx promptfoo validate -c suites/1up-search/evals.yaml
npx promptfoo validate -c suites/1up-impact/evals.yaml
npm run eval
npm run eval:impact
```

## Recall gate

The recall harness measures 1up semantic-search retrieval quality directly (not agent MCP tool selection), so it invokes the CLI `search`/`get` path rather than the MCP suites above. It is now a **baseline-relative gate**: it fails closed on a semantic-path preflight and exits non-zero when recall regresses beyond tolerance, so a vector-storage, embedder, or ranking change that quietly loses recall stops CI red instead of merging blind. It remains distinct from P5 MCP release-readiness evidence (that lives in the adoption suites above) but is no longer merely historical.

**MODEL-ENABLED — never run in-agent.** `just eval-recall`, `just eval-recall-ab`, and `just eval-recall-baseline` reindex with the embedding model enabled and hang inside agent sessions. Run them only as a manual pre-merge DoD step or on the scheduled `.github/workflows/embedding-quality.yml` workflow.

**Script**: [`suites/1up-search/recall.ts`](suites/1up-search/recall.ts) (impure driver)
**Gate logic**: [`suites/1up-search/recall-compare.ts`](suites/1up-search/recall-compare.ts) (pure, unit-tested with `bun test`, no model or index)
**Corpus**: [`suites/1up-search/recall-corpus.jsonl`](suites/1up-search/recall-corpus.jsonl)
**Baseline**: [`suites/1up-search/recall-baseline.json`](suites/1up-search/recall-baseline.json)

The harness reads a JSONL corpus of `{ query, expected_anchors }` rows, runs `1up search -n 20 --path <repo> <q>` once per query against the 1up repo itself, parses the lean row grammar (`<score>  <path>:<l1>-<l2>  <kind>  <breadcrumb>::<symbol>  :<segment_id>`) to recover top-10 and top-20 result lists, lazily hydrates handles with `1up get`, and computes:

```
recall@k = mean_over_scored_queries(matched_anchor_count / expected_anchor_count)
```

Rows with missing or empty anchors are recorded as `skipped_no_gold` and excluded from the mean so the output is always numeric. An empty corpus yields `recall = 0` rather than `NaN`. Legacy `expected_segment_ids` rows remain supported only for archived corpora.

### Gate semantics

The gate is enforced in two stages:

1. **Semantic-path preflight** (before scoring). Via `1up status <repo> -f json` the harness asserts `vector_rows > 0`, a current positive `schema_version`, and that `embedding_model` matches the expected variant (`ONEUP_MODEL_VARIANT`, default `int8` — INT8 identities carry the `@int8` suffix). Per-query `search` stderr is captured (no longer discarded) and any degraded / FTS-only wording fails the run. A vectorless, schema-less, wrong-variant, or degraded run fails closed with a `FAIL:` line and exit code 1 — it never scores silently.
2. **Baseline comparison** (after scoring). Candidate recall is compared against the pinned `recall-baseline.json` using an **absolute per-k tolerance** (default `0.02`, overridable via `ONEUP_RECALL_TOLERANCE`). Any k regressing by more than the tolerance sets `process.exitCode = 1` and prints a `FAIL:` line (mirroring `search-bench.ts`). A candidate scoring at or above baseline never fails — the gate guards regression only. A **missing baseline** or a **corpus/config mismatch** (differing `corpus.sha256`/`size`, `schema_version`, `model_id`, or `max_tokens`) is an explicit gate error, not a pass.

`ONEUP_RECALL_TOLERANCE` must be a non-negative number; anything else is a gate error.

Output JSON envelope (`suites/1up-search/recall-results.json`) gains `gates{}` (preflight result, expected variant, tolerance, degraded-stderr queries, and the recall verdict) plus `delta_vs_baseline`:

```json
{
  "schema_version": 18,
  "corpus_size": 15,
  "recall_at_10": 0.461,
  "recall_at_20": 0.589,
  "delta_vs_baseline": { "recall_at_10": 0.0, "recall_at_20": 0.0 },
  "gates": {
    "expected_variant": "int8",
    "tolerance": 0.02,
    "preflight": { "ok": true, "failures": [] },
    "degraded_stderr_queries": [],
    "recall": { "verdict": "pass", "regressions": [] }
  },
  "reports": [
    { "k": 10, "recall": 0.461, "scored_queries": 15, "total_queries": 15, "per_query": [ ... ] }
  ]
}
```

### Run the gate

```sh
just eval-recall
```

The recipe builds the repo-local `1up`, runs `1up reindex .` (`reindex`, not `index`, so the model-identity gate cannot fail closed on a stale index), then runs the harness under Bun. Recall numbers and the gate verdict are printed to stdout and written to `suites/1up-search/recall-results.json`. Exit code is non-zero on any preflight failure, degraded response, or out-of-tolerance regression.

All recall recipes reindex with `--exclude-glob 'evals/suites/1up-search/recall-*.json'`: the harness rewrites `recall-results.json` (and, when capturing, `recall-baseline.json`) inside the repo, and indexing its own outputs perturbs the corpus between runs — one extra segment was observed to shift recall@20 by more than the default tolerance. The baseline and the scored index must be computed over the same corpus, so both capture and gate runs exclude these artifacts.

### A/B parity recipe

```sh
just eval-recall-ab
```

Confirms INT8-vs-FP32 recall parity within tolerance — the required pre-merge DoD for a variant/model change (REQ-004). It runs `1up stop` then reindexes and scores the `fp32` leg (captured to a temporary baseline), then `1up stop` + reindexes + scores the `int8` leg gated against that temp baseline within `ONEUP_RECALL_TOLERANCE`, exiting non-zero beyond it. `1up stop` per leg prevents a live daemon holding the other variant from serving query embeddings for the wrong leg; the pinned `recall-baseline.json` is never touched. Record the resulting numbers in the baseline-update commit.

### Baseline-update policy

The pinned `recall-baseline.json` is the structured contract the gate compares against:

```json
{
  "captured_at": "<ISO-8601>",
  "schema_version": 18,
  "model_id": "<embedding model id, e.g. ...@int8>",
  "max_tokens": 256,
  "corpus": { "size": 15, "sha256": "<hex>" },
  "recall_at_10": 0.461,
  "recall_at_20": 0.589
}
```

The baseline changes **only** via the sanctioned capture recipe:

```sh
just eval-recall-baseline
```

This reindexes and writes a fresh structured baseline (with metadata) from the current run. **Never** regenerate the baseline to make the gate pass — that defeats the gate. Move it only for a legitimate reason (an intended recall improvement you are locking in, a corpus expansion, or a repo-layout change that makes anchors stale), and record the new recall numbers and the rationale in the baseline-update commit message (and, when relevant, in the KB `Recent Learnings` entry). A structurally invalid or absent baseline (e.g. the legacy schema-v11 prose form) is treated as missing, so the gate errors with a clear "capture a baseline" message rather than passing.

Recall is not P5 MCP release evidence; P5 readiness uses the MCP adoption suites above. The gate is most relevant when a change intentionally touches vector storage, the ANN index, the embedder, the tokenizer window, or retrieval ranking.

## Related scripts

- `scripts/benchmark_vector_index_size.sh` - REQ-001/REQ-003/REQ-005 gate. Fresh-reindexes the 1up repo and reports `db_size_bytes`, `indexing_ms`, and `schema_version`; pinned baseline at `scripts/baselines/vector_index_size_baseline.json`. Invoked via `just bench-vector-index-size`.
- `evals/suites/1up-search/search-bench.ts` - latency-oriented search harness; not part of P5 MCP eval readiness evidence.
