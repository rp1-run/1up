#!/usr/bin/env bash
# 1up install script. Safe under `curl | bash`.
#
# Detects platform, downloads the matching release archive from GitHub,
# verifies SHA256 when published, installs into $HOME/.1up/bin (or
# $1UP_INSTALL_DIR), and updates the user's shell rc with a PATH block.
#
# Env vars (names start with a digit, so set them via `env NAME=VALUE ...`
# or from a shell that accepts digit-leading identifiers):
#   1UP_VERSION       pin to a specific release tag (default: latest)
#   1UP_INSTALL_DIR   override install directory (default: $HOME/.1up/bin)
#   1UP_REPO          override GitHub repo slug (default: rp1-run/1up)
#
# bash 3.2 compatible. No $0-relative paths. All expansions quoted.

set -eu

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

log() {
    printf '%s\n' "$*"
}

warn() {
    printf '%s\n' "$*" >&2
}

fail() {
    warn "error: $*"
    exit 1
}

require_cmd() {
    if ! command -v "$1" >/dev/null 2>&1; then
        fail "missing required command: $1. Install $1 and retry."
    fi
}

# Read an env var whose name may begin with a digit (1UP_*). Portable across
# shells that forbid $1UP_VERSION-style expansion. Returns empty when unset.
read_env() {
    printenv "$1" 2>/dev/null || true
}

# ---------------------------------------------------------------------------
# Configuration (resolved once at start from env)
# ---------------------------------------------------------------------------

REPO=$(read_env 1UP_REPO)
if [ -z "$REPO" ]; then
    REPO="rp1-run/1up"
fi

VERSION_PIN=$(read_env 1UP_VERSION)
INSTALL_DIR_OVERRIDE=$(read_env 1UP_INSTALL_DIR)

# Populated by stages below.
HASH_CMD=""
TARGET=""
TAG=""
TMP=""
ARCHIVE=""
HAVE_SUMS=0
INSTALL_DIR=""

# ---------------------------------------------------------------------------
# Stage 1: preflight
# ---------------------------------------------------------------------------

preflight() {
    require_cmd curl
    require_cmd uname
    require_cmd mkdir
    require_cmd chmod
    require_cmd mktemp
    require_cmd tar
    require_cmd printenv
    require_cmd awk
    require_cmd sed
    require_cmd grep

    if command -v sha256sum >/dev/null 2>&1; then
        HASH_CMD="sha256sum"
    elif command -v shasum >/dev/null 2>&1; then
        HASH_CMD="shasum"
    else
        fail "missing required command: sha256sum or shasum. Install one and retry."
    fi

    if [ -z "${HOME:-}" ] || [ ! -d "$HOME" ]; then
        fail "HOME is not set or not a directory; cannot install to a user-local path."
    fi
}

# ---------------------------------------------------------------------------
# Stage 2: platform detection
# ---------------------------------------------------------------------------

detect_target() {
    local os arch os_label arch_label
    os=$(uname -s)
    arch=$(uname -m)

    case "$os" in
        Darwin) os_label="darwin" ;;
        Linux)  os_label="linux" ;;
        *)
            fail "unsupported platform: $os/$arch. See https://github.com/$REPO/releases for manual downloads."
            ;;
    esac

    case "$arch" in
        arm64|aarch64) arch_label="aarch64" ;;
        x86_64|amd64)  arch_label="x86_64" ;;
        *)
            fail "unsupported platform: $os/$arch. See https://github.com/$REPO/releases for manual downloads."
            ;;
    esac

    case "${os_label}-${arch_label}" in
        darwin-aarch64) TARGET="aarch64-apple-darwin" ;;
        darwin-x86_64)  TARGET="x86_64-apple-darwin" ;;
        linux-aarch64)  TARGET="aarch64-unknown-linux-gnu" ;;
        linux-x86_64)   TARGET="x86_64-unknown-linux-gnu" ;;
        *)
            fail "unsupported platform: $os/$arch. See https://github.com/$REPO/releases for manual downloads."
            ;;
    esac
}

# ---------------------------------------------------------------------------
# Stage 3: resolve release tag
# ---------------------------------------------------------------------------

