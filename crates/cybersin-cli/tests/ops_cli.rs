//! End-to-end CLI proof for issue #51's acceptance criteria that don't
//! require a real terminal: the `--plain` snapshot's content, and that
//! `cybersin ops [path]` resolves its `--db` default via issue #50's
//! project-root discovery — from CWD when `path` is omitted, and from an
//! explicit `path` argument otherwise (unlike `explain`'s own `path`,
//! which is never run through that discovery). The interactive TUI's
//! live-refresh loop, tab switching, and quit key were verified manually
//! against the real compiled binary under `tmux` (a real pty) — crossterm
//! input can't be driven from a piped-stdin `assert_cmd` process, which
//! is why `explain`'s own tests are `--plain`-only too.

use std::sync::Arc;

use assert_cmd::Command;
use cybersin_gateway::{ApprovalGate, EchoExecutor, RetryClass, ToolGateway};
use cybersin_runtime::DaemonHandle;
use cybersin_trace::{CacheStatus, Span, SpanKind, SpanStatus};
use predicates::prelude::*;

fn cybersin() -> Command {
    Command::cargo_bin("cybersin").expect("find cybersin binary")
}

fn write_hello_project_sources(project: &std::path::Path) {
    std::fs::write(
        project.join("fragments/tone.md"),
        "You are a friendly, concise assistant.\n",
    )
    .unwrap();
    std::fs::write(
        project.join("prompts/hello.prompt.yaml"),
        r#"name: hello
quality: medium
inputs:
  name: string
sections:
  - id: role
    priority: 100
    body: !include ../fragments/tone.md
  - id: instructions
    priority: 90
    body: |
      Greet {{ name }} warmly and briefly.
"#,
    )
    .unwrap();
    std::fs::write(
        project.join("agents/hello.agent.yaml"),
        r#"name: hello-agent
harness: { adapter: process, command: ["python", "loop.py"] }
budget: { usd_per_session: 1.00, on_breach: degrade }
tools: []
"#,
    )
    .unwrap();
}

#[test]
fn ops_plain_resolves_db_against_the_discovered_project_root_from_a_nested_subdir() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("myagent");
    cybersin().arg("init").arg(&project).assert().success();

    let nested = project.join("a/b/c");
    std::fs::create_dir_all(&nested).unwrap();

    // No `--db` flag and no `path` argument: `ops` must discover the
    // project root by walking up from CWD, exactly like every other
    // runtime command since issue #50.
    cybersin()
        .current_dir(&nested)
        .arg("ops")
        .arg("--plain")
        .assert()
        .success()
        .stdout(predicate::str::contains("Sessions (0)"))
        .stdout(predicate::str::contains("Recent traces (0)"))
        .stdout(predicate::str::contains("Cost by model"));

    assert!(
        project.join(".cybersin/cybersin.db").exists(),
        "auto-start should have created the sqlite state file at the discovered project root"
    );
}

#[test]
fn ops_plain_resolves_db_against_an_explicit_path_argument() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("myagent");
    cybersin().arg("init").arg(&project).assert().success();

    // Run from an unrelated CWD, pointing `ops` at the project via its
    // own `path` argument rather than `cd`-ing there first.
    cybersin()
        .current_dir(tmp.path())
        .arg("ops")
        .arg(&project)
        .arg("--plain")
        .assert()
        .success()
        .stdout(predicate::str::contains("Sessions (0)"));

    assert!(project.join(".cybersin/cybersin.db").exists());
}

#[test]
fn ops_explicit_db_flag_overrides_the_discovered_default() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("myagent");
    cybersin().arg("init").arg(&project).assert().success();
    let explicit_db = tmp.path().join("elsewhere.db");

    cybersin()
        .current_dir(&project)
        .arg("--db")
        .arg(&explicit_db)
        .arg("ops")
        .arg("--plain")
        .assert()
        .success();

    assert!(
        explicit_db.exists(),
        "explicit --db must win over discovery"
    );
    assert!(!project.join(".cybersin/cybersin.db").exists());
}

#[test]
fn ops_plain_lists_runnable_builds_by_agent_name() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("myagent");
    cybersin().arg("init").arg(&project).assert().success();
    write_hello_project_sources(&project);
    cybersin()
        .arg("build")
        .arg(&project)
        .arg("--profile")
        .arg("dev")
        .arg("--frozen")
        .assert()
        .success();
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(project.join("dist/manifest.json")).unwrap())
            .unwrap();
    let build_hash = manifest["build_hash"].as_str().unwrap();
    let build_hash_short = &build_hash[..12];

    cybersin()
        .arg("ops")
        .arg(&project)
        .arg("--plain")
        .assert()
        .success()
        .stdout(predicate::str::contains("Builds (1)"))
        .stdout(predicate::str::contains("hello-agent"))
        .stdout(predicate::str::contains(build_hash_short));
}

