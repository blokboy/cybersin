//! `cybersin-runtime` — the `cybersind` daemon skeleton (spec §8 intro,
//! "Project 5" minimal slice) plus the M1 stub agent (spec §14).
//!
//! - [`daemon::DaemonHandle`] — auto-starts the (in-process, for now)
//!   daemon: a `Storage` trait object + a `cybersin_trace::SpanStore`
//!   sharing one SQLite file.
//! - [`storage`] — the `Storage` trait (SQLite implementation; Postgres
//!   is issue #24), its event-sourced session log, and the `tool_calls`
//!   idempotency ledger `cybersin-gateway` (issue #11) is built on.
//! - [`dist`] — loads the hand-written `dist/`-shaped fixture the stub
//!   agent runs against (spec §14 M1, *not* real compiler output).
//! - [`session::RuntimeDaemon`] — the real daemon-side session loop,
//!   speaking `cybersin_adapter`'s `HarnessMessage`/`DaemonMessage`
//!   protocol; the real counterpart to `cybersin_adapter`'s
//!   `DaemonDouble` test fixture.
//! - [`stub_agent`] — scripts a `StubHarness` against `RuntimeDaemon` to
//!   drive one full stub session end-to-end.
//!
//! Per spec §13's dependency discipline: this crate depends on
//! `cybersin-ir` (compiled artifact types) and `cybersin-adapter` (the
//! harness protocol), never on `cybersin-frontend`/`cybersin-passes` — the
//! runtime consumes artifacts, not sources.

pub mod daemon;
pub mod dist;
pub mod error;
pub mod session;
pub mod storage;
pub mod stub_agent;
pub mod supervisor;

pub use daemon::DaemonHandle;
pub use dist::{bundled_stub_dist_dir, DistError, DistFixture, DistManifest, RoutingEntry};
pub use error::RuntimeError;
pub use session::{estimate_tokens, RuntimeDaemon, RuntimeSessionSummary};
pub use storage::{
    CheckpointRecord, EventRecord, SessionRecord, SqliteStorage, StateRecord, Storage,
    StorageError, ToolCallRecord,
};
pub use supervisor::SessionSupervisor;
