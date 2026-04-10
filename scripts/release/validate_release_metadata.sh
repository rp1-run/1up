#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
# shellcheck source=./common.sh
source "$SCRIPT_DIR/common.sh"

TAG="${1:-${ONEUP_RELEASE_TAG:-}}"

if [[ -z "$TAG" ]]; then
  fail "usage: $(basename "$0") <tag>"
fi

require_file "$ROOT_DIR/Cargo.toml"
require_file "$ROOT_DIR/CHANGELOG.md"

VERSION=$(release_tag_to_version "$TAG")
CARGO_VERSION=$(cargo_version)

if [[ -z "$CARGO_VERSION" ]]; then
  fail "Cargo.toml is missing package.version"
fi

if [[ "$CARGO_VERSION" != "$VERSION" ]]; then
  fail "Cargo.toml version ${CARGO_VERSION} does not match release tag ${TAG}"
fi

CHANGELOG_SECTION=$(read_versioned_changelog_section "$ROOT_DIR/CHANGELOG.md" "$VERSION")
if [[ -z "${CHANGELOG_SECTION//[[:space:]]/}" ]]; then
  fail "CHANGELOG.md is missing a non-empty section for ## [${VERSION}]"
fi

ONEUP_LICENSE_CHECK_ROOT="$ROOT_DIR" bash "$SCRIPT_DIR/check_license_consistency.sh"

log "validated tag ${TAG}, Cargo.toml version ${CARGO_VERSION}, changelog section, and release license surfaces"
