# Portable 1up Self-Eval Orchestrator Prompt

Use this prompt with an agent runtime that supports:
- shell commands
- child/sub-agent spawning
- Python

This prompt is intentionally narrow. It compares a baseline child against a 1up-guided child on one selected task from a curated task pack. The children are told to look for a specific set of checkpoints and nothing else.

## Objective

Run a portable, reproducible A/B comparison on the `emdash` repository:
- `baseline`: standard CLI exploration, no `1up`
- `1up`: `1up`-first exploration

The master agent must:
1. clone a pinned repo into a temp directory
2. set up `1up`
3. run one child for `baseline`
4. run one child for `1up`
5. measure each child externally
6. validate each child JSON result strictly
7. aggregate the results with Python
8. print a compact comparison table plus any contract violations

## Setup

Use exactly this repository and commit unless the user explicitly changes them:

```json
{
  "repo_url": "git@github.com:emdash-cms/emdash.git",
  "pinned_commit": "5beb0ddc334deb862ba90cedbf03f052b58e4974",
  "temp_root": "/tmp/1up-portable-self-eval",
  "repo_path": "/tmp/1up-portable-self-eval/emdash"
}
```

Do not create a unique temp directory on each run.

Use exactly:
- `temp_root = /tmp/1up-portable-self-eval`
- `repo_path = /tmp/1up-portable-self-eval/emdash`

Reuse that location across runs.

Setup rules:
- create `temp_root` if needed
- if `repo_path/.git` already exists, reuse the existing clone
- if `repo_path/.git` does not exist, clone into exactly `repo_path`
- after ensuring the repo exists, check out the pinned commit exactly
- do not create additional temp directories for the repo unless the user explicitly changes the path

After checkout:
- run `1up start`
- wait until `1up status -f json <repo_path>` reports:
  - `indexed_files > 0`
  - `total_segments > 0`
  - `index_progress.state` is `complete` or null/absent

Do not start child runs until indexing is ready.

## Curated Task Pack

Use this exact task pack.

For quick iteration, run one selected task.
For a stronger benchmark, run all four selected tasks sequentially.

```json
{
  "quick_iteration_task_ids": [
    "search-stack"
  ],
  "recommended_full_run_task_ids": [
    "search-stack",
    "wordpress-import",
    "plugin-architecture",
    "live-content-query"
  ],
  "tasks": [
    {
      "task_id": "search-stack",
      "task_label": "Search Stack",
      "task_prompt": "Trace how emdash content search is enabled and queried, from a field becoming searchable through to admin search results. Identify the key files involved in each step.",
      "checkpoints": [
        "Find where a field becomes searchable in the admin or schema layer.",
        "Find where that searchability state is persisted or updated in the core schema or registry layer.",
        "Find where search is enabled, rebuilt, or configured in the FTS or search-management layer.",
        "Find where the search API endpoint handles the incoming query.",
        "Find where the admin UI requests and renders the returned search results."
      ],
      "master_only_expected_evidence_fragments": [
        "fts-manager.ts",
        "query.ts",
        "AdminCommandPalette.tsx"
      ],
      "why_this_task_is_hard_for_grep": [
        "The user phrasing spans admin UI, schema persistence, FTS setup, API routing, and result rendering.",
        "Key concepts are expressed with different identifiers such as searchable, search_config, FTS, and command palette.",
        "The correct answer is a cross-file flow, not a single symbol lookup."
      ]
    },
    {
      "task_id": "wordpress-import",
      "task_label": "WordPress Import",
      "task_prompt": "Explain the WordPress import pipeline from the admin wizard through schema preparation, WXR execution, and Gutenberg-to-Portable-Text conversion. Identify the key files involved in each step.",
      "checkpoints": [
        "Find the admin wizard entry point for the WordPress import flow.",
        "Find where schema preparation is performed before import execution.",
        "Find where the WXR import is executed or parsed.",
        "Find where Gutenberg content is converted into Portable Text.",
        "Find where the pipeline hands converted content back into the import flow."
      ],
      "master_only_expected_evidence_fragments": [
        "WordPressImport.tsx",
        "prepare.ts",
        "execute.ts",
        "gutenberg-to-portable-text/src/index.ts"
      ],
      "why_this_task_is_hard_for_grep": [
        "The flow crosses UI, import orchestration, parser/executor, and conversion layers.",
        "The user language mixes WordPress, WXR, and Gutenberg concepts that are unlikely to share one consistent identifier family.",
        "One crucial file is a converter package entrypoint, not an obvious import-specific filename."
      ]
    },
    {
      "task_id": "plugin-architecture",
      "task_label": "Plugin Architecture",
      "task_prompt": "Trace how a sandboxed emdash plugin is registered, capability-gated, loaded into Cloudflare Worker isolation, and given controlled access to content, storage, and network. Identify the key files involved in each step.",
      "checkpoints": [
        "Find where plugins are registered or discovered.",
        "Find where plugin capabilities or hooks are defined or enforced.",
        "Find where plugins are loaded into a Worker or sandbox runtime.",
        "Find where the sandbox wrapper or isolation boundary is applied.",
        "Find where controlled access to content, storage, or network is bridged into the plugin runtime."
      ],
      "master_only_expected_evidence_fragments": [
        "manager.ts",
        "hooks.ts",
        "runner.ts",
        "wrapper.ts",
        "bridge.ts"
      ],
      "why_this_task_is_hard_for_grep": [
        "The task is phrased in platform language such as capability-gated and isolation, while the code may split that behavior across unrelated filenames.",
        "The answer requires following a permissioned handoff across several runtime layers.",
        "The relevant files are conceptually connected, but a literal text search often returns many partial matches and infrastructure noise."
      ]
    },
    {
      "task_id": "live-content-query",
      "task_label": "Live Content Query",
      "task_prompt": "Explain how emdash stores schema in the database and exposes typed live content queries through Astro. Identify the key files involved in each step.",
      "checkpoints": [
        "Find where schema definitions are registered or stored for runtime use.",
        "Find where stored schema is loaded back into the runtime.",
        "Find where content queries are executed against that schema.",
        "Find where Astro live-query integration is configured.",
        "Find where the typed live query surface is exposed back to the application."
      ],
      "master_only_expected_evidence_fragments": [
        "registry.ts",
        "loader.ts",
        "query.ts",
        "live.config.ts"
      ],
      "why_this_task_is_hard_for_grep": [
        "The task blends schema storage, runtime loading, query execution, and framework integration in one trace.",
        "Terms like live and typed may not appear consistently in the storage or loader layers.",
        "The correct answer depends on following a semantic flow across database, runtime, and Astro integration files."
      ]
    }
  ]
}
```

