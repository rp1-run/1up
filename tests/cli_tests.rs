use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

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

#[test]
fn help_shows_all_subcommands() {
    cmd().arg("--help").assert().success().stdout(
        predicate::str::contains("init")
            .and(predicate::str::contains("start"))
            .and(predicate::str::contains("stop"))
            .and(predicate::str::contains("status"))
            .and(predicate::str::contains("symbol"))
            .and(predicate::str::contains("search"))
            .and(predicate::str::contains("context"))
            .and(predicate::str::contains("index"))
            .and(predicate::str::contains("reindex"))
            .and(predicate::str::contains("hello-agent")),
    );
}

#[test]
fn worker_subcommand_hidden_from_help() {
    cmd()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("__worker").not());
}

#[test]
fn subcommand_help_works() {
    for sub in &[
        "init",
        "start",
        "stop",
        "status",
        "symbol",
        "search",
        "context",
        "index",
        "reindex",
        "hello-agent",
    ] {
        cmd()
            .args([sub, "--help"])
            .assert()
            .success()
            .stdout(predicate::str::contains("Usage:"));
    }
}

#[test]
fn indexing_subcommands_expose_parallel_controls() {
    for sub in &["index", "reindex", "start"] {
        cmd().args([sub, "--help"]).assert().success().stdout(
            predicate::str::contains("--jobs").and(predicate::str::contains("--embed-threads")),
        );
    }
}

#[test]
fn indexing_subcommands_reject_zero_parallel_values() {
    for sub in &["index", "reindex", "start"] {
        cmd()
            .args([sub, "--jobs", "0"])
            .assert()
            .failure()
            .stderr(predicate::str::contains("invalid value"));

        cmd()
            .args([sub, "--embed-threads", "0"])
            .assert()
            .failure()
            .stderr(predicate::str::contains("invalid value"));
    }
}

#[test]
fn format_flag_accepts_all_variants() {
    for fmt in &["json", "human", "plain"] {
        cmd().args(["--format", fmt, "--help"]).assert().success();
    }
}

#[test]
fn help_shows_plain_as_default_output_format() {
    cmd().arg("--help").assert().success().stdout(
        predicate::str::contains("Output format: plain (default), json, human")
            .and(predicate::str::contains("[default: plain]")),
    );
}

