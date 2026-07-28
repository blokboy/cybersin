//! Session storage (spec §8: "Storage behind a `Storage` trait: SQLite
//! (dev) and Postgres (server) via sqlx, no ORM") and the event-sourced
//! session loop's durable log (spec §8.1: "append-only `events`").
//!
//! Both SQLite and Postgres implement this trait boundary. Every caller
//! in this crate (the [`crate::session::RuntimeDaemon`] session loop, the
//! CLI's trace/session views) depends on `dyn Storage`, never on a
//! concrete backend.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::sqlite::{SqlitePoolOptions, SqliteRow};
use sqlx::{Row, SqlitePool};

use crate::dist::DistArtifactBundle;

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("io error at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("sqlite error: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("(de)serialization error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("state {namespace}.{key} has type {expected}, cannot set {actual}")]
    StateType {
        namespace: String,
        key: String,
        expected: String,
        actual: String,
    },
    #[error("artifact bundle for config_hash {0:?} is not stored; run a build/run that ingests it first")]
    ArtifactBundleMissing(String),
    #[error("artifact bundle for config_hash {config_hash:?} has no stored files")]
    ArtifactBundleEmpty { config_hash: String },
    #[error("stored artifact {path} for config_hash {config_hash:?} hash mismatch: stored {expected}, actual {actual}")]
    ArtifactHashMismatch {
        config_hash: String,
        path: String,
        expected: String,
        actual: String,
    },
    #[error("stored artifact path is not a safe relative file path: {0}")]
    UnsafeArtifactPath(String),
    #[error("materialize target is not empty: {0}")]
    MaterializeTargetNotEmpty(String),
}

pub(crate) fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn row_to_state(row: SqliteRow) -> Result<StateRecord> {
    Ok(StateRecord {
        namespace: row.get("namespace"),
        key: row.get("key"),
        value_type: row.get("value_type"),
        value: serde_json::from_str(&row.get::<String, _>("value"))?,
        updated_seq: row.get("updated_seq"),
    })
}

pub type Result<T> = std::result::Result<T, StorageError>;

pub async fn materialize_artifact_bundle(
    storage: &dyn Storage,
    config_hash: &str,
    target_dir: &std::path::Path,
) -> Result<usize> {
    let files = storage.load_artifact_bundle(config_hash).await?;
    if files.is_empty() {
        return Err(StorageError::ArtifactBundleEmpty {
            config_hash: config_hash.to_string(),
        });
    }
    if target_dir.exists() {
        let mut entries = std::fs::read_dir(target_dir).map_err(|source| StorageError::Io {
            path: target_dir.display().to_string(),
            source,
        })?;
        if entries.next().is_some() {
            return Err(StorageError::MaterializeTargetNotEmpty(
                target_dir.display().to_string(),
            ));
        }
    }
    let mut verified = Vec::with_capacity(files.len());
    for file in &files {
        let actual = hex_sha256(&file.bytes);
        if actual != file.sha256 {
            return Err(StorageError::ArtifactHashMismatch {
                config_hash: config_hash.to_string(),
                path: file.path.clone(),
                expected: file.sha256.clone(),
                actual,
            });
        }
        verified.push((safe_artifact_path(&file.path)?.to_path_buf(), file));
    }

    std::fs::create_dir_all(target_dir).map_err(|source| StorageError::Io {
        path: target_dir.display().to_string(),
        source,
    })?;
    for (relative, file) in verified {
        let destination = target_dir.join(relative);
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent).map_err(|source| StorageError::Io {
                path: parent.display().to_string(),
                source,
            })?;
        }
        std::fs::write(&destination, &file.bytes).map_err(|source| StorageError::Io {
            path: destination.display().to_string(),
            source,
        })?;
    }
    Ok(files.len())
}

fn hex_sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn safe_artifact_path(relative: &str) -> Result<&std::path::Path> {
    let path = std::path::Path::new(relative);
    if path.is_absolute()
        || relative.is_empty()
        || path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(StorageError::UnsafeArtifactPath(relative.to_string()));
    }
    Ok(path)
}

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
    pub config_hash: String,
    pub created_unix_ms: i64,
    pub last_heartbeat_unix_ms: Option<i64>,
    pub heartbeat_holder: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeartbeatLiveness {
    Fresh,
    Stale,
    None,
}

