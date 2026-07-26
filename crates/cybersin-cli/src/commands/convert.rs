//! `cybersin convert`: turns a raw prompt into a buildable prompt source.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{json, Value};

pub const DEFAULT_MODEL: &str = "openai/gpt-4.1-mini";
const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";

#[async_trait]
pub trait PromptConversionModel: Send + Sync {
    async fn convert(&self, raw_prompt: &str, schema: &Value) -> Result<Value, String>;
}

#[derive(Debug, PartialEq)]
pub struct ConvertReport {
    pub path: PathBuf,
    pub validated: bool,
    pub inputs: Vec<String>,
    pub tools: Vec<String>,
    pub unmapped_sections: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct PromptDraft {
    name: String,
    quality: String,
    #[serde(
        default,
        deserialize_with = "deserialize_input_declarations",
        skip_serializing_if = "Vec::is_empty"
    )]
    inputs: Vec<PromptInput>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tools: Vec<String>,
    sections: Vec<PromptSection>,
    #[serde(default)]
    output_contract: Option<PromptOutputContract>,
}

#[derive(Debug, Deserialize, Serialize)]
struct PromptInput {
    name: String,
    #[serde(rename = "type", default = "default_input_type")]
    input_type: String,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum InputDeclarations {
    Map(BTreeMap<String, String>),
    List(Vec<PromptInput>),
}

#[derive(Debug, Deserialize, Serialize)]
struct PromptSection {
    id: String,
    priority: u32,
    body: String,
    #[serde(default, skip_serializing)]
    unmapped: bool,
}

#[derive(Debug, Deserialize, Serialize)]
struct PromptOutputContract {
    #[serde(rename = "type")]
    contract_type: String,
    schema: String,
}

fn default_input_type() -> String {
    "string".to_string()
}

#[derive(Serialize)]
struct PromptYaml {
    name: String,
    quality: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    inputs: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<String>,
    sections: Vec<PromptSection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_contract: Option<PromptOutputContract>,
}

pub struct OpenRouterPromptConversionModel {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
    model: String,
}

impl OpenRouterPromptConversionModel {
    pub fn from_env(model: String) -> Result<Self, String> {
        let api_key = std::env::var("OPENROUTER_API_KEY").map_err(|_| {
            "error: OPENROUTER_API_KEY is required for `cybersin convert`".to_string()
        })?;
        let base_url =
            std::env::var("OPENROUTER_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
        Ok(Self {
            client: reqwest::Client::new(),
            api_key,
            base_url: base_url.trim_end_matches('/').to_string(),
            model,
        })
    }
}

#[async_trait]
impl PromptConversionModel for OpenRouterPromptConversionModel {
    async fn convert(&self, raw_prompt: &str, schema: &Value) -> Result<Value, String> {
        let body = json!({
            "model": self.model,
            "messages": [
                {
                    "role": "system",
                    "content": "Turn the user's raw prompt into the requested structured draft. \
                        Propose a short filesystem-safe name, use medium quality unless the prompt \
                        clearly calls for low or high quality. Decompose the prompt into multiple \
                        sections with distinct ids and priorities. Infer typed inputs from \
                        variable-looking spans, using string when no more specific type is clear, \
                        and infer tools such as web_search from recognizable capabilities. Preserve \
                        uncertain content in a low-priority section named unmapped-content and set \
                        its unmapped flag to true. Set unmapped to false on all other sections. \
                        Include output_contract only when the raw prompt clearly requires structured \
                        output such as JSON; otherwise return null for output_contract."
                },
                {"role": "user", "content": raw_prompt}
            ],
            "response_format": {
                "type": "json_schema",
                "json_schema": {
                    "name": "cybersin_prompt_conversion",
                    "strict": true,
                    "schema": schema
                }
            }
        });
        let response = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("error: OpenRouter conversion request failed: {e}"))?;
        let status = response.status();
        let payload: Value = response
            .json()
            .await
            .map_err(|e| format!("error: conversion model response was not JSON: {e}"))?;
        if !status.is_success() {
            return Err(format!(
                "error: OpenRouter conversion returned HTTP {status}: {payload}"
            ));
        }
        let content = payload
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                "error: OpenRouter conversion response had no message content".to_string()
            })?;
        serde_json::from_str(content)
            .map_err(|e| format!("error: conversion model returned invalid JSON content: {e}"))
    }
}

