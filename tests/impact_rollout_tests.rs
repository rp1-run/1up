#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const REQUIREMENTS_BASELINE_COMMIT: &str = "310097091d6dc3666563ee4ca4b8755a3e6e2934";

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

fn current_head_commit() -> String {
    String::from_utf8(
        Command::new("git")
            .arg("-C")
            .arg(repo_root())
            .arg("rev-parse")
            .arg("HEAD")
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string()
}

struct AccuracySummarySpec<'a> {
    baseline_commit: &'a str,
    candidate_commit: &'a str,
    false_positive_reduction_pct: u64,
    required_reduction_pct: u64,
    exact_anchor_regressions_after: u64,
    status_contract_failures_after: u64,
    gate_passed: bool,
}

fn write_accuracy_summary(path: &Path, spec: &AccuracySummarySpec<'_>) {
    fs::write(
        path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "baseline_commit": spec.baseline_commit,
            "candidate_commit": spec.candidate_commit,
            "false_positive_reduction_pct": spec.false_positive_reduction_pct,
            "gate": {
                "required_reduction_pct": spec.required_reduction_pct,
                "exact_anchor_regressions_after": spec.exact_anchor_regressions_after,
                "status_contract_failures_after": spec.status_contract_failures_after,
                "gate_passed": spec.gate_passed
            },
            "gate_passed": spec.gate_passed
        }))
        .unwrap(),
    )
    .unwrap();
}

struct PerformanceSummarySpec<'a> {
    baseline_commit: &'a str,
    candidate_commit: &'a str,
    baseline_command_failures: u64,
    baseline_contract_failures: u64,
    candidate_command_failures: u64,
    candidate_contract_failures: u64,
    p95_regression_pct: f64,
    max_p95_regression_pct: f64,
    gate_passed: bool,
}

fn write_performance_summary(path: &Path, spec: &PerformanceSummarySpec<'_>) {
    fs::write(
        path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "baseline_commit": spec.baseline_commit,
            "candidate_commit": spec.candidate_commit,
            "aggregate": {
                "p95_regression_pct": spec.p95_regression_pct
            },
            "gate": {
                "max_p95_regression_pct": spec.max_p95_regression_pct,
                "gate_passed": spec.gate_passed
            },
            "gate_passed": spec.gate_passed,
            "cases": [{
                "name": "rollout-case",
                "baseline": {
                    "command_failures": spec.baseline_command_failures,
                    "contract_failures": spec.baseline_contract_failures
                },
                "candidate": {
                    "command_failures": spec.candidate_command_failures,
                    "contract_failures": spec.candidate_contract_failures
                },
                "regression_pct": {
                    "p95": spec.p95_regression_pct
                },
                "gate": {
                    "max_p95_regression_pct": spec.max_p95_regression_pct
                }
            }]
        }))
        .unwrap(),
    )
    .unwrap();
}

fn write_field_notes(path: &Path, resolved_blockers: &[&str], unresolved_blockers: &[&str]) {
    let mut content = String::from("# Field Notes: impact-rollout\n\n## Rollout Blockers\n");

    for blocker in resolved_blockers {
        content.push_str(&format!("- [x] {blocker}\n"));
    }
    for blocker in unresolved_blockers {
        content.push_str(&format!("- [ ] {blocker}\n"));
    }

    fs::write(path, content).unwrap();
}

