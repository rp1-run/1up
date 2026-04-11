# Release

Operator runbook for cutting and publishing a public `1up` release.

## Canonical Sources

| Surface | Source of truth |
|---------|-----------------|
| Version | `Cargo.toml` `package.version` |
| Tag | `vX.Y.Z` |
| Release notes | `CHANGELOG.md` |
| Public record | GitHub Release plus `CHANGELOG.md` |
| Supported install channels | GitHub Releases, Homebrew, Scoop |

## Preconditions

- Start from a clean checkout of `main`
- Confirm the target version in `Cargo.toml`
- Update `CHANGELOG.md` for the release
- Confirm `README.md`, `CONTRIBUTING.md`, and `LICENSE` still describe the current public posture
- Ensure the release owner has access to GitHub Releases and the package publication repositories

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
- Homebrew and Scoop publication references

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
11. Confirm the package publication step updates Homebrew and Scoop to the same release, then wait for `release-evidence.json` to refresh with the published package references.

## Publish Checklist

- Version in `Cargo.toml` matches the release tag
- `CHANGELOG.md` contains the released notes
- Archives are present for macOS arm64, Linux arm64, Linux amd64, and Windows amd64
- `SHA256SUMS` and release metadata are attached to the draft release
- Security, eval, and benchmark evidence are retained or explicitly marked as skipped with a reason
- Homebrew and Scoop point at the published immutable assets
- The final `release-evidence.json` references `package-publication-record.json` after package publication completes

## Rollback And Repair

If the draft release is wrong, fix the branch and replace the tag before publishing.

If the release has already been published:

- publish a corrective patch release instead of mutating the shipped version in place
- document the correction in `CHANGELOG.md`
- repair package definitions so they reference the intended immutable assets
- retain notes explaining what changed and why
