use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn release_script(name: &str) -> PathBuf {
    repo_root().join("scripts").join("release").join(name)
}

fn copy_surface(root: &Path, relative_path: &str) {
    let source = repo_root().join(relative_path);
    let destination = root.join(relative_path);
    fs::create_dir_all(destination.parent().unwrap()).unwrap();
    fs::copy(source, destination).unwrap();
}

fn build_release_fixture() -> tempfile::TempDir {
    let tempdir = tempfile::tempdir().unwrap();

    for relative_path in [
        "Cargo.toml",
        "README.md",
        "LICENSE",
        "CHANGELOG.md",
        "skills/1up-search/SKILL.md",
    ] {
        copy_surface(tempdir.path(), relative_path);
    }

    tempdir
}

fn write_release_changelog(root: &Path, version: &str) {
    let changelog = format!(
        "# Changelog\n\n## [Unreleased]\n\n## [{version}] - 2026-04-09\n\n### Added\n\n- Release asset automation\n"
    );
    fs::write(root.join("CHANGELOG.md"), changelog).unwrap();
}

fn run_release_script(root: &Path, name: &str, args: &[&str]) -> std::process::Output {
    Command::new("bash")
        .arg(release_script(name))
        .args(args)
        .current_dir(repo_root())
        .env("ONEUP_RELEASE_ROOT", root)
        .output()
        .unwrap()
}

