//! `cybersin-trace` — the trace & cost core (spec §8.5, "Project 1").
//!
//! An OTel-compatible span per LLM call, tool call, sandbox exec, or cache
//! decision (see [`span::Span`]), persisted in a SQLite-backed
//! [`store::SpanStore`] via sqlx (no ORM, per spec §13), queryable offline
//! by the CLI: `cybersin trace ls|show` lists/inspects raw spans,
//! `cybersin cost --by <dim>` rolls spend up by session, agent, model,
//! tool, or day.
//!
//! This crate knows nothing about the adapter protocol, the session
//! supervisor, or the daemon process — `cybersin-runtime` is the crate
//! that writes spans in anger, by calling [`store::SpanStore`] from its
//! session loop. Keeping the store's dependencies to serde/sqlx/thiserror
//! only (no `cybersin-adapter`, no `cybersin-runtime`) matches spec §13's
//! dependency discipline: this crate describes runtime *observations*,
//! not the runtime itself.

pub mod cost;
pub mod span;
pub mod store;

pub use cost::{CostDimension, CostRollupRow, ParseCostDimensionError};
pub use span::{CacheStatus, Span, SpanKind, SpanStatus};
pub use store::{SpanFilter, SpanStore, TraceError};
