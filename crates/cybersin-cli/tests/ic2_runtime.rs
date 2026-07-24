//! IC-2 compile/runtime integration checkpoint (issue #14).
//!
//! This suite deliberately loads `fixtures/ic1-research-team/dist`, the
//! byte-for-byte compiler output committed by IC-1. It never substitutes
//! the older hand-written runtime fixture.

use std::path::PathBuf;
use std::sync::Arc;

use cybersin_adapter::messages::CallOutcome;
use cybersin_adapter::stub_harness::{CallOutcomeOrPark, StubHarness};
use cybersin_adapter::transport::stdio::in_memory_pair;
use cybersin_gateway::{EchoExecutor, GatewayOutcome, RetryClass, ToolGateway};
use cybersin_runtime::{
    stub_agent, BudgetConfig, DaemonHandle, DistFixture, OnBreach, RuntimeDaemon,
    SessionSupervisor, Storage,
};
use cybersin_trace::{SpanFilter, SpanKind, SpanStore};
use serde_json::json;

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/ic1-research-team")
}

fn compiled_dist() -> Arc<DistFixture> {
    Arc::new(
        DistFixture::load_dir(project_root().join("dist"))
            .expect("IC-1's committed compiler output must load directly"),
    )
}

fn researcher_inputs() -> serde_json::Value {
    json!({ "topic": "cybernetics", "depth": "quick", "documents": [] })
}

