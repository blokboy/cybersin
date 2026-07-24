//! Session budget config + breach detection (spec §8.5: "Budgets enforced
//! by the executor; on breach: halt | degrade (cheapest cascade step) |
//! ask (approval gate)").
//!
//! Declared in an agent's `budget:` block (spec §5.3,
//! `agents/*.agent.yaml` — see `cybersin_cli::commands::init`'s scaffolded
//! `HELLO_AGENT` template: `budget: { usd_per_session: 1.00, on_breach:
//! degrade }`) and parsed the same tolerant way
//! `cybersin_router::compile_from_yaml` parses `cybersin.yaml`'s
//! `cost_model:` block: `serde_yaml::from_str` into a typed subset,
//! ignoring every other top-level key (`name`, `harness`, `tools`, ...) —
//! see that crate's `ProjectConfig` for the same pattern.

use serde::Deserialize;

/// What [`crate::session::RuntimeDaemon`] does once a session's running
/// spend reaches [`BudgetConfig::usd_per_session`] (spec §8.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnBreach {
    /// Abort the session with a distinct, inspectable terminal status
    /// (`"halted"`, not a crash and not the generic `"aborted"`) rather
    /// than continuing to spend.
    Halt,
    /// Keep going, but re-route every further `llm.request` for the
    /// breached prompt to its cheapest available cascade step (spec
    /// §8.5). Once a prompt degrades in a session it stays degraded —
    /// spend never goes back down mid-session.
    Degrade,
    /// Park the session behind an approval gate — the same
    /// `awaiting_approval` mechanism a critical tool call with
    /// `approval: required` uses (spec §8.2) — until `cybersin
    /// approve|deny` lets this call proceed or fails it.
    Ask,
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub struct BudgetConfig {
    pub usd_per_session: f64,
    pub on_breach: OnBreach,
}

/// The subset of `agents/*.agent.yaml` this crate reads. Every other
/// field (`name`, `harness`, `tools`, ...) is parsed by serde_yaml and
/// dropped on the floor — no `#[serde(deny_unknown_fields)]`, so this
/// stays forward-compatible with fields later issues add to the same
/// file.
#[derive(Debug, Clone, Deserialize)]
struct AgentYaml {
    budget: BudgetConfig,
}

impl BudgetConfig {
    /// Parse the `budget:` block out of one `agents/*.agent.yaml` source.
    pub fn from_agent_yaml(yaml: &str) -> Result<Self, serde_yaml::Error> {
        let parsed: AgentYaml = serde_yaml::from_str(yaml)?;
        Ok(parsed.budget)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_scaffolded_agent_yaml_shape() {
        // Exactly `cybersin_cli::commands::init::HELLO_AGENT`'s shape —
        // this test breaks loudly if the two ever drift apart.
        let yaml = r#"
name: hello-agent
harness: { adapter: process, command: ["python", "loop.py"] }
budget: { usd_per_session: 1.00, on_breach: degrade }
tools: []
"#;
        let budget = BudgetConfig::from_agent_yaml(yaml).unwrap();
        assert_eq!(budget.usd_per_session, 1.00);
        assert_eq!(budget.on_breach, OnBreach::Degrade);
    }

    #[test]
    fn all_three_on_breach_values_parse() {
        for (text, expected) in [
            ("halt", OnBreach::Halt),
            ("degrade", OnBreach::Degrade),
            ("ask", OnBreach::Ask),
        ] {
            let yaml = format!("budget: {{ usd_per_session: 2.5, on_breach: {text} }}");
            assert_eq!(
                BudgetConfig::from_agent_yaml(&yaml).unwrap().on_breach,
                expected
            );
        }
    }

    #[test]
    fn missing_budget_block_is_a_clear_error() {
        assert!(BudgetConfig::from_agent_yaml("name: no-budget-here\n").is_err());
    }
}
