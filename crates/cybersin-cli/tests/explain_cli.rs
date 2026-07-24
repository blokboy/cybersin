use assert_cmd::Command;
use cybersin_runtime::DaemonHandle;
use cybersin_trace::{CacheStatus, Span, SpanKind, SpanStatus};
use predicates::prelude::*;
use std::fs;

fn cybersin() -> Command {
    Command::cargo_bin("cybersin").expect("find cybersin binary")
}

fn scaffold_and_build(project: &std::path::Path) {
    cybersin().arg("init").arg(project).assert().success();
    let config_path = project.join("cybersin.yaml");
    let config = fs::read_to_string(&config_path).unwrap();
    fs::write(
        config_path,
        config.replace("  - generic", "  - generic\n  - openai"),
    )
    .unwrap();
    cybersin()
        .args(["build", "--profile", "dev", "--frozen"])
        .arg(project)
        .assert()
        .success();
}

#[tokio::test]
async fn explain_plain_uses_compiled_artifacts_and_observed_telemetry() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    let db = temp.path().join("control-room.db");
    scaffold_and_build(&project);

    let daemon = DaemonHandle::auto_start(&db).await.unwrap();
    daemon
        .storage()
        .create_session_pinned("session-22", "hello-agent", "build-22")
        .await
        .unwrap();
    daemon
        .spans()
        .insert(&Span {
            id: "span-22".into(),
            session_id: "session-22".into(),
            agent_name: "hello-agent".into(),
            kind: SpanKind::LlmCall,
            name: "hello".into(),
            start_unix_ms: 100,
            end_unix_ms: 125,
            model: Some("stub-medium".into()),
            tokens_prompt: Some(12),
            tokens_completion: Some(4),
            usd_cost: 0.125,
            cache_status: CacheStatus::Miss,
            retries: 0,
            evicted_sections: vec![],
            status: SpanStatus::Ok,
            attributes: serde_json::json!({}),
        })
        .await
        .unwrap();
    drop(daemon);

    cybersin()
        .arg("--db")
        .arg(&db)
        .arg("explain")
        .arg("hello")
        .arg(&project)
        .arg("--plain")
        .assert()
        .success()
        .stdout(predicate::str::contains("Cybersin Explain: hello"))
        .stdout(predicate::str::contains("Section tokens by target"))
        .stdout(predicate::str::contains("generic"))
        .stdout(predicate::str::contains("openai"))
        .stdout(predicate::str::contains("role"))
        .stdout(predicate::str::contains("instructions"))
        .stdout(predicate::str::contains("Routing"))
        .stdout(predicate::str::contains("stub-medium"))
        .stdout(predicate::str::contains("estimated $"))
        .stdout(predicate::str::contains(
            "Observed: $0.125000 across 1 LLM call",
        ))
        .stdout(predicate::str::contains("Sessions (1)"))
        .stdout(predicate::str::contains("session-22"))
        .stdout(predicate::str::contains("Recent traces (1)"))
        .stdout(predicate::str::contains("Cost by model"));
}

#[test]
fn explain_reports_missing_build_artifacts_clearly() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    let db = temp.path().join("control-room.db");
    cybersin().arg("init").arg(&project).assert().success();

    cybersin()
        .arg("--db")
        .arg(db)
        .arg("explain")
        .arg("hello")
        .arg(&project)
        .arg("--plain")
        .assert()
        .failure()
        .stderr(predicate::str::contains("run `cybersin build"));
}
