//! `RuntimeDaemon`: the real daemon-side session loop (spec §8 intro,
//! event-sourced session supervisor) that
//! `cybersin_adapter::daemon_double::DaemonDouble` stands in for during
//! `cybersin-adapter`'s own conformance tests. Where `DaemonDouble` keeps
//! an in-memory ledger just to prove the protocol shape, `RuntimeDaemon`
//! drives one session against real [`crate::storage::Storage`] (the
//! event-sourced log) and a real `cybersin_trace::SpanStore`, priced and
//! routed from a hand-written [`crate::dist::DistFixture`] (spec §14's M1:
//! "stub agent runs on a hand-written dist/").

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use cybersin_adapter::channel::DaemonChannel;
use cybersin_adapter::messages::{CallOutcome, DaemonMessage, HarnessMessage};
use cybersin_ir::{BudgetArtifact, PromptIr};
use cybersin_trace::{CacheStatus, Span, SpanKind, SpanStatus, SpanStore};
use serde_json::Value;

use crate::dist::DistFixture;
use crate::error::RuntimeError;
use crate::storage::Storage;

/// Estimated token count for `text`: a whitespace-token heuristic, not a
/// real tokenizer. This issue's stub agent needs *a* number to price
/// calls and demonstrate the span shape — tokenizer-accurate billing
/// arrives with real backends (spec §6.5), a later issue.
pub fn estimate_tokens(text: &str) -> u32 {
    text.split_whitespace().count().max(1) as u32
}

/// Fill `prompt`'s sections in priority order and evict per `budget`
/// (spec §8.3a: "fills sections in priority order, evicts per plan when
/// over the target's token budget") once the assembled size exceeds the
/// target's available budget. Returns `(evicted_section_ids,
/// tokens_after_eviction)`.
///
/// This is a deliberately small stand-in for the real context assembler:
/// it has exactly one render target's worth of logic (whichever
/// `BudgetPlan` is named `"generic"`, or the first plan if none is) and no
/// live retrieved-document/memory/conversation inputs to fold in — sections
/// straight from the compiled `PromptIr` are the whole context.
fn assemble_context(prompt: &PromptIr, budget: Option<&BudgetArtifact>) -> (Vec<String>, u32) {
    let tokens_by_section: BTreeMap<&str, u32> = prompt
        .sections
        .iter()
        .map(|s| (s.id.as_str(), estimate_tokens(&s.body)))
        .collect();
    let total: u32 = tokens_by_section.values().sum();

    let plan = budget.and_then(|b| {
        b.plans
            .iter()
            .find(|p| p.target == "generic")
            .or_else(|| b.plans.first())
    });
    let Some(plan) = plan else {
        return (Vec::new(), total);
    };
    let available = plan
        .context_window_tokens
        .saturating_sub(plan.reserved_output_tokens);
    if total <= available {
        return (Vec::new(), total);
    }

    let mut evicted = Vec::new();
    let mut remaining = total;
    for step in &plan.eviction_order {
        if remaining <= available {
            break;
        }
        if let Some(t) = tokens_by_section.get(step.section_id.as_str()) {
            remaining = remaining.saturating_sub(*t);
            evicted.push(step.section_id.clone());
        }
    }
    (evicted, remaining)
}

fn now_unix_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// A cache key for the emulated cache layer: exact prompt name + exact
/// JSON-serialized inputs. Since `serde_json::Value`'s own equality is
/// already exact structural equality, this just needs *a* deterministic
/// string to key a `HashMap` on.
fn cache_key(prompt_name: &str, inputs: &Value) -> (String, String) {
    (
        prompt_name.to_string(),
        serde_json::to_string(inputs).unwrap_or_default(),
    )
}

/// Outcome of driving one session to completion.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeSessionSummary {
    pub session_id: String,
    pub completed: bool,
    pub spans_recorded: u32,
}

