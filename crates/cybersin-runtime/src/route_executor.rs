//! Runtime execution of compiler-produced `routing.json` and `cache.json`
//! (spec §8.3).

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use cybersin_router::{CascadeStep, RouteDecision, RouteModel, RoutingArtifact};
use cybersin_trace::{CacheStatus, Span, SpanKind, SpanStatus, SpanStore};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::allowlist::ModelAllowlist;

static NEXT_ROUTE_SPAN_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheArtifact {
    pub schema_version: u32,
    #[serde(deserialize_with = "deserialize_namespace_version")]
    pub namespace_version: String,
    #[serde(default, deserialize_with = "deserialize_cache_entries")]
    pub entries: Vec<CacheEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheEntry {
    pub prompt_name: String,
    pub input_hash: String,
    pub embedding: Vec<f32>,
    pub response: Value,
}

impl CacheArtifact {
    /// Incrementally insert or replace one vector in the in-process index.
    pub fn upsert(&mut self, entry: CacheEntry) {
        if let Some(existing) = self.entries.iter_mut().find(|existing| {
            existing.prompt_name == entry.prompt_name && existing.input_hash == entry.input_hash
        }) {
            *existing = entry;
        } else {
            self.entries.push(entry);
        }
    }
}

fn deserialize_namespace_version<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Version {
        String(String),
        Number(u64),
    }

    Ok(match Version::deserialize(deserializer)? {
        Version::String(value) => value,
        Version::Number(value) => value.to_string(),
    })
}

