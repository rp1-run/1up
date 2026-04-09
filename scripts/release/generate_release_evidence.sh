#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
# shellcheck source=./common.sh
source "$SCRIPT_DIR/common.sh"

MANIFEST_PATH=""
MERGE_GATE_PATH=""
SECURITY_CHECK_PATH=""
EVAL_SUMMARY_PATH=""
EVAL_SKIPPED_REASON=""
BENCHMARK_SUMMARY_PATH=""
BENCHMARK_SKIPPED_REASON=""
ARCHIVE_VERIFICATION_PATH=""
PACKAGE_RECORD_PATH=""
OUTPUT_PATH=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --manifest)
      MANIFEST_PATH="${2:-}"
      shift 2
      ;;
    --merge-gate)
      MERGE_GATE_PATH="${2:-}"
      shift 2
      ;;
    --security-check)
      SECURITY_CHECK_PATH="${2:-}"
      shift 2
      ;;
    --eval-summary)
      EVAL_SUMMARY_PATH="${2:-}"
      shift 2
      ;;
    --eval-skipped-reason)
      EVAL_SKIPPED_REASON="${2:-}"
      shift 2
      ;;
    --benchmark-summary)
      BENCHMARK_SUMMARY_PATH="${2:-}"
      shift 2
      ;;
    --benchmark-skipped-reason)
      BENCHMARK_SKIPPED_REASON="${2:-}"
      shift 2
      ;;
    --archive-verification)
      ARCHIVE_VERIFICATION_PATH="${2:-}"
      shift 2
      ;;
    --package-record)
      PACKAGE_RECORD_PATH="${2:-}"
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

if [[ -z "$MANIFEST_PATH" || -z "$MERGE_GATE_PATH" || -z "$SECURITY_CHECK_PATH" || -z "$ARCHIVE_VERIFICATION_PATH" || -z "$OUTPUT_PATH" ]]; then
  fail "usage: $(basename "$0") --manifest <path> --merge-gate <path> --security-check <path> --archive-verification <path> [--eval-summary <path> | --eval-skipped-reason <reason>] [--benchmark-summary <path> | --benchmark-skipped-reason <reason>] [--package-record <path>] --output <path>"
fi

require_cmd jq
require_file "$MANIFEST_PATH"
require_file "$MERGE_GATE_PATH"
require_file "$SECURITY_CHECK_PATH"
require_file "$ARCHIVE_VERIFICATION_PATH"

validate_optional_evidence() {
  local label="$1"
  local summary_path="$2"
  local skipped_reason="$3"

  if [[ -n "$summary_path" && -n "${skipped_reason//[[:space:]]/}" ]]; then
    fail "${label} evidence accepts either a summary path or a skipped reason, not both"
  fi

  if [[ -z "$summary_path" && -z "${skipped_reason//[[:space:]]/}" ]]; then
    fail "${label} evidence requires a summary path or a skipped reason"
  fi

  if [[ -n "$summary_path" ]]; then
    require_file "$summary_path"
  fi
}

validate_optional_evidence "eval" "$EVAL_SUMMARY_PATH" "$EVAL_SKIPPED_REASON"
validate_optional_evidence "benchmark" "$BENCHMARK_SUMMARY_PATH" "$BENCHMARK_SKIPPED_REASON"

if ! jq -e '.workflow and .run_url and .conclusion and .required_checks' "$MERGE_GATE_PATH" >/dev/null 2>&1; then
  fail "merge gate metadata is missing required fields"
fi

if ! jq -e '
  (.archive_count | numbers)
  and (.archives | type == "array")
  and ((.archives | length) == .archive_count)
  and ([.archives[]
    | (.target and .archive and .sha256)
    and (.verified_contents.binary and .verified_contents.license and .verified_contents.readme)
    and (.smoke_test.status and .smoke_test.command and .smoke_test.output)
  ] | all)
' "$ARCHIVE_VERIFICATION_PATH" >/dev/null 2>&1; then
  fail "archive verification summary is missing required fields"
