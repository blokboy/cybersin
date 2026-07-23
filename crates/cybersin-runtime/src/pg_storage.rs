//! Postgres implementation of the runtime storage boundary.

use async_trait::async_trait;
use serde_json::Value;
use sqlx::postgres::{PgPoolOptions, PgRow};
use sqlx::{PgPool, Row};

use crate::storage::{now_unix_ms, EventRecord, Result, SessionRecord, Storage, ToolCallRecord};

/// Multi-connection Postgres storage used by server-mode workers.
#[derive(Clone)]
pub struct PgStorage {
    pool: PgPool,
}

impl PgStorage {
    pub async fn connect(url: &str, max_connections: u32) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(max_connections)
            .connect(url)
            .await?;
        Self::from_pool(pool).await
    }

    pub async fn from_pool(pool: PgPool) -> Result<Self> {
        let storage = Self { pool };
        storage.migrate().await?;
        Ok(storage)
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    async fn migrate(&self) -> Result<()> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS sessions (
                session_id TEXT PRIMARY KEY,
                agent_name TEXT NOT NULL,
                status TEXT NOT NULL,
                created_unix_ms BIGINT NOT NULL
            )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS events (
                session_id TEXT NOT NULL,
                seq BIGINT NOT NULL,
                unix_ms BIGINT NOT NULL,
                kind TEXT NOT NULL,
                payload JSONB NOT NULL,
                PRIMARY KEY (session_id, seq)
            )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS tool_calls (
                call_id TEXT PRIMARY KEY,
                tool TEXT NOT NULL,
                idem_key TEXT NOT NULL,
                session_id TEXT NOT NULL,
                retry_class TEXT NOT NULL,
                args JSONB NOT NULL,
                status TEXT NOT NULL,
                attempts BIGINT NOT NULL DEFAULT 0,
                result JSONB,
                failure_reason TEXT,
                retriable BOOLEAN,
                awaiting_approval BOOLEAN NOT NULL DEFAULT FALSE,
                approval_id TEXT,
                dropped BOOLEAN NOT NULL DEFAULT FALSE,
                created_unix_ms BIGINT NOT NULL,
                updated_unix_ms BIGINT NOT NULL,
                UNIQUE (tool, idem_key)
            )",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    fn row_to_tool_call(row: PgRow) -> ToolCallRecord {
        ToolCallRecord {
            call_id: row.get("call_id"),
            tool: row.get("tool"),
            idem_key: row.get("idem_key"),
            session_id: row.get("session_id"),
            retry_class: row.get("retry_class"),
            args: row.get("args"),
            status: row.get("status"),
            attempts: row.get("attempts"),
            result: row.get("result"),
            failure_reason: row.get("failure_reason"),
            retriable: row.get("retriable"),
            awaiting_approval: row.get("awaiting_approval"),
            approval_id: row.get("approval_id"),
            dropped: row.get("dropped"),
            created_unix_ms: row.get("created_unix_ms"),
            updated_unix_ms: row.get("updated_unix_ms"),
        }
    }
}

#[async_trait]
impl Storage for PgStorage {
    async fn create_session(&self, session_id: &str, agent_name: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO sessions (session_id, agent_name, status, created_unix_ms)
             VALUES ($1, $2, 'running', $3) ON CONFLICT (session_id) DO NOTHING",
        )
        .bind(session_id)
        .bind(agent_name)
        .bind(now_unix_ms())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn set_session_status(&self, session_id: &str, status: &str) -> Result<()> {
        sqlx::query("UPDATE sessions SET status = $1 WHERE session_id = $2")
            .bind(status)
            .bind(session_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn get_session(&self, session_id: &str) -> Result<Option<SessionRecord>> {
        let row = sqlx::query(
            "SELECT session_id, agent_name, status, created_unix_ms
             FROM sessions WHERE session_id = $1",
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
            "SELECT session_id, agent_name, status, created_unix_ms
             FROM sessions ORDER BY created_unix_ms DESC",
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
        // A transaction-scoped advisory lock serializes sequence allocation
        // per session while allowing unrelated sessions to append in parallel.
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(session_id)
            .execute(&mut *tx)
            .await?;
        let seq: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM events WHERE session_id = $1",
        )
        .bind(session_id)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO events (session_id, seq, unix_ms, kind, payload)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(session_id)
        .bind(seq)
        .bind(now_unix_ms())
        .bind(kind)
        .bind(payload)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(seq)
    }

    async fn load_events(&self, session_id: &str) -> Result<Vec<EventRecord>> {
        let rows = sqlx::query(
            "SELECT session_id, seq, unix_ms, kind, payload FROM events
             WHERE session_id = $1 ORDER BY seq ASC",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| EventRecord {
                session_id: r.get("session_id"),
                seq: r.get("seq"),
                unix_ms: r.get("unix_ms"),
                kind: r.get("kind"),
                payload: r.get("payload"),
            })
            .collect())
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
        let result = sqlx::query(
            "INSERT INTO tool_calls
             (call_id, tool, idem_key, session_id, retry_class, args, status, attempts,
              awaiting_approval, dropped, created_unix_ms, updated_unix_ms)
             VALUES ($1, $2, $3, $4, $5, $6, 'pending', 0, FALSE, FALSE, $7, $7)
             ON CONFLICT (tool, idem_key) DO NOTHING",
        )
        .bind(call_id)
        .bind(tool)
        .bind(idem_key)
        .bind(session_id)
        .bind(retry_class)
        .bind(args)
        .bind(now_unix_ms())
        .execute(&self.pool)
        .await?;
        let won = result.rows_affected() == 1;
        let row = sqlx::query("SELECT * FROM tool_calls WHERE tool = $1 AND idem_key = $2")
            .bind(tool)
            .bind(idem_key)
            .fetch_one(&self.pool)
            .await?;
        Ok((Self::row_to_tool_call(row), won))
    }

