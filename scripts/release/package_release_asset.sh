#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
# shellcheck source=./common.sh
source "$SCRIPT_DIR/common.sh"

TARGET=""
BINARY_PATH=""
OUTPUT_DIR=""
VERSION=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --target)
      TARGET="${2:-}"
      shift 2
      ;;
    --binary)
      BINARY_PATH="${2:-}"
      shift 2
      ;;
    --output-dir)
      OUTPUT_DIR="${2:-}"
      shift 2
      ;;
    --version)
      VERSION="${2:-}"
      shift 2
      ;;
    *)
      fail "unknown argument: $1"
      ;;
  esac
done

if [[ -z "$TARGET" || -z "$BINARY_PATH" || -z "$OUTPUT_DIR" ]]; then
  fail "usage: $(basename "$0") --target <triple> --binary <path> --output-dir <dir> [--version <semver>]"
fi

if [[ -z "$VERSION" ]]; then
  VERSION=$(cargo_version)
fi

if [[ -z "$VERSION" ]]; then
  fail "unable to determine release version from Cargo.toml"
fi

require_file "$ROOT_DIR/LICENSE"
require_file "$BINARY_PATH"
require_cmd jq

ARCHIVE_EXT=$(target_archive_extension "$TARGET")
ARCHIVE_NAME="1up-v${VERSION}-${TARGET}.${ARCHIVE_EXT}"
ARCHIVE_PATH="$OUTPUT_DIR/$ARCHIVE_NAME"
METADATA_PATH="$OUTPUT_DIR/$ARCHIVE_NAME.metadata.json"
PACKAGE_DIR_NAME="1up-v${VERSION}-${TARGET}"
STAGE_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/oneup-release-asset.XXXXXX")
STAGE_DIR="$STAGE_ROOT/$PACKAGE_DIR_NAME"
TARGET_OS=$(target_os "$TARGET")
TARGET_ARCH=$(target_arch "$TARGET")
TARGET_BINARY=$(target_binary_name "$TARGET")
RELEASE_URL="$(release_repo_url)/releases/tag/v${VERSION}"
README_PATH="$STAGE_DIR/README.txt"

cleanup() {
  rm -rf "$STAGE_ROOT"
}
trap cleanup EXIT

mkdir -p "$STAGE_DIR" "$OUTPUT_DIR"
cp "$BINARY_PATH" "$STAGE_DIR/$TARGET_BINARY"
if [[ "$TARGET_OS" == "windows" ]]; then
  binary_dir=$(dirname "$BINARY_PATH")
  shopt -s nullglob
  for dll_path in "$binary_dir"/*.dll; do
    cp "$dll_path" "$STAGE_DIR/$(basename "$dll_path")"
  done
  shopt -u nullglob
fi
cp "$ROOT_DIR/LICENSE" "$STAGE_DIR/LICENSE"

cat >"$README_PATH" <<EOF
1up ${VERSION}
Target: ${TARGET}

Repository docs:
$(release_repo_url)#readme

Release page:
${RELEASE_URL}
EOF

case "$ARCHIVE_EXT" in
  tar.gz)
    require_cmd tar
    tar -C "$STAGE_ROOT" -czf "$ARCHIVE_PATH" "$PACKAGE_DIR_NAME"
    ;;
  zip)
    require_cmd pwsh
    POWERSHELL_STAGE_ROOT=$(native_path "$STAGE_ROOT")
    POWERSHELL_ARCHIVE_PATH=$(native_path "$ARCHIVE_PATH")
    pwsh -NoLogo -NoProfile -Command "Set-Location -LiteralPath '$POWERSHELL_STAGE_ROOT'; Compress-Archive -Path '$PACKAGE_DIR_NAME' -DestinationPath '$POWERSHELL_ARCHIVE_PATH' -Force" >/dev/null
    ;;
esac

jq -n \
  --arg target "$TARGET" \
  --arg os "$TARGET_OS" \
  --arg arch "$TARGET_ARCH" \
  --arg archive "$ARCHIVE_NAME" \
  --arg install_hint "$(target_install_hint "$TARGET")" \
  '{
    target: $target,
    os: $os,
    arch: $arch,
    archive: $archive,
    install_hint: $install_hint
  }' \
  >"$METADATA_PATH"

log "packaged ${ARCHIVE_NAME}"
printf '%s\n' "$ARCHIVE_PATH"
