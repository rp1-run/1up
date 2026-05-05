use assert_cmd::Command;
use oneup::storage::{
    db::Db,
    schema,
    segments::{self, IndexedFileMeta, SegmentInsert},
};
use predicates::prelude::*;
use std::collections::BTreeSet;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Stdio};
use std::sync::Mutex;
use tempfile::TempDir;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::net::UnixStream;

static MODEL_MUTEX: Mutex<()> = Mutex::new(());

fn cmd() -> Command {
    Command::cargo_bin("1up").unwrap()
}

fn test_data_dir(home: &std::path::Path) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        home.join("Library").join("Application Support").join("1up")
    }

    #[cfg(not(target_os = "macos"))]
    {
        home.join(".local").join("share").join("1up")
    }
}

fn cmd_with_home(home: &Path) -> Command {
    let mut command = cmd();
    command
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join(".local").join("share"))
        .env("XDG_CONFIG_HOME", home.join(".config"));
    command
}

fn seed_model_download_failure(home: &Path) {
    let model_dir = test_data_dir(home).join("models").join("all-MiniLM-L6-v2");
    fs::create_dir_all(&model_dir).unwrap();
    fs::write(model_dir.join(".download_failed"), "skip download in test").unwrap();
}

#[cfg(unix)]
fn write_framed_json(stream: &mut UnixStream, value: &serde_json::Value) {
    let payload = serde_json::to_vec(value).unwrap();
    let length = u32::try_from(payload.len()).unwrap().to_be_bytes();
    stream.write_all(&length).unwrap();
    stream.write_all(&payload).unwrap();
    stream.shutdown(std::net::Shutdown::Write).unwrap();
}

#[cfg(unix)]
fn read_framed_json(stream: &mut UnixStream) -> serde_json::Value {
    let mut length = [0u8; 4];
    stream.read_exact(&mut length).unwrap();
    let mut payload = vec![0u8; u32::from_be_bytes(length) as usize];
    stream.read_exact(&mut payload).unwrap();
    serde_json::from_slice(&payload).unwrap()
}

struct McpTestClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    _state_home: Option<TempDir>,
}

impl McpTestClient {
    fn start(path: &Path) -> Self {
        Self::start_with_initialize_response(path).0
    }

    fn start_with_isolated_state(path: &Path) -> Self {
        Self::start_with_initialize_response_and_state(path, true).0
    }

    fn start_with_initialize_response(path: &Path) -> (Self, serde_json::Value) {
        Self::start_with_initialize_response_and_state(path, false)
    }

