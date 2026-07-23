//! Integration tests for the `cybersin` CLI: `check`, `init`, `fmt`
//! (spec §11), exercised by shelling out to the built binary via
//! `assert_cmd`, matching this issue's acceptance criteria end-to-end.

use std::fs;

use assert_cmd::Command;
use predicates::prelude::*;

fn cybersin() -> Command {
    Command::cargo_bin("cybersin").unwrap()
}

#[test]
fn init_scaffolds_a_project_layout_that_passes_check() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("myagent");

    cybersin()
        .arg("init")
        .arg(&project)
        .assert()
        .success()
        .stdout(predicate::str::contains("scaffolded"));

    for expected in [
        "cybersin.yaml",
        "cybersin.lock",
        "prompts",
        "fragments",
        "evals",
        "agents",
        "dist",
    ] {
        assert!(project.join(expected).exists(), "missing {expected}");
    }

    cybersin()
        .arg("check")
        .arg(&project)
        .assert()
        .success()
        .stdout(predicate::str::contains("ok"));
}

#[test]
fn check_passes_on_a_hand_written_valid_source() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("fragments_method.md"),
        "Search broadly, then narrow.\n",
    )
    .unwrap();
    fs::create_dir_all(tmp.path().join("fragments")).unwrap();
    fs::write(
        tmp.path().join("fragments/research-method.md"),
        "Search broadly, then narrow.\n",
    )
    .unwrap();

    let source = r#"
name: researcher
quality: high
inputs:
  topic: string
  documents: list[document]
sections:
  - id: role
    priority: 100
    body: |
      You are a research analyst focused on {{ topic }}.
  - id: instructions
    priority: 90
    body: !include fragments/research-method.md
  - id: documents
    priority: 50
    body: "{{#each documents}}- {{this.title}}\n{{/each}}"
"#;
    let path = tmp.path().join("researcher.prompt.yaml");
    fs::write(&path, source).unwrap();

    cybersin().arg("check").arg(&path).assert().success();
}

#[test]
fn check_fails_clearly_on_cyclic_include() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir_all(tmp.path().join("fragments")).unwrap();
    fs::write(tmp.path().join("fragments/a.md"), "!include b.md\n").unwrap();
    fs::write(tmp.path().join("fragments/b.md"), "!include a.md\n").unwrap();

    let source = r#"
name: broken
quality: high
inputs:
  topic: string
sections:
  - id: role
    priority: 100
    body: !include fragments/a.md
"#;
    let path = tmp.path().join("broken.prompt.yaml");
    fs::write(&path, source).unwrap();

    cybersin()
        .arg("check")
        .arg(&path)
        .assert()
        .failure()
        .stderr(predicate::str::contains("include cycle"));
}

#[test]
fn check_fails_clearly_on_type_mismatch_and_unused_input() {
    let tmp = tempfile::tempdir().unwrap();
    let source = r#"
name: broken
quality: high
inputs:
  topic: string
  unused_one: string
sections:
  - id: role
    priority: 100
    body: "{{#each topic}}{{this}}{{/each}}"
"#;
    let path = tmp.path().join("broken.prompt.yaml");
    fs::write(&path, source).unwrap();

    cybersin()
        .arg("check")
        .arg(&path)
        .assert()
        .failure()
        .stderr(predicate::str::contains("typecheck failed"))
        .stderr(predicate::str::contains("unused_one"));
}

#[test]
fn fmt_normalizes_a_prompt_source_file() {
    let tmp = tempfile::tempdir().unwrap();
    let source = r#"
sections:
  - priority: 100
    id: role
    body: hello {{ name }}
quality: high
inputs:
  b_input: string
  a_input: string
name: unsorted
"#;
    let path = tmp.path().join("unsorted.prompt.yaml");
    fs::write(&path, source).unwrap();

    cybersin()
        .arg("fmt")
        .arg(&path)
        .assert()
        .success()
        .stdout(predicate::str::contains("formatted"));

    let formatted = fs::read_to_string(&path).unwrap();
    let name_pos = formatted.find("name:").unwrap();
    let quality_pos = formatted.find("quality:").unwrap();
    let inputs_pos = formatted.find("inputs:").unwrap();
    let sections_pos = formatted.find("sections:").unwrap();
    assert!(name_pos < quality_pos && quality_pos < inputs_pos && inputs_pos < sections_pos);

    // idempotent: a second fmt --check run reports already-formatted.
    cybersin()
        .arg("fmt")
        .arg("--check")
        .arg(&path)
        .assert()
        .success()
        .stdout(predicate::str::contains("already formatted"));
}

#[test]
fn build_frozen_fails_when_release_compression_is_not_pinned() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    cybersin().arg("init").arg(&project).assert().success();

    cybersin()
        .arg("build")
        .arg(&project)
        .arg("--frozen")
        .assert()
        .failure()
        .stderr(predicate::str::contains("would require a network call"));
}

#[test]
fn dev_build_excludes_compression_and_succeeds_frozen_without_pins() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    cybersin().arg("init").arg(&project).assert().success();

    cybersin()
        .arg("build")
        .arg(&project)
        .arg("--profile")
        .arg("dev")
        .arg("--frozen")
        .assert()
        .success();
    assert!(project.join("dist/prompts/hello.json").exists());
}

#[test]
fn durable_session_cli_lists_shows_notifies_migrates_and_resumes() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("state.db");

    cybersin()
        .args([
            "--db",
            db.to_str().unwrap(),
            "run",
            "--stub",
            "--session-id",
            "durable-1",
        ])
        .assert()
        .success();
    cybersin()
        .args(["--db", db.to_str().unwrap(), "sessions", "ls"])
        .assert()
        .success()
        .stdout(predicate::str::contains("durable-1"));
    cybersin()
        .args([
            "--db",
            db.to_str().unwrap(),
            "sessions",
            "show",
            "durable-1",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"checkpoint\""));
    cybersin()
        .args([
            "--db",
            db.to_str().unwrap(),
            "notify",
            "durable-1",
            "{\"go\":true}",
        ])
        .assert()
        .success();
    cybersin()
        .args([
            "--db",
            db.to_str().unwrap(),
            "sessions",
            "migrate",
            "durable-1",
            "--config-hash",
            "next",
        ])
        .assert()
        .success();
    cybersin()
        .args([
            "--db",
            db.to_str().unwrap(),
            "sessions",
            "resume",
            "durable-1",
            "--config-hash",
            "next",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("resumed durable-1"));
}