    async fn get_tool_call(&self, call_id: &str) -> Result<Option<ToolCallRecord>> {
        Ok(sqlx::query("SELECT * FROM tool_calls WHERE call_id = $1")
            .bind(call_id)
            .fetch_optional(&self.pool)
            .await?
            .map(Self::row_to_tool_call))
    }

    async fn increment_tool_call_attempt(&self, call_id: &str) -> Result<()> {
        sqlx::query(
            "UPDATE tool_calls SET attempts = attempts + 1, updated_unix_ms = $1
             WHERE call_id = $2",
        )
        .bind(now_unix_ms())
        .bind(call_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn resolve_tool_call_succeeded(&self, call_id: &str, result: Value) -> Result<()> {
        sqlx::query(
            "UPDATE tool_calls SET status = 'succeeded', result = $1,
             failure_reason = NULL, retriable = NULL, updated_unix_ms = $2
             WHERE call_id = $3",
        )
        .bind(result)
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
            "UPDATE tool_calls SET status = 'failed', failure_reason = $1,
             retriable = $2, updated_unix_ms = $3 WHERE call_id = $4",
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
            "UPDATE tool_calls SET awaiting_approval = TRUE, approval_id = $1,
             updated_unix_ms = $2 WHERE call_id = $3",
        )
        .bind(approval_id)
        .bind(now_unix_ms())
        .bind(call_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn clear_tool_call_awaiting_approval(&self, call_id: &str) -> Result<()> {
        self.update_flag(
            "UPDATE tool_calls SET awaiting_approval = FALSE, updated_unix_ms = $1 WHERE call_id = $2",
            call_id,
        )
        .await
    }

    async fn reopen_tool_call(&self, call_id: &str) -> Result<()> {
        self.update_flag(
            "UPDATE tool_calls SET status = 'pending', failure_reason = NULL, retriable = NULL,
             dropped = FALSE, updated_unix_ms = $1 WHERE call_id = $2",
            call_id,
        )
        .await
    }

    async fn set_tool_call_dropped(&self, call_id: &str, dropped: bool) -> Result<()> {
        sqlx::query("UPDATE tool_calls SET dropped = $1, updated_unix_ms = $2 WHERE call_id = $3")
            .bind(dropped)
            .bind(now_unix_ms())
            .bind(call_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn list_dead_letters(&self) -> Result<Vec<ToolCallRecord>> {
        let rows = sqlx::query(
            "SELECT * FROM tool_calls WHERE status = 'failed' AND dropped = FALSE
             ORDER BY updated_unix_ms DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Self::row_to_tool_call).collect())
    }

    async fn count_tool_calls_for_session(&self, session_id: &str) -> Result<i64> {
        Ok(
            sqlx::query_scalar("SELECT COUNT(*) FROM tool_calls WHERE session_id = $1")
                .bind(session_id)
                .fetch_one(&self.pool)
                .await?,
        )
    }
}

impl PgStorage {
    async fn update_flag(&self, query: &'static str, call_id: &str) -> Result<()> {
        sqlx::query(query)
            .bind(now_unix_ms())
            .bind(call_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
