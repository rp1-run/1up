#!/usr/bin/env bash
# Run each eval test case in parallel, each with its own promptfoo process.
# This avoids the promptfoo bug where concurrent test results get swapped.
#
# Usage:
#   ./run-parallel.sh [claude|luna]
#       Aggregate run: every case runs once with role=aggregate and a fresh
#       immutable attempt_id. Previous latest results are archived first.
#
#   ./run-parallel.sh [claude|luna] --retry <label> --retry-of <attempt_id> \
#       --role planned-repeat|diagnostic
#       Single-case retry: appends one lineage row into the existing latest
#       results without archiving. role=aggregate is refused when an aggregate
#       row for the label already exists (no silent promotion).
#
# runs.tsv is append-only, v2 columns (tab-separated):
#   attempt_id  label  role  retry_of  duration_s  status  log_path  diagnostic_path

set -uo pipefail

PROMPTFOO_BIN="${PROMPTFOO_BIN:-node_modules/.bin/promptfoo}"

usage() {
  echo "Usage: $0 [claude|luna] [--retry <label> --retry-of <attempt_id> --role planned-repeat|diagnostic]" >&2
}

PROVIDER="claude"
if [ "$#" -gt 0 ] && [ "${1#--}" = "$1" ]; then
  PROVIDER="$1"
  shift
fi

RETRY_LABEL=""
RETRY_OF=""
ROLE=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --retry)
      shift
      RETRY_LABEL="${1:-}"
      ;;
    --retry-of)
      shift
      RETRY_OF="${1:-}"
      ;;
    --role)
      shift
      ROLE="${1:-}"
      ;;
    *)
      echo "Unknown argument: $1" >&2
      usage
      exit 2
      ;;
  esac
  shift
done

case "$PROVIDER" in
  claude)
    CONFIG_SUFFIX=""
    ;;
  luna)
    CONFIG_SUFFIX="-luna"
    ;;
  *)
    usage
    exit 2
    ;;
esac

RETRY_MODE=0
if [ -n "$RETRY_LABEL" ] || [ -n "$RETRY_OF" ] || [ -n "$ROLE" ]; then
  RETRY_MODE=1
fi

mint_attempt_id() {
  if command -v uuidgen >/dev/null 2>&1; then
    uuidgen | tr '[:upper:]' '[:lower:]'
  elif [ -r /proc/sys/kernel/random/uuid ]; then
    cat /proc/sys/kernel/random/uuid
  else
    printf '%08x-%04x-%04x-%04x-%04x%08x\n' \
      "$((RANDOM << 15 | RANDOM))" "$((RANDOM))" "$((RANDOM))" \
      "$((RANDOM))" "$((RANDOM))" "$((RANDOM << 15 | RANDOM))"
  fi
}

SEARCH_CONFIG="suites/1up-search/evals${CONFIG_SUFFIX}.yaml"
SEARCH_TESTS=("Search Stack" "WordPress Import" "Plugin Architecture" "Live Content Query")

IMPACT_CONFIG="suites/1up-impact/evals${CONFIG_SUFFIX}.yaml"
IMPACT_TESTS=("FTSManager Impact" "Schema Registry Impact" "Plugin Runner Impact")

CASE_LABELS=()
CASE_CONFIGS=()
CASE_SLUGS=()
for i in "${!SEARCH_TESTS[@]}"; do
  CASE_LABELS+=("${SEARCH_TESTS[$i]}")
  CASE_CONFIGS+=("$SEARCH_CONFIG")
  CASE_SLUGS+=("search-$i")
done
for i in "${!IMPACT_TESTS[@]}"; do
  CASE_LABELS+=("${IMPACT_TESTS[$i]}")
  CASE_CONFIGS+=("$IMPACT_CONFIG")
  CASE_SLUGS+=("impact-$i")
done

RESULT_DIR="results/latest-${PROVIDER}"
ARCHIVE_ROOT="results/archive-${PROVIDER}"
RUNS_FILE="$RESULT_DIR/runs.tsv"

append_row() {
  # attempt_id label role retry_of duration_s status log_path diagnostic_path
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$1" "$2" "$3" "$4" "$5" "$6" "$7" "$8" >> "$RUNS_FILE"
}

# Aggregate runs start from a clean archived slate; retries append in place.
if [ "$RETRY_MODE" -eq 0 ]; then
  if [ -d "$RESULT_DIR" ] && [ -n "$(find "$RESULT_DIR" -mindepth 1 -maxdepth 1 -print -quit)" ]; then
    ARCHIVE_DIR="$ARCHIVE_ROOT/$(date -u +%Y%m%dT%H%M%SZ)-$$"
    mkdir -p "$ARCHIVE_ROOT"
    mv "$RESULT_DIR" "$ARCHIVE_DIR"
    echo "Archived previous results: $ARCHIVE_DIR"
  fi
fi
mkdir -p "$RESULT_DIR"

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

