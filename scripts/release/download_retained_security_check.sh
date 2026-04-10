#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
# shellcheck source=./common.sh
source "$SCRIPT_DIR/common.sh"

REPO="${REPO_SLUG}"
RUN_ID=""
ARTIFACT_NAME="security-check"
OUTPUT_PATH=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --repo)
      REPO="${2:-}"
      shift 2
      ;;
    --run-id)
      RUN_ID="${2:-}"
      shift 2
      ;;
    --artifact-name)
      ARTIFACT_NAME="${2:-}"
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

if [[ -z "$REPO" || -z "$RUN_ID" || -z "$OUTPUT_PATH" ]]; then
  fail "usage: $(basename "$0") --repo <owner/name> --run-id <id> [--artifact-name <name>] --output <path>"
fi

require_cmd gh
require_cmd jq

TMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/oneup-security-artifact.XXXXXX")

cleanup() {
  rm -rf "$TMP_DIR"
}
trap cleanup EXIT

gh run download "$RUN_ID" \
  --repo "$REPO" \
  --name "$ARTIFACT_NAME" \
  --dir "$TMP_DIR" >/dev/null

artifact_path=$(find "$TMP_DIR" -type f -name 'security-check.json' | head -n 1)
if [[ -z "$artifact_path" ]]; then
  fail "artifact ${ARTIFACT_NAME} from run ${RUN_ID} did not contain security-check.json"
fi

if ! jq -e '.status and .summary and .steps' "$artifact_path" >/dev/null 2>&1; then
  fail "downloaded security-check.json is missing required fields"
fi

mkdir -p "$(dirname "$OUTPUT_PATH")"
cp "$artifact_path" "$OUTPUT_PATH"

log "downloaded retained security evidence from run ${RUN_ID} to $(relative_path "$OUTPUT_PATH")"