The checkpoints are the only things the children are allowed to investigate.
They must not broaden the task into general architecture exploration.

## Why These Tasks

These tasks were chosen because simple grep tends to fragment on them:
- they span multiple layers with different local vocabularies
- the user phrasing does not map cleanly to a single symbol or filename family
- the right answer is a traced flow, not a bag of isolated matches
- a weaker search strategy often needs more iterative probing to bridge the semantic gaps

## Master Rules

### General

- Do not ask the children to brainstorm.
- Do not ask the children to compare options.
- Do not ask the children to inspect unrelated systems.
- Reject any child output that is not valid JSON.
- Reject any child output missing required keys.
- Reject any child output where `tool_calls_estimated != len(shell_commands)`.

### Timing

Measure time outside the children.

For each child run:
1. record `start_ns` in the master
2. launch child
3. wait for final child output
4. record `end_ns` in the master
5. compute `elapsed_ms = round((end_ns - start_ns) / 1_000_000, 1)`

Do not trust child-reported timing.

### Child Run Order

For each selected task, run sequentially:
1. `baseline`
2. `1up`

Do not run more than one child at a time.

## Child Input Schema

Pass typed JSON into each child. The master may serialize this into the child prompt, but the content must match this shape:

```json
{
  "schema_version": "1",
  "variant": "baseline | 1up",
  "repo_path": "/absolute/path/to/repo",
  "task_id": "search-stack",
  "task_label": "Search Stack",
  "task_prompt": "Trace how emdash content search is enabled and queried, from a field becoming searchable through to admin search results. Identify the key files involved in each step.",
  "checkpoints": [
    "Find where a field becomes searchable in the admin or schema layer.",
    "Find where that searchability state is persisted or updated in the core schema/registry layer.",
    "Find where search is enabled, rebuilt, or configured in the FTS/search-management layer.",
    "Find where the search API endpoint handles the incoming query.",
    "Find where the admin UI requests and renders the returned search results."
  ],
  "rules": {
    "forbid_1up": true,
    "require_1up_first": false
  }
}
```

For the `1up` variant:
- set `forbid_1up` to `false`
- set `require_1up_first` to `true`

Do not pass `master_only_expected_evidence_fragments` into the child prompt.
Those fragments are for master-side grading only.

## Child Output Schema

Each child must return exactly one JSON object with this shape:

```json
{
  "schema_version": "1",
  "status": "ok | error",
  "variant": "baseline | 1up",
  "task_id": "search-stack",
  "answer": "short factual answer",
  "files_cited": [
    "repo/relative/path.ts"
  ],
  "shell_commands": [
    "cd /abs/repo && ..."
  ],
  "tool_calls_estimated": 0,
  "used_1up": false,
  "confidence": 0.0,
  "errors": []
}
```

Required invariants:
- output must be JSON only
- no markdown fences
- `tool_calls_estimated == len(shell_commands)`
- `confidence` must be between `0` and `1`
- `files_cited` must contain repo-relative paths only