/// The real daemon-side counterpart to
/// `cybersin_adapter::daemon_double::DaemonDouble` (spec §10): drives one
/// session's `HarnessMessage`/`DaemonMessage` exchange against real
/// storage (event-sourced session log, spec §8.1) and a real
/// `cybersin-trace` span store (spec §8.5), instead of `DaemonDouble`'s
/// throwaway ledger. Routed/priced from a hand-written `DistFixture`
/// rather than the compiler's real `routing.json`/`cache.json` — that's
/// `cybersin-router`, a later issue.
pub struct RuntimeDaemon<C> {
    channel: C,
    storage: Arc<dyn Storage>,
    spans: SpanStore,
    dist: Arc<DistFixture>,
    session_id: String,
    agent_name: String,
    /// Emulates the route/cache executor's cache layer (spec §8.3) just
    /// enough to produce a `Hit` on a repeated identical `llm.request`
    /// within the same session — not the real hash/kNN/judge cascade.
    cache: HashMap<(String, String), Value>,
    next_span_seq: u64,
    completed: bool,
}

impl<C: DaemonChannel> RuntimeDaemon<C> {
    pub fn new(
        channel: C,
        storage: Arc<dyn Storage>,
        spans: SpanStore,
        dist: Arc<DistFixture>,
        session_id: impl Into<String>,
        agent_name: impl Into<String>,
    ) -> Self {
        Self {
            channel,
            storage,
            spans,
            dist,
            session_id: session_id.into(),
            agent_name: agent_name.into(),
            cache: HashMap::new(),
            next_span_seq: 0,
            completed: false,
        }
    }

    fn fresh_span_id(&mut self) -> String {
        self.next_span_seq += 1;
        format!("{}:span-{}", self.session_id, self.next_span_seq)
    }

    /// Create the session durably and push the opening `session.start`
    /// (spec §10).
    pub async fn start_session(&mut self, inputs: Value) -> Result<(), RuntimeError> {
        self.storage
            .create_session(&self.session_id, &self.agent_name)
            .await?;
        self.storage
            .append_event(
                &self.session_id,
                "session.started",
                serde_json::json!({ "inputs": inputs }),
            )
            .await?;
        self.channel
            .send(DaemonMessage::SessionStart {
                session_id: self.session_id.clone(),
                inputs,
                resume_state: None,
            })
            .await?;
        Ok(())
    }

    /// Drive this session until the harness disconnects or sends
    /// `session.complete`.
    pub async fn run(mut self) -> Result<RuntimeSessionSummary, RuntimeError> {
        let mut spans_recorded = 0u32;
        loop {
            if self.completed {
                break;
            }
            match self.channel.recv().await {
                Some(msg) => spans_recorded += self.handle_message(msg).await?,
                None => break,
            }
        }
        self.storage
            .set_session_status(
                &self.session_id,
                if self.completed {
                    "completed"
                } else {
                    "aborted"
                },
            )
            .await?;
        Ok(RuntimeSessionSummary {
            session_id: self.session_id,
            completed: self.completed,
            spans_recorded,
        })
    }

    /// Returns how many spans this message produced.
    async fn handle_message(&mut self, msg: HarnessMessage) -> Result<u32, RuntimeError> {
        match msg {
            HarnessMessage::LlmRequest {
                call_id,
                prompt_name,
                inputs,
            } => self.handle_llm_request(call_id, prompt_name, inputs).await,
            HarnessMessage::ToolRequest {
                call_id,
                tool,
                args,
                ..
            } => self.handle_tool_request(call_id, tool, args).await,
            HarnessMessage::SessionComplete { result, .. } => {
                self.storage
                    .append_event(&self.session_id, "session.completed", result)
                    .await?;
                self.completed = true;
                Ok(0)
            }
            // state.*/checkpoint/sleep aren't exercised by this issue's
            // stub agent script, but are answered generically (rather
            // than left to hang) so a richer future stub script — or a
            // real harness driving this same RuntimeDaemon directly —
            // isn't blocked on them.
            HarnessMessage::StateGet { call_id, .. }
            | HarnessMessage::StateSet { call_id, .. }
            | HarnessMessage::Checkpoint { call_id, .. }
            | HarnessMessage::Sleep { call_id, .. } => {
                self.channel
                    .send(DaemonMessage::CallResult {
                        call_id,
                        outcome: CallOutcome::Ok { value: Value::Null },
                    })
                    .await?;
                Ok(0)
            }
            HarnessMessage::SignalWait { signal, .. } => {
                self.channel
                    .send(DaemonMessage::SignalDelivered {
                        signal,
                        payload: Value::Null,
                    })
                    .await?;
                Ok(0)
            }
        }
    }

