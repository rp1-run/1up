<p align="center">
  <img src="assets/logo.png" alt="1up" width="128" height="128" />
</p>

# 1up

Unified search substrate for source repositories. `1up` is a single CLI binary for symbol lookup, reference search, context retrieval, structural queries, and hybrid semantic plus full-text code search with machine-readable output.

Built in Rust with tree-sitter for multi-language parsing, ONNX embeddings (`all-MiniLM-L6-v2`) for semantic retrieval, and libSQL for persistent indexing. On macOS and Linux, a background daemon can watch for file changes and keep the index warm between queries.

## Install

Public installs are intended to come from tagged GitHub releases and first-party package definitions. The distributed executable is always named `1up`.

| Channel | Platforms | Command |
|---------|-----------|---------|
| Homebrew | macOS, Linux | `brew install rp1-run/tap/1up` |
| Scoop | Windows | `scoop install https://github.com/rp1-run/scoop-bucket/raw/main/bucket/1up.json` |
| Direct release asset | macOS arm64, macOS amd64, Linux arm64, Linux amd64, Windows amd64 | Download the matching archive from [GitHub Releases](https://github.com/rp1-run/1up/releases) |

### Supported Platforms

| OS | Architectures | Notes |
|----|---------------|-------|
| macOS | arm64, amd64 | Full install-first workflow with daemon support |
| Linux | arm64, amd64 | Full install-first workflow with daemon support |
| Windows | amd64 | Local-mode workflow only for the first public release |

## Verify

Confirm the binary is available and reports the expected version:

```sh
1up --version
1up --help
```

If you downloaded a release archive directly, also download `SHA256SUMS` from the same GitHub Release and verify the one archive you actually downloaded before unpacking it.

On macOS and Linux, replace `ASSET` with the archive filename you downloaded:

```sh
ASSET=1up-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz
EXPECTED=$(awk -v asset="$ASSET" '$2 == asset {print $1}' SHA256SUMS)
printf '%s  %s\n' "$EXPECTED" "$ASSET" | shasum -a 256 -c -
```

On Windows PowerShell:

```powershell
$Asset = "1up-vX.Y.Z-x86_64-pc-windows-msvc.zip"
$Line = Select-String -Path .\SHA256SUMS -Pattern $Asset -SimpleMatch
if (-not $Line) { throw "No checksum entry found for $Asset" }
$Expected = ($Line.Line -split '\s+')[0].ToLower()
$Actual = (Get-FileHash ".\$Asset" -Algorithm SHA256).Hash.ToLower()
if ($Actual -ne $Expected) { throw "SHA256 mismatch for $Asset" }
"SHA256 OK: $Asset"
```

## Upgrade

Use the same channel you installed from:

```sh
brew upgrade 1up
scoop update 1up
```

For direct release assets, download the newer archive from [GitHub Releases](https://github.com/rp1-run/1up/releases), verify its checksum, and replace the existing binary.

## First Use

Initialize a project from the repository root you want to search:

```sh
cd /path/to/your/repo
1up init
```

On macOS and Linux, start daemon-backed indexing:

```sh
1up start
```

On Windows, use the local-mode workflow instead of daemon commands:

```sh
1up index .
```

Run a few common workflows:

```sh
1up search "error handling"
1up symbol MyFunction
1up context src/main.rs:42
1up structural "(function_item name: (identifier) @name)"
```

## Model Download

On the first indexing or semantic search run, `1up` may download `model.onnx` and `tokenizer.json` from Hugging Face into `~/.local/share/1up/models/all-MiniLM-L6-v2/verified/<artifact-id>/`.

Those files only become active after both pass pinned SHA-256 verification and `current.json` is updated. If a download fails, the last verified artifact stays active and `1up` writes `~/.local/share/1up/models/all-MiniLM-L6-v2/.download_failed`; remove that marker and rerun `1up index` or `1up start` to retry semantic-search setup.

## Windows Differences

The first Windows release is intended to support local-mode commands only: `init`, `index`, `reindex`, `search`, `symbol`, `context`, and `structural`.

Daemon-oriented workflows such as `start`, `stop`, and daemon-backed auto-start remain Unix-first surfaces. Use direct indexing on Windows and rerun `1up index` or `1up reindex` when you want to refresh the local database.

## Agent Skill

`1up` ships a portable [Agent Skill](https://agentskills.io/specification) that teaches AI coding agents to prefer `1up` over grep-like text search for code exploration.

```sh
npx skills add rp1-run/1up
```

This auto-detects supported agent clients and configures the skill for each one.

## Source Builds

Package-based installs and direct release assets are the supported onboarding path. If you need a development build instead, see [DEVELOPMENT.md](DEVELOPMENT.md).

```sh
git clone https://github.com/rp1-run/1up.git
cd 1up
cargo install --path .
```

## Project Docs

- Public release history: [CHANGELOG.md](CHANGELOG.md)
- Release runbook: [RELEASE.md](RELEASE.md)
- Contributor policy and merge expectations: [CONTRIBUTING.md](CONTRIBUTING.md)
- Source-build and internal engineering reference: [DEVELOPMENT.md](DEVELOPMENT.md)

## License

Apache 2.0. See [LICENSE](LICENSE).
