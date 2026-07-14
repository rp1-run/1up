#!/usr/bin/env bash
# Usage: ./summary.sh [claude|luna]
# Summarizes the durable logs written by run-parallel.sh.

set -uo pipefail

PROVIDER="${1:-claude}"
case "$PROVIDER" in
  claude|luna) ;;
  *)
    echo "Usage: $0 [claude|luna]" >&2
    exit 2
    ;;
esac

RESULT_DIR="results/latest-${PROVIDER}"
RUNS_FILE="$RESULT_DIR/runs.tsv"

if [ ! -f "$RUNS_FILE" ]; then
  echo "No ${PROVIDER} parallel eval results found at $RUNS_FILE."
  exit 1
fi

python3 - "$PROVIDER" "$RUNS_FILE" << 'PYEOF'
import json
import pathlib
import re
import sys

provider, runs_file = sys.argv[1:]
ansi = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]")
metric = re.compile(
    r"(?P<duration>\d+s|duration n/a)\s*\|\s*"
    r"(?P<cost>\$\d+(?:\.\d+)?|cost n/a)\s*\|\s*"
    r"(?P<turns>\d+) turns?\s*\|\s*tokens (?P<tokens>[^\n│]+)"
)


def extract_agents(log_path, diagnostic_path):
    """Best-effort per-agent metrics; missing evidence never drops the row."""
    agents = [None, None]
    log_file = pathlib.Path(log_path)
    if log_file.is_file():
        content = ansi.sub("", log_file.read_text(errors="replace"))
        for index, match in enumerate(list(metric.finditer(content))[:2]):
            agents[index] = (
                f"{match['duration']}, {match['cost']}, "
                f"{match['turns']}t, {match['tokens'].strip()}"
            )

    diagnostic_results = []
    diagnostic_file = pathlib.Path(diagnostic_path)
    if diagnostic_file.is_file():
        try:
            diagnostic = json.loads(diagnostic_file.read_text())
            diagnostic_results = diagnostic.get("results", {}).get("results", [])
        except (ValueError, OSError):
            diagnostic_results = []

    by_label = {
        result.get("provider", {}).get("label"): result
        for result in diagnostic_results
    }
    for index, label in enumerate(("1up-agent", "baseline-agent")):
        if agents[index] is not None:
            continue
        result = by_label.get(label)
        if not result:
            agents[index] = "metrics unavailable"
            continue
        try:
            raw = json.loads(result.get("response", {}).get("raw", "{}"))
        except (TypeError, json.JSONDecodeError):
            raw = {}
        usage = raw.get("usage", {})
        latency = result.get("latencyMs")
        cost = result.get("cost")
        turns = 1 if isinstance(raw.get("items"), list) else 0
        duration = f"{round(latency / 1000)}s" if isinstance(latency, (int, float)) else "duration n/a"
        cost_text = f"${cost:.2f}" if isinstance(cost, (int, float)) else "cost n/a"
        tokens = (
            f"in:{usage.get('input_tokens', 0):,} "
            f"out:{usage.get('output_tokens', 0):,} "
            f"cached:{usage.get('cached_input_tokens', 0):,} "
            f"reasoning:{usage.get('reasoning_output_tokens', 0):,}"
        )
        agents[index] = f"{duration}, {cost_text}, {turns}t, {tokens}"
    return agents


def parse_row(line):
    """Normalize a runs.tsv row to v2 fields, tolerating the legacy v1 shape."""
    fields = line.split("\t")
    if len(fields) == 8:
        attempt_id, label, role, retry_of, duration_s, status, log_path, diagnostic_path = fields
    elif len(fields) == 5:
        # Legacy v1 (pre-lineage) archived TSVs: treat every row as an aggregate.
        label, duration_s, status, log_path, diagnostic_path = fields
        attempt_id, role, retry_of = "-", "aggregate", "-"
    else:
        raise SystemExit(f"Unrecognized runs.tsv row ({len(fields)} columns): {line}")
    return {
        "attempt_id": attempt_id,
        "label": label,
        "role": role,
        "retry_of": retry_of,
        "seconds": int(duration_s),
        "status": status,
        "agents": extract_agents(log_path, diagnostic_path),
    }


rows = [
    parse_row(line)
    for line in pathlib.Path(runs_file).read_text().splitlines()
    if line.strip()
]

# Group by label in first-seen order so lineage renders under its aggregate.
order = []
groups = {}
for row in rows:
    if row["label"] not in groups:
        groups[row["label"]] = []
        order.append(row["label"])
    groups[row["label"]].append(row)

width = max(len(label) for label in order)


def short(attempt_id):
    return attempt_id[:8] if attempt_id and attempt_id != "-" else attempt_id


print(f"\n{provider.upper()} parallel eval summary")
for label in order:
    group = groups[label]
    aggregates = [row for row in group if row["role"] == "aggregate"]
    retries = [row for row in group if row["role"] != "aggregate"]

    # Aggregate rows are the durable record and are never dropped, pass or fail.
    for row in aggregates:
        oneup, baseline = row["agents"]
        print(f"  {label:<{width}}  {row['status'].upper():4}  process {row['seconds']:>4}s")
        print(f"    1up-agent: {oneup}")
        print(f"    baseline:  {baseline}")

    for row in retries:
        oneup, baseline = row["agents"]
        lineage = f"{row['role']} [{short(row['attempt_id'])}] retry_of {short(row['retry_of'])}"
        print(f"    ↳ {lineage}  {row['status'].upper():4}  process {row['seconds']:>4}s")
        print(f"      1up-agent: {oneup}")
        print(f"      baseline:  {baseline}")

print(f"\n  Total process-seconds: {sum(row['seconds'] for row in rows)}")
print(f"  Result directory: {pathlib.Path(runs_file).parent}")
PYEOF
