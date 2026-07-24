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
        // The bundled stub fixture's `cascade.json` declares a cheaper
        // "gpt-4o-nano" alternate ahead of the default "gpt-4o-mini": the
        // real route executor (issue #33) now genuinely escalates past it
        // before settling on the default — 2 real model-call spans for
        // the miss, not one.
        .stdout(predicate::str::contains("6 spans recorded"));

    assert!(
        db.exists(),
        "auto-start should have created the sqlite state file"
    );

    // `trace ls` sees the real spans this run recorded, scoped to the
    // session id.
    let ls = cybersin()
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
        .stdout(predicate::str::contains("gpt-4o-mini"))
        .get_output()
        .stdout
        .clone();
    let ls = String::from_utf8(ls).unwrap();
    // The real route executor (issue #33) writes `llm_call`/`cache_decision`
    // spans with their own `route-<session>-<nanos>-<seq>` ids rather than
    // the daemon's own `<session>:span-<n>` sequence (still used for tool
    // calls only), so this test finds a real `llm_call` span id from `ls`
    // instead of assuming a fixed one.
    let llm_call_span_id = ls
        .lines()
        .find(|line| line.contains("llm_call") && line.contains("gpt-4o-mini"))
        .and_then(|line| line.split_whitespace().next())
        .expect("an llm_call span line with an id")
        .to_string();

    // `trace show` on a specific span id returns real JSON with the
    // attributes spec §8.5 names.
    cybersin()
        .arg("--db")
        .arg(&db)
        .arg("trace")
        .arg("show")
        .arg(&llm_call_span_id)
        .assert()
        .success()
        .stdout(predicate::str::contains("\"usd_cost\""))
        .stdout(predicate::str::contains("\"tokens_prompt\""))
        .stdout(predicate::str::contains("\"model\": \"gpt-4o-mini\""))
        .stdout(predicate::str::contains("\"evicted_sections\""));

    // `cost --by <dim>` rolls this run's real spend up along every
    // dimension the spec names. Total now includes the real cascade
    // escalation's extra model-call cost (issue #33: gpt-4o-nano $0.000108
    // + gpt-4o-mini $0.000432 + the tool call's $0.0008 = $0.001340),
    // not just one flat per-request price.
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
            .stdout(predicate::str::contains("0.001340").or(predicate::str::contains("0.000")));
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
