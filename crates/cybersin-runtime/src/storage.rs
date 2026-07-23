//! Session storage (spec §8: "Storage behind a `Storage` trait: SQLite
//! (dev) and Postgres (server) via sqlx, no ORM") and the event-sourced
//! session loop's durable log (spec §8.1: "append-only `events`").
//!
//! This issue builds only the SQLite half of that trait boundary — a
//! `PgStorage` implementing the same [`Storage`] trait against Postgres is
//! issue #24 (`cybersind --server`). The trait is what matters here: every
//! caller in this crate (the [`crate::session::RuntimeDaemon`] session
//! loop, the CLI's `trace`/`sessions` views once they exist) depends on
//! `dyn Storage`, never on `SqliteStorage` directly.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::{Row, SqlitePool};

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("sqlite error: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("(de)serialization error: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, StorageError>;

/// One row of the `sessions` table: a session's identity and current
/// status. Sessions pin `agent_hash`/build hash in the real spec (§8.1);
/// this skeleton tracks just enough (`agent_name`, `status`) for M1's
/// `trace`/`cost` views to attribute spans to a session and an agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub session_id: String,
    pub agent_name: String,
    /// `"running" | "completed" | "aborted"`. Free-form rather than an
    /// enum for now — the real state machine (parked/awaiting_approval/
    /// etc., spec §8.1-§8.2) is a later issue's concern.
    pub status: String,
    pub created_unix_ms: i64,
}

/// One row of the append-only `events` log for a session (spec §8.1).
/// `payload` carries whatever JSON is relevant to `kind` — this skeleton
/// doesn't yet need a typed `SessionEvent` enum shared across crates since
/// nothing replays these events yet (that's resume, spec §8.1, a later
/// issue); recording them durably in an inspectable shape is this issue's
/// bar.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventRecord {
    pub session_id: String,
    pub seq: i64,
    pub unix_ms: i64,
    pub kind: String,
    pub payload: Value,
}

/// Storage trait boundary (spec §8). SQLite today; Postgres (`cybersind
/// --server`) is issue #24, implementing this same trait.
#[async_trait]
pub trait Storage: Send + Sync {
    async fn create_session(&self, session_id: &str, agent_name: &str) -> Result<()>;
    async fn set_session_status(&self, session_id: &str, status: &str) -> Result<()>;
    async fn get_session(&self, session_id: &str) -> Result<Option<SessionRecord>>;
    async fn list_sessions(&self) -> Result<Vec<SessionRecord>>;
    /// Append one event to a session's append-only log; returns the
    /// assigned sequence number.
    async fn append_event(&self, session_id: &str, kind: &str, payload: Value) -> Result<i64>;
    async fn load_events(&self, session_id: &str) -> Result<Vec<EventRecord>>;
}

/// SQLite implementation of [`Storage`] via sqlx, hand-written SQL (no
/// ORM, per spec §13) run through the runtime `query`/`query_as` API
/// rather than the compile-time `query!` macros — so building this crate
/// never needs a live database.
pub struct SqliteStorage {
    pool: SqlitePool,
}

impl SqliteStorage {
    /// Connect to (and migrate) a fresh pool at `url`.
    pub async fn connect(url: &str) -> Result<Self> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect(url)
            .await?;
        Self::from_pool(pool).await
    }

    /// An in-memory store — tests and ephemeral runs.
    pub async fn in_memory() -> Result<Self> {
        Self::connect("sqlite::memory:").await
    }

    /// Build from an already-open pool (e.g. shared with
    /// `cybersin-trace`'s `SpanStore` against the same sqlite file — see
    /// [`crate::daemon::DaemonHandle`]).
    pub async fn from_pool(pool: SqlitePool) -> Result<Self> {
        let storage = Self { pool };
        storage.migrate().await?;
        Ok(storage)
    }

    async fn migrate(&self) -> Result<()> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS sessions (
                session_id TEXT PRIMARY KEY,
                agent_name TEXT NOT NULL,
                status TEXT NOT NULL,
                created_unix_ms INTEGER NOT NULL
            )
            "#,
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS events (
                session_id TEXT NOT NULL,
                seq INTEGER NOT NULL,
                unix_ms INTEGER NOT NULL,
                kind TEXT NOT NULL,
                payload TEXT NOT NULL,
                PRIMARY KEY (session_id, seq)
            )
            "#,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

