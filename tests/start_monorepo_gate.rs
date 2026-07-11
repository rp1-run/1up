//! Integration test: CLI start command enforces monorepo file-count gate
//!
//! This test validates that `1up start` on an over-threshold repository:
//! 1. Refuses first unscoped index and returns facts envelope
//! 2. Allows scoped indexing with `--scope <dir>` flag
//! 3. Gate is keyed on completed run (segments > 0), not partial index

mod common;

use assert_cmd::prelude::*;
use common::HideModelGuard;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use tempfile::TempDir;

fn test_data_dir(home: &Path) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        home.join("Library").join("Application Support").join("1up")
    }

    #[cfg(not(target_os = "macos"))]
    {
        home.join(".local").join("share").join("1up")
    }
}

fn seed_model_download_failure(home: &Path) {
    let model_dir = test_data_dir(home).join("models").join("all-MiniLM-L6-v2");
    fs::create_dir_all(&model_dir).unwrap();
    fs::write(model_dir.join(".download_failed"), "skip download in test").unwrap();
}

fn run_1up_command(home: &Path, args: &[&str]) -> std::process::Output {
    let mut command = StdCommand::cargo_bin("1up").unwrap();
    command
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join(".local").join("share"))
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("ONEUP_DISABLE_MODEL_DOWNLOADS", "1")
        .env("ONEUP_FILE_COUNT_THRESHOLD", "100") // Set low threshold for testing
        .args(args);
    command.output().expect("failed to run 1up command")
}

fn create_repo_with_n_files(repo_path: &Path, n: usize) {
    fs::create_dir_all(repo_path).unwrap();

    // Initialize as a git repository
    let git_init = StdCommand::new("git")
        .args(["init"])
        .current_dir(repo_path)
        .output()
        .expect("git init failed");
    assert!(
        git_init.status.success(),
        "git init should succeed; stderr={}",
        String::from_utf8_lossy(&git_init.stderr)
    );

    // Create n files spread across directories
    for i in 0..n {
        let dir_num = i / 50; // 50 files per directory
        let dir = repo_path.join(format!("dir_{}", dir_num));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join(format!("file_{}.rs", i));
        fs::write(&file, format!("// File {}\nfn func_{i}() {{}}\n", i)).unwrap();
    }
}

#[test]
fn start_over_threshold_refuses_without_scope() {
    let _hide_model = HideModelGuard::new();

    // Create isolated test environment
    let home = TempDir::new().unwrap();
    let home_path = home.path().canonicalize().unwrap();
    seed_model_download_failure(&home_path);

    // Create a temporary base directory
    let temp_base = TempDir::new().unwrap();
    let project_path = temp_base.path().join("test_monorepo");

    // Create repo with 150 files (over 100 threshold)
    create_repo_with_n_files(&project_path, 150);
    let canonical_project_path = project_path.canonicalize().unwrap();

    // Run `1up start` without --scope on over-threshold repo
    let start_output = run_1up_command(
        &home_path,
        &["start", canonical_project_path.to_str().unwrap()],
    );

    // Should NOT succeed (gate fires)
    assert!(
        !start_output.status.success(),
        "1up start should fail on over-threshold without --scope; stdout={}",
        String::from_utf8_lossy(&start_output.stdout)
    );

    // Output should contain facts envelope information (human-readable)
    let stdout_str = String::from_utf8_lossy(&start_output.stdout);
    assert!(
        stdout_str.contains("threshold") || stdout_str.contains("scope"),
        "facts envelope should mention threshold or scope requirement; stdout={}",
        stdout_str
    );
}

#[test]
fn start_over_threshold_succeeds_with_scope() {
    let _hide_model = HideModelGuard::new();

    // Create isolated test environment
    let home = TempDir::new().unwrap();
    let home_path = home.path().canonicalize().unwrap();
    seed_model_download_failure(&home_path);

    // Create a temporary base directory
    let temp_base = TempDir::new().unwrap();
    let project_path = temp_base.path().join("test_monorepo");

    // Create repo with 150 files (over 100 threshold)
    create_repo_with_n_files(&project_path, 150);
    let canonical_project_path = project_path.canonicalize().unwrap();

    // Run `1up start --scope dir_0` on over-threshold repo
    let start_output = run_1up_command(
        &home_path,
        &[
            "start",
            canonical_project_path.to_str().unwrap(),
            "--scope",
            "dir_0",
        ],
    );

    // Should succeed (gate allows scoped index)
    assert!(
        start_output.status.success(),
        "1up start with --scope should succeed on over-threshold repo; stderr={}",
        String::from_utf8_lossy(&start_output.stderr)
    );

    // Output should indicate indexing success
    let stdout_str = String::from_utf8_lossy(&start_output.stdout);
    assert!(
        stdout_str.contains("Indexed") || stdout_str.contains("indexed"),
        "output should indicate successful indexing; stdout={}",
        stdout_str
    );
}

#[test]
fn start_under_threshold_succeeds_without_scope() {
    let _hide_model = HideModelGuard::new();

    // Create isolated test environment
    let home = TempDir::new().unwrap();
    let home_path = home.path().canonicalize().unwrap();
    seed_model_download_failure(&home_path);

    // Create a temporary base directory
    let temp_base = TempDir::new().unwrap();
    let project_path = temp_base.path().join("test_small_repo");

    // Create repo with 50 files (under 100 threshold)
    create_repo_with_n_files(&project_path, 50);
    let canonical_project_path = project_path.canonicalize().unwrap();

    // Run `1up start` without --scope on under-threshold repo
    let start_output = run_1up_command(
        &home_path,
        &["start", canonical_project_path.to_str().unwrap()],
    );

    // Should succeed (gate doesn't fire)
    assert!(
        start_output.status.success(),
        "1up start should succeed on under-threshold repo without --scope; stderr={}",
        String::from_utf8_lossy(&start_output.stderr)
    );

    // Output should indicate indexing success
    let stdout_str = String::from_utf8_lossy(&start_output.stdout);
    assert!(
        stdout_str.contains("Indexed")
            || stdout_str.contains("indexed")
            || stdout_str.contains("registered"),
        "output should indicate successful start; stdout={}",
        stdout_str
    );
}

#[test]
fn gate_keyed_on_completed_run_not_partial_index() {
    let _hide_model = HideModelGuard::new();

    // Create isolated test environment
    let home = TempDir::new().unwrap();
    let home_path = home.path().canonicalize().unwrap();
    seed_model_download_failure(&home_path);

    // Create a temporary base directory
    let temp_base = TempDir::new().unwrap();
    let project_path = temp_base.path().join("test_completed_gate");

    // Create repo with 150 files (over 100 threshold)
    create_repo_with_n_files(&project_path, 150);
    let canonical_project_path = project_path.canonicalize().unwrap();

    // First attempt: start with scope (should succeed and create segments)
    let first_start = run_1up_command(
        &home_path,
        &[
            "start",
            canonical_project_path.to_str().unwrap(),
            "--scope",
            "dir_0",
        ],
    );
    assert!(
        first_start.status.success(),
        "first scoped start should succeed; stderr={}",
        String::from_utf8_lossy(&first_start.stderr)
    );

    // Second attempt: start again without scope (should succeed now because index exists)
    // Note: We run with a short timeout to avoid hanging on rebuilds in this test
    let second_start = run_1up_command(
        &home_path,
        &["start", canonical_project_path.to_str().unwrap()],
    );

    // Should succeed because index now exists (gate only fires on first index)
    assert!(
        second_start.status.success(),
        "second start without scope should succeed when index exists; stderr={}",
        String::from_utf8_lossy(&second_start.stderr)
    );
}
