#!/usr/bin/env bash
#
# Size and throughput benchmark guard for schema v12 (shrink-hnsw-vector-index, T5).
#
# Fresh-reindexes the 1up repo into a temp worktree, captures db_size_bytes,
# indexing_ms (median), and schema_version, then gates against:
#
#   * REQ-001: db_size_bytes <= 80 * 1024 * 1024 (absolute upper bound)
#   * REQ-003: indexing_ms in [72900, 89100] (+/-10% of 81 s baseline)
#
# A pinned baseline JSON at scripts/baselines/vector_index_size_baseline.json
# is loaded for delta reporting, but gate thresholds remain the REQ-derived
# absolutes so the script's pass/fail does not drift with baseline updates.
#
# Usage:
#   scripts/benchmark_vector_index_size.sh [path-to-repo]
#
# Environment overrides:
#   RUNS=3                                 number of index runs (median is used)
#   ONEUP_BIN=target/release/1up           pre-built binary (default: cargo build)
#   OUT_DIR=<path>                         results directory
#   BASELINE_JSON=<path>                   alternate baseline file
#   SKIP_GATES=1                           emit JSON only; do not fail on violations
#
set -euo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)

log() {
  printf '[vector-index-size-bench] %s\n' "$*" >&2
}

require_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    printf 'missing required command: %s\n' "$1" >&2
    exit 1
  fi
}