pub fn resolve_input(input: &str, stdin: &mut dyn Read) -> Result<String, String> {
    if input != "-" {
        let path = Path::new(input);
        if path.is_file() {
            return std::fs::read_to_string(path)
                .map_err(|e| format!("error: failed to read {}: {e}", path.display()));
        }
        return Ok(input.to_owned());
    }
    let mut raw = String::new();
    stdin
        .read_to_string(&mut raw)
        .map_err(|e| format!("error: failed to read stdin: {e}"))?;
    Ok(raw)
}

pub async fn run_with(
    converter: &dyn PromptConversionModel,
    project_root: &Path,
    input: &str,
    stdin: &mut dyn Read,
    out: Option<&Path>,
) -> Result<ConvertReport, String> {
    let raw_prompt = resolve_input(input, stdin)?;
    run_raw_with(converter, project_root, &raw_prompt, out).await
}

pub async fn run_raw_with(
    converter: &dyn PromptConversionModel,
    project_root: &Path,
    raw_prompt: &str,
    out: Option<&Path>,
) -> Result<ConvertReport, String> {
    let response = converter.convert(&raw_prompt, &conversion_schema()).await?;
    let mut draft: PromptDraft = serde_json::from_value(response)
        .map_err(|e| format!("error: conversion model returned invalid structured data: {e}"))?;
    if draft.sections.is_empty() {
        return Err("error: conversion model must return at least one section".into());
    }
    normalize_input_types(&mut draft.inputs);

    if draft.inputs.is_empty()
        && draft.tools.is_empty()
        && draft.sections.len() == 1
        && !draft.sections[0].unmapped
    {
        draft.sections[0].body = raw_prompt.to_string();
    }
    if !raw_prompt_implies_structured_output(&raw_prompt) {
        draft.output_contract = None;
    }
    validate_proposed_output_contract(draft.output_contract.as_ref())?;
    validate_proposed_name(&draft.name)?;
    let inputs = draft
        .inputs
        .iter()
        .map(|input| format!("{}:{}", input.name, input.input_type))
        .collect();
    let tools = draft.tools.clone();
    let unmapped_sections = draft
        .sections
        .iter()
        .filter(|section| section.unmapped)
        .map(|section| section.id.clone())
        .collect();

    let path = match out {
        Some(path) if path.is_absolute() => path.to_path_buf(),
        Some(path) => project_root.join(path),
        None => project_root
            .join("prompts")
            .join(format!("{}.prompt.yaml", draft.name)),
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("error: failed to create {}: {e}", parent.display()))?;
    }

    let output = PromptYaml {
        name: draft.name,
        quality: draft.quality,
        inputs: draft
            .inputs
            .into_iter()
            .map(|input| (input.name, input.input_type))
            .collect(),
        tools: draft.tools,
        sections: draft.sections,
        output_contract: draft.output_contract,
    };
    let yaml = serde_yaml::to_string(&output)
        .map_err(|e| format!("error: failed to serialize converted prompt: {e}"))?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                format!(
                    "error: refusing to overwrite existing output {}",
                    path.display()
                )
            } else {
                format!("error: failed to create {}: {e}", path.display())
            }
        })?;
    file.write_all(yaml.as_bytes())
        .map_err(|e| format!("error: failed to write {}: {e}", path.display()))?;
    drop(file);

    cybersin_frontend::compile_prompt_source(&path)
        .map_err(|e| format!("wrote {}, but self-validation failed:\n{e}", path.display()))?;

    Ok(ConvertReport {
        path,
        validated: true,
        inputs,
        tools,
        unmapped_sections,
    })
}

pub async fn execute(input: String, out: Option<PathBuf>, model: String) -> anyhow::Result<()> {
    let cwd =
        std::env::current_dir().map_err(|e| anyhow::anyhow!("reading current directory: {e}"))?;
    let project_root = crate::project::discover_project_root(&cwd).ok_or_else(|| {
        anyhow::anyhow!(
            "no cybersin.yaml found in {} or any parent directory",
            cwd.display()
        )
    })?;
    let converter = OpenRouterPromptConversionModel::from_env(model).map_err(anyhow::Error::msg)?;
    let stdin = std::io::stdin();
    let mut stdin = stdin.lock();
    let report = run_with(
        &converter,
        &project_root,
        &input,
        &mut stdin,
        out.as_deref(),
    )
    .await
    .map_err(anyhow::Error::msg)?;
    println!("wrote {}", report.path.display());
    println!("self-validation passed");
    println!("inferred inputs: {}", summary_list(&report.inputs));
    println!("inferred tools: {}", summary_list(&report.tools));
    println!(
        "unmapped content: {}",
        summary_list(&report.unmapped_sections)
    );
    Ok(())
}

