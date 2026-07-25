//! Live [`ModelCaller`] backed by OpenRouter's OpenAI-compatible
//! `/chat/completions` endpoint (issue #35 Phase 1: gateway-mediated
//! multi-provider calling, OpenRouter shipped first).
//!
//! A compiled `PromptIr`'s section bodies are already plain minijinja by
//! the time they reach `dist/` — `cybersin-frontend`'s handlebars-sugar
//! translation already ran at build time (`cybersin_frontend::ir`) — so
//! this only needs to substitute call-time `inputs` into that template and
//! hand the result to `cybersin-backends`' existing OpenAI-dialect
//! renderer, the same one `cybersin build` uses for
//! `dist/prompts/<name>/openai.json`. This crate deliberately never
//! depends on `cybersin-frontend`/`cybersin-passes` (spec §13: "the
//! runtime consumes artifacts, not sources") — `minijinja` itself is a
//! plain third-party template engine, not a dependency on the compiler.
//!
//! A real model has no built-in notion of the `confidence` score
//! `RouteExecutor`'s cascade needs to decide whether to escalate — so
//! every request's structured output contract gets a `confidence` field
//! injected (or, if the prompt declared no contract, a minimal envelope
//! that still asks for one), and the model self-reports it.
//!
//! Gateway failover (Vercel AI Gateway, then a self-hosted LiteLLM proxy,
//! if OpenRouter itself is unreachable) is deliberately not implemented
//! here yet — issue #35's agreed sequencing ships OpenRouter alone first
//! and proves the `ModelCaller` seam before adding fallback tiers. A
//! future `GatewayChain<...>` wrapping multiple `ModelCaller`s the same
//! way this one wraps a single HTTP endpoint can add that without
//! changing this file.

use std::sync::Arc;

use async_trait::async_trait;
use cybersin_backends::{backend_for, RenderedPrompt};
use cybersin_ir::PromptIr;
use cybersin_router::RouteModel;
use serde_json::{json, Value};

use crate::dist::DistFixture;
use crate::route_executor::{ModelCaller, ModelOutput};

const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";

/// `OPENROUTER_API_KEY` was not set. A typed config error rather than a
/// panic, so a missing key surfaces as an ordinary startup failure instead
/// of taking down a session deep inside a call.
#[derive(Debug, thiserror::Error)]
#[error(
    "OPENROUTER_API_KEY is not set; live model calling requires an OpenRouter API key \
     (see docs/agents/issue-tracker.md issue #35)"
)]
pub struct MissingApiKey;

pub struct OpenRouterModelCaller {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
    dist: Arc<DistFixture>,
    /// Which `cybersin-backends` dialect to render through — OpenRouter's
    /// endpoint speaks the OpenAI dialect regardless of which model is
    /// ultimately served behind it.
    target: String,
}

impl OpenRouterModelCaller {
    pub fn new(dist: Arc<DistFixture>, api_key: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            api_key: api_key.into(),
            base_url: DEFAULT_BASE_URL.to_string(),
            dist,
            target: "openai".to_string(),
        }
    }

    /// Read `OPENROUTER_API_KEY` from the environment.
    pub fn from_env(dist: Arc<DistFixture>) -> Result<Self, MissingApiKey> {
        let api_key = std::env::var("OPENROUTER_API_KEY").map_err(|_| MissingApiKey)?;
        Ok(Self::new(dist, api_key))
    }

    #[cfg(test)]
    fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Substitute `inputs` into `prompt`'s section bodies, then render
    /// through this caller's backend dialect to get concrete
    /// messages/tools/response_format — the same shape `cybersin build`
    /// writes to `dist/prompts/<name>/<target>.json`, just with template
    /// placeholders filled in with this call's actual values.
    fn render_messages(&self, prompt: &PromptIr, inputs: &Value) -> Result<RenderedPrompt, String> {
        let mut rendered_ir = prompt.clone();
        for section in &mut rendered_ir.sections {
            if section.dedup_ref.is_some() {
                // Contributes no body text either way — backend rendering
                // skips these sections outright (mirrors
                // cybersin-backends::split_messages).
                continue;
            }
            section.body = render_template(&section.body, inputs)
                .map_err(|error| format!("rendering section {:?}: {error}", section.id))?;
        }
        backend_for(&self.target).and_then(|backend| backend.render(&rendered_ir))
    }
}

