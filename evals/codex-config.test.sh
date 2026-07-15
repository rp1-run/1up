#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
WORK_DIR=$(mktemp -d "${TMPDIR:-/tmp}/1up-codex-config-test.XXXXXX")

cleanup() {
  rm -rf -- "$WORK_DIR"
}
trap cleanup EXIT HUP INT TERM

fail() {
  echo "Codex config regression failed: $*" >&2
  exit 1
}

mkdir -p "$WORK_DIR/repo/.git" "$WORK_DIR/promptfoo-state"
CAPTURE="$WORK_DIR/codex-argv.tsv"
FAKE_CODEX="$WORK_DIR/fake-codex"

cat > "$FAKE_CODEX" <<'FAKE'
#!/usr/bin/env bash
set -euo pipefail

INPUT=$(cat)
case "$INPUT" in
  *ONEUP_TARGET*) KIND=oneup ;;
  *BASELINE_TARGET*) KIND=baseline ;;
  *) KIND=grader ;;
esac
printf '%s\t%s\n' "$KIND" "$*" >> "$FAKE_CODEX_CAPTURE"

printf '%s\n' "{\"type\":\"thread.started\",\"thread_id\":\"fake-$$\"}"
printf '%s\n' '{"type":"item.completed","item":{"id":"message-1","type":"agent_message","text":"{\"pass\":true,\"score\":1,\"reason\":\"fake grader passed\"}"}}'
printf '%s\n' '{"type":"turn.completed","usage":{"input_tokens":1,"cached_input_tokens":0,"output_tokens":1,"reasoning_output_tokens":0}}'
FAKE
chmod +x "$FAKE_CODEX"

cat > "$WORK_DIR/evals.yaml" <<EOF
description: "Codex target/grader argv regression"
evaluateOptions:
  maxConcurrency: 1
prompts:
  - id: file://prompt-1up.txt
    label: 1up
  - id: file://prompt-baseline.txt
    label: baseline
providers:
  - id: openai:codex-sdk
    label: 1up-agent
    prompts: [1up]
    config:
      model: gpt-5.6-luna
      working_dir: "$WORK_DIR/repo"
      codex_path_override: "$FAKE_CODEX"
      cli_env:
        PATH: "$PATH"
        FAKE_CODEX_CAPTURE: "$CAPTURE"
      cli_config:
        suppress_unstable_features_warning: true
        features:
          apps: false
          non_prefixed_mcp_tool_names: true
          plugins: false
        mcp_servers:
          oneup:
            command: 1up
            args: ["mcp", "--path", "$WORK_DIR/repo"]
            required: true
            startup_timeout_sec: 30
  - id: openai:codex-sdk
    label: baseline-agent
    prompts: [baseline]
    config:
      model: gpt-5.6-luna
      working_dir: "$WORK_DIR/repo"
      codex_path_override: "$FAKE_CODEX"
      cli_env:
        PATH: "$PATH"
        FAKE_CODEX_CAPTURE: "$CAPTURE"
defaultTest:
  options:
    provider:
      id: openai:codex-sdk:gpt-5.6-luna
      config:
        codex_path_override: "$FAKE_CODEX"
        cli_env:
          PATH: "$PATH"
          FAKE_CODEX_CAPTURE: "$CAPTURE"
tests:
  - description: argv isolation
    vars: {}
    assert:
      - type: llm-rubric
        value: The fake answer passes.
EOF
printf 'ONEUP_TARGET\n' > "$WORK_DIR/prompt-1up.txt"
printf 'BASELINE_TARGET\n' > "$WORK_DIR/prompt-baseline.txt"

PROMPTFOO_CONFIG_DIR="$WORK_DIR/promptfoo-state" \
  "$SCRIPT_DIR/node_modules/.bin/promptfoo" eval \
  -c "$WORK_DIR/evals.yaml" --no-table --no-progress-bar --no-cache \
  > "$WORK_DIR/promptfoo.log" 2>&1 || {
    cat "$WORK_DIR/promptfoo.log" >&2
    fail "fake Codex Promptfoo evaluation failed"
  }

[ -s "$CAPTURE" ] || fail "fake Codex did not capture invocations"
[ "$(awk -F '\t' '$1 == "oneup" { count++ } END { print count + 0 }' "$CAPTURE")" = 1 ] || fail "expected exactly one 1up target"
[ "$(awk -F '\t' '$1 == "baseline" { count++ } END { print count + 0 }' "$CAPTURE")" = 1 ] || fail "expected exactly one baseline target"
[ "$(awk -F '\t' '$1 == "grader" { count++ } END { print count + 0 }' "$CAPTURE")" -ge 1 ] || fail "expected at least one distinct grader invocation"

ONEUP_ARGV=$(awk -F '\t' '$1 == "oneup" { print $2 }' "$CAPTURE")
BASELINE_ARGV=$(awk -F '\t' '$1 == "baseline" { print $2 }' "$CAPTURE")
GRADER_ARGV=$(awk -F '\t' '$1 == "grader" { print $2 }' "$CAPTURE")

case "$ONEUP_ARGV" in
  *'features.apps=false'*'features.non_prefixed_mcp_tool_names=true'*'features.plugins=false'*'mcp_servers.oneup.command="1up"'*"mcp_servers.oneup.args=[\"mcp\", \"--path\", \"$WORK_DIR/repo\"]"*'mcp_servers.oneup.required=true'*'mcp_servers.oneup.startup_timeout_sec=30'*) ;;
  *) fail "1up target did not receive the workspace-bound oneup MCP overrides" ;;
esac
case "$ONEUP_ARGV" in
  *'suppress_unstable_features_warning=true'*) ;;
  *) fail "1up target did not receive the unstable-feature warning suppression" ;;
esac
case "$BASELINE_ARGV" in
  *mcp_servers.oneup*) fail "baseline target inherited oneup MCP overrides" ;;
  *non_prefixed_mcp_tool_names*) fail "baseline target inherited oneup tool-name behavior" ;;
  *'features.apps=false'*|*'features.plugins=false'*) fail "baseline target inherited oneup tool-surface isolation" ;;
  *suppress_unstable_features_warning*) fail "baseline target inherited the unstable-feature warning suppression" ;;
esac
case "$GRADER_ARGV" in
  *mcp_servers.oneup*) fail "grader inherited oneup MCP overrides" ;;
  *non_prefixed_mcp_tool_names*) fail "grader inherited oneup tool-name behavior" ;;
  *'features.apps=false'*|*'features.plugins=false'*) fail "grader inherited oneup tool-surface isolation" ;;
  *suppress_unstable_features_warning*) fail "grader inherited the unstable-feature warning suppression" ;;
esac

echo "Codex config regression passed"
