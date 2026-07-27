//! Process-supervision coverage for `cybersin run <agent.yaml>` (issue #35
//! Phase 3): a harness process that exits before completing the session
//! produces a clear error instead of hanging or panicking. Doesn't need a
//! real OpenRouter key (`OPENROUTER_API_KEY` only needs to be present,
//! never validated over the network before the crash) or Docker (the
//! harness crashes before any tool call would reach the sandbox backend).

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::json;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

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

#[tokio::test]
async fn setup_golden_path_runs_end_to_end_without_extra_project_files() {
    let provider_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", "Bearer test-key"))
        .and(body_partial_json(json!({"model": "test-converter"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "message": {
                    "content": "{\"name\":\"starter-draft\",\"quality\":\"medium\",\"sections\":[{\"id\":\"prompt\",\"priority\":100,\"body\":\"Summarize the starter harness path.\"}],\"output_contract\":{\"type\":\"json_schema\",\"schema\":\"{\\\"type\\\":\\\"object\\\",\\\"properties\\\":{\\\"summary\\\":{\\\"type\\\":\\\"string\\\"}},\\\"required\\\":[\\\"summary\\\"]}\"}}"
                }
            }]
        })))
        .mount(&provider_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", "Bearer test-key"))
        .and(body_partial_json(json!({"model": "openai/gpt-4o-mini"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "message": {
                    "content": "{\"summary\":\"starter completed\", \"__cascade_confidence\": 0.99}"
                }
            }]
        })))
        .mount(&provider_server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    let db = project.join(".cybersin/cybersin.db");
    std::fs::create_dir(&project).unwrap();
    std::fs::write(
        project.join(".env"),
        format!(
            "OPENROUTER_API_KEY=\"test-key\"\nOPENROUTER_BASE_URL=\"{}\"\n",
            provider_server.uri()
        ),
    )
    .unwrap();

    // The golden path is intentionally exactly these five user commands,
    // in order: no `check`, no agent creation, no explicit agent path, no
    // user-authored harness file, and no starter template.
    cybersin()
        .current_dir(&project)
        .arg("init")
        .arg(".")
        .assert()
        .success();

    cybersin()
        .current_dir(&project)
        .env_remove("OPENROUTER_API_KEY")
        .env_remove("OPENROUTER_BASE_URL")
        .arg("setup")
        .assert()
        .success()
        .stdout(predicate::str::contains("Cybersin doctor"));

    cybersin()
        .current_dir(&project)
        .env_remove("OPENROUTER_API_KEY")
        .env_remove("OPENROUTER_BASE_URL")
        .arg("convert")
        .arg("--model")
        .arg("test-converter")
        .arg("Create a starter harness smoke prompt.")
        .assert()
        .success()
        .stdout(predicate::str::contains("self-validation passed"));

    assert!(project.join("prompts/starter-draft.prompt.yaml").is_file());
    assert!(
        !project
            .join("agents")
            .join("starter-draft.agent.yaml")
            .exists(),
        "convert/build/run starter path should not require a user-authored harness file"
    );

    cybersin()
        .current_dir(&project)
        .arg("build")
        .assert()
        .success();

    cybersin()
        .current_dir(&project)
        .env_remove("OPENROUTER_API_KEY")
        .env_remove("OPENROUTER_BASE_URL")
        .arg("run")
        .arg("--session-id")
        .arg("sess-starter")
        .assert()
        .success()
        .stdout(predicate::str::contains("built-in starter harness"))
        .stdout(predicate::str::contains("sess-starter completed"))
        .stdout(predicate::str::contains("spans recorded"));

    cybersin()
        .current_dir(&project)
        .arg("sessions")
        .arg("ls")
        .assert()
        .success()
        .stdout(predicate::str::contains("sess-starter"))
        .stdout(predicate::str::contains("completed"))
        .stdout(predicate::str::contains("starter-draft-starter"));

    cybersin()
        .current_dir(&project)
        .arg("trace")
        .arg("ls")
        .arg("--session")
        .arg("sess-starter")
        .assert()
        .success()
        .stdout(predicate::str::contains("llm_call"))
        .stdout(predicate::str::contains("openai/gpt-4o-mini"));

    cybersin()
        .current_dir(&project)
        .arg("cost")
        .arg("--by")
        .arg("session")
        .assert()
        .success()
        .stdout(predicate::str::contains("sess-starter"))
        .stdout(predicate::str::contains("TOTAL"));

    assert!(db.is_file(), "runtime session should create the normal db");
    assert!(project.join("dist/manifest.json").is_file());
    assert!(project.join("dist/routing.json").is_file());
    assert!(project.join("dist/cache.json").is_file());
    assert!(project.join("dist/budget/starter-draft.json").is_file());
    assert!(project.join("dist/prompts/starter-draft.json").is_file());
    assert!(project
        .join("dist/prompts/starter-draft/generic.json")
        .is_file());
    assert!(!project.join("agents/starter-draft.agent.yaml").exists());
    assert!(!project.join("loop.py").exists());
}

#[tokio::test]
async fn init_starter_template_builds_and_runs_without_external_harness() {
    let run_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", "Bearer test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "message": {
                    "content": "{\"summary\":\"starter template completed\", \"next_steps\":[\"build\", \"run\"], \"__cascade_confidence\": 0.99}"
                }
            }]
        })))
        .mount(&run_server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    cybersin()
        .arg("init")
        .arg(&project)
        .arg("--template")
        .arg("starter")
        .assert()
        .success();

    cybersin()
        .arg("build")
        .arg(&project)
        .arg("--profile")
        .arg("dev")
        .arg("--frozen")
        .assert()
        .success();
    assert!(
        !project.join("agents/cybersin-starter.agent.yaml").exists(),
        "starter init should not require a user-authored harness file"
    );

    std::fs::write(
        project.join("cybersin.local.yaml"),
        format!(
            "providers:\n  openrouter:\n    api_key: ${{OPENROUTER_API_KEY}}\n    base_url: {}\n",
            run_server.uri()
        ),
    )
    .unwrap();

    cybersin()
        .current_dir(&project)
        .env("OPENROUTER_API_KEY", "test-key")
        .arg("run")
        .arg("--session-id")
        .arg("sess-init-starter")
        .arg("--input")
        .arg("inputs/cybersin-starter.input.json")
        .assert()
        .success()
        .stdout(predicate::str::contains("built-in starter harness"))
        .stdout(predicate::str::contains("sess-init-starter completed"))
        .stdout(predicate::str::contains("spans recorded"));
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