fn render_template(body: &str, inputs: &Value) -> Result<String, minijinja::Error> {
    let env = minijinja::Environment::new();
    let template = env.template_from_str(body)?;
    template.render(inputs)
}

/// Merge a self-reported `confidence` requirement into a structured
/// output contract's JSON schema — or, if the prompt declared none,
/// synthesize a minimal envelope that still asks for one. This is the
/// only route to a real cascade confidence signal (this module's doc).
fn response_format_with_confidence(rendered: &RenderedPrompt) -> Result<Value, String> {
    let mut format = rendered.response_format.clone().unwrap_or_else(|| {
        json!({
            "type": "json_schema",
            "json_schema": {
                "name": rendered.name,
                "schema": {"type": "object", "properties": {}, "required": []}
            }
        })
    });
    let schema = format
        .pointer_mut("/json_schema/schema")
        .ok_or_else(|| "response_format missing json_schema.schema".to_string())?;
    let schema_obj = schema
        .as_object_mut()
        .ok_or_else(|| "output_contract schema root must be a JSON object".to_string())?;
    let properties_obj = schema_obj
        .entry("properties")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| "output_contract schema `properties` must be a JSON object".to_string())?;
    properties_obj.insert(
        "confidence".to_string(),
        json!({"type": "number", "minimum": 0, "maximum": 1}),
    );
    let required_arr = schema_obj
        .entry("required")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or_else(|| "output_contract schema `required` must be a JSON array".to_string())?;
    if !required_arr.iter().any(|value| value == "confidence") {
        required_arr.push(json!("confidence"));
    }
    Ok(format)
}

#[async_trait]
impl ModelCaller for OpenRouterModelCaller {
    async fn call(
        &self,
        model: &RouteModel,
        prompt_name: &str,
        inputs: &Value,
    ) -> Result<ModelOutput, String> {
        let prompt = self
            .dist
            .prompt(prompt_name)
            .map_err(|error| format!("loading compiled prompt {prompt_name:?}: {error}"))?;
        let rendered = self.render_messages(prompt, inputs)?;
        let response_format = response_format_with_confidence(&rendered)?;

        let mut body = json!({
            "model": format!("{}/{}", model.provider, model.name),
            "messages": rendered
                .messages
                .iter()
                .map(|message| json!({"role": message.role, "content": message.content}))
                .collect::<Vec<_>>(),
            "response_format": response_format,
        });
        if !rendered.tools.is_empty() {
            body["tools"] = Value::Array(rendered.tools.clone());
        }

        let http_response = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|error| format!("calling OpenRouter for model {}: {error}", model.name))?;

        let status = http_response.status();
        let payload: Value = http_response.json().await.map_err(|error| {
            format!(
                "parsing OpenRouter response for model {}: {error}",
                model.name
            )
        })?;
        if !status.is_success() {
            return Err(format!(
                "OpenRouter returned {status} for model {}: {payload}",
                model.name
            ));
        }

