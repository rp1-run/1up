#!/usr/bin/env bash
# Run each eval test case in parallel, each with its own promptfoo process.
# This avoids the promptfoo bug where concurrent test results get swapped.

set -uo pipefail

CONFIG="suites/1up-search/evals.yaml"
PROMPTFOO="node_modules/.bin/promptfoo"
TESTS=("Search Stack" "WordPress Import" "Plugin Architecture" "Live Content Query")
PIDS=()
TMPDIR=$(mktemp -d)

echo "Running ${#TESTS[@]} tests in parallel..."
echo

for i in "${!TESTS[@]}"; do
  LOG="$TMPDIR/eval-$i.log"
  $PROMPTFOO eval -c "$CONFIG" --filter-pattern "^${TESTS[$i]}$" > "$LOG" 2>&1 &
  PIDS+=($!)
  echo "  Started: ${TESTS[$i]} (pid $!)"
done

echo
echo "Waiting for all tests to complete..."

FAILED=0
for i in "${!PIDS[@]}"; do
  if wait "${PIDS[$i]}" 2>/dev/null; then
    echo "  ✓ ${TESTS[$i]}"
  else
    echo "  ✗ ${TESTS[$i]}"
    FAILED=$((FAILED + 1))
  fi
done

echo
if [ $FAILED -eq 0 ]; then
  echo "All tests passed."
else
  echo "$FAILED test(s) had failures."
fi

rm -rf "$TMPDIR"
