#![cfg(unix)]

//! IC-3 execution-semantics checkpoint (issue #18).
//!
//! One live session uses the IC-1 compiled project with the real route
//! executor, context assembler, trace store, and sandbox containment
//! boundary. The fake Docker CLI is only a deterministic daemon stand-in:
//! it verifies the production containment flags and simulates the
//! container's observable outcomes without requiring Docker in CI.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use cybersin_adapter::messages::CallOutcome;
use cybersin_adapter::stub_harness::{CallOutcomeOrPark, StubHarness};
use cybersin_adapter::transport::stdio::in_memory_pair;
use cybersin_router::RouteModel;
use cybersin_runtime::{
    cache_key, CacheEntry, DistFixture, ExecutionRequest, Judge, ModelCaller, ModelOutput,
    RouteExecutor, RuntimeDaemon, RuntimeSandbox, Storage,
};
use cybersin_sandbox::{DockerBackend, ExecRequest, ResourceLimits, SandboxScope, WorkspaceStore};
use cybersin_trace::{CacheStatus, SpanFilter, SpanKind, SpanStatus, SpanStore};
use serde_json::{json, Value};

fn project_dist() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/ic1-research-team/dist")
}

struct ConfidenceCaller;

#[async_trait]
impl ModelCaller for ConfidenceCaller {
    async fn call(
        &self,
        model: &RouteModel,
        _prompt_name: &str,
        _inputs: &Value,
    ) -> Result<ModelOutput, String> {
        Ok(ModelOutput {
            response: json!({"model": model.name, "answer": "evidence-backed"}),
            // Force the real high-quality route to leave its cheapest
            // tier, then accept the medium tier.
            confidence: if model.name == "fast-low" { 0.2 } else { 0.95 },
        })
    }
}

struct RejectJudge;

#[async_trait]
impl Judge for RejectJudge {
    async fn accepts(
        &self,
        _model: &RouteModel,
        _prompt_name: &str,
        _inputs: &Value,
        _cached_response: &Value,
        _similarity: f64,
    ) -> Result<bool, String> {
        Ok(false)
    }
}

fn fake_docker() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let binary = temp.path().join("docker");
    fs::write(
        &binary,
        r#"#!/bin/sh
case "$*" in
  *"--network none"*"--pids-limit"*"--read-only"*) ;;
  *) echo "containment flags missing" >&2; exit 90 ;;
esac
case "$*" in
  *"attempt-exfiltration"*) echo "network denied; payload contained" >&2; exit 42 ;;
  *) printf 'generated code completed' ;;
esac
"#,
    )
    .unwrap();
    let mut permissions = fs::metadata(&binary).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&binary, permissions).unwrap();
    (temp, binary)
}