resolve_tag() {
    if [ -n "$VERSION_PIN" ]; then
        case "$VERSION_PIN" in
            v*) TAG="$VERSION_PIN" ;;
            *)  TAG="v$VERSION_PIN" ;;
        esac
        return
    fi

    local api_url response
    api_url="https://api.github.com/repos/$REPO/releases/latest"
    if ! response=$(curl -fsSL "$api_url" 2>&1); then
        fail "failed to resolve latest release from $api_url: $response"
    fi

    # Extract "tag_name": "vX.Y.Z" without jq.
    TAG=$(printf '%s\n' "$response" \
        | grep '"tag_name"' \
        | head -n 1 \
        | sed -e 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/')

    if [ -z "$TAG" ]; then
        fail "failed to parse release tag from $api_url response."
    fi
}

# ---------------------------------------------------------------------------
# Stage 4: download artifacts
# ---------------------------------------------------------------------------

download_artifacts() {
    TMP=$(mktemp -d "${TMPDIR:-/tmp}/1up-install.XXXXXX")
    # shellcheck disable=SC2064
    trap "rm -rf \"$TMP\"" EXIT

    ARCHIVE="1up-${TAG}-${TARGET}.tar.gz"
    local archive_url sums_url
    archive_url="https://github.com/$REPO/releases/download/$TAG/$ARCHIVE"
    sums_url="https://github.com/$REPO/releases/download/$TAG/SHA256SUMS"

    log "downloading $ARCHIVE"
    if ! curl -fsSL "$archive_url" -o "$TMP/$ARCHIVE"; then
        fail "release asset not found: $ARCHIVE for tag $TAG (from $archive_url)."
    fi

    HAVE_SUMS=0
    if curl -fsSL "$sums_url" -o "$TMP/SHA256SUMS" 2>/dev/null; then
        HAVE_SUMS=1
    fi
}

# ---------------------------------------------------------------------------
# Stage 5: verify checksum
# ---------------------------------------------------------------------------

verify_checksum() {
    if [ "$HAVE_SUMS" -eq 0 ]; then
        warn "warning: SHA256SUMS not published for $TAG; integrity not verified."
        return
    fi

    local expected actual sums_line
    expected=""
    while IFS= read -r sums_line; do
        case "$sums_line" in
            *"  $ARCHIVE")
                expected=$(printf '%s' "$sums_line" | awk '{print $1}')
                break
                ;;
        esac
    done <"$TMP/SHA256SUMS"

    if [ -z "$expected" ]; then
        fail "checksum entry missing for $ARCHIVE in SHA256SUMS."
    fi

    if [ "$HASH_CMD" = "sha256sum" ]; then
        actual=$(cd "$TMP" && sha256sum "$ARCHIVE" | awk '{print $1}')
    else
        actual=$(cd "$TMP" && shasum -a 256 "$ARCHIVE" | awk '{print $1}')
    fi

    if [ "$expected" != "$actual" ]; then
        fail "checksum mismatch for $ARCHIVE: expected $expected, got $actual."
    fi
    log "verified sha256 for $ARCHIVE"
}

# ---------------------------------------------------------------------------
# Stage 6: install binary
# ---------------------------------------------------------------------------

install_binary() {
    if [ -n "$INSTALL_DIR_OVERRIDE" ]; then
        INSTALL_DIR="$INSTALL_DIR_OVERRIDE"
    else
        INSTALL_DIR="$HOME/.1up/bin"
    fi

    if ! mkdir -p "$INSTALL_DIR"; then
        fail "cannot write to install directory: $INSTALL_DIR"
    fi

    if ! tar -xzf "$TMP/$ARCHIVE" -C "$TMP"; then
        fail "failed to extract archive $ARCHIVE"
    fi

    local package_dir staged_binary stage_target
    package_dir="1up-${TAG}-${TARGET}"
    staged_binary="$TMP/$package_dir/1up"

    if [ ! -f "$staged_binary" ]; then
        fail "archive did not contain expected binary: $package_dir/1up"
    fi

    chmod 0755 "$staged_binary"

    # Copy into the install dir under a sibling temp name, then rename.
    # Same-filesystem rename is atomic; the target path is only touched by
    # the final mv, so a failure before mv leaves any prior binary intact.
    stage_target="$INSTALL_DIR/.1up.tmp.$$"
    if ! cp -f "$staged_binary" "$stage_target"; then
        rm -f "$stage_target"
        fail "cannot write to install directory: $INSTALL_DIR"
    fi
    if ! mv -f "$stage_target" "$INSTALL_DIR/1up"; then
        rm -f "$stage_target"
        fail "failed to install binary to $INSTALL_DIR/1up"
    fi

    log "installed 1up $TAG to $INSTALL_DIR/1up"
}

