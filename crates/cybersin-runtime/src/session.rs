//! `RuntimeDaemon`: the real daemon-side session loop (spec §8 intro,
//! event-sourced session supervisor) that
//! `cybersin_adapter::daemon_double::DaemonDouble` stands in for during
//! `cybersin-adapter`'s own conformance tests. Where `DaemonDouble` keeps
//! an in-memory ledger just to prove the protocol shape, `RuntimeDaemon`
//! drives one session against real [`crate::storage::Storage`] (the
//! event-sourced log) and a real `cybersin_trace::SpanStore`, priced and
//! routed from a hand-written [`crate::dist::DistFixture`] (spec §14's M1:
//! "stub agent runs on a hand-written dist/").

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use cybersin_adapter::channel::DaemonChannel;
use cybersin_adapter::messages::{AbortReason, CallOutcome, DaemonMessage, HarnessMessage};
use cybersin_ir::{BudgetArtifact, PromptIr};
use cybersin_trace::{CacheStatus, Span, SpanFilter, SpanKind, SpanStatus, SpanStore};
use serde_json::Value;

use crate::budget::{BudgetConfig, OnBreach};
use crate::dist::DistFixture;
use crate::error::RuntimeError;
use crate::storage::{Storage, ToolCallRecord};

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
    /// Set when a `usd_per_session` budget breach's `on_breach: halt`
    /// terminated the session (spec §8.5) — distinct from `completed:
    /// false` meaning an ordinary abort (channel closed early, harness
    /// crash, ...).
    pub halted: bool,
    pub spans_recorded: u32,
}

