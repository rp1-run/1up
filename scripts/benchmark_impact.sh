#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)
source "$ROOT_DIR/scripts/lib/impact_fixture.sh"

DEFAULT_OUT_DIR=$(mktemp -d "${TMPDIR:-/tmp}/1up-impact-bench.XXXXXX")
OUT_DIR="$DEFAULT_OUT_DIR"
BASELINE_REF=""
BASELINE_BIN=""
CANDIDATE_BIN=""
WARMUP_RUNS="${WARMUP_RUNS:-1}"
MEASURED_RUNS="${MEASURED_RUNS:-7}"
MAX_P95_REGRESSION_PCT="${MAX_P95_REGRESSION_PCT:-20}"

log() {
  printf '[impact-bench] %s\n' "$*" >&2
}

fail() {
  log "$*"
  exit 1
}

require_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    fail "missing required command: $1"
  fi
}

build_binary() {
  local repo_dir="$1"

  log "building $(basename "$repo_dir") binary"
  impact_build_binary "$repo_dir"
}

measure_case_run() {
  local variant="$1"
  local bin_path="$2"
  local home_dir="$3"
  local repo_dir="$4"
  local case_json="$5"
  local phase="$6"
  local run_number="$7"

  local name
  local expected_status
  local case_dir
  local output_path
  local stderr_path
  local elapsed_seconds
  local elapsed_ms
  local exit_code
  local actual_status="command_failed"
  local command_ok=0
  local contract_ok=0
  local result_count=0
  local contextual_count=0

  name=$(jq -r '.name' <<<"$case_json")
  expected_status=$(jq -r '.expected_status' <<<"$case_json")
  case_dir="$OUT_DIR/cases/$name/$variant"
  output_path="$case_dir/${phase}-${run_number}.json"
  stderr_path="$case_dir/${phase}-${run_number}.stderr"

  mkdir -p "$case_dir"

  mapfile -t impact_args < <(jq -r '.args[]' <<<"$case_json")

  set +e
  elapsed_seconds=$(
    TIMEFORMAT='%R'
    {
      time env \
        HOME="$home_dir" \
        XDG_DATA_HOME="$home_dir/.local/share" \
        "$bin_path" --format json impact "${impact_args[@]}" --path "$repo_dir" \
        >"$output_path" \
        2>"$stderr_path"
    } 2>&1
  )
  exit_code=$?
  set -e

  if [[ $exit_code -eq 0 && -s "$output_path" ]]; then
    command_ok=1
    actual_status=$(jq -r '.status' "$output_path")
    result_count=$(jq '.results | length' "$output_path")
    contextual_count=$(jq '.contextual_results | if type == "array" then length else 0 end' "$output_path")

    case "$expected_status" in
      refused)
        if jq -e --arg expected_status "$expected_status" '
          .status == $expected_status
          and (.results | length == 0)
          and (.refusal | type == "object")
        ' "$output_path" >/dev/null; then
          contract_ok=1
        fi
        ;;
      empty|empty_scoped)
        if jq -e --arg expected_status "$expected_status" '
          .status == $expected_status
          and (.results | length == 0)
          and (.resolved_anchor | type == "object")
          and (.refusal == null)
        ' "$output_path" >/dev/null; then
          contract_ok=1
        fi
        ;;
      expanded|expanded_scoped)
        if jq -e --arg expected_status "$expected_status" '
          .status == $expected_status
          and (.results | length > 0)
          and (.resolved_anchor | type == "object")
          and (.refusal == null)
        ' "$output_path" >/dev/null; then
          contract_ok=1
        fi
        ;;
      *)
        fail "unsupported expected status for case ${name}: ${expected_status}"
        ;;
    esac
  fi

  elapsed_ms=$(awk -v secs="${elapsed_seconds:-0}" 'BEGIN { printf "%.3f", secs * 1000 }')

  jq -n \
    --arg variant "$variant" \
    --arg name "$name" \
    --arg expected_status "$expected_status" \
    --arg phase "$phase" \
    --argjson run_number "$run_number" \
    --argjson args "$(jq '.args' <<<"$case_json")" \
    --arg actual_status "$actual_status" \
    --arg output_path "$output_path" \
    --arg stderr_path "$stderr_path" \
    --argjson elapsed_ms "$elapsed_ms" \
    --argjson exit_code "$exit_code" \
    --argjson command_ok "$command_ok" \
    --argjson contract_ok "$contract_ok" \
    --argjson result_count "$result_count" \
    --argjson contextual_result_count "$contextual_count" \
    '{
      variant: $variant,
      name: $name,
      expected_status: $expected_status,
      phase: $phase,
      run_number: $run_number,
      args: $args,
      actual_status: $actual_status,
      elapsed_ms: $elapsed_ms,
      exit_code: $exit_code,
      command_ok: ($command_ok == 1),
      contract_ok: ($contract_ok == 1),
      result_count: $result_count,
      contextual_result_count: $contextual_result_count,
      output_path: $output_path,
      stderr_path: $stderr_path
    }'
}

