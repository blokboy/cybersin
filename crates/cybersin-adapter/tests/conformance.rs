//! Adapter protocol v0 conformance scenarios (spec §10):
//! resume mid-task, double-fire, budget breach, parked approval.
//!
//! Each scenario is written once, generically over any
//! `(HarnessChannel, DaemonChannel)` pair, and then run against both
//! transports (`transport::stdio`, `transport::grpc`) — proving the
//! scenarios (and by extension, any conforming harness adapter) hold
//! regardless of which transport carries the protocol.

use cybersin_adapter::channel::{DaemonChannel, HarnessChannel};
use cybersin_adapter::daemon_double::DaemonDouble;
use cybersin_adapter::messages::{AbortReason, CallOutcome, DaemonMessage};
use cybersin_adapter::stub_harness::{CallOutcomeOrPark, StubHarness};
use cybersin_adapter::transport::{grpc, stdio};
use serde_json::json;

// ---------------------------------------------------------------------
// Scenario: resume mid-task
//
// A tool call succeeds, the harness process then "crashes" (drops its
// channel without sending session.complete). A new daemon (sharing the
// durably-persisted ledger) and a new harness reconnect with
// `resume_state` set, replay the same call, and must get the memoized
// result without the side effect executing a second time (§8.1).
// ---------------------------------------------------------------------
async fn scenario_resume_mid_task<H, D>(pair_a: (H, D), pair_b: (H, D))
where
    H: HarnessChannel + 'static,
    D: DaemonChannel + 'static,
{
    let (harness_io_a, daemon_io_a) = pair_a;

    let (mut daemon, _ctrl) = DaemonDouble::new(daemon_io_a, "sess-resume", 100.0);
    daemon.start_session(json!({"topic": "resume"}), None).await;
    let daemon_task = tokio::spawn(daemon.run());

    let mut harness = StubHarness::new(harness_io_a);
    let (session_id, inputs, resume_state) = harness.recv_session_start().await;
    assert_eq!(session_id, "sess-resume");
    assert_eq!(inputs, json!({"topic": "resume"}));
    assert!(resume_state.is_none());

    let (_call_id, outcome) = harness
        .tool_request_with_call_id(
            "call-1",
            "send_email",
            json!({"to": "a@example.com"}),
            Some("step-1".into()),
        )
        .await;
    assert!(matches!(
        outcome,
        CallOutcomeOrPark::Result(CallOutcome::Ok { .. })
    ));

    // The harness process crashes mid-task: no session.complete.
    drop(harness);
    let daemon_after_crash = daemon_task.await.expect("daemon task join");
    assert_eq!(daemon_after_crash.execute_count("send_email", "step-1"), 1);
    let ledger = daemon_after_crash.ledger_snapshot();

    // "Restart": ledger persisted durably, fresh daemon + fresh harness,
    // resume_state signals this is a replay.
    let (harness_io_b, daemon_io_b) = pair_b;
    let (daemon2, _ctrl2) = DaemonDouble::new(daemon_io_b, "sess-resume", 100.0);
    let mut daemon2 = daemon2.with_ledger(ledger);
    daemon2
        .start_session(
            json!({"topic": "resume"}),
            Some(json!({"resumed_from": "step-1"})),
        )
        .await;
    let daemon2_task = tokio::spawn(daemon2.run());

    let mut harness2 = StubHarness::new(harness_io_b);
    let (_sid, _inputs, resume_state2) = harness2.recv_session_start().await;
    assert_eq!(resume_state2, Some(json!({"resumed_from": "step-1"})));

    // Harness replays the already-succeeded call as part of resuming...
    let (_call_id, replayed) = harness2
        .tool_request_with_call_id(
            "call-1-replay",
            "send_email",
            json!({"to": "a@example.com"}),
            Some("step-1".into()),
        )
        .await;
    assert!(matches!(
        replayed,
        CallOutcomeOrPark::Result(CallOutcome::Ok { .. })
    ));

    // ...then continues with genuinely new work.
    let (_call_id, new_step) = harness2
        .tool_request(
            "send_email",
            json!({"to": "b@example.com"}),
            Some("step-2".into()),
        )
        .await;
    assert!(matches!(
        new_step,
        CallOutcomeOrPark::Result(CallOutcome::Ok { .. })
    ));

    harness2
        .session_complete("sess-resume", json!({"status": "done"}))
        .await;
    harness2.wait_for_close().await;
    drop(harness2);
    let daemon2_final = daemon2_task.await.expect("daemon2 task join");

    assert_eq!(
        daemon2_final.execute_count("send_email", "step-1"),
        1,
        "resumed call must replay the memoized result, not re-run the side effect"
    );
    assert_eq!(daemon2_final.execute_count("send_email", "step-2"), 1);
    assert!(daemon2_final.did_complete());
}

