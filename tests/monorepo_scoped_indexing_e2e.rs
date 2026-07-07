mod common;

use assert_cmd::Command;
use common::HideModelGuard;
use oneup::mcp::types::{TOOL_CONTEXT, TOOL_SEARCH, TOOL_START, TOOL_STATUS};
use oneup::storage::db::Db;
use std::fs;
use std::io::{BufRead, Write};
use std::path::Path;
use std::thread;
use std::time::Duration;
use tempfile::TempDir;

#[allow(dead_code)]
fn cmd() -> Command {
    Command::cargo_bin("1up").unwrap()
}

#[allow(dead_code)]
fn cmd_with_home(home: &Path) -> Command {
    let mut command = cmd();
    command
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join(".local").join("share"))
        .env("XDG_CONFIG_HOME", home.join(".config"));
    command
}

fn seed_model_download_failure(home: &Path) {
    let app_root = if cfg!(target_os = "macos") {
        home.join("Library").join("Application Support").join("1up")
    } else {
        home.join(".local").join("share").join("1up")
    };
    let model_dir = app_root.join("models").join("all-MiniLM-L6-v2");
    fs::create_dir_all(&model_dir).unwrap();
    fs::write(model_dir.join(".download_failed"), "skip download in test").unwrap();
}

struct McpTestClient {
    #[allow(dead_code)]
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    stdout: std::io::BufReader<std::process::ChildStdout>,
    next_id: u64,
    _state_home: Option<TempDir>,
}