#[tokio::test]
async fn routing_context_and_sandbox_execute_in_one_real_session() {
    let session_id = "ic3-integrated";
    let agent_name = "research-team";
    let storage: Arc<dyn Storage> =
        Arc::new(cybersin_runtime::SqliteStorage::in_memory().await.unwrap());
    let spans = SpanStore::in_memory().await.unwrap();
    let dist = Arc::new(DistFixture::load_dir(project_dist()).unwrap());

    let oversized_context = (0..124_000).map(|_| "x").collect::<Vec<_>>().join(" ");
    let inputs = json!({
        "topic": "cybernetics",
        "depth": "thorough",
        "documents": [],
        "__live_context": [{
            "id": "retrieved-over-budget",
            "priority": 5,
            "body": oversized_context
        }]
    });

    // Keep the real daemon session live while the other two execution
    // subsystems operate under the same session id and trace store.
    let (harness_io, daemon_io) = in_memory_pair();
    let mut daemon = RuntimeDaemon::new(
        daemon_io,
        storage.clone(),
        spans.clone(),
        dist,
        session_id,
        agent_name,
    );
    // Keep `session.start` small enough to fit the in-memory transport
    // before the harness begins reading; the oversized live context is
    // call-time data and belongs on `llm.request` anyway.
    daemon.start_session(json!({})).await.unwrap();
    let daemon_task = tokio::spawn(daemon.run());
    let mut harness = StubHarness::new(harness_io);
    harness.recv_session_start().await;

    let (_, context_outcome) = tokio::time::timeout(
        Duration::from_secs(5),
        harness.llm_request("researcher", inputs.clone()),
    )
    .await
    .expect("context-assembly call timed out");
    assert!(matches!(
        context_outcome,
        CallOutcomeOrPark::Result(CallOutcome::Ok { .. })
    ));

    let mut routes =
        RouteExecutor::load_dir(project_dist(), ConfidenceCaller, RejectJudge, spans.clone())
            .unwrap();
    let route_request = ExecutionRequest {
        session_id: session_id.into(),
        agent_name: agent_name.into(),
        prompt_name: "researcher".into(),
        inputs: inputs.clone(),
        embedding: vec![1.0, 0.0],
        namespace_version: "1".into(),
        bypass: false,
    };
    let routed = routes.execute(&route_request).await.unwrap();
    assert_eq!(routed.model.as_deref(), Some("balanced-medium"));
    assert!(!routed.cache_hit);

    routes.upsert_cache(CacheEntry {
        prompt_name: "researcher".into(),
        input_hash: cache_key("researcher", &inputs),
        embedding: vec![1.0, 0.0],
        response: routed.response.clone(),
    });
    let cached = routes.execute(&route_request).await.unwrap();
    assert!(cached.cache_hit);
    assert_eq!(cached.response, routed.response);

    let sandbox_root = tempfile::tempdir().unwrap();
    let workspaces = WorkspaceStore::new(sandbox_root.path()).unwrap();
    let workspace = workspaces
        .open(SandboxScope::Call, session_id, "generated-code")
        .unwrap();
    let (_runtime_dir, docker) = fake_docker();
    let sandbox = RuntimeSandbox::new(DockerBackend::with_binary(docker), spans.clone());
    let request = |command: &str| ExecRequest {
        image: "generated-tool:locked".into(),
        command: vec![command.into()],
        workspace: workspace.path().to_path_buf(),
        scope: SandboxScope::Call,
        limits: ResourceLimits::default(),
    };
    let hostile = sandbox
        .execute(
            session_id,
            agent_name,
            "hostile-generated-code",
            request("attempt-exfiltration"),
        )
        .await
        .unwrap();
    assert!(!hostile.succeeded());
    let healthy = sandbox
        .execute(
            session_id,
            agent_name,
            "healthy-generated-code",
            request("write-report"),
        )
        .await
        .unwrap();
    assert!(
        healthy.succeeded(),
        "session should survive hostile payload"
    );

    harness
        .session_complete(session_id, json!({"status": "ok"}))
        .await;
    tokio::time::timeout(Duration::from_secs(5), harness.wait_for_close())
        .await
        .expect("daemon did not close the completed session");
    assert!(
        tokio::time::timeout(Duration::from_secs(5), daemon_task)
            .await
            .expect("daemon task did not finish")
            .unwrap()
            .unwrap()
            .completed
    );

    let recorded = spans
        .list(&SpanFilter {
            session_id: Some(session_id.into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(recorded.iter().any(|span| {
        span.kind == SpanKind::LlmCall
            && span.evicted_sections == vec!["retrieved-over-budget".to_string()]
    }));
    assert!(recorded.iter().any(|span| {
        span.attributes["decision"] == "cascade_escalation"
            && span.model.as_deref() == Some("fast-low")
            && span.usd_cost > 0.0
    }));
    assert!(recorded.iter().any(|span| {
        span.kind == SpanKind::CacheDecision
            && span.cache_status == CacheStatus::Hit
            && span.usd_cost == 0.0
    }));
    assert!(recorded.iter().any(|span| {
        span.kind == SpanKind::SandboxExec
            && matches!(span.status, SpanStatus::Error { .. })
            && span.attributes["contained"] == true
    }));
    assert!(recorded
        .iter()
        .any(|span| { span.kind == SpanKind::SandboxExec && span.status == SpanStatus::Ok }));
}
