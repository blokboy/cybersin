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
        // 2 cache-decision + 3 llm-call spans (the fixture's `cascade.json`
        // declares a cheaper "gpt-4o-nano" alternate ahead of the default
        // "gpt-4o-mini": the real executor's cascade now genuinely
        // escalates past it before settling on the default, spec §8.3 —
        // two real model-call spans for the miss, not one) + 1 tool-call.
        assert_eq!(summary.spans_recorded, 6);

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
        assert_eq!(recorded.len(), 6);

        let llm_spans: Vec<_> = recorded
            .iter()
            .filter(|s| s.kind == SpanKind::LlmCall)
            .collect();
        assert_eq!(llm_spans.len(), 3);
        for span in &llm_spans {
            assert!(span.tokens_prompt.is_some());
        }
        assert!(llm_spans
            .iter()
            .any(|s| s.model.as_deref() == Some("gpt-4o-nano")));

        let miss_spans: Vec<_> = llm_spans
            .iter()
            .filter(|s| s.cache_status == cybersin_trace::CacheStatus::Miss)
            .collect();
        assert_eq!(miss_spans.len(), 2, "cheap escalation + default accept");
        assert!(miss_spans.iter().all(|s| s.usd_cost > 0.0));
        assert!(miss_spans
            .iter()
            .all(|s| s.evicted_sections == vec!["documents".to_string()]));
        assert!(miss_spans.iter().all(|s| s.tokens_completion == Some(180)));
        let miss_accept = miss_spans
            .iter()
            .find(|s| s.model.as_deref() == Some("gpt-4o-mini"))
            .expect("the default model should be the cascade's accepted step");
        assert_eq!(miss_accept.attributes["decision"], "cascade_accept");

        let hit = llm_spans
            .iter()
            .find(|s| s.cache_status == cybersin_trace::CacheStatus::Hit)
            .expect("one llm span should be a cache hit");
        assert_eq!(hit.usd_cost, 0.0);
        // A cache hit is attributed to the prompt's default model — a
        // cache entry doesn't record which model originally produced it.
        assert_eq!(hit.model.as_deref(), Some("gpt-4o-mini"));

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

    /// End-to-end coverage for issue #16: a real `llm.request` driven
    /// through `RuntimeDaemon`, carrying a live context section via
    /// `inputs.__live_context`, produces an `llm_call` span whose
    /// `evicted_sections`/`attributes` record both the live and compiled
    /// drops the context assembler made (spec §8.3a).
    #[tokio::test]
    async fn stub_session_records_live_and_compiled_evictions_as_span_attributes() {
        let storage: Arc<dyn Storage> = Arc::new(SqliteStorage::in_memory().await.unwrap());
        let spans = SpanStore::in_memory().await.unwrap();
        let dist = Arc::new(DistFixture::load_dir(bundled_stub_dist_dir()).unwrap());

        let (harness_io, daemon_io) = in_memory_pair();
        let session_id = "sess-live-context-1".to_string();
        let mut daemon = RuntimeDaemon::new(
            daemon_io,
            storage.clone(),
            spans.clone(),
            dist,
            session_id.clone(),
            "research-agent",
        );

        // Fixture budget: context_window 40 - reserved 10 = 30 available.
        // Compiled total is 41 (role 10 + instructions 12 + documents 19).
        // Adding a 5-token live "memory" section (lowest priority of all,
        // so it's shed first) brings the total to 46. Dropping the live
        // section alone (46 - 5 = 41) still isn't enough, so the compiled
        // plan's own eviction of "documents" also runs (41 - 19 = 22),
        // landing under the 30-token budget.
        let inputs = json!({
            "topic": "cybernetics",
            "depth": "quick",
            "documents": [],
            "__live_context": [
                { "id": "memory", "priority": 5, "body": "one two three four five" }
            ],
        });

        daemon.start_session(inputs.clone()).await.unwrap();
        let daemon_task = tokio::spawn(daemon.run());

        let mut harness = StubHarness::new(harness_io);
        let (_sid, _inputs, _resume) = harness.recv_session_start().await;
        harness.llm_request("researcher", inputs).await;
        harness
            .session_complete(&session_id, json!({ "status": "ok" }))
            .await;
        harness.wait_for_close().await;

        let summary = daemon_task
            .await
            .map_err(|e| RuntimeError::Join(e.to_string()))
            .unwrap()
            .unwrap();
        assert!(summary.completed);

        let recorded = spans
            .list(&SpanFilter {
                session_id: Some(session_id.clone()),
                kind: Some(SpanKind::LlmCall),
                ..Default::default()
            })
            .await
            .unwrap();
        // The fixture's `cascade.json` cheap alternate ("gpt-4o-nano") is
        // escalated past before settling on the default ("gpt-4o-mini") —
        // two real model-call spans for this one `llm.request`, not one
        // (see `stub_session_produces_real_spans_with_cost_tokens_and_model`).
        // Both carry identical context-assembler bookkeeping (this
        // request's `prompt_tokens`/`evicted_sections`/`context`), so
        // either is equally valid for the assertions below.
        assert_eq!(recorded.len(), 2);
        let span = &recorded[0];

        assert_eq!(
            span.evicted_sections,
            vec!["memory".to_string(), "documents".to_string()]
        );
        assert_eq!(span.tokens_prompt, Some(22));

        let context_attrs = &span.attributes["context"];
        assert_eq!(context_attrs["target"], "generic");
        assert_eq!(
            context_attrs["included_sections"],
            json!(["role", "instructions"])
        );
        assert_eq!(context_attrs["evicted_live_sections"], json!(["memory"]));
    }
}
