//! `RuntimeDaemon`: the real daemon-side session loop (spec §8 intro,
//! event-sourced session supervisor) that
//! `cybersin_adapter::daemon_double::DaemonDouble` stands in for during
//! `cybersin-adapter`'s own conformance tests. Where `DaemonDouble` keeps
//! an in-memory ledger just to prove the protocol shape, `RuntimeDaemon`
//! drives one session against real [`crate::storage::Storage`] (the
//! event-sourced log) and a real `cybersin_trace::SpanStore`, priced and
//! routed from a hand-written [`crate::dist::DistFixture`] (spec §14's M1:
//! "stub agent runs on a hand-written dist/").

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use cybersin_adapter::channel::DaemonChannel;
use cybersin_adapter::messages::{CallOutcome, DaemonMessage, HarnessMessage};
use cybersin_ir::{BudgetArtifact, BudgetPlan, PromptIr, Section};
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

/// Reserved key inside `HarnessMessage::LlmRequest::inputs` carrying live,
/// call-time-only context (spec §8.3a: "retrieved documents, memory,
/// conversation") — a JSON array shaped exactly like [`Section`] (`{id,
/// priority, body}`).
///
/// `inputs: Value` is today's only source of call-time data on the wire
/// (`cybersin_adapter::messages::HarnessMessage::LlmRequest`); rather than
/// growing the protocol with a new field, live sections travel inside the
/// existing `inputs` object under this key. A harness that never sends one
/// (every `inputs` shape before this issue) folds in zero live sections
/// and behaves exactly as the stand-in assembler did.
const LIVE_CONTEXT_KEY: &str = "__live_context";

/// Pull the live, call-time-only sections out of an `llm.request`'s
/// `inputs` (see [`LIVE_CONTEXT_KEY`]). Absent or malformed entries just
/// mean no live sections — this never fails the call.
fn extract_live_sections(inputs: &Value) -> Vec<Section> {
    inputs
        .get(LIVE_CONTEXT_KEY)
        .and_then(|v| serde_json::from_value::<Vec<Section>>(v.clone()).ok())
        .unwrap_or_default()
}

/// Where one section folded into context assembly came from: straight
/// from the compiled `PromptIr::sections`, or a live section folded in via
/// [`extract_live_sections`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SectionSource {
    Compiled,
    Live,
}

#[derive(Debug, Clone)]
struct SectionEntry {
    priority: u32,
    tokens: u32,
    source: SectionSource,
}

/// Result of one `assemble_context` run: enough detail for the caller to
/// price the call and record what got dropped as span attributes (spec
/// §8.3a).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AssembledContext {
    /// Final token count after eviction (or the full total, if nothing
    /// needed to evict).
    pub prompt_tokens: u32,
    /// Every dropped section's id, in the order it was dropped: live
    /// sections first (ascending priority — see this function's doc
    /// comment for why), then compiled sections per the compiled plan's
    /// own `eviction_order`. This is what lands in `Span::evicted_sections`
    /// verbatim.
    pub evicted_sections: Vec<String>,
    /// The subset of `evicted_sections` that were live sections rather
    /// than compiled ones. `Span::evicted_sections` itself doesn't
    /// distinguish source, so this is carried separately for the
    /// `llm.call` span's `attributes` — compiled-vs-live is inspectable
    /// in `cybersin trace show` without a schema change to `Span`.
    pub evicted_live_sections: Vec<String>,
    /// Every section that made it into the final context, in fill order:
    /// priority descending, ties broken by insertion order (compiled
    /// sections in `PromptIr` order, then live sections in `inputs` order)
    /// — spec §8.3a's "fills sections in priority order".
    pub included_sections: Vec<String>,
}