fn summary_list(items: &[String]) -> String {
    if items.is_empty() {
        "none".to_string()
    } else {
        items.join(", ")
    }
}

fn deserialize_input_declarations<'de, D>(deserializer: D) -> Result<Vec<PromptInput>, D::Error>
where
    D: Deserializer<'de>,
{
    match InputDeclarations::deserialize(deserializer)? {
        InputDeclarations::Map(inputs) => Ok(inputs
            .into_iter()
            .map(|(name, input_type)| PromptInput { name, input_type })
            .collect()),
        InputDeclarations::List(inputs) => Ok(inputs),
    }
}

fn normalize_input_types(inputs: &mut [PromptInput]) {
    for input in inputs {
        input.input_type = match input.input_type.trim() {
            "integer" => "number".to_string(),
            "boolean" => "bool".to_string(),
            raw if is_supported_input_type(raw) => raw.to_string(),
            _ => default_input_type(),
        };
    }
}

fn is_supported_input_type(raw: &str) -> bool {
    matches!(raw, "string" | "number" | "bool" | "document")
        || strip_type_wrapper(raw, "enum[").is_some_and(|inner| {
            inner
                .split(',')
                .map(str::trim)
                .any(|variant| !variant.is_empty())
        })
        || strip_type_wrapper(raw, "list[").is_some_and(is_supported_input_type)
}

fn strip_type_wrapper<'a>(raw: &'a str, prefix: &str) -> Option<&'a str> {
    if raw.starts_with(prefix) && raw.ends_with(']') {
        Some(&raw[prefix.len()..raw.len() - 1])
    } else {
        None
    }
}

fn validate_proposed_name(name: &str) -> Result<(), String> {
    let path = Path::new(name);
    let valid = !name.is_empty()
        && path.components().count() == 1
        && matches!(path.components().next(), Some(Component::Normal(_)));
    if valid {
        Ok(())
    } else {
        Err(format!(
            "error: conversion model proposed unsafe prompt name {name:?}"
        ))
    }
}

fn validate_proposed_output_contract(
    contract: Option<&PromptOutputContract>,
) -> Result<(), String> {
    let Some(contract) = contract else {
        return Ok(());
    };
    if contract.contract_type != "json_schema" {
        return Err(format!(
            "error: conversion model proposed unsupported output_contract type {:?}",
            contract.contract_type
        ));
    }
    serde_json::from_str::<Value>(&contract.schema).map_err(|e| {
        format!("error: conversion model proposed invalid output_contract.schema JSON: {e}")
    })?;
    Ok(())
}

fn raw_prompt_implies_structured_output(raw_prompt: &str) -> bool {
    let normalized = raw_prompt.to_ascii_lowercase();
    [
        "json",
        "yaml",
        "xml",
        "schema",
        "structured output",
        "structured response",
        "output fields",
        "fields:",
        "return fields",
        "with fields",
        "return an object",
        "return object",
    ]
    .iter()
    .any(|marker| normalized.contains(marker))
}

