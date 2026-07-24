#![cfg(unix)]

//! IC-3 execution-semantics checkpoint (issue #18).
//!
//! One live session drives the IC-1 compiled project through the real
//! `RuntimeDaemon` — which itself now drives a real `RouteExecutor`
//! internally (issue #33) — for both the route/cache executor and the
//! context assembler, plus a real sandbox containment boundary in the
//! same session. Earlier revisions of this test drove a second,
//! disconnected `RouteExecutor` instance alongside (not inside) the one
//! live session; now that `RuntimeDaemon::handle_llm_request` genuinely
//! calls the real executor, the cache/cascade assertions below come from
//! spans the one real session itself produced. The fake Docker CLI is
//! only a deterministic daemon stand-in: it verifies the production
//! containment flags and simulates the container's observable outcomes
//! without requiring Docker in CI.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use cybersin_adapter::messages::CallOutcome;
use cybersin_adapter::stub_harness::{CallOutcomeOrPark, StubHarness};
use cybersin_adapter::transport::stdio::in_memory_pair;
use cybersin_runtime::{DistFixture, RuntimeDaemon, RuntimeSandbox, Storage};
use cybersin_sandbox::{DockerBackend, ExecRequest, ResourceLimits, SandboxScope, WorkspaceStore};
use cybersin_trace::{CacheStatus, SpanFilter, SpanKind, SpanStatus, SpanStore};
use serde_json::json;

fn project_dist() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/ic1-research-team/dist")
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

    // First call: a cache miss. The real `RouteExecutor` inside
    // `RuntimeDaemon` walks the researcher prompt's real compiled cascade
    // cheapest-first (fast-low -> balanced-medium -> premium-high),
    // escalating past any tier whose confidence doesn't clear its
    // `minimum_score` — recorded as a real `cascade_escalation` span for
    // "fast-low" below, settling on "premium-high".
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

    // Second call: the same prompt + inputs, still inside the same
    // session — a real exact-hash cache hit against the entry the first
    // call's miss just upserted, proving the executor and context
    // assembler both ran for real within the one live session (issue
    // #18's own AC bullet 4), not stitched together from a second,
    // disconnected executor instance.
    let (_, cached_outcome) = tokio::time::timeout(
        Duration::from_secs(5),
        harness.llm_request("researcher", inputs.clone()),
    )
    .await
    .expect("cached call timed out");
    assert!(matches!(
        cached_outcome,
        CallOutcomeOrPark::Result(CallOutcome::Ok { .. })
    ));

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
