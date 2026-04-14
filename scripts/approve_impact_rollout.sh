#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)
OUTPUT_DIR="$ROOT_DIR/target/impact/rollout-approval"
OUTPUT_PATH=""
ACCURACY_SUMMARY=""
PERFORMANCE_SUMMARY=""
FIELD_NOTES_PATH=""

log() {
  printf '[impact-rollout-approve] %s\n' "$*" >&2
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

canonical_path() {
  local path="$1"
  local dir

  dir=$(cd "$(dirname "$path")" && pwd -P)
  printf '%s/%s\n' "$dir" "$(basename "$path")"
}

validate_accuracy_summary() {
  local path="$1"

  [[ -f "$path" ]] || fail "accuracy summary does not exist: $path"

  if ! jq -e '
    (.baseline_commit | type == "string")
    and (.candidate_commit | type == "string")
    and (.false_positive_reduction_pct | numbers)
    and (.gate.required_reduction_pct | numbers)
    and (.gate_passed | type == "boolean")
  ' "$path" >/dev/null 2>&1; then
    fail "accuracy summary is missing required rollout fields: $path"
  fi
}

validate_performance_summary() {
  local path="$1"

  [[ -f "$path" ]] || fail "performance summary does not exist: $path"

  if ! jq -e '
    (.baseline_commit | type == "string")
    and (.candidate_commit | type == "string")
    and (.aggregate.p95_regression_pct | numbers)
    and (.gate.max_p95_regression_pct | numbers)
    and (.gate_passed | type == "boolean")
  ' "$path" >/dev/null 2>&1; then
    fail "performance summary is missing required rollout fields: $path"
  fi
}

collect_unresolved_field_note_blockers() {
  local path="$1"

  [[ -f "$path" ]] || fail "field notes do not exist: $path"

  awk '
    /^## Rollout Blockers$/ {
      in_blockers = 1
      next
    }
    /^## / {
      in_blockers = 0
    }
    in_blockers && /^- \[ \] / {
      sub(/^- \[ \] /, "", $0)
      print
    }
  ' "$path"
}

run_accuracy_summary() {
  local out_dir="$1"

  log "running impact-eval"
  "$ROOT_DIR/scripts/evaluate_impact_trust.sh" --output-dir "$out_dir"
}

run_performance_summary() {
  local out_dir="$1"

  log "running impact-bench"
  "$ROOT_DIR/scripts/benchmark_impact.sh" --output-dir "$out_dir"
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --accuracy-summary)
      ACCURACY_SUMMARY="${2:-}"
      shift 2
      ;;
    --performance-summary)
      PERFORMANCE_SUMMARY="${2:-}"
      shift 2
      ;;
    --output-dir)
      OUTPUT_DIR="${2:-}"
      shift 2
      ;;
    --field-notes)
      FIELD_NOTES_PATH="${2:-}"
      shift 2
      ;;
    --output)
      OUTPUT_PATH="${2:-}"
      shift 2
      ;;
    *)
      fail "unknown argument: $1"
      ;;
  esac
done

require_cmd jq

mkdir -p "$OUTPUT_DIR"
OUTPUT_DIR=$(cd "$OUTPUT_DIR" && pwd -P)
OUTPUT_PATH="${OUTPUT_PATH:-$OUTPUT_DIR/summary.json}"
mkdir -p "$(dirname "$OUTPUT_PATH")"
OUTPUT_PATH=$(canonical_path "$OUTPUT_PATH")

field_note_blockers_json='[]'
if [[ -z "$ACCURACY_SUMMARY" ]]; then
  ACCURACY_SUMMARY=$(run_accuracy_summary "$OUTPUT_DIR/impact-eval")
fi
if [[ -z "$PERFORMANCE_SUMMARY" ]]; then
  PERFORMANCE_SUMMARY=$(run_performance_summary "$OUTPUT_DIR/impact-bench")
fi

ACCURACY_SUMMARY=$(canonical_path "$ACCURACY_SUMMARY")
PERFORMANCE_SUMMARY=$(canonical_path "$PERFORMANCE_SUMMARY")
if [[ -n "$FIELD_NOTES_PATH" ]]; then
  FIELD_NOTES_PATH=$(canonical_path "$FIELD_NOTES_PATH")
fi

validate_accuracy_summary "$ACCURACY_SUMMARY"
validate_performance_summary "$PERFORMANCE_SUMMARY"

