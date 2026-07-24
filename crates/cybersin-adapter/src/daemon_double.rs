//! The minimum daemon-side test logic needed to drive the conformance
//! scenarios (spec §10) against a [`crate::stub_harness::StubHarness`].
//!
//! This is **not** `cybersin-runtime`/`cybersin-gateway` — no event log,
//! no storage backend, no policy engine, no real router/executor. It
//! reproduces just enough of the *observable outcomes* those subsystems
//! promise — idempotent tool calls (§8.2), budget halts (§8.5), approval
//! parking (§8.2), resumed sessions replaying memoized results instead of
//! re-running side effects (§8.1) — for the protocol layer's conformance
//! suite to prove a harness speaks the wire format correctly against
//! them. A real harness adapter is meant to be run through the same
//! scenario tests against the real daemon; this double stands in for that
//! daemon within this crate's scope only.

use crate::channel::DaemonChannel;
use crate::messages::{
    AbortReason, ApprovalId, CallId, CallOutcome, DaemonMessage, HarnessMessage, SessionId,
};
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use tokio::sync::mpsc;

/// A ledger row: the memoized outcome of one `(tool, idem_key)`, and how
/// many times its side effect has actually *executed* (as opposed to been
/// replayed from the ledger) — the number a passing double-fire or resume
/// scenario must keep at exactly 1.
#[derive(Debug, Clone)]
pub struct LedgerEntry {
    pub outcome: CallOutcome,
    pub execute_count: u32,
}

struct PendingApproval {
    call_id: CallId,
    tool: String,
    idem_key: String,
    args: Value,
}

enum ControlMsg {
    Approve(ApprovalId),
    Deny(ApprovalId, String),
}

/// A handle scenario tests use to play "the human" — `cybersin approve
/// <call-id>` / `cybersin deny <call-id>` (§8.2) — while the daemon
/// double's [`DaemonDouble::run`] loop is driving a session concurrently
/// in a background task.
#[derive(Clone)]
pub struct DaemonControlHandle {
    tx: mpsc::UnboundedSender<ControlMsg>,
}

impl DaemonControlHandle {
    pub fn approve(&self, approval_id: impl Into<ApprovalId>) {
        let _ = self.tx.send(ControlMsg::Approve(approval_id.into()));
    }

    pub fn deny(&self, approval_id: impl Into<ApprovalId>, reason: impl Into<String>) {
        let _ = self
            .tx
            .send(ControlMsg::Deny(approval_id.into(), reason.into()));
    }
}

/// The result of [`DaemonDouble::run`]: everything a conformance scenario
/// needs to assert against, detached from the (now-closed) channel.
#[derive(Debug, Clone)]
pub struct DaemonSummary {
    pub session_id: SessionId,
    usd_spent: f64,
    usd_budget: f64,
    aborted: bool,
    completed: bool,
    ledger: HashMap<(String, String), LedgerEntry>,
}

impl DaemonSummary {
    pub fn ledger_snapshot(&self) -> HashMap<(String, String), LedgerEntry> {
        self.ledger.clone()
    }

    pub fn execute_count(&self, tool: &str, idem_key: &str) -> u32 {
        self.ledger
            .get(&(tool.to_string(), idem_key.to_string()))
            .map(|e| e.execute_count)
            .unwrap_or(0)
    }

    pub fn usd_spent(&self) -> f64 {
        self.usd_spent
    }

    pub fn usd_budget(&self) -> f64 {
        self.usd_budget
    }

    pub fn was_aborted(&self) -> bool {
        self.aborted
    }

    pub fn did_complete(&self) -> bool {
        self.completed
    }
}

pub struct DaemonDouble<C> {
    channel: C,
    session_id: SessionId,
    usd_budget: f64,
    usd_spent: f64,
    cost_per_llm_call: f64,
    ledger: HashMap<(String, String), LedgerEntry>,
    approval_required: HashSet<String>,
    pending_approvals: HashMap<ApprovalId, PendingApproval>,
    state: HashMap<(String, String), Value>,
    mailboxes: HashMap<(String, String), VecDeque<Value>>,
    control_rx: mpsc::UnboundedReceiver<ControlMsg>,
    auto_idem_seq: u64,
    approval_seq: u64,
    aborted: bool,
    completed: bool,
}

