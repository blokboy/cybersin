//! `cybersin fmt`: canonical formatting for `*.prompt.yaml` sources.
//!
//! Deliberately simple, per the issue's scope: round-trip the source
//! through a struct with a fixed field order (`name`, `quality`, `inputs`,
//! `tools`, `sections`, `output_contract`) and deterministically sorted
//! `inputs` keys, then re-serialize with `serde_yaml`. This normalizes key
//! ordering and indentation without touching `!include` tags — formatting
//! must not bake fragment contents into the source, so this module parses
//! the raw YAML document directly and does **not** go through
//! [`crate::include`]; `!include foo.md` stays exactly `!include foo.md`
//! after a `fmt` pass, carried through untouched via `serde_yaml::Value`.
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_yaml::Value;

use crate::error::FrontendError;

#[derive(Debug, Serialize, Deserialize)]
struct FmtSource {
    name: String,
    quality: String,
    #[serde(default)]
    inputs: BTreeMap<String, String>,
    #[serde(default)]
    tools: Vec<String>,
    sections: Vec<FmtSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    output_contract: Option<FmtOutputContract>,
}

#[derive(Debug, Serialize, Deserialize)]
struct FmtSection {
    id: String,
    priority: u32,
    body: Value,
}

#[derive(Debug, Serialize, Deserialize)]
struct FmtOutputContract {
    #[serde(rename = "type")]
    contract_type: String,
    schema: Value,
}

/// Parse `path` as a `*.prompt.yaml` source (without resolving includes)
/// and return its canonical re-serialization.
pub fn format_prompt_source(path: &Path) -> Result<String, FrontendError> {
    let text = fs::read_to_string(path).map_err(|source| FrontendError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    format_prompt_source_str(&text, path)
}

pub(crate) fn format_prompt_source_str(text: &str, path: &Path) -> Result<String, FrontendError> {
    let value: Value = serde_yaml::from_str(text).map_err(|source| FrontendError::Yaml {
        path: path.to_path_buf(),
        source,
    })?;
    let source: FmtSource =
        serde_yaml::from_value(value).map_err(|source| FrontendError::Yaml {
            path: path.to_path_buf(),
            source,
        })?;
    serde_yaml::to_string(&source).map_err(|source| FrontendError::Yaml {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_key_order_and_sorts_inputs() {
        let input = r#"
sections:
  - priority: 100
    id: role
    body: hello
quality: high
inputs:
  documents: list[document]
  topic: string
name: researcher
"#;
        let out = format_prompt_source_str(input, Path::new("test.prompt.yaml")).unwrap();
        let name_pos = out.find("name:").unwrap();
        let quality_pos = out.find("quality:").unwrap();
        let inputs_pos = out.find("inputs:").unwrap();
        let sections_pos = out.find("sections:").unwrap();
        assert!(name_pos < quality_pos);
        assert!(quality_pos < inputs_pos);
        assert!(inputs_pos < sections_pos);

        let documents_pos = out.find("documents:").unwrap();
        let topic_pos = out.find("topic:").unwrap();
        assert!(
            documents_pos < topic_pos,
            "inputs keys should sort alphabetically"
        );
    }

    #[test]
    fn preserves_include_tags_verbatim() {
        let input = r#"
name: researcher
quality: high
inputs:
  topic: string
sections:
  - id: role
    priority: 100
    body: !include fragments/research-method.md
"#;
        let out = format_prompt_source_str(input, Path::new("test.prompt.yaml")).unwrap();
        assert!(
            out.contains("!include fragments/research-method.md"),
            "expected include tag preserved verbatim, got:\n{out}"
        );
    }

    #[test]
    fn is_idempotent() {
        let input = r#"
name: researcher
quality: high
inputs:
  topic: string
sections:
  - id: role
    priority: 100
    body: Hello {{ topic }}
"#;
        let once = format_prompt_source_str(input, Path::new("test.prompt.yaml")).unwrap();
        let twice = format_prompt_source_str(&once, Path::new("test.prompt.yaml")).unwrap();
        assert_eq!(once, twice);
    }
}
