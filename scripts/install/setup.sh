#!/usr/bin/env bash
# 1up install script. Safe under `curl | bash`.
#
# Detects platform, downloads the matching release archive from GitHub,
# verifies SHA256 when published, installs into $HOME/.local/bin (or
# $1UP_INSTALL_DIR), and updates the user's shell rc with a PATH block.
#
# Env vars (names start with a digit, so set them via `env NAME=VALUE ...`
# or from a shell that accepts digit-leading identifiers):
#   1UP_VERSION       pin to a specific release tag (default: latest)
#   1UP_INSTALL_DIR   override install directory (default: $HOME/.local/bin)
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

# The SHA256SUMS fetch is classified into published / unpublished / transient.
# A transient transport failure (connect/timeout/DNS/TLS) or a 5xx is retried
# up to this many times before the install fails closed. This is what keeps a
# network blip from being misread as "checksum genuinely unpublished" and
# silently skipping the mandatory integrity check.
SUMS_FETCH_ATTEMPTS=3
SUMS_FETCH_RETRY_DELAY=1

# OIDC issuer pinned for release attestation verification. Every legitimate
# 1up release attestation is a keyless GitHub Actions OIDC provenance, so the
# verifier requires the signer certificate to carry this issuer. Paired with a
# per-repo workflow-identity regexp (see attestation_identity_regexp), it is
# what makes a substituted artifact's attestation non-attributable and thus
# rejected.
ATTESTATION_OIDC_ISSUER="https://token.actions.githubusercontent.com"

# Populated by stages below.
HASH_CMD=""
TARGET=""
TAG=""
TMP=""
ARCHIVE=""
PACKAGE_DIR_NAME=""
# Tri-state checksum signal: "published" (must verify), "unpublished" (warn and
# continue), or "transient" (retry, then fail). Empty until classified.
SUMS_STATE=""
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
        darwin-x86_64)
            # Intel macOS is not in the published release matrix; the script
            # only advertises platforms that have a matching artifact so users
            # do not hit an opaque 404 on archive download.
            fail "unsupported platform: macOS Intel (x86_64) is not currently published. The install script supports macOS on Apple Silicon. See https://github.com/$REPO/releases for available downloads."
            ;;
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

    # The release workflow accepts both `vX.Y.Z` and legacy `oneup-vX.Y.Z`
    # tag forms but always publishes assets with the version-only prefix
    # (`1up-vX.Y.Z-...`). Strip the optional `oneup-` so we ask for the asset
    # name actually published; the URL path keeps the raw $TAG.
    local asset_tag
    asset_tag="$TAG"
    case "$asset_tag" in
        oneup-v*) asset_tag="${asset_tag#oneup-}" ;;
    esac
    ARCHIVE="1up-${asset_tag}-${TARGET}.tar.gz"
    PACKAGE_DIR_NAME="1up-${asset_tag}-${TARGET}"
    local archive_url sums_url
    archive_url="https://github.com/$REPO/releases/download/$TAG/$ARCHIVE"
    sums_url="https://github.com/$REPO/releases/download/$TAG/SHA256SUMS"

    log "downloading $ARCHIVE"
    if ! curl -fsSL "$archive_url" -o "$TMP/$ARCHIVE"; then
        fail "release asset not found: $ARCHIVE for tag $TAG (from $archive_url)."
    fi

    # Classify the SHA256SUMS fetch into published / unpublished / transient,
    # retrying only while the result is transient. A transient transport failure
    # or 5xx must never be mistaken for "genuinely unpublished" -- that would
    # silently skip the now-mandatory checksum. verify_checksum enforces the
    # policy; this loop only resolves the signal.
    local attempt
    attempt=1
    while :; do
        SUMS_STATE=$(classify_sums_fetch "$sums_url")
        if [ "$SUMS_STATE" != "transient" ]; then
            break
        fi
        if [ "$attempt" -ge "$SUMS_FETCH_ATTEMPTS" ]; then
            break
        fi
        warn "warning: could not fetch SHA256SUMS for $TAG (attempt $attempt/$SUMS_FETCH_ATTEMPTS); retrying in ${SUMS_FETCH_RETRY_DELAY}s..."
        attempt=$((attempt + 1))
        sleep "$SUMS_FETCH_RETRY_DELAY"
    done
}

