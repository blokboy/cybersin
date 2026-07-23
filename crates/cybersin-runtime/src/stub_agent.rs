//! The M1 stub agent (spec §14: "stub agent runs on a hand-written
//! dist/").
//!
//! The "harness" here is `cybersin_adapter::stub_harness::StubHarness`
//! scripted directly — no LLM, no real adapter subprocess — driving
//! [`crate::session::RuntimeDaemon`] (this crate's real daemon-side
//! session loop) over an in-memory stdio channel pair. This is exactly
//! the pattern `cybersin-adapter`'s own conformance tests use to drive
//! `DaemonDouble` (`tests/conformance.rs`), except the far end is a real
//! `Storage` + `SpanStore` instead of a throwaway test double — this is
//! what issue #10's description means by "the real daemon-side
//! counterpart that `daemon_double.rs` currently stubs out".

use std::sync::Arc;

use cybersin_adapter::stub_harness::StubHarness;
use cybersin_adapter::transport::stdio::in_memory_pair;
use cybersin_trace::SpanStore;
use serde_json::json;

use crate::dist::DistFixture;
use crate::error::RuntimeError;
use crate::session::{RuntimeDaemon, RuntimeSessionSummary};
use crate::storage::Storage;

/// Drive one end-to-end stub session against the `researcher` prompt in
/// `dist`: a cache-miss `llm.request`, a `tool.request`, and a second
/// identical `llm.request` that hits the emulated cache — enough to
/// record real spans covering every attribute spec §8.5 names (tokens,
/// `usd_cost`, model, cache status, retries, evicted sections).
pub async fn run_stub_session(
    storage: Arc<dyn Storage>,
    spans: SpanStore,
    dist: Arc<DistFixture>,
    session_id: impl Into<String>,
    agent_name: impl Into<String>,
) -> Result<RuntimeSessionSummary, RuntimeError> {
    let session_id = session_id.into();
    let agent_name = agent_name.into();
    let (harness_io, daemon_io) = in_memory_pair();

    let mut daemon = RuntimeDaemon::new(
        daemon_io,
        storage,
        spans,
        dist,
        session_id.clone(),
        agent_name,
    );
    let inputs = json!({
        "topic": "cybernetics",
        "depth": "quick",
        "documents": [],
    });
    daemon.start_session(inputs.clone()).await?;
    let daemon_task = tokio::spawn(daemon.run());

    let mut harness = StubHarness::new(harness_io);
    let (_sid, _inputs, _resume) = harness.recv_session_start().await;

    // 1. Cache miss: nobody's asked for this prompt+inputs yet.
    harness.llm_request("researcher", inputs.clone()).await;
    // 2. A tool call, exercised for its own span kind + retries attribute.
    harness
        .tool_request("web_search", json!({ "query": "cybernetics" }), None)
        .await;
    // 3. Same prompt + same inputs as (1): the emulated cache should
    //    report this as a `Hit`.
    harness.llm_request("researcher", inputs).await;

    harness
        .session_complete(&session_id, json!({ "status": "ok" }))
        .await;
    harness.wait_for_close().await;

    daemon_task
        .await
        .map_err(|e| RuntimeError::Join(e.to_string()))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dist::{bundled_stub_dist_dir, DistFixture};
    use crate::storage::SqliteStorage;
    use cybersin_trace::{CostDimension, SpanFilter, SpanKind, SpanStore};

    #[tokio::test]
    async fn stub_session_produces_real_spans_with_cost_tokens_and_model() {
        let storage: Arc<dyn Storage> = Arc::new(SqliteStorage::in_memory().await.unwrap());
        let spans = SpanStore::in_memory().await.unwrap();
        let dist = Arc::new(DistFixture::load_dir(bundled_stub_dist_dir()).unwrap());

        let summary = run_stub_session(
            storage.clone(),
            spans.clone(),
            dist,
            "sess-stub-1",
            "research-agent",
        )
        .await
        .unwrap();

        assert!(summary.completed);
        assert_eq!(summary.session_id, "sess-stub-1");
        // 2 cache-decision + 2 llm-call spans + 1 tool-call span.
        assert_eq!(summary.spans_recorded, 5);

        let session = storage.get_session("sess-stub-1").await.unwrap().unwrap();
        assert_eq!(session.status, "completed");
        assert_eq!(session.agent_name, "research-agent");

        let recorded = spans
            .list(&SpanFilter {
                session_id: Some("sess-stub-1".to_string()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(recorded.len(), 5);

        let llm_spans: Vec<_> = recorded
            .iter()
            .filter(|s| s.kind == SpanKind::LlmCall)
            .collect();
        assert_eq!(llm_spans.len(), 2);
        for span in &llm_spans {
            assert_eq!(span.model.as_deref(), Some("gpt-4o-mini"));
            assert!(span.tokens_prompt.is_some());
        }

        let miss = llm_spans
            .iter()
            .find(|s| s.cache_status == cybersin_trace::CacheStatus::Miss)
            .expect("one llm span should be a cache miss");
        assert!(miss.usd_cost > 0.0);
        assert_eq!(miss.evicted_sections, vec!["documents".to_string()]);
        assert_eq!(miss.tokens_completion, Some(180));

        let hit = llm_spans
            .iter()
            .find(|s| s.cache_status == cybersin_trace::CacheStatus::Hit)
            .expect("one llm span should be a cache hit");
        assert_eq!(hit.usd_cost, 0.0);

        let tool_spans: Vec<_> = recorded
            .iter()
            .filter(|s| s.kind == SpanKind::ToolCall)
            .collect();
        assert_eq!(tool_spans.len(), 1);
        assert_eq!(tool_spans[0].retries, 1);
        assert_eq!(tool_spans[0].name, "web_search");

        let cache_decisions: Vec<_> = recorded
            .iter()
            .filter(|s| s.kind == SpanKind::CacheDecision)
            .collect();
        assert_eq!(cache_decisions.len(), 2);

        // Cost rollups see this run's real data.
        let by_session = spans.cost_rollup(CostDimension::Session).await.unwrap();
        let row = by_session
            .iter()
            .find(|r| r.key == "sess-stub-1")
            .expect("session rollup row");
        assert!(row.usd_cost > 0.0);

        let by_model = spans.cost_rollup(CostDimension::Model).await.unwrap();
        assert!(by_model.iter().any(|r| r.key == "gpt-4o-mini"));

        let by_tool = spans.cost_rollup(CostDimension::Tool).await.unwrap();
        assert!(by_tool.iter().any(|r| r.key == "web_search"));

        let by_day = spans.cost_rollup(CostDimension::Day).await.unwrap();
        assert_eq!(by_day.len(), 1);
    }
}