        let content = payload
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                format!(
                    "OpenRouter response for model {} had no message content",
                    model.name
                )
            })?;
        let parsed: Value = serde_json::from_str(content).map_err(|error| {
            format!(
                "model {} did not return valid JSON matching its output contract: {error}",
                model.name
            )
        })?;
        let confidence = parsed
            .get("confidence")
            .and_then(Value::as_f64)
            .ok_or_else(|| {
                format!(
                    "model {} did not self-report a confidence field",
                    model.name
                )
            })?;

        Ok(ModelOutput {
            response: parsed,
            confidence,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dist::DistManifest;
    use crate::route_executor::CacheArtifact;
    use cybersin_ir::{OutputContract, QualityTier, Section};
    use cybersin_router::{ModelKind, RoutingArtifact};
    use std::collections::BTreeMap as StdBTreeMap;
    use wiremock::matchers::{body_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn dist_with_prompt(prompt: PromptIr) -> Arc<DistFixture> {
        let mut prompts = StdBTreeMap::new();
        prompts.insert(prompt.name.clone(), prompt);
        Arc::new(DistFixture {
            manifest: DistManifest {
                build_hash: "test".into(),
                git_sha: "test".into(),
            },
            prompts,
            routing: StdBTreeMap::new(),
            budgets: StdBTreeMap::new(),
            tools: StdBTreeMap::new(),
            cascades: StdBTreeMap::new(),
            routing_artifact: RoutingArtifact {
                schema_version: 1,
                prompts: StdBTreeMap::new(),
            },
            cache_artifact: CacheArtifact {
                schema_version: 1,
                namespace_version: "0".into(),
                entries: Vec::new(),
            },
        })
    }

    fn researcher_prompt() -> PromptIr {
        PromptIr::new(
            "researcher",
            QualityTier::High,
            StdBTreeMap::new(),
            vec![],
            vec![Section {
                id: "assignment".into(),
                priority: 90,
                body: "Investigate {{ topic }}.".into(),
                dedup_ref: None,
            }],
            Some(OutputContract {
                contract_type: "json_schema".into(),
                schema: r#"{"type":"object","properties":{"summary":{"type":"string"}},"required":["summary"]}"#.into(),
            }),
        )
    }

    fn model() -> RouteModel {
        RouteModel {
            name: "claude-3-5-sonnet".into(),
            provider: "anthropic".into(),
            quality: QualityTier::High,
            estimated_cost_usd: 0.01,
            model_kind: ModelKind::Provider,
        }
    }

    #[tokio::test]
    async fn renders_inputs_and_extracts_self_reported_confidence() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": "{\"summary\": \"done\", \"confidence\": 0.91}"
                    }
                }]
            })))
            .mount(&server)
            .await;

        let caller = OpenRouterModelCaller::new(dist_with_prompt(researcher_prompt()), "test-key")
            .with_base_url(server.uri());

        let output = caller
            .call(
                &model(),
                "researcher",
                &json!({"topic": "evidence quality"}),
            )
            .await
            .unwrap();

        assert_eq!(output.confidence, 0.91);
        assert_eq!(output.response["summary"], "done");
    }

    #[tokio::test]
    async fn sends_model_id_as_provider_slash_name_and_injects_confidence_into_schema() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(body_json(json!({
                "model": "anthropic/claude-3-5-sonnet",
                "messages": [{
                    "role": "system",
                    "content": "<section name=\"assignment\">\nInvestigate evidence quality.\n</section>"
                }],
                "response_format": {
                    "type": "json_schema",
                    "json_schema": {
                        "name": "researcher",
                        "schema": {
                            "type": "object",
                            "properties": {
                                "summary": {"type": "string"},
                                "confidence": {"type": "number", "minimum": 0, "maximum": 1}
                            },
                            "required": ["summary", "confidence"]
                        }
                    }
                }
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{"message": {"content": "{\"summary\": \"ok\", \"confidence\": 0.5}"}}]
            })))
            .mount(&server)
            .await;

        let caller = OpenRouterModelCaller::new(dist_with_prompt(researcher_prompt()), "test-key")
            .with_base_url(server.uri());

        caller
            .call(
                &model(),
                "researcher",
                &json!({"topic": "evidence quality"}),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn missing_confidence_in_the_model_response_is_a_call_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{"message": {"content": "{\"summary\": \"done\"}"}}]
            })))
            .mount(&server)
            .await;

        let caller = OpenRouterModelCaller::new(dist_with_prompt(researcher_prompt()), "test-key")
            .with_base_url(server.uri());

        let error = caller
            .call(&model(), "researcher", &json!({"topic": "x"}))
            .await
            .unwrap_err();
        assert!(error.contains("did not self-report a confidence field"));
    }

    #[test]
    fn from_env_reports_a_clear_error_when_the_key_is_missing() {
        std::env::remove_var("OPENROUTER_API_KEY");
        let error = OpenRouterModelCaller::from_env(dist_with_prompt(researcher_prompt()));
        assert!(error.is_err());
    }
}
