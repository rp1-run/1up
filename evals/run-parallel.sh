#!/usr/bin/env bash
# Run each eval test case in parallel, each with its own promptfoo process.
# This avoids the promptfoo bug where concurrent test results get swapped.
# Usage: ./run-parallel.sh [claude|luna]

set -uo pipefail

PROMPTFOO_BIN="${PROMPTFOO_BIN:-node_modules/.bin/promptfoo}"
PROVIDER="${1:-claude}"

case "$PROVIDER" in
  claude)
    CONFIG_SUFFIX=""
    ;;
  luna)
    CONFIG_SUFFIX="-luna"
    ;;
  *)
    echo "Usage: $0 [claude|luna]" >&2
    exit 2
    ;;
esac

SEARCH_CONFIG="suites/1up-search/evals${CONFIG_SUFFIX}.yaml"
SEARCH_TESTS=("Search Stack" "WordPress Import" "Plugin Architecture" "Live Content Query")

IMPACT_CONFIG="suites/1up-impact/evals${CONFIG_SUFFIX}.yaml"
IMPACT_TESTS=("FTSManager Impact" "Schema Registry Impact" "Plugin Runner Impact")

PIDS=()
LABELS=()
LOGS=()
STARTED_AT=()
RESULT_DIR="results/latest-${PROVIDER}"
mkdir -p "$RESULT_DIR"
rm -f "$RESULT_DIR"/*.log "$RESULT_DIR"/runs.tsv

STATE_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/1up-promptfoo-${PROVIDER}.XXXXXX") || {
  echo "Failed to create temporary Promptfoo state directory." >&2
  exit 1
}
STATE_ROOT=$(cd "$STATE_ROOT" && pwd -P)
RESULT_DIR_ABS=$(cd "$RESULT_DIR" && pwd -P)
case "$STATE_ROOT" in
  "$RESULT_DIR_ABS"|"$RESULT_DIR_ABS"/*)
    rm -rf -- "$STATE_ROOT"
    echo "Temporary Promptfoo state directory must be outside durable results." >&2
    exit 1
    ;;
esac

cleanup() {
  EXIT_STATUS=$?
  trap - EXIT HUP INT TERM

  for PID in "${PIDS[@]}"; do
    if [ -n "$PID" ]; then
      kill "$PID" 2>/dev/null || true
    fi
  done
  for PID in "${PIDS[@]}"; do
    if [ -n "$PID" ]; then
      wait "$PID" 2>/dev/null || true
    fi
  done

  rm -rf -- "$STATE_ROOT"
  exit "$EXIT_STATUS"
}

trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

TOTAL=$(( ${#SEARCH_TESTS[@]} + ${#IMPACT_TESTS[@]} ))
echo "Running $TOTAL ${PROVIDER} tests in parallel (${#SEARCH_TESTS[@]} search + ${#IMPACT_TESTS[@]} impact)..."
echo "Logs: $RESULT_DIR"
echo

for i in "${!SEARCH_TESTS[@]}"; do
  LOG="$RESULT_DIR/search-$i.log"
  CONFIG_DIR="$STATE_ROOT/search-$i"
  mkdir -p "$CONFIG_DIR"
  PROMPTFOO_CONFIG_DIR="$CONFIG_DIR" "$PROMPTFOO_BIN" eval -c "$SEARCH_CONFIG" --filter-pattern "^${SEARCH_TESTS[$i]}$" > "$LOG" 2>&1 &
  PIDS+=($!)
  LABELS+=("${SEARCH_TESTS[$i]}")
  LOGS+=("$LOG")
  STARTED_AT+=("$(date +%s)")
  echo "  Started: ${SEARCH_TESTS[$i]} (pid $!)"
done

for i in "${!IMPACT_TESTS[@]}"; do
  LOG="$RESULT_DIR/impact-$i.log"
  CONFIG_DIR="$STATE_ROOT/impact-$i"
  mkdir -p "$CONFIG_DIR"
  PROMPTFOO_CONFIG_DIR="$CONFIG_DIR" "$PROMPTFOO_BIN" eval -c "$IMPACT_CONFIG" --filter-pattern "^${IMPACT_TESTS[$i]}$" > "$LOG" 2>&1 &
  PIDS+=($!)
  LABELS+=("${IMPACT_TESTS[$i]}")
  LOGS+=("$LOG")
  STARTED_AT+=("$(date +%s)")
  echo "  Started: ${IMPACT_TESTS[$i]} (pid $!)"
done

echo
echo "Waiting for all tests to complete..."

FAILED=0
for i in "${!PIDS[@]}"; do
  STATUS="pass"
  if wait "${PIDS[$i]}" 2>/dev/null; then
    echo "  ✓ ${LABELS[$i]}"
  else
    echo "  ✗ ${LABELS[$i]}"
    FAILED=$((FAILED + 1))
    STATUS="fail"
  fi
  PIDS[$i]=""
  DURATION=$(( $(date +%s) - STARTED_AT[$i] ))
  printf '%s\t%s\t%s\t%s\n' "${LABELS[$i]}" "$DURATION" "$STATUS" "${LOGS[$i]}" >> "$RESULT_DIR/runs.tsv"
done

echo
if [ $FAILED -eq 0 ]; then
  echo "All tests passed."
else
  echo "$FAILED test(s) had failures."
  exit 1
fi
