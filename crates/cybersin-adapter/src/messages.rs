//! Adapter protocol v0 message types (spec §10).
//!
//! These are the wire types any harness adapter (Python, TypeScript, Rust)
//! and the daemon exchange, independent of transport. Both transports
//! (stdio newline-JSON, gRPC) carry these types verbatim — stdio as one
//! JSON object per line, gRPC as the `json` field of an `Envelope` message
//! (see `proto/adapter.proto`) — so the protocol has exactly one
//! definition, in Rust, per the shared-types philosophy of §13.
//!
//! Every request from the harness carries a `call_id` it minted, used to
//! correlate the eventual `DaemonMessage::CallResult` / `CallParked`. This
//! is deliberately request/response over an otherwise message-passing
//! transport, matching how `tool_calls` are keyed in the real gateway
//! (§8.2: `(tool, idem_key)`, keys auto-derived `session:seq` unless
//! supplied) — `call_id` here plays that correlation role at the protocol
//! layer.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub type SessionId = String;
pub type CallId = String;
pub type ApprovalId = String;

/// Harness → daemon messages (spec §10).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum HarnessMessage {
    /// Names a *prompt*, never a model — routing, caching, budget, and
    /// context assembly apply transparently (§6.3, §8.3, §8.3a).
    #[serde(rename = "llm.request")]
    LlmRequest {
        call_id: CallId,
        prompt_name: String,
        inputs: Value,
    },

    #[serde(rename = "tool.request")]
    ToolRequest {
        call_id: CallId,
        tool: String,
        args: Value,
        /// Idempotency key. Auto-derived (`session:seq`) by the daemon if
        /// omitted (§8.2).
        #[serde(skip_serializing_if = "Option::is_none", default)]
        idem_key: Option<String>,
    },

    #[serde(rename = "state.get")]
    StateGet {
        call_id: CallId,
        namespace: String,
        key: String,
    },

    #[serde(rename = "state.set")]
    StateSet {
        call_id: CallId,
        namespace: String,
        key: String,
        value: Value,
    },

    #[serde(rename = "checkpoint")]
    Checkpoint {
        call_id: CallId,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        label: Option<String>,
    },

    #[serde(rename = "sleep")]
    Sleep { call_id: CallId, duration_ms: u64 },

    #[serde(rename = "signal.wait")]
    SignalWait { call_id: CallId, signal: String },

    #[serde(rename = "session.complete")]
    SessionComplete {
        session_id: SessionId,
        result: Value,
    },
}

impl HarnessMessage {
    /// The `call_id` this message expects a correlated
    /// `DaemonMessage::CallResult`/`CallParked` reply for, if any.
    /// `session.complete` has no reply — it ends the session.
    pub fn call_id(&self) -> Option<&str> {
        match self {
            HarnessMessage::LlmRequest { call_id, .. }
            | HarnessMessage::ToolRequest { call_id, .. }
            | HarnessMessage::StateGet { call_id, .. }
            | HarnessMessage::StateSet { call_id, .. }
            | HarnessMessage::Checkpoint { call_id, .. }
            | HarnessMessage::Sleep { call_id, .. }
            | HarnessMessage::SignalWait { call_id, .. } => Some(call_id),
            HarnessMessage::SessionComplete { .. } => None,
        }
    }
}

/// Daemon → harness messages (spec §10).
///
/// `SessionStart`, `SignalDelivered`, and `SessionAbort` are the three
/// message kinds the spec names explicitly. `CallResult` and `CallParked`
/// are the minimal necessary extension for the protocol to actually
/// function as request/response: every `HarnessMessage` that carries a
/// `call_id` needs a correlated reply, and a gateway approval gate needs a
/// way to tell the harness "this call is parked, not failed, not yet
/// succeeded" (§8.2) distinct from those three named push messages.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum DaemonMessage {
    #[serde(rename = "session.start")]
    SessionStart {
        session_id: SessionId,
        inputs: Value,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        resume_state: Option<Value>,
    },

    #[serde(rename = "signal.delivered")]
    SignalDelivered { signal: String, payload: Value },

    #[serde(rename = "session.abort")]
    SessionAbort {
        session_id: SessionId,
        reason: AbortReason,
    },

    /// Reply to a `call_id`-bearing `HarnessMessage`.
    #[serde(rename = "call.result")]
    CallResult {
        call_id: CallId,
        outcome: CallOutcome,
    },

    /// The call was flagged by a policy hook (§8.2) and the session is now
    /// parked (`awaiting_approval`) pending `cybersin approve|deny
    /// <call-id>`. No `CallResult` for this `call_id` follows until that
    /// resolves — approval yields `CallResult::Ok`, denial yields
    /// `CallResult::Failed { reason: "denied", retriable: false }`
    /// delivered through the same normal result channel any failed call
    /// takes (§8.2).
    #[serde(rename = "call.parked")]
    CallParked {
        call_id: CallId,
        approval_id: ApprovalId,
    },
}

/// Outcome of a harness request, delivered via `DaemonMessage::CallResult`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "status")]
pub enum CallOutcome {
    #[serde(rename = "ok")]
    Ok { value: Value },

    /// A distinct terminal outcome from a transient execution failure
    /// (§8.2). `retriable` distinguishes the gateway's retry classes:
    /// `read` (retry freely), `write` (retry with key), `critical` (never
    /// auto-retry) — a denied approval is always `retriable: false`.
    #[serde(rename = "failed")]
    Failed { reason: String, retriable: bool },
}

/// Why the daemon aborted a session (§8.5: budget breach `halt` policy;
/// also used for harness-visible crash/kill notification).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind")]
pub enum AbortReason {
    /// Session budget breached and `on_breach: halt` (§8.5, §5.3).
    #[serde(rename = "budget_halt")]
    BudgetHalt { usd_spent: f64, usd_budget: f64 },
    #[serde(rename = "error")]
    Error { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn llm_request_round_trips_and_tags_type() {
        let msg = HarnessMessage::LlmRequest {
            call_id: "c1".into(),
            prompt_name: "researcher".into(),
            inputs: serde_json::json!({"topic": "cybernetics"}),
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "llm.request");
        assert_eq!(json["prompt_name"], "researcher");
        let back: HarnessMessage = serde_json::from_value(json).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn session_start_resume_state_omitted_when_none() {
        let msg = DaemonMessage::SessionStart {
            session_id: "s1".into(),
            inputs: serde_json::json!({}),
            resume_state: None,
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert!(json.get("resume_state").is_none());
    }

    #[test]
    fn call_id_extraction() {
        let msg = HarnessMessage::Sleep {
            call_id: "c2".into(),
            duration_ms: 10,
        };
        assert_eq!(msg.call_id(), Some("c2"));

        let msg = HarnessMessage::SessionComplete {
            session_id: "s1".into(),
            result: Value::Null,
        };
        assert_eq!(msg.call_id(), None);
    }
}
