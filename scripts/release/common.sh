#!/usr/bin/env bash

ROOT_DIR="${ONEUP_RELEASE_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)}"
EXPECTED_SPDX="Apache-2.0"
REPO_SLUG="${ONEUP_RELEASE_REPO_SLUG:-rp1-run/1up}"
HOMEBREW_TAP_REPO="${ONEUP_RELEASE_HOMEBREW_TAP_REPO:-rp1-run/homebrew-tap}"
HOMEBREW_FORMULA="${ONEUP_RELEASE_HOMEBREW_FORMULA:-brew install rp1-run/tap/1up}"
SCOOP_BUCKET_REPO="${ONEUP_RELEASE_SCOOP_BUCKET_REPO:-rp1-run/scoop-bucket}"
SCOOP_MANIFEST_URL="${ONEUP_RELEASE_SCOOP_MANIFEST_URL:-https://github.com/rp1-run/scoop-bucket/raw/main/bucket/1up.json}"

log() {
  printf '[release-assets] %s\n' "$*" >&2
}

fail() {
  log "$*"
  exit 1
}

require_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    fail "missing required command: $1"
  fi
}

relative_path() {
  local path="$1"

  if [[ "$path" == "$ROOT_DIR/"* ]]; then
    printf '%s\n' "${path#"$ROOT_DIR"/}"
    return
  fi

  printf '%s\n' "$path"
}

require_file() {
  local path="$1"

  if [[ ! -f "$path" ]]; then
    fail "missing required file: $(relative_path "$path")"
  fi
}

sha256_file() {
  local path="$1"

  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$path" | awk '{print $1}'
    return
  fi

  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$path" | awk '{print $1}'
    return
  fi

  fail "missing required command: sha256sum or shasum"
}

utc_timestamp() {
  date -u +"%Y-%m-%dT%H:%M:%SZ"
}

cargo_package_field() {
  local field="$1"

  awk -F'"' -v field="$field" '
    /^\[package\]$/ { in_package = 1; next }
    /^\[/ && in_package { exit }
    in_package && $0 ~ ("^" field "[[:space:]]*=") { print $2; exit }
  ' "$ROOT_DIR/Cargo.toml"
}

cargo_version() {
  cargo_package_field "version"
}

cargo_license() {
  cargo_package_field "license"
}

release_tag_to_version() {
  local tag="$1"

  if [[ ! "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    fail "release tag must have form vX.Y.Z, found ${tag}"
  fi

  printf '%s\n' "${tag#v}"
}

read_versioned_changelog_section() {
  local path="$1"
  local version="$2"

  awk -v version="$version" '
    $0 ~ "^## \\[" version "\\]" { in_section = 1; next }
    in_section && /^## / { exit }
    in_section { print }
  ' "$path"
}

target_os() {
  local target="$1"

  case "$target" in
    *-apple-darwin) printf 'macos\n' ;;
    *-unknown-linux-gnu) printf 'linux\n' ;;
    *-pc-windows-msvc) printf 'windows\n' ;;
    *) fail "unsupported release target: ${target}" ;;
  esac
}

target_arch() {
  local target="$1"

  case "$target" in
    aarch64-*) printf 'arm64\n' ;;
    x86_64-*) printf 'amd64\n' ;;
    *) fail "unsupported release architecture for target: ${target}" ;;
  esac
}

target_binary_name() {
  local target="$1"

  case "$(target_os "$target")" in
    windows) printf '1up.exe\n' ;;
    *) printf '1up\n' ;;
  esac
}

target_archive_extension() {
  local target="$1"

  case "$(target_os "$target")" in
    windows) printf 'zip\n' ;;
    *) printf 'tar.gz\n' ;;
  esac
}

target_install_hint() {
  local target="$1"
  local os
  local arch

  os=$(target_os "$target")
  arch=$(target_arch "$target")

  case "$os" in
    macos)
      printf 'Download the macOS %s archive from GitHub Releases and unpack with tar -xzf.\n' "$arch"
      ;;
    linux)
      printf 'Download the Linux %s archive from GitHub Releases and unpack with tar -xzf.\n' "$arch"
      ;;
    windows)
      printf 'Download the Windows %s archive from GitHub Releases and unpack with Expand-Archive.\n' "$arch"
      ;;
  esac
}

release_repo_url() {
  printf 'https://github.com/%s\n' "$REPO_SLUG"
}

native_path() {
  local path="$1"

  if command -v cygpath >/dev/null 2>&1; then
    cygpath -w "$path"
    return
  fi

  printf '%s\n' "$path"
}

manifest_value() {
  local manifest_path="$1"
  local filter="$2"

  jq -er "$filter" "$manifest_path"
}

manifest_artifact_value() {
  local manifest_path="$1"
  local target="$2"
  local field="$3"

  jq -er --arg target "$target" --arg field "$field" '
    .artifacts[]
    | select(.target == $target)
    | .[$field]
  ' "$manifest_path"
}

manifest_release_base_url() {
  local manifest_path="$1"
  local github_release_url
  local git_tag
  local suffix

  github_release_url=$(manifest_value "$manifest_path" '.channels.github_release')
  git_tag=$(manifest_value "$manifest_path" '.git_tag')
  suffix="/tag/${git_tag}"

  if [[ "$github_release_url" != *"$suffix" ]]; then
    fail "manifest github_release URL must end with ${suffix}"
  fi

  printf '%s\n' "${github_release_url%"$suffix"}"
}

manifest_release_download_url() {
  local manifest_path="$1"
  local target="$2"
  local archive
  local git_tag

  archive=$(manifest_artifact_value "$manifest_path" "$target" 'archive')
  git_tag=$(manifest_value "$manifest_path" '.git_tag')

  printf '%s/download/%s/%s\n' \
    "$(manifest_release_base_url "$manifest_path")" \
    "$git_tag" \
    "$archive"
}

escape_sed_replacement() {
  printf '%s' "$1" | sed -e 's/[&|]/\\&/g'
}

render_template() {
  local template_path="$1"
  local output_path="$2"

  shift 2

  if (( $# % 2 != 0 )); then
    fail "render_template requires placeholder/value pairs"
  fi

  require_file "$template_path"
  mkdir -p "$(dirname "$output_path")"

  local sed_args=()

  while [[ $# -gt 0 ]]; do
    local placeholder="$1"
    local value="$2"
    shift 2
    sed_args+=(-e "s|{{$placeholder}}|$(escape_sed_replacement "$value")|g")
  done

  sed "${sed_args[@]}" "$template_path" >"$output_path"
}
