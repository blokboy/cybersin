//! SQLite-backed span store (spec §8.5, §13: sqlx, no ORM).
//!
//! Every query here is hand-written SQL run through `sqlx::query`/
//! `query_as` at runtime (not the `query!`/`query_as!` compile-time
//! macros) — so building this crate never needs a live database or an
//! offline query cache, only the `sqlite`/`runtime-tokio` sqlx features.
//! `#[derive(sqlx::FromRow)]` on [`crate::span::SpanRow`] is column
//! mapping, not an ORM: there is no query builder, no lazy loading, no
//! entity graph — every statement below is exactly the SQL it looks like.

use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::{Row, SqlitePool};

use crate::cost::{CostDimension, CostRollupRow};
use crate::span::{Span, SpanKind, SpanRow};

#[derive(Debug, thiserror::Error)]
pub enum TraceError {
    #[error("sqlite error: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("(de)serialization error: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, TraceError>;

/// Filter applied by [`SpanStore::list`]. All fields are conjunctive
/// (AND'd together); `None` means "don't filter on this dimension".
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SpanFilter {
    pub session_id: Option<String>,
    pub agent_name: Option<String>,
    pub kind: Option<SpanKind>,
    pub model: Option<String>,
    /// Only spans starting at or after this unix-ms timestamp (spec §9's
    /// `cybersin optimize --since`). `None` means no lower bound.
    pub since_unix_ms: Option<i64>,
    /// Cap on the number of rows returned, newest-first. `None` returns
    /// everything matching.
    pub limit: Option<u32>,
}

/// A span store backed by a SQLite database via sqlx (spec §8: "Storage
/// behind a `Storage` trait: SQLite (dev) and Postgres (server) via
/// sqlx"). This crate only ever speaks the SQLite dev half of that
/// contract — `cybersin-runtime`'s `Storage` trait is the seam where a
/// future Postgres implementation would plug in for session/event
/// storage; the trace store is queried offline (`cybersin trace`,
/// `cybersin cost`) independent of that trait boundary.
#[derive(Clone)]
pub struct SpanStore {
    pool: SqlitePool,
}

impl SpanStore {
    /// Connect to a SQLite database at `url` (e.g.
    /// `sqlite:///path/to/cybersin.db?mode=rwc` or `sqlite::memory:`) and
    /// ensure the schema exists.
    pub async fn connect(url: &str) -> Result<Self> {
        let pool = SqlitePoolOptions::new().connect(url).await?;
        let store = Self { pool };
        store.migrate().await?;
        Ok(store)
    }

    /// An in-memory store — tests and short-lived CLI invocations that
    /// don't need durability across processes.
    pub async fn in_memory() -> Result<Self> {
        Self::connect("sqlite::memory:").await
    }

    /// Build a store from an already-open pool (e.g. shared with other
    /// tables in the same sqlite file).
    pub async fn from_pool(pool: SqlitePool) -> Result<Self> {
        let store = Self { pool };
        store.migrate().await?;
        Ok(store)
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    async fn migrate(&self) -> Result<()> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS spans (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                agent_name TEXT NOT NULL,
                kind TEXT NOT NULL,
                name TEXT NOT NULL,
                start_unix_ms INTEGER NOT NULL,
                end_unix_ms INTEGER NOT NULL,
                model TEXT,
                tokens_prompt INTEGER,
                tokens_completion INTEGER,
                usd_cost REAL NOT NULL,
                cache_status TEXT NOT NULL,
                retries INTEGER NOT NULL,
                evicted_sections TEXT NOT NULL,
                status TEXT NOT NULL,
                error_message TEXT,
                attributes TEXT NOT NULL
            )
            "#,
        )
        .execute(&self.pool)
        .await?;

