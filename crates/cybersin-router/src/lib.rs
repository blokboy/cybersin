//! Compile-time cost minimization and `routing.json` emission (spec §6.3).
//!
//! The router consumes optimized prompt IR, declared project defaults, and
//! lockfile model prices. Its output is deliberately deterministic and
//! diff-friendly: prompts and model candidates are held in `BTreeMap`s and
//! every prompt gets the same cache → cascade → fallback decision order.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::Path;

use cybersin_ir::{PromptIr, QualityTier};
use serde::{Deserialize, Serialize};

/// Cost assigned to cache and judge pseudo-models during optimization.
///
/// It is non-zero so callers that divide by cost do not encounter a special
/// case, while remaining negligible beside billed provider calls.
pub const PSEUDO_MODEL_COST_USD: f64 = 0.000_000_001;

/// The project config subset used by the router.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectConfig {
    pub cost_model: CostModelConfig,
}

/// Declared cold-start values. Observed values, when present, override only
/// the corresponding declared value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostModelConfig {
    pub cache_similarity_threshold: f64,
    pub judge_trigger_band: [f64; 2],
    #[serde(default = "default_judge_model")]
    pub judge_model: String,
}

fn default_judge_model() -> String {
    "cache-judge".to_owned()
}

/// Trace-derived routing statistics. `None` represents a first build.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ObservedRoutingStats {
    pub cache_similarity_threshold: Option<f64>,
    pub judge_trigger_band: Option<[f64; 2]>,
}

/// Lockfile subset required for route optimization.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelLock {
    #[serde(default)]
    pub models: BTreeMap<String, LockedModel>,
    #[serde(default)]
    pub prices: BTreeMap<String, ModelPrice>,
}

/// Pinned model metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LockedModel {
    pub provider: String,
    pub quality: QualityTier,
    /// Provider/model pins to try after this model is unavailable.
    #[serde(default)]
    pub fallbacks: Vec<String>,
}

/// Pinned token prices in USD.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ModelPrice {
    pub usd_per_1k_prompt_tokens: f64,
    pub usd_per_1k_completion_tokens: f64,
}

impl ModelPrice {
    fn estimated_call_cost(self, workload: WorkloadEstimate) -> f64 {
        workload.prompt_tokens as f64 / 1000.0 * self.usd_per_1k_prompt_tokens
            + workload.completion_tokens as f64 / 1000.0 * self.usd_per_1k_completion_tokens
    }
}

/// Token assumptions used only to compare pinned prices at compile time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkloadEstimate {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
}

impl Default for WorkloadEstimate {
    fn default() -> Self {
        Self {
            prompt_tokens: 1_000,
            completion_tokens: 500,
        }
    }
}

/// Complete diff-friendly `routing.json` document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutingArtifact {
    pub schema_version: u32,
    pub prompts: BTreeMap<String, PromptRoute>,
}

/// One prompt's ordered runtime decision list and optimization evidence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromptRoute {
    pub quality: QualityTier,
    pub decisions: Vec<RouteDecision>,
    /// All optimizer candidates, including the nearly-free pseudo-models.
    pub optimization_candidates: Vec<OptimizationCandidate>,
}

