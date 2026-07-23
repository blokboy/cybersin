//! Policy hooks (spec §8.2: "rate limits, declarative argument guards,
//! approval gates"). [`PolicyHook`] is the extensible seam all three are
//! meant to implement; only [`ApprovalGate`] is fleshed out here, because
//! it's the one the acceptance criteria actually exercises
//! (`cybersin approve|deny`). Rate limits and argument guards are real
//! future implementations of this same trait, not a separate mechanism —
//! adding one later means writing a `PolicyHook` impl, not touching
//! [`crate::gateway::ToolGateway`].

use async_trait::async_trait;
use serde_json::Value;

use crate::retry::RetryClass;

/// What a call is being evaluated for, handed to every registered hook in
/// registration order before it's admitted to execution.
pub struct PolicyContext<'a> {
    pub session_id: &'a str,
    pub tool: &'a str,
    pub args: &'a Value,
    pub retry_class: RetryClass,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PolicyDecision {
    /// Proceed to execution.
    Allow,
    /// Park the call (and the session) pending `cybersin approve|deny`
    /// (spec §8.2).
    RequireApproval,
    /// Fail the call immediately, without ever executing it (e.g. a rate
    /// limit or an argument guard rejecting the call outright).
    Reject { reason: String },
}

#[async_trait]
pub trait PolicyHook: Send + Sync {
    async fn evaluate(&self, ctx: &PolicyContext<'_>) -> PolicyDecision;
}

/// The concrete policy hook the acceptance criteria exercises: flags
/// every call to a configured set of tool names as requiring approval.
/// A real system would likely key this off richer conditions (argument
/// values, cost thresholds, ...) — tool-name matching is the minimal
/// shape that proves the park/resume mechanism end to end without
/// over-building a policy DSL nothing yet asks for.
pub struct ApprovalGate {
    gated_tools: Vec<String>,
}

impl ApprovalGate {
    pub fn for_tools<I, S>(tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            gated_tools: tools.into_iter().map(Into::into).collect(),
        }
    }
}

#[async_trait]
impl PolicyHook for ApprovalGate {
    async fn evaluate(&self, ctx: &PolicyContext<'_>) -> PolicyDecision {
        if self.gated_tools.iter().any(|t| t == ctx.tool) {
            PolicyDecision::RequireApproval
        } else {
            PolicyDecision::Allow
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx<'a>(tool: &'a str, args: &'a Value) -> PolicyContext<'a> {
        PolicyContext {
            session_id: "sess-1",
            tool,
            args,
            retry_class: RetryClass::Write,
        }
    }

    #[tokio::test]
    async fn gates_only_configured_tools() {
        let gate = ApprovalGate::for_tools(["wire_transfer"]);
        let args = json!({});
        assert_eq!(
            gate.evaluate(&ctx("wire_transfer", &args)).await,
            PolicyDecision::RequireApproval
        );
        assert_eq!(
            gate.evaluate(&ctx("web_search", &args)).await,
            PolicyDecision::Allow
        );
    }
}