# Median of the numbers passed as args. Accepts floats; prints integer ms.
median_ms() {
  local -a sorted
  IFS=$'\n' read -r -d '' -a sorted < <(printf '%s\n' "$@" | sort -n; printf '\0')
  local count=${#sorted[@]}
  if (( count == 0 )); then
    printf '0'
    return
  fi
  local mid=$(( count / 2 ))
  if (( count % 2 == 1 )); then
    printf '%s' "${sorted[$mid]}"
  else
    awk -v a="${sorted[$((mid - 1))]}" -v b="${sorted[$mid]}" \
      'BEGIN { printf "%d", (a + b) / 2 }'
  fi
}

sync_fixture() {
  local source_dir="$1"
  local target_dir="$2"

  rm -rf "$target_dir"
  mkdir -p "$target_dir"
  # Exclude generated/untracked content and build artifacts so the corpus
  # reflects the committed repository, matching the baseline's intent.
  rsync -a --delete \
    --exclude .git \
    --exclude .1up \
    --exclude .rp1 \
    --exclude target \
    --exclude 'evals/.cache' \
    --exclude 'evals/node_modules' \
    "$source_dir"/ "$target_dir"/
}

require_cmd cargo
require_cmd jq
require_cmd rsync
require_cmd sqlite3
require_cmd stat

REPO_INPUT="${1:-$ROOT_DIR}"
if [[ ! -d "$REPO_INPUT" ]]; then
  printf 'repo not found: %s\n' "$REPO_INPUT" >&2
  exit 1
fi

REPO=$(cd "$REPO_INPUT" && pwd -P)
REPO_NAME=$(basename "$REPO")
TIMESTAMP=$(date +"%Y%m%d-%H%M%S")

RUNS="${RUNS:-3}"
ONEUP_BIN="${ONEUP_BIN:-$ROOT_DIR/target/release/1up}"
OUT_DIR="${OUT_DIR:-$ROOT_DIR/target/vector-index-size-bench/${REPO_NAME}-${TIMESTAMP}}"
BASELINE_JSON="${BASELINE_JSON:-$ROOT_DIR/scripts/baselines/vector_index_size_baseline.json}"
# The fixture must live outside the repo tree: 1up's resolve_project_root
# walks up from the target path looking for a parent .1up/, so a fixture
# anywhere inside $ROOT_DIR would reuse the repo's own index instead of
# building a fresh one.
FIXTURE_ROOT="${FIXTURE_ROOT:-${TMPDIR:-/tmp}/vector-index-size-bench}"
FIXTURE_DIR="$FIXTURE_ROOT/${REPO_NAME}-${TIMESTAMP}"
RESULTS_JSON="$OUT_DIR/results.json"

# REQ-derived absolute gates. Kept as constants so the script's pass/fail
# semantics are independent of the pinned baseline file.
readonly SIZE_LIMIT_BYTES=$((80 * 1024 * 1024))        # REQ-001
readonly TIME_LOWER_MS=72900                            # REQ-003 -10%
readonly TIME_UPPER_MS=89100                            # REQ-003 +10%
readonly EXPECTED_SCHEMA_VERSION=12                     # REQ-005

mkdir -p "$OUT_DIR" "$FIXTURE_ROOT"

cleanup_fixture() {
  if [[ "${KEEP_FIXTURE:-0}" != "1" && -d "$FIXTURE_DIR" ]]; then
    rm -rf "$FIXTURE_DIR"
  fi
}
trap cleanup_fixture EXIT

if [[ ! -x "$ONEUP_BIN" ]]; then
  log "building release binary"
  cargo build --release --bin 1up --manifest-path "$ROOT_DIR/Cargo.toml" >/dev/null
fi

log "syncing fixture from $REPO"
sync_fixture "$REPO" "$FIXTURE_DIR"

# Warm the embedder model cache once so the ONNX runtime init cost does not
# dominate the first iteration and skew the median.
log "warming indexing environment (one discarded run)"
"$ONEUP_BIN" --format json index "$FIXTURE_DIR" >/dev/null
rm -rf "$FIXTURE_DIR/.1up"

declare -a size_runs=()
declare -a time_runs=()
LAST_INDEX_DB=""
LAST_SCHEMA_VERSION=""

for iter in $(seq 1 "$RUNS"); do
  rm -rf "$FIXTURE_DIR/.1up"

  log "run ${iter}/${RUNS}: fresh reindex"
  local_output=$("$ONEUP_BIN" --format json index "$FIXTURE_DIR")

  local_ms=$(jq -r '.progress.timings.total_ms' <<<"$local_output")
  files_indexed=$(jq -r '.progress.files_indexed // 0' <<<"$local_output")
  segments_stored=$(jq -r '.progress.segments_stored // 0' <<<"$local_output")

  if [[ -z "$local_ms" || "$local_ms" == "null" ]]; then
    printf 'run %d: missing total_ms in index output\n' "$iter" >&2
    exit 1
  fi
  if (( files_indexed == 0 || segments_stored == 0 )); then
    printf 'run %d: no indexed work (files=%s segments=%s)\n' \
      "$iter" "$files_indexed" "$segments_stored" >&2
    exit 1
  fi

  LAST_INDEX_DB="$FIXTURE_DIR/.1up/index.db"
  if [[ ! -f "$LAST_INDEX_DB" ]]; then
    printf 'run %d: index.db not produced at %s\n' "$iter" "$LAST_INDEX_DB" >&2
    exit 1
  fi

  local_size=$(stat -f%z "$LAST_INDEX_DB" 2>/dev/null || stat -c%s "$LAST_INDEX_DB")
  LAST_SCHEMA_VERSION=$(sqlite3 "$LAST_INDEX_DB" \
    "SELECT value FROM meta WHERE key='schema_version';")

  log "run ${iter}/${RUNS}: ${local_ms} ms, ${local_size} bytes, schema v${LAST_SCHEMA_VERSION}"

  size_runs+=("$local_size")
  time_runs+=("$local_ms")
done

MEDIAN_SIZE=$(median_ms "${size_runs[@]}")
MEDIAN_TIME=$(median_ms "${time_runs[@]}")

BASELINE_SNAPSHOT="null"
BASELINE_DELTA_SIZE_BYTES="null"
BASELINE_DELTA_TIME_MS="null"
if [[ -f "$BASELINE_JSON" ]]; then
  BASELINE_SNAPSHOT=$(jq '.' "$BASELINE_JSON")
  baseline_size=$(jq -r '.db_size_bytes // empty' "$BASELINE_JSON")
  baseline_time=$(jq -r '.indexing_ms // empty' "$BASELINE_JSON")
  if [[ -n "$baseline_size" ]]; then
    BASELINE_DELTA_SIZE_BYTES=$((MEDIAN_SIZE - baseline_size))
  fi
  if [[ -n "$baseline_time" ]]; then
    BASELINE_DELTA_TIME_MS=$((MEDIAN_TIME - baseline_time))
  fi
fi

size_pass=true
time_pass=true
schema_pass=true
(( MEDIAN_SIZE <= SIZE_LIMIT_BYTES )) || size_pass=false
(( MEDIAN_TIME >= TIME_LOWER_MS && MEDIAN_TIME <= TIME_UPPER_MS )) || time_pass=false
[[ "$LAST_SCHEMA_VERSION" == "$EXPECTED_SCHEMA_VERSION" ]] || schema_pass=false

jq -n \
  --arg repo "$REPO" \
  --arg out_dir "$OUT_DIR" \
  --arg timestamp "$TIMESTAMP" \
  --argjson runs "$RUNS" \
  --argjson db_size_bytes "$MEDIAN_SIZE" \
  --argjson indexing_ms "$MEDIAN_TIME" \
  --arg schema_version "$LAST_SCHEMA_VERSION" \
  --argjson size_limit_bytes "$SIZE_LIMIT_BYTES" \
  --argjson time_lower_ms "$TIME_LOWER_MS" \
  --argjson time_upper_ms "$TIME_UPPER_MS" \
  --argjson expected_schema_version "$EXPECTED_SCHEMA_VERSION" \
  --argjson per_run_bytes "$(printf '%s\n' "${size_runs[@]}" | jq -s '.')" \
  --argjson per_run_ms "$(printf '%s\n' "${time_runs[@]}" | jq -s '.')" \
  --argjson size_pass "$size_pass" \
  --argjson time_pass "$time_pass" \
  --argjson schema_pass "$schema_pass" \
  --argjson baseline "$BASELINE_SNAPSHOT" \
  --argjson delta_size_bytes "$BASELINE_DELTA_SIZE_BYTES" \
  --argjson delta_time_ms "$BASELINE_DELTA_TIME_MS" \
  '{
    repo: $repo,
    out_dir: $out_dir,
    timestamp: $timestamp,
    runs: $runs,
    db_size_bytes: $db_size_bytes,
    indexing_ms: $indexing_ms,
    schema_version: ($schema_version | tonumber),
    per_run: {
      db_size_bytes: $per_run_bytes,
      indexing_ms: $per_run_ms
    },
    gates: {
      size_limit_bytes: $size_limit_bytes,
      time_lower_ms: $time_lower_ms,
      time_upper_ms: $time_upper_ms,
      expected_schema_version: $expected_schema_version,
      size_pass: $size_pass,
      time_pass: $time_pass,
      schema_pass: $schema_pass
    },
    baseline: $baseline,
    delta_vs_baseline: {
      db_size_bytes: $delta_size_bytes,
      indexing_ms: $delta_time_ms
    }
  }' > "$RESULTS_JSON"

