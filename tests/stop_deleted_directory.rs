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

/// SIGKILL any straggler worker matching `pattern` on drop, so a panicking test never
/// leaks the long-idle daemon the live-daemon scenario keeps alive. Path-scoped to a
/// unique per-test project path, so it can never touch an unrelated daemon.
#[cfg(unix)]
struct WorkerReaper {
    pattern: String,
}

#[cfg(unix)]
impl Drop for WorkerReaper {
    fn drop(&mut self) {
        let _ = StdCommand::new("pkill")
            .args(["-9", "-f", &self.pattern])
            .output();
    }
}

/// Path to the global project registry under this test's fake HOME. Mirrors
/// `config::data_dir()` (`dirs::data_dir()/1up`), which resolves to the same
/// `XDG_DATA_HOME`/`~/Library/Application Support` location that `run_1up_command`
/// configures for the child processes.
fn registry_path(home: &Path) -> PathBuf {
    test_data_dir(home).join("projects.json")
}

/// Poll `1up status <project>` until the daemon reports running, so a subsequent
/// `stop` reliably exercises the *live-daemon* fallback branch rather than racing a
/// daemon that has not finished writing its pid file yet.
fn wait_for_daemon_running(home: &Path, project_path: &Path) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let output = run_1up_command(
            home,
            &["status", project_path.to_str().unwrap(), "--format", "json"],
        );
        if output.status.success() {
            if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&output.stdout) {
                if json.get("daemon_running").and_then(|v| v.as_bool()) == Some(true) {
                    return true;
                }
            }
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
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