run_case_series() {
  local variant="$1"
  local bin_path="$2"
  local home_dir="$3"
  local repo_dir="$4"
  local case_json="$5"

  local run_number

  for ((run_number = 1; run_number <= WARMUP_RUNS; run_number++)); do
    measure_case_run "$variant" "$bin_path" "$home_dir" "$repo_dir" "$case_json" "warmup" "$run_number"
  done

  for ((run_number = 1; run_number <= MEASURED_RUNS; run_number++)); do
    measure_case_run "$variant" "$bin_path" "$home_dir" "$repo_dir" "$case_json" "measured" "$run_number"
  done
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --baseline-ref)
      BASELINE_REF="${2:-}"
      shift 2
      ;;
    --baseline-bin)
      BASELINE_BIN="${2:-}"
      shift 2
      ;;
    --candidate-bin)
      CANDIDATE_BIN="${2:-}"
      shift 2
      ;;
    --output-dir)
      OUT_DIR="${2:-}"
      shift 2
      ;;
    --warmup-runs)
      WARMUP_RUNS="${2:-}"
      shift 2
      ;;
    --measured-runs)
      MEASURED_RUNS="${2:-}"
      shift 2
      ;;
    --max-p95-regression-pct)
      MAX_P95_REGRESSION_PCT="${2:-}"
      shift 2
      ;;
    *)
      fail "unknown argument: $1"
      ;;
  esac
done

require_cmd cargo
require_cmd git
require_cmd jq
require_cmd env

BASELINE_REF="${BASELINE_REF:-$(impact_default_baseline_ref "$ROOT_DIR")}"

if ! git -C "$ROOT_DIR" rev-parse --verify "$BASELINE_REF" >/dev/null 2>&1; then
  fail "baseline ref does not resolve: $BASELINE_REF"
fi

if ! [[ "$WARMUP_RUNS" =~ ^[0-9]+$ && "$MEASURED_RUNS" =~ ^[1-9][0-9]*$ ]]; then
  fail "warmup and measured runs must be non-negative integers, and measured runs must be greater than zero"
fi

if ! [[ "$MAX_P95_REGRESSION_PCT" =~ ^[0-9]+([.][0-9]+)?$ ]]; then
  fail "max p95 regression pct must be numeric"
fi

BASELINE_WORKTREE=""
cleanup() {
  if [[ -n "$BASELINE_WORKTREE" && -d "$BASELINE_WORKTREE" ]]; then
    git -C "$ROOT_DIR" worktree remove --force "$BASELINE_WORKTREE" >/dev/null 2>&1 || true
    rm -rf "$BASELINE_WORKTREE"
  fi
}
trap cleanup EXIT

mkdir -p "$OUT_DIR"
OUT_DIR=$(cd "$OUT_DIR" && pwd -P)
rm -rf "$OUT_DIR/cases" "$OUT_DIR/fixture" "$OUT_DIR/runtime" "$OUT_DIR/run-results.jsonl" "$OUT_DIR/summary.json"

if [[ -z "$BASELINE_BIN" ]]; then
  BASELINE_WORKTREE=$(mktemp -d "${TMPDIR:-/tmp}/1up-impact-bench-baseline.XXXXXX")
  rm -rf "$BASELINE_WORKTREE"
  log "creating baseline worktree at $(basename "$BASELINE_WORKTREE")"
  git -C "$ROOT_DIR" worktree add --detach "$BASELINE_WORKTREE" "$BASELINE_REF" >/dev/null
  build_binary "$BASELINE_WORKTREE"
  BASELINE_BIN="$BASELINE_WORKTREE/target/release/1up"
fi

