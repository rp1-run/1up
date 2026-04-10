#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
# shellcheck source=./common.sh
source "$SCRIPT_DIR/common.sh"

MANIFEST_PATH=""
OUTPUT_PATH=""
TEMPLATE_PATH="${ONEUP_SCOOP_TEMPLATE_PATH:-$ROOT_DIR/packaging/scoop/1up.json.tmpl}"
WINDOWS_TARGET='x86_64-pc-windows-msvc'

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
WINDOWS_URL=$(manifest_release_download_url "$MANIFEST_PATH" "$WINDOWS_TARGET")
WINDOWS_SHA256=$(manifest_artifact_value "$MANIFEST_PATH" "$WINDOWS_TARGET" 'sha256')
WINDOWS_ARCHIVE=$(manifest_artifact_value "$MANIFEST_PATH" "$WINDOWS_TARGET" 'archive')
WINDOWS_EXTRACT_DIR=${WINDOWS_ARCHIVE%.zip}

render_template \
  "$TEMPLATE_PATH" \
  "$OUTPUT_PATH" \
  VERSION "$VERSION" \
  LICENSE "$LICENSE" \
  WINDOWS_URL "$WINDOWS_URL" \
  WINDOWS_SHA256 "$WINDOWS_SHA256" \
  WINDOWS_EXTRACT_DIR "$WINDOWS_EXTRACT_DIR"

log "rendered Scoop manifest to $(relative_path "$OUTPUT_PATH")"
