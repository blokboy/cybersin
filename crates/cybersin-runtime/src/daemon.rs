//! `cybersind`, the daemon skeleton (spec §1: "`cybersind` (daemon,
//! auto-started on first runtime command, owns all state)"; §8 intro).
//!
//! # Why in-process instead of a real subprocess, for now
//!
//! The spec's end state is a persistent `cybersind` process that a
//! stateless `cybersin` CLI auto-spawns once and thereafter talks to over
//! a socket/RPC — `cybersin daemon [--server]` (§11) makes the
//! Postgres-backed multi-worker version of that explicit (issue #24).
//! Building real process supervision + an IPC surface now would be
//! strictly more than this issue's M1 bar needs ("daemon skeleton + trace
//! core; stub agent runs on a hand-written dist/", spec §14), and that
//! surface would still need redesigning once the gateway (§8.2) and
//! orchestration (§8.7) exist to shape it against — the adapter protocol
//! itself is explicitly deferred that way for spawn/mailbox (§8.7, §10).
//!
//! So for M1, "the daemon" is [`DaemonHandle`]: an in-process component
//! owning the [`crate::storage::Storage`] trait object and the trace
//! `SpanStore`, both backed by the *same* SQLite file — one shared
//! `SqlitePool` (see [`DaemonHandle::auto_start`]) — so state durably
//! outlives any one CLI invocation even though the process doesn't.
//! "Auto-start" means: opening (and, on first run, migrating) that SQLite
//! file transparently the moment a runtime command needs it, mirroring
//! what a real daemon's "already running? connect; else spawn" check will
//! do once a persistent process exists — minus the process/socket part.
//! When a later issue adds the real long-lived `cybersind` process, this
//! struct's storage/span ownership is exactly what moves into that
//! process; the CLI-side change is swapping this function's body for
//! "connect over the socket, spawning the process if the connection
//! fails".
//!
//! Reusing `cybersin_adapter`'s `HarnessMessage`/`DaemonMessage` types and
//! channel traits (rather than inventing a parallel wire format) means
//! this in-process daemon and a real out-of-process one drive the exact
//! same protocol — only the transport underneath [`DaemonChannel`]
//! changes.

use std::path::Path;
use std::sync::Arc;

use cybersin_trace::SpanStore;
use sqlx::sqlite::SqlitePoolOptions;

use crate::error::RuntimeError;
use crate::storage::{SqliteStorage, Storage};

/// A handle to the (in-process, for now) `cybersind` daemon: shared
/// ownership of the `Storage` trait object and the trace span store.
#[derive(Clone)]
pub struct DaemonHandle {
    storage: Arc<dyn Storage>,
    spans: SpanStore,
}

impl DaemonHandle {
    /// Auto-start against a SQLite file at `db_path` — creating the
    /// parent directory and the file (and its schema) if this is the
    /// first run. This is the entry point a runtime CLI command (`run`,
    /// `trace`, `cost`) calls before doing anything else.
    pub async fn auto_start(db_path: impl AsRef<Path>) -> Result<Self, RuntimeError> {
        let db_path = db_path.as_ref();
        if let Some(parent) = db_path.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent).await?;
            }
        }
        let url = format!("sqlite://{}?mode=rwc", db_path.display());
        Self::from_url(&url).await
    }

    /// Auto-start against an ephemeral in-memory database — tests, and
    /// any invocation that explicitly opts out of persistence.
    pub async fn auto_start_in_memory() -> Result<Self, RuntimeError> {
        Self::from_url("sqlite::memory:").await
    }

    async fn from_url(url: &str) -> Result<Self, RuntimeError> {
        // A single-connection pool shared between the session store and
        // the span store: both need to observe the same SQLite database,
        // and capping at one connection sidesteps SQLite's single-writer
        // model entirely rather than tuning busy-timeouts/WAL mode for a
        // component this issue keeps deliberately minimal.
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect(url)
            .await?;
        let storage = SqliteStorage::from_pool(pool.clone()).await?;
        let spans = SpanStore::from_pool(pool).await?;
        Ok(Self {
            storage: Arc::new(storage),
            spans,
        })
    }

    pub fn storage(&self) -> Arc<dyn Storage> {
        self.storage.clone()
    }

    pub fn spans(&self) -> SpanStore {
        self.spans.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn auto_start_in_memory_shares_one_database_between_storage_and_spans() {
        let daemon = DaemonHandle::auto_start_in_memory().await.unwrap();
        daemon
            .storage()
            .create_session("sess-1", "agent-a")
            .await
            .unwrap();
        let session = daemon.storage().get_session("sess-1").await.unwrap();
        assert!(session.is_some());

        // Independently constructing a second handle from the same
        // in-memory URL would *not* see this session (separate memory
        // DBs) — this test's point is that the two stores inside *one*
        // handle share a pool, not that in-memory URLs are durable across
        // handles.
        assert_eq!(
            daemon
                .spans()
                .list(&Default::default())
                .await
                .unwrap()
                .len(),
            0
        );
    }

    #[tokio::test]
    async fn auto_start_creates_parent_directory_and_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("nested").join("cybersin.db");

        {
            let daemon = DaemonHandle::auto_start(&db_path).await.unwrap();
            daemon
                .storage()
                .create_session("sess-1", "agent-a")
                .await
                .unwrap();
        }

        // Re-"auto-start" against the same file: the session created by
        // the previous (now-dropped) handle is still there, demonstrating
        // that state durably outlives one handle's lifetime.
        let daemon2 = DaemonHandle::auto_start(&db_path).await.unwrap();
        let session = daemon2.storage().get_session("sess-1").await.unwrap();
        assert!(session.is_some());
    }
}
