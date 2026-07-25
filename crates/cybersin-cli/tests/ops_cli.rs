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

use assert_cmd::Command;
use cybersin_runtime::DaemonHandle;
use cybersin_trace::{CacheStatus, Span, SpanKind, SpanStatus};
use predicates::prelude::*;

fn cybersin() -> Command {
    Command::cargo_bin("cybersin").expect("find cybersin binary")
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

    assert!(explicit_db.exists(), "explicit --db must win over discovery");
    assert!(!project.join(".cybersin/cybersin.db").exists());
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