#[test]
fn release_metadata_validation_passes_for_matching_tag_and_changelog() {
    let fixture_root = build_release_fixture();
    write_release_changelog(fixture_root.path(), "0.1.0");

    let output = run_release_script(
        fixture_root.path(),
        "validate_release_metadata.sh",
        &["v0.1.0"],
    );
    assert!(
        output.status.success(),
        "validation unexpectedly failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn release_metadata_validation_rejects_mismatched_tag_and_version() {
    let fixture_root = build_release_fixture();
    write_release_changelog(fixture_root.path(), "0.1.0");

    let output = run_release_script(
        fixture_root.path(),
        "validate_release_metadata.sh",
        &["v0.1.1"],
    );
    assert!(
        !output.status.success(),
        "validation unexpectedly passed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("does not match release tag"));
}

#[test]
fn release_manifest_generation_includes_platform_mapping_and_checksums() {
    let fixture_root = build_release_fixture();
    write_release_changelog(fixture_root.path(), "0.1.0");

    let dist_dir = fixture_root.path().join("dist");
    fs::create_dir_all(&dist_dir).unwrap();

    let artifacts = [
        (
            "1up-v0.1.0-aarch64-apple-darwin.tar.gz",
            serde_json::json!({
                "target": "aarch64-apple-darwin",
                "os": "macos",
                "arch": "arm64",
                "archive": "1up-v0.1.0-aarch64-apple-darwin.tar.gz",
                "install_hint": "Download the macOS arm64 archive from GitHub Releases and unpack with tar -xzf."
            }),
        ),
        (
            "1up-v0.1.0-x86_64-apple-darwin.tar.gz",
            serde_json::json!({
                "target": "x86_64-apple-darwin",
                "os": "macos",
                "arch": "amd64",
                "archive": "1up-v0.1.0-x86_64-apple-darwin.tar.gz",
                "install_hint": "Download the macOS amd64 archive from GitHub Releases and unpack with tar -xzf."
            }),
        ),
        (
            "1up-v0.1.0-aarch64-unknown-linux-gnu.tar.gz",
            serde_json::json!({
                "target": "aarch64-unknown-linux-gnu",
                "os": "linux",
                "arch": "arm64",
                "archive": "1up-v0.1.0-aarch64-unknown-linux-gnu.tar.gz",
                "install_hint": "Download the Linux arm64 archive from GitHub Releases and unpack with tar -xzf."
            }),
        ),
        (
            "1up-v0.1.0-x86_64-unknown-linux-gnu.tar.gz",
            serde_json::json!({
                "target": "x86_64-unknown-linux-gnu",
                "os": "linux",
                "arch": "amd64",
                "archive": "1up-v0.1.0-x86_64-unknown-linux-gnu.tar.gz",
                "install_hint": "Download the Linux amd64 archive from GitHub Releases and unpack with tar -xzf."
            }),
        ),
        (
            "1up-v0.1.0-x86_64-pc-windows-msvc.zip",
            serde_json::json!({
                "target": "x86_64-pc-windows-msvc",
                "os": "windows",
                "arch": "amd64",
                "archive": "1up-v0.1.0-x86_64-pc-windows-msvc.zip",
                "install_hint": "Download the Windows amd64 archive from GitHub Releases and unpack with Expand-Archive."
            }),
        ),
    ];

    for (archive_name, metadata) in artifacts {
        fs::write(dist_dir.join(archive_name), archive_name).unwrap();
        fs::write(
            dist_dir.join(format!("{archive_name}.metadata.json")),
            serde_json::to_vec_pretty(&metadata).unwrap(),
        )
        .unwrap();
    }

    let checksums_output = run_release_script(
        fixture_root.path(),
        "write_sha256sums.sh",
        &[
            "--assets-dir",
            dist_dir.to_str().unwrap(),
            "--output",
            dist_dir.join("SHA256SUMS").to_str().unwrap(),
        ],
    );
    assert!(
        checksums_output.status.success(),
        "checksum generation unexpectedly failed: {}",
        String::from_utf8_lossy(&checksums_output.stderr)
    );

    let manifest_output = run_release_script(
        fixture_root.path(),
        "generate_release_manifest.sh",
        &[
            "--tag",
            "v0.1.0",
            "--assets-dir",
            dist_dir.to_str().unwrap(),
            "--checksums",
            dist_dir.join("SHA256SUMS").to_str().unwrap(),
            "--output",
            dist_dir.join("release-manifest.json").to_str().unwrap(),
            "--commit-sha",
            "abc123def456",
        ],
    );
    assert!(
        manifest_output.status.success(),
        "manifest generation unexpectedly failed: {}",
        String::from_utf8_lossy(&manifest_output.stderr)
    );

    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(dist_dir.join("release-manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["version"], "0.1.0");
    assert_eq!(manifest["git_tag"], "v0.1.0");
    assert_eq!(manifest["commit_sha"], "abc123def456");
    assert_eq!(manifest["license"], "Apache-2.0");
    assert_eq!(manifest["binary_name"], "1up");
    assert_eq!(manifest["checksums_file"], "SHA256SUMS");
    assert_eq!(manifest["notes_source"], "CHANGELOG.md#[0.1.0]");
    assert_eq!(manifest["artifacts"].as_array().unwrap().len(), 5);
    assert_eq!(manifest["artifacts"][0]["target"], "aarch64-apple-darwin");
    assert_eq!(manifest["artifacts"][0]["arch"], "arm64");
    assert!(manifest["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .any(|artifact| artifact["target"] == "x86_64-pc-windows-msvc"));
    assert_eq!(manifest["channels"]["homebrew_tap"], "rp1-run/homebrew-tap");
    assert_eq!(
        manifest["channels"]["homebrew_formula"],
        "brew install rp1-run/tap/1up"
    );
    assert_eq!(manifest["channels"]["scoop_bucket"], "rp1-run/scoop-bucket");
    assert_eq!(
        manifest["channels"]["github_release"],
        "https://github.com/rp1-run/1up/releases/tag/v0.1.0"
    );
    assert!(manifest["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .all(|artifact| artifact["sha256"].as_str().unwrap().len() == 64));
}

#[cfg(unix)]
#[test]
fn packaging_helper_creates_release_archive_with_license_and_readme() {
    let fixture_root = tempfile::tempdir().unwrap();
    copy_surface(fixture_root.path(), "LICENSE");

    let bin_dir = fixture_root.path().join("bin");
    let out_dir = fixture_root.path().join("dist");
    fs::create_dir_all(&bin_dir).unwrap();
    fs::write(bin_dir.join("1up"), "fake binary").unwrap();

    let output = run_release_script(
        fixture_root.path(),
        "package_release_asset.sh",
        &[
            "--target",
            "x86_64-unknown-linux-gnu",
            "--binary",
            bin_dir.join("1up").to_str().unwrap(),
            "--output-dir",
            out_dir.to_str().unwrap(),
            "--version",
            "0.1.0",
        ],
    );
    assert!(
        output.status.success(),
        "packaging unexpectedly failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let archive_path = out_dir.join("1up-v0.1.0-x86_64-unknown-linux-gnu.tar.gz");
    let metadata_path = out_dir.join("1up-v0.1.0-x86_64-unknown-linux-gnu.tar.gz.metadata.json");
    assert!(archive_path.exists());
    assert!(metadata_path.exists());

    let listing = Command::new("tar")
        .arg("-tzf")
        .arg(&archive_path)
        .output()
        .unwrap();
    assert!(
        listing.status.success(),
        "failed to inspect archive: {}",
        String::from_utf8_lossy(&listing.stderr)
    );

    let listing_text = String::from_utf8_lossy(&listing.stdout);
    assert!(listing_text.contains("1up-v0.1.0-x86_64-unknown-linux-gnu/1up"));
    assert!(listing_text.contains("1up-v0.1.0-x86_64-unknown-linux-gnu/LICENSE"));
    assert!(listing_text.contains("1up-v0.1.0-x86_64-unknown-linux-gnu/README.txt"));
}
