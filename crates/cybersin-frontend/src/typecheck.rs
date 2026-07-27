//! Typechecks a parsed prompt source's `inputs:` map against how those
//! inputs are actually used in section bodies, and its `grounded_tiers:`
//! declarations against its own `quality:` cascade (spec §6.1, issue #82).
//!
//! Problems caught here: a template referencing an input that was never
//! declared, an input used in a way incompatible with its declared type
//! (looping over a scalar, or printing a list directly instead of
//! iterating it), an input declared but never referenced anywhere, a
//! `grounded_tiers:` entry that isn't a valid quality tier, and a
//! `grounded_tiers:` entry naming a tier above the prompt's own `quality:`
//! (the same semantic check the router enforces, re-derived here so it
//! surfaces at compile time). All problems found are collected and
//! reported together rather than stopping at the first one.

use std::collections::{BTreeMap, BTreeSet};

use cybersin_ir::{InputType, QualityTier};

use crate::error::TypecheckIssue;
use crate::raw::RawSource;
use crate::template::{self, RefKind};
use crate::types::{parse_input_type, parse_quality_tier, quality_tier_name, type_name};

/// Declared inputs (ready for [`cybersin_ir::PromptIr::inputs`]) alongside
/// declared grounded tiers (ready for
/// [`cybersin_ir::PromptIr::grounded_tiers`]), as produced by
/// [`typecheck`] on success.
type TypecheckOutput = (BTreeMap<String, InputType>, BTreeSet<QualityTier>);

