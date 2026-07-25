use std::path::{Path, PathBuf};
use std::sync::Arc;

use assert_cmd::Command;
use async_trait::async_trait;
use cybersin_gateway::{RetryClass, ToolExecutor, ToolGateway};
use cybersin_runtime::DaemonHandle;
use predicates::prelude::*;
use serde_json::json;

struct AlwaysFail;

#[async_trait]
impl ToolExecutor for AlwaysFail {
    async fn execute(
        &self,
        _session_id: &str,
        _call_id: &str,
        _tool: &str,
        _args: &serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        Err("seed failure".into())
    }
}

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/ic1-research-team")
}

async fn seed_failed_citation_call(db: &Path) -> String {
    let daemon = DaemonHandle::auto_start(db).await.unwrap();
    daemon
        .storage()
        .create_session("live-tool-session", "research-team")
        .await
        .unwrap();
    let gateway = ToolGateway::new(daemon.storage(), Arc::new(AlwaysFail));
    gateway
        .call(
            "live-tool-session",
            "citation_lookup",
            json!({"citation": "C-1"}),
            Some("live-1".into()),
            RetryClass::Critical,
        )
        .await
        .unwrap();
    "citation_lookup:live-1".into()
}

#[tokio::test]
#[ignore = "requires a live Docker daemon and python:3.12-slim"]
async fn compiled_custom_tool_executes_in_a_real_container() {
    // Docker Desktop only bind-mounts host paths from shared project
    // roots; keep this live test's workspace under the checkout instead
    // of the platform temp directory.
    let temp = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let db = temp.path().join("cybersin.db");
    let call_id = seed_failed_citation_call(&db).await;

    Command::cargo_bin("cybersin")
        .unwrap()
        .arg("--db")
        .arg(&db)
        .arg("--dist")
        .arg(fixture().join("dist"))
        .args(["--sandbox-backend", "docker"])
        .arg("--sandbox-root")
        .arg(temp.path().join("sandbox"))
        .args(["dlq", "retry"])
        .arg(call_id)
        .assert()
        .success()
        .stdout(predicate::str::contains("succeeded"))
        .stdout(predicate::str::contains("\"citation\":\"C-1\""))
        .stdout(predicate::str::contains("\"found\":true"));
}