fn conversion_schema() -> Value {
    let mut schema = json!({
        "type": "object",
        "required": ["name", "quality", "inputs", "tools", "sections", "output_contract"],
        "properties": {
            "name": {"type": "string"},
            "quality": {"type": "string", "enum": ["low", "medium", "high"]},
            "inputs": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["name", "type"],
                    "properties": {
                        "name": {"type": "string"},
                        "type": {"type": "string"}
                    }
                }
            },
            "tools": {
                "type": "array",
                "items": {"type": "string"}
            },
            "sections": {
                "type": "array",
                "minItems": 1,
                "items": {
                    "type": "object",
                    "required": ["id", "priority", "body", "unmapped"],
                    "properties": {
                        "id": {"type": "string"},
                        "priority": {"type": "integer"},
                        "body": {"type": "string"},
                        "unmapped": {"type": "boolean"}
                    }
                }
            },
            "output_contract": {
                "type": ["object", "null"],
                "required": ["type", "schema"],
                "additionalProperties": false,
                "properties": {
                    "type": {"type": "string", "enum": ["json_schema"]},
                    "schema": {"type": "string"}
                }
            }
        }
    });
    cybersin_backends::enforce_additional_properties_false(&mut schema);
    schema
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct FakeConverter {
        response: serde_json::Value,
    }

    #[async_trait::async_trait]
    impl PromptConversionModel for FakeConverter {
        async fn convert(
            &self,
            _raw_prompt: &str,
            _schema: &serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            Ok(self.response.clone())
        }
    }

    #[test]
    fn literal_input_is_used_verbatim() {
        let mut stdin = "".as_bytes();

        let resolved = resolve_input("Write a concise release note.", &mut stdin).unwrap();

        assert_eq!(resolved, "Write a concise release note.");
    }

    #[test]
    fn existing_file_path_wins_over_literal_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("raw prompt.txt");
        std::fs::write(&path, "Prompt read from disk.").unwrap();
        let mut stdin = "".as_bytes();

        let resolved = resolve_input(path.to_str().unwrap(), &mut stdin).unwrap();

        assert_eq!(resolved, "Prompt read from disk.");
    }

    #[test]
    fn dash_reads_standard_input() {
        let mut stdin = "Prompt piped on stdin.".as_bytes();

        let resolved = resolve_input("-", &mut stdin).unwrap();

        assert_eq!(resolved, "Prompt piped on stdin.");
    }

    #[test]
    fn conversion_schema_is_strict_at_every_object_node() {
        let schema = conversion_schema();

        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(
            schema["properties"]["sections"]["items"]["additionalProperties"],
            false
        );
        assert_eq!(
            schema["properties"]["output_contract"]["additionalProperties"],
            false
        );
    }

    #[tokio::test]
    async fn conversion_writes_a_compiling_draft_to_the_default_project_path() {
        let project = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("cybersin.yaml"), "name: test\n").unwrap();
        let converter = FakeConverter {
            response: json!({
                "name": "release-note",
                "quality": "medium",
                "sections": [{
                    "id": "prompt",
                    "priority": 100,
                    "body": "This model field must not replace the source text."
                }]
            }),
        };
        let mut stdin = "".as_bytes();

        let report = run_with(
            &converter,
            project.path(),
            "Write a concise release note.",
            &mut stdin,
            None,
        )
        .await
        .unwrap();

        let expected = project.path().join("prompts/release-note.prompt.yaml");
        assert_eq!(report.path, expected);
        assert!(report.validated);
        assert_eq!(
            std::fs::read_to_string(expected).unwrap(),
            "name: release-note\nquality: medium\nsections:\n- id: prompt\n  priority: 100\n  body: Write a concise release note.\n"
        );
    }

    #[tokio::test]
    async fn conversion_preserves_decomposition_and_reports_inferred_content() {
        let project = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("cybersin.yaml"), "name: test\n").unwrap();
        let converter = FakeConverter {
            response: json!({
                "name": "research-brief",
                "quality": "high",
                "inputs": [
                    {"name": "topic"},
                    {"name": "max_results", "type": "integer"}
                ],
                "tools": ["web_search"],
                "sections": [
                    {
                        "id": "research",
                        "priority": 100,
                        "body": "Search the web for {{topic}}."
                    },
                    {
                        "id": "format",
                        "priority": 80,
                        "body": "Return no more than {{max_results}} findings."
                    },
                    {
                        "id": "unmapped-content",
                        "priority": 10,
                        "body": "Keep the phrase 'blue hour' for review.",
                        "unmapped": true
                    }
                ]
            }),
        };
        let mut stdin = "".as_bytes();

        let report = run_with(
            &converter,
            project.path(),
            "Research {{topic}} with at most {{max_results}} results. blue hour",
            &mut stdin,
            None,
        )
        .await
        .unwrap();

        assert_eq!(report.inputs, vec!["topic:string", "max_results:number"]);
        assert_eq!(report.tools, vec!["web_search"]);
        assert_eq!(report.unmapped_sections, vec!["unmapped-content"]);
        assert_eq!(
            std::fs::read_to_string(report.path).unwrap(),
            "name: research-brief\nquality: high\ninputs:\n  max_results: number\n  topic: string\ntools:\n- web_search\nsections:\n- id: research\n  priority: 100\n  body: Search the web for {{topic}}.\n- id: format\n  priority: 80\n  body: Return no more than {{max_results}} findings.\n- id: unmapped-content\n  priority: 10\n  body: Keep the phrase 'blue hour' for review.\n"
        );
    }

    #[tokio::test]
    async fn input_declarations_accept_maps_and_default_unknown_types_to_string() {
        let project = tempfile::tempdir().unwrap();
        let converter = FakeConverter {
            response: json!({
                "name": "brief",
                "quality": "medium",
                "inputs": {
                    "audience": "business_person",
                    "depth": "enum[quick, thorough]",
                    "topic": "string"
                },
                "tools": [],
                "sections": [
                    {
                        "id": "instructions",
                        "priority": 100,
                        "body": "Write about {{ topic }} for {{ audience }} at {{ depth }} depth."
                    }
                ],
                "output_contract": null
            }),
        };
        let mut stdin = "".as_bytes();

        let report = run_with(
            &converter,
            project.path(),
            "Write a brief.",
            &mut stdin,
            None,
        )
        .await
        .unwrap();

        assert_eq!(
            report.inputs,
            vec![
                "audience:string",
                "depth:enum[quick, thorough]",
                "topic:string"
            ]
        );
        let yaml = std::fs::read_to_string(report.path).unwrap();
        assert!(yaml.contains(
            "inputs:\n  audience: string\n  depth: enum[quick, thorough]\n  topic: string\n"
        ));
    }

    #[tokio::test]
    async fn structured_output_prompt_writes_the_model_output_contract() {
        let project = tempfile::tempdir().unwrap();
        let output = project.path().join("structured.prompt.yaml");
        let converter = FakeConverter {
            response: json!({
                "name": "structured",
                "quality": "medium",
                "sections": [{"id": "prompt", "priority": 100, "body": "Return JSON."}],
                "output_contract": {
                    "type": "json_schema",
                    "schema": "{\"type\":\"object\",\"properties\":{\"answer\":{\"type\":\"string\"}}}"
                }
            }),
        };
        let mut stdin = "".as_bytes();

        run_with(
            &converter,
            project.path(),
            "Return JSON with a string field named answer.",
            &mut stdin,
            Some(&output),
        )
        .await
        .unwrap();

        let yaml = std::fs::read_to_string(output).unwrap();
        assert!(yaml.contains("output_contract:\n  type: json_schema\n"));
        assert!(yaml.contains(
            "schema: '{\"type\":\"object\",\"properties\":{\"answer\":{\"type\":\"string\"}}}'"
        ));
    }

    #[tokio::test]
    async fn absent_output_contract_is_omitted_from_yaml() {
        let project = tempfile::tempdir().unwrap();
        let output = project.path().join("plain.prompt.yaml");
        let converter = FakeConverter {
            response: json!({
                "name": "plain",
                "quality": "medium",
                "sections": [{"id": "prompt", "priority": 100, "body": "Write prose."}],
                "output_contract": null
            }),
        };
        let mut stdin = "".as_bytes();

        run_with(
            &converter,
            project.path(),
            "Write prose.",
            &mut stdin,
            Some(&output),
        )
        .await
        .unwrap();

        let yaml = std::fs::read_to_string(output).unwrap();
        assert!(!yaml.contains("output_contract"));
    }

    #[tokio::test]
    async fn unstructured_prompt_discards_a_model_proposed_output_contract() {
        let project = tempfile::tempdir().unwrap();
        let output = project.path().join("plain.prompt.yaml");
        let converter = FakeConverter {
            response: json!({
                "name": "plain",
                "quality": "medium",
                "sections": [{"id": "prompt", "priority": 100, "body": "Write prose."}],
                "output_contract": {
                    "type": "json_schema",
                    "schema": "{\"type\":\"object\"}"
                }
            }),
        };
        let mut stdin = "".as_bytes();

        run_with(
            &converter,
            project.path(),
            "Write a concise release note.",
            &mut stdin,
            Some(&output),
        )
        .await
        .unwrap();

        assert!(!std::fs::read_to_string(output)
            .unwrap()
            .contains("output_contract"));
    }

    #[tokio::test]
    async fn invalid_output_contract_schema_is_rejected_before_writing() {
        let project = tempfile::tempdir().unwrap();
        let output = project.path().join("invalid-contract.prompt.yaml");
        let converter = FakeConverter {
            response: json!({
                "name": "invalid-contract",
                "quality": "medium",
                "sections": [{"id": "prompt", "priority": 100, "body": "Return JSON."}],
                "output_contract": {
                    "type": "json_schema",
                    "schema": "{\"type\":\"object\""
                }
            }),
        };
        let mut stdin = "".as_bytes();

        let error = run_with(
            &converter,
            project.path(),
            "Return JSON.",
            &mut stdin,
            Some(&output),
        )
        .await
        .unwrap_err();

        assert!(error.contains("invalid output_contract.schema JSON"));
        assert!(!output.exists());
    }

    #[tokio::test]
    async fn unsupported_output_contract_type_is_rejected_before_writing() {
        let project = tempfile::tempdir().unwrap();
        let output = project.path().join("invalid-contract.prompt.yaml");
        let converter = FakeConverter {
            response: json!({
                "name": "invalid-contract",
                "quality": "medium",
                "sections": [{"id": "prompt", "priority": 100, "body": "Return JSON."}],
                "output_contract": {
                    "type": "xml_schema",
                    "schema": "{}"
                }
            }),
        };
        let mut stdin = "".as_bytes();

        let error = run_with(
            &converter,
            project.path(),
            "Return JSON.",
            &mut stdin,
            Some(&output),
        )
        .await
        .unwrap_err();

        assert!(error.contains("unsupported output_contract type"));
        assert!(!output.exists());
    }

    #[tokio::test]
    async fn existing_output_is_not_overwritten() {
        let project = tempfile::tempdir().unwrap();
        let output = project.path().join("existing.prompt.yaml");
        std::fs::write(&output, "keep me\n").unwrap();
        let converter = FakeConverter {
            response: json!({
                "name": "replacement",
                "quality": "medium",
                "sections": [{"id": "prompt", "priority": 100, "body": "ignored"}]
            }),
        };
        let mut stdin = "".as_bytes();

        let error = run_with(
            &converter,
            project.path(),
            "Replacement prompt",
            &mut stdin,
            Some(&output),
        )
        .await
        .unwrap_err();

        assert!(error.contains("refusing to overwrite"));
        assert_eq!(std::fs::read_to_string(output).unwrap(), "keep me\n");
    }

    #[tokio::test]
    async fn relative_out_path_overrides_the_model_derived_default() {
        let project = tempfile::tempdir().unwrap();
        let converter = FakeConverter {
            response: json!({
                "name": "model-name",
                "quality": "medium",
                "sections": [{"id": "prompt", "priority": 100, "body": "ignored"}]
            }),
        };
        let mut stdin = "".as_bytes();

        let report = run_with(
            &converter,
            project.path(),
            "Use the requested output path.",
            &mut stdin,
            Some(Path::new("drafts/custom.prompt.yaml")),
        )
        .await
        .unwrap();

        assert_eq!(
            report.path,
            project.path().join("drafts/custom.prompt.yaml")
        );
        assert!(!project
            .path()
            .join("prompts/model-name.prompt.yaml")
            .exists());
    }

    #[tokio::test]
    async fn failed_self_validation_leaves_the_written_draft_on_disk() {
        let project = tempfile::tempdir().unwrap();
        let output = project.path().join("invalid.prompt.yaml");
        let converter = FakeConverter {
            response: json!({
                "name": "invalid",
                "quality": "impossible",
                "sections": [{"id": "prompt", "priority": 100, "body": "ignored"}]
            }),
        };
        let mut stdin = "".as_bytes();

        let error = run_with(
            &converter,
            project.path(),
            "Keep this raw prompt.",
            &mut stdin,
            Some(&output),
        )
        .await
        .unwrap_err();

        assert!(error.contains("self-validation failed"));
        assert!(error.contains("invalid quality tier"));
        assert!(output.exists());
        assert!(std::fs::read_to_string(output)
            .unwrap()
            .contains("Keep this raw prompt."));
    }
}
