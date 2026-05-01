#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
# shellcheck source=./common.sh
source "$SCRIPT_DIR/common.sh"

HOST=""
STATUS=""
HOST_VERSION=""
SETUP_MODE=""
REPO_PATH=""
READINESS_STATUS=""
DISCOVERY_FLOW_STATUS=""
SKIPPED_REASON=""
OUTPUT_PATH=""
OBSERVED_TOOLS=()

usage() {
  cat >&2 <<'USAGE'
usage:
  record_mcp_host_smoke.sh --output <path> --host <host> --status recorded --host-version <version> --setup-mode wrapper|add-mcp|manual --repo <path> --tool <name>... --readiness <status> --discovery-flow <status>
  record_mcp_host_smoke.sh --output <path> --host <host> --status skipped --setup-mode skipped --reason <reason>

hosts: codex, claude-code, cursor, vscode, github-copilot-cli, generic
USAGE
}

trim() {
  local value="$1"
  value="${value#"${value%%[![:space:]]*}"}"
  value="${value%"${value##*[![:space:]]}"}"
  printf '%s\n' "$value"
}

add_tools_csv() {
  local csv="$1"
  local entry
  local -a entries=()

  IFS=',' read -r -a entries <<<"$csv"
  for entry in "${entries[@]}"; do
    entry=$(trim "$entry")
    if [[ -n "$entry" ]]; then
      OBSERVED_TOOLS+=("$entry")
    fi
  done
}

normalize_host() {
  case "$1" in
    codex) printf 'codex\n' ;;
    claude|claude-code) printf 'claude-code\n' ;;
    cursor) printf 'cursor\n' ;;
    vscode|vs-code) printf 'vscode\n' ;;
    github-copilot-cli|copilot) printf 'github-copilot-cli\n' ;;
    generic|generic-mcp) printf 'generic\n' ;;
    *) fail "unsupported MCP host: $1" ;;
  esac
}

validate_setup_mode() {
  case "$1" in
    wrapper|add-mcp|manual|skipped) ;;
    *) fail "unsupported setup mode: $1" ;;
  esac
}

validate_status() {
  case "$1" in
    recorded|skipped) ;;
    *) fail "unsupported host smoke status: $1" ;;
  esac
}

validate_readiness_status() {
  case "$1" in
    missing|indexing|stale|ready|degraded) ;;
    *) fail "unsupported readiness status: $1" ;;
  esac
}

validate_discovery_flow_status() {
  case "$1" in
    passed|failed|skipped) ;;
    *) fail "unsupported discovery flow status: $1" ;;
  esac
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --help|-h)
      usage
      exit 0
      ;;
    --host)
      HOST="${2:-}"
      shift 2
      ;;
    --status)
      STATUS="${2:-}"
      shift 2
      ;;
    --host-version)
      HOST_VERSION="${2:-}"
      shift 2
      ;;
    --setup-mode)
      SETUP_MODE="${2:-}"
      shift 2
      ;;
    --repo|--repo-path)
      REPO_PATH="${2:-}"
      shift 2
      ;;
    --tool)
      OBSERVED_TOOLS+=("${2:-}")
      shift 2
      ;;
    --tools)
      add_tools_csv "${2:-}"
      shift 2
      ;;
    --readiness|--readiness-status)
      READINESS_STATUS="${2:-}"
      shift 2
      ;;
    --discovery-flow|--discovery-flow-status)
      DISCOVERY_FLOW_STATUS="${2:-}"
      shift 2
      ;;
    --reason|--skipped-reason)
      SKIPPED_REASON="${2:-}"
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

if [[ -z "$HOST" || -z "$STATUS" || -z "$SETUP_MODE" || -z "$OUTPUT_PATH" ]]; then
  usage
  fail "missing required host smoke recorder arguments"
fi

require_cmd jq

HOST=$(normalize_host "$HOST")
validate_status "$STATUS"
validate_setup_mode "$SETUP_MODE"

