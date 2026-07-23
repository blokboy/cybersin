//! Typechecks a parsed prompt source's `inputs:` map against how those
//! inputs are actually used in section bodies (spec §6.1).
//!
//! Three problems are caught here, matching the acceptance criteria this
//! crate must satisfy: a template referencing an input that was never
//! declared, an input used in a way incompatible with its declared type
//! (looping over a scalar, or printing a list directly instead of
//! iterating it), and an input declared but never referenced anywhere.
//! All problems found are collected and reported together rather than
//! stopping at the first one.

use std::collections::{BTreeMap, BTreeSet};

use cybersin_ir::InputType;

use crate::error::TypecheckIssue;
use crate::raw::RawSource;
use crate::template::{self, RefKind};
use crate::types::{parse_input_type, type_name};

/// Typecheck `raw` and, on success, return its declared inputs parsed
/// into [`InputType`]s (ready for [`cybersin_ir::PromptIr::inputs`]).
pub(crate) fn typecheck(
    raw: &RawSource,
) -> Result<BTreeMap<String, InputType>, Vec<TypecheckIssue>> {
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

    if issues.is_empty() {
        Ok(declared)
    } else {
        Err(issues)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raw::RawSection;

    fn source_with(inputs: &[(&str, &str)], sections: &[(&str, &str)]) -> RawSource {
        RawSource {
            name: "test".to_string(),
            quality: "high".to_string(),
            inputs: inputs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            tools: vec![],
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
        let declared = typecheck(&raw).expect("should typecheck");
        assert_eq!(declared.len(), 2);
    }

    #[test]
    fn flags_undeclared_input() {
        let raw = source_with(
            &[("topic", "string")],
            &[("role", "{{ topic }} and {{ mystery }}")],
        );
        let issues = typecheck(&raw).unwrap_err();
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
        let issues = typecheck(&raw).unwrap_err();
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
        let issues = typecheck(&raw).unwrap_err();
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
        let issues = typecheck(&raw).unwrap_err();
        assert!(issues
            .iter()
            .any(|i| matches!(i, TypecheckIssue::UnusedInput { name } if name == "unused_one")));
    }

    #[test]
    fn flags_invalid_type_syntax() {
        let raw = source_with(&[("topic", "not_a_real_type")], &[("role", "{{ topic }}")]);
        let issues = typecheck(&raw).unwrap_err();
        assert!(issues
            .iter()
            .any(|i| matches!(i, TypecheckIssue::InvalidInputType { .. })));
    }
}
