#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
# shellcheck source=./common.sh
source "$SCRIPT_DIR/common.sh"

MANIFEST_PATH=""
OUTPUT_PATH=""
HOMEBREW_COMMIT_SHA=""
SCOOP_COMMIT_SHA=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --manifest)
      MANIFEST_PATH="${2:-}"
      shift 2
      ;;
    --output)
      OUTPUT_PATH="${2:-}"
      shift 2
      ;;
    --homebrew-commit)
      HOMEBREW_COMMIT_SHA="${2:-}"
      shift 2
      ;;
    --scoop-commit)
      SCOOP_COMMIT_SHA="${2:-}"
      shift 2
      ;;
    *)
      fail "unknown argument: $1"
      ;;
  esac
done

if [[ -z "$MANIFEST_PATH" || -z "$OUTPUT_PATH" || -z "$HOMEBREW_COMMIT_SHA" || -z "$SCOOP_COMMIT_SHA" ]]; then
  fail "usage: $(basename "$0") --manifest <path> --output <path> --homebrew-commit <sha> --scoop-commit <sha>"
fi

require_cmd jq
require_file "$MANIFEST_PATH"

VERSION=$(manifest_value "$MANIFEST_PATH" '.version')
GIT_TAG=$(manifest_value "$MANIFEST_PATH" '.git_tag')
HOMEBREW_REPO=$(manifest_value "$MANIFEST_PATH" '.channels.homebrew_tap')
SCOOP_REPO=$(manifest_value "$MANIFEST_PATH" '.channels.scoop_bucket')
GENERATED_AT=$(date -u +"%Y-%m-%dT%H:%M:%SZ")

mkdir -p "$(dirname "$OUTPUT_PATH")"

jq -n \
  --arg version "$VERSION" \
  --arg git_tag "$GIT_TAG" \
  --arg generated_at "$GENERATED_AT" \
  --arg homebrew_repo "$HOMEBREW_REPO" \
  --arg homebrew_path "Formula/1up.rb" \
  --arg homebrew_commit_sha "$HOMEBREW_COMMIT_SHA" \
  --arg scoop_repo "$SCOOP_REPO" \
  --arg scoop_path "bucket/1up.json" \
  --arg scoop_commit_sha "$SCOOP_COMMIT_SHA" \
  '{
    version: $version,
    git_tag: $git_tag,
    generated_at: $generated_at,
    packages: {
      homebrew: {
        repo: $homebrew_repo,
        path: $homebrew_path,
        commit_sha: $homebrew_commit_sha,
        commit_url: ("https://github.com/" + $homebrew_repo + "/commit/" + $homebrew_commit_sha)
      },
      scoop: {
        repo: $scoop_repo,
        path: $scoop_path,
        commit_sha: $scoop_commit_sha,
        commit_url: ("https://github.com/" + $scoop_repo + "/commit/" + $scoop_commit_sha)
      }
    }
  }' \
  >"$OUTPUT_PATH"

log "wrote package publication record to $(relative_path "$OUTPUT_PATH")"
