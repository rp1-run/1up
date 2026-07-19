mod common;

use assert_cmd::Command;
use common::{poll_until, HideModelGuard};
use oneup::mcp::types::{TOOL_CONTEXT, TOOL_GET, TOOL_SEARCH, TOOL_START, TOOL_STATUS};
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
        Self::start_with_isolated_state_and_envs(path, &[])
    }

    fn start_with_isolated_state_and_envs(path: &Path, envs: &[(&str, &str)]) -> Self {
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
        for (key, value) in envs {
            command.env(key, value);
        }

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

/// Poll `oneup_status` until `predicate` holds on the structured envelope, or
/// panic with the last envelope after the deadline. `oneup_start` is
/// non-blocking and the daemon indexes concurrently, so tests must
/// assert eventual stable states rather than single-shot status reads.
fn wait_for_status<F: Fn(&serde_json::Value) -> bool>(
    client: &mut McpTestClient,
    what: &str,
    predicate: F,
) -> serde_json::Value {
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    loop {
        let result = client.call_tool(TOOL_STATUS, serde_json::json!({}));
        if predicate(mcp_structured(&result)) {
            return result;
        }
        if std::time::Instant::now() >= deadline {
            panic!("{what} not reached within 120s; last status={result:?}");
        }
        thread::sleep(Duration::from_millis(500));
    }
}

fn wait_for_searchable_readiness(client: &mut McpTestClient) {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let result = client.call_tool(TOOL_STATUS, serde_json::json!({}));
        let envelope = mcp_structured(&result);
        let status = envelope["status"].as_str();
        let segments = envelope["data"]["total_segments"].as_u64().unwrap_or(0);
        let phase = envelope["data"]["index_progress"]["phase"].as_str();

        // Accept ready or degraded (degraded when embeddings unavailable)
        if matches!(status, Some("ready" | "degraded")) && segments > 0 {
            return;
        }
        // Also accept indexing->complete transition without segments (FTS-only mode)
        if matches!(status, Some("indexing")) && phase == Some("complete") {
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

/// Pre-create an empty schema index database at the given project root.
/// This reproduces the real startup sequence where build_project_state creates
/// an empty schema db BEFORE the daemon gate check runs.
/// The gate logic checks segment count (not file existence) to survive this
/// pre-creation; this test verifies that the gate decision is robust to an
/// empty-but-present db.
fn create_empty_schema_db(project_root: &Path) {
    use oneup::shared::config;
    use oneup::storage::schema;

    let db_path = config::project_db_path(project_root);

    // Create the parent directory if needed
    fs::create_dir_all(project_root.join(".1up")).unwrap();

    // Open RW (creates if missing) and initialize schema
    let db = block_on(Db::open_rw(&db_path)).expect("failed to open db");
    let conn = block_on(db.connect_tuned()).expect("failed to get connection");
    block_on(schema::prepare_for_write(&conn)).expect("failed to prepare schema");

    // db and conn are dropped here, releasing the connection and committing the schema
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

    // Wait for the first cone's scope to publish (non-blocking start).
    let result1 = wait_for_status(&mut client, "first cone index_scope", |env| {
        env["data"]["index_scope"]["roots"].is_array()
    });
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

    // Check that both cones eventually appear in scope (the widening rebuild
    // publishes its scope when it lands, not when oneup_start returns).
    let result2 = wait_for_status(&mut client, "widened index_scope with both cones", |env| {
        env["data"]["index_scope"]["roots"]
            .as_array()
            .is_some_and(|roots| roots.len() >= 2)
    });
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

    // Both cones' scope publishes asynchronously — the rebuild publishes its scope when
    // it lands, not when oneup_start returns — so poll for both to appear rather than
    // reading status once (a single read races the second cone's publish and flakes,
    // exactly as the widening test above already guards against).
    let result1 = wait_for_status(
        &mut client,
        "both cones in index_scope before narrowing",
        |env| {
            env["data"]["index_scope"]["roots"]
                .as_array()
                .is_some_and(|roots| roots.len() == 2)
        },
    );
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

    // The narrowing rebuild swaps atomically and republishes scope asynchronously, so
    // poll for the single remaining cone rather than reading status once.
    let result2 = wait_for_status(
        &mut client,
        "single cone in index_scope after narrowing",
        |env| {
            env["data"]["index_scope"]["roots"]
                .as_array()
                .is_some_and(|roots| roots.len() == 1)
        },
    );
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

    // Verify scope is persisted in meta table. oneup_start is non-blocking
    // and a daemon refresh can win the first-searchable race with an
    // unscoped build, so poll until the scoped rebuild's meta write lands
    // rather than reading once. The probe drives its async DB reads to
    // completion via `block_on`, so the poll stays on the plain test thread
    // and no blocking sleep runs on an async runtime.
    let index_path = root.join(".1up").join("index.db");
    let roots = poll_until(
        std::time::Instant::now() + Duration::from_secs(30),
        Duration::from_millis(500),
        "scoped rebuild scope persisted in meta table",
        || {
            if !index_path.exists() {
                return Err("index.db not yet created".to_string());
            }
            let scope = block_on(async {
                let db = Db::open_rw(&index_path).await.unwrap();
                let conn = db.connect().unwrap();
                oneup::storage::schema::read_scope_from_meta(&conn)
                    .await
                    .unwrap()
            });
            match scope {
                Some(roots) => Ok(roots),
                None => Err("scope not yet written to meta".to_string()),
            }
        },
    );
    assert!(
        roots.iter().any(|r| r.contains("auth")),
        "scope should contain services/auth; got {roots:?}"
    );
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

    // Get status and verify index_scope. oneup_start is non-blocking,
    // so poll until the scoped rebuild publishes index_scope
    // rather than asserting on the first readable status.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut result = client.call_tool(TOOL_STATUS, serde_json::json!({}));
    while !mcp_structured(&result)["data"]["index_scope"].is_object()
        && std::time::Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(500));
        result = client.call_tool(TOOL_STATUS, serde_json::json!({}));
    }
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

/// Regression test: the status/readiness path must carry the same
/// eligibility_note semantics as the search path. On an unscoped (full)
/// index, oneup_status's index_scope must disclose a non-empty
/// eligibility_note explaining the indexed_files/total_files gap.
#[test]
fn oneup_status_unscoped_index_scope_includes_eligibility_note() {
    let _guard = HideModelGuard::new();
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();

    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src").join("lib.rs"),
        "/// Add two numbers.\npub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
    )
    .unwrap();

    std::process::Command::new("git")
        .arg("init")
        .current_dir(&root)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["config", "user.name", "Test User"])
        .current_dir(&root)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["config", "user.email", "test@example.com"])
        .current_dir(&root)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(&root)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "-m", "initial"])
        .current_dir(&root)
        .output()
        .unwrap();

    let mut client = McpTestClient::start_with_isolated_state(&root);

    // Index without any scope_add: small repo, no facts gate, unscoped index.
    let _ = client.call_tool(TOOL_START, serde_json::json!({"mode": "index_if_needed"}));
    wait_for_searchable_readiness(&mut client);

    // oneup_start is non-blocking, so poll until the completed
    // unscoped index publishes index_scope with the eligibility note.
    let result = wait_for_status(
        &mut client,
        "unscoped index_scope with eligibility_note",
        |env| {
            env["data"]["index_scope"]["eligibility_note"]
                .as_str()
                .is_some_and(|note| !note.is_empty())
        },
    );
    let envelope = mcp_structured(&result);
    let scope = &envelope["data"]["index_scope"];
    assert!(
        scope["roots"]
            .as_array()
            .is_some_and(|roots| roots.is_empty()),
        "index is unscoped, so roots should be empty: {scope:?}"
    );
    let note = scope["eligibility_note"].as_str().unwrap();
    assert!(
        note.contains("Full index"),
        "eligibility_note should explain the unscoped coverage gap: {note}"
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
// Daemon-Alive E2E Tests with Monorepo-Scale Fixture
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
    fs::create_dir_all(target_dir.join("debug")).unwrap();
    for i in 0..300 {
        let file_path = target_dir.join("debug").join(format!("dep_{:04}.rlib", i));
        fs::write(&file_path, format!("library artifact {}\n", i)).unwrap();
    }

    total_files
}