#[test]
fn impact_rollout_approval_requires_both_gate_summaries_to_pass() {
    let tempdir = tempfile::tempdir().unwrap();
    let accuracy_summary = tempdir.path().join("impact-eval.json");
    let performance_summary = tempdir.path().join("impact-bench.json");
    let field_notes = tempdir.path().join("field-notes.md");
    let output_path = tempdir.path().join("rollout-approval.json");
    let head_commit = current_head_commit();
    write_accuracy_summary(
        &accuracy_summary,
        &AccuracySummarySpec {
            baseline_commit: REQUIREMENTS_BASELINE_COMMIT,
            candidate_commit: &head_commit,
            false_positive_reduction_pct: 75,
            required_reduction_pct: 50,
            exact_anchor_regressions_after: 0,
            status_contract_failures_after: 0,
            gate_passed: true,
        },
    );
    write_performance_summary(
        &performance_summary,
        &PerformanceSummarySpec {
            baseline_commit: REQUIREMENTS_BASELINE_COMMIT,
            candidate_commit: &head_commit,
            baseline_command_failures: 0,
            baseline_contract_failures: 0,
            candidate_command_failures: 0,
            candidate_contract_failures: 0,
            p95_regression_pct: 12.5,
            max_p95_regression_pct: 20.0,
            gate_passed: true,
        },
    );
    write_field_notes(
        &field_notes,
        &["baseline pin and blocker ingestion verified"],
        &[],
    );

    let output = run_rollout_approval(&[
        "--accuracy-summary",
        accuracy_summary.to_str().unwrap(),
        "--performance-summary",
        performance_summary.to_str().unwrap(),
        "--field-notes",
        field_notes.to_str().unwrap(),
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
        summary["requirements"]["required_baseline_commit"],
        REQUIREMENTS_BASELINE_COMMIT
    );
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
    assert_eq!(
        summary["field_notes"]["path"],
        fs::canonicalize(&field_notes)
            .unwrap()
            .display()
            .to_string()
    );
    assert_eq!(summary["field_notes"]["has_unresolved_blockers"], false);
    assert_eq!(
        summary["field_notes"]["unresolved_blockers"],
        serde_json::json!([])
    );
}

#[test]
fn impact_rollout_approval_blocks_when_any_gate_fails() {
    let tempdir = tempfile::tempdir().unwrap();
    let accuracy_summary = tempdir.path().join("impact-eval.json");
    let performance_summary = tempdir.path().join("impact-bench.json");
    let output_path = tempdir.path().join("rollout-approval.json");
    let head_commit = current_head_commit();
    write_accuracy_summary(
        &accuracy_summary,
        &AccuracySummarySpec {
            baseline_commit: REQUIREMENTS_BASELINE_COMMIT,
            candidate_commit: &head_commit,
            false_positive_reduction_pct: 75,
            required_reduction_pct: 50,
            exact_anchor_regressions_after: 0,
            status_contract_failures_after: 0,
            gate_passed: true,
        },
    );
    write_performance_summary(
        &performance_summary,
        &PerformanceSummarySpec {
            baseline_commit: REQUIREMENTS_BASELINE_COMMIT,
            candidate_commit: &head_commit,
            baseline_command_failures: 0,
            baseline_contract_failures: 0,
            candidate_command_failures: 0,
            candidate_contract_failures: 1,
            p95_regression_pct: 12.5,
            max_p95_regression_pct: 20.0,
            gate_passed: false,
        },
    );

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

#[test]
fn impact_rollout_approval_blocks_when_field_notes_list_unresolved_blockers() {
    let tempdir = tempfile::tempdir().unwrap();
    let accuracy_summary = tempdir.path().join("impact-eval.json");
    let performance_summary = tempdir.path().join("impact-bench.json");
    let field_notes = tempdir.path().join("field-notes.md");
    let output_path = tempdir.path().join("rollout-approval.json");
    let head_commit = current_head_commit();
    write_accuracy_summary(
        &accuracy_summary,
        &AccuracySummarySpec {
            baseline_commit: REQUIREMENTS_BASELINE_COMMIT,
            candidate_commit: &head_commit,
            false_positive_reduction_pct: 75,
            required_reduction_pct: 50,
            exact_anchor_regressions_after: 0,
            status_contract_failures_after: 0,
            gate_passed: true,
        },
    );
    write_performance_summary(
        &performance_summary,
        &PerformanceSummarySpec {
            baseline_commit: REQUIREMENTS_BASELINE_COMMIT,
            candidate_commit: &head_commit,
            baseline_command_failures: 0,
            baseline_contract_failures: 0,
            candidate_command_failures: 0,
            candidate_contract_failures: 0,
            p95_regression_pct: 12.5,
            max_p95_regression_pct: 20.0,
            gate_passed: true,
        },
    );
    write_field_notes(
        &field_notes,
        &["baseline pin landed"],
        &["refresh the feature verification artifact"],
    );

    let output = run_rollout_approval(&[
        "--accuracy-summary",
        accuracy_summary.to_str().unwrap(),
        "--performance-summary",
        performance_summary.to_str().unwrap(),
        "--field-notes",
        field_notes.to_str().unwrap(),
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
    assert_eq!(summary["field_notes"]["has_unresolved_blockers"], true);
    assert_eq!(
        summary["field_notes"]["unresolved_blockers"],
        serde_json::json!(["refresh the feature verification artifact"])
    );
    assert_eq!(
        summary["blocking_reasons"],
        serde_json::json!([
            "field-notes unresolved blocker: refresh the feature verification artifact"
        ])
    );
}

#[test]
fn impact_rollout_approval_blocks_when_candidate_commit_is_not_head() {
    let tempdir = tempfile::tempdir().unwrap();
    let accuracy_summary = tempdir.path().join("impact-eval.json");
    let performance_summary = tempdir.path().join("impact-bench.json");
    let output_path = tempdir.path().join("rollout-approval.json");
    write_accuracy_summary(
        &accuracy_summary,
        &AccuracySummarySpec {
            baseline_commit: REQUIREMENTS_BASELINE_COMMIT,
            candidate_commit: "not-head",
            false_positive_reduction_pct: 75,
            required_reduction_pct: 50,
            exact_anchor_regressions_after: 0,
            status_contract_failures_after: 0,
            gate_passed: true,
        },
    );
    write_performance_summary(
        &performance_summary,
        &PerformanceSummarySpec {
            baseline_commit: REQUIREMENTS_BASELINE_COMMIT,
            candidate_commit: "not-head",
            baseline_command_failures: 0,
            baseline_contract_failures: 0,
            candidate_command_failures: 0,
            candidate_contract_failures: 0,
            p95_regression_pct: 12.5,
            max_p95_regression_pct: 20.0,
            gate_passed: true,
        },
    );

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
    assert_eq!(
        summary["blocking_reasons"],
        serde_json::json!([
            "impact-eval candidate commit does not match current HEAD",
            "impact-bench candidate commit does not match current HEAD"
        ])
    );
}

#[test]
fn impact_rollout_approval_recomputes_performance_gate_from_case_details() {
    let tempdir = tempfile::tempdir().unwrap();
    let accuracy_summary = tempdir.path().join("impact-eval.json");
    let performance_summary = tempdir.path().join("impact-bench.json");
    let output_path = tempdir.path().join("rollout-approval.json");
    let head_commit = current_head_commit();
    write_accuracy_summary(
        &accuracy_summary,
        &AccuracySummarySpec {
            baseline_commit: REQUIREMENTS_BASELINE_COMMIT,
            candidate_commit: &head_commit,
            false_positive_reduction_pct: 75,
            required_reduction_pct: 50,
            exact_anchor_regressions_after: 0,
            status_contract_failures_after: 0,
            gate_passed: true,
        },
    );
    write_performance_summary(
        &performance_summary,
        &PerformanceSummarySpec {
            baseline_commit: REQUIREMENTS_BASELINE_COMMIT,
            candidate_commit: &head_commit,
            baseline_command_failures: 1,
            baseline_contract_failures: 0,
            candidate_command_failures: 0,
            candidate_contract_failures: 0,
            p95_regression_pct: 12.5,
            max_p95_regression_pct: 20.0,
            gate_passed: true,
        },
    );

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
    assert_eq!(
        summary["blocking_reasons"],
        serde_json::json!(["impact-bench gate failed"])
    );
}
