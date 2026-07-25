//! Process-supervision coverage for `cybersin run <agent.yaml>`'s
//! `harness.adapter: grpc` path (issue #37) — mirrors
//! `run_live_process.rs`'s two cases (a completed session, a harness that
//! crashes before ever connecting) but over the gRPC transport instead of
//! stdio. Doesn't need a real OpenRouter key or Docker, for the same
//! reasons `run_live_process.rs` doesn't.

use assert_cmd::Command;
use predicates::prelude::*;

fn cybersin() -> Command {
    Command::cargo_bin("cybersin").expect("find cybersin binary")
}

fn dist_dir() -> &'static str {
    concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/ic1-research-team/dist"
    )
}

#[test]
fn a_grpc_harness_completes_a_session() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("cybersin.db");
    let agent_yaml = dir.path().join("grpc.agent.yaml");
    let harness_bin = env!("CARGO_BIN_EXE_grpc_stub_harness");
    std::fs::write(
        &agent_yaml,
        format!(
            r#"
name: grpc-agent
harness:
  adapter: grpc
  command: ["{harness_bin}"]
budget:
  usd_per_session: 1.00
  on_breach: degrade
tools: []
"#
        ),
    )
    .unwrap();

    cybersin()
        .env("OPENROUTER_API_KEY", "test-key-not-validated-in-this-test")
        .arg("--db")
        .arg(&db)
        .arg("--dist")
        .arg(dist_dir())
        .arg("run")
        .arg(&agent_yaml)
        .assert()
        .success()
        .stdout(predicate::str::contains("completed"));
}

#[test]
fn a_grpc_harness_process_that_exits_early_produces_a_clear_error() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("cybersin.db");
    let agent_yaml = dir.path().join("crash.agent.yaml");
    std::fs::write(
        &agent_yaml,
        r#"
name: crash-agent
harness:
  adapter: grpc
  command: ["sh", "-c", "exit 3"]
budget:
  usd_per_session: 1.00
  on_breach: degrade
tools: []
"#,
    )
    .unwrap();

    cybersin()
        .env("OPENROUTER_API_KEY", "test-key-not-validated-before-crash")
        .arg("--db")
        .arg(&db)
        .arg("--dist")
        .arg(dist_dir())
        .arg("run")
        .arg(&agent_yaml)
        .assert()
        .failure()
        .stderr(predicate::str::contains("exited unexpectedly"))
        .stderr(predicate::str::contains("code 3"));
}
