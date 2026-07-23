use std::sync::Arc;

use cybersin_runtime::stub_agent::run_stub_session;
use cybersin_runtime::{bundled_stub_dist_dir, DistFixture, PgStorage, SqliteStorage, Storage};
use cybersin_trace::SpanStore;
use serde_json::json;

async fn exercise(storage: &dyn Storage, suffix: &str) -> (Vec<String>, String, i64, bool) {
    let session = format!("parity-session-{suffix}");
    let call = format!("parity-tool:{suffix}");
    storage
        .create_session(&session, "parity-agent")
        .await
        .unwrap();
    storage
        .append_event(&session, "session.started", json!({"input": 1}))
        .await
        .unwrap();
    storage
        .append_event(&session, "session.completed", json!({"output": 2}))
        .await
        .unwrap();
    storage
        .set_session_status(&session, "completed")
        .await
        .unwrap();
    let (_, won) = storage
        .begin_tool_call(
            &call,
            &session,
            "parity-tool",
            suffix,
            "write",
            &json!({"value": 3}),
        )
        .await
        .unwrap();
    storage.increment_tool_call_attempt(&call).await.unwrap();
    storage
        .resolve_tool_call_succeeded(&call, json!({"ok": true}))
        .await
        .unwrap();

    let events = storage.load_events(&session).await.unwrap();
    let status = storage.get_session(&session).await.unwrap().unwrap().status;
    let tool = storage.get_tool_call(&call).await.unwrap().unwrap();
    (
        events.into_iter().map(|event| event.kind).collect(),
        status,
        tool.attempts,
        won && tool.status == "succeeded" && tool.result == Some(json!({"ok": true})),
    )
}

/// Requires an isolated Postgres database supplied by TEST_POSTGRES_URL.
#[tokio::test]
#[ignore = "requires TEST_POSTGRES_URL"]
async fn postgres_session_event_and_ledger_semantics_match_sqlite() {
    let url = std::env::var("TEST_POSTGRES_URL").expect("TEST_POSTGRES_URL");
    let postgres = PgStorage::connect(&url, 8).await.unwrap();
    let sqlite = SqliteStorage::in_memory().await.unwrap();
    let suffix = format!("{}", std::process::id());

    assert_eq!(
        exercise(&sqlite, &suffix).await,
        exercise(&postgres, &suffix).await
    );
}

#[tokio::test]
#[ignore = "requires TEST_POSTGRES_URL"]
async fn complete_session_run_has_equivalent_postgres_and_sqlite_semantics() {
    let url = std::env::var("TEST_POSTGRES_URL").expect("TEST_POSTGRES_URL");
    let postgres: Arc<dyn Storage> = Arc::new(PgStorage::connect(&url, 8).await.unwrap());
    let sqlite: Arc<dyn Storage> = Arc::new(SqliteStorage::in_memory().await.unwrap());
    let dist = Arc::new(DistFixture::load_dir(bundled_stub_dist_dir()).unwrap());
    let suffix = std::process::id();
    let sqlite_id = format!("sqlite-run-{suffix}");
    let postgres_id = format!("postgres-run-{suffix}");

    let sqlite_summary = run_stub_session(
        sqlite.clone(),
        SpanStore::in_memory().await.unwrap(),
        dist.clone(),
        &sqlite_id,
        "research-agent",
    )
    .await
    .unwrap();
    let postgres_summary = run_stub_session(
        postgres.clone(),
        SpanStore::in_memory().await.unwrap(),
        dist,
        &postgres_id,
        "research-agent",
    )
    .await
    .unwrap();

    assert_eq!(sqlite_summary.completed, postgres_summary.completed);
    assert_eq!(
        sqlite_summary.spans_recorded,
        postgres_summary.spans_recorded
    );
    let sqlite_events = sqlite.load_events(&sqlite_id).await.unwrap();
    let postgres_events = postgres.load_events(&postgres_id).await.unwrap();
    assert_eq!(
        sqlite_events
            .iter()
            .map(|event| (&event.kind, &event.payload))
            .collect::<Vec<_>>(),
        postgres_events
            .iter()
            .map(|event| (&event.kind, &event.payload))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        sqlite
            .get_session(&sqlite_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        postgres
            .get_session(&postgres_id)
            .await
            .unwrap()
            .unwrap()
            .status
    );
}

#[tokio::test]
#[ignore = "requires TEST_POSTGRES_URL"]
async fn postgres_constraint_arbitrates_concurrent_tool_call_race() {
    let url = std::env::var("TEST_POSTGRES_URL").expect("TEST_POSTGRES_URL");
    let storage = std::sync::Arc::new(PgStorage::connect(&url, 16).await.unwrap());
    let suffix = format!("race-{}", std::process::id());
    let mut tasks = Vec::new();
    for n in 0..16 {
        let storage = storage.clone();
        let suffix = suffix.clone();
        tasks.push(tokio::spawn(async move {
            storage
                .begin_tool_call(
                    &format!("race-call-{n}-{suffix}"),
                    "race-session",
                    "race-tool",
                    &suffix,
                    "write",
                    &json!({}),
                )
                .await
                .unwrap()
                .1
        }));
    }
    let mut wins = 0;
    for task in tasks {
        wins += usize::from(task.await.unwrap());
    }
    assert_eq!(wins, 1);
}
