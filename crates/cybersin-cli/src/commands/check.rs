//! `cybersin check <path>` (spec §11): runs one prompt source, or every
//! `*.prompt.yaml` under a project, through `cybersin-frontend`'s
//! parse/resolve/typecheck/emit pipeline.

use std::path::Path;

use crate::capabilities::{
    check_output_stream, check_summary, execute_check, CapabilityEvent, CheckInput, OutputMode,
};

pub fn run(path: &Path) -> Result<Option<String>, String> {
    let execution = execute_check(CheckInput::new(path));
    render_check_events(&execution.events);
    check_summary(&execution.events)
        .unwrap_or_else(|| {
            Err("cybersin check failed: capability did not emit a terminal event".to_string())
        })
        .map(Some)
}

fn render_check_events(events: &[CapabilityEvent]) {
    for event in events {
        if let CapabilityEvent::Output {
            mode: OutputMode::Text,
            value,
        } = event
        {
            if let Some((stream, text)) = check_output_stream(value) {
                match stream {
                    "stdout" => println!("{text}"),
                    "stderr" => eprintln!("{text}"),
                    _ => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::capabilities::{check_summary, execute_check, CheckInput};

    use super::*;

    #[test]
    fn cli_adapter_summary_matches_direct_check_capability() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("researcher.prompt.yaml");
        fs::write(
            &path,
            r#"
name: researcher
quality: high
inputs:
  topic: string
sections:
  - id: role
    priority: 100
    body: "Research {{ topic }}."
"#,
        )
        .unwrap();

        let direct = execute_check(CheckInput::new(&path));
        let direct_summary = check_summary(&direct.events)
            .expect("check capability should emit a terminal event")
            .map(Some);
        let emitted_ir = direct
            .events
            .iter()
            .find_map(|event| match event {
                CapabilityEvent::Output {
                    mode: OutputMode::Json,
                    value,
                } => value.get("ir").and_then(|ir| ir.get("name")),
                _ => None,
            })
            .expect("check capability should emit compiled IR");

        assert_eq!(emitted_ir, "researcher");
        assert_eq!(run(&path), direct_summary);
    }

    #[test]
    fn direct_check_capability_emits_cli_equivalent_failure_output() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("broken.prompt.yaml");
        fs::write(
            &path,
            r#"
name: broken
quality: high
inputs:
  topic: string
  unused_one: string
sections:
  - id: role
    priority: 100
    body: "{{#each topic}}{{this}}{{/each}}"
"#,
        )
        .unwrap();

        let direct = execute_check(CheckInput::new(&path));
        let output_text = direct
            .events
            .iter()
            .find_map(|event| match event {
                CapabilityEvent::Output {
                    mode: OutputMode::Text,
                    value,
                } => check_output_stream(value).map(|(_, text)| text.to_string()),
                _ => None,
            })
            .expect("failed source should emit text output");

        assert!(output_text.starts_with(&format!("FAIL  {}", path.display())));
        assert!(output_text.contains("typecheck failed"));
        assert_eq!(
            check_summary(&direct.events),
            Some(Err(
                "cybersin check failed: 1 of 1 source(s) had errors".to_string()
            ))
        );
    }
}
