//! Stub [`ModelCaller`]/[`Judge`] implementations `RuntimeDaemon` defaults
//! to (spec §8.3), plus the `Box<dyn ModelCaller>` forwarding impl that
//! lets it swap the stub for a real one (`crate::live_model_caller`, issue
//! #35 Phase 1) via `RuntimeDaemon::with_models` without threading a
//! second generic parameter through every one of its methods.
//! `StubModelCaller` fabricates a deterministic response/confidence
//! instead of calling a real model — still used wherever no live
//! `ModelCaller` has been attached (every caller that predates issue #35,
//! and this crate's own tests).
//!
//! Confidence is derived from `RouteModel::quality`, mirroring the same
//! self-assessment a real model's judged output would plausibly score
//! (`cybersin-router`'s own `confidence_rubric` table sets cascade
//! thresholds on exactly this assumption: a low-tier model rarely clears a
//! high bar, a high-tier model almost always clears its own). This is what
//! lets a real cascade genuinely escalate through cheaper tiers instead of
//! always settling on one hardcoded model.

use async_trait::async_trait;
use cybersin_ir::QualityTier;
use cybersin_router::RouteModel;
use serde_json::Value;

use crate::route_executor::{Judge, ModelCaller, ModelOutput};

/// Fixed self-confidence per quality tier. Tuned against the compiler's own
/// `confidence_rubric` thresholds (`cybersin-router::confidence_rubric`) so
/// a tier only ever "passes" a cascade step whose bar it should plausibly
/// clear: Low never clears any step's 0.9 bar, Medium clears its own
/// prompt's medium-target bar (0.78) but not a high-target 0.9 bar, High
/// clears every bar it's offered (up to 0.82).
fn stub_confidence(quality: QualityTier) -> f64 {
    match quality {
        QualityTier::Low => 0.75,
        QualityTier::Medium => 0.85,
        QualityTier::High => 0.95,
    }
}

pub struct StubModelCaller;

#[async_trait]
impl ModelCaller for StubModelCaller {
    async fn call(
        &self,
        model: &RouteModel,
        prompt_name: &str,
        _inputs: &Value,
    ) -> Result<ModelOutput, String> {
        Ok(ModelOutput {
            response: serde_json::json!({
                "text": format!("stub completion for prompt `{prompt_name}`"),
                "model": model.name,
            }),
            confidence: stub_confidence(model.quality),
        })
    }
}

/// No real embedding backend exists yet (spec's brute-force kNN gate,
/// `SQLITE_VEC_EVALUATION`, already fails closed), so borderline
/// judge-tier cache hits are unreachable in practice today — this just
/// satisfies `RouteExecutor`'s trait bound with the same "always accept"
/// behavior the crate's own tests use for a non-adversarial judge stub.
pub struct StubJudge;

#[async_trait]
impl Judge for StubJudge {
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

/// Forwarding impl so `RouteExecutor<Box<dyn ModelCaller>, _>` can hold
/// any live `ModelCaller` behind one concrete field type — lets
/// `RuntimeDaemon` swap `StubModelCaller` for a real implementation
/// (`crate::live_model_caller::OpenRouterModelCaller`) via a builder
/// method without threading a second generic parameter through every one
/// of its methods.
#[async_trait]
impl ModelCaller for Box<dyn ModelCaller> {
    async fn call(
        &self,
        model: &RouteModel,
        prompt_name: &str,
        inputs: &Value,
    ) -> Result<ModelOutput, String> {
        (**self).call(model, prompt_name, inputs).await
    }
}
