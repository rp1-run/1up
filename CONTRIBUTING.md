# Contributing

Contribution and merge policy for `1up`.

## Branch And Review Policy

- `main` is the release-bearing branch.
- Changes should arrive through pull requests; direct pushes to `main` are not part of the normal release path.
- At least one code-owner review is required before merge.
- Required merge-gate checks must pass before merge.
- Advisory release evidence can be reviewed outside the blocking merge path, but it must be accounted for before a public release is published.

## Pull Request Expectations

Every pull request should clearly state:

- what changed
- whether the change affects install, upgrade, release, packaging, or governance surfaces
- what local validation was run
- whether follow-on release evidence is needed

If the change alters public behavior or release-facing documentation, update the relevant markdown surfaces in the same pull request.

## Required Checks

The blocking merge gates for the release-bearing branch are:

- `just security-check`
- macOS release-build smoke
- Linux release-build smoke
- Windows release-build smoke
- fast release-consistency validation for version, tag/changelog, and Apache 2.0 metadata alignment

## Advisory Release Evidence

The following evidence is not expected to block ordinary pull requests, but it should be attached to release assessment when the change risk warrants it:

- `just eval-parallel --summary`
- benchmark outputs from `just bench-parallel` or other release benchmark scripts
- archive verification notes
- package publication references

If an eval or benchmark is intentionally skipped for a release, record the skipped reason explicitly.

## Local Validation

Local validation is separate from the protected-branch merge gates. Run the commands that fit your platform and change surface before you open or update a pull request.

```sh
cargo fmt --check
cargo test
cargo build --release --bin 1up
```

On Linux, also run the documented security gate locally when your environment supports it:

```sh
just security-check
```

If you touch release-facing docs or metadata, manually confirm `Cargo.toml`, `README.md`, `CHANGELOG.md`, `RELEASE.md`, `LICENSE`, and related package metadata stay aligned before relying on the CI release-consistency check.

Optional heavier validation:

```sh
just eval-parallel --summary
just bench-parallel .
```

## Documentation And Release Surfaces

Keep these files aligned when a change touches public release posture:

- `README.md` for install, verification, upgrade, and first-use guidance
- `CHANGELOG.md` for user-facing shipped changes
- `RELEASE.md` for operator workflow and evidence expectations
- `LICENSE` and other metadata surfaces for Apache 2.0 consistency

## Reporting Issues

When reporting a bug or proposing a release-surface change, include the platform, installation channel, `1up --version` output, and any validation steps already attempted.
