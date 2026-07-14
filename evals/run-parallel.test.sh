#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
WORK_DIR=$(mktemp -d "${TMPDIR:-/tmp}/1up-run-parallel-test.XXXXXX")

cleanup() {
  rm -rf -- "$WORK_DIR"
}
trap cleanup EXIT HUP INT TERM

mkdir -p "$WORK_DIR/tmp"
FAKE_PROMPTFOO="$WORK_DIR/fake-promptfoo"

cat > "$FAKE_PROMPTFOO" <<'EOF'
#!/usr/bin/env bash
set -eu

if [ -z "${PROMPTFOO_CONFIG_DIR:-}" ]; then
  echo "PROMPTFOO_CONFIG_DIR was not set" >&2
  exit 20
fi

FILTER=""
OUTPUT=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --filter-pattern)
      shift
      FILTER="${1:-}"
      ;;
    --output)
      shift
      OUTPUT="${1:-}"
      ;;
  esac
  shift
done

if [ -z "$OUTPUT" ]; then
  echo "Promptfoo diagnostic output was not configured" >&2
  exit 21
fi

printf 'PROMPTFOO_CONFIG_DIR=%s\n' "$PROMPTFOO_CONFIG_DIR"
printf 'filter=%s\n' "$FILTER"
printf 'fake-state\n' > "$PROMPTFOO_CONFIG_DIR/state.txt"
printf '{"filter":"%s","results":[]}\n' "$FILTER" > "$OUTPUT"

if [ -n "${FAKE_PROMPTFOO_FAIL_FILTER:-}" ] && [ "$FILTER" = "$FAKE_PROMPTFOO_FAIL_FILTER" ]; then
  echo "fake Promptfoo failure" >&2
  exit 19
fi

echo "fake Promptfoo success"
EOF
chmod +x "$FAKE_PROMPTFOO"

fail() {
  echo "run-parallel regression failed: $*" >&2
  exit 1
}

RESULT_DIR="$WORK_DIR/results/latest-luna"

