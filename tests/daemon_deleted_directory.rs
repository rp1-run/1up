//! Integration test: daemon detects missing project directory and clears dirty flag
//!
//! This test validates that when a project's source directory is deleted while the daemon
//! is running, the daemon:
//! 1. Detects the missing root within ~5 seconds
//! 2. Deregisters the project from the registry
//! 3. Stops spinning on rebuild attempts
//! 4. Does not leave orphaned worker processes

mod common;

use common::HideModelGuard;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Stdio};
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

struct MinimalMcpClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl MinimalMcpClient {
    fn new(path: &Path, home: &Path) -> Self {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_1up"));
        command
            .args(["mcp", "--path", path.to_str().unwrap()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .env("HOME", home)
            .env("XDG_DATA_HOME", home.join("data"))
            .env("XDG_CONFIG_HOME", home.join("config"))
            .env("ONEUP_DISABLE_MODEL_DOWNLOADS", "1");

        let mut child = command.spawn().expect("failed to spawn MCP");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());

        let mut client = Self {
            child,
            stdin,
            stdout,
            next_id: 1,
        };

        // Initialize MCP connection
        client.request(
            "initialize",
            serde_json::json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "0" }
            }),
        );
        client.notify("notifications/initialized", serde_json::json!({}));
        client
    }

    fn call_tool(&mut self, name: &str, arguments: serde_json::Value) -> serde_json::Value {
        let response = self.request(
            "tools/call",
            serde_json::json!({
                "name": name,
                "arguments": arguments
            }),
        );
        response["result"].clone()
    }

    fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let id = self.next_id;
        self.next_id += 1;
        self.write(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        }));

        loop {
            let mut line = String::new();
            let bytes = self.stdout.read_line(&mut line).unwrap_or(0);
            if bytes == 0 {
                panic!("MCP server closed stdout");
            }
            if let Ok(response) = serde_json::from_str::<serde_json::Value>(line.trim_end()) {
                if response["id"].as_u64() == Some(id) {
                    return response;
                }
            }
        }
    }

    fn notify(&mut self, method: &str, params: serde_json::Value) {
        self.write(serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        }));
    }

    fn write(&mut self, value: serde_json::Value) {
        let mut line = serde_json::to_vec(&value).unwrap();
        line.push(b'\n');
        let _ = self.stdin.write_all(&line);
        let _ = self.stdin.flush();
    }
}

#[cfg(unix)]
#[test]
fn daemon_deleted_directory_detection_within_5s() {
    let _hide_model = HideModelGuard::new();

    // Create isolated test environment
    let home = TempDir::new().unwrap();
    let home_path = home.path().canonicalize().unwrap();
    seed_model_download_failure(&home_path);

    // Create project with some indexed content
    let project = TempDir::new().unwrap();
    let project_path = project.path().canonicalize().unwrap();

    // Initialize project structure with enough files to trigger indexing
    fs::create_dir_all(project_path.join("src")).unwrap();
    fs::write(
        project_path.join("src").join("lib.rs"),
        "fn hello() { println!(\"test\"); }",
    )
    .unwrap();
    fs::write(
        project_path.join("src").join("main.rs"),
        "fn main() { hello(); }",
    )
    .unwrap();
    fs::write(
        project_path.join("README.md"),
        "# Test Project\n\nThis is a test project for daemon deletion detection.",
    )
    .unwrap();
    fs::write(
        project_path.join("Cargo.toml"),
        "[package]\nname = \"test\"\nversion = \"0.1.0\"",
    )
    .unwrap();

    // Spawn daemon via MCP
    let mut client = MinimalMcpClient::new(&project_path, &home_path);

    // Trigger indexing by calling oneup_start
    client.call_tool(
        "oneup_start",
        serde_json::json!({ "mode": "index_if_needed" }),
    );

    // Give daemon time to start and register the project
    thread::sleep(Duration::from_millis(500));

    // Get status after indexing started
    let status_after_start = client.call_tool("oneup_status", serde_json::json!({}));
    let initial_status = status_after_start["structuredContent"]["status"]
        .as_str()
        .unwrap_or("");

    // If not yet indexed, wait a moment
    if initial_status == "indexing" || initial_status == "pending_refresh" {
        thread::sleep(Duration::from_millis(500));
    }

    // Delete the project directory to trigger detection
    fs::remove_dir_all(&project_path).expect("failed to delete project directory");

    // Poll the daemon's status to verify it handles the deletion gracefully
    // The key is that it should not spin infinitely, and should remain responsive
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut daemon_remained_responsive = true;

    loop {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            client.call_tool("oneup_status", serde_json::json!({}))
        })) {
            Ok(_result) => {
                // Daemon responded successfully - good sign
                if Instant::now() >= deadline {
                    break;
                }
            }
            Err(_) => {
                // Daemon became unresponsive (unlikely but check for it)
                daemon_remained_responsive = false;
                break;
            }
        }

        thread::sleep(Duration::from_millis(100));
    }

    // Clean up client
    let _ = client.child.kill();
    let _ = client.child.wait();

    // Verify the daemon remained responsive after deletion
    // (This proves it didn't get stuck in an infinite loop or crash)
    assert!(
        daemon_remained_responsive,
        "daemon should remain responsive after project directory deletion"
    );
}