/// Fixed default resume/listing freshness threshold for harness leases.
/// Kept as data rather than sleep timing so tests can compare against a
/// controlled `now_unix_ms`.
pub const DEFAULT_HEARTBEAT_STALE_AFTER_MS: i64 = 30_000;

pub fn heartbeat_liveness_at(
    session: &SessionRecord,
    now_unix_ms: i64,
    stale_after_ms: i64,
) -> HeartbeatLiveness {
    match session.last_heartbeat_unix_ms {
        Some(last) if now_unix_ms.saturating_sub(last) <= stale_after_ms => {
            HeartbeatLiveness::Fresh
        }
        Some(_) => HeartbeatLiveness::Stale,
        None => HeartbeatLiveness::None,
    }
}

pub fn is_terminal_session_status(status: &str) -> bool {
    matches!(
        status,
        "completed" | "aborted" | "failed" | "halted" | "killed"
    )
}

pub fn default_heartbeat_holder() -> String {
    let pid = std::process::id();
    let host = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "unknown".to_string());
    format!("pid={pid} host={host}")
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateRecord {
    pub namespace: String,
    pub key: String,
    pub value_type: String,
    pub value: Value,
    pub updated_seq: i64,
}

/// Result of [`Storage::cas_state`] — spec §8.7's blackboard
/// "optimistic CAS — versioned writes where a stale write fails and the
/// caller retries." The DB is the referee: `Applied` / `Stale` reflects
/// exactly one atomic `UPDATE`/`INSERT ... ON CONFLICT` statement's
/// affected-row count, the same "constraint wins races" pattern
/// `begin_tool_call` uses — never a separate read-then-write check in
/// application code.
#[derive(Debug, Clone, PartialEq)]
pub enum CasOutcome {
    Applied(StateRecord),
    /// The write was rejected because `expected_version` didn't match the
    /// row's actual version at the moment the constrained statement ran.
    /// `actual` is a best-effort follow-up read for the caller's retry —
    /// it is not part of the atomic decision itself.
    Stale {
        actual: Option<i64>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckpointRecord {
    pub checkpoint_id: i64,
    pub session_id: String,
    pub event_seq: i64,
    pub label: Option<String>,
    pub state: Value,
    pub created_unix_ms: i64,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactFileRecord {
    pub config_hash: String,
    pub path: String,
    pub sha256: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArtifactIngestOutcome {
    Stored,
    Reused,
}

/// Storage trait boundary (spec §8), implemented by SQLite for local
/// development and Postgres for server mode.
#[async_trait]
pub trait Storage: Send + Sync {
    async fn create_session(&self, session_id: &str, agent_name: &str) -> Result<()>;
    async fn create_session_pinned(
        &self,
        session_id: &str,
        agent_name: &str,
        config_hash: &str,
    ) -> Result<()>;
    async fn set_session_status(&self, session_id: &str, status: &str) -> Result<()>;
    async fn write_session_heartbeat(
        &self,
        session_id: &str,
        unix_ms: i64,
        holder: &str,
    ) -> Result<()>;
    async fn get_session(&self, session_id: &str) -> Result<Option<SessionRecord>>;
    async fn list_sessions(&self) -> Result<Vec<SessionRecord>>;
    /// Append one event to a session's append-only log; returns the
    /// assigned sequence number.
    async fn append_event(&self, session_id: &str, kind: &str, payload: Value) -> Result<i64>;
    async fn load_events(&self, session_id: &str) -> Result<Vec<EventRecord>>;
    async fn set_state(
        &self,
        session_id: &str,
        namespace: &str,
        key: &str,
        value: &Value,
    ) -> Result<()>;
    async fn get_state(
        &self,
        session_id: &str,
        namespace: &str,
        key: &str,
    ) -> Result<Option<StateRecord>>;
    async fn list_state(&self, session_id: &str) -> Result<Vec<StateRecord>>;
    /// Compare-and-set a state row: the version check and the write are one
    /// atomic statement (`UPDATE ... WHERE updated_seq = expected` when a
    /// row is expected to exist, `INSERT ... ON CONFLICT DO NOTHING` when
    /// `expected_version` is `None`), so two concurrent callers racing the
    /// same key are refereed by the database's affected-row count — never
    /// by a `get_state` read followed by a separate `set_state` write.
    /// Implementations must not rely on any in-process lock for
    /// correctness; a single-connection pool is an optimization detail, not
    /// the safety mechanism.
    async fn cas_state(
        &self,
        session_id: &str,
        namespace: &str,
        key: &str,
        expected_version: Option<i64>,
        value: &Value,
    ) -> Result<CasOutcome>;
    async fn create_checkpoint(
        &self,
        session_id: &str,
        label: Option<&str>,
    ) -> Result<CheckpointRecord>;
    async fn latest_checkpoint(&self, session_id: &str) -> Result<Option<CheckpointRecord>>;
    async fn enqueue_signal(&self, session_id: &str, signal: &str, payload: &Value) -> Result<()>;
    async fn take_signal(&self, session_id: &str, signal: &str) -> Result<Option<Value>>;
    async fn migrate_session(&self, session_id: &str, config_hash: &str) -> Result<()>;
    async fn ingest_artifact_bundle(
        &self,
        bundle: &DistArtifactBundle,
    ) -> Result<ArtifactIngestOutcome>;
    async fn has_artifact_bundle(&self, config_hash: &str) -> Result<bool>;
    async fn load_artifact_bundle(&self, config_hash: &str) -> Result<Vec<ArtifactFileRecord>>;

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

    /// Every call currently parked behind an approval gate, across all
    /// sessions, most recently updated first — the query behind `cybersin
    /// ops`'s Approvals tab (issue #52). The pending-side analogue of
    /// `list_dead_letters`: `WHERE status='pending' AND
    /// awaiting_approval=1` instead of `WHERE status='failed' AND
    /// dropped=0`.
    async fn list_awaiting_approval(&self) -> Result<Vec<ToolCallRecord>>;

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
                created_unix_ms INTEGER NOT NULL,
                config_hash TEXT NOT NULL DEFAULT ''
            )
            "#,
        )
        .execute(&self.pool)
        .await?;
        let columns = sqlx::query("PRAGMA table_info(sessions)")
            .fetch_all(&self.pool)
            .await?;
        if !columns
            .iter()
            .any(|row| row.get::<String, _>("name") == "config_hash")
        {
            sqlx::query("ALTER TABLE sessions ADD COLUMN config_hash TEXT NOT NULL DEFAULT ''")
                .execute(&self.pool)
                .await?;
        }
        if !columns
            .iter()
            .any(|row| row.get::<String, _>("name") == "last_heartbeat_unix_ms")
        {
            sqlx::query("ALTER TABLE sessions ADD COLUMN last_heartbeat_unix_ms INTEGER")
                .execute(&self.pool)
                .await?;
        }
        if !columns
            .iter()
            .any(|row| row.get::<String, _>("name") == "heartbeat_holder")
        {
            sqlx::query("ALTER TABLE sessions ADD COLUMN heartbeat_holder TEXT")
                .execute(&self.pool)
                .await?;
        }

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

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS session_state (
                session_id TEXT NOT NULL, namespace TEXT NOT NULL, key TEXT NOT NULL,
                value_type TEXT NOT NULL, value TEXT NOT NULL, updated_seq INTEGER NOT NULL,
                PRIMARY KEY (session_id, namespace, key)
            )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS checkpoints (
                checkpoint_id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL,
                event_seq INTEGER NOT NULL, label TEXT, state TEXT NOT NULL,
                created_unix_ms INTEGER NOT NULL
            )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS signals (
                signal_id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL,
                signal TEXT NOT NULL, payload TEXT NOT NULL, delivered INTEGER NOT NULL DEFAULT 0
            )",
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
            CREATE TABLE IF NOT EXISTS artifact_files (
                config_hash TEXT NOT NULL,
                path TEXT NOT NULL,
                sha256 TEXT NOT NULL,
                bytes BLOB NOT NULL,
                created_unix_ms INTEGER NOT NULL,
                PRIMARY KEY (config_hash, path)
            )
            "#,
        )
        .execute(&self.pool)
        .await?;

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
        self.create_session_pinned(session_id, agent_name, "").await
    }