#[tokio::test]
async fn ic1_compiler_output_runs_through_the_real_daemon() {
    let storage: Arc<dyn Storage> =
        Arc::new(cybersin_runtime::SqliteStorage::in_memory().await.unwrap());
    let spans = SpanStore::in_memory().await.unwrap();

    let summary = stub_agent::run_stub_session(
        storage.clone(),
        spans.clone(),
        compiled_dist(),
        "ic2-real-dist",
        "research-team",
    )
    .await
    .unwrap();

    assert!(summary.completed);
    assert_eq!(summary.spans_recorded, 5);
    let llm_spans = spans
        .list(&SpanFilter {
            session_id: Some("ic2-real-dist".into()),
            kind: Some(SpanKind::LlmCall),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(llm_spans.len(), 2);
    assert!(llm_spans
        .iter()
        .all(|span| span.model.as_deref() == Some("premium-high")));
    assert!(llm_spans.iter().any(|span| span.usd_cost > 0.0));
    assert!(llm_spans.iter().any(|span| span.usd_cost == 0.0));
}

#[tokio::test]
async fn crash_resume_replays_zero_successful_tool_calls() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("cybersin.db");
    let dist = compiled_dist();
    let idem_key = "ic2-crash:tool-1";

    {
        let daemon = DaemonHandle::auto_start(&db).await.unwrap();
        daemon
            .storage()
            .create_session_pinned("ic2-crash", "research-team", &dist.manifest.build_hash)
            .await
            .unwrap();
        let gateway = ToolGateway::new(daemon.storage(), Arc::new(EchoExecutor));
        let first = gateway
            .call(
                "ic2-crash",
                "web_search",
                json!({"query": "cybernetics"}),
                Some(idem_key.into()),
                RetryClass::Read,
            )
            .await
            .unwrap();
        assert!(matches!(
            first,
            GatewayOutcome::Resolved(CallOutcome::Ok { .. })
        ));
        SessionSupervisor::new(daemon.storage())
            .kill("ic2-crash")
            .await
            .unwrap();
        // Dropping every handle simulates the process boundary: only the
        // SQLite event log and idempotency ledger survive.
    }

    let restarted = DaemonHandle::auto_start(&db).await.unwrap();
    SessionSupervisor::new(restarted.storage())
        .resume("ic2-crash", &dist.manifest.build_hash)
        .await
        .unwrap();
    let gateway = ToolGateway::new(restarted.storage(), Arc::new(EchoExecutor));
    let replay = gateway
        .call(
            "ic2-crash",
            "web_search",
            json!({"query": "cybernetics"}),
            Some(idem_key.into()),
            RetryClass::Read,
        )
        .await
        .unwrap();
    assert!(matches!(
        replay,
        GatewayOutcome::Resolved(CallOutcome::Ok { .. })
    ));

    let row = restarted
        .storage()
        .get_tool_call(&format!("web_search:{idem_key}"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.attempts, 1, "resume must not execute the tool again");
    assert_eq!(
        restarted
            .storage()
            .count_tool_calls_for_session("ic2-crash")
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn real_compiled_route_degrades_to_its_cheapest_cascade_step() {
    let storage: Arc<dyn Storage> =
        Arc::new(cybersin_runtime::SqliteStorage::in_memory().await.unwrap());
    let spans = SpanStore::in_memory().await.unwrap();
    let dist = compiled_dist();
    assert_eq!(dist.routing("researcher").unwrap().model, "premium-high");
    assert_eq!(dist.cascade("researcher")[0].model, "fast-low");

    let (harness_io, daemon_io) = in_memory_pair();
    let mut daemon = RuntimeDaemon::new(
        daemon_io,
        storage,
        spans.clone(),
        dist,
        "ic2-degrade",
        "research-team",
    )
    .with_budget(BudgetConfig {
        usd_per_session: 0.0,
        on_breach: OnBreach::Degrade,
    });
    daemon.start_session(researcher_inputs()).await.unwrap();
    let daemon_task = tokio::spawn(daemon.run());

    let mut harness = StubHarness::new(harness_io);
    harness.recv_session_start().await;
    let (_, outcome) = harness.llm_request("researcher", researcher_inputs()).await;
    assert!(matches!(
        outcome,
        CallOutcomeOrPark::Result(CallOutcome::Ok { .. })
    ));
    harness
        .session_complete("ic2-degrade", json!({"status": "ok"}))
        .await;
    harness.wait_for_close().await;
    daemon_task.await.unwrap().unwrap();

    let llm_spans = spans
        .list(&SpanFilter {
            session_id: Some("ic2-degrade".into()),
            kind: Some(SpanKind::LlmCall),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(llm_spans[0].model.as_deref(), Some("fast-low"));
}

/// Issue #31: `publish_report`'s `class: critical` / `approval: required`
/// policy is read from the *real compiled* `dist/tools.json` — via
/// `dist.tool_policy`, the same lookup `RuntimeDaemon::handle_tool_request`
/// uses — and the parking scenario is driven through that real daemon over
/// the wire, not through a hand-built `ToolGateway` + `ApprovalGate`
/// standing in for it.
#[tokio::test]
async fn real_projects_critical_tool_parks_and_resumes_on_approval() {
    let dist = compiled_dist();
    let policy = dist
        .tool_policy("publish_report")
        .expect("compiled dist/tools.json must declare publish_report's policy");
    assert!(policy.requires_approval());
    assert_eq!(policy.retry_class, "critical");

    let storage: Arc<dyn Storage> =
        Arc::new(cybersin_runtime::SqliteStorage::in_memory().await.unwrap());
    let spans = SpanStore::in_memory().await.unwrap();

    let (harness_io, daemon_io) = in_memory_pair();
    let mut daemon = RuntimeDaemon::new(
        daemon_io,
        storage.clone(),
        spans,
        dist,
        "ic2-approval",
        "research-team",
    );
    daemon.start_session(researcher_inputs()).await.unwrap();
    let daemon_task = tokio::spawn(daemon.run());

    let mut harness = StubHarness::new(harness_io);
    harness.recv_session_start().await;

    let (call_id, outcome) = harness
        .tool_request("publish_report", json!({"report": "evidence-backed"}), None)
        .await;
    let approval_id = match outcome {
        CallOutcomeOrPark::Parked(approval_id) => approval_id,
        other => panic!("critical project tool should park, got {other:?}"),
    };
    assert_eq!(
        storage
            .get_session("ic2-approval")
            .await
            .unwrap()
            .unwrap()
            .status,
        "awaiting_approval"
    );

    // Simulate `cybersin approve <call-id>` issued from a separate
    // process: a fresh gateway bound to the same durable storage, exactly
    // as `park_for_approval`'s doc comment describes.
    let approving_gateway = ToolGateway::new(storage.clone(), Arc::new(EchoExecutor));
    let resumed = approving_gateway.approve(&approval_id).await.unwrap();
    assert!(matches!(
        resumed,
        GatewayOutcome::Resolved(CallOutcome::Ok { .. })
    ));

    let result = harness.await_result(&call_id).await;
    assert!(matches!(
        result,
        CallOutcomeOrPark::Result(CallOutcome::Ok { .. })
    ));

    harness
        .session_complete("ic2-approval", json!({"status": "ok"}))
        .await;
    harness.wait_for_close().await;
    daemon_task.await.unwrap().unwrap();

    assert_eq!(
        storage
            .get_session("ic2-approval")
            .await
            .unwrap()
            .unwrap()
            .status,
        "completed"
    );
    assert_eq!(
        storage
            .get_tool_call(&approval_id)
            .await
            .unwrap()
            .unwrap()
            .attempts,
        1
    );
}
