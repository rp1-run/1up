#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
# shellcheck source=./common.sh
source "$SCRIPT_DIR/common.sh"

TAG="${1:-${ONEUP_RELEASE_TAG:-}}"

if [[ -z "$TAG" ]]; then
  fail "usage: $(basename "$0") <tag>"
fi

VERSION=$(release_tag_to_version "$TAG")
CHANGELOG_PATH="$ROOT_DIR/CHANGELOG.md"

require_file "$CHANGELOG_PATH"

SECTION=$(read_versioned_changelog_section "$CHANGELOG_PATH" "$VERSION")
if [[ -z "${SECTION//[[:space:]]/}" ]]; then
  fail "CHANGELOG.md is missing release notes for ## [${VERSION}]"
fi

printf '## 1up %s\n\n%s\n' "$VERSION" "$SECTION"
