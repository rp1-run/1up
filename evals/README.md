# 1up evals

Deterministic quality harnesses for 1up search and agent adoption.

## MCP adoption harness

The 1up agent variants in `suites/1up-search/evals.yaml` and `suites/1up-impact/evals.yaml` run the local MCP server with command `1up` and args `["mcp", "--path", "."]`. Prompts instruct agents to use canonical retained MCP tools: `oneup_status`, `oneup_start`, `oneup_search`, `oneup_get`, `oneup_symbol`, `oneup_context`, `oneup_impact`, and `oneup_structural` instead of shelling out to `1up ...`.

The shared assertions inspect provider MCP tool-call metadata. They require MCP search before discovery, handle hydration with `oneup_get` or file-line context with `oneup_context`, symbol verification with `oneup_symbol` when completeness matters, and `oneup_impact` plus primary/contextual interpretation for impact tasks. Broad raw `grep`, `rg`, and `find` usage is a failure in the 1up variant; exact literal `grep` or `rg` verification is allowed only after MCP discovery narrows scope to precise files.

Release readiness uses these existing suites as the MCP adoption evidence source. `scripts/release/generate_release_evidence.sh` records a retained summary JSON when one is available, or an explicit skipped reason when provider credentials, host access, or artifact retention are unavailable; no separate installation-readiness eval harness is required.

Useful checks:

```sh
npm run lint
npm test
npx promptfoo validate -c suites/1up-search/evals.yaml
npx promptfoo validate -c suites/1up-impact/evals.yaml
```

## Recall harness

The recall harness is a separate REQ-002 gate for vector-index changes (schema bumps, element-type flips, HNSW option changes). It still invokes the manual CLI because it measures retrieval ranking directly, not agent MCP tool selection, and must produce a single comparable recall number across baseline and post-change runs.

**Script**: [`suites/1up-search/recall.ts`](suites/1up-search/recall.ts)
**Corpus**: [`suites/1up-search/recall-corpus.jsonl`](suites/1up-search/recall-corpus.jsonl)
**Baseline**: [`suites/1up-search/recall-baseline.json`](suites/1up-search/recall-baseline.json)

The harness reads a JSONL corpus of `{ query, expected_segment_ids, expected_files? }` rows, runs `1up search -n 20 --path <repo> <q>` once per query against the 1up repo itself, parses the lean row grammar (`<score>  <path>:<l1>-<l2>  <kind>  <breadcrumb>::<symbol>  :<segment_id>`) to recover top-10 and top-20 `segment_id` lists, and computes:

```
recall@k = mean_over_scored_queries(|retrieved_top_k ∩ gold| / |gold|)
```

Rows with missing or empty `expected_segment_ids` are recorded as `skipped_no_gold` and excluded from the mean so the output is always numeric. An empty corpus yields `recall = 0` rather than `NaN`.

Output JSON envelope (`suites/1up-search/recall-results.json`):

```json
{
  "schema_version": 12,
  "k": [10, 20],
  "recall": { "10": 0.889, "20": 0.978 },
  "per_query": [ ... ]
}
```

### Run it

```sh
just eval-recall
```

The recipe runs `1up index .` to ensure the index is current, then invokes the harness under Bun. Recall numbers are printed to stdout and written to `suites/1up-search/recall-results.json`.

### Baseline and the 2 pt gate

The pinned baseline at `suites/1up-search/recall-baseline.json` was captured against schema v11 on the 1up repo:

| k | recall |
|---|---|
| 10 | 0.889 |
| 20 | 0.978 |

Three back-to-back runs produced identical numbers (0 pt variance), well inside HYP-003's 0.5 pt envelope.

REQ-002 requires post-change recall to stay within **2 percentage points absolute** of the baseline at both k=10 and k=20. Compare `recall-results.json` (post-change) against `recall-baseline.json` after any change that touches vector storage, the HNSW index, the embedder, or retrieval ranking.

### Regenerate the baseline

Only regenerate when the comparison contract itself needs to move (e.g. the corpus expands or the repo layout changes enough that prior segment IDs no longer map). Do not regenerate to "make the gate pass".

```sh
just eval-recall
cp evals/suites/1up-search/recall-results.json evals/suites/1up-search/recall-baseline.json
```

Record why the baseline moved in the commit message and, when relevant, in the KB `Recent Learnings` entry.

## Related scripts

- `scripts/benchmark_vector_index_size.sh` - REQ-001/REQ-003/REQ-005 gate. Fresh-reindexes the 1up repo and reports `db_size_bytes`, `indexing_ms`, and `schema_version`; pinned baseline at `scripts/baselines/vector_index_size_baseline.json`. Invoked via `just bench-vector-index-size`.
- `evals/suites/1up-search/search-bench.ts` - latency-oriented search harness; not part of the REQ-002 recall gate.
