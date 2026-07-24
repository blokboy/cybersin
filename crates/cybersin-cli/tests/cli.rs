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
fn build_writes_the_full_dist_shape_and_renders_every_configured_target() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    cybersin().arg("init").arg(&project).assert().success();

    // Add `openai` alongside the default `generic` target so both a
    // concrete model family and the portable target render (spec §6.5).
    let cybersin_yaml = fs::read_to_string(project.join("cybersin.yaml")).unwrap();
    fs::write(
        project.join("cybersin.yaml"),
        cybersin_yaml.replace("targets:\n  - generic", "targets:\n  - generic\n  - openai"),
    )
    .unwrap();

    cybersin()
        .arg("build")
        .arg(&project)
        .arg("--profile")
        .arg("dev")
        .arg("--frozen")
        .assert()
        .success();

    let dist = project.join("dist");
    assert!(dist.join("manifest.json").exists());
    assert!(dist.join("routing.json").exists());
    assert!(dist.join("cache.json").exists());
    assert!(dist.join("evals").is_dir());
    assert!(dist.join("budget/hello.json").exists());
    assert!(dist.join("prompts/hello.json").exists());
    assert!(dist.join("prompts/hello/generic.json").exists());
    assert!(dist.join("prompts/hello/openai.json").exists());

    let openai_rendered: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dist.join("prompts/hello/openai.json")).unwrap())
            .unwrap();
    assert_eq!(openai_rendered["target"], "openai");
    assert!(openai_rendered["messages"][0]["content"]
        .as_str()
        .unwrap()
        .contains("<section"));

    let manifest: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dist.join("manifest.json")).unwrap()).unwrap();
    assert!(manifest["artifacts"]
        .as_object()
        .unwrap()
        .contains_key("prompts/hello/openai.json"));
}

#[test]
fn diff_reports_a_change_against_head_via_the_cli() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        assert!(std::process::Command::new("git")
            .args(args)
            .current_dir(&repo)
            .status()
            .unwrap()
            .success());
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "test@example.com"]);
    git(&["config", "user.name", "Test"]);

    let project = repo.join("project");
    cybersin().arg("init").arg(&project).assert().success();
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "initial"]);

    let prompt_path = project.join("prompts/hello.prompt.yaml");
    let updated = fs::read_to_string(&prompt_path)
        .unwrap()
        .replace("warmly", "with great enthusiasm");
    fs::write(&prompt_path, updated).unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "reword"]);

    cybersin()
        .arg("diff")
        .arg("HEAD~1")
        .arg(&project)
        .assert()
        .success()
        .stdout(predicate::str::contains("changed"))
        .stdout(predicate::str::contains("prompts/hello"));
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
