# Release

Operator runbook for cutting and publishing a public `1up` release.

## Canonical Sources

| Surface | Source of truth |
|---------|-----------------|
| Version | `Cargo.toml` `package.version` |
| Tag | `vX.Y.Z` |
| Release notes | `CHANGELOG.md` |
| Public record | GitHub Release plus `CHANGELOG.md` |
| Primary user install channel | `scripts/install/setup.sh`, attached to each GitHub Release; fetched via `https://1up.rp1.run/setup.sh` (a redirect to `https://github.com/rp1-run/1up/releases/latest/download/setup.sh`) |
| Stable update metadata | `update-manifest.json` on `main`, sourced from the GitHub Release manifest |

## User Install Channel

The `curl -fsSL https://1up.rp1.run/setup.sh | bash` command in `README.md` is the single user-facing install path. `1up.rp1.run/setup.sh` is a Cloudflare 302 redirect to `https://github.com/rp1-run/1up/releases/latest/download/setup.sh`, and GitHub Releases is the only channel that serves the script. The installer consumes the archive and `SHA256SUMS` artifacts attached to each GitHub Release, so the release flow must keep attaching `setup.sh` alongside those assets under stable names (`setup.sh`, `1up-vX.Y.Z-<target>.tar.gz`, and `SHA256SUMS`).

The redirect is load-bearing beyond the README: binaries shipped before v0.1.16 print `https://1up.rp1.run/setup.sh` in their upgrade instructions, and the release/update manifests carry it as the install URL. Do not remove the Cloudflare rule or repoint it away from the latest-release asset without a migration plan for those clients.

The stable update manifest is retained for `1up update` and installer metadata. No downstream package channel is required for release readiness.

## Preconditions

- Start from a clean checkout of `main`
- Confirm the target version in `Cargo.toml`
- Update `CHANGELOG.md` for the release
- Confirm `README.md`, `CONTRIBUTING.md`, and `LICENSE` still describe the current public posture
- Ensure the release owner has access to GitHub Releases and permission to publish `update-manifest.json` to `main`

## Merge-Gate Versus Release-Time Evidence

Required merge gates are the checks expected to stay stable on normal pull requests:

- formatting and test validation
- `just security-check`
- release smoke builds for supported platforms
- fast release consistency validation for version, changelog, and license metadata

Release-time evidence is heavier and should be reviewed before publishing a public release:

- retained `target/security/security-check.json`
- eval summary or an explicit skipped-eval reason
- benchmark summary or an explicit skipped-benchmark reason
- archive verification notes
- MCP smoke evidence for the retained `oneup_*` tools
- script installer and update-manifest distribution metadata

## Standard Release Flow

1. Prepare a release PR.
2. Update `Cargo.toml` to the target version if needed.
3. Add the user-facing release notes to `CHANGELOG.md`.
4. Run the local validation set:

   ```sh
   cargo fmt --check
   cargo test
   cargo build --release
   just security-check
   ```

5. Run heavier evidence only when the release scope warrants it:

   ```sh
   just eval-parallel --summary
   just bench-parallel .
   ```

6. Merge the release prep PR once code-owner review and required checks pass.
7. Create an annotated tag from `main`:

   ```sh
   git tag -a vX.Y.Z -m "Release vX.Y.Z"
   git push origin vX.Y.Z
   ```

8. Review the draft GitHub Release generated from the tag-triggered release workflows.
9. Confirm the release includes archives, `SHA256SUMS`, manifest data, and release evidence references.
10. Publish the GitHub Release.
11. Confirm `publish-update-manifest` writes `update-manifest.json` to `main`, then verify the stable manifest matches the released manifest.

## Publish Checklist

- Version in `Cargo.toml` matches the release tag
- `CHANGELOG.md` contains the released notes
- Archives are present for macOS arm64, Linux arm64, Linux amd64, and Windows amd64
- `SHA256SUMS` and release metadata are attached to the draft release
- Security, eval, and benchmark evidence are retained or explicitly marked as skipped with a reason
- Archive verification and MCP smoke evidence are retained for the released assets
- `update-manifest.json` on `main` points at the published immutable assets
- The final `release-evidence.json` references the GitHub Release, script installer, update manifest, archive verification, security, eval, and benchmark evidence

## Rollback And Repair

If the draft release is wrong, fix the branch and replace the tag before publishing.

If the release has already been published:

- publish a corrective patch release instead of mutating the shipped version in place
- document the correction in `CHANGELOG.md`
- repair `update-manifest.json` if it points at the wrong immutable assets
- retain notes explaining what changed and why
