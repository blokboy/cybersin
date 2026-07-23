//! `cybersin-frontend`: the compiler frontend (spec §6.1).
//!
//! Parses `*.prompt.yaml` sources (spec §5.1), resolves the `!include`
//! graph with cycle detection ([`include`]), typechecks declared `inputs`
//! against how they're actually used in section-body templates
//! ([`typecheck`]), and emits [`cybersin_ir::PromptIr`] ([`ir`]).
//!
//! Section bodies are rendered with **minijinja** ([`template`]); see that
//! module's doc comment for the `{{#each}}`-vs-native-minijinja judgment
//! call. [`fmt`] implements `cybersin fmt`'s canonical re-serialization.
//!
//! Per spec §13's dependency discipline, this crate depends on
//! `cybersin-ir` and nothing that depends on it back.

mod error;
mod fmt;
mod include;
mod ir;
mod raw;
mod template;
mod typecheck;
mod types;

use std::fs;
use std::path::{Path, PathBuf};

use cybersin_ir::PromptIr;

pub use error::{FrontendError, TypecheckIssue};
pub use fmt::format_prompt_source;
pub use template::render as render_section;

/// Compile a single `*.prompt.yaml` source file: parse it, resolve its
/// `!include` graph, typecheck its inputs, and emit a fully-resolved
/// [`PromptIr`] (spec §6.1).
pub fn compile_prompt_source(path: &Path) -> Result<PromptIr, FrontendError> {
    let text = fs::read_to_string(path).map_err(|source| FrontendError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let value: serde_yaml::Value =
        serde_yaml::from_str(&text).map_err(|source| FrontendError::Yaml {
            path: path.to_path_buf(),
            source,
        })?;
    let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
    let resolved = include::resolve_source_includes(value, base_dir)?;
    let raw: raw::RawSource =
        serde_yaml::from_value(resolved).map_err(|source| FrontendError::Yaml {
            path: path.to_path_buf(),
            source,
        })?;
    ir::build_ir(raw)
}

/// Recursively find every `*.prompt.yaml` file under `dir` (or return
/// `[dir]` itself if it already names such a file), for whole-project
/// `cybersin check` runs (spec §5's `prompts/*.prompt.yaml` layout).
pub fn discover_prompt_sources(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    if dir.is_file() {
        if is_prompt_source(dir) {
            found.push(dir.to_path_buf());
        }
        return Ok(found);
    }

    // Prefer the conventional `prompts/` subdirectory (spec §5) when
    // checking a whole project, but fall back to scanning the given
    // directory directly so `cybersin check prompts/` also works.
    let search_root = if dir.join("prompts").is_dir() {
        dir.join("prompts")
    } else {
        dir.to_path_buf()
    };

    walk(&search_root, &mut found)?;
    found.sort();
    Ok(found)
}

fn walk(dir: &Path, found: &mut Vec<PathBuf>) -> std::io::Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            walk(&path, found)?;
        } else if is_prompt_source(&path) {
            found.push(path);
        }
    }
    Ok(())
}

fn is_prompt_source(path: &Path) -> bool {
    path.to_string_lossy().ends_with(".prompt.yaml")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(dir: &Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, content).unwrap();
        path
    }

    const VALID_SOURCE: &str = r#"
name: researcher
quality: high
inputs:
  topic: string
  depth: enum[quick, thorough]
  documents: list[document]
tools: [web_search, web_fetch]
sections:
  - id: role
    priority: 100
    body: |
      You are a research analyst focused on {{ topic }} at {{ depth }} depth.
  - id: instructions
    priority: 90
    body: !include fragments/research-method.md
  - id: documents
    priority: 50
    body: "{{#each documents}}- {{this.title}}\n{{/each}}"
output_contract: { type: json_schema, schema: !include fragments/schemas/report.json }
"#;

    #[test]
    fn valid_source_parses_resolves_typechecks_and_emits_ir() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "fragments/research-method.md",
            "Search broadly, then narrow.\n",
        );
        write(
            tmp.path(),
            "fragments/schemas/report.json",
            r#"{"type":"object","properties":{"summary":{"type":"string"}}}"#,
        );
        let prompt_path = write(tmp.path(), "researcher.prompt.yaml", VALID_SOURCE);

        let ir = compile_prompt_source(&prompt_path).expect("valid source should compile");
        assert_eq!(ir.name, "researcher");
        assert_eq!(ir.sections.len(), 3);
        assert_eq!(ir.sections[1].body, "Search broadly, then narrow.\n");
        assert_eq!(
            ir.sections[2].body,
            "{% for item in documents %}- {{ item.title}}\n{% endfor %}"
        );
        let contract = ir.output_contract.expect("output contract present");
        assert!(contract.schema.contains("\"summary\""));
    }

    #[test]
    fn cyclic_include_fails_clearly() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "fragments/a.md", "!include b.md\n");
        write(tmp.path(), "fragments/b.md", "!include a.md\n");
        let source = r#"
name: broken
quality: high
inputs:
  topic: string
sections:
  - id: role
    priority: 100
    body: !include fragments/a.md
"#;
        let prompt_path = write(tmp.path(), "broken.prompt.yaml", source);
        let err = compile_prompt_source(&prompt_path).unwrap_err();
        assert!(
            matches!(err, FrontendError::IncludeCycle { .. }),
            "got: {err}"
        );
    }

    #[test]
    fn type_mismatch_fails_clearly() {
        let tmp = tempfile::tempdir().unwrap();
        let source = r#"
name: broken
quality: high
inputs:
  topic: string
sections:
  - id: role
    priority: 100
    body: "{{#each topic}}{{this}}{{/each}}"
"#;
        let prompt_path = write(tmp.path(), "broken.prompt.yaml", source);
        let err = compile_prompt_source(&prompt_path).unwrap_err();
        match &err {
            FrontendError::Typecheck(issues) => {
                assert!(issues
                    .iter()
                    .any(|i| matches!(i, TypecheckIssue::TypeMismatch { .. })));
            }
            other => panic!("expected Typecheck error, got: {other}"),
        }
    }

    #[test]
    fn unused_input_fails_clearly() {
        let tmp = tempfile::tempdir().unwrap();
        let source = r#"
name: broken
quality: high
inputs:
  topic: string
  never_used: string
sections:
  - id: role
    priority: 100
    body: "{{ topic }}"
"#;
        let prompt_path = write(tmp.path(), "broken.prompt.yaml", source);
        let err = compile_prompt_source(&prompt_path).unwrap_err();
        match &err {
            FrontendError::Typecheck(issues) => {
                assert!(issues.iter().any(
                    |i| matches!(i, TypecheckIssue::UnusedInput { name } if name == "never_used")
                ));
            }
            other => panic!("expected Typecheck error, got: {other}"),
        }
    }

    #[test]
    fn discover_prompt_sources_finds_project_prompts_dir() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "prompts/researcher.prompt.yaml", VALID_SOURCE);
        write(tmp.path(), "prompts/nested/other.prompt.yaml", VALID_SOURCE);
        write(tmp.path(), "fragments/research-method.md", "x\n");
        write(tmp.path(), "fragments/schemas/report.json", "{}");

        let found = discover_prompt_sources(tmp.path()).unwrap();
        assert_eq!(found.len(), 2);
    }
}
