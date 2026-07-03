use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use oneup::mcp::types::RETAINED_PUBLIC_TOOLS;

const TEST_RELEASE_VERSION: &str = env!("CARGO_PKG_VERSION");

fn repo_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn release_script(name: &str) -> PathBuf {
    repo_root().join("scripts").join("release").join(name)
}

fn test_release_tag() -> String {
    format!("v{TEST_RELEASE_VERSION}")
}

fn legacy_component_release_tag() -> String {
    format!("oneup-v{TEST_RELEASE_VERSION}")
}

fn next_patch_tag() -> String {
    let mut parts = TEST_RELEASE_VERSION.split('.');
    let major = parts.next().unwrap();
    let minor = parts.next().unwrap();
    let patch = parts.next().unwrap().parse::<u64>().unwrap() + 1;
    format!("v{major}.{minor}.{patch}")
}

fn copy_surface(root: &Path, relative_path: &str) {
    let source = repo_root().join(relative_path);
    let destination = root.join(relative_path);
    fs::create_dir_all(destination.parent().unwrap()).unwrap();
    fs::copy(source, destination).unwrap();
}

fn build_release_fixture() -> tempfile::TempDir {
    let tempdir = tempfile::tempdir().unwrap();

    for relative_path in ["Cargo.toml", "README.md", "LICENSE", "CHANGELOG.md"] {
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

fn write_mcp_smoke_fixture_binary(path: &Path, non_json_stdout: bool) {
    let mut script = String::from(
        r#"#!/bin/sh
set -eu

if [ "${1:-}" = "--version" ]; then
  printf '1up smoke-fixture\n'
  exit 0
fi

if [ "${1:-}" = "mcp" ]; then
"#,
    );
    if non_json_stdout {
        script.push_str("  printf 'startup banner\\n'\n");
    }
    script.push_str(
        r#"  while IFS= read -r line; do
    case "$line" in
      *'"id":1'*'"method":"initialize"'*)
        printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"oneup-smoke-fixture","version":"0"}}}'
        ;;
      *'"id":2'*'"method":"tools/list"'*)
        printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"oneup_status"},{"name":"oneup_start"},{"name":"oneup_search"},{"name":"oneup_get"},{"name":"oneup_symbol"},{"name":"oneup_context"},{"name":"oneup_impact"},{"name":"oneup_structural"},{"name":"oneup_overview"}]}}'
        ;;
      *'"id":3'*'"method":"tools/call"'*'"oneup_status"'*)
        printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"structuredContent":{"status":"missing","summary":"Fixture repository needs indexing.","data":{"index_readable":false},"next_actions":[{"tool":"oneup_start","reason":"index fixture code","arguments":{"mode":"index_if_needed"}}]}}}'
        ;;
      *'"id":4'*'"method":"tools/call"'*'"oneup_start"'*'"mode":"index_if_needed"'*)
        printf '%s\n' '{"jsonrpc":"2.0","id":4,"result":{"structuredContent":{"status":"degraded","summary":"Fixture repository is indexed in FTS-only mode.","data":{"index_readable":true},"next_actions":[{"tool":"oneup_search","reason":"search indexed fixture code","arguments":{"query":"PolicyRuleValidator"}}]}}}'
        ;;
      *'"id":5'*'"method":"tools/call"'*'"oneup_search"'*'"query":"PolicyRuleValidator"'*'"limit":5'*)
        printf '%s\n' '{"jsonrpc":"2.0","id":5,"result":{"structuredContent":{"status":"degraded","summary":"Found fixture search results.","data":{"results":[{"handle":"abcdef123456","path":"src/policy.rs","line_start":1,"line_end":7,"score":96,"kind":"struct","breadcrumb":"PolicyRuleValidator","symbol":"PolicyRuleValidator"}]},"next_actions":[]}}}'
        ;;
      *'"id":5'*'"method":"tools/call"'*)
        printf '%s\n' '{"jsonrpc":"2.0","id":5,"error":{"code":-32602,"message":"unexpected search arguments"}}'
        ;;
      *'"id":6'*'"method":"tools/call"'*'"oneup_get"'*'"handles":[":abcdef123456"]'*)
        printf '%s\n' '{"jsonrpc":"2.0","id":6,"result":{"structuredContent":{"status":"ok","summary":"Hydrated fixture segment.","data":{"records":[{"status":"found","handle":":abcdef123456","segment":{"path":"src/policy.rs","line_start":1,"line_end":7,"content":"pub struct PolicyRuleValidator;\npub fn validate(&self, policy: &str) -> bool {"}}]},"next_actions":[]}}}'
        ;;
      *'"id":6'*'"method":"tools/call"'*)
        printf '%s\n' '{"jsonrpc":"2.0","id":6,"error":{"code":-32602,"message":"unexpected get arguments"}}'
        ;;
      *'"id":7'*'"method":"tools/call"'*'"oneup_symbol"'*'"name":"PolicyRuleValidator"'*'"include":"both"'*)
        printf '%s\n' '{"jsonrpc":"2.0","id":7,"result":{"structuredContent":{"status":"ok","summary":"Found fixture symbol evidence.","data":{"definitions":[{"path":"src/policy.rs","line":1,"handle":"abcdef123456"}],"references":[{"path":"src/runner.rs","line":1,"handle":"fedcba654321"}]},"next_actions":[]}}}'
        ;;
      *'"id":7'*'"method":"tools/call"'*)
        printf '%s\n' '{"jsonrpc":"2.0","id":7,"error":{"code":-32602,"message":"unexpected symbol arguments"}}'
        ;;
      *'"id":8'*'"method":"tools/call"'*'"oneup_context"'*'"locations":[{"path":"src/policy.rs","line":4,"expansion":2}]'*)
        printf '%s\n' '{"jsonrpc":"2.0","id":8,"result":{"structuredContent":{"status":"ok","summary":"Hydrated fixture file-line context.","data":{"records":[{"status":"found","location":{"path":"src/policy.rs","line":4},"context":{"path":"src/policy.rs","line_start":2,"line_end":6,"content":"pub fn validate(&self, policy: &str) -> bool {"}}]},"next_actions":[]}}}'
        ;;
      *'"id":8'*'"method":"tools/call"'*)
        printf '%s\n' '{"jsonrpc":"2.0","id":8,"error":{"code":-32602,"message":"unexpected context arguments"}}'
        ;;
      *'"id":9'*'"method":"tools/call"'*'"oneup_impact"'*'"handle":":abcdef123456"'*)
        printf '%s\n' '{"jsonrpc":"2.0","id":9,"result":{"structuredContent":{"status":"empty","summary":"No fixture impact results.","data":{"results":[]},"next_actions":[{"tool":"oneup_search","reason":"search for a narrower anchor","arguments":{"query":"PolicyRuleValidator"}}]}}}'
        ;;
      *'"id":10'*'"method":"tools/call"'*'"oneup_structural"'*)
        printf '%s\n' '{"jsonrpc":"2.0","id":10,"result":{"structuredContent":{"status":"ok","summary":"Found fixture structural matches.","data":{"results":[{"file_path":"src/policy.rs","language":"rust","pattern_name":"name","content":"PolicyRuleValidator","line_start":1,"line_end":1}]},"next_actions":[]}}}'
        ;;
      *'"id":11'*'"method":"tools/call"'*'"oneup_overview"'*)
        printf '%s\n' '{"jsonrpc":"2.0","id":11,"result":{"structuredContent":{"status":"ok","summary":"Indexed 2 file(s) and 5 segment(s) across 1 language(s); densest module is src; most-referenced type is PolicyRuleValidator.","data":{"status":"ok","stats":{"indexed_files":2,"total_segments":5,"languages":[{"language":"rust","files":2,"segments":5}]},"top_symbols":[{"name":"PolicyRuleValidator","handle":"abcdef123456","path":"src/policy.rs","line_start":1,"line_end":7,"referencing_files":1,"definition_count":1}],"modules":[{"module":"src","segments":5}],"module_dependencies":[],"entry_points":[{"handle":"abcdef123456","path":"src/policy.rs","line_start":1,"line_end":7,"role":"DEFINITION","symbol":"PolicyRuleValidator"}]},"next_actions":[{"tool":"oneup_symbol","reason":"Inspect the definition and references of the most-referenced type.","arguments":{"name":"PolicyRuleValidator"}},{"tool":"oneup_search","reason":"Start targeted discovery inside the densest module.","arguments":{"query":"src module responsibilities"}}]}}}'
        ;;
    esac
  done
  exit 0
fi

printf 'unsupported fixture invocation\n' >&2
exit 2
"#,
    );
    write_executable(path, &script);
}

