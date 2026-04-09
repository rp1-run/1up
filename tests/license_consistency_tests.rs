use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn script_path() -> PathBuf {
    repo_root()
        .join("scripts")
        .join("release")
        .join("check_license_consistency.sh")
}

fn copy_surface(root: &Path, relative_path: &str) {
    let source = repo_root().join(relative_path);
    let destination = root.join(relative_path);
    fs::create_dir_all(destination.parent().unwrap()).unwrap();
    fs::copy(source, destination).unwrap();
}

fn build_fixture_root() -> tempfile::TempDir {
    let tempdir = tempfile::tempdir().unwrap();

    for relative_path in [
        "Cargo.toml",
        "README.md",
        "LICENSE",
        "skills/1up-search/SKILL.md",
    ] {
        copy_surface(tempdir.path(), relative_path);
    }

    tempdir
}

fn run_license_check(root: &Path) -> std::process::Output {
    Command::new("bash")
        .arg(script_path())
        .current_dir(repo_root())
        .env("ONEUP_LICENSE_CHECK_ROOT", root)
        .output()
        .unwrap()
}

#[test]
fn license_consistency_check_passes_for_release_metadata_surfaces() {
    let fixture_root = build_fixture_root();

    let output = run_license_check(fixture_root.path());
    assert!(
        output.status.success(),
        "license check unexpectedly failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn license_consistency_check_rejects_conflicting_readme_license_text() {
    let fixture_root = build_fixture_root();
    let readme_path = fixture_root.path().join("README.md");
    let readme = fs::read_to_string(&readme_path).unwrap();
    fs::write(&readme_path, readme.replacen("Apache 2.0", "MIT", 1)).unwrap();

    let output = run_license_check(fixture_root.path());
    assert!(
        !output.status.success(),
        "license check unexpectedly passed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("README.md license section"));
}
