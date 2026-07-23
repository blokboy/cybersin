//! [`ToolGateway`]: the idempotent tool gateway itself (spec §8.2). Every
//! tool call goes through [`ToolGateway::call`]: schema validation, then
//! admission to the `tool_calls` ledger (`cybersin_runtime::storage`'s
//! `begin_tool_call` — the `UNIQUE(tool, idem_key)` constraint decides who
//! actually executes), then policy hooks, then execution with a
//! retry-class-bounded in-line retry loop.
//!
//! # Why calls don't block forever waiting on an approval
//!
//! A parked call returns [`GatewayOutcome::Parked`] immediately rather
//! than blocking [`ToolGateway::call`] until a human resolves it — spec
//! §8.2's whole point of durable parking is that "durability makes
//! multi-day approval waits free," which an `.await` sitting inside one
//! in-process call can't provide (nothing survives that process exiting,
//! and full cross-process resume is issue #12). [`ToolGateway::approve`]
//! and [`ToolGateway::deny`] instead operate directly on the durable row —
//! by call-id, from any process that can see the same storage — and
//! [`ToolGateway::wait_for_resolution`] is the optional mechanism a
//! same-process caller (a session loop, a test harness) can use to
//! observe a call's eventual outcome, notified immediately when resolution
//! happens in-process and polling on a short fallback interval otherwise
//! (e.g. resolution arriving from a different `cybersin approve` process
//! against the same database).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use cybersin_adapter::messages::{ApprovalId, CallId, CallOutcome};
use cybersin_runtime::storage::ToolCallRecord;
use cybersin_runtime::Storage;
use serde_json::Value;
use tokio::sync::Notify;

use crate::error::{GatewayError, Result};
use crate::executor::ToolExecutor;
use crate::policy::{PolicyContext, PolicyDecision, PolicyHook};
use crate::retry::RetryClass;
use crate::schema::ToolSchema;

/// Every terminal or in-flight-but-parked shape a gateway call can end
/// in — deliberately shaped like `cybersin_adapter::messages::DaemonMessage`'s
/// `CallResult`/`CallParked` split, since that *is* "the normal tool-result
/// channel" spec §8.2 refers to: a caller wiring this into a real session
/// loop sends `Resolved` as `CallResult` and `Parked` as `CallParked`
/// verbatim.
#[derive(Debug, Clone, PartialEq)]
pub enum GatewayOutcome {
    Resolved(CallOutcome),
    Parked {
        call_id: CallId,
        approval_id: ApprovalId,
    },
}

fn call_id_for(tool: &str, idem_key: &str) -> String {
    format!("{tool}:{idem_key}")
}

/// The idempotent tool gateway (spec §8.2).
pub struct ToolGateway {
    storage: Arc<dyn Storage>,
    executor: Arc<dyn ToolExecutor>,
    schemas: HashMap<String, ToolSchema>,
    policy_hooks: Vec<Arc<dyn PolicyHook>>,
    /// Broadcasts "something resolved" to every same-process
    /// [`ToolGateway::wait_for_resolution`] caller — see this module's
    /// doc for why this is best-effort rather than the sole resume path.
    notify: Arc<Notify>,
}

impl ToolGateway {
    pub fn new(storage: Arc<dyn Storage>, executor: Arc<dyn ToolExecutor>) -> Self {
        Self {
            storage,
            executor,
            schemas: HashMap::new(),
            policy_hooks: Vec::new(),
            notify: Arc::new(Notify::new()),
        }
    }

    pub fn with_schema(mut self, tool: impl Into<String>, schema: ToolSchema) -> Self {
        self.schemas.insert(tool.into(), schema);
        self
    }

    pub fn with_policy_hook(mut self, hook: Arc<dyn PolicyHook>) -> Self {
        self.policy_hooks.push(hook);
        self
    }