/// What [`RuntimeDaemon::enforce_session_budget`] decided (spec §8.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BudgetOutcome {
    /// No breach, or a `degrade` breach that already re-routed the
    /// prompt — the caller proceeds with the call.
    Proceed,
    /// `on_breach: halt` fired; the session is over, a `session.abort`
    /// was already sent, no `call.result` follows for this call.
    Halted,
    /// `on_breach: ask` fired and was denied; a `call.result` failure
    /// was already sent for this call.
    Denied,
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
    /// Session-level `usd_per_session` budget (spec §8.5). `None` means
    /// "no budget declared" — this issue's enforcement is then a no-op,
    /// matching every session's behavior before this issue existed.
    budget: Option<BudgetConfig>,
    /// Prompts a `degrade` breach has re-routed to their cheapest
    /// cascade step (spec §8.5) — the minimal "which cascade step is
    /// active" state this issue adds, scoped per prompt rather than a
    /// single flag, since different prompts may breach independently.
    degraded_prompts: HashSet<String>,
    /// Set once `on_breach: halt` fires; distinct from `completed` so
    /// [`RuntimeDaemon::run`] can report a clean, inspectable terminal
    /// status (`"halted"`) instead of the generic `"aborted"`.
    halted: bool,
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
            budget: None,
            degraded_prompts: HashSet::new(),
            halted: false,
        }
    }

    /// Declare this session's `usd_per_session` budget (spec §8.5,
    /// `agents/*.agent.yaml`'s `budget:` block — see
    /// [`crate::budget::BudgetConfig::from_agent_yaml`]). A builder
    /// method rather than a `RuntimeDaemon::new` parameter so every
    /// existing caller (the M1 stub-agent scenario, this crate's own
    /// tests) that doesn't care about budgets is unaffected.
    pub fn with_budget(mut self, budget: BudgetConfig) -> Self {
        self.budget = Some(budget);
        self
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
            if self.completed || self.halted {
                break;
            }
            match self.channel.recv().await {
                Some(msg) => spans_recorded += self.handle_message(msg).await?,
                None => break,
            }
        }
        // A halt already set `status = "halted"` durably (spec §8.5)
        // where the breach was discovered — don't overwrite that
        // inspectable terminal status with the generic "aborted" here.
        if !self.halted {
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
        }
        Ok(RuntimeSessionSummary {
            session_id: self.session_id,
            completed: self.completed,
            halted: self.halted,
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

    /// This session's running spend so far (spec §8.5): the sum of every
    /// already-recorded span's `usd_cost`. Read from `self.spans`
    /// (durable storage) rather than an in-memory running total, so a
    /// budget check after a simulated process restart sees exactly what
    /// a fresh process reconstructed from the same `Storage` would.
    async fn session_spend_usd(&self) -> Result<f64, RuntimeError> {
        let spans = self
            .spans
            .list(&SpanFilter {
                session_id: Some(self.session_id.clone()),
                ..Default::default()
            })
            .await?;
        Ok(spans.iter().map(|s| s.usd_cost).sum())
    }

    /// Park this session behind an approval gate (spec §8.2 / §8.5): admit
    /// `(tool, idem_key)` into the exact same `tool_calls` ledger
    /// `cybersin_gateway::ToolGateway` uses — same call-id format
    /// (`"{tool}:{idem_key}"`), same `awaiting_approval` flag, same
    /// `sessions.status` transition — tell the harness the call is parked
    /// (`call.parked`), then block/poll for a resolution: the same "set
    /// status, block/poll, resume, restore status" shape
    /// `HarnessMessage::SignalWait` above already uses.
    ///
    /// Durable by construction: every bit of state this loop reads back
    /// (`tool_calls.status`/`awaiting_approval`, `sessions.status`) lives
    /// in [`Storage`], not this task — so a `cybersin approve|deny
    /// <call-id>` issued by a completely different process, hours or days
    /// later, resolves it identically whether this task is still running
    /// or not. This poll loop costs nothing but idle wakeups while
    /// parked, never another billed LLM/tool call — that's what makes the
    /// wait itself free.
    async fn park_for_approval(
        &mut self,
        tool: &str,
        idem_key: &str,
        harness_call_id: &str,
        args: Value,
        retry_class: &str,
    ) -> Result<ToolCallRecord, RuntimeError> {
        let call_id = format!("{tool}:{idem_key}");
        let (row, _won) = self
            .storage
            .begin_tool_call(
                &call_id,
                &self.session_id,
                tool,
                idem_key,
                retry_class,
                &args,
            )
            .await?;
        // Already resolved (e.g. a replay of a call_id admitted earlier
        // in this same session) — nothing to park.
        if row.status != "pending" {
            return Ok(row);
        }

        self.storage
            .set_tool_call_awaiting_approval(&call_id, &call_id)
            .await?;
        self.storage
            .set_session_status(&self.session_id, "awaiting_approval")
            .await?;
        self.storage
            .append_event(
                &self.session_id,
                "session.parked",
                serde_json::json!({ "call_id": call_id, "tool": tool }),
            )
            .await?;
        self.channel
            .send(DaemonMessage::CallParked {
                call_id: harness_call_id.to_string(),
                approval_id: call_id.clone(),
            })
            .await?;

        let resolved = loop {
            let row = self
                .storage
                .get_tool_call(&call_id)
                .await?
                .expect("ledger row exists: this method just admitted it");
            if row.status != "pending" || !row.awaiting_approval {
                break row;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        };

        self.storage
            .set_session_status(&self.session_id, "running")
            .await?;
        self.storage
            .append_event(
                &self.session_id,
                "session.resumed",
                serde_json::json!({ "call_id": call_id, "status": resolved.status }),
            )
            .await?;
        Ok(resolved)
    }

    /// Budget enforcement (spec §8.5), checked before every `llm.request`
    /// executes: compares this session's running spend against
    /// `self.budget.usd_per_session` and applies `on_breach`. A no-op
    /// (`BudgetOutcome::Proceed`) whenever no budget is declared, or
    /// spend hasn't reached it yet.
    async fn enforce_session_budget(
        &mut self,
        call_id: &str,
        prompt_name: &str,
    ) -> Result<BudgetOutcome, RuntimeError> {
        let Some(budget) = self.budget else {
            return Ok(BudgetOutcome::Proceed);
        };
        let spent = self.session_spend_usd().await?;
        if spent < budget.usd_per_session {
            return Ok(BudgetOutcome::Proceed);
        }

        match budget.on_breach {
            OnBreach::Degrade => {
                self.degraded_prompts.insert(prompt_name.to_string());
                self.storage
                    .append_event(
                        &self.session_id,
                        "budget.degraded",
                        serde_json::json!({
                            "prompt_name": prompt_name,
                            "usd_spent": spent,
                            "usd_budget": budget.usd_per_session,
                        }),
                    )
                    .await?;
                Ok(BudgetOutcome::Proceed)
            }
            OnBreach::Halt => {
                self.halted = true;
                self.storage
                    .append_event(
                        &self.session_id,
                        "budget.halted",
                        serde_json::json!({
                            "usd_spent": spent,
                            "usd_budget": budget.usd_per_session,
                        }),
                    )
                    .await?;
                self.storage
                    .set_session_status(&self.session_id, "halted")
                    .await?;
                self.channel
                    .send(DaemonMessage::SessionAbort {
                        session_id: self.session_id.clone(),
                        reason: AbortReason::BudgetHalt {
                            usd_spent: spent,
                            usd_budget: budget.usd_per_session,
                        },
                    })
                    .await?;
                Ok(BudgetOutcome::Halted)
            }
            OnBreach::Ask => {
                // The call_id doubles as the idem_key: one budget-ask
                // decision per `llm.request`, same reasoning
                // `ToolGateway::call` uses for its own approval_id.
                let row = self
                    .park_for_approval(
                        "budget",
                        call_id,
                        call_id,
                        serde_json::json!({
                            "prompt_name": prompt_name,
                            "usd_spent": spent,
                            "usd_budget": budget.usd_per_session,
                        }),
                        "critical",
                    )
                    .await?;
                if row.status == "succeeded" {
                    Ok(BudgetOutcome::Proceed)
                } else {
                    self.channel
                        .send(DaemonMessage::CallResult {
                            call_id: call_id.to_string(),
                            outcome: CallOutcome::Failed {
                                reason: row.failure_reason.unwrap_or_else(|| "denied".to_string()),
                                retriable: row.retriable.unwrap_or(false),
                            },
                        })
                        .await?;
                    Ok(BudgetOutcome::Denied)
                }
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

        match self.enforce_session_budget(&call_id, &prompt_name).await? {
            BudgetOutcome::Proceed => {}
            // `Halted` already sent `session.abort`; `Denied` already
            // sent a `call.result` failure. Either way there's nothing
            // left for this message to do.
            BudgetOutcome::Halted | BudgetOutcome::Denied => return Ok(0),
        }

        let prompt = self.dist.prompt(&prompt_name)?.clone();
        // `degrade` re-routes to the cheapest declared cascade step (spec
        // §8.5); a prompt with no `cascade.json` entry has nothing
        // cheaper to fall back to, so it just keeps its normal routing.
        let routing = if self.degraded_prompts.contains(&prompt_name) {
            self.dist
                .cascade(&prompt_name)
                .first()
                .cloned()
                .unwrap_or(self.dist.routing(&prompt_name)?.clone())
        } else {
            self.dist.routing(&prompt_name)?.clone()
        };
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

        // A critical-class call with `approval: required` parks the
        // session (spec §8.2) instead of running immediately. Tools with
        // no `tools.json` entry (e.g. this crate's own stub scenario's
        // `web_search`) are unaffected — same immediate-execution path as
        // before this issue.
        if let Some(policy) = self.dist.tool_policy(&tool).cloned() {
            if policy.requires_approval() {
                let idem_key = format!(
                    "{}:{}",
                    self.session_id,
                    self.storage
                        .count_tool_calls_for_session(&self.session_id)
                        .await?
                        + 1
                );
                let row = self
                    .park_for_approval(&tool, &idem_key, &call_id, args, &policy.retry_class)
                    .await?;
                return self.finish_parked_tool_call(call_id, tool, row).await;
            }
        }

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

    /// Deliver the result of a `park_for_approval`-resolved tool call:
    /// a `ToolCall` span for cost/trace visibility, a `tool.call` event,
    /// and the harness's `call.result` — `Ok` with the ledger's recorded
    /// result on `cybersin approve`, `Failed(reason: "denied", retriable:
    /// false)` on `cybersin deny`, mirroring `ToolGateway::approve`/`deny`
    /// exactly since that's what actually resolved the row.
    async fn finish_parked_tool_call(
        &mut self,
        harness_call_id: String,
        tool: String,
        row: ToolCallRecord,
    ) -> Result<u32, RuntimeError> {
        let succeeded = row.status == "succeeded";
        let usd_cost = if succeeded { 0.0008 } else { 0.0 };

        self.emit_span(
            SpanKind::ToolCall,
            tool.clone(),
            None,
            None,
            None,
            usd_cost,
            CacheStatus::NotApplicable,
            row.attempts as u32,
            Vec::new(),
            serde_json::json!({ "args": row.args, "approval_resolved": row.status }),
        )
        .await?;
        self.storage
            .append_event(
                &self.session_id,
                "tool.call",
                serde_json::json!({
                    "tool": tool, "usd_cost": usd_cost, "approval_resolved": row.status,
                }),
            )
            .await?;

        let outcome = if succeeded {
            CallOutcome::Ok {
                value: row.result.unwrap_or(Value::Null),
            }
        } else {
            CallOutcome::Failed {
                reason: row.failure_reason.unwrap_or_default(),
                retriable: row.retriable.unwrap_or(false),
            }
        };
        self.channel
            .send(DaemonMessage::CallResult {
                call_id: harness_call_id,
                outcome,
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