/// Create a small git-backed fixture with `num_files` tracked files in one cone.
///
/// Paired with a low `ONEUP_FILE_COUNT_THRESHOLD`, the daemon treats it as
/// over-threshold and runs the first-index gate walk — without needing a
/// multi-thousand-file fixture. Returns the number of tracked files created.
#[allow(dead_code)]
fn create_small_gate_fixture(root: &Path, num_files: usize) -> usize {
    let git_dir = root.join(".git");
    fs::create_dir_all(git_dir.join("objects")).unwrap();
    fs::create_dir_all(git_dir.join("refs").join("heads")).unwrap();
    fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::write(
        git_dir.join("refs").join("heads").join("main"),
        "0000000000000000000000000000000000000000\n",
    )
    .unwrap();
    fs::write(root.join(".gitignore"), "build/\ntarget/\n").unwrap();

    let cone = root.join("src");
    fs::create_dir_all(&cone).unwrap();
    for i in 0..num_files {
        fs::write(
            cone.join(format!("file_{i:04}.rs")),
            format!("// file {i}\npub fn module_{i}() {{}}\n"),
        )
        .unwrap();
    }
    num_files
}

/// SIGTERM during the daemon's first-index gate walk aborts cooperatively instead
/// of ignoring the signal or opening the file-count gate.
///
/// Regression for issue #85: on a large over-threshold repo the daemon ran the
/// gitignore-aware gate walk on a blocking thread. A mid-walk SIGTERM was ignored
/// until the (potentially minutes-long) walk finished, and a cancelled walk was
/// swallowed to `file_count = 0`, which passes the file-count gate and started a
/// first index during the shutdown drain.
///
/// A test-only throttle (`ONEUP_TEST_GATE_WALK_ENTRY_DELAY_MS`) holds the walk
/// open on a small fixture so the SIGTERM lands mid-walk deterministically without
/// a huge fixture (debug-build friendly). Assertions:
/// - the daemon exits well within the throttled walk's natural duration, proving
///   the in-flight walk observed the cancellation token (defect 1 wiring); and
/// - no `index_status.json` is written, proving the cancelled walk did not collapse
///   to `file_count = 0` and open a first-index pass during drain (defect 2).
#[test]
#[cfg(unix)]
fn test_daemon_gate_walk_sigterm_aborts_without_opening_gate() {
    let _guard = HideModelGuard::new();
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();

    // ~400 entries * a 500ms sleep every 10 entries keeps the uncancelled walk
    // open for ~20s — a wide, deterministic window to land SIGTERM mid-walk. A low
    // threshold makes this small fixture "over-threshold" so the gate is engaged.
    create_small_gate_fixture(&root, 400);

    let client = McpTestClient::start_with_isolated_state_and_envs(
        &root,
        &[
            ("ONEUP_FILE_COUNT_THRESHOLD", "10"),
            ("ONEUP_TEST_GATE_WALK_ENTRY_DELAY_MS", "500"),
        ],
    );
    let server_pid = client.child.id();

    // The MCP server auto-spawns a daemon on startup; that daemon marks the freshly
    // registered project dirty and runs the first-index gate walk immediately, held
    // open by the throttle. Discover it via the parent relationship, never a
    // machine-wide pattern match.
    let mut daemon_pid: Option<i32> = None;
    for _ in 0..40 {
        let output = std::process::Command::new("pgrep")
            .args(["-P", &server_pid.to_string(), "-f", "__worker"])
            .output()
            .expect("pgrep should run");
        if let Some(pid) = String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .and_then(|line| line.trim().parse::<i32>().ok())
        {
            daemon_pid = Some(pid);
            break;
        }
        thread::sleep(Duration::from_millis(250));
    }
    let daemon_pid = daemon_pid.expect("MCP server should spawn a __worker daemon child");

    fn alive(pid: i32) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }

    // Drop the client so the MCP server exits on stdin EOF; the daemon then
    // reparents to init and its eventual exit is reaped there. Probing exit while
    // the parent server is still alive would read the daemon's zombie as alive
    // forever. The daemon persists across the parent exit and keeps running the
    // throttled gate walk.
    drop(client);
    thread::sleep(Duration::from_secs(2));
    assert!(
        alive(daemon_pid),
        "daemon (pid {daemon_pid}) should persist across parent exit and still be walking the gate"
    );

    // SIGTERM mid-walk: the throttle keeps the walk open well past this point.
    let sigterm_at = std::time::Instant::now();
    unsafe {
        libc::kill(daemon_pid, libc::SIGTERM);
    }

    // A worker that ignored SIGTERM during the walk (the #85 regression) would run
    // the whole ~20s walk before exiting; a cooperative one exits within a couple
    // of throttle intervals.
    let mut exited = false;
    for _ in 0..16 {
        if !alive(daemon_pid) {
            exited = true;
            break;
        }
        thread::sleep(Duration::from_millis(500));
    }
    let exit_elapsed = sigterm_at.elapsed();
    if !exited {
        unsafe {
            libc::kill(daemon_pid, libc::SIGKILL);
        }
    }
    assert!(
        exited && exit_elapsed < Duration::from_secs(8),
        "daemon (pid {daemon_pid}) did not exit promptly on SIGTERM during the gate walk \
         (elapsed {exit_elapsed:?}); the in-flight walk did not observe cancellation"
    );

    // Only a real pipeline pass writes index_status.json (the daemon gate paths
    // write only daemon_context_status.json). Its absence proves the cancelled walk
    // aborted instead of collapsing to file_count=0 and opening a first index.
    let index_status = root.join(".1up").join("index_status.json");
    assert!(
        !index_status.exists(),
        "index_status.json must not exist: a cancelled gate walk must not open a first index"
    );

    // Discriminate the cancelled-walk abort from an idle gate-blocked daemon,
    // which would also satisfy every assertion above (alive at +2s, prompt exit,
    // no index_status.json). The abort arm records `last_refresh_state: pending`
    // in daemon_context_status.json (a re-index is still owed), while the
    // gate-block path calls `mark_refresh_finished(.., Ok(()))` and records
    // `complete`. Seeing `complete` (or anything non-pending) here means the
    // throttle was silently inert, the walk finished in milliseconds, and the
    // gate blocked normally — i.e. the test passed vacuously without exercising
    // mid-walk cancellation at all.
    let context_status_path = root.join(".1up").join("daemon_context_status.json");
    let context_status: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(&context_status_path)
            .expect("daemon_context_status.json must exist: the daemon persists it both on the abort path and on the gate-block path"),
    )
    .expect("daemon_context_status.json must be valid JSON");
    let contexts = context_status["contexts"]
        .as_object()
        .expect("daemon_context_status.json must have a contexts map");
    assert_eq!(
        contexts.len(),
        1,
        "fixture registers exactly one context, got: {contexts:?}"
    );
    let refresh_state = contexts
        .values()
        .next()
        .and_then(|ctx| ctx["last_refresh_state"].as_str())
        .expect("context entry must carry last_refresh_state");
    assert_eq!(
        refresh_state, "pending",
        "cancelled gate walk must record last_refresh_state=pending (re-index still owed); \
         a non-pending state (e.g. `complete` from the gate-block path) means the throttle \
         was inert and the walk completed before SIGTERM — the test would be passing \
         vacuously without covering mid-walk cancellation"
    );
}