    /// Submit a tool call (spec §8.2). `idem_key` auto-derives to
    /// `"{session_id}:{seq}"` when `None`.
    pub async fn call(
        &self,
        session_id: &str,
        tool: &str,
        args: Value,
        idem_key: Option<String>,
        retry_class: RetryClass,
    ) -> Result<GatewayOutcome> {
        // Schema validation happens before the call ever touches the
        // ledger (spec §8.2: "schema validation, then the idempotency
        // ledger").
        if let Some(schema) = self.schemas.get(tool) {
            schema.validate(&args)?;
        }

        let (call_id, row, won) = match idem_key {
            Some(key) => {
                let call_id = call_id_for(tool, &key);
                let (row, won) = self
                    .storage
                    .begin_tool_call(
                        &call_id,
                        session_id,
                        tool,
                        &key,
                        retry_class.as_str(),
                        &args,
                    )
                    .await?;
                (call_id, row, won)
            }
            None => {
                self.begin_with_derived_key(session_id, tool, &args, retry_class)
                    .await?
            }
        };

        if !won {
            // We lost the race for (tool, idem_key) — spec §8.2's whole
            // point: never execute a second time. Replay the winner's
            // terminal outcome, report the same park, or wait for the
            // winner still executing to finish.
            return self.outcome_for_existing_row(&call_id, &row).await;
        }

        // We won the ledger race: run policy hooks (spec §8.2: "rate
        // limits, declarative argument guards, approval gates"), in
        // registration order, first non-`Allow` decision wins.
        let ctx = PolicyContext {
            session_id,
            tool,
            args: &args,
            retry_class,
        };
        for hook in &self.policy_hooks {
            match hook.evaluate(&ctx).await {
                PolicyDecision::Allow => {}
                PolicyDecision::Reject { reason } => {
                    self.storage
                        .resolve_tool_call_failed(&call_id, &reason, false)
                        .await?;
                    self.notify.notify_waiters();
                    return Ok(GatewayOutcome::Resolved(CallOutcome::Failed {
                        reason,
                        retriable: false,
                    }));
                }
                PolicyDecision::RequireApproval => {
                    // The call_id doubles as the approval_id: there's
                    // exactly one approval decision per call, so a
                    // second identifier would just be indirection.
                    let approval_id = call_id.clone();
                    self.storage
                        .set_tool_call_awaiting_approval(&call_id, &approval_id)
                        .await?;
                    self.storage
                        .set_session_status(session_id, "awaiting_approval")
                        .await?;
                    self.notify.notify_waiters();
                    return Ok(GatewayOutcome::Parked {
                        call_id,
                        approval_id,
                    });
                }
            }
        }

        self.execute_and_resolve(&call_id, tool, &args, retry_class)
            .await
    }

    async fn begin_with_derived_key(
        &self,
        session_id: &str,
        tool: &str,
        args: &Value,
        retry_class: RetryClass,
    ) -> Result<(String, ToolCallRecord, bool)> {
        // "Keys auto-derived (session:seq) unless supplied" (spec §8.2).
        // Not fully race-proofed against two concurrent auto-derived
        // calls in the *same* session computing the same seq (a narrow
        // window between this count and the insert below) — the UNIQUE
        // constraint still guarantees only one of them would ever be
        // treated as that seq's canonical call, so this is a liveness
        // simplification, not a correctness gap: worst case, an unlucky
        // racer's call reads as a replay of the other's outcome instead
        // of getting its own fresh row. Callers that need real
        // concurrent-submission independence should supply their own
        // idem_key.
        let seq = self
            .storage
            .count_tool_calls_for_session(session_id)
            .await?
            + 1;
        let idem_key = format!("{session_id}:{seq}");
        let call_id = call_id_for(tool, &idem_key);
        let (row, won) = self
            .storage
            .begin_tool_call(
                &call_id,
                session_id,
                tool,
                &idem_key,
                retry_class.as_str(),
                args,
            )
            .await?;
        Ok((call_id, row, won))
    }

