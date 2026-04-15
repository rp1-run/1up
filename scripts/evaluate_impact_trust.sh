#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)
source "$ROOT_DIR/scripts/lib/impact_fixture.sh"

DEFAULT_OUT_DIR=$(mktemp -d "${TMPDIR:-/tmp}/1up-impact-trust.XXXXXX")
OUT_DIR="$DEFAULT_OUT_DIR"
BASELINE_REF=""
BASELINE_BIN=""
CANDIDATE_BIN=""

log() {
  printf '[impact-trust-eval] %s\n' "$*" >&2
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

evaluate_case() {
  local variant="$1"
  local bin_path="$2"
  local home_dir="$3"
  local repo_dir="$4"
  local case_json="$5"

  local kind
  local name
  local expected_status
  local expected_primary
  local expected_hint
  local case_dir
  local output_path
  local stderr_path
  local actual_status
  local result_count
  local contextual_count
  local hint_code
  local contract_ok=0
  local expected_primary_present=0
  local false_positive_count=0
  local exact_regression=0
  local status_contract_failure=0

  kind=$(jq -r '.kind' <<<"$case_json")
  name=$(jq -r '.name' <<<"$case_json")
  expected_status=$(jq -r '.expected_status' <<<"$case_json")
  expected_primary=$(jq -r '.expected_primary // ""' <<<"$case_json")
  expected_hint=$(jq -r '.expected_hint // ""' <<<"$case_json")

  case_dir="$OUT_DIR/cases/$name"
  output_path="$case_dir/${variant}.json"
  stderr_path="$case_dir/${variant}.stderr"

  mkdir -p "$case_dir"

  mapfile -t impact_args < <(jq -r '.args[]' <<<"$case_json")

  impact_run_oneup_json "$bin_path" "$home_dir" impact "${impact_args[@]}" --path "$repo_dir" \
    >"$output_path" \
    2>"$stderr_path"

  actual_status=$(jq -r '.status' "$output_path")
  result_count=$(jq '.results | length' "$output_path")
  contextual_count=$(jq '.contextual_results | if type == "array" then length else 0 end' "$output_path")
  hint_code=$(jq -r '.hint.code // ""' "$output_path")

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

  if [[ -n "$expected_hint" && "$hint_code" != "$expected_hint" ]]; then
    contract_ok=0
  fi

  if [[ -n "$expected_primary" ]] && jq -e --arg expected_primary "$expected_primary" '
    [.results[]?.file_path] | index($expected_primary) != null
  ' "$output_path" >/dev/null; then
    expected_primary_present=1
  fi

  if [[ "$kind" == "ambiguous" ]]; then
    false_positive_count="$result_count"
  fi

  if [[ "$kind" == "exact" && ( $contract_ok -eq 0 || $expected_primary_present -eq 0 ) ]]; then
    exact_regression=1
  fi

  if [[ $contract_ok -eq 0 ]]; then
    status_contract_failure=1
  fi

  jq -n \
    --arg variant "$variant" \
    --arg kind "$kind" \
    --arg name "$name" \
    --arg expected_status "$expected_status" \
    --arg expected_primary "$expected_primary" \
    --arg expected_hint "$expected_hint" \
    --arg actual_status "$actual_status" \
    --arg hint_code "$hint_code" \
    --arg output_path "$output_path" \
    --arg stderr_path "$stderr_path" \
    --argjson result_count "$result_count" \
    --argjson contextual_count "$contextual_count" \
    --argjson contract_ok "$contract_ok" \
    --argjson expected_primary_present "$expected_primary_present" \
    --argjson false_positive_count "$false_positive_count" \
    --argjson exact_regression "$exact_regression" \
    --argjson status_contract_failure "$status_contract_failure" \
    --argjson primary_paths "$(jq '[.results[]?.file_path]' "$output_path")" \
    --argjson contextual_paths "$(jq '[.contextual_results[]?.file_path]' "$output_path")" \
    '{
      variant: $variant,
      kind: $kind,
      name: $name,
      expected_status: $expected_status,
      expected_primary: (if $expected_primary == "" then null else $expected_primary end),
      expected_hint: (if $expected_hint == "" then null else $expected_hint end),
      actual_status: $actual_status,
      hint_code: (if $hint_code == "" then null else $hint_code end),
      result_count: $result_count,
      contextual_result_count: $contextual_count,
      contract_ok: ($contract_ok == 1),
      expected_primary_present: ($expected_primary_present == 1),
      false_positive_count: $false_positive_count,
      exact_regression: ($exact_regression == 1),
      status_contract_failure: ($status_contract_failure == 1),
      primary_paths: $primary_paths,
      contextual_paths: $contextual_paths,
      output_path: $output_path,
      stderr_path: $stderr_path
    }'
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
    *)
      fail "unknown argument: $1"
      ;;
  esac
