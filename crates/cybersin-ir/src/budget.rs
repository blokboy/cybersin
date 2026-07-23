//! Budget-plan IR: per-target eviction plans produced by the `budget`
//! optimizer pass (spec §6.2) and executed by the runtime's context
//! assembler (spec §8.3a).
//!
//! Compile time decides the *policy* — which sections drop, in what
//! order, at which context sizes; the assembler applies that policy to
//! data that only exists at call time (live inputs, retrieved documents,
//! conversation history), filling sections in priority order and evicting
//! per plan once the target's token budget is exceeded. This module is
//! the shared shape both sides read and write via serde (spec §6.6).

use serde::{Deserialize, Serialize};

/// One step of a budget plan's eviction order: a section to drop once the
/// assembled context reaches a given size, for one specific render
/// target.
///
/// A `BudgetPlan`'s `eviction_order` is a `Vec` of these, so the three
/// facts spec §6.2 calls out — *which* sections drop, in what *order*,
/// at which context *sizes* — are respectively: `section_id`, the vector
/// position, and `evict_at_tokens`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvictionStep {
    /// The `Section::id` this step evicts (spec §5.1).
    pub section_id: String,
    /// Evict this section once the assembled context is at or above this
    /// many tokens for the target in question.
    pub evict_at_tokens: u32,
}

/// A per-target eviction plan for a single compiled prompt (spec §6.2's
/// `budget` pass output, one entry of the `budget/` dist artifact,
/// §6.6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetPlan {
    /// Render target this plan applies to, e.g. a model family name or
    /// `generic` (spec §6.5's `--target generic`).
    pub target: String,
    /// Total context window available to this target, in tokens.
    pub context_window_tokens: u32,
    /// Tokens reserved for model output, subtracted from the budget
    /// available to input sections.
    pub reserved_output_tokens: u32,
    /// Ordered eviction steps the context assembler executes in sequence
    /// once assembled content exceeds the available budget (spec §8.3a).
    pub eviction_order: Vec<EvictionStep>,
}

/// The `budget/` dist artifact for one prompt (spec §6.6): every
/// per-target plan the `budget` pass computed for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetArtifact {
    /// Matches `PromptIr::name` for the prompt this artifact belongs to.
    pub prompt_name: String,
    /// One plan per render target the build was configured for.
    pub plans: Vec<BudgetPlan>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_plan(target: &str) -> BudgetPlan {
        BudgetPlan {
            target: target.to_string(),
            context_window_tokens: 128_000,
            reserved_output_tokens: 4_096,
            eviction_order: vec![
                EvictionStep {
                    section_id: "documents".to_string(),
                    evict_at_tokens: 100_000,
                },
                EvictionStep {
                    section_id: "instructions".to_string(),
                    evict_at_tokens: 120_000,
                },
            ],
        }
    }

    #[test]
    fn eviction_step_round_trips() {
        let original = EvictionStep {
            section_id: "documents".to_string(),
            evict_at_tokens: 100_000,
        };
        let json = serde_json::to_string(&original).expect("serialize");
        let restored: EvictionStep = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, restored);
    }

    #[test]
    fn budget_plan_round_trips() {
        let original = sample_plan("gpt-4o");
        let json = serde_json::to_string(&original).expect("serialize");
        let restored: BudgetPlan = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, restored);
    }

    #[test]
    fn budget_plan_with_empty_eviction_order_round_trips() {
        let original = BudgetPlan {
            target: "generic".to_string(),
            context_window_tokens: 32_000,
            reserved_output_tokens: 1_024,
            eviction_order: vec![],
        };
        let json = serde_json::to_string(&original).expect("serialize");
        let restored: BudgetPlan = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, restored);
    }

    #[test]
    fn budget_artifact_round_trips() {
        let original = BudgetArtifact {
            prompt_name: "researcher".to_string(),
            plans: vec![sample_plan("gpt-4o"), sample_plan("claude-sonnet")],
        };
        let json = serde_json::to_string(&original).expect("serialize");
        let restored: BudgetArtifact = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, restored);
    }
}