    async fn create_session_pinned(
        &self,
        session_id: &str,
        agent_name: &str,
        config_hash: &str,
    ) -> Result<()> {
        let now = now_unix_ms();
        sqlx::query(
            "INSERT OR IGNORE INTO sessions \
             (session_id, agent_name, status, created_unix_ms, config_hash) \
             VALUES (?, ?, 'running', ?, ?)",
        )
        .bind(session_id)
        .bind(agent_name)
        .bind(now)
        .bind(config_hash)
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

    async fn write_session_heartbeat(
        &self,
        session_id: &str,
        unix_ms: i64,
        holder: &str,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE sessions SET last_heartbeat_unix_ms = ?, heartbeat_holder = ? \
             WHERE session_id = ?",
        )
        .bind(unix_ms)
        .bind(holder)
        .bind(session_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get_session(&self, session_id: &str) -> Result<Option<SessionRecord>> {
        let row = sqlx::query(
            "SELECT session_id, agent_name, status, created_unix_ms, config_hash, \
             last_heartbeat_unix_ms, heartbeat_holder FROM sessions WHERE session_id = ?",
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
            last_heartbeat_unix_ms: r.get("last_heartbeat_unix_ms"),
            heartbeat_holder: r.get("heartbeat_holder"),
        }))
    }

