//! A minimal, scriptable reference harness (spec §10: "any harness adapter
//! integrates via a small adapter protocol"). Real adapters (Python,
//! TypeScript, this crate's own Rust one) implement an agent loop on top
//! of a [`crate::channel::HarnessChannel`]; `StubHarness` is the thinnest
//! possible such loop, driven explicitly by test code instead of an LLM,
//! used to exercise the conformance scenarios against a
//! [`crate::daemon_double::DaemonDouble`] over either transport.

use crate::channel::HarnessChannel;
use crate::messages::{
    AbortReason, ApprovalId, CallId, CallOutcome, DaemonMessage, HarnessMessage, SessionId,
};
use serde_json::Value;
use std::collections::VecDeque;

/// The outcome of a `call_id`-correlated request, as observed by the
/// harness side.
#[derive(Debug, Clone, PartialEq)]
pub enum CallOutcomeOrPark {
    /// The daemon replied with `call.result`.
    Result(CallOutcome),
    /// The daemon parked the call (`call.parked`) pending human approval;
    /// no `call.result` has arrived yet for this `call_id` — poll
    /// [`StubHarness::await_result`] with the same `call_id` for the
    /// eventual resolution.
    Parked(ApprovalId),
    /// The daemon aborted the whole session (`session.abort`) before this
    /// call could be answered — e.g. a budget breach discovered while
    /// processing this very request.
    Aborted(AbortReason),
}

/// A stub harness driving one connected session over any
/// [`HarnessChannel`] (stdio or gRPC — identical either way).
pub struct StubHarness<C> {
    channel: C,
    next_id: u64,
    /// Daemon pushes received while awaiting a different call's reply
    /// (e.g. `signal.delivered` arriving mid-`tool.request`), stashed for
    /// later retrieval via [`StubHarness::await_push`].
    pending_pushes: VecDeque<DaemonMessage>,
}

impl<C: HarnessChannel> StubHarness<C> {
    pub fn new(channel: C) -> Self {
        Self {
            channel,
            next_id: 0,
            pending_pushes: VecDeque::new(),
        }
    }

    fn fresh_call_id(&mut self) -> CallId {
        self.next_id += 1;
        format!("call-{}", self.next_id)
    }

    /// Blocks for the daemon's opening `session.start` push.
    pub async fn recv_session_start(&mut self) -> (SessionId, Value, Option<Value>) {
        match self
            .channel
            .recv()
            .await
            .expect("channel closed before session.start")
        {
            DaemonMessage::SessionStart {
                session_id,
                inputs,
                resume_state,
            } => (session_id, inputs, resume_state),
            other => panic!("expected session.start, got {other:?}"),
        }
    }

    /// `llm.request {prompt_name, inputs}` — names a prompt, never a model.
    pub async fn llm_request(
        &mut self,
        prompt_name: impl Into<String>,
        inputs: Value,
    ) -> (CallId, CallOutcomeOrPark) {
        let call_id = self.fresh_call_id();
        self.channel
            .send(HarnessMessage::LlmRequest {
                call_id: call_id.clone(),
                prompt_name: prompt_name.into(),
                inputs,
            })
            .await
            .expect("send llm.request");
        let outcome = self.await_result(&call_id).await;
        (call_id, outcome)
    }

    /// `tool.request`, with an explicit `call_id` and optional `idem_key`
    /// — the two knobs the double-fire conformance scenario needs
    /// (resend with a *new* `call_id` but the *same* `idem_key`, as a real
    /// harness would after a lost ack).
    pub async fn tool_request_with_call_id(
        &mut self,
        call_id: impl Into<String>,
        tool: impl Into<String>,
        args: Value,
        idem_key: Option<String>,
    ) -> (CallId, CallOutcomeOrPark) {
        let call_id = call_id.into();
        self.channel
            .send(HarnessMessage::ToolRequest {
                call_id: call_id.clone(),
                tool: tool.into(),
                args,
                idem_key,
            })
            .await
            .expect("send tool.request");
        let outcome = self.await_result(&call_id).await;
        (call_id, outcome)
    }

    /// `tool.request` with an auto-assigned `call_id`.
    pub async fn tool_request(
        &mut self,
        tool: impl Into<String>,
        args: Value,
        idem_key: Option<String>,
    ) -> (CallId, CallOutcomeOrPark) {
        let call_id = self.fresh_call_id();
        self.tool_request_with_call_id(call_id, tool, args, idem_key)
            .await
    }

    pub async fn state_set(
        &mut self,
        namespace: impl Into<String>,
        key: impl Into<String>,
        value: Value,
    ) -> (CallId, CallOutcomeOrPark) {
        let call_id = self.fresh_call_id();
        self.channel
            .send(HarnessMessage::StateSet {
                call_id: call_id.clone(),
                namespace: namespace.into(),
                key: key.into(),
                value,
            })
            .await
            .expect("send state.set");
        let outcome = self.await_result(&call_id).await;
        (call_id, outcome)
    }

    pub async fn state_get(
        &mut self,
        namespace: impl Into<String>,
        key: impl Into<String>,
    ) -> (CallId, CallOutcomeOrPark) {
        let call_id = self.fresh_call_id();
        self.channel
            .send(HarnessMessage::StateGet {
                call_id: call_id.clone(),
                namespace: namespace.into(),
                key: key.into(),
            })
            .await
            .expect("send state.get");
        let outcome = self.await_result(&call_id).await;
        (call_id, outcome)
    }

    pub async fn checkpoint(&mut self, label: Option<String>) -> (CallId, CallOutcomeOrPark) {
        let call_id = self.fresh_call_id();
        self.channel
            .send(HarnessMessage::Checkpoint {
                call_id: call_id.clone(),
                label,
            })
            .await
            .expect("send checkpoint");
        let outcome = self.await_result(&call_id).await;
        (call_id, outcome)
    }

