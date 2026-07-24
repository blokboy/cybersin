//! End-to-end proof of issue #13's four acceptance criteria (spec §8.5,
//! §8.2), driven through the real [`RuntimeDaemon`] session loop and a
//! scripted [`StubHarness`] exactly the way `src/stub_agent.rs` drives the
//! M1 stub scenario — not a lower-level unit test of the pieces, but the
//! actual harness↔daemon protocol exchange a real adapter would see:
//!
//! 1. `on_breach: degrade` re-routes to the cheapest declared cascade step.
//! 2. `on_breach: halt` aborts the session cleanly with a distinct status.
//! 3. `on_breach: ask` parks the session behind an approval gate.
//! 4. A critical tool call with `approval: required` also parks the
//!    session, and `cybersin approve <call-id>` (here: `ToolGateway::approve`,
//!    exactly what that CLI command calls — see
//!    `cybersin-cli/src/commands/approval.rs`) resumes it — durably, i.e.
//!    still correct after the parking process is gone and a completely
//!    fresh one reconstructs from the same on-disk `Storage`.

use std::sync::Arc;

use cybersin_adapter::messages::{AbortReason, CallOutcome};
use cybersin_adapter::stub_harness::{CallOutcomeOrPark, StubHarness};
use cybersin_adapter::transport::stdio::in_memory_pair;
use cybersin_gateway::{EchoExecutor, ToolGateway};
use cybersin_runtime::{
    bundled_stub_dist_dir, BudgetConfig, DaemonHandle, DistFixture, OnBreach, RuntimeDaemon,
    SessionSupervisor, Storage,
};
use cybersin_trace::{SpanFilter, SpanKind, SpanStore};
use serde_json::json;

fn researcher_inputs() -> serde_json::Value {
    json!({ "topic": "cybernetics", "depth": "quick", "documents": [] })
}

/// A budget that's already breached before the very first call: `spent
/// (0.0) < usd_per_session (0.0)` is false, so enforcement fires on the
/// first `llm.request` — the minimal setup that exercises `on_breach`
/// without needing a second call just to cross the threshold.
fn already_breached(on_breach: OnBreach) -> BudgetConfig {
    BudgetConfig {
        usd_per_session: 0.0,
        on_breach,
    }
}

