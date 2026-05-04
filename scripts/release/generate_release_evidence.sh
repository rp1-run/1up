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
MCP_HOST_SMOKE_PATH=""
MCP_HOST_SKIPPED_REASON=""
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
    --mcp-host-smoke)
      MCP_HOST_SMOKE_PATH="${2:-}"
      shift 2
      ;;
    --mcp-host-skipped-reason)
      MCP_HOST_SKIPPED_REASON="${2:-}"
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
  fail "usage: $(basename "$0") --manifest <path> --merge-gate <path> --security-check <path> --archive-verification <path> [--mcp-host-smoke <path> | --mcp-host-skipped-reason <reason>] [--eval-summary <path> | --eval-skipped-reason <reason>] [--benchmark-summary <path> | --benchmark-skipped-reason <reason>] [--package-record <path>] --output <path>"
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

validate_mcp_host_evidence() {
  if [[ -n "$MCP_HOST_SMOKE_PATH" && -n "${MCP_HOST_SKIPPED_REASON//[[:space:]]/}" ]]; then
    fail "MCP host smoke evidence accepts either a summary path or a skipped reason, not both"
  fi

  if [[ -z "$MCP_HOST_SMOKE_PATH" && -z "${MCP_HOST_SKIPPED_REASON//[[:space:]]/}" ]]; then
    MCP_HOST_SKIPPED_REASON="Live MCP host smoke checks require local proprietary agent hosts and were not recorded for this release evidence run."
  fi

  if [[ -n "$MCP_HOST_SMOKE_PATH" ]]; then
    require_file "$MCP_HOST_SMOKE_PATH"

    if ! jq -e '
      def valid_readiness:
        . as $status | ["missing", "indexing", "stale", "ready", "degraded", "blocked"] | index($status) != null;
      def valid_discovery_flow:
        . as $status | ["passed", "failed", "skipped"] | index($status) != null;
      def valid_recorded:
        (.status == "recorded")
        and (.host | type == "string" and length > 0)
        and (.host_version | type == "string" and length > 0)
        and (.setup_mode as $mode | ["wrapper", "add-mcp", "manual"] | index($mode) != null)
        and (.repo_path | type == "string" and length > 0)
        and (.tools_listed == true)
        and (.tools | type == "array" and length > 0)
        and (.readiness | valid_readiness)
        and (.discovery_flow | valid_discovery_flow);
      def valid_skipped:
        (.status == "skipped")
        and (.setup_mode == "skipped")
        and (.reason | type == "string" and length > 0);
      (.schema == "mcp_host_smoke.v1")
      and (.hosts | type == "object")
      and ((.hosts | length) > 0)
      and ([.hosts[] | (valid_recorded or valid_skipped)] | all)
    ' "$MCP_HOST_SMOKE_PATH" >/dev/null 2>&1; then
      fail "MCP host smoke evidence is missing required recorded or skipped fields"
    fi
  fi
}

validate_optional_evidence "eval" "$EVAL_SUMMARY_PATH" "$EVAL_SKIPPED_REASON"
validate_optional_evidence "benchmark" "$BENCHMARK_SUMMARY_PATH" "$BENCHMARK_SKIPPED_REASON"
validate_mcp_host_evidence

if ! jq -e '.workflow and .run_url and .conclusion and .required_checks' "$MERGE_GATE_PATH" >/dev/null 2>&1; then
  fail "merge gate metadata is missing required fields"
fi

