use assert_cmd::Command;
use predicates::prelude::*;

fn cmd() -> Command {
    Command::cargo_bin("1up").unwrap()
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
            .and(predicate::str::contains("reindex")),
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
        "init", "start", "stop", "status", "symbol", "search", "context", "index", "reindex",
    ] {
        cmd()
            .args([sub, "--help"])
            .assert()
            .success()
            .stdout(predicate::str::contains("Usage:"));
    }
}

#[test]
fn format_flag_accepts_all_variants() {
    for fmt in &["json", "human", "plain"] {
        cmd().args(["--format", fmt, "--help"]).assert().success();
    }
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
