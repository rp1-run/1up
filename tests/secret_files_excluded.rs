//! Integration test: Secret file exclusion (REQ-004)
//!
//! This test validates that:
//! 1. Secret files matching expanded glob patterns are excluded from indexing
//! 2. Search results do not contain content from secret files
//! 3. .1up/.gitignore is created with `*` at project init to prevent git-add

mod common;

use assert_cmd::prelude::*;
use common::HideModelGuard;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::thread;
use std::time::{Duration, Instant};
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

fn wait_for_index_ready(home: &Path, project_path: &Path, deadline: Instant) -> bool {
    loop {
        if Instant::now() >= deadline {
            return false;
        }

        let status_output = run_1up_command(home, &["status", project_path.to_str().unwrap()]);
        if !status_output.status.success() {
            thread::sleep(Duration::from_millis(100));
            continue;
        }

        let status_text = String::from_utf8_lossy(&status_output.stdout);

        // Check if index is ready (not "indexing" or "pending_refresh", and not degraded due to model absence)
        // In FTS-only mode, "ready" means the index exists and isn't currently being rebuilt
        if !status_text.contains("indexing") && !status_text.contains("pending_refresh") {
            // Found a stable state (ready, degraded_reason with "FTS-only", etc.)
            return true;
        }

        thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn secret_files_are_excluded_from_indexing() {
    let _hide_model = HideModelGuard::new();

    // Create isolated test environment
    let home = TempDir::new().unwrap();
    let home_path = home.path().canonicalize().unwrap();
    seed_model_download_failure(&home_path);

    // Create a temporary project directory
    let temp_base = TempDir::new().unwrap();
    let project_path = temp_base.path().join("test_project");
    fs::create_dir_all(&project_path).unwrap();

    // Initialize as a git repository (required by 1up start)
    let git_init = StdCommand::new("git")
        .args(["init"])
        .current_dir(&project_path)
        .output()
        .expect("git init failed");
    assert!(git_init.status.success(), "git init should succeed");

    // Create legitimate source files with unique content
    fs::create_dir_all(project_path.join("src")).unwrap();
    fs::write(
        project_path.join("src").join("lib.rs"),
        "pub fn hello_world() { println!(\"Hello from source code\"); }",
    )
    .unwrap();

    // Create secret files with sensitive content (REQ-004)
    // These should NOT appear in search results
    fs::write(
        project_path.join("gcp-service-account.json"),
        r#"{"type": "service_account", "private_key": "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEA2Z3qX2BTLS...\n-----END RSA PRIVATE KEY-----"}"#,
    ).unwrap();

    fs::write(
        project_path.join("secrets.yaml"),
        "database_password: super_secret_db_password_12345\napi_key: sk_live_secret_key_abcdef",
    )
    .unwrap();

    fs::write(
        project_path.join(".aws_credentials"),
        "[default]\naws_access_key_id = AKIA1234567890ABCDEF\naws_secret_access_key = wJal123456789/Secret/Key/String",
    ).unwrap();

    fs::write(
        project_path.join("id_rsa"),
        "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEA2Z3qX2BTLS...\n-----END RSA PRIVATE KEY-----",
    ).unwrap();

    fs::write(
        project_path.join("cert.p12"),
        "Binary P12 certificate with sensitive data",
    )
    .unwrap();

    fs::write(
        project_path.join(".env.local"),
        "DATABASE_URL=postgresql://user:secret@localhost/db\nAPI_TOKEN=secret_token_xyz",
    )
    .unwrap();

    // Get the canonical path
    let canonical_project_path = project_path.canonicalize().unwrap();

    // Start the project (index it)
    let start_output = run_1up_command(
        &home_path,
        &["start", canonical_project_path.to_str().unwrap()],
    );
    assert!(
        start_output.status.success(),
        "1up start should succeed; stderr={}",
        String::from_utf8_lossy(&start_output.stderr)
    );

    // Wait for index to be ready before searching (bounded 10-second deadline)
    // The daemon indexes concurrently; we must wait for it to stabilize
    let readiness_deadline = Instant::now() + Duration::from_secs(10);
    assert!(
        wait_for_index_ready(&home_path, &canonical_project_path, readiness_deadline),
        "index should become ready within 10 seconds"
    );

    // Verify .1up/.gitignore was created with `*` (REQ-004)
    let gitignore_path = project_path.join(".1up").join(".gitignore");
    assert!(
        gitignore_path.exists(),
        ".1up/.gitignore should be created at project init"
    );
    let gitignore_content = fs::read_to_string(&gitignore_path).unwrap();
    assert_eq!(
        gitignore_content, "*",
        ".1up/.gitignore should contain exactly `*` to exclude all files in .1up"
    );

    // Search for content from legitimate source files (should be found)
    let search_hello = run_1up_command(
        &home_path,
        &[
            "search",
            "--path",
            canonical_project_path.to_str().unwrap(),
            "Hello from source code",
        ],
    );
    assert!(
        search_hello.status.success(),
        "search for legitimate source content should succeed; stderr={}",
        String::from_utf8_lossy(&search_hello.stderr)
    );
    let search_output = String::from_utf8_lossy(&search_hello.stdout);
    assert!(
        search_output.contains("Hello from source code") || search_output.contains("src/lib.rs"),
        "search should find legitimate source file content"
    );

    // Search for content from secret files (should NOT be found)
    // REQ-004: Secret files are excluded, so searching for their content returns no results

    let search_gcp_secret = run_1up_command(
        &home_path,
        &[
            "search",
            "--path",
            canonical_project_path.to_str().unwrap(),
            "service_account",
        ],
    );
    let gcp_output = String::from_utf8_lossy(&search_gcp_secret.stdout);
    // Should not contain service account JSON content
    assert!(
        !gcp_output.contains("private_key") && !gcp_output.contains("service-account.json"),
        "search should NOT return content from service-account.json secret file"
    );

    let search_aws_secret = run_1up_command(
        &home_path,
        &[
            "search",
            "--path",
            canonical_project_path.to_str().unwrap(),
            "AKIA",
        ],
    );
    let aws_output = String::from_utf8_lossy(&search_aws_secret.stdout);
    assert!(
        !aws_output.contains("AKIA") && !aws_output.contains("credentials"),
        "search should NOT return AWS credential content"
    );

    let search_env_secret = run_1up_command(
        &home_path,
        &[
            "search",
            "--path",
            canonical_project_path.to_str().unwrap(),
            "DATABASE_URL",
        ],
    );
    let env_output = String::from_utf8_lossy(&search_env_secret.stdout);
    assert!(
        !env_output.contains("DATABASE_URL") && !env_output.contains("secret_token"),
        "search should NOT return .env.local secret content"
    );

    println!("REQ-004 test passed: secret files excluded, .1up/.gitignore created");
}