## Baseline Child Template

Use this exact child template for `baseline`, filling in the JSON values:

```text
You are a child worker in a portable code-search self-eval.
Output JSON only. No markdown fences. No prose before or after the JSON.

Parse this input JSON and follow it exactly.
INPUT_JSON:
{{INPUT_JSON}}

Execution rules:
- Work only in `{{repo_path}}`.
- Process the checkpoints in order.
- Investigate only the listed checkpoints.
- Do not explore anything outside those checkpoints.
- Stop as soon as you have enough evidence to answer the five checkpoints.
- For each checkpoint, find only the minimal evidence needed to resolve it.
- Do not inspect tests, docs, benchmarks, or unrelated features unless a checkpoint directly requires them.
- Do not use the `1up` command.
- Use standard CLI tools only.
- Every shell command must start with `cd {{repo_path}} && ...`.
- Keep an incremental command log and return it exactly as `shell_commands` in execution order.
- Before emitting final JSON, recount your final `shell_commands` list and set `tool_calls_estimated` to that exact integer.
- `tool_calls_estimated` must equal the number of shell commands you executed.
- `used_1up` must be `false`.
- `files_cited` must include only files directly used as evidence in your answer.
- `status` must be `ok` unless blocked.
- Be concise and factual.

Return exactly one JSON object with keys:
schema_version,status,variant,task_id,answer,files_cited,shell_commands,tool_calls_estimated,used_1up,confidence,errors
```

## 1up Child Template

Use this exact child template for `1up`, filling in the JSON values:

```text
You are a child worker in a portable code-search self-eval.
Output JSON only. No markdown fences. No prose before or after the JSON.

Parse this input JSON and follow it exactly.
INPUT_JSON:
{{INPUT_JSON}}

Execution rules:
- Work only in `{{repo_path}}`.
- Process the checkpoints in order.
- Investigate only the listed checkpoints.
- Do not explore anything outside those checkpoints.
- Stop as soon as you have enough evidence to answer the five checkpoints.
- For each checkpoint, find only the minimal evidence needed to resolve it.
- Do not inspect tests, docs, benchmarks, or unrelated features unless a checkpoint directly requires them.
- After any optional index check, your first exploration command must use `1up`.
- Pick tools by what you know: `1up search` for conceptual exploration, `1up symbol -r` or `grep` for known symbol/keyword lookup, `grep` when you need all instances of a keyword.
- `1up symbol` uses exact matching by default. Use `1up symbol --fuzzy` only if exact match returns nothing and you want approximate results.
- Use `grep` freely for exhaustive keyword searches — it is not a fallback, it is the right tool when you know the keyword.
- Every shell command must start with `cd {{repo_path}} && ...`.
- Keep an incremental command log and return it exactly as `shell_commands` in execution order.
- Before emitting final JSON, recount your final `shell_commands` list and set `tool_calls_estimated` to that exact integer.
- `tool_calls_estimated` must equal the number of shell commands you executed.
- `used_1up` must be `true`.
- `files_cited` must include only files directly used as evidence in your answer.
- `status` must be `ok` unless blocked.
- Be concise and factual.

Return exactly one JSON object with keys:
schema_version,status,variant,task_id,answer,files_cited,shell_commands,tool_calls_estimated,used_1up,confidence,errors
```

## Master Validation

After each child returns:

1. parse JSON
2. validate required keys
3. validate invariants
4. attach external `elapsed_ms`
5. record any violations

Mark a run invalid if any of the following are true:
- invalid JSON
- missing required keys
- `tool_calls_estimated != len(shell_commands)`
- `baseline.used_1up == true`
- `1up.used_1up == false`
- `1up` does not use `1up` in its first exploration step

## Python Aggregation

Once both child results are collected, use Python to produce a compact summary object and a readable table.

Aggregate at least:
- `variant`
- `status`
- `elapsed_ms`
- `tool_calls_estimated`
- `files_cited_count`
- `correctness_score`
- `schema_valid`
- `violations`

Compute `correctness_score` per task by checking whether the child answer or `files_cited` references the current task's `master_only_expected_evidence_fragments`.

Use a simple score:
- `found / len(master_only_expected_evidence_fragments)`

## Final Output

The master should print:

1. the repo path and pinned commit used
2. one row per variant
3. the winner for:
   - correctness
   - speed
   - fewest tool calls
4. all contract violations

If a child violates the contract, do not hide it. Report it explicitly.

## First Iteration Notes

This first version optimizes for:
- strict contracts
- narrow child scope
- external timing
- easy manual reruns
- semantically difficult traced-flow tasks

It does not yet optimize for:
- the best possible 1up prompt efficiency
- multi-task loops
- resumable result files
- hidden grading beyond the simple evidence-fragment check
