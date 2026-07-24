//! Postgres implementation of the runtime storage boundary.

use async_trait::async_trait;
use serde_json::Value;
use sqlx::postgres::{PgPoolOptions, PgRow};
use sqlx::{PgPool, Row};

use crate::storage::{
    json_type, now_unix_ms, CasOutcome, CheckpointRecord, EventRecord, Result, SessionRecord,
    StateRecord, Storage, StorageError, ToolCallRecord,
};

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
                created_unix_ms BIGINT NOT NULL,
                config_hash TEXT NOT NULL DEFAULT ''
            )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "ALTER TABLE sessions ADD COLUMN IF NOT EXISTS config_hash TEXT NOT NULL DEFAULT ''",
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
            "CREATE TABLE IF NOT EXISTS session_state (
                session_id TEXT NOT NULL,
                namespace TEXT NOT NULL,
                key TEXT NOT NULL,
                value_type TEXT NOT NULL,
                value JSONB NOT NULL,
                updated_seq BIGINT NOT NULL,
                PRIMARY KEY (session_id, namespace, key)
            )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS checkpoints (
                checkpoint_id BIGSERIAL PRIMARY KEY,
                session_id TEXT NOT NULL,
                event_seq BIGINT NOT NULL,
                label TEXT,
                state JSONB NOT NULL,
                created_unix_ms BIGINT NOT NULL
            )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS signals (
                signal_id BIGSERIAL PRIMARY KEY,
                session_id TEXT NOT NULL,
                signal TEXT NOT NULL,
                payload JSONB NOT NULL,
                delivered BOOLEAN NOT NULL DEFAULT FALSE
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

    fn row_to_state(row: PgRow) -> StateRecord {
        StateRecord {
            namespace: row.get("namespace"),
            key: row.get("key"),
            value_type: row.get("value_type"),
            value: row.get("value"),
            updated_seq: row.get("updated_seq"),
        }
    }
}

#[async_trait]
impl Storage for PgStorage {
    async fn create_session(&self, session_id: &str, agent_name: &str) -> Result<()> {
        self.create_session_pinned(session_id, agent_name, "").await
    }

