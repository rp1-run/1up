#!/usr/bin/env bash
#
# Semantic index storage and throughput benchmark for retained release readiness.
#
# Fresh-reindexes the 1up repo into a temp worktree, captures db_size_bytes,
# indexing_ms (median), and schema_version, then gates retained semantic
# indexing evidence against:
#
#   * db_size_bytes <= 80 * 1024 * 1024
#   * indexing_ms <= 90000
#   * current schema version
#
# A pinned baseline JSON at scripts/baselines/vector_index_size_baseline.json
# is loaded for delta reporting, but gate thresholds remain script constants
# so pass/fail does not drift with baseline updates.
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
#   BENCHMARK_SKIPPED_REASON=<reason>      emit skipped evidence and do not run
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

write_skipped_summary() {
  local reason="$1"
  local results_json="$2"

  mkdir -p "$(dirname "$results_json")"
  jq -n \
    --arg reason "$reason" \
    '{
      status: "skipped",
      evidence_type: "semantic_index_storage",
      retained_performance_outcome: "semantic_indexing",
      skipped_reason: $reason,
      manual_readiness_note_required: true
    }' > "$results_json"

  cat "$results_json"
  printf '\nVector index size benchmark skipped.\n'
  printf 'Output: %s\n' "$results_json"
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

REPO_INPUT="${1:-$ROOT_DIR}"
if [[ -d "$REPO_INPUT" ]]; then
  REPO_NAME=$(basename "$(cd "$REPO_INPUT" && pwd -P)")
else
  REPO_NAME=$(basename "$REPO_INPUT")
fi
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

BENCHMARK_SKIPPED_REASON="${BENCHMARK_SKIPPED_REASON:-${SKIP_REASON:-}}"
if [[ -n "${BENCHMARK_SKIPPED_REASON//[[:space:]]/}" ]]; then
  require_cmd jq
  write_skipped_summary "$BENCHMARK_SKIPPED_REASON" "$RESULTS_JSON"
  exit 0
fi

require_cmd cargo
require_cmd jq
require_cmd rsync
require_cmd sqlite3
require_cmd stat

if [[ ! -d "$REPO_INPUT" ]]; then
  printf 'repo not found: %s\n' "$REPO_INPUT" >&2
  exit 1
fi

REPO=$(cd "$REPO_INPUT" && pwd -P)
REPO_NAME=$(basename "$REPO")

# Retained-performance gates. Kept as constants so pass/fail semantics are
# independent of the pinned baseline file.
readonly SIZE_LIMIT_BYTES=$((80 * 1024 * 1024))
readonly TIME_LIMIT_MS=90000

# Source of truth for the expected schema version: src/shared/constants.rs.
# Derive it at runtime (anchored on `SCHEMA_VERSION: u32 = <N>;`) so this gate
# can never drift from the binary; a constants.rs bump updates it with no edit.
CONSTANTS_RS="$ROOT_DIR/src/shared/constants.rs"
if [[ ! -f "$CONSTANTS_RS" ]]; then
  log "FATAL: schema source of truth not found: $CONSTANTS_RS"
  exit 1
fi
EXPECTED_SCHEMA_VERSION=$(sed -n \
  's/^[[:space:]]*pub const SCHEMA_VERSION:[[:space:]]*u32[[:space:]]*=[[:space:]]*\([0-9][0-9]*\)[[:space:]]*;.*$/\1/p' \
  "$CONSTANTS_RS" | head -n1)
if [[ -z "$EXPECTED_SCHEMA_VERSION" || ! "$EXPECTED_SCHEMA_VERSION" =~ ^[0-9]+$ ]]; then
  log "FATAL: could not parse SCHEMA_VERSION (u32 literal) from $CONSTANTS_RS"
  log "       got: '${EXPECTED_SCHEMA_VERSION}'"
  exit 1
fi
readonly EXPECTED_SCHEMA_VERSION

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
(( MEDIAN_TIME <= TIME_LIMIT_MS )) || time_pass=false
[[ "$LAST_SCHEMA_VERSION" == "$EXPECTED_SCHEMA_VERSION" ]] || schema_pass=false

jq -n \
  --arg repo "$REPO" \
  --arg out_dir "$OUT_DIR" \
  --arg timestamp "$TIMESTAMP" \
  --argjson runs "$RUNS" \
  --arg evidence_type "semantic_index_storage" \
  --arg retained_outcome "semantic_indexing" \
  --argjson db_size_bytes "$MEDIAN_SIZE" \
  --argjson indexing_ms "$MEDIAN_TIME" \
  --arg schema_version "$LAST_SCHEMA_VERSION" \
  --argjson size_limit_bytes "$SIZE_LIMIT_BYTES" \
  --argjson time_limit_ms "$TIME_LIMIT_MS" \
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
    status: "recorded",
    evidence_type: $evidence_type,
    retained_performance_outcome: $retained_outcome,
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
      indexing_time_limit_ms: $time_limit_ms,
      expected_schema_version: $expected_schema_version,
      size_pass: $size_pass,
      indexing_time_pass: $time_pass,
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
printf 'indexing_ms   (median of %d runs): %s (limit %s)\n' \
  "$RUNS" "$MEDIAN_TIME" "$TIME_LIMIT_MS"
printf 'schema_version: %s (expected %s)\n' \
  "$LAST_SCHEMA_VERSION" "$EXPECTED_SCHEMA_VERSION"

fail_count=0
if [[ "$size_pass" != "true" ]]; then
  printf 'FAIL: db_size_bytes %s > %s (semantic index storage limit)\n' \
    "$MEDIAN_SIZE" "$SIZE_LIMIT_BYTES" >&2
  fail_count=$((fail_count + 1))
fi
if [[ "$time_pass" != "true" ]]; then
  printf 'FAIL: indexing_ms %s > %s (semantic indexing throughput limit)\n' \
    "$MEDIAN_TIME" "$TIME_LIMIT_MS" >&2
  fail_count=$((fail_count + 1))
fi
if [[ "$schema_pass" != "true" ]]; then
  printf 'FAIL: schema_version %s != %s (current retained schema)\n' \
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
