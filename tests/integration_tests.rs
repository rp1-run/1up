use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use tempfile::TempDir;

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

/// RAII guard that temporarily hides the embedding model to force FTS-only mode.
/// On drop, the model is restored. This works around a pre-existing vector
/// dimension mismatch bug in the int8 search path. Holds a mutex to prevent
/// concurrent test interference.
struct HideModelGuard {
    model_path: PathBuf,
    hidden_path: PathBuf,
    marker_path: PathBuf,
    active: bool,
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
        let marker_path = model_dir.join(".download_failed");

        let active = model_path.exists();
        if active {
            fs::rename(&model_path, &hidden_path).unwrap();
        }
        // Create download failure marker to prevent auto-download during tests
        let _ = fs::write(&marker_path, "hidden_by_test");

        Self {
            model_path,
            hidden_path,
            marker_path,
            active,
            _lock: lock,
        }
    }
}

impl Drop for HideModelGuard {
    fn drop(&mut self) {
        if self.active && self.hidden_path.exists() {
            let _ = fs::rename(&self.hidden_path, &self.model_path);
        }
        let _ = fs::remove_file(&self.marker_path);
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
        .args(["--format", "json", "init", dir.path().to_str().unwrap()])
        .assert()
        .success();

    cmd()
        .args(["--format", "json", "index", dir.path().to_str().unwrap()])
        .assert()
        .success();
}

fn init_and_index_fts_only(dir: &TempDir) -> HideModelGuard {
    let guard = HideModelGuard::new();

    cmd()
        .args(["--format", "json", "init", dir.path().to_str().unwrap()])
        .assert()
        .success();

    cmd()
        .args(["--format", "json", "index", dir.path().to_str().unwrap()])
        .assert()
        .success();

    guard
}

fn init_project(dir: &std::path::Path) {
    cmd()
        .args(["--format", "json", "init", dir.to_str().unwrap()])
        .assert()
        .success();
}

fn run_index_json(dir: &std::path::Path, extra_args: &[&str]) -> serde_json::Value {
    let mut command = cmd();
    command.arg("--format").arg("json").arg("index");
    for arg in extra_args {
        command.arg(arg);
    }
    command.arg(dir);

    let output = command.output().unwrap();
    assert!(output.status.success());

    serde_json::from_str(String::from_utf8(output.stdout).unwrap().trim()).unwrap()
}

fn lookup_symbol_json(dir: &std::path::Path, symbol: &str) -> Vec<serde_json::Value> {
    let output = cmd()
        .args([
            "--format",
            "json",
            "symbol",
            symbol,
            "--path",
            dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    serde_json::from_str(String::from_utf8(output.stdout).unwrap().trim()).unwrap()
}

fn search_json(dir: &std::path::Path, query: &str) -> Vec<serde_json::Value> {
    let output = cmd()
        .args([
            "--format",
            "json",
            "search",
            query,
            "--path",
            dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "search failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    serde_json::from_str(String::from_utf8(output.stdout).unwrap().trim()).unwrap()
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
        tmp.path().join("src").join("routing.rs"),
        r#"pub struct PolicyRuleValidator;

impl PolicyRuleValidator {
    pub fn validate(&self, route: &str) -> bool {
        !route.is_empty()
    }
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("src").join("runner.rs"),
        r#"use crate::routing::PolicyRuleValidator;

pub fn run_validation(validator: &PolicyRuleValidator) -> bool {
    validator.validate("orders")
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("src").join("webhooks.rs"),
        r#"// validate incoming webhook signatures
pub fn validate_incoming_request_signatures(secret: &str, header: &str) -> bool {
    !secret.is_empty() && header.contains(secret)
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("config").join("webhooks.yaml"),
        r#"request_signing_secret: sq-test-secret
description: request signing secret used for request validation
policy_rule_preview_enabled: true
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("docs").join("webhooks.md"),
        r#"# Webhooks API documentation guide

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

// ---------- Test fixture: index a small multi-language repository ----------

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

// ---------- Verify symbol lookup returns correct definitions ----------

#[test]
fn symbol_lookup_returns_definitions_json() {
    let tmp = create_multi_lang_fixture();
    init_and_index(&tmp);

    let output = cmd()
        .args([
            "--format",
            "json",
            "symbol",
            "greet",
            "--path",
            tmp.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    let results: Vec<serde_json::Value> = serde_json::from_str(stdout.trim()).unwrap();

    assert!(!results.is_empty(), "should find 'greet' symbol");

    let first = &results[0];
    assert!(first.get("name").is_some());
    assert!(first.get("kind").is_some());
    assert!(first.get("file_path").is_some());
    assert!(first.get("line_start").is_some());
    assert!(first.get("line_end").is_some());
    assert!(first.get("content").is_some());
    assert!(first.get("reference_kind").is_some());

    assert_eq!(first["name"], "greet");
    assert_eq!(first["reference_kind"], "definition");
}

#[test]
fn symbol_lookup_references_include_definitions_and_usages() {
    let tmp = create_multi_lang_fixture();
    init_and_index(&tmp);

    let output = cmd()
        .args([
            "--format",
            "json",
            "symbol",
            "--references",
            "Config",
            "--path",
            tmp.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    let results: Vec<serde_json::Value> = serde_json::from_str(stdout.trim()).unwrap();

    let definitions: Vec<_> = results
        .iter()
        .filter(|r| r["reference_kind"] == "definition")
        .collect();
    assert!(
        !definitions.is_empty(),
        "should have at least one definition for Config"
    );
}

#[test]
fn symbol_lookup_acceptance_queries_cover_exact_canonical_and_references() {
    let tmp = create_search_acceptance_fixture();
    init_and_index(&tmp);

    let exact = lookup_symbol_json(tmp.path(), "PolicyRuleValidator");
    assert_eq!(exact[0]["name"], "PolicyRuleValidator");
    assert_eq!(exact[0]["file_path"], "src/policy.rs");

    let canonical = lookup_symbol_json(tmp.path(), "routing rule validator");
    assert_eq!(canonical[0]["name"], "PolicyRuleValidator");
    assert_eq!(canonical[0]["file_path"], "src/policy.rs");

    let references = cmd()
        .args([
            "--format",
            "json",
            "symbol",
            "--references",
            "PolicyRuleValidator",
            "--path",
            tmp.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(references.status.success());

    let results: Vec<serde_json::Value> =
        serde_json::from_str(String::from_utf8(references.stdout).unwrap().trim()).unwrap();

    assert!(results.iter().any(|result| {
        result["reference_kind"] == "definition" && result["file_path"] == "src/policy.rs"
    }));
    assert!(results.iter().any(|result| {
        result["reference_kind"] == "usage" && result["file_path"] == "src/runner.rs"
    }));
}

// ---------- Verify hybrid search returns ranked results ----------

#[test]
fn fts_search_returns_ranked_results_json() {
    let tmp = create_multi_lang_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let output = cmd()
        .args([
            "--format",
            "json",
            "search",
            "Config host port",
            "--path",
            tmp.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    let results: Vec<serde_json::Value> = serde_json::from_str(stdout.trim()).unwrap();

    assert!(
        !results.is_empty(),
        "search for 'Config host port' should return results"
    );

    let first = &results[0];
    assert!(first.get("file_path").is_some());
    assert!(first.get("language").is_some());
    assert!(first.get("block_type").is_some());
    assert!(first.get("content").is_some());
    assert!(first.get("score").is_some());
    assert!(first.get("line_number").is_some());

    let score = first["score"].as_f64().unwrap();
    assert!(score > 0.0, "search results should have positive scores");
}

#[test]
fn search_acceptance_queries_preserve_top_hits_for_priority_classes() {
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
        ("validate incoming webhook signatures", "src/signatures.rs"),
        ("PolicyRulePreview", "proto/policy_rules.proto"),
        ("policy_rules_preview table", "sql/policy_rules.sql"),
    ];

    for (query, expected_top_path) in cases {
        let first = search_json(tmp.path(), query);
        let second = search_json(tmp.path(), query);

        assert!(
            !first.is_empty(),
            "query {query:?} should produce at least one result"
        );
        assert_eq!(
            first[0]["file_path"], expected_top_path,
            "query {query:?} returned an unexpected top hit"
        );

        let first_paths: Vec<_> = first
            .iter()
            .take(3)
            .map(|result| result["file_path"].clone())
            .collect();
        let second_paths: Vec<_> = second
            .iter()
            .take(3)
            .map(|result| result["file_path"].clone())
            .collect();

        assert_eq!(
            first_paths, second_paths,
            "query {query:?} should keep a stable top-3 result set"
        );
    }
}

// ---------- Verify context retrieval returns enclosing scope ----------

#[test]
fn context_retrieval_returns_enclosing_scope_json() {
    let tmp = create_multi_lang_fixture();
    init_and_index(&tmp);

    let output = cmd()
        .args([
            "--format",
            "json",
            "context",
            "main.rs:4",
            "--path",
            tmp.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    let result: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();

    assert!(result.get("file_path").is_some());
    assert!(result.get("language").is_some());
    assert!(result.get("content").is_some());
    assert!(result.get("line_start").is_some());
    assert!(result.get("line_end").is_some());
    assert!(result.get("scope_type").is_some());
    assert!(result.get("access_scope").is_some());

    assert_eq!(result["scope_type"], "function");
    assert_eq!(result["access_scope"], "project_root");
    assert!(result["content"].as_str().unwrap().contains("fn greet"));
}

#[test]
fn context_retrieval_python_scope() {
    let tmp = create_multi_lang_fixture();
    init_and_index(&tmp);

    let output = cmd()
        .args([
            "--format",
            "json",
            "context",
            "utils.py:6",
            "--path",
            tmp.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    let result: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();

    assert_eq!(result["scope_type"], "function");
    assert_eq!(result["access_scope"], "project_root");
    assert!(result["content"].as_str().unwrap().contains("parse_config"));
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

    let output = cmd()
        .args([
            "--format",
            "json",
            "context",
            &location,
            "--path",
            project_root.to_str().unwrap(),
            "--allow-outside-root",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    let result: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();

    assert_eq!(result["scope_type"], "function");
    assert_eq!(result["access_scope"], "outside_root");
    assert!(result["content"].as_str().unwrap().contains("fn leaked"));
}

// ---------- Verify JSON output conforms to schema ----------

#[test]
fn json_output_search_schema_conformance() {
    let tmp = create_multi_lang_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let output = cmd()
        .args([
            "--format",
            "json",
            "search",
            "config",
            "--path",
            tmp.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    let stdout = String::from_utf8(output.stdout).unwrap();
    let results: Vec<serde_json::Value> = serde_json::from_str(stdout.trim()).unwrap();

    for result in &results {
        assert!(result["file_path"].is_string(), "file_path must be string");
        assert!(result["language"].is_string(), "language must be string");
        assert!(
            result["block_type"].is_string(),
            "block_type must be string"
        );
        assert!(result["content"].is_string(), "content must be string");
        assert!(result["score"].is_number(), "score must be number");
        assert!(
            result["line_number"].is_number(),
            "line_number must be number"
        );
    }
}

#[test]
fn json_output_symbol_schema_conformance() {
    let tmp = create_multi_lang_fixture();
    init_and_index(&tmp);

    let output = cmd()
        .args([
            "--format",
            "json",
            "symbol",
            "Config",
            "--path",
            tmp.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    let stdout = String::from_utf8(output.stdout).unwrap();
    let results: Vec<serde_json::Value> = serde_json::from_str(stdout.trim()).unwrap();

    assert!(!results.is_empty());
    for result in &results {
        assert!(result["name"].is_string(), "name must be string");
        assert!(result["kind"].is_string(), "kind must be string");
        assert!(result["file_path"].is_string(), "file_path must be string");
        assert!(
            result["line_start"].is_number(),
            "line_start must be number"
        );
        assert!(result["line_end"].is_number(), "line_end must be number");
        assert!(result["content"].is_string(), "content must be string");
        assert!(
            result["reference_kind"].is_string(),
            "reference_kind must be string"
        );
        let ref_kind = result["reference_kind"].as_str().unwrap();
        assert!(
            ref_kind == "definition" || ref_kind == "usage",
            "reference_kind must be 'definition' or 'usage'"
        );
    }
}

#[test]
fn json_output_context_schema_conformance() {
    let tmp = create_multi_lang_fixture();
    init_and_index(&tmp);

    let output = cmd()
        .args([
            "--format",
            "json",
            "context",
            "handler.js:2",
            "--path",
            tmp.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    let stdout = String::from_utf8(output.stdout).unwrap();
    let result: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();

    assert!(result["file_path"].is_string());
    assert!(result["language"].is_string());
    assert!(result["content"].is_string());
    assert!(result["line_start"].is_number());
    assert!(result["line_end"].is_number());
    assert!(result["scope_type"].is_string());
    assert!(result["access_scope"].is_string());
}

// ---------- Verify incremental indexing ----------

#[test]
fn incremental_indexing_detects_changes() {
    let tmp = create_multi_lang_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let output1 = cmd()
        .args([
            "--format",
            "json",
            "symbol",
            "greet",
            "--path",
            tmp.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let results1: Vec<serde_json::Value> =
        serde_json::from_str(String::from_utf8(output1.stdout).unwrap().trim()).unwrap();
    assert!(!results1.is_empty());

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
        .args(["--format", "json", "index", tmp.path().to_str().unwrap()])
        .assert()
        .success();

    let output2 = cmd()
        .args([
            "--format",
            "json",
            "symbol",
            "greet",
            "--path",
            tmp.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let results2: Vec<serde_json::Value> =
        serde_json::from_str(String::from_utf8(output2.stdout).unwrap().trim()).unwrap();
    assert!(
        results2.is_empty(),
        "greet should no longer exist after re-index"
    );

    let output3 = cmd()
        .args([
            "--format",
            "json",
            "symbol",
            "welcome",
            "--path",
            tmp.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let results3: Vec<serde_json::Value> =
        serde_json::from_str(String::from_utf8(output3.stdout).unwrap().trim()).unwrap();
    assert!(!results3.is_empty(), "welcome should exist after re-index");
    assert_eq!(results3[0]["name"], "welcome");
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

    assert!(lookup_symbol_json(default_repo.path(), "removed_symbol").is_empty());
    assert!(lookup_symbol_json(serial_repo.path(), "removed_symbol").is_empty());
    assert_eq!(
        lookup_symbol_json(default_repo.path(), "beta_symbol").len(),
        1
    );
    assert_eq!(
        lookup_symbol_json(serial_repo.path(), "beta_symbol").len(),
        1
    );
    assert_eq!(
        lookup_symbol_json(default_repo.path(), "fresh_symbol").len(),
        1
    );
    assert_eq!(
        lookup_symbol_json(serial_repo.path(), "fresh_symbol").len(),
        1
    );
    assert_eq!(
        lookup_symbol_json(default_repo.path(), "stable_symbol").len(),
        1
    );
    assert_eq!(
        lookup_symbol_json(serial_repo.path(), "stable_symbol").len(),
        1
    );
}

// ---------- Daemon lifecycle test ----------

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

// ---------- CLI integration tests ----------

#[test]
fn cli_init_then_index_then_search_workflow() {
    let tmp = create_multi_lang_fixture();
    let _guard = HideModelGuard::new();

    cmd()
        .args(["--format", "json", "init", tmp.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("Initialized"));

    let id_path = tmp.path().join(".1up").join("project_id");
    assert!(id_path.exists());

    cmd()
        .args(["--format", "json", "index", tmp.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("Indexed"));

    cmd()
        .args([
            "--format",
            "json",
            "search",
            "logger",
            "--path",
            tmp.path().to_str().unwrap(),
        ])
        .assert()
        .success();
}

#[test]
fn index_json_output_includes_progress_summary() {
    let tmp = create_multi_lang_fixture();
    let _guard = HideModelGuard::new();

    cmd()
        .args(["--format", "json", "init", tmp.path().to_str().unwrap()])
        .assert()
        .success();

    let output = cmd()
        .args(["--format", "json", "index", tmp.path().to_str().unwrap()])
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
        .args(["--format", "json", "index", tmp.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(second_index.status.success());

    let output = cmd()
        .args(["--format", "json", "status", tmp.path().to_str().unwrap()])
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

#[test]
fn cli_search_uses_daemon_results_before_local_fallback() {
    let home = tempfile::Builder::new()
        .prefix("1up-home-")
        .tempdir_in("/tmp")
        .unwrap();
    let project = TempDir::new().unwrap();
    let socket_path = test_data_dir(home.path()).join("daemon.sock");
    let expected_root = project.path().canonicalize().unwrap();

    let server_socket_path = socket_path.clone();
    let server_expected_root = expected_root.clone();
    let server = std::thread::spawn(move || {
        use std::io::{Read, Write};
        use std::os::unix::net::UnixListener;

        if let Some(parent) = server_socket_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let _ = fs::remove_file(&server_socket_path);

        let listener = UnixListener::bind(&server_socket_path).unwrap();
        let (mut stream, _) = listener.accept().unwrap();

        let mut request = Vec::new();
        stream.read_to_end(&mut request).unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&request).unwrap();
        assert_eq!(
            payload["project_root"].as_str().unwrap(),
            server_expected_root.to_str().unwrap()
        );
        assert_eq!(payload["query"], "test");
        assert_eq!(payload["limit"], 20);

        let response = serde_json::json!({
            "status": "results",
            "results": [
                {
                    "file_path": "src/daemon.rs",
                    "language": "rust",
                    "block_type": "function",
                    "content": "fn daemon_search() {}",
                    "score": 1.0,
                    "line_number": 3,
                    "line_end": 3
                }
            ]
        });
        stream.write_all(response.to_string().as_bytes()).unwrap();
    });

    cmd()
        .env("HOME", home.path())
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_CONFIG_HOME")
        .args([
            "--format",
            "json",
            "search",
            "test",
            "--path",
            project.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("src/daemon.rs"));

    server.join().unwrap();
}

#[test]
fn cli_search_without_index_requires_reindex() {
    let tmp = TempDir::new().unwrap();

    cmd()
        .args([
            "--format",
            "json",
            "search",
            "test",
            "--path",
            tmp.path().to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("1up reindex"));
}

#[test]
fn cli_symbol_without_index_requires_reindex() {
    let tmp = TempDir::new().unwrap();

    cmd()
        .args([
            "--format",
            "json",
            "symbol",
            "test",
            "--path",
            tmp.path().to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("1up reindex"));
}

#[test]
fn cli_context_nonexistent_file_fails() {
    let tmp = TempDir::new().unwrap();

    cmd()
        .args([
            "--format",
            "json",
            "context",
            "nonexistent.rs:1",
            "--path",
            tmp.path().to_str().unwrap(),
        ])
        .assert()
        .failure();
}

#[test]
fn cli_output_formats_all_work() {
    let tmp = create_multi_lang_fixture();
    init_and_index(&tmp);

    for fmt in &["json", "human", "plain"] {
        cmd()
            .args([
                "--format",
                fmt,
                "symbol",
                "Config",
                "--path",
                tmp.path().to_str().unwrap(),
            ])
            .assert()
            .success();
    }
}

#[test]
fn cli_search_empty_results_returns_empty_array_json() {
    let tmp = create_multi_lang_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let output = cmd()
        .args([
            "--format",
            "json",
            "search",
            "zznonexistentqueryzz",
            "--path",
            tmp.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    let results: Vec<serde_json::Value> = serde_json::from_str(stdout.trim()).unwrap();
    assert!(results.is_empty());
}

#[test]
fn cli_symbol_empty_results_returns_empty_array_json() {
    let tmp = create_multi_lang_fixture();
    init_and_index(&tmp);

    let output = cmd()
        .args([
            "--format",
            "json",
            "symbol",
            "zznonexistentsymbolzz",
            "--path",
            tmp.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    let results: Vec<serde_json::Value> = serde_json::from_str(stdout.trim()).unwrap();
    assert!(results.is_empty());
}