/// Runtime order is encoded by the vector rather than implied by object keys.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RouteDecision {
    Cache(CacheDecision),
    Cascade(CascadeDecision),
    Fallbacks(FallbackDecision),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheDecision {
    pub similarity_threshold: f64,
    pub judge_trigger_band: [f64; 2],
    pub judge: RouteModel,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CascadeDecision {
    pub steps: Vec<CascadeStep>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CascadeStep {
    pub model: RouteModel,
    pub confidence: ConfidenceRubric,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfidenceRubric {
    pub minimum_score: f64,
    pub instruction: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FallbackDecision {
    pub providers: Vec<RouteModel>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteModel {
    pub name: String,
    pub provider: String,
    pub quality: QualityTier,
    pub estimated_cost_usd: f64,
    pub model_kind: ModelKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelKind {
    Provider,
    Cache,
    Judge,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OptimizationCandidate {
    #[serde(flatten)]
    pub model: RouteModel,
    pub selected_for_cascade: bool,
}

#[derive(Debug)]
pub enum RouterError {
    Config(String),
    Lock(String),
    Io(std::io::Error),
    Json(serde_json::Error),
    Yaml(serde_yaml::Error),
}

impl fmt::Display for RouterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(message) => write!(f, "invalid router config: {message}"),
            Self::Lock(message) => write!(f, "invalid model lock: {message}"),
            Self::Io(error) => write!(f, "failed to emit routing artifact: {error}"),
            Self::Json(error) => write!(f, "failed to serialize routing artifact: {error}"),
            Self::Yaml(error) => write!(f, "failed to parse router input: {error}"),
        }
    }
}

impl std::error::Error for RouterError {}

impl From<std::io::Error> for RouterError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for RouterError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

impl From<serde_yaml::Error> for RouterError {
    fn from(value: serde_yaml::Error) -> Self {
        Self::Yaml(value)
    }
}

/// Parse the relevant portions of `cybersin.yaml` and `cybersin.lock`, then
/// compile routes. Unknown fields are intentionally ignored.
pub fn compile_from_yaml(
    prompts: &[PromptIr],
    project_yaml: &str,
    lock_yaml: &str,
    observed: Option<&ObservedRoutingStats>,
    workload: WorkloadEstimate,
) -> Result<RoutingArtifact, RouterError> {
    let project: ProjectConfig = serde_yaml::from_str(project_yaml)?;
    let lock: ModelLock = serde_yaml::from_str(lock_yaml)?;
    compile(prompts, &project.cost_model, &lock, observed, workload)
}

/// Build deterministic routes for all prompts.
pub fn compile(
    prompts: &[PromptIr],
    config: &CostModelConfig,
    lock: &ModelLock,
    observed: Option<&ObservedRoutingStats>,
    workload: WorkloadEstimate,
) -> Result<RoutingArtifact, RouterError> {
    validate_config(config)?;
    validate_lock(lock)?;

    let similarity_threshold = observed
        .and_then(|stats| stats.cache_similarity_threshold)
        .unwrap_or(config.cache_similarity_threshold);
    let judge_trigger_band = observed
        .and_then(|stats| stats.judge_trigger_band)
        .unwrap_or(config.judge_trigger_band);
    validate_thresholds(similarity_threshold, judge_trigger_band)?;

    let provider_models = priced_models(lock, workload);
    let mut routes = BTreeMap::new();
    for prompt in prompts {
        let route = compile_prompt(
            prompt,
            config,
            lock,
            &provider_models,
            similarity_threshold,
            judge_trigger_band,
        )?;
        if routes.insert(prompt.name.clone(), route).is_some() {
            return Err(RouterError::Config(format!(
                "duplicate prompt name {:?}",
                prompt.name
            )));
        }
    }

    Ok(RoutingArtifact {
        schema_version: 1,
        prompts: routes,
    })
}

/// Write pretty, newline-terminated `routing.json`.
pub fn emit_routing_json(path: &Path, artifact: &RoutingArtifact) -> Result<(), RouterError> {
    let mut bytes = serde_json::to_vec_pretty(artifact)?;
    bytes.push(b'\n');
    fs::write(path, bytes)?;
    Ok(())
}

fn compile_prompt(
    prompt: &PromptIr,
    config: &CostModelConfig,
    lock: &ModelLock,
    provider_models: &[RouteModel],
    similarity_threshold: f64,
    judge_trigger_band: [f64; 2],
) -> Result<PromptRoute, RouterError> {
    let eligible: Vec<RouteModel> = provider_models
        .iter()
        .filter(|model| model.quality <= prompt.quality)
        .cloned()
        .collect();

    let mut steps = Vec::new();
    for &quality in tiers_through(prompt.quality) {
        if let Some(model) = eligible
            .iter()
            .filter(|model| model.quality == quality)
            .min_by(|left, right| compare_cost_then_name(left, right))
        {
            steps.push(CascadeStep {
                model: model.clone(),
                confidence: confidence_rubric(quality, prompt.quality),
            });
        }
    }
    if !steps
        .iter()
        .any(|step| step.model.quality == prompt.quality)
    {
        return Err(RouterError::Lock(format!(
            "prompt {:?} requests {:?} quality, but no priced model provides it",
            prompt.name, prompt.quality
        )));
    }

    let selected: BTreeSet<&str> = steps.iter().map(|step| step.model.name.as_str()).collect();
    let fallback_names: Vec<&str> = steps
        .iter()
        .flat_map(|step| {
            lock.models
                .get(&step.model.name)
                .into_iter()
                .flat_map(|model| model.fallbacks.iter().map(String::as_str))
        })
        .filter(|name| !selected.contains(name))
        .collect();
    let mut seen = BTreeSet::new();
    let fallbacks = fallback_names
        .into_iter()
        .filter(|name| seen.insert(*name))
        .map(|name| {
            provider_models
                .iter()
                .find(|model| model.name == name)
                .cloned()
                .ok_or_else(|| {
                    RouterError::Lock(format!(
                        "model fallback {name:?} has no model metadata and price"
                    ))
                })
        })
        .collect::<Result<Vec<_>, _>>()?;

    let cache_model = pseudo_model("semantic-cache", ModelKind::Cache, QualityTier::Low);
    let judge_model = pseudo_model(&config.judge_model, ModelKind::Judge, prompt.quality);
    let mut candidates = vec![
        OptimizationCandidate {
            model: cache_model,
            selected_for_cascade: false,
        },
        OptimizationCandidate {
            model: judge_model.clone(),
            selected_for_cascade: false,
        },
    ];
    candidates.extend(provider_models.iter().cloned().map(|model| {
        let selected_for_cascade = selected.contains(model.name.as_str());
        OptimizationCandidate {
            model,
            selected_for_cascade,
        }
    }));
    candidates.sort_by(|left, right| compare_cost_then_name(&left.model, &right.model));

    Ok(PromptRoute {
        quality: prompt.quality,
        decisions: vec![
            RouteDecision::Cache(CacheDecision {
                similarity_threshold,
                judge_trigger_band,
                judge: judge_model,
            }),
            RouteDecision::Cascade(CascadeDecision { steps }),
            RouteDecision::Fallbacks(FallbackDecision {
                providers: fallbacks,
            }),
        ],
        optimization_candidates: candidates,
    })
}

fn priced_models(lock: &ModelLock, workload: WorkloadEstimate) -> Vec<RouteModel> {
    let mut models: Vec<_> = lock
        .models
        .iter()
        .filter_map(|(name, model)| {
            lock.prices.get(name).map(|price| RouteModel {
                name: name.clone(),
                provider: model.provider.clone(),
                quality: model.quality,
                estimated_cost_usd: price.estimated_call_cost(workload),
                model_kind: ModelKind::Provider,
            })
        })
        .collect();
    models.sort_by(compare_cost_then_name);
    models
}

fn pseudo_model(name: &str, model_kind: ModelKind, quality: QualityTier) -> RouteModel {
    RouteModel {
        name: name.to_owned(),
        provider: "internal".to_owned(),
        quality,
        estimated_cost_usd: PSEUDO_MODEL_COST_USD,
        model_kind,
    }
}

fn compare_cost_then_name(left: &RouteModel, right: &RouteModel) -> Ordering {
    left.estimated_cost_usd
        .total_cmp(&right.estimated_cost_usd)
        .then_with(|| left.name.cmp(&right.name))
}

fn tiers_through(maximum: QualityTier) -> &'static [QualityTier] {
    match maximum {
        QualityTier::Low => &[QualityTier::Low],
        QualityTier::Medium => &[QualityTier::Low, QualityTier::Medium],
        QualityTier::High => &[QualityTier::Low, QualityTier::Medium, QualityTier::High],
    }
}

fn confidence_rubric(step: QualityTier, target: QualityTier) -> ConfidenceRubric {
    let minimum_score = match (step, target) {
        (QualityTier::Low, QualityTier::Low) => 0.72,
        (QualityTier::Low, _) => 0.90,
        (QualityTier::Medium, QualityTier::Medium) => 0.78,
        (QualityTier::Medium, QualityTier::High) => 0.90,
        (QualityTier::High, QualityTier::High) => 0.82,
        _ => 1.0,
    };
    ConfidenceRubric {
        minimum_score,
        instruction: format!(
            "Score 0..1 whether the response satisfies the {:?} quality contract; accept at or above {minimum_score:.2}.",
            target
        ),
    }
}

fn validate_config(config: &CostModelConfig) -> Result<(), RouterError> {
    if config.judge_model.trim().is_empty() {
        return Err(RouterError::Config(
            "judge_model must not be empty".to_owned(),
        ));
    }
    validate_thresholds(config.cache_similarity_threshold, config.judge_trigger_band)
}

fn validate_thresholds(
    similarity_threshold: f64,
    judge_trigger_band: [f64; 2],
) -> Result<(), RouterError> {
    if !(0.0..=1.0).contains(&similarity_threshold) {
        return Err(RouterError::Config(
            "cache_similarity_threshold must be between 0 and 1".to_owned(),
        ));
    }
    if !(0.0..=1.0).contains(&judge_trigger_band[0])
        || !(0.0..=1.0).contains(&judge_trigger_band[1])
        || judge_trigger_band[0] > judge_trigger_band[1]
        || judge_trigger_band[1] > similarity_threshold
    {
        return Err(RouterError::Config(
            "judge_trigger_band must be ordered, between 0 and 1, and end at or below the cache threshold"
                .to_owned(),
        ));
    }
    Ok(())
}

fn validate_lock(lock: &ModelLock) -> Result<(), RouterError> {
    for (name, model) in &lock.models {
        if model.provider.trim().is_empty() {
            return Err(RouterError::Lock(format!(
                "model {name:?} has an empty provider"
            )));
        }
        let price = lock
            .prices
            .get(name)
            .ok_or_else(|| RouterError::Lock(format!("model {name:?} has no pinned price")))?;
        if !price.usd_per_1k_prompt_tokens.is_finite()
            || !price.usd_per_1k_completion_tokens.is_finite()
            || price.usd_per_1k_prompt_tokens < 0.0
            || price.usd_per_1k_completion_tokens < 0.0
        {
            return Err(RouterError::Lock(format!(
                "model {name:?} has an invalid pinned price"
            )));
        }
    }
    for name in lock.prices.keys() {
        if !lock.models.contains_key(name) {
            return Err(RouterError::Lock(format!(
                "price {name:?} has no model metadata"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn prompt(name: &str, quality: QualityTier) -> PromptIr {
        PromptIr::new(name, quality, BTreeMap::new(), vec![], vec![], None)
    }

    fn project_yaml() -> &'static str {
        r#"
name: test
cost_model:
  cache_similarity_threshold: 0.97
  judge_trigger_band: [0.90, 0.97]
  judge_model: route-judge
"#
    }

    fn lock_yaml() -> &'static str {
        r#"
models:
  cheap-low:
    provider: alpha
    quality: low
    fallbacks: [backup-low]
  backup-low:
    provider: beta
    quality: low
  pricey-medium:
    provider: alpha
    quality: medium
    fallbacks: [backup-medium]
  backup-medium:
    provider: beta
    quality: medium
  premium-high:
    provider: alpha
    quality: high
    fallbacks: [backup-high]
  backup-high:
    provider: beta
    quality: high
prices:
  cheap-low:
    usd_per_1k_prompt_tokens: 0.10
    usd_per_1k_completion_tokens: 0.20
  backup-low:
    usd_per_1k_prompt_tokens: 0.20
    usd_per_1k_completion_tokens: 0.30
  pricey-medium:
    usd_per_1k_prompt_tokens: 1.00
    usd_per_1k_completion_tokens: 2.00
  backup-medium:
    usd_per_1k_prompt_tokens: 1.20
    usd_per_1k_completion_tokens: 2.20
  premium-high:
    usd_per_1k_prompt_tokens: 5.00
    usd_per_1k_completion_tokens: 10.00
  backup-high:
    usd_per_1k_prompt_tokens: 5.50
    usd_per_1k_completion_tokens: 11.00
"#
    }

    fn compile_tiers() -> RoutingArtifact {
        compile_from_yaml(
            &[
                prompt("low", QualityTier::Low),
                prompt("medium", QualityTier::Medium),
                prompt("high", QualityTier::High),
            ],
            project_yaml(),
            lock_yaml(),
            None,
            WorkloadEstimate::default(),
        )
        .expect("compile routes")
    }

    fn cascade(route: &PromptRoute) -> &CascadeDecision {
        match &route.decisions[1] {
            RouteDecision::Cascade(cascade) => cascade,
            other => panic!("expected cascade, got {other:?}"),
        }
    }

    #[test]
    fn emits_ordered_cache_cascade_fallback_decisions_per_prompt() {
        let artifact = compile_tiers();
        for route in artifact.prompts.values() {
            assert!(matches!(route.decisions[0], RouteDecision::Cache(_)));
            assert!(matches!(route.decisions[1], RouteDecision::Cascade(_)));
            assert!(matches!(route.decisions[2], RouteDecision::Fallbacks(_)));
        }

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("routing.json");
        emit_routing_json(&path, &artifact).expect("emit");
        let emitted = fs::read_to_string(path).unwrap();
        let restored: RoutingArtifact = serde_json::from_str(&emitted).unwrap();
        assert_eq!(restored, artifact);
        assert!(emitted.ends_with('\n'));
    }

    #[test]
    fn cold_start_uses_declared_yaml_defaults() {
        let artifact = compile_tiers();
        for route in artifact.prompts.values() {
            match &route.decisions[0] {
                RouteDecision::Cache(cache) => {
                    assert_eq!(cache.similarity_threshold, 0.97);
                    assert_eq!(cache.judge_trigger_band, [0.90, 0.97]);
                    assert_eq!(cache.judge.name, "route-judge");
                }
                other => panic!("expected cache, got {other:?}"),
            }
        }
    }

    #[test]
    fn observed_values_override_cold_start_defaults() {
        let observed = ObservedRoutingStats {
            cache_similarity_threshold: Some(0.95),
            judge_trigger_band: Some([0.88, 0.95]),
        };
        let artifact = compile_from_yaml(
            &[prompt("high", QualityTier::High)],
            project_yaml(),
            lock_yaml(),
            Some(&observed),
            WorkloadEstimate::default(),
        )
        .unwrap();
        match &artifact.prompts["high"].decisions[0] {
            RouteDecision::Cache(cache) => {
                assert_eq!(cache.similarity_threshold, 0.95);
                assert_eq!(cache.judge_trigger_band, [0.88, 0.95]);
            }
            other => panic!("expected cache, got {other:?}"),
        }
    }

    #[test]
    fn quality_tiers_choose_progressively_deeper_cascades() {
        let artifact = compile_tiers();
        assert_eq!(cascade(&artifact.prompts["low"]).steps.len(), 1);
        assert_eq!(cascade(&artifact.prompts["medium"]).steps.len(), 2);
        assert_eq!(cascade(&artifact.prompts["high"]).steps.len(), 3);
        assert_eq!(
            cascade(&artifact.prompts["high"])
                .steps
                .iter()
                .map(|step| step.model.name.as_str())
                .collect::<Vec<_>>(),
            ["cheap-low", "pricey-medium", "premium-high"]
        );
    }

    #[test]
    fn lockfile_prices_pick_cheapest_model_within_each_quality_tier() {
        let mut lock: ModelLock = serde_yaml::from_str(lock_yaml()).unwrap();
        lock.prices
            .get_mut("backup-medium")
            .unwrap()
            .usd_per_1k_prompt_tokens = 0.01;
        lock.prices
            .get_mut("backup-medium")
            .unwrap()
            .usd_per_1k_completion_tokens = 0.01;
        let config: ProjectConfig = serde_yaml::from_str(project_yaml()).unwrap();
        let artifact = compile(
            &[prompt("medium", QualityTier::Medium)],
            &config.cost_model,
            &lock,
            None,
            WorkloadEstimate::default(),
        )
        .unwrap();
        assert_eq!(
            cascade(&artifact.prompts["medium"]).steps[1].model.name,
            "backup-medium"
        );
    }

    #[test]
    fn cache_and_judge_are_nearly_free_pseudo_models() {
        let artifact = compile_tiers();
        let candidates = &artifact.prompts["high"].optimization_candidates;
        let cache = candidates
            .iter()
            .find(|candidate| candidate.model.model_kind == ModelKind::Cache)
            .unwrap();
        let judge = candidates
            .iter()
            .find(|candidate| candidate.model.model_kind == ModelKind::Judge)
            .unwrap();
        assert_eq!(cache.model.estimated_cost_usd, PSEUDO_MODEL_COST_USD);
        assert_eq!(judge.model.estimated_cost_usd, PSEUDO_MODEL_COST_USD);
        assert!(candidates
            .iter()
            .filter(|candidate| candidate.model.model_kind == ModelKind::Provider)
            .all(|candidate| {
                candidate.model.estimated_cost_usd > judge.model.estimated_cost_usd
            }));
    }

    #[test]
    fn missing_target_quality_is_a_clear_error() {
        let config: ProjectConfig = serde_yaml::from_str(project_yaml()).unwrap();
        let mut lock: ModelLock = serde_yaml::from_str(lock_yaml()).unwrap();
        lock.models
            .retain(|_, model| model.quality != QualityTier::High);
        lock.prices.retain(|name, _| lock.models.contains_key(name));
        let error = compile(
            &[prompt("high", QualityTier::High)],
            &config.cost_model,
            &lock,
            None,
            WorkloadEstimate::default(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("no priced model provides it"));
    }
}
