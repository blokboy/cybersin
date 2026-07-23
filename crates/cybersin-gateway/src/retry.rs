//! Retry classes (spec §8.2): "`read` (retry freely), `write` (retry with
//! key), `critical` (never auto-retry)."
//!
//! These only govern *automatic* retries [`crate::gateway::ToolGateway`]
//! attempts in-line, inside one [`crate::gateway::ToolGateway::call`],
//! before a failure is ever written to the ledger as terminal. A manual
//! `cybersin dlq retry` is an explicit human override and bypasses this
//! entirely, including for `critical` calls — spec §8.2: "Critical
//! class's 'never auto-retry' already stops the runtime from silently
//! resubmitting a denied call; the gateway's job ends at recording the
//! human's decision durably." Once a call is terminal (`succeeded` or
//! `failed`), [`crate::gateway::ToolGateway::call`] never silently
//! resurrects it even for `read`/`write` classes — only `dlq retry` (a
//! failed call) or `approve` (a parked call) reopen a terminal/parked row.
//!
//! Stored in the ledger as free-form text (see
//! `cybersin_runtime::storage::ToolCallRecord::retry_class`'s doc) — this
//! enum is cybersin-gateway's typed view of that column.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryClass {
    /// No side effects — safe to retry as many times as the gateway's
    /// bounded in-line budget allows.
    Read,
    /// Mutates state — retried in-line reusing the *same* idem_key, so a
    /// resubmitted write can never double-execute (the ledger already
    /// guarantees that; this just means "it's safe to try again").
    Write,
    /// Never auto-retried, in-line or otherwise. A failure is immediately
    /// terminal and lands in the dead-letter queue for a human to
    /// `cybersin dlq retry`.
    Critical,
}

impl RetryClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            RetryClass::Read => "read",
            RetryClass::Write => "write",
            RetryClass::Critical => "critical",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "read" => Some(RetryClass::Read),
            "write" => Some(RetryClass::Write),
            "critical" => Some(RetryClass::Critical),
            _ => None,
        }
    }

    /// Extra attempts beyond the first that [`crate::gateway::ToolGateway::call`]
    /// will make in-line after an executor failure, before recording the
    /// call as terminally `failed`. Deliberately small, bounded numbers —
    /// "retry freely" doesn't mean "retry forever": an unbounded loop
    /// inside one gateway call would just turn a slow failure into a
    /// hang.
    pub fn max_auto_retries(&self) -> u32 {
        match self {
            RetryClass::Read => 3,
            RetryClass::Write => 1,
            RetryClass::Critical => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_str() {
        for class in [RetryClass::Read, RetryClass::Write, RetryClass::Critical] {
            assert_eq!(RetryClass::parse(class.as_str()), Some(class));
        }
    }

    #[test]
    fn critical_never_auto_retries() {
        assert_eq!(RetryClass::Critical.max_auto_retries(), 0);
    }

    #[test]
    fn read_retries_more_than_write() {
        assert!(RetryClass::Read.max_auto_retries() > RetryClass::Write.max_auto_retries());
    }
}