    async fn list_sessions(&self) -> Result<Vec<SessionRecord>> {
        let rows = sqlx::query(
            "SELECT session_id, agent_name, status, created_unix_ms, config_hash, \
             last_heartbeat_unix_ms, heartbeat_holder FROM sessions \
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
                config_hash: r.get("config_hash"),
                created_unix_ms: r.get("created_unix_ms"),
                last_heartbeat_unix_ms: r.get("last_heartbeat_unix_ms"),
                heartbeat_holder: r.get("heartbeat_holder"),
            })
            .collect())
    }

    async fn append_event(&self, session_id: &str, kind: &str, payload: Value) -> Result<i64> {
        let payload_str = serde_json::to_string(&payload)?;
        let now = now_unix_ms();
        // Sequence allocation and insertion must be one SQLite statement.
        // Even a single-connection pool can interleave two async tasks
        // between a standalone SELECT and INSERT, giving both the same
        // `MAX(seq) + 1` under concurrent mailbox/blackboard activity.
        let next_seq: i64 = sqlx::query_scalar(
            "INSERT INTO events (session_id, seq, unix_ms, kind, payload) \
             SELECT ?, COALESCE(MAX(seq), 0) + 1, ?, ?, ? \
             FROM events WHERE session_id = ? \
             RETURNING seq",
        )
        .bind(session_id)
        .bind(now)
        .bind(kind)
        .bind(payload_str)
        .bind(session_id)
        .fetch_one(&self.pool)
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
                    "namespace": namespace, "key": key, "value_type": value_type, "value": value
                }),
            )
            .await?;
        sqlx::query(
            "INSERT INTO session_state (session_id, namespace, key, value_type, value, updated_seq)
            VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT(session_id, namespace, key) DO UPDATE SET
            value = excluded.value, updated_seq = excluded.updated_seq",
        )
        .bind(session_id)
        .bind(namespace)
        .bind(key)
        .bind(value_type)
        .bind(serde_json::to_string(value)?)
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
        let row = sqlx::query(
            "SELECT namespace, key, value_type, value, updated_seq FROM session_state
            WHERE session_id = ? AND namespace = ? AND key = ?",
        )
        .bind(session_id)
        .bind(namespace)
        .bind(key)
        .fetch_optional(&self.pool)
        .await?;
        row.map(row_to_state).transpose()
    }

    async fn list_state(&self, session_id: &str) -> Result<Vec<StateRecord>> {
        sqlx::query(
            "SELECT namespace, key, value_type, value, updated_seq FROM session_state
            WHERE session_id = ? ORDER BY namespace, key",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(row_to_state)
        .collect()
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
        let value_str = serde_json::to_string(value)?;
        let applied = match expected_version {
            None => {
                sqlx::query(
                    "INSERT INTO session_state
                     (session_id, namespace, key, value_type, value, updated_seq)
                     VALUES (?, ?, ?, ?, ?, 1)
                     ON CONFLICT(session_id, namespace, key) DO NOTHING",
                )
                .bind(session_id)
                .bind(namespace)
                .bind(key)
                .bind(value_type)
                .bind(&value_str)
                .execute(&self.pool)
                .await?
                .rows_affected()
                    == 1
            }
            Some(expected) => {
                sqlx::query(
                    "UPDATE session_state
                     SET value = ?, value_type = ?, updated_seq = updated_seq + 1
                     WHERE session_id = ? AND namespace = ? AND key = ? AND updated_seq = ?",
                )
                .bind(&value_str)
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
                serde_json::json!({
                    "label": label, "state": state
                }),
            )
            .await?;
        let now = now_unix_ms();
        let result = sqlx::query(
            "INSERT INTO checkpoints
            (session_id, event_seq, label, state, created_unix_ms) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(session_id)
        .bind(event_seq)
        .bind(label)
        .bind(serde_json::to_string(&state)?)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(CheckpointRecord {
            checkpoint_id: result.last_insert_rowid(),
            session_id: session_id.into(),
            event_seq,
            label: label.map(str::to_owned),
            state,
            created_unix_ms: now,
        })
    }

    async fn latest_checkpoint(&self, session_id: &str) -> Result<Option<CheckpointRecord>> {
        let row = sqlx::query("SELECT checkpoint_id, session_id, event_seq, label, state,
            created_unix_ms FROM checkpoints WHERE session_id = ? ORDER BY checkpoint_id DESC LIMIT 1")
            .bind(session_id).fetch_optional(&self.pool).await?;
        row.map(|r| {
            Ok(CheckpointRecord {
                checkpoint_id: r.get("checkpoint_id"),
                session_id: r.get("session_id"),
                event_seq: r.get("event_seq"),
                label: r.get("label"),
                state: serde_json::from_str(&r.get::<String, _>("state"))?,
                created_unix_ms: r.get("created_unix_ms"),
            })
        })
        .transpose()
    }

    async fn enqueue_signal(&self, session_id: &str, signal: &str, payload: &Value) -> Result<()> {
        sqlx::query("INSERT INTO signals (session_id, signal, payload) VALUES (?, ?, ?)")
            .bind(session_id)
            .bind(signal)
            .bind(serde_json::to_string(payload)?)
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
        let row = sqlx::query(
            "SELECT signal_id, payload FROM signals WHERE session_id = ?
            AND signal = ? AND delivered = 0 ORDER BY signal_id LIMIT 1",
        )
        .bind(session_id)
        .bind(signal)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else { return Ok(None) };
        let id: i64 = row.get("signal_id");
        let value: Value = serde_json::from_str(&row.get::<String, _>("payload"))?;
        sqlx::query("UPDATE signals SET delivered = 1 WHERE signal_id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        self.append_event(
            session_id,
            "signal.delivered",
            serde_json::json!({"signal": signal, "payload": value}),
        )
        .await?;
        Ok(Some(value))
    }

    async fn migrate_session(&self, session_id: &str, config_hash: &str) -> Result<()> {
        if !self.has_artifact_bundle(config_hash).await? {
            return Err(StorageError::ArtifactBundleMissing(config_hash.to_string()));
        }
        sqlx::query("UPDATE sessions SET config_hash = ? WHERE session_id = ?")
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

    async fn ingest_artifact_bundle(
        &self,
        bundle: &DistArtifactBundle,
    ) -> Result<ArtifactIngestOutcome> {
        if self.has_artifact_bundle(&bundle.config_hash).await? {
            return Ok(ArtifactIngestOutcome::Reused);
        }
        let mut tx = self.pool.begin().await?;
        let now = now_unix_ms();
        let mut inserted = 0u64;
        for file in &bundle.files {
            inserted += sqlx::query(
                "INSERT INTO artifact_files (config_hash, path, sha256, bytes, created_unix_ms)
                 VALUES (?, ?, ?, ?, ?)
                 ON CONFLICT(config_hash, path) DO NOTHING",
            )
            .bind(&bundle.config_hash)
            .bind(&file.path)
            .bind(&file.sha256)
            .bind(&file.bytes)
            .bind(now)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        }
        tx.commit().await?;
        Ok(if inserted == 0 {
            ArtifactIngestOutcome::Reused
        } else {
            ArtifactIngestOutcome::Stored
        })
    }

    async fn has_artifact_bundle(&self, config_hash: &str) -> Result<bool> {
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM artifact_files WHERE config_hash = ?")
                .bind(config_hash)
                .fetch_one(&self.pool)
                .await?;
        Ok(count > 0)
    }

    async fn load_artifact_bundle(&self, config_hash: &str) -> Result<Vec<ArtifactFileRecord>> {
        let rows = sqlx::query(
            "SELECT config_hash, path, sha256, bytes FROM artifact_files
             WHERE config_hash = ? ORDER BY path",
        )
        .bind(config_hash)
        .fetch_all(&self.pool)
        .await?;
        if rows.is_empty() {
            return Err(StorageError::ArtifactBundleMissing(config_hash.to_string()));
        }
        Ok(rows
            .into_iter()
            .map(|row| ArtifactFileRecord {
                config_hash: row.get("config_hash"),
                path: row.get("path"),
                sha256: row.get("sha256"),
                bytes: row.get("bytes"),
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

    async fn list_awaiting_approval(&self) -> Result<Vec<ToolCallRecord>> {
        let rows = sqlx::query(
            "SELECT * FROM tool_calls WHERE status = 'pending' AND awaiting_approval = 1 \
             ORDER BY updated_unix_ms DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(Self::row_to_tool_call).collect()
    }
}

pub(crate) fn now_unix_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dist::{DistArtifactBundle, DistArtifactFile};
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
        assert_eq!(record.last_heartbeat_unix_ms, None);
        assert_eq!(record.heartbeat_holder, None);
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
    async fn heartbeat_write_read_round_trips_and_reports_liveness() {
        let storage = SqliteStorage::in_memory().await.unwrap();
        storage
            .create_session("sess-1", "research-agent")
            .await
            .unwrap();
        let session = storage.get_session("sess-1").await.unwrap().unwrap();
        assert_eq!(
            heartbeat_liveness_at(&session, 10_000, DEFAULT_HEARTBEAT_STALE_AFTER_MS),
            HeartbeatLiveness::None
        );

        storage
            .write_session_heartbeat("sess-1", 10_000, "pid=123 host=test")
            .await
            .unwrap();
        let session = storage.get_session("sess-1").await.unwrap().unwrap();
        assert_eq!(session.last_heartbeat_unix_ms, Some(10_000));
        assert_eq!(
            session.heartbeat_holder.as_deref(),
            Some("pid=123 host=test")
        );
        assert_eq!(
            heartbeat_liveness_at(&session, 39_999, DEFAULT_HEARTBEAT_STALE_AFTER_MS),
            HeartbeatLiveness::Fresh
        );
        assert_eq!(
            heartbeat_liveness_at(&session, 40_001, DEFAULT_HEARTBEAT_STALE_AFTER_MS),
            HeartbeatLiveness::Stale
        );
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
    async fn concurrent_event_appends_allocate_distinct_sequences() {
        let storage = Arc::new(SqliteStorage::in_memory().await.unwrap());
        storage.create_session("root", "supervisor").await.unwrap();

        let (left, right) = tokio::join!(
            storage.append_event("root", "mailbox.left", serde_json::json!(1)),
            storage.append_event("root", "mailbox.right", serde_json::json!(2)),
        );
        let mut sequences = vec![left.unwrap(), right.unwrap()];
        sequences.sort_unstable();
        assert_eq!(sequences, vec![1, 2]);
        assert_eq!(storage.load_events("root").await.unwrap().len(), 2);
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
    async fn artifact_bundle_ingest_is_idempotent_and_materializes_bytes() {
        let storage = SqliteStorage::in_memory().await.unwrap();
        let bytes = br#"{"build_hash":"hash-1"}"#.to_vec();
        let bundle = DistArtifactBundle {
            config_hash: "hash-1".to_string(),
            files: vec![DistArtifactFile {
                path: "manifest.json".to_string(),
                sha256: hex_sha256(&bytes),
                bytes: bytes.clone(),
            }],
        };
        assert_eq!(
            storage.ingest_artifact_bundle(&bundle).await.unwrap(),
            ArtifactIngestOutcome::Stored
        );
        assert_eq!(
            storage.ingest_artifact_bundle(&bundle).await.unwrap(),
            ArtifactIngestOutcome::Reused
        );
        assert!(storage.has_artifact_bundle("hash-1").await.unwrap());

        let out = tempfile::tempdir().unwrap();
        let count = materialize_artifact_bundle(&storage, "hash-1", out.path())
            .await
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(
            std::fs::read(out.path().join("manifest.json")).unwrap(),
            bytes
        );
    }

    #[tokio::test]
    async fn materialize_reverifies_stored_hashes_before_writing_tree() {
        let storage = SqliteStorage::in_memory().await.unwrap();
        let good = b"good".to_vec();
        let poisoned = b"original".to_vec();
        storage
            .ingest_artifact_bundle(&DistArtifactBundle {
                config_hash: "hash-1".to_string(),
                files: vec![
                    DistArtifactFile {
                        path: "a.txt".to_string(),
                        sha256: hex_sha256(&good),
                        bytes: good,
                    },
                    DistArtifactFile {
                        path: "nested/poisoned.bin".to_string(),
                        sha256: hex_sha256(&poisoned),
                        bytes: poisoned,
                    },
                ],
            })
            .await
            .unwrap();
        sqlx::query("UPDATE artifact_files SET bytes = ? WHERE config_hash = ? AND path = ?")
            .bind(b"tampered".to_vec())
            .bind("hash-1")
            .bind("nested/poisoned.bin")
            .execute(&storage.pool)
            .await
            .unwrap();

        let out = tempfile::tempdir().unwrap();
        let err = materialize_artifact_bundle(&storage, "hash-1", out.path())
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            StorageError::ArtifactHashMismatch { path, .. } if path == "nested/poisoned.bin"
        ));
        assert!(!out.path().join("a.txt").exists());
    }

    #[tokio::test]
    async fn migrate_session_requires_stored_target_hash() {
        let storage = SqliteStorage::in_memory().await.unwrap();
        storage
            .create_session_pinned("s", "a", "hash-1")
            .await
            .unwrap();
        assert!(matches!(
            storage.migrate_session("s", "hash-2").await,
            Err(StorageError::ArtifactBundleMissing(hash)) if hash == "hash-2"
        ));
        let bytes = b"stored".to_vec();
        storage
            .ingest_artifact_bundle(&DistArtifactBundle {
                config_hash: "hash-2".to_string(),
                files: vec![DistArtifactFile {
                    path: "manifest.json".to_string(),
                    sha256: hex_sha256(&bytes),
                    bytes,
                }],
            })
            .await
            .unwrap();
        storage.migrate_session("s", "hash-2").await.unwrap();
        assert_eq!(
            storage.get_session("s").await.unwrap().unwrap().config_hash,
            "hash-2"
        );
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

    #[tokio::test]
    async fn list_awaiting_approval_returns_only_pending_and_flagged_rows() {
        let storage = SqliteStorage::in_memory().await.unwrap();
        // Parked: pending + awaiting_approval — must show up.
        storage
            .begin_tool_call("t:k1", "sess-1", "t", "k1", "write", &serde_json::json!({}))
            .await
            .unwrap();
        storage
            .set_tool_call_awaiting_approval("t:k1", "t:k1")
            .await
            .unwrap();

        // Plain pending, never gated — must not show up.
        storage
            .begin_tool_call("t:k2", "sess-1", "t", "k2", "write", &serde_json::json!({}))
            .await
            .unwrap();

        // Was parked, but already resolved — must not show up even though
        // nothing ever clears the flag on a terminal row.
        storage
            .begin_tool_call("t:k3", "sess-1", "t", "k3", "write", &serde_json::json!({}))
            .await
            .unwrap();
        storage
            .set_tool_call_awaiting_approval("t:k3", "t:k3")
            .await
            .unwrap();
        storage
            .resolve_tool_call_succeeded("t:k3", serde_json::json!({"ok": true}))
            .await
            .unwrap();

        let rows = storage.list_awaiting_approval().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].call_id, "t:k1");
    }

    #[tokio::test]
    async fn typed_state_rejects_type_changes_and_checkpoint_snapshots_it() {
        let storage = SqliteStorage::in_memory().await.unwrap();
        storage
            .create_session_pinned("s", "a", "hash-1")
            .await
            .unwrap();
        storage
            .set_state("s", "memory", "count", &serde_json::json!(1))
            .await
            .unwrap();
        storage
            .set_state("s", "memory", "count", &serde_json::json!(2))
            .await
            .unwrap();
        assert!(matches!(
            storage
                .set_state("s", "memory", "count", &serde_json::json!("two"))
                .await,
            Err(StorageError::StateType { .. })
        ));
        let checkpoint = storage
            .create_checkpoint("s", Some("manual"))
            .await
            .unwrap();
        assert_eq!(checkpoint.state[0]["value"], 2);
        assert_eq!(
            storage.latest_checkpoint("s").await.unwrap(),
            Some(checkpoint)
        );
    }

    #[tokio::test]
    async fn signals_are_durable_and_delivered_once() {
        let storage = SqliteStorage::in_memory().await.unwrap();
        storage.create_session("s", "a").await.unwrap();
        storage
            .enqueue_signal("s", "notify", &serde_json::json!({"go": true}))
            .await
            .unwrap();
        assert_eq!(
            storage.take_signal("s", "notify").await.unwrap(),
            Some(serde_json::json!({"go": true}))
        );
        assert_eq!(storage.take_signal("s", "notify").await.unwrap(), None);
    }
}