#[tokio::test]
async fn resume_mid_task_stdio() {
    scenario_resume_mid_task(stdio::in_memory_pair(), stdio::in_memory_pair()).await;
}

#[tokio::test]
async fn resume_mid_task_grpc() {
    scenario_resume_mid_task(grpc::in_memory_pair().await, grpc::in_memory_pair().await).await;
}

// ---------------------------------------------------------------------
// Scenario: double-fire
//
// The harness sends the same logical tool call twice (same idem_key,
// fresh call_id each time — a real harness would do this after a lost
// ack). Both replies must carry the identical result, and the
// side-effect must have executed exactly once (§8.2).
// ---------------------------------------------------------------------
async fn scenario_double_fire<H, D>(harness_io: H, daemon_io: D)
where
    H: HarnessChannel + 'static,
    D: DaemonChannel + 'static,
{
    let (mut daemon, _ctrl) = DaemonDouble::new(daemon_io, "sess-double-fire", 100.0);
    daemon.start_session(json!({}), None).await;
    let daemon_task = tokio::spawn(daemon.run());

    let mut harness = StubHarness::new(harness_io);
    harness.recv_session_start().await;

    let (_c1, outcome1) = harness
        .tool_request_with_call_id(
            "call-1",
            "charge_card",
            json!({"amount": 500}),
            Some("charge-42".into()),
        )
        .await;
    let (_c2, outcome2) = harness
        .tool_request_with_call_id(
            "call-2",
            "charge_card",
            json!({"amount": 500}),
            Some("charge-42".into()),
        )
        .await;

    let v1 = match outcome1 {
        CallOutcomeOrPark::Result(CallOutcome::Ok { value }) => value,
        other => panic!("expected ok, got {other:?}"),
    };
    let v2 = match outcome2 {
        CallOutcomeOrPark::Result(CallOutcome::Ok { value }) => value,
        other => panic!("expected ok, got {other:?}"),
    };
    assert_eq!(
        v1, v2,
        "a double-fired call must return the identical memoized result"
    );

    harness
        .session_complete("sess-double-fire", json!({}))
        .await;
    harness.wait_for_close().await;
    drop(harness);
    let daemon_final = daemon_task.await.expect("daemon task join");
    assert_eq!(
        daemon_final.execute_count("charge_card", "charge-42"),
        1,
        "the side effect must run exactly once despite two fires"
    );
}

#[tokio::test]
async fn double_fire_stdio() {
    let (h, d) = stdio::in_memory_pair();
    scenario_double_fire(h, d).await;
}

#[tokio::test]
async fn double_fire_grpc() {
    let (h, d) = grpc::in_memory_pair().await;
    scenario_double_fire(h, d).await;
}

// ---------------------------------------------------------------------
// Scenario: budget breach
//
// A session with a small USD budget issues llm.requests until one would
// push spend over the ceiling. `on_breach: halt` (§8.5): that call fails
// with a non-retriable reason and the daemon aborts the session,
// reporting spend that never exceeds the ceiling.
// ---------------------------------------------------------------------
async fn scenario_budget_breach<H, D>(harness_io: H, daemon_io: D)
where
    H: HarnessChannel + 'static,
    D: DaemonChannel + 'static,
{
    let (daemon, _ctrl) = DaemonDouble::new(daemon_io, "sess-budget", 0.25);
    let mut daemon = daemon.with_llm_cost(0.10);
    daemon.start_session(json!({}), None).await;
    let daemon_task = tokio::spawn(daemon.run());

    let mut harness = StubHarness::new(harness_io);
    harness.recv_session_start().await;

    // Two calls fit: $0.10 + $0.10 = $0.20 <= $0.25.
    for _ in 0..2 {
        let (_call_id, outcome) = harness.llm_request("researcher", json!({})).await;
        assert!(matches!(
            outcome,
            CallOutcomeOrPark::Result(CallOutcome::Ok { .. })
        ));
    }

    // A third would push spend to $0.30 > $0.25: breach, halt.
    let (_call_id, outcome) = harness.llm_request("researcher", json!({})).await;
    match outcome {
        CallOutcomeOrPark::Result(CallOutcome::Failed { reason, retriable }) => {
            assert_eq!(reason, "budget_halt");
            assert!(!retriable, "a budget halt must never be auto-retried");
        }
        other => panic!("expected a budget_halt failure, got {other:?}"),
    }

    let push = harness.await_push().await;
    match push {
        DaemonMessage::SessionAbort {
            reason:
                AbortReason::BudgetHalt {
                    usd_spent,
                    usd_budget,
                },
            ..
        } => {
            assert!(
                (usd_spent - 0.20).abs() < 1e-9,
                "reported spend must not include the halted call's cost, got {usd_spent}"
            );
            assert_eq!(usd_budget, 0.25);
        }
        other => panic!("expected session.abort(budget_halt), got {other:?}"),
    }

    drop(harness);
    let daemon_final = daemon_task.await.expect("daemon task join");
    assert!(daemon_final.was_aborted());
    assert!((daemon_final.usd_spent() - 0.20).abs() < 1e-9);
}

