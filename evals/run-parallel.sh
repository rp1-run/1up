#!/usr/bin/env bash
# Run each eval test case in parallel, each with its own promptfoo process.
# This avoids the promptfoo bug where concurrent test results get swapped.

set -uo pipefail

PROMPTFOO="node_modules/.bin/promptfoo"

SEARCH_CONFIG="suites/1up-search/evals.yaml"
SEARCH_TESTS=("Search Stack" "WordPress Import" "Plugin Architecture" "Live Content Query")

IMPACT_CONFIG="suites/1up-impact/evals.yaml"
IMPACT_TESTS=("FTSManager Impact" "Schema Registry Impact" "Plugin Runner Impact")

PIDS=()
LABELS=()
TMPDIR=$(mktemp -d)

TOTAL=$(( ${#SEARCH_TESTS[@]} + ${#IMPACT_TESTS[@]} ))
echo "Running $TOTAL tests in parallel (${#SEARCH_TESTS[@]} search + ${#IMPACT_TESTS[@]} impact)..."
echo

for i in "${!SEARCH_TESTS[@]}"; do
  LOG="$TMPDIR/search-$i.log"
  $PROMPTFOO eval -c "$SEARCH_CONFIG" --filter-pattern "^${SEARCH_TESTS[$i]}$" > "$LOG" 2>&1 &
  PIDS+=($!)
  LABELS+=("${SEARCH_TESTS[$i]}")
  echo "  Started: ${SEARCH_TESTS[$i]} (pid $!)"
done

for i in "${!IMPACT_TESTS[@]}"; do
  LOG="$TMPDIR/impact-$i.log"
  $PROMPTFOO eval -c "$IMPACT_CONFIG" --filter-pattern "^${IMPACT_TESTS[$i]}$" > "$LOG" 2>&1 &
  PIDS+=($!)
  LABELS+=("${IMPACT_TESTS[$i]}")
  echo "  Started: ${IMPACT_TESTS[$i]} (pid $!)"
done

echo
echo "Waiting for all tests to complete..."

FAILED=0
for i in "${!PIDS[@]}"; do
  if wait "${PIDS[$i]}" 2>/dev/null; then
    echo "  ✓ ${LABELS[$i]}"
  else
    echo "  ✗ ${LABELS[$i]}"
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
