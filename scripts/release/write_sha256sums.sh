#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
# shellcheck source=./common.sh
source "$SCRIPT_DIR/common.sh"

ASSETS_DIR=""
OUTPUT_PATH=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --assets-dir)
      ASSETS_DIR="${2:-}"
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

if [[ -z "$ASSETS_DIR" || -z "$OUTPUT_PATH" ]]; then
  fail "usage: $(basename "$0") --assets-dir <dir> --output <path>"
fi

if command -v sha256sum >/dev/null 2>&1; then
  HASH_COMMAND=(sha256sum)
elif command -v shasum >/dev/null 2>&1; then
  HASH_COMMAND=(shasum -a 256)
else
  fail "missing required command: sha256sum or shasum"
fi

mapfile -t ASSET_PATHS < <(find "$ASSETS_DIR" -maxdepth 1 -type f \( -name '*.tar.gz' -o -name '*.zip' \) | sort)

if [[ ${#ASSET_PATHS[@]} -eq 0 ]]; then
  fail "no release archives found in $(relative_path "$ASSETS_DIR")"
fi

mkdir -p "$(dirname "$OUTPUT_PATH")"
: >"$OUTPUT_PATH"

for asset_path in "${ASSET_PATHS[@]}"; do
  asset_name=$(basename "$asset_path")
  asset_hash=$("${HASH_COMMAND[@]}" "$asset_path" | awk '{print $1}')
  printf '%s  %s\n' "$asset_hash" "$asset_name" >>"$OUTPUT_PATH"
done

log "wrote checksums for ${#ASSET_PATHS[@]} archive(s) to $(relative_path "$OUTPUT_PATH")"