impl<C: DaemonChannel> DaemonDouble<C> {
    pub fn new(
        channel: C,
        session_id: impl Into<String>,
        usd_budget: f64,
    ) -> (Self, DaemonControlHandle) {
        let (tx, rx) = mpsc::unbounded_channel();
        let double = Self {
            channel,
            session_id: session_id.into(),
            usd_budget,
            usd_spent: 0.0,
            cost_per_llm_call: 1.0,
            ledger: HashMap::new(),
            approval_required: HashSet::new(),
            pending_approvals: HashMap::new(),
            state: HashMap::new(),
            mailboxes: HashMap::new(),
            control_rx: rx,
            auto_idem_seq: 0,
            approval_seq: 0,
            aborted: false,
            completed: false,
        };
        (double, DaemonControlHandle { tx })
    }

    pub fn with_llm_cost(mut self, usd: f64) -> Self {
        self.cost_per_llm_call = usd;
        self
    }

    pub fn require_approval(mut self, tool: impl Into<String>) -> Self {
        self.approval_required.insert(tool.into());
        self
    }

    /// Seed the idempotency ledger as if a prior run had already persisted
    /// it durably (§8.1/§8.2) — the "resume mid-task" scenario's way of
    /// simulating a daemon restart between two `DaemonDouble` instances
    /// without needing real storage.
    pub fn with_ledger(mut self, ledger: HashMap<(String, String), LedgerEntry>) -> Self {
        self.ledger = ledger;
        self
    }

    pub async fn start_session(&mut self, inputs: Value, resume_state: Option<Value>) {
        let _ = self
            .channel
            .send(DaemonMessage::SessionStart {
                session_id: self.session_id.clone(),
                inputs,
                resume_state,
            })
            .await;
    }

    /// Drive this session until the harness disconnects or a budget
    /// breach aborts it, handling every `HarnessMessage` kind and any
    /// queued approve/deny control messages as they arrive. Returns a
    /// [`DaemonSummary`] — deliberately *not* `Self` — so callers can
    /// inspect ledger/budget/completion state afterward without holding
    /// the channel open. If this returned `Self`, the channel (and, for
    /// gRPC, its still-open response stream) would stay alive inside the
    /// `JoinHandle`'s result slot until whoever `tokio::spawn`ed this task
    /// both awaits the handle *and* drops the result — which a caller
    /// that first does `StubHarness::wait_for_close` (waiting for this
    /// side to close) then awaits the handle would deadlock on. Building
    /// the summary and letting `self` (and its channel) drop at the end
    /// of this function's scope means the close happens as soon as the
    /// spawned task's work is actually done, independent of whether or
    /// when the caller ever awaits the `JoinHandle`.
    pub async fn run(mut self) -> DaemonSummary {
        loop {
            if self.aborted || self.completed {
                // Stop as soon as we've processed the terminal signal
                // rather than waiting for the harness to also close its
                // end — over gRPC, the harness dropping its channel
                // immediately after sending session.complete can cancel
                // the underlying stream before that final frame is known
                // to have been flushed. Ending the daemon's own stream
                // here (dropping our sender once `self` goes out of
                // scope below) is itself the harness's signal it's now
                // safe to disconnect — see `StubHarness::wait_for_close`.
                break;
            }
            tokio::select! {
                biased;
                ctrl = self.control_rx.recv() => {
                    if let Some(c) = ctrl {
                        self.handle_control(c).await;
                    }
                    // With no control handles left, keep serving the channel.
                }
                msg = self.channel.recv() => {
                    match msg {
                        Some(m) => self.handle_harness_message(m).await,
                        None => break, // harness closed the channel
                    }
                }
            }
        }
        DaemonSummary {
            session_id: self.session_id.clone(),
            usd_spent: self.usd_spent,
            usd_budget: self.usd_budget,
            aborted: self.aborted,
            completed: self.completed,
            ledger: self.ledger.clone(),
        }
        // `self` (including `self.channel`) drops here, at the end of
        // this scope — see the doc comment above.
    }