/// Companion to the test above, exercising the OTHER fallback branch: a live daemon
/// is running when `1up stop <deleted-path>` is issued, so `finish_stop_after_fallback`
/// must probe the daemon and report its true state (`daemon_running: true` + a pid)
/// after notifying it — not the hardcoded `daemon: false` the pre-fix code returned.
///
/// Determinism: the daemon reloads its watch set ONLY on SIGHUP (worker.rs), so a
/// registry entry it was never notified about is never watched — and therefore never
/// hits the runtime deletion-prune that would otherwise race `stop` and remove the
/// entry first. We keep one real project (B) alive to hold the daemon up, then inject
/// a deleted-directory entry (A) straight into the registry without a SIGHUP. `stop A`
/// then deterministically takes the live-daemon fallback path with B still present, so
/// the daemon is SIGHUP'd (not SIGTERM'd) and reported as still running.
#[cfg(unix)]
#[test]
fn stop_deleted_directory_notifies_live_daemon_via_fallback() {
    let _hide_model = HideModelGuard::new();

    let home = TempDir::new().unwrap();
    let home_path = home.path().canonicalize().unwrap();
    seed_model_download_failure(&home_path);

    // Project B: a real, on-disk project that keeps the daemon alive for the duration.
    let live_base = TempDir::new().unwrap();
    let live_path = live_base.path().join("live_project");
    fs::create_dir_all(&live_path).unwrap();
    let git_init = StdCommand::new("git")
        .args(["init"])
        .current_dir(&live_path)
        .output()
        .expect("git init failed");
    assert!(git_init.status.success(), "git init should succeed");
    fs::write(live_path.join("lib.rs"), "fn live() {}").unwrap();
    let live_path = live_path.canonicalize().unwrap();

    // Reap the live worker on any exit path (including panics): its idle-shutdown is
    // pinned high below, so an un-reaped worker would linger for minutes.
    let _reaper = WorkerReaper {
        pattern: format!("__worker {}", live_path.display()),
    };

    // Start B with a high idle-shutdown so its daemon is guaranteed alive when we stop A.
    let start = StdCommand::cargo_bin("1up")
        .unwrap()
        .args(["start", live_path.to_str().unwrap()])
        .env("HOME", &home_path)
        .env("XDG_DATA_HOME", home_path.join(".local").join("share"))
        .env("XDG_CONFIG_HOME", home_path.join(".config"))
        .env("ONEUP_DISABLE_MODEL_DOWNLOADS", "1")
        .env("ONEUP_DAEMON_IDLE_SHUTDOWN_SECS", "300")
        .output()
        .expect("failed to run 1up start");
    assert!(
        start.status.success(),
        "1up start (live project) should succeed; stderr={}",
        String::from_utf8_lossy(&start.stderr)
    );
    assert!(
        wait_for_daemon_running(&home_path, &live_path),
        "daemon for the live project should be running before stopping the deleted one"
    );

    // Path A: a deleted/absent directory. Its parent exists (canonical) but A itself is
    // never created — exactly the "directory was removed" shape the fallback handles.
    let deleted_base = TempDir::new().unwrap();
    let deleted_parent = deleted_base.path().canonicalize().unwrap();
    let deleted_path = deleted_parent.join("deleted_project_a");
    let deleted_str = deleted_path.to_str().unwrap().to_string();
    assert!(!deleted_path.exists(), "A must not exist on disk");

    // Inject A into the registry WITHOUT notifying the daemon (no SIGHUP), so it stays
    // unwatched and unpruned. A minimal entry is enough: the loader's `normalize_entries`
    // back-fills the derived context fields, and the deleted path keeps its stored value
    // (canonicalize fails, so it is not rewritten), which is what the fallback matches on.
    let reg_path = registry_path(&home_path);
    let mut registry: serde_json::Value = serde_json::from_slice(
        &fs::read(&reg_path).expect("registry should exist after starting the live project"),
    )
    .expect("registry should be valid JSON");
    let projects = registry["projects"]
        .as_array_mut()
        .expect("registry should have a projects array");
    let registered_at = projects
        .first()
        .and_then(|p| p["registered_at"].as_str())
        .unwrap_or("2026-01-01T00:00:00Z")
        .to_string();
    projects.push(serde_json::json!({
        "project_id": "deleted-project-a",
        "project_root": deleted_str,
        "registered_at": registered_at,
    }));
    // Plain write: the loader validates only that the path is a non-symlink regular file
    // within the state root — it does not enforce permission bits — so no 0600 dance.
    fs::write(&reg_path, serde_json::to_vec_pretty(&registry).unwrap())
        .expect("failed to write injected registry");

    // Stop the deleted project. resolve_project_root fails (A is gone) -> registry
    // fallback -> finish_stop_after_fallback with the live daemon still up and B still
    // registered.
    let stop_output = run_1up_command(&home_path, &["stop", &deleted_str, "--format", "json"]);
    assert!(
        stop_output.status.success(),
        "1up stop should succeed on the deleted directory; exit={}, stderr={}",
        stop_output.status,
        String::from_utf8_lossy(&stop_output.stderr)
    );
    let stop_json: serde_json::Value =
        serde_json::from_slice(&stop_output.stdout).expect("stop output should be valid JSON");

    assert_eq!(
        stop_json["status"], "stopped",
        "stop should report the deleted project as stopped; got {stop_json:?}"
    );
    // The core of the fix: the live daemon is probed and reported, not hardcoded false.
    assert_eq!(
        stop_json["daemon_running"], true,
        "stop must report the still-running daemon after fallback; got {stop_json:?}"
    );
    assert!(
        stop_json["pid"].is_number(),
        "stop must report the live daemon's pid after fallback; got {stop_json:?}"
    );
    assert!(
        stop_json["message"]
            .as_str()
            .map(|m| m.contains("notified to stop watching"))
            .unwrap_or(false),
        "stop message should say the daemon was notified; got {stop_json:?}"
    );

    // A must be gone from the registry; B must remain (it kept the daemon alive).
    let list_json: serde_json::Value = {
        let out = run_1up_command(&home_path, &["list", "--format", "json"]);
        assert!(out.status.success(), "1up list should succeed after stop");
        serde_json::from_slice(&out.stdout).expect("list output should be valid JSON")
    };
    let roots: Vec<String> = list_json["projects"]
        .as_array()
        .expect("projects should be an array")
        .iter()
        .filter_map(|p| p["project_root"].as_str().map(|s| s.to_string()))
        .collect();
    assert!(
        !roots.iter().any(|r| r == &deleted_str),
        "deleted project A should be deregistered; list roots: {roots:?}"
    );
    assert!(
        roots.iter().any(|r| Path::new(r) == live_path),
        "live project B should still be registered; list roots: {roots:?}"
    );

    // Tear down the live project's daemon via the normal path (B still exists); the
    // `_reaper` above SIGKILLs any straggler worker on drop as a backstop.
    let _ = run_1up_command(&home_path, &["stop", live_path.to_str().unwrap()]);
}
