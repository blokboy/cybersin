//! IC-4 orchestration checkpoint (issue #20).
//!
//! This is intentionally one integrated tree, not a collection of unit
//! scenarios: both children consume IC-1 prompts, share the real durable
//! store/gateway/trace stack, communicate concurrently, contend on the
//! blackboard, and one restores its session sandbox after a crash.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use cybersin_adapter::messages::CallOutcome;
use cybersin_adapter::stub_harness::{CallOutcomeOrPark, StubHarness};
use cybersin_adapter::transport::stdio::in_memory_pair;
use cybersin_gateway::{EchoExecutor, GatewayOutcome, RetryClass, ToolGateway};
use cybersin_runtime::{
    DistFixture, OrchestrationError, Orchestrator, RuntimeDaemon, SessionSupervisor, Storage,
    WorkerExit,
};
use cybersin_sandbox::{SandboxScope, WorkspaceStore};
use cybersin_trace::{SpanFilter, SpanKind, SpanStore};
use serde_json::{json, Value};

fn compiled_dist() -> Arc<DistFixture> {
    Arc::new(
        DistFixture::load_dir(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/ic1-research-team/dist"),
        )
        .unwrap(),
    )
}

async fn run_completed_child(
    storage: Arc<dyn Storage>,
    spans: SpanStore,
    dist: Arc<DistFixture>,
    child_id: &str,
    prompt_name: &str,
    inputs: Value,
) {
    let (harness_io, daemon_io) = in_memory_pair();
    let mut daemon = RuntimeDaemon::new(daemon_io, storage, spans, dist, child_id, "research-team");
    daemon.start_session(json!({})).await.unwrap();
    let daemon_task = tokio::spawn(daemon.run());
    let mut harness = StubHarness::new(harness_io);
    harness.recv_session_start().await;
    let (_, outcome) = harness.llm_request(prompt_name, inputs).await;
    assert!(matches!(
        outcome,
        CallOutcomeOrPark::Result(CallOutcome::Ok { .. })
    ));
    harness
        .session_complete(child_id, json!({"status": "ok"}))
        .await;
    harness.wait_for_close().await;
    assert!(daemon_task.await.unwrap().unwrap().completed);
}

