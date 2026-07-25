//! Parses `agents/*.agent.yaml`'s `harness:` block (spec §5.3) for
//! `cybersin run <agent.yaml>` (issue #35 Phase 3) — the process a live
//! session's harness adapter spawns.
//!
//! Mirrors `cybersin_runtime::budget::BudgetConfig::from_agent_yaml`'s
//! established pattern exactly: a narrow struct, `serde_yaml::from_str` on
//! the raw agent.yaml source text at `cybersin run` invocation time, every
//! other top-level key ignored. Lives here, not in `cybersin-runtime`,
//! because nothing inside `RuntimeDaemon` needs to know how its channel was
//! wired up — only this crate's `commands::run`, which does the spawning,
//! cares. Not compiled into `dist/`: `harness:` is a "how to run" concern
//! like `budget:`, not a "what was compiled" concern like `tools:`/`routing:`.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct HarnessConfig {
    pub adapter: String,
    pub command: Vec<String>,
}

/// The subset of `agents/*.agent.yaml` this module reads. Every other
/// field (`budget`, `tools`, `sandbox`, ...) is parsed by serde_yaml and
/// dropped on the floor — no `#[serde(deny_unknown_fields)]`.
#[derive(Debug, Clone, Deserialize)]
struct AgentYaml {
    name: String,
    harness: HarnessConfig,
}

#[derive(Debug, Clone)]
pub(crate) struct AgentMeta {
    pub name: String,
    pub harness: HarnessConfig,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum HarnessConfigError {
    #[error("parsing agent.yaml: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error(
        "harness.adapter = {0:?} is not supported; only \"process\" (stdio) and \"grpc\" \
         are implemented"
    )]
    UnsupportedAdapter(String),
    #[error("harness.command must not be empty")]
    EmptyCommand,
}

impl AgentMeta {
    /// Parse `name:`/`harness:` out of one `agents/*.agent.yaml` source.
    pub(crate) fn from_agent_yaml(yaml: &str) -> Result<Self, HarnessConfigError> {
        let parsed: AgentYaml = serde_yaml::from_str(yaml)?;
        if parsed.harness.adapter != "process" && parsed.harness.adapter != "grpc" {
            return Err(HarnessConfigError::UnsupportedAdapter(
                parsed.harness.adapter,
            ));
        }
        if parsed.harness.command.is_empty() {
            return Err(HarnessConfigError::EmptyCommand);
        }
        Ok(Self {
            name: parsed.name,
            harness: parsed.harness,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_ic1_fixture_harness_block() {
        let yaml = r#"
name: research-team
harness:
  adapter: process
  command: ["python", "loop.py"]
budget:
  usd_per_session: 2.50
  on_breach: degrade
tools: []
"#;
        let meta = AgentMeta::from_agent_yaml(yaml).unwrap();
        assert_eq!(meta.name, "research-team");
        assert_eq!(meta.harness.adapter, "process");
        assert_eq!(meta.harness.command, vec!["python", "loop.py"]);
    }

    #[test]
    fn parses_a_grpc_adapter() {
        let yaml = r#"
name: research-team
harness:
  adapter: grpc
  command: ["python", "loop.py"]
"#;
        let meta = AgentMeta::from_agent_yaml(yaml).unwrap();
        assert_eq!(meta.harness.adapter, "grpc");
        assert_eq!(meta.harness.command, vec!["python", "loop.py"]);
    }

    #[test]
    fn rejects_an_unknown_adapter() {
        let yaml = r#"
name: research-team
harness:
  adapter: websocket
  command: ["python", "loop.py"]
"#;
        let err = AgentMeta::from_agent_yaml(yaml).unwrap_err();
        assert!(matches!(err, HarnessConfigError::UnsupportedAdapter(a) if a == "websocket"));
    }

    #[test]
    fn rejects_an_empty_command() {
        let yaml = r#"
name: research-team
harness:
  adapter: process
  command: []
"#;
        let err = AgentMeta::from_agent_yaml(yaml).unwrap_err();
        assert!(matches!(err, HarnessConfigError::EmptyCommand));
    }
}
