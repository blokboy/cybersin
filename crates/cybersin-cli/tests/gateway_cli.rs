//! End-to-end CLI proof for issue #11's acceptance criteria: `cybersin
//! dlq ls|show|retry|drop` and `cybersin approve|deny <call-id>`, driven
//! through the actual compiled `cybersin` binary (spec §8.2, §11).
//!
//! A full agent run is still Phase 3, so each test seeds the shared
//! SQLite ledger through `cybersin-gateway`/`cybersin-runtime`, then
//! drives the CLI subprocess against that same database. Retry and
//! approval execute through the real sandboxed CLI executor.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use assert_cmd::Command;
use async_trait::async_trait;
use cybersin_gateway::{ApprovalGate, EchoExecutor, RetryClass, ToolExecutor, ToolGateway};
use cybersin_runtime::DaemonHandle;
use predicates::prelude::*;
use serde_json::json;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

fn cybersin() -> Command {
    Command::cargo_bin("cybersin").expect("find cybersin binary")
}

fn copy_tree(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let target = destination.join(entry.file_name());
        if entry.path().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

#[cfg(unix)]
fn executable_tool_fixture(root: &Path, tool: &str) -> (PathBuf, PathBuf) {
    let source =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../cybersin-runtime/fixtures/dist");
    let dist = root.join("dist");
    copy_tree(&source, &dist);
    fs::write(
        dist.join("tools.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            tool: {
                "retry_class": "critical",
                "image": "python:3.12-slim",
                "run": ["python3", format!("{tool}.py")],
                "sandbox_scope": "call",
                "egress": [],
                "cpu": 1.0,
                "mem_mb": 64,
                "wall_s": 10
            }
        }))
        .unwrap(),
    )
    .unwrap();
    fs::create_dir_all(dist.join("tools")).unwrap();
    fs::write(
        dist.join("tools").join(format!("{tool}.py")),
        "print('ok')\n",
    )
    .unwrap();

    let runtime = root.join("docker");
    fs::write(&runtime, "#!/bin/sh\nprintf '{\"executed\":true}'\n").unwrap();
    let mut permissions = fs::metadata(&runtime).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&runtime, permissions).unwrap();
    (dist, runtime)
}

struct AlwaysFailExecutor(&'static str);

#[async_trait]
impl ToolExecutor for AlwaysFailExecutor {
    async fn execute(
        &self,
        _session_id: &str,
        _call_id: &str,
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
#[cfg(unix)]
async fn dlq_ls_show_retry_drop_work_against_a_deliberately_failed_call() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("cybersin.db");
    let (dist, runtime) = executable_tool_fixture(dir.path(), "charge_card");

    let call_id = seed_failed_call(&db, "sess-1", "charge-1").await;

    cybersin()
        .env("CYBERSIN_CONTAINER_RUNTIME", &runtime)
        .arg("--db")
        .arg(&db)
        .arg("--dist")
        .arg(&dist)
        .args(["--sandbox-backend", "docker"])
        .arg("--sandbox-root")
        .arg(dir.path().join("sandbox"))
        .arg("dlq")
        .arg("ls")
        .assert()
        .success()
        .stdout(predicate::str::contains("charge_card:charge-1"))
        .stdout(predicate::str::contains("critical"));

    cybersin()
        .arg("--db")
        .arg(&db)
        .arg("--dist")
        .arg(cybersin_runtime::bundled_stub_dist_dir())
        .arg("dlq")
        .arg("show")
        .arg(&call_id)
        .assert()
        .success()
        .stdout(predicate::str::contains("\"status\": \"failed\""))
        .stdout(predicate::str::contains("connection refused"));

    // `dlq retry` runs the compiled custom tool through the selected
    // sandbox backend, proving retry actually re-executes rather than
    // just flipping a status bit.
    cybersin()
        .env("CYBERSIN_CONTAINER_RUNTIME", &runtime)
        .arg("--db")
        .arg(&db)
        .arg("--dist")
        .arg(&dist)
        .args(["--sandbox-backend", "docker"])
        .arg("--sandbox-root")
        .arg(dir.path().join("sandbox"))
        .arg("dlq")
        .arg("retry")
        .arg(&call_id)
        .assert()
        .success()
        .stdout(predicate::str::contains("succeeded"));

    cybersin()
        .arg("--db")
        .arg(&db)
        .arg("--dist")
        .arg(cybersin_runtime::bundled_stub_dist_dir())
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
        .arg("--dist")
        .arg(cybersin_runtime::bundled_stub_dist_dir())
        .arg("dlq")
        .arg("drop")
        .arg(&call_id_2)
        .assert()
        .success()
        .stdout(predicate::str::contains("dropped"));

    cybersin()
        .arg("--db")
        .arg(&db)
        .arg("--dist")
        .arg(cybersin_runtime::bundled_stub_dist_dir())
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
        .arg("--dist")
        .arg(cybersin_runtime::bundled_stub_dist_dir())
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
#[cfg(unix)]
async fn approve_resumes_the_parked_session_via_the_cli() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("cybersin.db");
    let (dist, runtime) = executable_tool_fixture(dir.path(), "wire_transfer");

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
        .env("CYBERSIN_CONTAINER_RUNTIME", runtime)
        .arg("--db")
        .arg(&db)
        .arg("--dist")
        .arg(dist)
        .args(["--sandbox-backend", "docker"])
        .arg("--sandbox-root")
        .arg(dir.path().join("sandbox"))
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
        .arg("--dist")
        .arg(cybersin_runtime::bundled_stub_dist_dir())
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
        .arg("--dist")
        .arg(cybersin_runtime::bundled_stub_dist_dir())
        .arg("approve")
        .arg(&call_id)
        .assert()
        .failure()
        .stderr(predicate::str::contains("not awaiting approval"));
}
