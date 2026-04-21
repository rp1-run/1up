# `1up update` Verification — setup.sh install channel

**Feature**: update-script (T5)
**Date**: 2026-04-20
**Host**: macOS (Darwin arm64)
**Verifier**: task-builder (feature `update-script`)
**Outcome**: Verified. `1up update` replaces the binary in place at the `setup.sh` install path with no manual intervention and reports "Already up to date" on a back-to-back re-run.

## Scope

Satisfies REQ-040, REQ-041, REQ-042 and design §2.3 (Interaction with existing `1up update`). The feature explicitly scopes `1up update` as **verify-only**: record evidence that the existing self-update path works against the new install layout, or record a discovery naming the gap. No code change was intended or made for this task.

## Environment

| Field | Value |
|-------|-------|
| OS / arch | `Darwin arm64` (`aarch64-apple-darwin`) |
| Release latest tag | `v0.1.8` |
| Pinned older tag for install | `v0.1.7` |
| Install script | `scripts/install/setup.sh` (commit `bd0e357`) |
| Install dir (default) | `$HOME/.1up/bin` |
| Update manifest URL | `https://raw.githubusercontent.com/rp1-run/1up/main/update-manifest.json` (live; serves 0.1.8) |
| SHA256SUMS | Published for `v0.1.8`; published for `v0.1.7` (hash verified during install) |

## Step 1 — Install v0.1.7 via `setup.sh`

Command:

```sh
rm -rf /tmp/1up-verify && mkdir -p /tmp/1up-verify && cd /tmp/1up-verify
curl -fsSL https://raw.githubusercontent.com/rp1-run/1up/update-script/scripts/install/setup.sh | env 1UP_VERSION=v0.1.7 bash
```

Stdout:

```
downloading 1up-v0.1.7-aarch64-apple-darwin.tar.gz
verified sha256 for 1up-v0.1.7-aarch64-apple-darwin.tar.gz
installed 1up v0.1.7 to /Users/prem/.1up/bin/1up
Updated /Users/prem/.zshrc. Run `source /Users/prem/.zshrc` or open a new shell to put 1up on PATH for this session.
Installed 1up v0.1.7 to /Users/prem/.1up/bin.
Run: 1up start
```

Exit code: `0`.

Install-dir state (before `1up update`):

```
drwxr-xr-x@ 3 prem  staff        96 20 Apr 16:37 .
-rwxr-xr-x@ 1 prem  staff  42190272 20 Apr 16:37 1up
```

| Field | Value |
|-------|-------|
| Path | `/Users/prem/.1up/bin/1up` |
| Inode | `238996472` |
| Size | `42190272` bytes |
| Mtime | `Apr 20 16:37:49 2026` |
| `1up --version` | `1up 0.1.7` |

## Step 2 — Channel detection

`detect_install_channel()` in `src/shared/update.rs:168` resolves the running `current_exe()` and delegates to `detect_channel_from_path` (line 182). For `/Users/prem/.1up/bin/1up` none of the three markers hit:

- `/Cellar/` — absent
- `/homebrew/` — absent
- `\scoop\apps\` — absent (and `cfg!(target_os = "windows")` is false)

Therefore the function returns `InstallChannel::Manual`, which in `src/cli/update.rs:143` is the branch that calls `self_update(...)`. Behavior in Step 3 (actual atomic replace, not a printed brew/scoop upgrade hint) confirms the Manual branch fired.

## Step 3 — First `1up update` (0.1.7 -> 0.1.8)

Command: `/Users/prem/.1up/bin/1up update`

Stdout/stderr:

```
Stopping daemon (pid=65535) before update...
Daemon stopped.
Updated 1up from 0.1.7 to 0.1.8.
```

Exit code: `0`.

Install-dir state (after `1up update`):

```
drwxr-xr-x@ 3 prem  staff        96 20 Apr 16:38 .
-rwxr-xr-x@ 1 prem  staff  42190272 19 Apr 18:34 1up
```

| Field | Before | After | Observation |
|-------|--------|-------|-------------|
| Path | `/Users/prem/.1up/bin/1up` | `/Users/prem/.1up/bin/1up` | Unchanged |
| Inode | `238996472` | `238996553` | **Replaced** (fresh inode, consistent with `std::fs::rename` of a staged temp file over the target -- see `self_update` in `src/shared/update.rs:730`) |
| Size | `42190272` | `42190272` | Same for these adjacent releases |
| Mtime | `Apr 20 16:37:49 2026` | `Apr 19 18:34:53 2026` | **Replaced** (mtime carries the release-archive mtime rather than the install time, confirming the payload is the new archive's binary, not the old one retouched) |
| Mode | `0755` | `0755` | Preserved |
| `1up --version` | `1up 0.1.7` | `1up 0.1.8` | Upgraded |

No sibling `.old`, `.bak`, or staging directory remained in `~/.1up/bin/` after the update (`ls -la` shows only `1up`). `tempfile::tempdir_in(staging_parent)` placed the staging dir inside `~/.1up/bin/`, and its `Drop` cleaned it up on success.

## Step 4 — Second `1up update` (no-op)

Command: `/Users/prem/.1up/bin/1up update`

Stdout:

```
Already up to date (version 0.1.8).
```

Exit code: `0`.

Install-dir state is unchanged from the end of Step 3. Satisfies **REQ-042**: back-to-back `1up update` reports current and exits 0.

## Step 5 — Final functional smoke

`/Users/prem/.1up/bin/1up hello-agent` printed the expected agent quick-reference output, confirming the replaced binary is executable and self-consistent.

## Acceptance mapping

| AC (T5) | Status | Evidence |
|---------|--------|----------|
| Run `setup.sh` locally to install an older pinned version into `$HOME/.1up/bin` | Verified | Step 1 |
| Run `1up update` twice; record `1up --version` before and after | Verified | Steps 1, 3, 4 |
| Record install-dir contents, inode state, exit codes | Verified | Steps 1, 3, 4 tables |
| Confirm `detect_install_channel()` resolves to `InstallChannel::Manual` for `~/.1up/bin/1up` | Verified | Step 2 (behavioral + code reference) |
| Second back-to-back `1up update` reports current and exits 0 | Verified | Step 4 |
| Evidence file exists with before/after + exit codes + verified-or-discovery | Verified | this document |

| Requirement | Status | Evidence |
|-------------|--------|----------|
| REQ-040: `1up update` replaces binary in place at the setup.sh install path | Verified | Step 3 |
| REQ-041: Verification log or discovery entry attached to feature artifacts | Verified | this document at `docs/verification/update-cli-run.md` |
| REQ-042: Back-to-back `1up update` reports current and exits 0 | Verified | Step 4 |

## Discoveries

None. The existing `1up update` self-update path works correctly against binaries installed by the new `setup.sh` with no changes to either side. No gap to record against D5 (Design Decisions Log).

## Reproduction

```sh
# 1. Install pinned older release via setup.sh.
#    Run either from the repo root (so `scripts/install/setup.sh` resolves),
#    or via the curl|bash form shown in Step 1.
env 1UP_VERSION=v0.1.7 bash scripts/install/setup.sh

# 2. Inspect install
stat -f 'inode=%i size=%z mtime=%Sm' "$HOME/.1up/bin/1up"
"$HOME/.1up/bin/1up" --version

# 3. Upgrade in place
"$HOME/.1up/bin/1up" update

# 4. Inspect again
stat -f 'inode=%i size=%z mtime=%Sm' "$HOME/.1up/bin/1up"
"$HOME/.1up/bin/1up" --version

# 5. Confirm idempotent no-op
"$HOME/.1up/bin/1up" update
```
