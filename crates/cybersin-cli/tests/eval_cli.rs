use std::fs;

use assert_cmd::Command;
use cybersin_runtime::DaemonHandle;
use cybersin_trace::{CacheStatus, Span, SpanKind, SpanStatus};
use predicates::prelude::*;

fn cybersin() -> Command {
    Command::cargo_bin("cybersin").expect("find cybersin binary")
}

fn project() -> (tempfile::TempDir, std::path::PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    cybersin().arg("init").arg(&project).assert().success();
    cybersin()
        .args(["build", "--profile", "dev", "--frozen"])
        .arg(&project)
        .assert()
        .success();
    (temp, project)
}

#[test]
fn eval_run_reports_a_distribution_and_gate_catches_seeded_regression() {
    let (_temp, project) = project();
    fs::write(
        project.join("evals/hello.eval.yaml"),
        r#"prompt: hello
cases:
  - name: greeting
    inputs: { name: Ada }
    assertions:
      - type: contains_none
        values: [error]
    recorded_outputs:
      - { output: "Hello Ada" }
      - { output: "error: regression" }
      - { output: "Welcome Ada" }
runs_per_case: 3
"#,
    )
    .unwrap();

    cybersin()
        .args(["eval", "run"])
        .arg(&project)
        .assert()
        .success()
        .stdout(predicate::str::contains("runs=3"))
        .stdout(predicate::str::contains("min=0.000"))
        .stdout(predicate::str::contains("FAIL"));

    cybersin()
        .args(["eval", "gate"])
        .arg(&project)
        .assert()
        .failure()
        .stderr(predicate::str::contains("eval gate failed"));
}

#[tokio::test]
async fn trace_sample_promotes_an_llm_span_to_an_eval_fixture() {
    let (temp, project) = project();
    let db = temp.path().join("trace.db");
    let destination = project.join("evals/production.eval.yaml");
    let daemon = DaemonHandle::auto_start(&db).await.unwrap();
    daemon
        .spans()
        .insert(&Span {
            id: "production-span".into(),
            session_id: "session".into(),
            agent_name: "agent".into(),
            kind: SpanKind::LlmCall,
            name: "hello".into(),
            start_unix_ms: 1,
            end_unix_ms: 2,
            model: Some("stub-medium".into()),
            tokens_prompt: Some(2),
            tokens_completion: Some(2),
            usd_cost: 0.01,
            cache_status: CacheStatus::Miss,
            retries: 0,
            evicted_sections: vec![],
            status: SpanStatus::Ok,
            attributes: serde_json::json!({
                "inputs": {"name": "Ada"},
                "output": "Hello Ada"
            }),
        })
        .await
        .unwrap();
    drop(daemon);

    cybersin()
        .arg("--db")
        .arg(db)
        .args(["trace", "sample", "production-span", "--to-eval"])
        .arg(&destination)
        .assert()
        .success();

    let fixture = fs::read_to_string(destination).unwrap();
    assert!(fixture.contains("prompt: hello"));
    assert!(fixture.contains("production_production-span"));
    assert!(fixture.contains("Hello Ada"));
}
