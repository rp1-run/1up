use assert_cmd::Command;
use oneup::storage::{db::Db, queries, schema};
use predicates::prelude::*;
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tempfile::TempDir;

static MODEL_MUTEX: Mutex<()> = Mutex::new(());

fn cmd() -> Command {
    Command::cargo_bin("1up").unwrap()
}

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

fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Runtime::new().unwrap().block_on(future)
}

fn db_path(dir: &Path) -> PathBuf {
    dir.join(".1up").join("index.db")
}

fn create_search_fixture() -> TempDir {
    let tmp = TempDir::new().unwrap();

    fs::write(
        tmp.path().join("main.rs"),
        r#"fn greet_user() -> &'static str {
    "greetingmarker pipeline output"
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("config.rs"),
        r#"pub fn load_config() -> &'static str {
    "config loading host port settings"
}
"#,
    )
    .unwrap();

    tmp
}

fn index_fts_only(dir: &Path) -> HideModelGuard {
    let guard = HideModelGuard::new();

    cmd()
        .args(["--format", "json", "index", dir.to_str().unwrap()])
        .assert()
        .success();

    guard
}

fn search_json(dir: &Path, query: &str) -> (Vec<serde_json::Value>, String) {
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

    let stdout = String::from_utf8(output.stdout).unwrap();
    let results = serde_json::from_str(stdout.trim()).unwrap();

    (results, String::from_utf8(output.stderr).unwrap())
}

fn create_stale_v4_index(dir: &Path) {
    block_on(async {
        let db = Db::open_rw(&db_path(dir)).await.unwrap();
        let conn = db.connect().unwrap();
        conn.execute(queries::CREATE_META_TABLE, ()).await.unwrap();
        conn.execute(queries::UPSERT_META, ["schema_version", "4"])
            .await
            .unwrap();
    });
}

fn create_partial_v6_index(dir: &Path) {
    block_on(async {
        let db = Db::open_rw(&db_path(dir)).await.unwrap();
        let conn = db.connect().unwrap();
        schema::initialize(&conn).await.unwrap();
        conn.execute("DROP INDEX idx_segment_vectors_embedding", ())
            .await
            .unwrap();
    });
}

#[test]
fn stale_schema_search_requires_explicit_reindex_guidance() {
    let tmp = TempDir::new().unwrap();
    create_stale_v4_index(tmp.path());

    cmd()
        .args([
            "--format",
            "json",
            "search",
            "config loading",
            "--path",
            tmp.path().to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("found v4")
                .and(predicate::str::contains("expected v6"))
                .and(predicate::str::contains("1up reindex")),
        );
}

#[test]
fn partial_v6_index_search_requires_explicit_reindex_guidance() {
    let tmp = TempDir::new().unwrap();
    create_partial_v6_index(tmp.path());

    cmd()
        .args([
            "--format",
            "json",
            "search",
            "config loading",
            "--path",
            tmp.path().to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("incomplete")
                .and(predicate::str::contains("idx_segment_vectors_embedding"))
                .and(predicate::str::contains("1up reindex")),
        );
}

#[test]
fn degraded_search_warns_and_returns_results() {
    let tmp = create_search_fixture();
    let _guard = index_fts_only(tmp.path());

    let (results, stderr) = search_json(tmp.path(), "config loading host port");

    assert!(!results.is_empty());
    assert!(stderr.contains("degraded to FTS-only mode"));
}

#[test]
fn rebuilt_v6_index_keeps_add_edit_delete_search_freshness() {
    let tmp = create_search_fixture();
    let _guard = index_fts_only(tmp.path());

    let (initial_results, _) = search_json(tmp.path(), "greetingmarker");
    assert!(initial_results
        .iter()
        .any(|result| result["file_path"] == "main.rs"));

    fs::write(
        tmp.path().join("auth.rs"),
        r#"pub fn validate_token() -> &'static str {
    "tokenvalidationmarker middleware"
}
"#,
    )
    .unwrap();

    cmd()
        .args(["--format", "json", "index", tmp.path().to_str().unwrap()])
        .assert()
        .success();

    let (added_results, _) = search_json(tmp.path(), "tokenvalidationmarker");
    assert!(added_results
        .iter()
        .any(|result| result["file_path"] == "auth.rs"));

    fs::write(
        tmp.path().join("main.rs"),
        r#"fn welcome_user() -> &'static str {
    "welcomemarker pipeline output"
}
"#,
    )
    .unwrap();

    cmd()
        .args(["--format", "json", "index", tmp.path().to_str().unwrap()])
        .assert()
        .success();

    let (stale_results, _) = search_json(tmp.path(), "greetingmarker");
    assert!(stale_results.is_empty());

    let (updated_results, _) = search_json(tmp.path(), "welcomemarker");
    assert!(updated_results
        .iter()
        .any(|result| result["file_path"] == "main.rs"));

    fs::remove_file(tmp.path().join("auth.rs")).unwrap();

    cmd()
        .args(["--format", "json", "index", tmp.path().to_str().unwrap()])
        .assert()
        .success();

    let (deleted_results, _) = search_json(tmp.path(), "tokenvalidationmarker");
    assert!(deleted_results.is_empty());
}

#[test]
fn index_and_search_leave_source_files_unchanged() {
    let tmp = create_search_fixture();
    let before_main = fs::read_to_string(tmp.path().join("main.rs")).unwrap();
    let before_config = fs::read_to_string(tmp.path().join("config.rs")).unwrap();
    let _guard = index_fts_only(tmp.path());

    let (results, _) = search_json(tmp.path(), "config loading host port");
    assert!(!results.is_empty());

    assert_eq!(
        fs::read_to_string(tmp.path().join("main.rs")).unwrap(),
        before_main
    );
    assert_eq!(
        fs::read_to_string(tmp.path().join("config.rs")).unwrap(),
        before_config
    );
}