if [[ -z "$CANDIDATE_BIN" ]]; then
  build_binary "$ROOT_DIR"
  CANDIDATE_BIN="$ROOT_DIR/target/release/1up"
fi

[[ -x "$BASELINE_BIN" ]] || fail "baseline binary is not executable: $BASELINE_BIN"
[[ -x "$CANDIDATE_BIN" ]] || fail "candidate binary is not executable: $CANDIDATE_BIN"

TEMPLATE_REPO="$OUT_DIR/fixture/template_repo"
BASELINE_REPO="$OUT_DIR/fixture/baseline_repo"
CANDIDATE_REPO="$OUT_DIR/fixture/candidate_repo"
BASELINE_HOME="$OUT_DIR/runtime/baseline_home"
CANDIDATE_HOME="$OUT_DIR/runtime/candidate_home"
DETAILS_JSONL="$OUT_DIR/run-results.jsonl"
SUMMARY_PATH="$OUT_DIR/summary.json"

impact_create_fixture "$TEMPLATE_REPO"
impact_sync_repo "$TEMPLATE_REPO" "$BASELINE_REPO"
impact_sync_repo "$TEMPLATE_REPO" "$CANDIDATE_REPO"
impact_prepare_fts_only_home "$BASELINE_HOME"
impact_prepare_fts_only_home "$CANDIDATE_HOME"
impact_init_and_index_repo "$BASELINE_BIN" "$BASELINE_HOME" "$BASELINE_REPO"
impact_init_and_index_repo "$CANDIDATE_BIN" "$CANDIDATE_HOME" "$CANDIDATE_REPO"

CASES_JSON=$(cat <<'EOF'
[
  {
    "name": "expanded_file_anchor",
    "expected_status": "expanded",
    "args": ["--from-file", "src/auth/runtime.rs"]
  },
  {
    "name": "refused_broad_symbol",
    "expected_status": "refused",
    "args": ["--from-symbol", "load_config"]
  },
  {
    "name": "empty_file_line",
    "expected_status": "empty",
    "args": ["--from-file", "src/auth/runtime.rs:5"]
  },
  {
    "name": "empty_scoped_file_line",
    "expected_status": "empty_scoped",
    "args": ["--from-file", "src/auth/runtime.rs:5", "--scope", "src/auth"]
  }
]
EOF
)

: > "$DETAILS_JSONL"

while IFS= read -r case_json; do
  run_case_series "baseline" "$BASELINE_BIN" "$BASELINE_HOME" "$BASELINE_REPO" "$case_json" >>"$DETAILS_JSONL"
  run_case_series "candidate" "$CANDIDATE_BIN" "$CANDIDATE_HOME" "$CANDIDATE_REPO" "$case_json" >>"$DETAILS_JSONL"
done < <(jq -c '.[]' <<<"$CASES_JSON")

BASELINE_COMMIT=$(git -C "$ROOT_DIR" rev-parse "$BASELINE_REF")
CANDIDATE_COMMIT=$(git -C "$ROOT_DIR" rev-parse HEAD)

