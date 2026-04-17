use assert_cmd::Command;
use oneup::shared::constants::SCHEMA_VERSION;
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

fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Runtime::new().unwrap().block_on(future)
}

fn db_path(dir: &Path) -> PathBuf {
    dir.canonicalize()
        .unwrap_or_else(|_| dir.to_path_buf())
        .join(".1up")
        .join("index.db")
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
        .args(["index", dir.to_str().unwrap(), "--format", "json"])
        .assert()
        .success();

    guard
}

/// A lean search row: `<score>  <path>:<l1>-<l2>  <kind>  <breadcrumb>::<symbol>  :<segment_id>`.
///
/// Only the file_path is surfaced here since the rewrite-SQL verification
/// suite asserts which files appear in the ranked results, not the full row
/// shape (grammar conformance is already covered by integration_tests.rs).
#[derive(Debug, Clone)]
struct LeanSearchRow {
    file_path: String,
}

fn parse_lean_rows(stdout: &str) -> Vec<LeanSearchRow> {
    stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            let parts: Vec<&str> = l.split("  ").collect();
            assert!(
                parts.len() >= 5,
                "expected >=5 double-space fields in lean row, got {}: {:?}",
                parts.len(),
                l
            );
            let path_and_lines = parts[1];
            let (file_path, _) = path_and_lines
                .rsplit_once(':')
                .unwrap_or_else(|| panic!("expected <path>:<l1>-<l2>, got {path_and_lines:?}"));
            LeanSearchRow {
                file_path: file_path.to_string(),
            }
        })
        .collect()
}

fn search_lean(dir: &Path, query: &str) -> (Vec<LeanSearchRow>, String) {
    let output = cmd()
        .args(["search", query, "--path", dir.to_str().unwrap()])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "search failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    let rows = parse_lean_rows(&stdout);

    (rows, String::from_utf8(output.stderr).unwrap())
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

fn create_partial_current_index(dir: &Path) {
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
            "search",
            "config loading",
            "--path",
            tmp.path().to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("found v4")
                .and(predicate::str::contains(format!(
                    "expected v{SCHEMA_VERSION}"
                )))
                .and(predicate::str::contains("1up reindex")),
        );
}

#[test]
fn partial_current_index_search_requires_explicit_reindex_guidance() {
    let tmp = TempDir::new().unwrap();
    create_partial_current_index(tmp.path());

    cmd()
        .args([
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

    let (rows, stderr) = search_lean(tmp.path(), "config loading host port");

    assert!(!rows.is_empty());
    assert!(stderr.contains("degraded to FTS-only mode"));
}

#[test]
fn rebuilt_current_index_keeps_add_edit_delete_search_freshness() {
    let tmp = create_search_fixture();
    let _guard = index_fts_only(tmp.path());

    let (initial_rows, _) = search_lean(tmp.path(), "greetingmarker");
    assert!(initial_rows.iter().any(|r| r.file_path == "main.rs"));

    fs::write(
        tmp.path().join("auth.rs"),
        r#"pub fn validate_token() -> &'static str {
    "tokenvalidationmarker middleware"
}
"#,
    )
    .unwrap();

    cmd()
        .args(["index", tmp.path().to_str().unwrap(), "--format", "json"])
        .assert()
        .success();

    let (added_rows, _) = search_lean(tmp.path(), "tokenvalidationmarker");
    assert!(added_rows.iter().any(|r| r.file_path == "auth.rs"));

    fs::write(
        tmp.path().join("main.rs"),
        r#"fn welcome_user() -> &'static str {
    "welcomemarker pipeline output"
}
"#,
    )
    .unwrap();

    cmd()
        .args(["index", tmp.path().to_str().unwrap(), "--format", "json"])
        .assert()
        .success();

    let (stale_rows, _) = search_lean(tmp.path(), "greetingmarker");
    assert!(stale_rows.is_empty());

    let (updated_rows, _) = search_lean(tmp.path(), "welcomemarker");
    assert!(updated_rows.iter().any(|r| r.file_path == "main.rs"));

    fs::remove_file(tmp.path().join("auth.rs")).unwrap();

    cmd()
        .args(["index", tmp.path().to_str().unwrap(), "--format", "json"])
        .assert()
        .success();

    let (deleted_rows, _) = search_lean(tmp.path(), "tokenvalidationmarker");
    assert!(deleted_rows.is_empty());
}

#[test]
fn index_and_search_leave_source_files_unchanged() {
    let tmp = create_search_fixture();
    let before_main = fs::read_to_string(tmp.path().join("main.rs")).unwrap();
    let before_config = fs::read_to_string(tmp.path().join("config.rs")).unwrap();
    let _guard = index_fts_only(tmp.path());

    let (rows, _) = search_lean(tmp.path(), "config loading host port");
    assert!(!rows.is_empty());

    assert_eq!(
        fs::read_to_string(tmp.path().join("main.rs")).unwrap(),
        before_main
    );
    assert_eq!(
        fs::read_to_string(tmp.path().join("config.rs")).unwrap(),
        before_config
    );
}
