//! `cybersin-runtime` — the `cybersind` daemon skeleton (spec §8 intro,
//! "Project 5" minimal slice) plus the M1 stub agent (spec §14).
//!
//! - [`daemon::DaemonHandle`] — auto-starts the (in-process, for now)
//!   daemon: a `Storage` trait object + a `cybersin_trace::SpanStore`
//!   sharing one SQLite file.
//! - [`storage`] — the `Storage` trait (SQLite and Postgres
//!   implementations), its event-sourced session log, and the `tool_calls`
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

pub mod budget;
pub mod daemon;
pub mod dist;
pub mod error;
pub mod orchestration;
mod pg_storage;
pub mod route_executor;
pub mod sandbox_executor;
pub mod session;
pub mod storage;
pub mod stub_agent;
pub mod supervisor;

pub use budget::{BudgetConfig, OnBreach};
pub use daemon::{serve_server, DaemonHandle, ServerConfig};
pub use dist::{
    bundled_stub_dist_dir, DistError, DistFixture, DistManifest, RoutingEntry, ToolPolicy,
};
pub use error::RuntimeError;
pub use orchestration::{
    Mail, OrchestrationError, Orchestrator, Worker, WorkerExit, DEFAULT_MAX_RESTARTS,
};
pub use pg_storage::PgStorage;
pub use route_executor::{
    cache_key, CacheArtifact, CacheEntry, ExecutionRequest, ExecutionResponse, Judge, KnnBackend,
    ModelCaller, ModelOutput, RouteExecutor, RouteExecutorError, SQLITE_VEC_EVALUATION,
};
pub use sandbox_executor::RuntimeSandbox;
pub use session::{estimate_tokens, RuntimeDaemon, RuntimeSessionSummary};
pub use storage::{
    CasOutcome, CheckpointRecord, EventRecord, SessionRecord, SqliteStorage, StateRecord, Storage,
    StorageError, ToolCallRecord,
};
pub use supervisor::SessionSupervisor;
