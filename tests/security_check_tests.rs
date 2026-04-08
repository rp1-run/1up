use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

static SECURITY_CHECK_MUTEX: Mutex<()> = Mutex::new(());

fn repo_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn script_path() -> PathBuf {
    repo_root().join("scripts").join("security_check.sh")
}

fn evidence_path() -> PathBuf {
    repo_root()
        .join("target")
        .join("security")
        .join("security-check.json")
}

fn clear_evidence() {
    let _ = fs::remove_file(evidence_path());
}

fn install_fake_cargo(bin_dir: &Path) {
    let cargo_path = bin_dir.join("cargo");
    fs::write(
        &cargo_path,
        r#"#!/bin/sh
set -eu
printf '%s\n' "$*" >> "${FAKE_CARGO_LOG:?}"
case "$*" in
  "fmt --check") exit 0 ;;
  "clippy --all-targets -- -D warnings") exit 0 ;;
  "test --quiet") exit 0 ;;
  "test --quiet shared::fs::tests") exit 0 ;;
  "test --quiet daemon::search_service::tests") exit 0 ;;
  "test --quiet indexer::embedder::tests") exit 0 ;;
  "test --quiet search::context::tests") exit 0 ;;
  "audit --json")
    cat "${FAKE_CARGO_AUDIT_JSON:?}"
    exit "${FAKE_CARGO_AUDIT_EXIT:-0}"
    ;;
  *)
    printf 'unexpected cargo args: %s\n' "$*" >&2
    exit 99
    ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(&cargo_path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn run_security_check(bin_dir: &Path, audit_json: &Path, audit_exit: i32) -> std::process::Output {
    let path = std::env::var("PATH").unwrap();
    Command::new("bash")
        .arg(script_path())
        .current_dir(repo_root())
        .env("PATH", format!("{}:{path}", bin_dir.display()))
        .env("FAKE_CARGO_LOG", bin_dir.join("commands.log"))
        .env("FAKE_CARGO_AUDIT_JSON", audit_json)
        .env("FAKE_CARGO_AUDIT_EXIT", audit_exit.to_string())
        .output()
        .unwrap()
}

fn read_evidence() -> serde_json::Value {
    serde_json::from_slice(&fs::read(evidence_path()).unwrap()).unwrap()
}

#[test]
fn security_check_emits_json_evidence_and_fails_for_blocking_advisories() {
    let _lock = SECURITY_CHECK_MUTEX
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    clear_evidence();

    let bin_dir = tempfile::tempdir().unwrap();
    install_fake_cargo(bin_dir.path());
    let audit_json_path = bin_dir.path().join("audit.json");
    fs::write(
        &audit_json_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "settings": {
                "ignore": [],
                "informational_warnings": [],
                "severity": "high"
            },
            "vulnerabilities": {
                "found": true,
                "count": 1,
                "list": [{
                    "advisory": {
                        "id": "RUSTSEC-2026-0001",
                        "title": "blocking advisory"
                    },
                    "package": {
                        "name": "badcrate",
                        "version": "1.0.0"
                    },
                    "versions": {
                        "patched": [">=1.0.1"]
                    }
                }]
            },
            "warnings": {}
        }))
        .unwrap(),
    )
    .unwrap();

    let output = run_security_check(bin_dir.path(), &audit_json_path, 1);
    assert!(
        !output.status.success(),
        "security_check.sh unexpectedly passed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let evidence = read_evidence();
    assert_eq!(evidence["status"], "failed");
    assert_eq!(evidence["summary"]["failed_steps"], 1);
    assert_eq!(evidence["audit"]["vulnerabilities"]["count"], 1);
    assert_eq!(
        evidence["audit"]["vulnerabilities"]["blocking"][0]["id"],
        "RUSTSEC-2026-0001"
    );
    assert_eq!(
        evidence["audit"]["policy_exceptions"],
        serde_json::json!([])
    );
    assert!(evidence["steps"]
        .as_array()
        .unwrap()
        .iter()
        .any(|step| step["id"] == "cargo_audit" && step["status"] == "failed"));

    clear_evidence();
}

#[test]
fn security_check_reports_policy_exceptions_in_retained_evidence() {
    let _lock = SECURITY_CHECK_MUTEX
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    clear_evidence();

    let bin_dir = tempfile::tempdir().unwrap();
    install_fake_cargo(bin_dir.path());
    let audit_json_path = bin_dir.path().join("audit.json");
    fs::write(
        &audit_json_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "settings": {
                "ignore": ["RUSTSEC-2026-0002"],
                "informational_warnings": ["unmaintained"],
                "severity": "high"
            },
            "vulnerabilities": {
                "found": false,
                "count": 0,
                "list": []
            },
            "warnings": {
                "unmaintained": [{
                    "kind": "unmaintained",
                    "advisory": {
                        "id": "RUSTSEC-2026-0002",
                        "title": "accepted exception"
                    },
                    "package": {
                        "name": "legacycrate",
                        "version": "0.9.0"
                    }
                }]
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let output = run_security_check(bin_dir.path(), &audit_json_path, 0);
    assert!(
        output.status.success(),
        "security_check.sh unexpectedly failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let evidence = read_evidence();
    assert_eq!(evidence["status"], "passed");
    assert_eq!(
        evidence["audit"]["policy_exceptions"],
        serde_json::json!(["RUSTSEC-2026-0002"])
    );
    assert_eq!(
        evidence["audit"]["warnings"]["unmaintained"][0]["package"],
        "legacycrate"
    );
    assert!(evidence["steps"]
        .as_array()
        .unwrap()
        .iter()
        .any(|step| step["id"] == "cargo_audit" && step["status"] == "passed"));

    clear_evidence();
}