accuracy_gate_passed=$(jq -r '.gate_passed' "$ACCURACY_SUMMARY")
performance_gate_passed=$(jq -r '.gate_passed' "$PERFORMANCE_SUMMARY")
accuracy_baseline_commit=$(jq -r '.baseline_commit' "$ACCURACY_SUMMARY")
performance_baseline_commit=$(jq -r '.baseline_commit' "$PERFORMANCE_SUMMARY")
accuracy_candidate_commit=$(jq -r '.candidate_commit' "$ACCURACY_SUMMARY")
performance_candidate_commit=$(jq -r '.candidate_commit' "$PERFORMANCE_SUMMARY")

declare -a blocking_reasons=()

if [[ "$accuracy_gate_passed" != "true" ]]; then
  blocking_reasons+=("impact-eval gate failed")
fi
if [[ "$performance_gate_passed" != "true" ]]; then
  blocking_reasons+=("impact-bench gate failed")
fi
if [[ "$accuracy_baseline_commit" != "$performance_baseline_commit" ]]; then
  blocking_reasons+=("impact-eval and impact-bench baseline commits do not match")
fi
if [[ "$accuracy_candidate_commit" != "$performance_candidate_commit" ]]; then
  blocking_reasons+=("impact-eval and impact-bench candidate commits do not match")
fi
if [[ -n "$FIELD_NOTES_PATH" ]]; then
  mapfile -t field_note_blockers < <(collect_unresolved_field_note_blockers "$FIELD_NOTES_PATH")
  if (( ${#field_note_blockers[@]} > 0 )); then
    field_note_blockers_json=$(printf '%s\n' "${field_note_blockers[@]}" | jq -R . | jq -s .)
    for blocker in "${field_note_blockers[@]}"; do
      blocking_reasons+=("field-notes unresolved blocker: $blocker")
    done
  fi
fi

gate_passed=true
if (( ${#blocking_reasons[@]} > 0 )); then
  gate_passed=false
fi

blocking_reasons_json='[]'
if (( ${#blocking_reasons[@]} > 0 )); then
  blocking_reasons_json=$(printf '%s\n' "${blocking_reasons[@]}" | jq -R . | jq -s .)
fi

jq -n \
  --arg generated_at "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
  --arg status "$(if [[ "$gate_passed" == "true" ]]; then printf 'approved'; else printf 'blocked'; fi)" \
  --arg output_path "$OUTPUT_PATH" \
  --arg accuracy_summary_path "$ACCURACY_SUMMARY" \
  --arg performance_summary_path "$PERFORMANCE_SUMMARY" \
  --arg field_notes_path "$FIELD_NOTES_PATH" \
  --argjson gate_passed "$(if [[ "$gate_passed" == "true" ]]; then printf 'true'; else printf 'false'; fi)" \
  --argjson blocking_reasons "$blocking_reasons_json" \
  --argjson field_note_blockers "$field_note_blockers_json" \
  --slurpfile accuracy "$ACCURACY_SUMMARY" \
  --slurpfile performance "$PERFORMANCE_SUMMARY" \
  '{
    generated_at: $generated_at,
    status: $status,
    gate_passed: $gate_passed,
    output_path: $output_path,
    requirements: {
      both_gates_required: true,
      required_entry_points: ["impact-eval", "impact-bench"]
    },
    accuracy: {
      summary_path: $accuracy_summary_path,
      baseline_commit: $accuracy[0].baseline_commit,
      candidate_commit: $accuracy[0].candidate_commit,
      false_positive_reduction_pct: $accuracy[0].false_positive_reduction_pct,
      required_reduction_pct: $accuracy[0].gate.required_reduction_pct,
      gate_passed: $accuracy[0].gate_passed
    },
    performance: {
      summary_path: $performance_summary_path,
      baseline_commit: $performance[0].baseline_commit,
      candidate_commit: $performance[0].candidate_commit,
      aggregate_p95_regression_pct: $performance[0].aggregate.p95_regression_pct,
      max_p95_regression_pct: $performance[0].gate.max_p95_regression_pct,
      gate_passed: $performance[0].gate_passed
    },
    field_notes: (
      if $field_notes_path == "" then
        null
      else
        {
          path: $field_notes_path,
          unresolved_blockers: $field_note_blockers,
          has_unresolved_blockers: (($field_note_blockers | length) > 0)
        }
      end
    ),
    blocking_reasons: $blocking_reasons
  }' >"$OUTPUT_PATH"

log "wrote summary to $OUTPUT_PATH"
if [[ "$gate_passed" != "true" ]]; then
  log "rollout approval blocked"
  exit 1
fi

log "rollout approval granted"
printf '%s\n' "$OUTPUT_PATH"