#[async_trait]
impl Storage for SqliteStorage {
    async fn create_session(&self, session_id: &str, agent_name: &str) -> Result<()> {
        let now = now_unix_ms();
        sqlx::query(
            "INSERT OR IGNORE INTO sessions (session_id, agent_name, status, created_unix_ms) \
             VALUES (?, ?, 'running', ?)",
        )
        .bind(session_id)
        .bind(agent_name)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn set_session_status(&self, session_id: &str, status: &str) -> Result<()> {
        sqlx::query("UPDATE sessions SET status = ? WHERE session_id = ?")
            .bind(status)
            .bind(session_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn get_session(&self, session_id: &str) -> Result<Option<SessionRecord>> {
        let row = sqlx::query(
            "SELECT session_id, agent_name, status, created_unix_ms FROM sessions WHERE session_id = ?",
        )
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| SessionRecord {
            session_id: r.get("session_id"),
            agent_name: r.get("agent_name"),
            status: r.get("status"),
            created_unix_ms: r.get("created_unix_ms"),
        }))
    }

    async fn list_sessions(&self) -> Result<Vec<SessionRecord>> {
        let rows = sqlx::query(
            "SELECT session_id, agent_name, status, created_unix_ms FROM sessions \
             ORDER BY created_unix_ms DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| SessionRecord {
                session_id: r.get("session_id"),
                agent_name: r.get("agent_name"),
                status: r.get("status"),
                created_unix_ms: r.get("created_unix_ms"),
            })
            .collect())
    }

    async fn append_event(&self, session_id: &str, kind: &str, payload: Value) -> Result<i64> {
        let payload_str = serde_json::to_string(&payload)?;
        let now = now_unix_ms();
        // Single-connection pool (see DaemonHandle) makes this
        // read-then-insert race-free without an explicit transaction:
        // there is only ever one sqlite connection in flight.
        let next_seq: i64 =
            sqlx::query_scalar("SELECT COALESCE(MAX(seq), 0) + 1 FROM events WHERE session_id = ?")
                .bind(session_id)
                .fetch_one(&self.pool)
                .await?;
        sqlx::query(
            "INSERT INTO events (session_id, seq, unix_ms, kind, payload) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(session_id)
        .bind(next_seq)
        .bind(now)
        .bind(kind)
        .bind(payload_str)
        .execute(&self.pool)
        .await?;
        Ok(next_seq)
    }

    async fn load_events(&self, session_id: &str) -> Result<Vec<EventRecord>> {
        let rows = sqlx::query(
            "SELECT session_id, seq, unix_ms, kind, payload FROM events \
             WHERE session_id = ? ORDER BY seq ASC",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let payload_str: String = row.get("payload");
            let payload: Value = serde_json::from_str(&payload_str)?;
            out.push(EventRecord {
                session_id: row.get("session_id"),
                seq: row.get("seq"),
                unix_ms: row.get("unix_ms"),
                kind: row.get("kind"),
                payload,
            });
        }
        Ok(out)
    }
}

fn now_unix_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn create_and_get_session_round_trips() {
        let storage = SqliteStorage::in_memory().await.unwrap();
        storage
            .create_session("sess-1", "research-agent")
            .await
            .unwrap();
        let record = storage.get_session("sess-1").await.unwrap().unwrap();
        assert_eq!(record.session_id, "sess-1");
        assert_eq!(record.agent_name, "research-agent");
        assert_eq!(record.status, "running");
    }

    #[tokio::test]
    async fn set_session_status_updates_row() {
        let storage = SqliteStorage::in_memory().await.unwrap();
        storage
            .create_session("sess-1", "research-agent")
            .await
            .unwrap();
        storage
            .set_session_status("sess-1", "completed")
            .await
            .unwrap();
        let record = storage.get_session("sess-1").await.unwrap().unwrap();
        assert_eq!(record.status, "completed");
    }

    #[tokio::test]
    async fn append_event_assigns_increasing_seq() {
        let storage = SqliteStorage::in_memory().await.unwrap();
        storage
            .create_session("sess-1", "research-agent")
            .await
            .unwrap();
        let seq1 = storage
            .append_event("sess-1", "session.started", serde_json::json!({}))
            .await
            .unwrap();
        let seq2 = storage
            .append_event("sess-1", "llm.call", serde_json::json!({"cost": 0.01}))
            .await
            .unwrap();
        assert_eq!(seq1, 1);
        assert_eq!(seq2, 2);

        let events = storage.load_events("sess-1").await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, "session.started");
        assert_eq!(events[1].kind, "llm.call");
        assert_eq!(events[1].payload["cost"], 0.01);
    }

    #[tokio::test]
    async fn events_are_scoped_per_session() {
        let storage = SqliteStorage::in_memory().await.unwrap();
        storage.create_session("sess-1", "agent-a").await.unwrap();
        storage.create_session("sess-2", "agent-a").await.unwrap();
        storage
            .append_event("sess-1", "k", serde_json::json!({}))
            .await
            .unwrap();
        storage
            .append_event("sess-2", "k", serde_json::json!({}))
            .await
            .unwrap();
        storage
            .append_event("sess-2", "k", serde_json::json!({}))
            .await
            .unwrap();

        assert_eq!(storage.load_events("sess-1").await.unwrap().len(), 1);
        assert_eq!(storage.load_events("sess-2").await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn list_sessions_returns_all() {
        let storage = SqliteStorage::in_memory().await.unwrap();
        storage.create_session("sess-1", "agent-a").await.unwrap();
        storage.create_session("sess-2", "agent-b").await.unwrap();
        let sessions = storage.list_sessions().await.unwrap();
        assert_eq!(sessions.len(), 2);
    }
}