if [[ "$STATUS" == "recorded" ]]; then
  if [[ "$SETUP_MODE" == "skipped" ]]; then
    fail "recorded host smoke evidence requires setup mode wrapper, add-mcp, or manual"
  fi
  if [[ -z "$HOST_VERSION" || -z "$REPO_PATH" || -z "$READINESS_STATUS" || -z "$DISCOVERY_FLOW_STATUS" ]]; then
    fail "recorded host smoke evidence requires --host-version, --repo, --readiness, and --discovery-flow"
  fi
  if [[ "${#OBSERVED_TOOLS[@]}" -eq 0 ]]; then
    fail "recorded host smoke evidence requires at least one --tool or --tools value"
  fi
  validate_readiness_status "$READINESS_STATUS"
  validate_discovery_flow_status "$DISCOVERY_FLOW_STATUS"
else
  if [[ "$SETUP_MODE" != "skipped" ]]; then
    fail "skipped host smoke evidence requires setup mode skipped"
  fi
  if [[ -z "${SKIPPED_REASON//[[:space:]]/}" ]]; then
    fail "skipped host smoke evidence requires --reason"
  fi
fi

mkdir -p "$(dirname "$OUTPUT_PATH")"

recorded_at=$(utc_timestamp)

if [[ "$STATUS" == "recorded" ]]; then
  tools_json=$(printf '%s\n' "${OBSERVED_TOOLS[@]}" | jq -R . | jq -s .)
  record_json=$(jq -n \
    --arg status "$STATUS" \
    --arg host "$HOST" \
    --arg host_version "$HOST_VERSION" \
    --arg setup_mode "$SETUP_MODE" \
    --arg repo_path "$REPO_PATH" \
    --arg readiness "$READINESS_STATUS" \
    --arg discovery_flow "$DISCOVERY_FLOW_STATUS" \
    --arg recorded_at "$recorded_at" \
    --argjson tools "$tools_json" \
    '{
      status: $status,
      host: $host,
      host_version: $host_version,
      setup_mode: $setup_mode,
      repo_path: $repo_path,
      tools_listed: (($tools | length) > 0),
      tools: $tools,
      readiness: $readiness,
      discovery_flow: $discovery_flow,
      recorded_at: $recorded_at
    }')
else
  record_json=$(jq -n \
    --arg status "$STATUS" \
    --arg host "$HOST" \
    --arg setup_mode "$SETUP_MODE" \
    --arg reason "$SKIPPED_REASON" \
    --arg recorded_at "$recorded_at" \
    --arg host_version "$HOST_VERSION" \
    '{
      status: $status,
      host: $host,
      setup_mode: $setup_mode,
      reason: $reason,
      recorded_at: $recorded_at
    }
    + (if $host_version == "" then {} else {host_version: $host_version} end)')
fi

tmp_path="$OUTPUT_PATH.tmp.$$"
if [[ -f "$OUTPUT_PATH" ]]; then
  if ! jq \
    --arg generated_at "$recorded_at" \
    --arg host "$HOST" \
    --argjson record "$record_json" \
    '
      if (.hosts | type) != "object" then
        error("existing host smoke evidence is missing hosts object")
      else
        . + {
          generated_at: $generated_at,
          schema: "mcp_host_smoke.v1"
        }
        | .hosts[$host] = $record
      end
    ' "$OUTPUT_PATH" >"$tmp_path"; then
    rm -f "$tmp_path"
    fail "failed to update host smoke evidence at $(relative_path "$OUTPUT_PATH")"
  fi
else
  jq -n \
    --arg generated_at "$recorded_at" \
    --arg host "$HOST" \
    --argjson record "$record_json" \
    '{
      generated_at: $generated_at,
      schema: "mcp_host_smoke.v1",
      hosts: {
        ($host): $record
      }
    }' >"$tmp_path"
fi

mv "$tmp_path" "$OUTPUT_PATH"
log "recorded ${HOST} MCP host smoke evidence at $(relative_path "$OUTPUT_PATH")"