fn write_mcp_smoke_tool_error_fixture_binary(path: &Path) {
    write_executable(
        path,
        r#"#!/bin/sh
set -eu

if [ "${1:-}" = "--version" ]; then
  printf '1up smoke-fixture\n'
  exit 0
fi

if [ "${1:-}" = "mcp" ]; then
  while IFS= read -r line; do
    case "$line" in
      *'"id":1'*'"method":"initialize"'*)
        printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"oneup-smoke-fixture","version":"0"}}}'
        ;;
      *'"id":2'*'"method":"tools/list"'*)
        printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"oneup_status"},{"name":"oneup_start"},{"name":"oneup_search"},{"name":"oneup_get"},{"name":"oneup_symbol"},{"name":"oneup_context"},{"name":"oneup_impact"},{"name":"oneup_structural"},{"name":"oneup_overview"}]}}'
        ;;
      *'"id":3'*'"method":"tools/call"'*'"oneup_status"'*)
        printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"structuredContent":{"status":"missing","summary":"Fixture repository needs indexing.","data":{"index_readable":false},"next_actions":[{"tool":"oneup_start","reason":"index fixture code","arguments":{"mode":"index_if_needed"}}]}}}'
        ;;
      *'"id":4'*'"method":"tools/call"'*'"oneup_start"'*)
        printf '%s\n' '{"jsonrpc":"2.0","id":4,"result":{"structuredContent":{"status":"degraded","summary":"Fixture repository is indexed in FTS-only mode.","data":{"index_readable":true},"next_actions":[{"tool":"oneup_search","reason":"search indexed fixture code","arguments":{"query":"PolicyRuleValidator"}}]}}}'
        ;;
      *'"id":5'*'"method":"tools/call"'*'"oneup_search"'*)
        printf '%s\n' '{"jsonrpc":"2.0","id":5,"result":{"isError":true,"structuredContent":{"status":"error","summary":"search failed","data":{},"next_actions":[]}}}'
        ;;
    esac
  done
  exit 0
fi

printf 'unsupported fixture invocation\n' >&2
exit 2
"#,
    );
}

fn write_release_artifacts(root: &Path, version: &str) -> PathBuf {
    let dist_dir = root.join("dist");
    fs::create_dir_all(&dist_dir).unwrap();

    let artifacts = [
        ("aarch64-apple-darwin", "macos", "arm64", "tar.gz", "Download the macOS arm64 archive from GitHub Releases and unpack with tar -xzf."),
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
        &format!(
            r#"#!/bin/sh
set -eu

if [ "${{1:-}}" = "--version" ]; then
  printf '1up {version} ({target})\n'
  exit 0
fi

if [ "${{1:-}}" = "mcp" ]; then
  while IFS= read -r line; do
    case "$line" in
      *'"id":1'*'"method":"initialize"'*)
        printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":"2025-11-25","capabilities":{{"tools":{{}}}},"serverInfo":{{"name":"oneup-fixture","version":"{version}"}}}}}}'
        ;;
      *'"id":2'*'"method":"tools/list"'*)
        printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"tools":[{{"name":"oneup_status"}},{{"name":"oneup_start"}},{{"name":"oneup_search"}},{{"name":"oneup_get"}},{{"name":"oneup_symbol"}},{{"name":"oneup_context"}},{{"name":"oneup_impact"}},{{"name":"oneup_structural"}},{{"name":"oneup_overview"}}]}}}}'
        ;;
      *'"id":3'*'"method":"tools/call"'*'"oneup_status"'*)
        printf '%s\n' '{{"jsonrpc":"2.0","id":3,"result":{{"structuredContent":{{"status":"missing","summary":"Fixture repository needs indexing.","data":{{"index_readable":false}},"next_actions":[{{"tool":"oneup_start","reason":"index fixture code","arguments":{{"mode":"index_if_needed"}}}}]}}}}}}'
        ;;
      *'"id":4'*'"method":"tools/call"'*'"oneup_start"'*)
        printf '%s\n' '{{"jsonrpc":"2.0","id":4,"result":{{"structuredContent":{{"status":"degraded","summary":"Fixture repository is indexed in FTS-only mode.","data":{{"index_readable":true}},"next_actions":[{{"tool":"oneup_search","reason":"search indexed fixture code","arguments":{{"query":"PolicyRuleValidator"}}}}]}}}}}}'
        ;;
      *'"id":5'*'"method":"tools/call"'*'"oneup_search"'*)
        printf '%s\n' '{{"jsonrpc":"2.0","id":5,"result":{{"structuredContent":{{"status":"degraded","summary":"Found fixture search results.","data":{{"results":[{{"handle":"abcdef123456","path":"src/policy.rs","line_start":1,"line_end":7,"score":96,"kind":"struct","breadcrumb":"PolicyRuleValidator","symbol":"PolicyRuleValidator"}}]}},"next_actions":[]}}}}}}'
        ;;
      *'"id":6'*'"method":"tools/call"'*'"oneup_get"'*)
        printf '%s\n' '{{"jsonrpc":"2.0","id":6,"result":{{"structuredContent":{{"status":"ok","summary":"Hydrated fixture segment.","data":{{"records":[{{"status":"found","handle":":abcdef123456","segment":{{"path":"src/policy.rs","line_start":1,"line_end":7,"content":"pub struct PolicyRuleValidator;\npub fn validate(&self, policy: &str) -> bool {{"}}}}]}},"next_actions":[]}}}}}}'
        ;;
      *'"id":7'*'"method":"tools/call"'*'"oneup_symbol"'*)
        printf '%s\n' '{{"jsonrpc":"2.0","id":7,"result":{{"structuredContent":{{"status":"ok","summary":"Found fixture symbol evidence.","data":{{"definitions":[{{"path":"src/policy.rs","line":1,"handle":"abcdef123456"}}],"references":[{{"path":"src/runner.rs","line":1,"handle":"fedcba654321"}}]}},"next_actions":[]}}}}}}'
        ;;
      *'"id":8'*'"method":"tools/call"'*'"oneup_context"'*)
        printf '%s\n' '{{"jsonrpc":"2.0","id":8,"result":{{"structuredContent":{{"status":"ok","summary":"Hydrated fixture file-line context.","data":{{"records":[{{"status":"found","location":{{"path":"src/policy.rs","line":4}},"context":{{"path":"src/policy.rs","line_start":2,"line_end":6,"content":"pub fn validate(&self, policy: &str) -> bool {{"}}}}]}},"next_actions":[]}}}}}}'
        ;;
      *'"id":9'*'"method":"tools/call"'*'"oneup_impact"'*)
        printf '%s\n' '{{"jsonrpc":"2.0","id":9,"result":{{"structuredContent":{{"status":"empty","summary":"No fixture impact results.","data":{{"results":[]}},"next_actions":[{{"tool":"oneup_search","reason":"search for a narrower anchor","arguments":{{"query":"PolicyRuleValidator"}}}}]}}}}}}'
        ;;
      *'"id":10'*'"method":"tools/call"'*'"oneup_structural"'*)
        printf '%s\n' '{{"jsonrpc":"2.0","id":10,"result":{{"structuredContent":{{"status":"ok","summary":"Found fixture structural matches.","data":{{"results":[{{"file_path":"src/policy.rs","language":"rust","pattern_name":"name","content":"PolicyRuleValidator","line_start":1,"line_end":1}}]}},"next_actions":[]}}}}}}'
        ;;
      *'"id":11'*'"method":"tools/call"'*'"oneup_overview"'*)
        printf '%s\n' '{{"jsonrpc":"2.0","id":11,"result":{{"structuredContent":{{"status":"ok","summary":"Indexed 2 file(s) and 5 segment(s) across 1 language(s); densest module is src; most-referenced type is PolicyRuleValidator.","data":{{"status":"ok","stats":{{"indexed_files":2,"total_segments":5,"languages":[{{"language":"rust","files":2,"segments":5}}]}},"top_symbols":[{{"name":"PolicyRuleValidator","handle":"abcdef123456","path":"src/policy.rs","line_start":1,"line_end":7,"referencing_files":1,"definition_count":1}}],"modules":[{{"module":"src","segments":5}}],"module_dependencies":[],"entry_points":[{{"handle":"abcdef123456","path":"src/policy.rs","line_start":1,"line_end":7,"role":"DEFINITION","symbol":"PolicyRuleValidator"}}]}},"next_actions":[{{"tool":"oneup_symbol","reason":"Inspect the definition and references of the most-referenced type.","arguments":{{"name":"PolicyRuleValidator"}}}},{{"tool":"oneup_search","reason":"Start targeted discovery inside the densest module.","arguments":{{"query":"src module responsibilities"}}}}]}}}}}}'
        ;;
    esac
  done
  exit 0