#[tokio::test]
async fn budget_breach_stdio() {
    let (h, d) = stdio::in_memory_pair();
    scenario_budget_breach(h, d).await;
}

#[tokio::test]
async fn budget_breach_grpc() {
    let (h, d) = grpc::in_memory_pair().await;
    scenario_budget_breach(h, d).await;
}

// ---------------------------------------------------------------------
// Scenario: parked approval
//
// A guarded tool call parks the session (`call.parked`) rather than
// executing or failing outright. The harness can keep doing other work
// while parked (durability makes multi-day approval waits free, §8.2).
// `cybersin approve` resolves it to a normal ok result and the side
// effect executes exactly once; `cybersin deny` resolves it to
// failed(denied) without ever executing the side effect, and the session
// is not killed.
// ---------------------------------------------------------------------
async fn scenario_parked_approval_approved<H, D>(harness_io: H, daemon_io: D)
where
    H: HarnessChannel + 'static,
    D: DaemonChannel + 'static,
{
    let (daemon, ctrl) = DaemonDouble::new(daemon_io, "sess-approval", 100.0);
    let mut daemon = daemon.require_approval("send_email");
    daemon.start_session(json!({}), None).await;
    let daemon_task = tokio::spawn(daemon.run());

    let mut harness = StubHarness::new(harness_io);
    harness.recv_session_start().await;

    let (call_id, outcome) = harness
        .tool_request(
            "send_email",
            json!({"to": "vip@example.com"}),
            Some("send-1".into()),
        )
        .await;
    let approval_id = match outcome {
        CallOutcomeOrPark::Parked(id) => id,
        other => panic!("expected the call to park, got {other:?}"),
    };

    // Durability makes multi-day approval waits free: the harness can do
    // other work while parked.
    let (_c2, other_outcome) = harness
        .state_set("scratch", "note", json!("waiting on approval"))
        .await;
    assert!(matches!(
        other_outcome,
        CallOutcomeOrPark::Result(CallOutcome::Ok { .. })
    ));

    // `cybersin approve <call-id>`
    ctrl.approve(approval_id);
    let resolved = harness.await_result(&call_id).await;
    assert!(matches!(
        resolved,
        CallOutcomeOrPark::Result(CallOutcome::Ok { .. })
    ));

    harness.session_complete("sess-approval", json!({})).await;
    harness.wait_for_close().await;
    drop(harness);
    let daemon_final = daemon_task.await.expect("daemon task join");
    assert_eq!(daemon_final.execute_count("send_email", "send-1"), 1);
    assert!(daemon_final.did_complete());
}