#[test]
fn status_defaults_to_plain_output() {
    let dir = tempfile::tempdir().unwrap();

    cmd()
        .args(["status", dir.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(
            predicate::str::starts_with("daemon:").and(predicate::str::contains("Daemon:").not()),
        );
}

#[test]
fn status_reports_uninitialized_project_and_missing_index() {
    let dir = tempfile::tempdir().unwrap();
    let output = cmd()
        .args(["--format", "json", "status", dir.path().to_str().unwrap()])
        .output()
        .unwrap();

    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    let payload: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(payload["project_initialized"], false);
    assert_eq!(payload["index_status"], "not_built");
    assert!(payload["last_file_check_at"].is_null());
    assert!(payload["project_id"].is_null());
    assert!(payload["indexed_files"].is_null());
}

#[test]
fn invalid_format_flag_rejected() {
    cmd()
        .args(["--format", "xml", "search", "test"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown output format"));
}

#[test]
fn init_creates_project_id() {
    let dir = tempfile::tempdir().unwrap();
    cmd()
        .args(["--format", "json", "init", dir.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("Initialized project"));

    let id_path = dir.path().join(".1up").join("project_id");
    assert!(id_path.exists());
    let id = std::fs::read_to_string(&id_path).unwrap();
    assert!(!id.trim().is_empty());
}

#[test]
fn init_warns_if_already_initialized() {
    let dir = tempfile::tempdir().unwrap();
    let dot_dir = dir.path().join(".1up");
    std::fs::create_dir_all(&dot_dir).unwrap();
    std::fs::write(dot_dir.join("project_id"), "existing-id").unwrap();

    cmd()
        .args(["--format", "json", "init", dir.path().to_str().unwrap()])
        .assert()
        .success()
        .stderr(predicate::str::contains("already initialized"));
}

#[test]
fn start_auto_initializes_project_if_needed() {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let canonical_dir = dir.path().canonicalize().unwrap();
    let canonical_home = home.path().canonicalize().unwrap();
    let data_dir = test_data_dir(&canonical_home);
    let model_dir = data_dir.join("models").join("all-MiniLM-L6-v2");
    fs::create_dir_all(&model_dir).unwrap();
    fs::write(model_dir.join(".download_failed"), "skip download in test").unwrap();
    fs::write(
        dir.path().join("main.rs"),
        "fn main() {\n    println!(\"hi\");\n}\n",
    )
    .unwrap();

    let output = cmd()
        .env("HOME", &canonical_home)
        .env("XDG_DATA_HOME", canonical_home.join(".local").join("share"))
        .env("XDG_CONFIG_HOME", canonical_home.join(".config"))
        .args(["--format", "json", "start", canonical_dir.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    let payload: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    let message = payload["message"].as_str().unwrap();
    assert!(message.contains("Initialized project"));
    assert!(message.contains("Daemon started"));
    assert!(payload["progress"]["files_indexed"].as_u64().unwrap() > 0);
    assert!(payload["progress"]["segments_stored"].as_u64().unwrap() > 0);

    let id_path = canonical_dir.join(".1up").join("project_id");
    assert!(id_path.exists(), "start should create .1up/project_id");

    let pid_file = data_dir.join("daemon.pid");
    let deadline = Instant::now() + Duration::from_secs(2);
    while !pid_file.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }

    let status = cmd()
        .env("HOME", &canonical_home)
        .env("XDG_DATA_HOME", canonical_home.join(".local").join("share"))
        .env("XDG_CONFIG_HOME", canonical_home.join(".config"))
        .args([
            "--format",
            "json",
            "status",
            canonical_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(status.status.success());
    let status_stdout = String::from_utf8(status.stdout).unwrap();
    let status_payload: serde_json::Value = serde_json::from_str(status_stdout.trim()).unwrap();
    assert!(status_payload["indexed_files"].as_u64().unwrap() > 0);
    assert!(status_payload["total_segments"].as_u64().unwrap() > 0);
    assert!(status_payload["last_file_check_at"].as_str().is_some());

    if pid_file.exists() {
        cmd()
            .env("HOME", &canonical_home)
            .env("XDG_DATA_HOME", canonical_home.join(".local").join("share"))
            .env("XDG_CONFIG_HOME", canonical_home.join(".config"))
            .args(["--format", "json", "stop", canonical_dir.to_str().unwrap()])
            .assert()
            .success();
    }
}

#[test]
fn json_output_is_valid_json() {
    let dir = tempfile::tempdir().unwrap();
    let output = cmd()
        .args(["--format", "json", "init", dir.path().to_str().unwrap()])
        .output()
        .unwrap();

    let stdout = String::from_utf8(output.stdout).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert!(parsed.get("message").is_some());
}

#[test]
fn verbose_flag_accepted() {
    cmd().args(["-vv", "--help"]).assert().success();
}

#[test]
fn search_without_index_requires_reindex() {
    let dir = tempfile::tempdir().unwrap();

    cmd()
        .args([
            "--format",
            "json",
            "search",
            "needle",
            "--path",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("1up reindex"));
}

#[test]
fn status_human_output_includes_last_index_progress() {
    let _guard = HideModelGuard::new();
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("main.rs"),
        "fn main() {\n    println!(\"hi\");\n}\n",
    )
    .unwrap();

    cmd()
        .args(["--format", "json", "init", dir.path().to_str().unwrap()])
        .assert()
        .success();

    cmd()
        .args(["--format", "json", "index", dir.path().to_str().unwrap()])
        .assert()
        .success();

    cmd()
        .args(["--format", "human", "status", dir.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("Index status:")
                .and(predicate::str::contains("Index phase:"))
                .and(predicate::str::contains("Last index:")),
        );
}

#[test]
fn hello_agent_plain_output_contains_key_commands() {
    cmd().args(["hello-agent"]).assert().success().stdout(
        predicate::str::contains("1up search")
            .and(predicate::str::contains("1up symbol"))
            .and(predicate::str::contains("1up context"))
            .and(predicate::str::contains("1up structural")),
    );
}

#[test]
fn hello_agent_json_output_is_valid_json() {
    let output = cmd()
        .args(["--format", "json", "hello-agent"])
        .output()
        .unwrap();
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert!(parsed["instruction"]
        .as_str()
        .unwrap()
        .contains("1up search"));
}

#[test]
fn hello_agent_human_output_has_header() {
    cmd()
        .args(["--format", "human", "hello-agent"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("1up Agent Instructions")
                .and(predicate::str::contains("1up search")),
        );
}

#[cfg(not(unix))]
#[test]
fn start_reports_local_mode_guidance_on_non_unix_platforms() {
    let dir = tempfile::tempdir().unwrap();

    cmd()
        .args(["start", dir.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Background daemon workflows are not supported",
        ));
}

#[cfg(not(unix))]
#[test]
fn stop_is_a_safe_noop_on_non_unix_platforms() {
    let dir = tempfile::tempdir().unwrap();

    cmd()
        .args(["stop", dir.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("no local daemon to stop"));
}
