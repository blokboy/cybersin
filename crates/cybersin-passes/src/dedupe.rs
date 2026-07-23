//! `dedupe` (spec §6.2): shared fragments collapse to refs.
//!
//! The frontend inlines every `!include` target before IR reaches the
//! optimizer (`Section::body`'s doc comment), so two sections that
//! `!include`d the same fragment source arrive here as two byte-for-byte
//! identical `body` strings rather than a shared reference. This pass
//! finds those duplicates and collapses every non-canonical copy: its
//! `body` is emptied and [`cybersin_ir::Section::dedup_ref`] is set to
//! the id of the first (canonical) section carrying that text — the
//! shared content is paid for once in every rendered/cached artifact
//! downstream, not once per including section.
//!
//! Sections are compared by exact `body` equality only (no fuzzy/near-
//! duplicate matching); empty bodies are never deduped against each
//! other since there'd be nothing to collapse.

use std::collections::HashMap;

use crate::{Pass, PassContext};

pub struct Dedupe;

impl Pass for Dedupe {
    fn name(&self) -> &'static str {
        "dedupe"
    }

    fn run(&self, ctx: &mut PassContext) {
        // section id -> index of the canonical (first-seen) section with
        // this exact body.
        let mut canonical_by_body: HashMap<String, usize> = HashMap::new();
        let mut dedup_refs: Vec<Option<String>> = vec![None; ctx.ir.sections.len()];

        for (idx, section) in ctx.ir.sections.iter().enumerate() {
            if section.body.trim().is_empty() {
                continue;
            }
            match canonical_by_body.get(&section.body) {
                Some(&canonical_idx) => {
                    dedup_refs[idx] = Some(ctx.ir.sections[canonical_idx].id.clone());
                }
                None => {
                    canonical_by_body.insert(section.body.clone(), idx);
                }
            }
        }

        for (section, dedup_ref) in ctx.ir.sections.iter_mut().zip(dedup_refs) {
            if let Some(canonical_id) = dedup_ref {
                section.body.clear();
                section.dedup_ref = Some(canonical_id);
            }
        }
    }
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
    fn golden_duplicate_fragment_collapses_to_a_ref() {
        // "role" and "system_reminder" both `!include`d the same
        // fragment; the frontend inlined it into identical bodies.
        let ir = PromptIr::new(
            "researcher",
            QualityTier::High,
            BTreeMap::new(),
            vec![],
            vec![
                section("role", 100, "Follow the house safety policy."),
                section("instructions", 90, "Answer using the documents."),
                section("system_reminder", 10, "Follow the house safety policy."),
            ],
            None,
        );

        let mut ctx = PassContext::new(ir);
        Dedupe.run(&mut ctx);

        let expected = vec![
            section("role", 100, "Follow the house safety policy."),
            section("instructions", 90, "Answer using the documents."),
            Section {
                id: "system_reminder".to_string(),
                priority: 10,
                body: String::new(),
                dedup_ref: Some("role".to_string()),
            },
        ];
        assert_eq!(ctx.ir.sections, expected);
    }

    #[test]
    fn golden_three_way_duplicate_all_ref_the_first_occurrence() {
        let ir = PromptIr::new(
            "researcher",
            QualityTier::High,
            BTreeMap::new(),
            vec![],
            vec![
                section("a", 100, "shared text"),
                section("b", 90, "shared text"),
                section("c", 80, "shared text"),
            ],
            None,
        );

        let mut ctx = PassContext::new(ir);
        Dedupe.run(&mut ctx);

        assert_eq!(ctx.ir.sections[0].dedup_ref, None);
        assert_eq!(ctx.ir.sections[0].body, "shared text");
        assert_eq!(ctx.ir.sections[1].dedup_ref, Some("a".to_string()));
        assert_eq!(ctx.ir.sections[1].body, "");
        assert_eq!(ctx.ir.sections[2].dedup_ref, Some("a".to_string()));
        assert_eq!(ctx.ir.sections[2].body, "");
    }

    #[test]
    fn golden_no_duplicates_leaves_ir_untouched() {
        let ir = PromptIr::new(
            "researcher",
            QualityTier::High,
            BTreeMap::new(),
            vec![],
            vec![
                section("role", 100, "You are a research analyst."),
                section("instructions", 90, "Answer using the documents."),
            ],
            None,
        );
        let original = ir.clone();

        let mut ctx = PassContext::new(ir);
        Dedupe.run(&mut ctx);

        assert_eq!(ctx.ir, original);
    }

    #[test]
    fn empty_bodies_are_never_deduped_against_each_other() {
        let ir = PromptIr::new(
            "researcher",
            QualityTier::High,
            BTreeMap::new(),
            vec![],
            vec![section("a", 100, ""), section("b", 90, "")],
            None,
        );

        let mut ctx = PassContext::new(ir);
        Dedupe.run(&mut ctx);

        assert!(ctx.ir.sections.iter().all(|s| s.dedup_ref.is_none()));
    }
}
