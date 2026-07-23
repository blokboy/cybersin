//! The seam between the gateway and whatever actually runs a tool.
//!
//! No real tool backend exists yet in this workspace (sandboxed tool
//! dispatch is a later issue, alongside `cybersin-sandbox`) — this crate's
//! job is the ledger/retry/approval machinery *around* execution, not
//! execution itself. [`ToolExecutor`] is that seam, mirroring how
//! `cybersin-runtime`'s M1 stub agent runs against a hand-written `dist/`
//! fixture instead of a real compiled agent: [`EchoExecutor`] is the
//! equivalent stand-in here, used by this crate's own tests and by the
//! CLI's `dlq`/`approve`/`deny` wiring until real backends exist.

use async_trait::async_trait;
use serde_json::Value;

/// Runs one tool call. `Err` is a transient/terminal execution failure
/// (its message becomes `CallOutcome::Failed::reason`); it is never how a
/// denied approval is represented — that path never reaches the executor
/// at all (spec §8.2).
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    async fn execute(&self, tool: &str, args: &Value) -> Result<Value, String>;
}

/// Always succeeds, echoing `tool`/`args` back as the result. The
/// gateway's default stand-in executor (see this module's doc) — every
/// real side effect this issue's tests need is instead modeled by
/// test-only executors that fail on demand (see `cybersin-gateway`'s
/// integration tests).
#[derive(Debug, Default, Clone, Copy)]
pub struct EchoExecutor;

#[async_trait]
impl ToolExecutor for EchoExecutor {
    async fn execute(&self, tool: &str, args: &Value) -> Result<Value, String> {
        Ok(serde_json::json!({
            "tool": tool,
            "echoed_args": args,
            "status": "ok",
        }))
    }
}
