use assert_cmd::prelude::CommandCargoExt;
use assert_cmd::Command;
use oneup::shared::constants::{SCHEMA_VERSION, VERSION};
use oneup::storage::{
    db::Db,
    queries, schema,
    segments::{self, IndexedFileMeta, SegmentInsert},
};
use predicates::prelude::*;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Output, Stdio};
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

fn cmd_with_home(home: &Path) -> Command {
    let mut command = cmd();
    command
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join(".local").join("share"))
        .env("XDG_CONFIG_HOME", home.join(".config"));
    command
}

#[cfg(unix)]
fn std_cmd_with_home(home: &Path) -> StdCommand {
    let mut command = StdCommand::cargo_bin("1up").unwrap();
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

fn json_stdout(output: &std::process::Output) -> serde_json::Value {
    let stdout = String::from_utf8(output.stdout.clone()).unwrap();
    serde_json::from_str(stdout.trim()).unwrap_or_else(|err| {
        panic!(
            "expected JSON stdout ({err}); stdout={stdout} stderr={}",
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn text_stdout(output: &std::process::Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn assert_no_ansi(output: &str) {
    assert!(
        !output.contains('\u{1b}'),
        "plain output should not contain ANSI styling: {output:?}"
    );
}

fn assert_plain_field_order(line: &str, fields: &[&str]) {
    let mut cursor = 0;
    for field in fields {
        let remaining = &line[cursor..];
        let offset = remaining
            .find(field)
            .unwrap_or_else(|| panic!("missing field {field:?} in line {line:?}"));
        cursor += offset + field.len();
    }
}

fn status_json(home: &Path, project: &Path) -> serde_json::Value {
    let output = cmd_with_home(home)
        .args(["status", project.to_str().unwrap(), "--format", "json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "status should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    json_stdout(&output)
}

fn wait_for_daemon_running(home: &Path, project: &Path) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let payload = status_json(home, project);
        if payload["daemon_running"].as_bool() == Some(true) {
            return payload;
        }
        if Instant::now() >= deadline {
            panic!("daemon did not become running; last status={payload}");
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn stop_daemon(home: &Path, project: &Path) {
    let _ = cmd_with_home(home)
        .args(["stop", project.to_str().unwrap(), "--format", "json"])
        .output();
}

#[cfg(unix)]
fn run_start_json(home: &Path, project: &Path) -> Output {
    let output_dir = tempfile::tempdir().unwrap();
    let stdout_path = output_dir.path().join("stdout.json");
    let stderr_path = output_dir.path().join("stderr.txt");
    let stdout_file = fs::File::create(&stdout_path).unwrap();
    let stderr_file = fs::File::create(&stderr_path).unwrap();
    let status = std_cmd_with_home(home)
        .args(["start", project.to_str().unwrap(), "--format", "json"])
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file))
        .status()
        .unwrap();

    Output {
        status,
        stdout: fs::read(stdout_path).unwrap(),
        stderr: fs::read(stderr_path).unwrap(),
    }
}

#[cfg(unix)]
fn git(repo: &Path, args: &[&str]) {
    let output = StdCommand::new("git")
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

struct DaemonCleanupGuard {
    home: PathBuf,
    project: PathBuf,
}

impl DaemonCleanupGuard {
    fn new(home: &Path, project: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
            project: project.to_path_buf(),
        }
    }
}

impl Drop for DaemonCleanupGuard {
    fn drop(&mut self) {
        stop_daemon(&self.home, &self.project);
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
fn top_level_help_shows_only_p1_lifecycle_commands() {
    let output = cmd().arg("--help").output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();

    for visible in ["start", "status", "list", "stop"] {
        assert!(
            stdout
                .lines()
                .any(|line| line.trim_start().starts_with(visible)),
            "expected top-level help to show {visible}; help was:\n{stdout}"
        );
    }

    for hidden in [
        "add-mcp",
        "init",
        "symbol",
        "search",
        "get",
        "context",
        "impact",
        "structural",
        "mcp",
        "index",
        "reindex",
        "update",
        "__worker",
        "hello-agent",
        "1up",
    ] {
        assert!(
            !stdout
                .lines()
                .any(|line| line.trim_start().starts_with(hidden)),
            "expected top-level help to hide {hidden}; help was:\n{stdout}"
        );
    }

    for unsupported in ["add-mcp", "Homebrew", "Scoop", "hello-agent"] {
        assert!(
            !stdout.contains(unsupported),
            "top-level help should not advertise {unsupported}; help was:\n{stdout}"
        );
    }
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
    for sub in &["start", "stop", "status", "list"] {
        cmd()
            .args([sub, "--help"])
            .assert()
            .success()
            .stdout(predicate::str::contains("Usage:"));
    }
}

#[test]
fn hello_agent_subcommand_is_removed() {
    cmd()
        .arg("hello-agent")
        .assert()
        .failure()
        .stderr(predicate::str::contains("unrecognized subcommand"));
}

#[test]
fn indexing_subcommands_expose_parallel_controls() {
    for sub in &["index", "reindex"] {
        cmd().args([sub, "--help"]).assert().success().stdout(
            predicate::str::contains("--jobs")
                .and(predicate::str::contains("--embed-threads"))
                .and(predicate::str::contains("--watch")),
        );
    }

    cmd().args(["start", "--help"]).assert().success().stdout(
        predicate::str::contains("--jobs").and(predicate::str::contains("--embed-threads")),
    );
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
    // `--format`/`-f` lives on each maintenance Args struct post-T7, so it is
    // parsed after the subcommand rather than before it.
    for fmt in &["json", "human", "plain"] {
        cmd()
            .args(["status", "--format", fmt, "--help"])
            .assert()
            .success();
    }
}

#[test]
fn help_describes_maintenance_format_flag() {
    // Global `--help` no longer advertises `--format` because the flag moved
    // onto maintenance command Args structs. Per-command help still documents
    // it (exercised by the `_maintenance_command_help_documents_format` test
    // below).
    cmd()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Output format override").not());
}

#[test]
fn maintenance_command_help_documents_format_flag() {
    for sub in &["init", "index", "reindex", "update"] {
        cmd()
            .args([sub, "--help"])
            .assert()
            .success()
            .stdout(predicate::str::contains("Output format override"));
    }
}

#[test]
fn lifecycle_command_help_documents_plain_and_hides_removed_surfaces() {
    for sub in &["start", "status", "list", "stop"] {
        cmd().args([sub, "--help"]).assert().success().stdout(
            predicate::str::contains("--plain")
                .and(predicate::str::contains("--format").not())
                .and(predicate::str::contains("Output format override").not())
                .and(predicate::str::contains("add-mcp").not())
                .and(predicate::str::contains("Homebrew").not())
                .and(predicate::str::contains("Scoop").not())
                .and(predicate::str::contains("hello-agent").not()),
        );
    }
}

#[test]
fn status_defaults_to_human_output() {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let canonical_home = home.path().canonicalize().unwrap();

    cmd_with_home(&canonical_home)
        .args(["status", dir.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("Lifecycle:")
                .and(predicate::str::contains("Registered:"))
                .and(predicate::str::contains("Daemon:"))
                .and(predicate::str::contains("Project root:"))
                .and(predicate::str::contains("Source root:"))
                .and(predicate::str::contains("Index:"))
                .and(predicate::str::contains("daemon:").not()),
        );
}

#[test]
fn status_reports_uninitialized_project_and_missing_index() {
    let dir = tempfile::tempdir().unwrap();
    let output = cmd()
        .args(["status", dir.path().to_str().unwrap(), "--format", "json"])
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
fn update_status_reports_disabled_when_manifest_is_unconfigured() {
    let output = cmd()
        .env("ONEUP_UPDATE_MANIFEST_URL", "")
        .args(["update", "--status", "--format", "human"])
        .output()
        .unwrap();

    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Updates are disabled for this build."));
}

#[test]
fn update_status_ignores_cache_from_different_binary_version() {
    let home = tempfile::tempdir().unwrap();
    let data_dir = test_data_dir(home.path());
    fs::create_dir_all(&data_dir).unwrap();

    fs::write(
        data_dir.join("update-check.json"),
        r#"{
  "current_version": "0.0.1",
  "latest_version": "99.0.0",
  "checked_at": "2026-04-10T10:27:24Z",
  "install_channel": "manual",
  "yanked": false,
  "upgrade_instruction": "1up update"
}"#,
    )
    .unwrap();

    let output = cmd()
        .env("HOME", home.path())
        .env_remove("XDG_DATA_HOME")
        .env(
            "ONEUP_UPDATE_MANIFEST_URL",
            "https://example.com/update-manifest.json",
        )
        .args(["update", "--status", "--format", "human"])
        .output()
        .unwrap();

    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("No cached update information."));
    assert!(!stdout.contains("Latest version:"));
}

#[test]
fn update_status_formats_cached_update_in_all_maintenance_formats() {
    let home = tempfile::tempdir().unwrap();
    let canonical_home = home.path().canonicalize().unwrap();
    let data_dir = test_data_dir(&canonical_home);
    fs::create_dir_all(&data_dir).unwrap();
    fs::write(
        data_dir.join("update-check.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "current_version": VERSION,
            "latest_version": "99.0.0",
            "checked_at": "2026-04-10T10:27:24Z",
            "install_channel": "manual",
            "yanked": false,
            "message": "New lifecycle release available",
            "notes_url": "https://example.com/notes",
            "upgrade_instruction": "1up update"
        }))
        .unwrap(),
    )
    .unwrap();

    let json_output = cmd_with_home(&canonical_home)
        .env(
            "ONEUP_UPDATE_MANIFEST_URL",
            "https://example.com/update-manifest.json",
        )
        .args(["update", "--status", "--format", "json"])
        .output()
        .unwrap();
    assert!(json_output.status.success());
    let json = json_stdout(&json_output);
    assert_eq!(json["current_version"], VERSION);
    assert_eq!(json["latest_version"], "99.0.0");
    assert_eq!(json["update_available"], true);
    assert_eq!(json["install_channel"], "manual");
    assert_eq!(json["message"], "New lifecycle release available");
    assert_eq!(json["upgrade_instruction"], "1up update");
    assert_eq!(json["cached"], true);
    assert!(json["cache_age_secs"].as_i64().is_some());

    let human_output = cmd_with_home(&canonical_home)
        .env(
            "ONEUP_UPDATE_MANIFEST_URL",
            "https://example.com/update-manifest.json",
        )
        .args(["update", "--status", "--format", "human"])
        .output()
        .unwrap();
    assert!(human_output.status.success());
    let human = text_stdout(&human_output);
    assert!(human.contains("Current version:"));
    assert!(human.contains("Latest version:"));
    assert!(human.contains("Install source:"));
    assert!(human.contains("Last checked:"));
    assert!(human.contains("update available (99.0.0)"));
    assert!(human.contains("Run: 1up update"));

    let plain_output = cmd_with_home(&canonical_home)
        .env(
            "ONEUP_UPDATE_MANIFEST_URL",
            "https://example.com/update-manifest.json",
        )
        .args(["update", "--status", "--format", "plain"])
        .output()
        .unwrap();
    assert!(plain_output.status.success());
    let plain = text_stdout(&plain_output);
    assert_plain_field_order(
        plain.lines().next().unwrap(),
        &[
            "current:",
            "latest:99.0.0",
            "status:update_available",
            "update_available:true",
            "channel:manual",
            "instruction:1up update",
            "checked_at:",
            "cache_age_secs:",
            "message:New lifecycle release available",
        ],
    );
    assert_no_ansi(&plain);
}

#[test]
fn update_status_human_reports_yanked_and_minimum_safe_cache_states() {
    let home = tempfile::tempdir().unwrap();
    let canonical_home = home.path().canonicalize().unwrap();
    let data_dir = test_data_dir(&canonical_home);
    fs::create_dir_all(&data_dir).unwrap();
    let update_env = "https://example.com/update-manifest.json";

    fs::write(
        data_dir.join("update-check.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "current_version": VERSION,
            "latest_version": "99.0.0",
            "checked_at": "2026-04-10T10:27:24Z",
            "install_channel": "manual",
            "yanked": true,
            "message": "Current version was recalled",
            "upgrade_instruction": "1up update"
        }))
        .unwrap(),
    )
    .unwrap();
    let yanked_output = cmd_with_home(&canonical_home)
        .env("ONEUP_UPDATE_MANIFEST_URL", update_env)
        .args(["update", "--status", "--format", "human"])
        .output()
        .unwrap();
    assert!(yanked_output.status.success());
    let yanked = text_stdout(&yanked_output);
    assert!(yanked.contains("YANKED"));
    assert!(yanked.contains("Current version was recalled"));
    assert!(yanked.contains("Run: 1up update"));

    fs::write(
        data_dir.join("update-check.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "current_version": VERSION,
            "latest_version": "99.0.0",
            "checked_at": "2026-04-10T10:27:24Z",
            "install_channel": "manual",
            "yanked": false,
            "minimum_safe_version": "99.0.0",
            "message": "Minimum safe version changed",
            "upgrade_instruction": "1up update"
        }))
        .unwrap(),
    )
    .unwrap();
    let minimum_output = cmd_with_home(&canonical_home)
        .env("ONEUP_UPDATE_MANIFEST_URL", update_env)
        .args(["update", "--status", "--format", "human"])
        .output()
        .unwrap();
    assert!(minimum_output.status.success());
    let minimum = text_stdout(&minimum_output);
    assert!(minimum.contains("below minimum safe version (99.0.0)"));
    assert!(minimum.contains("Minimum safe version changed"));
    assert!(minimum.contains("Run: 1up update"));
}

#[test]
fn update_check_fails_when_manifest_is_unconfigured() {
    cmd()
        .env("ONEUP_UPDATE_MANIFEST_URL", "")
        .args(["update", "--check"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "Updates are disabled for this build.",
        ));
}

#[test]
fn invalid_format_flag_rejected() {
    // `--format` is rejected on core commands (like `search`) entirely, so we
    // verify the maintenance-command path still surfaces the value-parser
    // error for unknown formats.
    cmd()
        .args(["status", "--format", "xml"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown output format"));
}

#[test]
fn init_creates_project_id() {
    let dir = tempfile::tempdir().unwrap();
    cmd()
        .args(["init", dir.path().to_str().unwrap(), "--format", "json"])
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
        .args(["init", dir.path().to_str().unwrap(), "--format", "json"])
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
    fs::create_dir_all(canonical_dir.join(".git")).unwrap();
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
        .args(["start", canonical_dir.to_str().unwrap(), "--format", "json"])
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
    assert!(
        !canonical_dir.join("AGENTS.md").exists(),
        "start must not install legacy AGENTS fence guidance"
    );
    assert!(
        !canonical_dir.join("CLAUDE.md").exists(),
        "start must not install legacy CLAUDE fence guidance"
    );

    let pid_file = data_dir.join("daemon.pid");
    let deadline = Instant::now() + Duration::from_secs(2);
    while !pid_file.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }

    let status_deadline = Instant::now() + Duration::from_secs(2);
    let status_payload = loop {
        let status = cmd()
            .env("HOME", &canonical_home)
            .env("XDG_DATA_HOME", canonical_home.join(".local").join("share"))
            .env("XDG_CONFIG_HOME", canonical_home.join(".config"))
            .args([
                "status",
                canonical_dir.to_str().unwrap(),
                "--format",
                "json",
            ])
            .output()
            .unwrap();
        assert!(status.status.success());
        let status_stdout = String::from_utf8(status.stdout).unwrap();
        let status_payload: serde_json::Value = serde_json::from_str(status_stdout.trim()).unwrap();
        if status_payload["last_file_check_at"].as_str().is_some()
            || Instant::now() >= status_deadline
        {
            break status_payload;
        }
        thread::sleep(Duration::from_millis(50));
    };
    assert!(status_payload["indexed_files"].as_u64().unwrap() > 0);
    assert!(status_payload["total_segments"].as_u64().unwrap() > 0);
    assert!(status_payload["last_file_check_at"].as_str().is_some());

    if pid_file.exists() {
        cmd()
            .env("HOME", &canonical_home)
            .env("XDG_DATA_HOME", canonical_home.join(".local").join("share"))
            .env("XDG_CONFIG_HOME", canonical_home.join(".config"))
            .args(["stop", canonical_dir.to_str().unwrap(), "--format", "json"])
            .assert()
            .success();
    }
}

#[cfg(unix)]
#[test]
fn lifecycle_plain_flow_covers_start_status_list_and_stop() {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let canonical_dir = dir.path().canonicalize().unwrap();
    let canonical_home = home.path().canonicalize().unwrap();
    fs::create_dir_all(canonical_dir.join(".git")).unwrap();
    seed_model_download_failure(&canonical_home);

    let source_file = canonical_dir.join("main.rs");
    fs::write(
        &source_file,
        "fn main() {\n    println!(\"plain lifecycle\");\n}\n",
    )
    .unwrap();
    let source_before = fs::read(&source_file).unwrap();

    let _cleanup = DaemonCleanupGuard::new(&canonical_home, &canonical_dir);

    let empty_list = cmd_with_home(&canonical_home)
        .args(["list", "--plain"])
        .output()
        .unwrap();
    assert!(empty_list.status.success());
    let empty_list_stdout = text_stdout(&empty_list);
    assert_eq!(empty_list_stdout.trim_end(), "projects:0");
    assert_no_ansi(&empty_list_stdout);

    let before_status = cmd_with_home(&canonical_home)
        .args(["status", canonical_dir.to_str().unwrap(), "--plain"])
        .output()
        .unwrap();
    assert!(before_status.status.success());
    let before_status_stdout = text_stdout(&before_status);
    assert_plain_field_order(
        before_status_stdout.lines().next().unwrap(),
        &[
            "lifecycle:not_started",
            "registered:false",
            "daemon:stopped",
            "pid:none",
            "project_initialized:false",
            "project_id:none",
            "project_root:",
            "source_root:",
            "index:not_built",
            "last_file_check:none",
        ],
    );
    assert_no_ansi(&before_status_stdout);

    let start = cmd_with_home(&canonical_home)
        .args(["start", canonical_dir.to_str().unwrap(), "--plain"])
        .output()
        .unwrap();
    assert!(
        start.status.success(),
        "start --plain should succeed: {}",
        String::from_utf8_lossy(&start.stderr)
    );
    let start_stdout = text_stdout(&start);
    let start_line = start_stdout.lines().next().unwrap();
    assert_plain_field_order(
        start_line,
        &[
            "status:",
            "project_id:",
            "project_root:",
            "source_root:",
            "registered:true",
            "index:ready",
            "pid:",
            "message:",
        ],
    );
    assert!(start_line.contains("Indexed") || start_line.contains("Project registered"));
    assert_no_ansi(&start_stdout);

    let project_id_path = canonical_dir.join(".1up").join("project_id");
    let index_path = canonical_dir.join(".1up").join("index.db");
    assert!(
        project_id_path.exists(),
        "start should create .1up/project_id"
    );
    assert!(index_path.exists(), "start should create .1up/index.db");
    let project_id = fs::read_to_string(&project_id_path).unwrap();
    let project_id = project_id.trim();
    assert!(!project_id.is_empty());

    let started_status = cmd_with_home(&canonical_home)
        .args(["status", canonical_dir.to_str().unwrap(), "--plain"])
        .output()
        .unwrap();
    assert!(started_status.status.success());
    let started_status_stdout = text_stdout(&started_status);
    let started_status_line = started_status_stdout.lines().next().unwrap();
    assert!(
        started_status_line.starts_with("lifecycle:active")
            || started_status_line.starts_with("lifecycle:registered")
            || started_status_line.starts_with("lifecycle:indexing"),
        "started lifecycle should be active, registered, or indexing; got {started_status_line:?}"
    );
    assert!(started_status_line.contains("\tregistered:true\t"));
    assert!(started_status_line.contains("\tproject_initialized:true\t"));
    assert!(started_status_line.contains(&format!("\tproject_id:{project_id}\t")));
    assert!(started_status_line.contains("\tindex:ready\t"));
    assert_no_ansi(&started_status_stdout);

    let registered_list = cmd_with_home(&canonical_home)
        .args(["list", "--plain"])
        .output()
        .unwrap();
    assert!(registered_list.status.success());
    let registered_list_stdout = text_stdout(&registered_list);
    let registered_line = registered_list_stdout.lines().next().unwrap();
    assert_plain_field_order(
        registered_line,
        &[
            "project:",
            "state:",
            "project_root:",
            "source_root:",
            "index:ready",
            "files:",
            "segments:",
            "last_file_check:",
            "registered_at:",
        ],
    );
    assert!(registered_line.starts_with(&format!("project:{project_id}\t")));
    assert!(registered_line.contains(&format!("\tproject_root:{}\t", canonical_dir.display())));
    assert_no_ansi(&registered_list_stdout);

    let stop = cmd_with_home(&canonical_home)
        .args(["stop", canonical_dir.to_str().unwrap(), "--plain"])
        .output()
        .unwrap();
    assert!(
        stop.status.success(),
        "stop --plain should succeed: {}",
        String::from_utf8_lossy(&stop.stderr)
    );
    let stop_stdout = text_stdout(&stop);
    let stop_line = stop_stdout.lines().next().unwrap();
    assert_plain_field_order(
        stop_line,
        &[
            "status:",
            "project_root:",
            "registered:false",
            "daemon:",
            "pid:",
            "message:",
        ],
    );
    assert!(
        stop_line.starts_with("status:stopped")
            || stop_line.starts_with("status:daemon_not_running")
    );
    assert_no_ansi(&stop_stdout);

    let after_status = cmd_with_home(&canonical_home)
        .args(["status", canonical_dir.to_str().unwrap(), "--plain"])
        .output()
        .unwrap();
    assert!(after_status.status.success());
    let after_status_stdout = text_stdout(&after_status);
    let after_status_line = after_status_stdout.lines().next().unwrap();
    assert!(after_status_line.starts_with("lifecycle:stopped"));
    assert!(after_status_line.contains("\tregistered:false\t"));
    assert_no_ansi(&after_status_stdout);

    let after_list = cmd_with_home(&canonical_home)
        .args(["list", "--plain"])
        .output()
        .unwrap();
    assert!(after_list.status.success());
    let after_list_stdout = text_stdout(&after_list);
    assert_eq!(after_list_stdout.trim_end(), "projects:0");
    assert_no_ansi(&after_list_stdout);

    assert_eq!(
        source_before,
        fs::read(&source_file).unwrap(),
        "stop must not modify source files"
    );
}

#[cfg(unix)]
#[test]
fn start_from_worktree_uses_main_state_and_indexes_worktree_source() {
    let _guard = HideModelGuard::new();
    let tmp = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let tmp_root = tmp.path().canonicalize().unwrap();
    let canonical_home = home.path().canonicalize().unwrap();
    seed_model_download_failure(&canonical_home);

    let main_repo = tmp_root.join("main");
    fs::create_dir_all(&main_repo).unwrap();
    StdCommand::new("git")
        .args(["init", main_repo.to_str().unwrap()])
        .output()
        .expect("git init failed");
    StdCommand::new("git")
        .args(["config", "user.email", "oneup-test@example.com"])
        .current_dir(&main_repo)
        .output()
        .expect("git config user.email failed");
    StdCommand::new("git")
        .args(["config", "user.name", "1up Test"])
        .current_dir(&main_repo)
        .output()
        .expect("git config user.name failed");

    fs::write(
        main_repo.join("main_only.rs"),
        "fn main_only() -> bool { true }\n",
    )
    .unwrap();
    StdCommand::new("git")
        .args(["add", "."])
        .current_dir(&main_repo)
        .output()
        .expect("git add failed");
    let commit = StdCommand::new("git")
        .args(["commit", "-m", "initial"])
        .current_dir(&main_repo)
        .output()
        .expect("git commit failed");
    assert!(
        commit.status.success(),
        "git commit failed: {}",
        String::from_utf8_lossy(&commit.stderr)
    );

    let worktree = tmp_root.join("feature-worktree");
    let add_worktree = StdCommand::new("git")
        .args([
            "worktree",
            "add",
            worktree.to_str().unwrap(),
            "-b",
            "feature-branch",
        ])
        .current_dir(&main_repo)
        .output()
        .expect("git worktree add failed");
    assert!(
        add_worktree.status.success(),
        "git worktree add failed: {}",
        String::from_utf8_lossy(&add_worktree.stderr)
    );
    let canonical_main = main_repo.canonicalize().unwrap();
    let canonical_worktree = worktree.canonicalize().unwrap();
    let _cleanup = DaemonCleanupGuard::new(&canonical_home, &canonical_worktree);

    fs::write(
        canonical_worktree.join("worktree_only.rs"),
        "fn worktree_start_marker() -> &'static str { \"worktree start marker\" }\n",
    )
    .unwrap();

    let output = cmd_with_home(&canonical_home)
        .args([
            "start",
            canonical_worktree.to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "start from worktree should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let payload = json_stdout(&output);
    assert!(payload["progress"]["files_indexed"].as_u64().unwrap() > 0);

    assert!(
        canonical_main.join(".1up").join("project_id").exists(),
        "start should create project id under main worktree state root"
    );
    assert!(
        !canonical_worktree.join(".1up").join("project_id").exists(),
        "start must not create a worktree-local project id"
    );

    let registry_path = test_data_dir(&canonical_home).join("projects.json");
    let registry: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&registry_path).unwrap()).unwrap();
    let projects = registry["projects"].as_array().unwrap();
    assert_eq!(projects.len(), 1);
    assert_eq!(
        projects[0]["project_root"].as_str(),
        Some(canonical_main.to_str().unwrap())
    );
    assert_eq!(
        projects[0]["source_root"].as_str(),
        Some(canonical_worktree.to_str().unwrap())
    );

    let search = cmd_with_home(&canonical_home)
        .args([
            "search",
            "worktree_start_marker",
            "--path",
            canonical_worktree.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        search.status.success(),
        "search from worktree should succeed: {}",
        String::from_utf8_lossy(&search.stderr)
    );
    let search_stdout = String::from_utf8(search.stdout).unwrap();
    assert!(
        search_stdout.contains("worktree_only.rs"),
        "worktree-only file should be indexed from start; stdout={search_stdout}"
    );
}

#[cfg(unix)]
#[test]
fn list_and_status_show_linked_worktree_context_sharing_main_state() {
    let _guard = HideModelGuard::new();
    let tmp = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let tmp_root = tmp.path().canonicalize().unwrap();
    let canonical_home = home.path().canonicalize().unwrap();
    seed_model_download_failure(&canonical_home);

    let main_repo = tmp_root.join("main");
    let worktree = tmp_root.join("linked-worktree");
    fs::create_dir_all(&main_repo).unwrap();
    git(&tmp_root, &["init", main_repo.to_str().unwrap()]);
    git(
        &main_repo,
        &["config", "user.email", "oneup-test@example.com"],
    );
    git(&main_repo, &["config", "user.name", "1up Test"]);
    fs::write(
        main_repo.join("shared.rs"),
        "pub fn shared_worktree_metadata_marker() -> bool { true }\n",
    )
    .unwrap();
    git(&main_repo, &["add", "."]);
    git(&main_repo, &["commit", "-m", "initial"]);
    git(&main_repo, &["branch", "-M", "main"]);
    git(
        &main_repo,
        &[
            "worktree",
            "add",
            worktree.to_str().unwrap(),
            "-b",
            "linked-acceptance",
        ],
    );

    let canonical_main = main_repo.canonicalize().unwrap();
    let canonical_worktree = worktree.canonicalize().unwrap();
    let _cleanup = DaemonCleanupGuard::new(&canonical_home, &canonical_worktree);

    let output = run_start_json(&canonical_home, &canonical_worktree);
    assert!(
        output.status.success(),
        "start from linked worktree should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let status = status_json(&canonical_home, &canonical_worktree);
    assert_eq!(
        status["project_root"].as_str(),
        Some(canonical_main.to_str().unwrap())
    );
    assert_eq!(
        status["source_root"].as_str(),
        Some(canonical_worktree.to_str().unwrap())
    );
    assert_eq!(
        status["main_worktree_root"].as_str(),
        Some(canonical_main.to_str().unwrap())
    );
    assert_eq!(status["worktree_role"], "linked");
    assert_eq!(status["branch_name"], "linked-acceptance");
    assert_eq!(status["branch_status"], "named");

    let list_output = cmd_with_home(&canonical_home)
        .args(["list", "--format", "json"])
        .output()
        .unwrap();
    assert!(
        list_output.status.success(),
        "list should succeed: {}",
        String::from_utf8_lossy(&list_output.stderr)
    );
    let list = json_stdout(&list_output);
    let projects = list["projects"].as_array().unwrap();
    assert_eq!(projects.len(), 1);
    assert_eq!(
        projects[0]["project_root"].as_str(),
        Some(canonical_main.to_str().unwrap())
    );
    assert_eq!(
        projects[0]["source_root"].as_str(),
        Some(canonical_worktree.to_str().unwrap())
    );
    assert_eq!(
        projects[0]["main_worktree_root"].as_str(),
        Some(canonical_main.to_str().unwrap())
    );
    assert_eq!(projects[0]["worktree_role"], "linked");
    assert_eq!(projects[0]["branch_name"], "linked-acceptance");
    assert_eq!(projects[0]["branch_status"], "named");

    assert!(canonical_main.join(".1up").join("project_id").exists());
    assert!(!canonical_worktree.join(".1up").join("project_id").exists());
}

#[test]
fn start_refuses_to_auto_initialize_non_git_directory() {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let canonical_dir = dir.path().canonicalize().unwrap();
    let canonical_home = home.path().canonicalize().unwrap();

    cmd_with_home(&canonical_home)
        .args(["start", canonical_dir.to_str().unwrap(), "--format", "json"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not a git root"));

    assert!(
        !canonical_dir.join(".1up").exists(),
        "start must not create project state outside a git root"
    );
}

#[test]
fn index_refuses_to_auto_initialize_non_git_directory() {
    let dir = tempfile::tempdir().unwrap();
    let canonical_dir = dir.path().canonicalize().unwrap();

    cmd()
        .args(["index", canonical_dir.to_str().unwrap(), "--format", "json"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not a git root"));

    assert!(
        !canonical_dir.join(".1up").exists(),
        "index must not create project state outside a git root"
    );
}

#[test]
fn start_indexes_project_when_daemon_is_already_running_and_index_is_missing() {
    let home = tempfile::tempdir().unwrap();
    let project_a = tempfile::tempdir().unwrap();
    let project_b = tempfile::tempdir().unwrap();
    let canonical_home = home.path().canonicalize().unwrap();
    let canonical_project_a = project_a.path().canonicalize().unwrap();
    let canonical_project_b = project_b.path().canonicalize().unwrap();
    fs::create_dir_all(canonical_project_a.join(".git")).unwrap();
    fs::create_dir_all(canonical_project_b.join(".git")).unwrap();
    let data_dir = test_data_dir(&canonical_home);
    let model_dir = data_dir.join("models").join("all-MiniLM-L6-v2");
    fs::create_dir_all(&model_dir).unwrap();
    fs::write(model_dir.join(".download_failed"), "skip download in test").unwrap();
    fs::write(
        canonical_project_a.join("lib.rs"),
        "pub fn first_project() -> &'static str {\n    \"ready\"\n}\n",
    )
    .unwrap();
    fs::write(
        canonical_project_b.join("main.rs"),
        "fn main() {\n    println!(\"second project\");\n}\n",
    )
    .unwrap();

    cmd()
        .env("HOME", &canonical_home)
        .env("XDG_DATA_HOME", canonical_home.join(".local").join("share"))
        .env("XDG_CONFIG_HOME", canonical_home.join(".config"))
        .args([
            "start",
            canonical_project_a.to_str().unwrap(),
            "--format",
            "json",
        ])
        .assert()
        .success();

    let pid_file = data_dir.join("daemon.pid");
    let deadline = Instant::now() + Duration::from_secs(2);
    while !pid_file.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        pid_file.exists(),
        "daemon pid file should exist after start"
    );

    let output = cmd()
        .env("HOME", &canonical_home)
        .env("XDG_DATA_HOME", canonical_home.join(".local").join("share"))
        .env("XDG_CONFIG_HOME", canonical_home.join(".config"))
        .args([
            "start",
            canonical_project_b.to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    let payload: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    let message = payload["message"].as_str().unwrap();
    assert!(message.contains("Indexed"));
    assert!(message.contains("Daemon already running"));
    assert!(payload["progress"]["files_indexed"].as_u64().unwrap() > 0);
    assert!(payload["progress"]["segments_stored"].as_u64().unwrap() > 0);
    assert!(canonical_project_b.join(".1up").join("index.db").exists());

    cmd()
        .env("HOME", &canonical_home)
        .env("XDG_DATA_HOME", canonical_home.join(".local").join("share"))
        .env("XDG_CONFIG_HOME", canonical_home.join(".config"))
        .args([
            "stop",
            canonical_project_a.to_str().unwrap(),
            "--format",
            "json",
        ])
        .assert()
        .success();
}

#[test]
fn json_output_is_valid_json() {
    let dir = tempfile::tempdir().unwrap();
    let output = cmd()
        .args(["init", dir.path().to_str().unwrap(), "--format", "json"])
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

    // Core commands (like `search`) do not accept `--format` post-T7.
    cmd()
        .args(["search", "needle", "--path", dir.path().to_str().unwrap()])
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
        .args(["init", dir.path().to_str().unwrap(), "--format", "json"])
        .assert()
        .success();

    cmd()
        .args(["index", dir.path().to_str().unwrap(), "--format", "json"])
        .assert()
        .success();

    cmd()
        .args(["status", dir.path().to_str().unwrap(), "--format", "human"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("Index status:")
                .and(predicate::str::contains("Index phase:"))
                .and(predicate::str::contains("Processed:"))
                .and(predicate::str::contains("Last index:")),
        );
}

#[test]
fn index_json_output_includes_full_run_prefilter_counters() {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let canonical_dir = dir.path().canonicalize().unwrap();
    let canonical_home = home.path().canonicalize().unwrap();
    fs::create_dir_all(canonical_dir.join(".git")).unwrap();
    seed_model_download_failure(&canonical_home);

    fs::write(canonical_dir.join("a.rs"), "fn a() {}\n").unwrap();
    fs::write(canonical_dir.join("b.rs"), "fn b() {}\n").unwrap();

    let first = cmd_with_home(&canonical_home)
        .args(["index", canonical_dir.to_str().unwrap(), "--format", "json"])
        .output()
        .unwrap();
    assert!(
        first.status.success(),
        "initial index should succeed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first_payload = json_stdout(&first);
    assert_eq!(first_payload["progress"]["prefilter"]["discovered"], 2);
    assert_eq!(
        first_payload["progress"]["prefilter"]["metadata_skipped"],
        0
    );
    assert_eq!(first_payload["progress"]["prefilter"]["content_read"], 2);

    let second = cmd_with_home(&canonical_home)
        .args(["index", canonical_dir.to_str().unwrap(), "--format", "json"])
        .output()
        .unwrap();
    assert!(
        second.status.success(),
        "second index should succeed: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let second_payload = json_stdout(&second);

    assert_eq!(second_payload["progress"]["prefilter"]["discovered"], 2);
    assert_eq!(
        second_payload["progress"]["prefilter"]["metadata_skipped"],
        2
    );
    assert_eq!(second_payload["progress"]["prefilter"]["content_read"], 0);
    assert_eq!(second_payload["progress"]["prefilter"]["deleted"], 0);
}

#[test]
fn index_watch_plain_output_streams_progress_updates() {
    let _guard = HideModelGuard::new();
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".git")).unwrap();
    fs::write(
        dir.path().join("main.rs"),
        "fn main() {\n    println!(\"watch\");\n}\n",
    )
    .unwrap();

    cmd()
        .args([
            "index",
            "--watch",
            dir.path().to_str().unwrap(),
            "--format",
            "plain",
        ])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("event:index_progress")
                .and(predicate::str::contains("index_phase:preparing"))
                .and(predicate::str::contains("index_phase:loading_model"))
                .and(predicate::str::contains("index_phase:complete")),
        );
}

#[test]
fn index_watch_json_output_streams_progress_updates() {
    let _guard = HideModelGuard::new();
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".git")).unwrap();
    fs::write(
        dir.path().join("main.rs"),
        "fn main() {\n    println!(\"watch\");\n}\n",
    )
    .unwrap();

    cmd()
        .args([
            "index",
            "--watch",
            dir.path().to_str().unwrap(),
            "--format",
            "json",
        ])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("\"event\":\"index_progress\"")
                .and(predicate::str::contains("\"phase\":\"preparing\""))
                .and(predicate::str::contains("\"phase\":\"loading_model\""))
                .and(predicate::str::contains("\"phase\":\"complete\"")),
        );
}

#[test]
fn index_watch_human_output_keeps_progress_off_stdout() {
    let _guard = HideModelGuard::new();
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".git")).unwrap();
    fs::write(
        dir.path().join("main.rs"),
        "fn main() {\n    println!(\"watch\");\n}\n",
    )
    .unwrap();

    cmd()
        .args([
            "index",
            "--watch",
            dir.path().to_str().unwrap(),
            "--format",
            "human",
        ])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("Indexed 1 files")
                .and(predicate::str::contains("event:index_progress").not()),
        );
}

#[test]
fn reindex_watch_plain_output_streams_rebuild_and_completion() {
    let _guard = HideModelGuard::new();
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".git")).unwrap();
    fs::write(
        dir.path().join("lib.rs"),
        "pub fn watch_mode() -> &'static str {\n    \"ready\"\n}\n",
    )
    .unwrap();

    cmd()
        .args([
            "reindex",
            "--watch",
            dir.path().to_str().unwrap(),
            "--format",
            "plain",
        ])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("event:index_progress")
                .and(predicate::str::contains("index_phase:rebuilding"))
                .and(predicate::str::contains("index_phase:complete")),
        );
}

#[cfg(not(unix))]
#[test]
fn start_reports_local_mode_guidance_on_non_unix_platforms() {
    let dir = tempfile::tempdir().unwrap();

    let output = cmd()
        .args(["start", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = text_stdout(&output);
    assert!(stdout.contains("Background daemon workflows are not supported"));
    for retained in ["1up start", "1up status", "1up list", "1up stop"] {
        assert!(
            stdout.contains(retained),
            "unsupported daemon guidance should mention retained command {retained}; stdout={stdout}"
        );
    }
    for hidden in ["1up init", "1up index", "1up reindex", "1up add-mcp"] {
        assert!(
            !stdout.contains(hidden),
            "unsupported daemon guidance must not mention hidden command {hidden}; stdout={stdout}"
        );
    }
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

// ---------------------------------------------------------------------------
// `1up start` UX guarantees introduced by the shell-install feature
// (REQ-031 silent-pass on valid index, REQ-032 `1up status` hint on fresh
// index, REQ-033 stale-schema warning + non-zero exit, BR-06 no in-place
// migration). See features/update-script/design.md §3.2.
// ---------------------------------------------------------------------------

fn project_db_path(dir: &Path) -> PathBuf {
    dir.canonicalize()
        .unwrap_or_else(|_| dir.to_path_buf())
        .join(".1up")
        .join("index.db")
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Runtime::new().unwrap().block_on(future)
}

/// Seed a project DB with a schema version older than the running binary.
/// Matches `rewrite_sql_verification::create_stale_v4_index` so
/// `schema::ensure_current` produces the same stable "out of date" error.
fn create_stale_v4_index(dir: &Path) {
    block_on(async {
        let db = Db::open_rw(&project_db_path(dir)).await.unwrap();
        let conn = db.connect().unwrap();
        conn.execute(queries::CREATE_META_TABLE, ()).await.unwrap();
        conn.execute(queries::UPSERT_META, ["schema_version", "4"])
            .await
            .unwrap();
    });
}

fn create_newer_index(dir: &Path) {
    block_on(async {
        let db = Db::open_rw(&project_db_path(dir)).await.unwrap();
        let conn = db.connect().unwrap();
        conn.execute(queries::CREATE_META_TABLE, ()).await.unwrap();
        conn.execute(
            queries::UPSERT_META,
            ["schema_version", &(SCHEMA_VERSION + 1).to_string()],
        )
        .await
        .unwrap();
    });
}

fn create_schema_missing_index(dir: &Path) {
    block_on(async {
        let db = Db::open_rw(&project_db_path(dir)).await.unwrap();
        let conn = db.connect().unwrap();
        conn.execute("CREATE TABLE orphaned_segments(id TEXT)", ())
            .await
            .unwrap();
    });
}

/// Seed a project DB at the current schema version without running the
/// indexer. Matches `prepare_for_write`'s fresh-database branch.
fn create_current_index(dir: &Path) {
    block_on(async {
        let db = Db::open_rw(&project_db_path(dir)).await.unwrap();
        let conn = db.connect().unwrap();
        schema::initialize(&conn).await.unwrap();
    });
}

fn seed_current_index_for_context(dir: &Path, context_id: &str) {
    fs::create_dir_all(dir.join(".1up")).unwrap();
    fs::write(dir.join(".1up").join("project_id"), "context-count-project").unwrap();

    block_on(async {
        let db = Db::open_rw(&project_db_path(dir)).await.unwrap();
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

fn write_registry(home: &Path, projects: serde_json::Value) {
    let data_dir = test_data_dir(home);
    fs::create_dir_all(&data_dir).unwrap();
    fs::write(
        data_dir.join("projects.json"),
        serde_json::to_vec_pretty(&serde_json::json!({ "projects": projects })).unwrap(),
    )
    .unwrap();
}

fn write_lifecycle_progress_fixture(project: &Path, project_id: &str) {
    let dot_dir = project.join(".1up");
    fs::create_dir_all(&dot_dir).unwrap();
    fs::write(dot_dir.join("project_id"), project_id).unwrap();
    fs::write(
        dot_dir.join("index_status.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "state": "running",
            "phase": "storing",
            "files_total": 4,
            "files_scanned": 4,
            "files_processed": 3,
            "files_indexed": 2,
            "files_skipped": 1,
            "files_deleted": 1,
            "segments_stored": 9,
            "embeddings_enabled": true,
            "message": "storing lifecycle fixture",
            "parallelism": {
                "jobs_configured": 4,
                "jobs_effective": 2,
                "embed_threads": 1
            },
            "timings": {
                "scan_ms": 10,
                "parse_ms": 20,
                "embed_ms": 30,
                "store_ms": 40,
                "total_ms": 100,
                "db_prepare_ms": 5,
                "model_prepare_ms": 6,
                "input_prep_ms": 7
            },
            "scope": {
                "requested": "scoped",
                "executed": "full",
                "changed_paths": 2,
                "fallback_reason": "fixture fallback"
            },
            "prefilter": {
                "discovered": 4,
                "metadata_skipped": 1,
                "content_read": 3,
                "deleted": 1
            },
            "updated_at": "2026-05-01T00:00:00Z"
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(
        dot_dir.join("daemon_status.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "last_file_check_at": "2026-05-01T00:05:00Z"
        }))
        .unwrap(),
    )
    .unwrap();
}

#[test]
fn status_and_list_ignore_daemon_status_from_other_contexts() {
    let home = tempfile::tempdir().unwrap();
    let project_dir = tempfile::tempdir().unwrap();
    let canonical_home = home.path().canonicalize().unwrap();
    let project = project_dir.path().canonicalize().unwrap();
    let dot_dir = project.join(".1up");
    let legacy_check = "2026-05-01T00:05:00Z";
    let other_context_check = "2026-05-01T00:09:00Z";

    fs::create_dir_all(&dot_dir).unwrap();
    seed_current_index_for_context(&project, "other-context");
    fs::write(dot_dir.join("project_id"), "cross-context-project").unwrap();
    fs::write(
        dot_dir.join("daemon_status.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "last_file_check_at": legacy_check
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(
        dot_dir.join("daemon_context_status.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "contexts": {
                "other-context": {
                    "context_id": "other-context",
                    "source_root": project,
                    "watch_status": "watching",
                    "last_file_check_at": other_context_check,
                    "last_refresh_state": "complete",
                    "last_refresh_completed_at": other_context_check,
                    "branch_name": "other",
                    "branch_status": "named"
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();
    write_registry(
        &canonical_home,
        serde_json::json!([
            {
                "project_id": "cross-context-project",
                "project_root": project,
                "registered_at": "2026-05-01T00:10:00Z"
            }
        ]),
    );

    let status_output = cmd_with_home(&canonical_home)
        .args(["status", project.to_str().unwrap(), "--format", "json"])
        .output()
        .unwrap();
    assert!(status_output.status.success());
    let status_json = json_stdout(&status_output);
    assert_eq!(status_json["last_file_check_at"], legacy_check);
    assert_ne!(status_json["last_file_check_at"], other_context_check);
    assert_eq!(status_json["indexed_files"], 0);
    assert_eq!(status_json["total_segments"], 0);

    let list_output = cmd_with_home(&canonical_home)
        .args(["list", "--format", "json"])
        .output()
        .unwrap();
    assert!(list_output.status.success());
    let list_json = json_stdout(&list_output);
    assert_eq!(list_json["projects"][0]["last_file_check_at"], legacy_check);
    assert_ne!(
        list_json["projects"][0]["last_file_check_at"],
        other_context_check
    );
    assert_eq!(list_json["projects"][0]["index_status"], "not_built");
    assert_eq!(list_json["projects"][0]["files"], 0);
    assert_eq!(list_json["projects"][0]["segments"], 0);
}

#[cfg(unix)]
#[test]
fn start_warns_on_stale_schema() {
    // REQ-033 + BR-06: an existing index at a prior schema version must
    // produce a warning that names `1up reindex`, exit non-zero, and leave
    // the on-disk `.1up/index.db` byte-identical (no delete, no migrate).
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let canonical_dir = dir.path().canonicalize().unwrap();
    let canonical_home = home.path().canonicalize().unwrap();
    fs::create_dir_all(&canonical_dir).unwrap();
    create_stale_v4_index(&canonical_dir);

    let db_path = project_db_path(&canonical_dir);
    let bytes_before = fs::read(&db_path).unwrap();
    let mtime_before = fs::metadata(&db_path).unwrap().modified().unwrap();

    let output = cmd()
        .env("HOME", &canonical_home)
        .env("XDG_DATA_HOME", canonical_home.join(".local").join("share"))
        .env("XDG_CONFIG_HOME", canonical_home.join(".config"))
        .args(["start", canonical_dir.to_str().unwrap()])
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "start should exit non-zero on stale schema; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    // The human stale-schema warning must come from
    // `emit_stale_schema_warning` on stdout, not just from the trailing
    // anyhow error printed on non-zero exit. Asserting on stdout
    // independently catches a regression where the warning emitter stops
    // running but the error message alone keeps the substrings alive.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("out of date"),
        "expected stale-schema warning on stdout to mention 'out of date'; stdout={stdout} stderr={stderr}",
    );
    assert!(
        stdout.contains("1up reindex"),
        "expected stale-schema warning on stdout to name `1up reindex`; stdout={stdout} stderr={stderr}",
    );
    assert!(
        stdout.contains(&format!("expected v{SCHEMA_VERSION}")),
        "expected warning on stdout to name expected schema version v{SCHEMA_VERSION}; stdout={stdout} stderr={stderr}",
    );

    // BR-06: the on-disk index must be untouched. Content is the primary
    // gate; mtime is asserted additively to catch silent re-writes.
    let bytes_after = fs::read(&db_path).unwrap();
    assert_eq!(
        bytes_before, bytes_after,
        "stale-schema warning path must not rewrite the index db"
    );
    assert_eq!(
        mtime_before,
        fs::metadata(&db_path).unwrap().modified().unwrap(),
        "stale-schema warning path must not touch index db mtime"
    );
}

#[cfg(unix)]
#[test]
fn start_warns_on_stale_schema_json_envelope() {
    // JSON formatter variant of REQ-033: the envelope literal is fixed by
    // design §3.2 (`schema_out_of_date`, `found`, `expected`, `action`).
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let canonical_dir = dir.path().canonicalize().unwrap();
    let canonical_home = home.path().canonicalize().unwrap();
    create_stale_v4_index(&canonical_dir);

    let output = cmd()
        .env("HOME", &canonical_home)
        .env("XDG_DATA_HOME", canonical_home.join(".local").join("share"))
        .env("XDG_CONFIG_HOME", canonical_home.join(".config"))
        .args(["start", canonical_dir.to_str().unwrap(), "--format", "json"])
        .output()
        .unwrap();
    assert!(!output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    let envelope_line = stdout
        .lines()
        .find(|l| l.contains("schema_out_of_date"))
        .unwrap_or_else(|| panic!("expected schema_out_of_date line in stdout, got: {stdout}"));
    let payload: serde_json::Value = serde_json::from_str(envelope_line).unwrap();
    assert_eq!(payload["status"], "schema_out_of_date");
    assert_eq!(payload["found"], 4);
    assert_eq!(payload["expected"], SCHEMA_VERSION);
    assert_eq!(payload["action"], "1up reindex");
    assert!(payload["path"].as_str().unwrap().ends_with("index.db"));
}

#[cfg(unix)]
#[test]
fn start_reports_newer_schema_as_binary_update_action() {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let canonical_dir = dir.path().canonicalize().unwrap();
    let canonical_home = home.path().canonicalize().unwrap();
    create_newer_index(&canonical_dir);

    let json_output = cmd_with_home(&canonical_home)
        .args(["start", canonical_dir.to_str().unwrap(), "--format", "json"])
        .output()
        .unwrap();
    assert!(!json_output.status.success());
    let json_stdout = text_stdout(&json_output);
    let envelope_line = json_stdout
        .lines()
        .find(|line| line.contains("binary_out_of_date"))
        .unwrap_or_else(|| panic!("expected binary_out_of_date envelope; stdout={json_stdout}"));
    let payload: serde_json::Value = serde_json::from_str(envelope_line).unwrap();
    assert_eq!(payload["status"], "binary_out_of_date");
    assert_eq!(payload["found"], SCHEMA_VERSION + 1);
    assert_eq!(payload["expected"], SCHEMA_VERSION);
    assert_eq!(payload["action"], "1up update");

    let human_dir = tempfile::tempdir().unwrap();
    let human_project = human_dir.path().canonicalize().unwrap();
    create_newer_index(&human_project);
    let human_output = cmd_with_home(&canonical_home)
        .args(["start", human_project.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!human_output.status.success());
    let human_stdout = text_stdout(&human_output);
    assert!(human_stdout.contains("newer than this binary supports"));
    assert!(human_stdout.contains("1up update"));
}

#[cfg(unix)]
#[test]
fn start_reports_schema_missing_index_as_reindex_action() {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let canonical_dir = dir.path().canonicalize().unwrap();
    let canonical_home = home.path().canonicalize().unwrap();
    create_schema_missing_index(&canonical_dir);

    let json_output = cmd_with_home(&canonical_home)
        .args(["start", canonical_dir.to_str().unwrap(), "--format", "json"])
        .output()
        .unwrap();
    assert!(!json_output.status.success());
    let json_stdout = text_stdout(&json_output);
    let envelope_line = json_stdout
        .lines()
        .find(|line| line.contains("index_unreadable"))
        .unwrap_or_else(|| panic!("expected index_unreadable envelope; stdout={json_stdout}"));
    let payload: serde_json::Value = serde_json::from_str(envelope_line).unwrap();
    assert_eq!(payload["status"], "index_unreadable");
    assert_eq!(payload["action"], "1up reindex");

    let human_dir = tempfile::tempdir().unwrap();
    let human_project = human_dir.path().canonicalize().unwrap();
    create_schema_missing_index(&human_project);
    let human_output = cmd_with_home(&canonical_home)
        .args(["start", human_project.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!human_output.status.success());
    let human_stdout = text_stdout(&human_output);
    assert!(human_stdout.contains("unreadable"));
    assert!(human_stdout.contains("1up reindex"));
}

#[cfg(unix)]
#[test]
fn lifecycle_registry_outputs_cover_json_human_progress_and_stop_states() {
    let home = tempfile::tempdir().unwrap();
    let project_dir = tempfile::tempdir().unwrap();
    let unavailable_dir = tempfile::tempdir().unwrap();
    let canonical_home = home.path().canonicalize().unwrap();
    let project = project_dir.path().canonicalize().unwrap();
    let unavailable_project = unavailable_dir.path().canonicalize().unwrap();
    let source_root = project.join("source-root");
    fs::create_dir_all(&source_root).unwrap();

    write_lifecycle_progress_fixture(&project, "progress-project");
    fs::create_dir_all(unavailable_project.join(".1up")).unwrap();
    fs::write(
        unavailable_project.join(".1up").join("project_id"),
        "unavailable-project",
    )
    .unwrap();
    create_newer_index(&unavailable_project);
    write_registry(
        &canonical_home,
        serde_json::json!([
            {
                "project_id": "progress-project",
                "project_root": project,
                "source_root": source_root,
                "registered_at": "2026-05-01T00:10:00Z"
            },
            {
                "project_id": "unavailable-project",
                "project_root": unavailable_project,
                "registered_at": "2026-05-01T00:15:00Z"
            }
        ]),
    );

    let list_json_output = cmd_with_home(&canonical_home)
        .args(["list", "--format", "json"])
        .output()
        .unwrap();
    assert!(list_json_output.status.success());
    let list_json = json_stdout(&list_json_output);
    assert_eq!(list_json["projects"][0]["project_id"], "progress-project");
    assert_eq!(list_json["projects"][0]["state"], "indexing");
    assert_eq!(list_json["projects"][0]["index_status"], "not_built");
    assert_eq!(list_json["projects"][0]["files"], 2);
    assert_eq!(list_json["projects"][0]["segments"], 9);
    assert_eq!(
        list_json["projects"][0]["last_file_check_at"],
        "2026-05-01T00:05:00Z"
    );
    assert_eq!(list_json["projects"][1]["index_status"], "unavailable");

    let list_plain_output = cmd_with_home(&canonical_home)
        .args(["list", "--plain"])
        .output()
        .unwrap();
    assert!(list_plain_output.status.success());
    let list_plain = text_stdout(&list_plain_output);
    assert!(list_plain.contains("project:progress-project\tstate:indexing"));
    assert!(list_plain.contains("\tindex:not_built\tfiles:2\tsegments:9\t"));
    assert!(list_plain.contains("project:unavailable-project\tstate:registered"));
    assert!(list_plain.contains("\tindex:unavailable\tfiles:unknown\tsegments:unknown\t"));
    assert_no_ansi(&list_plain);

    let list_human_output = cmd_with_home(&canonical_home)
        .args(["list"])
        .output()
        .unwrap();
    assert!(list_human_output.status.success());
    let list_human = text_stdout(&list_human_output);
    assert!(list_human.contains("Registered projects"));
    assert!(list_human.contains("progress-project"));
    assert!(list_human.contains("unavailable-project"));

    let status_json_output = cmd_with_home(&canonical_home)
        .args(["status", project.to_str().unwrap(), "--format", "json"])
        .output()
        .unwrap();
    assert!(status_json_output.status.success());
    let status_json = json_stdout(&status_json_output);
    assert_eq!(status_json["lifecycle_state"], "indexing");
    assert_eq!(status_json["registered"], true);
    assert_eq!(status_json["index_status"], "indexing");
    assert_eq!(
        status_json["index_progress"]["message"],
        "storing lifecycle fixture"
    );
    assert_eq!(status_json["index_work"]["files_completed"], 3);

    let status_human_output = cmd_with_home(&canonical_home)
        .args(["status", project.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(status_human_output.status.success());
    let status_human = text_stdout(&status_human_output);
    assert!(status_human.contains("Lifecycle:"));
    assert!(status_human.contains("Index message: storing lifecycle fixture"));
    assert!(status_human.contains("Parallelism: workers 2 effective / 4 configured"));
    assert!(status_human.contains("Timings: db_prepare 5ms"));
    assert!(status_human.contains("Scope: requested scoped | executed full"));
    assert!(status_human.contains("fallback: fixture fallback"));
    assert!(status_human.contains("Prefilter: discovered 4"));
    assert!(status_human.contains("Last file check:"));

    let stop_human_output = cmd_with_home(&canonical_home)
        .args(["stop", unavailable_project.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(stop_human_output.status.success());
    let stop_human = text_stdout(&stop_human_output);
    assert!(stop_human.contains("Status:"));
    assert!(stop_human.contains("daemon not running"));
    assert!(stop_human.contains("Project root:"));
    assert!(stop_human.contains("Registered:"));
    assert!(stop_human.contains("Message: Project deregistered."));

    let stop_registered = cmd_with_home(&canonical_home)
        .args(["stop", project.to_str().unwrap(), "--format", "json"])
        .output()
        .unwrap();
    assert!(stop_registered.status.success());
    let stop_registered_json = json_stdout(&stop_registered);
    assert_eq!(stop_registered_json["status"], "daemon_not_running");
    assert_eq!(stop_registered_json["registered"], false);
    assert_eq!(stop_registered_json["daemon_running"], false);

    let stop_again = cmd_with_home(&canonical_home)
        .args(["stop", project.to_str().unwrap(), "--format", "json"])
        .output()
        .unwrap();
    assert!(stop_again.status.success());
    let stop_again_json = json_stdout(&stop_again);
    assert_eq!(stop_again_json["status"], "not_registered");
    assert_eq!(stop_again_json["registered"], false);
    assert_eq!(stop_again_json["daemon_running"], false);

    let empty_human_list = cmd_with_home(&canonical_home)
        .args(["list"])
        .output()
        .unwrap();
    assert!(empty_human_list.status.success());
    let empty_human = text_stdout(&empty_human_list);
    assert!(empty_human.contains("No registered projects."));
    assert!(empty_human.contains("1up start"));
}

#[cfg(unix)]
#[test]
fn start_prints_status_hint_on_fresh_index() {
    // REQ-032: the post-index success message must point the user at
    // `1up status`. Runs end-to-end against a one-file fixture so the
    // success branch (cold start -> daemon spawn) actually fires.
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let canonical_dir = dir.path().canonicalize().unwrap();
    let canonical_home = home.path().canonicalize().unwrap();
    fs::create_dir_all(canonical_dir.join(".git")).unwrap();
    let data_dir = test_data_dir(&canonical_home);
    let model_dir = data_dir.join("models").join("all-MiniLM-L6-v2");
    fs::create_dir_all(&model_dir).unwrap();
    fs::write(model_dir.join(".download_failed"), "skip download in test").unwrap();
    fs::write(
        canonical_dir.join("main.rs"),
        "fn main() {\n    println!(\"status-hint\");\n}\n",
    )
    .unwrap();

    let output = cmd()
        .env("HOME", &canonical_home)
        .env("XDG_DATA_HOME", canonical_home.join(".local").join("share"))
        .env("XDG_CONFIG_HOME", canonical_home.join(".config"))
        .args(["start", canonical_dir.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "start should succeed: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("1up status"),
        "expected status hint on fresh-index success message; got: {stdout}",
    );

    // Tidy up the daemon we spawned so we don't leak it across tests.
    // `start` can return before the forked daemon has finished writing
    // its pidfile; poll briefly so the cleanup doesn't race that fork.
    let pid_file = data_dir.join("daemon.pid");
    let deadline = Instant::now() + Duration::from_secs(2);
    while !pid_file.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    if pid_file.exists() {
        cmd()
            .env("HOME", &canonical_home)
            .env("XDG_DATA_HOME", canonical_home.join(".local").join("share"))
            .env("XDG_CONFIG_HOME", canonical_home.join(".config"))
            .args(["stop", canonical_dir.to_str().unwrap(), "--format", "json"])
            .assert()
            .success();
    }
}

#[cfg(unix)]
#[test]
fn start_skips_init_on_existing_valid_index() {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let canonical_dir = dir.path().canonicalize().unwrap();
    let canonical_home = home.path().canonicalize().unwrap();
    fs::create_dir_all(canonical_dir.join(".git")).unwrap();
    seed_model_download_failure(&canonical_home);
    let _cleanup = DaemonCleanupGuard::new(&canonical_home, &canonical_dir);

    create_current_index(&canonical_dir);
    fs::write(
        canonical_dir.join(".1up").join("project_id"),
        "fixture-project-id",
    )
    .unwrap();

    let output = run_start_json(&canonical_home, &canonical_dir);
    assert!(
        output.status.success(),
        "start should succeed on an existing valid index: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    let payload = json_stdout(&output);
    assert_eq!(payload["status"], "started");
    assert!(
        payload.get("progress").is_none(),
        "current-index start must not include foreground index progress: {payload}"
    );
    assert!(
        payload.get("work").is_none(),
        "current-index start must not include foreground index work: {payload}"
    );
    let message = payload["message"].as_str().unwrap();
    assert!(
        !message.contains("Initialized project"),
        "existing-index start must not mention init; got: {message}",
    );
    assert!(
        !message.contains("Indexed"),
        "current-index start must not report indexing work; got: {message}",
    );

    let status = wait_for_daemon_running(&canonical_home, &canonical_dir);
    assert!(status["pid"].as_u64().is_some());
}

#[cfg(unix)]
#[test]
fn concurrent_start_current_index_converges_to_one_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let canonical_dir = dir.path().canonicalize().unwrap();
    let canonical_home = home.path().canonicalize().unwrap();
    seed_model_download_failure(&canonical_home);
    let _cleanup = DaemonCleanupGuard::new(&canonical_home, &canonical_dir);

    create_current_index(&canonical_dir);
    fs::write(
        canonical_dir.join(".1up").join("project_id"),
        "fixture-project-id",
    )
    .unwrap();

    let handles: Vec<_> = (0..8)
        .map(|_| {
            let home = canonical_home.clone();
            let project = canonical_dir.clone();
            thread::spawn(move || run_start_json(&home, &project))
        })
        .collect();

    let mut reported_pids = HashSet::new();
    for handle in handles {
        let output = handle.join().unwrap();
        assert!(
            output.status.success(),
            "concurrent start should succeed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let payload = json_stdout(&output);
        let status = payload["status"].as_str().unwrap();
        assert!(
            matches!(
                status,
                "started" | "already_running" | "startup_in_progress"
            ),
            "unexpected current-index start status: {payload}"
        );
        assert!(
            payload.get("progress").is_none(),
            "current-index concurrent start must not index: {payload}"
        );
        assert!(
            payload.get("work").is_none(),
            "current-index concurrent start must not report work: {payload}"
        );
        if let Some(pid) = payload["pid"].as_u64() {
            reported_pids.insert(pid);
        }
    }

    let status = wait_for_daemon_running(&canonical_home, &canonical_dir);
    let final_pid = status["pid"].as_u64().unwrap();
    assert!(
        reported_pids.iter().all(|pid| *pid == final_pid),
        "all reported daemon pids should match final pid {final_pid}; got {reported_pids:?}"
    );
    assert!(
        reported_pids.len() <= 1,
        "concurrent starts should report at most one daemon pid; got {reported_pids:?}"
    );
}

#[cfg(unix)]
#[test]
fn start_contention_preserves_existing_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let canonical_dir = dir.path().canonicalize().unwrap();
    let canonical_home = home.path().canonicalize().unwrap();
    seed_model_download_failure(&canonical_home);
    let _cleanup = DaemonCleanupGuard::new(&canonical_home, &canonical_dir);

    create_current_index(&canonical_dir);
    fs::write(
        canonical_dir.join(".1up").join("project_id"),
        "fixture-project-id",
    )
    .unwrap();

    let output = run_start_json(&canonical_home, &canonical_dir);
    assert!(
        output.status.success(),
        "initial start should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let first_status = wait_for_daemon_running(&canonical_home, &canonical_dir);
    let first_pid = first_status["pid"].as_u64().unwrap();

    let output = run_start_json(&canonical_home, &canonical_dir);
    assert!(
        output.status.success(),
        "contending start should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let payload = json_stdout(&output);
    assert_eq!(payload["status"], "already_running");
    assert_eq!(payload["pid"].as_u64(), Some(first_pid));

    let second_status = wait_for_daemon_running(&canonical_home, &canonical_dir);
    assert_eq!(second_status["pid"].as_u64(), Some(first_pid));
}

#[cfg(unix)]
#[test]
fn concurrent_first_start_reuses_project_id() {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let canonical_dir = dir.path().canonicalize().unwrap();
    let canonical_home = home.path().canonicalize().unwrap();
    fs::create_dir_all(canonical_dir.join(".git")).unwrap();
    seed_model_download_failure(&canonical_home);
    let _cleanup = DaemonCleanupGuard::new(&canonical_home, &canonical_dir);
    fs::write(
        canonical_dir.join("main.rs"),
        "fn main() {\n    println!(\"first start\");\n}\n",
    )
    .unwrap();

    let handles: Vec<_> = (0..4)
        .map(|_| {
            let home = canonical_home.clone();
            let project = canonical_dir.clone();
            thread::spawn(move || run_start_json(&home, &project))
        })
        .collect();

    for handle in handles {
        let output = handle.join().unwrap();
        assert!(
            output.status.success(),
            "first concurrent start should succeed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let payload = json_stdout(&output);
        let status = payload["status"].as_str().unwrap();
        assert!(
            matches!(
                status,
                "indexed_and_started" | "started" | "already_running" | "startup_in_progress"
            ),
            "unexpected first-start status: {payload}"
        );
    }

    let id_path = canonical_dir.join(".1up").join("project_id");
    let project_id = fs::read_to_string(&id_path).unwrap();
    assert!(
        !project_id.trim().is_empty(),
        "project id should be created once"
    );
    wait_for_daemon_running(&canonical_home, &canonical_dir);

    let registry_path = test_data_dir(&canonical_home).join("projects.json");
    let registry: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(registry_path).unwrap()).unwrap();
    let projects = registry["projects"].as_array().unwrap();
    let matching: Vec<_> = projects
        .iter()
        .filter(|entry| entry["project_root"].as_str() == canonical_dir.to_str())
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "registry should contain one project entry"
    );
    assert_eq!(matching[0]["project_id"].as_str(), Some(project_id.trim()));
}

#[cfg(unix)]
#[test]
fn watched_project_refreshes_added_edited_and_deleted_files() {
    let _guard = HideModelGuard::new();
    let root = tempfile::Builder::new()
        .prefix("oneup-watch-")
        .tempdir()
        .unwrap();
    let home = tempfile::tempdir().unwrap();
    let project_dir = root.path().join("watched-project");
    fs::create_dir_all(&project_dir).unwrap();
    let canonical_dir = project_dir.canonicalize().unwrap();
    let canonical_home = home.path().canonicalize().unwrap();
    fs::create_dir_all(canonical_dir.join(".git")).unwrap();
    seed_model_download_failure(&canonical_home);
    let _cleanup = DaemonCleanupGuard::new(&canonical_home, &canonical_dir);

    fs::write(
        canonical_dir.join("watched.rs"),
        "pub fn watched_refresh_before_marker() -> &'static str { \"before\" }\n",
    )
    .unwrap();
    fs::write(
        canonical_dir.join("removed.rs"),
        "pub fn watched_refresh_removed_marker() -> &'static str { \"removed\" }\n",
    )
    .unwrap();

    let output = run_start_json(&canonical_home, &canonical_dir);
    assert!(
        output.status.success(),
        "start should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    wait_for_daemon_running(&canonical_home, &canonical_dir);

    let wait_for_status = |field: &str, expected: &str| {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let status = status_json(&canonical_home, &canonical_dir);
            if status[field].as_str() == Some(expected) {
                return status;
            }
            if Instant::now() >= deadline {
                panic!("status field {field:?} did not become {expected:?}; last status={status}");
            }
            thread::sleep(Duration::from_millis(100));
        }
    };
    wait_for_status("watch_status", "watching");
    let wait_for_refresh_complete = || {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let status = status_json(&canonical_home, &canonical_dir);
            if status["last_update_state"].as_str() == Some("complete")
                && status["last_update_completed_at"].as_str().is_some()
            {
                return status;
            }
            if Instant::now() >= deadline {
                panic!("watched refresh did not complete; last status={status}");
            }
            thread::sleep(Duration::from_secs(1));
        }
    };

    let search_stdout = |query: &str| -> (bool, String, String) {
        let output = cmd_with_home(&canonical_home)
            .args(["search", query, "--path", canonical_dir.to_str().unwrap()])
            .output()
            .unwrap();
        (
            output.status.success(),
            String::from_utf8(output.stdout).unwrap(),
            String::from_utf8(output.stderr).unwrap(),
        )
    };
    let wait_for_search = |query: &str, should_find: bool| {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let (ok, stdout, stderr) = search_stdout(query);
            let found = stdout.contains(query);
            if ok && found == should_find {
                return;
            }
            if Instant::now() >= deadline {
                panic!(
                    "search for {query:?} did not reach expected found={should_find}; stdout={stdout} stderr={stderr}"
                );
            }
            thread::sleep(Duration::from_millis(100));
        }
    };

    wait_for_search("watched_refresh_before_marker", true);
    wait_for_search("watched_refresh_removed_marker", true);

    fs::write(
        canonical_dir.join("watched.rs"),
        "pub fn watched_refresh_after_marker() -> &'static str { \"after\" }\n",
    )
    .unwrap();
    fs::write(
        canonical_dir.join("added.rs"),
        "pub fn watched_refresh_added_marker() -> &'static str { \"added\" }\n",
    )
    .unwrap();
    fs::remove_file(canonical_dir.join("removed.rs")).unwrap();

    wait_for_refresh_complete();
    wait_for_search("watched_refresh_after_marker", true);
    wait_for_search("watched_refresh_added_marker", true);
    wait_for_search("watched_refresh_before_marker", false);
    wait_for_search("watched_refresh_removed_marker", false);

    let final_status = wait_for_refresh_complete();
    assert_eq!(final_status["watch_status"], "watching");
    assert_eq!(final_status["last_update_state"], "complete");
    assert_eq!(final_status["index_status"], "ready");
}