if ! jq -e '
  def canonical_mcp_tools_present:
    (.mcp_smoke_test.tools | type == "array")
    and (.mcp_smoke_test.tools as $tools
      | ["oneup_prepare", "oneup_search", "oneup_read", "oneup_symbol", "oneup_impact"]
      | all(. as $tool | $tools | index($tool)));
  def valid_readiness:
    . as $status | ["missing", "indexing", "stale", "ready", "degraded", "blocked"] | index($status) != null;
  def valid_p2_mcp_smoke:
    .mcp_smoke_test as $smoke
    | ($smoke.schema == "mcp_smoke.v2")
    and ($smoke.presentation_free == true)
    and ($smoke.discovery_flow.status == "passed")
    and ($smoke.exercised_tools | type == "array")
    and (["oneup_prepare", "oneup_search", "oneup_read", "oneup_symbol"]
      | all(. as $tool | ($smoke.exercised_tools | index($tool) != null)))
    and ($smoke.tool_calls | type == "array")
    and (["prepare", "search", "read_handle", "symbol", "read_location"]
      | all(. as $label
        | ([$smoke.tool_calls[]
          | select(.label == $label)
          | select((.name | type == "string" and length > 0)
              and (.status | type == "string" and length > 0)
              and (.structured_content == true)
              and (.presentation_free == true))]
          | length > 0)))
    and ($smoke.structured_content_present.prepare == true)
    and ($smoke.structured_content_present.search == true)
    and ($smoke.structured_content_present.read_handle == true)
    and ($smoke.structured_content_present.symbol == true)
    and ($smoke.structured_content_present.read_location == true);
  (.archive_count | numbers)
  and (.archives | type == "array")
  and ((.archives | length) == .archive_count)
  and ([.archives[]
    | (.target and .archive and .sha256)
    and (.verified_contents.binary and .verified_contents.license and .verified_contents.readme)
    and (.smoke_test.status == "passed")
    and (.smoke_test.command and .smoke_test.output)
    and (.mcp_smoke_test.status == "passed")
    and (.mcp_smoke_test.binary and .mcp_smoke_test.version and .mcp_smoke_test.server_command)
    and canonical_mcp_tools_present
    and (.mcp_smoke_test.readiness_status | valid_readiness)
    and (.mcp_smoke_test.stdout_protocol_clean == true)
    and valid_p2_mcp_smoke
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
  eval_json=$(jq \
    --arg asset "$(basename "$EVAL_SUMMARY_PATH")" \
    '{
      status: "recorded",
      evidence_type: "mcp_adoption",
      summary_asset: $asset,
      summary: .
    }' "$EVAL_SUMMARY_PATH")
else
  eval_json=$(jq -n \
    --arg reason "$EVAL_SKIPPED_REASON" \
    '{
      status: "skipped",
      evidence_type: "mcp_adoption",
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

if [[ -n "$MCP_HOST_SMOKE_PATH" ]]; then
  mcp_host_smoke_json=$(jq --arg asset "$(basename "$MCP_HOST_SMOKE_PATH")" \
    '. + {
      status: "recorded",
      evidence_asset: $asset
    }' "$MCP_HOST_SMOKE_PATH")
else
  mcp_host_smoke_json=$(jq -n \
    --arg reason "$MCP_HOST_SKIPPED_REASON" \
    '{
      status: "skipped",
      schema: "mcp_host_smoke.v1",
      skipped_reason: $reason,
      hosts: {
        "codex": { status: "skipped", host: "codex", setup_mode: "skipped", reason: $reason },
        "claude-code": { status: "skipped", host: "claude-code", setup_mode: "skipped", reason: $reason },
        "cursor": { status: "skipped", host: "cursor", setup_mode: "skipped", reason: $reason },
        "vscode": { status: "skipped", host: "vscode", setup_mode: "skipped", reason: $reason },
        "github-copilot-cli": { status: "skipped", host: "github-copilot-cli", setup_mode: "skipped", reason: $reason },
        "generic": { status: "skipped", host: "generic", setup_mode: "skipped", reason: $reason }
      }
    }')
fi

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
  --argjson mcp_host_smoke "$mcp_host_smoke_json" \
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
    mcp_host_smoke: $mcp_host_smoke,
    packages: $packages
  }' \
  >"$OUTPUT_PATH"

log "wrote release evidence to $(relative_path "$OUTPUT_PATH")"
