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
