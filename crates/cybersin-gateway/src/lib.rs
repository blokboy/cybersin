//! `cybersin-gateway` — the idempotent tool gateway (spec §8.2, Project
//! 3): "All tool calls pass through `cybersind`: schema validation, then
//! the idempotency ledger — `tool_calls` UNIQUE `(tool, idem_key)`, states
//! `pending -> succeeded | failed`, DB constraint wins races."
//!
//! - [`schema`] — minimal required-field/type validation, the first gate
//!   a call passes through, before it ever reaches the ledger.
//! - The ledger itself lives in `cybersin_runtime::storage`'s `Storage`
//!   trait (the `tool_calls` table alongside `sessions`/`events`), not in
//!   this crate — see [`gateway`]'s module doc for why, and this issue's
//!   report for the full justification. This crate is the *policy* layer
//!   on top of that ledger.
//! - [`retry`] — the `read`/`write`/`critical` retry-class taxonomy (spec
//!   §8.2) and how many in-line auto-retries each gets.
//! - [`policy`] — the `PolicyHook` seam (spec §8.2: "rate limits,
//!   declarative argument guards, approval gates") and [`policy::ApprovalGate`],
//!   the one concrete hook this issue's acceptance criteria needs.
//! - [`executor`] — the seam between the gateway and whatever actually
//!   runs a tool; no real tool backend exists yet in this workspace, so
//!   [`executor::EchoExecutor`] stands in, the same way the M1 stub agent
//!   stands in for a real compiled agent.
//! - [`gateway::ToolGateway`] — ties all of the above together: submit a
//!   call, park/approve/deny it, and the `cybersin dlq ls|show|retry|drop`
//!   dead-letter queue operations.

pub mod error;
pub mod executor;
pub mod gateway;
pub mod policy;
pub mod retry;
pub mod schema;

pub use error::{GatewayError, Result};
pub use executor::{EchoExecutor, ToolExecutor};
pub use gateway::{GatewayOutcome, ToolGateway};
pub use policy::{ApprovalGate, PolicyContext, PolicyDecision, PolicyHook};
pub use retry::RetryClass;
pub use schema::{FieldType, SchemaError, ToolSchema};