#[tokio::test]
async fn degrade_falls_back_to_the_cheapest_cascade_step() {
    let storage: Arc<dyn Storage> =
        Arc::new(cybersin_runtime::SqliteStorage::in_memory().await.unwrap());
    let spans = SpanStore::in_memory().await.unwrap();
    let dist = Arc::new(DistFixture::load_dir(bundled_stub_dist_dir()).unwrap());
    // Sanity: the fixture actually declares a cheaper cascade step than
    // its default routing entry, or this test would prove nothing.
    assert_eq!(dist.routing("researcher").unwrap().model, "gpt-4o-mini");
    assert_eq!(dist.cascade("researcher")[0].model, "gpt-4o-nano");

    let (harness_io, daemon_io) = in_memory_pair();
    let mut daemon = RuntimeDaemon::new(
        daemon_io,
        storage.clone(),
        spans.clone(),
        dist,
        "sess-degrade",
        "agent-a",
    )
    .with_budget(already_breached(OnBreach::Degrade));
    daemon.start_session(json!({})).await.unwrap();
    let daemon_task = tokio::spawn(daemon.run());

    let mut harness = StubHarness::new(harness_io);
    harness.recv_session_start().await;

    let (_call_id, outcome) = harness.llm_request("researcher", researcher_inputs()).await;
    assert!(
        matches!(outcome, CallOutcomeOrPark::Result(CallOutcome::Ok { .. })),
        "a degrade breach still answers the call: {outcome:?}"
    );

    harness
        .session_complete("sess-degrade", json!({"status": "ok"}))
        .await;
    harness.wait_for_close().await;
    let summary = daemon_task.await.unwrap().unwrap();
    assert!(summary.completed);
    assert!(!summary.halted);

    let llm_spans = spans
        .list(&SpanFilter {
            session_id: Some("sess-degrade".to_string()),
            kind: Some(SpanKind::LlmCall),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(llm_spans.len(), 1);
    assert_eq!(
        llm_spans[0].model.as_deref(),
        Some("gpt-4o-nano"),
        "degrade should have re-routed to the cheapest cascade step, not the default model"
    );

    let session = storage.get_session("sess-degrade").await.unwrap().unwrap();
    assert_eq!(session.status, "completed");
}

#[tokio::test]
async fn halt_aborts_the_session_cleanly_with_a_distinct_terminal_status() {
    let storage: Arc<dyn Storage> =
        Arc::new(cybersin_runtime::SqliteStorage::in_memory().await.unwrap());
    let spans = SpanStore::in_memory().await.unwrap();
    let dist = Arc::new(DistFixture::load_dir(bundled_stub_dist_dir()).unwrap());

    let (harness_io, daemon_io) = in_memory_pair();
    let mut daemon = RuntimeDaemon::new(
        daemon_io,
        storage.clone(),
        spans.clone(),
        dist,
        "sess-halt",
        "agent-a",
    )
    .with_budget(already_breached(OnBreach::Halt));
    daemon.start_session(json!({})).await.unwrap();
    let daemon_task = tokio::spawn(daemon.run());

    let mut harness = StubHarness::new(harness_io);
    harness.recv_session_start().await;

    let (_call_id, outcome) = harness.llm_request("researcher", researcher_inputs()).await;
    match outcome {
        CallOutcomeOrPark::Aborted(AbortReason::BudgetHalt {
            usd_spent,
            usd_budget,
        }) => {
            assert_eq!(usd_spent, 0.0);
            assert_eq!(usd_budget, 0.0);
        }
        other => panic!("expected a budget_halt session.abort, got {other:?}"),
    }

    let summary = daemon_task.await.unwrap().unwrap();
    assert!(
        summary.halted,
        "halt should not report the run as completed"
    );
    assert!(!summary.completed);
    // No LLM call ever executed — halt fires before pricing/emitting spans.
    assert_eq!(summary.spans_recorded, 0);

    let session = storage.get_session("sess-halt").await.unwrap().unwrap();
    assert_eq!(
        session.status, "halted",
        "halt gets its own inspectable status, distinct from the generic \"aborted\""
    );
}

#[tokio::test]
async fn ask_parks_the_session_and_approving_lets_the_call_proceed() {
    let storage: Arc<dyn Storage> =
        Arc::new(cybersin_runtime::SqliteStorage::in_memory().await.unwrap());
    let spans = SpanStore::in_memory().await.unwrap();
    let dist = Arc::new(DistFixture::load_dir(bundled_stub_dist_dir()).unwrap());

    let (harness_io, daemon_io) = in_memory_pair();
    let mut daemon = RuntimeDaemon::new(
        daemon_io,
        storage.clone(),
        spans.clone(),
        dist,
        "sess-ask",
        "agent-a",
    )
    .with_budget(already_breached(OnBreach::Ask));
    daemon.start_session(json!({})).await.unwrap();
    let daemon_task = tokio::spawn(daemon.run());

    let mut harness = StubHarness::new(harness_io);
    harness.recv_session_start().await;

    let (call_id, outcome) = harness.llm_request("researcher", researcher_inputs()).await;
    let approval_id = match outcome {
        CallOutcomeOrPark::Parked(approval_id) => approval_id,
        other => panic!("expected the call to park pending approval, got {other:?}"),
    };

    // Durable parking, observable independent of the harness: the session
    // is awaiting_approval and the ledger row backing `approval_id`
    // matches — the exact shape `cybersin sessions show`/`dlq show` would
    // report.
    let session = storage.get_session("sess-ask").await.unwrap().unwrap();
    assert_eq!(session.status, "awaiting_approval");
    let row = storage.get_tool_call(&approval_id).await.unwrap().unwrap();
    assert!(row.awaiting_approval);
    assert_eq!(row.status, "pending");
    assert_eq!(row.tool, "budget");
    assert_eq!(row.retry_class, "critical");

    // `cybersin approve <call-id>` is exactly `ToolGateway::approve` (see
    // `cybersin-cli/src/commands/approval.rs`) — call it the same way the
    // CLI does, against the same storage, from what's logically a
    // separate process/command invocation.
    let gateway = ToolGateway::new(storage.clone(), Arc::new(EchoExecutor));
    gateway.approve(&approval_id).await.unwrap();

    // The parked `llm.request` never got its `call.result` the first
    // time around (it parked instead) — poll the same call_id again, per
    // `StubHarness::await_result`'s own doc, for the eventual resolution.
    let resumed = harness.await_result(&call_id).await;
    assert!(
        matches!(resumed, CallOutcomeOrPark::Result(CallOutcome::Ok { .. })),
        "approving the budget ask should let the original llm.request complete: {resumed:?}"
    );

    harness
        .session_complete("sess-ask", json!({"status": "ok"}))
        .await;
    harness.wait_for_close().await;
    let summary = daemon_task.await.unwrap().unwrap();
    assert!(summary.completed);

    let session = storage.get_session("sess-ask").await.unwrap().unwrap();
    assert_eq!(session.status, "completed");
}

#[tokio::test]
async fn critical_tool_call_with_required_approval_parks_the_session() {
    let storage: Arc<dyn Storage> =
        Arc::new(cybersin_runtime::SqliteStorage::in_memory().await.unwrap());
    let spans = SpanStore::in_memory().await.unwrap();
    let dist = Arc::new(DistFixture::load_dir(bundled_stub_dist_dir()).unwrap());
    assert!(
        dist.tool_policy("wire_transfer")
            .unwrap()
            .requires_approval(),
        "fixture sanity: wire_transfer must be the approval-gated tool"
    );

    let (harness_io, daemon_io) = in_memory_pair();
    let mut daemon = RuntimeDaemon::new(
        daemon_io,
        storage.clone(),
        spans.clone(),
        dist,
        "sess-tool-approval",
        "agent-a",
    );
    daemon.start_session(json!({})).await.unwrap();
    let daemon_task = tokio::spawn(daemon.run());

    let mut harness = StubHarness::new(harness_io);
    harness.recv_session_start().await;

    let (call_id, outcome) = harness
        .tool_request("wire_transfer", json!({"amount": 10_000}), None)
        .await;
    let approval_id = match outcome {
        CallOutcomeOrPark::Parked(approval_id) => approval_id,
        other => panic!("expected the critical call to park, got {other:?}"),
    };
    assert!(approval_id.starts_with("wire_transfer:"));

    let session = storage
        .get_session("sess-tool-approval")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(session.status, "awaiting_approval");

    let gateway = ToolGateway::new(storage.clone(), Arc::new(EchoExecutor));
    gateway.approve(&approval_id).await.unwrap();

    let resumed = harness.await_result(&call_id).await;
    assert!(matches!(
        resumed,
        CallOutcomeOrPark::Result(CallOutcome::Ok { .. })
    ));

    harness
        .session_complete("sess-tool-approval", json!({"status": "ok"}))
        .await;
    harness.wait_for_close().await;
    let summary = daemon_task.await.unwrap().unwrap();
    assert!(summary.completed);

    let session = storage
        .get_session("sess-tool-approval")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(session.status, "completed", "approval resumed the session");
}

#[tokio::test]
async fn deny_resolves_the_parked_call_failed_without_killing_the_session() {
    let storage: Arc<dyn Storage> =
        Arc::new(cybersin_runtime::SqliteStorage::in_memory().await.unwrap());
    let spans = SpanStore::in_memory().await.unwrap();
    let dist = Arc::new(DistFixture::load_dir(bundled_stub_dist_dir()).unwrap());

    let (harness_io, daemon_io) = in_memory_pair();
    let mut daemon = RuntimeDaemon::new(
        daemon_io,
        storage.clone(),
        spans.clone(),
        dist,
        "sess-deny",
        "agent-a",
    );
    daemon.start_session(json!({})).await.unwrap();
    let daemon_task = tokio::spawn(daemon.run());

    let mut harness = StubHarness::new(harness_io);
    harness.recv_session_start().await;

    let (call_id, outcome) = harness
        .tool_request("wire_transfer", json!({"amount": 10_000}), None)
        .await;
    let approval_id = match outcome {
        CallOutcomeOrPark::Parked(approval_id) => approval_id,
        other => panic!("expected the critical call to park, got {other:?}"),
    };

    let gateway = ToolGateway::new(storage.clone(), Arc::new(EchoExecutor));
    gateway.deny(&approval_id).await.unwrap();

    let resumed = harness.await_result(&call_id).await;
    match resumed {
        CallOutcomeOrPark::Result(CallOutcome::Failed { reason, retriable }) => {
            assert_eq!(reason, "denied");
            assert!(!retriable);
        }
        other => panic!("expected a denied call.result, got {other:?}"),
    }

    harness
        .session_complete("sess-deny", json!({"status": "ok"}))
        .await;
    harness.wait_for_close().await;
    let summary = daemon_task.await.unwrap().unwrap();
    assert!(
        summary.completed,
        "deny resolves the call but doesn't kill the session"
    );
}

/// The durability claim, proven the way issue #13's own scope guidance
/// describes: park a session against **file-backed** storage (so state
/// genuinely outlives any one process), then don't just leave the parking
/// task running across "the gap" — abort it, drop every handle, and
/// reconstruct a completely fresh `DaemonHandle`/`ToolGateway`/
/// `SessionSupervisor` against the same database file, exactly as a
/// brand-new `cybersin approve <call-id>` invocation days later would.
/// Nothing is polling or spending anything during "the gap" — the
/// multi-day wait's only cost is disk space for one row.
#[tokio::test]
async fn approval_wait_survives_a_simulated_process_restart_for_free() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("cybersin.db");

    // --- "Process 1": park the session, then simulate a crash. ---
    let call_id = {
        let daemon_handle = DaemonHandle::auto_start(&db_path).await.unwrap();
        let dist = Arc::new(DistFixture::load_dir(bundled_stub_dist_dir()).unwrap());
        let config_hash = dist.manifest.build_hash.clone();

        let (harness_io, daemon_io) = in_memory_pair();
        let mut daemon = RuntimeDaemon::new(
            daemon_io,
            daemon_handle.storage(),
            daemon_handle.spans(),
            dist,
            "sess-durable",
            "agent-a",
        );
        daemon.start_session(json!({})).await.unwrap();
        let daemon_task = tokio::spawn(daemon.run());

        let mut harness = StubHarness::new(harness_io);
        harness.recv_session_start().await;
        let (_call_id, outcome) = harness
            .tool_request("wire_transfer", json!({"amount": 25_000}), None)
            .await;
        let approval_id = match outcome {
            CallOutcomeOrPark::Parked(approval_id) => approval_id,
            other => panic!("expected the critical call to park, got {other:?}"),
        };

        // Durable parked state, before anything is torn down.
        let session = daemon_handle
            .storage()
            .get_session("sess-durable")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(session.status, "awaiting_approval");
        assert_eq!(session.config_hash, config_hash);

        // Simulate the process dying mid-wait: abort the still-polling
        // daemon task and drop every handle into this database. No
        // in-memory state about this park survives past this point.
        daemon_task.abort();
        let _ = daemon_task.await;
        drop(harness);
        drop(daemon_handle);

        approval_id
    };

    // --- "The gap": nothing at all is running here. ---

    // --- "Process 2", hours/days later: reconnect, and prove the park is
    // still exactly as it was, purely from disk. ---
    let daemon_handle_2 = DaemonHandle::auto_start(&db_path).await.unwrap();
    let session = daemon_handle_2
        .storage()
        .get_session("sess-durable")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        session.status, "awaiting_approval",
        "the park survived the simulated restart with no task alive to keep it"
    );
    let row = daemon_handle_2
        .storage()
        .get_tool_call(&call_id)
        .await
        .unwrap()
        .unwrap();
    assert!(row.awaiting_approval);
    assert_eq!(row.status, "pending");

    // `cybersin approve <call-id>`, from this fresh process.
    let gateway = ToolGateway::new(daemon_handle_2.storage(), Arc::new(EchoExecutor));
    let outcome = gateway.approve(&call_id).await.unwrap();
    assert!(matches!(
        outcome,
        cybersin_gateway::GatewayOutcome::Resolved(CallOutcome::Ok { .. })
    ));

    let session = daemon_handle_2
        .storage()
        .get_session("sess-durable")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(session.status, "running", "approval resumed the session");

    // The session is durably resumable in the full `SessionSupervisor`
    // sense too, not just at the raw ledger level.
    let dist = DistFixture::load_dir(bundled_stub_dist_dir()).unwrap();
    let supervisor = SessionSupervisor::new(daemon_handle_2.storage());
    supervisor
        .resume("sess-durable", &dist.manifest.build_hash)
        .await
        .unwrap();
    let session = daemon_handle_2
        .storage()
        .get_session("sess-durable")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(session.status, "running");
}
