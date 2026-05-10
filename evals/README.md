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

## Historical recall harness

The recall harness is historical vector-ranking evidence, not P5 MCP release-readiness evidence. It still invokes the manual CLI because it measures retrieval ranking directly, not agent MCP tool selection, and must produce comparable recall numbers across baseline and post-change runs.

**Script**: [`suites/1up-search/recall.ts`](suites/1up-search/recall.ts)
**Corpus**: [`suites/1up-search/recall-corpus.jsonl`](suites/1up-search/recall-corpus.jsonl)
**Baseline**: [`suites/1up-search/recall-baseline.json`](suites/1up-search/recall-baseline.json)

The harness reads a JSONL corpus of `{ query, expected_anchors }` rows, runs `1up search -n 20 --path <repo> <q>` once per query against the 1up repo itself, parses the lean row grammar (`<score>  <path>:<l1>-<l2>  <kind>  <breadcrumb>::<symbol>  :<segment_id>`) to recover top-10 and top-20 result lists, lazily hydrates handles with `1up get`, and computes:

```
recall@k = mean_over_scored_queries(matched_anchor_count / expected_anchor_count)
```

Rows with missing or empty anchors are recorded as `skipped_no_gold` and excluded from the mean so the output is always numeric. An empty corpus yields `recall = 0` rather than `NaN`. Legacy `expected_segment_ids` rows remain supported only for archived corpora.

Output JSON envelope (`suites/1up-search/recall-results.json`):

```json
{
  "schema_version": 13,
  "corpus_size": 15,
  "reports": [
    {
      "k": 10,
      "recall": 0.461,
      "scored_queries": 15,
      "total_queries": 15,
      "per_query": [ ... ]
    }
  ]
}
```

### Run it

```sh
just eval-recall
```

The recipe runs `1up index .` to ensure the index is current, then invokes the harness under Bun. Recall numbers are printed to stdout and written to `suites/1up-search/recall-results.json`.

### Historical baseline

The pinned baseline at `suites/1up-search/recall-baseline.json` is retained for historical vector-quality comparison only:

| k | recall |
|---|---|
| 10 | 0.467 |
| 20 | 0.589 |

Do not count this baseline as retained P5 release evidence. P5 readiness uses the MCP adoption suites above; recall remains useful when a change intentionally touches vector storage, the HNSW index, the embedder, or retrieval ranking.

### Regenerate the baseline

Only regenerate when the comparison contract itself needs to move, such as corpus expansion or repo layout changes that make anchors stale. Do not regenerate to "make the gate pass".

```sh
just eval-recall
cp evals/suites/1up-search/recall-results.json evals/suites/1up-search/recall-baseline.json
```

Record why the baseline moved in the commit message and, when relevant, in the KB `Recent Learnings` entry.

## Related scripts

- `scripts/benchmark_vector_index_size.sh` - REQ-001/REQ-003/REQ-005 gate. Fresh-reindexes the 1up repo and reports `db_size_bytes`, `indexing_ms`, and `schema_version`; pinned baseline at `scripts/baselines/vector_index_size_baseline.json`. Invoked via `just bench-vector-index-size`.
- `evals/suites/1up-search/search-bench.ts` - latency-oriented search harness; not part of P5 MCP eval readiness evidence.