    async fn outcome_for_existing_row(
        &self,
        call_id: &str,
        row: &ToolCallRecord,
    ) -> Result<GatewayOutcome> {
        match row.status.as_str() {
            "succeeded" => Ok(GatewayOutcome::Resolved(CallOutcome::Ok {
                value: row.result.clone().unwrap_or(Value::Null),
            })),
            "failed" => Ok(GatewayOutcome::Resolved(CallOutcome::Failed {
                reason: row.failure_reason.clone().unwrap_or_default(),
                retriable: row.retriable.unwrap_or(false),
            })),
            _ if row.awaiting_approval => Ok(GatewayOutcome::Parked {
                call_id: call_id.to_string(),
                approval_id: row.approval_id.clone().unwrap_or_default(),
            }),
            _ => self.wait_for_resolution(call_id).await,
        }
    }

    /// Run the executor, retrying in-line up to `retry_class`'s bounded
    /// budget (spec §8.2's retry classes), then record the terminal
    /// outcome. Shared by a fresh [`ToolGateway::call`], an
    /// [`ToolGateway::approve`]d call now cleared to run, and a manual
    /// [`ToolGateway::dlq_retry`].
    async fn execute_and_resolve(
        &self,
        call_id: &str,
        tool: &str,
        args: &Value,
        retry_class: RetryClass,
    ) -> Result<GatewayOutcome> {
        let attempts_allowed = 1 + retry_class.max_auto_retries();
        let mut last_reason = String::new();
        for _ in 0..attempts_allowed {
            self.storage.increment_tool_call_attempt(call_id).await?;
            match self.executor.execute(tool, args).await {
                Ok(value) => {
                    self.storage
                        .resolve_tool_call_succeeded(call_id, value.clone())
                        .await?;
                    self.notify.notify_waiters();
                    return Ok(GatewayOutcome::Resolved(CallOutcome::Ok { value }));
                }
                Err(reason) => last_reason = reason,
            }
        }
        // `critical` never auto-retries and its exhausted failure isn't
        // worth a `dlq retry`-less resubmission either — `retriable`
        // tells the harness that (spec §8.2 / messages.rs's CallOutcome
        // doc). `read`/`write` exhausting their in-line budget is still
        // `retriable: true`: a human `dlq retry` may well succeed where
        // the bounded auto-retry gave up.
        let retriable = retry_class != RetryClass::Critical;
        self.storage
            .resolve_tool_call_failed(call_id, &last_reason, retriable)
            .await?;
        self.notify.notify_waiters();
        Ok(GatewayOutcome::Resolved(CallOutcome::Failed {
            reason: last_reason,
            retriable,
        }))
    }

    /// Block until `call_id` leaves the pending-and-not-parked state,
    /// returning its terminal outcome (or `Parked`, if it's gated on an
    /// approval instead). See this module's doc for the notify+poll
    /// design and why it's best-effort across processes.
    pub async fn wait_for_resolution(&self, call_id: &str) -> Result<GatewayOutcome> {
        loop {
            let row = self
                .storage
                .get_tool_call(call_id)
                .await?
                .ok_or_else(|| GatewayError::NotFound(call_id.to_string()))?;
            match row.status.as_str() {
                "succeeded" => {
                    return Ok(GatewayOutcome::Resolved(CallOutcome::Ok {
                        value: row.result.unwrap_or(Value::Null),
                    }))
                }
                "failed" => {
                    return Ok(GatewayOutcome::Resolved(CallOutcome::Failed {
                        reason: row.failure_reason.unwrap_or_default(),
                        retriable: row.retriable.unwrap_or(false),
                    }))
                }
                _ if row.awaiting_approval => {
                    return Ok(GatewayOutcome::Parked {
                        call_id: call_id.to_string(),
                        approval_id: row.approval_id.unwrap_or_default(),
                    })
                }
                _ => {
                    let _ = tokio::time::timeout(Duration::from_millis(20), self.notify.notified())
                        .await;
                }
            }
        }
    }

