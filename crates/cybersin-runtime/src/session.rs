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
use cybersin_adapter::messages::{AbortReason, CallOutcome, DaemonMessage, HarnessMessage};
use cybersin_ir::{BudgetArtifact, BudgetPlan, PromptIr, Section};
use cybersin_sandbox::{SandboxScope, WorkspaceStore};
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
    /// Persistent per-session sandbox workspace (spec §8.4's `scope:
    /// session`), snapshotted at every [`RuntimeDaemon::create_checkpoint`]
    /// so a crash-and-resume restores the paired filesystem state, not
    /// just the event log. `None` means no session-scoped sandbox is in
    /// play — every checkpoint before this issue existed behaved this way.
    session_sandbox: Option<WorkspaceStore>,
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
            session_sandbox: None,
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

    /// Attach a persistent per-session sandbox workspace (spec §8.4).
    /// Builder method, same reasoning as [`RuntimeDaemon::with_budget`]:
    /// every existing caller that doesn't declare `scope: session` sandbox
    /// is unaffected.
    pub fn with_session_sandbox(mut self, session_sandbox: WorkspaceStore) -> Self {
        self.session_sandbox = Some(session_sandbox);
        self
    }

    /// Every durable checkpoint (spec §8.1) also snapshots the session
    /// sandbox when one is attached (spec §8.4: "every session checkpoint
    /// also takes a sandbox snapshot tied to the same checkpoint ID"), so
    /// every call site below goes through this instead of
    /// `self.storage.create_checkpoint` directly.
    async fn create_checkpoint(
        &self,
        label: Option<&str>,
    ) -> Result<crate::CheckpointRecord, RuntimeError> {
        let checkpoint = self
            .storage
            .create_checkpoint(&self.session_id, label)
            .await?;
        if let Some(store) = &self.session_sandbox {
            store
                .open(SandboxScope::Session, &self.session_id, "checkpoint")?
                .snapshot(&checkpoint.checkpoint_id.to_string())?;
        }
        Ok(checkpoint)
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
                let checkpoint = self.create_checkpoint(label.as_deref()).await?;
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
            HarnessMessage::Spawn {
                call_id,
                child_config,
                budget_usd,
            } => {
                let orchestrator = crate::Orchestrator::new(self.storage.clone());
                if self
                    .storage
                    .get_state(&self.session_id, "orchestration", "budget_usd")
                    .await?
                    .is_none()
                {
                    orchestrator
                        .register_parent(
                            &self.session_id,
                            &self.agent_name,
                            self.budget
                                .as_ref()
                                .map(|b| b.usd_per_session)
                                .unwrap_or(f64::INFINITY),
                        )
                        .await
                        .map_err(|e| RuntimeError::Session(e.to_string()))?;
                }
                let worker_id = format!("{}:{call_id}", self.session_id);
                let worker = orchestrator
                    .spawn(&self.session_id, &worker_id, child_config, budget_usd, None)
                    .await
                    .map_err(|e| RuntimeError::Session(e.to_string()))?;
                self.channel
                    .send(DaemonMessage::CallResult {
                        call_id,
                        outcome: CallOutcome::Ok {
                            value: serde_json::to_value(worker)
                                .map_err(crate::StorageError::from)?,
                        },
                    })
                    .await?;
                Ok(0)
            }
            HarnessMessage::MailboxSend {
                call_id,
                recipient,
                payload,
            } => {
                crate::Orchestrator::new(self.storage.clone())
                    .send(&self.session_id, &recipient, payload)
                    .await
                    .map_err(|e| RuntimeError::Session(e.to_string()))?;
                self.channel
                    .send(DaemonMessage::CallResult {
                        call_id,
                        outcome: CallOutcome::Ok { value: Value::Null },
                    })
                    .await?;
                Ok(0)
            }
            HarnessMessage::MailboxReceive { call_id, sender } => {
                let mail = crate::Orchestrator::new(self.storage.clone())
                    .drain(&self.session_id, &sender)
                    .await
                    .map_err(|e| RuntimeError::Session(e.to_string()))?;
                self.channel
                    .send(DaemonMessage::CallResult {
                        call_id,
                        outcome: CallOutcome::Ok {
                            value: serde_json::to_value(mail).map_err(crate::StorageError::from)?,
                        },
                    })
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
        self.create_checkpoint(Some("pre-llm")).await?;

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
        self.create_checkpoint(Some("periodic")).await?;

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
        self.create_checkpoint(Some("pre-tool")).await?;

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
        self.create_checkpoint(Some("periodic")).await?;

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
