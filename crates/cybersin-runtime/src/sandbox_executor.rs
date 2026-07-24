//! Trace-integrated sandbox execution for runtime sessions.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use cybersin_sandbox::{ExecOutcome, ExecRequest, SandboxBackend};
use cybersin_trace::{CacheStatus, Span, SpanKind, SpanStatus, SpanStore};

use crate::RuntimeError;

static NEXT_SANDBOX_SPAN_ID: AtomicU64 = AtomicU64::new(1);

/// Runs generated commands through a selected containment backend and
/// records the outcome on the same trace stream as routing and context
/// assembly. A contained failure is an error span, not a session abort.
pub struct RuntimeSandbox<B> {
    backend: B,
    spans: SpanStore,
}

impl<B: SandboxBackend> RuntimeSandbox<B> {
    pub fn new(backend: B, spans: SpanStore) -> Self {
        Self { backend, spans }
    }

    pub async fn execute(
        &self,
        session_id: &str,
        agent_name: &str,
        name: &str,
        request: ExecRequest,
    ) -> Result<ExecOutcome, RuntimeError> {
        let start = now_unix_ms();
        let outcome = self.backend.exec(request)?;
        let end = now_unix_ms();
        let status = if outcome.succeeded() {
            SpanStatus::Ok
        } else {
            SpanStatus::Error {
                message: format!(
                    "sandbox command contained: termination={:?}, exit_code={:?}",
                    outcome.termination, outcome.exit_code
                ),
            }
        };
        self.spans
            .insert(&Span {
                id: format!(
                    "sandbox-{session_id}-{}",
                    NEXT_SANDBOX_SPAN_ID.fetch_add(1, Ordering::Relaxed)
                ),
                session_id: session_id.to_string(),
                agent_name: agent_name.to_string(),
                kind: SpanKind::SandboxExec,
                name: name.to_string(),
                start_unix_ms: start,
                end_unix_ms: end,
                model: None,
                tokens_prompt: None,
                tokens_completion: None,
                usd_cost: 0.0,
                cache_status: CacheStatus::NotApplicable,
                retries: 0,
                evicted_sections: vec![],
                status,
                attributes: serde_json::json!({
                    "exit_code": outcome.exit_code,
                    "termination": format!("{:?}", outcome.termination),
                    "stdout": outcome.stdout,
                    "stderr": outcome.stderr,
                    "contained": !outcome.succeeded(),
                }),
            })
            .await?;
        Ok(outcome)
    }
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