#[test]
fn ops_plain_lists_runnable_builds_from_nested_agent_dirs() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("myagent");
    cybersin().arg("init").arg(&project).assert().success();
    write_hello_project_sources(&project);

    let nested_agents = project.join("agents/fleet");
    std::fs::create_dir_all(&nested_agents).unwrap();
    let nested_agent = nested_agents.join("bismarck.agent.yaml");
    std::fs::rename(project.join("agents/hello.agent.yaml"), &nested_agent).unwrap();
    let yaml = std::fs::read_to_string(&nested_agent).unwrap();
    std::fs::write(&nested_agent, yaml.replace("hello-agent", "bismarck-agent")).unwrap();

    cybersin()
        .arg("build")
        .arg(&project)
        .arg("--profile")
        .arg("dev")
        .arg("--frozen")
        .assert()
        .success();

    cybersin()
        .arg("ops")
        .arg(&project)
        .arg("--plain")
        .assert()
        .success()
        .stdout(predicate::str::contains("Builds (1)"))
        .stdout(predicate::str::contains("bismarck-agent"));
}

#[tokio::test]
async fn ops_plain_shows_sessions_traces_and_cost_together() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("myagent");
    let db = tmp.path().join("control-room.db");
    cybersin().arg("init").arg(&project).assert().success();

    let daemon = DaemonHandle::auto_start(&db).await.unwrap();
    daemon
        .storage()
        .create_session_pinned("session-51", "hello-agent", "build-51")
        .await
        .unwrap();
    daemon
        .spans()
        .insert(&Span {
            id: "span-51".into(),
            session_id: "session-51".into(),
            agent_name: "hello-agent".into(),
            kind: SpanKind::LlmCall,
            name: "hello".into(),
            start_unix_ms: 100,
            end_unix_ms: 125,
            model: Some("stub-medium".into()),
            tokens_prompt: Some(12),
            tokens_completion: Some(4),
            usd_cost: 0.25,
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
        .arg("ops")
        .arg(&project)
        .arg("--plain")
        .assert()
        .success()
        .stdout(predicate::str::contains("Sessions (1)"))
        .stdout(predicate::str::contains("session-51"))
        .stdout(predicate::str::contains("Recent traces (1)"))
        .stdout(predicate::str::contains("stub-medium"))
        .stdout(predicate::str::contains("Cost by model"))
        .stdout(predicate::str::contains("$0.250000"));
}

#[tokio::test]
async fn ops_plain_lists_calls_awaiting_approval() {
    // Issue #52's `Storage::list_awaiting_approval` query, surfaced
    // through `ops`'s plain report the same way Sessions/Traces/Cost
    // are — the interactive Approvals tab's row selection and a/d
    // approve/deny confirmation are verified manually against the real
    // compiled binary under `tmux`, same as the rest of this file's
    // interactive-TUI disclaimer.
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("myagent");
    let db = tmp.path().join("control-room.db");
    cybersin().arg("init").arg(&project).assert().success();

    {
        let daemon = DaemonHandle::auto_start(&db).await.unwrap();
        daemon
            .storage()
            .create_session("session-52", "hello-agent")
            .await
            .unwrap();
        let gateway = ToolGateway::new(daemon.storage(), Arc::new(EchoExecutor))
            .with_policy_hook(Arc::new(ApprovalGate::for_tools(["wire_transfer"])));
        gateway
            .call(
                "session-52",
                "wire_transfer",
                serde_json::json!({"amount": 500}),
                Some("wt-1".to_string()),
                RetryClass::Write,
            )
            .await
            .unwrap();
    }

    cybersin()
        .arg("--db")
        .arg(&db)
        .arg("ops")
        .arg(&project)
        .arg("--plain")
        .assert()
        .success()
        .stdout(predicate::str::contains("Approvals (1)"))
        .stdout(predicate::str::contains("wire_transfer:wt-1"))
        .stdout(predicate::str::contains("session-52"))
        .stdout(predicate::str::contains("parked"));
}

#[test]
fn ops_reports_a_nonexistent_path_clearly() {
    let tmp = tempfile::tempdir().unwrap();

    cybersin()
        .arg("ops")
        .arg(tmp.path().join("does-not-exist"))
        .arg("--plain")
        .assert()
        .failure()
        .stderr(predicate::str::contains("resolving project path"));
}