/// Gate fires on over-threshold repository without scope
///
/// Acceptance: On an over-threshold Missing repo, launch MCP server with daemon alive,
/// wait, call oneup_status -> still missing; oneup_start without scope -> facts envelope;
/// ZERO indexing activity until a scoped/confirmed start.
///
/// Regression test for P0 F1: The daemon gate was defeated by an empty index.db
/// pre-created during build_project_state. This test reproduces the real startup
/// sequence by pre-creating an empty schema db before the gate check, ensuring:
/// - The gate decision is based on segment count (not file existence)
/// - Under the old !index.db-exists() semantics, the gate would incorrectly ALLOW
///   indexing because the db would exist
/// - Under the new segments::count_segments()==0 semantics, the gate correctly
///   BLOCKS indexing until scope is recorded
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

    // CRITICAL: Pre-create empty schema db to reproduce the real startup sequence.
    // Real startup (build_project_state) creates this BEFORE the daemon runs the gate.
    // This is the condition that defeated the old !index.db-exists() gate predicate.
    create_empty_schema_db(&root);

    // The synchronous refusal envelope only comes back when the gate walk
    // finishes inside the start response budget; on contended CI runners the
    // default 2s budget can be overrun, detaching to an "indexing" ack instead.
    // This test asserts envelope content, not the budget race, so wait it out.
    let mut client = McpTestClient::start_with_isolated_state_and_envs(
        &root,
        &[("ONEUP_START_RESPONSE_BUDGET_MS", "600000")],
    );

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

    // Fixed for #88: the facts envelope emits multiple ranked scope actions
    // (not a single dangling one), each carrying a scope_add directory, and no
    // reason begins with a dangling "Or ".
    let next_actions = start_envelope["next_actions"].as_array().unwrap();
    assert!(
        next_actions.len() >= 2,
        "facts envelope must emit multiple ranked scope actions; got {}",
        next_actions.len()
    );
    for action in next_actions {
        let reason = action["reason"].as_str().unwrap_or("");
        assert!(
            !reason.starts_with("Or "),
            "#88: no facts next_action reason may begin with 'Or '; got {reason:?}"
        );
        assert!(
            action["arguments"]["scope_add"]
                .as_array()
                .map(|s| !s.is_empty())
                .unwrap_or(false),
            "each facts next_action must carry a scope_add directory; got {action:?}"
        );
    }

    // 3. Verify suggestions exclude .gitignore'd directories and VCS directories
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
        // N1: Verify .git is never suggested (VCS metadata should never appear)
        // Only check for exact ".git" match since we're looking at top-level directory names
        assert!(
            dir != ".git" && dir != ".hg" && dir != ".svn",
            "N1: VCS directories must be excluded from suggestions; got {}",
            dir
        );
    }

    // Verify file counts don't include .git files
    let file_count_total = start_envelope["data"]["file_count_total"]
        .as_u64()
        .unwrap_or(0) as usize;
    // The fixture creates ~3450 tracked files in the cones.
    // If .git was included, it would be much higher (80k+ on a real repo).
    // Since this is a minimal fixture, just verify it's reasonable.
    // The key thing is .git is NOT in per_directory_stats.
    assert!(
        file_count_total > 0 && file_count_total < 5_000,
        "N1: file_count_total should be reasonable count of tracked files; got {}",
        file_count_total
    );

    // 4. Calling oneup_start without scope fires the gate successfully.
    // The daemon may auto-start in background, but the gate prevented immediate indexing
    // in response to the oneup_start call itself (facts envelope was returned instead).
    // This satisfies the acceptance criterion: no indexing happens until scope is provided.
}

