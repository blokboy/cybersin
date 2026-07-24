//! Backends: per-model-family idiomatic rendering behind a shared
//! [`Backend`] trait (spec §6.5).
//!
//! Each backend turns a fully-optimized [`PromptIr`] into a
//! [`RenderedPrompt`] — a message split and a tool-schema dialect that
//! matches how that model family actually expects to be called — and
//! validates the prompt against that family's constraints before
//! rendering it. `generic` ([`GenericBackend`]) renders a portable,
//! provider-agnostic shape for `--target generic` (spec §6.5); concrete
//! model families (starting with [`OpenAiBackend`]) render their native
//! dialect instead.

use cybersin_ir::PromptIr;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// One target's rendered form of a prompt (spec §6.6's per-target
/// `dist/prompts/<name>/<target>.json`). Deliberately generic across
/// backends — `messages`/`tools`/`response_format` all speak plain JSON
/// so this shape doesn't grow a variant per model family.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RenderedPrompt {
    pub schema_version: u32,
    pub target: String,
    pub name: String,
    pub messages: Vec<Message>,
    pub tools: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

/// A model family's rendering + constraint validation (spec §6.5).
///
/// `render` is expected to validate before producing output — callers
/// should be able to trust that any `Ok(RenderedPrompt)` satisfies this
/// backend's dialect constraints, not just that it typechecks as JSON.
pub trait Backend: Send + Sync {
    /// Stable target identifier, e.g. `"generic"` or `"openai"` — matches
    /// `cybersin.yaml`'s `targets:` list and `--target`.
    fn target(&self) -> &'static str;

    fn render(&self, prompt: &PromptIr) -> Result<RenderedPrompt, String>;
}

/// Portable rendering with no model-specific dialect assumptions (spec
/// §6.5: "`--target generic` retained for portability").
pub struct GenericBackend;

/// OpenAI Chat Completions dialect: `type: function` tool schemas, a
/// `json_schema` response-format wrapper, and tagless system/user
/// message bodies.
pub struct OpenAiBackend;

impl Backend for GenericBackend {
    fn target(&self) -> &'static str {
        "generic"
    }

    fn render(&self, prompt: &PromptIr) -> Result<RenderedPrompt, String> {
        validate(prompt)?;
        Ok(RenderedPrompt {
            schema_version: 1,
            target: self.target().into(),
            name: prompt.name.clone(),
            messages: split_messages(prompt, false),
            tools: prompt
                .tools
                .iter()
                .map(|name| json!({"name": name, "input_schema": {"type": "object"}}))
                .collect(),
            response_format: contract(prompt)?,
        })
    }
}

impl Backend for OpenAiBackend {
    fn target(&self) -> &'static str {
        "openai"
    }

    fn render(&self, prompt: &PromptIr) -> Result<RenderedPrompt, String> {
        validate(prompt)?;
        validate_openai(prompt)?;
        Ok(RenderedPrompt {
            schema_version: 1,
            target: self.target().into(),
            name: prompt.name.clone(),
            messages: split_messages(prompt, true),
            tools: prompt
                .tools
                .iter()
                .map(|name| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": name,
                            "description": format!("Cybersin tool {name}"),
                            "parameters": {"type": "object", "properties": {}, "additionalProperties": true}
                        }
                    })
                })
                .collect(),
            response_format: contract(prompt)?.map(|schema| {
                json!({
                    "type": "json_schema",
                    "json_schema": {"name": prompt.name, "schema": schema}
                })
            }),
        })
    }
}

/// Resolve `--target`/`cybersin.yaml`'s `targets:` entries to a backend.
/// `gpt` is accepted as an alias so a project can name the target after
/// the model family it actually calls without inventing a second
/// `Backend` impl for it.
pub fn backend_for(target: &str) -> Result<Box<dyn Backend>, String> {
    match target {
        "generic" => Ok(Box::new(GenericBackend)),
        "openai" | "gpt" => Ok(Box::new(OpenAiBackend)),
        other => Err(format!(
            "unknown backend target {other:?}; supported targets: generic, openai"
        )),
    }
}

/// Constraints every backend shares, regardless of dialect: a prompt
/// that renders to zero content, or names a tool with an empty name,
/// can't be idiomatic in *any* model family.
fn validate(prompt: &PromptIr) -> Result<(), String> {
    if prompt.sections.is_empty() {
        return Err("prompt must contain at least one section".into());
    }
    if prompt.tools.iter().any(|name| name.trim().is_empty()) {
        return Err("tool names must not be empty".into());
    }
    Ok(())
}

