#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
# shellcheck source=./common.sh
source "$SCRIPT_DIR/common.sh"

INPUT_PATH=""
OUTPUT_PATH=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --input)
      INPUT_PATH="${2:-}"
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

if [[ -z "$INPUT_PATH" || -z "$OUTPUT_PATH" ]]; then
  fail "usage: $(basename "$0") --input <release-manifest.json> --output <update-manifest.json>"
fi

require_cmd jq
require_file "$INPUT_PATH"

mkdir -p "$(dirname "$OUTPUT_PATH")"

# Client-facing projection of the release manifest: drops release-generation-only
# fields (commit_sha, binary_name, license, checksums_file, notes_source) that
# update clients never consume. This is the sole place the projection is
# computed -- release-assets.yml runs it once and publish-update-manifest.yml
# propagates the resulting bytes verbatim, so the attested asset and the
# committed main copy are byte-identical.
jq '{
  version,
  git_tag,
  published_at,
  expiry,
  notes_url,
  artifacts: [.artifacts[] | {target, archive, sha256, url}],
  channels: {
    github_release: .channels.github_release,
    script_install: .channels.script_install,
    update_manifest: .channels.update_manifest
  },
  yanked,
  minimum_safe_version,
  message
}' "$INPUT_PATH" >"$OUTPUT_PATH"

log "wrote update manifest to $(relative_path "$OUTPUT_PATH")"