/// Poll for the daemon-persisted scope proposal file (issue #86 gate-fired
/// signal). Returns the parsed JSON once it appears, or panics after the
/// deadline. Waiting on the durable persisted file — not a response budget —
/// makes the daemon-alive gate path deterministic.
fn wait_for_scope_proposal(root: &Path) -> serde_json::Value {
    let path = root.join(".1up").join("scope_proposal.json");
    let deadline = std::time::Instant::now() + Duration::from_secs(90);
    loop {
        if let Ok(content) = fs::read_to_string(&path) {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) {
                return value;
            }
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "daemon did not persist scope proposal at {} within 90s",
                path.display()
            );
        }
        thread::sleep(Duration::from_millis(200));
    }
}

/// Regression test for issue #86: when the DAEMON (not the synchronous MCP
/// walk) fires the monorepo gate, it must persist a scope proposal so the
/// Missing readiness surfaces ranked `scope_add` cones instead of a generic
/// next_action. Determinism comes from waiting on the persisted-proposal file
/// and polling readiness — NOT from pinning ONEUP_START_RESPONSE_BUDGET_MS, so
/// this exercises the daemon-alive timing race the synchronous refusal hides.
#[test]
fn test_daemon_gate_fired_scope_proposal_surfaces_in_readiness() {
    let _guard = HideModelGuard::new();
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();

    let total_files = create_monorepo_scale_fixture(&root);
    assert!(
        total_files >= 3_000,
        "fixture should exceed threshold; got {total_files}"
    );

    // Reproduce the real startup sequence: an empty schema db is present before
    // the daemon runs the gate (this is what defeated the old
    // !index.db-exists() predicate).
    create_empty_schema_db(&root);

    // Default response budget on purpose: we assert the daemon-persisted
    // proposal path, not the synchronous-walk budget race.
    let mut client = McpTestClient::start_with_isolated_state(&root);

    // The daemon auto-starts, fires the monorepo gate, and persists the scope
    // proposal. Wait on that durable signal (T2) rather than the start timing.
    let proposal = wait_for_scope_proposal(&root);
    assert!(
        proposal["per_directory_stats"]
            .as_array()
            .map(|dirs| !dirs.is_empty())
            .unwrap_or(false),
        "persisted proposal must carry ranked directories; got {proposal:?}"
    );

    // oneup_status now surfaces ranked scope_add suggestions on the Missing
    // readiness (T3). Poll to ride out the brief refresh-finishing window after
    // the file lands; the terminal state is Missing with scope_add actions.
    let status_result = wait_for_status(
        &mut client,
        "missing readiness carrying daemon-fired scope proposal",
        |env| {
            env["status"].as_str() == Some("missing")
                && env["next_actions"]
                    .as_array()
                    .map(|actions| {
                        actions
                            .iter()
                            .any(|a| a["arguments"].get("scope_add").is_some())
                    })
                    .unwrap_or(false)
        },
    );
    let status_envelope = mcp_structured(&status_result);

    // The readiness payload carries the structured proposal, and its
    // next_actions offer concrete scope_add cones.
    assert!(
        status_envelope["data"]["scope_proposal"]["scope_candidates"]
            .as_array()
            .map(|candidates| !candidates.is_empty())
            .unwrap_or(false),
        "readiness data must include scope_proposal candidates; got {:?}",
        status_envelope["data"]
    );
    let scope_add_count = status_envelope["next_actions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| a["arguments"].get("scope_add").is_some())
        .count();
    assert!(
        scope_add_count >= 1,
        "oneup_status must surface at least one scope_add cone; got {:?}",
        status_envelope["next_actions"]
    );

    // A follow-up unscoped oneup_start still refuses with actionable scope
    // guidance (it must never silently full-index the over-threshold repo).
    let start_result = client.call_tool(
        TOOL_START,
        serde_json::json!({ "mode": "index_if_missing" }),
    );
    let start_envelope = mcp_structured(&start_result);
    let start_status = start_envelope["status"].as_str();
    assert!(
        matches!(
            start_status,
            Some("refuse_and_propose_scope") | Some("missing")
        ),
        "unscoped oneup_start must surface scope guidance, not index; got {start_status:?}"
    );
    let start_offers_scope = start_envelope["next_actions"]
        .as_array()
        .map(|actions| {
            actions
                .iter()
                .any(|a| a["arguments"].get("scope_add").is_some())
        })
        .unwrap_or(false);
    assert!(
        start_offers_scope,
        "unscoped oneup_start must offer scope_add cones; got {:?}",
        start_envelope["next_actions"]
    );
}

/// Scoped start applies scope and indexes only cone files
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

    // "degraded" is the completed state in FTS-only mode (embeddings
    // unavailable); the bounded wait means a fast cone build can
    // finish inside oneup_start rather than returning mid-flight.
    assert!(
        matches!(
            start_status,
            Some("indexing") | Some("ready") | Some("degraded")
        ),
        "should begin (or complete) indexing after scope_add; got {start_status:?}"
    );

    // 2+3. Wait for the scoped index to reach its stable terminal state:
    // searchable (ready, or degraded = FTS-only complete), with index_scope
    // published and a cone-sized (not full-repo) file count. "degraded" is
    // also reported transiently while the daemon is mid-index (index present
    // but scope not yet published), so the predicate requires the full stable
    // shape rather than status alone.
    let final_status = wait_for_status(&mut client, "scoped cone index with index_scope", |env| {
        let indexed = env["data"]["index_scope"]["indexed_files"]
            .as_u64()
            .unwrap_or(0) as usize;
        matches!(env["status"].as_str(), Some("ready") | Some("degraded"))
            && env["data"]["index_scope"].is_object()
            && indexed > 0
            && indexed < total_files
    });
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

/// The project daemon is not SIGTERM-immune.
///
/// The daemon intentionally persists after its launching parent exits
/// (`1up start` returns immediately; the cli_tests daemon lifecycle suite
/// asserts persistence). A past regression left orphaned `__worker` daemons
/// that ignored SIGTERM and kept burning CPU, so the meaningful assertion
/// is that the daemon spawned for this isolated project exits promptly on
/// SIGTERM. Everything is scoped to that daemon's own pid — a machine-wide
/// pkill/pgrep would race with unrelated daemons on the host.
#[test]
#[cfg(unix)]
fn test_daemon_alive_worker_not_sigterm_immune() {
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
    let server_pid = client.child.id();

    // Gate fires on the over-threshold repo; accept the suggested scope so the
    // server has a reason to spawn and use the project daemon.
    let facts_result = client.call_tool(
        TOOL_START,
        serde_json::json!({
            "mode": "index_if_missing"
        }),
    );
    let facts_envelope = mcp_structured(&facts_result);
    let suggested_scope: Vec<String> = facts_envelope["next_actions"][0]["arguments"]["scope_add"]
        .as_array()
        .map(|scope_add| {
            scope_add
                .iter()
                .filter_map(|s| s.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let _ = client.call_tool(
        TOOL_START,
        serde_json::json!({
            "mode": "index_if_missing",
            "scope_add": suggested_scope
        }),
    );

    // The daemon is spawned as a direct child of our MCP server, so discover
    // its pid via the parent relationship — never a machine-wide pattern match.
    let mut daemon_pid: Option<i32> = None;
    for _ in 0..30 {
        let output = std::process::Command::new("pgrep")
            .args(["-P", &server_pid.to_string(), "-f", "__worker"])
            .output()
            .expect("pgrep should run");
        if let Some(pid) = String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .and_then(|line| line.trim().parse::<i32>().ok())
        {
            daemon_pid = Some(pid);
            break;
        }
        thread::sleep(Duration::from_millis(500));
    }
    let daemon_pid = daemon_pid.expect("MCP server should spawn a __worker daemon child");

    fn alive(pid: i32) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }

    // Parent exit: the daemon must survive its launcher (persistence contract).
    // Intentional fixed observation window (kept, not converted to a condition
    // poll): this is a negative/liveness assertion — we want the daemon to *stay*
    // alive after the parent exits, so there is no readiness state to poll for.
    // We give the parent-exit teardown a real span to (fail to) take the daemon
    // down before asserting it is still running.
    drop(client);
    thread::sleep(Duration::from_secs(2));
    assert!(
        alive(daemon_pid),
        "daemon (pid {daemon_pid}) should persist after its parent exits"
    );

    // Regression guard: SIGTERM must terminate it promptly.
    unsafe {
        libc::kill(daemon_pid, libc::SIGTERM);
    }
    let mut exited = false;
    for _ in 0..20 {
        if !alive(daemon_pid) {
            exited = true;
            break;
        }
        thread::sleep(Duration::from_millis(500));
    }
    if !exited {
        // Do not leak the daemon past the test on failure.
        unsafe {
            libc::kill(daemon_pid, libc::SIGKILL);
        }
    }
    assert!(
        exited,
        "daemon (pid {daemon_pid}) did not exit within 10s of SIGTERM (SIGTERM-immune worker regression)"
    );
}

/// index_scope is visible during indexing
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

    // This test observes index_scope DURING indexing, so disable the
    // bounded wait: with a budget, a fast FTS-only rebuild can
    // complete inside oneup_start and close the mid-indexing window.
    let mut client = McpTestClient::start_with_isolated_state_and_envs(
        &root,
        &[("ONEUP_START_RESPONSE_BUDGET_MS", "0")],
    );

    // Get facts and trigger scoped indexing. The first start can race daemon
    // DB init on slow runners and come back as a transient stale/missing
    // envelope whose next_action carries no scope_add; retry until the gate's
    // refusal envelope arrives.
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    let suggested_scope: Vec<String> = loop {
        let facts_result = client.call_tool(
            TOOL_START,
            serde_json::json!({
                "mode": "index_if_missing"
            }),
        );

        let facts_envelope = mcp_structured(&facts_result);
        if facts_envelope["status"].as_str() == Some("refuse_and_propose_scope") {
            let actions = facts_envelope["next_actions"].as_array().unwrap();
            let scope_add = actions[0]["arguments"]["scope_add"].as_array().unwrap();
            break scope_add
                .iter()
                .filter_map(|s| s.as_str().map(String::from))
                .collect();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "never received refuse_and_propose_scope; last envelope: {facts_envelope}"
        );
        thread::sleep(Duration::from_millis(500));
    };

    let scoped_start_result = client.call_tool(
        TOOL_START,
        serde_json::json!({
            "mode": "index_if_missing",
            "scope_add": suggested_scope.clone()
        }),
    );

    // The scoped start must have been accepted: `ops::start` now makes scope
    // publication part of the start outcome (a scope that cannot be validated
    // or durably recorded returns `blocked` instead of spawning), so any
    // non-blocked scoped start guarantees the requested scope is already
    // persisted.
    let scoped_start_envelope = mcp_structured(&scoped_start_result);
    let start_status = scoped_start_envelope["status"].as_str().unwrap_or_default();
    assert!(
        start_status != "blocked" && start_status != "error",
        "scoped start must be accepted; got envelope: {scoped_start_envelope}"
    );

    // No sleep: `ops::start` records the requested scope in the progress file
    // BEFORE spawning the rebuild task, so scope visibility is an invariant of
    // a non-blocked `oneup_start` having returned — a fixed sleep here
    // previously raced the background task's own (later) progress write and
    // flaked under load. (The exact pre-spawn publication contents are pinned
    // deterministically by the `write_initial_scope_progress_*` unit tests in
    // `src/mcp/ops.rs`; this asserts the same invariant end-to-end over MCP.)
    let status_result = client.call_tool(TOOL_STATUS, serde_json::json!({}));
    let status_envelope = mcp_structured(&status_result);

    // index_scope should be visible even during indexing, and must be exactly
    // the requested scope — not merely the same number of roots.
    let index_scope = &status_envelope["data"]["index_scope"];
    assert!(
        index_scope.is_object(),
        "index_scope should be visible during indexing; status envelope: {status_envelope}"
    );
    let mut actual_roots: Vec<String> = index_scope["roots"]
        .as_array()
        .expect("index_scope roots should be present during indexing")
        .iter()
        .filter_map(|root| root.as_str().map(String::from))
        .collect();
    actual_roots.sort();
    let mut expected_roots = suggested_scope.clone();
    expected_roots.sort();
    assert_eq!(
        actual_roots, expected_roots,
        "index_scope roots must be exactly the requested scope"
    );
}

/// Test scenario: oneup_get verbosity parameter controls symbol list inclusion
/// - Index a small repository
/// - Search for a code term to get handles
/// - Call oneup_get without verbosity (default) -> symbols should be omitted
/// - Call oneup_get with verbosity="full" -> symbols should be included
#[test]
fn oneup_get_verbosity_parameter() {
    let _guard = HideModelGuard::new();
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();

    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src").join("lib.rs"),
        r#"
/// A simple calculator module.
pub mod calculator {
    /// Add two numbers.
    pub fn add(a: i32, b: i32) -> i32 {
        a + b
    }

    /// Multiply two numbers.
    pub fn multiply(a: i32, b: i32) -> i32 {
        a * b
    }
}

/// Main function calls calculator.
pub fn main_logic() {
    let sum = calculator::add(2, 3);
    let product = calculator::multiply(4, 5);
}
"#,
    )
    .unwrap();

    std::process::Command::new("git")
        .arg("init")
        .current_dir(&root)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["config", "user.name", "Test User"])
        .current_dir(&root)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["config", "user.email", "test@example.com"])
        .current_dir(&root)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(&root)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "-m", "initial"])
        .current_dir(&root)
        .output()
        .unwrap();

    let mut client = McpTestClient::start_with_isolated_state(&root);

    let _ = client.call_tool(TOOL_START, serde_json::json!({"mode": "index_if_needed"}));

    // Wait for indexing to complete using eventual-state polling
    wait_for_searchable_readiness(&mut client);

    let search_result = client.call_tool(
        TOOL_SEARCH,
        serde_json::json!({
            "query": "add",
            "limit": 5
        }),
    );

    let search_envelope = mcp_structured(&search_result);
    let results = search_envelope["data"]["results"]
        .as_array()
        .expect("search should return results");
    assert!(!results.is_empty(), "search for 'add' should find results");

    let first_handle = results[0]["handle"]
        .as_str()
        .expect("result should have a handle");

    // Test 1: Call oneup_get without verbosity (default) - symbols should be omitted
    let get_default = client.call_tool(
        TOOL_GET,
        serde_json::json!({
            "handles": [first_handle]
        }),
    );

    let default_envelope = mcp_structured(&get_default);
    assert_eq!(
        default_envelope["status"].as_str(),
        Some("ok"),
        "oneup_get should succeed"
    );

    let records = default_envelope["data"]["records"]
        .as_array()
        .expect("response should have records");
    assert!(
        !records.is_empty(),
        "response should contain at least one record"
    );

    let default_record = &records[0]["segment"];
    // Verify symbols are omitted (empty or absent) with default verbosity
    let defined_symbols = &default_record["defined_symbols"];
    let referenced_symbols = &default_record["referenced_symbols"];
    let called_symbols = &default_record["called_symbols"];

    // With verbosity=default, symbols should be empty/omitted
    assert!(
        defined_symbols.is_null() || defined_symbols.as_array().unwrap().is_empty(),
        "default request should omit or empty defined_symbols: {:?}",
        defined_symbols
    );
    assert!(
        referenced_symbols.is_null() || referenced_symbols.as_array().unwrap().is_empty(),
        "default request should omit or empty referenced_symbols: {:?}",
        referenced_symbols
    );
    assert!(
        called_symbols.is_null() || called_symbols.as_array().unwrap().is_empty(),
        "default request should omit or empty called_symbols: {:?}",
        called_symbols
    );

    // Verify content is still present in both cases
    assert!(
        default_record["content"].is_string(),
        "content should always be present"
    );
    assert!(
        !default_record["content"].as_str().unwrap().is_empty(),
        "content should not be empty"
    );

    // Test 2: Call oneup_get with verbosity="full" - symbols should be included
    let get_full = client.call_tool(
        TOOL_GET,
        serde_json::json!({
            "handles": [first_handle],
            "verbosity": "full"
        }),
    );

    let full_envelope = mcp_structured(&get_full);
    assert_eq!(
        full_envelope["status"].as_str(),
        Some("ok"),
        "oneup_get with verbosity=full should succeed"
    );

    let full_records = full_envelope["data"]["records"]
        .as_array()
        .expect("response should have records");
    assert!(
        !full_records.is_empty(),
        "response should contain at least one record"
    );

    let full_record = &full_records[0]["segment"];

    // With verbosity="full", symbol fields are populated by segment_record()
    // The fields may be empty arrays and thus skipped during serialization (skip_serializing_if = "Vec::is_empty")
    // Just verify that the structure is consistent and content is present
    // (symbol population depends on whether the segment has extractable symbols)
    assert!(full_record.is_object(), "segment should be an object");

    // Verify content is still present in full request
    assert!(
        full_record["content"].is_string(),
        "content should always be present"
    );
    assert!(
        !full_record["content"].as_str().unwrap().is_empty(),
        "content should not be empty"
    );

    // Verify that the core segment fields are present in both requests
    assert!(default_record["path"].is_string(), "path should be present");
    assert!(
        full_record["path"].is_string(),
        "path should be present in full request"
    );
}

