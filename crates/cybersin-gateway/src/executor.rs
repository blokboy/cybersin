//! The seam between the gateway and whatever actually runs a tool.
//!
//! This crate owns the ledger/retry/approval machinery *around*
//! execution, while concrete dispatch belongs to callers.
//! `cybersin-cli` supplies the production sandboxed implementation;
//! [`EchoExecutor`] remains a deterministic test double.

use async_trait::async_trait;
use serde_json::Value;

/// Runs one tool call. `Err` is a transient/terminal execution failure
/// (its message becomes `CallOutcome::Failed::reason`); it is never how a
/// denied approval is represented — that path never reaches the executor
/// at all (spec §8.2).
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    async fn execute(
        &self,
        session_id: &str,
        call_id: &str,
        tool: &str,
        args: &Value,
    ) -> Result<Value, String>;
}

/// Always succeeds, echoing `tool`/`args` back as the result. The
/// gateway test double (see this module's doc).
#[derive(Debug, Default, Clone, Copy)]
pub struct EchoExecutor;

#[async_trait]
impl ToolExecutor for EchoExecutor {
    async fn execute(
        &self,
        _session_id: &str,
        _call_id: &str,
        tool: &str,
        args: &Value,
    ) -> Result<Value, String> {
        Ok(serde_json::json!({
            "tool": tool,
            "echoed_args": args,
            "status": "ok",
        }))
    }
}
