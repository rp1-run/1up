#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
# shellcheck source=./common.sh
source "$SCRIPT_DIR/common.sh"

MANIFEST_PATH=""
ASSETS_DIR=""
CHECKSUMS_PATH=""
OUTPUT_PATH=""
TARGET_FILTERS=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --manifest)
      MANIFEST_PATH="${2:-}"
      shift 2
      ;;
    --assets-dir)
      ASSETS_DIR="${2:-}"
      shift 2
      ;;
    --checksums)
      CHECKSUMS_PATH="${2:-}"
      shift 2
      ;;
    --output)
      OUTPUT_PATH="${2:-}"
      shift 2
      ;;
    --target)
      TARGET_FILTERS+=("${2:-}")
      shift 2
      ;;
    *)
      fail "unknown argument: $1"
      ;;
  esac
done

if [[ -z "$MANIFEST_PATH" || -z "$ASSETS_DIR" || -z "$CHECKSUMS_PATH" || -z "$OUTPUT_PATH" ]]; then
  fail "usage: $(basename "$0") --manifest <path> --assets-dir <dir> --checksums <path> [--target <triple>]... --output <path>"
fi

require_cmd jq
require_cmd tar
require_file "$MANIFEST_PATH"
require_file "$CHECKSUMS_PATH"

artifact_count=$(manifest_value "$MANIFEST_PATH" '.artifacts | length')
if [[ "$artifact_count" -eq 0 ]]; then
  fail "release manifest does not contain any artifacts"
fi

TMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/oneup-release-verify.XXXXXX")
ARCHIVES_JSONL="$TMP_DIR/archives.jsonl"

cleanup() {
  rm -rf "$TMP_DIR"
}
trap cleanup EXIT

target_selected() {
  local candidate="$1"

  if [[ "${#TARGET_FILTERS[@]}" -eq 0 ]]; then
    return 0
  fi

  local selected
  for selected in "${TARGET_FILTERS[@]}"; do
    if [[ "$selected" == "$candidate" ]]; then
      return 0
    fi
  done

  return 1
}

extract_archive() {
  local archive_path="$1"
  local destination="$2"

  mkdir -p "$destination"

  case "$archive_path" in
    *.tar.gz)
      tar -xzf "$archive_path" -C "$destination"
      ;;
    *.zip)
      if command -v unzip >/dev/null 2>&1; then
        unzip -q "$archive_path" -d "$destination"
      elif command -v pwsh >/dev/null 2>&1; then
        pwsh -NoLogo -NoProfile -Command \
          "Expand-Archive -LiteralPath '$(native_path "$archive_path")' -DestinationPath '$(native_path "$destination")' -Force" >/dev/null
      else
        fail "missing required command: unzip or pwsh"
      fi
      ;;
    *)
      fail "unsupported archive format for ${archive_path}"
      ;;
  esac
}

manifest_version=$(manifest_value "$MANIFEST_PATH" '.version')
selected_count=0

while IFS=$'\t' read -r target archive expected_sha; do
  if ! target_selected "$target"; then
    continue
  fi

  selected_count=$((selected_count + 1))
  archive_path="$ASSETS_DIR/$archive"
  require_file "$archive_path"

  checksum_entry=$(awk -v asset="$archive" '$2 == asset { print $1; exit }' "$CHECKSUMS_PATH")
  if [[ -z "$checksum_entry" ]]; then
    fail "SHA256SUMS is missing an entry for ${archive}"
  fi

  if [[ "$checksum_entry" != "$expected_sha" ]]; then
    fail "release manifest checksum for ${archive} does not match SHA256SUMS"
  fi

  actual_sha=$(sha256_file "$archive_path")
  if [[ "$actual_sha" != "$expected_sha" ]]; then
    fail "archive checksum mismatch for ${archive}"
  fi

  package_dir="${archive%.tar.gz}"
  package_dir="${package_dir%.zip}"
  binary_path="${package_dir}/$(target_binary_name "$target")"
  license_path="${package_dir}/LICENSE"
  readme_path="${package_dir}/README.txt"
  extract_dir="$TMP_DIR/extracted/${target}"
  extract_archive "$archive_path" "$extract_dir"

  binary_fs_path="$extract_dir/$binary_path"
  license_fs_path="$extract_dir/$license_path"
  readme_fs_path="$extract_dir/$readme_path"
  require_file "$binary_fs_path"
  require_file "$license_fs_path"
  require_file "$readme_fs_path"

  smoke_command="./$(target_binary_name "$target") --version"
  smoke_output=$(
    cd "$extract_dir/$package_dir"
    "${smoke_command%% *}" --version
  )
  smoke_output=$(printf '%s' "$smoke_output" | tr -d '\r')

  if [[ "$smoke_output" != *"$manifest_version"* ]]; then
    fail "archive ${archive} smoke command did not report version ${manifest_version}"
  fi

  jq -n \
    --arg target "$target" \
    --arg archive "$archive" \
    --arg sha256 "$actual_sha" \
    --arg binary_path "$binary_path" \
    --arg license_path "$license_path" \
    --arg readme_path "$readme_path" \
    --arg smoke_command "$smoke_command" \
    --arg smoke_output "$smoke_output" \
    '{
      target: $target,
      archive: $archive,
      sha256: $sha256,
      verified_contents: {
        binary: $binary_path,
        license: $license_path,
        readme: $readme_path
      },
      smoke_test: {
        status: "passed",
        command: $smoke_command,
        output: $smoke_output
      }
    }' \
    >>"$ARCHIVES_JSONL"
done < <(jq -r '.artifacts[] | [.target, .archive, .sha256] | @tsv' "$MANIFEST_PATH")

if [[ "$selected_count" -eq 0 ]]; then
  fail "no release artifacts matched the requested target filters"
fi

jq -s 'sort_by(.target)' "$ARCHIVES_JSONL" >"$TMP_DIR/archives.json"

mkdir -p "$(dirname "$OUTPUT_PATH")"

jq -n \
  --arg generated_at "$(utc_timestamp)" \
  --arg manifest_asset "$(basename "$MANIFEST_PATH")" \
  --arg checksums_asset "$(basename "$CHECKSUMS_PATH")" \
  --slurpfile archives "$TMP_DIR/archives.json" \
  '{
    generated_at: $generated_at,
    manifest_asset: $manifest_asset,
    checksums_asset: $checksums_asset,
    archive_count: ($archives[0] | length),
    archives: $archives[0]
  }' \
  >"$OUTPUT_PATH"

log "verified $(jq -r '.archive_count' "$OUTPUT_PATH") archive(s) and wrote $(relative_path "$OUTPUT_PATH")"