/// OpenAI's function-calling dialect constrains tool names to
/// `^[A-Za-z0-9_-]{1,64}$` (spec §6.5's "constraint validation" per
/// model family) — a name that violates this would render a
/// `RenderedPrompt` the OpenAI API rejects outright, so catch it here
/// rather than at call time.
fn validate_openai(prompt: &PromptIr) -> Result<(), String> {
    for name in &prompt.tools {
        let valid_len = !name.is_empty() && name.len() <= 64;
        let valid_chars = name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if !valid_len || !valid_chars {
            return Err(format!(
                "openai backend: tool name {name:?} must be 1-64 characters of [A-Za-z0-9_-]"
            ));
        }
    }
    Ok(())
}

/// Split sections into a system message (the first section, plus any
/// explicitly named `role`/`system`) and a user message (everything
/// else) — the message-split half of spec §6.5's "message split".
/// Sections collapsed onto an earlier duplicate by `dedupe` (§6.2) carry
/// no body of their own and are skipped; the canonical section they
/// point at already contributed its content.
fn split_messages(prompt: &PromptIr, tags: bool) -> Vec<Message> {
    let mut system = Vec::new();
    let mut user = Vec::new();
    for (index, section) in prompt.sections.iter().enumerate() {
        if section.dedup_ref.is_some() {
            continue;
        }
        let body = if tags {
            format!(
                "<section name=\"{}\">\n{}\n</section>",
                section.id, section.body
            )
        } else {
            format!("## {}\n{}", section.id, section.body)
        };
        if index == 0 || matches!(section.id.as_str(), "role" | "system") {
            system.push(body);
        } else {
            user.push(body);
        }
    }
    let mut messages = vec![Message {
        role: "system".into(),
        content: system.join("\n\n"),
    }];
    if !user.is_empty() {
        messages.push(Message {
            role: "user".into(),
            content: user.join("\n\n"),
        });
    }
    messages
}

fn contract(prompt: &PromptIr) -> Result<Option<Value>, String> {
    prompt
        .output_contract
        .as_ref()
        .map(|contract| {
            serde_json::from_str(&contract.schema)
                .map_err(|e| format!("invalid output JSON schema: {e}"))
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cybersin_ir::{OutputContract, QualityTier, Section};
    use std::collections::BTreeMap;

    fn prompt() -> PromptIr {
        PromptIr::new(
            "research",
            QualityTier::High,
            BTreeMap::new(),
            vec!["web_search".into()],
            vec![
                Section {
                    id: "role".into(),
                    priority: 100,
                    body: "Be exact.".into(),
                    dedup_ref: None,
                },
                Section {
                    id: "task".into(),
                    priority: 90,
                    body: "Research.".into(),
                    dedup_ref: None,
                },
            ],
            Some(OutputContract {
                contract_type: "json_schema".into(),
                schema: r#"{"type":"object"}"#.into(),
            }),
        )
    }

    #[test]
    fn openai_has_function_dialect_and_message_split() {
        let rendered = OpenAiBackend.render(&prompt()).unwrap();
        assert_eq!(
            rendered
                .messages
                .iter()
                .map(|m| m.role.as_str())
                .collect::<Vec<_>>(),
            ["system", "user"]
        );
        assert!(rendered.messages[0]
            .content
            .contains("<section name=\"role\">"));
        assert_eq!(rendered.tools[0]["type"], "function");
        assert_eq!(rendered.tools[0]["function"]["name"], "web_search");
        assert_eq!(
            rendered.response_format.as_ref().unwrap()["type"],
            "json_schema"
        );
    }

    #[test]
    fn generic_is_portable() {
        let rendered = GenericBackend.render(&prompt()).unwrap();
        assert_eq!(rendered.tools[0]["input_schema"]["type"], "object");
        assert_eq!(rendered.tools[0]["name"], "web_search");
        assert!(rendered.messages[0].content.contains("## role"));
        assert!(!rendered.messages[0].content.contains("<section"));
    }

    #[test]
    fn openai_rejects_a_tool_name_its_dialect_cannot_express() {
        let mut invalid = prompt();
        invalid.tools = vec!["web search!".into()];
        let error = OpenAiBackend.render(&invalid).unwrap_err();
        assert!(error.contains("tool name"), "unexpected error: {error}");
        // The same prompt still renders fine on the portable target,
        // since `generic` never claimed to speak OpenAI's dialect.
        assert!(GenericBackend.render(&invalid).is_ok());
    }

    #[test]
    fn unknown_target_is_a_clear_error() {
        let error = match backend_for("does-not-exist") {
            Ok(_) => panic!("expected an unknown-target error"),
            Err(error) => error,
        };
        assert!(error.contains("does-not-exist"));
    }

    #[test]
    fn dedup_ref_sections_contribute_no_body_text() {
        let mut deduped = prompt();
        deduped.sections.push(Section {
            id: "task2".into(),
            priority: 80,
            body: String::new(),
            dedup_ref: Some("task".into()),
        });
        let rendered = GenericBackend.render(&deduped).unwrap();
        let all_content: String = rendered
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect();
        assert!(!all_content.contains("## task2"));
    }
}
