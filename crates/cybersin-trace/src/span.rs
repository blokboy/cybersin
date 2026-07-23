//! Span data model (spec §8.5): one OTel-compatible span per LLM call,
//! tool call, sandbox exec, or cache decision, carrying the attributes the
//! cost core and `cybersin trace`/`cybersin cost` read back: tokens,
//! `usd_cost`, model, cache status, retries, evicted sections.
//!
//! This module only defines the shape; [`crate::store::SpanStore`] is
//! where spans get persisted and queried.

use serde::{Deserialize, Serialize};

/// The kind of event a span records (spec §8.5's four event kinds).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpanKind {
    /// A `llm.request` call (spec §10) routed through the executor.
    LlmCall,
    /// A `tool.request` call through the gateway (spec §8.2).
    ToolCall,
    /// An agent-generated code execution in the sandbox (spec §8.4).
    SandboxExec,
    /// A cache lookup outcome (hit/miss/escalation) from the route/cache
    /// executor (spec §8.3), recorded as its own span distinct from the
    /// `LlmCall` span it precedes.
    CacheDecision,
}

impl SpanKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            SpanKind::LlmCall => "llm_call",
            SpanKind::ToolCall => "tool_call",
            SpanKind::SandboxExec => "sandbox_exec",
            SpanKind::CacheDecision => "cache_decision",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "llm_call" => Some(SpanKind::LlmCall),
            "tool_call" => Some(SpanKind::ToolCall),
            "sandbox_exec" => Some(SpanKind::SandboxExec),
            "cache_decision" => Some(SpanKind::CacheDecision),
            _ => None,
        }
    }
}

/// Whether — and how — a cache lookup resolved for this span. `NotApplicable`
/// covers span kinds the cache/route executor never touches (tool calls,
/// sandbox execs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheStatus {
    Hit,
    Miss,
    NotApplicable,
}

impl CacheStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            CacheStatus::Hit => "hit",
            CacheStatus::Miss => "miss",
            CacheStatus::NotApplicable => "not_applicable",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "hit" => Some(CacheStatus::Hit),
            "miss" => Some(CacheStatus::Miss),
            "not_applicable" => Some(CacheStatus::NotApplicable),
            _ => None,
        }
    }
}

/// Terminal status of the event this span records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SpanStatus {
    Ok,
    Error { message: String },
}

impl SpanStatus {
    fn tag(&self) -> &'static str {
        match self {
            SpanStatus::Ok => "ok",
            SpanStatus::Error { .. } => "error",
        }
    }

    fn error_message(&self) -> Option<&str> {
        match self {
            SpanStatus::Ok => None,
            SpanStatus::Error { message } => Some(message),
        }
    }

    pub(crate) fn from_parts(tag: &str, message: Option<String>) -> Self {
        match tag {
            "error" => SpanStatus::Error {
                message: message.unwrap_or_default(),
            },
            _ => SpanStatus::Ok,
        }
    }
}

/// One OTel-compatible span (spec §8.5). Every attribute the spec names
/// explicitly — tokens (prompt/completion), `usd_cost`, model, cache
/// status, retries, evicted sections — is a first-class field rather than
/// a loose attribute bag, so query code (cost rollups, `trace show`) isn't
/// parsing an untyped map; `attributes` remains as an escape hatch for
/// event-kind-specific extras that don't need their own column yet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Span {
    /// Unique span id.
    pub id: String,
    /// The session this span belongs to.
    pub session_id: String,
    /// The agent config name that owns this session (spec §5.3's
    /// `name: research-agent`) — the `agent` dimension of `cybersin cost
    /// --by agent`.
    pub agent_name: String,
    pub kind: SpanKind,
    /// The prompt name (LLM calls), tool name (tool calls), or a
    /// free-form label (sandbox execs, cache decisions).
    pub name: String,
    pub start_unix_ms: i64,
    pub end_unix_ms: i64,
    /// Model that served this span, when applicable (LLM calls; `None`
    /// for tool calls/sandbox execs, and for cache decisions that never
    /// escalate to a model).
    pub model: Option<String>,
    pub tokens_prompt: Option<u32>,
    pub tokens_completion: Option<u32>,
    /// Cost of this event in USD. Zero for cache hits, tool calls with no
    /// billed cost, etc. — always present (never `Option`) since every
    /// span contributes to a cost rollup, even if that contribution is
    /// zero.
    pub usd_cost: f64,
    pub cache_status: CacheStatus,
    /// How many retries this event needed before reaching its terminal
    /// status (spec §8.2's retry classes).
    pub retries: u32,
    /// Section ids the context assembler evicted while assembling this
    /// call's prompt (spec §8.3a) — empty when nothing was evicted or the
    /// span kind has no context assembly step.
    pub evicted_sections: Vec<String>,
    pub status: SpanStatus,
    /// Event-kind-specific extra attributes that don't warrant their own
    /// column (e.g. a cache decision's similarity score). Kept as raw JSON
    /// rather than typed per-kind since callers query the typed fields
    /// above for anything the cost core or CLI needs.
    pub attributes: serde_json::Value,
}

impl Span {
    pub fn duration_ms(&self) -> i64 {
        self.end_unix_ms - self.start_unix_ms
    }
}

/// Row shape used internally to move between SQL rows and [`Span`] without
/// making every consumer deal with the flattened `status`/`error_message`
/// and JSON-encoded `evicted_sections` columns directly.
#[derive(Debug, sqlx::FromRow)]
pub(crate) struct SpanRow {
    pub id: String,
    pub session_id: String,
    pub agent_name: String,
    pub kind: String,
    pub name: String,
    pub start_unix_ms: i64,
    pub end_unix_ms: i64,
    pub model: Option<String>,
    pub tokens_prompt: Option<i64>,
    pub tokens_completion: Option<i64>,
    pub usd_cost: f64,
    pub cache_status: String,
    pub retries: i64,
    pub evicted_sections: String,
    pub status: String,
    pub error_message: Option<String>,
    pub attributes: String,
}

impl SpanRow {
    pub(crate) fn into_span(self) -> Span {
        Span {
            id: self.id,
            session_id: self.session_id,
            agent_name: self.agent_name,
            kind: SpanKind::parse(&self.kind).unwrap_or(SpanKind::LlmCall),
            name: self.name,
            start_unix_ms: self.start_unix_ms,
            end_unix_ms: self.end_unix_ms,
            model: self.model,
            tokens_prompt: self.tokens_prompt.map(|v| v as u32),
            tokens_completion: self.tokens_completion.map(|v| v as u32),
            usd_cost: self.usd_cost,
            cache_status: CacheStatus::parse(&self.cache_status)
                .unwrap_or(CacheStatus::NotApplicable),
            retries: self.retries as u32,
            evicted_sections: serde_json::from_str(&self.evicted_sections).unwrap_or_default(),
            status: SpanStatus::from_parts(&self.status, self.error_message),
            attributes: serde_json::from_str(&self.attributes).unwrap_or(serde_json::Value::Null),
        }
    }

    pub(crate) fn status_tag(status: &SpanStatus) -> &'static str {
        status.tag()
    }

    pub(crate) fn error_message(status: &SpanStatus) -> Option<&str> {
        status.error_message()
    }
}
