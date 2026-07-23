//! `lint` (spec §6.2): checks that depend on other passes' output,
//! split from `lint-fast` by that real dependency rather than by
//! convention. Two checks:
//!
//! - **Contradictions**: two sections sharing the same `id`.
//!   `Section::id` is the identity `EvictionStep::section_id`, backend
//!   rendering, and (after `dedupe`) `dedup_ref` all key off — once
//!   `dedupe`/`reorder` have run, a duplicate id is an unresolvable
//!   ambiguity about which section a downstream reference means, not
//!   just untidy authoring.
//! - **Dead sections**: a section evicted at *every* render target's
//!   budget, so it never reaches a model for any target regardless of
//!   which one gets used. Only knowable once the `budget` pass (issue
//!   #6) has produced a [`cybersin_ir::BudgetArtifact`] for this prompt;
//!   this pass reads it from [`PassContext::budget`] when present and
//!   skips the check entirely otherwise (it runs in every profile, spec
//!   §6.2, including builds where `budget` hasn't run).

use std::collections::HashMap;

use crate::{Diagnostic, Pass, PassContext};

pub struct Lint;

impl Pass for Lint {
    fn name(&self) -> &'static str {
        "lint"
    }

    fn run(&self, ctx: &mut PassContext) {
        let contradictions = duplicate_section_ids(ctx);
        for id in contradictions {
            ctx.push(Diagnostic::error(
                self.name(),
                format!("contradiction: multiple sections share id `{id}`"),
            ));
        }

        let dead = dead_sections(ctx);
        for id in dead {
            ctx.push(Diagnostic::warning(
                self.name(),
                format!(
                    "section `{id}` is evicted at every render target's budget and never \
                     reaches a model"
                ),
            ));
        }
    }
}

fn duplicate_section_ids(ctx: &PassContext) -> Vec<String> {
    let mut seen: HashMap<&str, u32> = HashMap::new();
    for section in &ctx.ir.sections {
        *seen.entry(section.id.as_str()).or_insert(0) += 1;
    }
    let mut dupes: Vec<String> = seen
        .into_iter()
        .filter(|(_, count)| *count > 1)
        .map(|(id, _)| id.to_string())
        .collect();
    dupes.sort();
    dupes
}

fn dead_sections(ctx: &PassContext) -> Vec<String> {
    let Some(budget) = &ctx.budget else {
        return Vec::new();
    };
    if budget.prompt_name != ctx.ir.name || budget.plans.is_empty() {
        return Vec::new();
    }

    ctx.ir
        .sections
        .iter()
        .filter(|section| {
            budget.plans.iter().all(|plan| {
                plan.eviction_order
                    .iter()
                    .any(|step| step.section_id == section.id)
            })
        })
        .map(|section| section.id.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use cybersin_ir::{BudgetArtifact, BudgetPlan, EvictionStep, PromptIr, QualityTier, Section};

    use super::*;
    use crate::Severity;

    fn section(id: &str, priority: u32, body: &str) -> Section {
        Section {
            id: id.to_string(),
            priority,
            body: body.to_string(),
            dedup_ref: None,
        }
    }

    fn base_ir() -> PromptIr {
        PromptIr::new(
            "researcher",
            QualityTier::High,
            BTreeMap::new(),
            vec![],
            vec![
                section("role", 100, "You are a research analyst."),
                section("documents", 50, "{{ documents }}"),
            ],
            None,
        )
    }

    #[test]
    fn golden_duplicate_section_ids_are_a_contradiction() {
        let mut ir = base_ir();
        ir.sections.push(section("role", 40, "A different role."));

        let mut ctx = PassContext::new(ir);
        Lint.run(&mut ctx);

        assert_eq!(ctx.diagnostics.len(), 1);
        assert_eq!(ctx.diagnostics[0].severity, Severity::Error);
        assert!(ctx.diagnostics[0].message.contains("role"));
    }

    #[test]
    fn golden_no_budget_yet_skips_dead_section_check() {
        let ctx_ir = base_ir();
        let mut ctx = PassContext::new(ctx_ir);
        Lint.run(&mut ctx);
        assert!(ctx.diagnostics.is_empty());
    }

    #[test]
    fn golden_section_evicted_at_every_target_is_dead() {
        let ir = base_ir();
        let budget = BudgetArtifact {
            prompt_name: "researcher".to_string(),
            plans: vec![
                BudgetPlan {
                    target: "gpt-4o".to_string(),
                    context_window_tokens: 128_000,
                    reserved_output_tokens: 4_096,
                    eviction_order: vec![EvictionStep {
                        section_id: "documents".to_string(),
                        evict_at_tokens: 0,
                    }],
                },
                BudgetPlan {
                    target: "claude-sonnet".to_string(),
                    context_window_tokens: 200_000,
                    reserved_output_tokens: 4_096,
                    eviction_order: vec![EvictionStep {
                        section_id: "documents".to_string(),
                        evict_at_tokens: 0,
                    }],
                },
            ],
        };

        let mut ctx = PassContext::new(ir).with_budget(budget);
        Lint.run(&mut ctx);

        assert_eq!(ctx.diagnostics.len(), 1);
        assert_eq!(ctx.diagnostics[0].severity, Severity::Warning);
        assert!(ctx.diagnostics[0].message.contains("documents"));
    }

    #[test]
    fn golden_section_kept_by_at_least_one_target_is_not_dead() {
        let ir = base_ir();
        let budget = BudgetArtifact {
            prompt_name: "researcher".to_string(),
            plans: vec![
                BudgetPlan {
                    target: "gpt-4o".to_string(),
                    context_window_tokens: 128_000,
                    reserved_output_tokens: 4_096,
                    eviction_order: vec![EvictionStep {
                        section_id: "documents".to_string(),
                        evict_at_tokens: 0,
                    }],
                },
                BudgetPlan {
                    target: "claude-sonnet".to_string(),
                    context_window_tokens: 200_000,
                    reserved_output_tokens: 4_096,
                    // "documents" survives for this target: never listed.
                    eviction_order: vec![],
                },
            ],
        };

        let mut ctx = PassContext::new(ir).with_budget(budget);
        Lint.run(&mut ctx);

        assert!(ctx.diagnostics.is_empty());
    }

    #[test]
    fn budget_for_a_different_prompt_is_ignored() {
        let ir = base_ir();
        let budget = BudgetArtifact {
            prompt_name: "other-prompt".to_string(),
            plans: vec![BudgetPlan {
                target: "gpt-4o".to_string(),
                context_window_tokens: 1_000,
                reserved_output_tokens: 100,
                eviction_order: vec![EvictionStep {
                    section_id: "documents".to_string(),
                    evict_at_tokens: 0,
                }],
            }],
        };

        let mut ctx = PassContext::new(ir).with_budget(budget);
        Lint.run(&mut ctx);

        assert!(ctx.diagnostics.is_empty());
    }
}
