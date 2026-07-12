/// REQ-005 (T6): Per-file size and segment caps integration tests.
///
/// Validates that files exceeding the per-file size cap are skipped without
/// reading into memory, and that the segment cap bounds unbounded segmentation
/// from dense/minified files. Tests verify index contents (segment counts) so
/// they remain deterministic without requiring the embedding model.
mod common;

#[allow(unused_imports)]
use assert_cmd::prelude::CommandCargoExt;
use assert_cmd::Command;
use common::HideModelGuard;
use oneup::storage::{db::Db, segments};
use std::fs;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn test_data_dir(home: &Path) -> std::path::PathBuf {
    #[cfg(target_os = "macos")]
    {
        home.join("Library").join("Application Support").join("1up")
    }

    #[cfg(not(target_os = "macos"))]
    {
        home.join(".local").join("share").join("1up")
    }
}

fn project_db_path(project: &Path) -> std::path::PathBuf {
    project.join(".1up").join("index.db")
}

fn seed_model_download_failure(home: &Path) {
    let model_dir = test_data_dir(home).join("models").join("all-MiniLM-L6-v2");
    fs::create_dir_all(&model_dir).unwrap();
    fs::write(model_dir.join(".download_failed"), "skip download in test").unwrap();
}

fn cmd_with_home(home: &Path) -> Command {
    let mut command = Command::cargo_bin("1up").unwrap();
    command
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join(".local").join("share"))
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("ONEUP_DISABLE_MODEL_DOWNLOADS", "1");
    command
}

/// Wait for the indexing to complete by checking status
fn wait_for_indexing_complete(home: &Path, project_path: &Path) {
    // Generous under parallel CI load: the security-check job runs the whole suite
    // concurrently, so a fresh FTS-only index can take well over 10s to reach ready.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let output = cmd_with_home(home)
            .args(["status", project_path.to_str().unwrap(), "--format", "json"])
            .output();

        if let Ok(output) = output {
            if output.status.success() {
                if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&output.stdout) {
                    if let Some("ready") = json.get("state").and_then(|s| s.as_str()) {
                        return;
                    }
                }
            }
        }

        if Instant::now() >= deadline {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn get_segment_count_for_file_blocking(project_path: &Path, file_path: &str) -> usize {
    let db_path = project_db_path(project_path);

    // Handle tokio runtime in tests
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        match Db::open_ro(&db_path).await {
            Ok(db) => {
                match db.connect() {
                    Ok(conn) => {
                        // Get all contexts from the database
                        if let Ok(contexts) = segments::list_worktree_contexts(&conn).await {
                            if let Some(ctx) = contexts.first() {
                                // Use the first context (typically only one per project)
                                match segments::get_segments_by_file_for_context(
                                    &conn,
                                    &ctx.context_id,
                                    file_path,
                                )
                                .await
                                {
                                    Ok(segs) => segs.len(),
                                    Err(_) => 0,
                                }
                            } else {
                                0
                            }
                        } else {
                            0
                        }
                    }
                    Err(_) => 0,
                }
            }
            Err(_) => 0,
        }
    })
}