    fn fresh_approval_id(&mut self) -> ApprovalId {
        self.approval_seq += 1;
        format!("approval-{}", self.approval_seq)
    }

    fn auto_idem_key(&mut self) -> String {
        self.auto_idem_seq += 1;
        format!("{}:{}", self.session_id, self.auto_idem_seq)
    }

    async fn handle_control(&mut self, ctrl: ControlMsg) {
        match ctrl {
            ControlMsg::Approve(id) => {
                if let Some(p) = self.pending_approvals.remove(&id) {
                    let outcome = self.execute_tool(&p.tool, &p.idem_key, p.args).await;
                    let _ = self
                        .channel
                        .send(DaemonMessage::CallResult {
                            call_id: p.call_id,
                            outcome,
                        })
                        .await;
                }
            }
            ControlMsg::Deny(id, reason) => {
                if let Some(p) = self.pending_approvals.remove(&id) {
                    // A denied approval resolves to failed(reason: denied)
                    // through the normal result channel — the same path
                    // any failed call takes (§8.2) — and is never
                    // auto-retried.
                    let outcome = CallOutcome::Failed {
                        reason,
                        retriable: false,
                    };
                    self.ledger.insert(
                        (p.tool, p.idem_key),
                        LedgerEntry {
                            outcome: outcome.clone(),
                            execute_count: 0,
                        },
                    );
                    let _ = self
                        .channel
                        .send(DaemonMessage::CallResult {
                            call_id: p.call_id,
                            outcome,
                        })
                        .await;
                }
            }
        }
    }

    async fn execute_tool(&mut self, tool: &str, idem_key: &str, args: Value) -> CallOutcome {
        let entry = self
            .ledger
            .entry((tool.to_string(), idem_key.to_string()))
            .or_insert(LedgerEntry {
                outcome: CallOutcome::Ok { value: Value::Null },
                execute_count: 0,
            });
        entry.execute_count += 1;
        let outcome = CallOutcome::Ok {
            value: serde_json::json!({"tool": tool, "args": args, "run": entry.execute_count}),
        };
        entry.outcome = outcome.clone();
        outcome
    }