assert_artifacts() {
  EXPECTED_FAILURES=$1
  DIRS_FILE="$WORK_DIR/config-dirs.txt"

  [ -f "$RESULT_DIR/runs.tsv" ] || fail "runs.tsv was not retained"
  [ "$(wc -l < "$RESULT_DIR/runs.tsv" | tr -d ' ')" = "7" ] || fail "runs.tsv did not contain seven rows"

  # v2 schema: attempt_id, label, role, retry_of, duration_s, status, log_path, diagnostic_path
  [ "$(awk -F '\t' 'NF == 8 { count++ } END { print count + 0 }' "$RESULT_DIR/runs.tsv")" = "7" ] || fail "runs.tsv rows were not v2 (eight columns)"
  [ "$(awk -F '\t' '$6 == "fail" { count++ } END { print count + 0 }' "$RESULT_DIR/runs.tsv")" = "$EXPECTED_FAILURES" ] || fail "unexpected failure count in runs.tsv"
  [ "$(awk -F '\t' '$6 == "pass" { count++ } END { print count + 0 }' "$RESULT_DIR/runs.tsv")" = "$((7 - EXPECTED_FAILURES))" ] || fail "unexpected pass count in runs.tsv"

  # Aggregate runs default every row to role=aggregate with no lineage parent.
  [ "$(awk -F '\t' '$3 == "aggregate" { count++ } END { print count + 0 }' "$RESULT_DIR/runs.tsv")" = "7" ] || fail "aggregate rows did not default role=aggregate"
  [ "$(awk -F '\t' '$4 == "-" { count++ } END { print count + 0 }' "$RESULT_DIR/runs.tsv")" = "7" ] || fail "aggregate rows carried a retry_of parent"

  # Every attempt_id is present and immutably unique.
  [ "$(awk -F '\t' '$1 != "" { count++ } END { print count + 0 }' "$RESULT_DIR/runs.tsv")" = "7" ] || fail "runs.tsv rows were missing an attempt_id"
  [ "$(awk -F '\t' '{ print $1 }' "$RESULT_DIR/runs.tsv" | sort -u | wc -l | tr -d ' ')" = "7" ] || fail "attempt_ids were not unique"

  : > "$DIRS_FILE"
  for LOG in "$RESULT_DIR"/*.log; do
    [ -s "$LOG" ] || fail "durable log missing or empty: $LOG"
    sed -n 's/^PROMPTFOO_CONFIG_DIR=//p' "$LOG" >> "$DIRS_FILE"
  done

  for DIAGNOSTIC in "$RESULT_DIR"/*.json; do
    [ -s "$DIAGNOSTIC" ] || fail "machine-readable diagnostic missing or empty: $DIAGNOSTIC"
  done
  [ "$(find "$RESULT_DIR" -name '*.json' -type f | wc -l | tr -d ' ')" = "7" ] || fail "did not retain seven diagnostics"

  [ "$(wc -l < "$DIRS_FILE" | tr -d ' ')" = "7" ] || fail "did not observe seven Promptfoo config directories"
  [ "$(sort -u "$DIRS_FILE" | wc -l | tr -d ' ')" = "7" ] || fail "Promptfoo config directories were not unique"

  while IFS= read -r CONFIG_DIR; do
    case "$CONFIG_DIR" in
      "$RESULT_DIR"|"$RESULT_DIR"/*)
        fail "temporary Promptfoo state was placed under durable results"
        ;;
    esac
    [ ! -e "$CONFIG_DIR" ] || fail "temporary Promptfoo state survived cleanup: $CONFIG_DIR"
  done < "$DIRS_FILE"
}

cd "$WORK_DIR"
TMPDIR="$WORK_DIR/tmp" PROMPTFOO_BIN="$FAKE_PROMPTFOO" "$SCRIPT_DIR/run-parallel.sh" luna > "$WORK_DIR/success.out"
assert_artifacts 0

printf 'old failed run\n' > "$RESULT_DIR/old-marker.txt"

set +e
TMPDIR="$WORK_DIR/tmp" PROMPTFOO_BIN="$FAKE_PROMPTFOO" FAKE_PROMPTFOO_FAIL_FILTER='^Schema Registry Impact$' "$SCRIPT_DIR/run-parallel.sh" luna > "$WORK_DIR/failure.out"
RUN_STATUS=$?
set -e
[ "$RUN_STATUS" = "1" ] || fail "runner returned $RUN_STATUS instead of aggregate failure status 1"
assert_artifacts 1

ARCHIVED_MARKER=$(find "$WORK_DIR/results/archive-luna" -name old-marker.txt -type f -print -quit)
[ -n "$ARCHIVED_MARKER" ] || fail "previous latest-luna evidence was not archived"
[ "$(cat "$ARCHIVED_MARKER")" = "old failed run" ] || fail "archived evidence changed"

# --- Retry lineage: a passing retry appends without archiving and preserves
#     the failed aggregate row it descends from. ---
FAILED_ID=$(awk -F '\t' '$2 == "Schema Registry Impact" && $3 == "aggregate" { print $1 }' "$RESULT_DIR/runs.tsv")
[ -n "$FAILED_ID" ] || fail "could not locate failed aggregate attempt_id"

TMPDIR="$WORK_DIR/tmp" PROMPTFOO_BIN="$FAKE_PROMPTFOO" \
  "$SCRIPT_DIR/run-parallel.sh" luna \
  --retry 'Schema Registry Impact' --retry-of "$FAILED_ID" --role planned-repeat \
  > "$WORK_DIR/retry.out"

# Append-only: the row count grows by exactly one (no archive/reset).
[ "$(wc -l < "$RESULT_DIR/runs.tsv" | tr -d ' ')" = "8" ] || fail "retry did not append a single lineage row"

# The failed aggregate row is preserved verbatim.
[ "$(awk -F '\t' -v id="$FAILED_ID" '$1 == id && $3 == "aggregate" && $6 == "fail" { count++ } END { print count + 0 }' "$RESULT_DIR/runs.tsv")" = "1" ] || fail "failed aggregate row was dropped by retry"

# The retry row carries the requested role and explicit lineage parent.
[ "$(awk -F '\t' -v id="$FAILED_ID" '$2 == "Schema Registry Impact" && $3 == "planned-repeat" && $4 == id && $6 == "pass" { count++ } END { print count + 0 }' "$RESULT_DIR/runs.tsv")" = "1" ] || fail "retry lineage row was not recorded"

# Retry evidence lands in the existing latest dir, not a fresh archive.
[ "$(find "$RESULT_DIR" -name 'retry-*.json' -type f | wc -l | tr -d ' ')" = "1" ] || fail "retry diagnostic was not written into latest results"

# --- Refusal: a retry may not silently claim role=aggregate for a label that
#     already has an aggregate row. ---
set +e
TMPDIR="$WORK_DIR/tmp" PROMPTFOO_BIN="$FAKE_PROMPTFOO" \
  "$SCRIPT_DIR/run-parallel.sh" luna \
  --retry 'Schema Registry Impact' --retry-of "$FAILED_ID" --role aggregate \
  > "$WORK_DIR/refuse.out" 2>&1
REFUSE_STATUS=$?
set -e
[ "$REFUSE_STATUS" != "0" ] || fail "aggregate-role claim on an existing aggregate label was not refused"
[ "$(wc -l < "$RESULT_DIR/runs.tsv" | tr -d ' ')" = "8" ] || fail "refused retry mutated runs.tsv"

echo "run-parallel regression passed"