/// Pick the `BudgetPlan` matching `target`, falling back to a plan named
/// `"generic"` and then to the first plan if `target` itself has no exact
/// match — the same fallback the stand-in assembler used unconditionally,
/// now only reached once exact-target matching has had a chance.
fn resolve_plan<'a>(budget: Option<&'a BudgetArtifact>, target: &str) -> Option<&'a BudgetPlan> {
    budget.and_then(|b| {
        b.plans
            .iter()
            .find(|p| p.target == target)
            .or_else(|| b.plans.iter().find(|p| p.target == "generic"))
            .or_else(|| b.plans.first())
    })
}

/// Assemble the final context for one `llm.request` (spec §8.3a): fold the
/// compiled prompt's sections and `live_sections` (retrieved
/// documents/memory/conversation, folded in from `inputs` — see
/// [`extract_live_sections`]) into one priority-ordered fill list, then —
/// only if the assembled size exceeds `target`'s available budget in
/// `budget` — evict down to fit.
///
/// Compile time authors the eviction *policy* for sections it knew about
/// (`BudgetPlan::eviction_order`); it has no opinion about live sections,
/// since those only exist at call time. So eviction runs in two phases:
///
/// 1. **Live sections first**, lowest priority first. They're extra
///    weight the compiled plan never priced in, so all of it is shed
///    before the compiled policy is touched at all.
/// 2. **Compiled sections**, per `eviction_order`, unchanged from the
///    stand-in assembler's logic (already correct per this issue's scope).
///
/// A compiled section not named in `eviction_order` is never evicted,
/// budget or no budget — same as before. With no matching budget plan at
/// all, nothing evicts (no plan means no policy to execute).
fn assemble_context(
    prompt: &PromptIr,
    live_sections: &[Section],
    budget: Option<&BudgetArtifact>,
    target: &str,
) -> AssembledContext {
    // Compiled sections first, live sections after: on an id collision,
    // the live section wins (last insertion wins) — a harness can
    // deliberately shadow a compiled placeholder section with live
    // content by reusing its id.
    let mut fill_order: Vec<String> = Vec::new();
    let mut entries: HashMap<String, SectionEntry> = HashMap::new();
    for s in &prompt.sections {
        if !entries.contains_key(&s.id) {
            fill_order.push(s.id.clone());
        }
        entries.insert(
            s.id.clone(),
            SectionEntry {
                priority: s.priority,
                tokens: estimate_tokens(&s.body),
                source: SectionSource::Compiled,
            },
        );
    }
    for s in live_sections {
        if !entries.contains_key(&s.id) {
            fill_order.push(s.id.clone());
        }
        entries.insert(
            s.id.clone(),
            SectionEntry {
                priority: s.priority,
                tokens: estimate_tokens(&s.body),
                source: SectionSource::Live,
            },
        );
    }

    // Priority-order fill: stable sort keeps the compiled-then-live
    // insertion order as the tiebreak at equal priority.
    fill_order.sort_by_key(|id| std::cmp::Reverse(entries[id].priority));
    let total: u32 = entries.values().map(|e| e.tokens).sum();

    let Some(plan) = resolve_plan(budget, target) else {
        return AssembledContext {
            prompt_tokens: total,
            evicted_sections: Vec::new(),
            evicted_live_sections: Vec::new(),
            included_sections: fill_order,
        };
    };
    let available = plan
        .context_window_tokens
        .saturating_sub(plan.reserved_output_tokens);
    if total <= available {
        return AssembledContext {
            prompt_tokens: total,
            evicted_sections: Vec::new(),
            evicted_live_sections: Vec::new(),
            included_sections: fill_order,
        };
    }

    let mut remaining = total;
    let mut evicted_sections = Vec::new();
    let mut evicted_live_sections = Vec::new();
    let mut evicted: HashSet<String> = HashSet::new();

    // Phase 1: live sections, lowest priority first.
    let mut live_ids: Vec<&String> = fill_order
        .iter()
        .filter(|id| entries[*id].source == SectionSource::Live)
        .collect();
    live_ids.sort_by_key(|id| entries[*id].priority);
    for id in live_ids {
        if remaining <= available {
            break;
        }
        remaining = remaining.saturating_sub(entries[id].tokens);
        evicted_sections.push(id.clone());
        evicted_live_sections.push(id.clone());
        evicted.insert(id.clone());
    }

    // Phase 2: compiled sections, per the compiled plan's own eviction
    // order — identical to the stand-in assembler's logic.
    for step in &plan.eviction_order {
        if remaining <= available {
            break;
        }
        if evicted.contains(&step.section_id) {
            continue;
        }
        if let Some(entry) = entries.get(&step.section_id) {
            remaining = remaining.saturating_sub(entry.tokens);
            evicted_sections.push(step.section_id.clone());
            evicted.insert(step.section_id.clone());
        }
    }

    let included_sections: Vec<String> = fill_order
        .into_iter()
        .filter(|id| !evicted.contains(id))
        .collect();

    AssembledContext {
        prompt_tokens: remaining,
        evicted_sections,
        evicted_live_sections,
        included_sections,
    }
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
            .create_session_pinned(
                &self.session_id,
                &self.agent_name,
                &self.dist.manifest.build_hash,
            )
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
            HarnessMessage::StateGet {
                call_id,
                namespace,
                key,
            } => {
                let value = self
                    .storage
                    .get_state(&self.session_id, &namespace, &key)
                    .await?
                    .map(|r| r.value)
                    .unwrap_or(Value::Null);
                self.channel
                    .send(DaemonMessage::CallResult {
                        call_id,
                        outcome: CallOutcome::Ok { value },
                    })
                    .await?;
                Ok(0)
            }
            HarnessMessage::StateSet {
                call_id,
                namespace,
                key,
                value,
            } => {
                self.storage
                    .set_state(&self.session_id, &namespace, &key, &value)
                    .await?;
                self.channel
                    .send(DaemonMessage::CallResult {
                        call_id,
                        outcome: CallOutcome::Ok { value },
                    })
                    .await?;
                Ok(0)
            }
            HarnessMessage::Checkpoint { call_id, label } => {
                let checkpoint = self
                    .storage
                    .create_checkpoint(&self.session_id, label.as_deref())
                    .await?;
                self.channel.send(DaemonMessage::CallResult { call_id,
                    outcome: CallOutcome::Ok { value: serde_json::json!({
                        "checkpoint_id": checkpoint.checkpoint_id, "event_seq": checkpoint.event_seq
                    }) } }).await?;
                Ok(0)
            }
            HarnessMessage::Sleep {
                call_id,
                duration_ms,
            } => {
                self.storage
                    .append_event(
                        &self.session_id,
                        "sleep.requested",
                        serde_json::json!({"duration_ms": duration_ms}),
                    )
                    .await?;
                tokio::time::sleep(std::time::Duration::from_millis(duration_ms)).await;
                self.storage
                    .append_event(
                        &self.session_id,
                        "sleep.completed",
                        serde_json::json!({"duration_ms": duration_ms}),
                    )
                    .await?;
                self.channel
                    .send(DaemonMessage::CallResult {
                        call_id,
                        outcome: CallOutcome::Ok { value: Value::Null },
                    })
                    .await?;
                Ok(0)
            }
            HarnessMessage::SignalWait { signal, .. } => {
                self.storage
                    .set_session_status(&self.session_id, "waiting")
                    .await?;
                let payload = loop {
                    if let Some(payload) =
                        self.storage.take_signal(&self.session_id, &signal).await?
                    {
                        break payload;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                };
                self.storage
                    .set_session_status(&self.session_id, "running")
                    .await?;
                self.channel
                    .send(DaemonMessage::SignalDelivered { signal, payload })
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
        self.storage
            .create_checkpoint(&self.session_id, Some("pre-llm"))
            .await?;
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

        let live_sections = extract_live_sections(&inputs);
        let assembled = assemble_context(&prompt, &live_sections, budget.as_ref(), &routing.target);
        let prompt_tokens = assembled.prompt_tokens;

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
            assembled.evicted_sections.clone(),
            serde_json::json!({
                "inputs": inputs,
                "context": {
                    "target": routing.target,
                    "included_sections": assembled.included_sections,
                    "evicted_live_sections": assembled.evicted_live_sections,
                },
            }),
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
                    "evicted_sections": assembled.evicted_sections,
                    "evicted_live_sections": assembled.evicted_live_sections,
                    "response": response_value,
                }),
            )
            .await?;
        self.storage
            .create_checkpoint(&self.session_id, Some("periodic"))
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
        self.storage
            .create_checkpoint(&self.session_id, Some("pre-tool"))
            .await?;
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
        self.storage
            .create_checkpoint(&self.session_id, Some("periodic"))
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
    use cybersin_ir::EvictionStep;
    use std::collections::BTreeMap;

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
                    dedup_ref: None,
                },
                Section {
                    id: "documents".to_string(),
                    priority: 50,
                    body: "a b c d e f g h i j".to_string(), // 10 tokens
                    dedup_ref: None,
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

    fn generic_budget(context_window_tokens: u32, reserved_output_tokens: u32) -> BudgetArtifact {
        BudgetArtifact {
            prompt_name: "researcher".to_string(),
            plans: vec![BudgetPlan {
                target: "generic".to_string(),
                context_window_tokens,
                reserved_output_tokens,
                eviction_order: vec![EvictionStep {
                    section_id: "documents".to_string(),
                    evict_at_tokens: 6,
                }],
            }],
        }
    }

    #[test]
    fn assemble_context_keeps_everything_under_budget() {
        let prompt = sample_prompt();
        let budget = generic_budget(100, 10);
        let assembled = assemble_context(&prompt, &[], Some(&budget), "generic");
        assert!(assembled.evicted_sections.is_empty());
        assert_eq!(assembled.prompt_tokens, 15); // 5 + 10, nothing evicted
        assert_eq!(
            assembled.included_sections,
            vec!["role".to_string(), "documents".to_string()]
        );
    }

    #[test]
    fn assemble_context_evicts_lowest_priority_first_over_budget() {
        let prompt = sample_prompt();
        let budget = generic_budget(10, 4); // available = 6, total = 15
        let assembled = assemble_context(&prompt, &[], Some(&budget), "generic");
        assert_eq!(assembled.evicted_sections, vec!["documents".to_string()]);
        assert!(assembled.evicted_live_sections.is_empty());
        assert_eq!(assembled.prompt_tokens, 5); // just "role" left
        assert_eq!(assembled.included_sections, vec!["role".to_string()]);
    }

    #[test]
    fn assemble_context_with_no_budget_never_evicts() {
        let prompt = sample_prompt();
        let assembled = assemble_context(&prompt, &[], None, "generic");
        assert!(assembled.evicted_sections.is_empty());
        assert_eq!(assembled.prompt_tokens, 15);
        assert_eq!(
            assembled.included_sections,
            vec!["role".to_string(), "documents".to_string()]
        );
    }

    #[test]
    fn assemble_context_folds_live_sections_into_priority_order_fill() {
        // priority 100(role) > 70(live "memory") > 50(documents): the live
        // section should slot between the two compiled ones in fill order.
        let prompt = sample_prompt();
        let live = vec![Section {
            id: "memory".to_string(),
            priority: 70,
            body: "k l m".to_string(), // 3 tokens
            dedup_ref: None,
        }];
        let budget = generic_budget(100, 10); // available = 90, well under total
        let assembled = assemble_context(&prompt, &live, Some(&budget), "generic");
        assert!(assembled.evicted_sections.is_empty());
        assert_eq!(assembled.prompt_tokens, 18); // 5 + 3 + 10
        assert_eq!(
            assembled.included_sections,
            vec![
                "role".to_string(),
                "memory".to_string(),
                "documents".to_string()
            ]
        );
    }

    #[test]
    fn assemble_context_evicts_live_sections_before_compiled_ones() {
        // total = 5 (role) + 3 (live "memory", priority 10 - lowest) + 10
        // (documents) = 18; available = 6 - 4 = ... use a budget that only
        // needs the live section dropped to fit.
        let prompt = sample_prompt();
        let live = vec![Section {
            id: "memory".to_string(),
            priority: 10,              // lower priority than "documents" (50)
            body: "k l m".to_string(), // 3 tokens
            dedup_ref: None,
        }];
        let budget = BudgetArtifact {
            prompt_name: "researcher".to_string(),
            plans: vec![BudgetPlan {
                target: "generic".to_string(),
                context_window_tokens: 20,
                reserved_output_tokens: 5, // available = 15, total = 18
                eviction_order: vec![EvictionStep {
                    section_id: "documents".to_string(),
                    evict_at_tokens: 6,
                }],
            }],
        };
        let assembled = assemble_context(&prompt, &live, Some(&budget), "generic");
        // Dropping the 3-token live section alone (18 - 3 = 15) is enough
        // to fit, so the compiled "documents" section, despite being
        // lower priority than "role", is never touched.
        assert_eq!(assembled.evicted_sections, vec!["memory".to_string()]);
        assert_eq!(assembled.evicted_live_sections, vec!["memory".to_string()]);
        assert_eq!(assembled.prompt_tokens, 15);
        assert_eq!(
            assembled.included_sections,
            vec!["role".to_string(), "documents".to_string()]
        );
    }

    #[test]
    fn assemble_context_falls_through_to_compiled_eviction_after_live_sections_exhausted() {
        // Dropping the live section alone isn't enough; the compiled plan's
        // own eviction_order still runs afterward.
        let prompt = sample_prompt();
        let live = vec![Section {
            id: "memory".to_string(),
            priority: 10,
            body: "k l m".to_string(), // 3 tokens
            dedup_ref: None,
        }];
        let budget = generic_budget(10, 4); // available = 6, total = 18
        let assembled = assemble_context(&prompt, &live, Some(&budget), "generic");
        assert_eq!(
            assembled.evicted_sections,
            vec!["memory".to_string(), "documents".to_string()]
        );
        assert_eq!(assembled.evicted_live_sections, vec!["memory".to_string()]);
        assert_eq!(assembled.prompt_tokens, 5); // just "role" left
        assert_eq!(assembled.included_sections, vec!["role".to_string()]);
    }

    #[test]
    fn assemble_context_picks_the_plan_matching_the_calls_target() {
        let prompt = sample_prompt();
        let budget = BudgetArtifact {
            prompt_name: "researcher".to_string(),
            plans: vec![
                BudgetPlan {
                    target: "generic".to_string(),
                    context_window_tokens: 100,
                    reserved_output_tokens: 10, // available = 90: nothing evicts
                    eviction_order: vec![EvictionStep {
                        section_id: "documents".to_string(),
                        evict_at_tokens: 6,
                    }],
                },
                BudgetPlan {
                    target: "openai".to_string(),
                    context_window_tokens: 10,
                    reserved_output_tokens: 4, // available = 6: documents evicts
                    eviction_order: vec![EvictionStep {
                        section_id: "documents".to_string(),
                        evict_at_tokens: 6,
                    }],
                },
            ],
        };

        let generic = assemble_context(&prompt, &[], Some(&budget), "generic");
        assert!(generic.evicted_sections.is_empty());

        let openai = assemble_context(&prompt, &[], Some(&budget), "openai");
        assert_eq!(openai.evicted_sections, vec!["documents".to_string()]);

        // An unknown target falls back to the "generic" plan rather than
        // the first plan in the list (which happens to also be "generic"
        // here, but the fallback is by name, not position).
        let unknown = assemble_context(&prompt, &[], Some(&budget), "anthropic");
        assert!(unknown.evicted_sections.is_empty());
    }
}
