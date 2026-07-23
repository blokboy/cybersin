//! Per-target context-budget planning (spec §6.2).

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use cybersin_ir::{BudgetArtifact, BudgetPlan, EvictionStep};

use crate::{Pass, PassContext};

/// Context limits used to compile one render target's eviction plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetBudget {
    pub target: String,
    pub context_window_tokens: u32,
    pub reserved_output_tokens: u32,
}

impl TargetBudget {
    pub fn new(
        target: impl Into<String>,
        context_window_tokens: u32,
        reserved_output_tokens: u32,
    ) -> Self {
        Self {
            target: target.into(),
            context_window_tokens,
            reserved_output_tokens,
        }
    }
}

/// Produces one deterministic eviction plan per configured render target.
///
/// Every section is present because live inputs can make any prompt exceed
/// its static size. Lower priorities are evicted first; source order breaks
/// priority ties so byte-identical inputs produce byte-identical artifacts.
#[derive(Debug, Clone)]
pub struct Budget {
    targets: Vec<TargetBudget>,
}

impl Budget {
    pub fn new(targets: Vec<TargetBudget>) -> Self {
        Self { targets }
    }
}

impl Pass for Budget {
    fn name(&self) -> &'static str {
        "budget"
    }

    fn run(&self, ctx: &mut PassContext) {
        let mut section_indexes: Vec<usize> = (0..ctx.ir.sections.len()).collect();
        section_indexes.sort_by_key(|&index| (ctx.ir.sections[index].priority, index));

        let plans = self
            .targets
            .iter()
            .map(|target| {
                let available = target
                    .context_window_tokens
                    .saturating_sub(target.reserved_output_tokens);
                BudgetPlan {
                    target: target.target.clone(),
                    context_window_tokens: target.context_window_tokens,
                    reserved_output_tokens: target.reserved_output_tokens,
                    eviction_order: section_indexes
                        .iter()
                        .map(|&index| EvictionStep {
                            section_id: ctx.ir.sections[index].id.clone(),
                            evict_at_tokens: available,
                        })
                        .collect(),
                }
            })
            .collect();

        ctx.budget = Some(BudgetArtifact {
            prompt_name: ctx.ir.name.clone(),
            plans,
        });
    }
}

/// Write a compiled artifact to `dist/budget/<prompt-name>.json`.
pub fn write_budget_artifact(
    dist_dir: impl AsRef<Path>,
    artifact: &BudgetArtifact,
) -> io::Result<PathBuf> {
    let prompt_path = Path::new(&artifact.prompt_name);
    if artifact.prompt_name.is_empty()
        || prompt_path.components().count() != 1
        || !matches!(prompt_path.components().next(), Some(Component::Normal(_)))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "prompt name must be a single non-empty path component",
        ));
    }

    let budget_dir = dist_dir.as_ref().join("budget");
    fs::create_dir_all(&budget_dir)?;
    let path = budget_dir.join(format!("{}.json", artifact.prompt_name));
    let mut json = serde_json::to_vec_pretty(artifact).map_err(io::Error::other)?;
    json.push(b'\n');
    fs::write(&path, json)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use cybersin_ir::{PromptIr, QualityTier, Section};

    use super::*;

    fn prompt() -> PromptIr {
        PromptIr::new(
            "researcher",
            QualityTier::High,
            BTreeMap::new(),
            vec![],
            vec![
                section("medium", 50),
                section("lowest", 10),
                section("highest", 100),
                section("also_lowest", 10),
            ],
            None,
        )
    }

    fn section(id: &str, priority: u32) -> Section {
        Section {
            id: id.to_string(),
            priority,
            body: id.to_string(),
            dedup_ref: None,
        }
    }

    #[test]
    fn lowest_priority_sections_are_evicted_first_with_stable_ties() {
        let mut ctx = PassContext::new(prompt());
        Budget::new(vec![TargetBudget::new("generic", 100, 20)]).run(&mut ctx);
        let plan = &ctx.budget.unwrap().plans[0];
        let ids: Vec<&str> = plan
            .eviction_order
            .iter()
            .map(|step| step.section_id.as_str())
            .collect();
        assert_eq!(ids, ["lowest", "also_lowest", "medium", "highest"]);
        assert!(plan
            .eviction_order
            .iter()
            .all(|step| step.evict_at_tokens == 80));
    }

    #[test]
    fn creates_independent_plans_for_multiple_targets() {
        let mut ctx = PassContext::new(prompt());
        Budget::new(vec![
            TargetBudget::new("small", 32_000, 2_000),
            TargetBudget::new("large", 200_000, 8_000),
        ])
        .run(&mut ctx);
        let artifact = ctx.budget.unwrap();
        assert_eq!(artifact.plans.len(), 2);
        assert_eq!(artifact.plans[0].target, "small");
        assert_eq!(artifact.plans[0].eviction_order[0].evict_at_tokens, 30_000);
        assert_eq!(artifact.plans[1].target, "large");
        assert_eq!(artifact.plans[1].eviction_order[0].evict_at_tokens, 192_000);
    }

    #[test]
    fn writes_pretty_json_under_dist_budget() {
        let tmp = tempfile::tempdir().unwrap();
        let artifact = BudgetArtifact {
            prompt_name: "researcher".to_string(),
            plans: vec![],
        };
        let path = write_budget_artifact(tmp.path().join("dist"), &artifact).unwrap();
        assert_eq!(path, tmp.path().join("dist/budget/researcher.json"));
        let restored: BudgetArtifact = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(restored, artifact);
    }

    #[test]
    fn refuses_prompt_names_that_escape_the_budget_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let artifact = BudgetArtifact {
            prompt_name: "../outside".to_string(),
            plans: vec![],
        };
        let error = write_budget_artifact(tmp.path().join("dist"), &artifact).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}