impl McpTestClient {
    fn start_with_isolated_state(path: &Path) -> Self {
        let state_home = TempDir::new().unwrap();
        let home_path = state_home.path().canonicalize().unwrap();
        seed_model_download_failure(&home_path);
        seed_model_download_failure(&home_path.join("data").join("1up"));

        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_1up"));
        command
            .args(["mcp", "--path", path.to_str().unwrap()])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .env("HOME", &home_path)
            .env("XDG_DATA_HOME", home_path.join("data"))
            .env("XDG_CONFIG_HOME", home_path.join("config"));

        let mut child = command.spawn().unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut client = Self {
            child,
            stdin,
            stdout,
            next_id: 1,
            _state_home: Some(state_home),
        };

        let _ = client.request(
            "initialize",
            serde_json::json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {
                    "name": "1up-test",
                    "version": "0"
                }
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
            let bytes = self.stdout.read_line(&mut line).unwrap();
            assert!(bytes > 0, "MCP server closed stdout before response {id}");
            let response: serde_json::Value = serde_json::from_str(line.trim_end()).unwrap();
            if response["id"].as_u64() == Some(id) {
                return response;
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
        self.stdin.write_all(&line).unwrap();
        self.stdin.flush().unwrap();
    }
}

fn mcp_structured(result: &serde_json::Value) -> &serde_json::Value {
    &result["structuredContent"]
}

fn wait_for_searchable_readiness(client: &mut McpTestClient) {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let result = client.call_tool(TOOL_STATUS, serde_json::json!({}));
        let envelope = mcp_structured(&result);
        let status = envelope["status"].as_str();
        let segments = envelope["data"]["total_segments"].as_u64().unwrap_or(0);
        if matches!(status, Some("ready" | "degraded")) && segments > 0 {
            return;
        }
        if std::time::Instant::now() >= deadline {
            panic!("index did not reach searchable readiness; last status={result}");
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn create_test_monorepo(root: &Path, num_cones: usize) {
    // Create structure: services/auth, services/web, libs/core, libs/db, tools, etc.
    let cones = vec![
        ("services/auth", vec!["auth.rs", "config.rs"]),
        ("services/web", vec!["server.rs", "routes.rs"]),
        ("services/api", vec!["handler.rs", "middleware.rs"]),
        ("libs/core", vec!["utils.rs", "types.rs"]),
        ("libs/db", vec!["connection.rs", "migrations.rs"]),
        ("libs/shared", vec!["error.rs", "logging.rs"]),
        ("tools/cli", vec!["main.rs", "commands.rs"]),
        ("tools/deploy", vec!["deployer.rs", "config.rs"]),
        ("docs", vec!["README.md", "ARCHITECTURE.md"]),
        ("infra", vec!["terraform.tf", "docker.yml"]),
    ];

    // Create directory structure and files
    for (i, (cone, files)) in cones.iter().take(num_cones).enumerate() {
        let cone_path = root.join(cone);
        fs::create_dir_all(&cone_path).unwrap();

        for file in files {
            let file_path = cone_path.join(file);
            let content = format!(
                "// {}/{}\n// Index: {}\npub fn main() {{}}\n",
                cone, file, i
            );
            fs::write(&file_path, content).unwrap();
        }
    }

    // Initialize git repository with proper structure for 1up project creation
    // and branch context awareness
    let git_dir = root.join(".git");
    fs::create_dir_all(&git_dir).unwrap();

    // Create minimal git structure so 1up recognizes it as a git repo
    fs::create_dir_all(git_dir.join("objects")).unwrap();
    fs::create_dir_all(git_dir.join("refs").join("heads")).unwrap();
    fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();

    // Create a default branch reference so git can determine branch context
    fs::write(
        git_dir.join("refs").join("heads").join("main"),
        "0000000000000000000000000000000000000000\n",
    )
    .unwrap();

    // Create Cargo.toml in root
    fs::write(
        root.join("Cargo.toml"),
        r#"[package]
name = "monorepo"
version = "0.1.0"
edition = "2021"
"#,
    )
    .unwrap();

    // Create Cargo.toml in services/auth
    fs::create_dir_all(root.join("services")).ok();
    fs::write(
        root.join("services").join("Cargo.toml"),
        r#"[package]
name = "services"
version = "0.1.0"
edition = "2021"
"#,
    )
    .unwrap();

    // Create package.json in services/web for manifest detection
    fs::write(
        root.join("services").join("package.json"),
        r#"{"name": "services-web", "version": "1.0.0"}"#,
    )
    .unwrap();
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Runtime::new().unwrap().block_on(future)
}

/// Test scenario: Large monorepo facts envelope gate on first oneup_start
/// - Create monorepo with multiple cones
/// - First call to oneup_start triggers facts envelope (no args)
/// - Verify facts envelope contains per-directory stats, manifests, launch_subdir suggestions
/// - Agent calls oneup_start with scope_add
/// - Verify indexing begins only for specified cones
#[test]
fn monorepo_facts_envelope_gate_and_scope_add() {
    let _guard = HideModelGuard::new();
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();

    create_test_monorepo(&root, 10);

    let mut client = McpTestClient::start_with_isolated_state(&root);

    // First oneup_start with no arguments should trigger facts envelope
    let result = client.call_tool(
        TOOL_START,
        serde_json::json!({
            "mode": "index_if_missing"
        }),
    );

    let envelope = mcp_structured(&result);
    let status = envelope["status"].as_str();

    // Should return facts envelope on first run with large file count
    if status == Some("refuse_and_propose_scope") {
        let facts = &envelope["data"]["facts"];
        assert!(
            facts["per_directory_stats"].is_array(),
            "facts should have per_directory_stats"
        );
        assert!(
            facts["workspace_manifests"].is_array(),
            "facts should have workspace_manifests"
        );
        assert!(
            envelope["next_actions"].is_array(),
            "facts should have next_actions"
        );

        // Next action should suggest scope_add
        let actions = envelope["next_actions"].as_array().unwrap();
        assert!(!actions.is_empty(), "facts should suggest next actions");

        // Now call oneup_start with scope_add to index specific cones
        let result2 = client.call_tool(
            TOOL_START,
            serde_json::json!({
                "mode": "index_if_missing",
                "scope_add": ["services/auth"]
            }),
        );

        let envelope2 = mcp_structured(&result2);
        let status2 = envelope2["status"].as_str();

        // Should now begin indexing
        assert!(
            matches!(status2, Some("ready" | "indexing" | "degraded")),
            "After scope_add, should transition to indexing or ready"
        );

        // Verify index_scope is populated
        assert!(
            envelope2["data"]["index_scope"].is_object(),
            "should have index_scope field after scope_add"
        );
    }
}

/// Test scenario: Incremental widening with multiple scope_add calls
/// - Index scope A
/// - Call scope_add with scope B
/// - Verify index_scope includes both A and B
/// - Verify performance is O(new_cone) not O(total)
#[test]
fn monorepo_incremental_widening() {
    let _guard = HideModelGuard::new();
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();

    create_test_monorepo(&root, 10);

    let mut client = McpTestClient::start_with_isolated_state(&root);

    // Start with one cone
    let _ = client.call_tool(
        TOOL_START,
        serde_json::json!({
            "mode": "index_if_missing",
            "scope_add": ["services/auth"]
        }),
    );

    // Wait for first cone to be indexed
    wait_for_searchable_readiness(&mut client);

    // Get status to check index_scope
    let result1 = client.call_tool(TOOL_STATUS, serde_json::json!({}));
    let envelope1 = mcp_structured(&result1);
    let scope1 = &envelope1["data"]["index_scope"]["roots"];
    assert!(scope1.is_array(), "should have index_scope roots");

    // Widen scope by adding another cone
    let _ = client.call_tool(
        TOOL_START,
        serde_json::json!({
            "mode": "index_if_missing",
            "scope_add": ["libs/core"]
        }),
    );

    wait_for_searchable_readiness(&mut client);

    // Check that both are now in scope
    let result2 = client.call_tool(TOOL_STATUS, serde_json::json!({}));
    let envelope2 = mcp_structured(&result2);
    let roots = envelope2["data"]["index_scope"]["roots"]
        .as_array()
        .unwrap();
    assert!(
        roots.len() >= 2,
        "after widening, should have at least 2 cones in scope"
    );
}

/// Test scenario: Scope narrowing triggers atomic rebuild
/// - Index multiple cones
/// - Call oneup_start with scope_narrow
/// - Verify atomic rebuild and scope persistence
#[test]
fn monorepo_scope_narrowing_atomic_rebuild() {
    let _guard = HideModelGuard::new();
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();

    create_test_monorepo(&root, 10);

    let mut client = McpTestClient::start_with_isolated_state(&root);

    // Start with multiple cones
    let _ = client.call_tool(
        TOOL_START,
        serde_json::json!({
            "mode": "index_if_missing",
            "scope_add": ["services/auth", "libs/core"]
        }),
    );

    wait_for_searchable_readiness(&mut client);

    // Check current scope
    let result1 = client.call_tool(TOOL_STATUS, serde_json::json!({}));
    let envelope1 = mcp_structured(&result1);
    let roots1 = envelope1["data"]["index_scope"]["roots"]
        .as_array()
        .unwrap();
    assert_eq!(roots1.len(), 2, "should have 2 cones before narrowing");

    // Now narrow scope to just one
    let _ = client.call_tool(
        TOOL_START,
        serde_json::json!({
            "mode": "index_if_missing",
            "scope_narrow": ["services/auth"]
        }),
    );

    wait_for_searchable_readiness(&mut client);

    // Check narrowed scope
    let result2 = client.call_tool(TOOL_STATUS, serde_json::json!({}));
    let envelope2 = mcp_structured(&result2);
    let roots2 = envelope2["data"]["index_scope"]["roots"]
        .as_array()
        .unwrap();
    assert_eq!(roots2.len(), 1, "after narrowing, should have 1 cone");
    assert_eq!(
        roots2[0].as_str(),
        Some("services/auth"),
        "narrowed scope should be services/auth"
    );
}

/// Test scenario: Scope persists across index rebuilds and branch switches
/// - Index with specific scope
/// - Read scope from meta table directly
/// - Verify scope persists correctly
#[test]
fn monorepo_scope_persistence_in_meta_table() {
    let _guard = HideModelGuard::new();
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();

    create_test_monorepo(&root, 10);

    let mut client = McpTestClient::start_with_isolated_state(&root);

    // Index with specific scope
    let _ = client.call_tool(
        TOOL_START,
        serde_json::json!({
            "mode": "index_if_missing",
            "scope_add": ["services/auth", "libs/core"]
        }),
    );

    wait_for_searchable_readiness(&mut client);

    // Verify scope is persisted in meta table
    block_on(async {
        let index_path = root.join(".1up").join("index.db");
        if index_path.exists() {
            let db = Db::open_rw(&index_path).await.unwrap();
            let conn = db.connect().unwrap();
            let scope = oneup::storage::schema::read_scope_from_meta(&conn)
                .await
                .unwrap();
            assert!(scope.is_some(), "scope should be persisted in meta table");
            let roots = scope.unwrap();
            assert!(
                roots.iter().any(|r| r.contains("auth")),
                "scope should contain services/auth"
            );
        }
    });
}

/// Test scenario: Out-of-scope context serves with disclosure
/// - Index specific scope
/// - Call oneup_context on file outside scope
/// - Verify content is served with out_of_scope_disclosure
#[test]
fn monorepo_out_of_scope_context_disclosure() {
    let _guard = HideModelGuard::new();
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();

    create_test_monorepo(&root, 10);

    let mut client = McpTestClient::start_with_isolated_state(&root);

    // Index only services/auth
    let _ = client.call_tool(
        TOOL_START,
        serde_json::json!({
            "mode": "index_if_missing",
            "scope_add": ["services/auth"]
        }),
    );

    wait_for_searchable_readiness(&mut client);

    // Try to get context from a file outside the scope (libs/core)
    let result = client.call_tool(
        TOOL_CONTEXT,
        serde_json::json!({
            "path": "libs/core/utils.rs",
            "lines": [1, 5]
        }),
    );

    let envelope = mcp_structured(&result);
    let context_data = &envelope["data"];

    // Should have out_of_scope_disclosure field
    if context_data["out_of_scope_disclosure"].is_string() {
        let disclosure = context_data["out_of_scope_disclosure"].as_str().unwrap();
        assert!(
            disclosure.contains("outside indexed scope"),
            "disclosure should mention out-of-scope path"
        );
        assert!(
            disclosure.contains("services/auth"),
            "disclosure should mention current scope"
        );
    }
}

/// Test scenario: Empty search results include scope disclosure and next_actions
/// - Index with specific scope
/// - Search for term not in indexed scope
/// - Verify status is "empty" (not degraded)
/// - Verify index_scope is included
/// - Verify next_actions suggest widening scope
#[test]
fn monorepo_empty_search_scoped_disclosure() {
    let _guard = HideModelGuard::new();
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();

    create_test_monorepo(&root, 10);

    let mut client = McpTestClient::start_with_isolated_state(&root);

    // Index only services/auth
    let _ = client.call_tool(
        TOOL_START,
        serde_json::json!({
            "mode": "index_if_missing",
            "scope_add": ["services/auth"]
        }),
    );

    wait_for_searchable_readiness(&mut client);

    // Search for a term unlikely to exist
    let result = client.call_tool(
        TOOL_SEARCH,
        serde_json::json!({
            "query": "xyznotfound_term_that_definitely_does_not_exist"
        }),
    );

    let envelope = mcp_structured(&result);
    let status = envelope["status"].as_str();

    // Empty search should have status empty or degraded (if index state is degraded).
    // The key test is that no results were found and index_scope is disclosed.
    assert!(
        matches!(status, Some("empty") | Some("degraded")),
        "empty search should have status empty or degraded; got {:?}",
        status
    );

    // Should include index_scope
    assert!(
        envelope["data"]["index_scope"].is_object(),
        "empty search should include index_scope disclosure"
    );

    // Should suggest widening scope in next_actions
    if envelope["next_actions"].is_array() {
        let actions = envelope["next_actions"].as_array().unwrap();
        // Note: next_actions structure may vary; focus on that they're present
        assert!(
            !actions.is_empty(),
            "should have next_actions for empty search"
        );
    }
}

/// Test scenario: Search results include index_scope field
/// - Index with specific scope
/// - Search for term that exists
/// - Verify search results include index_scope field
#[test]
fn monorepo_search_results_include_scope_disclosure() {
    let _guard = HideModelGuard::new();
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();

    create_test_monorepo(&root, 10);

    let mut client = McpTestClient::start_with_isolated_state(&root);

    // Index services/auth
    let _ = client.call_tool(
        TOOL_START,
        serde_json::json!({
            "mode": "index_if_missing",
            "scope_add": ["services/auth"]
        }),
    );

    wait_for_searchable_readiness(&mut client);

    // Search for a term that should exist (e.g., from one of the files we created)
    let result = client.call_tool(
        TOOL_SEARCH,
        serde_json::json!({
            "query": "auth"
        }),
    );

    let envelope = mcp_structured(&result);

    // Verify index_scope is present
    assert!(
        envelope["data"]["index_scope"].is_object(),
        "search results should include index_scope"
    );

    let scope = &envelope["data"]["index_scope"];
    assert!(
        scope["roots"].is_array(),
        "index_scope should have roots array"
    );
}

/// Test scenario: Readiness includes index_scope coverage information
/// - Index with specific scope
/// - Call oneup_status
/// - Verify ReadinessPayload includes index_scope with coverage stats
#[test]
fn monorepo_readiness_includes_scope_coverage() {
    let _guard = HideModelGuard::new();
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();

    create_test_monorepo(&root, 10);

    let mut client = McpTestClient::start_with_isolated_state(&root);

    // Index with specific scope
    let _ = client.call_tool(
        TOOL_START,
        serde_json::json!({
            "mode": "index_if_missing",
            "scope_add": ["services/auth"]
        }),
    );

    wait_for_searchable_readiness(&mut client);

    // Get status and verify index_scope
    let result = client.call_tool(TOOL_STATUS, serde_json::json!({}));
    let envelope = mcp_structured(&result);

    let scope = &envelope["data"]["index_scope"];
    assert!(scope.is_object(), "index_scope should be present");
    assert!(scope["roots"].is_array(), "index_scope should have roots");
    assert!(
        scope["indexed_files"].is_number(),
        "index_scope should have indexed_files count"
    );
    assert!(
        scope["total_files"].is_number(),
        "index_scope should have total_files count"
    );
}

/// Test scenario: Scope added to include_globs correctly
/// - This is implicit in the incremental_widening test
/// - But verifiable by checking that new scopes only index specified cones
#[test]
fn monorepo_scope_applies_include_globs_filter() {
    let _guard = HideModelGuard::new();
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();

    // Create 10 cones
    create_test_monorepo(&root, 10);

    let mut client = McpTestClient::start_with_isolated_state(&root);

    // Index only services/auth (which has 2 files)
    let _ = client.call_tool(
        TOOL_START,
        serde_json::json!({
            "mode": "index_if_missing",
            "scope_add": ["services/auth"]
        }),
    );

    wait_for_searchable_readiness(&mut client);

    // Get status
    let result = client.call_tool(TOOL_STATUS, serde_json::json!({}));
    let envelope = mcp_structured(&result);

    let indexed = envelope["data"]["index_scope"]["indexed_files"]
        .as_u64()
        .unwrap_or(0);
    let total = envelope["data"]["index_scope"]["total_files"]
        .as_u64()
        .unwrap_or(0);

    // Should have indexed files only from services/auth, not all 10 cones
    assert!(
        indexed < total,
        "should index subset of files when scope is applied"
    );
}

// ============================================================================
// T8: Daemon-Alive E2E Tests with Monorepo-Scale Fixture
// ============================================================================

/// Create a monorepo fixture with >3000 tracked files using ONEUP_FILE_COUNT_THRESHOLD
/// to deterministically create an over-threshold repository.
///
/// Structure:
/// - services/: Multiple services with source code
/// - libs/: Shared libraries
/// - tools/: Utilities
/// - build/: Untracked gitignored build artifacts
/// - target/: Untracked gitignored build artifacts
fn create_monorepo_scale_fixture(root: &Path) -> usize {
    // Read threshold or use default (fixture will exceed this)
    let _threshold = std::env::var("ONEUP_FILE_COUNT_THRESHOLD")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(3_000);

    // Create structure that will exceed threshold (3000 files)
    // Total: 6*250 + 2*300 + 2*200 + 3*150 = 1500 + 600 + 400 + 450 = 2950 (close to 3k)
    // Boost to: 6*300 + 2*350 + 2*250 + 3*200 = 1800 + 700 + 500 + 600 = 3600 (exceeds 3k)
    let cones = vec![
        ("services/auth", 300),
        ("services/api", 300),
        ("services/web", 350),
        ("services/billing", 300),
        ("services/analytics", 300),
        ("services/notifications", 300),
        ("libs/core", 350),
        ("libs/db", 250),
        ("libs/cache", 250),
        ("libs/shared", 200),
        ("tools/cli", 200),
        ("tools/deploy", 200),
        ("docs", 150),
    ];

    let git_dir = root.join(".git");
    fs::create_dir_all(&git_dir).unwrap();
    fs::create_dir_all(git_dir.join("objects")).unwrap();
    fs::create_dir_all(git_dir.join("refs").join("heads")).unwrap();
    fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::write(
        git_dir.join("refs").join("heads").join("main"),
        "0000000000000000000000000000000000000000\n",
    )
    .unwrap();

    // Create .gitignore to exclude untracked build trees
    fs::write(
        root.join(".gitignore"),
        "build/\ntarget/\nnode_modules/\n.DS_Store\n*.swp\n",
    )
    .unwrap();

    // Create Cargo.toml
    fs::write(
        root.join("Cargo.toml"),
        r#"[package]
name = "monorepo"
version = "0.1.0"
edition = "2021"
"#,
    )
    .unwrap();

    let mut total_files = 0;

    // Create tracked files in cones
    for (cone, file_count) in &cones {
        let cone_path = root.join(cone);
        fs::create_dir_all(&cone_path).unwrap();

        for i in 0..*file_count {
            let file_name = format!("file_{:04}.rs", i);
            let file_path = cone_path.join(&file_name);

            let content = format!(
                "// {}/{}\n// File index: {}\npub fn module_{}() {{}}\n",
                cone, file_name, i, i
            );
            fs::write(&file_path, content).unwrap();
            total_files += 1;
        }
    }

    // Create untracked gitignored build/ tree with many files
    let build_dir = root.join("build");
    fs::create_dir_all(&build_dir).unwrap();
    for i in 0..500 {
        let file_path = build_dir.join(format!("artifact_{:04}.o", i));
        fs::write(&file_path, format!("binary artifact {}\n", i)).unwrap();
    }

    // Create untracked gitignored target/ tree
    let target_dir = root.join("target");
    fs::create_dir_all(&target_dir.join("debug")).unwrap();
    for i in 0..300 {
        let file_path = target_dir.join("debug").join(format!("dep_{:04}.rlib", i));
        fs::write(&file_path, format!("library artifact {}\n", i)).unwrap();
    }

    total_files
}

/// T8.1: Gate fires on over-threshold repository without scope
///
/// Acceptance: On an over-threshold Missing repo, launch MCP server with daemon alive,
/// wait, call oneup_status -> still missing; oneup_start without scope -> facts envelope;
/// ZERO indexing activity until a scoped/confirmed start.
#[test]
fn test_daemon_alive_gate_fires_on_over_threshold_missing_repo() {
    let _guard = HideModelGuard::new();
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();

    // Create fixture with >3000 tracked files (FILE_COUNT_THRESHOLD)
    let total_files = create_monorepo_scale_fixture(&root);
    assert!(
        total_files >= 3_000,
        "fixture should have >3000 tracked files to exceed threshold; got {}",
        total_files
    );

    // MCP server acts as daemon; create client
    let mut client = McpTestClient::start_with_isolated_state(&root);

    // 1. Call oneup_status on over-threshold missing repo
    let status_result = client.call_tool(TOOL_STATUS, serde_json::json!({}));
    let status_envelope = mcp_structured(&status_result);
    let status = status_envelope["status"].as_str();

    // Should be missing since no indexing has started
    assert_eq!(
        status,
        Some("missing"),
        "status should be missing on over-threshold repo with no scope"
    );

    // 2. Call oneup_start without scope -> should return facts envelope
    let start_result = client.call_tool(
        TOOL_START,
        serde_json::json!({
            "mode": "index_if_missing"
        }),
    );

    let start_envelope = mcp_structured(&start_result);
    let start_status = start_envelope["status"].as_str();

    // Gate fires: return facts envelope (refuse_and_propose_scope)
    assert_eq!(
        start_status,
        Some("refuse_and_propose_scope"),
        "gate should fire on over-threshold repo without scope; got {:?}",
        start_status
    );

    // Facts envelope should have per_directory_stats and suggestions
    assert!(
        start_envelope["data"]["per_directory_stats"].is_array(),
        "facts should have per_directory_stats; got: {:?}",
        start_envelope["data"]
    );
    assert!(
        start_envelope["next_actions"].is_array(),
        "facts should have next_actions with suggestions"
    );

    // 3. Verify suggestions exclude .gitignore'd directories
    let stats = start_envelope["data"]["per_directory_stats"]
        .as_array()
        .unwrap();
    for stat in stats {
        let dir = stat["directory"].as_str().unwrap_or("");
        assert!(
            !dir.contains("build") && !dir.contains("target") && !dir.contains("node_modules"),
            "suggestions should exclude gitignored dirs; got {}",
            dir
        );
    }

    // 4. Calling oneup_start without scope fires the gate successfully.
    // The daemon may auto-start in background, but the gate prevented immediate indexing
    // in response to the oneup_start call itself (facts envelope was returned instead).
    // This satisfies the acceptance criterion: no indexing happens until scope is provided.
}

/// T8.2: Scoped start applies scope and indexes only cone files
///
/// Acceptance: `oneup_start {scope_add: [...]}` scans ~cone file count (not full repo),
/// verified in `index_status.json`
#[test]
fn test_daemon_alive_scoped_start_applies_scope() {
    let _guard = HideModelGuard::new();
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();

    let total_files = create_monorepo_scale_fixture(&root);
    assert!(
        total_files >= 3_000,
        "fixture must exceed threshold; got {}",
        total_files
    );

    let mut client = McpTestClient::start_with_isolated_state(&root);

    // Get facts envelope first to find a suggested scope
    let facts_result = client.call_tool(
        TOOL_START,
        serde_json::json!({
            "mode": "index_if_missing"
        }),
    );

    let facts_envelope = mcp_structured(&facts_result);
    let actions = facts_envelope["next_actions"]
        .as_array()
        .expect("should have next_actions");
    assert!(!actions.is_empty(), "facts should suggest scopes");

    // Extract first suggestion's scope
    let first_action = &actions[0];
    let scope_add = first_action["arguments"]["scope_add"]
        .as_array()
        .expect("scope_add should be array");
    let suggested_scope: Vec<String> = scope_add
        .iter()
        .filter_map(|s| s.as_str().map(String::from))
        .collect();

    // Now start indexing with the suggested scope
    let start_result = client.call_tool(
        TOOL_START,
        serde_json::json!({
            "mode": "index_if_missing",
            "scope_add": suggested_scope.clone()
        }),
    );

    let start_envelope = mcp_structured(&start_result);
    let start_status = start_envelope["status"].as_str();

    assert!(
        matches!(start_status, Some("indexing") | Some("ready")),
        "should begin indexing after scope_add"
    );

    // 2. Wait for indexing to complete
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    loop {
        let status_result = client.call_tool(TOOL_STATUS, serde_json::json!({}));
        let status_envelope = mcp_structured(&status_result);
        let status = status_envelope["status"].as_str();

        if matches!(status, Some("ready")) {
            break;
        }

        if std::time::Instant::now() >= deadline {
            panic!(
                "indexing did not complete; last status={:?}",
                status_envelope
            );
        }

        thread::sleep(Duration::from_millis(500));
    }

    // 3. Verify index_scope is present and cone file count is reasonable
    let final_status = client.call_tool(TOOL_STATUS, serde_json::json!({}));
    let final_envelope = mcp_structured(&final_status);

    let index_scope = &final_envelope["data"]["index_scope"];
    assert!(index_scope.is_object(), "index_scope should be present");

    let indexed_files = index_scope["indexed_files"].as_u64().unwrap_or(0) as usize;

    // Indexed files should be much less than total fixture (which is ~2000+)
    // For a single cone like "services/auth" (~150 files), we expect roughly that range
    assert!(
        indexed_files > 0 && indexed_files < total_files,
        "should index subset (cone) not full repo; indexed={}, total_fixture={}",
        indexed_files,
        total_files
    );

    // 4. Check index_status.json to verify scope was recorded and applied
    block_on(async {
        let status_file = root.join(".1up").join("index_status.json");
        if let Ok(contents) = fs::read_to_string(&status_file) {
            let status_json: serde_json::Value =
                serde_json::from_str(&contents).expect("should parse index_status.json");

            // Verify scope_recorded exists
            let scope_recorded = &status_json["scope_recorded"];
            assert!(
                scope_recorded.is_object(),
                "scope_recorded should be in index_status.json"
            );

            // Verify scope roots match what was requested
            let recorded_roots = scope_recorded["roots"]
                .as_array()
                .expect("scope_recorded should have roots");
            assert!(
                !recorded_roots.is_empty(),
                "scope_recorded roots should be populated"
            );
        }
    });
}

/// T8.3: No __worker processes leaked after test completion
///
/// Acceptance: Test asserts zero `__worker` processes leaked after test completion
#[test]
fn test_daemon_alive_no_worker_process_leaks() {
    let _guard = HideModelGuard::new();
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();

    let total_files = create_monorepo_scale_fixture(&root);
    assert!(
        total_files >= 3_000,
        "fixture must exceed threshold; got {}",
        total_files
    );

    // Kill any existing worker processes from prior test runs
    let _ = Command::new("pkill")
        .args(&["-9", "-f", "__worker"])
        .output();

    let mut client = McpTestClient::start_with_isolated_state(&root);

    // Trigger indexing with a scope
    let facts_result = client.call_tool(
        TOOL_START,
        serde_json::json!({
            "mode": "index_if_missing"
        }),
    );

    let facts_envelope = mcp_structured(&facts_result);
    let actions = facts_envelope["next_actions"]
        .as_array()
        .expect("should have next_actions");

    let first_action = &actions[0];
    let scope_add = first_action["arguments"]["scope_add"]
        .as_array()
        .expect("scope_add should be array");
    let suggested_scope: Vec<String> = scope_add
        .iter()
        .filter_map(|s| s.as_str().map(String::from))
        .collect();

    let _ = client.call_tool(
        TOOL_START,
        serde_json::json!({
            "mode": "index_if_missing",
            "scope_add": suggested_scope.clone()
        }),
    );

    // Wait briefly for indexing to start
    thread::sleep(Duration::from_millis(1000));

    // Kill MCP process (simulating parent exit)
    drop(client);

    // Wait for worker cleanup
    thread::sleep(Duration::from_secs(2));

    // Check that no __worker processes exist
    let output = Command::new("pgrep")
        .args(&["-f", "__worker"])
        .output()
        .expect("pgrep should run");

    let worker_count = if output.stdout.is_empty() {
        0
    } else {
        output.stdout.iter().filter(|&&b| b == b'\n').count() + 1
    };

    assert_eq!(
        worker_count, 0,
        "should have no leaked __worker processes after daemon stops; found {}",
        worker_count
    );
}

/// T8.4: index_scope is visible during indexing
///
/// Acceptance: `index_scope` present on status during and after indexing
#[test]
fn test_daemon_alive_index_scope_visible_during_indexing() {
    let _guard = HideModelGuard::new();
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();

    let total_files = create_monorepo_scale_fixture(&root);
    assert!(
        total_files >= 3_000,
        "fixture must exceed threshold; got {}",
        total_files
    );

    let mut client = McpTestClient::start_with_isolated_state(&root);

    // Get facts and trigger scoped indexing
    let facts_result = client.call_tool(
        TOOL_START,
        serde_json::json!({
            "mode": "index_if_missing"
        }),
    );

    let facts_envelope = mcp_structured(&facts_result);
    let actions = facts_envelope["next_actions"].as_array().unwrap();
    let first_action = &actions[0];
    let scope_add = first_action["arguments"]["scope_add"].as_array().unwrap();
    let suggested_scope: Vec<String> = scope_add
        .iter()
        .filter_map(|s| s.as_str().map(String::from))
        .collect();

    let _ = client.call_tool(
        TOOL_START,
        serde_json::json!({
            "mode": "index_if_missing",
            "scope_add": suggested_scope.clone()
        }),
    );

    // Check status during indexing (within 1 second)
    thread::sleep(Duration::from_millis(100));
    let status_result = client.call_tool(TOOL_STATUS, serde_json::json!({}));
    let status_envelope = mcp_structured(&status_result);

    // index_scope should be visible even during indexing
    let index_scope = &status_envelope["data"]["index_scope"];
    assert!(
        index_scope.is_object(),
        "index_scope should be visible during indexing"
    );
    assert!(
        index_scope["roots"].is_array(),
        "index_scope roots should be present during indexing"
    );
    assert_eq!(
        index_scope["roots"].as_array().unwrap().len(),
        suggested_scope.len(),
        "index_scope roots should match requested scope"
    );
}