    async fn handle_harness_message(&mut self, msg: HarnessMessage) {
        match msg {
            HarnessMessage::LlmRequest { call_id, .. } => {
                let prospective_spend = self.usd_spent + self.cost_per_llm_call;
                if prospective_spend > self.usd_budget {
                    // Budget breach, on_breach: halt (§8.5) — reply to the
                    // triggering call and abort the session.
                    let _ = self
                        .channel
                        .send(DaemonMessage::CallResult {
                            call_id,
                            outcome: CallOutcome::Failed {
                                reason: "budget_halt".into(),
                                retriable: false,
                            },
                        })
                        .await;
                    let reason = AbortReason::BudgetHalt {
                        usd_spent: self.usd_spent,
                        usd_budget: self.usd_budget,
                    };
                    let _ = self
                        .channel
                        .send(DaemonMessage::SessionAbort {
                            session_id: self.session_id.clone(),
                            reason,
                        })
                        .await;
                    self.aborted = true;
                    return;
                }
                self.usd_spent = prospective_spend;
                let _ = self
                    .channel
                    .send(DaemonMessage::CallResult {
                        call_id,
                        outcome: CallOutcome::Ok {
                            value: serde_json::json!({"text": "stub llm response"}),
                        },
                    })
                    .await;
            }
            HarnessMessage::ToolRequest {
                call_id,
                tool,
                args,
                idem_key,
            } => {
                let key = idem_key.unwrap_or_else(|| self.auto_idem_key());
                if let Some(entry) = self.ledger.get(&(tool.clone(), key.clone())) {
                    // Found `succeeded` in the ledger: replay the
                    // memoized result, no re-execution (§8.1).
                    let outcome = entry.outcome.clone();
                    let _ = self
                        .channel
                        .send(DaemonMessage::CallResult { call_id, outcome })
                        .await;
                    return;
                }
                if self.approval_required.contains(&tool) {
                    let approval_id = self.fresh_approval_id();
                    self.pending_approvals.insert(
                        approval_id.clone(),
                        PendingApproval {
                            call_id: call_id.clone(),
                            tool,
                            idem_key: key,
                            args,
                        },
                    );
                    let _ = self
                        .channel
                        .send(DaemonMessage::CallParked {
                            call_id,
                            approval_id,
                        })
                        .await;
                    return;
                }
                let outcome = self.execute_tool(&tool, &key, args).await;
                let _ = self
                    .channel
                    .send(DaemonMessage::CallResult { call_id, outcome })
                    .await;
            }
            HarnessMessage::StateSet {
                call_id,
                namespace,
                key,
                value,
            } => {
                self.state.insert((namespace, key), value);
                let _ = self
                    .channel
                    .send(DaemonMessage::CallResult {
                        call_id,
                        outcome: CallOutcome::Ok { value: Value::Null },
                    })
                    .await;
            }
            HarnessMessage::StateGet {
                call_id,
                namespace,
                key,
            } => {
                let value = self
                    .state
                    .get(&(namespace, key))
                    .cloned()
                    .unwrap_or(Value::Null);
                let _ = self
                    .channel
                    .send(DaemonMessage::CallResult {
                        call_id,
                        outcome: CallOutcome::Ok { value },
                    })
                    .await;
            }
            HarnessMessage::Checkpoint { call_id, .. } => {
                let _ = self
                    .channel
                    .send(DaemonMessage::CallResult {
                        call_id,
                        outcome: CallOutcome::Ok { value: Value::Null },
                    })
                    .await;
            }
            HarnessMessage::Sleep { call_id, .. } => {
                let _ = self
                    .channel
                    .send(DaemonMessage::CallResult {
                        call_id,
                        outcome: CallOutcome::Ok { value: Value::Null },
                    })
                    .await;
            }
            HarnessMessage::SignalWait { signal, .. } => {
                // Real `signal.wait` blocks for `cybersin notify`
                // (§8.1); the double answers immediately since driving
                // notify's timing is outside this crate's scope — the
                // conformance concern here is the message shape, not
                // notify's own scheduling.
                let _ = self
                    .channel
                    .send(DaemonMessage::SignalDelivered {
                        signal,
                        payload: Value::Null,
                    })
                    .await;
            }
            HarnessMessage::Spawn {
                call_id,
                budget_usd,
                ..
            } => {
                let worker_id = format!("{}:{call_id}", self.session_id);
                let outcome = if budget_usd <= self.usd_budget - self.usd_spent {
                    CallOutcome::Ok {
                        value: serde_json::json!({"worker_id": worker_id, "budget_usd": budget_usd}),
                    }
                } else {
                    CallOutcome::Failed {
                        reason: "parent_budget_exceeded".into(),
                        retriable: false,
                    }
                };
                let _ = self
                    .channel
                    .send(DaemonMessage::CallResult { call_id, outcome })
                    .await;
            }
            HarnessMessage::MailboxSend {
                call_id,
                recipient,
                payload,
            } => {
                self.mailboxes
                    .entry((recipient, self.session_id.clone()))
                    .or_default()
                    .push_back(payload);
                let _ = self
                    .channel
                    .send(DaemonMessage::CallResult {
                        call_id,
                        outcome: CallOutcome::Ok { value: Value::Null },
                    })
                    .await;
            }
            HarnessMessage::MailboxReceive { call_id, sender } => {
                let messages = self
                    .mailboxes
                    .remove(&(self.session_id.clone(), sender))
                    .unwrap_or_default()
                    .into_iter()
                    .collect::<Vec<_>>();
                let _ = self
                    .channel
                    .send(DaemonMessage::CallResult {
                        call_id,
                        outcome: CallOutcome::Ok {
                            value: serde_json::json!(messages),
                        },
                    })
                    .await;
            }
            HarnessMessage::SessionComplete { .. } => {
                self.completed = true;
            }
        }
    }
}