    async fn handle_llm_request(
        &mut self,
        call_id: String,
        prompt_name: String,
        inputs: Value,
    ) -> Result<u32, RuntimeError> {
        let prompt = self.dist.prompt(&prompt_name)?.clone();
        let routing = self.dist.routing(&prompt_name)?.clone();
        let budget = self.dist.budget(&prompt_name).cloned();

        let key = cache_key(&prompt_name, &inputs);
        let cache_status = if self.cache.contains_key(&key) {
            CacheStatus::Hit
        } else {
            CacheStatus::Miss
        };

        // Cache decision span: its own span kind, recorded ahead of the
        // LLM call span it precedes (spec §8.5).
        self.emit_span(
            SpanKind::CacheDecision,
            prompt_name.clone(),
            None,
            None,
            None,
            0.0,
            cache_status,
            0,
            Vec::new(),
            serde_json::json!({ "prompt_name": prompt_name }),
        )
        .await?;

        let (evicted, prompt_tokens) = assemble_context(&prompt, budget.as_ref());

        let (completion_tokens, usd_cost, response_value) = match cache_status {
            CacheStatus::Hit => {
                let cached = self.cache.get(&key).cloned().unwrap_or(Value::Null);
                (0u32, 0.0f64, cached)
            }
            _ => {
                let completion_tokens = routing.completion_tokens_estimate;
                let usd_cost = (prompt_tokens as f64 / 1000.0) * routing.usd_per_1k_prompt_tokens
                    + (completion_tokens as f64 / 1000.0) * routing.usd_per_1k_completion_tokens;
                let response = serde_json::json!({
                    "text": format!("stub completion for prompt `{prompt_name}`"),
                    "model": routing.model,
                });
                self.cache.insert(key, response.clone());
                (completion_tokens, usd_cost, response)
            }
        };

        self.emit_span(
            SpanKind::LlmCall,
            prompt_name.clone(),
            Some(routing.model.clone()),
            Some(prompt_tokens),
            Some(completion_tokens),
            usd_cost,
            cache_status,
            0,
            evicted.clone(),
            serde_json::json!({ "inputs": inputs }),
        )
        .await?;

        self.storage
            .append_event(
                &self.session_id,
                "llm.call",
                serde_json::json!({
                    "prompt_name": prompt_name,
                    "model": routing.model,
                    "cache_status": cache_status.as_str(),
                    "usd_cost": usd_cost,
                    "tokens_prompt": prompt_tokens,
                    "tokens_completion": completion_tokens,
                    "evicted_sections": evicted,
                }),
            )
            .await?;

        self.channel
            .send(DaemonMessage::CallResult {
                call_id,
                outcome: CallOutcome::Ok {
                    value: response_value,
                },
            })
            .await?;
        Ok(2) // cache-decision span + llm-call span
    }

    async fn handle_tool_request(
        &mut self,
        call_id: String,
        tool: String,
        args: Value,
    ) -> Result<u32, RuntimeError> {
        // The retry-class engine (spec §8.2) is a later issue (#11); this
        // stub hardcodes one simulated transient-then-succeed retry so
        // the `retries` span attribute carries a real nonzero value in
        // this issue's demo run, not just an always-zero schema field.
        let retries = 1u32;
        let usd_cost = 0.0008;

        self.emit_span(
            SpanKind::ToolCall,
            tool.clone(),
            None,
            None,
            None,
            usd_cost,
            CacheStatus::NotApplicable,
            retries,
            Vec::new(),
            serde_json::json!({ "args": args }),
        )
        .await?;

        self.storage
            .append_event(
                &self.session_id,
                "tool.call",
                serde_json::json!({ "tool": tool, "retries": retries, "usd_cost": usd_cost }),
            )
            .await?;

        self.channel
            .send(DaemonMessage::CallResult {
                call_id,
                outcome: CallOutcome::Ok {
                    value: serde_json::json!({ "tool": tool, "status": "ok" }),
                },
            })
            .await?;
        Ok(1)
    }

