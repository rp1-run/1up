#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="${ONEUP_LICENSE_CHECK_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)}"
EXPECTED_SPDX="Apache-2.0"
FAILURES=0

log() {
  printf '[release-license-check] %s\n' "$*" >&2
}

fail() {
  log "$*"
  FAILURES=$((FAILURES + 1))
}

relative_path() {
  local path="$1"
  printf '%s\n' "${path#"$ROOT_DIR"/}"
}

require_file() {
  local path="$1"

  if [[ ! -f "$path" ]]; then
    fail "missing required file: $(relative_path "$path")"
    return 1
  fi
}

read_markdown_section() {
  local path="$1"
  local heading="$2"

  awk -v heading="$heading" '
    $0 == heading { in_section = 1; next }
    in_section && /^## / { exit }
    in_section { print }
  ' "$path"
}

CARGO_TOML="$ROOT_DIR/Cargo.toml"
README_PATH="$ROOT_DIR/README.md"
SKILL_PATH="$ROOT_DIR/skills/1up-search/SKILL.md"
LICENSE_PATH="$ROOT_DIR/LICENSE"

for path in "$CARGO_TOML" "$README_PATH" "$SKILL_PATH" "$LICENSE_PATH"; do
  require_file "$path"
done

cargo_license=$(awk -F'"' '/^license[[:space:]]*=/ { print $2; exit }' "$CARGO_TOML")
if [[ -z "$cargo_license" ]]; then
  fail "Cargo.toml is missing a package license field"
elif [[ "$cargo_license" != "$EXPECTED_SPDX" ]]; then
  fail "Cargo.toml license must be ${EXPECTED_SPDX}, found ${cargo_license}"
fi

skill_license=$(awk -F': *' '/^license:/ { print $2; exit }' "$SKILL_PATH")
if [[ -z "$skill_license" ]]; then
  fail "skills/1up-search/SKILL.md is missing a license field"
elif [[ "$skill_license" != "$EXPECTED_SPDX" ]]; then
  fail "skills/1up-search/SKILL.md license must be ${EXPECTED_SPDX}, found ${skill_license}"
fi

readme_license_section=$(read_markdown_section "$README_PATH" "## License")
if [[ -z "$readme_license_section" ]]; then
  fail "README.md is missing a License section"
else
  if [[ "$readme_license_section" != *"Apache 2.0"* ]]; then
    fail "README.md license section must mention Apache 2.0"
  fi
  if [[ "$readme_license_section" == *"MIT"* ]]; then
    fail "README.md license section still mentions MIT"
  fi
fi

if ! grep -Fq "Apache License" "$LICENSE_PATH"; then
  fail "LICENSE must contain the Apache License text"
fi
if ! grep -Fq "Version 2.0, January 2004" "$LICENSE_PATH"; then
  fail "LICENSE must contain the Apache 2.0 heading"
fi
if grep -Fq "MIT License" "$LICENSE_PATH"; then
  fail "LICENSE unexpectedly contains MIT text"
fi

if (( FAILURES > 0 )); then
  log "license consistency check failed with ${FAILURES} issue(s)"
  exit 1
fi

log "validated ${EXPECTED_SPDX} across Cargo.toml, README.md, skills/1up-search/SKILL.md, and LICENSE"
