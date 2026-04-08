#!/usr/bin/env bash
set -uo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)
EVIDENCE_DIR="$ROOT_DIR/target/security"
EVIDENCE_PATH="$EVIDENCE_DIR/security-check.json"
TMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/oneup-security-check.XXXXXX")
STEP_DIR="$TMP_DIR/steps"

mkdir -p "$STEP_DIR"

cleanup() {
  rm -rf "$TMP_DIR"
}
trap cleanup EXIT

require_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    printf 'missing required command: %s\n' "$1" >&2
    exit 1
  fi
}

log() {
  printf '[security-check] %s\n' "$*" >&2
}

timestamp() {
  date -u +"%Y-%m-%dT%H:%M:%SZ"
}

command_string() {
  printf '%q ' "$@"
}

run_step() {
  local id="$1"
  local kind="$2"
  local label="$3"
  shift 3

  local stdout_path="$TMP_DIR/${id}.stdout"
  local stderr_path="$TMP_DIR/${id}.stderr"
  local start_seconds="$SECONDS"
  local status="passed"
  local exit_code=0
  local command_text
  local stdout_excerpt=""
  local stderr_excerpt=""

  command_text=$(command_string "$@")

  log "running ${label}"
  if "$@" >"$stdout_path" 2>"$stderr_path"; then
    log "passed ${label}"
  else
    exit_code=$?
    status="failed"
    OVERALL_STATUS="failed"
    FAILED_STEPS+=("$label")
    log "failed ${label} (exit=${exit_code})"
  fi

  if [[ -s "$stdout_path" ]]; then
    stdout_excerpt=$(tail -n 20 "$stdout_path")
  fi
  if [[ -s "$stderr_path" ]]; then
    stderr_excerpt=$(tail -n 20 "$stderr_path")
  fi

  jq -n \
    --arg id "$id" \
    --arg kind "$kind" \
    --arg label "$label" \
    --arg command "${command_text% }" \
    --arg status "$status" \
    --argjson exit_code "$exit_code" \
    --argjson duration_ms "$(( (SECONDS - start_seconds) * 1000 ))" \
    --arg stdout_excerpt "$stdout_excerpt" \
    --arg stderr_excerpt "$stderr_excerpt" \
    '{
      id: $id,
      kind: $kind,
      label: $label,
      command: $command,
      status: $status,
      exit_code: $exit_code,
      duration_ms: $duration_ms
    }
    + (if $stdout_excerpt == "" then {} else {stdout_excerpt: $stdout_excerpt} end)
    + (if $stderr_excerpt == "" then {} else {stderr_excerpt: $stderr_excerpt} end)' \
    >"$STEP_DIR/${id}.json"
}

write_default_audit_summary() {
  jq -n \
    --arg policy_path ".cargo/audit.toml" \
    '{
      policy_path: $policy_path,
      raw_report_available: false,
      config: {
        ignored_advisories: [],
        informational_warnings: [],
        severity_threshold: null
      },
      policy_exceptions: [],
      vulnerabilities: {
        found: false,
        count: 0,
        blocking: []
      },
      warnings: {}
    }' \
    >"$TMP_DIR/audit-summary.json"
}

write_audit_summary() {
  local audit_stdout="$TMP_DIR/cargo_audit.stdout"

  if [[ ! -s "$audit_stdout" ]] || ! jq -e '.settings and .vulnerabilities' "$audit_stdout" >/dev/null 2>&1; then
    write_default_audit_summary
    return
  fi

  jq \
    --arg policy_path ".cargo/audit.toml" \
    '{
      policy_path: $policy_path,
      raw_report_available: true,
      config: {
        ignored_advisories: (.settings.ignore // []),
        informational_warnings: (.settings.informational_warnings // []),
        severity_threshold: .settings.severity
      },
      policy_exceptions: (.settings.ignore // []),
      vulnerabilities: {
        found: (.vulnerabilities.found // false),
        count: (.vulnerabilities.count // 0),
        blocking: [
          (.vulnerabilities.list // [])[]
          | {
              id: .advisory.id,
              package: .package.name,
              version: .package.version,
              title: .advisory.title,
              patched_versions: (.versions.patched // [])
            }
        ]
      },
      warnings: (
        (.warnings // {})
        | with_entries(
            .value |= [
              .[]?
              | {
                  kind: .kind,
                  id: .advisory.id,
                  package: .package.name,
                  version: .package.version,
                  title: .advisory.title
                }
            ]
          )
      )
    }' \
    "$audit_stdout" >"$TMP_DIR/audit-summary.json"
}

write_evidence() {
  local generated_at="$1"
  local steps_json
  local audit_json

  steps_json=$(jq -s '.' "$STEP_DIR"/*.json)
  audit_json=$(cat "$TMP_DIR/audit-summary.json")

  mkdir -p "$EVIDENCE_DIR"

  jq -n \
    --arg generated_at "$generated_at" \
    --arg repo_root "$ROOT_DIR" \
    --arg evidence_path "$EVIDENCE_PATH" \
    --arg status "$OVERALL_STATUS" \
    --argjson steps "$steps_json" \
    --argjson audit "$audit_json" \
    '{
      generated_at: $generated_at,
      repo_root: $repo_root,
      evidence_path: $evidence_path,
      status: $status,
      summary: {
        total_steps: ($steps | length),
        passed_steps: ([ $steps[] | select(.status == "passed") ] | length),
        failed_steps: ([ $steps[] | select(.status == "failed") ] | length)
      },
      steps: $steps,
      audit: $audit
    }' \
    >"$EVIDENCE_PATH"
}

require_cmd cargo
require_cmd jq

cd "$ROOT_DIR"

OVERALL_STATUS="passed"
FAILED_STEPS=()
GENERATED_AT=$(timestamp)

run_step "fmt" "formatter" "cargo fmt --check" cargo fmt --check
run_step "clippy" "linter" "cargo clippy --all-targets -- -D warnings" cargo clippy --all-targets -- -D warnings
run_step "tests_all" "tests" "cargo test --quiet" cargo test --quiet
run_step "tests_shared_fs" "security-tests" "cargo test --quiet shared::fs::tests" cargo test --quiet shared::fs::tests
run_step "tests_daemon_search" "security-tests" "cargo test --quiet daemon::search_service::tests" cargo test --quiet daemon::search_service::tests
run_step "cargo_audit" "audit" "cargo audit --json" cargo audit --json

write_audit_summary
write_evidence "$GENERATED_AT"

if [[ "$OVERALL_STATUS" == "passed" ]]; then
  log "all checks passed"
else
  log "failed steps: ${FAILED_STEPS[*]}"
fi
log "evidence written to ${EVIDENCE_PATH}"

[[ "$OVERALL_STATUS" == "passed" ]]