done

require_cmd cargo
require_cmd git
require_cmd jq

BASELINE_REF="${BASELINE_REF:-$(impact_default_baseline_ref "$ROOT_DIR")}"

if ! git -C "$ROOT_DIR" rev-parse --verify "$BASELINE_REF" >/dev/null 2>&1; then
  fail "baseline ref does not resolve: $BASELINE_REF"
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
rm -rf "$OUT_DIR/cases" "$OUT_DIR/fixture" "$OUT_DIR/runtime" "$OUT_DIR/case-results.jsonl" "$OUT_DIR/summary.json"

if [[ -z "$BASELINE_BIN" ]]; then
  BASELINE_WORKTREE=$(mktemp -d "${TMPDIR:-/tmp}/1up-impact-baseline.XXXXXX")
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
DETAILS_JSONL="$OUT_DIR/case-results.jsonl"
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
    "kind": "ambiguous",
    "name": "auth_context_only_unscoped",
    "expected_status": "empty",
    "expected_hint": "context_only",
    "args": ["--from-file", "src/auth/runtime.rs:5"]
  },
  {
    "kind": "ambiguous",
    "name": "auth_context_only_scoped",
    "expected_status": "empty_scoped",
    "expected_hint": "context_only",
    "args": ["--from-file", "src/auth/runtime.rs:5", "--scope", "src/auth"]
  },
  {
    "kind": "ambiguous",
    "name": "broad_symbol_refusal",
    "expected_status": "refused",
    "expected_hint": "narrow_with_scope",
    "args": ["--from-symbol", "load_config"]
  },
  {
    "kind": "ambiguous",
    "name": "neutral_helper_context_only",
    "expected_status": "empty",
    "expected_hint": "context_only",
    "args": ["--from-symbol", "boot_global_config"]
  },
  {
    "kind": "exact",
    "name": "auth_file_anchor",
    "expected_status": "expanded",
    "expected_primary": "src/auth/bootstrap.rs",
    "args": ["--from-file", "src/auth/runtime.rs"]
  },
  {
    "kind": "exact",
    "name": "auth_symbol_anchor",
    "expected_status": "expanded",
    "expected_primary": "src/auth/bootstrap.rs",
    "args": ["--from-symbol", "load_auth_config"]
  },
  {
    "kind": "exact",
    "name": "auth_scoped_symbol",
    "expected_status": "expanded_scoped",
    "expected_primary": "src/auth/config_builder.rs",
    "args": ["--from-symbol", "load_config", "--scope", "src/auth"]
  },
  {
    "kind": "exact",
    "name": "cache_symbol_anchor",
    "expected_status": "expanded",
    "expected_primary": "src/cache/worker.rs",
    "args": ["--from-symbol", "warm_cache_key"]
  }
]
EOF
)

: > "$DETAILS_JSONL"

