//! End-to-end CLI proof (issue #10's acceptance criteria): a fresh
//! checkout's `cybersin run --stub` produces real spans that `cybersin
//! trace ls|show` and `cybersin cost --by <dim>` then surface — driven
//! through the actual compiled `cybersin` binary, not library calls.

use assert_cmd::Command;
use predicates::prelude::*;

fn cybersin() -> Command {
    Command::cargo_bin("cybersin").expect("find cybersin binary")
}

#[test]
fn stub_run_then_trace_and_cost_show_real_data() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("cybersin.db");

    // `cybersind` auto-starts here (spec §1): nothing was running before
    // this command, and the db file doesn't exist yet.
    cybersin()
        .arg("--db")
        .arg(&db)
        .arg("run")
        .arg("--stub")
        .arg("--session-id")
        .arg("sess-e2e")
        .assert()
        .success()
        .stdout(predicate::str::contains("cybersind: auto-starting"))
        .stdout(predicate::str::contains("sess-e2e completed"))
        .stdout(predicate::str::contains("5 spans recorded"));

    assert!(
        db.exists(),
        "auto-start should have created the sqlite state file"
    );

    // `trace ls` sees the real spans this run recorded, scoped to the
    // session id.
    cybersin()
        .arg("--db")
        .arg(&db)
        .arg("trace")
        .arg("ls")
        .arg("--session")
        .arg("sess-e2e")
        .assert()
        .success()
        .stdout(predicate::str::contains("llm_call"))
        .stdout(predicate::str::contains("tool_call"))
        .stdout(predicate::str::contains("cache_decision"))
        .stdout(predicate::str::contains("gpt-4o-mini"));

    // `trace show` on a specific span id returns real JSON with the
    // attributes spec §8.5 names.
    cybersin()
        .arg("--db")
        .arg(&db)
        .arg("trace")
        .arg("show")
        .arg("sess-e2e:span-2")
        .assert()
        .success()
        .stdout(predicate::str::contains("\"usd_cost\""))
        .stdout(predicate::str::contains("\"tokens_prompt\""))
        .stdout(predicate::str::contains("\"model\": \"gpt-4o-mini\""))
        .stdout(predicate::str::contains("\"evicted_sections\""));

    // `cost --by <dim>` rolls this run's real spend up along every
    // dimension the spec names.
    for dim in ["session", "agent", "model", "tool", "day"] {
        cybersin()
            .arg("--db")
            .arg(&db)
            .arg("cost")
            .arg("--by")
            .arg(dim)
            .assert()
            .success()
            .stdout(predicate::str::contains("TOTAL"))
            .stdout(predicate::str::contains("0.001245").or(predicate::str::contains("0.000")));
    }

    // Cost by model specifically names the model this run priced.
    cybersin()
        .arg("--db")
        .arg(&db)
        .arg("cost")
        .arg("--by")
        .arg("model")
        .assert()
        .success()
        .stdout(predicate::str::contains("gpt-4o-mini"));

    // Cost by tool specifically names the tool this run invoked.
    cybersin()
        .arg("--db")
        .arg(&db)
        .arg("cost")
        .arg("--by")
        .arg("tool")
        .assert()
        .success()
        .stdout(predicate::str::contains("web_search"));
}

#[test]
fn run_without_stub_fails_with_a_clear_message() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("cybersin.db");

    cybersin()
        .arg("--db")
        .arg(&db)
        .arg("run")
        .assert()
        .failure()
        .stderr(predicate::str::contains("--stub"));
}

#[test]
fn trace_ls_before_any_run_reports_no_data_instead_of_erroring() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("cybersin.db");

    cybersin()
        .arg("--db")
        .arg(&db)
        .arg("trace")
        .arg("ls")
        .assert()
        .success()
        .stdout(predicate::str::contains("no spans recorded yet"));
}
