use assert_cmd::Command;
use oneup::shared::constants::SCHEMA_VERSION;
use oneup::storage::{db::Db, queries, schema};
use predicates::prelude::*;
use std::fs;
use std::path::{Path, PathBuf};
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
    for sub in &[
        "status", "start", "stop", "init", "index", "reindex", "update",
    ] {
        cmd()
            .args([sub, "--help"])
            .assert()
            .success()
            .stdout(predicate::str::contains("Output format override"));
    }
}

#[test]
fn status_defaults_to_human_output() {
    let dir = tempfile::tempdir().unwrap();

    cmd()
        .args(["status", dir.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("Daemon:")
                .and(predicate::str::contains("Project:"))
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

#[test]
fn start_indexes_project_when_daemon_is_already_running_and_index_is_missing() {
    let home = tempfile::tempdir().unwrap();
    let project_a = tempfile::tempdir().unwrap();
    let project_b = tempfile::tempdir().unwrap();
    let canonical_home = home.path().canonicalize().unwrap();
    let canonical_project_a = project_a.path().canonicalize().unwrap();
    let canonical_project_b = project_b.path().canonicalize().unwrap();
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
fn index_watch_plain_output_streams_progress_updates() {
    let _guard = HideModelGuard::new();
    let dir = tempfile::tempdir().unwrap();
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
        .args(["hello-agent", "--format", "json"])
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
        .args(["hello-agent", "--format", "human"])
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

/// Seed a project DB at the current schema version without running the
/// indexer. Matches `prepare_for_write`'s fresh-database branch.
fn create_current_index(dir: &Path) {
    block_on(async {
        let db = Db::open_rw(&project_db_path(dir)).await.unwrap();
        let conn = db.connect().unwrap();
        schema::initialize(&conn).await.unwrap();
    });
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

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        combined.contains("out of date"),
        "expected stale-schema warning to mention 'out of date'; got: {combined}",
    );
    assert!(
        combined.contains("1up reindex"),
        "expected stale-schema warning to name `1up reindex`; got: {combined}",
    );
    assert!(
        combined.contains(&format!("expected v{SCHEMA_VERSION}")),
        "expected warning to name expected schema version v{SCHEMA_VERSION}; got: {combined}",
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
fn start_prints_status_hint_on_fresh_index() {
    // REQ-032: the post-index success message must point the user at
    // `1up status`. Runs end-to-end against a one-file fixture so the
    // success branch (cold start -> daemon spawn) actually fires.
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let canonical_dir = dir.path().canonicalize().unwrap();
    let canonical_home = home.path().canonicalize().unwrap();
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
    // REQ-031: when a project already has a current-schema index, `start`
    // must not mention the "Initialized project ..." prefix that the
    // first-run init path emits. The check is content-only: we do not
    // assert on daemon state because the pre-existing index means the
    // happy path is the silent-registration branch.
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let canonical_dir = dir.path().canonicalize().unwrap();
    let canonical_home = home.path().canonicalize().unwrap();
    let data_dir = test_data_dir(&canonical_home);
    let model_dir = data_dir.join("models").join("all-MiniLM-L6-v2");
    fs::create_dir_all(&model_dir).unwrap();
    fs::write(model_dir.join(".download_failed"), "skip download in test").unwrap();

    // Pre-populate `.1up/` with a current-schema index + project_id so the
    // "already initialized" branch is taken end-to-end.
    create_current_index(&canonical_dir);
    fs::write(
        canonical_dir.join(".1up").join("project_id"),
        "fixture-project-id",
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
        "start should succeed on an existing valid index: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        !stdout.contains("Initialized project"),
        "existing-index start must not mention init; got: {stdout}",
    );

    // Poll briefly for the daemon pidfile: `start` can return before the
    // forked daemon writes it, and the cleanup must not race that fork.
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
