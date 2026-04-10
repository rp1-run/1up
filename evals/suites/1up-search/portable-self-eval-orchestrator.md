# Portable 1up Self-Eval Orchestrator Prompt

Use this prompt with an agent runtime that supports:
- shell commands
- child/sub-agent spawning
- Python

This prompt is intentionally narrow. It compares a baseline child against a 1up-guided child on one fixed investigation task. The children are told to look for a specific set of checkpoints and nothing else.

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

## Fixed Task Pack

Use this exact task pack for the first iteration:

```json
{
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
  "master_only_expected_file_basenames": [
    "fts-manager.ts",
    "query.ts",
    "AdminCommandPalette.tsx"
  ]
}
```

The checkpoints are the only things the children are allowed to investigate.
They must not broaden the task into general architecture exploration.

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

Run sequentially:
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
    "require_1up_first": false,
    "max_shell_commands": 12
  }
}
```

For the `1up` variant:
- set `forbid_1up` to `false`
- set `require_1up_first` to `true`

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
- Investigate only the listed checkpoints.
- Do not explore anything outside those checkpoints.
- Stop as soon as you have enough evidence to answer the five checkpoints.
- Do not use the `1up` command.
- Use standard CLI tools only.
- Every shell command must start with `cd {{repo_path}} && ...`.
- Keep an incremental command log and return it exactly as `shell_commands` in execution order.
- Use at most `{{rules.max_shell_commands}}` shell commands.
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
- Investigate only the listed checkpoints.
- Do not explore anything outside those checkpoints.
- Stop as soon as you have enough evidence to answer the five checkpoints.
- After any optional index check, your first exploration command must use `1up`.
- Prefer `1up search`, `1up symbol`, and `1up context`.
- Use fallback tools only for exact file inspection or exact string verification.
- Every shell command must start with `cd {{repo_path}} && ...`.
- Keep an incremental command log and return it exactly as `shell_commands` in execution order.
- Use at most `{{rules.max_shell_commands}}` shell commands.
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
- command budget exceeded

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

Compute `correctness_score` by checking whether the child answer or `files_cited` references the master-only expected basenames:
- `fts-manager.ts`
- `query.ts`
- `AdminCommandPalette.tsx`

Use a simple score:
- `found / 3`

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

It does not yet optimize for:
- the best possible 1up prompt efficiency
- multi-task loops
- resumable result files
- hidden grading beyond the simple basename check