cat "$RESULTS_JSON"

printf '\n'
printf 'Vector index size benchmark complete.\n'
printf 'Repository: %s\n' "$REPO"
printf 'Output: %s\n' "$RESULTS_JSON"
printf 'db_size_bytes (median of %d runs): %s (limit %s)\n' \
  "$RUNS" "$MEDIAN_SIZE" "$SIZE_LIMIT_BYTES"
printf 'indexing_ms   (median of %d runs): %s (gate [%s, %s])\n' \
  "$RUNS" "$MEDIAN_TIME" "$TIME_LOWER_MS" "$TIME_UPPER_MS"
printf 'schema_version: %s (expected %s)\n' \
  "$LAST_SCHEMA_VERSION" "$EXPECTED_SCHEMA_VERSION"

fail_count=0
if [[ "$size_pass" != "true" ]]; then
  printf 'FAIL: db_size_bytes %s > %s (REQ-001)\n' "$MEDIAN_SIZE" "$SIZE_LIMIT_BYTES" >&2
  fail_count=$((fail_count + 1))
fi
if [[ "$time_pass" != "true" ]]; then
  printf 'FAIL: indexing_ms %s outside [%s, %s] (REQ-003)\n' \
    "$MEDIAN_TIME" "$TIME_LOWER_MS" "$TIME_UPPER_MS" >&2
  fail_count=$((fail_count + 1))
fi
if [[ "$schema_pass" != "true" ]]; then
  printf 'FAIL: schema_version %s != %s (REQ-005)\n' \
    "$LAST_SCHEMA_VERSION" "$EXPECTED_SCHEMA_VERSION" >&2
  fail_count=$((fail_count + 1))
fi

if (( fail_count > 0 )); then
  if [[ "${SKIP_GATES:-0}" == "1" ]]; then
    printf 'SKIP_GATES=1 set; not failing despite %d violation(s).\n' "$fail_count" >&2
    exit 0
  fi
  exit 1
fi

printf 'All gates pass.\n'
