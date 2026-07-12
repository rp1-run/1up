//! Integration test: CLI stop command handles deleted project directories via registry fallback
//!
//! This test validates that when a project directory has been deleted from disk,
//! the `1up stop <deleted-path>` command:
//! 1. Succeeds in deregistering the project via registry-keyed fallback
//! 2. Returns exit code 0
//! 3. Removes the project from the registry so `1up list` no longer shows it

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
        .args(args);
    command.output().expect("failed to run 1up command")
}

#[test]
fn stop_deleted_directory_succeeds_via_registry_fallback() {
    let _hide_model = HideModelGuard::new();

    // Create isolated test environment
    let home = TempDir::new().unwrap();
    let home_path = home.path().canonicalize().unwrap();
    seed_model_download_failure(&home_path);

    // Create a temporary base directory that we'll manage manually
    let temp_base = TempDir::new().unwrap();
    let project_path = temp_base.path().join("test_project");
    fs::create_dir_all(&project_path).unwrap();

    // Initialize as a git repository (required by 1up start)
    let git_init = StdCommand::new("git")
        .args(["init"])
        .current_dir(&project_path)
        .output()
        .expect("git init failed");
    assert!(
        git_init.status.success(),
        "git init should succeed; stderr={}",
        String::from_utf8_lossy(&git_init.stderr)
    );

    // Initialize project structure with some files
    fs::create_dir_all(project_path.join("src")).unwrap();
    fs::write(project_path.join("src").join("lib.rs"), "fn hello() {}").unwrap();
    fs::write(project_path.join("README.md"), "# Test Project").unwrap();

    // Get the canonical path
    let canonical_project_path = project_path.canonicalize().unwrap();

    // Start the project (register it in the registry)
    let start_output = run_1up_command(
        &home_path,
        &["start", canonical_project_path.to_str().unwrap()],
    );
    assert!(
        start_output.status.success(),
        "1up start should succeed; stderr={}",
        String::from_utf8_lossy(&start_output.stderr)
    );

    // Verify the project is registered by checking list output
    let list_before_output = run_1up_command(&home_path, &["list", "--format", "json"]);
    assert!(
        list_before_output.status.success(),
        "1up list should succeed before deletion"
    );
    let list_before_json: serde_json::Value = serde_json::from_slice(&list_before_output.stdout)
        .expect("list output should be valid JSON");
    let projects_before = list_before_json["projects"]
        .as_array()
        .expect("projects should be an array");

    // Extract the registered path from the list output
    let registered_path = projects_before
        .first()
        .and_then(|p| p["project_root"].as_str())
        .expect("project should be registered before deletion")
        .to_string();

    // Kill only THIS test's worker (holding locks on the directory), scoped to its
    // unique worktree path so it can never terminate an unrelated developer or test
    // daemon. The worker is spawned as `1up __worker <source_root>` (see
    // lifecycle::spawn_daemon), and each test uses a distinct TempDir.
    let worker_pattern = format!("__worker {}", canonical_project_path.display());
    let _ = StdCommand::new("pkill")
        .args(["-f", &worker_pattern])
        .output();

    // Give the daemon a moment to release file handles
    std::thread::sleep(std::time::Duration::from_millis(200));

    // Now delete the temp_base which will delete the entire directory tree
    drop(temp_base);

    // Give cleanup a moment to complete
    std::thread::sleep(std::time::Duration::from_millis(100));

    // Verify the directory is actually deleted
    assert!(
        !canonical_project_path.exists(),
        "project directory should be deleted"
    );

    // Run `1up stop <deleted-path>` using the registered path and verify it succeeds
    let stop_output = run_1up_command(&home_path, &["stop", &registered_path]);

    // The critical assertion: exit code must be 0 (success)
    assert!(
        stop_output.status.success(),
        "1up stop should succeed on deleted directory; exit code={}, stderr={}",
        stop_output.status,
        String::from_utf8_lossy(&stop_output.stderr)
    );

    // Verify the project was deregistered by checking list again
    let list_after_output = run_1up_command(&home_path, &["list", "--format", "json"]);
    assert!(
        list_after_output.status.success(),
        "1up list should succeed after stop"
    );
    let list_after_json: serde_json::Value = serde_json::from_slice(&list_after_output.stdout)
        .expect("list output should be valid JSON");
    let projects_after = list_after_json["projects"]
        .as_array()
        .expect("projects should be an array");

    // The project should no longer be in the registry
    let still_registered = projects_after.iter().any(|p| {
        p["project_root"]
            .as_str()
            .map(|s| s == registered_path)
            .unwrap_or(false)
    });
    assert!(
        !still_registered,
        "project should be removed from registry after stop; list output: {:?}",
        list_after_json
    );
}