fi

manifest_version=$(manifest_value "$MANIFEST_PATH" '.version')
manifest_tag=$(manifest_value "$MANIFEST_PATH" '.git_tag')
manifest_commit_sha=$(manifest_value "$MANIFEST_PATH" '.commit_sha')
notes_source=$(manifest_value "$MANIFEST_PATH" '.notes_source')

security_json=$(jq -n \
  --arg asset "$(basename "$SECURITY_CHECK_PATH")" \
  '{
    status: "recorded",
    artifact: $asset
  }')

if [[ -n "$EVAL_SUMMARY_PATH" ]]; then
  eval_json=$(jq -n \
    --arg asset "$(basename "$EVAL_SUMMARY_PATH")" \
    '{
      status: "recorded",
      summary_asset: $asset
    }')
else
  eval_json=$(jq -n \
    --arg reason "$EVAL_SKIPPED_REASON" \
    '{
      status: "skipped",
      skipped_reason: $reason
    }')
fi

if [[ -n "$BENCHMARK_SUMMARY_PATH" ]]; then
  benchmark_json=$(jq -n \
    --arg asset "$(basename "$BENCHMARK_SUMMARY_PATH")" \
    '{
      status: "recorded",
      summary_asset: $asset
    }')
else
  benchmark_json=$(jq -n \
    --arg reason "$BENCHMARK_SKIPPED_REASON" \
    '{
      status: "skipped",
      skipped_reason: $reason
    }')
fi

archive_json=$(jq --arg asset "$(basename "$ARCHIVE_VERIFICATION_PATH")" \
  '. + {
    status: "recorded",
    summary_asset: $asset
  }' "$ARCHIVE_VERIFICATION_PATH")

if [[ -n "$PACKAGE_RECORD_PATH" ]]; then
  require_file "$PACKAGE_RECORD_PATH"

  if [[ "$(jq -r '.version' "$PACKAGE_RECORD_PATH")" != "$manifest_version" ]]; then
    fail "package publication record version does not match release manifest"
  fi

  if [[ "$(jq -r '.git_tag' "$PACKAGE_RECORD_PATH")" != "$manifest_tag" ]]; then
    fail "package publication record tag does not match release manifest"
  fi

  packages_json=$(jq --arg asset "$(basename "$PACKAGE_RECORD_PATH")" '
    {
      status: "recorded",
      record_asset: $asset,
      homebrew: .packages.homebrew,
      scoop: .packages.scoop
    }' "$PACKAGE_RECORD_PATH")
else
  packages_json=$(jq -n '
    {
      status: "pending",
      pending_reason: "Package publication runs after the GitHub Release is published."
    }')
fi

mkdir -p "$(dirname "$OUTPUT_PATH")"

jq -n \
  --arg version "$manifest_version" \
  --arg git_tag "$manifest_tag" \
  --arg generated_at "$(utc_timestamp)" \
  --arg commit_sha "$manifest_commit_sha" \
  --arg release_manifest_asset "$(basename "$MANIFEST_PATH")" \
  --arg notes_source "$notes_source" \
  --argjson merge_gate "$(cat "$MERGE_GATE_PATH")" \
  --argjson security_check "$security_json" \
  --argjson evals "$eval_json" \
  --argjson benchmarks "$benchmark_json" \
  --argjson archive_verification "$archive_json" \
  --argjson packages "$packages_json" \
  '{
    version: $version,
    git_tag: $git_tag,
    generated_at: $generated_at,
    commit_sha: $commit_sha,
    release_manifest_asset: $release_manifest_asset,
    notes_source: $notes_source,
    merge_gate: $merge_gate,
    security_check: $security_check,
    evals: $evals,
    benchmarks: $benchmarks,
    archive_verification: $archive_verification,
    packages: $packages
  }' \
  >"$OUTPUT_PATH"

log "wrote release evidence to $(relative_path "$OUTPUT_PATH")"
