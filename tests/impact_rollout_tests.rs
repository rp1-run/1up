#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn script_path() -> PathBuf {
    repo_root()
        .join("scripts")
        .join("approve_impact_rollout.sh")
}

fn run_rollout_approval(args: &[&str]) -> std::process::Output {
    Command::new("bash")
        .arg(script_path())
        .args(args)
        .current_dir(repo_root())
        .output()
        .unwrap()
}

fn write_accuracy_summary(path: &Path, gate_passed: bool) {
    fs::write(
        path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "baseline_commit": "baseline123",
            "candidate_commit": "candidate456",
            "false_positive_reduction_pct": 75,
            "gate": {
                "required_reduction_pct": 50,
                "gate_passed": gate_passed
            },
            "gate_passed": gate_passed
        }))
        .unwrap(),
    )
    .unwrap();
}

fn write_performance_summary(path: &Path, gate_passed: bool) {
    fs::write(
        path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "baseline_commit": "baseline123",
            "candidate_commit": "candidate456",
            "aggregate": {
                "p95_regression_pct": 12.5
            },
            "gate": {
                "max_p95_regression_pct": 20,
                "gate_passed": gate_passed
            },
            "gate_passed": gate_passed
        }))
        .unwrap(),
    )
    .unwrap();
}

#[test]
fn impact_rollout_approval_requires_both_gate_summaries_to_pass() {
    let tempdir = tempfile::tempdir().unwrap();
    let accuracy_summary = tempdir.path().join("impact-eval.json");
    let performance_summary = tempdir.path().join("impact-bench.json");
    let output_path = tempdir.path().join("rollout-approval.json");
    write_accuracy_summary(&accuracy_summary, true);
    write_performance_summary(&performance_summary, true);

    let output = run_rollout_approval(&[
        "--accuracy-summary",
        accuracy_summary.to_str().unwrap(),
        "--performance-summary",
        performance_summary.to_str().unwrap(),
        "--output",
        output_path.to_str().unwrap(),
    ]);
    assert!(
        output.status.success(),
        "rollout approval unexpectedly failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let summary: serde_json::Value =
        serde_json::from_slice(&fs::read(&output_path).unwrap()).unwrap();
    assert_eq!(summary["status"], "approved");
    assert_eq!(summary["gate_passed"], true);
    assert_eq!(summary["requirements"]["both_gates_required"], true);
    assert_eq!(
        summary["requirements"]["required_entry_points"],
        serde_json::json!(["impact-eval", "impact-bench"])
    );
    assert_eq!(
        summary["accuracy"]["summary_path"],
        fs::canonicalize(&accuracy_summary)
            .unwrap()
            .display()
            .to_string()
    );
    assert_eq!(
        summary["performance"]["summary_path"],
        fs::canonicalize(&performance_summary)
            .unwrap()
            .display()
            .to_string()
    );
}

#[test]
fn impact_rollout_approval_blocks_when_any_gate_fails() {
    let tempdir = tempfile::tempdir().unwrap();
    let accuracy_summary = tempdir.path().join("impact-eval.json");
    let performance_summary = tempdir.path().join("impact-bench.json");
    let output_path = tempdir.path().join("rollout-approval.json");
    write_accuracy_summary(&accuracy_summary, true);
    write_performance_summary(&performance_summary, false);

    let output = run_rollout_approval(&[
        "--accuracy-summary",
        accuracy_summary.to_str().unwrap(),
        "--performance-summary",
        performance_summary.to_str().unwrap(),
        "--output",
        output_path.to_str().unwrap(),
    ]);
    assert!(
        !output.status.success(),
        "rollout approval unexpectedly passed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let summary: serde_json::Value =
        serde_json::from_slice(&fs::read(&output_path).unwrap()).unwrap();
    assert_eq!(summary["status"], "blocked");
    assert_eq!(summary["gate_passed"], false);
    assert_eq!(
        summary["blocking_reasons"],
        serde_json::json!(["impact-bench gate failed"])
    );
}
