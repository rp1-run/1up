# Eval Record: MCP Adoption Suite Not Run (Credentials Unavailable)

**Date:** 2026-06-10
**Branch:** fix/windows-paths-and-pipeline-hardening
**Status:** NOT RUN — harness verified ready, blocked on provider credentials
**Suites:** `suites/1up-search/evals.yaml`, `suites/1up-impact/evals.yaml` (canonical `oneup_*` MCP surface)

## Why No Results Are Recorded

Agent eval runs call the Anthropic API for both agents and the grading model
and cost real money. This run was prepared under an explicit cost directive:
verify prerequisites, run the standard suite at most once if credentials are
available, and never fake or extrapolate results.

`ANTHROPIC_API_KEY` is not set in the execution environment, so the suite was
not executed. No numbers below are measurements; this record exists so the
next runner starts from a verified-ready harness instead of rediscovering
setup state.

## Blockers (Exact)

1. **`ANTHROPIC_API_KEY` is not set.** Both `anthropic:claude-agent-sdk`
   providers and the `claude-haiku-4-5-20251001` grading provider require it.
2. **PATH `1up` is stale.** The harness resolves `1up` from PATH for fixture
   indexing (`suites/shared/extension.ts`) and as the MCP server command
   (`command: 1up`, args `["mcp", "--path", "."]`). The currently installed
   binary is v0.1.7, which predates the MCP surface entirely. A binary with
   the canonical `oneup_*` tools (v0.1.9+; ideally built from this branch)
   must be first on PATH.

## Harness Readiness (Verified 2026-06-10)

| Check | Result |
|---|---|
| `bunx tsc -p tsconfig.json --noEmit` | pass |
| `bun test suites/shared/assertions/index.test.ts` | 36/36 pass |
| `bunx promptfoo validate -c suites/1up-search/evals.yaml` | Configuration is valid |
| `bunx promptfoo validate -c suites/1up-impact/evals.yaml` | Configuration is valid |
| Suites exercise MCP surface, not lean CLI | confirmed: `mcp__oneup__oneup_{status,start,search,get,symbol,context,impact,structural}` allowed tools, server `1up mcp --path .` |
| emdash fixture cache (`evals/.cache/emdash`, pinned 5beb0dd) | present |
| Runner versions | bun 1.3.14, promptfoo 0.121.3, node 26.3.0 |

### Harness fix applied during verification

`promptfoo` crashed at startup with `ERR_DLOPEN_FAILED`: the
`better-sqlite3` native module in `evals/node_modules` was compiled against
an older Node ABI (`NODE_MODULE_VERSION 141`) while the active Node is
v26.3.0 (`NODE_MODULE_VERSION 147`). Fixed locally (not a repo change) with:

```sh
cd evals && npm rebuild better-sqlite3 --nodedir="$(mise where node)"
```

The `--nodedir` flag matters in this environment because node-gyp's header
download fails through the local HTTP stack. If Node changes ABI again,
expect to repeat this.

## How To Run (When Credentials Are Available)

```sh
# 1. Put a current binary on PATH (from the repo root):
cargo build --release --bin 1up && export PATH="$PWD/target/release:$PATH"
1up --version   # must be >= 0.1.9

# 2. Provide credentials:
export ANTHROPIC_API_KEY=...

# 3. Standard suite (search), single run, with summary:
just eval-parallel --summary    # or: just eval --summary

# Optional impact suite:
cd evals && bun run eval:impact
```

Run once. If the run fails mid-flight, record the failure mode here instead
of re-running blindly — each full run costs roughly $3-4 in API spend per
side (see pinned baseline below).

## Comparison Target For The Next Run

The pinned Product Proof numbers from `2026-04-19-sonnet-lean-cli.md`
(run 4, both sides forbidden sub-agents): **1up −33% time / −25% cost vs
baseline, 7/7 pass at 0.787 average quality**. That run measured the lean
CLI; the next recorded run measures the `oneup_*` MCP surface, so report
both the absolute numbers and the delta against this baseline, and note the
surface change when comparing.