    pub async fn sleep(&mut self, duration_ms: u64) -> (CallId, CallOutcomeOrPark) {
        let call_id = self.fresh_call_id();
        self.channel
            .send(HarnessMessage::Sleep {
                call_id: call_id.clone(),
                duration_ms,
            })
            .await
            .expect("send sleep");
        let outcome = self.await_result(&call_id).await;
        (call_id, outcome)
    }

    /// `signal.wait` — blocks until the daemon pushes `signal.delivered`
    /// (the durable counterpart of `cybersin notify`, §8.1).
    pub async fn signal_wait(&mut self, signal: impl Into<String>) -> (String, Value) {
        let call_id = self.fresh_call_id();
        self.channel
            .send(HarnessMessage::SignalWait {
                call_id,
                signal: signal.into(),
            })
            .await
            .expect("send signal.wait");
        loop {
            if let Some(pos) = self
                .pending_pushes
                .iter()
                .position(|m| matches!(m, DaemonMessage::SignalDelivered { .. }))
            {
                if let DaemonMessage::SignalDelivered { signal, payload } =
                    self.pending_pushes.remove(pos).unwrap()
                {
                    return (signal, payload);
                }
            }
            match self
                .channel
                .recv()
                .await
                .expect("channel closed while awaiting signal.delivered")
            {
                DaemonMessage::SignalDelivered { signal, payload } => return (signal, payload),
                other => self.pending_pushes.push_back(other),
            }
        }
    }

    pub async fn spawn(
        &mut self,
        child_config: Value,
        budget_usd: f64,
    ) -> (CallId, CallOutcomeOrPark) {
        let call_id = self.fresh_call_id();
        self.channel
            .send(HarnessMessage::Spawn {
                call_id: call_id.clone(),
                child_config,
                budget_usd,
            })
            .await
            .expect("send spawn");
        let outcome = self.await_result(&call_id).await;
        (call_id, outcome)
    }

    pub async fn mailbox_send(
        &mut self,
        recipient: impl Into<String>,
        payload: Value,
    ) -> (CallId, CallOutcomeOrPark) {
        let call_id = self.fresh_call_id();
        self.channel
            .send(HarnessMessage::MailboxSend {
                call_id: call_id.clone(),
                recipient: recipient.into(),
                payload,
            })
            .await
            .expect("send mailbox.send");
        let outcome = self.await_result(&call_id).await;
        (call_id, outcome)
    }

    pub async fn mailbox_receive(
        &mut self,
        sender: impl Into<String>,
    ) -> (CallId, CallOutcomeOrPark) {
        let call_id = self.fresh_call_id();
        self.channel
            .send(HarnessMessage::MailboxReceive {
                call_id: call_id.clone(),
                sender: sender.into(),
            })
            .await
            .expect("send mailbox.receive");
        let outcome = self.await_result(&call_id).await;
        (call_id, outcome)
    }

    /// `session.complete` — no reply; ends the session.
    pub async fn session_complete(&mut self, session_id: impl Into<String>, result: Value) {
        self.channel
            .send(HarnessMessage::SessionComplete {
                session_id: session_id.into(),
                result,
            })
            .await
            .expect("send session.complete");
    }

    /// Block until the daemon closes its side of the channel. Scenario
    /// tests call this after `session.complete` (which has no reply of
    /// its own) so it's provably safe to drop the harness channel:
    /// waiting for the daemon-driven close proves the daemon actually
    /// processed session.complete, rather than racing a channel drop
    /// against an in-flight final message (a real race over gRPC, where
    /// dropping the client stream can cancel an unflushed frame).
    pub async fn wait_for_close(&mut self) {
        while self.channel.recv().await.is_some() {}
    }

    /// Block for the next daemon push not correlated to a specific call
    /// (`session.abort`, or anything stashed while awaiting another
    /// call's reply).
    pub async fn await_push(&mut self) -> DaemonMessage {
        if let Some(msg) = self.pending_pushes.pop_front() {
            return msg;
        }
        self.channel
            .recv()
            .await
            .expect("channel closed while awaiting a push")
    }

    /// Wait for the `call.result` / `call.parked` correlated to
    /// `call_id`, or a `session.abort` that preempts it. Used both
    /// internally (every `*_request` method awaits its own reply this
    /// way) and directly by scenario tests polling a previously parked
    /// call for its eventual resolution.
    pub async fn await_result(&mut self, call_id: &str) -> CallOutcomeOrPark {
        if let Some(pos) = self.pending_pushes.iter().position(|m| matches!(
            m,
            DaemonMessage::CallResult { call_id: id, .. } | DaemonMessage::CallParked { call_id: id, .. }
                if id == call_id
        )) {
            return Self::classify(self.pending_pushes.remove(pos).unwrap());
        }
        loop {
            let msg = self
                .channel
                .recv()
                .await
                .expect("channel closed while awaiting a call reply");
            match &msg {
                DaemonMessage::CallResult { call_id: id, .. }
                | DaemonMessage::CallParked { call_id: id, .. }
                    if id == call_id =>
                {
                    return Self::classify(msg);
                }
                DaemonMessage::SessionAbort { reason, .. } => {
                    return CallOutcomeOrPark::Aborted(reason.clone());
                }
                _ => self.pending_pushes.push_back(msg),
            }
        }
    }

    fn classify(msg: DaemonMessage) -> CallOutcomeOrPark {
        match msg {
            DaemonMessage::CallResult { outcome, .. } => CallOutcomeOrPark::Result(outcome),
            DaemonMessage::CallParked { approval_id, .. } => CallOutcomeOrPark::Parked(approval_id),
            other => panic!("not a call reply: {other:?}"),
        }
    }
}