    fn start_with_initialize_response_and_state(
        path: &Path,
        isolate_state: bool,
    ) -> (Self, serde_json::Value) {
        let state_home = isolate_state.then(|| TempDir::new().unwrap());
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_1up"));
        command
            .args(["mcp", "--path", path.to_str().unwrap()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(home) = &state_home {
            let home_path = home.path().canonicalize().unwrap();
            command
                .env("HOME", &home_path)
                .env("XDG_DATA_HOME", home_path.join("data"))
                .env("XDG_CONFIG_HOME", home_path.join("config"));
        }

        let mut child = command.spawn().unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut client = Self {
            child,
            stdin,
            stdout,
            next_id: 1,
            _state_home: state_home,
        };

        let initialize_response = client.request(
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
        (client, initialize_response)
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

    fn list_tools(&mut self) -> serde_json::Value {
        self.request("tools/list", serde_json::json!({}))["result"].clone()
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

fn assert_mcp_text_matches_summary(result: &serde_json::Value) {
    assert_eq!(
        result["content"][0]["text"],
        result["structuredContent"]["summary"]
    );
}

fn assert_mcp_next_actions_are_canonical(envelope: &serde_json::Value) {
    let actions = envelope["next_actions"]
        .as_array()
        .expect("next_actions must be an array");
    assert!(
        !actions.is_empty(),
        "MCP tool envelopes should include a next action"
    );
    for action in actions {
        let tool = action["tool"]
            .as_str()
            .expect("next action tool must be a string");
        assert!(
            tool.starts_with("oneup_") && !tool.starts_with("1up_"),
            "next action should name canonical oneup_* tools only: {action:?}"
        );
    }
}

fn assert_mcp_response_is_presentation_free(result: &serde_json::Value) {
    assert_mcp_text_matches_summary(result);
    assert!(
        result["structuredContent"]["data"].is_object(),
        "MCP structured content should carry object-shaped data: {result:?}"
    );
    assert_mcp_next_actions_are_canonical(mcp_structured(result));
    assert_value_strings_are_presentation_free("MCP response", result);
}

fn assert_value_strings_are_presentation_free(label: &str, value: &serde_json::Value) {
    match value {
        serde_json::Value::String(text) => assert_text_is_presentation_free(label, text),
        serde_json::Value::Array(items) => {
            for item in items {
                assert_value_strings_are_presentation_free(label, item);
            }
        }
        serde_json::Value::Object(entries) => {
            for (key, value) in entries {
                if key == "content" {
                    continue;
                }
                assert_value_strings_are_presentation_free(label, value);
            }
        }
        _ => {}
    }
}

fn assert_text_is_presentation_free(label: &str, text: &str) {
    assert!(
        !text
            .as_bytes()
            .windows(2)
            .any(|window| window == [0x1b, b'[']),
        "{label} should not include ANSI color/control sequences: {text:?}"
    );
    for ch in text.chars() {
        let codepoint = ch as u32;
        assert!(
            !(0x2500..=0x257f).contains(&codepoint),
            "{label} should not include box/table drawing characters: {text:?}"
        );
        assert!(
            !(0x2800..=0x28ff).contains(&codepoint),
            "{label} should not include spinner glyphs: {text:?}"
        );
    }
    assert!(
        !text.lines().any(|line| {
            let trimmed = line.trim();
            trimmed.starts_with('|') && trimmed.ends_with('|') && trimmed.matches('|').count() >= 2
        }),
        "{label} should not include terminal-oriented table rows: {text:?}"
    );
}

fn write_running_progress(project: &Path) {
    fs::create_dir_all(project.join(".1up")).unwrap();
    fs::write(
        project.join(".1up").join("index_status.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "state": "running",
            "phase": "scanning",
            "files_total": 1,
            "files_scanned": 0,
            "files_processed": 0,
            "files_indexed": 0,
            "files_skipped": 0,
            "files_deleted": 0,
            "segments_stored": 0,
            "embeddings_enabled": true,
            "message": "test indexing",
            "updated_at": "2026-04-26T00:00:00Z"
        }))
        .unwrap(),
    )
    .unwrap();
}

fn write_running_progress_for_context(project: &Path, context_id: &str) {
    fs::create_dir_all(project.join(".1up")).unwrap();
    fs::write(
        project.join(".1up").join("index_status.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "state": "running",
            "phase": "scanning",
            "context_id": context_id,
            "source_root": project,
            "branch_name": "other",
            "branch_status": "named",
            "files_total": 1,
            "files_scanned": 0,
            "files_processed": 0,
            "files_indexed": 0,
            "files_skipped": 0,
            "files_deleted": 0,
            "segments_stored": 0,
            "embeddings_enabled": true,
            "message": "other context indexing",
            "updated_at": "2026-04-26T00:00:00Z"
        }))
        .unwrap(),
    )
    .unwrap();
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Runtime::new().unwrap().block_on(future)
}

fn seed_current_index_for_context(project: &Path, context_id: &str) {
    fs::create_dir_all(project.join(".1up")).unwrap();
    fs::write(
        project.join(".1up").join("project_id"),
        "context-count-project",
    )
    .unwrap();

    block_on(async {
        let db = Db::open_rw(&project.join(".1up").join("index.db"))
            .await
            .unwrap();
        let conn = db.connect().unwrap();
        schema::initialize(&conn).await.unwrap();
        let segment = SegmentInsert {
            id: format!("{context_id}-segment"),
            file_path: "src/other.rs".to_string(),
            language: "rust".to_string(),
            block_type: "function".to_string(),
            content: "pub fn other_context_only() {}\n".to_string(),
            line_start: 1,
            line_end: 1,
            embedding_vec: None,
            breadcrumb: None,
            complexity: 1,
            role: "source".to_string(),
            defined_symbols: "[]".to_string(),
            referenced_symbols: "[]".to_string(),
            referenced_relations: "[]".to_string(),
            called_symbols: "[]".to_string(),
            called_relations: "[]".to_string(),
            file_hash: "other-context-hash".to_string(),
        };
        let meta = IndexedFileMeta {
            extension: "rs".to_string(),
            file_hash: segment.file_hash.clone(),
            file_size: segment.content.len() as i64,
            modified_ns: 1,
        };
        segments::replace_file_segments_for_context_tx_with_meta(
            &conn,
            context_id,
            "src/other.rs",
            &[segment],
            Some(&meta),
        )
        .await
        .unwrap();
    });
}

#[cfg(unix)]
fn write_fake_runner(path: &Path) {
    fs::write(
        path,
        r#"#!/bin/sh
set -eu
{
  printf 'runner=%s\n' "${0##*/}"
  i=0
  for arg in "$@"; do
    printf 'arg[%s]=%s\n' "$i" "$arg"
    i=$((i + 1))
  done
} > "${ONEUP_FAKE_RUNNER_LOG:?}"
exit "${ONEUP_FAKE_RUNNER_STATUS:-0}"
"#,
    )
    .unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

impl Drop for McpTestClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// RAII guard that temporarily hides the embedding model to force FTS-only mode.
/// On drop, the model is restored. This works around a pre-existing vector
/// dimension mismatch bug in the int8 search path. Holds a mutex to prevent
/// concurrent test interference.
struct HideModelGuard {
    model_path: PathBuf,
    hidden_path: PathBuf,
    current_path: PathBuf,
    hidden_current_path: PathBuf,
    marker_path: PathBuf,
    active: bool,
    current_active: bool,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl HideModelGuard {
    fn new() -> Self {
        let lock = MODEL_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

        let model_dir = dirs::data_dir()
            .unwrap()
            .join("1up")
            .join("models")
            .join("all-MiniLM-L6-v2");
        let _ = fs::create_dir_all(&model_dir);
        let model_path = model_dir.join("model.onnx");
        let hidden_path = model_dir.join("model.onnx.hidden_by_test");
        let current_path = model_dir.join("current.json");
        let hidden_current_path = model_dir.join("current.json.hidden_by_test");
        let marker_path = model_dir.join(".download_failed");

        let active = model_path.exists();
        if active {
            fs::rename(&model_path, &hidden_path).unwrap();
        }
        let current_active = current_path.exists();
        if current_active {
            fs::rename(&current_path, &hidden_current_path).unwrap();
        }
        // Create download failure marker to prevent auto-download during tests
        let _ = fs::write(&marker_path, "hidden_by_test");

        Self {
            model_path,
            hidden_path,
            current_path,
            hidden_current_path,
            marker_path,
            active,
            current_active,
            _lock: lock,
        }
    }
}

impl Drop for HideModelGuard {
    fn drop(&mut self) {
        if self.active && self.hidden_path.exists() {
            let _ = fs::rename(&self.hidden_path, &self.model_path);
        }
        if self.current_active && self.hidden_current_path.exists() {
            let _ = fs::rename(&self.hidden_current_path, &self.current_path);
        }
        let _ = fs::remove_file(&self.marker_path);
    }
}

struct RestoreHiddenModelGuard {
    model_path: PathBuf,
    hidden_path: PathBuf,
    restored: bool,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl RestoreHiddenModelGuard {
    fn new() -> Self {
        let lock = MODEL_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let model_dir = dirs::data_dir()
            .unwrap()
            .join("1up")
            .join("models")
            .join("all-MiniLM-L6-v2");
        let model_path = model_dir.join("model.onnx");
        let hidden_path = model_dir.join("model.onnx.hidden_by_test");
        let restored = !model_path.exists() && hidden_path.exists();

        if restored {
            fs::rename(&hidden_path, &model_path).unwrap();
        }

        Self {
            model_path,
            hidden_path,
            restored,
            _lock: lock,
        }
    }
}

impl Drop for RestoreHiddenModelGuard {
    fn drop(&mut self) {
        if self.restored && self.model_path.exists() {
            let _ = fs::rename(&self.model_path, &self.hidden_path);
        }
    }
}

fn create_multi_lang_fixture() -> TempDir {
    let tmp = TempDir::new().unwrap();

    fs::write(
        tmp.path().join("main.rs"),
        r#"use std::io;

fn greet(name: &str) -> String {
    format!("Hello, {}", name)
}

struct Config {
    pub host: String,
    pub port: u16,
}

impl Config {
    fn new(host: String, port: u16) -> Self {
        Config { host, port }
    }

    fn address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

fn main() {
    let cfg = Config::new("localhost".to_string(), 8080);
    println!("{}", greet(&cfg.host));
    println!("Listening on {}", cfg.address());
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("utils.py"),
        r#"import os
import json

def parse_config(path: str) -> dict:
    """Parse a JSON configuration file."""
    with open(path) as f:
        return json.load(f)

class Logger:
    def __init__(self, name: str):
        self.name = name
        self.entries = []

    def log(self, message: str):
        self.entries.append(message)
        print(f"[{self.name}] {message}")

    def flush(self):
        self.entries.clear()

def validate_input(data: str) -> bool:
    return len(data.strip()) > 0
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("handler.js"),
        r#"function handleRequest(req, res) {
    const method = req.method;
    if (method === "GET") {
        return handleGet(req, res);
    }
    return handlePost(req, res);
}

function handleGet(req, res) {
    res.status(200).json({ ok: true });
}

function handlePost(req, res) {
    const body = req.body;
    if (!body) {
        res.status(400).json({ error: "missing body" });
        return;
    }
    res.status(201).json({ created: true });
}

module.exports = { handleRequest, handleGet, handlePost };
"#,
    )
    .unwrap();

    tmp
}

fn init_and_index(dir: &TempDir) {
    cmd()
        .args(["init", dir.path().to_str().unwrap(), "--format", "json"])
        .assert()
        .success();

    cmd()
        .args(["index", dir.path().to_str().unwrap(), "--format", "json"])
        .assert()
        .success();
}

fn init_and_index_fts_only(dir: &TempDir) -> HideModelGuard {
    let guard = HideModelGuard::new();

    cmd()
        .args(["init", dir.path().to_str().unwrap(), "--format", "json"])
        .assert()
        .success();

    cmd()
        .args(["index", dir.path().to_str().unwrap(), "--format", "json"])
        .assert()
        .success();

    guard
}

fn init_project(dir: &std::path::Path) {
    cmd()
        .args(["init", dir.to_str().unwrap(), "--format", "json"])
        .assert()
        .success();
}

fn run_index_json(dir: &std::path::Path, extra_args: &[&str]) -> serde_json::Value {
    let mut command = cmd();
    command.arg("index");
    for arg in extra_args {
        command.arg(arg);
    }
    command.arg(dir);
    command.arg("--format").arg("json");

    let output = command.output().unwrap();
    assert!(output.status.success());

    serde_json::from_str(String::from_utf8(output.stdout).unwrap().trim()).unwrap()
}

// =============================================================================
// Lean row grammar helpers
// =============================================================================
//
// The six core commands (`search`, `symbol`, `impact`, `context`, `structural`,
// `get`) all emit a line-oriented grammar described in design §2.2-§2.3. These
// helpers parse that grammar just enough to assert field presence, field shape,
// and cross-row ordering without pulling in a regex dependency.

/// A discovery row produced by `search`, `symbol`, or `impact`:
/// `<score>  <path>:<l1>-<l2>  <kind>  <breadcrumb>::<symbol>  :<segment_id>[  ~<channel>]`.
#[derive(Debug, Clone)]
struct LeanDiscoveryRow {
    score: u32,
    file_path: String,
    line_start: usize,
    line_end: usize,
    kind: String,
    breadcrumb: String,
    symbol: String,
    segment_id: String,
    channel: Option<char>,
}

fn parse_discovery_row(line: &str) -> LeanDiscoveryRow {
    // Fields are separated by two ASCII spaces (design D2). We split on the
    // fixed separator rather than on whitespace so that single spaces inside
    // e.g. breadcrumbs are not misread as a field break.
    let parts: Vec<&str> = line.split("  ").collect();
    assert!(
        parts.len() == 5 || parts.len() == 6,
        "expected 5 or 6 double-space-separated fields, got {} in line: {line:?}",
        parts.len()
    );

    let score: u32 = parts[0]
        .parse()
        .unwrap_or_else(|_| panic!("score field must be integer 0-100, got {:?}", parts[0]));
    assert!(
        score <= 100,
        "score must be in [0,100], got {score} in line: {line:?}"
    );

    let (file_path, line_span) = parts[1]
        .rsplit_once(':')
        .unwrap_or_else(|| panic!("expected <path>:<l1>-<l2>, got {:?}", parts[1]));
    let (l1_raw, l2_raw) = line_span
        .split_once('-')
        .unwrap_or_else(|| panic!("expected <l1>-<l2>, got {line_span:?}"));
    let line_start: usize = l1_raw.parse().expect("l1 is integer");
    let line_end: usize = l2_raw.parse().expect("l2 is integer");

    let (breadcrumb, symbol) = parts[3]
        .split_once("::")
        .unwrap_or_else(|| panic!("expected <breadcrumb>::<symbol>, got {:?}", parts[3]));

    let segment_token = parts[4];
    assert!(
        segment_token.starts_with(':'),
        "segment handle must start with ':', got {segment_token:?}"
    );
    let segment_id = segment_token.trim_start_matches(':').to_string();
    assert!(
        !segment_id.is_empty(),
        "segment id body must be non-empty in {line:?}"
    );

    let channel = if parts.len() == 6 {
        let suffix = parts[5];
        assert!(
            suffix == "~P" || suffix == "~C",
            "channel suffix must be ~P or ~C, got {suffix:?}"
        );
        Some(suffix.chars().nth(1).unwrap())
    } else {
        None
    };

    LeanDiscoveryRow {
        score,
        file_path: file_path.to_string(),
        line_start,
        line_end,
        kind: parts[2].to_string(),
        breadcrumb: breadcrumb.to_string(),
        symbol: symbol.to_string(),
        segment_id,
        channel,
    }
}

fn parse_discovery_rows(stdout: &str) -> Vec<LeanDiscoveryRow> {
    stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(parse_discovery_row)
        .collect()
}

fn run_core_cmd(args: &[&str]) -> (String, String, bool) {
    let output = cmd().args(args).output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    (stdout, stderr, output.status.success())
}

fn run_core_cmd_with_home(home: &Path, args: &[&str]) -> (String, String, bool) {
    let output = cmd_with_home(home).args(args).output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    (stdout, stderr, output.status.success())
}

fn search_rows(dir: &std::path::Path, query: &str) -> Vec<LeanDiscoveryRow> {
    let (stdout, stderr, ok) = run_core_cmd(&["search", query, "--path", dir.to_str().unwrap()]);
    assert!(ok, "search failed: {stderr}");
    parse_discovery_rows(&stdout)
}

fn search_rows_with_home(home: &Path, dir: &std::path::Path, query: &str) -> Vec<LeanDiscoveryRow> {
    let (stdout, stderr, ok) =
        run_core_cmd_with_home(home, &["search", query, "--path", dir.to_str().unwrap()]);
    assert!(ok, "search failed: {stderr}");
    parse_discovery_rows(&stdout)
}

fn git(repo: &std::path::Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap_or_else(|err| panic!("git {args:?} failed to launch: {err}"));
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn create_branch_filtering_fixture() -> (TempDir, PathBuf, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let main_repo = root.join("main");
    let feature_worktree = root.join("feature-worktree");
    fs::create_dir_all(&main_repo).unwrap();

    git(&root, &["init", main_repo.to_str().unwrap()]);
    git(
        &main_repo,
        &["config", "user.email", "oneup-test@example.com"],
    );
    git(&main_repo, &["config", "user.name", "1up Test"]);
    fs::write(
        main_repo.join("shared.rs"),
        "pub fn shared_branch_acceptance_marker() -> &'static str { \"shared branch acceptance sentinel\" }\n",
    )
    .unwrap();
    git(&main_repo, &["add", "."]);
    git(&main_repo, &["commit", "-m", "shared"]);
    git(&main_repo, &["branch", "-M", "main"]);
    git(
        &main_repo,
        &[
            "worktree",
            "add",
            "-b",
            "feature-acceptance",
            feature_worktree.to_str().unwrap(),
            "HEAD",
        ],
    );

    fs::write(
        main_repo.join("main_only.rs"),
        "pub fn main_branch_acceptance_marker() -> &'static str { \"main branch only acceptance sentinel\" }\n",
    )
    .unwrap();
    git(&main_repo, &["add", "."]);
    git(&main_repo, &["commit", "-m", "main only"]);

    fs::write(
        feature_worktree.join("feature_only.rs"),
        "pub fn feature_branch_acceptance_marker() -> &'static str { \"feature branch only acceptance sentinel\" }\n",
    )
    .unwrap();

    (
        tmp,
        main_repo.canonicalize().unwrap(),
        feature_worktree.canonicalize().unwrap(),
    )
}

fn symbol_rows(dir: &std::path::Path, name: &str, extra: &[&str]) -> Vec<LeanDiscoveryRow> {
    let mut args: Vec<&str> = vec!["symbol", name, "--path", dir.to_str().unwrap()];
    args.extend_from_slice(extra);
    let (stdout, _stderr, ok) = run_core_cmd(&args);
    assert!(ok, "symbol lookup failed");
    parse_discovery_rows(&stdout)
}

fn impact_output(dir: &std::path::Path, args: &[&str]) -> (String, String, bool) {
    let mut full: Vec<&str> = vec!["impact"];
    full.extend_from_slice(args);
    full.extend_from_slice(&["--path", dir.to_str().unwrap()]);
    run_core_cmd(&full)
}

fn impact_rows(dir: &std::path::Path, args: &[&str]) -> Vec<LeanDiscoveryRow> {
    let (stdout, stderr, ok) = impact_output(dir, args);
    assert!(ok, "impact failed: {stderr}");
    parse_discovery_rows(
        &stdout
            .lines()
            .filter(|l| {
                !l.starts_with("hint") && !l.starts_with("refused") && !l.starts_with("empty")
            })
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

fn write_parallel_regression_fixture(dir: &std::path::Path) {
    fs::write(
        dir.join("changed.rs"),
        "pub fn alpha_symbol() -> &'static str {\n    \"alpha\"\n}\n",
    )
    .unwrap();
    fs::write(
        dir.join("stable.rs"),
        "pub fn stable_symbol() -> &'static str {\n    \"stable\"\n}\n",
    )
    .unwrap();
    fs::write(
        dir.join("removed.rs"),
        "pub fn removed_symbol() -> &'static str {\n    \"removed\"\n}\n",
    )
    .unwrap();
}

fn mutate_parallel_regression_fixture(dir: &std::path::Path) {
    fs::write(
        dir.join("changed.rs"),
        "pub fn beta_symbol() -> &'static str {\n    \"beta\"\n}\n",
    )
    .unwrap();
    fs::remove_file(dir.join("removed.rs")).unwrap();
    fs::write(
        dir.join("fresh.rs"),
        "pub fn fresh_symbol() -> &'static str {\n    \"fresh\"\n}\n",
    )
    .unwrap();
}

fn create_search_acceptance_fixture() -> TempDir {
    let tmp = TempDir::new().unwrap();
    fs::create_dir_all(tmp.path().join("src")).unwrap();
    fs::create_dir_all(tmp.path().join("config")).unwrap();
    fs::create_dir_all(tmp.path().join("docs")).unwrap();
    fs::create_dir_all(tmp.path().join("proto")).unwrap();
    fs::create_dir_all(tmp.path().join("sql")).unwrap();

    fs::write(
        tmp.path().join("src").join("policy.rs"),
        r#"pub struct PolicyRuleValidator;

impl PolicyRuleValidator {
    pub fn validate(&self, policy: &str) -> bool {
        !policy.is_empty()
    }
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("src").join("runner.rs"),
        r#"use crate::policy::PolicyRuleValidator;

pub fn run_validation(validator: &PolicyRuleValidator) -> bool {
    validator.validate("allow")
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("src").join("signatures.rs"),
        r#"// validate incoming request signatures
pub fn validate_incoming_request_signatures(secret: &str, header: &str) -> bool {
    !secret.is_empty() && header.contains(secret)
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("config").join("signatures.yaml"),
        r#"request_signing_secret: test-secret
description: request signing secret used for request validation
policy_rule_preview_enabled: true
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("docs").join("signatures.md"),
        r#"# Request signing documentation guide

Use config/signatures.yaml to set the request signing secret for local development.
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("proto").join("policy_rules.proto"),
        r#"syntax = "proto3";

message PolicyRulePreview {
  string id = 1;
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("sql").join("policy_rules.sql"),
        r#"CREATE TABLE policy_rules_preview (
    id TEXT PRIMARY KEY,
    validator_name TEXT NOT NULL
);
"#,
    )
    .unwrap();

    tmp
}

fn create_ambiguous_handle_fixture() -> TempDir {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir_all(&src).unwrap();

    for i in 0..20 {
        fs::write(
            src.join(format!("ambiguous_{i:02}.rs")),
            format!("pub fn ambiguous_collision_token_{i:02}() -> usize {{ {i} }}\n"),
        )
        .unwrap();
    }

    tmp
}

fn ambiguous_handle_prefix(handles: &[String]) -> String {
    for prefix_len in 1..=12 {
        let mut counts = std::collections::BTreeMap::new();
        for handle in handles {
            if handle.len() >= prefix_len {
                *counts
                    .entry(handle[..prefix_len].to_string())
                    .or_insert(0usize) += 1;
            }
        }
        if let Some((prefix, _)) = counts.into_iter().find(|(_, count)| *count > 1) {
            return prefix;
        }
    }

    panic!("expected at least one ambiguous handle prefix in {handles:?}");
}

fn create_impact_acceptance_fixture() -> TempDir {
    let tmp = TempDir::new().unwrap();
    for dir in [
        "src/admin",
        "src/app",
        "src/auth",
        "src/cache",
        "src/contracts",
        "src/ui",
        "tests",
    ] {
        fs::create_dir_all(tmp.path().join(dir)).unwrap();
    }

    fs::write(
        tmp.path().join("src").join("auth").join("runtime.rs"),
        r#"pub fn load_auth_config() -> &'static str {
    "auth"
}

pub fn parse_auth_config(raw: &str) -> bool {
    !raw.trim().is_empty()
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("src").join("auth").join("bootstrap.rs"),
        r#"use crate::auth::runtime::load_auth_config;

pub fn boot_auth() -> &'static str {
    load_auth_config()
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("tests").join("auth_runtime_test.rs"),
        r#"use crate::auth::runtime::load_auth_config;

#[test]
fn loads_auth_runtime() {
    assert_eq!(load_auth_config(), "auth");
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("src").join("auth").join("config.rs"),
        r#"pub fn load_config() -> &'static str {
    "auth-scope"
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path()
            .join("src")
            .join("auth")
            .join("config_builder.rs"),
        r#"use crate::auth::config::load_config;

pub fn build_auth_config() -> &'static str {
    load_config()
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("src").join("auth").join("reload.rs"),
        r#"pub fn reload_auth_config() -> &'static str {
    crate::auth::config::load_config()
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path()
            .join("src")
            .join("contracts")
            .join("auth_store.ts"),
        r#"export interface BaseAuthStore {
    get(key: string): string | null;
}

export interface AuthStore extends BaseAuthStore {
    set(key: string, value: string): void;
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("src").join("auth").join("auth_store.ts"),
        r#"import type { AuthStore } from "../contracts/auth_store";

export class SqlAuthStore implements AuthStore {
    get(key: string): string | null {
        return key;
    }

    set(key: string, value: string): void {
        void value;
    }
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path()
            .join("src")
            .join("contracts")
            .join("formatter.ts"),
        r#"export interface Formatter {
    format(value: string): string;
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("src").join("ui").join("plain_formatter.ts"),
        r#"import type { Formatter } from "../contracts/formatter";

export class PlainFormatter implements Formatter {
    format(value: string): string {
        return value.trim();
    }
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("src").join("ui").join("render_search.ts"),
        r#"import type { Formatter } from "../contracts/formatter";

export function renderSearch(formatter: Formatter, value: string): string {
    return formatter.format(value);
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("src").join("ui").join("render_status.ts"),
        r#"import type { Formatter } from "../contracts/formatter";

export function renderStatus(formatter: Formatter, value: string): string {
    return formatter.format(value);
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("src").join("cache").join("config.rs"),
        r#"pub fn load_config() -> &'static str {
    "cache"
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("src").join("cache").join("runtime.rs"),
        r#"pub fn warm_cache_key() -> &'static str {
    "cache"
}

pub fn normalize_cache_key(raw: &str) -> String {
    raw.trim().to_lowercase()
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("src").join("cache").join("priming.rs"),
        r#"use crate::cache::runtime::warm_cache_key;

pub fn prime_cache() -> &'static str {
    warm_cache_key()
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("src").join("cache").join("worker.rs"),
        r#"use crate::cache::runtime::{normalize_cache_key, warm_cache_key};

pub fn warm_cache_for_request(user_key: &str) -> String {
    let normalized = normalize_cache_key(user_key);
    if normalized.is_empty() {
        return warm_cache_key().to_string();
    }
    format!("{}:{}", warm_cache_key(), normalized)
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("src").join("cache").join("test_support.rs"),
        r#"mod cache_tests {
    use crate::cache::runtime::warm_cache_key;

    fn inline_warm_cache_test() {
        assert_eq!(warm_cache_key(), "cache");
    }
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("src").join("ui").join("config.rs"),
        r#"pub fn load_config() -> &'static str {
    "ui"
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("src").join("admin").join("config.rs"),
        r#"pub fn load_config() -> &'static str {
    "admin"
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("src").join("app").join("bootstrap.rs"),
        r#"use crate::auth::config::load_config;

pub fn boot_global_config() -> &'static str {
    load_config()
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("tests").join("config_fixture.rs"),
        r#"pub fn load_config() -> &'static str {
    "tests"
}
"#,
    )
    .unwrap();

    tmp
}

// =============================================================================
// Indexing / storage integration
// =============================================================================

#[test]
fn index_multi_language_repository() {
    let tmp = create_multi_lang_fixture();
    init_and_index(&tmp);

    let db_path = tmp.path().join(".1up").join("index.db");
    assert!(
        db_path.exists(),
        "index.db should be created after indexing"
    );
}

// =============================================================================
// Lean row grammar — search / symbol / impact / get
// =============================================================================

#[test]
fn search_row_grammar() {
    // design §2.2: every search hit is one line of
    // `<score>  <path>:<l1>-<l2>  <kind>  <breadcrumb>::<symbol>  :<segment_id>`.
    let tmp = create_multi_lang_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let rows = search_rows(tmp.path(), "Config host port");
    assert!(
        !rows.is_empty(),
        "search for 'Config host port' should return rows"
    );
    for row in &rows {
        assert!(
            !row.file_path.is_empty() && !row.kind.is_empty(),
            "required fields must be populated: {row:?}"
        );
        assert!(
            row.line_end >= row.line_start,
            "l2 >= l1 invariant violated: {row:?}"
        );
        assert!(
            row.channel.is_none(),
            "search rows must not carry a channel suffix: {row:?}"
        );
        // `:<segment_id>` is 1 to 12 chars of lowercase hex (design D3).
        assert!(row.segment_id.len() <= 12);
        assert!(
            row.segment_id
                .chars()
                .all(|c| c.is_ascii_hexdigit() || c == '_' || c.is_ascii_alphanumeric()),
            "segment id must be ascii alphanumeric hex-ish: {row:?}"
        );
    }
}

#[test]
fn search_default_limit_caps_results_at_three() {
    // design §3.4: `1up search <query>` defaults to -n=3. The fixture
    // produces more than three matches for "config", so we pin the cap.
    let tmp = create_multi_lang_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let rows = search_rows(tmp.path(), "config");
    assert!(
        rows.len() <= 3,
        "default limit is 3, got {} rows",
        rows.len()
    );
}

#[test]
fn search_lean_output_contains_no_segment_prefix_literal() {
    // design D-grammar: the `:<id>` trailing token replaces the old
    // `segment=<id>` metadata substring. Grep-style guard against regressions.
    let tmp = create_multi_lang_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let (stdout, _stderr, ok) =
        run_core_cmd(&["search", "Config", "--path", tmp.path().to_str().unwrap()]);
    assert!(ok);
    assert!(
        !stdout.contains("segment="),
        "lean search output must not include `segment=`: {stdout}"
    );
}

#[test]
fn symbol_uses_same_row_grammar() {
    // Symbol rows reuse the discovery grammar with a `<reference_kind>:<kind>`
    // composite in the kind slot (design §2.2, §3.5).
    let tmp = create_multi_lang_fixture();
    init_and_index(&tmp);

    let rows = symbol_rows(tmp.path(), "greet", &[]);
    assert!(!rows.is_empty(), "symbol 'greet' should resolve to a row");
    for row in &rows {
        assert!(
            row.kind.starts_with("def:") || row.kind.starts_with("usage:"),
            "symbol kind must be `def:<k>` or `usage:<k>`, got {:?}",
            row.kind
        );
        assert!(
            row.channel.is_none(),
            "symbol rows must not carry a channel suffix: {row:?}"
        );
        assert_eq!(row.score, 0, "symbol rows have no score; grammar fills 0");
    }
    assert!(
        rows.iter()
            .any(|r| r.symbol == "greet" || r.breadcrumb.contains("greet")),
        "greet should appear somewhere in symbol output: {rows:?}"
    );
}

#[test]
fn symbol_references_include_definitions_and_usages() {
    let tmp = create_search_acceptance_fixture();
    init_and_index(&tmp);

    let rows = symbol_rows(tmp.path(), "PolicyRuleValidator", &["--references"]);
    assert!(
        rows.iter()
            .any(|r| r.kind.starts_with("def:") && r.file_path == "src/policy.rs"),
        "definition row missing: {rows:?}"
    );
    assert!(
        rows.iter()
            .any(|r| r.kind.starts_with("usage:") && r.file_path == "src/runner.rs"),
        "usage row missing: {rows:?}"
    );
}

#[test]
fn symbol_handle_roundtrips_through_get_and_impact() {
    // The advertised flow is `symbol -> get -> impact --from-segment`. The
    // 12-char handle printed by `symbol` must resolve both through `get` (full
    // segment body) and through `impact --from-segment` (anchor expansion).
    let tmp = create_multi_lang_fixture();
    init_and_index(&tmp);

    let rows = symbol_rows(tmp.path(), "greet", &[]);
    let row = rows
        .iter()
        .find(|r| r.kind.starts_with("def:"))
        .expect("expected a definition row for `greet`");
    let handle = row.segment_id.clone();
    assert_eq!(
        handle.len(),
        12,
        "symbol row must carry a 12-char lean handle, got {handle:?}"
    );

    let (get_out, get_err, get_ok) =
        run_core_cmd(&["get", &handle, "--path", tmp.path().to_str().unwrap()]);
    assert!(get_ok, "get failed: {get_err}");
    assert!(
        get_out.starts_with("segment "),
        "get should resolve the handle and emit a segment record, got: {get_out}"
    );
    assert!(
        !get_out.starts_with("not_found"),
        "handle `{handle}` must not resolve to not_found: {get_out}"
    );

    let (impact_out, impact_err, impact_ok) = run_core_cmd(&[
        "impact",
        "--from-segment",
        &handle,
        "--path",
        tmp.path().to_str().unwrap(),
    ]);
    assert!(impact_ok, "impact failed: {impact_err}");
    assert!(
        !impact_out.contains("anchor_not_found"),
        "impact --from-segment must accept the 12-char handle; got: {impact_out}"
    );
    assert!(
        !impact_out.contains("anchor_ambiguous"),
        "impact --from-segment should uniquely resolve a 12-char handle for a definition; got: {impact_out}"
    );
}

#[test]
fn search_acceptance_queries_preserve_top_hit_for_priority_classes() {
    // Ranking stability: each acceptance query should keep the expected top
    // file across two consecutive runs (covers the "handoff does not perturb
    // search ranking" contract at the grammar layer).
    let tmp = create_search_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let cases = [
        (
            "request_signing_secret policy_rule_preview_enabled",
            "config/signatures.yaml",
        ),
        (
            "api documentation guide local development",
            "docs/signatures.md",
        ),
        ("validate incoming request signatures", "src/signatures.rs"),
        ("PolicyRulePreview", "proto/policy_rules.proto"),
        ("policy_rules_preview table", "sql/policy_rules.sql"),
    ];

    for (query, expected_top_path) in cases {
        let first = search_rows(tmp.path(), query);
        let second = search_rows(tmp.path(), query);

        assert!(
            !first.is_empty(),
            "query {query:?} should produce at least one row"
        );
        assert_eq!(
            first[0].file_path, expected_top_path,
            "query {query:?} returned an unexpected top hit"
        );

        let first_paths: Vec<_> = first.iter().take(3).map(|r| r.file_path.clone()).collect();
        let second_paths: Vec<_> = second.iter().take(3).map(|r| r.file_path.clone()).collect();
        assert_eq!(
            first_paths, second_paths,
            "query {query:?} should keep a stable top-3 result set"
        );
    }
}

// =============================================================================
// Impact lean envelope
// =============================================================================

fn impact_status_line(stdout: &str) -> Option<&str> {
    stdout.lines().find(|l| {
        let token = l.split("  ").next().unwrap_or("");
        matches!(token, "refused" | "empty" | "empty_scoped")
    })
}

#[test]
fn mcp_initialize_advertises_primary_code_search_instructions() {
    let tmp = TempDir::new().unwrap();
    let (_client, initialize_response) = McpTestClient::start_with_initialize_response(tmp.path());

    let result = &initialize_response["result"];
    let instructions = result["instructions"]
        .as_str()
        .expect("initialize should expose server instructions");
    assert!(
        instructions.contains("primary code-search interface"),
        "instructions should position 1up as the primary code-search path: {instructions}"
    );
    assert!(
        instructions.contains("oneup_search before raw grep, rg, find"),
        "instructions should guide agents to use MCP before broad raw search: {instructions}"
    );
    assert!(
        instructions.contains("oneup_read"),
        "instructions should teach the search/read hydration flow: {instructions}"
    );
    assert!(
        instructions.contains("oneup_read locations for file-line context"),
        "instructions should make read-location context retrieval discoverable: {instructions}"
    );
    assert!(
        instructions.contains("Use oneup_impact only for explicit blast-radius questions"),
        "instructions should keep impact out of the default core discovery loop: {instructions}"
    );
    assert!(
        instructions.contains(&tmp.path().display().to_string()),
        "instructions should include the configured repository: {instructions}"
    );
    assert_eq!(
        result["serverInfo"]["description"],
        "Primary local code search and discovery MCP server"
    );
}

#[cfg(unix)]
#[test]
fn mcp_rejects_second_instance_for_same_project_root() {
    let tmp = TempDir::new().unwrap();
    let _client = McpTestClient::start(tmp.path());

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_1up"))
        .args(["mcp", "--path", tmp.path().to_str().unwrap()])
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "second MCP process should exit while first one holds the lock"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("already running"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn mcp_tools_list_default_palette_and_schemas() {
    let tmp = TempDir::new().unwrap();
    let mut client = McpTestClient::start(tmp.path());

    let result = client.list_tools();
    let tools = result["tools"].as_array().unwrap();
    let names = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect::<BTreeSet<_>>();

    assert_eq!(
        names,
        BTreeSet::from([
            "oneup_impact",
            "oneup_prepare",
            "oneup_read",
            "oneup_search",
            "oneup_symbol",
        ])
    );
    assert_eq!(tools.len(), 5);

    for tool in tools {
        let name = tool["name"].as_str().unwrap();
        let description = tool["description"].as_str().unwrap();
        assert!(name.starts_with("oneup_"));
        assert!(!name.starts_with("1up_"));
        assert!(
            description.starts_with("Check ")
                || description.starts_with("Search ")
                || description.starts_with("Read ")
                || description.starts_with("Find ")
                || description.starts_with("Explore "),
            "description should front-load tool selection guidance: {description}"
        );
        if name == "oneup_search" {
            assert!(
                description.contains("primary discovery path")
                    && description.contains("before raw grep, rg, find"),
                "oneup_search description should be strong enough for tool adoption: {description}"
            );
        }
        if name == "oneup_read" {
            assert!(
                description.contains("file-line context") && description.contains("locations"),
                "oneup_read description should expose read-location context retrieval: {description}"
            );
            let input_schema = tool
                .get("inputSchema")
                .or_else(|| tool.get("input_schema"))
                .expect("oneup_read should expose an input schema");
            let locations_schema = &input_schema["properties"]["locations"];
            assert!(
                locations_schema["description"]
                    .as_str()
                    .unwrap_or("")
                    .contains("file-line context retrieval"),
                "locations schema should describe file-line context retrieval: {input_schema:?}"
            );
            let location_def = &input_schema["$defs"]["ReadLocationInput"];
            assert!(
                location_def["description"]
                    .as_str()
                    .unwrap_or("")
                    .contains("file-line location"),
                "location schema should define the context location shape: {input_schema:?}"
            );
            assert!(
                location_def["properties"]["line"]["description"]
                    .as_str()
                    .unwrap_or("")
                    .contains("1-based"),
                "location line schema should state 1-based input: {input_schema:?}"
            );
        }
        if name == "oneup_impact" {
            assert!(
                description.contains("explicit blast-radius questions"),
                "oneup_impact description should keep impact as an explicit non-core follow-up: {description}"
            );
        }
        assert!(
            tool.get("inputSchema").is_some() || tool.get("input_schema").is_some(),
            "tool should expose an input schema: {tool:?}"
        );

        let output_schema = tool
            .get("outputSchema")
            .or_else(|| tool.get("output_schema"))
            .expect("tool should expose an output schema");
        assert_eq!(
            output_schema["properties"]["data"]["type"],
            "object",
            "dynamic envelope data should use an object schema instead of boolean true for OpenCode compatibility: {output_schema:?}"
        );
        assert_eq!(
            output_schema["$defs"]["NextAction"]["properties"]["arguments"]["type"],
            "object",
            "dynamic next-action arguments should use an object schema instead of boolean true for OpenCode compatibility: {output_schema:?}"
        );
    }
}

#[test]
fn mcp_prepare_reports_readiness_states_and_next_actions() {
    let missing = TempDir::new().unwrap();
    fs::create_dir_all(missing.path().join(".git")).unwrap();
    let mut missing_client = McpTestClient::start_with_isolated_state(missing.path());
    let missing_result = missing_client.call_tool("oneup_prepare", serde_json::json!({}));
    let missing_envelope = mcp_structured(&missing_result);
    assert_mcp_response_is_presentation_free(&missing_result);
    assert_eq!(missing_envelope["status"], "missing");
    assert_eq!(missing_envelope["data"]["project_initialized"], true);
    assert_eq!(
        missing_envelope["next_actions"][0]["arguments"]["mode"],
        "index_if_missing"
    );
    assert_mcp_next_actions_are_canonical(missing_envelope);

    let indexing = TempDir::new().unwrap();
    write_running_progress(indexing.path());
    let mut indexing_client = McpTestClient::start_with_isolated_state(indexing.path());
    let indexing_result = indexing_client.call_tool("oneup_prepare", serde_json::json!({}));
    let indexing_envelope = mcp_structured(&indexing_result);
    assert_mcp_response_is_presentation_free(&indexing_result);
    assert_eq!(indexing_envelope["status"], "indexing");
    assert_eq!(
        indexing_envelope["next_actions"][0]["tool"],
        "oneup_prepare"
    );
    assert_eq!(
        indexing_envelope["next_actions"][0]["arguments"]["mode"],
        "check"
    );

    let auto_result =
        indexing_client.call_tool("oneup_prepare", serde_json::json!({ "mode": "auto" }));
    let auto_envelope = mcp_structured(&auto_result);
    assert_mcp_response_is_presentation_free(&auto_result);
    assert_eq!(auto_envelope["status"], "indexing");
    assert_mcp_next_actions_are_canonical(auto_envelope);

    let default_result =
        indexing_client.call_tool("oneup_prepare", serde_json::json!({ "mode": "default" }));
    let default_envelope = mcp_structured(&default_result);
    assert_mcp_response_is_presentation_free(&default_result);
    assert_eq!(default_envelope["status"], "indexing");
    assert_mcp_next_actions_are_canonical(default_envelope);

    let read_result =
        indexing_client.call_tool("oneup_prepare", serde_json::json!({ "mode": "read" }));
    let read_envelope = mcp_structured(&read_result);
    assert_mcp_response_is_presentation_free(&read_result);
    assert_eq!(read_envelope["status"], "indexing");
    assert_mcp_next_actions_are_canonical(read_envelope);

    let stale = TempDir::new().unwrap();
    fs::create_dir_all(stale.path().join(".1up")).unwrap();
    fs::write(stale.path().join(".1up").join("project_id"), "test-project").unwrap();
    fs::write(
        stale.path().join(".1up").join("index.db"),
        b"not a current schema",
    )
    .unwrap();
    let mut stale_client = McpTestClient::start_with_isolated_state(stale.path());
    let stale_result = stale_client.call_tool("oneup_prepare", serde_json::json!({}));
    let stale_envelope = mcp_structured(&stale_result);
    assert_mcp_response_is_presentation_free(&stale_result);
    assert_eq!(stale_envelope["status"], "stale");
    assert_eq!(stale_envelope["next_actions"][0]["tool"], "oneup_prepare");
    assert_eq!(
        stale_envelope["next_actions"][0]["arguments"]["mode"],
        "reindex"
    );

    {
        let _model_guard = RestoreHiddenModelGuard::new();
        let ready = create_multi_lang_fixture();
        init_and_index(&ready);
        let mut ready_client = McpTestClient::start(ready.path());
        let ready_result = ready_client.call_tool("oneup_prepare", serde_json::json!({}));
        let ready_envelope = mcp_structured(&ready_result);
        assert_mcp_response_is_presentation_free(&ready_result);
        assert_eq!(ready_envelope["status"], "degraded");
        assert_eq!(ready_envelope["data"]["index_readable"], true);
        assert_eq!(ready_envelope["data"]["branch_status"], "unknown");
        assert_eq!(ready_envelope["data"]["last_update_state"], "complete");
        assert_eq!(ready_envelope["next_actions"][0]["tool"], "oneup_search");
    }

    let degraded = create_multi_lang_fixture();
    let _guard = init_and_index_fts_only(&degraded);
    let mut degraded_client = McpTestClient::start(degraded.path());
    let degraded_result = degraded_client.call_tool("oneup_prepare", serde_json::json!({}));
    let degraded_envelope = mcp_structured(&degraded_result);
    assert_mcp_response_is_presentation_free(&degraded_result);
    assert_eq!(degraded_envelope["status"], "degraded");
    assert_eq!(degraded_envelope["data"]["index_readable"], true);
    assert!(
        degraded_envelope["next_actions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|action| action["tool"] == "oneup_search"),
        "degraded readiness should still allow search as a next action"
    );
}

#[test]
fn mcp_prepare_ignores_index_progress_from_other_context() {
    let project = TempDir::new().unwrap();
    fs::create_dir_all(project.path().join(".git")).unwrap();
    write_running_progress_for_context(project.path(), "other-context");

    let mut client = McpTestClient::start_with_isolated_state(project.path());
    let result = client.call_tool("oneup_prepare", serde_json::json!({}));
    let envelope = mcp_structured(&result);

    assert_ne!(result["isError"], true);
    assert_mcp_response_is_presentation_free(&result);
    assert_eq!(envelope["status"], "missing");
    assert!(
        envelope["data"].get("index_progress").is_none(),
        "non-active context progress should not be exposed in readiness data: {envelope:?}"
    );
    assert_eq!(envelope["data"]["last_update_state"], "unknown");
    assert_eq!(
        envelope["next_actions"][0]["arguments"]["mode"],
        "index_if_missing"
    );
}

#[test]
fn mcp_prepare_ignores_index_rows_from_other_context() {
    let project = TempDir::new().unwrap();
    let project_root = project.path().canonicalize().unwrap();
    fs::create_dir_all(project_root.join(".git")).unwrap();
    seed_current_index_for_context(&project_root, "other-context");

    let mut client = McpTestClient::start_with_isolated_state(&project_root);
    let result = client.call_tool("oneup_prepare", serde_json::json!({}));
    let envelope = mcp_structured(&result);

    assert_ne!(result["isError"], true);
    assert_mcp_response_is_presentation_free(&result);
    assert_eq!(envelope["status"], "missing");
    assert_eq!(envelope["data"]["index_readable"], true);
    assert_eq!(envelope["data"]["indexed_files"], 0);
    assert_eq!(envelope["data"]["total_segments"], 0);
    assert_eq!(
        envelope["next_actions"][0]["arguments"]["mode"],
        "index_if_missing"
    );
}

#[test]
fn mcp_prepare_index_if_missing_builds_index_state_only() {
    let tmp = TempDir::new().unwrap();
    fs::create_dir_all(tmp.path().join(".git")).unwrap();
    fs::create_dir_all(tmp.path().join("src")).unwrap();

    let source_path = tmp.path().join("src").join("lib.rs");
    let source_content = "pub fn readiness_probe() -> &'static str {\n    \"ready\"\n}\n";
    fs::write(&source_path, source_content).unwrap();

    let _guard = HideModelGuard::new();
    let mut client = McpTestClient::start(tmp.path());
    let mut result = client.call_tool(
        "oneup_prepare",
        serde_json::json!({ "mode": "index_if_missing" }),
    );
    for _ in 0..20 {
        let status = mcp_structured(&result)["status"]
            .as_str()
            .map(str::to_owned);
        if status.as_deref() != Some("stale") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
        result = client.call_tool("oneup_prepare", serde_json::json!({}));
    }
    let envelope = mcp_structured(&result);

    assert_ne!(result["isError"], true);
    assert_mcp_response_is_presentation_free(&result);
    assert!(
        matches!(envelope["status"].as_str(), Some("ready" | "degraded")),
        "explicit indexing should produce searchable readiness: {envelope:?}"
    );
    assert_eq!(envelope["data"]["index_present"], true);
    assert_eq!(envelope["data"]["index_readable"], true);
    assert!(
        envelope["data"]["total_segments"].as_u64().unwrap() > 0,
        "explicit MCP indexing should create searchable segment state"
    );
    assert_eq!(fs::read_to_string(source_path).unwrap(), source_content);
    assert!(tmp.path().join(".1up").join("index.db").exists());
    assert_mcp_next_actions_are_canonical(envelope);
}

#[test]
fn mcp_prepare_reports_blocked_when_indexing_cannot_auto_initialize() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("loose.rs"),
        "pub fn loose_directory() -> &'static str { \"not a repo\" }\n",
    )
    .unwrap();

    let mut client = McpTestClient::start(tmp.path());
    let result = client.call_tool(
        "oneup_prepare",
        serde_json::json!({ "mode": "index_if_missing" }),
    );
    let envelope = mcp_structured(&result);

    assert_ne!(result["isError"], true);
    assert_mcp_response_is_presentation_free(&result);
    assert_eq!(envelope["status"], "blocked");
    assert_eq!(envelope["data"]["status"], "blocked");
    assert!(
        envelope["data"]["reason"]
            .as_str()
            .unwrap()
            .contains("automatic project creation requires an existing 1up project or a git root"),
        "blocked readiness should explain why MCP could not create index state: {envelope:?}"
    );
    assert!(!tmp.path().join(".1up").exists());
    assert_mcp_next_actions_are_canonical(envelope);
}

#[test]
fn mcp_core_discovery_loop_returns_structured_evidence() {
    let tmp = create_search_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);
    let mut client = McpTestClient::start(tmp.path());

    let prepare = client.call_tool("oneup_prepare", serde_json::json!({}));
    assert_ne!(prepare["isError"], true);
    assert_mcp_response_is_presentation_free(&prepare);
    let prepare_envelope = mcp_structured(&prepare);
    assert!(
        matches!(
            prepare_envelope["status"].as_str(),
            Some("ready" | "degraded")
        ),
        "prepare should be ready or degraded depending on local model cache: {prepare_envelope:?}"
    );
    assert_eq!(prepare_envelope["data"]["index_readable"], true);
    assert!(
        prepare_envelope["next_actions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|action| action["tool"] == "oneup_search"),
        "ready or degraded prepare should lead agents to search: {prepare_envelope:?}"
    );

    let search = client.call_tool(
        "oneup_search",
        serde_json::json!({ "query": "PolicyRuleValidator", "limit": 3 }),
    );
    assert_ne!(search["isError"], true);
    assert_mcp_response_is_presentation_free(&search);
    assert_mcp_text_matches_summary(&search);
    let search_text = search["content"][0]["text"].as_str().unwrap();
    assert!(
        search_text.contains("src/policy.rs:"),
        "oneup_search text should include ranked rows, not only a count summary: {search_text}"
    );
    assert!(
        search_text
            .lines()
            .any(|line| line.contains("PolicyRuleValidator")
                && line.contains(":")
                && line.contains("  ")),
        "oneup_search text should include a CLI-like row with symbol and handle: {search_text}"
    );
    let search_envelope = mcp_structured(&search);
    assert!(
        matches!(search_envelope["status"].as_str(), Some("ok" | "degraded")),
        "search should be ok or degraded depending on local model cache: {search_envelope:?}"
    );
    assert_mcp_next_actions_are_canonical(search_envelope);
    let search_actions = search_envelope["next_actions"].as_array().unwrap();
    assert!(
        search_actions
            .iter()
            .any(|action| action["tool"] == "oneup_read"
                && action["arguments"]["handles"].is_array()),
        "search hits should offer handle hydration: {search_actions:?}"
    );
    assert!(
        search_actions
            .iter()
            .any(|action| action["tool"] == "oneup_read"
                && action["arguments"]["locations"].is_array()),
        "search hits should offer file-line context retrieval: {search_actions:?}"
    );
    assert!(
        search_actions
            .iter()
            .any(|action| action["tool"] == "oneup_symbol"),
        "search hits with symbol hints should offer symbol verification: {search_actions:?}"
    );
    assert!(
        search_actions
            .iter()
            .all(|action| action["tool"] != "oneup_impact"),
        "impact should not be a primary P2 search next action: {search_actions:?}"
    );

    let hit = &search_envelope["data"]["results"][0];
    let handle = hit["handle"].as_str().unwrap();
    assert!(!handle.is_empty());
    assert_eq!(hit["path"], "src/policy.rs");
    assert!(!hit["kind"].as_str().unwrap().is_empty());
    assert!(hit["score"].as_u64().unwrap() <= 100);
    assert!(hit["line_end"].as_u64().unwrap() >= hit["line_start"].as_u64().unwrap());

    let read_handle = client.call_tool(
        "oneup_read",
        serde_json::json!({ "handles": [format!(":{handle}")] }),
    );
    assert_mcp_response_is_presentation_free(&read_handle);
    assert_mcp_text_matches_summary(&read_handle);
    let read_handle_text = read_handle["content"][0]["text"].as_str().unwrap();
    assert!(
        read_handle_text.contains("segment "),
        "oneup_read text should include hydrated segment records, not only a status summary: {read_handle_text}"
    );
    assert!(
        read_handle_text.contains("PolicyRuleValidator"),
        "oneup_read text should include hydrated source content: {read_handle_text}"
    );
    let read_handle_envelope = mcp_structured(&read_handle);
    assert_eq!(read_handle_envelope["status"], "ok");
    assert_eq!(
        read_handle_envelope["data"]["records"][0]["segment"]["path"],
        "src/policy.rs"
    );
    assert!(
        read_handle_envelope["data"]["records"][0]["segment"]["content"]
            .as_str()
            .unwrap()
            .contains("PolicyRuleValidator")
    );
    assert_mcp_next_actions_are_canonical(read_handle_envelope);
    let read_handle_actions = read_handle_envelope["next_actions"].as_array().unwrap();
    assert!(
        read_handle_actions
            .iter()
            .any(|action| action["tool"] == "oneup_read"
                && action["arguments"]["locations"].is_array()),
        "hydrated segments should offer surrounding file-line context: {read_handle_actions:?}"
    );
    assert!(
        read_handle_actions
            .iter()
            .any(|action| action["tool"] == "oneup_symbol"),
        "hydrated defining segments should offer symbol verification: {read_handle_actions:?}"
    );
    assert!(
        read_handle_actions
            .iter()
            .all(|action| action["tool"] != "oneup_impact"),
        "impact should not be a primary P2 read next action: {read_handle_actions:?}"
    );

    let read_location = client.call_tool(
        "oneup_read",
        serde_json::json!({
            "locations": [{ "path": "src/policy.rs", "line": 4, "expansion": 2 }]
        }),
    );
    assert_mcp_response_is_presentation_free(&read_location);
    let read_location_text = read_location["content"][0]["text"].as_str().unwrap();
    assert!(
        read_location_text.contains("context src/policy.rs:"),
        "oneup_read location text should include hydrated context records: {read_location_text}"
    );
    assert!(
        read_location_text.contains("validate(&self"),
        "oneup_read location text should include source context: {read_location_text}"
    );
    let read_location_envelope = mcp_structured(&read_location);
    assert_eq!(read_location_envelope["status"], "ok");
    assert_eq!(
        read_location_envelope["data"]["records"][0]["context"]["path"],
        "src/policy.rs"
    );
    assert!(
        read_location_envelope["data"]["records"][0]["context"]["line_start"]
            .as_u64()
            .unwrap()
            <= 4
    );

    let symbol = client.call_tool(
        "oneup_symbol",
        serde_json::json!({ "name": "PolicyRuleValidator", "include": "both" }),
    );
    assert_mcp_response_is_presentation_free(&symbol);
    assert_mcp_text_matches_summary(&symbol);
    let symbol_envelope = mcp_structured(&symbol);
    assert_eq!(symbol_envelope["status"], "ok");
    assert!(symbol_envelope["data"]["definitions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|record| record["path"] == "src/policy.rs"
            && !record["handle"].as_str().unwrap().is_empty()));
    assert!(symbol_envelope["data"]["references"]
        .as_array()
        .unwrap()
        .iter()
        .any(|record| record["path"] == "src/runner.rs"
            && !record["handle"].as_str().unwrap().is_empty()));
    assert_mcp_next_actions_are_canonical(symbol_envelope);
    let symbol_actions = symbol_envelope["next_actions"].as_array().unwrap();
    assert!(
        symbol_actions
            .iter()
            .any(|action| action["tool"] == "oneup_read"
                && action["arguments"]["handles"].is_array()),
        "symbol results should offer handle hydration: {symbol_actions:?}"
    );
    assert!(
        symbol_actions
            .iter()
            .any(|action| action["tool"] == "oneup_read"
                && action["arguments"]["locations"].is_array()),
        "symbol results should offer file-line context retrieval: {symbol_actions:?}"
    );
    assert!(
        symbol_actions
            .iter()
            .all(|action| action["tool"] != "oneup_impact"),
        "impact should not be a primary P2 symbol next action: {symbol_actions:?}"
    );
}

#[test]
fn mcp_read_locations_returns_context_and_structured_location_failures() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::write(
        repo.join("src").join("policy.rs"),
        "pub fn policy_context() -> bool {\n    true\n}\n",
    )
    .unwrap();
    let outside = tmp.path().join("outside.rs");
    fs::write(&outside, "pub fn outside_context() {}\n").unwrap();

    let mut client = McpTestClient::start(&repo);
    let result = client.call_tool(
        "oneup_read",
        serde_json::json!({
            "locations": [
                { "path": "src/policy.rs", "line": 2, "expansion": 1 },
                { "path": "src/policy.rs", "line": 0 },
                { "path": "../outside.rs", "line": 1 },
                { "path": outside.to_str().unwrap(), "line": 1 },
                { "path": "src/missing.rs", "line": 1 }
            ]
        }),
    );

    assert_ne!(result["isError"], true);
    assert_mcp_response_is_presentation_free(&result);
    assert_mcp_text_matches_summary(&result);
    let envelope = mcp_structured(&result);
    assert_eq!(envelope["status"], "partial");
    assert!(
        envelope["summary"]
            .as_str()
            .unwrap()
            .contains("file-line context"),
        "read-location summaries should identify context retrieval: {envelope:?}"
    );

    let records = envelope["data"]["records"].as_array().unwrap();
    assert_eq!(records.len(), 5);
    assert_eq!(records[0]["status"], "found");
    assert_eq!(records[0]["source"]["kind"], "location");
    assert_eq!(records[0]["context"]["path"], "src/policy.rs");
    assert!(records[0]["context"]["content"]
        .as_str()
        .unwrap()
        .contains("policy_context"));

    assert_eq!(records[1]["status"], "rejected");
    assert!(records[1]["message"].as_str().unwrap().contains("1-based"));
    assert_eq!(records[2]["status"], "rejected");
    assert!(records[2]["message"]
        .as_str()
        .unwrap()
        .contains("outside the configured repository"));
    assert_eq!(records[3]["status"], "rejected");
    assert!(records[3]["message"]
        .as_str()
        .unwrap()
        .contains("outside the configured repository"));
    assert_eq!(records[4]["status"], "error");
    assert!(records[4]["message"].is_string());
    assert_mcp_next_actions_are_canonical(envelope);

    let failed = client.call_tool(
        "oneup_read",
        serde_json::json!({
            "locations": [
                { "path": "src/policy.rs", "line": 0 },
                { "path": "../outside.rs", "line": 1 }
            ]
        }),
    );
    assert_eq!(failed["isError"], true);
    assert_mcp_response_is_presentation_free(&failed);
    let failed_envelope = mcp_structured(&failed);
    assert_eq!(failed_envelope["status"], "empty");
    assert_eq!(failed_envelope["data"]["records"][0]["status"], "rejected");
    assert_eq!(failed_envelope["next_actions"][0]["tool"], "oneup_search");
}

#[test]
fn mcp_read_handles_return_structured_not_found_and_ambiguous_records() {
    let tmp = create_ambiguous_handle_fixture();
    let _guard = init_and_index_fts_only(&tmp);
    let mut client = McpTestClient::start(tmp.path());

    let search = client.call_tool(
        "oneup_search",
        serde_json::json!({ "query": "ambiguous_collision_token", "limit": 32 }),
    );
    assert_ne!(search["isError"], true);
    assert_mcp_response_is_presentation_free(&search);
    let handles = mcp_structured(&search)["data"]["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|result| result["handle"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert!(
        handles.len() >= 17,
        "fixture should produce enough hits to force a prefix collision: {handles:?}"
    );
    let ambiguous_prefix = ambiguous_handle_prefix(&handles);

    let result = client.call_tool(
        "oneup_read",
        serde_json::json!({ "handles": [ambiguous_prefix, ":does-not-exist"] }),
    );
    assert_eq!(result["isError"], true);
    assert_mcp_response_is_presentation_free(&result);
    let envelope = mcp_structured(&result);
    assert_eq!(envelope["status"], "empty");

    let records = envelope["data"]["records"].as_array().unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["status"], "ambiguous");
    assert_eq!(records[0]["source"]["kind"], "handle");
    assert!(
        records[0]["matching_handles"].as_array().unwrap().len() > 1,
        "ambiguous handle records should include disambiguation candidates: {records:?}"
    );
    assert_eq!(records[1]["status"], "not_found");
    assert_eq!(records[1]["source"]["normalized"], "does-not-exist");
    assert_eq!(envelope["next_actions"][0]["tool"], "oneup_search");
}

#[test]
fn mcp_impact_preserves_trust_buckets_and_followups() {
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);
    let mut client = McpTestClient::start(tmp.path());

    let expanded = client.call_tool(
        "oneup_impact",
        serde_json::json!({ "symbol": "warm_cache_key" }),
    );
    assert_mcp_text_matches_summary(&expanded);
    let expanded_envelope = mcp_structured(&expanded);
    assert_eq!(expanded_envelope["status"], "expanded");
    assert!(
        !expanded_envelope["data"]["results"]
            .as_array()
            .unwrap()
            .is_empty(),
        "file impact should include primary likely-impact results"
    );
    assert!(
        expanded_envelope["data"]["contextual_results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|record| record["file_path"] == "src/cache/test_support.rs"),
        "impact output should preserve contextual lower-confidence guidance"
    );
    assert_eq!(expanded_envelope["next_actions"][0]["tool"], "oneup_read");

    let empty = client.call_tool(
        "oneup_impact",
        serde_json::json!({ "file": "src/admin/config.rs" }),
    );
    let empty_envelope = mcp_structured(&empty);
    assert!(
        matches!(
            empty_envelope["status"].as_str().unwrap(),
            "empty" | "empty_scoped"
        ),
        "expected explicit empty impact status, got {empty_envelope:?}"
    );
    assert_mcp_next_actions_are_canonical(empty_envelope);
}

#[test]
fn mcp_impact_refusal_sets_is_error() {
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);
    let mut client = McpTestClient::start(tmp.path());

    let result = client.call_tool(
        "oneup_impact",
        serde_json::json!({ "segment_id": "does-not-exist" }),
    );

    assert_eq!(result["isError"], true);
    assert_eq!(result["structuredContent"]["status"], "refused");
    assert_eq!(result["structuredContent"]["data"]["status"], "refused");
    assert!(result["structuredContent"]["next_actions"]
        .as_array()
        .unwrap()
        .iter()
        .all(|action| action["tool"].as_str().unwrap().starts_with("oneup_")));
}

#[test]
fn mcp_read_all_failed_handles_sets_is_error() {
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);
    let mut client = McpTestClient::start(tmp.path());

    let result = client.call_tool(
        "oneup_read",
        serde_json::json!({ "handles": [":does-not-exist"] }),
    );

    assert_eq!(result["isError"], true);
    assert_mcp_response_is_presentation_free(&result);
    assert_eq!(result["structuredContent"]["status"], "empty");
    assert_eq!(
        result["structuredContent"]["data"]["records"][0]["status"],
        "not_found"
    );
    assert_eq!(
        result["structuredContent"]["next_actions"][0]["tool"],
        "oneup_search"
    );
}

#[test]
fn impact_rows_carry_channel_suffix() {
    // design §2.2, D5: every impact row ends with ` ~P` or ` ~C`.
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let (stdout, stderr, ok) = impact_output(tmp.path(), &["--from-file", "src/auth/runtime.rs"]);
    assert!(ok, "impact failed: {stderr}");

    // status_line should be absent on expanded envelopes; every non-empty
    // stdout line must end with the channel suffix.
    for line in stdout.lines().filter(|l| !l.is_empty()) {
        assert!(
            line.ends_with("  ~P") || line.ends_with("  ~C"),
            "every impact row must end with ~P or ~C, got {line:?}"
        );
    }

    let rows = parse_discovery_rows(&stdout);
    assert!(!rows.is_empty());
    // At least one primary; bootstrap is the known call site.
    assert!(rows.iter().any(|r| r.channel == Some('P')));
    assert!(
        rows.iter()
            .any(|r| r.channel == Some('P') && r.file_path == "src/auth/bootstrap.rs"),
        "expected bootstrap primary row in: {rows:?}"
    );
}

#[test]
fn impact_primary_precedes_contextual() {
    // All primary (~P) rows must appear before any contextual (~C) row so an
    // agent can split the stream by channel without re-sorting.
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let (stdout, _stderr, ok) = impact_output(tmp.path(), &["--from-symbol", "warm_cache_key"]);
    assert!(ok);

    let rows = parse_discovery_rows(&stdout);
    let first_contextual = rows.iter().position(|r| r.channel == Some('C'));
    if let Some(idx) = first_contextual {
        assert!(
            rows[..idx].iter().all(|r| r.channel == Some('P')),
            "primary rows must precede contextual rows"
        );
    }
}

#[test]
fn impact_file_anchor_surfaces_bootstrap_primary() {
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let rows = impact_rows(tmp.path(), &["--from-file", "src/auth/runtime.rs"]);
    assert!(rows
        .iter()
        .any(|r| r.channel == Some('P') && r.file_path == "src/auth/bootstrap.rs"));
}

#[test]
fn impact_file_line_anchor_resolves_requested_line() {
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let rows = impact_rows(tmp.path(), &["--from-file", "src/auth/runtime.rs:1"]);
    assert!(rows
        .iter()
        .any(|r| r.file_path == "src/auth/bootstrap.rs" && r.channel == Some('P')));
}

#[test]
fn impact_symbol_anchor_expands_with_resolved_seed() {
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let rows = impact_rows(tmp.path(), &["--from-symbol", "load_auth_config"]);
    assert!(rows
        .iter()
        .any(|r| r.file_path == "src/auth/bootstrap.rs" && r.channel == Some('P')));
}

#[test]
fn impact_symbol_anchor_scope_narrows_ambiguous_matches() {
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let rows = impact_rows(
        tmp.path(),
        &["--from-symbol", "load_config", "--scope", "src/auth"],
    );
    assert!(!rows.is_empty());
    // top primary comes from the scoped subtree
    let top_primary = rows
        .iter()
        .find(|r| r.channel == Some('P'))
        .expect("at least one primary row");
    assert_eq!(top_primary.file_path, "src/auth/reload.rs");
}

#[test]
fn impact_symbol_anchor_qualified_relation_promotes_matching_definition() {
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let rows = impact_rows(tmp.path(), &["--from-symbol", "reload_auth_config"]);
    assert!(
        rows.iter()
            .any(|r| r.channel == Some('P') && r.file_path == "src/auth/config.rs"),
        "config.rs should appear as primary: {rows:?}"
    );
}

#[test]
fn impact_symbol_anchor_interface_implementor_surfaces_primary() {
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let rows = impact_rows(tmp.path(), &["--from-symbol", "AuthStore"]);
    assert!(rows.iter().any(|r| r.channel == Some('P')
        && r.file_path == "src/auth/auth_store.ts"
        && r.kind == "class"
        && r.symbol == "SqlAuthStore"));
}

#[test]
fn impact_symbol_anchor_formatter_implementor_stays_primary_under_reference_pressure() {
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let rows = impact_rows(tmp.path(), &["--from-symbol", "Formatter"]);
    let primaries: Vec<_> = rows.iter().filter(|r| r.channel == Some('P')).collect();
    assert!(!primaries.is_empty());
    assert_eq!(primaries[0].file_path, "src/ui/plain_formatter.ts");

    // Same path should not also appear in the contextual bucket.
    let contextual_has_plain = rows
        .iter()
        .any(|r| r.channel == Some('C') && r.file_path == "src/ui/plain_formatter.ts");
    assert!(
        !contextual_has_plain,
        "primary implementor should not also be duplicated as contextual"
    );
}

#[test]
fn impact_symbol_anchor_ambiguous_helper_emits_context_only_hint() {
    // Lean renderer collapses `empty` envelopes to a status line plus a hint
    // line (design §3.6); no discovery rows follow. The hint's `context_only`
    // code signals that contextual guidance exists without embedding the rows
    // directly on the wire.
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let (stdout, _stderr, ok) = impact_output(tmp.path(), &["--from-symbol", "boot_global_config"]);
    assert!(ok);

    assert_eq!(
        stdout.lines().next().unwrap_or(""),
        "empty",
        "expected bare `empty` status, got: {stdout}"
    );
    let hint_line = stdout
        .lines()
        .find(|l| l.starts_with("hint"))
        .expect("empty envelope should carry a hint line");
    assert!(hint_line.contains("context_only"));
    // No discovery rows: every remaining line is either the status or hint.
    for line in stdout.lines().filter(|l| !l.is_empty()) {
        assert!(
            line.starts_with("empty") || line.starts_with("hint"),
            "unexpected discovery row in empty envelope: {line:?}"
        );
    }
}

#[test]
fn impact_symbol_anchor_prefers_stronger_primary_over_wrapper() {
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let rows = impact_rows(tmp.path(), &["--from-symbol", "warm_cache_key"]);
    let primaries: Vec<_> = rows.iter().filter(|r| r.channel == Some('P')).collect();
    assert!(!primaries.is_empty());
    assert_eq!(primaries[0].file_path, "src/cache/worker.rs");

    if let Some(wrapper_idx) = primaries
        .iter()
        .position(|r| r.file_path == "src/cache/priming.rs")
    {
        assert!(wrapper_idx > 0, "wrapper should never outrank worker");
    }
}

#[test]
fn impact_symbol_anchor_inline_test_context_stays_contextual() {
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let rows = impact_rows(tmp.path(), &["--from-symbol", "warm_cache_key"]);

    assert!(rows
        .iter()
        .filter(|r| r.channel == Some('P'))
        .all(|r| r.file_path != "src/cache/test_support.rs"));
    assert!(rows
        .iter()
        .any(|r| r.channel == Some('C') && r.file_path == "src/cache/test_support.rs"));
}

#[test]
fn impact_file_anchor_limit_caps_total_rows() {
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let (stdout, _stderr, ok) = impact_output(
        tmp.path(),
        &["--from-file", "src/auth/runtime.rs", "--limit", "1"],
    );
    assert!(ok);

    let rows = parse_discovery_rows(&stdout);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].channel, Some('P'));
}

#[test]
fn impact_file_anchor_scope_refuses_out_of_scope_seed() {
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let (stdout, _stderr, ok) = impact_output(
        tmp.path(),
        &["--from-file", "src/auth/runtime.rs", "--scope", "src/cache"],
    );
    assert!(ok);

    let first_line = stdout.lines().next().unwrap_or("");
    assert!(
        first_line.starts_with("refused"),
        "expected refused line, got {first_line:?}"
    );
    assert!(first_line.contains("anchor_out_of_scope"));
    // Any hint line should point at alignment guidance.
    assert!(stdout
        .lines()
        .any(|l| l.starts_with("hint") && l.contains("align_anchor_and_scope")));
}

#[test]
fn impact_symbol_anchor_refuses_broad_requests_with_hint() {
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let (stdout, _stderr, ok) = impact_output(tmp.path(), &["--from-symbol", "load_config"]);
    assert!(ok);

    assert!(stdout.lines().next().unwrap_or("").starts_with("refused"));
    assert!(stdout.contains("symbol_too_broad"));
    assert!(stdout.lines().any(|l| l.starts_with("hint")
        && l.contains("narrow_with_scope")
        && l.contains("--scope")));
}

#[test]
fn impact_file_line_anchor_returns_empty_with_hint() {
    // Lean renderer: `empty` envelopes emit the status label plus a hint line
    // (no `~C` rows on the wire — see design §3.6).
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let (stdout, _stderr, ok) =
        impact_output(tmp.path(), &["--from-file", "src/auth/runtime.rs:5"]);
    assert!(ok);

    assert_eq!(
        impact_status_line(&stdout).unwrap_or(""),
        "empty",
        "expected bare `empty` status, got {stdout:?}"
    );
    let hint = stdout
        .lines()
        .find(|l| l.starts_with("hint"))
        .expect("empty envelope should carry a hint line");
    assert!(hint.contains("context_only"));
}

#[test]
fn impact_scoped_file_line_anchor_returns_empty_scoped_with_hint() {
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let args = &[
        "--from-file",
        "src/auth/runtime.rs:5",
        "--scope",
        "src/auth",
    ];
    let (stdout, _stderr, ok) = impact_output(tmp.path(), args);
    assert!(ok);

    assert_eq!(
        stdout.lines().next().unwrap_or(""),
        "empty_scoped",
        "expected bare `empty_scoped` status line, got: {stdout}"
    );
    let hint = stdout
        .lines()
        .find(|l| l.starts_with("hint"))
        .expect("empty_scoped envelope should carry a hint line");
    assert!(hint.contains("context_only"));
    // Scope echoed on the hint line via `scope=<s>` per design §3.6.
    assert!(
        hint.contains("scope=src/auth"),
        "hint should echo the requested scope, got: {hint}"
    );
    // No discovery rows in empty_scoped envelopes.
    for line in stdout.lines().filter(|l| !l.is_empty()) {
        assert!(
            line.starts_with("empty") || line.starts_with("hint"),
            "unexpected discovery row in empty_scoped envelope: {line:?}"
        );
    }
}

// =============================================================================
// Search -> get round-trip
// =============================================================================

fn parse_get_record_header(line: &str) -> Option<&str> {
    line.strip_prefix("segment ")
}

fn parse_get_records(stdout: &str) -> Vec<(String, Option<String>)> {
    // Parse lean `get` output: each record is `segment <id>\n<tab-metadata>\n\n<body>\n\n---\n`
    // or `not_found\t<raw>\n---\n`. Returns (id_or_raw, Some(content_string)) for
    // resolved records and (raw, None) for not_found.
    let lines: Vec<&str> = stdout.lines().collect();
    let mut records = Vec::new();
    let mut idx = 0;
    while idx < lines.len() {
        let line = lines[idx];
        if let Some(rest) = line.strip_prefix("not_found\t") {
            // Skip this line and the following `---` sentinel if present.
            idx += 1;
            if idx < lines.len() && lines[idx] == "---" {
                idx += 1;
            }
            records.push((rest.to_string(), None));
        } else if let Some(id) = parse_get_record_header(line) {
            // Advance past the header line.
            idx += 1;
            // Consume the tab-delimited metadata line.
            if idx < lines.len() {
                idx += 1;
            }
            // Consume the blank line separating metadata from body.
            if idx < lines.len() && lines[idx].is_empty() {
                idx += 1;
            }
            // Accumulate body lines until the `---` sentinel is reached; the
            // last blank line before `---` is considered the record terminator.
            let mut body = String::new();
            while idx < lines.len() && lines[idx] != "---" {
                let body_line = lines[idx];
                if body_line.is_empty() && idx + 1 < lines.len() && lines[idx + 1] == "---" {
                    idx += 1;
                    break;
                }
                if !body.is_empty() {
                    body.push('\n');
                }
                body.push_str(body_line);
                idx += 1;
            }
            // Consume the `---` sentinel if still pointing at it.
            if idx < lines.len() && lines[idx] == "---" {
                idx += 1;
            }
            records.push((id.to_string(), Some(body)));
        } else {
            idx += 1;
        }
    }
    records
}

#[test]
fn get_returns_body_for_known_handle() {
    let tmp = create_multi_lang_fixture();
    init_and_index(&tmp);

    let rows = search_rows(tmp.path(), "Config host port");
    assert!(!rows.is_empty());
    let handle = rows[0].segment_id.clone();

    let (stdout, stderr, ok) =
        run_core_cmd(&["get", &handle, "--path", tmp.path().to_str().unwrap()]);
    assert!(ok, "get failed: {stderr}");

    let records = parse_get_records(&stdout);
    assert_eq!(records.len(), 1);
    let (returned_id, body) = &records[0];
    assert!(
        returned_id.starts_with(&handle[..handle.len().min(returned_id.len())])
            || handle.starts_with(returned_id),
        "get header `segment {returned_id}` should correspond to queried handle {handle}"
    );
    assert!(body.as_ref().is_some_and(|b| !b.is_empty()));
}

#[test]
fn get_tolerates_leading_colon_handle() {
    // The lean row grammar emits `:<id>` as the trailing token; agents should
    // be able to paste that directly into `1up get`.
    let tmp = create_multi_lang_fixture();
    init_and_index(&tmp);

    let rows = search_rows(tmp.path(), "Config");
    let handle_with_colon = format!(":{}", rows[0].segment_id);

    let (stdout, stderr, ok) = run_core_cmd(&[
        "get",
        &handle_with_colon,
        "--path",
        tmp.path().to_str().unwrap(),
    ]);
    assert!(ok, "get failed: {stderr}");
    let records = parse_get_records(&stdout);
    assert_eq!(records.len(), 1);
    assert!(
        records[0].1.is_some(),
        "leading-colon handle should resolve"
    );
}

#[test]
fn get_reports_not_found_for_unknown_handle() {
    let tmp = create_multi_lang_fixture();
    init_and_index(&tmp);

    let (stdout, _stderr, ok) = run_core_cmd(&[
        "get",
        "ffffffffffff",
        "--path",
        tmp.path().to_str().unwrap(),
    ]);
    // `get` does not fail on an unresolved handle; it emits `not_found\t<raw>`.
    assert!(ok);
    let records = parse_get_records(&stdout);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].0, "ffffffffffff");
    assert!(records[0].1.is_none());
}

#[test]
fn get_preserves_order_across_handles() {
    let tmp = create_multi_lang_fixture();
    init_and_index(&tmp);

    let rows = search_rows(tmp.path(), "Config");
    assert!(rows.len() >= 2, "need at least two hits for ordering test");
    let first = rows[0].segment_id.clone();
    let second = rows[1].segment_id.clone();

    let (stdout, _stderr, ok) = run_core_cmd(&[
        "get",
        &first,
        "ffffffffffff",
        &second,
        "--path",
        tmp.path().to_str().unwrap(),
    ]);
    assert!(ok);

    let records = parse_get_records(&stdout);
    assert_eq!(records.len(), 3);
    assert!(records[0].1.is_some());
    assert_eq!(records[1].0, "ffffffffffff");
    assert!(records[1].1.is_none());
    assert!(records[2].1.is_some());
}

// =============================================================================
// Search handle handoff: search -> impact --from-segment preserves ranking
// =============================================================================

#[test]
fn search_segment_id_round_trips_into_impact_from_segment() {
    // The lean row grammar emits a 12-char display handle (`:<prefix>`). `get`
    // resolves that prefix back to the full segment id, which is what
    // `impact --from-segment` expects for its exact-anchor lookup. This pins
    // the discovery -> hydrate -> impact follow-up chain at the row-grammar
    // layer.
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let rows = search_rows(tmp.path(), "load auth config");
    let seed = rows
        .iter()
        .find(|r| r.file_path == "src/auth/runtime.rs")
        .expect("search should return the runtime definition segment");
    let handle_prefix = seed.segment_id.clone();

    let (get_stdout, _stderr, ok) = run_core_cmd(&[
        "get",
        &handle_prefix,
        "--path",
        tmp.path().to_str().unwrap(),
    ]);
    assert!(ok);
    let records = parse_get_records(&get_stdout);
    let full_segment_id = records
        .iter()
        .find_map(|(id, body)| body.as_ref().map(|_| id.clone()))
        .expect("get should resolve the prefix to a full segment id");
    assert!(full_segment_id.starts_with(&handle_prefix));

    let impact = impact_rows(tmp.path(), &["--from-segment", &full_segment_id]);
    assert!(!impact.is_empty());
    assert!(impact
        .iter()
        .any(|r| r.channel == Some('P') && r.file_path == "src/auth/bootstrap.rs"));
    // Seeds never appear in their own primary results.
    assert!(impact
        .iter()
        .all(|r| !full_segment_id.starts_with(&r.segment_id)));
}

#[test]
fn search_segment_id_handoff_keeps_search_top_hits_stable() {
    // The hand-off from `search` to `impact --from-segment` must not perturb
    // subsequent search ranking.
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let before = search_rows(tmp.path(), "load auth config");
    assert!(!before.is_empty());

    let segment_id = before[0].segment_id.clone();
    let _ = impact_rows(tmp.path(), &["--from-segment", &segment_id]);
    let after = search_rows(tmp.path(), "load auth config");

    let before_ranked: Vec<_> = before
        .iter()
        .take(5)
        .map(|r| (r.file_path.clone(), r.line_start, r.kind.clone()))
        .collect();
    let after_ranked: Vec<_> = after
        .iter()
        .take(5)
        .map(|r| (r.file_path.clone(), r.line_start, r.kind.clone()))
        .collect();
    assert_eq!(before_ranked, after_ranked);
}

// =============================================================================
// Context lean shape
// =============================================================================

#[test]
fn context_retrieval_returns_enclosing_scope() {
    let tmp = create_multi_lang_fixture();
    init_and_index(&tmp);

    let (stdout, _stderr, ok) = run_core_cmd(&[
        "context",
        "main.rs:4",
        "--path",
        tmp.path().to_str().unwrap(),
    ]);
    assert!(ok);

    // Header line: `<path>:<l1>-<l2>  context  <scope_type>`.
    let header = stdout.lines().next().unwrap_or("");
    let parts: Vec<&str> = header.split("  ").collect();
    assert_eq!(parts.len(), 3, "context header shape: {header:?}");
    assert!(
        parts[0].ends_with("main.rs:3-5") || parts[0].contains("main.rs:"),
        "context path/lines token shape: {:?}",
        parts[0]
    );
    assert_eq!(parts[1], "context");
    assert_eq!(parts[2], "function");

    // The enclosing body should quote `fn greet`.
    assert!(stdout.contains("fn greet"));
}

#[test]
fn context_retrieval_python_scope() {
    let tmp = create_multi_lang_fixture();
    init_and_index(&tmp);

    let (stdout, _stderr, ok) = run_core_cmd(&[
        "context",
        "utils.py:6",
        "--path",
        tmp.path().to_str().unwrap(),
    ]);
    assert!(ok);
    let header = stdout.lines().next().unwrap_or("");
    assert!(header.contains("  context  function"));
    assert!(stdout.contains("parse_config"));
}

#[test]
fn context_rejects_outside_root_by_default() {
    let tmp = TempDir::new().unwrap();
    let project_root = tmp.path().join("project");
    let outside_file = tmp.path().join("outside.rs");
    fs::create_dir_all(&project_root).unwrap();
    fs::write(
        &outside_file,
        "fn leaked() {\n    println!(\"outside\");\n}\n",
    )
    .unwrap();
    let location = format!("{}:1", outside_file.display());

    cmd()
        .args([
            "context",
            &location,
            "--path",
            project_root.to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("--allow-outside-root"));
}

#[test]
fn context_rejects_absolute_in_root_path_by_default() {
    let tmp = TempDir::new().unwrap();
    let project_root = tmp.path().join("project");
    let in_root_file = project_root.join("in_root.rs");
    fs::create_dir_all(&project_root).unwrap();
    fs::write(
        &in_root_file,
        "fn internal() {\n    println!(\"inside\");\n}\n",
    )
    .unwrap();
    let location = format!("{}:1", in_root_file.display());

    cmd()
        .args([
            "context",
            &location,
            "--path",
            project_root.to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "absolute context paths are disabled by default",
        ))
        .stderr(predicate::str::contains("--allow-outside-root"));
}

#[test]
fn context_allows_absolute_in_root_path_with_explicit_override() {
    let tmp = TempDir::new().unwrap();
    let project_root = tmp.path().join("project");
    let in_root_file = project_root.join("in_root.rs");
    fs::create_dir_all(&project_root).unwrap();
    fs::write(
        &in_root_file,
        "fn internal() {\n    println!(\"inside\");\n}\n",
    )
    .unwrap();
    let location = format!("{}:1", in_root_file.display());

    let (stdout, _stderr, ok) = run_core_cmd(&[
        "context",
        &location,
        "--path",
        project_root.to_str().unwrap(),
        "--allow-outside-root",
    ]);
    assert!(ok);
    let header = stdout.lines().next().unwrap_or("");
    assert!(header.contains("  context  function"));
    assert!(stdout.contains("fn internal"));
}

#[test]
fn context_allows_outside_root_with_explicit_override() {
    let tmp = TempDir::new().unwrap();
    let project_root = tmp.path().join("project");
    let outside_file = tmp.path().join("outside.rs");
    fs::create_dir_all(&project_root).unwrap();
    fs::write(
        &outside_file,
        "fn leaked() {\n    println!(\"outside\");\n}\n",
    )
    .unwrap();
    let location = format!("{}:1", outside_file.display());

    let (stdout, _stderr, ok) = run_core_cmd(&[
        "context",
        &location,
        "--path",
        project_root.to_str().unwrap(),
        "--allow-outside-root",
    ]);
    assert!(ok);
    let header = stdout.lines().next().unwrap_or("");
    assert!(header.contains("  context  function"));
    assert!(stdout.contains("fn leaked"));
}

// =============================================================================
// Incremental indexing
// =============================================================================

#[test]
fn incremental_indexing_detects_changes() {
    let tmp = create_multi_lang_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let before = symbol_rows(tmp.path(), "greet", &[]);
    assert!(!before.is_empty());

    fs::write(
        tmp.path().join("main.rs"),
        r#"fn welcome(name: &str) -> String {
    format!("Welcome, {}", name)
}

fn main() {
    println!("{}", welcome("world"));
}
"#,
    )
    .unwrap();

    cmd()
        .args(["index", tmp.path().to_str().unwrap(), "--format", "json"])
        .assert()
        .success();

    let after_greet = symbol_rows(tmp.path(), "greet", &[]);
    assert!(
        after_greet.is_empty(),
        "greet should no longer exist after re-index"
    );

    let after_welcome = symbol_rows(tmp.path(), "welcome", &[]);
    assert!(
        !after_welcome.is_empty(),
        "welcome should exist after re-index"
    );
    assert!(after_welcome
        .iter()
        .any(|r| r.symbol == "welcome" || r.breadcrumb.contains("welcome")));
}

#[test]
fn default_parallel_index_matches_jobs_one_for_incremental_cleanup() {
    let _guard = HideModelGuard::new();
    let default_repo = TempDir::new().unwrap();
    let serial_repo = TempDir::new().unwrap();

    write_parallel_regression_fixture(default_repo.path());
    write_parallel_regression_fixture(serial_repo.path());

    init_project(default_repo.path());
    init_project(serial_repo.path());

    let initial_default = run_index_json(default_repo.path(), &[]);
    let initial_serial = run_index_json(serial_repo.path(), &["--jobs", "1"]);
    assert!(
        initial_default["progress"]["files_indexed"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(
        initial_default["progress"]["segments_stored"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(
        initial_serial["progress"]["files_indexed"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(
        initial_serial["progress"]["segments_stored"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert_eq!(
        initial_default["progress"]["files_indexed"],
        initial_serial["progress"]["files_indexed"]
    );

    mutate_parallel_regression_fixture(default_repo.path());
    mutate_parallel_regression_fixture(serial_repo.path());

    let rerun_default = run_index_json(default_repo.path(), &[]);
    let rerun_serial = run_index_json(serial_repo.path(), &["--jobs", "1"]);
    assert!(rerun_default["progress"]["files_indexed"].as_u64().unwrap() > 0);
    assert!(
        rerun_default["progress"]["segments_stored"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(rerun_serial["progress"]["files_indexed"].as_u64().unwrap() > 0);
    assert!(
        rerun_serial["progress"]["segments_stored"]
            .as_u64()
            .unwrap()
            > 0
    );

    for field in ["files_indexed", "files_skipped", "files_deleted"] {
        assert_eq!(
            rerun_default["progress"][field], rerun_serial["progress"][field],
            "mismatched {field} after incremental re-index"
        );
    }

    assert_eq!(rerun_default["progress"]["files_indexed"], 2);
    assert_eq!(rerun_default["progress"]["files_skipped"], 1);
    assert_eq!(rerun_default["progress"]["files_deleted"], 1);

    assert!(symbol_rows(default_repo.path(), "removed_symbol", &[]).is_empty());
    assert!(symbol_rows(serial_repo.path(), "removed_symbol", &[]).is_empty());
    assert_eq!(
        symbol_rows(default_repo.path(), "beta_symbol", &[]).len(),
        1
    );
    assert_eq!(symbol_rows(serial_repo.path(), "beta_symbol", &[]).len(), 1);
    assert_eq!(
        symbol_rows(default_repo.path(), "fresh_symbol", &[]).len(),
        1
    );
    assert_eq!(
        symbol_rows(serial_repo.path(), "fresh_symbol", &[]).len(),
        1
    );
    assert_eq!(
        symbol_rows(default_repo.path(), "stable_symbol", &[]).len(),
        1
    );
    assert_eq!(
        symbol_rows(serial_repo.path(), "stable_symbol", &[]).len(),
        1
    );
}

// =============================================================================
// Daemon lifecycle + PID
// =============================================================================

#[cfg(unix)]
#[test]
fn daemon_pid_file_lifecycle() {
    let tmp = TempDir::new().unwrap();
    let pid_path = tmp.path().join("test_daemon.pid");

    assert!(!pid_path.exists());

    let pid = std::process::id();
    fs::write(&pid_path, pid.to_string()).unwrap();
    assert!(pid_path.exists());

    let read_pid: u32 = fs::read_to_string(&pid_path)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(read_pid, pid);

    fs::remove_file(&pid_path).unwrap();
    assert!(!pid_path.exists());
}

#[cfg(unix)]
#[test]
fn daemon_stale_pid_detection() {
    let tmp = TempDir::new().unwrap();
    let pid_path = tmp.path().join("stale_daemon.pid");

    fs::write(&pid_path, "99999").unwrap();
    assert!(pid_path.exists());

    let content = fs::read_to_string(&pid_path).unwrap();
    let stale_pid: u32 = content.trim().parse().unwrap();

    let is_alive = unsafe {
        libc::kill(stale_pid as i32, 0) == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    };
    assert!(
        !is_alive,
        "PID 99999 should not be a live process in test environment"
    );

    fs::remove_file(&pid_path).unwrap();
    assert!(!pid_path.exists(), "stale PID file should be cleaned up");
}

// =============================================================================
// add-mcp wrapper delegation
// =============================================================================

#[cfg(unix)]
#[test]
fn add_mcp_prefers_bunx_then_npx() {
    let tmp = TempDir::new().unwrap();
    let fake_bin = tmp.path().join("bin");
    let repo = tmp.path().join("repo");
    let log_path = tmp.path().join("runner.log");
    fs::create_dir_all(&fake_bin).unwrap();
    fs::create_dir_all(&repo).unwrap();
    write_fake_runner(&fake_bin.join("bunx"));
    write_fake_runner(&fake_bin.join("npx"));

    cmd()
        .env("PATH", &fake_bin)
        .env("ONEUP_FAKE_RUNNER_LOG", &log_path)
        .args([
            "add-mcp",
            "--path",
            repo.to_str().unwrap(),
            "--agent",
            "codex",
            "--yes",
        ])
        .assert()
        .success();

    let log = fs::read_to_string(log_path).unwrap();
    assert!(log.contains("runner=bunx"), "unexpected runner log: {log}");
    assert!(log.contains("arg[0]=add-mcp"));
}

#[cfg(unix)]
#[test]
fn add_mcp_builds_oneup_server_command() {
    let tmp = TempDir::new().unwrap();
    let fake_bin = tmp.path().join("bin");
    let repo = tmp.path().join("repo with spaces");
    let log_path = tmp.path().join("runner.log");
    fs::create_dir_all(&fake_bin).unwrap();
    fs::create_dir_all(&repo).unwrap();
    write_fake_runner(&fake_bin.join("npx"));

    cmd()
        .env("PATH", &fake_bin)
        .env("ONEUP_FAKE_RUNNER_LOG", &log_path)
        .args([
            "add-mcp",
            "--path",
            repo.to_str().unwrap(),
            "--runner",
            "npx",
            "--agent",
            "codex",
            "--agent",
            "cursor",
            "--global",
            "--yes",
        ])
        .assert()
        .success();

    let canonical_repo = repo.canonicalize().unwrap();
    let expected_source = format!("arg[1]=1up mcp --path '{}'", canonical_repo.display());
    let log = fs::read_to_string(log_path).unwrap();
    assert!(log.contains("runner=npx"), "unexpected runner log: {log}");
    assert!(log.contains("arg[0]=add-mcp"), "unexpected argv: {log}");
    assert!(log.contains(&expected_source), "unexpected argv: {log}");
    assert!(log.contains("arg[2]=--name"), "unexpected argv: {log}");
    assert!(log.contains("arg[3]=oneup"), "unexpected argv: {log}");
    assert!(log.contains("arg[4]=--agent"), "unexpected argv: {log}");
    assert!(log.contains("arg[5]=codex"), "unexpected argv: {log}");
    assert!(log.contains("arg[6]=--agent"), "unexpected argv: {log}");
    assert!(log.contains("arg[7]=cursor"), "unexpected argv: {log}");
    assert!(log.contains("arg[8]=--global"), "unexpected argv: {log}");
    assert!(log.contains("arg[9]=--yes"), "unexpected argv: {log}");
}

#[cfg(unix)]
#[test]
fn add_mcp_runner_override_requires_available_runner() {
    let tmp = TempDir::new().unwrap();
    let fake_bin = tmp.path().join("bin");
    let repo = tmp.path().join("repo");
    fs::create_dir_all(&fake_bin).unwrap();
    fs::create_dir_all(&repo).unwrap();
    write_fake_runner(&fake_bin.join("bunx"));

    cmd()
        .env("PATH", &fake_bin)
        .args([
            "add-mcp",
            "--path",
            repo.to_str().unwrap(),
            "--runner",
            "npx",
        ])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("Manual MCP setup fallback")
                .and(predicate::str::contains(
                    "selected runner `npx` was not found on PATH",
                ))
                .and(predicate::str::contains("call `oneup_prepare`")),
        );
}

#[cfg(unix)]
#[test]
fn add_mcp_fallback_has_manual_snippets() {
    let tmp = TempDir::new().unwrap();
    let fake_bin = tmp.path().join("bin");
    let repo = tmp.path().join("repo");
    let log_path = tmp.path().join("runner.log");
    fs::create_dir_all(&fake_bin).unwrap();
    fs::create_dir_all(&repo).unwrap();
    write_fake_runner(&fake_bin.join("npx"));

    cmd()
        .env("PATH", &fake_bin)
        .env("ONEUP_FAKE_RUNNER_LOG", &log_path)
        .env("ONEUP_FAKE_RUNNER_STATUS", "17")
        .args([
            "add-mcp",
            "--path",
            repo.to_str().unwrap(),
            "--runner",
            "npx",
        ])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("Manual MCP setup fallback")
                .and(predicate::str::contains("Generic MCP JSON"))
                .and(predicate::str::contains("Codex TOML"))
                .and(predicate::str::contains("server identity `oneup`"))
                .and(predicate::str::contains("call `oneup_prepare`"))
                .and(predicate::str::contains("add-mcp exited with exit code 17")),
        );
}

#[test]
fn add_mcp_does_not_add_config_writer_artifacts() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for forbidden_path in [
        "src/cli/mcp_install.rs",
        "src/mcp/install",
        "src/mcp/codex.rs",
        "src/mcp/claude_code.rs",
        "src/mcp/cursor.rs",
        "src/mcp/vscode.rs",
    ] {
        assert!(
            !root.join(forbidden_path).exists(),
            "unexpected native MCP installer or host adapter artifact: {forbidden_path}"
        );
    }

    let wrapper = fs::read_to_string(root.join("src/cli/add_mcp.rs")).unwrap();
    let production_wrapper = wrapper.split("#[cfg(test)]").next().unwrap();
    for forbidden_snippet in [
        "mcp_install",
        "mcp-install",
        "std::fs::write",
        "fs::write",
        "File::create",
        "serde_json::to_writer",
        "toml::to_string",
    ] {
        assert!(
            !production_wrapper.contains(forbidden_snippet),
            "add-mcp wrapper should not include native config mutation snippet {forbidden_snippet:?}"
        );
    }
}

// =============================================================================
// End-to-end workflow + maintenance command JSON surface
// =============================================================================

#[test]
fn cli_init_then_index_then_search_workflow() {
    let tmp = create_multi_lang_fixture();
    let _guard = HideModelGuard::new();

    cmd()
        .args(["init", tmp.path().to_str().unwrap(), "--format", "json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Initialized"));

    let id_path = tmp.path().join(".1up").join("project_id");
    assert!(id_path.exists());

    cmd()
        .args(["index", tmp.path().to_str().unwrap(), "--format", "json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Indexed"));

    // Search now renders lean rows; just assert it succeeds.
    cmd()
        .args(["search", "logger", "--path", tmp.path().to_str().unwrap()])
        .assert()
        .success();
}

#[test]
fn index_json_output_includes_progress_summary() {
    let tmp = create_multi_lang_fixture();
    let _guard = HideModelGuard::new();

    cmd()
        .args(["init", tmp.path().to_str().unwrap(), "--format", "json"])
        .assert()
        .success();

    let output = cmd()
        .args(["index", tmp.path().to_str().unwrap(), "--format", "json"])
        .output()
        .unwrap();

    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    let payload: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    let progress = &payload["progress"];

    assert!(payload["message"].as_str().unwrap().contains("Indexed"));
    assert_eq!(progress["state"], "complete");
    assert_eq!(progress["phase"], "complete");
    assert!(progress["files_scanned"].as_u64().unwrap() > 0);
    assert!(progress["segments_stored"].as_u64().unwrap() > 0);
    assert_eq!(progress["embeddings_enabled"], false);
    assert!(progress["updated_at"].as_str().is_some());
}

#[test]
fn status_json_reports_noop_index_progress() {
    let tmp = create_multi_lang_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let second_index = cmd()
        .args(["index", tmp.path().to_str().unwrap(), "--format", "json"])
        .output()
        .unwrap();
    assert!(second_index.status.success());

    let output = cmd()
        .args(["status", tmp.path().to_str().unwrap(), "--format", "json"])
        .output()
        .unwrap();

    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    let payload: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    let progress = &payload["index_progress"];

    assert_eq!(progress["state"], "complete");
    assert_eq!(progress["phase"], "complete");
    assert_eq!(progress["files_indexed"], 0);
    assert_eq!(progress["segments_stored"], 0);
    assert!(progress["files_skipped"].as_u64().unwrap() > 0);
    assert_eq!(progress["files_total"], progress["files_scanned"]);
    assert_eq!(progress["embeddings_enabled"], false);
    assert!(payload["indexed_files"].as_u64().unwrap() > 0);
}

// =============================================================================
// Daemon IPC: lean SearchResult round-trip
// =============================================================================

#[cfg(unix)]
#[test]
fn daemon_response_carries_lean_results() {
    // The CLI should deserialize the lean `SearchResult` shape sent back by the
    // daemon (framed JSON over Unix socket) and re-render it through the lean
    // row grammar on stdout.
    let home = tempfile::Builder::new()
        .prefix("1up-home-")
        .tempdir_in("/tmp")
        .unwrap();
    let project = TempDir::new().unwrap();
    let socket_path = test_data_dir(home.path()).join("daemon.sock");
    let expected_root = project.path().canonicalize().unwrap();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();

    let server_socket_path = socket_path.clone();
    let server_expected_root = expected_root.clone();
    let server = std::thread::spawn(move || {
        use std::os::unix::net::UnixListener;

        if let Some(parent) = server_socket_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let _ = fs::remove_file(&server_socket_path);

        let listener = UnixListener::bind(&server_socket_path).unwrap();
        ready_tx.send(()).unwrap();
        let (mut stream, _) = listener.accept().unwrap();

        let payload = read_framed_json(&mut stream);
        assert_eq!(
            payload["project_root"].as_str().unwrap(),
            server_expected_root.to_str().unwrap()
        );
        assert_eq!(payload["query"], "test");
        assert_eq!(payload["limit"], 3);

        // Lean SearchResult: segment_id required, score u32 integer, no
        // complexity/role/referenced_symbols/called_symbols fields.
        let response = serde_json::json!({
            "status": "results",
            "results": [
                {
                    "segment_id": "daemonseg000",
                    "file_path": "src/daemon.rs",
                    "language": "rust",
                    "block_type": "function",
                    "content": "fn daemon_search() {}",
                    "score": 87,
                    "line_number": 3,
                    "line_end": 5
                }
            ]
        });
        write_framed_json(&mut stream, &response);
    });

    ready_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .unwrap();

    let output = cmd()
        .env("HOME", home.path())
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_CONFIG_HOME")
        .args(["search", "test", "--path", project.path().to_str().unwrap()])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let rows = parse_discovery_rows(&stdout);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].score, 87);
    assert_eq!(rows[0].file_path, "src/daemon.rs");
    assert_eq!(rows[0].line_start, 3);
    assert_eq!(rows[0].line_end, 5);
    assert_eq!(rows[0].segment_id, "daemonseg000");

    server.join().unwrap();
}

// =============================================================================
// Flag rejection on core commands
// =============================================================================

#[test]
fn search_rejects_format_flag() {
    // core commands reject all presentation flags at clap parse time.
    for flag_pair in [["-f", "human"], ["--format", "json"], ["-f", "plain"]] {
        cmd()
            .args(["search", "needle", flag_pair[0], flag_pair[1]])
            .assert()
            .failure()
            .stderr(predicate::str::contains("unexpected argument"));
    }
}

#[test]
fn core_commands_reject_legacy_flags() {
    // no core command should quietly accept `--full`, `--human`, or
    // `--verbose-fields` either.
    for bad_flag in ["--full", "--human", "--verbose-fields"] {
        cmd()
            .args(["search", "needle", bad_flag])
            .assert()
            .failure();
    }
}

#[test]
fn cli_search_without_index_requires_reindex() {
    let tmp = TempDir::new().unwrap();

    cmd()
        .args(["search", "test", "--path", tmp.path().to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("1up reindex"));
}

#[test]
fn cli_symbol_without_index_requires_reindex() {
    let tmp = TempDir::new().unwrap();

    cmd()
        .args(["symbol", "test", "--path", tmp.path().to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("1up reindex"));
}

#[test]
fn cli_context_nonexistent_file_fails() {
    let tmp = TempDir::new().unwrap();

    cmd()
        .args([
            "context",
            "nonexistent.rs:1",
            "--path",
            tmp.path().to_str().unwrap(),
        ])
        .assert()
        .failure();
}

#[test]
fn cli_search_empty_results_emits_nothing() {
    let tmp = create_multi_lang_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let (stdout, _stderr, ok) = run_core_cmd(&[
        "search",
        "zznonexistentqueryzz",
        "--path",
        tmp.path().to_str().unwrap(),
    ]);
    assert!(ok);
    assert!(
        stdout.lines().filter(|l| !l.is_empty()).count() == 0,
        "empty search should emit zero rows, got: {stdout:?}"
    );
}

#[test]
fn cli_symbol_empty_results_emits_nothing_on_stdout() {
    let tmp = create_multi_lang_fixture();
    init_and_index(&tmp);

    let (stdout, _stderr, ok) = run_core_cmd(&[
        "symbol",
        "zznonexistentsymbolzz",
        "--path",
        tmp.path().to_str().unwrap(),
    ]);
    assert!(ok);
    assert!(
        stdout.lines().filter(|l| !l.is_empty()).count() == 0,
        "empty symbol lookup should emit zero rows, got: {stdout:?}"
    );
}

#[test]
fn cli_worktree_resolves_to_main_repo_index() {
    let _guard = HideModelGuard::new();

    let tmp = TempDir::new().unwrap();
    let tmp_root = tmp.path().canonicalize().unwrap();
    let main_repo = tmp_root.join("main");
    fs::create_dir_all(&main_repo).unwrap();

    std::process::Command::new("git")
        .args(["init", main_repo.to_str().unwrap()])
        .output()
        .expect("git init failed");

    fs::write(
        main_repo.join("hello.rs"),
        "fn greet() -> &'static str { \"hello\" }\n",
    )
    .unwrap();

    std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(&main_repo)
        .output()
        .expect("git add failed");

    std::process::Command::new("git")
        .args(["commit", "-m", "initial"])
        .current_dir(&main_repo)
        .output()
        .expect("git commit failed");

    cmd()
        .args(["init", main_repo.to_str().unwrap(), "--format", "json"])
        .assert()
        .success();

    cmd()
        .args(["index", main_repo.to_str().unwrap(), "--format", "json"])
        .assert()
        .success();

    let worktree_path = tmp_root.join("wt-feature");
    std::process::Command::new("git")
        .args([
            "worktree",
            "add",
            worktree_path.to_str().unwrap(),
            "-b",
            "feature-branch",
        ])
        .current_dir(&main_repo)
        .output()
        .expect("git worktree add failed");

    assert!(worktree_path.join(".git").is_file());

    let status_output = cmd()
        .args([
            "status",
            worktree_path.to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();

    assert!(
        status_output.status.success(),
        "status from worktree failed: {}",
        String::from_utf8_lossy(&status_output.stderr)
    );

    let status_json: serde_json::Value = serde_json::from_slice(&status_output.stdout).unwrap();
    assert_eq!(status_json["project_initialized"], true);

    // Core command from a worktree renders lean rows and should succeed against
    // the main repo's index.
    cmd()
        .args(["search", "greet", "--path", worktree_path.to_str().unwrap()])
        .assert()
        .success();

    // Write a worktree-only file and re-index from the worktree; the indexer
    // scans the worktree's files, not the main repo's.
    fs::write(
        worktree_path.join("worktree_only.rs"),
        "fn worktree_exclusive() -> bool { true }\n",
    )
    .unwrap();

    cmd()
        .args([
            "reindex",
            worktree_path.to_str().unwrap(),
            "--format",
            "json",
        ])
        .assert()
        .success();

    let rows = search_rows(&worktree_path, "worktree_exclusive");
    assert!(
        !rows.is_empty(),
        "worktree-only symbol should appear after reindex from worktree"
    );
}

#[test]
fn branch_context_search_excludes_other_worktree_only_content() {
    let home = TempDir::new().unwrap();
    let canonical_home = home.path().canonicalize().unwrap();
    seed_model_download_failure(&canonical_home);
    let (_tmp, main_repo, feature_worktree) = create_branch_filtering_fixture();

    cmd_with_home(&canonical_home)
        .args(["init", main_repo.to_str().unwrap(), "--format", "json"])
        .assert()
        .success();
    cmd_with_home(&canonical_home)
        .args(["index", main_repo.to_str().unwrap(), "--format", "json"])
        .assert()
        .success();
    cmd_with_home(&canonical_home)
        .args([
            "index",
            feature_worktree.to_str().unwrap(),
            "--format",
            "json",
        ])
        .assert()
        .success();

    let main_rows = search_rows_with_home(
        &canonical_home,
        &main_repo,
        "main branch only acceptance sentinel",
    );
    assert!(
        main_rows
            .iter()
            .any(|row| row.file_path.ends_with("main_only.rs")),
        "main branch should find main-only content: {main_rows:?}"
    );
    assert!(
        !search_rows_with_home(
            &canonical_home,
            &main_repo,
            "feature branch only acceptance sentinel",
        )
        .iter()
        .any(|row| row.file_path.ends_with("feature_only.rs")),
        "main branch search must not return feature-only file rows"
    );

    let feature_rows = search_rows_with_home(
        &canonical_home,
        &feature_worktree,
        "feature branch only acceptance sentinel",
    );
    assert!(
        feature_rows
            .iter()
            .any(|row| row.file_path.ends_with("feature_only.rs")),
        "feature branch should find feature-only content: {feature_rows:?}"
    );
    assert!(
        !search_rows_with_home(
            &canonical_home,
            &feature_worktree,
            "main branch only acceptance sentinel",
        )
        .iter()
        .any(|row| row.file_path.ends_with("main_only.rs")),
        "feature branch search must not return main-only file rows"
    );

    assert!(
        search_rows_with_home(
            &canonical_home,
            &main_repo,
            "shared branch acceptance sentinel"
        )
        .iter()
        .any(|row| row.file_path.ends_with("shared.rs")),
        "main branch should keep shared content discoverable"
    );
    assert!(
        search_rows_with_home(
            &canonical_home,
            &feature_worktree,
            "shared branch acceptance sentinel",
        )
        .iter()
        .any(|row| row.file_path.ends_with("shared.rs")),
        "feature branch should keep shared content discoverable"
    );
}