#[tokio::test]
async fn real_multi_agent_tree_survives_worker_crash_and_coordinates_under_budget() {
    let parent_id = "ic4-root";
    let researcher_id = "ic4-researcher";
    let synthesizer_id = "ic4-synthesizer";
    let storage: Arc<dyn Storage> =
        Arc::new(cybersin_runtime::SqliteStorage::in_memory().await.unwrap());
    let spans = SpanStore::in_memory().await.unwrap();
    let dist = compiled_dist();
    let sandbox_root = tempfile::tempdir().unwrap();
    let workspaces = WorkspaceStore::new(sandbox_root.path()).unwrap();
    let supervisor = SessionSupervisor::with_session_sandbox(storage.clone(), workspaces.clone());
    let orchestration = Arc::new(Orchestrator::with_supervisor(storage.clone(), supervisor));

    // The researcher prompt's real compiled cascade (issue #33) now
    // genuinely escalates cheapest-first — fast-low ($0.2) and
    // balanced-medium ($1.0) are both attempted and paid for before
    // settling on premium-high ($4.0), for a real accumulated cost of
    // $5.2 per call, not just the single accepted step's price. The
    // researcher child's budget slice (and the parent ceiling, exactly
    // the sum of both children's slices so the "over ceiling" spawn
    // below still has zero headroom to work with) is sized accordingly.
    orchestration
        .register_parent(parent_id, "research-supervisor", 11.0)
        .await
        .unwrap();
    orchestration
        .spawn(
            parent_id,
            researcher_id,
            json!({
                "agent": "researcher",
                "prompt": "researcher",
                "config_hash": dist.manifest.build_hash,
            }),
            6.0,
            Some(2),
        )
        .await
        .unwrap();
    orchestration
        .spawn(
            parent_id,
            synthesizer_id,
            json!({
                "agent": "synthesizer",
                "prompt": "synthesizer",
                "config_hash": dist.manifest.build_hash,
            }),
            5.0,
            Some(2),
        )
        .await
        .unwrap();
    assert!(matches!(
        orchestration
            .spawn(parent_id, "over-budget", Value::Null, 0.01, None)
            .await,
        Err(OrchestrationError::BudgetExceeded { .. })
    ));

    // Research worker: execute its real compiled prompt and checkpoint a
    // session-scoped workspace, then die before completing the session.
    let researcher_workspace = workspaces
        .open(SandboxScope::Session, researcher_id, "work")
        .unwrap();
    fs::write(
        researcher_workspace.path().join("research.md"),
        "checkpointed evidence",
    )
    .unwrap();
    let (research_harness_io, research_daemon_io) = in_memory_pair();
    let mut researcher = RuntimeDaemon::new(
        research_daemon_io,
        storage.clone(),
        spans.clone(),
        dist.clone(),
        researcher_id,
        "researcher",
    )
    .with_session_sandbox(workspaces.clone());
    researcher.start_session(json!({})).await.unwrap();
    let researcher_task = tokio::spawn(researcher.run());
    let mut research_harness = StubHarness::new(research_harness_io);
    research_harness.recv_session_start().await;
    let (_, research_outcome) = research_harness
        .llm_request(
            "researcher",
            json!({"topic": "cybernetics", "depth": "thorough", "documents": []}),
        )
        .await;
    assert!(matches!(
        research_outcome,
        CallOutcomeOrPark::Result(CallOutcome::Ok { .. })
    ));
    fs::write(
        researcher_workspace.path().join("research.md"),
        "uncheckpointed corruption",
    )
    .unwrap();
    researcher_task.abort();
    let _ = researcher_task.await;
    drop(research_harness);

    let restarted = orchestration
        .worker_exit(researcher_id, WorkerExit::HarnessCrash)
        .await
        .unwrap();
    assert_eq!(restarted.restarts, 1);
    assert_eq!(restarted.status, "running");
    assert_eq!(
        fs::read_to_string(researcher_workspace.path().join("research.md")).unwrap(),
        "checkpointed evidence",
        "resume must restore the snapshot paired with the latest checkpoint"
    );

    // The sibling continues independently and executes its own real
    // compiled prompt while the researcher is being reassigned.
    run_completed_child(
        storage.clone(),
        spans.clone(),
        dist.clone(),
        synthesizer_id,
        "synthesizer",
        json!({"topic": "cybernetics", "audience": "technical", "findings": []}),
    )
    .await;

    // Both workers also use the real idempotent gateway against their
    // durable session rows.
    let gateway = Arc::new(ToolGateway::new(storage.clone(), Arc::new(EchoExecutor)));
    let (research_tool, synthesis_tool) = tokio::join!(
        gateway.call(
            researcher_id,
            "web_search",
            json!({"query": "cybernetics"}),
            Some("research-1".into()),
            RetryClass::Read,
        ),
        gateway.call(
            synthesizer_id,
            "citation_lookup",
            json!({"id": "source-1"}),
            Some("synthesis-1".into()),
            RetryClass::Read,
        )
    );
    assert!(matches!(
        research_tool.unwrap(),
        GatewayOutcome::Resolved(CallOutcome::Ok { .. })
    ));
    assert!(matches!(
        synthesis_tool.unwrap(),
        GatewayOutcome::Resolved(CallOutcome::Ok { .. })
    ));

    // Concurrent mailbox delivery preserves both senders, while CAS
    // gives exactly one writer the contested blackboard version.
    let (mail_a, mail_b) = tokio::join!(
        orchestration.send(
            researcher_id,
            parent_id,
            json!({"finding": "feedback loops"})
        ),
        orchestration.send(
            synthesizer_id,
            parent_id,
            json!({"summary": "systems evidence"})
        )
    );
    mail_a.unwrap();
    mail_b.unwrap();
    assert_eq!(
        orchestration
            .drain(parent_id, researcher_id)
            .await
            .unwrap()
            .len(),
        2,
        "restart notification and finding must both arrive"
    );
    assert_eq!(
        orchestration
            .drain(parent_id, synthesizer_id)
            .await
            .unwrap()
            .len(),
        1
    );

    let first = orchestration
        .blackboard_cas(parent_id, "report", "draft", None, json!("initial"))
        .await
        .unwrap();
    let (left, right) = tokio::join!(
        orchestration.blackboard_cas(
            parent_id,
            "report",
            "draft",
            Some(first.updated_seq),
            json!("researcher revision"),
        ),
        orchestration.blackboard_cas(
            parent_id,
            "report",
            "draft",
            Some(first.updated_seq),
            json!("synthesizer revision"),
        )
    );
    assert_ne!(left.is_ok(), right.is_ok(), "exactly one CAS writer wins");
    let loser = if left.is_err() { left } else { right };
    assert!(matches!(
        loser,
        Err(OrchestrationError::StaleBlackboard { .. })
    ));

    let researcher_spans = spans
        .list(&SpanFilter {
            session_id: Some(researcher_id.into()),
            kind: Some(SpanKind::LlmCall),
            ..Default::default()
        })
        .await
        .unwrap();
    let synthesizer_spans = spans
        .list(&SpanFilter {
            session_id: Some(synthesizer_id.into()),
            kind: Some(SpanKind::LlmCall),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(researcher_spans
        .iter()
        .any(|span| span.name == "researcher"));
    assert!(synthesizer_spans
        .iter()
        .any(|span| span.name == "synthesizer"));

    let researcher_spend: f64 = researcher_spans.iter().map(|span| span.usd_cost).sum();
    let synthesizer_spend: f64 = synthesizer_spans.iter().map(|span| span.usd_cost).sum();
    orchestration
        .charge(researcher_id, researcher_spend)
        .await
        .unwrap();
    orchestration
        .charge(synthesizer_id, synthesizer_spend)
        .await
        .unwrap();
    assert!(researcher_spend + synthesizer_spend <= 10.0);
    assert!(matches!(
        orchestration.charge(researcher_id, 5.01).await,
        Err(OrchestrationError::BudgetExceeded { .. })
    ));
}
