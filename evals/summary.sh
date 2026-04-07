#!/usr/bin/env bash
# Usage: ./summary.sh
# Shows a clean comparison table from the latest eval run(s).
# Handles both single-process and parallel runs by reading all recent logs.

# Find logs from the last 15 minutes
RECENT_LOGS=$(find ~/.promptfoo/logs -name 'promptfoo-debug-*.log' -type f -mmin -15 | sort)

if [ -z "$RECENT_LOGS" ]; then
  echo "No recent eval logs found (last 15 minutes)."
  exit 1
fi

python3 - $RECENT_LOGS << 'PYEOF'
import json, re, sys

kw_map = [("search is enabled", "Search Stack"), ("WordPress import", "WordPress Import"),
          ("sandboxed", "Plugin Arch"), ("schema in the database", "Live Content")]
tasks_order = ["Search Stack", "WordPress Import", "Plugin Arch", "Live Content"]

from collections import defaultdict
by_task = defaultdict(dict)

for logfile in sys.argv[1:]:
    with open(logfile) as f:
        content = f.read()
    prompts = list(re.finditer(r'Calling Claude Agent SDK: ({.*?})\n', content))
    responses = list(re.finditer(r'Claude Agent SDK response: ({.*?})\n', content))
    for p, r in zip(prompts, responses):
        pd = json.loads(p.group(1))
        rd = json.loads(r.group(1))
        pt = pd.get("prompt", "")
        agent = "1up" if "1up search" in pt else "baseline"
        task = "?"
        for kw, label in kw_map:
            if kw in pt:
                task = label
                break
        by_task[task][agent] = (rd.get("num_turns",0), rd.get("duration_ms",0)/1000, rd.get("total_cost_usd",0))

def fmt(t, d, c):
    return f"{d:>3.0f}s  ${c:.2f}  {t:>2}t"

W = 18
A = 16

print()
print(f"  {'Task':<{W}}  {'1up-agent':^{A}}  {'baseline':^{A}}  Winner")
print(f"  {'─'*W}  {'─'*A}  {'─'*A}  ──────")
for task in tasks_order:
    d = by_task.get(task, {})
    one = d.get("1up", (0,0,0))
    bas = d.get("baseline", (0,0,0))
    winner = "← 1up" if one[1] < bas[1] else "baseline →"
    print(f"  {task:<{W}}  {fmt(*one):>{A}}  {fmt(*bas):>{A}}  {winner}")

one_dur = sum(by_task[t].get("1up",(0,0,0))[1] for t in tasks_order)
one_cost = sum(by_task[t].get("1up",(0,0,0))[2] for t in tasks_order)
bas_dur = sum(by_task[t].get("baseline",(0,0,0))[1] for t in tasks_order)
bas_cost = sum(by_task[t].get("baseline",(0,0,0))[2] for t in tasks_order)

print(f"  {'─'*W}  {'─'*A}  {'─'*A}  ──────")
print(f"  {'TOTAL':<{W}}  {f'{one_dur:.0f}s  ${one_cost:.2f}':>{A}}  {f'{bas_dur:.0f}s  ${bas_cost:.2f}':>{A}}")

if bas_dur > 0 and bas_cost > 0:
    td = (one_dur - bas_dur) / bas_dur * 100
    tc = (one_cost - bas_cost) / bas_cost * 100
    print()
    print(f"  1up vs baseline: {td:+.0f}% time, {tc:+.0f}% cost")
print()
PYEOF
