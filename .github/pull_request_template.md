## Summary

-

## Release Impact

- [ ] No user-facing or release-surface impact
- [ ] Install or upgrade guidance changed
- [ ] Release notes or changelog update required
- [ ] Packaging or published asset behavior changed
- [ ] Governance or required-check policy changed

## Required Merge Gates (CI)

Expected on the `main` merge path:

- Linux security gate (`just security-check`)
- macOS release-build smoke
- Linux release-build smoke
- Windows release-build smoke
- Fast release-consistency validation

## Local Validation Run

- [ ] `cargo fmt --check`
- [ ] `cargo test`
- [ ] `cargo build --release --bin 1up`
- [ ] `just security-check` (Linux)
- [ ] Release-facing docs and metadata checked for version, changelog, and Apache 2.0 consistency
- [ ] Other validation noted below

## Advisory Release Evidence

- [ ] Not needed for this change
- [ ] `just eval-parallel --summary`
- [ ] `just bench-parallel .`
- [ ] Manual archive or package verification
- [ ] Skipped with explanation below

## Notes

- Include any version, changelog, package, or release-workflow follow-up here.
