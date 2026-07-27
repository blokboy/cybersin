//! End-to-end CLI proof for issue #50's acceptance criteria:
//! project-root discovery (walking up from CWD for `cybersin.yaml`) and
//! the `--db`/`--dist`/`--sandbox-root`/`--sandbox-backend` defaults it
//! derives, driven through the actual compiled `cybersin` binary.
//!
//! Unit coverage for `discover_project_root`/`ProjectDefaults` itself
//! (including both `sandbox.backend` spellings) lives in
//! `src/project.rs`'s own `#[cfg(test)]` module; these tests only prove
//! the CLI wiring end-to-end.

use assert_cmd::Command;
use predicates::prelude::*;

fn cybersin() -> Command {
    Command::cargo_bin("cybersin").expect("find cybersin binary")
}

fn write_hello_prompt(project: &std::path::Path) {
    std::fs::write(
        project.join("prompts/hello.prompt.yaml"),
        "name: hello\nquality: medium\nsections:\n- id: prompt\n  priority: 100\n  body: Hello.\n",
    )
    .unwrap();
}

#[test]
fn db_resolves_against_the_discovered_project_root_from_a_nested_subdir() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("myagent");
    cybersin().arg("init").arg(&project).assert().success();

    let nested = project.join("a/b/c");
    std::fs::create_dir_all(&nested).unwrap();

    // No `--db` flag at all: `trace ls` must still find (and create) the
    // project's own `.cybersin/cybersin.db`, discovered by walking up from
    // this nested subdirectory for `cybersin.yaml`.
    cybersin()
        .current_dir(&nested)
        .arg("trace")
        .arg("ls")
        .assert()
        .success()
        .stdout(predicate::str::contains("no spans recorded yet"));

    assert!(
        project.join(".cybersin/cybersin.db").exists(),
        "auto-start should have created the sqlite state file at the discovered project root"
    );
}

#[test]
fn explicit_db_flag_overrides_the_discovered_default() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("myagent");
    cybersin().arg("init").arg(&project).assert().success();
    let explicit_db = tmp.path().join("elsewhere.db");

    cybersin()
        .current_dir(&project)
        .arg("--db")
        .arg(&explicit_db)
        .arg("trace")
        .arg("ls")
        .assert()
        .success();

    assert!(
        explicit_db.exists(),
        "explicit --db must win over discovery"
    );
    assert!(
        !project.join(".cybersin/cybersin.db").exists(),
        "the discovered default must not be used once --db is explicit"
    );
}

#[test]
fn omitting_dist_when_root_dist_does_not_exist_produces_a_clear_error() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("myagent");
    cybersin().arg("init").arg(&project).assert().success();

    cybersin()
        .current_dir(&project)
        .arg("run")
        .arg("--stub")
        .assert()
        .failure()
        .stderr(predicate::str::contains("no dist/ found"))
        .stderr(predicate::str::contains("cybersin build"));
}

#[test]
fn dist_resolves_against_the_discovered_project_root_when_present() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("myagent");
    cybersin().arg("init").arg(&project).assert().success();
    write_hello_prompt(&project);
    cybersin()
        .arg("build")
        .arg(&project)
        .arg("--profile")
        .arg("dev")
        .arg("--frozen")
        .assert()
        .success();

    let nested = project.join("a/b");
    std::fs::create_dir_all(&nested).unwrap();

    // No `--dist` flag: `run` (without `--stub` or an agent path) must get
    // past dist resolution and reach its own no-runnable-target validation,
    // proving `<root>/dist` was found rather than erroring out for a
    // missing dist/.
    cybersin()
        .current_dir(&nested)
        .arg("run")
        .assert()
        .failure()
        .stderr(predicate::str::contains("no runnable agent targets found"));
}