jq -s \
  --arg baseline_ref "$BASELINE_REF" \
  --arg baseline_commit "$BASELINE_COMMIT" \
  --arg candidate_commit "$CANDIDATE_COMMIT" \
  --arg output_dir "$OUT_DIR" \
  --arg generated_at "$(impact_utc_timestamp)" \
  --argjson warmup_runs "$WARMUP_RUNS" \
  --argjson measured_runs "$MEASURED_RUNS" \
  --argjson max_p95_regression_pct "$MAX_P95_REGRESSION_PCT" \
  '
  def percentile($xs; $p):
    if ($xs | length) == 0 then
      0
    else
      ($xs | sort | .[((((length - 1) * $p) | ceil) | tonumber)])
    end;

  def regression_pct($candidate; $baseline):
    if $baseline == 0 then
      (if $candidate == 0 then 0 else 100 end)
    else
      ((($candidate - $baseline) / $baseline) * 100)
    end;

  def stats($records):
    ($records | map(.elapsed_ms)) as $times
    | {
        measured_runs: ($records | map(select(.phase == "measured")) | length),
        all_runs: ($records | length),
        p50_ms: percentile(($records | map(select(.phase == "measured") | .elapsed_ms)); 0.5),
        p95_ms: percentile(($records | map(select(.phase == "measured") | .elapsed_ms)); 0.95),
        min_ms: ($times | min // 0),
        max_ms: ($times | max // 0),
        command_failures: ([ $records[] | select(.command_ok | not) ] | length),
        contract_failures: ([ $records[] | select(.contract_ok | not) ] | length),
        actual_statuses: ([ $records[] | .actual_status ] | unique),
        run_times_ms: ($records | map(.elapsed_ms))
      };

  . as $runs
  | ($runs | map(select(.phase == "measured" and .variant == "baseline") | .elapsed_ms)) as $baseline_times
  | ($runs | map(select(.phase == "measured" and .variant == "candidate") | .elapsed_ms)) as $candidate_times
  | (
      $runs
      | group_by(.name)
      | map(
          . as $case_runs
          | ($case_runs | map(select(.variant == "baseline"))) as $baseline_records
          | ($case_runs | map(select(.variant == "candidate"))) as $candidate_records
          | (stats($baseline_records)) as $baseline
          | (stats($candidate_records)) as $candidate
          | (regression_pct($candidate.p50_ms; $baseline.p50_ms)) as $p50_regression_pct
          | (regression_pct($candidate.p95_ms; $baseline.p95_ms)) as $p95_regression_pct
          | {
              name: $case_runs[0].name,
              expected_status: $case_runs[0].expected_status,
              args: $case_runs[0].args,
              baseline: $baseline,
              candidate: $candidate,
              regression_pct: {
                p50: $p50_regression_pct,
                p95: $p95_regression_pct
              },
              gate: {
                max_p95_regression_pct: $max_p95_regression_pct,
                p95_within_threshold: ($p95_regression_pct <= $max_p95_regression_pct),
                baseline_contract_passed: ($baseline.command_failures == 0 and $baseline.contract_failures == 0),
                candidate_contract_passed: ($candidate.command_failures == 0 and $candidate.contract_failures == 0),
                gate_passed: (
                  $baseline.command_failures == 0
                  and $baseline.contract_failures == 0
                  and $candidate.command_failures == 0
                  and $candidate.contract_failures == 0
                  and $p95_regression_pct <= $max_p95_regression_pct
                )
              }
            }
        )
    ) as $case_summaries
  | (percentile($baseline_times; 0.5)) as $baseline_p50_ms
  | (percentile($candidate_times; 0.5)) as $candidate_p50_ms
  | (percentile($baseline_times; 0.95)) as $baseline_p95_ms
  | (percentile($candidate_times; 0.95)) as $candidate_p95_ms
  | (regression_pct($candidate_p50_ms; $baseline_p50_ms)) as $aggregate_p50_regression_pct
  | (regression_pct($candidate_p95_ms; $baseline_p95_ms)) as $aggregate_p95_regression_pct
  | ([ $case_summaries[] | select(.gate.gate_passed | not) | .name ]) as $failing_cases
  | {
      baseline_ref: $baseline_ref,
      baseline_commit: $baseline_commit,
      candidate_commit: $candidate_commit,
      generated_at: $generated_at,
      output_dir: $output_dir,
      warmup_runs: $warmup_runs,
      measured_runs: $measured_runs,
      aggregate: {
        baseline_p50_ms: $baseline_p50_ms,
        candidate_p50_ms: $candidate_p50_ms,
        p50_regression_pct: $aggregate_p50_regression_pct,
        baseline_p95_ms: $baseline_p95_ms,
        candidate_p95_ms: $candidate_p95_ms,
        p95_regression_pct: $aggregate_p95_regression_pct
      },
      gate: {
        max_p95_regression_pct: $max_p95_regression_pct,
        failing_cases: $failing_cases,
        gate_passed: (($failing_cases | length) == 0)
      },
      gate_passed: (($failing_cases | length) == 0),
      cases: $case_summaries
    }
  ' "$DETAILS_JSONL" >"$SUMMARY_PATH"

gate_passed=$(jq -r '.gate_passed' "$SUMMARY_PATH")
aggregate_p95=$(jq -r '.aggregate.p95_regression_pct' "$SUMMARY_PATH")

log "wrote summary to $SUMMARY_PATH"
log "performance gate: ${gate_passed} (aggregate p95 regression ${aggregate_p95}%)"
printf '%s\n' "$SUMMARY_PATH"

if [[ "$gate_passed" != "true" ]]; then
  exit 1
fi