# Fetch SHA256SUMS once and classify the outcome. Writes the sums body to
# $TMP/SHA256SUMS on HTTP 200, and echoes exactly one of:
#   published   -- HTTP 200; the checksum exists and verification is mandatory
#   unpublished -- HTTP 404; the project ships no checksum for this tag
#   transient   -- connect/timeout/DNS/TLS failure or a non-200/404 status
#                  (e.g. 5xx, 403); the signal is unreliable, do not downgrade
#
# Deliberately omits curl's -f: with -f every HTTP >= 400 collapses to exit 22,
# which cannot tell a definitive 404 from a flaky 5xx. Reading %{http_code}
# instead lets a real 404 mean "unpublished" while a 5xx stays "transient".
classify_sums_fetch() {
    local url http_code curl_exit
    url="$1"
    curl_exit=0
    http_code=$(curl -sSL -o "$TMP/SHA256SUMS" -w '%{http_code}' "$url" 2>/dev/null) || curl_exit=$?

    if [ "$curl_exit" -ne 0 ]; then
        # No usable HTTP response (curl prints 000 for %{http_code} here).
        printf '%s\n' "transient"
        return
    fi

    case "$http_code" in
        200) printf '%s\n' "published" ;;
        404) printf '%s\n' "unpublished" ;;
        *)   printf '%s\n' "transient" ;;
    esac
}

# ---------------------------------------------------------------------------
# Stage 5: verify checksum
# ---------------------------------------------------------------------------

verify_checksum() {
    case "$SUMS_STATE" in
        unpublished)
            # Genuinely no checksum for this tag: preserve warn-and-continue so
            # releases that legitimately ship without SHA256SUMS still install.
            warn "warning: SHA256SUMS not published for $TAG; integrity not verified."
            return
            ;;
        transient)
            # The checksum may exist but we could not obtain it. Fail closed
            # rather than installing unverified bytes; do NOT downgrade to the
            # unpublished warn-and-continue path.
            fail "could not obtain SHA256SUMS for $TAG after $SUMS_FETCH_ATTEMPTS attempts; refusing to install without integrity verification. Check connectivity and retry, or pin a known-good release with 1UP_VERSION."
            ;;
        published)
            : # checksum is published; verification below is mandatory.
            ;;
        *)
            fail "internal error: unclassified checksum state '$SUMS_STATE'."
            ;;
    esac

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
# Stage 5b: verify attestation (opt-in, independent trust channel)
# ---------------------------------------------------------------------------
#
# An independent-channel check layered on top of (never replacing) the SHA256
# checksum floor. The checksum only proves the archive matches a value served
# from the same host; a keyless-OIDC GitHub artifact attestation proves the
# archive was built by this repo's own release workflow, defeating the
# substituted-artifact / compromised-host threat.
#
# This is deliberately OPT-IN and best-effort so `curl | bash` keeps working on
# a bare machine: it runs only when a verifier (gh or cosign) is available and
# never hard-fails for a missing or unable verifier. Three outcomes:
#   verified   -> proceed silently
#   cannot-run -> degrade to the checksum floor with a notice (verifier absent,
#                 offline, rate-limited, no attestation found, or the gh
#                 multi-entry/offline bug cli/cli#10059)
#   disproved  -> fail(): an attestation was retrieved but rejected by policy
#                 (foreign identity / bad signature) -- a real tamper.
# Only a positive disproof is fatal; everything else degrades, because breaking
# a legitimate install on a verifier quirk is worse than relying on the
# checksum floor (the in-binary self-update enforces attestation more strictly).

# Build the certificate-identity regexp that pins the signer to this repo's
# release workflow for ANY git ref. The attested SAN is
#   https://github.com/<REPO>/.github/workflows/release-assets.yml@<ref>
# and <ref> is a per-release tag, so we anchor a prefix rather than an exact
# identity. Only '.' is a realistic regexp metacharacter in a GitHub owner/repo
# slug (alnum, '-', '_', '.'), so escaping dots is sufficient.
attestation_identity_regexp() {
    local repo_escaped
    repo_escaped=$(printf '%s' "$REPO" | sed 's/\./\\./g')
    printf '^https://github\.com/%s/\.github/workflows/release-assets\.yml@' "$repo_escaped"
}

