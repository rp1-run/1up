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

rows = []
for line in pathlib.Path(runs_file).read_text().splitlines():
    task, process_seconds, status, log_path, diagnostic_path = line.split("\t", 4)
    if not pathlib.Path(diagnostic_path).is_file():
        raise SystemExit(f"Missing diagnostic output: {diagnostic_path}")
    content = ansi.sub("", pathlib.Path(log_path).read_text(errors="replace"))
    matches = list(metric.finditer(content))
    agents = []
    for match in matches[:2]:
        agents.append(
            f"{match['duration']}, {match['cost']}, {match['turns']}t, {match['tokens'].strip()}"
        )
    while len(agents) < 2:
        agents.append(None)

    diagnostic = json.loads(pathlib.Path(diagnostic_path).read_text())
    diagnostic_results = diagnostic.get("results", {}).get("results", [])
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
    rows.append((task, status, int(process_seconds), *agents))

width = max(len(row[0]) for row in rows)
print(f"\n{provider.upper()} parallel eval summary")
for task, status, seconds, oneup, baseline in rows:
    print(f"  {task:<{width}}  {status.upper():4}  process {seconds:>4}s")
    print(f"    1up-agent: {oneup}")
    print(f"    baseline:  {baseline}")
print(f"\n  Total process-seconds: {sum(row[2] for row in rows)}")
print(f"  Result directory: {pathlib.Path(runs_file).parent}")
PYEOF