async fn scenario_parked_approval_denied<H, D>(harness_io: H, daemon_io: D)
where
    H: HarnessChannel + 'static,
    D: DaemonChannel + 'static,
{
    let (daemon, ctrl) = DaemonDouble::new(daemon_io, "sess-approval-deny", 100.0);
    let mut daemon = daemon.require_approval("send_email");
    daemon.start_session(json!({}), None).await;
    let daemon_task = tokio::spawn(daemon.run());

    let mut harness = StubHarness::new(harness_io);
    harness.recv_session_start().await;

    let (call_id, outcome) = harness
        .tool_request(
            "send_email",
            json!({"to": "vip@example.com"}),
            Some("send-2".into()),
        )
        .await;
    let approval_id = match outcome {
        CallOutcomeOrPark::Parked(id) => id,
        other => panic!("expected the call to park, got {other:?}"),
    };

    // `cybersin deny <call-id>`
    ctrl.deny(approval_id, "denied");
    let resolved = harness.await_result(&call_id).await;
    match resolved {
        CallOutcomeOrPark::Result(CallOutcome::Failed { reason, retriable }) => {
            assert_eq!(reason, "denied");
            assert!(!retriable);
        }
        other => panic!("expected a denied failure, got {other:?}"),
    }

    // Denial does not kill the session (§8.2): the harness keeps going.
    let (_c2, next) = harness
        .state_set("scratch", "note", json!("handled denial"))
        .await;
    assert!(matches!(
        next,
        CallOutcomeOrPark::Result(CallOutcome::Ok { .. })
    ));

    harness
        .session_complete("sess-approval-deny", json!({}))
        .await;
    harness.wait_for_close().await;
    drop(harness);
    let daemon_final = daemon_task.await.expect("daemon task join");
    assert_eq!(
        daemon_final.execute_count("send_email", "send-2"),
        0,
        "a denied call must never execute the side effect"
    );
    assert!(daemon_final.did_complete());
}

#[tokio::test]
async fn parked_approval_approved_stdio() {
    let (h, d) = stdio::in_memory_pair();
    scenario_parked_approval_approved(h, d).await;
}

#[tokio::test]
async fn parked_approval_approved_grpc() {
    let (h, d) = grpc::in_memory_pair().await;
    scenario_parked_approval_approved(h, d).await;
}

#[tokio::test]
async fn parked_approval_denied_stdio() {
    let (h, d) = stdio::in_memory_pair();
    scenario_parked_approval_denied(h, d).await;
}

#[tokio::test]
async fn parked_approval_denied_grpc() {
    let (h, d) = grpc::in_memory_pair().await;
    scenario_parked_approval_denied(h, d).await;
}

// ---------------------------------------------------------------------
// Scenario: spawn + mailbox queue
//
// Spawn is budget-gated and mailbox delivery preserves every queued
// message in order, then drains it exactly once.
// ---------------------------------------------------------------------
async fn scenario_spawn_and_mailbox<H, D>(harness_io: H, daemon_io: D)
where
    H: HarnessChannel + 'static,
    D: DaemonChannel + 'static,
{
    let (mut daemon, _ctrl) = DaemonDouble::new(daemon_io, "parent", 2.0);
    daemon.start_session(json!({}), None).await;
    let daemon_task = tokio::spawn(daemon.run());
    let mut harness = StubHarness::new(harness_io);
    harness.recv_session_start().await;

    let (_, spawned) = harness.spawn(json!({"agent":"worker"}), 1.5).await;
    assert!(matches!(
        spawned,
        CallOutcomeOrPark::Result(CallOutcome::Ok { .. })
    ));
    let (_, rejected) = harness.spawn(json!({"agent":"worker"}), 3.0).await;
    assert!(matches!(
        rejected,
        CallOutcomeOrPark::Result(CallOutcome::Failed { .. })
    ));

    harness.mailbox_send("parent", json!({"seq": 1})).await;
    harness.mailbox_send("parent", json!({"seq": 2})).await;
    let (_, received) = harness.mailbox_receive("parent").await;
    let CallOutcomeOrPark::Result(CallOutcome::Ok { value }) = received else {
        panic!("expected mailbox values");
    };
    assert_eq!(value, json!([{"seq": 1}, {"seq": 2}]));

    let (_, drained) = harness.mailbox_receive("parent").await;
    let CallOutcomeOrPark::Result(CallOutcome::Ok { value }) = drained else {
        panic!("expected drained mailbox");
    };
    assert_eq!(value, json!([]));

    harness.session_complete("parent", json!({})).await;
    harness.wait_for_close().await;
    drop(harness);
    assert!(daemon_task.await.expect("daemon task join").did_complete());
}

#[tokio::test]
async fn spawn_and_mailbox_stdio() {
    let (h, d) = stdio::in_memory_pair();
    scenario_spawn_and_mailbox(h, d).await;
}

#[tokio::test]
async fn spawn_and_mailbox_grpc() {
    let (h, d) = grpc::in_memory_pair().await;
    scenario_spawn_and_mailbox(h, d).await;
}