fi

printf 'unsupported fixture invocation\n' >&2
exit 2
"#
        ),
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

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const HOST_RELEASE_TARGET: Option<&str> = Some("aarch64-apple-darwin");
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
const HOST_RELEASE_TARGET: Option<&str> = None;
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const HOST_RELEASE_TARGET: Option<&str> = Some("aarch64-unknown-linux-gnu");
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const HOST_RELEASE_TARGET: Option<&str> = Some("x86_64-unknown-linux-gnu");
#[cfg(target_os = "windows")]
const HOST_RELEASE_TARGET: Option<&str> = Some("x86_64-pc-windows-msvc");

fn supported_host_release_target() -> Option<&'static str> {
    HOST_RELEASE_TARGET
}

#[test]
fn release_metadata_validation_passes_for_matching_tag_and_changelog() {
    let fixture_root = build_release_fixture();
    write_release_changelog(fixture_root.path(), TEST_RELEASE_VERSION);
    let release_tag = test_release_tag();

    let output = run_release_script(
        fixture_root.path(),
        "validate_release_metadata.sh",
        &[&release_tag],
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
    write_release_changelog(fixture_root.path(), TEST_RELEASE_VERSION);
    let mismatched_tag = next_patch_tag();

    let output = run_release_script(
        fixture_root.path(),
        "validate_release_metadata.sh",
        &[&mismatched_tag],
    );
    assert!(
        !output.status.success(),
        "validation unexpectedly passed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("does not match release tag"));
}

#[test]
fn release_metadata_validation_accepts_component_prefixed_tag_and_changelog() {
    let fixture_root = build_release_fixture();
    write_release_changelog(fixture_root.path(), TEST_RELEASE_VERSION);
    let release_tag = legacy_component_release_tag();

    let output = run_release_script(
        fixture_root.path(),
        "validate_release_metadata.sh",
        &[&release_tag],
    );
    assert!(
        output.status.success(),
        "validation unexpectedly failed for component-prefixed tag: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn release_manifest_generation_accepts_component_prefixed_tag() {
    let fixture_root = build_release_fixture();
    write_release_changelog(fixture_root.path(), TEST_RELEASE_VERSION);
    let dist_dir = write_release_artifacts(fixture_root.path(), TEST_RELEASE_VERSION);
    let release_tag = legacy_component_release_tag();

    let output = run_release_script(
        fixture_root.path(),
        "generate_release_manifest.sh",
        &[
            "--tag",
            &release_tag,
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
        output.status.success(),
        "manifest generation unexpectedly failed for component-prefixed tag: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(dist_dir.join("release-manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["version"], TEST_RELEASE_VERSION);
    assert_eq!(manifest["git_tag"], release_tag);
}

#[test]
fn release_manifest_generation_includes_platform_mapping_and_checksums() {
    let fixture_root = build_release_fixture();
    write_release_changelog(fixture_root.path(), TEST_RELEASE_VERSION);
    let dist_dir = write_release_artifacts(fixture_root.path(), TEST_RELEASE_VERSION);
    let release_tag = test_release_tag();

    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(dist_dir.join("release-manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["version"], TEST_RELEASE_VERSION);
    assert_eq!(manifest["git_tag"], release_tag);
    assert_eq!(manifest["commit_sha"], "abc123def456");
    assert_eq!(manifest["license"], "Apache-2.0");
    assert_eq!(manifest["binary_name"], "1up");
    assert_eq!(manifest["checksums_file"], "SHA256SUMS");
    assert_eq!(
        manifest["notes_source"],
        format!("CHANGELOG.md#[{TEST_RELEASE_VERSION}]")
    );
    assert_eq!(manifest["artifacts"].as_array().unwrap().len(), 4);
    assert_eq!(manifest["artifacts"][0]["target"], "aarch64-apple-darwin");
    assert_eq!(manifest["artifacts"][0]["arch"], "arm64");
    assert!(manifest["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .any(|artifact| artifact["target"] == "x86_64-pc-windows-msvc"));
    assert_eq!(
        manifest["channels"]["github_release"],
        format!("https://github.com/rp1-run/1up/releases/tag/{release_tag}")
    );
    assert_eq!(
        manifest["channels"]["script_install"],
        "https://1up.rp1.run/setup.sh"
    );
    assert_eq!(
        manifest["channels"]["update_manifest"],
        "https://raw.githubusercontent.com/rp1-run/1up/main/update-manifest.json"
    );
    assert!(manifest["channels"].get("homebrew_tap").is_none());
    assert!(manifest["channels"].get("scoop_bucket").is_none());
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
    let expiry = manifest["expiry"]
        .as_str()
        .expect("manifest should include a non-null expiry string");
    assert!(
        expiry.ends_with('Z') && expiry.contains('T'),
        "expiry should be ISO 8601 UTC: {expiry}"
    );
    assert!(
        expiry > published_at,
        "expiry ({expiry}) should be after published_at ({published_at})"
    );
    assert_eq!(
        manifest["notes_url"],
        format!("https://github.com/rp1-run/1up/releases/tag/{release_tag}")
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
            url.starts_with(&format!(
                "https://github.com/rp1-run/1up/releases/download/{release_tag}/"
            )) && url.ends_with(artifact["archive"].as_str().unwrap())
        }));
}

#[test]
fn release_manifest_deserializes_as_update_manifest() {
    let fixture_root = build_release_fixture();
    write_release_changelog(fixture_root.path(), TEST_RELEASE_VERSION);
    let dist_dir = write_release_artifacts(fixture_root.path(), TEST_RELEASE_VERSION);

    let raw = fs::read(dist_dir.join("release-manifest.json")).unwrap();
    let manifest: oneup::shared::update::UpdateManifest = serde_json::from_slice(&raw)
        .expect("release manifest should deserialize as UpdateManifest");

    assert_eq!(manifest.version, TEST_RELEASE_VERSION);
    assert_eq!(manifest.git_tag, test_release_tag());
    assert!(!manifest.published_at.is_empty());
    let expiry = manifest
        .expiry
        .as_deref()
        .expect("generated manifest should carry a non-null expiry");
    assert!(
        expiry.ends_with('Z') && expiry.contains('T'),
        "expiry should be RFC3339 UTC: {expiry}"
    );
    // RFC3339 UTC `Z` timestamps sort lexicographically in chronological order,
    // so a string comparison is a valid "expiry is after published_at" check.
    assert!(
        expiry > manifest.published_at.as_str(),
        "expiry ({expiry}) should be after published_at ({})",
        manifest.published_at
    );
    assert!(manifest
        .notes_url
        .contains(&format!("/releases/tag/{}", test_release_tag())));
    assert_eq!(manifest.artifacts.len(), 4);
    assert!(!manifest.yanked);
    assert!(manifest.minimum_safe_version.is_none());
    assert!(manifest.message.is_none());
    assert_eq!(
        manifest.channels.script_install.as_deref(),
        Some("https://1up.rp1.run/setup.sh")
    );
    assert_eq!(
        manifest.channels.update_manifest.as_deref(),
        Some("https://raw.githubusercontent.com/rp1-run/1up/main/update-manifest.json")
    );

    for artifact in &manifest.artifacts {
        assert!(!artifact.target.is_empty());
        assert!(!artifact.archive.is_empty());
        assert_eq!(artifact.sha256.len(), 64);
        assert!(
            artifact.url.contains(&artifact.archive),
            "artifact url should contain archive name"
        );
    }

    let update_manifest_path = dist_dir.join("update-manifest.json");
    let write_output = run_release_script(
        fixture_root.path(),
        "write_update_manifest.sh",
        &[
            "--input",
            dist_dir.join("release-manifest.json").to_str().unwrap(),
            "--output",
            update_manifest_path.to_str().unwrap(),
        ],
    );
    assert!(
        write_output.status.success(),
        "write_update_manifest.sh unexpectedly failed: {}",
        String::from_utf8_lossy(&write_output.stderr)
    );

    let update_raw = fs::read(&update_manifest_path).unwrap();
    let update_manifest: oneup::shared::update::UpdateManifest =
        serde_json::from_slice(&update_raw)
            .expect("write_update_manifest.sh output should deserialize as UpdateManifest");

    assert_eq!(update_manifest.version, manifest.version);
    assert_eq!(update_manifest.git_tag, manifest.git_tag);
    assert_eq!(update_manifest.published_at, manifest.published_at);
    assert_eq!(update_manifest.expiry, manifest.expiry);
    assert_eq!(update_manifest.notes_url, manifest.notes_url);
    assert_eq!(update_manifest.yanked, manifest.yanked);
    assert_eq!(
        update_manifest.minimum_safe_version,
        manifest.minimum_safe_version
    );
    assert_eq!(update_manifest.message, manifest.message);
    assert_eq!(
        update_manifest.channels.github_release,
        manifest.channels.github_release
    );
    assert_eq!(
        update_manifest.channels.script_install,
        manifest.channels.script_install
    );
    assert_eq!(
        update_manifest.channels.update_manifest,
        manifest.channels.update_manifest
    );
    assert_eq!(update_manifest.artifacts.len(), manifest.artifacts.len());
    for (projected, source) in update_manifest
        .artifacts
        .iter()
        .zip(manifest.artifacts.iter())
    {
        assert_eq!(projected.target, source.target);
        assert_eq!(projected.archive, source.archive);
        assert_eq!(projected.sha256, source.sha256);
        assert_eq!(projected.url, source.url);
    }

    // The projection contract drops release-generation-only fields (commit_sha,
    // binary_name, license, checksums_file, notes_source) that are not part of
    // the client-facing update manifest.
    let update_value: serde_json::Value = serde_json::from_slice(&update_raw).unwrap();
    for dropped_field in [
        "commit_sha",
        "binary_name",
        "license",
        "checksums_file",
        "notes_source",
    ] {
        assert!(
            update_value.get(dropped_field).is_none(),
            "update-manifest.json projection must drop {dropped_field}"
        );
    }
}

/// Guards that the committed repo-root `update-manifest.json` never advertises a
/// version *ahead* of the binary it describes. The manifest is the client's
/// source of truth for "what version is available"; if its `version` ever
/// exceeds the shipped binary version, clients are told a version exists that the
/// binary cannot be — an ahead/typo'd version is the real drift bug to catch.
///
/// The invariant is `manifest.version <= CARGO_PKG_VERSION` (semver), not strict
/// equality, because the manifest is allowed to *lag* by a release during a
/// publish window: release-please bumps `Cargo.toml` in the release PR (so
/// `push:main` already carries the new `CARGO_PKG_VERSION`), but
/// `publish-update-manifest.yml` commits `update-manifest.json` separately on
/// `release:published`. Between those two events there is a guaranteed window
/// where `manifest.version` is one release behind the binary; a strict `==`
/// would hard-fail (red-X) CI on every release during that window. The
/// stuck-behind case (manifest never catching up) is covered at publish time by
/// the `verify-update-manifest` job, so this test only forbids the
/// ahead-of-binary direction.
///
/// The binary version is sourced from `CARGO_PKG_VERSION` (via
/// `TEST_RELEASE_VERSION`), never a hardcoded literal, so a normal version bump
/// that updates both `Cargo.toml` and the manifest keeps this green with no test
/// edit. This complements
/// `release_manifest_generation_includes_platform_mapping_and_checksums` and
/// `release_manifest_deserializes_as_update_manifest`, which only validate a
/// freshly generated fixture manifest, by asserting against the committed
/// manifest clients actually fetch. A failing `cargo test` fails CI, so this is
/// hard-fail enforcement with no warn-only path.
#[test]
fn committed_update_manifest_version_not_ahead_of_binary() {
    use semver::Version;

    let manifest_path = repo_root().join("update-manifest.json");
    let raw = fs::read(&manifest_path).unwrap_or_else(|err| {
        panic!(
            "failed to read committed manifest at {}: {err}",
            manifest_path.display()
        )
    });
    let manifest: oneup::shared::update::UpdateManifest = serde_json::from_slice(&raw)
        .expect("committed update-manifest.json should deserialize as UpdateManifest");

    let manifest_version = Version::parse(&manifest.version).unwrap_or_else(|err| {
        panic!(
            "committed update-manifest.json version `{}` is not valid semver: {err}",
            manifest.version
        )
    });
    let binary_version = Version::parse(TEST_RELEASE_VERSION).unwrap_or_else(|err| {
        panic!("binary CARGO_PKG_VERSION `{TEST_RELEASE_VERSION}` is not valid semver: {err}")
    });

    assert!(
        manifest_version <= binary_version,
        "committed update-manifest.json version `{}` is AHEAD of the binary version \
         `{}` (CARGO_PKG_VERSION). The manifest must never advertise a version the \
         binary cannot be: the invariant is manifest.version <= CARGO_PKG_VERSION. \
         Lagging by one release during the publish window is allowed (release-please \
         bumps Cargo.toml on push:main while publish-update-manifest.yml commits the \
         manifest later on release:published; the publish-time verify-update-manifest \
         job covers the stuck-behind case), but an ahead/typo'd manifest version is real \
         drift — fix update-manifest.json `version`",
        manifest.version,
        TEST_RELEASE_VERSION
    );
}

#[test]
fn mcp_installation_docs_keep_script_installer_and_manual_mcp_guidance() {
    let guide = fs::read_to_string(repo_root().join("docs/mcp-installation.md")).unwrap();
    let readme = fs::read_to_string(repo_root().join("README.md")).unwrap();

    for required in [
        "# MCP Installation Reference",
        "## Server Entry",
        "## Host Config Shapes",
        "## After Saving Config",
        "## Troubleshooting",
        "## Safety",
        "1up mcp",
        "Server identity: `oneup`",
        "Command: `1up`",
        "Preferred project/workspace args: `[\"mcp\"]`",
        "Use `--path` only for user-global or static config",
        "### Codex",
        "### Claude Code, Cursor, And Generic MCP JSON Clients",
        "### VS Code And Copilot",
        "[mcp_servers.oneup]",
        "\"mcpServers\"",
        "\"servers\"",
        "args = [\"mcp\"]",
        "\"args\": [\"mcp\"]",
        "args = [\"mcp\", \"--path\", \"/absolute/path/to/repo\"]",
        "\"args\": [\"mcp\", \"--path\", \"/absolute/path/to/repo\"]",
        "List MCP tools and call `oneup_status`.",
        "MCP stdio expects protocol messages on stdout.",
        "It does not edit files",
        "execute arbitrary shell commands",
        "mutate host config",
    ] {
        assert!(
            guide.contains(required),
            "MCP installation guide is missing required text: {required}"
        );
    }

    for required in [
        "curl -fsSL https://1up.rp1.run/setup.sh | bash",
        "Configure 1up MCP for this repository.",
        "Configure the `oneup` MCP server in project/workspace scope.",
        "Use explicit `--path` config only when project/workspace config is not available.",
        "Do not try to restart this active host or verify newly added MCP tools from it.",
        "ask the user to restart/reload this host so it can load `oneup`",
        "Option 2: Install 1up Yourself",
        "Option 3: Manual MCP Config",
        "Server name: `oneup`",
        "Command: `1up`",
        "Args: `[\"mcp\"]`",
        "[docs/mcp-installation.md](docs/mcp-installation.md)",
        "`--plain` is only for shell scripts and terminal automation",
        "Agents should use the `oneup_*` MCP tools",
    ] {
        assert!(
            readme.contains(required),
            "README is missing required MCP setup text: {required}"
        );
    }

    for required_tool in [
        "oneup_status",
        "oneup_start",
        "oneup_search",
        "oneup_get",
        "oneup_symbol",
        "oneup_context",
        "oneup_impact",
        "oneup_structural",
    ] {
        assert!(
            guide.contains(required_tool),
            "MCP installation guide is missing retained tool: {required_tool}"
        );
        assert!(
            readme.contains(required_tool),
            "README is missing retained tool: {required_tool}"
        );
    }

    for unsupported in [
        "1up add-mcp",
        "add-mcp",
        "Run Wrapper-First Setup",
        "wrapper-first",
        "bunx",
        "npx",
        "Homebrew",
        "Scoop",
        "hello-agent",
        "oneup_prepare",
        "oneup_read",
        "1up mcp-install",
        "1up mcp install",
        "src/mcp/install",
    ] {
        assert!(
            !guide.contains(unsupported),
            "MCP installation guide should not document unsupported setup/package path {unsupported}"
        );
        assert!(
            !readme.contains(unsupported),
            "README should not document unsupported setup/package path {unsupported}"
        );
    }

    for paste_recommendation in [
        "## Repository Instruction Hint",
        "For code-discovery questions in this repo, use the `oneup` MCP tools before broad raw search.",
        "Insert this minimal 1up hint",
        "repo instruction file changed",
    ] {
        assert!(
            !guide.contains(paste_recommendation),
            "MCP installation guide should no longer recommend pasting a 1up hint into a user instruction file: {paste_recommendation}"
        );
        assert!(
            !readme.contains(paste_recommendation),
            "README should no longer recommend pasting a 1up hint into a user instruction file: {paste_recommendation}"
        );
    }
}

/// Extract every `oneup_[a-z_]+` token from `content`, mirroring regex
/// find_iter semantics (a literal `oneup_` prefix followed by a maximal run of
/// `[a-z_]`, scanning left-to-right with non-overlapping matches). Kept as a
/// plain byte scan to avoid adding a `regex` dependency just for one guard.
fn extract_oneup_tokens(content: &str) -> Vec<String> {
    const PREFIX: &str = "oneup_";
    let bytes = content.as_bytes();
    let mut tokens = Vec::new();
    let mut search_from = 0;
    while let Some(rel) = content[search_from..].find(PREFIX) {
        let start = search_from + rel;
        let mut end = start + PREFIX.len();
        while end < bytes.len() && (bytes[end].is_ascii_lowercase() || bytes[end] == b'_') {
            end += 1;
        }
        if end > start + PREFIX.len() {
            tokens.push(content[start..end].to_string());
            search_from = end;
        } else {
            search_from = start + PREFIX.len();
        }
    }
    tokens
}

/// Every MCP tool name an agent can read in the repo's own instruction and doc
/// surfaces must be a currently-retained tool. The authority is the live
/// `RETAINED_PUBLIC_TOOLS` list, never a hardcoded duplicate, so adding or
/// removing a real tool keeps this guard correct. Catches the agent-hint drift
/// class (e.g. `oneup_prepare`/`oneup_read`) returning on any future doc edit.
#[test]
fn documentation_tool_names_match_retained_public_tools() {
    let scanned_docs = [
        "AGENTS.md",
        "CLAUDE.md",
        "DEVELOPMENT.md",
        "README.md",
        "docs/mcp-installation.md",
        "evals/README.md",
    ];

    for doc in scanned_docs {
        let content = fs::read_to_string(repo_root().join(doc))
            .unwrap_or_else(|err| panic!("failed to read scanned doc {doc}: {err}"));
        for token in extract_oneup_tokens(&content) {
            assert!(
                RETAINED_PUBLIC_TOOLS.contains(&token.as_str()),
                "{doc} references unknown MCP tool `{token}` not in RETAINED_PUBLIC_TOOLS \
                 (authority: src/mcp/types.rs); correct the doc or update the retained tool set"
            );
        }
    }
}

#[test]
fn verify_mcp_smoke_lists_tools_and_readiness() {
    let fixture_root = build_release_fixture();
    let repo_path = fixture_root.path().join("repo");
    let binary_path = fixture_root.path().join("fixture-1up");
    let output_path = fixture_root.path().join("mcp-smoke.json");
    fs::create_dir_all(&repo_path).unwrap();
    write_mcp_smoke_fixture_binary(&binary_path, false);

    let output = run_release_script(
        fixture_root.path(),
        "verify_mcp_smoke.sh",
        &[
            "--binary",
            binary_path.to_str().unwrap(),
            "--repo",
            repo_path.to_str().unwrap(),
            "--output",
            output_path.to_str().unwrap(),
        ],
    );
    assert!(
        output.status.success(),
        "MCP smoke unexpectedly failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let evidence: serde_json::Value =
        serde_json::from_slice(&fs::read(output_path).unwrap()).unwrap();
    assert_eq!(evidence["status"], "passed");
    assert_eq!(evidence["schema"], "mcp_smoke.v2");
    assert_eq!(evidence["version"], "1up smoke-fixture");
    assert_eq!(evidence["readiness_status"], "degraded");
    assert_eq!(evidence["stdout_protocol_clean"], true);
    assert_eq!(evidence["presentation_free"], true);
    assert_eq!(evidence["discovery_flow"]["status"], "passed");
    assert_eq!(evidence["response_statuses"]["status"], "missing");
    assert_eq!(evidence["response_statuses"]["start"], "degraded");
    assert_eq!(evidence["response_statuses"]["search"], "degraded");
    assert_eq!(evidence["response_statuses"]["get"], "ok");
    assert_eq!(evidence["response_statuses"]["symbol"], "ok");
    assert_eq!(evidence["response_statuses"]["context"], "ok");
    assert_eq!(evidence["response_statuses"]["impact"], "empty");
    assert_eq!(evidence["response_statuses"]["structural"], "ok");
    assert_eq!(evidence["response_statuses"]["overview"], "ok");
    assert_eq!(evidence["server_command"][1], "mcp");
    assert_eq!(evidence["server_command"][2], "--path");
    assert_eq!(
        evidence["server_command"][3],
        repo_path.canonicalize().unwrap().to_str().unwrap()
    );
    let tools = evidence["tools"].as_array().unwrap();
    for expected_tool in [
        "oneup_status",
        "oneup_start",
        "oneup_search",
        "oneup_get",
        "oneup_symbol",
        "oneup_context",
        "oneup_impact",
        "oneup_structural",
        "oneup_overview",
    ] {
        assert!(
            tools.iter().any(|tool| tool == expected_tool),
            "MCP smoke should include {expected_tool}"
        );
    }
    let exercised_tools = evidence["exercised_tools"].as_array().unwrap();
    for expected_tool in [
        "oneup_status",
        "oneup_start",
        "oneup_search",
        "oneup_get",
        "oneup_symbol",
        "oneup_context",
        "oneup_impact",
        "oneup_structural",
        "oneup_overview",
    ] {
        assert!(
            exercised_tools.iter().any(|tool| tool == expected_tool),
            "MCP smoke should exercise {expected_tool}"
        );
    }
    let calls = evidence["tool_calls"].as_array().unwrap();
    for expected_label in [
        "status",
        "start",
        "search",
        "get",
        "symbol",
        "context",
        "impact",
        "structural",
        "overview",
    ] {
        assert!(
            calls.iter().any(|call| call["label"] == expected_label
                && call["structured_content"] == true
                && call["presentation_free"] == true),
            "MCP smoke should record a structured presentation-free {expected_label} call"
        );
    }
}

#[test]
fn verify_mcp_smoke_rejects_non_json_stdout() {
    let fixture_root = build_release_fixture();
    let repo_path = fixture_root.path().join("repo");
    let binary_path = fixture_root.path().join("fixture-1up");
    let output_path = fixture_root.path().join("mcp-smoke.json");
    fs::create_dir_all(&repo_path).unwrap();
    write_mcp_smoke_fixture_binary(&binary_path, true);

    let output = run_release_script(
        fixture_root.path(),
        "verify_mcp_smoke.sh",
        &[
            "--binary",
            binary_path.to_str().unwrap(),
            "--repo",
            repo_path.to_str().unwrap(),
            "--output",
            output_path.to_str().unwrap(),
        ],
    );
    assert!(
        !output.status.success(),
        "MCP smoke unexpectedly accepted non-JSON stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );

    let evidence: serde_json::Value =
        serde_json::from_slice(&fs::read(output_path).unwrap()).unwrap();
    assert_eq!(evidence["status"], "failed");
    assert_eq!(evidence["stdout_protocol_clean"], false);
    let diagnostics = evidence["diagnostics"].as_array().unwrap();
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.as_str().unwrap().contains("non-JSON stdout")),
        "expected non-JSON stdout diagnostic, got {diagnostics:?}"
    );
}

#[test]
fn verify_mcp_smoke_rejects_tool_error_results() {
    let fixture_root = build_release_fixture();
    let repo_path = fixture_root.path().join("repo");
    let binary_path = fixture_root.path().join("fixture-1up");
    let output_path = fixture_root.path().join("mcp-smoke.json");
    fs::create_dir_all(&repo_path).unwrap();
    write_mcp_smoke_tool_error_fixture_binary(&binary_path);

    let output = run_release_script(
        fixture_root.path(),
        "verify_mcp_smoke.sh",
        &[
            "--binary",
            binary_path.to_str().unwrap(),
            "--repo",
            repo_path.to_str().unwrap(),
            "--output",
            output_path.to_str().unwrap(),
        ],
    );
    assert!(
        !output.status.success(),
        "MCP smoke unexpectedly accepted a tool error result: {}",
        String::from_utf8_lossy(&output.stdout)
    );

    let evidence: serde_json::Value =
        serde_json::from_slice(&fs::read(output_path).unwrap()).unwrap();
    assert_eq!(evidence["status"], "failed");
    let diagnostics = evidence["diagnostics"].as_array().unwrap();
    assert!(
        diagnostics.iter().any(|diagnostic| diagnostic
            .as_str()
            .unwrap()
            .contains("search returned tool error result")),
        "MCP smoke should explain that required tool errors are rejected: {diagnostics:?}"
    );
}

#[test]
fn archive_verification_confirms_expected_release_contents() {
    let Some(host_release_target) = supported_host_release_target() else {
        return;
    };
    let fixture_root = build_release_fixture();
    write_release_changelog(fixture_root.path(), TEST_RELEASE_VERSION);
    let dist_dir = write_verifiable_release_artifacts(fixture_root.path(), TEST_RELEASE_VERSION);
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
            host_release_target,
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
    assert_eq!(verification["archives"][0]["target"], host_release_target);
    assert_eq!(
        verification["archives"][0]["verified_contents"]["license"],
        format!("1up-v{TEST_RELEASE_VERSION}-{host_release_target}/LICENSE")
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
        .contains(TEST_RELEASE_VERSION));
    assert_eq!(
        verification["archives"][0]["mcp_smoke_test"]["status"],
        "passed"
    );
    assert_eq!(
        verification["archives"][0]["mcp_smoke_test"]["readiness_status"],
        "degraded"
    );
    assert_eq!(
        verification["archives"][0]["mcp_smoke_test"]["stdout_protocol_clean"],
        true
    );
    assert_eq!(
        verification["archives"][0]["mcp_smoke_test"]["presentation_free"],
        true
    );
    assert_eq!(
        verification["archives"][0]["mcp_smoke_test"]["discovery_flow"]["status"],
        "passed"
    );
    let tools = verification["archives"][0]["mcp_smoke_test"]["tools"]
        .as_array()
        .unwrap();
    for expected_tool in [
        "oneup_status",
        "oneup_start",
        "oneup_search",
        "oneup_get",
        "oneup_symbol",
        "oneup_context",
        "oneup_impact",
        "oneup_structural",
        "oneup_overview",
    ] {
        assert!(
            tools.iter().any(|tool| tool == expected_tool),
            "archive MCP smoke should include {expected_tool}"
        );
    }
}

#[test]
fn mcp_host_smoke_recorder_writes_manual_and_skipped_hosts() {
    let fixture_root = build_release_fixture();
    let repo_path = fixture_root.path().join("repo");
    let output_path = fixture_root.path().join("mcp-host-smoke.json");
    fs::create_dir_all(&repo_path).unwrap();

    let recorded_output = run_release_script(
        fixture_root.path(),
        "record_mcp_host_smoke.sh",
        &[
            "--output",
            output_path.to_str().unwrap(),
            "--host",
            "codex",
            "--status",
            "recorded",
            "--host-version",
            "codex-cli 0.125.0",
            "--setup-mode",
            "manual",
            "--repo",
            repo_path.to_str().unwrap(),
            "--tool",
            "oneup_status",
            "--tool",
            "oneup_start",
            "--tool",
            "oneup_search",
            "--tool",
            "oneup_get",
            "--tool",
            "oneup_context",
            "--tool",
            "oneup_structural",
            "--readiness",
            "ready",
            "--discovery-flow",
            "passed",
        ],
    );
    assert!(
        recorded_output.status.success(),
        "recorded host smoke unexpectedly failed: {}",
        String::from_utf8_lossy(&recorded_output.stderr)
    );

    let blocked_output = run_release_script(
        fixture_root.path(),
        "record_mcp_host_smoke.sh",
        &[
            "--output",
            output_path.to_str().unwrap(),
            "--host",
            "generic",
            "--status",
            "recorded",
            "--host-version",
            "generic-mcp 1.0.0",
            "--setup-mode",
            "manual",
            "--repo",
            repo_path.to_str().unwrap(),
            "--tool",
            "oneup_status",
            "--readiness",
            "blocked",
            "--discovery-flow",
            "failed",
        ],
    );
    assert!(
        blocked_output.status.success(),
        "blocked host smoke unexpectedly failed: {}",
        String::from_utf8_lossy(&blocked_output.stderr)
    );

    let skipped_output = run_release_script(
        fixture_root.path(),
        "record_mcp_host_smoke.sh",
        &[
            "--output",
            output_path.to_str().unwrap(),
            "--host",
            "cursor",
            "--status",
            "skipped",
            "--setup-mode",
            "skipped",
            "--reason",
            "Cursor approval flow was unavailable in the maintainer environment.",
        ],
    );
    assert!(
        skipped_output.status.success(),
        "skipped host smoke unexpectedly failed: {}",
        String::from_utf8_lossy(&skipped_output.stderr)
    );

    let evidence: serde_json::Value =
        serde_json::from_slice(&fs::read(output_path).unwrap()).unwrap();
    assert_eq!(evidence["schema"], "mcp_host_smoke.v1");
    assert_eq!(evidence["hosts"]["codex"]["status"], "recorded");
    assert_eq!(
        evidence["hosts"]["codex"]["host_version"],
        "codex-cli 0.125.0"
    );
    assert_eq!(evidence["hosts"]["codex"]["setup_mode"], "manual");
    assert_eq!(
        evidence["hosts"]["codex"]["repo_path"],
        repo_path.to_str().unwrap()
    );
    assert_eq!(evidence["hosts"]["codex"]["tools_listed"], true);
    assert_eq!(evidence["hosts"]["codex"]["readiness"], "ready");
    assert_eq!(evidence["hosts"]["codex"]["discovery_flow"], "passed");
    let tools = evidence["hosts"]["codex"]["tools"].as_array().unwrap();
    assert!(tools.iter().any(|tool| tool == "oneup_status"));
    assert!(tools.iter().any(|tool| tool == "oneup_start"));
    assert!(tools.iter().any(|tool| tool == "oneup_search"));
    assert!(tools.iter().any(|tool| tool == "oneup_get"));
    assert!(tools.iter().any(|tool| tool == "oneup_context"));
    assert!(tools.iter().any(|tool| tool == "oneup_structural"));

    assert_eq!(evidence["hosts"]["generic"]["status"], "recorded");
    assert_eq!(evidence["hosts"]["generic"]["readiness"], "blocked");
    assert_eq!(evidence["hosts"]["generic"]["discovery_flow"], "failed");

    assert_eq!(evidence["hosts"]["cursor"]["status"], "skipped");
    assert_eq!(evidence["hosts"]["cursor"]["setup_mode"], "skipped");
    assert_eq!(
        evidence["hosts"]["cursor"]["reason"],
        "Cursor approval flow was unavailable in the maintainer environment."
    );
}

#[test]
fn mcp_host_smoke_recorder_requires_skip_reason() {
    let fixture_root = build_release_fixture();
    let output_path = fixture_root.path().join("mcp-host-smoke.json");

    let output = run_release_script(
        fixture_root.path(),
        "record_mcp_host_smoke.sh",
        &[
            "--output",
            output_path.to_str().unwrap(),
            "--host",
            "generic",
            "--status",
            "skipped",
            "--setup-mode",
            "skipped",
        ],
    );
    assert!(
        !output.status.success(),
        "host smoke unexpectedly accepted a skipped record without a reason: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("skipped host smoke evidence requires --reason"));
}

#[test]
fn mcp_host_smoke_recorder_rejects_removed_setup_modes() {
    let fixture_root = build_release_fixture();
    let repo_path = fixture_root.path().join("repo");
    let output_path = fixture_root.path().join("mcp-host-smoke.json");
    fs::create_dir_all(&repo_path).unwrap();

    let output = run_release_script(
        fixture_root.path(),
        "record_mcp_host_smoke.sh",
        &[
            "--output",
            output_path.to_str().unwrap(),
            "--host",
            "claude-code",
            "--status",
            "recorded",
            "--host-version",
            "claude-code 1.0.0",
            "--setup-mode",
            "wrapper",
            "--repo",
            repo_path.to_str().unwrap(),
            "--tool",
            "oneup_status",
            "--readiness",
            "ready",
            "--discovery-flow",
            "passed",
        ],
    );
    assert!(
        !output.status.success(),
        "host smoke unexpectedly accepted removed setup mode: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("unsupported setup mode: wrapper"));
}

#[test]
fn release_evidence_supports_explicit_skipped_eval_reason() {
    let Some(host_release_target) = supported_host_release_target() else {
        return;
    };
    let fixture_root = build_release_fixture();
    write_release_changelog(fixture_root.path(), TEST_RELEASE_VERSION);
    let dist_dir = write_verifiable_release_artifacts(fixture_root.path(), TEST_RELEASE_VERSION);
    let merge_gate_path = dist_dir.join("merge-gate.json");
    let security_check_path = dist_dir.join("security-check.json");
    let benchmark_summary_path = dist_dir.join("benchmark-summary.json");
    let archive_verification_path = dist_dir.join("archive-verification.json");
    let mcp_host_smoke_path = dist_dir.join("mcp-host-smoke.json");
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
    fs::write(
        &mcp_host_smoke_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "generated_at": "2026-04-09T00:00:00Z",
            "schema": "mcp_host_smoke.v1",
            "hosts": {
                "generic": {
                    "status": "recorded",
                    "host": "generic",
                    "host_version": "generic-mcp 1.0.0",
                    "setup_mode": "manual",
                    "repo_path": "/tmp/blocked-repo",
                    "tools_listed": true,
                    "tools": ["oneup_status"],
                    "readiness": "blocked",
                    "discovery_flow": "failed",
                    "recorded_at": "2026-04-09T00:00:00Z"
                }
            }
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
            host_release_target,
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
            "--mcp-host-smoke",
            mcp_host_smoke_path.to_str().unwrap(),
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
    assert_eq!(evidence["version"], TEST_RELEASE_VERSION);
    assert_eq!(evidence["git_tag"], test_release_tag());
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
    assert_eq!(evidence["mcp_host_smoke"]["status"], "recorded");
    assert_eq!(
        evidence["mcp_host_smoke"]["hosts"]["generic"]["readiness"],
        "blocked"
    );
    assert_eq!(evidence["distribution"]["status"], "recorded");
    assert_eq!(
        evidence["distribution"]["script_install"],
        "https://1up.rp1.run/setup.sh"
    );
    assert_eq!(
        evidence["distribution"]["update_manifest"],
        "https://raw.githubusercontent.com/rp1-run/1up/main/update-manifest.json"
    );
    assert!(evidence.get("packages").is_none());
}

#[test]
fn release_evidence_rejects_missing_eval_reference_and_skip_reason() {
    let Some(host_release_target) = supported_host_release_target() else {
        return;
    };
    let fixture_root = build_release_fixture();
    write_release_changelog(fixture_root.path(), TEST_RELEASE_VERSION);
    let dist_dir = write_verifiable_release_artifacts(fixture_root.path(), TEST_RELEASE_VERSION);
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
            host_release_target,
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
    let Some(host_release_target) = supported_host_release_target() else {
        return;
    };
    let fixture_root = build_release_fixture();
    write_release_changelog(fixture_root.path(), TEST_RELEASE_VERSION);
    let dist_dir = write_verifiable_release_artifacts(fixture_root.path(), TEST_RELEASE_VERSION);
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
                "target": host_release_target,
                "archive": format!(
                    "1up-v{TEST_RELEASE_VERSION}-{host_release_target}.{}",
                    if host_release_target.contains("windows") { "zip" } else { "tar.gz" }
                ),
                "sha256": "abc123",
                "verified_contents": {
                    "binary": "1up",
                    "license": "LICENSE",
                    "readme": "README.txt"
                },
                "smoke_test": {
                    "status": "passed",
                    "command": "1up --version",
                    "output": "1up fixture"
                }
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

#[test]
fn release_evidence_rejects_smoke_results_missing_overview_label() {
    let Some(host_release_target) = supported_host_release_target() else {
        return;
    };
    let fixture_root = build_release_fixture();
    write_release_changelog(fixture_root.path(), TEST_RELEASE_VERSION);
    let dist_dir = write_verifiable_release_artifacts(fixture_root.path(), TEST_RELEASE_VERSION);
    let merge_gate_path = dist_dir.join("merge-gate.json");
    let security_check_path = dist_dir.join("security-check.json");
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

    // Smoke evidence covering only the original eight per-call labels: the
    // nine-tool gate must reject it because oneup_overview was never
    // independently re-verified.
    let eight_labels = [
        "status",
        "start",
        "search",
        "get",
        "symbol",
        "context",
        "impact",
        "structural",
    ];
    let nine_tools = [
        "oneup_status",
        "oneup_start",
        "oneup_search",
        "oneup_get",
        "oneup_symbol",
        "oneup_context",
        "oneup_impact",
        "oneup_structural",
        "oneup_overview",
    ];
    let tool_calls: Vec<serde_json::Value> = eight_labels
        .iter()
        .map(|label| {
            serde_json::json!({
                "label": label,
                "name": format!("oneup_{label}"),
                "status": "ok",
                "structured_content": true,
                "presentation_free": true
            })
        })
        .collect();
    let structured_content_present: serde_json::Value = eight_labels
        .iter()
        .map(|label| (label.to_string(), serde_json::Value::Bool(true)))
        .collect::<serde_json::Map<String, serde_json::Value>>()
        .into();
    fs::write(
        &archive_verification_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "generated_at": "2026-04-09T00:00:00Z",
            "manifest_asset": "release-manifest.json",
            "checksums_asset": "SHA256SUMS",
            "archive_count": 1,
            "archives": [{
                "target": host_release_target,
                "archive": format!(
                    "1up-v{TEST_RELEASE_VERSION}-{host_release_target}.{}",
                    if host_release_target.contains("windows") { "zip" } else { "tar.gz" }
                ),
                "sha256": "abc123",
                "verified_contents": {
                    "binary": "1up",
                    "license": "LICENSE",
                    "readme": "README.txt"
                },
                "smoke_test": {
                    "status": "passed",
                    "command": "1up --version",
                    "output": "1up fixture"
                },
                "mcp_smoke_test": {
                    "schema": "mcp_smoke.v2",
                    "status": "passed",
                    "binary": "1up",
                    "version": "1up fixture",
                    "server_command": ["1up", "mcp", "--path", "/tmp/repo"],
                    "tools": nine_tools,
                    "exercised_tools": nine_tools,
                    "tool_calls": tool_calls,
                    "structured_content_present": structured_content_present,
                    "readiness_status": "degraded",
                    "discovery_flow": { "status": "passed" },
                    "stdout_protocol_clean": true,
                    "presentation_free": true
                }
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
            "--benchmark-skipped-reason",
            "Benchmark evidence is not retained for this release candidate.",
            "--archive-verification",
            archive_verification_path.to_str().unwrap(),
            "--output",
            output_path.to_str().unwrap(),
        ],
    );
    assert!(
        !output.status.success(),
        "release evidence unexpectedly accepted smoke results without the overview label: {}",
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
    let update_manifest_workflow =
        fs::read_to_string(repo_root().join(".github/workflows/publish-update-manifest.yml"))
            .unwrap();
    // The channel projection itself now lives solely in write_update_manifest.sh
    // (publish-update-manifest.yml propagates the attested asset verbatim, with
    // no inline jq regeneration), so the retained-vs-removed channel checks
    // below read the script rather than the workflow text.
    let write_update_manifest_script =
        fs::read_to_string(release_script("write_update_manifest.sh")).unwrap();

    assert!(workflow.contains("workflow_dispatch:"));
    assert!(
        workflow.contains("workflow_run:") && workflow.contains("- release-assets"),
        "release evidence must trigger on release-assets completion instead of release publication"
    );
    assert!(
        workflow.contains("github.event.workflow_run.conclusion == 'success'"),
        "automatic release evidence runs must require a successful release-assets run"
    );
    assert!(workflow.contains("github.event.workflow_run.head_branch"));
    assert!(
        !workflow.contains("github.event.release.tag_name"),
        "release evidence must not race the release publication event for its tag"
    );
    assert!(workflow.contains("macos-26"));
    assert!(!workflow.contains("macos-13"));
    assert!(!workflow.contains("x86_64-apple-darwin"));
    assert!(workflow.contains("download retained security check"));
    assert!(workflow.contains("bash scripts/release/download_retained_security_check.sh"));
    assert!(workflow.contains("pattern: archive-verification-*"));
    assert!(workflow.contains("ubuntu-24.04-arm"));
    assert!(workflow.contains("--target \"${{ matrix.target }}\""));
    assert!(!workflow.contains("package-publication-record"));
    assert!(!repo_root()
        .join(".github/workflows/publish-packages.yml")
        .exists());

    assert!(update_manifest_workflow.contains("name: publish-update-manifest"));
    assert!(update_manifest_workflow.contains("publish update manifest to main"));
    assert!(update_manifest_workflow.contains("verify stable update manifest"));
    assert!(write_update_manifest_script.contains("script_install"));
    assert!(write_update_manifest_script.contains("update_manifest"));
    assert!(!update_manifest_workflow.contains("PACKAGE_PUBLISH_TOKEN"));
    assert!(!update_manifest_workflow.contains("Homebrew"));
    assert!(!update_manifest_workflow.contains("Scoop"));
}

#[test]
fn release_runbook_omits_package_publication_gates() {
    let runbook = fs::read_to_string(repo_root().join("RELEASE.md")).unwrap();

    for retained in [
        "Primary user install channel",
        "Stable update metadata",
        "archive verification notes",
        "MCP smoke evidence for the retained `oneup_*` tools",
        "script installer and update-manifest distribution metadata",
        "publish-update-manifest",
        "The final `release-evidence.json` references the GitHub Release, script installer, update manifest, archive verification, security, eval, and benchmark evidence",
    ] {
        assert!(
            runbook.contains(retained),
            "release runbook is missing retained release evidence text: {retained}"
        );
    }

    for removed in [
        "Homebrew",
        "Scoop",
        "package-publication-record",
        "package publication repositories",
        "package publication step",
        "published package references",
    ] {
        assert!(
            !runbook.contains(removed),
            "release runbook should not require removed package publication gate {removed}"
        );
    }
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

    assert!(workflow.contains("workflow_dispatch:"));
    assert!(workflow.contains("macos-26"));
    assert!(!workflow.contains("macos-13"));
    assert!(!workflow.contains("x86_64-apple-darwin"));
    assert!(workflow.contains("oneup-v*.*.*"));
    assert!(workflow.contains("RELEASE_TAG"));
    assert!(workflow.contains("ref: ${{ env.RELEASE_TAG }}"));
    assert!(workflow.contains("UPDATE_MANIFEST_URL"));
    assert!(workflow.contains("ONEUP_UPDATE_MANIFEST_URL"));
    assert!(workflow.contains("--commit-sha \"${commit_sha}\""));
    assert!(workflow.contains("waiting for existing release"));
    assert!(workflow.contains("gh release edit \"$tag\""));
    assert!(workflow.contains("gh release create \"$tag\""));
    assert!(!workflow.contains("release ${tag} is already published"));
    assert!(workflow.contains("stage Windows ONNX Runtime DLL"));
    assert!(workflow.contains("onnxruntime.dll"));
    assert!(workflow.contains("Get-FileHash"));
    assert!(workflow.contains("onnxruntime-win-x64-1.24.2.zip"));
    assert!(workflow.contains("Expand-Archive -LiteralPath $archivePath"));
}

#[test]
fn release_assets_workflow_emits_build_provenance_attestation() {
    let workflow =
        fs::read_to_string(repo_root().join(".github/workflows/release-assets.yml")).unwrap();

    // The publish job that mints the OIDC token must hold both elevated grants.
    assert!(
        workflow.contains("id-token: write"),
        "release-assets must grant id-token:write for keyless-OIDC attestation"
    );
    assert!(
        workflow.contains("attestations: write"),
        "release-assets must grant attestations:write to record provenance"
    );
    // contents:write is still required for the gh release create/upload.
    assert!(workflow.contains("contents: write"));

    // The provenance step must run and cover the exact archives clients verify.
    // The exact pinned SHA is asserted by
    // release_workflow_third_party_actions_are_sha_pinned; this only checks
    // the step is present.
    assert!(
        workflow.contains("actions/attest-build-provenance@"),
        "release-assets must run actions/attest-build-provenance for build provenance"
    );
    assert!(workflow.contains("subject-path:"));
    assert!(
        workflow.contains("/dist/*.tar.gz") && workflow.contains("/dist/*.zip"),
        "attestation subject must cover the published .tar.gz and .zip archives"
    );
    assert!(
        workflow.contains("/dist/update-manifest.json"),
        "attestation subject must cover update-manifest.json so the manifest client-side gate has an attested digest to verify"
    );

    // No regression: existing artifact/checksum/manifest emission still present.
    assert!(workflow.contains("write_sha256sums.sh"));
    assert!(workflow.contains("generate_release_manifest.sh"));
    assert!(workflow.contains("gh release upload \"$tag\""));
}

/// Guards REQ-001's producer side: `update-manifest.json` must be generated
/// exactly once (via `write_update_manifest.sh`, out of line in
/// `release-assets.yml`), attested alongside the archives, uploaded as a
/// release asset, and then propagated to `main` byte-verbatim -- no inline jq
/// re-projection in `publish-update-manifest.yml` that could diverge from the
/// attested bytes.
#[test]
fn update_manifest_is_generated_once_and_published_verbatim() {
    let release_assets_workflow =
        fs::read_to_string(repo_root().join(".github/workflows/release-assets.yml")).unwrap();
    let publish_workflow =
        fs::read_to_string(repo_root().join(".github/workflows/publish-update-manifest.yml"))
            .unwrap();

    assert!(
        release_assets_workflow.contains("write_update_manifest.sh"),
        "release-assets must generate update-manifest.json via write_update_manifest.sh"
    );
    assert!(
        release_assets_workflow.contains("update-manifest.json") && {
            let upload_section = release_assets_workflow
                .split("gh release upload \"$tag\"")
                .nth(1)
                .expect("release-assets must upload release assets via gh release upload");
            upload_section.contains("update-manifest.json")
        },
        "release-assets must upload update-manifest.json as a release asset"
    );

    assert!(
        publish_workflow.contains("--pattern update-manifest.json"),
        "publish-update-manifest must download the attested update-manifest.json release asset"
    );
    assert!(
        !publish_workflow.contains("--pattern release-manifest.json"),
        "publish-update-manifest must not re-derive the manifest from release-manifest.json"
    );
    assert!(
        !publish_workflow.contains("artifacts: [.artifacts[] | {target, archive, sha256, url}]"),
        "publish-update-manifest must not run the inline jq projection anymore -- the projection lives solely in write_update_manifest.sh"
    );
    assert!(
        publish_workflow
            .contains("cp \"${RUNNER_TEMP}/release/update-manifest.json\" update-manifest.json"),
        "publish-update-manifest must copy the attested asset verbatim onto main"
    );
    assert!(
        publish_workflow.contains("sha256sum"),
        "verify job must byte-compare (sha256) the raw-fetched manifest against the release asset"
    );
}

/// Guards REQ-004: every third-party GitHub Action referenced from the four
/// release-path workflows must be pinned to a full commit SHA (not a
/// floating tag such as `@v4` or a moving branch such as `@stable`), so a
/// repointed tag or branch cannot inject malicious action code into a
/// release run. Each pin must also carry a trailing `#` comment naming the
/// resolved ref for human auditability -- most actions publish semver tags
/// (`# v4.3.1`), but `dtolnay/rust-toolchain` has no semver releases and is
/// commonly annotated with its tracked channel (`# stable`) instead, so the
/// comment is required to be present rather than matching a fixed shape.
#[test]
fn release_workflow_third_party_actions_are_sha_pinned() {
    const RELEASE_WORKFLOW_PATHS: [&str; 4] = [
        ".github/workflows/release-assets.yml",
        ".github/workflows/publish-update-manifest.yml",
        ".github/workflows/release-please.yml",
        ".github/workflows/release-evidence.yml",
    ];

    fn is_lowercase_hex_sha(candidate: &str) -> bool {
        candidate.len() == 40
            && candidate
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    }

    for relative_path in RELEASE_WORKFLOW_PATHS {
        let workflow = fs::read_to_string(repo_root().join(relative_path)).unwrap();

        for (index, line) in workflow.lines().enumerate() {
            let line_number = index + 1;
            let Some(uses_at) = line.find("uses:") else {
                continue;
            };
            let reference = line[uses_at + "uses:".len()..].trim();

            let Some(at) = reference.find('@') else {
                panic!("{relative_path}:{line_number}: `uses: {reference}` has no pinned ref");
            };
            let action = &reference[..at];
            let pinned = &reference[at + 1..];
            let (sha, comment) = match pinned.split_once('#') {
                Some((sha, comment)) => (sha.trim(), comment.trim()),
                None => (pinned.trim(), ""),
            };

            assert!(
                is_lowercase_hex_sha(sha),
                "{relative_path}:{line_number}: `{action}` must be pinned to a 40-character lowercase commit SHA, found `{sha}`"
            );
            assert!(
                !comment.is_empty(),
                "{relative_path}:{line_number}: SHA-pinned `{action}` must carry a trailing `# <ref>` comment naming the resolved version"
            );
        }
    }
}

#[test]
fn release_please_config_disables_component_prefixed_tags() {
    let config = fs::read_to_string(repo_root().join("release-please-config.json")).unwrap();

    assert!(config.contains("\"include-component-in-tag\": false"));
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
