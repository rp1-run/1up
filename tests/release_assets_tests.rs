use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
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
        "packaging/homebrew/1up.rb.tmpl",
        "packaging/scoop/1up.json.tmpl",
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

fn run_release_script_with_path(
    root: &Path,
    name: &str,
    args: &[&str],
    path_prefix: &Path,
) -> std::process::Output {
    let path = std::env::var("PATH").unwrap();
    Command::new("bash")
        .arg(release_script(name))
        .args(args)
        .current_dir(repo_root())
        .env("ONEUP_RELEASE_ROOT", root)
        .env("PATH", format!("{}:{path}", path_prefix.display()))
        .output()
        .unwrap()
}

fn write_executable(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn write_release_artifacts(root: &Path, version: &str) -> PathBuf {
    let dist_dir = root.join("dist");
    fs::create_dir_all(&dist_dir).unwrap();

    let artifacts = [
        ("aarch64-apple-darwin", "macos", "arm64", "tar.gz", "Download the macOS arm64 archive from GitHub Releases and unpack with tar -xzf."),
        ("x86_64-apple-darwin", "macos", "amd64", "tar.gz", "Download the macOS amd64 archive from GitHub Releases and unpack with tar -xzf."),
        ("aarch64-unknown-linux-gnu", "linux", "arm64", "tar.gz", "Download the Linux arm64 archive from GitHub Releases and unpack with tar -xzf."),
        ("x86_64-unknown-linux-gnu", "linux", "amd64", "tar.gz", "Download the Linux amd64 archive from GitHub Releases and unpack with tar -xzf."),
        ("x86_64-pc-windows-msvc", "windows", "amd64", "zip", "Download the Windows amd64 archive from GitHub Releases and unpack with Expand-Archive."),
    ];

    for (target, os, arch, extension, install_hint) in artifacts {
        let archive_name = format!("1up-v{version}-{target}.{extension}");
        fs::write(dist_dir.join(&archive_name), &archive_name).unwrap();
        fs::write(
            dist_dir.join(format!("{archive_name}.metadata.json")),
            serde_json::to_vec_pretty(&serde_json::json!({
                "target": target,
                "os": os,
                "arch": arch,
                "archive": archive_name,
                "install_hint": install_hint,
            }))
            .unwrap(),
        )
        .unwrap();
    }

    let checksums_output = run_release_script(
        root,
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
        root,
        "generate_release_manifest.sh",
        &[
            "--tag",
            &format!("v{version}"),
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

    dist_dir
}

fn write_real_release_archive(
    root: &Path,
    dist_dir: &Path,
    version: &str,
    target: &str,
    binary_name: &str,
) {
    let stage_root = root.join(format!("stage-{target}"));
    let package_dir_name = format!("1up-v{version}-{target}");
    let package_dir = stage_root.join(&package_dir_name);
    fs::create_dir_all(&package_dir).unwrap();
    write_executable(
        &package_dir.join(binary_name),
        &format!("#!/bin/sh\nprintf '1up {version} ({target})\\n'\n"),
    );
    fs::copy(repo_root().join("LICENSE"), package_dir.join("LICENSE")).unwrap();
    fs::write(
        package_dir.join("README.txt"),
        format!("1up {version}\nTarget: {target}\n"),
    )
    .unwrap();

    let extension = if target.contains("windows") {
        "zip"
    } else {
        "tar.gz"
    };
    let archive_name = format!("1up-v{version}-{target}.{extension}");
    let archive_path = dist_dir.join(&archive_name);

    if extension == "tar.gz" {
        let output = Command::new("tar")
            .arg("-C")
            .arg(&stage_root)
            .arg("-czf")
            .arg(&archive_path)
            .arg(&package_dir_name)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "tar packaging unexpectedly failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    } else {
        let output = Command::new("zip")
            .arg("-qr")
            .arg(&archive_path)
            .arg(&package_dir_name)
            .current_dir(&stage_root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "zip packaging unexpectedly failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let os = if target.contains("apple-darwin") {
        "macos"
    } else if target.contains("windows") {
        "windows"
    } else {
        "linux"
    };
    let arch = if target.starts_with("aarch64-") {
        "arm64"
    } else {
        "amd64"
    };
    let install_hint = if os == "windows" {
        format!("Download the Windows {arch} archive from GitHub Releases and unpack with Expand-Archive.")
    } else if os == "macos" {
        format!("Download the macOS {arch} archive from GitHub Releases and unpack with tar -xzf.")
    } else {
        format!("Download the Linux {arch} archive from GitHub Releases and unpack with tar -xzf.")
    };

    fs::write(
        dist_dir.join(format!("{archive_name}.metadata.json")),
        serde_json::to_vec_pretty(&serde_json::json!({
            "target": target,
            "os": os,
            "arch": arch,
            "archive": archive_name,
            "install_hint": install_hint,
        }))
        .unwrap(),
    )
    .unwrap();
}

fn write_verifiable_release_artifacts(root: &Path, version: &str) -> PathBuf {
    let dist_dir = root.join("verifiable-dist");
    fs::create_dir_all(&dist_dir).unwrap();

    for (target, binary_name) in [
        ("aarch64-apple-darwin", "1up"),
        ("x86_64-apple-darwin", "1up"),
        ("aarch64-unknown-linux-gnu", "1up"),
        ("x86_64-unknown-linux-gnu", "1up"),
        ("x86_64-pc-windows-msvc", "1up.exe"),
    ] {
        write_real_release_archive(root, &dist_dir, version, target, binary_name);
    }

    let checksums_output = run_release_script(
        root,
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
        root,
        "generate_release_manifest.sh",
        &[
            "--tag",
            &format!("v{version}"),
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

    dist_dir
}

#[cfg(target_os = "macos")]
#[cfg(target_arch = "aarch64")]
const HOST_RELEASE_TARGET: &str = "aarch64-apple-darwin";
#[cfg(target_os = "macos")]
#[cfg(target_arch = "x86_64")]
const HOST_RELEASE_TARGET: &str = "x86_64-apple-darwin";
#[cfg(target_os = "linux")]
#[cfg(target_arch = "aarch64")]
const HOST_RELEASE_TARGET: &str = "aarch64-unknown-linux-gnu";
#[cfg(target_os = "linux")]
#[cfg(target_arch = "x86_64")]
const HOST_RELEASE_TARGET: &str = "x86_64-unknown-linux-gnu";
#[cfg(target_os = "windows")]
const HOST_RELEASE_TARGET: &str = "x86_64-pc-windows-msvc";

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
    let dist_dir = write_release_artifacts(fixture_root.path(), "0.1.0");

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

    // Update lifecycle fields
    let published_at = manifest["published_at"].as_str().unwrap();
    assert!(
        published_at.ends_with('Z') && published_at.contains('T'),
        "published_at should be ISO 8601 UTC: {published_at}"
    );
    assert_eq!(
        manifest["notes_url"],
        "https://github.com/rp1-run/1up/releases/tag/v0.1.0"
    );
    assert_eq!(manifest["yanked"], false);
    assert!(manifest["minimum_safe_version"].is_null());
    assert!(manifest["message"].is_null());
    assert!(manifest["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .all(|artifact| {
            let url = artifact["url"].as_str().unwrap();
            url.starts_with("https://github.com/rp1-run/1up/releases/download/v0.1.0/")
                && url.ends_with(artifact["archive"].as_str().unwrap())
        }));
}

#[test]
fn release_manifest_deserializes_as_update_manifest() {
    let fixture_root = build_release_fixture();
    write_release_changelog(fixture_root.path(), "0.1.0");
    let dist_dir = write_release_artifacts(fixture_root.path(), "0.1.0");

    let raw = fs::read(dist_dir.join("release-manifest.json")).unwrap();
    let manifest: oneup::shared::update::UpdateManifest = serde_json::from_slice(&raw)
        .expect("release manifest should deserialize as UpdateManifest");

    assert_eq!(manifest.version, "0.1.0");
    assert_eq!(manifest.git_tag, "v0.1.0");
    assert!(!manifest.published_at.is_empty());
    assert!(manifest.notes_url.contains("/releases/tag/v0.1.0"));
    assert_eq!(manifest.artifacts.len(), 5);
    assert!(!manifest.yanked);
    assert!(manifest.minimum_safe_version.is_none());
    assert!(manifest.message.is_none());

    for artifact in &manifest.artifacts {
        assert!(!artifact.target.is_empty());
        assert!(!artifact.archive.is_empty());
        assert_eq!(artifact.sha256.len(), 64);
        assert!(
            artifact.url.contains(&artifact.archive),
            "artifact url should contain archive name"
        );
    }
}

#[test]
fn homebrew_formula_rendering_uses_release_manifest_urls_and_checksums() {
    let fixture_root = build_release_fixture();
    write_release_changelog(fixture_root.path(), "0.1.0");
    let dist_dir = write_release_artifacts(fixture_root.path(), "0.1.0");
    let output_path = dist_dir.join("1up.rb");

    let output = run_release_script(
        fixture_root.path(),
        "render_homebrew_formula.sh",
        &[
            "--manifest",
            dist_dir.join("release-manifest.json").to_str().unwrap(),
            "--output",
            output_path.to_str().unwrap(),
        ],
    );
    assert!(
        output.status.success(),
        "Homebrew rendering unexpectedly failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(dist_dir.join("release-manifest.json")).unwrap()).unwrap();
    let formula = fs::read_to_string(&output_path).unwrap();
    let git_tag = manifest["git_tag"].as_str().unwrap();

    for target in [
        "aarch64-apple-darwin",
        "x86_64-apple-darwin",
        "aarch64-unknown-linux-gnu",
        "x86_64-unknown-linux-gnu",
    ] {
        let artifact = manifest["artifacts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|artifact| artifact["target"] == target)
            .unwrap();
        let archive = artifact["archive"].as_str().unwrap();
        let sha256 = artifact["sha256"].as_str().unwrap();
        let url = format!("https://github.com/rp1-run/1up/releases/download/{git_tag}/{archive}");
        assert!(formula.contains(&url));
        assert!(formula.contains(sha256));
    }

    assert!(formula.contains("class Oneup < Formula"));
    assert!(formula.contains("license \"Apache-2.0\""));
    assert!(!formula.contains("x86_64-pc-windows-msvc.zip"));
}

#[test]
fn scoop_manifest_rendering_uses_release_manifest_windows_asset() {
    let fixture_root = build_release_fixture();
    write_release_changelog(fixture_root.path(), "0.1.0");
    let dist_dir = write_release_artifacts(fixture_root.path(), "0.1.0");
    let output_path = dist_dir.join("1up.json");

    let output = run_release_script(
        fixture_root.path(),
        "render_scoop_manifest.sh",
        &[
            "--manifest",
            dist_dir.join("release-manifest.json").to_str().unwrap(),
            "--output",
            output_path.to_str().unwrap(),
        ],
    );
    assert!(
        output.status.success(),
        "Scoop rendering unexpectedly failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(dist_dir.join("release-manifest.json")).unwrap()).unwrap();
    let scoop_manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&output_path).unwrap()).unwrap();
    let windows_artifact = manifest["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|artifact| artifact["target"] == "x86_64-pc-windows-msvc")
        .unwrap();

    assert_eq!(scoop_manifest["version"], manifest["version"]);
    assert_eq!(scoop_manifest["license"], manifest["license"]);
    assert_eq!(
        scoop_manifest["url"],
        format!(
            "https://github.com/rp1-run/1up/releases/download/{}/{}",
            manifest["git_tag"].as_str().unwrap(),
            windows_artifact["archive"].as_str().unwrap()
        )
    );
    assert_eq!(scoop_manifest["hash"], windows_artifact["sha256"]);
    assert_eq!(
        scoop_manifest["extract_dir"],
        "1up-v0.1.0-x86_64-pc-windows-msvc"
    );
    assert_eq!(scoop_manifest["bin"], "1up.exe");
}

#[test]
fn package_publication_record_captures_repo_commit_refs() {
    let fixture_root = build_release_fixture();
    write_release_changelog(fixture_root.path(), "0.1.0");
    let dist_dir = write_release_artifacts(fixture_root.path(), "0.1.0");
    let output_path = dist_dir.join("package-publication-record.json");

    let output = run_release_script(
        fixture_root.path(),
        "write_package_publication_record.sh",
        &[
            "--manifest",
            dist_dir.join("release-manifest.json").to_str().unwrap(),
            "--homebrew-commit",
            "deadbeef1234",
            "--scoop-commit",
            "feedface5678",
            "--output",
            output_path.to_str().unwrap(),
        ],
    );
    assert!(
        output.status.success(),
        "package publication record unexpectedly failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let record: serde_json::Value =
        serde_json::from_slice(&fs::read(output_path).unwrap()).unwrap();
    assert_eq!(record["version"], "0.1.0");
    assert_eq!(record["git_tag"], "v0.1.0");
    assert_eq!(
        record["packages"]["homebrew"]["repo"],
        "rp1-run/homebrew-tap"
    );
    assert_eq!(record["packages"]["homebrew"]["path"], "Formula/1up.rb");
    assert_eq!(record["packages"]["homebrew"]["commit_sha"], "deadbeef1234");
    assert_eq!(
        record["packages"]["homebrew"]["commit_url"],
        "https://github.com/rp1-run/homebrew-tap/commit/deadbeef1234"
    );
    assert_eq!(record["packages"]["scoop"]["repo"], "rp1-run/scoop-bucket");
    assert_eq!(record["packages"]["scoop"]["path"], "bucket/1up.json");
    assert_eq!(record["packages"]["scoop"]["commit_sha"], "feedface5678");
    assert_eq!(
        record["packages"]["scoop"]["commit_url"],
        "https://github.com/rp1-run/scoop-bucket/commit/feedface5678"
    );
}

#[test]
fn archive_verification_confirms_expected_release_contents() {
    let fixture_root = build_release_fixture();
    write_release_changelog(fixture_root.path(), "0.1.0");
    let dist_dir = write_verifiable_release_artifacts(fixture_root.path(), "0.1.0");
    let output_path = dist_dir.join("archive-verification.json");

    let output = run_release_script(
        fixture_root.path(),
        "verify_release_archives.sh",
        &[
            "--manifest",
            dist_dir.join("release-manifest.json").to_str().unwrap(),
            "--assets-dir",
            dist_dir.to_str().unwrap(),
            "--checksums",
            dist_dir.join("SHA256SUMS").to_str().unwrap(),
            "--target",
            HOST_RELEASE_TARGET,
            "--output",
            output_path.to_str().unwrap(),
        ],
    );
    assert!(
        output.status.success(),
        "archive verification unexpectedly failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let verification: serde_json::Value =
        serde_json::from_slice(&fs::read(output_path).unwrap()).unwrap();
    assert_eq!(verification["archive_count"], 1);
    assert_eq!(verification["archives"][0]["target"], HOST_RELEASE_TARGET);
    assert_eq!(
        verification["archives"][0]["verified_contents"]["license"],
        format!("1up-v0.1.0-{}/LICENSE", HOST_RELEASE_TARGET)
    );
    assert_eq!(
        verification["archives"][0]["smoke_test"]["status"],
        "passed"
    );
    assert!(verification["archives"][0]["smoke_test"]["command"]
        .as_str()
        .unwrap()
        .contains("--version"));
    assert!(verification["archives"][0]["smoke_test"]["output"]
        .as_str()
        .unwrap()
        .contains("0.1.0"));
}

#[test]
fn release_evidence_supports_explicit_skipped_eval_reason() {
    let fixture_root = build_release_fixture();
    write_release_changelog(fixture_root.path(), "0.1.0");
    let dist_dir = write_verifiable_release_artifacts(fixture_root.path(), "0.1.0");
    let merge_gate_path = dist_dir.join("merge-gate.json");
    let security_check_path = dist_dir.join("security-check.json");
    let benchmark_summary_path = dist_dir.join("benchmark-summary.json");
    let archive_verification_path = dist_dir.join("archive-verification.json");
    let output_path = dist_dir.join("release-evidence.json");

    fs::write(
        &merge_gate_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "workflow": "ci",
            "run_id": 42,
            "run_number": 7,
            "run_url": "https://github.com/rp1-run/1up/actions/runs/42",
            "head_sha": "abc123def456",
            "conclusion": "success",
            "required_checks": [
                "security-check",
                "release-smoke-macos",
                "release-smoke-linux",
                "release-smoke-windows",
                "release-consistency"
            ]
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(
        &security_check_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "status": "passed"
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(
        &benchmark_summary_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "scenario": "parallel-indexing"
        }))
        .unwrap(),
    )
    .unwrap();

    let archive_output = run_release_script(
        fixture_root.path(),
        "verify_release_archives.sh",
        &[
            "--manifest",
            dist_dir.join("release-manifest.json").to_str().unwrap(),
            "--assets-dir",
            dist_dir.to_str().unwrap(),
            "--checksums",
            dist_dir.join("SHA256SUMS").to_str().unwrap(),
            "--target",
            HOST_RELEASE_TARGET,
            "--output",
            archive_verification_path.to_str().unwrap(),
        ],
    );
    assert!(
        archive_output.status.success(),
        "archive verification unexpectedly failed: {}",
        String::from_utf8_lossy(&archive_output.stderr)
    );

    let output = run_release_script(
        fixture_root.path(),
        "generate_release_evidence.sh",
        &[
            "--manifest",
            dist_dir.join("release-manifest.json").to_str().unwrap(),
            "--merge-gate",
            merge_gate_path.to_str().unwrap(),
            "--security-check",
            security_check_path.to_str().unwrap(),
            "--eval-skipped-reason",
            "Hosted eval artifacts are not retained for this release candidate.",
            "--benchmark-summary",
            benchmark_summary_path.to_str().unwrap(),
            "--archive-verification",
            archive_verification_path.to_str().unwrap(),
            "--output",
            output_path.to_str().unwrap(),
        ],
    );
    assert!(
        output.status.success(),
        "release evidence generation unexpectedly failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let evidence: serde_json::Value =
        serde_json::from_slice(&fs::read(output_path).unwrap()).unwrap();
    assert_eq!(evidence["version"], "0.1.0");
    assert_eq!(evidence["git_tag"], "v0.1.0");
    assert_eq!(evidence["merge_gate"]["workflow"], "ci");
    assert_eq!(
        evidence["security_check"]["artifact"],
        "security-check.json"
    );
    assert_eq!(evidence["evals"]["status"], "skipped");
    assert_eq!(
        evidence["evals"]["skipped_reason"],
        "Hosted eval artifacts are not retained for this release candidate."
    );
    assert_eq!(evidence["benchmarks"]["status"], "recorded");
    assert_eq!(
        evidence["benchmarks"]["summary_asset"],
        "benchmark-summary.json"
    );
    assert_eq!(evidence["archive_verification"]["status"], "recorded");
    assert_eq!(evidence["archive_verification"]["archive_count"], 1);
    assert_eq!(evidence["packages"]["status"], "pending");
}

#[test]
fn release_evidence_rejects_missing_eval_reference_and_skip_reason() {
    let fixture_root = build_release_fixture();
    write_release_changelog(fixture_root.path(), "0.1.0");
    let dist_dir = write_verifiable_release_artifacts(fixture_root.path(), "0.1.0");
    let merge_gate_path = dist_dir.join("merge-gate.json");
    let security_check_path = dist_dir.join("security-check.json");
    let benchmark_summary_path = dist_dir.join("benchmark-summary.json");
    let archive_verification_path = dist_dir.join("archive-verification.json");
    let output_path = dist_dir.join("release-evidence.json");

    fs::write(
        &merge_gate_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "workflow": "ci",
            "run_id": 42,
            "run_number": 7,
            "run_url": "https://github.com/rp1-run/1up/actions/runs/42",
            "head_sha": "abc123def456",
            "conclusion": "success",
            "required_checks": ["security-check"]
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(
        &security_check_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "status": "passed"
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(
        &benchmark_summary_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "scenario": "parallel-indexing"
        }))
        .unwrap(),
    )
    .unwrap();

    let archive_output = run_release_script(
        fixture_root.path(),
        "verify_release_archives.sh",
        &[
            "--manifest",
            dist_dir.join("release-manifest.json").to_str().unwrap(),
            "--assets-dir",
            dist_dir.to_str().unwrap(),
            "--checksums",
            dist_dir.join("SHA256SUMS").to_str().unwrap(),
            "--target",
            HOST_RELEASE_TARGET,
            "--output",
            archive_verification_path.to_str().unwrap(),
        ],
    );
    assert!(
        archive_output.status.success(),
        "archive verification unexpectedly failed: {}",
        String::from_utf8_lossy(&archive_output.stderr)
    );

    let output = run_release_script(
        fixture_root.path(),
        "generate_release_evidence.sh",
        &[
            "--manifest",
            dist_dir.join("release-manifest.json").to_str().unwrap(),
            "--merge-gate",
            merge_gate_path.to_str().unwrap(),
            "--security-check",
            security_check_path.to_str().unwrap(),
            "--benchmark-summary",
            benchmark_summary_path.to_str().unwrap(),
            "--archive-verification",
            archive_verification_path.to_str().unwrap(),
            "--output",
            output_path.to_str().unwrap(),
        ],
    );
    assert!(
        !output.status.success(),
        "release evidence unexpectedly passed without eval evidence: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("eval evidence requires a summary path or a skipped reason"));
}

#[test]
fn release_evidence_rejects_archive_verification_without_smoke_results() {
    let fixture_root = build_release_fixture();
    write_release_changelog(fixture_root.path(), "0.1.0");
    let dist_dir = write_verifiable_release_artifacts(fixture_root.path(), "0.1.0");
    let merge_gate_path = dist_dir.join("merge-gate.json");
    let security_check_path = dist_dir.join("security-check.json");
    let benchmark_summary_path = dist_dir.join("benchmark-summary.json");
    let archive_verification_path = dist_dir.join("archive-verification.json");
    let output_path = dist_dir.join("release-evidence.json");

    fs::write(
        &merge_gate_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "workflow": "ci",
            "run_id": 42,
            "run_number": 7,
            "run_url": "https://github.com/rp1-run/1up/actions/runs/42",
            "head_sha": "abc123def456",
            "conclusion": "success",
            "required_checks": ["security-check"]
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(
        &security_check_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "status": "passed",
            "summary": {},
            "steps": []
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(
        &benchmark_summary_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "scenario": "parallel-indexing"
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(
        &archive_verification_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "generated_at": "2026-04-09T00:00:00Z",
            "manifest_asset": "release-manifest.json",
            "checksums_asset": "SHA256SUMS",
            "archive_count": 1,
            "archives": [{
                "target": HOST_RELEASE_TARGET,
                "archive": format!(
                    "1up-v0.1.0-{HOST_RELEASE_TARGET}.{}",
                    if HOST_RELEASE_TARGET.contains("windows") { "zip" } else { "tar.gz" }
                ),
                "sha256": "abc123"
            }]
        }))
        .unwrap(),
    )
    .unwrap();

    let output = run_release_script(
        fixture_root.path(),
        "generate_release_evidence.sh",
        &[
            "--manifest",
            dist_dir.join("release-manifest.json").to_str().unwrap(),
            "--merge-gate",
            merge_gate_path.to_str().unwrap(),
            "--security-check",
            security_check_path.to_str().unwrap(),
            "--eval-skipped-reason",
            "Hosted eval artifacts are not retained for this release candidate.",
            "--benchmark-summary",
            benchmark_summary_path.to_str().unwrap(),
            "--archive-verification",
            archive_verification_path.to_str().unwrap(),
            "--output",
            output_path.to_str().unwrap(),
        ],
    );
    assert!(
        !output.status.success(),
        "release evidence unexpectedly accepted incomplete archive verification: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("archive verification summary is missing required fields"));
}

#[cfg(unix)]
#[test]
fn retained_security_download_helper_uses_run_artifact() {
    let fixture_root = build_release_fixture();
    let fake_gh_dir = tempfile::tempdir().unwrap();
    let artifact_path = fake_gh_dir.path().join("security-check.json");
    fs::write(
        &artifact_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "status": "passed",
            "summary": { "failed_steps": 0 },
            "steps": []
        }))
        .unwrap(),
    )
    .unwrap();

    let gh_path = fake_gh_dir.path().join("gh");
    write_executable(
        &gh_path,
        r#"#!/bin/sh
set -eu
printf '%s\n' "$*" >> "${FAKE_GH_LOG:?}"
dir="."
while [ "$#" -gt 0 ]; do
  case "$1" in
    -D|--dir)
      dir="$2"
      shift 2
      ;;
    *)
      shift
      ;;
  esac
done
mkdir -p "$dir"
cp "${FAKE_GH_ARTIFACT:?}" "$dir/security-check.json"
"#,
    );

    let log_path = fake_gh_dir.path().join("gh.log");
    let output_path = fixture_root.path().join("downloaded-security-check.json");
    let path = std::env::var("PATH").unwrap();
    let output = Command::new("bash")
        .arg(release_script("download_retained_security_check.sh"))
        .args([
            "--repo",
            "rp1-run/1up",
            "--run-id",
            "42",
            "--output",
            output_path.to_str().unwrap(),
        ])
        .current_dir(repo_root())
        .env("ONEUP_RELEASE_ROOT", fixture_root.path())
        .env("PATH", format!("{}:{path}", fake_gh_dir.path().display()))
        .env("FAKE_GH_ARTIFACT", &artifact_path)
        .env("FAKE_GH_LOG", &log_path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "retained security download unexpectedly failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let downloaded: serde_json::Value =
        serde_json::from_slice(&fs::read(output_path).unwrap()).unwrap();
    assert_eq!(downloaded["status"], "passed");

    let gh_log = fs::read_to_string(log_path).unwrap();
    assert!(gh_log.contains("run download 42"));
    assert!(gh_log.contains("--repo rp1-run/1up"));
    assert!(gh_log.contains("--name security-check"));
}

#[test]
fn ci_workflow_retains_security_check_artifact() {
    let workflow = fs::read_to_string(repo_root().join(".github/workflows/ci.yml")).unwrap();
    assert!(workflow.contains("retain security evidence"));
    assert!(workflow.contains("actions/upload-artifact@v4"));
    assert!(workflow.contains("target/security/security-check.json"));
    assert!(workflow.contains("retention-days: 30"));
}

#[test]
fn release_evidence_workflow_uses_retained_security_and_native_archive_verification() {
    let workflow =
        fs::read_to_string(repo_root().join(".github/workflows/release-evidence.yml")).unwrap();

    assert!(workflow.contains("download retained security check"));
    assert!(workflow.contains("bash scripts/release/download_retained_security_check.sh"));
    assert!(workflow.contains("pattern: archive-verification-*"));
    assert!(workflow.contains("ubuntu-24.04-arm"));
    assert!(workflow.contains("--target \"${{ matrix.target }}\""));
}

#[test]
fn cargo_manifest_uses_runtime_loaded_ort_on_windows() {
    let manifest = fs::read_to_string(repo_root().join("Cargo.toml")).unwrap();

    assert!(manifest.contains("[target.'cfg(windows)'.dependencies]"));
    assert!(manifest.contains("load-dynamic"));
    assert!(manifest.contains("[target.'cfg(not(windows))'.dependencies]"));
}

#[test]
fn release_assets_workflow_stages_windows_onnx_runtime_dll() {
    let workflow =
        fs::read_to_string(repo_root().join(".github/workflows/release-assets.yml")).unwrap();

    assert!(workflow.contains("UPDATE_MANIFEST_URL"));
    assert!(workflow.contains("ONEUP_UPDATE_MANIFEST_URL"));
    assert!(workflow.contains("waiting for existing release"));
    assert!(workflow.contains("gh release edit \"$tag\""));
    assert!(workflow.contains("gh release create \"$tag\""));
    assert!(!workflow.contains("release ${tag} is already published"));
    assert!(workflow.contains("stage Windows ONNX Runtime DLL"));
    assert!(workflow.contains("onnxruntime.dll"));
    assert!(workflow.contains("Get-FileHash"));
    assert!(workflow.contains("x86_64-pc-windows-msvc.tar.lzma2"));
}

#[test]
fn publish_packages_workflow_verifies_stable_update_manifest() {
    let workflow =
        fs::read_to_string(repo_root().join(".github/workflows/publish-packages.yml")).unwrap();

    assert!(workflow.contains("verify stable update manifest"));
    assert!(workflow.contains("waiting for release-manifest.json"));
    assert!(workflow.contains("wait for stable update manifest"));
    assert!(workflow.contains("curl --fail --silent --show-error --location"));
    assert!(workflow.contains("jq -S"));
    assert!(workflow.contains("diff -u"));
    assert!(workflow.contains("UPDATE_MANIFEST_URL"));
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

#[cfg(unix)]
#[test]
fn packaging_helper_includes_windows_dll_sidecars() {
    let fixture_root = tempfile::tempdir().unwrap();
    let fake_pwsh_dir = tempfile::tempdir().unwrap();
    copy_surface(fixture_root.path(), "LICENSE");

    let bin_dir = fixture_root.path().join("bin");
    let out_dir = fixture_root.path().join("dist");
    fs::create_dir_all(&bin_dir).unwrap();
    fs::write(bin_dir.join("1up.exe"), "fake binary").unwrap();
    fs::write(bin_dir.join("onnxruntime.dll"), "fake runtime").unwrap();
    fs::write(bin_dir.join("ignored.txt"), "not packaged").unwrap();

    let pwsh_path = fake_pwsh_dir.path().join("pwsh");
    write_executable(
        &pwsh_path,
        r#"#!/bin/sh
set -eu
cmd=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    -Command)
      cmd="$2"
      shift 2
      ;;
    *)
      shift
      ;;
  esac
done
stage_root=$(printf '%s' "$cmd" | sed -n "s/.*Set-Location -LiteralPath '\([^']*\)'.*/\1/p")
package_dir=$(printf '%s' "$cmd" | sed -n "s/.*Compress-Archive -Path '\([^']*\)'.*/\1/p")
archive_path=$(printf '%s' "$cmd" | sed -n "s/.*-DestinationPath '\([^']*\)'.*/\1/p")
test -n "$stage_root"
test -n "$package_dir"
test -n "$archive_path"
cd "$stage_root"
zip -qr "$archive_path" "$package_dir"
"#,
    );

    let output = run_release_script_with_path(
        fixture_root.path(),
        "package_release_asset.sh",
        &[
            "--target",
            "x86_64-pc-windows-msvc",
            "--binary",
            bin_dir.join("1up.exe").to_str().unwrap(),
            "--output-dir",
            out_dir.to_str().unwrap(),
            "--version",
            "0.1.0",
        ],
        fake_pwsh_dir.path(),
    );
    assert!(
        output.status.success(),
        "windows packaging unexpectedly failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let archive_path = out_dir.join("1up-v0.1.0-x86_64-pc-windows-msvc.zip");
    let listing = Command::new("unzip")
        .arg("-Z1")
        .arg(&archive_path)
        .output()
        .unwrap();
    assert!(
        listing.status.success(),
        "failed to inspect windows archive: {}",
        String::from_utf8_lossy(&listing.stderr)
    );

    let listing_text = String::from_utf8_lossy(&listing.stdout);
    assert!(listing_text.contains("1up-v0.1.0-x86_64-pc-windows-msvc/1up.exe"));
    assert!(listing_text.contains("1up-v0.1.0-x86_64-pc-windows-msvc/onnxruntime.dll"));
    assert!(!listing_text.contains("1up-v0.1.0-x86_64-pc-windows-msvc/ignored.txt"));
}
