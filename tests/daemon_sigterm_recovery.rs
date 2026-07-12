//! Integration test: daemon terminates cleanly when sent SIGTERM after directory deletion
//!
//! This test validates that when a project's source directory is deleted and the daemon
//! is sent SIGTERM, the daemon:
//! 1. Terminates cleanly within ~2 seconds
//! 2. Does not become unresponsive to signals
//! 3. Does not leave orphaned worker processes

mod common;

use common::HideModelGuard;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
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

#[cfg(unix)]
#[test]
fn daemon_sigterm_terminates_cleanly_after_directory_deletion() {
    let _hide_model = HideModelGuard::new();

    // Create isolated test environment
    let home = TempDir::new().unwrap();
    let home_path = home.path().canonicalize().unwrap();
    seed_model_download_failure(&home_path);

    // Create project with some indexed content
    let project = TempDir::new().unwrap();
    let project_path = project.path().canonicalize().unwrap();

    // Initialize project structure
    fs::create_dir_all(project_path.join("src")).unwrap();
    fs::write(project_path.join("src").join("lib.rs"), "fn hello() {}").unwrap();
    fs::write(project_path.join("README.md"), "# Test Project").unwrap();

    // Spawn daemon via MCP to start indexing
    let mut daemon_child = std::process::Command::new(env!("CARGO_BIN_EXE_1up"))
        .args(["mcp", "--path", project_path.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env("HOME", &home_path)
        .env("XDG_DATA_HOME", home_path.join("data"))
        .env("XDG_CONFIG_HOME", home_path.join("config"))
        .env("ONEUP_DISABLE_MODEL_DOWNLOADS", "1")
        // Short idle-shutdown so the detached worker self-reaps quickly once the
        // deleted project empties the registry, making the orphan-worker assertion
        // below deterministic. Inherited by the spawned `__worker`.
        .env("ONEUP_DAEMON_IDLE_SHUTDOWN_SECS", "2")
        .spawn()
        .expect("failed to spawn daemon");

    // Give daemon time to start and register the project
    thread::sleep(Duration::from_millis(500));

    let daemon_pid = daemon_child.id();

    // Delete the project directory to trigger detection
    fs::remove_dir_all(&project_path).expect("failed to delete project directory");

    // Give daemon a moment to start processing the deletion
    thread::sleep(Duration::from_millis(200));

    // Send SIGTERM to the daemon
    unsafe {
        let _ = libc::kill(daemon_pid as i32, libc::SIGTERM);
    }

    // Wait for daemon to exit with a 2-second timeout
    let start = Instant::now();
    let timeout = Duration::from_secs(2);
    let mut daemon_exited = false;

    loop {
        match daemon_child.try_wait() {
            Ok(Some(_status)) => {
                daemon_exited = true;
                break;
            }
            Ok(None) => {
                if Instant::now() >= start + timeout {
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(_) => {
                break;
            }
        }
    }

    // If still running, forcefully kill it
    if !daemon_exited {
        let _ = daemon_child.kill();
    }
    // Always wait for the child to ensure it's properly reaped
    let _ = daemon_child.wait();

    // Verify the daemon exited within timeout
    assert!(
        daemon_exited,
        "daemon should have terminated cleanly within 2 seconds after SIGTERM"
    );

    // Verify no orphaned/spinning worker remains. The worker is a DETACHED process
    // (`1up __worker <source_root>`), reparented away from the MCP frontend, so the
    // previous `pgrep -P <mcp_pid> __worker` filtered on a parent that never owns it
    // AND matched `__worker` as a process *name* (the process name is `1up`) — a
    // double false-green that always passed. Match the worker's full argv scoped to
    // THIS test's unique project path instead, and poll: after the project deletion
    // the worker deregisters the gone project, empties, and self-exits within the
    // short idle-shutdown window set above. Scoped so it can never observe or kill
    // an unrelated daemon.
    let worker_pattern = format!("__worker {}", project_path.display());
    let worker_deadline = Instant::now() + Duration::from_secs(15);
    let mut worker_gone = false;
    while Instant::now() < worker_deadline {
        let found = std::process::Command::new("pgrep")
            .args(["-f", &worker_pattern])
            .output()
            .expect("pgrep command failed");
        if String::from_utf8_lossy(&found.stdout).trim().is_empty() {
            worker_gone = true;
            break;
        }
        thread::sleep(Duration::from_millis(200));
    }
    // Path-scoped cleanup so a still-spinning worker never leaks into the host.
    let _ = std::process::Command::new("pkill")
        .args(["-f", &worker_pattern])
        .output();
    assert!(
        worker_gone,
        "detached __worker for {} must self-exit after the project is deleted; it was still running",
        project_path.display()
    );
}