fn deserialize_cache_entries<'de, D>(deserializer: D) -> Result<Vec<CacheEntry>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Entries {
        List(Vec<CacheEntry>),
        Map(std::collections::BTreeMap<String, CacheEntry>),
    }

    Ok(match Entries::deserialize(deserializer)? {
        Entries::List(entries) => entries,
        Entries::Map(entries) => entries.into_values().collect(),
    })
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExecutionRequest {
    pub session_id: String,
    pub agent_name: String,
    pub prompt_name: String,
    pub inputs: Value,
    pub embedding: Vec<f32>,
    pub namespace_version: String,
    pub bypass: bool,
    /// Context-assembler output for this call (spec §8.3a), threaded
    /// through purely so the `llm_call` span this executor writes carries
    /// it — the executor itself has no opinion on context assembly.
    #[doc(hidden)]
    pub prompt_tokens: u32,
    #[doc(hidden)]
    pub completion_tokens: Option<u32>,
    #[doc(hidden)]
    pub evicted_sections: Vec<String>,
    /// Merged verbatim into the recorded `llm_call` span's
    /// `attributes["context"]` — the caller's own context-assembly
    /// metadata (target, included/evicted sections), opaque to this
    /// executor.
    #[doc(hidden)]
    pub context_attributes: Value,
    /// spec §8.5's `on_breach: degrade`: skip cascade confidence checks
    /// entirely and accept the first (cheapest) step's output
    /// unconditionally. Set by the caller once its own budget enforcement
    /// (outside this executor's scope) has decided to degrade.
    pub force_cheapest_cascade_step: bool,
    /// The model a cache hit should be attributed to for span/cost
    /// purposes — a cache entry doesn't record which model originally
    /// produced it, so the caller supplies "the model this prompt would
    /// use" (its highest-quality cascade step) purely for labeling.
    pub default_model: Option<RouteModel>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModelUsage {
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
    pub usd_cost: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModelOutput {
    pub response: Value,
    pub confidence: f64,
    pub usage: Option<ModelUsage>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelErrorClass {
    Transient { retry_after: Option<Duration> },
    Permanent,
    Ambiguous,
}

impl ModelErrorClass {
    pub fn transient() -> Self {
        Self::Transient { retry_after: None }
    }

    pub fn transient_with_retry_after(retry_after: Duration) -> Self {
        Self::Transient {
            retry_after: Some(retry_after),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ModelCallError {
    pub class: ModelErrorClass,
    pub message: String,
}

impl ModelCallError {
    pub fn transient(message: impl Into<String>) -> Self {
        Self {
            class: ModelErrorClass::transient(),
            message: message.into(),
        }
    }

    pub fn transient_with_retry_after(message: impl Into<String>, retry_after: Duration) -> Self {
        Self {
            class: ModelErrorClass::transient_with_retry_after(retry_after),
            message: message.into(),
        }
    }

    pub fn permanent(message: impl Into<String>) -> Self {
        Self {
            class: ModelErrorClass::Permanent,
            message: message.into(),
        }
    }

    pub fn ambiguous(message: impl Into<String>) -> Self {
        Self {
            class: ModelErrorClass::Ambiguous,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExecutionResponse {
    pub response: Value,
    pub model: Option<String>,
    pub cache_hit: bool,
    pub usd_cost: f64,
    pub usage: Option<ModelUsage>,
}

/// The model a "normal" (non-degraded) call to `route` would use: its
/// highest-quality cascade step, or failing that its first provider
/// fallback. Shared by [`crate::dist::DistFixture`]'s own legacy-bridge
/// view of the same routing document so both callers agree on what
/// "the model this prompt uses" means.
pub fn default_model(route: &cybersin_router::PromptRoute) -> Option<&RouteModel> {
    route
        .decisions
        .iter()
        .find_map(|decision| match decision {
            RouteDecision::Cascade(cascade) => cascade.steps.last().map(|step| &step.model),
            _ => None,
        })
        .or_else(|| {
            route.decisions.iter().find_map(|decision| match decision {
                RouteDecision::Fallbacks(fallbacks) => fallbacks
                    .providers
                    .iter()
                    .find(|model| model.model_kind == cybersin_router::ModelKind::Provider),
                _ => None,
            })
        })
}

#[async_trait]
pub trait ModelCaller: Send + Sync {
    /// `confidence_instruction`: the cascade step's own
    /// `ConfidenceRubric::instruction` (spec's confidence rubric,
    /// compiled into `routing.json` — e.g. "Score 0..1 whether the
    /// response satisfies the High quality contract; accept at or above
    /// 0.90") for a real implementation to actually put in front of the
    /// model, so its self-reported confidence means something instead of
    /// being an ungrounded guess at a bare schema field. `None` for calls
    /// with no such rubric (`RouteDecision::Fallbacks`, which accepts
    /// unconditionally and never compares confidence against a
    /// threshold at all).
    ///
    /// `grounded`: the cascade step's own `CascadeStep::grounded` (spec
    /// issue #80/#83) for a real implementation to request OpenRouter's
    /// web-search grounding plugin on this specific call. Plain `bool`,
    /// not `Option`, because it is unconditionally either true or false —
    /// unlike `confidence_instruction`, there's no "no such concept"
    /// case: fallback-tier calls always pass `false`, since
    /// `RouteModel`/`FallbackDecision::providers` structurally has no
    /// grounding marker at all.
    async fn call(
        &self,
        model: &RouteModel,
        prompt_name: &str,
        inputs: &Value,
        confidence_instruction: Option<&str>,
        grounded: bool,
    ) -> Result<ModelOutput, ModelCallError>;
}

#[async_trait]
pub trait Judge: Send + Sync {
    async fn accepts(
        &self,
        model: &RouteModel,
        prompt_name: &str,
        inputs: &Value,
        cached_response: &Value,
        similarity: f64,
    ) -> Result<bool, String>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnnBackend {
    BruteForce,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouteRetryPolicy {
    pub max_attempts: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
}

impl Default for RouteRetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(60),
        }
    }
}

impl RouteRetryPolicy {
    /// Pure full-jitter delay function. `attempt` is one-based for the
    /// retry about to be slept before, and `jitter` must be in `[0.0, 1.0]`.
    pub fn delay_for(
        &self,
        attempt: u32,
        class: &ModelErrorClass,
        jitter: f64,
    ) -> Option<Duration> {
        let ModelErrorClass::Transient { retry_after } = class else {
            return None;
        };
        if let Some(retry_after) = retry_after {
            return Some((*retry_after).min(self.max_delay));
        }
        let multiplier = 1_u128 << attempt.saturating_sub(1).min(31);
        let uncapped = duration_mul(self.base_delay, multiplier);
        let capped = uncapped.min(self.max_delay);
        let jitter = jitter.clamp(0.0, 1.0);
        Some(duration_mul_f64(capped, jitter))
    }
}

#[async_trait]
pub trait AsyncSleeper: Send + Sync {
    async fn sleep(&self, delay: Duration);
}

pub struct TokioSleeper;

#[async_trait]
impl AsyncSleeper for TokioSleeper {
    async fn sleep(&self, delay: Duration) {
        tokio::time::sleep(delay).await;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SqliteVecEvaluation {
    pub static_linkable: bool,
    pub cross_platform: bool,
    pub incremental_upsert: bool,
    pub concurrent_safe_under_tokio: bool,
    pub p99_under_10ms_at_50k: bool,
    pub selected_backend: KnnBackend,
}

/// sqlite-vec is not linked into this workspace, so none of its required
/// build/runtime properties can be demonstrated. The fail-closed maturity
/// gate therefore selects the in-process brute-force implementation.
pub const SQLITE_VEC_EVALUATION: SqliteVecEvaluation = SqliteVecEvaluation {
    static_linkable: false,
    cross_platform: false,
    incremental_upsert: false,
    concurrent_safe_under_tokio: false,
    p99_under_10ms_at_50k: false,
    selected_backend: KnnBackend::BruteForce,
};

pub struct RouteExecutor<M, J> {
    routing: RoutingArtifact,
    cache: CacheArtifact,
    models: M,
    judge: J,
    spans: SpanStore,
    /// Environment-level restriction on which candidates this executor may
    /// actually call (issue #35 Phase 1). Defaults to "everything allowed"
    /// so every caller that predates this config is unaffected. Enforced
    /// here, at call time, rather than by filtering `routing.json` at
    /// build time — see `crate::allowlist` — so `dist/` stays portable
    /// across environments.
    allowlist: ModelAllowlist,
    retry_policy: RouteRetryPolicy,
    sleeper: Arc<dyn AsyncSleeper>,
}

#[derive(Debug, thiserror::Error)]
pub enum RouteExecutorError {
    #[error("io error reading route/cache artifact at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse {path}: {source}")]
    Json {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("routing artifact has no prompt {0:?}")]
    MissingPrompt(String),
    #[error("all route decisions were exhausted for prompt {0:?}")]
    Exhausted(String),
    #[error(
        "all route decisions were exhausted for prompt {prompt:?}; last errors: {last_errors}"
    )]
    ExhaustedWithLastErrors { prompt: String, last_errors: String },
    #[error("trace store error: {0}")]
    Trace(#[from] cybersin_trace::TraceError),
}

impl<M: ModelCaller, J: Judge> RouteExecutor<M, J> {
    pub fn new(
        routing: RoutingArtifact,
        cache: CacheArtifact,
        models: M,
        judge: J,
        spans: SpanStore,
    ) -> Self {
        Self {
            routing,
            cache,
            models,
            judge,
            spans,
            allowlist: ModelAllowlist::allow_all(),
            retry_policy: RouteRetryPolicy::default(),
            sleeper: Arc::new(TokioSleeper),
        }
    }

    /// Restrict which candidates this executor will actually call. See
    /// `crate::allowlist::ModelAllowlist`.
    pub fn with_allowlist(mut self, allowlist: ModelAllowlist) -> Self {
        self.allowlist = allowlist;
        self
    }

    pub fn with_retry_policy(mut self, retry_policy: RouteRetryPolicy) -> Self {
        self.retry_policy = retry_policy;
        self
    }

    pub fn with_sleeper(mut self, sleeper: Arc<dyn AsyncSleeper>) -> Self {
        self.sleeper = sleeper;
        self
    }

    pub fn load_dir(
        dir: impl AsRef<Path>,
        models: M,
        judge: J,
        spans: SpanStore,
    ) -> Result<Self, RouteExecutorError> {
        let dir = dir.as_ref();
        let routing = read_json(&dir.join("routing.json"))?;
        let cache = read_json(&dir.join("cache.json"))?;
        Ok(Self::new(routing, cache, models, judge, spans))
    }

    /// Seed or replace one cache entry using the same deterministic key
    /// shape the on-disk artifact uses. Runtime persistence is a later
    /// storage concern; this makes incremental in-process cache writes
    /// available to the integrated executor today.
    pub fn upsert_cache(&mut self, entry: CacheEntry) {
        self.cache.upsert(entry);
    }

    pub async fn execute(
        &self,
        request: &ExecutionRequest,
    ) -> Result<ExecutionResponse, RouteExecutorError> {
        let route = self
            .routing
            .prompts
            .get(&request.prompt_name)
            .ok_or_else(|| RouteExecutorError::MissingPrompt(request.prompt_name.clone()))?;
        let mut last_cascade_error: Option<String> = None;
        let mut last_fallback_error: Option<String> = None;

        for decision in &route.decisions {
            match decision {
                RouteDecision::Cache(cache) => {
                    if let Some(mut response) = self.execute_cache(request, cache).await? {
                        // A cache hit reuses whatever model originally
                        // produced the entry, but `CacheEntry` doesn't
                        // record that — so this executor attributes the
                        // hit to `default_model` (the model this prompt
                        // would normally use) purely for span/cost
                        // labeling. Zero cost either way: no model call
                        // actually happened.
                        response.model = request.default_model.as_ref().map(|m| m.name.clone());
                        response.usd_cost = 0.0;
                        self.record(
                            request,
                            SpanKind::LlmCall,
                            CacheStatus::Hit,
                            request.default_model.as_ref(),
                            0.0,
                            None,
                            serde_json::json!({ "decision": "cache_hit" }),
                        )
                        .await?;
                        return Ok(response);
                    }
                }
                RouteDecision::Cascade(cascade) => {
                    // spec §8.5 `on_breach: degrade`: skip confidence
                    // checks and accept the first (cheapest) step's
                    // output unconditionally, rather than walking the
                    // full cascade.
                    let steps: &[CascadeStep] = if request.force_cheapest_cascade_step {
                        cascade
                            .steps
                            .first()
                            .map(std::slice::from_ref)
                            .unwrap_or(&[])
                    } else {
                        &cascade.steps
                    };
                    for step in steps {
                        if !self.allowlist.allows(&step.model) {
                            // Not a call failure — this environment simply
                            // isn't configured to reach this candidate.
                            // Fall through the cascade exactly as if this
                            // step didn't exist, same as a real error
                            // would, but without spending a call on it.
                            continue;
                        }
                        let result = self
                            .call_model_with_retries(
                                &step.model,
                                &request.prompt_name,
                                &request.inputs,
                                Some(&step.confidence.instruction),
                                step.grounded,
                            )
                            .await;
                        let force_accept = request.force_cheapest_cascade_step;
                        match result {
                            Ok(output)
                                if force_accept
                                    || output.confidence >= step.confidence.minimum_score =>
                            {
                                let usage = output.usage.clone();
                                let usd_cost =
                                    observed_usd_cost(&usage, step.model.estimated_cost_usd);
                                self.record(
                                    request,
                                    SpanKind::LlmCall,
                                    CacheStatus::Miss,
                                    Some(&step.model),
                                    step.model.estimated_cost_usd,
                                    usage.as_ref(),
                                    serde_json::json!({
                                        "decision": if force_accept { "degrade_forced" } else { "cascade_accept" },
                                        "model": step.model.name,
                                        "confidence": output.confidence,
                                        "minimum_confidence": step.confidence.minimum_score,
                                        "grounded": step.grounded,
                                    }),
                                )
                                .await?;
                                return Ok(ExecutionResponse {
                                    response: output.response,
                                    model: Some(step.model.name.clone()),
                                    cache_hit: false,
                                    usd_cost,
                                    usage,
                                });
                            }
                            Ok(output) => {
                                let usage = output.usage.clone();
                                self.record(
                                    request,
                                    SpanKind::LlmCall,
                                    CacheStatus::Miss,
                                    Some(&step.model),
                                    step.model.estimated_cost_usd,
                                    usage.as_ref(),
                                    serde_json::json!({
                                        "decision": "cascade_escalation",
                                        "model": step.model.name,
                                        "confidence": output.confidence,
                                        "minimum_confidence": step.confidence.minimum_score,
                                        "grounded": step.grounded,
                                    }),
                                )
                                .await?;
                            }
                            Err(error) => {
                                last_cascade_error =
                                    Some(format!("{}: {}", step.model.name, error));
                                self.record(
                                    request,
                                    SpanKind::LlmCall,
                                    CacheStatus::Miss,
                                    Some(&step.model),
                                    step.model.estimated_cost_usd,
                                    None,
                                    serde_json::json!({
                                        "decision": "cascade_error",
                                        "model": step.model.name,
                                        "error": error.to_string(),
                                        "error_class": error_class_name(&error.class),
                                        "grounded": step.grounded,
                                    }),
                                )
                                .await?;
                            }
                        }
                    }
                }
                RouteDecision::Fallbacks(fallbacks) => {
                    for model in &fallbacks.providers {
                        if !self.allowlist.allows(model) {
                            continue;
                        }
                        match self
                            .call_model_with_retries(
                                model,
                                &request.prompt_name,
                                &request.inputs,
                                None,
                                false,
                            )
                            .await
                        {
                            Ok(output) => {
                                let usage = output.usage.clone();
                                let usd_cost = observed_usd_cost(&usage, model.estimated_cost_usd);
                                self.record(
                                    request,
                                    SpanKind::LlmCall,
                                    CacheStatus::Miss,
                                    Some(model),
                                    model.estimated_cost_usd,
                                    usage.as_ref(),
                                    serde_json::json!({
                                        "decision": "fallback_accept",
                                        "model": model.name,
                                        "confidence": output.confidence,
                                        "grounded": false,
                                    }),
                                )
                                .await?;
                                return Ok(ExecutionResponse {
                                    response: output.response,
                                    model: Some(model.name.clone()),
                                    cache_hit: false,
                                    usd_cost,
                                    usage,
                                });
                            }
                            Err(error) => {
                                last_fallback_error = Some(format!("{}: {}", model.name, error));
                                self.record(
                                    request,
                                    SpanKind::LlmCall,
                                    CacheStatus::Miss,
                                    Some(model),
                                    model.estimated_cost_usd,
                                    None,
                                    serde_json::json!({
                                        "decision": "fallback_error",
                                        "model": model.name,
                                        "error": error.to_string(),
                                        "error_class": error_class_name(&error.class),
                                        "grounded": false,
                                    }),
                                )
                                .await?;
                            }
                        }
                    }
                }
            }
        }
        let last_errors = [
            last_cascade_error.map(|error| format!("cascade={error}")),
            last_fallback_error.map(|error| format!("fallback={error}")),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        if !last_errors.is_empty() {
            return Err(RouteExecutorError::ExhaustedWithLastErrors {
                prompt: request.prompt_name.clone(),
                last_errors: last_errors.join("; "),
            });
        }
        Err(RouteExecutorError::Exhausted(request.prompt_name.clone()))
    }

    async fn call_model_with_retries(
        &self,
        model: &RouteModel,
        prompt_name: &str,
        inputs: &Value,
        confidence_instruction: Option<&str>,
        grounded: bool,
    ) -> Result<ModelOutput, ModelCallError> {
        let mut failed_attempts = 0;
        loop {
            match self
                .models
                .call(model, prompt_name, inputs, confidence_instruction, grounded)
                .await
            {
                Ok(output) => return Ok(output),
                Err(error) => {
                    let attempts_so_far = failed_attempts + 1;
                    if attempts_so_far >= self.retry_policy.max_attempts {
                        return Err(error);
                    }
                    let retry_attempt = failed_attempts + 1;
                    let Some(delay) = self.retry_policy.delay_for(
                        retry_attempt,
                        &error.class,
                        deterministic_jitter(model, prompt_name, retry_attempt),
                    ) else {
                        return Err(error);
                    };
                    failed_attempts += 1;
                    self.sleeper.sleep(delay).await;
                }
            }
        }
    }

    async fn execute_cache(
        &self,
        request: &ExecutionRequest,
        cache: &cybersin_router::CacheDecision,
    ) -> Result<Option<ExecutionResponse>, RouteExecutorError> {
        if request.bypass {
            self.record(
                request,
                SpanKind::CacheDecision,
                CacheStatus::Miss,
                None,
                0.0,
                None,
                serde_json::json!({"decision": "bypass", "similarity": Value::Null}),
            )
            .await?;
            return Ok(None);
        }
        if request.namespace_version != self.cache.namespace_version {
            self.record(
                request,
                SpanKind::CacheDecision,
                CacheStatus::Miss,
                None,
                0.0,
                None,
                serde_json::json!({
                    "decision": "namespace_invalidated",
                    "similarity": Value::Null,
                    "requested_namespace_version": request.namespace_version,
                    "cache_namespace_version": self.cache.namespace_version,
                }),
            )
            .await?;
            return Ok(None);
        }

        let key = cache_key(&request.prompt_name, &request.inputs);
        let entries = self
            .cache
            .entries
            .iter()
            .filter(|entry| entry.prompt_name == request.prompt_name);
        if let Some(entry) = entries.clone().find(|entry| entry.input_hash == key) {
            self.record(
                request,
                SpanKind::CacheDecision,
                CacheStatus::Hit,
                None,
                0.0,
                None,
                serde_json::json!({"decision": "hash_hit", "similarity": 1.0}),
            )
            .await?;
            return Ok(Some(cache_response(entry.response.clone())));
        }

        let nearest = entries
            .filter_map(|entry| {
                cosine_similarity(&request.embedding, &entry.embedding).map(|s| (entry, s))
            })
            .max_by(|left, right| left.1.total_cmp(&right.1));
        let Some((entry, similarity)) = nearest else {
            self.record(
                request,
                SpanKind::CacheDecision,
                CacheStatus::Miss,
                None,
                0.0,
                None,
                serde_json::json!({"decision": "miss", "similarity": Value::Null}),
            )
            .await?;
            return Ok(None);
        };

        if similarity >= cache.similarity_threshold {
            self.record(
                request,
                SpanKind::CacheDecision,
                CacheStatus::Hit,
                None,
                0.0,
                None,
                serde_json::json!({"decision": "knn_hit", "similarity": similarity}),
            )
            .await?;
            return Ok(Some(cache_response(entry.response.clone())));
        }

        if similarity >= cache.judge_trigger_band[0] && similarity <= cache.judge_trigger_band[1] {
            let judge_result = self
                .judge
                .accepts(
                    &cache.judge,
                    &request.prompt_name,
                    &request.inputs,
                    &entry.response,
                    similarity,
                )
                .await;
            let accepted = judge_result.as_ref().copied().unwrap_or(false);
            self.record(
                request,
                SpanKind::CacheDecision,
                if accepted {
                    CacheStatus::Hit
                } else {
                    CacheStatus::Miss
                },
                Some(&cache.judge),
                0.0,
                None,
                serde_json::json!({
                    "decision": if accepted { "judge_hit" } else { "judge_reject" },
                    "similarity": similarity,
                    "judge_outcome": judge_result.as_ref().ok(),
                    "judge_error": judge_result.as_ref().err(),
                }),
            )
            .await?;
            if accepted {
                return Ok(Some(cache_response(entry.response.clone())));
            }
            return Ok(None);
        }

        self.record(
            request,
            SpanKind::CacheDecision,
            CacheStatus::Miss,
            None,
            0.0,
            None,
            serde_json::json!({"decision": "miss", "similarity": similarity}),
        )
        .await?;
        Ok(None)
    }

    #[allow(clippy::too_many_arguments)]
    async fn record(
        &self,
        request: &ExecutionRequest,
        kind: SpanKind,
        cache_status: CacheStatus,
        model: Option<&RouteModel>,
        usd_cost: f64,
        usage: Option<&ModelUsage>,
        mut attributes: Value,
    ) -> Result<(), RouteExecutorError> {
        let now = now_unix_ms();
        // Context-assembler bookkeeping (spec §8.3a) only applies to the
        // `llm_call` span itself — cache-decision spans stay exactly as
        // narrow as before.
        let (tokens_prompt, tokens_completion, evicted_sections) = if kind == SpanKind::LlmCall {
            if let Value::Object(map) = &mut attributes {
                map.insert("context".to_string(), request.context_attributes.clone());
                map.insert(
                    "usage_source".to_string(),
                    Value::String(
                        if usage.is_some() {
                            "provider"
                        } else {
                            "route_estimate"
                        }
                        .to_string(),
                    ),
                );
            }
            (
                usage
                    .and_then(|usage| usage.prompt_tokens)
                    .or(Some(request.prompt_tokens)),
                usage
                    .and_then(|usage| usage.completion_tokens)
                    .or(request.completion_tokens),
                request.evicted_sections.clone(),
            )
        } else {
            (None, None, Vec::new())
        };
        let usd_cost = usage.and_then(|usage| usage.usd_cost).unwrap_or(usd_cost);
        self.spans
            .insert(&Span {
                id: format!(
                    "route-{}-{}-{}",
                    request.session_id,
                    now_unix_nanos(),
                    NEXT_ROUTE_SPAN_ID.fetch_add(1, Ordering::Relaxed)
                ),
                session_id: request.session_id.clone(),
                agent_name: request.agent_name.clone(),
                kind,
                name: request.prompt_name.clone(),
                start_unix_ms: now,
                end_unix_ms: now,
                model: model.map(|model| model.name.clone()),
                tokens_prompt,
                tokens_completion,
                usd_cost,
                cache_status,
                retries: 0,
                evicted_sections,
                status: SpanStatus::Ok,
                attributes,
            })
            .await?;
        Ok(())
    }
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, RouteExecutorError> {
    let text = std::fs::read_to_string(path).map_err(|source| RouteExecutorError::Io {
        path: path.display().to_string(),
        source,
    })?;
    serde_json::from_str(&text).map_err(|source| RouteExecutorError::Json {
        path: path.display().to_string(),
        source,
    })
}

fn cache_response(response: Value) -> ExecutionResponse {
    ExecutionResponse {
        response,
        // Overwritten by `execute`'s `Cache` arm with `default_model` —
        // a cache entry alone doesn't know which model produced it.
        model: None,
        cache_hit: true,
        usd_cost: 0.0,
        usage: None,
    }
}

fn observed_usd_cost(usage: &Option<ModelUsage>, estimate: f64) -> f64 {
    usage
        .as_ref()
        .and_then(|usage| usage.usd_cost)
        .unwrap_or(estimate)
}

fn duration_mul(duration: Duration, multiplier: u128) -> Duration {
    let millis = duration.as_millis().saturating_mul(multiplier);
    Duration::from_millis(millis.min(u128::from(u64::MAX)) as u64)
}

fn duration_mul_f64(duration: Duration, multiplier: f64) -> Duration {
    Duration::from_secs_f64(duration.as_secs_f64() * multiplier)
}

fn deterministic_jitter(model: &RouteModel, prompt_name: &str, attempt: u32) -> f64 {
    let mut hasher = Sha256::new();
    hasher.update(model.provider.as_bytes());
    hasher.update([0]);
    hasher.update(model.name.as_bytes());
    hasher.update([0]);
    hasher.update(prompt_name.as_bytes());
    hasher.update([0]);
    hasher.update(attempt.to_le_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    let value = u64::from_le_bytes(bytes);
    value as f64 / u64::MAX as f64
}

fn error_class_name(class: &ModelErrorClass) -> &'static str {
    match class {
        ModelErrorClass::Transient { .. } => "transient",
        ModelErrorClass::Permanent => "permanent",
        ModelErrorClass::Ambiguous => "ambiguous",
    }
}

fn cosine_similarity(left: &[f32], right: &[f32]) -> Option<f64> {
    if left.is_empty() || left.len() != right.len() {
        return None;
    }
    let dot = left
        .iter()
        .zip(right)
        .map(|(left, right)| f64::from(*left) * f64::from(*right))
        .sum::<f64>();
    let left_norm = left
        .iter()
        .map(|value| f64::from(*value).powi(2))
        .sum::<f64>()
        .sqrt();
    let right_norm = right
        .iter()
        .map(|value| f64::from(*value).powi(2))
        .sum::<f64>()
        .sqrt();
    if left_norm == 0.0 || right_norm == 0.0 {
        None
    } else {
        Some((dot / (left_norm * right_norm)).clamp(-1.0, 1.0))
    }
}

fn now_unix_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn now_unix_nanos() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

pub fn cache_key(prompt_name: &str, inputs: &Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(prompt_name.as_bytes());
    hasher.update([0]);
    hasher.update(serde_json::to_vec(&canonicalize(inputs)).expect("JSON values always serialize"));
    format!("{:x}", hasher.finalize())
}

fn canonicalize(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let sorted = object
                .iter()
                .map(|(key, value)| (key.clone(), canonicalize(value)))
                .collect::<std::collections::BTreeMap<_, _>>();
            Value::Object(sorted.into_iter().collect())
        }
        Value::Array(values) => Value::Array(values.iter().map(canonicalize).collect()),
        _ => value.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::Arc;
    use std::sync::Mutex;

    use cybersin_ir::QualityTier;
    use cybersin_router::{
        CacheDecision, CascadeDecision, CascadeStep, ConfidenceRubric, FallbackDecision, ModelKind,
        PromptRoute, RouteDecision,
    };
    use cybersin_trace::{CacheStatus, SpanFilter, SpanKind};

    #[derive(Default)]
    struct Calls(Mutex<Vec<String>>);

    #[async_trait]
    impl ModelCaller for Calls {
        async fn call(
            &self,
            model: &RouteModel,
            _prompt_name: &str,
            _inputs: &Value,
            _confidence_instruction: Option<&str>,
            _grounded: bool,
        ) -> Result<ModelOutput, ModelCallError> {
            self.0.lock().unwrap().push(model.name.clone());
            Ok(ModelOutput {
                response: serde_json::json!({"from": model.name}),
                confidence: if model.name == "cheap" { 0.4 } else { 0.95 },
                usage: None,
            })
        }
    }

    struct UsageCalls;

    #[async_trait]
    impl ModelCaller for UsageCalls {
        async fn call(
            &self,
            model: &RouteModel,
            _prompt_name: &str,
            _inputs: &Value,
            _confidence_instruction: Option<&str>,
            _grounded: bool,
        ) -> Result<ModelOutput, ModelCallError> {
            Ok(ModelOutput {
                response: serde_json::json!({"from": model.name}),
                confidence: 0.95,
                usage: Some(ModelUsage {
                    prompt_tokens: Some(7),
                    completion_tokens: Some(11),
                    usd_cost: Some(0.00042),
                }),
            })
        }
    }

    #[derive(Debug, Clone)]
    enum ScriptedOutcome {
        Answer { confidence: f64 },
        Transient(&'static str),
        TransientRetryAfter(&'static str, Duration),
        Permanent(&'static str),
        Ambiguous(&'static str),
    }

    #[derive(Default)]
    struct ScriptedCalls {
        outcomes: Mutex<VecDeque<ScriptedOutcome>>,
        calls: Mutex<Vec<String>>,
    }

    impl ScriptedCalls {
        fn new(outcomes: impl IntoIterator<Item = ScriptedOutcome>) -> Self {
            Self {
                outcomes: Mutex::new(outcomes.into_iter().collect()),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl ModelCaller for ScriptedCalls {
        async fn call(
            &self,
            model: &RouteModel,
            _prompt_name: &str,
            _inputs: &Value,
            _confidence_instruction: Option<&str>,
            _grounded: bool,
        ) -> Result<ModelOutput, ModelCallError> {
            self.calls.lock().unwrap().push(model.name.clone());
            let outcome = self
                .outcomes
                .lock()
                .unwrap()
                .pop_front()
                .expect("scripted outcome");
            match outcome {
                ScriptedOutcome::Answer { confidence } => Ok(ModelOutput {
                    response: serde_json::json!({"from": model.name}),
                    confidence,
                    usage: None,
                }),
                ScriptedOutcome::Transient(message) => Err(ModelCallError::transient(message)),
                ScriptedOutcome::TransientRetryAfter(message, delay) => {
                    Err(ModelCallError::transient_with_retry_after(message, delay))
                }
                ScriptedOutcome::Permanent(message) => Err(ModelCallError::permanent(message)),
                ScriptedOutcome::Ambiguous(message) => Err(ModelCallError::ambiguous(message)),
            }
        }
    }

    #[derive(Default)]
    struct RecordingSleeper(Mutex<Vec<Duration>>);

    #[async_trait]
    impl AsyncSleeper for RecordingSleeper {
        async fn sleep(&self, delay: Duration) {
            self.0.lock().unwrap().push(delay);
        }
    }

    struct AcceptJudge;

    #[async_trait]
    impl Judge for AcceptJudge {
        async fn accepts(
            &self,
            _model: &RouteModel,
            _prompt_name: &str,
            _inputs: &Value,
            _cached_response: &Value,
            _similarity: f64,
        ) -> Result<bool, String> {
            Ok(true)
        }
    }

    #[derive(Default)]
    struct CountingJudge(AtomicUsize);

    #[async_trait]
    impl Judge for CountingJudge {
        async fn accepts(
            &self,
            _model: &RouteModel,
            _prompt_name: &str,
            _inputs: &Value,
            _cached_response: &Value,
            _similarity: f64,
        ) -> Result<bool, String> {
            self.0.fetch_add(1, AtomicOrdering::Relaxed);
            Ok(true)
        }
    }

    #[derive(Default)]
    struct FallbackCalls(Mutex<Vec<String>>);

    #[async_trait]
    impl ModelCaller for FallbackCalls {
        async fn call(
            &self,
            model: &RouteModel,
            _prompt_name: &str,
            _inputs: &Value,
            _confidence_instruction: Option<&str>,
            _grounded: bool,
        ) -> Result<ModelOutput, ModelCallError> {
            self.0.lock().unwrap().push(model.name.clone());
            if model.name == "backup" {
                Ok(ModelOutput {
                    response: serde_json::json!("fallback"),
                    confidence: 0.1,
                    usage: None,
                })
            } else {
                Err(ModelCallError::permanent("unavailable"))
            }
        }
    }

    /// Captures `(model_name, grounded)` per invocation — used to assert
    /// which calls the executor actually requested grounding on (issue
    /// #83), independent of `Calls`/`FallbackCalls`'s plain name log.
    /// Mirrors `Calls`'s accept/escalate confidences (`cheap` under
    /// threshold, everything else over).
    #[derive(Default)]
    struct GroundingCapturingCalls(Mutex<Vec<(String, bool)>>);

    #[async_trait]
    impl ModelCaller for GroundingCapturingCalls {
        async fn call(
            &self,
            model: &RouteModel,
            _prompt_name: &str,
            _inputs: &Value,
            _confidence_instruction: Option<&str>,
            grounded: bool,
        ) -> Result<ModelOutput, ModelCallError> {
            self.0.lock().unwrap().push((model.name.clone(), grounded));
            Ok(ModelOutput {
                response: serde_json::json!({"from": model.name}),
                confidence: if model.name == "cheap" { 0.4 } else { 0.95 },
                usage: None,
            })
        }
    }

    /// Same `(model_name, grounded)` capture as `GroundingCapturingCalls`,
    /// but fails every candidate except `"backup"` — mirrors
    /// `FallbackCalls`'s exhaust-the-cascade-then-fallback shape, so a
    /// grounded cascade step can be made to fail and fall through to the
    /// fallback tier while still recording what it was called with.
    #[derive(Default)]
    struct GroundingCapturingFallbackCalls(Mutex<Vec<(String, bool)>>);

    #[async_trait]
    impl ModelCaller for GroundingCapturingFallbackCalls {
        async fn call(
            &self,
            model: &RouteModel,
            _prompt_name: &str,
            _inputs: &Value,
            _confidence_instruction: Option<&str>,
            grounded: bool,
        ) -> Result<ModelOutput, ModelCallError> {
            self.0.lock().unwrap().push((model.name.clone(), grounded));
            if model.name == "backup" {
                Ok(ModelOutput {
                    response: serde_json::json!("fallback"),
                    confidence: 0.1,
                    usage: None,
                })
            } else {
                Err(ModelCallError::permanent("unavailable"))
            }
        }
    }

    fn model(name: &str, kind: ModelKind) -> RouteModel {
        RouteModel {
            name: name.into(),
            provider: "test".into(),
            quality: QualityTier::Medium,
            estimated_cost_usd: 0.01,
            model_kind: kind,
        }
    }

    fn routing() -> RoutingArtifact {
        RoutingArtifact {
            schema_version: 1,
            prompts: BTreeMap::from([(
                "answer".into(),
                PromptRoute {
                    quality: QualityTier::Medium,
                    decisions: vec![
                        RouteDecision::Cache(CacheDecision {
                            similarity_threshold: 0.95,
                            judge_trigger_band: [0.80, 0.95],
                            judge: model("judge", ModelKind::Judge),
                        }),
                        RouteDecision::Cascade(CascadeDecision {
                            steps: vec![
                                CascadeStep {
                                    model: model("cheap", ModelKind::Provider),
                                    confidence: ConfidenceRubric {
                                        minimum_score: 0.9,
                                        instruction: "score".into(),
                                    },
                                    grounded: false,
                                },
                                CascadeStep {
                                    model: model("strong", ModelKind::Provider),
                                    confidence: ConfidenceRubric {
                                        minimum_score: 0.9,
                                        instruction: "score".into(),
                                    },
                                    grounded: false,
                                },
                            ],
                        }),
                        RouteDecision::Fallbacks(FallbackDecision { providers: vec![] }),
                    ],
                    optimization_candidates: vec![],
                },
            )]),
        }
    }

    fn request() -> ExecutionRequest {
        ExecutionRequest {
            session_id: "s1".into(),
            agent_name: "agent".into(),
            prompt_name: "answer".into(),
            inputs: serde_json::json!({"question": "hello"}),
            embedding: vec![1.0, 0.0],
            namespace_version: "v1".into(),
            bypass: false,
            prompt_tokens: 0,
            completion_tokens: None,
            evicted_sections: Vec::new(),
            context_attributes: Value::Null,
            force_cheapest_cascade_step: false,
            default_model: None,
        }
    }

    #[tokio::test]
    async fn exact_hash_hit_returns_cache_and_records_why() {
        let req = request();
        let cached = serde_json::json!({"cached": true});
        let spans = SpanStore::in_memory().await.unwrap();
        let executor = RouteExecutor::new(
            routing(),
            CacheArtifact {
                schema_version: 1,
                namespace_version: "v1".into(),
                entries: vec![CacheEntry {
                    prompt_name: "answer".into(),
                    input_hash: cache_key("answer", &req.inputs),
                    embedding: vec![0.0, 1.0],
                    response: cached.clone(),
                }],
            },
            Calls::default(),
            AcceptJudge,
            spans.clone(),
        );

        let response = executor.execute(&req).await.unwrap();

        assert_eq!(response.response, cached);
        assert!(response.cache_hit);
        assert!(executor.models.0.lock().unwrap().is_empty());
        let recorded = spans
            .list(&SpanFilter {
                kind: Some(SpanKind::CacheDecision),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(recorded[0].cache_status, CacheStatus::Hit);
        assert_eq!(recorded[0].attributes["decision"], "hash_hit");
    }

    #[tokio::test]
    async fn bypass_then_cascade_escalates_on_low_confidence() {
        let mut req = request();
        req.bypass = true;
        let spans = SpanStore::in_memory().await.unwrap();
        let executor = RouteExecutor::new(
            routing(),
            CacheArtifact {
                schema_version: 1,
                namespace_version: "v1".into(),
                entries: vec![],
            },
            Calls::default(),
            AcceptJudge,
            spans.clone(),
        );

        let response = executor.execute(&req).await.unwrap();

        assert_eq!(response.model.as_deref(), Some("strong"));
        assert_eq!(
            *executor.models.0.lock().unwrap(),
            vec!["cheap".to_string(), "strong".to_string()]
        );
        let recorded = spans.list(&SpanFilter::default()).await.unwrap();
        assert!(recorded
            .iter()
            .any(|span| span.attributes["decision"] == "bypass"));
        assert!(recorded.iter().any(|span| {
            span.attributes["decision"] == "cascade_escalation"
                && span.attributes["model"] == "cheap"
        }));
    }

    #[tokio::test]
    async fn provider_usage_overrides_route_estimates_in_llm_span() {
        let mut req = request();
        req.bypass = true;
        req.prompt_tokens = 999;
        req.completion_tokens = Some(1_000);
        let spans = SpanStore::in_memory().await.unwrap();
        let executor = RouteExecutor::new(
            routing(),
            CacheArtifact {
                schema_version: 1,
                namespace_version: "v1".into(),
                entries: vec![],
            },
            UsageCalls,
            AcceptJudge,
            spans.clone(),
        );

        let response = executor.execute(&req).await.unwrap();

        assert_eq!(response.usd_cost, 0.00042);
        assert_eq!(
            response
                .usage
                .as_ref()
                .and_then(|usage| usage.prompt_tokens),
            Some(7)
        );
        let recorded = spans
            .list(&SpanFilter {
                kind: Some(SpanKind::LlmCall),
                ..Default::default()
            })
            .await
            .unwrap();
        let accepted = recorded
            .iter()
            .find(|span| span.attributes["decision"] == "cascade_accept")
            .expect("accepted llm span");
        assert_eq!(accepted.tokens_prompt, Some(7));
        assert_eq!(accepted.tokens_completion, Some(11));
        assert_eq!(accepted.usd_cost, 0.00042);
        assert_eq!(accepted.attributes["usage_source"], "provider");
    }

    #[tokio::test]
    async fn borderline_vector_hit_is_accepted_by_judge_and_traced() {
        let mut req = request();
        req.inputs = serde_json::json!({"question": "nearby"});
        let spans = SpanStore::in_memory().await.unwrap();
        let executor = RouteExecutor::new(
            routing(),
            CacheArtifact {
                schema_version: 1,
                namespace_version: "v1".into(),
                entries: vec![CacheEntry {
                    prompt_name: "answer".into(),
                    input_hash: "a-different-hash".into(),
                    embedding: vec![0.9, 0.4358899],
                    response: serde_json::json!("semantic"),
                }],
            },
            Calls::default(),
            CountingJudge::default(),
            spans.clone(),
        );

        let response = executor.execute(&req).await.unwrap();

        assert_eq!(response.response, "semantic");
        assert_eq!(executor.judge.0.load(AtomicOrdering::Relaxed), 1);
        let recorded = spans.list(&SpanFilter::default()).await.unwrap();
        let decision = recorded
            .iter()
            .find(|span| span.attributes["decision"] == "judge_hit")
            .expect("judge decision span");
        assert_eq!(decision.attributes["judge_outcome"], true);
        assert!(decision.attributes["similarity"].as_f64().unwrap() >= 0.8);
    }

    #[tokio::test]
    async fn namespace_change_invalidates_cache_before_routing() {
        let mut req = request();
        req.namespace_version = "v2".into();
        let spans = SpanStore::in_memory().await.unwrap();
        let executor = RouteExecutor::new(
            routing(),
            CacheArtifact {
                schema_version: 1,
                namespace_version: "v1".into(),
                entries: vec![CacheEntry {
                    prompt_name: "answer".into(),
                    input_hash: cache_key("answer", &req.inputs),
                    embedding: req.embedding.clone(),
                    response: serde_json::json!("stale"),
                }],
            },
            Calls::default(),
            AcceptJudge,
            spans.clone(),
        );

        let response = executor.execute(&req).await.unwrap();

        assert_eq!(response.model.as_deref(), Some("strong"));
        let recorded = spans.list(&SpanFilter::default()).await.unwrap();
        assert!(recorded
            .iter()
            .any(|span| span.attributes["decision"] == "namespace_invalidated"));
    }

    #[tokio::test]
    async fn provider_fallbacks_run_after_the_cascade_is_exhausted() {
        let mut artifact = routing();
        let route = artifact.prompts.get_mut("answer").unwrap();
        route.decisions[2] = RouteDecision::Fallbacks(FallbackDecision {
            providers: vec![model("backup", ModelKind::Provider)],
        });
        let mut req = request();
        req.bypass = true;
        let spans = SpanStore::in_memory().await.unwrap();
        let executor = RouteExecutor::new(
            artifact,
            CacheArtifact {
                schema_version: 1,
                namespace_version: "v1".into(),
                entries: vec![],
            },
            FallbackCalls::default(),
            AcceptJudge,
            spans.clone(),
        );

        let response = executor.execute(&req).await.unwrap();

        assert_eq!(response.response, "fallback");
        assert_eq!(response.model.as_deref(), Some("backup"));
        assert_eq!(
            *executor.models.0.lock().unwrap(),
            vec!["cheap", "strong", "backup"]
        );
        let recorded = spans.list(&SpanFilter::default()).await.unwrap();
        assert!(recorded
            .iter()
            .any(|span| span.attributes["decision"] == "fallback_accept"));
    }

    #[tokio::test]
    async fn a_grounded_cascade_step_is_called_with_grounded_true_and_tags_its_span() {
        // Issue #83: the executor reads `CascadeStep::grounded` (issue #81)
        // and threads it through both to the model caller and onto the
        // recorded span's attributes.
        let mut artifact = routing();
        let route = artifact.prompts.get_mut("answer").unwrap();
        let RouteDecision::Cascade(cascade) = &mut route.decisions[1] else {
            panic!("expected a cascade decision");
        };
        cascade.steps[0].grounded = true; // "cheap" -- under threshold, escalates
        cascade.steps[1].grounded = false; // "strong" -- accepted

        let mut req = request();
        req.bypass = true;
        let spans = SpanStore::in_memory().await.unwrap();
        let executor = RouteExecutor::new(
            artifact,
            CacheArtifact {
                schema_version: 1,
                namespace_version: "v1".into(),
                entries: vec![],
            },
            GroundingCapturingCalls::default(),
            AcceptJudge,
            spans.clone(),
        );

        let response = executor.execute(&req).await.unwrap();

        assert_eq!(response.model.as_deref(), Some("strong"));
        assert_eq!(
            *executor.models.0.lock().unwrap(),
            vec![("cheap".to_string(), true), ("strong".to_string(), false)]
        );

        let recorded = spans.list(&SpanFilter::default()).await.unwrap();
        let escalation = recorded
            .iter()
            .find(|span| span.attributes["decision"] == "cascade_escalation")
            .expect("cascade_escalation span");
        assert_eq!(escalation.attributes["model"], "cheap");
        assert_eq!(escalation.attributes["grounded"], true);

        let accept = recorded
            .iter()
            .find(|span| span.attributes["decision"] == "cascade_accept")
            .expect("cascade_accept span");
        assert_eq!(accept.attributes["model"], "strong");
        assert_eq!(accept.attributes["grounded"], false);
    }

    #[tokio::test]
    async fn fallback_calls_are_never_grounded_even_after_a_grounded_cascade_step_fails() {
        // Issue #83 acceptance criterion: fallback-tier calls must never
        // carry the grounding marker, regardless of which cascade step
        // failed first -- confirmed with a real test rather than just
        // relying on `RouteModel` having no `grounded` field to set.
        let mut artifact = routing();
        let route = artifact.prompts.get_mut("answer").unwrap();
        let RouteDecision::Cascade(cascade) = &mut route.decisions[1] else {
            panic!("expected a cascade decision");
        };
        cascade.steps[0].grounded = true; // "cheap" -- grounded, and fails
        route.decisions[2] = RouteDecision::Fallbacks(FallbackDecision {
            providers: vec![model("backup", ModelKind::Provider)],
        });

        let mut req = request();
        req.bypass = true;
        let spans = SpanStore::in_memory().await.unwrap();
        let executor = RouteExecutor::new(
            artifact,
            CacheArtifact {
                schema_version: 1,
                namespace_version: "v1".into(),
                entries: vec![],
            },
            GroundingCapturingFallbackCalls::default(),
            AcceptJudge,
            spans.clone(),
        );

        let response = executor.execute(&req).await.unwrap();

        assert_eq!(response.response, "fallback");
        assert_eq!(response.model.as_deref(), Some("backup"));
        assert_eq!(
            *executor.models.0.lock().unwrap(),
            vec![
                ("cheap".to_string(), true),
                ("strong".to_string(), false),
                ("backup".to_string(), false),
            ]
        );

        let recorded = spans.list(&SpanFilter::default()).await.unwrap();
        let fallback_accept = recorded
            .iter()
            .find(|span| span.attributes["decision"] == "fallback_accept")
            .expect("fallback_accept span");
        assert_eq!(fallback_accept.attributes["model"], "backup");
        assert_eq!(fallback_accept.attributes["grounded"], false);

        let cascade_error = recorded
            .iter()
            .find(|span| {
                span.attributes["decision"] == "cascade_error"
                    && span.attributes["model"] == "cheap"
            })
            .expect("cascade_error span for the grounded, failing step");
        assert_eq!(cascade_error.attributes["grounded"], true);
    }

    #[tokio::test]
    async fn allowlist_skips_disallowed_cascade_steps_without_calling_them() {
        let mut req = request();
        req.bypass = true;
        let spans = SpanStore::in_memory().await.unwrap();
        let executor = RouteExecutor::new(
            routing(),
            CacheArtifact {
                schema_version: 1,
                namespace_version: "v1".into(),
                entries: vec![],
            },
            Calls::default(),
            AcceptJudge,
            spans,
        )
        .with_allowlist(crate::allowlist::ModelAllowlist::new(
            vec!["test".into()],
            BTreeMap::from([("test".to_string(), vec!["strong".to_string()])]),
        ));

        let response = executor.execute(&req).await.unwrap();

        assert_eq!(response.model.as_deref(), Some("strong"));
        assert_eq!(*executor.models.0.lock().unwrap(), vec!["strong"]);
    }

    #[tokio::test]
    async fn allowlist_exhausting_every_candidate_reports_exhausted_not_a_call_error() {
        let mut req = request();
        req.bypass = true;
        let spans = SpanStore::in_memory().await.unwrap();
        let executor = RouteExecutor::new(
            routing(),
            CacheArtifact {
                schema_version: 1,
                namespace_version: "v1".into(),
                entries: vec![],
            },
            Calls::default(),
            AcceptJudge,
            spans,
        )
        .with_allowlist(crate::allowlist::ModelAllowlist::new(
            vec!["anthropic".into()],
            BTreeMap::new(),
        ));

        let error = executor.execute(&req).await.unwrap_err();

        assert!(matches!(error, RouteExecutorError::Exhausted(prompt) if prompt == "answer"));
        assert!(executor.models.0.lock().unwrap().is_empty());
    }

    #[test]
    fn retry_policy_grows_with_full_jitter_bounds_and_cap() {
        let policy = RouteRetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_millis(250),
        };
        let class = ModelErrorClass::transient();

        assert_eq!(
            policy.delay_for(1, &class, 0.0),
            Some(Duration::from_millis(0))
        );
        assert_eq!(
            policy.delay_for(1, &class, 1.0),
            Some(Duration::from_millis(100))
        );
        assert_eq!(
            policy.delay_for(2, &class, 1.0),
            Some(Duration::from_millis(200))
        );
        assert_eq!(
            policy.delay_for(3, &class, 1.0),
            Some(Duration::from_millis(250))
        );
        assert_eq!(
            policy.delay_for(3, &class, 0.5),
            Some(Duration::from_millis(125))
        );
    }

    #[test]
    fn retry_policy_uses_retry_after_exactly_but_still_caps() {
        let policy = RouteRetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(2),
        };

        assert_eq!(
            policy.delay_for(
                1,
                &ModelErrorClass::transient_with_retry_after(Duration::from_millis(750)),
                0.0
            ),
            Some(Duration::from_millis(750))
        );
        assert_eq!(
            policy.delay_for(
                1,
                &ModelErrorClass::transient_with_retry_after(Duration::from_secs(10)),
                0.0
            ),
            Some(Duration::from_secs(2))
        );
    }

    #[test]
    fn retry_policy_never_retries_non_transient_errors() {
        let policy = RouteRetryPolicy::default();

        assert_eq!(policy.delay_for(1, &ModelErrorClass::Permanent, 1.0), None);
        assert_eq!(policy.delay_for(1, &ModelErrorClass::Ambiguous, 1.0), None);
    }

    #[tokio::test]
    async fn transient_cascade_failure_retries_same_model_before_escalating() {
        let mut req = request();
        req.bypass = true;
        let sleeper = Arc::new(RecordingSleeper::default());
        let spans = SpanStore::in_memory().await.unwrap();
        let executor = RouteExecutor::new(
            routing(),
            CacheArtifact {
                schema_version: 1,
                namespace_version: "v1".into(),
                entries: vec![],
            },
            ScriptedCalls::new([
                ScriptedOutcome::Transient("rate limited"),
                ScriptedOutcome::Transient("timeout"),
                ScriptedOutcome::Answer { confidence: 0.95 },
            ]),
            AcceptJudge,
            spans,
        )
        .with_retry_policy(RouteRetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(10),
            max_delay: Duration::from_millis(100),
        })
        .with_sleeper(sleeper.clone());

        let response = executor.execute(&req).await.unwrap();

        assert_eq!(response.model.as_deref(), Some("cheap"));
        assert_eq!(
            *executor.models.calls.lock().unwrap(),
            vec!["cheap", "cheap", "cheap"]
        );
        assert_eq!(sleeper.0.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn permanent_errors_do_not_retry() {
        let mut req = request();
        req.bypass = true;
        let sleeper = Arc::new(RecordingSleeper::default());
        let spans = SpanStore::in_memory().await.unwrap();
        let executor = RouteExecutor::new(
            routing(),
            CacheArtifact {
                schema_version: 1,
                namespace_version: "v1".into(),
                entries: vec![],
            },
            ScriptedCalls::new([
                ScriptedOutcome::Permanent("bad request"),
                ScriptedOutcome::Answer { confidence: 0.95 },
            ]),
            AcceptJudge,
            spans,
        )
        .with_sleeper(sleeper.clone());

        let response = executor.execute(&req).await.unwrap();

        assert_eq!(response.model.as_deref(), Some("strong"));
        assert_eq!(
            *executor.models.calls.lock().unwrap(),
            vec!["cheap", "strong"]
        );
        assert!(sleeper.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn low_confidence_cascade_outcome_does_not_retry() {
        let mut req = request();
        req.bypass = true;
        let sleeper = Arc::new(RecordingSleeper::default());
        let spans = SpanStore::in_memory().await.unwrap();
        let executor = RouteExecutor::new(
            routing(),
            CacheArtifact {
                schema_version: 1,
                namespace_version: "v1".into(),
                entries: vec![],
            },
            ScriptedCalls::new([
                ScriptedOutcome::Answer { confidence: 0.4 },
                ScriptedOutcome::Answer { confidence: 0.95 },
            ]),
            AcceptJudge,
            spans,
        )
        .with_sleeper(sleeper.clone());

        let response = executor.execute(&req).await.unwrap();

        assert_eq!(response.model.as_deref(), Some("strong"));
        assert_eq!(
            *executor.models.calls.lock().unwrap(),
            vec!["cheap", "strong"]
        );
        assert!(sleeper.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn fallback_waits_until_cascade_retry_budget_is_exhausted() {
        let mut artifact = routing();
        let route = artifact.prompts.get_mut("answer").unwrap();
        let RouteDecision::Cascade(cascade) = &mut route.decisions[1] else {
            panic!("expected cascade");
        };
        cascade.steps.truncate(1);
        route.decisions[2] = RouteDecision::Fallbacks(FallbackDecision {
            providers: vec![model("backup", ModelKind::Provider)],
        });

        let mut req = request();
        req.bypass = true;
        let sleeper = Arc::new(RecordingSleeper::default());
        let spans = SpanStore::in_memory().await.unwrap();
        let executor = RouteExecutor::new(
            artifact,
            CacheArtifact {
                schema_version: 1,
                namespace_version: "v1".into(),
                entries: vec![],
            },
            ScriptedCalls::new([
                ScriptedOutcome::Transient("first"),
                ScriptedOutcome::TransientRetryAfter("second", Duration::from_millis(25)),
                ScriptedOutcome::Transient("third"),
                ScriptedOutcome::Answer { confidence: 0.1 },
            ]),
            AcceptJudge,
            spans,
        )
        .with_retry_policy(RouteRetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(10),
            max_delay: Duration::from_millis(50),
        })
        .with_sleeper(sleeper.clone());

        let response = executor.execute(&req).await.unwrap();

        assert_eq!(response.model.as_deref(), Some("backup"));
        assert_eq!(
            *executor.models.calls.lock().unwrap(),
            vec!["cheap", "cheap", "cheap", "backup"]
        );
        assert_eq!(sleeper.0.lock().unwrap()[1], Duration::from_millis(25));
    }

    #[tokio::test]
    async fn ambiguous_model_outcome_is_not_retried_and_surfaces_for_recovery() {
        let mut artifact = routing();
        let route = artifact.prompts.get_mut("answer").unwrap();
        let RouteDecision::Cascade(cascade) = &mut route.decisions[1] else {
            panic!("expected cascade");
        };
        cascade.steps.truncate(1);
        route.decisions[2] = RouteDecision::Fallbacks(FallbackDecision { providers: vec![] });

        let mut req = request();
        req.bypass = true;
        let sleeper = Arc::new(RecordingSleeper::default());
        let spans = SpanStore::in_memory().await.unwrap();
        let executor = RouteExecutor::new(
            artifact,
            CacheArtifact {
                schema_version: 1,
                namespace_version: "v1".into(),
                entries: vec![],
            },
            ScriptedCalls::new([ScriptedOutcome::Ambiguous(
                "response may have been persisted",
            )]),
            AcceptJudge,
            spans,
        )
        .with_sleeper(sleeper.clone());

        let error = executor.execute(&req).await.unwrap_err();

        assert!(matches!(
            error,
            RouteExecutorError::ExhaustedWithLastErrors {
                prompt,
                last_errors
            } if prompt == "answer"
                && last_errors.contains("cheap: response may have been persisted")
        ));
        assert_eq!(*executor.models.calls.lock().unwrap(), vec!["cheap"]);
        assert!(sleeper.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn exhausted_errors_name_the_last_failure_per_tier() {
        let mut artifact = routing();
        let route = artifact.prompts.get_mut("answer").unwrap();
        let RouteDecision::Cascade(cascade) = &mut route.decisions[1] else {
            panic!("expected cascade");
        };
        cascade.steps.truncate(1);
        route.decisions[2] = RouteDecision::Fallbacks(FallbackDecision {
            providers: vec![model("backup", ModelKind::Provider)],
        });

        let mut req = request();
        req.bypass = true;
        let spans = SpanStore::in_memory().await.unwrap();
        let executor = RouteExecutor::new(
            artifact,
            CacheArtifact {
                schema_version: 1,
                namespace_version: "v1".into(),
                entries: vec![],
            },
            ScriptedCalls::new([
                ScriptedOutcome::Permanent("cascade stopped"),
                ScriptedOutcome::Permanent("fallback stopped"),
            ]),
            AcceptJudge,
            spans,
        );

        let error = executor.execute(&req).await.unwrap_err();

        assert!(matches!(
            error,
            RouteExecutorError::ExhaustedWithLastErrors {
                prompt,
                last_errors
            } if prompt == "answer"
                && last_errors.contains("cascade=cheap: cascade stopped")
                && last_errors.contains("fallback=backup: fallback stopped")
        ));
    }

    #[test]
    fn sqlite_vec_gate_fails_closed_to_brute_force() {
        assert_eq!(
            SQLITE_VEC_EVALUATION.selected_backend,
            KnnBackend::BruteForce
        );
    }

    #[test]
    fn brute_force_cache_supports_incremental_upsert() {
        let mut cache = CacheArtifact {
            schema_version: 1,
            namespace_version: "v1".into(),
            entries: vec![],
        };
        cache.upsert(CacheEntry {
            prompt_name: "answer".into(),
            input_hash: "one".into(),
            embedding: vec![1.0, 0.0],
            response: serde_json::json!("first"),
        });
        cache.upsert(CacheEntry {
            prompt_name: "answer".into(),
            input_hash: "one".into(),
            embedding: vec![0.0, 1.0],
            response: serde_json::json!("updated"),
        });

        assert_eq!(cache.entries.len(), 1);
        assert_eq!(cache.entries[0].response, "updated");
    }

    #[test]
    fn cache_hash_is_independent_of_json_object_insertion_order() {
        let mut left = serde_json::Map::new();
        left.insert("b".into(), serde_json::json!({"d": 4, "c": 3}));
        left.insert("a".into(), serde_json::json!(1));
        let mut right = serde_json::Map::new();
        right.insert("a".into(), serde_json::json!(1));
        right.insert("b".into(), serde_json::json!({"c": 3, "d": 4}));

        assert_eq!(
            cache_key("answer", &Value::Object(left)),
            cache_key("answer", &Value::Object(right))
        );
    }
}