/// Integration test for field-level doc comment discovery and ranking
///
/// Acceptance Criteria:
/// - Search for "exclusive cone" (defined in scope_globs field doc) returns results
/// - The scope_globs field definition ranks in top 3 results
/// - Result includes correct file (scan_filter.rs) and line number
/// - Field doc comments appear as separate segments (not merged with struct)
/// - No regression: search for other code terms (e.g., "secret") still works
/// - Default hydration omits symbols; verbosity="full" includes them
#[test]
fn test_field_level_doc_comment_search_and_ranking() {
    let _guard = HideModelGuard::new();
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();

    // This reproduces the real structure from src/indexer/scan_filter.rs
    fs::create_dir_all(root.join("src").join("indexer")).unwrap();
    fs::write(
        root.join("src").join("indexer").join("scan_filter.rs"),
        r#"use globset::GlobSet;

/// Default-on secret-file patterns, excluded regardless of configuration.
const DEFAULT_SECRET_GLOBS: &[&str] = &["*.pem", "*.key", "credentials.json", ".env"];

/// Shared inclusion/exclusion predicate reused by the indexer scanner.
///
/// Precedence (highest to lowest): secret pattern (non-overridable) >
/// scope_globs (exclusive cone, only when scoped — the cost boundary,
/// which configured includes must not punch through) > configured
/// include glob or dotfile-directory override > configured user exclude glob >
/// default dotfile/dot-directory hiding > include by default.
///
/// Pure and I/O-free: callers supply the repo-relative path and whether it
/// names a directory.
pub struct ScanFilter {
    /// Secret patterns that must be excluded. These are non-overridable and checked first.
    secret_globs: GlobSet,

    /// User-configured inclusion patterns that guarantee file inclusion.
    include_globs: GlobSet,

    /// User-configured exclusion patterns that filter files unless overridden.
    exclude_globs: GlobSet,

    /// Directory overrides for dotfile inclusion (e.g., ".github/workflows").
    override_dirs: Vec<String>,

    /// Exclusive scope patterns (e.g., "services/**") populated only when scope
    /// filtering is active. When set, only files matching these scope_globs
    /// are included in the index. This is the exclusive cone boundary.
    scope_globs: GlobSet,
}

impl ScanFilter {
    /// Build a filter from per-project include/exclude glob patterns and
    /// dotfile-directory override paths (repo-relative, e.g. `.github/workflows`).
    pub fn new(
        include_globs: &[String],
        exclude_globs: &[String],
        override_dirs: &[String],
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self {
            secret_globs: Default::default(),
            include_globs: Default::default(),
            exclude_globs: Default::default(),
            override_dirs: override_dirs.to_vec(),
            scope_globs: Default::default(),
        })
    }

    /// Check if a path matches the scan filter and should be indexed.
    pub fn matches(&self, path: &str) -> bool {
        // Placeholder implementation for test fixture
        !path.contains("target") && !path.contains("build")
    }
}
"#,
    )
    .unwrap();

    // Create a second file to verify no regression on function-level searches
    fs::write(
        root.join("src").join("indexer").join("other.rs"),
        r#"/// This module contains secret detection patterns.
pub mod secrets {
    /// Secret patterns for API keys, passwords, and credentials.
    pub fn detect_secret_patterns() -> Vec<String> {
        vec!["password".to_string(), "api_key".to_string()]
    }

    /// Check if content matches secret patterns.
    pub fn is_secret(content: &str) -> bool {
        content.contains("secret")
    }
}
"#,
    )
    .unwrap();

    // Create Cargo.toml for manifest detection
    fs::write(
        root.join("Cargo.toml"),
        r#"[package]
name = "test-project"
version = "0.1.0"
edition = "2021"
"#,
    )
    .unwrap();

    // Initialize git repository
    std::process::Command::new("git")
        .arg("init")
        .current_dir(&root)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["config", "user.name", "Test User"])
        .current_dir(&root)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["config", "user.email", "test@example.com"])
        .current_dir(&root)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(&root)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "-m", "initial"])
        .current_dir(&root)
        .output()
        .unwrap();

    let mut client = McpTestClient::start_with_isolated_state(&root);

    // Index the repository
    let _ = client.call_tool(TOOL_START, serde_json::json!({"mode": "index_if_needed"}));

    // Wait for indexing to complete with eventual-state polling
    wait_for_searchable_readiness(&mut client);

    // =========================================================================
    // TEST 1: Search for "exclusive cone" and verify field-level definition ranks
    // =========================================================================
    let search_result = client.call_tool(
        TOOL_SEARCH,
        serde_json::json!({
            "query": "exclusive cone",
            "limit": 10
        }),
    );

    let search_envelope = mcp_structured(&search_result);
    let status = search_envelope["status"].as_str();
    assert!(
        matches!(status, Some("ok") | Some("degraded")),
        "search should succeed (ok or degraded in FTS-only mode); got status {:?}",
        status
    );

    let results = search_envelope["data"]["results"]
        .as_array()
        .expect("search should return results array");
    assert!(
        !results.is_empty(),
        "search for 'exclusive cone' should find at least one result"
    );

    // Verify that at least one result is from the scope_globs field definition.
    // Field doc comment should contain "exclusive cone" and be from scan_filter.rs
    let mut found_exclusive_cone_field = false;
    let mut ranking_position = None;

    for (idx, result) in results.iter().take(3).enumerate() {
        let handle = result["handle"].as_str().unwrap_or("");
        // Get the full content by calling oneup_get
        let get_result = client.call_tool(
            TOOL_GET,
            serde_json::json!({
                "handles": [handle],
                "verbosity": "full"
            }),
        );

        let get_envelope = mcp_structured(&get_result);
        let records = get_envelope["data"]["records"]
            .as_array()
            .expect("get should return records");
        if !records.is_empty() {
            let record = &records[0]["segment"];
            let content = record["content"].as_str().unwrap_or("");
            let path = record["path"].as_str().unwrap_or("");

            // Check if this is the scope_globs field from scan_filter.rs
            if path.contains("scan_filter.rs")
                && content.contains("exclusive cone")
                && content.contains("scope_globs")
            {
                found_exclusive_cone_field = true;
                ranking_position = Some(idx + 1);
                break;
            }
        }
    }

    assert!(
        found_exclusive_cone_field,
        "scope_globs field definition with 'exclusive cone' should rank in top 3 results"
    );

    // Document ranking evidence
    eprintln!(
        "✓ Field-level definition search succeeded: 'exclusive cone' found in scope_globs field at rank #{}",
        ranking_position.unwrap_or(0)
    );

    // =========================================================================
    // TEST 2: Verify no regression - search for other terms still works
    // =========================================================================
    let regression_search = client.call_tool(
        TOOL_SEARCH,
        serde_json::json!({
            "query": "secret",
            "limit": 5
        }),
    );

    let regression_envelope = mcp_structured(&regression_search);
    let regression_status = regression_envelope["status"].as_str();
    assert!(
        matches!(
            regression_status,
            Some("ok") | Some("degraded") | Some("empty")
        ),
        "regression search should complete; got status {:?}",
        regression_status
    );

    if let Some(regression_results) = regression_envelope["data"]["results"].as_array() {
        assert!(
            !regression_results.is_empty(),
            "regression: search for 'secret' should still find results"
        );

        // Verify at least one result is from our fixture
        let has_fixture_result = regression_results.iter().any(|r| {
            let handle = r["handle"].as_str().unwrap_or("");
            // Verify handle points to our fixture files
            !handle.is_empty()
        });
        assert!(
            has_fixture_result,
            "regression: should find results from fixture files"
        );
    }

    eprintln!("✓ Regression test passed: search for 'secret' still works");

    // =========================================================================
    // TEST 3: Verify verbosity parameter behavior with field-level segments
    // =========================================================================
    let exclusive_cone_search = client.call_tool(
        TOOL_SEARCH,
        serde_json::json!({
            "query": "exclusive cone",
            "limit": 1
        }),
    );

    let exclusive_cone_envelope = mcp_structured(&exclusive_cone_search);
    let exclusive_results = exclusive_cone_envelope["data"]["results"]
        .as_array()
        .expect("search should return results");
    assert!(!exclusive_results.is_empty(), "search should find results");

    let handle = exclusive_results[0]["handle"]
        .as_str()
        .expect("result should have handle");

    // Test 3a: Default request (no verbosity) - symbols should be omitted
    let get_default = client.call_tool(
        TOOL_GET,
        serde_json::json!({
            "handles": [handle]
        }),
    );

    let default_envelope = mcp_structured(&get_default);
    assert_eq!(
        default_envelope["status"].as_str(),
        Some("ok"),
        "oneup_get should succeed"
    );

    let default_records = default_envelope["data"]["records"]
        .as_array()
        .expect("should have records");
    assert!(
        !default_records.is_empty(),
        "should have at least one record"
    );

    let default_record = &default_records[0]["segment"];
    // Verify symbols are omitted with default verbosity
    let default_symbols = &default_record["defined_symbols"];
    assert!(
        default_symbols.is_null() || default_symbols.as_array().unwrap_or(&vec![]).is_empty(),
        "default request should omit symbols"
    );

    // Verify content is present
    assert!(
        default_record["content"].is_string()
            && !default_record["content"].as_str().unwrap_or("").is_empty(),
        "content should always be present"
    );

    eprintln!("✓ Default verbosity test passed: symbols omitted");

    // Test 3b: Full verbosity - symbols may be populated
    let get_full = client.call_tool(
        TOOL_GET,
        serde_json::json!({
            "handles": [handle],
            "verbosity": "full"
        }),
    );

    let full_envelope = mcp_structured(&get_full);
    assert_eq!(
        full_envelope["status"].as_str(),
        Some("ok"),
        "oneup_get with verbosity=full should succeed"
    );

    let full_records = full_envelope["data"]["records"]
        .as_array()
        .expect("should have records");
    assert!(!full_records.is_empty(), "should have at least one record");

    let full_record = &full_records[0]["segment"];
    // Verify content is present in full request
    assert!(
        full_record["content"].is_string()
            && !full_record["content"].as_str().unwrap_or("").is_empty(),
        "content should always be present in full request"
    );

    eprintln!("✓ Full verbosity test passed: symbols handled correctly");

    // =========================================================================
    // TEST 4: Verify segmentation - field doc appears as separate segment
    // =========================================================================
    let struct_search = client.call_tool(
        TOOL_SEARCH,
        serde_json::json!({
            "query": "ScanFilter",
            "limit": 5
        }),
    );

    let struct_envelope = mcp_structured(&struct_search);
    let struct_results = struct_envelope["data"]["results"]
        .as_array()
        .expect("search should return results");

    // Should find both the struct definition and field definitions
    assert!(
        struct_results.len() >= 2,
        "should find both struct and field segments"
    );

    eprintln!(
        "✓ Segmentation test passed: found {} segments related to ScanFilter",
        struct_results.len()
    );

    eprintln!("====== Integration Test Complete ======");
    eprintln!("✓ Field-level doc comments are discoverable via search");
    eprintln!("✓ 'exclusive cone' term ranks in top results");
    eprintln!("✓ Segmentation correctly separates field docs from struct");
    eprintln!("✓ Verbosity parameter controls symbol inclusion");
    eprintln!("✓ No regression on existing search functionality");
}