/// Poll the on-disk segment count for `file_path` until it reaches at least
/// `min_expected`, or a generous deadline elapses; returns the last observed count.
/// Indexing is asynchronous — `start` runs a foreground initial index AND the daemon
/// then refreshes by rebuilding into a staging DB and atomically swapping it in — so
/// a single read taken right after `wait_for_indexing_complete` (which silently
/// returns on timeout) can race the swap window and observe 0 under parallel CI
/// load. Polling for the expected content removes that flake without weakening the
/// assertion.
fn wait_for_file_segments(project_path: &Path, file_path: &str, min_expected: usize) -> usize {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let count = get_segment_count_for_file_blocking(project_path, file_path);
        if count >= min_expected || Instant::now() >= deadline {
            return count;
        }
        thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn test_file_above_size_cap_is_skipped() {
    let _hide_model = HideModelGuard::new();

    let home = TempDir::new().unwrap();
    let home_path = home.path().canonicalize().unwrap();
    seed_model_download_failure(&home_path);

    let project_dir = TempDir::new().unwrap();
    let project_path = project_dir.path().canonicalize().unwrap();

    // Initialize git repo
    std::process::Command::new("git")
        .arg("init")
        .current_dir(&project_path)
        .output()
        .unwrap();

    // Create a normal file that should be indexed
    fs::write(
        project_path.join("normal.txt"),
        "small file\nwith content\n",
    )
    .unwrap();

    // Create a file exceeding the 2MB size cap (3MB file)
    let oversized_content = "x".repeat(3 * 1024 * 1024);
    fs::write(project_path.join("oversized.txt"), &oversized_content).unwrap();

    // Index the project
    let output = cmd_with_home(&home_path)
        .args(["start", project_path.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "1up start should succeed despite oversized file: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Wait for indexing to complete
    wait_for_indexing_complete(&home_path, &project_path);

    // Poll the normal file until it has segments (ensures indexing reached it), then
    // read the oversized file — which must have produced none.
    let normal_segments = wait_for_file_segments(&project_path, "normal.txt", 1);
    let oversized_segments = get_segment_count_for_file_blocking(&project_path, "oversized.txt");

    // Cleanup
    let _ = cmd_with_home(&home_path)
        .args(["stop", project_path.to_str().unwrap()])
        .output();

    // Assertions
    assert_eq!(
        oversized_segments, 0,
        "file exceeding 2MB cap should produce 0 segments"
    );
    assert!(
        normal_segments > 0,
        "normal small file should produce at least 1 segment, got {}",
        normal_segments
    );
}

#[test]
fn test_segment_cap_bounds_dense_file() {
    let _hide_model = HideModelGuard::new();

    let home = TempDir::new().unwrap();
    let home_path = home.path().canonicalize().unwrap();
    seed_model_download_failure(&home_path);

    let project_dir = TempDir::new().unwrap();
    let project_path = project_dir.path().canonicalize().unwrap();

    // Initialize git repo
    std::process::Command::new("git")
        .arg("init")
        .current_dir(&project_path)
        .output()
        .unwrap();

    // Create a dense file with many lines that will generate lots of segments
    let mut dense_content = String::new();
    for i in 0..5000 {
        // Each line is long enough to create more segment windows
        dense_content.push_str(&format!(
            "line_{:05}_content_intentionally_long_to_test_segment_cap________________________________{}\n",
            i, i
        ));
    }
    fs::write(project_path.join("dense.txt"), &dense_content).unwrap();

    // Index the project
    let output = cmd_with_home(&home_path)
        .args(["start", project_path.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "1up start should succeed with segment capping: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Wait for indexing to complete
    wait_for_indexing_complete(&home_path, &project_path);

    // Poll the dense file until it has segments, then assert the cap bound.
    let segment_count = wait_for_file_segments(&project_path, "dense.txt", 1);

    // Cleanup
    let _ = cmd_with_home(&home_path)
        .args(["stop", project_path.to_str().unwrap()])
        .output();

    // Assertion: segment count should be capped at MAX_SEGMENTS_PER_FILE (1000)
    // Allow some tolerance for variation in chunking
    assert!(
        segment_count <= 1050,
        "dense file should have segments capped at ~1000, got {}",
        segment_count
    );
    assert!(
        segment_count > 0,
        "dense file should still produce some segments"
    );
}

#[test]
fn test_normal_file_indexes_successfully() {
    let _hide_model = HideModelGuard::new();

    let home = TempDir::new().unwrap();
    let home_path = home.path().canonicalize().unwrap();
    seed_model_download_failure(&home_path);

    let project_dir = TempDir::new().unwrap();
    let project_path = project_dir.path().canonicalize().unwrap();

    // Initialize git repo
    std::process::Command::new("git")
        .arg("init")
        .current_dir(&project_path)
        .output()
        .unwrap();

    // Create a normal source file
    fs::write(
        project_path.join("example.rs"),
        "fn hello() {\n    println!(\"Hello\");\n}\n\nfn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
    ).unwrap();

    // Index the project
    let output = cmd_with_home(&home_path)
        .args(["start", project_path.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "1up start should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Wait for indexing to complete
    wait_for_indexing_complete(&home_path, &project_path);

    // Poll until the file has segments — indexing is async and can lag under load.
    let segment_count = wait_for_file_segments(&project_path, "example.rs", 1);

    // Cleanup
    let _ = cmd_with_home(&home_path)
        .args(["stop", project_path.to_str().unwrap()])
        .output();

    // Assertion: normal file should produce at least 1 segment
    assert!(
        segment_count > 0,
        "normal source file should produce segments, got {}",
        segment_count
    );
}
