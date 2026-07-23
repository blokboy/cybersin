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
use sqlx::sqlite::{SqlitePoolOptions, SqliteRow};
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

/// One row of the idempotency ledger `tool_calls` (spec §8.2: "All tool
/// calls pass through cybersind: schema validation, then the idempotency
/// ledger — tool_calls UNIQUE (tool, idem_key), states pending ->
/// succeeded | failed, DB constraint wins races"). `call_id` is
/// `"{tool}:{idem_key}"`, computed once at insert time and stored so every
/// other lookup (`cybersin approve|deny|dlq <call-id>`) is a single-column
/// primary-key fetch instead of splitting the string back apart.
///
/// `awaiting_approval` is a flag on a `pending` row, not a fourth ledger
/// state — a parked call hasn't resolved yet, it's just pending with a
/// gate in front of it (spec §8.2's approval-gate policy hook). `dropped`
/// similarly doesn't change `status`; it just excludes an acknowledged
/// dead letter from `cybersin dlq ls` (spec's `dlq ls|show|retry|drop`).
///
/// `retry_class` is free-form text, like [`SessionRecord::status`] —
/// cybersin-gateway (issue #11) owns the `read|write|critical` vocabulary
/// and what each one does; storage just persists whatever string it's
/// given.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallRecord {
    pub call_id: String,
    pub tool: String,
    pub idem_key: String,
    pub session_id: String,
    pub retry_class: String,
    pub args: Value,
    /// `"pending" | "succeeded" | "failed"` (spec §8.2).
    pub status: String,
    pub attempts: i64,
    pub result: Option<Value>,
    pub failure_reason: Option<String>,
    pub retriable: Option<bool>,
    pub awaiting_approval: bool,
    pub approval_id: Option<String>,
    pub dropped: bool,
    pub created_unix_ms: i64,
    pub updated_unix_ms: i64,
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

    /// Admit `(tool, idem_key)` into the ledger as a fresh `pending` row —
    /// or, if that pair is already there, return the existing row instead
    /// of inserting a second one. The `UNIQUE(tool, idem_key)` constraint
    /// (not a check-then-insert race in application code) is what decides
    /// the winner when two callers race this concurrently (spec §8.2: "DB
    /// constraint wins races") — implementations must express this as one
    /// `INSERT ... ON CONFLICT DO NOTHING` and inspect the affected-row
    /// count, not as a `SELECT` followed by a conditional `INSERT`.
    /// Returns `(row, true)` for the caller that won the insert, `(row,
    /// false)` for every caller that lost it.
    #[allow(clippy::too_many_arguments)]
    async fn begin_tool_call(
        &self,
        call_id: &str,
        session_id: &str,
        tool: &str,
        idem_key: &str,
        retry_class: &str,
        args: &Value,
    ) -> Result<(ToolCallRecord, bool)>;

    async fn get_tool_call(&self, call_id: &str) -> Result<Option<ToolCallRecord>>;

    /// Record that another attempt at `call_id` is starting: `attempts +=
    /// 1`. Called for the winning insert's first attempt, every in-line
    /// auto-retry `cybersin-gateway`'s retry-class policy allows, and
    /// every manual `cybersin dlq retry`/`cybersin approve`.
    async fn increment_tool_call_attempt(&self, call_id: &str) -> Result<()>;

    /// Resolve a `pending` row to the terminal `succeeded` state.
    async fn resolve_tool_call_succeeded(&self, call_id: &str, result: Value) -> Result<()>;

    /// Resolve a `pending` row to the terminal `failed` state. A denied
    /// approval (`reason: "denied"`, `retriable: false`) takes this exact
    /// path too — spec §8.2: "distinct terminal outcome from a transient
    /// execution failure ... isn't treated as retriable by `dlq retry`."
    async fn resolve_tool_call_failed(
        &self,
        call_id: &str,
        reason: &str,
        retriable: bool,
    ) -> Result<()>;

    /// Flag a still-`pending` row as parked behind an approval gate (spec
    /// §8.2). Doesn't change `status` — see [`ToolCallRecord`]'s doc.
    async fn set_tool_call_awaiting_approval(&self, call_id: &str, approval_id: &str)
        -> Result<()>;

    /// Clear the approval-gate flag — `cybersin approve`/`cybersin deny`
    /// both call this before resolving the call one way or the other.
    async fn clear_tool_call_awaiting_approval(&self, call_id: &str) -> Result<()>;

    /// Reopen a `failed` row back to `pending` (`cybersin dlq retry`):
    /// clears the failure fields and the `dropped` flag so it disappears
    /// from `dlq ls` until (if ever) it fails again.
    async fn reopen_tool_call(&self, call_id: &str) -> Result<()>;

    /// Mark/unmark a dead letter as acknowledged (`cybersin dlq drop`) —
    /// excluded from `list_dead_letters` without deleting the audit row.
    async fn set_tool_call_dropped(&self, call_id: &str, dropped: bool) -> Result<()>;

    /// The dead-letter queue: `failed` rows not yet `drop`ped, most
    /// recently updated first.
    async fn list_dead_letters(&self) -> Result<Vec<ToolCallRecord>>;

    /// How many tool calls this session has ever admitted to the ledger —
    /// `cybersin-gateway`'s input to auto-deriving `"session:seq"` idem
    /// keys (spec §8.2) when a caller doesn't supply one.
    async fn count_tool_calls_for_session(&self, session_id: &str) -> Result<i64>;
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

        // spec §8.2's idempotency ledger. `UNIQUE(tool, idem_key)` is the
        // constraint `begin_tool_call`'s `ON CONFLICT` targets — this is
        // the actual race-arbiter, not the single-connection pool (a
        // future multi-connection Postgres impl, issue #24, must keep
        // relying on this same constraint).
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS tool_calls (
                call_id TEXT PRIMARY KEY,
                tool TEXT NOT NULL,
                idem_key TEXT NOT NULL,
                session_id TEXT NOT NULL,
                retry_class TEXT NOT NULL,
                args TEXT NOT NULL,
                status TEXT NOT NULL,
                attempts INTEGER NOT NULL DEFAULT 0,
                result TEXT,
                failure_reason TEXT,
                retriable INTEGER,
                awaiting_approval INTEGER NOT NULL DEFAULT 0,
                approval_id TEXT,
                dropped INTEGER NOT NULL DEFAULT 0,
                created_unix_ms INTEGER NOT NULL,
                updated_unix_ms INTEGER NOT NULL,
                UNIQUE (tool, idem_key)
            )
            "#,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    fn row_to_tool_call(row: SqliteRow) -> Result<ToolCallRecord> {
        let args_str: String = row.get("args");
        let result_str: Option<String> = row.get("result");
        Ok(ToolCallRecord {
            call_id: row.get("call_id"),
            tool: row.get("tool"),
            idem_key: row.get("idem_key"),
            session_id: row.get("session_id"),
            retry_class: row.get("retry_class"),
            args: serde_json::from_str(&args_str)?,
            status: row.get("status"),
            attempts: row.get("attempts"),
            result: result_str.map(|s| serde_json::from_str(&s)).transpose()?,
            failure_reason: row.get("failure_reason"),
            retriable: row.get::<Option<i64>, _>("retriable").map(|v| v != 0),
            awaiting_approval: row.get::<i64, _>("awaiting_approval") != 0,
            approval_id: row.get("approval_id"),
            dropped: row.get::<i64, _>("dropped") != 0,
            created_unix_ms: row.get("created_unix_ms"),
            updated_unix_ms: row.get("updated_unix_ms"),
        })
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

    async fn begin_tool_call(
        &self,
        call_id: &str,
        session_id: &str,
        tool: &str,
        idem_key: &str,
        retry_class: &str,
        args: &Value,
    ) -> Result<(ToolCallRecord, bool)> {
        let args_str = serde_json::to_string(args)?;
        let now = now_unix_ms();
        // The whole race arbitration is this one statement: on conflict
        // with the UNIQUE(tool, idem_key) constraint, this INSERT is a
        // no-op and `rows_affected() == 0` tells us we lost. Nothing here
        // reads before deciding whether to write.
        let result = sqlx::query(
            "INSERT INTO tool_calls \
             (call_id, tool, idem_key, session_id, retry_class, args, status, attempts, \
              awaiting_approval, dropped, created_unix_ms, updated_unix_ms) \
             VALUES (?, ?, ?, ?, ?, ?, 'pending', 0, 0, 0, ?, ?) \
             ON CONFLICT (tool, idem_key) DO NOTHING",
        )
        .bind(call_id)
        .bind(tool)
        .bind(idem_key)
        .bind(session_id)
        .bind(retry_class)
        .bind(&args_str)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;

        let won = result.rows_affected() == 1;
        let row = sqlx::query("SELECT * FROM tool_calls WHERE tool = ? AND idem_key = ?")
            .bind(tool)
            .bind(idem_key)
            .fetch_one(&self.pool)
            .await?;
        Ok((Self::row_to_tool_call(row)?, won))
    }

    async fn get_tool_call(&self, call_id: &str) -> Result<Option<ToolCallRecord>> {
        let row = sqlx::query("SELECT * FROM tool_calls WHERE call_id = ?")
            .bind(call_id)
            .fetch_optional(&self.pool)
            .await?;
        row.map(Self::row_to_tool_call).transpose()
    }

    async fn increment_tool_call_attempt(&self, call_id: &str) -> Result<()> {
        sqlx::query(
            "UPDATE tool_calls SET attempts = attempts + 1, updated_unix_ms = ? WHERE call_id = ?",
        )
        .bind(now_unix_ms())
        .bind(call_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn resolve_tool_call_succeeded(&self, call_id: &str, result: Value) -> Result<()> {
        let result_str = serde_json::to_string(&result)?;
        sqlx::query(
            "UPDATE tool_calls SET status = 'succeeded', result = ?, failure_reason = NULL, \
             retriable = NULL, updated_unix_ms = ? WHERE call_id = ?",
        )
        .bind(result_str)
        .bind(now_unix_ms())
        .bind(call_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn resolve_tool_call_failed(
        &self,
        call_id: &str,
        reason: &str,
        retriable: bool,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE tool_calls SET status = 'failed', failure_reason = ?, retriable = ?, \
             updated_unix_ms = ? WHERE call_id = ?",
        )
        .bind(reason)
        .bind(retriable)
        .bind(now_unix_ms())
        .bind(call_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn set_tool_call_awaiting_approval(
        &self,
        call_id: &str,
        approval_id: &str,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE tool_calls SET awaiting_approval = 1, approval_id = ?, updated_unix_ms = ? \
             WHERE call_id = ?",
        )
        .bind(approval_id)
        .bind(now_unix_ms())
        .bind(call_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn clear_tool_call_awaiting_approval(&self, call_id: &str) -> Result<()> {
        sqlx::query(
            "UPDATE tool_calls SET awaiting_approval = 0, updated_unix_ms = ? WHERE call_id = ?",
        )
        .bind(now_unix_ms())
        .bind(call_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn reopen_tool_call(&self, call_id: &str) -> Result<()> {
        sqlx::query(
            "UPDATE tool_calls SET status = 'pending', failure_reason = NULL, retriable = NULL, \
             dropped = 0, updated_unix_ms = ? WHERE call_id = ?",
        )
        .bind(now_unix_ms())
        .bind(call_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn set_tool_call_dropped(&self, call_id: &str, dropped: bool) -> Result<()> {
        sqlx::query("UPDATE tool_calls SET dropped = ?, updated_unix_ms = ? WHERE call_id = ?")
            .bind(dropped)
            .bind(now_unix_ms())
            .bind(call_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn list_dead_letters(&self) -> Result<Vec<ToolCallRecord>> {
        let rows = sqlx::query(
            "SELECT * FROM tool_calls WHERE status = 'failed' AND dropped = 0 \
             ORDER BY updated_unix_ms DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(Self::row_to_tool_call).collect()
    }

    async fn count_tool_calls_for_session(&self, session_id: &str) -> Result<i64> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tool_calls WHERE session_id = ?")
            .bind(session_id)
            .fetch_one(&self.pool)
            .await?;
        Ok(count)
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
    use std::sync::Arc;

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

    #[tokio::test]
    async fn begin_tool_call_first_caller_wins_second_sees_existing_row() {
        let storage = SqliteStorage::in_memory().await.unwrap();
        let (row1, won1) = storage
            .begin_tool_call(
                "web_search:k1",
                "sess-1",
                "web_search",
                "k1",
                "read",
                &serde_json::json!({"q": "cybernetics"}),
            )
            .await
            .unwrap();
        assert!(won1);
        assert_eq!(row1.status, "pending");

        let (row2, won2) = storage
            .begin_tool_call(
                "web_search:k1",
                "sess-1",
                "web_search",
                "k1",
                "read",
                &serde_json::json!({"q": "cybernetics"}),
            )
            .await
            .unwrap();
        assert!(!won2);
        assert_eq!(row2.call_id, row1.call_id);
    }

    #[tokio::test]
    async fn begin_tool_call_concurrent_races_only_one_insert_wins() {
        // The chaos test at the storage layer (spec §8.2: "DB constraint
        // wins races"): N callers race to insert the exact same (tool,
        // idem_key). Exactly one must see `won == true` — proving the
        // UNIQUE constraint arbitrates, not app-level locking (this pool
        // is capped at one connection, but the constraint is what a
        // multi-connection Postgres impl, issue #24, would have to lean
        // on too).
        let storage = Arc::new(SqliteStorage::in_memory().await.unwrap());
        let mut handles = Vec::new();
        for _ in 0..16 {
            let storage = storage.clone();
            handles.push(tokio::spawn(async move {
                storage
                    .begin_tool_call(
                        "pay:order-42",
                        "sess-1",
                        "pay",
                        "order-42",
                        "write",
                        &serde_json::json!({"amount": 100}),
                    )
                    .await
                    .unwrap()
                    .1
            }));
        }
        let mut wins = 0;
        for h in handles {
            if h.await.unwrap() {
                wins += 1;
            }
        }
        assert_eq!(wins, 1);
    }

    #[tokio::test]
    async fn tool_call_resolves_succeeded_and_failed() {
        let storage = SqliteStorage::in_memory().await.unwrap();
        storage
            .begin_tool_call("t:k1", "sess-1", "t", "k1", "read", &serde_json::json!({}))
            .await
            .unwrap();
        storage
            .resolve_tool_call_succeeded("t:k1", serde_json::json!({"ok": true}))
            .await
            .unwrap();
        let row = storage.get_tool_call("t:k1").await.unwrap().unwrap();
        assert_eq!(row.status, "succeeded");
        assert_eq!(row.result, Some(serde_json::json!({"ok": true})));

        storage
            .begin_tool_call(
                "t:k2",
                "sess-1",
                "t",
                "k2",
                "critical",
                &serde_json::json!({}),
            )
            .await
            .unwrap();
        storage
            .resolve_tool_call_failed("t:k2", "boom", false)
            .await
            .unwrap();
        let row = storage.get_tool_call("t:k2").await.unwrap().unwrap();
        assert_eq!(row.status, "failed");
        assert_eq!(row.failure_reason.as_deref(), Some("boom"));
        assert_eq!(row.retriable, Some(false));
    }

    #[tokio::test]
    async fn dead_letters_hide_dropped_rows() {
        let storage = SqliteStorage::in_memory().await.unwrap();
        storage
            .begin_tool_call(
                "t:k1",
                "sess-1",
                "t",
                "k1",
                "critical",
                &serde_json::json!({}),
            )
            .await
            .unwrap();
        storage
            .resolve_tool_call_failed("t:k1", "boom", false)
            .await
            .unwrap();
        assert_eq!(storage.list_dead_letters().await.unwrap().len(), 1);

        storage.set_tool_call_dropped("t:k1", true).await.unwrap();
        assert_eq!(storage.list_dead_letters().await.unwrap().len(), 0);

        storage.set_tool_call_dropped("t:k1", false).await.unwrap();
        assert_eq!(storage.list_dead_letters().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn reopen_tool_call_clears_failure_and_dropped() {
        let storage = SqliteStorage::in_memory().await.unwrap();
        storage
            .begin_tool_call("t:k1", "sess-1", "t", "k1", "write", &serde_json::json!({}))
            .await
            .unwrap();
        storage
            .resolve_tool_call_failed("t:k1", "boom", true)
            .await
            .unwrap();
        storage.set_tool_call_dropped("t:k1", true).await.unwrap();

        storage.reopen_tool_call("t:k1").await.unwrap();
        let row = storage.get_tool_call("t:k1").await.unwrap().unwrap();
        assert_eq!(row.status, "pending");
        assert!(row.failure_reason.is_none());
        assert!(!row.dropped);
    }

    #[tokio::test]
    async fn awaiting_approval_flag_round_trips() {
        let storage = SqliteStorage::in_memory().await.unwrap();
        storage
            .begin_tool_call("t:k1", "sess-1", "t", "k1", "write", &serde_json::json!({}))
            .await
            .unwrap();
        storage
            .set_tool_call_awaiting_approval("t:k1", "appr-1")
            .await
            .unwrap();
        let row = storage.get_tool_call("t:k1").await.unwrap().unwrap();
        assert!(row.awaiting_approval);
        assert_eq!(row.approval_id.as_deref(), Some("appr-1"));
        assert_eq!(row.status, "pending"); // awaiting_approval isn't a ledger state

        storage
            .clear_tool_call_awaiting_approval("t:k1")
            .await
            .unwrap();
        let row = storage.get_tool_call("t:k1").await.unwrap().unwrap();
        assert!(!row.awaiting_approval);
    }
}