# ---------------------------------------------------------------------------
# Stage 7: configure PATH
# ---------------------------------------------------------------------------

configure_path() {
    local shell_name rc_path
    shell_name=""
    if [ -n "${SHELL:-}" ]; then
        shell_name=$(basename "$SHELL")
    fi

    case "$shell_name" in
        zsh) rc_path="$HOME/.zshrc" ;;
        *)   rc_path="$HOME/.bashrc" ;;
    esac

    # Already on PATH?
    case ":${PATH:-}:" in
        *":$INSTALL_DIR:"*)
            log "PATH already includes $INSTALL_DIR; no changes to $rc_path."
            return
            ;;
    esac

    # Already contains our managed block?
    if [ -f "$rc_path" ] && grep -q '^# >>> 1up install (managed) >>>$' "$rc_path" 2>/dev/null; then
        # Extract the install dir recorded in the existing block so we can
        # detect a rerun that points at a different directory. The block
        # body is a single `export PATH="<dir>:$PATH"` line.
        local old_dir
        old_dir=$(awk '
            /^# >>> 1up install \(managed\) >>>$/ { in_block = 1; next }
            /^# <<< 1up install \(managed\) <<<$/ { in_block = 0; next }
            in_block && /^export PATH=/ {
                line = $0
                sub(/^export PATH="/, "", line)
                sub(/:\$PATH"$/, "", line)
                print line
                exit
            }
        ' "$rc_path")

        if [ "$old_dir" = "$INSTALL_DIR" ]; then
            log "PATH block already present in $rc_path; no changes."
            return
        fi

        # Rerun with a different install dir: replace the block in place.
        # Write everything outside the managed block to a temp file, then
        # append the refreshed block. bash 3.2 compatible.
        local tmp_rc
        tmp_rc="${rc_path}.1up.tmp.$$"
        awk '
            /^# >>> 1up install \(managed\) >>>$/ { in_block = 1; next }
            /^# <<< 1up install \(managed\) <<<$/ { in_block = 0; next }
            !in_block { print }
        ' "$rc_path" >"$tmp_rc"

        {
            printf '\n# >>> 1up install (managed) >>>\n'
            # shellcheck disable=SC2016  # literal $PATH is intentional; expanded at rc load time.
            printf 'export PATH="%s:$PATH"\n' "$INSTALL_DIR"
            printf '# <<< 1up install (managed) <<<\n'
        } >>"$tmp_rc"

        if ! mv -f "$tmp_rc" "$rc_path"; then
            rm -f "$tmp_rc"
            fail "failed to replace PATH block in $rc_path"
        fi

        log "Replaced PATH block in $rc_path (was $old_dir -> $INSTALL_DIR)."
        log "Run \`source $rc_path\` or open a new shell to put 1up on PATH for this session."
        return
    fi

    {
        printf '\n# >>> 1up install (managed) >>>\n'
        # shellcheck disable=SC2016  # literal $PATH is intentional; expanded at rc load time.
        printf 'export PATH="%s:$PATH"\n' "$INSTALL_DIR"
        printf '# <<< 1up install (managed) <<<\n'
    } >>"$rc_path"

    log "Updated $rc_path. Run \`source $rc_path\` or open a new shell to put 1up on PATH for this session."
}

# ---------------------------------------------------------------------------
# Stage 8: next-step message
# ---------------------------------------------------------------------------

print_next_steps() {
    printf 'Installed 1up %s to %s.\n' "$TAG" "$INSTALL_DIR"
    printf 'Run: 1up start\n'
}

# ---------------------------------------------------------------------------
# Entry (flat, no main dispatch -- safe under `curl | bash`)
# ---------------------------------------------------------------------------

preflight
detect_target
resolve_tag
download_artifacts
verify_checksum
install_binary
configure_path
print_next_steps