# Echo the preferred available+usable verifier ("cosign" or "gh"), or empty if
# none. Each probe runs the tool locally (no network) so a too-old or stubbed
# tool is correctly seen as unusable. cosign is preferred for its unambiguous
# per-artifact verify (sidesteps cli/cli#10059), but this repo publishes no
# sidecar bundle -- the attestation lives in the GitHub API keyed by digest --
# so cosign needs gh to fetch the bundle. cosign is therefore only selectable
# when gh is also usable; gh alone can verify directly by digest.
detect_attestation_verifier() {
    local have_gh have_cosign
    have_gh=no
    have_cosign=no

    if command -v gh >/dev/null 2>&1 && gh attestation --help >/dev/null 2>&1; then
        have_gh=yes
    fi
    if command -v cosign >/dev/null 2>&1 && cosign version >/dev/null 2>&1; then
        have_cosign=yes
    fi

    if [ "$have_cosign" = yes ] && [ "$have_gh" = yes ]; then
        printf '%s\n' "cosign"
    elif [ "$have_gh" = yes ]; then
        printf '%s\n' "gh"
    else
        printf '%s\n' ""
    fi
}

# Run the selected verifier over the downloaded archive, capturing its combined
# output to $TMP/attest.log. Echoes nothing; returns the verifier's exit code
# (which classify_attestation_result interprets alongside the log). Never aborts
# under `set -e`: the verifier's non-zero exit is captured, not propagated.
run_attestation_verifier() {
    local verifier archive identity_re rc bundle
    verifier="$1"
    archive="$TMP/$ARCHIVE"
    identity_re=$(attestation_identity_regexp)

    if [ "$verifier" = "gh" ]; then
        rc=0
        gh attestation verify "$archive" \
            --repo "$REPO" \
            --cert-identity-regexp "$identity_re" \
            --cert-oidc-issuer "$ATTESTATION_OIDC_ISSUER" \
            >"$TMP/attest.log" 2>&1 || rc=$?
        return "$rc"
    fi

    # cosign path: no sidecar bundle is published, so fetch the by-digest
    # bundle via gh, then verify exactly this one artifact with cosign. Any
    # fetch/locate problem leaves a marker in the log and returns non-zero;
    # the caller classifies that as cannot-run (and falls back to gh verify).
    rc=0
    ( cd "$TMP" && gh attestation download "$archive" --repo "$REPO" ) >"$TMP/attest.log" 2>&1 || rc=$?
    if [ "$rc" -ne 0 ]; then
        return "$rc"
    fi
    # First downloaded bundle. A literal glob (no `ls`) handles the digest
    # filename's ':' and the no-match case (the pattern stays literal and the
    # -f test fails, leaving bundle empty).
    bundle=""
    local candidate
    for candidate in "$TMP"/*.jsonl; do
        if [ -f "$candidate" ]; then
            bundle="$candidate"
            break
        fi
    done
    if [ -z "$bundle" ]; then
        printf '%s\n' "could not locate downloaded attestation bundle" >>"$TMP/attest.log"
        return 1
    fi
    rc=0
    cosign verify-blob "$archive" \
        --new-bundle-format \
        --bundle "$bundle" \
        --certificate-identity-regexp "$identity_re" \
        --certificate-oidc-issuer "$ATTESTATION_OIDC_ISSUER" \
        >"$TMP/attest.log" 2>&1 || rc=$?
    return "$rc"
}

# Map a verifier exit code + its $TMP/attest.log output to one of:
#   verified | disproved | cannot_run
# Cannot-run signals (checked first) are "could not reach a verdict" cases that
# must degrade. A disproof requires a positive rejection signal. Anything
# unrecognized defaults to cannot_run: a verifier quirk or output-wording change
# must never break a legitimate `curl | bash` install -- the checksum floor
# still applies and the self-update path enforces attestation more strictly.
classify_attestation_result() {
    local rc log
    rc="$1"
    log="$TMP/attest.log"

    if [ "$rc" -eq 0 ]; then
        printf '%s\n' "verified"
        return
    fi

    if [ -f "$log" ] && grep -qiE \
        'no attestations? (found|present)|found no attestation|could not (find|fetch|load|locate)|failed to fetch|loading attestation|rate.?limit|\b429\b|could not resolve host|no such host|connection (refused|reset)|could(n.?t| not) connect|network is unreachable|i/o timeout|deadline exceeded|temporary failure|tls handshake|x509|multiple attestation|more than one' \
        "$log"; then
        printf '%s\n' "cannot_run"
        return
    fi

    if [ -f "$log" ] && grep -qiE \
        'verification failed|failed to verify|none of the|do(es)? not match|no matching|bad signature|invalid signature|expected identity|certificate identity|untrusted|rejected by policy' \
        "$log"; then
        printf '%s\n' "disproved"
        return
    fi

    printf '%s\n' "cannot_run"
}

verify_attestation() {
    local verifier rc result
    verifier=$(detect_attestation_verifier)

    if [ -z "$verifier" ]; then
        warn "note: no attestation verifier (gh or cosign) available; skipping provenance check and relying on the SHA256 checksum. Install GitHub CLI (gh) to enable attestation verification."
        return
    fi

    rc=0
    run_attestation_verifier "$verifier" || rc=$?
    result=$(classify_attestation_result "$rc")

    # Prefer cosign, but never lose verification to a flaky cosign/bundle-fetch:
    # if the cosign path could not reach a verdict, fall back to gh's direct
    # by-digest verify (gh is always present when cosign was selected).
    if [ "$verifier" = "cosign" ] && [ "$result" = "cannot_run" ]; then
        rc=0
        run_attestation_verifier "gh" || rc=$?
        result=$(classify_attestation_result "$rc")
    fi

    case "$result" in
        verified)
            # Silent on success to keep stdout clean; diagnostics only on
            # degrade/fail (both to stderr).
            return
            ;;
        cannot_run)
            warn "note: could not verify release attestation for $ARCHIVE (verifier unavailable, offline, or no attestation found); relying on the SHA256 checksum."
            return
            ;;
        disproved)
            fail "attestation verification failed for $ARCHIVE: the release artifact is not attributable to $REPO's release workflow. Refusing to install."
            ;;
        *)
            fail "internal error: unclassified attestation result '$result'."
            ;;
    esac
}

# ---------------------------------------------------------------------------
# Stage 6: install binary
# ---------------------------------------------------------------------------

install_binary() {
    if [ -n "$INSTALL_DIR_OVERRIDE" ]; then
        INSTALL_DIR="$INSTALL_DIR_OVERRIDE"
    else
        INSTALL_DIR="$HOME/.local/bin"
    fi

    # Reject install-dir values that would corrupt the rc file when
    # interpolated into `export PATH="<dir>:$PATH"`. The allowlist is
    # intentionally narrow (path-safe punctuation common in $HOME paths);
    # quotes, backslashes, command-substitution triggers, whitespace, and
    # control characters are rejected so a hostile $1UP_INSTALL_DIR cannot
    # write executable code into ~/.zshrc or ~/.bashrc.
    case "$INSTALL_DIR" in
        "")
            fail "install directory is empty"
            ;;
        *[!A-Za-z0-9_./@:+-]*)
            fail "install directory contains unsupported characters: $INSTALL_DIR (allowed: A-Z a-z 0-9 _ . / : @ + -)"
            ;;
    esac

    if ! mkdir -p "$INSTALL_DIR"; then
        fail "cannot write to install directory: $INSTALL_DIR"
    fi

    if ! tar -xzf "$TMP/$ARCHIVE" -C "$TMP"; then
        fail "failed to extract archive $ARCHIVE"
    fi

    local staged_binary stage_target
    staged_binary="$TMP/$PACKAGE_DIR_NAME/1up"

    if [ ! -f "$staged_binary" ]; then
        fail "archive did not contain expected binary: $PACKAGE_DIR_NAME/1up"
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
verify_attestation
install_binary
configure_path
print_next_steps
