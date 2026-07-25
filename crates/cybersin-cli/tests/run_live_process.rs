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

/// Regression test: a harness that finishes every step and sends
/// `session.complete` before exiting must be reported as a completed
/// session, not a crash, even if the OS happens to reap the harness
/// process before `runtime_daemon.run()` is next polled. `printf` (a
/// direct exec, not `sh -c`, so there's no shell startup latency at all)
/// writes the closing message and exits about as fast as an OS process
/// can, which reliably wins that race in practice -- first surfaced live
/// as a fully successful scripted run (every step, including a
/// Docker-sandboxed approval, resolved correctly) that still printed
/// "exited unexpectedly (code 0)".
#[test]
fn a_harness_process_that_completes_and_exits_immediately_is_reported_as_success() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("cybersin.db");
    let agent_yaml = dir.path().join("fast-complete.agent.yaml");
    std::fs::write(
        &agent_yaml,
        r#"
name: fast-complete-agent
harness:
  adapter: process
  command: ["printf", "%s\n", "{\"type\":\"session.complete\",\"session_id\":\"sess-race-test\",\"result\":{}}"]
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
        .env("OPENROUTER_API_KEY", "test-key-not-validated")
        .arg("--db")
        .arg(&db)
        .arg("--dist")
        .arg(dist_dir)
        .arg("run")
        .arg("--session-id")
        .arg("sess-race-test")
        .arg(&agent_yaml)
        .assert()
        .success()
        .stdout(predicate::str::contains("completed"));
}