while IFS= read -r case_json; do
  evaluate_case "baseline" "$BASELINE_BIN" "$BASELINE_HOME" "$BASELINE_REPO" "$case_json" >>"$DETAILS_JSONL"
  evaluate_case "candidate" "$CANDIDATE_BIN" "$CANDIDATE_HOME" "$CANDIDATE_REPO" "$case_json" >>"$DETAILS_JSONL"
done < <(jq -c '.[]' <<<"$CASES_JSON")

BASELINE_COMMIT=$(git -C "$ROOT_DIR" rev-parse "$BASELINE_REF")
CANDIDATE_COMMIT=$(git -C "$ROOT_DIR" rev-parse HEAD)

jq -s \
  --arg baseline_ref "$BASELINE_REF" \
  --arg baseline_commit "$BASELINE_COMMIT" \
  --arg candidate_commit "$CANDIDATE_COMMIT" \
  --arg output_dir "$OUT_DIR" \
  --arg generated_at "$(impact_utc_timestamp)" \
  '
  . as $cases
  | ($cases | map(select(.kind == "ambiguous" and .variant == "baseline") | .false_positive_count) | add // 0) as $false_positives_before
  | ($cases | map(select(.kind == "ambiguous" and .variant == "candidate") | .false_positive_count) | add // 0) as $false_positives_after
  | ($cases | map(select(.kind == "exact" and .variant == "baseline" and .exact_regression) | 1) | add // 0) as $exact_regressions_before
  | ($cases | map(select(.kind == "exact" and .variant == "candidate" and .exact_regression) | 1) | add // 0) as $exact_regressions_after
  | ($cases | map(select(.variant == "baseline" and .status_contract_failure) | 1) | add // 0) as $status_failures_before
  | ($cases | map(select(.variant == "candidate" and .status_contract_failure) | 1) | add // 0) as $status_failures_after
  | (if $false_positives_before == 0 then
       (if $false_positives_after == 0 then 100 else 0 end)
     else
       ((($false_positives_before - $false_positives_after) / $false_positives_before) * 100)
     end) as $reduction_pct
  | ($reduction_pct >= 50 and $exact_regressions_after == 0 and $status_failures_after == 0) as $gate_passed
  | {
      baseline_ref: $baseline_ref,
      baseline_commit: $baseline_commit,
      candidate_commit: $candidate_commit,
      generated_at: $generated_at,
      output_dir: $output_dir,
      ambiguous_cases: {
        total: ($cases | map(select(.kind == "ambiguous" and .variant == "candidate")) | length),
        false_positives_before: $false_positives_before,
        false_positives_after: $false_positives_after
      },
      exact_anchor_regressions: {
        before: $exact_regressions_before,
        after: $exact_regressions_after
      },
      status_contract_failures: {
        before: $status_failures_before,
        after: $status_failures_after
      },
      false_positive_reduction_pct: $reduction_pct,
      gate: {
        required_reduction_pct: 50,
        reduction_target_passed: ($reduction_pct >= 50),
        exact_anchor_regressions_after: $exact_regressions_after,
        status_contract_failures_after: $status_failures_after,
        gate_passed: $gate_passed
      },
      gate_passed: $gate_passed,
      cases: (
        $cases
        | group_by(.name)
        | map({
            name: .[0].name,
            kind: .[0].kind,
            expected_status: .[0].expected_status,
            expected_hint: (.[0].expected_hint // null),
            expected_primary: (.[0].expected_primary // null),
            baseline: (map(select(.variant == "baseline"))[0]),
            candidate: (map(select(.variant == "candidate"))[0])
          })
      )
    }
  ' "$DETAILS_JSONL" >"$SUMMARY_PATH"

gate_passed=$(jq -r '.gate_passed' "$SUMMARY_PATH")
reduction_pct=$(jq -r '.false_positive_reduction_pct' "$SUMMARY_PATH")

log "wrote summary to $SUMMARY_PATH"
log "accuracy gate: ${gate_passed} (false-positive reduction ${reduction_pct}%)"
printf '%s\n' "$SUMMARY_PATH"