PIDS=()
cleanup() {
  EXIT_STATUS=$?
  trap - EXIT HUP INT TERM

  for PID in "${PIDS[@]:-}"; do
    if [ -n "$PID" ]; then
      kill "$PID" 2>/dev/null || true
    fi
  done
  for PID in "${PIDS[@]:-}"; do
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

if [ "$RETRY_MODE" -eq 1 ]; then
  if [ -z "$RETRY_LABEL" ] || [ -z "$RETRY_OF" ] || [ -z "$ROLE" ]; then
    echo "Retry mode requires --retry <label> --retry-of <attempt_id> --role planned-repeat|diagnostic" >&2
    usage
    exit 2
  fi

  case "$ROLE" in
    planned-repeat|diagnostic)
      ;;
    aggregate)
      if [ -f "$RUNS_FILE" ] && awk -F '\t' -v lbl="$RETRY_LABEL" \
        '$2 == lbl && $3 == "aggregate" { found = 1 } END { exit !found }' "$RUNS_FILE"; then
        echo "Refusing role=aggregate: an aggregate row for \"$RETRY_LABEL\" already exists (no silent promotion)." >&2
        exit 3
      fi
      ;;
    *)
      echo "Invalid --role: $ROLE (expected planned-repeat, diagnostic, or aggregate)" >&2
      exit 2
      ;;
  esac

  CASE_INDEX=-1
  for i in "${!CASE_LABELS[@]}"; do
    if [ "${CASE_LABELS[$i]}" = "$RETRY_LABEL" ]; then
      CASE_INDEX=$i
      break
    fi
  done
  if [ "$CASE_INDEX" -lt 0 ]; then
    echo "Unknown case label: $RETRY_LABEL" >&2
    exit 2
  fi

  ATTEMPT_ID=$(mint_attempt_id)
  CONFIG="${CASE_CONFIGS[$CASE_INDEX]}"
  LOG="$RESULT_DIR/retry-$ATTEMPT_ID.log"
  DIAGNOSTIC="$RESULT_DIR/retry-$ATTEMPT_ID.json"
  CONFIG_DIR="$STATE_ROOT/retry-$ATTEMPT_ID"
  mkdir -p "$CONFIG_DIR"

  echo "Retrying \"$RETRY_LABEL\" (role=$ROLE, attempt_id=$ATTEMPT_ID, retry_of=$RETRY_OF)"
  STARTED_AT=$(date +%s)
  STATUS="pass"
  if PROMPTFOO_CONFIG_DIR="$CONFIG_DIR" "$PROMPTFOO_BIN" eval -c "$CONFIG" --filter-pattern "^${RETRY_LABEL}$" --output "$DIAGNOSTIC" > "$LOG" 2>&1; then
    echo "  ✓ $RETRY_LABEL"
  else
    echo "  ✗ $RETRY_LABEL"
    STATUS="fail"
  fi
  DURATION=$(( $(date +%s) - STARTED_AT ))
  append_row "$ATTEMPT_ID" "$RETRY_LABEL" "$ROLE" "$RETRY_OF" "$DURATION" "$STATUS" "$LOG" "$DIAGNOSTIC"

  if [ "$STATUS" = "pass" ]; then
    exit 0
  fi
  exit 1
fi

LABELS=()
ATTEMPTS=()
LOGS=()
DIAGNOSTICS=()
STARTED_AT=()

TOTAL=${#CASE_LABELS[@]}
echo "Running $TOTAL ${PROVIDER} tests in parallel (${#SEARCH_TESTS[@]} search + ${#IMPACT_TESTS[@]} impact)..."
echo "Logs: $RESULT_DIR"
echo

for i in "${!CASE_LABELS[@]}"; do
  ATTEMPT_ID=$(mint_attempt_id)
  LOG="$RESULT_DIR/${CASE_SLUGS[$i]}.log"
  DIAGNOSTIC="$RESULT_DIR/${CASE_SLUGS[$i]}.json"
  CONFIG_DIR="$STATE_ROOT/${CASE_SLUGS[$i]}"
  mkdir -p "$CONFIG_DIR"
  PROMPTFOO_CONFIG_DIR="$CONFIG_DIR" "$PROMPTFOO_BIN" eval -c "${CASE_CONFIGS[$i]}" --filter-pattern "^${CASE_LABELS[$i]}$" --output "$DIAGNOSTIC" > "$LOG" 2>&1 &
  PIDS+=($!)
  LABELS+=("${CASE_LABELS[$i]}")
  ATTEMPTS+=("$ATTEMPT_ID")
  LOGS+=("$LOG")
  DIAGNOSTICS+=("$DIAGNOSTIC")
  STARTED_AT+=("$(date +%s)")
  echo "  Started: ${CASE_LABELS[$i]} (pid $!)"
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
  PIDS[i]=""
  DURATION=$(( $(date +%s) - STARTED_AT[i] ))
  append_row "${ATTEMPTS[$i]}" "${LABELS[$i]}" "aggregate" "-" "$DURATION" "$STATUS" "${LOGS[$i]}" "${DIAGNOSTICS[$i]}"
done

echo
if [ $FAILED -eq 0 ]; then
  echo "All tests passed."
else
  echo "$FAILED test(s) had failures."
  exit 1
fi