        sqlx::query("CREATE INDEX IF NOT EXISTS idx_spans_session ON spans(session_id)")
            .execute(&self.pool)
            .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_spans_agent ON spans(agent_name)")
            .execute(&self.pool)
            .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_spans_model ON spans(model)")
            .execute(&self.pool)
            .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_spans_start ON spans(start_unix_ms)")
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Insert one span. Spans are immutable once recorded (append-only,
    /// matching the event-sourced philosophy of the runtime this store
    /// serves).
    pub async fn insert(&self, span: &Span) -> Result<()> {
        let evicted_sections = serde_json::to_string(&span.evicted_sections)?;
        let attributes = serde_json::to_string(&span.attributes)?;
        sqlx::query(
            r#"
            INSERT INTO spans (
                id, session_id, agent_name, kind, name, start_unix_ms, end_unix_ms,
                model, tokens_prompt, tokens_completion, usd_cost, cache_status,
                retries, evicted_sections, status, error_message, attributes
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(&span.id)
        .bind(&span.session_id)
        .bind(&span.agent_name)
        .bind(span.kind.as_str())
        .bind(&span.name)
        .bind(span.start_unix_ms)
        .bind(span.end_unix_ms)
        .bind(&span.model)
        .bind(span.tokens_prompt.map(|v| v as i64))
        .bind(span.tokens_completion.map(|v| v as i64))
        .bind(span.usd_cost)
        .bind(span.cache_status.as_str())
        .bind(span.retries as i64)
        .bind(evicted_sections)
        .bind(SpanRow::status_tag(&span.status))
        .bind(SpanRow::error_message(&span.status))
        .bind(attributes)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Fetch one span by id.
    pub async fn get(&self, id: &str) -> Result<Option<Span>> {
        let row: Option<SpanRow> = sqlx::query_as(
            r#"
            SELECT id, session_id, agent_name, kind, name, start_unix_ms, end_unix_ms,
                   model, tokens_prompt, tokens_completion, usd_cost, cache_status,
                   retries, evicted_sections, status, error_message, attributes
            FROM spans WHERE id = ?
            "#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(SpanRow::into_span))
    }

    /// List spans matching `filter`, most recent first.
    pub async fn list(&self, filter: &SpanFilter) -> Result<Vec<Span>> {
        let mut sql = String::from(
            r#"
            SELECT id, session_id, agent_name, kind, name, start_unix_ms, end_unix_ms,
                   model, tokens_prompt, tokens_completion, usd_cost, cache_status,
                   retries, evicted_sections, status, error_message, attributes
            FROM spans WHERE 1=1
            "#,
        );
        if filter.session_id.is_some() {
            sql.push_str(" AND session_id = ?");
        }
        if filter.agent_name.is_some() {
            sql.push_str(" AND agent_name = ?");
        }
        if filter.kind.is_some() {
            sql.push_str(" AND kind = ?");
        }
        if filter.model.is_some() {
            sql.push_str(" AND model = ?");
        }
        if filter.since_unix_ms.is_some() {
            sql.push_str(" AND start_unix_ms >= ?");
        }
        sql.push_str(" ORDER BY start_unix_ms DESC");
        if filter.limit.is_some() {
            sql.push_str(" LIMIT ?");
        }

        // `sql` is built above by concatenating only static fragments
        // chosen from this function's own `match`/`if` arms — never from
        // caller-supplied strings — so it is safe to assert past sqlx's
        // "audit dynamic SQL" guard (spec §13: hand-written SQL, no ORM,
        // but still no string-interpolated user input).
        let mut query = sqlx::query_as::<_, SpanRow>(sqlx::AssertSqlSafe(sql));
        if let Some(v) = &filter.session_id {
            query = query.bind(v);
        }
        if let Some(v) = &filter.agent_name {
            query = query.bind(v);
        }
        if let Some(v) = &filter.kind {
            query = query.bind(v.as_str());
        }
        if let Some(v) = &filter.model {
            query = query.bind(v);
        }
        if let Some(v) = filter.since_unix_ms {
            query = query.bind(v);
        }
        if let Some(v) = filter.limit {
            query = query.bind(v as i64);
        }

        let rows = query.fetch_all(&self.pool).await?;
        Ok(rows.into_iter().map(SpanRow::into_span).collect())
    }

    /// Cost rollup grouped by `dimension` (spec §8.5: `cybersin cost --by
    /// session|agent|model|tool|day`). The `tool` dimension only rolls up
    /// `ToolCall` spans (grouping LLM calls under "tool" would conflate
    /// two different cost axes); every other dimension rolls up all spans.
    pub async fn cost_rollup(&self, dimension: CostDimension) -> Result<Vec<CostRollupRow>> {
        let (group_expr, extra_where) = match dimension {
            CostDimension::Session => ("session_id", ""),
            CostDimension::Agent => ("agent_name", ""),
            CostDimension::Model => ("COALESCE(model, 'n/a')", ""),
            CostDimension::Tool => ("name", " AND kind = 'tool_call'"),
            // SQLite has no native date type; start_unix_ms is milliseconds
            // since epoch, so dividing by 86_400_000 and flooring gives a
            // stable day bucket we then render as YYYY-MM-DD in Rust.
            CostDimension::Day => ("CAST(start_unix_ms / 86400000 AS INTEGER)", ""),
        };

        let sql = format!(
            r#"
            SELECT {group_expr} AS bucket,
                   SUM(usd_cost) AS usd_cost,
                   COUNT(*) AS span_count,
                   SUM(COALESCE(tokens_prompt, 0)) AS tokens_prompt,
                   SUM(COALESCE(tokens_completion, 0)) AS tokens_completion
            FROM spans
            WHERE 1=1{extra_where}
            GROUP BY bucket
            ORDER BY usd_cost DESC
            "#
        );

        // Same reasoning as above: `sql` interpolates only fixed fragments
        // selected from `dimension`'s match arms, never external input.
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .fetch_all(&self.pool)
            .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let bucket_key = match dimension {
                CostDimension::Day => {
                    let day_index: i64 = row.try_get("bucket")?;
                    day_bucket_to_iso_date(day_index)
                }
                _ => row.try_get::<String, _>("bucket")?,
            };
            out.push(CostRollupRow {
                key: bucket_key,
                usd_cost: row.try_get("usd_cost")?,
                span_count: row.try_get::<i64, _>("span_count")? as u64,
                tokens_prompt: row.try_get::<i64, _>("tokens_prompt")? as u64,
                tokens_completion: row.try_get::<i64, _>("tokens_completion")? as u64,
            });
        }
        Ok(out)
    }
}

/// Render a day-since-epoch bucket (`start_unix_ms / 86_400_000`, floored)
/// as an ISO `YYYY-MM-DD` date, without pulling in a datetime crate.
fn day_bucket_to_iso_date(day_index: i64) -> String {
    // Civil-from-days algorithm (Howard Hinnant's public-domain
    // `civil_from_days`), proleptic Gregorian calendar. Avoids adding
    // `chrono`/`time` as a dependency for one cosmetic conversion.
    let z = day_index + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::span::{CacheStatus, SpanStatus};

    fn sample_span(id: &str, session_id: &str, agent: &str, model: &str, cost: f64) -> Span {
        Span {
            id: id.to_string(),
            session_id: session_id.to_string(),
            agent_name: agent.to_string(),
            kind: SpanKind::LlmCall,
            name: "researcher".to_string(),
            start_unix_ms: 1_700_000_000_000,
            end_unix_ms: 1_700_000_000_500,
            model: Some(model.to_string()),
            tokens_prompt: Some(120),
            tokens_completion: Some(40),
            usd_cost: cost,
            cache_status: CacheStatus::Miss,
            retries: 0,
            evicted_sections: vec![],
            status: SpanStatus::Ok,
            attributes: serde_json::json!({}),
        }
    }

    #[tokio::test]
    async fn insert_and_get_round_trips() {
        let store = SpanStore::in_memory().await.unwrap();
        let span = sample_span("s1", "sess-1", "agent-a", "gpt-4o", 0.01);
        store.insert(&span).await.unwrap();
        let fetched = store.get("s1").await.unwrap().unwrap();
        assert_eq!(fetched, span);
    }

    #[tokio::test]
    async fn get_missing_returns_none() {
        let store = SpanStore::in_memory().await.unwrap();
        assert!(store.get("nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn list_filters_by_session() {
        let store = SpanStore::in_memory().await.unwrap();
        store
            .insert(&sample_span("s1", "sess-1", "agent-a", "gpt-4o", 0.01))
            .await
            .unwrap();
        store
            .insert(&sample_span("s2", "sess-2", "agent-a", "gpt-4o", 0.02))
            .await
            .unwrap();

        let filtered = store
            .list(&SpanFilter {
                session_id: Some("sess-1".to_string()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, "s1");
    }

    #[tokio::test]
    async fn list_respects_limit_and_ordering() {
        let store = SpanStore::in_memory().await.unwrap();
        let mut span_a = sample_span("s1", "sess-1", "agent-a", "gpt-4o", 0.01);
        span_a.start_unix_ms = 1;
        let mut span_b = sample_span("s2", "sess-1", "agent-a", "gpt-4o", 0.02);
        span_b.start_unix_ms = 2;
        store.insert(&span_a).await.unwrap();
        store.insert(&span_b).await.unwrap();

        let listed = store
            .list(&SpanFilter {
                limit: Some(1),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(listed.len(), 1);
        // Most recent (highest start_unix_ms) first.
        assert_eq!(listed[0].id, "s2");
    }

    #[tokio::test]
    async fn list_filters_by_since_unix_ms() {
        let store = SpanStore::in_memory().await.unwrap();
        let mut old = sample_span("s1", "sess-1", "agent-a", "gpt-4o", 0.01);
        old.start_unix_ms = 100;
        let mut recent = sample_span("s2", "sess-1", "agent-a", "gpt-4o", 0.02);
        recent.start_unix_ms = 200;
        store.insert(&old).await.unwrap();
        store.insert(&recent).await.unwrap();

        let listed = store
            .list(&SpanFilter {
                since_unix_ms: Some(150),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "s2");
    }

    #[tokio::test]
    async fn cost_rollup_by_session_sums_correctly() {
        let store = SpanStore::in_memory().await.unwrap();
        store
            .insert(&sample_span("s1", "sess-1", "agent-a", "gpt-4o", 0.01))
            .await
            .unwrap();
        store
            .insert(&sample_span("s2", "sess-1", "agent-a", "gpt-4o", 0.02))
            .await
            .unwrap();
        store
            .insert(&sample_span("s3", "sess-2", "agent-a", "gpt-4o", 0.05))
            .await
            .unwrap();

        let rollup = store.cost_rollup(CostDimension::Session).await.unwrap();
        let sess1 = rollup.iter().find(|r| r.key == "sess-1").unwrap();
        assert!((sess1.usd_cost - 0.03).abs() < 1e-9);
        assert_eq!(sess1.span_count, 2);
        let sess2 = rollup.iter().find(|r| r.key == "sess-2").unwrap();
        assert!((sess2.usd_cost - 0.05).abs() < 1e-9);
    }

    #[tokio::test]
    async fn cost_rollup_by_model_groups_across_sessions() {
        let store = SpanStore::in_memory().await.unwrap();
        store
            .insert(&sample_span("s1", "sess-1", "agent-a", "gpt-4o", 0.01))
            .await
            .unwrap();
        store
            .insert(&sample_span("s2", "sess-2", "agent-b", "gpt-4o", 0.02))
            .await
            .unwrap();
        store
            .insert(&sample_span("s3", "sess-3", "agent-a", "claude", 0.05))
            .await
            .unwrap();

        let rollup = store.cost_rollup(CostDimension::Model).await.unwrap();
        let gpt = rollup.iter().find(|r| r.key == "gpt-4o").unwrap();
        assert!((gpt.usd_cost - 0.03).abs() < 1e-9);
        assert_eq!(gpt.span_count, 2);
    }

    #[tokio::test]
    async fn cost_rollup_by_tool_only_counts_tool_calls() {
        let store = SpanStore::in_memory().await.unwrap();
        let mut llm = sample_span("s1", "sess-1", "agent-a", "gpt-4o", 1.0);
        llm.kind = SpanKind::LlmCall;
        llm.name = "researcher".to_string();
        store.insert(&llm).await.unwrap();

        let mut tool = sample_span("s2", "sess-1", "agent-a", "gpt-4o", 0.001);
        tool.kind = SpanKind::ToolCall;
        tool.name = "web_search".to_string();
        tool.model = None;
        store.insert(&tool).await.unwrap();

        let rollup = store.cost_rollup(CostDimension::Tool).await.unwrap();
        assert_eq!(rollup.len(), 1);
        assert_eq!(rollup[0].key, "web_search");
        assert!((rollup[0].usd_cost - 0.001).abs() < 1e-9);
    }

    #[tokio::test]
    async fn cost_rollup_by_day_buckets_by_calendar_day() {
        let store = SpanStore::in_memory().await.unwrap();
        // 2023-11-14T22:13:20Z and later the same UTC day.
        let mut a = sample_span("s1", "sess-1", "agent-a", "gpt-4o", 0.01);
        a.start_unix_ms = 1_700_000_000_000;
        let mut b = sample_span("s2", "sess-1", "agent-a", "gpt-4o", 0.02);
        b.start_unix_ms = 1_700_000_000_000 + 3_600_000; // +1h, same day
        store.insert(&a).await.unwrap();
        store.insert(&b).await.unwrap();

        let rollup = store.cost_rollup(CostDimension::Day).await.unwrap();
        assert_eq!(rollup.len(), 1);
        assert_eq!(rollup[0].key, "2023-11-14");
        assert!((rollup[0].usd_cost - 0.03).abs() < 1e-9);
    }
}
