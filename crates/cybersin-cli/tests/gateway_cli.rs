//! End-to-end CLI proof for issue #11's acceptance criteria: `cybersin
//! dlq ls|show|retry|drop` and `cybersin approve|deny <call-id>`, driven
//! through the actual compiled `cybersin` binary (spec §8.2, §11).
//!
//! There's no real tool backend wired into this workspace yet to produce
//! ledger rows through a full agent run (that's a later issue), so each
//! test seeds the shared sqlite file directly through the
//! `cybersin-gateway`/`cybersin-runtime` libraries first — exactly the
//! same ledger a real session would produce — then drives the CLI
//! subprocess against that same file for the commands under test.

use std::path::Path;
use std::sync::Arc;

use assert_cmd::Command;
use async_trait::async_trait;
use cybersin_gateway::{ApprovalGate, EchoExecutor, RetryClass, ToolExecutor, ToolGateway};
use cybersin_runtime::DaemonHandle;
use predicates::prelude::*;
use serde_json::json;

fn cybersin() -> Command {
    Command::cargo_bin("cybersin").expect("find cybersin binary")
}

struct AlwaysFailExecutor(&'static str);

#[async_trait]
impl ToolExecutor for AlwaysFailExecutor {
    async fn execute(
        &self,
        _tool: &str,
        _args: &serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        Err(self.0.to_string())
    }
}

async fn seed_failed_call(db: &Path, session_id: &str, call_seed: &str) -> String {
    let daemon = DaemonHandle::auto_start(db).await.unwrap();
    daemon
        .storage()
        .create_session(session_id, "agent-a")
        .await
        .unwrap();
    let gateway = ToolGateway::new(
        daemon.storage(),
        Arc::new(AlwaysFailExecutor("connection refused")),
    );
    gateway
        .call(
            session_id,
            "charge_card",
            json!({"amount": 500}),
            Some(call_seed.to_string()),
            RetryClass::Critical,
        )
        .await
        .unwrap();
    format!("charge_card:{call_seed}")
}

#[tokio::test]
async fn dlq_ls_show_retry_drop_work_against_a_deliberately_failed_call() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("cybersin.db");

    let call_id = seed_failed_call(&db, "sess-1", "charge-1").await;

    cybersin()
        .arg("--db")
        .arg(&db)
        .arg("dlq")
        .arg("ls")
        .assert()
        .success()
        .stdout(predicate::str::contains("charge_card:charge-1"))
        .stdout(predicate::str::contains("critical"));

    cybersin()
        .arg("--db")
        .arg(&db)
        .arg("dlq")
        .arg("show")
        .arg(&call_id)
        .assert()
        .success()
        .stdout(predicate::str::contains("\"status\": \"failed\""))
        .stdout(predicate::str::contains("connection refused"));

    // `dlq retry` runs the CLI's own stand-in executor (EchoExecutor),
    // which always succeeds — proving retry actually re-executes rather
    // than just flipping a status bit.
    cybersin()
        .arg("--db")
        .arg(&db)
        .arg("dlq")
        .arg("retry")
        .arg(&call_id)
        .assert()
        .success()
        .stdout(predicate::str::contains("succeeded"));

    cybersin()
        .arg("--db")
        .arg(&db)
        .arg("dlq")
        .arg("ls")
        .assert()
        .success()
        .stdout(predicate::str::contains("no dead letters"));

    // Seed a second failure to exercise `drop`.
    let call_id_2 = seed_failed_call(&db, "sess-1", "charge-2").await;
    cybersin()
        .arg("--db")
        .arg(&db)
        .arg("dlq")
        .arg("drop")
        .arg(&call_id_2)
        .assert()
        .success()
        .stdout(predicate::str::contains("dropped"));

    cybersin()
        .arg("--db")
        .arg(&db)
        .arg("dlq")
        .arg("ls")
        .assert()
        .success()
        .stdout(predicate::str::contains("no dead letters"));
}

#[tokio::test]
async fn dlq_ls_before_any_failure_reports_no_data_instead_of_erroring() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("cybersin.db");

    cybersin()
        .arg("--db")
        .arg(&db)
        .arg("dlq")
        .arg("ls")
        .assert()
        .success()
        .stdout(predicate::str::contains("no dead letters"));
}

async fn seed_parked_call(db: &Path, session_id: &str, call_seed: &str) -> String {
    let daemon = DaemonHandle::auto_start(db).await.unwrap();
    daemon
        .storage()
        .create_session(session_id, "agent-a")
        .await
        .unwrap();
    let gateway = ToolGateway::new(daemon.storage(), Arc::new(EchoExecutor))
        .with_policy_hook(Arc::new(ApprovalGate::for_tools(["wire_transfer"])));
    gateway
        .call(
            session_id,
            "wire_transfer",
            json!({"amount": 10_000}),
            Some(call_seed.to_string()),
            RetryClass::Write,
        )
        .await
        .unwrap();
    format!("wire_transfer:{call_seed}")
}

#[tokio::test]
async fn approve_resumes_the_parked_session_via_the_cli() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("cybersin.db");

    let call_id = seed_parked_call(&db, "sess-1", "wt-1").await;

    {
        let daemon = DaemonHandle::auto_start(&db).await.unwrap();
        let session = daemon
            .storage()
            .get_session("sess-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(session.status, "awaiting_approval");
    }

    cybersin()
        .arg("--db")
        .arg(&db)
        .arg("approve")
        .arg(&call_id)
        .assert()
        .success()
        .stdout(predicate::str::contains("succeeded"));

    let daemon = DaemonHandle::auto_start(&db).await.unwrap();
    let session = daemon
        .storage()
        .get_session("sess-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(session.status, "running", "approval resumes the session");

    let row = daemon
        .storage()
        .get_tool_call(&call_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.status, "succeeded");
    assert!(!row.awaiting_approval);
}

#[tokio::test]
async fn deny_resolves_failed_denied_without_killing_the_session_via_the_cli() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("cybersin.db");

    let call_id = seed_parked_call(&db, "sess-1", "wt-2").await;

    cybersin()
        .arg("--db")
        .arg(&db)
        .arg("deny")
        .arg(&call_id)
        .assert()
        .success()
        .stdout(predicate::str::contains("failed"))
        .stdout(predicate::str::contains("denied"));

    let daemon = DaemonHandle::auto_start(&db).await.unwrap();
    let session = daemon
        .storage()
        .get_session("sess-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(session.status, "running", "deny does not kill the session");

    let row = daemon
        .storage()
        .get_tool_call(&call_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.status, "failed");
    assert_eq!(row.failure_reason.as_deref(), Some("denied"));
    assert_eq!(row.retriable, Some(false));
}

#[tokio::test]
async fn approve_on_a_call_that_is_not_parked_fails_clearly() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("cybersin.db");
    let call_id = seed_failed_call(&db, "sess-1", "charge-1").await;

    cybersin()
        .arg("--db")
        .arg(&db)
        .arg("approve")
        .arg(&call_id)
        .assert()
        .failure()
        .stderr(predicate::str::contains("not awaiting approval"));
}