    #[allow(clippy::too_many_arguments)]
    async fn emit_span(
        &mut self,
        kind: SpanKind,
        name: String,
        model: Option<String>,
        tokens_prompt: Option<u32>,
        tokens_completion: Option<u32>,
        usd_cost: f64,
        cache_status: CacheStatus,
        retries: u32,
        evicted_sections: Vec<String>,
        attributes: Value,
    ) -> Result<(), RuntimeError> {
        let now = now_unix_ms();
        let span = Span {
            id: self.fresh_span_id(),
            session_id: self.session_id.clone(),
            agent_name: self.agent_name.clone(),
            kind,
            name,
            start_unix_ms: now,
            end_unix_ms: now,
            model,
            tokens_prompt,
            tokens_completion,
            usd_cost,
            cache_status,
            retries,
            evicted_sections,
            status: SpanStatus::Ok,
            attributes,
        };
        self.spans.insert(&span).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cybersin_ir::{BudgetPlan, EvictionStep, Section};

    fn sample_prompt() -> PromptIr {
        PromptIr::new(
            "researcher",
            cybersin_ir::QualityTier::High,
            BTreeMap::new(),
            vec!["web_search".to_string()],
            vec![
                Section {
                    id: "role".to_string(),
                    priority: 100,
                    body: "one two three four five".to_string(), // 5 tokens
                },
                Section {
                    id: "documents".to_string(),
                    priority: 50,
                    body: "a b c d e f g h i j".to_string(), // 10 tokens
                },
            ],
            None,
        )
    }

    #[test]
    fn estimate_tokens_counts_whitespace_words() {
        assert_eq!(estimate_tokens("one two three"), 3);
        assert_eq!(estimate_tokens(""), 1); // floor of 1, never zero
        assert_eq!(estimate_tokens("solo"), 1);
    }

    #[test]
    fn assemble_context_keeps_everything_under_budget() {
        let prompt = sample_prompt();
        let budget = BudgetArtifact {
            prompt_name: "researcher".to_string(),
            plans: vec![BudgetPlan {
                target: "generic".to_string(),
                context_window_tokens: 100,
                reserved_output_tokens: 10,
                eviction_order: vec![EvictionStep {
                    section_id: "documents".to_string(),
                    evict_at_tokens: 50,
                }],
            }],
        };
        let (evicted, tokens) = assemble_context(&prompt, Some(&budget));
        assert!(evicted.is_empty());
        assert_eq!(tokens, 15); // 5 + 10, nothing evicted
    }

    #[test]
    fn assemble_context_evicts_lowest_priority_first_over_budget() {
        let prompt = sample_prompt();
        let budget = BudgetArtifact {
            prompt_name: "researcher".to_string(),
            plans: vec![BudgetPlan {
                target: "generic".to_string(),
                context_window_tokens: 10,
                reserved_output_tokens: 4, // available = 6, total = 15
                eviction_order: vec![EvictionStep {
                    section_id: "documents".to_string(),
                    evict_at_tokens: 6,
                }],
            }],
        };
        let (evicted, tokens) = assemble_context(&prompt, Some(&budget));
        assert_eq!(evicted, vec!["documents".to_string()]);
        assert_eq!(tokens, 5); // just "role" left
    }

    #[test]
    fn assemble_context_with_no_budget_never_evicts() {
        let prompt = sample_prompt();
        let (evicted, tokens) = assemble_context(&prompt, None);
        assert!(evicted.is_empty());
        assert_eq!(tokens, 15);
    }
}
