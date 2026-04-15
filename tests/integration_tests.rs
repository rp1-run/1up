use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Mutex;
use tempfile::TempDir;

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

fn impact_json(dir: &std::path::Path, args: &[&str]) -> serde_json::Value {
    let mut command = cmd();
    command.arg("--format").arg("json").arg("impact");
    for arg in args {
        command.arg(arg);
    }
    command.arg("--path").arg(dir);

    let output = command.output().unwrap();

    assert!(
        output.status.success(),
        "impact failed: {}",
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

    let canonical = lookup_symbol_json(tmp.path(), "policy rule validator");
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
        ("validate incoming request signatures", "src/signatures.rs"),
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

#[test]
fn impact_file_anchor_returns_ranked_results_json() {
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let result = impact_json(tmp.path(), &["--from-file", "src/auth/runtime.rs"]);

    assert_eq!(result["status"], "expanded");
    assert_eq!(result["resolved_anchor"]["kind"], "file");
    assert_eq!(result["resolved_anchor"]["value"], "src/auth/runtime.rs");

    let results = result["results"].as_array().unwrap();
    assert!(!results.is_empty());
    assert_eq!(results[0]["file_path"], "src/auth/bootstrap.rs");
    assert_eq!(results[0]["reasons"][0]["kind"], "called_by");
}

#[test]
fn impact_file_line_anchor_resolves_requested_line_json() {
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let result = impact_json(tmp.path(), &["--from-file", "src/auth/runtime.rs:1"]);

    assert_eq!(result["status"], "expanded");
    assert_eq!(result["resolved_anchor"]["kind"], "file_line");
    assert_eq!(result["resolved_anchor"]["value"], "src/auth/runtime.rs");
    assert_eq!(result["resolved_anchor"]["line"], 1);

    let results = result["results"].as_array().unwrap();
    assert!(!results.is_empty());
    assert_eq!(results[0]["file_path"], "src/auth/bootstrap.rs");
}

#[test]
fn impact_symbol_anchor_expands_with_resolved_seed_json() {
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let result = impact_json(tmp.path(), &["--from-symbol", "load_auth_config"]);

    assert_eq!(result["status"], "expanded");
    assert_eq!(result["resolved_anchor"]["kind"], "symbol");
    assert_eq!(result["resolved_anchor"]["value"], "load_auth_config");

    let results = result["results"].as_array().unwrap();
    assert!(!results.is_empty());
    assert!(results
        .iter()
        .any(|candidate| candidate["file_path"] == "src/auth/bootstrap.rs"));
}

#[test]
fn impact_symbol_anchor_scope_narrows_ambiguous_matches_json() {
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let result = impact_json(
        tmp.path(),
        &["--from-symbol", "load_config", "--scope", "src/auth"],
    );

    assert_eq!(result["status"], "expanded_scoped");
    assert_eq!(result["resolved_anchor"]["kind"], "symbol");
    assert_eq!(result["resolved_anchor"]["value"], "load_config");
    assert_eq!(result["resolved_anchor"]["scope"], "src/auth");

    let results = result["results"].as_array().unwrap();
    assert!(!results.is_empty());
    assert_eq!(results[0]["file_path"], "src/auth/reload.rs");
}

#[test]
fn impact_symbol_anchor_qualified_relation_promotes_matching_definition_json() {
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let result = impact_json(tmp.path(), &["--from-symbol", "reload_auth_config"]);

    assert_eq!(result["status"], "expanded");
    assert_eq!(result["resolved_anchor"]["kind"], "symbol");
    assert_eq!(result["resolved_anchor"]["value"], "reload_auth_config");

    let results = result["results"]
        .as_array()
        .expect("qualified relation should surface a primary definition");
    assert!(!results.is_empty());
    assert!(
        results
            .iter()
            .any(|r| r["file_path"] == "src/auth/config.rs"),
        "config.rs should appear in results, got: {:?}",
        results
            .iter()
            .map(|r| r["file_path"].as_str().unwrap_or("?"))
            .collect::<Vec<_>>()
    );
}

#[test]
fn impact_symbol_anchor_interface_implementor_surfaces_primary_json() {
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let result = impact_json(tmp.path(), &["--from-symbol", "AuthStore"]);

    assert_eq!(result["status"], "expanded");
    assert_eq!(result["resolved_anchor"]["kind"], "symbol");
    assert_eq!(result["resolved_anchor"]["value"], "AuthStore");

    let results = result["results"]
        .as_array()
        .expect("interface anchor should surface implementing classes");
    assert!(results.iter().any(|candidate| {
        candidate["file_path"].as_str() == Some("src/auth/auth_store.ts")
            && candidate["block_type"].as_str() == Some("class")
            && candidate["defined_symbols"]
                .as_array()
                .map(|symbols| {
                    symbols
                        .iter()
                        .any(|symbol| symbol.as_str() == Some("SqlAuthStore"))
                })
                .unwrap_or(false)
            && candidate["reasons"]
                .as_array()
                .map(|reasons| {
                    reasons
                        .iter()
                        .any(|reason| reason["kind"].as_str() == Some("implemented_by"))
                })
                .unwrap_or(false)
    }));
}

#[test]
fn impact_symbol_anchor_formatter_implementor_stays_primary_under_reference_pressure_json() {
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let result = impact_json(tmp.path(), &["--from-symbol", "Formatter"]);

    assert_eq!(result["status"], "expanded");
    assert_eq!(result["resolved_anchor"]["kind"], "symbol");
    assert_eq!(result["resolved_anchor"]["value"], "Formatter");

    let results = result["results"]
        .as_array()
        .expect("formatter anchor should surface implementors");
    assert!(!results.is_empty());
    assert_eq!(results[0]["file_path"], "src/ui/plain_formatter.ts");

    if let Some(contextual) = result["contextual_results"].as_array() {
        assert!(contextual.iter().all(|candidate| {
            candidate["file_path"]
                .as_str()
                .map(|path| path != "src/ui/plain_formatter.ts")
                .unwrap_or(false)
        }));
    }
}

#[test]
fn impact_symbol_anchor_ambiguous_helper_returns_context_only_json() {
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let result = impact_json(tmp.path(), &["--from-symbol", "boot_global_config"]);

    assert_eq!(result["status"], "empty");
    assert_eq!(result["resolved_anchor"]["kind"], "symbol");
    assert_eq!(result["resolved_anchor"]["value"], "boot_global_config");
    assert_eq!(result["hint"]["code"], "context_only");
    assert_eq!(result["results"], serde_json::json!([]));

    let contextual = result["contextual_results"]
        .as_array()
        .expect("ambiguous helper follow-up should stay contextual");
    assert!(!contextual.is_empty());
    assert!(contextual.iter().all(|candidate| {
        candidate["file_path"]
            .as_str()
            .map(|path| matches!(path, "src/auth/config.rs" | "src/app/bootstrap.rs"))
            .unwrap_or(false)
    }));
}

#[test]
fn impact_symbol_anchor_prefers_stronger_primary_over_wrapper_json() {
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let result = impact_json(tmp.path(), &["--from-symbol", "warm_cache_key"]);

    assert_eq!(result["status"], "expanded");
    assert_eq!(result["resolved_anchor"]["kind"], "symbol");
    assert_eq!(result["resolved_anchor"]["value"], "warm_cache_key");

    let results = result["results"].as_array().unwrap();
    assert!(!results.is_empty());
    assert_eq!(results[0]["file_path"], "src/cache/worker.rs");

    if let Some(wrapper_index) = results
        .iter()
        .position(|candidate| candidate["file_path"].as_str() == Some("src/cache/priming.rs"))
    {
        assert!(wrapper_index > 0);
    }
}

#[test]
fn impact_symbol_anchor_inline_test_context_stays_contextual_json() {
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let result = impact_json(tmp.path(), &["--from-symbol", "warm_cache_key"]);

    assert_eq!(result["status"], "expanded");

    let results = result["results"]
        .as_array()
        .expect("warm_cache_key should still have primary candidates");
    assert!(results.iter().all(|candidate| {
        candidate["file_path"]
            .as_str()
            .map(|path| path != "src/cache/test_support.rs")
            .unwrap_or(false)
    }));

    let contextual = result["contextual_results"]
        .as_array()
        .expect("inline test context should remain available as contextual guidance");
    assert!(contextual
        .iter()
        .any(|candidate| { candidate["file_path"].as_str() == Some("src/cache/test_support.rs") }));
}

#[test]
fn impact_file_anchor_limit_caps_total_primary_and_contextual_results_json() {
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let result = impact_json(
        tmp.path(),
        &["--from-file", "src/auth/runtime.rs", "--limit", "1"],
    );

    assert_eq!(result["status"], "expanded");

    let results = result["results"].as_array().unwrap();
    let contextual_len = result["contextual_results"]
        .as_array()
        .map(|results| results.len())
        .unwrap_or(0);

    assert_eq!(results.len(), 1);
    assert_eq!(results.len() + contextual_len, 1);
}

#[test]
fn impact_file_anchor_scope_refuses_out_of_scope_seed_json() {
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let result = impact_json(
        tmp.path(),
        &["--from-file", "src/auth/runtime.rs", "--scope", "src/cache"],
    );

    assert_eq!(result["status"], "refused");
    assert_eq!(result["refusal"]["reason"], "anchor_out_of_scope");
    assert_eq!(result["hint"]["code"], "align_anchor_and_scope");
}

#[test]
fn impact_symbol_anchor_refuses_broad_requests_with_hint_json() {
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let result = impact_json(tmp.path(), &["--from-symbol", "load_config"]);

    assert_eq!(result["status"], "refused");
    assert_eq!(result["refusal"]["reason"], "symbol_too_broad");
    assert_eq!(result["hint"]["code"], "narrow_with_scope");
    assert!(result["hint"]["message"]
        .as_str()
        .unwrap()
        .contains("--scope"));
}

#[test]
fn impact_file_line_anchor_returns_empty_with_contextual_guidance_json() {
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let result = impact_json(tmp.path(), &["--from-file", "src/auth/runtime.rs:5"]);

    assert_eq!(result["status"], "empty");
    assert_eq!(result["resolved_anchor"]["kind"], "file_line");
    assert_eq!(result["hint"]["code"], "context_only");
    assert_eq!(result["results"], serde_json::json!([]));

    let seed_id = result["resolved_anchor"]["seed_segment_ids"][0]
        .as_str()
        .expect("file-line anchor should resolve to a seed segment");
    let contextual = result["contextual_results"]
        .as_array()
        .expect("empty impact should still surface contextual guidance");

    assert!(!contextual.is_empty());
    assert_eq!(contextual[0]["file_path"], "src/auth/runtime.rs");
    assert!(contextual
        .iter()
        .all(|candidate| candidate["segment_id"].as_str() != Some(seed_id)));
}

#[test]
fn impact_scoped_file_line_anchor_returns_empty_scoped_without_anchor_echo_json() {
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let result = impact_json(
        tmp.path(),
        &[
            "--from-file",
            "src/auth/runtime.rs:5",
            "--scope",
            "src/auth",
        ],
    );

    assert_eq!(result["status"], "empty_scoped");
    assert_eq!(result["resolved_anchor"]["kind"], "file_line");
    assert_eq!(result["resolved_anchor"]["scope"], "src/auth");
    assert_eq!(result["hint"]["code"], "context_only");
    assert_eq!(result["results"], serde_json::json!([]));

    let seed_id = result["resolved_anchor"]["seed_segment_ids"][0]
        .as_str()
        .expect("scoped file-line anchor should resolve to a seed segment");
    let contextual = result["contextual_results"]
        .as_array()
        .expect("empty-scoped impact should retain contextual guidance");

    assert!(!contextual.is_empty());
    assert!(contextual.iter().all(|candidate| {
        candidate["file_path"]
            .as_str()
            .map(|path| path.starts_with("src/auth/"))
            .unwrap_or(false)
    }));
    assert!(contextual
        .iter()
        .all(|candidate| candidate["segment_id"].as_str() != Some(seed_id)));
}

#[test]
fn search_segment_id_round_trips_into_impact_from_segment_json() {
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let search_results = search_json(tmp.path(), "load auth config");
    assert!(
        !search_results.is_empty(),
        "search fixture should produce ranked hits"
    );

    let seed = search_results
        .iter()
        .find(|result| {
            result["file_path"].as_str() == Some("src/auth/runtime.rs")
                && result["content"]
                    .as_str()
                    .map(|content| content.contains("load_auth_config"))
                    .unwrap_or(false)
        })
        .expect("search should return the runtime definition segment");
    let segment_id = seed["segment_id"]
        .as_str()
        .expect("search results should expose a segment_id follow-up handle");

    let result = impact_json(tmp.path(), &["--from-segment", segment_id]);

    assert_eq!(result["status"], "expanded");
    assert_eq!(result["resolved_anchor"]["kind"], "segment");
    assert_eq!(result["resolved_anchor"]["value"], segment_id);

    let results = result["results"].as_array().unwrap();
    assert!(
        !results.is_empty(),
        "round-tripped impact should return candidates"
    );
    assert_eq!(results[0]["file_path"], "src/auth/bootstrap.rs");
    assert!(results
        .iter()
        .all(|candidate| candidate["segment_id"].as_str() != Some(segment_id)));
    assert!(results[0]["reasons"]
        .as_array()
        .unwrap()
        .iter()
        .any(|reason| reason["from_segment_id"].as_str() == Some(segment_id)));
    if let Some(contextual) = result["contextual_results"].as_array() {
        assert!(contextual
            .iter()
            .all(|candidate| candidate["segment_id"].as_str() != Some(segment_id)));
    }
}

#[test]
fn search_segment_id_handoff_keeps_search_top_hits_stable() {
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let before = search_json(tmp.path(), "load auth config");
    assert!(
        !before.is_empty(),
        "search fixture should produce ranked hits"
    );

    let segment_id = before
        .iter()
        .find_map(|result| result["segment_id"].as_str())
        .expect("search results should expose at least one segment_id");

    let _result = impact_json(tmp.path(), &["--from-segment", segment_id]);
    let after = search_json(tmp.path(), "load auth config");

    let before_ranked: Vec<_> = before
        .iter()
        .take(5)
        .map(|result| {
            (
                result["file_path"].as_str().unwrap().to_string(),
                result["line_number"].as_u64().unwrap(),
                result["block_type"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    let after_ranked: Vec<_> = after
        .iter()
        .take(5)
        .map(|result| {
            (
                result["file_path"].as_str().unwrap().to_string(),
                result["line_number"].as_u64().unwrap(),
                result["block_type"].as_str().unwrap().to_string(),
            )
        })
        .collect();

    assert_eq!(before_ranked, after_ranked);
}

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
    assert_eq!(result["access_scope"], "project_root");
    assert!(result["content"].as_str().unwrap().contains("fn internal"));
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

#[cfg(unix)]
#[test]
fn cli_search_uses_daemon_results_before_local_fallback() {
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
        write_framed_json(&mut stream, &response);
    });

    ready_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .unwrap();

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