    async fn create_session_pinned(
        &self,
        session_id: &str,
        agent_name: &str,
        config_hash: &str,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO sessions
             (session_id, agent_name, status, created_unix_ms, config_hash)
             VALUES ($1, $2, 'running', $3, $4) ON CONFLICT (session_id) DO NOTHING",
        )
        .bind(session_id)
        .bind(agent_name)
        .bind(now_unix_ms())
        .bind(config_hash)
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
            "SELECT session_id, agent_name, status, created_unix_ms, config_hash
             FROM sessions WHERE session_id = $1",
        )
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| SessionRecord {
            session_id: r.get("session_id"),
            agent_name: r.get("agent_name"),
            status: r.get("status"),
            config_hash: r.get("config_hash"),
            created_unix_ms: r.get("created_unix_ms"),
        }))
    }

    async fn list_sessions(&self) -> Result<Vec<SessionRecord>> {
        let rows = sqlx::query(
            "SELECT session_id, agent_name, status, created_unix_ms, config_hash
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
                config_hash: r.get("config_hash"),
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

    async fn set_state(
        &self,
        session_id: &str,
        namespace: &str,
        key: &str,
        value: &Value,
    ) -> Result<()> {
        let value_type = json_type(value);
        if let Some(existing) = self.get_state(session_id, namespace, key).await? {
            if existing.value_type != value_type {
                return Err(StorageError::StateType {
                    namespace: namespace.into(),
                    key: key.into(),
                    expected: existing.value_type,
                    actual: value_type.into(),
                });
            }
        }
        let seq = self
            .append_event(
                session_id,
                "state.set",
                serde_json::json!({
                    "namespace": namespace,
                    "key": key,
                    "value_type": value_type,
                    "value": value
                }),
            )
            .await?;
        sqlx::query(
            "INSERT INTO session_state
             (session_id, namespace, key, value_type, value, updated_seq)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (session_id, namespace, key) DO UPDATE SET
             value = EXCLUDED.value, updated_seq = EXCLUDED.updated_seq",
        )
        .bind(session_id)
        .bind(namespace)
        .bind(key)
        .bind(value_type)
        .bind(value)
        .bind(seq)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get_state(
        &self,
        session_id: &str,
        namespace: &str,
        key: &str,
    ) -> Result<Option<StateRecord>> {
        Ok(sqlx::query(
            "SELECT namespace, key, value_type, value, updated_seq
             FROM session_state
             WHERE session_id = $1 AND namespace = $2 AND key = $3",
        )
        .bind(session_id)
        .bind(namespace)
        .bind(key)
        .fetch_optional(&self.pool)
        .await?
        .map(Self::row_to_state))
    }

    async fn list_state(&self, session_id: &str) -> Result<Vec<StateRecord>> {
        Ok(sqlx::query(
            "SELECT namespace, key, value_type, value, updated_seq
             FROM session_state WHERE session_id = $1 ORDER BY namespace, key",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(Self::row_to_state)
        .collect())
    }

    async fn cas_state(
        &self,
        session_id: &str,
        namespace: &str,
        key: &str,
        expected_version: Option<i64>,
        value: &Value,
    ) -> Result<CasOutcome> {
        let value_type = json_type(value);
        let applied = match expected_version {
            None => {
                sqlx::query(
                    "INSERT INTO session_state
                     (session_id, namespace, key, value_type, value, updated_seq)
                     VALUES ($1, $2, $3, $4, $5, 1)
                     ON CONFLICT (session_id, namespace, key) DO NOTHING",
                )
                .bind(session_id)
                .bind(namespace)
                .bind(key)
                .bind(value_type)
                .bind(value)
                .execute(&self.pool)
                .await?
                .rows_affected()
                    == 1
            }
            Some(expected) => {
                sqlx::query(
                    "UPDATE session_state
                     SET value = $1, value_type = $2, updated_seq = updated_seq + 1
                     WHERE session_id = $3 AND namespace = $4 AND key = $5 AND updated_seq = $6",
                )
                .bind(value)
                .bind(value_type)
                .bind(session_id)
                .bind(namespace)
                .bind(key)
                .bind(expected)
                .execute(&self.pool)
                .await?
                .rows_affected()
                    == 1
            }
        };
        if !applied {
            let actual = self
                .get_state(session_id, namespace, key)
                .await?
                .map(|r| r.updated_seq);
            return Ok(CasOutcome::Stale { actual });
        }
        self.append_event(
            session_id,
            "state.set",
            serde_json::json!({
                "namespace": namespace, "key": key, "value_type": value_type, "value": value
            }),
        )
        .await?;
        Ok(CasOutcome::Applied(
            self.get_state(session_id, namespace, key)
                .await?
                .expect("cas_state's own write just materialized this row"),
        ))
    }

    async fn create_checkpoint(
        &self,
        session_id: &str,
        label: Option<&str>,
    ) -> Result<CheckpointRecord> {
        let state = serde_json::to_value(self.list_state(session_id).await?)?;
        let event_seq = self
            .append_event(
                session_id,
                "checkpoint",
                serde_json::json!({"label": label, "state": state}),
            )
            .await?;
        let now = now_unix_ms();
        let checkpoint_id: i64 = sqlx::query_scalar(
            "INSERT INTO checkpoints
             (session_id, event_seq, label, state, created_unix_ms)
             VALUES ($1, $2, $3, $4, $5) RETURNING checkpoint_id",
        )
        .bind(session_id)
        .bind(event_seq)
        .bind(label)
        .bind(&state)
        .bind(now)
        .fetch_one(&self.pool)
        .await?;
        Ok(CheckpointRecord {
            checkpoint_id,
            session_id: session_id.into(),
            event_seq,
            label: label.map(str::to_owned),
            state,
            created_unix_ms: now,
        })
    }

    async fn latest_checkpoint(&self, session_id: &str) -> Result<Option<CheckpointRecord>> {
        Ok(sqlx::query(
            "SELECT checkpoint_id, session_id, event_seq, label, state, created_unix_ms
             FROM checkpoints WHERE session_id = $1
             ORDER BY checkpoint_id DESC LIMIT 1",
        )
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await?
        .map(|row| CheckpointRecord {
            checkpoint_id: row.get("checkpoint_id"),
            session_id: row.get("session_id"),
            event_seq: row.get("event_seq"),
            label: row.get("label"),
            state: row.get("state"),
            created_unix_ms: row.get("created_unix_ms"),
        }))
    }

    async fn enqueue_signal(&self, session_id: &str, signal: &str, payload: &Value) -> Result<()> {
        sqlx::query("INSERT INTO signals (session_id, signal, payload) VALUES ($1, $2, $3)")
            .bind(session_id)
            .bind(signal)
            .bind(payload)
            .execute(&self.pool)
            .await?;
        self.append_event(
            session_id,
            "signal.notified",
            serde_json::json!({"signal": signal, "payload": payload}),
        )
        .await?;
        Ok(())
    }

    async fn take_signal(&self, session_id: &str, signal: &str) -> Result<Option<Value>> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            "SELECT signal_id, payload FROM signals
             WHERE session_id = $1 AND signal = $2 AND delivered = FALSE
             ORDER BY signal_id LIMIT 1 FOR UPDATE SKIP LOCKED",
        )
        .bind(session_id)
        .bind(signal)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            tx.commit().await?;
            return Ok(None);
        };
        let signal_id: i64 = row.get("signal_id");
        let payload: Value = row.get("payload");
        sqlx::query("UPDATE signals SET delivered = TRUE WHERE signal_id = $1")
            .bind(signal_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        self.append_event(
            session_id,
            "signal.delivered",
            serde_json::json!({"signal": signal, "payload": payload}),
        )
        .await?;
        Ok(Some(payload))
    }

    async fn migrate_session(&self, session_id: &str, config_hash: &str) -> Result<()> {
        sqlx::query("UPDATE sessions SET config_hash = $1 WHERE session_id = $2")
            .bind(config_hash)
            .bind(session_id)
            .execute(&self.pool)
            .await?;
        self.append_event(
            session_id,
            "session.migrated",
            serde_json::json!({"config_hash": config_hash}),
        )
        .await?;
        Ok(())
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
