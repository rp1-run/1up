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
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--filter-pattern" ]; then
    shift
    FILTER="${1:-}"
  fi
  shift
done

printf 'PROMPTFOO_CONFIG_DIR=%s\n' "$PROMPTFOO_CONFIG_DIR"
printf 'filter=%s\n' "$FILTER"
printf 'fake-state\n' > "$PROMPTFOO_CONFIG_DIR/state.txt"

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

assert_artifacts() {
  EXPECTED_FAILURES=$1
  RESULT_DIR="$WORK_DIR/results/latest-luna"
  DIRS_FILE="$WORK_DIR/config-dirs.txt"

  [ -f "$RESULT_DIR/runs.tsv" ] || fail "runs.tsv was not retained"
  [ "$(wc -l < "$RESULT_DIR/runs.tsv" | tr -d ' ')" = "7" ] || fail "runs.tsv did not contain seven rows"
  [ "$(awk -F '\t' '$3 == "fail" { count++ } END { print count + 0 }' "$RESULT_DIR/runs.tsv")" = "$EXPECTED_FAILURES" ] || fail "unexpected failure count in runs.tsv"
  [ "$(awk -F '\t' '$3 == "pass" { count++ } END { print count + 0 }' "$RESULT_DIR/runs.tsv")" = "$((7 - EXPECTED_FAILURES))" ] || fail "unexpected pass count in runs.tsv"

  : > "$DIRS_FILE"
  for LOG in "$RESULT_DIR"/*.log; do
    [ -s "$LOG" ] || fail "durable log missing or empty: $LOG"
    sed -n 's/^PROMPTFOO_CONFIG_DIR=//p' "$LOG" >> "$DIRS_FILE"
  done

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

set +e
TMPDIR="$WORK_DIR/tmp" PROMPTFOO_BIN="$FAKE_PROMPTFOO" FAKE_PROMPTFOO_FAIL_FILTER='^Schema Registry Impact$' "$SCRIPT_DIR/run-parallel.sh" luna > "$WORK_DIR/failure.out"
RUN_STATUS=$?
set -e
[ "$RUN_STATUS" = "1" ] || fail "runner returned $RUN_STATUS instead of aggregate failure status 1"
assert_artifacts 1

echo "run-parallel regression passed"
