#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
# shellcheck source=./common.sh
source "$SCRIPT_DIR/common.sh"

TAG=""
ASSETS_DIR=""
CHECKSUMS_PATH=""
OUTPUT_PATH=""
COMMIT_SHA="${ONEUP_RELEASE_COMMIT_SHA:-}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --tag)
      TAG="${2:-}"
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
    --commit-sha)
      COMMIT_SHA="${2:-}"
      shift 2
      ;;
    *)
      fail "unknown argument: $1"
      ;;
  esac
done

if [[ -z "$TAG" || -z "$ASSETS_DIR" || -z "$CHECKSUMS_PATH" || -z "$OUTPUT_PATH" ]]; then
  fail "usage: $(basename "$0") --tag <tag> --assets-dir <dir> --checksums <path> --output <path> [--commit-sha <sha>]"
fi

require_cmd jq
require_file "$ROOT_DIR/Cargo.toml"
require_file "$CHECKSUMS_PATH"

VERSION=$(release_tag_to_version "$TAG")
CARGO_VERSION=$(cargo_version)
LICENSE=$(cargo_license)

if [[ "$CARGO_VERSION" != "$VERSION" ]]; then
  fail "Cargo.toml version ${CARGO_VERSION} does not match release tag ${TAG}"
fi

if [[ "$LICENSE" != "$EXPECTED_SPDX" ]]; then
  fail "Cargo.toml license must be ${EXPECTED_SPDX}, found ${LICENSE}"
fi

if [[ -z "$COMMIT_SHA" ]]; then
  COMMIT_SHA=$(git -C "$ROOT_DIR" rev-parse HEAD 2>/dev/null || true)
fi

if [[ -z "$COMMIT_SHA" ]]; then
  fail "unable to determine commit sha for release manifest"
fi

mapfile -t METADATA_FILES < <(find "$ASSETS_DIR" -maxdepth 1 -type f -name '*.metadata.json' | sort)

if [[ ${#METADATA_FILES[@]} -eq 0 ]]; then
  fail "no release metadata files found in $(relative_path "$ASSETS_DIR")"
fi

TMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/oneup-release-manifest.XXXXXX")

cleanup() {
  rm -rf "$TMP_DIR"
}
trap cleanup EXIT

ARTIFACTS_JSONL="$TMP_DIR/artifacts.jsonl"

for metadata_path in "${METADATA_FILES[@]}"; do
  archive_name=$(jq -r '.archive' "$metadata_path")

  if [[ -z "$archive_name" || "$archive_name" == "null" ]]; then
    fail "metadata file $(relative_path "$metadata_path") is missing an archive field"
  fi

  checksum=$(awk -v asset="$archive_name" '$2 == asset { print $1; exit }' "$CHECKSUMS_PATH")
  if [[ -z "$checksum" ]]; then
    fail "SHA256SUMS is missing an entry for ${archive_name}"
  fi

  jq --arg sha256 "$checksum" '. + {sha256: $sha256}' "$metadata_path" >>"$ARTIFACTS_JSONL"
done

jq -s 'sort_by(.target)' "$ARTIFACTS_JSONL" >"$TMP_DIR/artifacts.json"

mkdir -p "$(dirname "$OUTPUT_PATH")"

jq -n \
  --arg version "$VERSION" \
  --arg git_tag "$TAG" \
  --arg commit_sha "$COMMIT_SHA" \
  --arg binary_name "1up" \
  --arg license "$LICENSE" \
  --arg checksums_file "$(basename "$CHECKSUMS_PATH")" \
  --arg notes_source "CHANGELOG.md#[${VERSION}]" \
  --arg github_release "$(release_repo_url)/releases/tag/${TAG}" \
  --arg homebrew_tap "$HOMEBREW_TAP_REPO" \
  --arg homebrew_formula "$HOMEBREW_FORMULA" \
  --arg scoop_bucket "$SCOOP_BUCKET_REPO" \
  --arg scoop_manifest "$SCOOP_MANIFEST_URL" \
  --slurpfile artifacts "$TMP_DIR/artifacts.json" \
  '{
    version: $version,
    git_tag: $git_tag,
    commit_sha: $commit_sha,
    binary_name: $binary_name,
    license: $license,
    artifacts: $artifacts[0],
    checksums_file: $checksums_file,
    notes_source: $notes_source,
    channels: {
      github_release: $github_release,
      homebrew_tap: $homebrew_tap,
      homebrew_formula: $homebrew_formula,
      scoop_bucket: $scoop_bucket,
      scoop_manifest: $scoop_manifest
    }
  }' \
  >"$OUTPUT_PATH"

log "wrote release manifest to $(relative_path "$OUTPUT_PATH")"
