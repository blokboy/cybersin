//! End-to-end smoke test for issue #35's final acceptance criterion:
//! `cybersin build` a real agent, then `cybersin run` it live against
//! OpenRouter with at least one real (sandboxed) tool call, session
//! recorded in `cybersin trace ls` and `cybersin cost --by session`.
//!
//! `#[ignore]`d — needs a real Docker daemon (for `citation_lookup`'s
//! sandboxed execution) and a real `OPENROUTER_API_KEY` (for the
//! `researcher`/`synthesizer` `llm.request`s), so it doesn't run in
//! default `cargo test`/CI. Run explicitly: `cargo test -p cybersin
//! --test run_live_smoke -- --ignored`.

use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::*;

fn cybersin() -> Command {
    Command::cargo_bin("cybersin").expect("find cybersin binary")
}

fn copy_tree(source: &Path, destination: &Path) {
    std::fs::create_dir_all(destination).unwrap();
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let target = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

#[test]
#[ignore = "needs a real Docker daemon and a real OPENROUTER_API_KEY"]
fn build_then_run_live_records_real_traces_and_cost() {
    let fixture = Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/ic1-research-team"
    ));
    let workdir = tempfile::tempdir().unwrap();
    let project = workdir.path().join("project");
    copy_tree(fixture, &project);
    // Build into a fresh `dist/` rather than trusting whatever's checked
    // in, per the issue's own acceptance criterion ("cybersin build a
    // real agent, then cybersin run it live").
    std::fs::remove_dir_all(project.join("dist")).unwrap();

    cybersin()
        .arg("build")
        .arg(&project)
        .arg("--profile")
        .arg("dev")
        .arg("--frozen")
        .assert()
        .success();

    let db = workdir.path().join("cybersin.db");
    let session_id = "sess-live-smoke";

    cybersin()
        .arg("--db")
        .arg(&db)
        .arg("--dist")
        .arg(project.join("dist"))
        .arg("run")
        .arg(project.join("agents/research-team.agent.yaml"))
        .arg("--session-id")
        .arg(session_id)
        .assert()
        .success()
        .stdout(predicate::str::contains(format!("{session_id} completed")));

    cybersin()
        .arg("--db")
        .arg(&db)
        .arg("trace")
        .arg("ls")
        .arg("--session")
        .arg(session_id)
        .assert()
        .success()
        .stdout(predicate::str::contains("llm_call"))
        .stdout(predicate::str::contains("tool_call"));

    cybersin()
        .arg("--db")
        .arg(&db)
        .arg("cost")
        .arg("--by")
        .arg("session")
        .assert()
        .success()
        .stdout(predicate::str::contains(session_id));
}