/// Typecheck `raw` and, on success, return its declared inputs and
/// declared grounded tiers. `quality` is `raw.quality`, already parsed by
/// the caller, used to validate `grounded_tiers:`.
pub(crate) fn typecheck(
    raw: &RawSource,
    quality: QualityTier,
) -> Result<TypecheckOutput, Vec<TypecheckIssue>> {
    let mut declared = BTreeMap::new();
    let mut issues = Vec::new();

    for (name, raw_type) in &raw.inputs {
        match parse_input_type(raw_type) {
            Some(t) => {
                declared.insert(name.clone(), t);
            }
            None => issues.push(TypecheckIssue::InvalidInputType {
                name: name.clone(),
                raw: raw_type.clone(),
            }),
        }
    }

    let mut used: BTreeSet<String> = BTreeSet::new();

    for section in &raw.sections {
        for r in template::extract_refs(&section.body) {
            used.insert(r.name.clone());
            match declared.get(&r.name) {
                None => issues.push(TypecheckIssue::UndeclaredInput {
                    location: section.id.clone(),
                    name: r.name.clone(),
                }),
                Some(t) => {
                    let is_list = matches!(t, InputType::List { .. });
                    match (&r.kind, is_list) {
                        (RefKind::Collection, false) => issues.push(TypecheckIssue::TypeMismatch {
                            location: section.id.clone(),
                            name: r.name.clone(),
                            expected: "a list (looped with {{#each}} / {% for %})".to_string(),
                            found: type_name(t),
                        }),
                        (RefKind::Plain, true) => issues.push(TypecheckIssue::TypeMismatch {
                            location: section.id.clone(),
                            name: r.name.clone(),
                            expected: "a scalar interpolation".to_string(),
                            found: type_name(t),
                        }),
                        _ => {}
                    }
                }
            }
        }
    }

    for name in declared.keys() {
        if !used.contains(name) {
            issues.push(TypecheckIssue::UnusedInput { name: name.clone() });
        }
    }

    let mut grounded_tiers = BTreeSet::new();
    for raw_tier in &raw.grounded_tiers {
        match parse_quality_tier(raw_tier) {
            Some(tier) if tier > quality => {
                issues.push(TypecheckIssue::GroundedTierAboveQuality {
                    tier: quality_tier_name(tier).to_string(),
                    quality: quality_tier_name(quality).to_string(),
                });
            }
            Some(tier) => {
                grounded_tiers.insert(tier);
            }
            None => issues.push(TypecheckIssue::InvalidGroundedTier {
                raw: raw_tier.trim().to_string(),
            }),
        }
    }

    if issues.is_empty() {
        Ok((declared, grounded_tiers))
    } else {
        Err(issues)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raw::RawSection;

    fn source_with(inputs: &[(&str, &str)], sections: &[(&str, &str)]) -> RawSource {
        source_with_grounded(inputs, sections, &[])
    }

    fn source_with_grounded(
        inputs: &[(&str, &str)],
        sections: &[(&str, &str)],
        grounded_tiers: &[&str],
    ) -> RawSource {
        RawSource {
            name: "test".to_string(),
            quality: "high".to_string(),
            inputs: inputs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            tools: vec![],
            grounded_tiers: grounded_tiers.iter().map(|s| s.to_string()).collect(),
            sections: sections
                .iter()
                .enumerate()
                .map(|(i, (id, body))| RawSection {
                    id: id.to_string(),
                    priority: 100 - i as u32,
                    body: body.to_string(),
                })
                .collect(),
            output_contract: None,
        }
    }

    #[test]
    fn valid_source_typechecks() {
        let raw = source_with(
            &[("topic", "string"), ("documents", "list[document]")],
            &[
                ("role", "About {{ topic }}"),
                ("docs", "{{#each documents}}{{this.title}}{{/each}}"),
            ],
        );
        let (declared, grounded_tiers) =
            typecheck(&raw, QualityTier::High).expect("should typecheck");
        assert_eq!(declared.len(), 2);
        assert!(grounded_tiers.is_empty());
    }

    #[test]
    fn flags_undeclared_input() {
        let raw = source_with(
            &[("topic", "string")],
            &[("role", "{{ topic }} and {{ mystery }}")],
        );
        let issues = typecheck(&raw, QualityTier::High).unwrap_err();
        assert!(issues.iter().any(
            |i| matches!(i, TypecheckIssue::UndeclaredInput { name, .. } if name == "mystery")
        ));
    }

    #[test]
    fn flags_type_mismatch_each_over_scalar() {
        let raw = source_with(
            &[("topic", "string")],
            &[("role", "{{#each topic}}{{this}}{{/each}}")],
        );
        let issues = typecheck(&raw, QualityTier::High).unwrap_err();
        assert!(issues
            .iter()
            .any(|i| matches!(i, TypecheckIssue::TypeMismatch { name, .. } if name == "topic")));
    }

    #[test]
    fn flags_type_mismatch_plain_print_of_list() {
        let raw = source_with(
            &[("documents", "list[document]")],
            &[("role", "Here: {{ documents }}")],
        );
        let issues = typecheck(&raw, QualityTier::High).unwrap_err();
        assert!(issues.iter().any(
            |i| matches!(i, TypecheckIssue::TypeMismatch { name, .. } if name == "documents")
        ));
    }

    #[test]
    fn flags_unused_input() {
        let raw = source_with(
            &[("topic", "string"), ("unused_one", "string")],
            &[("role", "{{ topic }}")],
        );
        let issues = typecheck(&raw, QualityTier::High).unwrap_err();
        assert!(issues
            .iter()
            .any(|i| matches!(i, TypecheckIssue::UnusedInput { name } if name == "unused_one")));
    }

    #[test]
    fn flags_invalid_type_syntax() {
        let raw = source_with(&[("topic", "not_a_real_type")], &[("role", "{{ topic }}")]);
        let issues = typecheck(&raw, QualityTier::High).unwrap_err();
        assert!(issues
            .iter()
            .any(|i| matches!(i, TypecheckIssue::InvalidInputType { .. })));
    }

    #[test]
    fn grounded_tiers_within_quality_round_trip() {
        let raw = source_with_grounded(
            &[("topic", "string")],
            &[("role", "{{ topic }}")],
            &["medium", "high"],
        );
        let (_, grounded_tiers) = typecheck(&raw, QualityTier::High).expect("should typecheck");
        assert_eq!(
            grounded_tiers,
            BTreeSet::from([QualityTier::Medium, QualityTier::High])
        );
    }

    #[test]
    fn flags_grounded_tier_above_quality() {
        let raw = source_with_grounded(
            &[("topic", "string")],
            &[("role", "{{ topic }}")],
            &["high"],
        );
        let issues = typecheck(&raw, QualityTier::Medium).unwrap_err();
        assert!(issues.iter().any(|i| matches!(
            i,
            TypecheckIssue::GroundedTierAboveQuality { tier, quality }
                if tier == "high" && quality == "medium"
        )));
    }

    #[test]
    fn flags_invalid_grounded_tier_string() {
        let raw = source_with_grounded(
            &[("topic", "string")],
            &[("role", "{{ topic }}")],
            &["ultra"],
        );
        let issues = typecheck(&raw, QualityTier::High).unwrap_err();
        assert!(issues
            .iter()
            .any(|i| matches!(i, TypecheckIssue::InvalidGroundedTier { raw } if raw == "ultra")));
    }
}
