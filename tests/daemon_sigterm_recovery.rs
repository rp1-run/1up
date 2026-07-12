//! Integration test: the detached daemon WORKER honors SIGTERM after its watched
//! directory is deleted.
//!
//! This guards the release BLOCKER where a deleted/renamed source root pinned the
//! worker in an unresponsive 100%-CPU spin. The worker is a *detached* process
//! (`1up __worker <source_root>`), reparented away from whatever spawned it. An
//! earlier version of this test signaled the MCP *frontend* PID and filtered orphans
//! with `pgrep -P <mcp_pid> __worker` — a parent that never owns the worker plus a
//! name match that never fires (the process name is `1up`, not `__worker`): a double
//! false-green that always passed. This version:
//!   1. spawns the daemon via `1up start` (which returns after detaching the worker —
//!      no lingering frontend to respawn or confuse the signal),
//!   2. confirms the worker actually spawned (closing the vacuous-pass hole),
//!   3. deletes the watched directory to trigger the potential spin, then
//!   4. SIGTERMs the WORKER directly and asserts it exits promptly.
//!
//! Idle-shutdown is pinned high so the ONLY thing that can reap the worker inside the
//! assertion window is the signal being honored — a still-spinning worker would ignore
//! it and the test fails.

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

/// PIDs of detached workers whose full argv contains `pattern`
/// (`__worker <project_path>`). Scoped to a unique per-test project path so it can
/// never observe or signal an unrelated developer or CI daemon.
#[cfg(unix)]
fn worker_pids(pattern: &str) -> Vec<u32> {
    let out = StdCommand::new("pgrep")
        .args(["-f", pattern])
        .output()
        .expect("pgrep command failed");
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .filter_map(|s| s.parse::<u32>().ok())
        .collect()
}

/// SIGKILL any straggler worker matching `pattern` on drop, so even a panicking test
/// never leaks a detached daemon (idle-shutdown is pinned high for these tests, so an
/// un-reaped worker would linger for minutes). Path-scoped, so it can never touch an
/// unrelated daemon.
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

/// Delete a directory that a live worker may still be writing into. A plain
/// `remove_dir_all` races the daemon creating `.1up/index.db.rebuild-<uuid>` staging
/// files (the walker lists a directory, the worker adds a file, the `rmdir` then fails
/// `ENOTEMPTY`) — an inherent test-harness race, not a defect in the code under test.
/// Retry until the tree is gone; each pass removes more, and once the target vanishes
/// the worker's rebuild errors out and stops writing, so this converges quickly.
#[cfg(unix)]
fn remove_dir_all_robust(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match fs::remove_dir_all(path) {
            Ok(()) => return,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) if Instant::now() >= deadline => {
                panic!("failed to delete project directory {}: {e}", path.display())
            }
            Err(_) => thread::sleep(Duration::from_millis(150)),
        }
    }
}

#[cfg(unix)]
#[test]
fn daemon_worker_honors_sigterm_after_directory_deletion() {
    let _hide_model = HideModelGuard::new();

    // Isolated fake HOME (canonical so secure-fs accepts it).
    let home = TempDir::new().unwrap();
    let home_path = home.path().canonicalize().unwrap();
    seed_model_download_failure(&home_path);

    // Project with a little content to index.
    let project = TempDir::new().unwrap();
    let project_path = project.path().canonicalize().unwrap();
    let git_init = StdCommand::new("git")
        .args(["init"])
        .current_dir(&project_path)
        .output()
        .expect("git init failed");
    assert!(git_init.status.success(), "git init should succeed");
    fs::create_dir_all(project_path.join("src")).unwrap();
    fs::write(project_path.join("src").join("lib.rs"), "fn hello() {}").unwrap();
    fs::write(project_path.join("README.md"), "# Test Project").unwrap();

    // Start the project. `1up start` runs the foreground initial index, spawns the
    // DETACHED worker, and returns — leaving the worker watching the project with no
    // frontend attached. Idle-shutdown is pinned high so the worker cannot self-reap
    // within the test window; only the SIGTERM below can end it promptly.
    let start = StdCommand::cargo_bin("1up")
        .unwrap()
        .args(["start", project_path.to_str().unwrap()])
        .env("HOME", &home_path)
        .env("XDG_DATA_HOME", home_path.join(".local").join("share"))
        .env("XDG_CONFIG_HOME", home_path.join(".config"))
        .env("ONEUP_DISABLE_MODEL_DOWNLOADS", "1")
        .env("ONEUP_DAEMON_IDLE_SHUTDOWN_SECS", "300")
        .output()
        .expect("failed to run 1up start");
    assert!(
        start.status.success(),
        "1up start should succeed; stderr={}",
        String::from_utf8_lossy(&start.stderr)
    );

    let worker_pattern = format!("__worker {}", project_path.display());
    // Reap the worker on any exit path (including panics) so a failed assertion never
    // leaks a long-idle daemon.
    let _reaper = WorkerReaper {
        pattern: worker_pattern.clone(),
    };

    // Confirm the detached worker actually spawned before we rely on signaling it.
    // (The old test never verified this, so its orphan check passed vacuously.)
    let spawn_deadline = Instant::now() + Duration::from_secs(20);
    let mut worker_pid = None;
    while Instant::now() < spawn_deadline {
        if let Some(pid) = worker_pids(&worker_pattern).first().copied() {
            worker_pid = Some(pid);
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    let worker_pid = worker_pid.unwrap_or_else(|| {
        panic!(
            "detached __worker for {} never spawned",
            project_path.display()
        )
    });

    // Delete the watched directory (tolerating the worker's concurrent staging writes)
    // to trigger the potential spin, then give the worker a moment to observe it.
    remove_dir_all_robust(&project_path);
    thread::sleep(Duration::from_millis(300));

    // SIGTERM the WORKER directly (not any frontend).
    unsafe {
        let _ = libc::kill(worker_pid as i32, libc::SIGTERM);
    }

    // The worker must exit promptly. With idle-shutdown pinned at 300s, a prompt
    // disappearance can ONLY be the signal being honored — proving the worker is not
    // wedged in an unresponsive CPU spin on the now-deleted directory.
    let exit_deadline = Instant::now() + Duration::from_secs(8);
    let mut worker_gone = false;
    while Instant::now() < exit_deadline {
        if worker_pids(&worker_pattern).is_empty() {
            worker_gone = true;
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }

    // Belt-and-suspenders reap of any straggler, scoped to this test's project path.
    let _ = StdCommand::new("pkill")
        .args(["-f", &worker_pattern])
        .output();

    assert!(
        worker_gone,
        "detached __worker for {} must honor SIGTERM within 8s after its directory is deleted; it was still running (possible unresponsive spin)",
        project_path.display()
    );
}
