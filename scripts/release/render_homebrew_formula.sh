#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
# shellcheck source=./common.sh
source "$SCRIPT_DIR/common.sh"

MANIFEST_PATH=""
OUTPUT_PATH=""
TEMPLATE_PATH="${ONEUP_HOMEBREW_TEMPLATE_PATH:-$ROOT_DIR/packaging/homebrew/1up.rb.tmpl}"

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
    --template)
      TEMPLATE_PATH="${2:-}"
      shift 2
      ;;
    *)
      fail "unknown argument: $1"
      ;;
  esac
done

if [[ -z "$MANIFEST_PATH" || -z "$OUTPUT_PATH" ]]; then
  fail "usage: $(basename "$0") --manifest <path> --output <path> [--template <path>]"
fi

require_cmd jq
require_file "$MANIFEST_PATH"

VERSION=$(manifest_value "$MANIFEST_PATH" '.version')
LICENSE=$(manifest_value "$MANIFEST_PATH" '.license')
MACOS_ARM64_URL=$(manifest_release_download_url "$MANIFEST_PATH" 'aarch64-apple-darwin')
MACOS_ARM64_SHA256=$(manifest_artifact_value "$MANIFEST_PATH" 'aarch64-apple-darwin' 'sha256')
MACOS_AMD64_URL=$(manifest_release_download_url "$MANIFEST_PATH" 'x86_64-apple-darwin')
MACOS_AMD64_SHA256=$(manifest_artifact_value "$MANIFEST_PATH" 'x86_64-apple-darwin' 'sha256')
LINUX_ARM64_URL=$(manifest_release_download_url "$MANIFEST_PATH" 'aarch64-unknown-linux-gnu')
LINUX_ARM64_SHA256=$(manifest_artifact_value "$MANIFEST_PATH" 'aarch64-unknown-linux-gnu' 'sha256')
LINUX_AMD64_URL=$(manifest_release_download_url "$MANIFEST_PATH" 'x86_64-unknown-linux-gnu')
LINUX_AMD64_SHA256=$(manifest_artifact_value "$MANIFEST_PATH" 'x86_64-unknown-linux-gnu' 'sha256')

render_template \
  "$TEMPLATE_PATH" \
  "$OUTPUT_PATH" \
  VERSION "$VERSION" \
  LICENSE "$LICENSE" \
  MACOS_ARM64_URL "$MACOS_ARM64_URL" \
  MACOS_ARM64_SHA256 "$MACOS_ARM64_SHA256" \
  MACOS_AMD64_URL "$MACOS_AMD64_URL" \
  MACOS_AMD64_SHA256 "$MACOS_AMD64_SHA256" \
  LINUX_ARM64_URL "$LINUX_ARM64_URL" \
  LINUX_ARM64_SHA256 "$LINUX_ARM64_SHA256" \
  LINUX_AMD64_URL "$LINUX_AMD64_URL" \
  LINUX_AMD64_SHA256 "$LINUX_AMD64_SHA256"

log "rendered Homebrew formula to $(relative_path "$OUTPUT_PATH")"
