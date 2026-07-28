//! [`ToolCaller`]: the seam `RuntimeDaemon` calls for an ungated tool
//! request (spec §8.2), mirroring `crate::model_caller`'s
//! `ModelCaller`/`with_models` shape exactly — a `Stub*` default plus a
//! `Box<dyn ToolCaller>` forwarding impl so `RuntimeDaemon::with_tool_caller`
//! can swap in a real implementation without threading a second generic
//! parameter through every one of its methods.
//!
//! `cybersin-gateway` (the real sandboxed tool executor's home, issue #35
//! Phase 2) depends on `cybersin-runtime`, not the other way around, so a
//! real implementation of this trait can't live here — it's a bridge type
//! in `cybersin-cli` (the only crate that depends on both `cybersin-gateway`
//! and `cybersin-runtime` normally) delegating to
//! `cybersin_gateway::ToolExecutor`.

use async_trait::async_trait;
use serde_json::Value;

/// Result of one tool call — enough for `RuntimeDaemon::handle_tool_request`
/// to emit the same `ToolCall` span / `tool.call` event shape it already
/// does today.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutput {
    pub value: Value,
    pub retries: u32,
    pub usd_cost: f64,
}

/// A tool call's terminal failure — mirrors `cybersin_adapter::messages::
/// CallOutcome::Failed`'s shape exactly, since `RuntimeDaemon::
/// handle_tool_request` forwards `retriable` straight into that message.
/// A `ToolCaller` backed by a real `cybersin_gateway::ToolGateway` reports
/// the ledger's actual retriable determination here; `StubToolCaller`
/// never fails so it never constructs one.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCallFailure {
    pub reason: String,
    pub retriable: bool,
}

#[async_trait]
pub trait ToolCaller: Send + Sync {
    async fn call(
        &self,
        session_id: &str,
        call_id: &str,
        idem_key: Option<&str>,
        tool: &str,
        args: &Value,
    ) -> Result<ToolOutput, ToolCallFailure>;
}

/// Reproduces the ungated tool-call behavior `RuntimeDaemon` hardcoded
/// before issue #35 Phase 3 byte-for-byte (`retries: 1`, `usd_cost:
/// 0.0008`, a synthetic `{"tool": tool, "status": "ok"}` result) — the
/// default `RuntimeDaemon::new` still uses, so `cybersin run --stub`'s
/// behavior is unaffected by this seam's introduction.
pub struct StubToolCaller;

#[async_trait]
impl ToolCaller for StubToolCaller {
    async fn call(
        &self,
        _session_id: &str,
        _call_id: &str,
        _idem_key: Option<&str>,
        tool: &str,
        _args: &Value,
    ) -> Result<ToolOutput, ToolCallFailure> {
        Ok(ToolOutput {
            value: serde_json::json!({ "tool": tool, "status": "ok" }),
            retries: 1,
            usd_cost: 0.0008,
        })
    }
}

/// Forwarding impl, mirroring `model_caller.rs`'s `impl ModelCaller for
/// Box<dyn ModelCaller>` — lets `RuntimeDaemon` hold a boxed `ToolCaller`
/// and swap it via `with_tool_caller` without a second generic parameter.
#[async_trait]
impl ToolCaller for Box<dyn ToolCaller> {
    async fn call(
        &self,
        session_id: &str,
        call_id: &str,
        idem_key: Option<&str>,
        tool: &str,
        args: &Value,
    ) -> Result<ToolOutput, ToolCallFailure> {
        (**self)
            .call(session_id, call_id, idem_key, tool, args)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stub_tool_caller_reproduces_the_legacy_hardcoded_shape() {
        let caller = StubToolCaller;
        let output = caller
            .call(
                "sess-1",
                "web_search:sess-1:1",
                None,
                "web_search",
                &serde_json::json!({}),
            )
            .await
            .expect("stub never fails");
        assert_eq!(output.retries, 1);
        assert_eq!(output.usd_cost, 0.0008);
        assert_eq!(
            output.value,
            serde_json::json!({ "tool": "web_search", "status": "ok" })
        );
    }

    #[tokio::test]
    async fn boxed_tool_caller_forwards() {
        let boxed: Box<dyn ToolCaller> = Box::new(StubToolCaller);
        let output = boxed
            .call(
                "sess-1",
                "web_search:sess-1:1",
                None,
                "web_search",
                &serde_json::json!({}),
            )
            .await
            .expect("stub never fails");
        assert_eq!(output.retries, 1);
    }
}