    /// `cybersin approve <call-id>` (spec §8.2): clears the approval gate,
    /// resumes the session (`awaiting_approval` -> `running`), and runs
    /// the call to a normal terminal outcome — approval doesn't itself
    /// decide success, it just lets execution proceed.
    pub async fn approve(&self, call_id: &str) -> Result<GatewayOutcome> {
        let row = self.parked_row(call_id).await?;
        self.storage
            .clear_tool_call_awaiting_approval(call_id)
            .await?;
        self.storage
            .set_session_status(&row.session_id, "running")
            .await?;
        self.notify.notify_waiters();
        let retry_class = RetryClass::parse(&row.retry_class).unwrap_or(RetryClass::Write);
        self.execute_and_resolve(call_id, &row.tool, &row.args, retry_class)
            .await
    }

    /// `cybersin deny <call-id>` (spec §8.2): resolves the call to
    /// `failed(reason: "denied", retriable: false)` — a distinct terminal
    /// outcome from a transient execution failure — through the exact
    /// same resolution path `execute_and_resolve` uses, and resumes the
    /// session to `running` (denial does not kill the session: the
    /// gateway's job ends at recording the human's decision durably, the
    /// agent's own logic decides what happens next).
    pub async fn deny(&self, call_id: &str) -> Result<GatewayOutcome> {
        let row = self.parked_row(call_id).await?;
        self.storage
            .clear_tool_call_awaiting_approval(call_id)
            .await?;
        self.storage
            .resolve_tool_call_failed(call_id, "denied", false)
            .await?;
        self.storage
            .set_session_status(&row.session_id, "running")
            .await?;
        self.notify.notify_waiters();
        Ok(GatewayOutcome::Resolved(CallOutcome::Failed {
            reason: "denied".to_string(),
            retriable: false,
        }))
    }

    async fn parked_row(&self, call_id: &str) -> Result<ToolCallRecord> {
        let row = self
            .storage
            .get_tool_call(call_id)
            .await?
            .ok_or_else(|| GatewayError::NotFound(call_id.to_string()))?;
        if row.status != "pending" || !row.awaiting_approval {
            return Err(GatewayError::NotAwaitingApproval(call_id.to_string()));
        }
        Ok(row)
    }

    // -- Dead-letter queue: `cybersin dlq ls|show|retry|drop` (spec §8.2) --

    pub async fn dlq_list(&self) -> Result<Vec<ToolCallRecord>> {
        Ok(self.storage.list_dead_letters().await?)
    }

    pub async fn dlq_show(&self, call_id: &str) -> Result<ToolCallRecord> {
        self.storage
            .get_tool_call(call_id)
            .await?
            .ok_or_else(|| GatewayError::NotFound(call_id.to_string()))
    }

    /// Reopen a dead letter and run it again. Unlike `call`'s in-line
    /// auto-retry budget, this is a deliberate human override — it runs
    /// regardless of retry class, including `critical` (spec §8.2:
    /// "never auto-retry" governs the gateway's own automatic behavior,
    /// not an explicit human decision).
    pub async fn dlq_retry(&self, call_id: &str) -> Result<GatewayOutcome> {
        let row = self.dead_letter_row(call_id).await?;
        self.storage.reopen_tool_call(call_id).await?;
        self.notify.notify_waiters();
        let retry_class = RetryClass::parse(&row.retry_class).unwrap_or(RetryClass::Write);
        self.execute_and_resolve(call_id, &row.tool, &row.args, retry_class)
            .await
    }

    /// Acknowledge and discard a dead letter — excluded from `dlq_list`
    /// from now on, without erasing the audit row.
    pub async fn dlq_drop(&self, call_id: &str) -> Result<()> {
        self.dead_letter_row(call_id).await?;
        self.storage.set_tool_call_dropped(call_id, true).await?;
        Ok(())
    }

    async fn dead_letter_row(&self, call_id: &str) -> Result<ToolCallRecord> {
        let row = self
            .storage
            .get_tool_call(call_id)
            .await?
            .ok_or_else(|| GatewayError::NotFound(call_id.to_string()))?;
        if row.status != "failed" || row.dropped {
            return Err(GatewayError::NotADeadLetter(call_id.to_string()));
        }
        Ok(row)
    }
}
