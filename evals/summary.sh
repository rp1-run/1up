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
    task, process_seconds, status, log_path = line.split("\t", 3)
    content = ansi.sub("", pathlib.Path(log_path).read_text(errors="replace"))
    matches = list(metric.finditer(content))
    agents = []
    for match in matches[:2]:
        agents.append(
            f"{match['duration']}, {match['cost']}, {match['turns']}t, {match['tokens'].strip()}"
        )
    while len(agents) < 2:
        agents.append("metrics unavailable")
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
