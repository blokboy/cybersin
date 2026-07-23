//! `reorder` (spec §6.2): stable sections first, for provider prefix
//! caching.
//!
//! A section is "stable" here if its rendered text is identical on
//! every call — i.e. its body has no template placeholders (`{{ ... }}`
//! / `{% ... %}`) left for the runtime to fill in. That's exactly the
//! property prefix caching needs: providers cache a prompt's *byte-
//! identical* leading prefix across calls, so grouping the
//! never-changes-per-call sections at the front maximizes how much of
//! the prompt can land in that shared prefix, while sections that vary
//! per call (documents, user input) sort after.
//!
//! Deriving stability from `body` rather than adding a `stable: bool`
//! field to `cybersin_ir::Section` keeps this a read of what's already
//! there instead of a fact the frontend would have to remember to set
//! correctly on every section it emits.
//!
//! The reordering is a stable sort on "is this section stable", so
//! sections within each group keep their original relative (priority)
//! order — only the stable/unstable partition point moves.

use crate::{Pass, PassContext};

pub struct Reorder;

impl Pass for Reorder {
    fn name(&self) -> &'static str {
        "reorder"
    }

    fn run(&self, ctx: &mut PassContext) {
        ctx.ir.sections.sort_by_key(|s| !is_stable(&s.body));
    }
}

fn is_stable(body: &str) -> bool {
    !body.contains("{{") && !body.contains("{%")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use cybersin_ir::{PromptIr, QualityTier, Section};

    use super::*;

    fn section(id: &str, priority: u32, body: &str) -> Section {
        Section {
            id: id.to_string(),
            priority,
            body: body.to_string(),
            dedup_ref: None,
        }
    }

    #[test]
    fn golden_stable_sections_move_ahead_of_dynamic_ones() {
        let ir = PromptIr::new(
            "researcher",
            QualityTier::High,
            BTreeMap::new(),
            vec![],
            vec![
                section(
                    "documents",
                    50,
                    "{% for d in documents %}{{ d.title }}{% endfor %}",
                ),
                section("role", 100, "You are a research analyst."),
                section("greeting", 95, "Hello, {{ topic }}."),
                section("footer", 10, "End of prompt."),
            ],
            None,
        );

        let mut ctx = PassContext::new(ir);
        Reorder.run(&mut ctx);

        let ids: Vec<&str> = ctx.ir.sections.iter().map(|s| s.id.as_str()).collect();
        // Stable ("role", "footer") first, in original relative order;
        // dynamic ("documents", "greeting") after, in original relative
        // order.
        assert_eq!(ids, vec!["role", "footer", "documents", "greeting"]);
    }

    #[test]
    fn golden_all_stable_is_a_no_op_ordering() {
        let ir = PromptIr::new(
            "researcher",
            QualityTier::High,
            BTreeMap::new(),
            vec![],
            vec![
                section("role", 100, "You are a research analyst."),
                section("footer", 10, "End of prompt."),
            ],
            None,
        );

        let mut ctx = PassContext::new(ir);
        Reorder.run(&mut ctx);

        let ids: Vec<&str> = ctx.ir.sections.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["role", "footer"]);
    }

    #[test]
    fn golden_all_dynamic_is_a_no_op_ordering() {
        let ir = PromptIr::new(
            "researcher",
            QualityTier::High,
            BTreeMap::new(),
            vec![],
            vec![
                section("greeting", 100, "Hello, {{ topic }}."),
                section(
                    "documents",
                    50,
                    "{% for d in documents %}{{ d }}{% endfor %}",
                ),
            ],
            None,
        );

        let mut ctx = PassContext::new(ir);
        Reorder.run(&mut ctx);

        let ids: Vec<&str> = ctx.ir.sections.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["greeting", "documents"]);
    }
}
