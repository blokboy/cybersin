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

fn copy_dir(source: &std::path::Path, destination: &std::path::Path) {
    std::fs::create_dir_all(destination).unwrap();
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&source_path, &destination_path);
        } else {
            std::fs::copy(&source_path, &destination_path).unwrap();
        }
    }
}

fn fixture_dist() -> &'static std::path::Path {
    std::path::Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/ic1-research-team/dist"
    ))
}

fn write_fast_complete_agent(path: &std::path::Path, name: &str, session_id: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let completion =
        format!(r#"{{"type":"session.complete","session_id":"{session_id}","result":{{}}}}"#);
    let script =
        serde_json::to_string(&format!("IFS= read -r _; printf '%s\\n' '{completion}'")).unwrap();
    std::fs::write(
        path,
        format!(
            r#"
name: {name}
harness:
  adapter: process
  command: ["sh", "-c", {script}]
budget:
  usd_per_session: 1.00
  on_breach: degrade
tools: []
"#
        ),
    )
    .unwrap();
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

    cybersin()
        .env("OPENROUTER_API_KEY", "test-key-not-validated-before-crash")
        .arg("--db")
        .arg(&db)
        .arg("--dist")
        .arg(fixture_dist())
        .arg("run")
        .arg(&agent_yaml)
        .assert()
        .failure()
        .stderr(predicate::str::contains("exited unexpectedly"))
        .stderr(predicate::str::contains("code 3"));
}

#[test]
fn project_dotenv_is_loaded_before_openrouter_readiness_check() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("cybersin.db");
    std::fs::write(dir.path().join("cybersin.yaml"), "name: dotenv-test\n").unwrap();
    std::fs::write(
        dir.path().join(".env"),
        "OPENROUTER_API_KEY=test-key-from-dotenv\n",
    )
    .unwrap();
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

    cybersin()
        .current_dir(dir.path())
        .env_remove("OPENROUTER_API_KEY")
        .arg("--db")
        .arg(&db)
        .arg("--dist")
        .arg(fixture_dist())
        .arg("run")
        .arg(&agent_yaml)
        .assert()
        .failure()
        .stderr(predicate::str::contains("exited unexpectedly"))
        .stderr(predicate::str::contains("code 3"))
        .stderr(predicate::str::contains("OPENROUTER_API_KEY is not set").not());
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
    write_fast_complete_agent(&agent_yaml, "fast-complete-agent", "sess-race-test");

    cybersin()
        .env("OPENROUTER_API_KEY", "test-key-not-validated")
        .arg("--db")
        .arg(&db)
        .arg("--dist")
        .arg(fixture_dist())
        .arg("run")
        .arg("--session-id")
        .arg("sess-race-test")
        .arg(&agent_yaml)
        .assert()
        .success()
        .stdout(predicate::str::contains("completed"));
}

#[test]
fn run_without_an_agent_path_infers_the_single_runnable_target() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    std::fs::create_dir(&project).unwrap();
    std::fs::write(project.join("cybersin.yaml"), "name: infer-test\n").unwrap();
    copy_dir(fixture_dist(), &project.join("dist"));
    write_fast_complete_agent(
        &project.join("agents/only.agent.yaml"),
        "only-agent",
        "sess-infer",
    );

    cybersin()
        .current_dir(&project)
        .env("OPENROUTER_API_KEY", "test-key-not-validated")
        .arg("run")
        .arg("--session-id")
        .arg("sess-infer")
        .assert()
        .success()
        .stdout(predicate::str::contains("completed"));
}

#[test]
fn run_without_an_agent_path_lists_choices_when_multiple_targets_exist() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    std::fs::create_dir(&project).unwrap();
    std::fs::write(project.join("cybersin.yaml"), "name: infer-test\n").unwrap();
    std::fs::create_dir(project.join("dist")).unwrap();
    write_fast_complete_agent(
        &project.join("agents/alpha.agent.yaml"),
        "alpha",
        "sess-alpha",
    );
    write_fast_complete_agent(
        &project.join("agents/fleet/beta.agent.yaml"),
        "beta",
        "sess-beta",
    );

    cybersin()
        .current_dir(&project)
        .arg("run")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "multiple runnable agent targets found",
        ))
        .stderr(predicate::str::contains(
            "cybersin run agents/alpha.agent.yaml",
        ))
        .stderr(predicate::str::contains(
            "cybersin run agents/fleet/beta.agent.yaml",
        ));
}
