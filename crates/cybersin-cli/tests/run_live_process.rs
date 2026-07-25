//! Process-supervision coverage for `cybersin run <agent.yaml>` (issue #35
//! Phase 3): a harness process that exits before completing the session
//! produces a clear error instead of hanging or panicking. Doesn't need a
//! real OpenRouter key (`OPENROUTER_API_KEY` only needs to be present,
//! never validated over the network before the crash) or Docker (the
//! harness crashes before any tool call would reach the sandbox backend).

use assert_cmd::Command;
use predicates::prelude::*;

fn cybersin() -> Command {
    Command::cargo_bin("cybersin").expect("find cybersin binary")
}

#[test]
fn a_harness_process_that_exits_early_produces_a_clear_error() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("cybersin.db");
    let agent_yaml = dir.path().join("crash.agent.yaml");
    std::fs::write(
        &agent_yaml,
        r#"
name: crash-agent
harness:
  adapter: process
  command: ["sh", "-c", "exit 3"]
budget:
  usd_per_session: 1.00
  on_breach: degrade
tools: []
"#,
    )
    .unwrap();

    let dist_dir = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/ic1-research-team/dist"
    );

    cybersin()
        .env("OPENROUTER_API_KEY", "test-key-not-validated-before-crash")
        .arg("--db")
        .arg(&db)
        .arg("--dist")
        .arg(dist_dir)
        .arg("run")
        .arg(&agent_yaml)
        .assert()
        .failure()
        .stderr(predicate::str::contains("exited unexpectedly"))
        .stderr(predicate::str::contains("code 3"));
}
