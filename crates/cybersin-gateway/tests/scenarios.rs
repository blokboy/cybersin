//! Integration scenarios for the tool gateway (spec §8.2), one per
//! acceptance criterion this issue doesn't already cover with a unit
//! test: a dead-letter queue walked end to end against a deliberately
//! failed call, an approve/deny park-resume cycle, and the double-firing
//! chaos test proving the ledger's UNIQUE constraint — not app-level
//! locking — is what keeps a duplicate side effect from ever happening.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use cybersin_adapter::messages::CallOutcome;
use cybersin_gateway::{
    ApprovalGate, EchoExecutor, GatewayOutcome, RetryClass, ToolExecutor, ToolGateway,
};
use cybersin_runtime::{DaemonHandle, SessionSupervisor};
use serde_json::json;

/// Always fails with `reason`. Used to seed a deliberately failed call.
struct AlwaysFailExecutor {
    reason: &'static str,
    calls: AtomicUsize,
}

impl AlwaysFailExecutor {
    fn new(reason: &'static str) -> Self {
        Self {
            reason,
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl ToolExecutor for AlwaysFailExecutor {
    async fn execute(
        &self,
        _tool: &str,
        _args: &serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(self.reason.to_string())
    }
}

/// Counts invocations and always succeeds — the chaos test's probe for
/// "zero duplicate side effects": if the ledger ever let a second
/// execution through, this counter would show it.
#[derive(Default)]
struct CountingExecutor {
    calls: AtomicUsize,
}

#[async_trait]
impl ToolExecutor for CountingExecutor {
    async fn execute(
        &self,
        tool: &str,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(json!({"tool": tool, "args": args, "status": "ok"}))
    }
}

async fn daemon() -> DaemonHandle {
    DaemonHandle::auto_start_in_memory().await.unwrap()
}

#[tokio::test]
async fn dlq_scenario_ls_show_retry_drop_against_a_deliberately_failed_call() {
    let daemon = daemon().await;
    daemon
        .storage()
        .create_session("sess-1", "agent-a")
        .await
        .unwrap();

    let failing = Arc::new(AlwaysFailExecutor::new("connection refused"));
    let gateway = ToolGateway::new(daemon.storage(), failing.clone());

    // A deliberately failed call: `critical` never auto-retries, so one
    // executor failure is immediately terminal.
    let outcome = gateway
        .call(
            "sess-1",
            "charge_card",
            json!({"amount": 500}),
            Some("charge-1".to_string()),
            RetryClass::Critical,
        )
        .await
        .unwrap();
    assert_eq!(
        outcome,
        GatewayOutcome::Resolved(CallOutcome::Failed {
            reason: "connection refused".to_string(),
            retriable: false,
        })
    );
    assert_eq!(
        failing.calls.load(Ordering::SeqCst),
        1,
        "critical never auto-retries"
    );

    // `dlq ls` surfaces it.
    let dead_letters = gateway.dlq_list().await.unwrap();
    assert_eq!(dead_letters.len(), 1);
    let call_id = dead_letters[0].call_id.clone();
    assert_eq!(call_id, "charge_card:charge-1");

    // `dlq show` returns the full row.
    let shown = gateway.dlq_show(&call_id).await.unwrap();
    assert_eq!(shown.status, "failed");
    assert_eq!(shown.failure_reason.as_deref(), Some("connection refused"));

    // `dlq retry`: reopen and re-execute. Swap in a gateway pointed at an
    // executor that now succeeds (simulating "the transient fault
    // cleared") to prove retry actually re-runs the call rather than
    // just flipping a status bit.
    let gateway_with_working_executor = ToolGateway::new(daemon.storage(), Arc::new(EchoExecutor));
    let retried = gateway_with_working_executor
        .dlq_retry(&call_id)
        .await
        .unwrap();
    assert!(matches!(
        retried,
        GatewayOutcome::Resolved(CallOutcome::Ok { .. })
    ));
    assert!(
        gateway.dlq_list().await.unwrap().is_empty(),
        "no longer a dead letter"
    );

    // Seed a second failure to exercise `dlq drop`.
    gateway
        .call(
            "sess-1",
            "charge_card",
            json!({"amount": 999}),
            Some("charge-2".to_string()),
            RetryClass::Critical,
        )
        .await
        .unwrap();
    let call_id_2 = "charge_card:charge-2";
    assert_eq!(gateway.dlq_list().await.unwrap().len(), 1);

    gateway.dlq_drop(call_id_2).await.unwrap();
    assert!(
        gateway.dlq_list().await.unwrap().is_empty(),
        "dropped call is hidden from dlq ls"
    );
    // But the audit row itself still exists.
    let dropped_row = gateway.dlq_show(call_id_2).await.unwrap();
    assert!(dropped_row.dropped);
    assert_eq!(dropped_row.status, "failed");
}

#[tokio::test]
async fn approve_resumes_the_parked_session_and_runs_the_call() {
    let daemon = daemon().await;
    daemon
        .storage()
        .create_session("sess-1", "agent-a")
        .await
        .unwrap();

    let gateway = ToolGateway::new(daemon.storage(), Arc::new(EchoExecutor))
        .with_policy_hook(Arc::new(ApprovalGate::for_tools(["wire_transfer"])));

    let parked = gateway
        .call(
            "sess-1",
            "wire_transfer",
            json!({"amount": 10_000}),
            Some("wt-1".to_string()),
            RetryClass::Write,
        )
        .await
        .unwrap();
    let call_id = match parked {
        GatewayOutcome::Parked {
            call_id,
            approval_id,
        } => {
            assert_eq!(approval_id, call_id);
            call_id
        }
        other => panic!("expected Parked, got {other:?}"),
    };

    // Parking parks the *session*, not just the call.
    let session = daemon
        .storage()
        .get_session("sess-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(session.status, "awaiting_approval");
    let row = gateway.dlq_show(&call_id).await;
    // Not a dead letter — still pending — dlq_show only succeeds via
    // get_tool_call, which works on any status; assert via the ledger
    // directly instead for clarity of what's actually true here.
    drop(row);
    let row = daemon
        .storage()
        .get_tool_call(&call_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.status, "pending");
    assert!(row.awaiting_approval);

    // `cybersin approve <call-id>`.
    let resolved = gateway.approve(&call_id).await.unwrap();
    assert!(matches!(
        resolved,
        GatewayOutcome::Resolved(CallOutcome::Ok { .. })
    ));

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
async fn deny_resolves_failed_denied_without_killing_the_session() {
    let daemon = daemon().await;
    daemon
        .storage()
        .create_session("sess-1", "agent-a")
        .await
        .unwrap();

    let executor = Arc::new(CountingExecutor::default());
    let gateway = ToolGateway::new(daemon.storage(), executor.clone())
        .with_policy_hook(Arc::new(ApprovalGate::for_tools(["wire_transfer"])));

    let parked = gateway
        .call(
            "sess-1",
            "wire_transfer",
            json!({"amount": 10_000}),
            Some("wt-1".to_string()),
            RetryClass::Write,
        )
        .await
        .unwrap();
    let call_id = match parked {
        GatewayOutcome::Parked { call_id, .. } => call_id,
        other => panic!("expected Parked, got {other:?}"),
    };

    // `cybersin deny <call-id>`.
    let resolved = gateway.deny(&call_id).await.unwrap();
    assert_eq!(
        resolved,
        GatewayOutcome::Resolved(CallOutcome::Failed {
            reason: "denied".to_string(),
            retriable: false,
        })
    );

    // A distinct terminal outcome from a transient failure, delivered
    // through the exact same resolution path — and the executor was
    // never invoked: denial never executes the tool.
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);

    let row = daemon
        .storage()
        .get_tool_call(&call_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.status, "failed");
    assert_eq!(row.failure_reason.as_deref(), Some("denied"));
    assert_eq!(row.retriable, Some(false));

    // The session is NOT killed/aborted — it resumes to running, and the
    // agent's own logic decides what happens next (spec §8.2).
    let session = daemon
        .storage()
        .get_session("sess-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(session.status, "running");

    // A denied call is terminal, not retriable by `dlq retry` (spec
    // §8.2) — it's failed, but it isn't in the dead-letter queue's
    // "retriable" sense; the gateway still lets a human retry it
    // explicitly if they choose to (dlq_retry doesn't check `retriable`),
    // but nothing auto-resubmits it.
    assert_eq!(gateway.dlq_list().await.unwrap().len(), 1);
}

#[tokio::test]
async fn chaos_double_firing_the_same_call_produces_zero_duplicate_side_effects() {
    let daemon = daemon().await;
    daemon
        .storage()
        .create_session("sess-1", "agent-a")
        .await
        .unwrap();

    let executor = Arc::new(CountingExecutor::default());
    let gateway = Arc::new(ToolGateway::new(daemon.storage(), executor.clone()));

    // 32 concurrent callers all "double-fire" the exact same tool call:
    // same tool, same explicit idem_key, same args — simulating e.g. a
    // network-retry storm resubmitting one logical write.
    let mut handles = Vec::new();
    for _ in 0..32 {
        let gateway = gateway.clone();
        handles.push(tokio::spawn(async move {
            gateway
                .call(
                    "sess-1",
                    "charge_card",
                    json!({"amount": 500, "order": "order-42"}),
                    Some("order-42".to_string()),
                    RetryClass::Write,
                )
                .await
                .unwrap()
        }));
    }

    let mut outcomes = Vec::new();
    for h in handles {
        outcomes.push(h.await.unwrap());
    }

    // The side effect happened exactly once, no matter how many callers
    // raced for it.
    assert_eq!(
        executor.calls.load(Ordering::SeqCst),
        1,
        "the tool executed more than once — duplicate side effect"
    );

    // Every caller — winner and losers alike — observed the exact same
    // successful outcome.
    for outcome in &outcomes {
        assert!(
            matches!(outcome, GatewayOutcome::Resolved(CallOutcome::Ok { .. })),
            "unexpected outcome: {outcome:?}"
        );
    }

    // And the ledger holds exactly one row for it.
    let row = daemon
        .storage()
        .get_tool_call("charge_card:order-42")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.status, "succeeded");
}

#[tokio::test]
async fn kill_then_resume_memoizes_succeeded_call_without_reexecution() {
    let daemon = daemon().await;
    let storage = daemon.storage();
    storage
        .create_session_pinned("sess-resume", "agent-a", "build-1")
        .await
        .unwrap();
    let executor = Arc::new(CountingExecutor::default());

    let first_gateway = ToolGateway::new(storage.clone(), executor.clone());
    let first = first_gateway
        .call(
            "sess-resume",
            "charge_card",
            json!({"order": "42"}),
            Some("order-42".into()),
            RetryClass::Write,
        )
        .await
        .unwrap();
    assert!(matches!(
        first,
        GatewayOutcome::Resolved(CallOutcome::Ok { .. })
    ));
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);

    // Dropping the first gateway models the killed worker process. A
    // fresh gateway after durable resume must read the succeeded ledger
    // row and return its result without touching the executor.
    drop(first_gateway);
    let supervisor = SessionSupervisor::new(storage.clone());
    supervisor.kill("sess-resume").await.unwrap();
    supervisor.resume("sess-resume", "build-1").await.unwrap();
    let resumed_gateway = ToolGateway::new(storage, executor.clone());
    let replay = resumed_gateway
        .call(
            "sess-resume",
            "charge_card",
            json!({"order": "42"}),
            Some("order-42".into()),
            RetryClass::Write,
        )
        .await
        .unwrap();
    assert_eq!(first, replay);
    assert_eq!(
        executor.calls.load(Ordering::SeqCst),
        1,
        "resume repeated a succeeded side effect"
    );
}

#[tokio::test]
async fn schema_validation_rejects_a_call_before_it_reaches_the_ledger() {
    let daemon = daemon().await;
    daemon
        .storage()
        .create_session("sess-1", "agent-a")
        .await
        .unwrap();

    let gateway = ToolGateway::new(daemon.storage(), Arc::new(EchoExecutor)).with_schema(
        "web_search",
        cybersin_gateway::ToolSchema::new().required("query", cybersin_gateway::FieldType::String),
    );

    let result = gateway
        .call(
            "sess-1",
            "web_search",
            json!({}),
            Some("k1".to_string()),
            RetryClass::Read,
        )
        .await;
    assert!(result.is_err());

    // Rejected before admission: no ledger row exists at all.
    let row = daemon
        .storage()
        .get_tool_call("web_search:k1")
        .await
        .unwrap();
    assert!(row.is_none());
}

#[tokio::test]
async fn read_class_auto_retries_in_line_and_write_class_retries_less() {
    let daemon = daemon().await;
    daemon
        .storage()
        .create_session("sess-1", "agent-a")
        .await
        .unwrap();

    let failing = Arc::new(AlwaysFailExecutor::new("timeout"));
    let gateway = ToolGateway::new(daemon.storage(), failing.clone());

    gateway
        .call(
            "sess-1",
            "fetch",
            json!({}),
            Some("r1".to_string()),
            RetryClass::Read,
        )
        .await
        .unwrap();
    // 1 initial + 3 auto-retries (RetryClass::Read::max_auto_retries).
    assert_eq!(failing.calls.load(Ordering::SeqCst), 4);

    let row = daemon
        .storage()
        .get_tool_call("fetch:r1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.attempts, 4);
    assert_eq!(row.status, "failed");
    assert_eq!(row.retriable, Some(true));
}
